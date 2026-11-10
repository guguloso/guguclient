//! Launch profiles (`instances:*`). Port of `InstancesService`.
//!
//! Each instance is `instances/<uuid>/instance.json` (Studio under
//! `studio/instances`), kept as a JSON object so fields this build does not
//! know about survive a save.

use std::path::Path;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::roblox_layout::{
    self as layout, ensure_instance_data_dir, instance_config_dir, instance_data_dir, is_valid_instance_id,
    is_valid_version_hash, read_json, write_json, PRODUCTS,
};
use crate::state::AppState;
use crate::util::now_iso;

const DEFAULT_FPS_CAP: i64 = 60;
const ICON_COLORS: [&str; 12] = [
    "#FF6B6B", "#4ECDC4", "#45B7D1", "#96CEB4", "#FFEAA7", "#DDA0DD", "#98D8C8", "#F7DC6F", "#BB8FCE", "#85C1E9", "#F8B500",
    "#16A085",
];

pub type Instance = Map<String, Value>;

#[derive(Default)]
pub struct InstancesService {
    cache: Mutex<Option<Vec<Instance>>>,
}

pub fn product_of(instance: &Instance) -> &str {
    instance.get("product").and_then(Value::as_str).unwrap_or("player")
}

pub fn version_hash_of(instance: &Instance) -> &str {
    instance.pointer_str("/version/versionHash")
}

trait PointerStr {
    fn pointer_str(&self, pointer: &str) -> &str;
}

impl PointerStr for Instance {
    fn pointer_str(&self, pointer: &str) -> &str {
        let mut parts = pointer.trim_start_matches('/').split('/');
        let first = parts.next().unwrap_or_default();
        let mut node = self.get(first);
        for part in parts {
            node = node.and_then(|n| n.get(part));
        }
        node.and_then(Value::as_str).unwrap_or_default()
    }
}

fn config_path(base: &Path, id: &str, product: &str) -> AppResult<std::path::PathBuf> {
    Ok(instance_config_dir(base, id, product)?.join("instance.json"))
}

/// `readInstanceFromProduct`, including the legacy `channel` migration.
fn read_instance(base: &Path, id: &str, product: &str) -> Option<Instance> {
    let path = config_path(base, id, product).ok()?;
    let Value::Object(mut instance) = read_json(&path)? else { return None };
    if instance.get("version").is_none_or(Value::is_null) {
        if let Some(channel) = instance.get("channel").cloned().filter(|c| !c.is_null()) {
            instance.insert(
                "version".into(),
                json!({ "versionHash": channel, "displayVersion": "Unknown", "channel": channel }),
            );
            instance.insert("autoUpdate".into(), Value::Bool(true));
        }
    }
    instance.insert("id".into(), Value::String(id.to_owned()));
    if instance.get("product").is_none_or(Value::is_null) {
        instance.insert("product".into(), Value::String(product.to_owned()));
    }
    if !instance.get("accountUserId").is_some_and(|v| v.is_number() || v.is_null()) {
        instance.insert("accountUserId".into(), Value::Null);
    }
    Some(instance)
}

impl InstancesService {
    fn invalidate(&self) {
        *self.cache.lock() = None;
    }

