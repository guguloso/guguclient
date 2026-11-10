//! Saved outfits library (`outfits:*` except wear/saveAsCharacter). Port of
//! `OutfitsService`, `OutfitLibraryStore` and `OutfitThumbnailsService`.
//!
//! Layout (shared with Electron): `outfits/<userId>/library.json` (folder
//! manifest), `outfits/<userId>/<uuid>.json` (one outfit each) and
//! `outfits/<userId>/<uuid>.png` (captured preview).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::ipc::Router;
use crate::services::roblox_layout::{is_valid_instance_id as is_uuid, read_json, write_json};
use crate::state::AppState;
use crate::util::now_iso;

const MAX_SAVED_OUTFITS: usize = 1000;
const MAX_OUTFIT_ITEMS: usize = 30;
const MAX_FOLDERS: usize = 20;
const MAX_NAME: usize = 50;
const MAX_FOLDER_NAME: usize = 30;
const MAX_ITEM_NAME: usize = 100;
const MAX_THUMB_URL: usize = 2048;
const MAX_BODY_SCALE: f64 = 10.0;
const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;
const ITEM_FIELDS: [&str; 8] = ["assetTypeId", "bundleTypeId", "id", "itemType", "name", "owned", "price", "thumbnailUrl"];
const SAVE_SOURCES: [&str; 3] = ["fitting_room", "duplicate", "import"];

#[derive(Clone, Default)]
struct Manifest {
    folders: Vec<Map<String, Value>>,
    root: Vec<String>,
}

#[derive(Clone, Default)]
struct Library {
    manifest: Manifest,
    outfits: HashMap<String, Map<String, Value>>,
}

#[derive(Default)]
pub struct OutfitsState {
    loaded: Mutex<Option<(u64, Library)>>,
}

fn user_dir(state: &AppState, user: u64) -> PathBuf {
    state.paths.base.join("outfits").join(user.to_string())
}

fn trim_name(v: Option<&Value>, max: usize) -> String {
    v.and_then(Value::as_str).map(|s| s.trim().chars().take(max).collect()).unwrap_or_default()
}

fn pos_int(v: Option<&Value>) -> Option<u64> {
    v.filter(|v| v.is_u64()).and_then(Value::as_u64).filter(|n| *n > 0)
}

fn item_key(item: &Value) -> String {
    format!("{}:{}", item.get("itemType").and_then(Value::as_str).unwrap_or(""), item.get("id").map(Value::to_string).unwrap_or_default())
}

/// `savedOutfitItemSchema` + name normalization; invalid optional fields drop.
fn sanitize_item(entry: &Value) -> Option<Map<String, Value>> {
    let mut item = entry.as_object()?.clone();
    pos_int(item.get("id"))?;
    if !matches!(item.get("itemType").and_then(Value::as_str), Some("Asset" | "Bundle")) {
        return None;
    }
    let name = trim_name(item.get("name"), MAX_ITEM_NAME);
    if name.is_empty() {
        return None;
    }
    item.insert("name".into(), json!(name));
    for key in ["assetTypeId", "bundleTypeId"] {
        if item.contains_key(key) && pos_int(item.get(key)).is_none() {
            item.remove(key);
        }
    }
    if item.get("owned").is_some_and(|v| !v.is_boolean()) {
        item.remove("owned");
    }
    if item.get("price").is_some_and(|v| !(v.is_null() || v.is_u64())) {
        item.remove("price");
    }
    if item.get("thumbnailUrl").is_some_and(|v| !(v.is_null() || v.as_str().is_some_and(|s| s.len() <= MAX_THUMB_URL))) {
        item.remove("thumbnailUrl");
    }
    Some(item)
}

fn sanitize_items(stored: Option<&Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for entry in stored.and_then(Value::as_array).into_iter().flatten() {
        let Some(item) = sanitize_item(entry) else { continue };
        let value = Value::Object(item);
        if !seen.insert(item_key(&value)) {
            continue;
        }
        out.push(value);
        if out.len() >= MAX_OUTFIT_ITEMS {
            break;
        }
    }
    out
}

