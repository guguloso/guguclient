//! Pinned communities with folders (`pinnedCommunities:*`). Port of
//! `PinnedCommunitiesService`; one document per account at
//! `pinned-communities/pins-<userId>.json`.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::ipc::Router;
use crate::services::roblox_layout::{is_valid_instance_id as is_uuid, read_json, write_json};
use crate::state::AppState;
use crate::util::now_iso;

const MAX_FOLDER_NAME: usize = 30;
const MAX_PINS: usize = 50;
const MAX_FOLDERS: usize = 10;

#[derive(Clone, Default, PartialEq)]
struct Doc {
    folders: Vec<Value>,
    pins: Vec<Value>,
}

impl Doc {
    fn to_json(&self) -> Value {
        json!({ "folders": self.folders, "pins": self.pins })
    }
}

fn path(state: &AppState, user_id: u64) -> PathBuf {
    state.paths.base.join("pinned-communities").join(format!("pins-{user_id}.json"))
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `sanitizePinnedCommunities`.
fn sanitize(stored: Option<Value>) -> Doc {
    let stored = stored.unwrap_or(Value::Null);
    let mut seen = HashSet::new();
    let mut folders = Vec::new();
    for entry in stored.get("folders").and_then(Value::as_array).into_iter().flatten() {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        let name = entry.get("name").and_then(Value::as_str).map(str::trim).unwrap_or("");
        let Some(created) = entry.get("createdAt").and_then(Value::as_str) else { continue };
        if !is_uuid(id) || name.is_empty() || !seen.insert(id.to_owned()) {
            continue;
        }
        folders.push(json!({ "createdAt": created, "id": id, "name": truncate(name, MAX_FOLDER_NAME) }));
        if folders.len() >= MAX_FOLDERS {
            break;
        }
    }
    let folder_ids: HashSet<String> = folders.iter().filter_map(|f| f["id"].as_str().map(str::to_owned)).collect();
    let mut seen_pins = HashSet::new();
    let mut pins = Vec::new();
    for entry in stored.get("pins").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = entry.get("communityId").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0) else { continue };
        let Some(pinned) = entry.get("pinnedAt").and_then(Value::as_str) else { continue };
        if !seen_pins.insert(id.to_bits()) {
            continue;
        }
        let mut pin = Map::new();
        pin.insert("communityId".into(), entry["communityId"].clone());
        pin.insert("pinnedAt".into(), json!(pinned));
        if let Some(folder) = entry.get("folderId").and_then(Value::as_str).filter(|f| is_uuid(f) && folder_ids.contains(*f)) {
            pin.insert("folderId".into(), json!(folder));
        }
        pins.push(Value::Object(pin));
        if pins.len() >= MAX_PINS {
            break;
        }
    }
    Doc { folders, pins }
}

fn read_doc(state: &AppState, user_id: u64) -> Doc {
    sanitize(read_json(&path(state, user_id)))
}

fn write_doc(state: &AppState, user_id: u64, doc: &Doc) {
    if let Err(error) = write_json(&path(state, user_id), &doc.to_json()) {
        tracing::warn!("Failed to write pinned communities for user {user_id}: {error}");
    }
}

fn community_of(pin: &Value) -> Option<f64> {
    pin.get("communityId").and_then(Value::as_f64)
}

fn folder_of(pin: &Value) -> Option<&str> {
    pin.get("folderId").and_then(Value::as_str)
}

/// `reorderItems`: `None` when nothing moves.
fn reorder<T: Clone>(items: &[T], from: usize, to: usize) -> Option<Vec<T>> {
    if from >= items.len() || to >= items.len() || from == to {
        return None;
    }
    let mut next = items.to_vec();
    let moved = next.remove(from);
    next.insert(to, moved);
    Some(next)
}

fn clamp_index(to: i64, len: usize) -> usize {
    to.clamp(0, (len as i64 - 1).max(0)) as usize
}

/// Applies `f` to the active account's document and persists changes.
fn mutate(state: &AppState, f: impl FnOnce(&Doc) -> Option<Doc>) -> Value {
    let Some(user_id) = state.auth.current_user_id() else { return Doc::default().to_json() };
    let doc = read_doc(state, user_id);
    match f(&doc) {
        Some(next) => {
            write_doc(state, user_id, &next);
            next.to_json()
        }
        None => doc.to_json(),
    }
}

fn pin_without_folder(pin: &Value) -> Value {
    json!({ "communityId": pin.get("communityId"), "pinnedAt": pin.get("pinnedAt") })
}