    pub fn product_of_id(&self, base: &Path, id: &str) -> Option<&'static str> {
        if !is_valid_instance_id(id) {
            return None;
        }
        PRODUCTS.into_iter().find(|p| config_path(base, id, p).is_ok_and(|path| path.exists()))
    }

    pub fn get(&self, base: &Path, id: &str) -> Option<Instance> {
        let product = self.product_of_id(base, id)?;
        read_instance(base, id, product)
    }

    pub fn all(&self, base: &Path) -> Vec<Instance> {
        if let Some(cached) = self.cache.lock().clone() {
            return cached;
        }
        let mut instances = Vec::new();
        for product in PRODUCTS {
            let dir = layout::product_paths(base, product).instances_dir;
            let _ = std::fs::create_dir_all(&dir);
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !(entry.file_type().is_ok_and(|t| t.is_dir()) && is_valid_instance_id(&name)) {
                    continue;
                }
                if let Some(instance) = read_instance(base, &name, product) {
                    instances.push(instance);
                }
            }
        }
        // Most recently launched first, then newest created. Timestamps are
        // `toISOString()` UTC strings, so text order is chronological order.
        instances.sort_by(|a, b| {
            let (la, lb) = (a.get("lastLaunchedAt").filter(|v| v.is_string()), b.get("lastLaunchedAt").filter(|v| v.is_string()));
            match (la, lb) {
                (Some(x), Some(y)) => y.as_str().cmp(&x.as_str()),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => {
                    let (ca, cb) = (a.get("createdAt").and_then(Value::as_str), b.get("createdAt").and_then(Value::as_str));
                    cb.cmp(&ca)
                }
            }
        });
        *self.cache.lock() = Some(instances.clone());
        instances
    }

    pub fn save(&self, base: &Path, instance: &Instance) -> AppResult<()> {
        self.invalidate();
        let id = instance.get("id").and_then(Value::as_str).unwrap_or_default();
        write_json(&config_path(base, id, product_of(instance))?, &Value::Object(instance.clone()))
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOptions {
    pub name: String,
    pub product: String,
    pub version: Value,
    pub auto_update: Option<bool>,
    pub account_user_id: Option<u64>,
    pub description: Option<Value>,
    pub icon_color: Option<String>,
    pub copy_from_instance_id: Option<String>,
}

fn random_color() -> &'static str {
    let index = uuid::Uuid::new_v4().as_bytes()[0] as usize % ICON_COLORS.len();
    ICON_COLORS[index]
}

pub fn create(state: &AppState, options: CreateOptions) -> AppResult<Instance> {
    let base = &state.paths.base;
    let hash = options.version.get("versionHash").and_then(Value::as_str).unwrap_or_default();
    if !is_valid_version_hash(hash) {
        tracing::warn!("Refused to create instance '{}' with invalid version hash '{hash}'", options.name);
        return Err(AppError::msg(format!("Invalid version hash: '{hash}'")));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let fast_flags = options
        .copy_from_instance_id
        .as_deref()
        .and_then(|src| state.instances.get(base, src))
        .and_then(|src| src.get("fastFlags").cloned())
        .unwrap_or_else(|| json!({}));
    let mut instance = Map::new();
    instance.insert("id".into(), json!(id));
    instance.insert("name".into(), json!(options.name));
    instance.insert("product".into(), json!(options.product));
    instance.insert("version".into(), options.version);
    instance.insert("autoUpdate".into(), json!(options.auto_update.unwrap_or(false)));
    instance.insert("accountUserId".into(), json!(options.account_user_id));
    instance.insert("fastFlags".into(), fast_flags);
    instance.insert("fpsCap".into(), json!(DEFAULT_FPS_CAP));
    instance.insert("isolated".into(), json!(true));
    instance.insert("createdAt".into(), json!(now_iso()));
    instance.insert("lastLaunchedAt".into(), Value::Null);
    if let Some(description) = options.description {
        instance.insert("description".into(), description);
    }
    let color = options.icon_color.filter(|c| !c.is_empty()).unwrap_or_else(|| random_color().to_owned());
    instance.insert("iconColor".into(), json!(color));
    state.instances.save(base, &instance)?;
    ensure_instance_data_dir(base, &id, &options.product)?;
    tracing::info!("Created {} instance: {} ({id})", options.product, options.name);
    state.track("instance.create", json!({ "instance_id": id, "instance_name": options.name }));
    Ok(instance)
}

/// `resolveLaunchAvatarUpdate`: `Err(reason)` refuses the update;
/// `Ok(None)` is JS `undefined` (key dropped), `Ok(Some(Null))` is `null`.
fn resolve_launch_avatar(
    state: &AppState,
    instance: &Instance,
    updates: &Map<String, Value>,
) -> Result<Option<Value>, &'static str> {
    let requested = updates.get("launchAvatar").filter(|v| !v.is_null() && *v != &Value::Bool(false));
    if let Some(requested) = requested {
        if product_of(instance) != "player" {
            return Err("only Player instances wear one");
        }
        let Some(user_id) = requested.get("userId").and_then(Value::as_u64) else {
            return Err("it is malformed");
        };
        if !state.auth.has_account(user_id) {
            return Err("its account is not stored");
        }
    }
    let account = if updates.contains_key("accountUserId") { updates.get("accountUserId") } else { instance.get("accountUserId") };
    let avatar = if updates.contains_key("launchAvatar") {
        updates.get("launchAvatar").cloned()
    } else {
        instance.get("launchAvatar").cloned()
    };
    let avatar_user = avatar.as_ref().and_then(|a| a.get("userId")).and_then(Value::as_u64);
    let other_account = account.and_then(Value::as_u64).is_some_and(|a| Some(a) != avatar_user);
    let has_avatar = avatar.as_ref().is_some_and(|a| a.is_object());
    if has_avatar && other_account {
        return if requested.is_some() { Err("it belongs to another account") } else { Ok(Some(Value::Null)) };
    }
    Ok(avatar)
}