/// `sanitizeSavedOutfitBody`: keeps colors/scales only when something is set.
fn sanitize_body(stored: Option<&Value>) -> Option<Value> {
    let mut body = stored?.as_object()?.clone();
    let colors = body.remove("bodyColors").and_then(|c| {
        let mut c = c.as_object()?.clone();
        c.retain(|k, v| !k.ends_with("ColorId") || pos_int(Some(v)).is_some());
        let any = ["headColorId", "leftArmColorId", "leftLegColorId", "rightArmColorId", "rightLegColorId", "torsoColorId"]
            .iter()
            .any(|k| c.contains_key(*k));
        any.then_some(Value::Object(c))
    });
    let scales = body.remove("scales").and_then(|s| {
        let mut s = s.as_object()?.clone();
        let axes = ["bodyType", "depth", "head", "height", "proportion", "width"];
        s.retain(|k, v| !axes.contains(&k.as_str()) || v.as_f64().is_some_and(|f| (0.0..=MAX_BODY_SCALE).contains(&f)));
        axes.iter().any(|k| s.contains_key(*k)).then_some(Value::Object(s))
    });
    if colors.is_none() && scales.is_none() {
        return None;
    }
    if let Some(c) = colors {
        body.insert("bodyColors".into(), c);
    }
    if let Some(s) = scales {
        body.insert("scales".into(), s);
    }
    Some(Value::Object(body))
}

fn has_ownership(items: &[Value]) -> bool {
    items.iter().any(|i| i.get("owned").is_some())
}

fn sanitize_ids(stored: Option<&Value>) -> Vec<String> {
    let mut seen = HashSet::new();
    stored
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| is_uuid(id) && seen.insert(id.to_string()))
        .map(str::to_owned)
        .collect()
}

/// `sanitizeStoredOutfit`.
fn sanitize_outfit(raw: &Value) -> Option<Map<String, Value>> {
    let mut o = raw.as_object()?.clone();
    if !o.get("createdAt").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) || !o.get("id").and_then(Value::as_str).is_some_and(is_uuid) {
        return None;
    }
    let name = trim_name(o.get("name"), MAX_NAME);
    let items = sanitize_items(o.get("items"));
    if name.is_empty() || items.is_empty() {
        return None;
    }
    let body = sanitize_body(o.get("body"));
    o.remove("body");
    o.remove("folderId");
    for key in ["lastWornAt", "ownershipCheckedAt", "thumbnailUpdatedAt"] {
        if o.get(key).is_some_and(|v| !v.as_str().is_some_and(|s| !s.is_empty())) {
            o.remove(key);
        }
    }
    if o.contains_key("robloxOutfitId") && pos_int(o.get("robloxOutfitId")).is_none() {
        o.remove("robloxOutfitId");
    }
    if !o.get("updatedAt").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        let created = o["createdAt"].clone();
        o.insert("updatedAt".into(), created);
    }
    o.insert("name".into(), json!(name));
    o.insert("items".into(), Value::Array(items));
    if let Some(body) = body {
        o.insert("body".into(), body);
    }
    Some(o)
}

fn sanitize_manifest(raw: Option<Value>) -> Manifest {
    let raw = raw.unwrap_or(Value::Null);
    let mut seen = HashSet::new();
    let mut folders = Vec::new();
    for f in raw.get("folders").and_then(Value::as_array).into_iter().flatten() {
        let Some(mut folder) = f.as_object().cloned() else { continue };
        let id = folder.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        if !is_uuid(&id) || !folder.get("createdAt").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
            continue;
        }
        let name = trim_name(folder.get("name"), MAX_FOLDER_NAME);
        if name.is_empty() || !seen.insert(id) {
            continue;
        }
        let ids = sanitize_ids(folder.get("outfitIds"));
        folder.insert("name".into(), json!(name));
        folder.insert("outfitIds".into(), json!(ids));
        folders.push(folder);
        if folders.len() >= MAX_FOLDERS {
            break;
        }
    }
    Manifest { folders, root: sanitize_ids(raw.get("rootOutfitIds")) }
}

fn manifest_json(m: &Manifest) -> Value {
    json!({ "folders": m.folders, "rootOutfitIds": m.root })
}

