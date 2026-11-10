//! Per-game launch preferences (`launchPreferences:*`) stored in
//! `settings/game-preferences.json` under `games.<placeId>`, plus the
//! Overwolf/CMP channels, which are inert outside the Overwolf runtime.

use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::paths::LauncherPaths;
use crate::state::AppState;
use crate::store::JsonStore;
use crate::util::now_iso;

pub struct GamePreferences {
    store: JsonStore,
}

/// `storedGamePreferenceSchema` (loose on unknown sections).
fn valid_preference(p: &Value) -> bool {
    let Some(map) = p.as_object() else {
        return false;
    };
    let section_ok = |k: &str| map.get(k).is_none_or(|v| v.is_object());
    map.get("placeId")
        .and_then(Value::as_u64)
        .is_some_and(|id| id > 0)
        && map.get("updatedAt").is_some_and(Value::is_string)
        && section_ok("launch")
        && map
            .get("launch")
            .and_then(|l| l.get("preferredInstanceId"))
            .is_none_or(Value::is_string)
        && section_ok("avatar")
        && section_ok("macros")
}

fn has_sections(p: &Map<String, Value>) -> bool {
    p.iter()
        .any(|(k, v)| k != "placeId" && k != "updatedAt" && !v.is_null())
}

impl GamePreferences {
    pub fn open(paths: &LauncherPaths) -> AppResult<Self> {
        Ok(Self {
            store: JsonStore::open(&paths.settings_dir, "game-preferences")?,
        })
    }

    /// `readStore`: drops malformed entries (rewriting the file if any).
    pub fn read(&self) -> Map<String, Value> {
        let all = self
            .store
            .read("games", |v| v.and_then(Value::as_object).cloned())
            .unwrap_or_default();
        let valid: Map<String, Value> = all
            .iter()
            .filter(|(_, v)| valid_preference(v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if valid.len() != all.len() {
            self.write(&valid);
        }
        valid
    }

    /// The `universes` section (game avatar picks) of the same file; the
    /// caller validates the records.
    pub fn read_universes(&self) -> Map<String, Value> {
        self.store.read("universes", |v| v.and_then(Value::as_object).cloned()).unwrap_or_default()
    }

    pub fn write_universes(&self, universes: &Map<String, Value>) {
        if let Err(error) = self.store.update(|root| root["universes"] = Value::Object(universes.clone())) {
            tracing::warn!(%error, "Failed to write game avatar preferences");
        }
    }

    pub fn write(&self, games: &Map<String, Value>) {
        if let Err(error) = self
            .store
            .update(|root| root["games"] = Value::Object(games.clone()))
        {
            tracing::warn!(%error, "Failed to write game preferences");
        }
    }
}

fn to_launch(p: &Value) -> Option<Value> {
    let id = p
        .pointer("/launch/preferredInstanceId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(json!({ "placeId": p.get("placeId"), "instanceId": id, "updatedAt": p.get("updatedAt") }))
}

/// Drops the `launch` section, keeping other sections when present.
fn without_launch(store: &mut Map<String, Value>, key: &str, preference: &Value) {
    let mut next = preference.as_object().cloned().unwrap_or_default();
    next.insert("updatedAt".into(), json!(now_iso()));
    next.remove("launch");
    if has_sections(&next) {
        store.insert(key.to_owned(), Value::Object(next));
    } else {
        store.remove(key);
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("launchPreferences:get", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            let state: &AppState = &ctx.state;
            let mut store = state.game_preferences.read();
            let key = place_id.to_string();
            let Some(preference) = store.get(&key).cloned() else { return Ok(Value::Null) };
            let Some(launch) = to_launch(&preference) else { return Ok(Value::Null) };
            let instance_id = launch["instanceId"].as_str().unwrap_or_default();
            if state.instances.get(&state.paths.base, instance_id).is_none() {
                without_launch(&mut store, &key, &preference);
                state.game_preferences.write(&store);
                tracing::info!("Removed stale launch preference for placeId={place_id} (missing instance {instance_id})");
                return Ok(Value::Null);
            }
            Ok(launch)
        })
        .handle("launchPreferences:set", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            let instance_id: String = args.get(1)?;
            let state: &AppState = &ctx.state;
            if state.instances.get(&state.paths.base, &instance_id).is_none() {
                return Err(AppError::msg(format!("Instance not found: {instance_id}")));
            }
            let mut store = state.game_preferences.read();
            let key = place_id.to_string();
            let updated_at = now_iso();
            let mut preference = store.get(&key).and_then(Value::as_object).cloned().unwrap_or_default();
            let mut launch = preference.get("launch").and_then(Value::as_object).cloned().unwrap_or_default();
            launch.insert("preferredInstanceId".into(), json!(instance_id));
            preference.insert("placeId".into(), json!(place_id));
            preference.insert("updatedAt".into(), json!(updated_at));
            preference.insert("launch".into(), Value::Object(launch));
            store.insert(key, Value::Object(preference));
            state.game_preferences.write(&store);
            tracing::info!("Saved launch preference for placeId={place_id} -> instanceId={instance_id}");
            Ok(json!({ "placeId": place_id, "instanceId": instance_id, "updatedAt": updated_at }))
        })
        .handle("launchPreferences:clear", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            let mut store = ctx.state.game_preferences.read();
            let key = place_id.to_string();
            let Some(preference) = store.get(&key).cloned().filter(|p| to_launch(p).is_some()) else { return Ok(false) };
            without_launch(&mut store, &key, &preference);
            ctx.state.game_preferences.write(&store);
            tracing::info!("Cleared launch preference for placeId={place_id}");
            Ok(true)
        })
        // This runtime is never the Overwolf Electron build (`IS_OVERWOLF_ELECTRON`).
        .handle("overwolf:isOverwolf", |_, _| async move { Ok(false) })
        .handle("overwolf:getMuid", |_, _| async move { Ok(Value::Null) })
        .handle("overwolf:isCmpRequired", |_, _| async move { Ok(false) })
        .handle("overwolf:openAdPrivacySettings", |_, _| async move { Ok(Value::Null) })
        .handle("overwolf:isFirstLayerRequired", |_, _| async move { Ok(false) })
        .handle("overwolf:acknowledgeFirstLayer", |_, _| async move { Ok(Value::Null) });
}