pub fn update(state: &AppState, id: &str, updates: Map<String, Value>) -> Option<Instance> {
    let base = &state.paths.base;
    let instance = state.instances.get(base, id)?;
    let product = product_of(&instance).to_owned();
    if let Some(p) = updates.get("product").and_then(Value::as_str).filter(|p| *p != product) {
        tracing::warn!("Refused to mutate instance product for {id} from {product} to {p}");
        return None;
    }
    if let Some(hash) = updates.get("version").and_then(|v| v.get("versionHash")).and_then(Value::as_str) {
        if !is_valid_version_hash(hash) {
            tracing::warn!("Refused to pin invalid version hash '{hash}' on instance {id}");
            return None;
        }
    }
    let avatar = match resolve_launch_avatar(state, &instance, &updates) {
        Ok(avatar) => avatar,
        Err(reason) => {
            tracing::warn!("Refused launch avatar on instance {id}: {reason}");
            return None;
        }
    };
    let mut updated = instance;
    updated.extend(updates);
    // `launchAvatar: undefined` is dropped by JSON.stringify; `null` is kept.
    match avatar {
        Some(avatar) => updated.insert("launchAvatar".into(), avatar),
        None => updated.remove("launchAvatar"),
    };
    updated.insert("product".into(), Value::String(product));
    if let Err(error) = state.instances.save(base, &updated) {
        tracing::warn!(%error, "Failed to save instance {id}");
        return None;
    }
    Some(updated)
}

async fn delete(state: &AppState, id: &str) -> bool {
    let base = &state.paths.base;
    let Some(instance) = state.instances.get(base, id) else { return false };
    state.instances.invalidate();
    let product = product_of(&instance);
    let mut deleted = false;
    for dir in [instance_config_dir(base, id, product), instance_data_dir(base, id, product)].into_iter().flatten() {
        if dir.exists() {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => deleted = true,
                Err(error) => tracing::warn!(%error, "Failed to delete {}", dir.display()),
            }
        }
    }
    if deleted {
        tracing::info!("Deleted {product} instance: {id}");
        state.track("instance.delete", json!({ "instance_id": id }));
    }
    deleted
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn duplicate(state: &AppState, id: &str, new_name: String) -> AppResult<Option<Instance>> {
    let base = &state.paths.base;
    let Some(source) = state.instances.get(base, id) else { return Ok(None) };
    let product = product_of(&source).to_owned();
    let duplicated = create(
        state,
        CreateOptions {
            name: new_name,
            product: product.clone(),
            version: source.get("version").cloned().unwrap_or(Value::Null),
            auto_update: source.get("autoUpdate").and_then(Value::as_bool),
            account_user_id: source.get("accountUserId").and_then(Value::as_u64),
            description: source.get("description").cloned(),
            icon_color: source.get("iconColor").and_then(Value::as_str).map(str::to_owned),
            copy_from_instance_id: Some(id.to_owned()),
        },
    )?;
    let new_id = duplicated.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    if source.get("autoLaunchPlaceId").is_some_and(|v| !v.is_null()) {
        let mut patch = Map::new();
        patch.insert("autoLaunchPlaceId".into(), source["autoLaunchPlaceId"].clone());
        patch.insert("autoLaunchPlaceName".into(), source.get("autoLaunchPlaceName").cloned().unwrap_or(Value::Null));
        update(state, &new_id, patch);
    }
    if let Some(avatar) = source.get("launchAvatar").filter(|v| !v.is_null()) {
        let mut patch = Map::new();
        patch.insert("launchAvatar".into(), avatar.clone());
        update(state, &new_id, patch);
    }
    let (src_dir, dst_dir) = (instance_config_dir(base, id, &product)?, instance_config_dir(base, &new_id, &product)?);
    for name in ["mods", "mods-manifest.json"] {
        let from = src_dir.join(name);
        if from.exists() {
            if let Err(error) = copy_tree(&from, &dst_dir.join(name)) {
                tracing::warn!(%error, "Failed to copy {name} while duplicating instance {id}");
            }
        }
    }
    if let Some(font) = source.get("customFont").filter(|v| !v.is_null()) {
        let from = src_dir.join("font").join("CustomFont.ttf");
        if from.exists() {
            match copy_tree(&from, &dst_dir.join("font").join("CustomFont.ttf")) {
                Ok(()) => {
                    let mut patch = Map::new();
                    patch.insert("customFont".into(), font.clone());
                    update(state, &new_id, patch);
                }
                Err(error) => tracing::warn!(%error, "Failed to copy custom font while duplicating instance {id}"),
            }
        }
    }
    state.track(
        "instance.duplicate",
        json!({ "source_instance_id": id, "new_instance_id": new_id, "new_instance_name": duplicated.get("name") }),
    );
    Ok(Some(state.instances.get(base, &new_id).unwrap_or(duplicated)))
}