fn folder_ids(f: &Map<String, Value>) -> Vec<String> {
    f.get("outfitIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// `loadOutfitLibrary`: reads every outfit file, drops orphans, reconciles.
fn load(state: &AppState, user: u64) -> Library {
    let dir = user_dir(state, user);
    let mut outfits = HashMap::new();
    let mut thumbs = HashSet::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".json").filter(|id| is_uuid(id)) {
            let path = entry.path();
            match read_json(&path).as_ref().and_then(sanitize_outfit) {
                Some(o) if o["id"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(id)) => {
                    let key = o["id"].as_str().unwrap_or(id).to_owned();
                    outfits.entry(key).or_insert(o);
                }
                _ => {
                    let raw = std::fs::read_to_string(&path).unwrap_or_default();
                    let backup = path.with_file_name(format!("{id}.corrupt-{}.json", crate::util::now_millis()));
                    if std::fs::write(&backup, raw).is_ok() {
                        let _ = std::fs::remove_file(&path);
                    }
                    tracing::error!("Outfit file {} did not parse; moved aside", path.display());
                }
            }
        } else if let Some(id) = name.strip_suffix(".png").filter(|id| is_uuid(id)) {
            thumbs.insert(id.to_lowercase());
        }
    }
    for (id, outfit) in outfits.iter_mut() {
        if outfit.contains_key("thumbnailUpdatedAt") && !thumbs.contains(&id.to_lowercase()) {
            outfit.remove("thumbnailUpdatedAt");
            let _ = write_json(&dir.join(format!("{id}.json")), &Value::Object(outfit.clone()));
            tracing::warn!("Outfit {id} lost its captured preview; it will be recaptured");
        }
    }
    for id in &thumbs {
        if !outfits.keys().any(|k| k.to_lowercase() == *id) {
            let _ = std::fs::remove_file(dir.join(format!("{id}.png")));
            tracing::warn!("Removed preview of missing outfit {id}");
        }
    }
    let path = dir.join("library.json");
    let corrupt = path.exists() && read_json(&path).is_none();
    let stored = sanitize_manifest(read_json(&path));
    // `reconcileManifest`: drop unknown/duplicate ids, prepend orphans newest first.
    let mut seen = HashSet::new();
    let mut dropped = 0;
    let mut keep = |ids: Vec<String>| -> Vec<String> {
        ids.into_iter()
            .filter(|id| {
                let ok = outfits.contains_key(id) && seen.insert(id.clone());
                if !ok {
                    dropped += 1;
                }
                ok
            })
            .collect()
    };
    let root = keep(stored.root.clone());
    let folders: Vec<Map<String, Value>> = stored
        .folders
        .iter()
        .map(|f| {
            let mut f = f.clone();
            let ids = keep(folder_ids(&f));
            f.insert("outfitIds".into(), json!(ids));
            f
        })
        .collect();
    let mut orphans: Vec<&Map<String, Value>> = outfits.values().filter(|o| !seen.contains(o["id"].as_str().unwrap_or(""))).collect();
    orphans.sort_by(|a, b| {
        b["createdAt"].as_str().cmp(&a["createdAt"].as_str()).then(a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let orphan_ids: Vec<String> = orphans.iter().filter_map(|o| o["id"].as_str().map(str::to_owned)).collect();
    let changed = dropped > 0 || !orphan_ids.is_empty();
    let manifest = if changed {
        Manifest { folders, root: orphan_ids.into_iter().chain(root).collect() }
    } else {
        stored
    };
    if changed || corrupt {
        let _ = write_json(&path, &manifest_json(&manifest));
        tracing::info!("Outfit library for user {user} reconciled: {} outfits, {} folders", outfits.len(), manifest.folders.len());
    }
    Library { manifest, outfits }
}

/// `composeOutfitsView`.
fn view(lib: &Library) -> Value {
    let mut list = Vec::new();
    for id in &lib.manifest.root {
        if let Some(o) = lib.outfits.get(id) {
            list.push(Value::Object(o.clone()));
        }
    }
    for f in &lib.manifest.folders {
        for id in folder_ids(f) {
            if let Some(o) = lib.outfits.get(&id) {
                let mut o = o.clone();
                o.insert("folderId".into(), f["id"].clone());
                list.push(Value::Object(o));
            }
        }
    }
    let folders: Vec<Value> = lib
        .manifest
        .folders
        .iter()
        .map(|f| {
            let mut f = f.clone();
            f.remove("outfitIds");
            Value::Object(f)
        })
        .collect();
    json!({ "folders": folders, "outfits": list })
}

fn empty_view() -> Value {
    json!({ "folders": [], "outfits": [] })
}

/// Loads (or reuses) the active account's library.
fn current(state: &AppState) -> Option<(u64, Library)> {
    let user = state.auth.current_user_id();
    let mut loaded = state.outfits.loaded.lock();
    let Some(user) = user else {
        *loaded = None;
        return None;
    };
    if loaded.as_ref().map(|(u, _)| *u) != Some(user) {
        *loaded = Some((user, load(state, user)));
    }
    loaded.clone()
}

/// `mutate`: applies `edit`, writing only the outfit files that changed.
fn mutate(state: &AppState, edit: impl FnOnce(&Library) -> Option<Library>) -> Value {
    let Some((user, lib)) = current(state) else { return empty_view() };
    let Some(next) = edit(&lib) else { return view(&lib) };
    commit(state, user, &lib, &next);
    view(&next)
}

fn commit(state: &AppState, user: u64, prev: &Library, next: &Library) {
    let dir = user_dir(state, user);
    for (id, o) in &next.outfits {
        if prev.outfits.get(id) != Some(o) {
            if let Err(error) = write_json(&dir.join(format!("{id}.json")), &Value::Object(o.clone())) {
                tracing::warn!("Failed to write outfit {id}: {error}");
            }
        }
    }
    for id in prev.outfits.keys() {
        if !next.outfits.contains_key(id) {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }
    let m_prev = manifest_json(&prev.manifest);
    let m_next = manifest_json(&next.manifest);
    if m_prev != m_next {
        let _ = write_json(&dir.join("library.json"), &m_next);
    }
    *state.outfits.loaded.lock() = Some((user, next.clone()));
}

fn group_ids(m: &Manifest, folder: Option<&str>) -> Option<Vec<String>> {
    match folder {
        None => Some(m.root.clone()),
        Some(f) => m.folders.iter().find(|x| x["id"] == f).map(folder_ids),
    }
}

fn with_group(m: &Manifest, folder: Option<&str>, ids: Vec<String>) -> Manifest {
    let mut m = m.clone();
    match folder {
        None => m.root = ids,
        Some(f) => {
            for x in m.folders.iter_mut() {
                if x["id"] == f {
                    x.insert("outfitIds".into(), json!(ids));
                }
            }
        }
    }
    m
}

fn find_group(m: &Manifest, outfit: &str) -> Option<(Option<String>, Vec<String>)> {
    if m.root.iter().any(|id| id == outfit) {
        return Some((None, m.root.clone()));
    }
    m.folders.iter().find(|f| folder_ids(f).iter().any(|id| id == outfit)).map(|f| (f["id"].as_str().map(str::to_owned), folder_ids(f)))
}

fn replace(lib: &Library, outfit: Map<String, Value>) -> Library {
    let mut next = lib.clone();
    let id = outfit["id"].as_str().unwrap_or("").to_owned();
    next.outfits.insert(id, outfit);
    next
}

fn reorder_clamped<T: Clone>(items: &[T], from: usize, to: i64) -> Option<Vec<T>> {
    let to = to.clamp(0, items.len() as i64 - 1) as usize;
    if from >= items.len() || from == to {
        return None;
    }
    let mut next = items.to_vec();
    let moved = next.remove(from);
    next.insert(to, moved);
    Some(next)
}

fn failure(state: &AppState, error: &str) -> Value {
    let document = current(state).map(|(_, l)| view(&l)).unwrap_or_else(empty_view);
    json!({ "ok": false, "document": document, "error": error })
}

fn signed_out() -> Value {
    json!({ "document": empty_view(), "error": "signedOut", "ok": false })
}

fn track_save(state: &AppState, outfit: &Map<String, Value>, in_folder: bool, source: &str) {
    state.track(
        "marketplace.outfit.save",
        json!({ "in_folder": in_folder, "item_count": outfit["items"].as_array().map_or(0, Vec::len), "outfit_id": outfit["id"], "source": source }),
    );
}

fn create(state: &AppState, input: &Value) -> Value {
    let Some((_, lib)) = current(state) else { return signed_out() };
    let source = input.get("source").and_then(Value::as_str).filter(|s| SAVE_SOURCES.contains(s));
    let raw_items = input.get("items").and_then(Value::as_array);
    let folder = input.get("folderId").and_then(Value::as_str).filter(|f| is_uuid(f)).map(str::to_owned);
    let schema_ok = source.is_some()
        && input.get("name").is_some_and(Value::is_string)
        && raw_items.is_some_and(|i| (1..=MAX_OUTFIT_ITEMS).contains(&i.len()) && i.iter().all(|e| sanitize_item(e).is_some()))
        && input.get("folderId").is_none_or(|f| f.is_null() || folder.is_some());
    if !schema_ok {
        return failure(state, "invalidItems");
    }
    let name = trim_name(input.get("name"), MAX_NAME);
    if name.is_empty() {
        return failure(state, "emptyName");
    }
    let items = sanitize_items(input.get("items"));
    if items.is_empty() {
        return failure(state, "invalidItems");
    }
    if lib.outfits.len() >= MAX_SAVED_OUTFITS {
        return failure(state, "outfitCapReached");
    }
    let Some(ids) = group_ids(&lib.manifest, folder.as_deref()) else { return failure(state, "folderMissing") };
    let now = now_iso();
    let id = uuid::Uuid::new_v4().to_string();
    let mut outfit = Map::new();
    outfit.insert("createdAt".into(), json!(now));
    outfit.insert("id".into(), json!(id));
    outfit.insert("items".into(), Value::Array(items.clone()));
    outfit.insert("name".into(), json!(name));
    outfit.insert("updatedAt".into(), json!(now));
    if let Some(body) = sanitize_body(input.get("body")) {
        outfit.insert("body".into(), body);
    }
    if has_ownership(&items) {
        outfit.insert("ownershipCheckedAt".into(), json!(now));
    }
    let folder_ref = folder.clone();
    let saved = outfit.clone();
    let document = mutate(state, |lib| {
        let mut next = replace(lib, saved);
        let mut ids = ids;
        ids.insert(0, id.clone());
        next.manifest = with_group(&lib.manifest, folder_ref.as_deref(), ids);
        Some(next)
    });
    track_save(state, &outfit, folder.is_some(), source.unwrap_or("fitting_room"));
    json!({ "ok": true, "document": document, "outfitId": outfit["id"] })
}

fn dedupe_name(name: &str, existing: &HashSet<String>) -> String {
    let mut index = 2;
    loop {
        let suffix = format!(" ({index})");
        let keep = MAX_NAME.saturating_sub(suffix.chars().count());
        let candidate = format!("{}{suffix}", name.chars().take(keep).collect::<String>());
        if !existing.contains(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn duplicate(state: &AppState, outfit_id: &str) -> Value {
    let Some((_, lib)) = current(state) else { return signed_out() };
    let (Some(original), Some((folder, ids))) = (lib.outfits.get(outfit_id), find_group(&lib.manifest, outfit_id)) else {
        return failure(state, "outfitMissing");
    };
    if lib.outfits.len() >= MAX_SAVED_OUTFITS {
        return failure(state, "outfitCapReached");
    }
    let mut copy = original.clone();
    for key in ["lastWornAt", "robloxOutfitId", "thumbnailUpdatedAt"] {
        copy.remove(key);
    }
    let now = now_iso();
    let new_id = uuid::Uuid::new_v4().to_string();
    let existing: HashSet<String> = lib.outfits.values().filter_map(|o| o["name"].as_str().map(str::to_owned)).collect();
    copy.insert("createdAt".into(), json!(now));
    copy.insert("id".into(), json!(new_id));
    copy.insert("name".into(), json!(dedupe_name(original["name"].as_str().unwrap_or(""), &existing)));
    copy.insert("updatedAt".into(), json!(now));
    let mut ids = ids;
    let at = ids.iter().position(|id| id == outfit_id).map_or(ids.len(), |p| p + 1);
    ids.insert(at, new_id.clone());
    let saved = copy.clone();
    let document = mutate(state, |lib| {
        let mut next = replace(lib, saved);
        next.manifest = with_group(&lib.manifest, folder.as_deref(), ids);
        Some(next)
    });
    track_save(state, &copy, folder.is_some(), "duplicate");
    json!({ "ok": true, "document": document, "outfitId": new_id })
}

fn edit_outfit(state: &AppState, outfit_id: &str, f: impl FnOnce(&Map<String, Value>) -> Option<Map<String, Value>>) -> Value {
    mutate(state, |lib| {
        let outfit = lib.outfits.get(outfit_id)?;
        Some(replace(lib, f(outfit)?))
    })
}

/// `OutfitsService.list`.
pub fn list(state: &AppState) -> Value {
    current(state).map(|(_, l)| view(&l)).unwrap_or_else(empty_view)
}

/// `findForUser`: an outfit of the active or another stored account.
pub fn find_for_user(state: &AppState, user: u64, outfit_id: &str) -> Option<Map<String, Value>> {
    if user == 0 {
        return None;
    }
    match current(state) {
        Some((u, lib)) if u == user => lib.outfits.get(outfit_id).cloned(),
        _ => load(state, user).outfits.get(outfit_id).cloned(),
    }
}

/// `withOwnership` + `recordOutfitOwnership`.
fn with_ownership(outfit: &Map<String, Value>, ownership: &HashMap<String, bool>) -> Map<String, Value> {
    let mut o = outfit.clone();
    if let Some(items) = o.get_mut("items").and_then(Value::as_array_mut) {
        for item in items.iter_mut() {
            if let Some(owned) = ownership.get(&item_key(item)) {
                item["owned"] = json!(owned);
            }
        }
    }
    o.insert("ownershipCheckedAt".into(), json!(now_iso()));
    o
}

/// `recordOwnership`.
pub fn record_ownership(state: &AppState, outfit_id: &str, ownership: &HashMap<String, bool>) -> Value {
    edit_outfit(state, outfit_id, |o| Some(with_ownership(o, ownership)))
}

/// `recordWear`: ownership plus `lastWornAt`.
pub fn record_wear(state: &AppState, outfit_id: &str, ownership: &HashMap<String, bool>) -> Value {
    edit_outfit(state, outfit_id, |o| {
        let mut o = with_ownership(o, ownership);
        o.insert("lastWornAt".into(), o["ownershipCheckedAt"].clone());
        Some(o)
    })
}

/// `linkRobloxOutfit`: links (or clears) the character made from an outfit.
pub fn link_roblox_outfit(state: &AppState, outfit_id: &str, roblox_outfit: Option<u64>) -> Value {
    edit_outfit(state, outfit_id, |o| {
        if o.get("robloxOutfitId").and_then(Value::as_u64) == roblox_outfit {
            return None;
        }
        let mut o = o.clone();
        match roblox_outfit {
            Some(id) => o.insert("robloxOutfitId".into(), json!(id)),
            None => o.remove("robloxOutfitId"),
        };
        Some(o)
    })
}

fn thumbnail_path(state: &AppState, user: u64, outfit: &str) -> Option<PathBuf> {
    (is_uuid(outfit) && user > 0).then(|| user_dir(state, user).join(format!("{outfit}.png")))
}

/// `lbx-outfit` protocol: `http://lbx-outfit.localhost/<userId>/<uuid>.png`,
/// served only for the active account.
pub fn serve_thumbnail(state: &AppState, path: &str) -> Option<Vec<u8>> {
    let mut parts = path.trim_start_matches('/').split('/');
    let user: u64 = parts.next()?.parse().ok()?;
    let file = parts.next()?.strip_suffix(".png")?;
    if parts.next().is_some() || state.auth.current_user_id() != Some(user) {
        return None;
    }
    std::fs::read(thumbnail_path(state, user, file)?).ok()
}

pub fn register(router: &mut Router) {
    router
        .handle("outfits:list", |ctx, _| async move { Ok(current(&ctx.state).map(|(_, l)| view(&l)).unwrap_or_else(empty_view)) })
        .handle("outfits:listForUser", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            let state = &ctx.state;
            if user == 0 || !state.auth.has_account(user) {
                return Ok(empty_view());
            }
            if state.auth.current_user_id() == Some(user) {
                return Ok(current(state).map(|(_, l)| view(&l)).unwrap_or_else(empty_view));
            }
            Ok(view(&load(state, user)))
        })
        .handle("outfits:create", |ctx, args| async move {
            let input: Value = args.get(0)?;
            Ok(create(&ctx.state, &input))
        })
        .handle("outfits:rename", |ctx, args| async move {
            let id: String = args.get(0)?;
            let name: Value = args.get(1)?;
            Ok(edit_outfit(&ctx.state, &id, |o| {
                let trimmed = trim_name(Some(&name), MAX_NAME);
                if trimmed.is_empty() || o["name"] == trimmed.as_str() {
                    return None;
                }
                let mut o = o.clone();
                o.insert("name".into(), json!(trimmed));
                o.insert("updatedAt".into(), json!(now_iso()));
                Some(o)
            }))
        })
        .handle("outfits:refreshItems", |ctx, args| async move {
            let id: String = args.get(0)?;
            let items: Value = args.get(1)?;
            Ok(edit_outfit(&ctx.state, &id, |o| {
                let refreshed = sanitize_items(Some(&items));
                let old = o["items"].as_array().cloned().unwrap_or_default();
                let same_keys = refreshed.len() == old.len() && refreshed.iter().zip(&old).all(|(a, b)| item_key(a) == item_key(b));
                let equal = same_keys && refreshed.iter().zip(&old).all(|(a, b)| ITEM_FIELDS.iter().all(|f| a.get(*f) == b.get(*f)));
                if !same_keys || equal {
                    return None;
                }
                let mut o = o.clone();
                if has_ownership(&refreshed) {
                    o.insert("ownershipCheckedAt".into(), json!(now_iso()));
                }
                o.insert("items".into(), Value::Array(refreshed));
                Some(o)
            }))
        })
        .handle("outfits:overwrite", |ctx, args| async move {
            let id: String = args.get(0)?;
            let contents: Value = args.get(1)?;
            Ok(edit_outfit(&ctx.state, &id, |o| {
                let raw = contents.get("items").and_then(Value::as_array)?;
                if !(1..=MAX_OUTFIT_ITEMS).contains(&raw.len()) || !raw.iter().all(|e| sanitize_item(e).is_some()) {
                    return None;
                }
                let items = sanitize_items(contents.get("items"));
                if items.is_empty() {
                    return None;
                }
                let now = now_iso();
                let mut o = o.clone();
                for key in ["body", "ownershipCheckedAt", "thumbnailUpdatedAt"] {
                    o.remove(key);
                }
                if let Some(body) = sanitize_body(contents.get("body")) {
                    o.insert("body".into(), body);
                }
                if has_ownership(&items) {
                    o.insert("ownershipCheckedAt".into(), json!(now));
                }
                o.insert("items".into(), Value::Array(items));
                o.insert("updatedAt".into(), json!(now));
                Some(o)
            }))
        })
        .handle("outfits:duplicate", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(duplicate(&ctx.state, &id))
        })
        .handle("outfits:delete", |ctx, args| async move {
            let id: String = args.get(0)?;
            let state = &ctx.state;
            let Some((user, lib)) = current(state) else { return Ok(empty_view()) };
            let existed = lib.outfits.contains_key(&id);
            let document = mutate(state, |lib| {
                if !lib.outfits.contains_key(&id) {
                    return None;
                }
                let mut next = lib.clone();
                next.outfits.remove(&id);
                if let Some((folder, ids)) = find_group(&lib.manifest, &id) {
                    next.manifest = with_group(&lib.manifest, folder.as_deref(), ids.into_iter().filter(|x| *x != id).collect());
                }
                Some(next)
            });
            if existed {
                if let Some(p) = thumbnail_path(state, user, &id) {
                    let _ = std::fs::remove_file(p);
                }
            }
            Ok(document)
        })
        .handle("outfits:moveToFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            let folder: Option<String> = args.opt(1)?;
            Ok(mutate(&ctx.state, |lib| {
                let (from, ids) = find_group(&lib.manifest, &id)?;
                if from == folder {
                    return None;
                }
                let target = group_ids(&lib.manifest, folder.as_deref())?;
                let removed = with_group(&lib.manifest, from.as_deref(), ids.into_iter().filter(|x| *x != id).collect());
                let mut target = target;
                target.insert(0, id.clone());
                let mut next = lib.clone();
                next.manifest = with_group(&removed, folder.as_deref(), target);
                Some(next)
            }))
        })
        .handle("outfits:reorderOutfit", |ctx, args| async move {
            let id: String = args.get(0)?;
            let to: Value = args.get(1)?;
            Ok(mutate(&ctx.state, |lib| {
                let to = to.as_i64().filter(|_| to.is_i64() || to.is_u64())?;
                let (folder, ids) = find_group(&lib.manifest, &id)?;
                let from = ids.iter().position(|x| *x == id)?;
                let ids = reorder_clamped(&ids, from, to)?;
                let mut next = lib.clone();
                next.manifest = with_group(&lib.manifest, folder.as_deref(), ids);
                Some(next)
            }))
        })
        .handle("outfits:createFolder", |ctx, args| async move {
            let name: Value = args.get(0)?;
            let moves: Option<Vec<String>> = args.opt(1)?;
            let state = &ctx.state;
            let Some((_, lib)) = current(state) else { return Ok(signed_out()) };
            let trimmed = trim_name(Some(&name), MAX_FOLDER_NAME);
            if trimmed.is_empty() {
                return Ok(failure(state, "emptyName"));
            }
            if lib.manifest.folders.len() >= MAX_FOLDERS {
                return Ok(failure(state, "folderCapReached"));
            }
            let folder_id = uuid::Uuid::new_v4().to_string();
            let moves: HashSet<String> = moves.unwrap_or_default().into_iter().collect();
            let fid = folder_id.clone();
            let document = mutate(state, |lib| {
                let all: Vec<String> = lib.manifest.root.iter().cloned().chain(lib.manifest.folders.iter().flat_map(folder_ids)).collect();
                let moved: Vec<String> = all.into_iter().filter(|id| moves.contains(id)).collect();
                let moved_set: HashSet<&String> = moved.iter().collect();
                let mut m = lib.manifest.clone();
                m.root.retain(|id| !moved_set.contains(id));
                for f in m.folders.iter_mut() {
                    let ids: Vec<String> = folder_ids(f).into_iter().filter(|id| !moved_set.contains(id)).collect();
                    f.insert("outfitIds".into(), json!(ids));
                }
                let mut folder = Map::new();
                folder.insert("createdAt".into(), json!(now_iso()));
                folder.insert("id".into(), json!(fid));
                folder.insert("name".into(), json!(trimmed));
                folder.insert("outfitIds".into(), json!(moved));
                m.folders.push(folder);
                let mut next = lib.clone();
                next.manifest = m;
                Some(next)
            });
            Ok(json!({ "ok": true, "document": document, "folderId": folder_id }))
        })
        .handle("outfits:renameFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            let name: Value = args.get(1)?;
            Ok(mutate(&ctx.state, |lib| {
                let trimmed = trim_name(Some(&name), MAX_FOLDER_NAME);
                let f = lib.manifest.folders.iter().find(|f| f["id"] == id.as_str())?;
                if trimmed.is_empty() || f["name"] == trimmed.as_str() {
                    return None;
                }
                let mut next = lib.clone();
                for f in next.manifest.folders.iter_mut() {
                    if f["id"] == id.as_str() {
                        f.insert("name".into(), json!(trimmed));
                    }
                }
                Some(next)
            }))
        })
        .handle("outfits:deleteFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(mutate(&ctx.state, |lib| {
                let f = lib.manifest.folders.iter().find(|f| f["id"] == id.as_str())?;
                let mut next = lib.clone();
                next.manifest.root = folder_ids(f).into_iter().chain(lib.manifest.root.iter().cloned()).collect();
                next.manifest.folders.retain(|x| x["id"] != id.as_str());
                Some(next)
            }))
        })
        .handle("outfits:reorderFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            let to: Value = args.get(1)?;
            Ok(mutate(&ctx.state, |lib| {
                let to = to.as_i64().filter(|_| to.is_i64() || to.is_u64())?;
                let from = lib.manifest.folders.iter().position(|f| f["id"] == id.as_str())?;
                let folders = reorder_clamped(&lib.manifest.folders, from, to)?;
                let mut next = lib.clone();
                next.manifest.folders = folders;
                Some(next)
            }))
        })
        .handle("outfits:setThumbnail", |ctx, args| async move {
            let id: String = args.get(0)?;
            let png: String = args.get(1)?;
            let state = &ctx.state;
            let Some((user, lib)) = current(state) else { return Ok(signed_out()) };
            if !lib.outfits.contains_key(&id) {
                return Ok(failure(state, "outfitMissing"));
            }
            let max_b64 = MAX_THUMBNAIL_BYTES.div_ceil(3) * 4;
            let bytes = (!png.is_empty() && png.len() <= max_b64)
                .then(|| base64::engine::general_purpose::STANDARD.decode(&png).ok())
                .flatten()
                .filter(|b| !b.is_empty() && b.len() <= MAX_THUMBNAIL_BYTES && b.starts_with(&[0x89, b'P', b'N', b'G']));
            let Some(bytes) = bytes else {
                tracing::warn!("Refused a thumbnail for outfit {id}");
                return Ok(failure(state, "invalidImage"));
            };
            let written = thumbnail_path(state, user, &id).is_some_and(|p| {
                p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&p, bytes).is_ok()
            });
            if !written {
                return Ok(failure(state, "writeFailed"));
            }
            let document = edit_outfit(state, &id, |o| {
                let mut o = o.clone();
                o.insert("thumbnailUpdatedAt".into(), json!(now_iso()));
                Some(o)
            });
            Ok(json!({ "ok": true, "document": document }))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_items() {
        let existing: HashSet<String> = ["Fit (2)".to_owned()].into_iter().collect();
        assert_eq!(dedupe_name("Fit", &existing), "Fit (3)");
        let items = sanitize_items(Some(&json!([
            { "id": 1, "itemType": "Asset", "name": " Hat ", "price": -5 },
            { "id": 1, "itemType": "Asset", "name": "dup" },
            { "id": 2, "itemType": "Nope", "name": "x" },
        ])));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "Hat");
        assert!(items[0].get("price").is_none());
    }
}