pub fn register(router: &mut Router) {
    router
        .handle("pinnedCommunities:list", |ctx, _| async move { Ok(mutate(&ctx.state, |_| None)) })
        .handle("pinnedCommunities:pin", |ctx, args| async move {
            let id: Value = args.get(0)?;
            let folder: Option<String> = args.opt(1)?;
            Ok(mutate(&ctx.state, |doc| {
                let valid = id.as_f64().is_some_and(|v| v.is_finite() && v > 0.0);
                let already = doc.pins.iter().any(|p| community_of(p) == id.as_f64());
                let target_exists = folder.as_deref().is_none_or(|f| doc.folders.iter().any(|x| x["id"] == f));
                if !valid || already || !target_exists || doc.pins.len() >= MAX_PINS {
                    return None;
                }
                let mut pin = Map::new();
                pin.insert("communityId".into(), id.clone());
                pin.insert("pinnedAt".into(), json!(now_iso()));
                if let Some(folder) = &folder {
                    pin.insert("folderId".into(), json!(folder));
                }
                let mut pins = vec![Value::Object(pin)];
                pins.extend(doc.pins.iter().cloned());
                Some(Doc { folders: doc.folders.clone(), pins })
            }))
        })
        .handle("pinnedCommunities:unpin", |ctx, args| async move {
            let id: f64 = args.get(0)?;
            Ok(mutate(&ctx.state, |doc| {
                let pins: Vec<Value> = doc.pins.iter().filter(|p| community_of(p) != Some(id)).cloned().collect();
                (pins.len() != doc.pins.len()).then(|| Doc { folders: doc.folders.clone(), pins })
            }))
        })
        .handle("pinnedCommunities:createFolder", |ctx, args| async move {
            let name: String = args.get(0)?;
            let moves: Option<Vec<f64>> = args.opt(1)?;
            let state = &ctx.state;
            let Some(user_id) = state.auth.current_user_id() else {
                return Ok(json!({ "document": Doc::default().to_json(), "folderId": null }));
            };
            let doc = read_doc(state, user_id);
            let trimmed = truncate(name.trim(), MAX_FOLDER_NAME);
            if trimmed.is_empty() || doc.folders.len() >= MAX_FOLDERS {
                return Ok(json!({ "document": doc.to_json(), "folderId": null }));
            }
            let folder_id = uuid::Uuid::new_v4().to_string();
            let moves: Vec<f64> = moves.unwrap_or_default();
            let mut folders = doc.folders.clone();
            folders.push(json!({ "createdAt": now_iso(), "id": folder_id, "name": trimmed }));
            let pins = doc
                .pins
                .iter()
                .map(|p| {
                    let mut p = p.clone();
                    if community_of(&p).is_some_and(|c| moves.contains(&c)) {
                        p["folderId"] = json!(folder_id);
                    }
                    p
                })
                .collect();
            let next = Doc { folders, pins };
            write_doc(state, user_id, &next);
            Ok(json!({ "document": next.to_json(), "folderId": folder_id }))
        })
        .handle("pinnedCommunities:renameFolder", |ctx, args| async move {
            let folder: String = args.get(0)?;
            let name: String = args.get(1)?;
            Ok(mutate(&ctx.state, |doc| {
                let trimmed = truncate(name.trim(), MAX_FOLDER_NAME);
                let current = doc.folders.iter().find(|f| f["id"] == folder.as_str())?;
                if trimmed.is_empty() || current["name"] == trimmed.as_str() {
                    return None;
                }
                let folders = doc
                    .folders
                    .iter()
                    .map(|f| {
                        let mut f = f.clone();
                        if f["id"] == folder.as_str() {
                            f["name"] = json!(trimmed);
                        }
                        f
                    })
                    .collect();
                Some(Doc { folders, pins: doc.pins.clone() })
            }))
        })
        .handle("pinnedCommunities:deleteFolder", |ctx, args| async move {
            let folder: String = args.get(0)?;
            Ok(mutate(&ctx.state, |doc| {
                if !doc.folders.iter().any(|f| f["id"] == folder.as_str()) {
                    return None;
                }
                Some(Doc {
                    folders: doc.folders.iter().filter(|f| f["id"] != folder.as_str()).cloned().collect(),
                    pins: doc.pins.iter().map(|p| if folder_of(p) == Some(folder.as_str()) { pin_without_folder(p) } else { p.clone() }).collect(),
                })
            }))
        })
        .handle("pinnedCommunities:moveToFolder", |ctx, args| async move {
            let id: f64 = args.get(0)?;
            let folder: Option<String> = args.opt(1)?;
            Ok(mutate(&ctx.state, |doc| {
                let pin = doc.pins.iter().find(|p| community_of(p) == Some(id))?;
                if folder_of(pin) == folder.as_deref() {
                    return None;
                }
                if !folder.as_deref().is_none_or(|f| doc.folders.iter().any(|x| x["id"] == f)) {
                    return None;
                }
                let pins = doc
                    .pins
                    .iter()
                    .map(|p| {
                        if community_of(p) != Some(id) {
                            return p.clone();
                        }
                        let mut moved = pin_without_folder(p);
                        if let Some(folder) = &folder {
                            moved["folderId"] = json!(folder);
                        }
                        moved
                    })
                    .collect();
                Some(Doc { folders: doc.folders.clone(), pins })
            }))
        })
        .handle("pinnedCommunities:reorderFolder", |ctx, args| async move {
            let folder: String = args.get(0)?;
            let to: Value = args.get(1)?;
            Ok(mutate(&ctx.state, |doc| {
                let to = to.as_i64().filter(|_| to.is_i64() || to.is_u64())?;
                let from = doc.folders.iter().position(|f| f["id"] == folder.as_str())?;
                let folders = reorder(&doc.folders, from, clamp_index(to, doc.folders.len()))?;
                Some(Doc { folders, pins: doc.pins.clone() })
            }))
        })
        .handle("pinnedCommunities:reorderPin", |ctx, args| async move {
            let id: f64 = args.get(0)?;
            let to: Value = args.get(1)?;
            Ok(mutate(&ctx.state, |doc| {
                let to = to.as_i64().filter(|_| to.is_i64() || to.is_u64())?;
                let target = doc.pins.iter().find(|p| community_of(p) == Some(id))?;
                let key = folder_of(target).map(str::to_owned);
                let group: Vec<Value> = doc.pins.iter().filter(|p| folder_of(p).map(str::to_owned) == key).cloned().collect();
                let from = group.iter().position(|p| community_of(p) == Some(id))?;
                let reordered = reorder(&group, from, clamp_index(to, group.len()))?;
                let mut cursor = reordered.into_iter();
                let pins = doc
                    .pins
                    .iter()
                    .map(|p| if folder_of(p).map(str::to_owned) == key { cursor.next().unwrap_or_else(|| p.clone()) } else { p.clone() })
                    .collect();
                Some(Doc { folders: doc.folders.clone(), pins })
            }))
        });
}