fn set_fast_flags(state: &AppState, id: &str, fast_flags: Map<String, Value>) -> AppResult<bool> {
    let base = &state.paths.base;
    let Some(mut instance) = state.instances.get(base, id) else { return Ok(false) };
    let parsed = match fast_flags.get("DFIntTaskSchedulerTargetFps") {
        Some(Value::Number(n)) => Some(Value::Number(n.clone())),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            let t = s.trim();
            t.parse::<i64>().map(Value::from).ok().or_else(|| t.parse::<f64>().ok().and_then(|f| serde_json::Number::from_f64(f).map(Value::Number)))
        }
        _ => None,
    };
    let had_key = fast_flags.contains_key("DFIntTaskSchedulerTargetFps");
    let mut flags = fast_flags;
    match parsed {
        Some(fps) => {
            instance.insert("fpsCap".into(), fps.clone());
            flags.insert("DFIntTaskSchedulerTargetFps".into(), fps);
        }
        None if !had_key => {
            instance.insert("fpsCap".into(), json!(DEFAULT_FPS_CAP));
        }
        None => {}
    }
    instance.insert("fastFlags".into(), Value::Object(flags));
    state.instances.save(base, &instance)?;
    Ok(true)
}

fn set_fps_cap(state: &AppState, id: &str, fps: Value) -> AppResult<bool> {
    let base = &state.paths.base;
    let Some(mut instance) = state.instances.get(base, id) else { return Ok(false) };
    let mut flags = instance.get("fastFlags").and_then(Value::as_object).cloned().unwrap_or_default();
    flags.insert("DFIntTaskSchedulerTargetFps".into(), fps.clone());
    if fps.as_f64() != Some(DEFAULT_FPS_CAP as f64) {
        flags.insert("FFlagFixForcingVsyncOff".into(), Value::Bool(true));
    }
    instance.insert("fpsCap".into(), fps);
    instance.insert("fastFlags".into(), Value::Object(flags));
    state.instances.save(base, &instance)?;
    Ok(true)
}

/// `markInstanceLaunched`.
pub fn mark_launched(state: &AppState, id: &str) {
    let Some(instance) = state.instances.get(&state.paths.base, id) else { return };
    let count = instance.get("playCount").and_then(Value::as_u64).unwrap_or(0) + 1;
    let now = now_iso();
    let mut patch = Map::new();
    patch.insert("lastLaunchedAt".into(), json!(now));
    patch.insert("playCount".into(), json!(count));
    patch.insert("currentSessionStart".into(), json!(now));
    update(state, id, patch);
}

/// `markInstanceStopped`: adds the session to `totalPlaytimeMs`.
pub fn mark_stopped(state: &AppState, id: &str) {
    let Some(instance) = state.instances.get(&state.paths.base, id) else { return };
    let Some(start) = instance.get("currentSessionStart").and_then(Value::as_str) else { return };
    let started = parse_iso_millis(start).unwrap_or_else(crate::util::now_millis);
    let duration = crate::util::now_millis().saturating_sub(started);
    let total = instance.get("totalPlaytimeMs").and_then(Value::as_u64).unwrap_or(0) + duration;
    let mut patch = Map::new();
    patch.insert("totalPlaytimeMs".into(), json!(total));
    patch.insert("currentSessionStart".into(), Value::Null);
    update(state, id, patch);
}

/// Parses `YYYY-MM-DDTHH:MM:SS(.mmm)Z` to Unix milliseconds.
pub fn parse_iso_millis(s: &str) -> Option<u64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (hms, ms) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>());
    let (h, mi, sec) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    let ms: i64 = ms.get(..3.min(ms.len()))?.parse().ok()?;
    // days_from_civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + ms).ok()
}

pub fn register(router: &mut Router) {
    router
        .handle("instances:getAll", |ctx, _| async move { Ok(ctx.state.instances.all(&ctx.state.paths.base)) })
        .handle("instances:get", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(ctx.state.instances.get(&ctx.state.paths.base, &id))
        })
        .handle("instances:create", |ctx, args| async move {
            let options: CreateOptions = args.get(0)?;
            create(&ctx.state, options)
        })
        .handle("instances:update", |ctx, args| async move {
            let id: String = args.get(0)?;
            let updates: Map<String, Value> = args.get(1)?;
            Ok(update(&ctx.state, &id, updates))
        })
        .handle("instances:delete", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(delete(&ctx.state, &id).await)
        })
        .handle("instances:duplicate", |ctx, args| async move {
            let id: String = args.get(0)?;
            let name: String = args.get(1)?;
            duplicate(&ctx.state, &id, name)
        })
        .handle("instances:updateVersion", |ctx, args| async move {
            let id: String = args.get(0)?;
            let version: Value = args.get(1)?;
            let mut patch = Map::new();
            patch.insert("version".into(), version);
            Ok(update(&ctx.state, &id, patch))
        })
        .handle("instances:isVersionInstalled", |ctx, args| async move {
            let id: String = args.get(0)?;
            let base = &ctx.state.paths.base;
            Ok(ctx
                .state
                .instances
                .get(base, &id)
                .is_some_and(|i| layout::is_version_installed(base, version_hash_of(&i), product_of(&i))))
        })
        .handle("instances:getFastFlags", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(ctx
                .state
                .instances
                .get(&ctx.state.paths.base, &id)
                .and_then(|i| i.get("fastFlags").cloned())
                .filter(|f| !f.is_null())
                .unwrap_or_else(|| json!({})))
        })
        .handle("instances:setFastFlags", |ctx, args| async move {
            let id: String = args.get(0)?;
            let flags: Map<String, Value> = args.get(1)?;
            set_fast_flags(&ctx.state, &id, flags)
        })
        .handle("instances:setFpsCap", |ctx, args| async move {
            let id: String = args.get(0)?;
            let fps: Value = args.get(1)?;
            set_fps_cap(&ctx.state, &id, fps)
        })
        .handle("instances:getUsedVersions", |ctx, _| async move {
            let mut seen = std::collections::HashSet::new();
            let used: Vec<Value> = ctx
                .state
                .instances
                .all(&ctx.state.paths.base)
                .iter()
                .filter(|i| seen.insert(format!("{}:{}", product_of(i), version_hash_of(i))))
                .map(|i| json!({ "product": product_of(i), "versionHash": version_hash_of(i) }))
                .collect();
            Ok(used)
        })
        .handle("instances:openDataFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            let base = &ctx.state.paths.base;
            let instance =
                ctx.state.instances.get(base, &id).ok_or_else(|| AppError::msg(format!("Instance '{id}' not found")))?;
            Ok(layout::open_path(&instance_data_dir(base, &id, product_of(&instance))?))
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_round_trip() {
        let ms = 1_794_297_176_730u64;
        assert_eq!(super::parse_iso_millis(&crate::util::iso8601_from_millis(ms)), Some(ms));
    }
}
