//! Macros (`macros:*`). Port of `MacrosService`, `MacroEnablementService`,
//! `MacroPoliciesService`, `MacroShareService`, `MacroExecutorService`,
//! `MacroTriggerService`, `RecorderService` and `MacrosFeature`: macro files
//! live in `macros/<id>.json`, per-game bindings in the `games.<placeId>.macros`
//! section of `settings/game-preferences.json`, and hotkeys only fire while a
//! tracked Roblox window is in the foreground.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::Manager;
use tokio::sync::watch;

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::macro_input::{self as input, HookMessage, HookRecorder, RawEvent};
use crate::services::mods::locale_compare;
use crate::state::AppState;
use crate::store::JsonStore;
use crate::util::{now_iso, now_millis};

const FLAG: &str = "feature-macros";
const CANCEL_ACCELERATOR: &str = "Escape";
const EXECUTION_EVENT_CHANNEL: &str = "macros:executionChanged";
const RECORDING_EVENT_CHANNEL: &str = "macros:recordingChanged";
const MAX_ACTIONS_PER_MACRO: usize = 200;
const MAX_ACTIONS_PER_RECORDING: usize = 5000;
const MAX_MACROS_PER_PLACE: usize = 32;
const MAX_DELAY_MS: i64 = 60_000;
const MAX_SCROLL_CLICKS: i64 = 100;
const MAX_TYPE_STRING_LENGTH: usize = 500;
const MAX_MACRO_REPEAT: i64 = 100;
const MAX_MACRO_ID_LENGTH: usize = 64;
const MAX_MACRO_NAME_LENGTH: usize = 100;
const MACRO_CANCELLED_TOAST_TITLE: &str = "Macro cancelled";
const FOCUS_LOST_BODY: &str = "Your macro stopped because Roblox is no longer the focused window.";
const PLACE_LEFT_BODY: &str = "Your macro stopped because you left the game it was bound to.";
const POLICIES_REFRESH: Duration = Duration::from_secs(6 * 60 * 60);
const DEFAULT_POLICY: &str = "WARN";
const PLACE_UNIVERSE_CACHE_CAP: usize = 500;
const FOREGROUND_POLL: Duration = Duration::from_millis(250);
const MIN_DELAY_MS: f64 = 15.0;
const TAP_MAX_DWELL_MS: i64 = 200;
const EVENT_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const LRMACRO_FORMAT: &str = "lrmacro";
const LRMACRO_FORMAT_VERSION: i64 = 1;
const LRMACRO_EXTENSION: &str = "lrmacro";
const MAX_LRMACRO_FILE_BYTES: u64 = 262_144;
const MAX_MACROS_PER_ENVELOPE: usize = 20;
const MAX_STAGED_IMPORTS: usize = 4;
const ANALYTICS_ERROR_MESSAGE_MAX_LENGTH: usize = 200;

struct Execution {
    macro_id: String,
    phase: &'static str,
    body: watch::Sender<bool>,
    end: watch::Sender<bool>,
}

#[derive(Default)]
struct Trigger {
    started: bool,
    registered: HashSet<String>,
    roblox_foreground: bool,
    recording_active: bool,
    running_place: Option<u64>,
    pending_universes: HashSet<u64>,
}

struct RecorderSession {
    raw: Vec<RawEvent>,
    capturing: bool,
    roblox_foreground: bool,
    at_limit: bool,
    dirty: bool,
    finalized: bool,
    hooks: Option<HookRecorder>,
}

#[derive(Default)]
pub struct MacrosState {
    records: Mutex<Option<HashMap<String, (PathBuf, Value)>>>,
    policies: Mutex<Option<HashMap<String, String>>>,
    universes: Mutex<Vec<(u64, u64)>>,
    staged: Mutex<Vec<Value>>,
    pending_preview: Mutex<Option<Value>>,
    execution: Mutex<Option<Execution>>,
    trigger: Mutex<Trigger>,
    recorder: Mutex<Option<RecorderSession>>,
    capture_enabled: Arc<AtomicBool>,
    generation: AtomicU64,
    feature_running: AtomicBool,
}

/// `JSON.stringify(value, null, 4)`.
fn pretty4(value: &Value) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    let _ = serde::Serialize::serialize(value, &mut ser);
    String::from_utf8(out).unwrap_or_default()
}

fn macros_dir(state: &AppState) -> PathBuf {
    state.paths.base.join("macros")
}

// ------------------------------------------------------------ validation

/// `isPlausibleAccelerator`.
pub fn is_plausible_accelerator(a: &str) -> bool {
    !a.is_empty() && a == a.trim() && !a.chars().any(char::is_whitespace) && a.split('+').all(|p| !p.is_empty())
}

fn is_reserved_accelerator(a: &str) -> bool {
    a.split('+').next_back().is_some_and(|k| k.eq_ignore_ascii_case("escape"))
}

fn int_in(v: Option<&Value>, min: i64, max: i64) -> bool {
    v.and_then(Value::as_i64).is_some_and(|n| (min..=max).contains(&n)) && v.is_some_and(|v| v.is_i64() || v.is_u64())
}

fn valid_modifiers(v: Option<&Value>) -> bool {
    v.is_none_or(|m| m.as_array().is_some_and(|a| a.iter().all(|x| matches!(x.as_str(), Some("control" | "shift" | "alt" | "command")))))
}

fn valid_button(v: Option<&Value>) -> bool {
    matches!(v.and_then(Value::as_str), Some("left" | "right" | "middle"))
}

/// `macroActionSchema` (`recording` only at the top level).
fn valid_action(a: &Value, allow_recording: bool) -> bool {
    let key_ok = || a.get("key").and_then(Value::as_str).is_some_and(|k| !k.is_empty()) && valid_modifiers(a.get("modifiers"));
    match a.get("type").and_then(Value::as_str) {
        Some("key-tap" | "key-down" | "key-up") => key_ok(),
        Some("hold-key") => key_ok() && int_in(a.get("ms"), 0, MAX_DELAY_MS),
        Some("type-string") => a.get("text").and_then(Value::as_str).is_some_and(|t| t.encode_utf16().count() <= MAX_TYPE_STRING_LENGTH),
        Some("mouse-click") => valid_button(a.get("button")) && a.get("double").is_none_or(Value::is_boolean),
        Some("mouse-down" | "mouse-up") => valid_button(a.get("button")),
        Some("scroll") => matches!(a.get("direction").and_then(Value::as_str), Some("up" | "down")) && int_in(a.get("clicks"), 1, MAX_SCROLL_CLICKS),
        Some("delay") => int_in(a.get("ms"), 0, MAX_DELAY_MS),
        Some("random-delay") => {
            int_in(a.get("minMs"), 0, MAX_DELAY_MS) && int_in(a.get("maxMs"), 0, MAX_DELAY_MS) && a["maxMs"].as_i64() >= a["minMs"].as_i64()
        }
        Some("recording") if allow_recording => {
            a.get("actions").and_then(Value::as_array).is_some_and(|l| l.len() <= MAX_ACTIONS_PER_RECORDING && l.iter().all(|x| valid_action(x, false)))
        }
        _ => false,
    }
}

fn valid_action_list(v: Option<&Value>, required: bool) -> bool {
    match v {
        None => !required,
        Some(l) => l.as_array().is_some_and(|l| l.len() <= MAX_ACTIONS_PER_MACRO && l.iter().all(|a| valid_action(a, true))),
    }
}

/// `macroSchema.safeParse`: the validated macro (with `version` defaulted).
pub fn validate_macro(raw: &Value) -> Result<Value, String> {
    let mut m = raw.as_object().cloned().ok_or("Invalid input: expected object")?;
    let mut issues = Vec::new();
    match m.get("version") {
        None => {
            m.insert("version".into(), json!(1));
        }
        Some(v) if v.as_i64().is_some_and(|n| n >= 1) && (v.is_i64() || v.is_u64()) => {}
        _ => issues.push("Invalid version"),
    }
    let id_ok = m.get("id").and_then(Value::as_str).is_some_and(|id| {
        !id.is_empty() && id.len() <= MAX_MACRO_ID_LENGTH && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if !id_ok {
        issues.push("Invalid id");
    }
    if !m.get("name").and_then(Value::as_str).is_some_and(|n| (1..=MAX_MACRO_NAME_LENGTH).contains(&n.encode_utf16().count())) {
        issues.push("Invalid name");
    }
    match m.get("defaultHotkey") {
        Some(Value::Null) => {}
        Some(Value::String(h)) if !is_plausible_accelerator(h) => issues.push("Hotkey is not a valid accelerator string"),
        Some(Value::String(h)) if is_reserved_accelerator(h) => issues.push("Escape cannot be used as a macro hotkey; it is reserved as the panic cancel key"),
        Some(Value::String(_)) => {}
        _ => issues.push("Invalid defaultHotkey"),
    }
    if !valid_action_list(m.get("actions"), true) || !valid_action_list(m.get("startActions"), false) || !valid_action_list(m.get("endActions"), false) {
        issues.push("Invalid actions");
    }
    if m.get("repeat").is_some_and(|r| !int_in(Some(r), 1, MAX_MACRO_REPEAT)) {
        issues.push("Invalid repeat");
    }
    if m.get("loop").is_some_and(|l| !l.is_boolean()) {
        issues.push("Invalid loop");
    }
    if !m.get("enabled").is_some_and(Value::is_boolean) {
        issues.push("Invalid enabled");
    }
    if issues.is_empty() {
        Ok(Value::Object(m))
    } else {
        Err(issues.join(" "))
    }
}

// --------------------------------------------------------------- library

fn with_records<R>(state: &AppState, f: impl FnOnce(&mut HashMap<String, (PathBuf, Value)>) -> R) -> R {
    let mut guard = state.macros.records.lock();
    if guard.is_none() {
        *guard = Some(load_records(state));
    }
    f(guard.as_mut().expect("loaded"))
}

/// `reload`: every `*.json` under `macros/` that passes validation.
fn load_records(state: &AppState) -> HashMap<String, (PathBuf, Value)> {
    let dir = macros_dir(state);
    let _ = std::fs::create_dir_all(&dir);
    let mut records = HashMap::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(error) => {
            tracing::warn!("Failed to read macros directory {}: {error}", dir.display());
            return records;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json")) {
            continue;
        }
        let parsed = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
        match parsed.and_then(|raw| validate_macro(&raw)) {
            Ok(m) => {
                records.insert(m["id"].as_str().unwrap_or("").to_owned(), (path, m));
            }
            Err(error) => tracing::warn!("Discarded malformed macro file {}: {error}", path.display()),
        }
    }
    records
}

/// `list`: sorted by name.
pub fn list(state: &AppState) -> Vec<Value> {
    let mut macros: Vec<Value> = with_records(state, |r| r.values().map(|(_, m)| m.clone()).collect());
    macros.sort_by(|a, b| locale_compare(a["name"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or("")));
    macros
}

fn get(state: &AppState, id: &str) -> Option<Value> {
    with_records(state, |r| r.get(id).map(|(_, m)| m.clone()))
}

/// `emitChanged` → `macros:libraryChanged` + trigger re-sync.
fn emit_library_changed(state: &AppState) {
    state.events.emit("macros:libraryChanged", Value::Null);
    if let Some(state) = state.arc() {
        sync_registrations(&state);
    }
}

/// `upsert`: `(macro, created, previous)`.
fn upsert(state: &AppState, raw: &Value) -> Result<(Value, bool, Option<Value>), String> {
    let validated = validate_macro(raw).inspect_err(|e| tracing::warn!("[Macros] Rejected macro payload: {e}"))?;
    let id = validated["id"].as_str().unwrap_or("").to_owned();
    let path = macros_dir(state).join(format!("{id}.json"));
    let previous = get(state, &id);
    std::fs::write(&path, pretty4(&validated)).map_err(|e| e.to_string())?;
    with_records(state, |r| r.insert(id.clone(), (path, validated.clone())));
    tracing::info!("Saved macro id={id}");
    emit_library_changed(state);
    Ok((validated, previous.is_none(), previous))
}

fn delete(state: &AppState, id: &str) -> bool {
    let Some((path, _)) = with_records(state, |r| r.get(id).cloned()) else { return false };
    if let Err(error) = std::fs::remove_file(&path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }) {
        tracing::warn!("Failed to delete macro file {}: {error}", path.display());
        return false;
    }
    with_records(state, |r| r.remove(id));
    tracing::info!("Deleted macro id={id} (file={})", path.display());
    emit_library_changed(state);
    true
}

// ------------------------------------------------------------ enablement

fn extract_bindings(entry: Option<&Value>) -> Vec<(String, Option<String>)> {
    let Some(section) = entry.and_then(|e| e.get("macros")).filter(|m| m.is_object()) else { return Vec::new() };
    let Some(bindings) = section.get("bindings").map_or(Some(Vec::new()), |b| b.as_array().cloned()) else { return Vec::new() };
    let parsed: Option<Vec<_>> = bindings
        .iter()
        .map(|b| {
            let id = b.get("macroId")?.as_str()?.to_owned();
            let hotkey = match b.get("hotkey")? {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                _ => return None,
            };
            Some((id, hotkey))
        })
        .collect();
    parsed.unwrap_or_default()
}

fn get_bindings(state: &AppState, place: u64) -> Vec<(String, Option<String>)> {
    extract_bindings(state.game_preferences.read().get(&place.to_string()))
}

/// `setBindings`: last duplicate wins; an empty set drops the section.
fn set_bindings(state: &AppState, place: u64, bindings: Vec<(String, Option<String>)>) -> Vec<(String, Option<String>)> {
    let mut deduped: Vec<(String, Option<String>)> = Vec::new();
    for b in bindings {
        match deduped.iter().position(|(id, _)| *id == b.0) {
            Some(i) => deduped[i] = b,
            None => deduped.push(b),
        }
    }
    let mut games = state.game_preferences.read();
    let key = place.to_string();
    let mut next = games.get(&key).and_then(Value::as_object).cloned().unwrap_or_default();
    next.insert("placeId".into(), json!(place));
    next.insert("updatedAt".into(), json!(now_iso()));
    if deduped.is_empty() {
        next.remove("macros");
    } else {
        let mut section = next.get("macros").and_then(Value::as_object).cloned().unwrap_or_default();
        section.insert("bindings".into(), Value::Array(deduped.iter().map(|(id, h)| json!({ "macroId": id, "hotkey": h })).collect()));
        next.insert("macros".into(), Value::Object(section));
    }
    if next.iter().any(|(k, v)| k != "placeId" && k != "updatedAt" && !v.is_null()) {
        games.insert(key, Value::Object(next));
    } else {
        games.remove(&key);
    }
    state.game_preferences.write(&games);
    tracing::info!("[Macros] Set bindings for placeId={place} -> [{}]", deduped.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(","));
    emit_library_changed(state);
    deduped
}

/// `syncBindings`: prunes bindings whose macro file is gone.
fn sync_bindings(state: &AppState, place: u64) -> Vec<(String, Option<String>)> {
    let bindings = get_bindings(state, place);
    let valid: Vec<_> = bindings.iter().filter(|(id, _)| with_records(state, |r| r.contains_key(id))).cloned().collect();
    if valid.len() == bindings.len() {
        return bindings;
    }
    set_bindings(state, place, valid)
}

/// `listForPlace`: `{ macro, hotkey }` sorted by name.
fn list_for_place(state: &AppState, place: u64) -> Vec<Value> {
    let mut out: Vec<Value> = sync_bindings(state, place).into_iter().filter_map(|(id, hotkey)| Some(json!({ "macro": get(state, &id)?, "hotkey": hotkey }))).collect();
    out.sort_by(|a, b| locale_compare(a["macro"]["name"].as_str().unwrap_or(""), b["macro"]["name"].as_str().unwrap_or("")));
    out
}

fn places_for_macro(state: &AppState, id: &str) -> Vec<u64> {
    if get(state, id).is_none() {
        return Vec::new();
    }
    state.game_preferences.read().iter().filter(|(_, e)| extract_bindings(Some(e)).iter().any(|(m, _)| m == id)).filter_map(|(k, _)| k.parse().ok()).collect()
}

fn hotkey_taken(bindings: &[(String, Option<String>)], hotkey: Option<&str>, except: Option<&str>) -> bool {
    hotkey.is_some_and(|h| bindings.iter().any(|(id, bh)| Some(id.as_str()) != except && bh.as_deref() == Some(h)))
}

async fn enable_for_place(state: &Arc<AppState>, place: u64, id: &str) -> Result<Vec<Value>, String> {
    let m = get(state, id).ok_or_else(|| format!("Macro not found: {id}"))?;
    if policy_for_place(state, place).await == "DENY" {
        return Err("Macros are disabled for this game".into());
    }
    let mut bindings = sync_bindings(state, place);
    if bindings.iter().any(|(b, _)| b == id) {
        return Ok(list_for_place(state, place));
    }
    if bindings.len() + 1 > MAX_MACROS_PER_PLACE {
        return Err(format!("Cannot enable more than {MAX_MACROS_PER_PLACE} macros in placeId={place}"));
    }
    let default = m["defaultHotkey"].as_str().map(str::to_owned);
    let hotkey = if hotkey_taken(&bindings, default.as_deref(), None) { None } else { default };
    bindings.push((id.to_owned(), hotkey));
    set_bindings(state, place, bindings);
    Ok(list_for_place(state, place))
}

fn set_hotkey_for_place(state: &AppState, place: u64, id: &str, hotkey: Option<String>) -> Result<Vec<Value>, String> {
    let bindings = sync_bindings(state, place);
    if !bindings.iter().any(|(b, _)| b == id) {
        return Err("This macro is not enabled in this game".into());
    }
    if let Some(h) = &hotkey {
        if !is_plausible_accelerator(h) {
            return Err("Hotkey is not a valid accelerator string".into());
        }
        if is_reserved_accelerator(h) {
            return Err("Escape cannot be used as a macro hotkey; it is reserved as the panic cancel key".into());
        }
        if hotkey_taken(&bindings, Some(h), Some(id)) {
            let other = bindings.iter().find(|(_, bh)| bh.as_deref() == Some(h.as_str())).and_then(|(o, _)| get(state, o)).and_then(|m| m["name"].as_str().map(str::to_owned));
            return Err(format!("Hotkey \"{h}\" is already used by {} in this game", other.unwrap_or_else(|| "another macro".into())));
        }
    }
    let next = bindings.into_iter().map(|(b, h)| if b == id { (b, hotkey.clone()) } else { (b, h) }).collect();
    set_bindings(state, place, next);
    Ok(list_for_place(state, place))
}

// -------------------------------------------------------------- policies

fn policies_store(state: &AppState) -> Option<JsonStore> {
    JsonStore::open(&state.paths.settings_dir, "macro-policies").ok()
}

fn with_policies<R>(state: &AppState, f: impl FnOnce(&mut HashMap<String, String>) -> R) -> R {
    let mut guard = state.macros.policies.lock();
    if guard.is_none() {
        let stored = policies_store(state).and_then(|s| s.read("policies", |v| v.and_then(Value::as_object).cloned())).unwrap_or_default();
        let map: HashMap<String, String> =
            stored.into_iter().filter_map(|(k, v)| v.as_str().filter(|p| matches!(*p, "DENY" | "WARN" | "ALLOW")).map(|p| (k, p.to_owned()))).collect();
        if !map.is_empty() {
            tracing::info!("[MacroPolicies] Rehydrated {} policies from store", map.len());
        }
        *guard = Some(map);
    }
    f(guard.as_mut().expect("loaded"))
}

fn lookup(state: &AppState, ids: &[Option<u64>]) -> String {
    with_policies(state, |p| ids.iter().flatten().filter(|id| **id > 0).find_map(|id| p.get(&id.to_string()).cloned())).unwrap_or_else(|| DEFAULT_POLICY.into())
}

fn universe_from_activity(state: &AppState, place: u64) -> Option<u64> {
    state.launch.running_ids().iter().find_map(|id| {
        let (p, u) = crate::services::activity::current_place(state, id)?;
        (p == place).then_some(u).flatten().filter(|u| *u > 0)
    })
}

fn cached_universe(state: &AppState, place: u64) -> Option<u64> {
    universe_from_activity(state, place).or_else(|| state.macros.universes.lock().iter().find(|(p, _)| *p == place).map(|(_, u)| *u))
}

/// `getPolicyForPlace`.
async fn policy_for_place(state: &AppState, place: u64) -> String {
    let universe = match cached_universe(state, place) {
        Some(u) => Some(u),
        None => match crate::services::games::universe_by_place(state, place).await {
            Some(u) => {
                let mut cache = state.macros.universes.lock();
                if cache.len() >= PLACE_UNIVERSE_CACHE_CAP {
                    cache.remove(0);
                }
                cache.push((place, u));
                Some(u)
            }
            None => {
                tracing::warn!("[MacroPolicies] Failed to resolve universe for placeId={place}");
                None
            }
        },
    };
    lookup(state, &[Some(place), universe])
}

async fn refresh_policies(state: &Arc<AppState>) {
    let installation = crate::services::metadata::installation_id_value(&state.paths.base);
    let query = [("installation_id", installation.as_str()), ("version", env!("CARGO_PKG_VERSION"))];
    let response: AppResult<Value> = state.metadata.api().get("/v1/games/macro-policies", &query).await;
    let response = match response {
        Ok(r) => r,
        Err(error) => {
            tracing::warn!("[MacroPolicies] Failed to fetch policies: {error}");
            return;
        }
    };
    let next: HashMap<String, String> = response["policies"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect();
    let changed = with_policies(state, |p| {
        let changed = *p != next;
        *p = next.clone();
        changed
    });
    if let Some(store) = policies_store(state) {
        let _ = store.update(|root| {
            root["policies"] = json!(next);
            root["fetchedAt"] = json!(now_millis());
        });
    }
    tracing::info!("[MacroPolicies] Fetched {} policies ({})", next.len(), if changed { "changed" } else { "unchanged" });
    if changed {
        state.events.emit("macros:policiesChanged", Value::Null);
        sync_registrations(state);
    }
}

// ----------------------------------------------------------------- share

fn analytics_track(state: &AppState, event: &str, payload: Value) {
    state.track(event, payload);
}

/// `parseEnvelopeJson`.
fn parse_envelope(text: &str) -> Result<(Option<String>, Vec<Value>), String> {
    const INVALID: &str = "This is not a valid Gugu macro export.";
    let raw: Value = serde_json::from_str(text).map_err(|_| INVALID.to_owned())?;
    let version = raw.get("version").and_then(Value::as_i64).filter(|v| *v >= 1);
    if raw.get("format").and_then(Value::as_str) != Some(LRMACRO_FORMAT) || version.is_none() {
        return Err(INVALID.into());
    }
    if version > Some(LRMACRO_FORMAT_VERSION) {
        return Err("This macro was made with a newer version of Gugu Client for Roblox. Update Gugu Client for Roblox to import it.".into());
    }
    let macros = raw.get("macros").and_then(Value::as_array).filter(|m| (1..=MAX_MACROS_PER_ENVELOPE).contains(&m.len())).ok_or(INVALID)?;
    let validated: Result<Vec<Value>, String> = macros.iter().map(validate_macro).collect();
    let validated = validated.map_err(|issues| {
        tracing::warn!("Macro envelope failed validation: {issues}");
        INVALID.to_owned()
    })?;
    let export_id = raw.get("exportId").and_then(Value::as_str).filter(|id| uuid::Uuid::parse_str(id).is_ok()).map(str::to_owned);
    Ok((export_id, validated))
}

fn read_envelope_file(path: &std::path::Path) -> Result<(Option<String>, Vec<Value>), String> {
    let size = std::fs::metadata(path).map_err(|_| "Could not read the macro file.".to_owned())?.len();
    if size > MAX_LRMACRO_FILE_BYTES {
        return Err("The macro file is too large to import.".into());
    }
    let text = std::fs::read_to_string(path).map_err(|_| "Could not read the macro file.".to_owned())?;
    parse_envelope(&text)
}

/// `stageImport`: offers the preview to the renderer for confirmation.
fn stage_import(state: &AppState, export_id: Option<String>, file_name: Option<String>, macros: Vec<Value>, source: &str) {
    let names: HashSet<String> = list(state).iter().filter_map(|m| m["name"].as_str().map(str::to_lowercase)).collect();
    let mut preview = json!({
        "importId": uuid::Uuid::new_v4().to_string(),
        "nameCollisions": macros.iter().filter_map(|m| m["name"].as_str()).filter(|n| names.contains(&n.to_lowercase())).collect::<Vec<_>>(),
        "macros": macros,
        "source": source,
    });
    if let Some(id) = export_id {
        preview["exportId"] = json!(id);
    }
    if let Some(name) = file_name {
        preview["fileName"] = json!(name);
    }
    {
        let mut staged = state.macros.staged.lock();
        staged.push(preview.clone());
        while staged.len() > MAX_STAGED_IMPORTS {
            staged.remove(0);
        }
    }
    *state.macros.pending_preview.lock() = Some(preview.clone());
    if let Some(window) = state.app.get_webview_window(crate::MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    state.events.emit_main("macros:importRequested", preview);
}

/// `stageImportFromFile`: a `.lrmacro` opened through the OS.
pub fn stage_import_from_file(state: &AppState, path: &str) {
    if !path.to_lowercase().ends_with(&format!(".{LRMACRO_EXTENSION}")) {
        return;
    }
    if !state.feature_flags.is_enabled(FLAG) {
        tracing::info!("Ignoring macro import file while feature-macros is off: {path}");
        crate::services::app::show_toast(
            state,
            &json!({ "title": "Macros are turned off right now", "body": "The macro file was not imported. Try again once macros are turned back on." }),
        );
        return;
    }
    tracing::info!("Received macro import file: {path}");
    let file = std::path::Path::new(path);
    match read_envelope_file(file) {
        Ok((export_id, macros)) => stage_import(state, export_id, file.file_name().map(|n| n.to_string_lossy().into_owned()), macros, "file"),
        Err(error) => {
            tracing::warn!("Rejected macro import file {path}: {error}");
            crate::services::app::show_toast(state, &json!({ "title": "Couldn't import macro", "body": error }));
        }
    }
}

/// `extractLrmacroPath`.
pub fn lrmacro_from_argv(argv: &[String]) -> Option<String> {
    argv.iter().find(|a| a.to_lowercase().ends_with(&format!(".{LRMACRO_EXTENSION}"))).cloned()
}

fn drop_staged(state: &AppState, import_id: &str) {
    state.macros.staged.lock().retain(|p| p["importId"] != import_id);
    let mut pending = state.macros.pending_preview.lock();
    if pending.as_ref().is_some_and(|p| p["importId"] == import_id) {
        *pending = None;
    }
}

fn dedupe_name(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(&name.to_lowercase()) {
        return name.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!(" ({n})");
            let keep = MAX_MACRO_NAME_LENGTH.saturating_sub(suffix.encode_utf16().count());
            format!("{}{suffix}", name.chars().take(keep).collect::<String>())
        })
        .find(|c| !taken.contains(&c.to_lowercase()))
        .unwrap_or_else(|| name.to_owned())
}

fn confirm_import(state: &AppState, import_id: &str) -> Value {
    let Some(preview) = state.macros.staged.lock().iter().find(|p| p["importId"] == import_id).cloned() else {
        return json!({ "ok": false, "error": "This import request has expired." });
    };
    drop_staged(state, import_id);
    let mut taken: HashSet<String> = list(state).iter().filter_map(|m| m["name"].as_str().map(str::to_lowercase)).collect();
    let mut persisted = Vec::new();
    for m in preview["macros"].as_array().into_iter().flatten() {
        let name = dedupe_name(m["name"].as_str().unwrap_or(""), &taken);
        taken.insert(name.to_lowercase());
        let mut copy = m.clone();
        copy["enabled"] = json!(false);
        copy["id"] = json!(uuid::Uuid::new_v4().to_string());
        copy["name"] = json!(name);
        match upsert(state, &copy) {
            Ok((saved, _, _)) => persisted.push(saved),
            Err(error) => {
                tracing::error!("Failed to persist imported macros: {error}");
                return json!({ "ok": false, "error": error });
            }
        }
    }
    let mut payload = json!({
        "action_count": persisted.iter().map(|m| m["actions"].as_array().map_or(0, Vec::len)).sum::<usize>(),
        "macro_count": persisted.len(),
        "source": preview["source"],
    });
    if let Some(id) = preview.get("exportId") {
        payload["export_id"] = id.clone();
    }
    analytics_track(state, "macro.import", payload);
    json!({ "ok": true, "value": persisted })
}

async fn export_to_file(state: &AppState, id: &str) -> Value {
    let Some(m) = list(state).into_iter().find(|m| m["id"] == id) else { return json!({ "ok": false, "error": "Macro not found." }) };
    let cleaned: String = m["name"].as_str().unwrap_or("").chars().filter(|c| !"\\/:*?\"<>|".contains(*c) && (*c as u32) >= 0x20).collect();
    let file_name = format!("{}.{LRMACRO_EXTENSION}", if cleaned.trim().is_empty() { "macro".to_owned() } else { cleaned.trim().to_owned() });
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Export Macro").set_file_name(&file_name).add_filter("Gugu Macro", &[LRMACRO_EXTENSION]);
    if let Some(downloads) = dirs::download_dir() {
        dialog = dialog.set_directory(downloads);
    }
    let Some(picked) = dialog.save_file().await else { return json!({ "ok": true, "value": null }) };
    let mut path = picked.path().to_path_buf();
    if !path.to_string_lossy().to_lowercase().ends_with(&format!(".{LRMACRO_EXTENSION}")) {
        path = PathBuf::from(format!("{}.{LRMACRO_EXTENSION}", path.display()));
    }
    let export_id = uuid::Uuid::new_v4().to_string();
    let envelope = json!({ "exportId": export_id, "format": LRMACRO_FORMAT, "macros": [m], "version": LRMACRO_FORMAT_VERSION });
    if let Err(error) = std::fs::write(&path, pretty4(&envelope)) {
        tracing::error!("Failed to write macro export: {error}");
        return json!({ "ok": false, "error": "Failed to write the export file." });
    }
    analytics_track(state, "macro.export", json!({ "action_count": m["actions"].as_array().map_or(0, Vec::len), "export_id": export_id, "macro_id": m["id"] }));
    json!({ "ok": true, "value": path.to_string_lossy() })
}

async fn import_from_picker(state: &AppState) -> Value {
    let picked = rfd::AsyncFileDialog::new().set_title("Import Macro").add_filter("Gugu Macro", &[LRMACRO_EXTENSION]).add_filter("All files", &["*"]).pick_file().await;
    let Some(picked) = picked else { return json!({ "ok": true, "value": false }) };
    match read_envelope_file(picked.path()) {
        Ok((export_id, macros)) => {
            stage_import(state, export_id, Some(picked.file_name()), macros, "picker");
            json!({ "ok": true, "value": true })
        }
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

// -------------------------------------------------------------- executor

fn emit_execution(state: &AppState, payload: Value) {
    state.events.emit(EXECUTION_EVENT_CHANNEL, payload);
}

/// `delay`: rejects once `abort` flips.
async fn delay(ms: u64, abort: &mut watch::Receiver<bool>) -> Result<(), ()> {
    if *abort.borrow() {
        return Err(());
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
        _ = abort.wait_for(|a| *a) => Err(()),
    }
}

enum StepError {
    Aborted,
    Input(String),
}

fn modifiers_of(a: &Value) -> Vec<String> {
    a.get("modifiers").and_then(Value::as_array).into_iter().flatten().filter_map(|m| m.as_str().map(str::to_owned)).collect()
}

/// `runActionSequence` + `applyMacroAction`.
fn run_sequence<'a>(
    actions: &'a [Value],
    abort: &'a mut watch::Receiver<bool>,
    keys: &'a mut HashSet<String>,
    buttons: &'a mut HashSet<String>,
) -> futures::future::BoxFuture<'a, Result<(), StepError>> {
    Box::pin(async move {
        for a in actions {
            if *abort.borrow() {
                return Err(StepError::Aborted);
            }
            let kind = a["type"].as_str().unwrap_or("");
            if kind == "recording" {
                let nested: Vec<Value> = a["actions"].as_array().cloned().unwrap_or_default();
                run_sequence(&nested, abort, keys, buttons).await?;
                continue;
            }
            let key = a["key"].as_str().unwrap_or("");
            let mods = modifiers_of(a);
            match kind {
                "key-tap" | "key-down" | "key-up" | "hold-key" => {
                    keys.insert(key.to_owned());
                    keys.extend(mods.iter().cloned());
                }
                "mouse-click" | "mouse-down" | "mouse-up" => {
                    buttons.insert(a["button"].as_str().unwrap_or("left").to_owned());
                }
                _ => {}
            }
            let button = a["button"].as_str().unwrap_or("left");
            let result = match kind {
                "key-tap" => input::key_toggle(key, true, &mods).and_then(|_| input::key_toggle(key, false, &mods)),
                "key-down" => input::key_toggle(key, true, &mods),
                "key-up" => input::key_toggle(key, false, &mods),
                "hold-key" => {
                    input::key_toggle(key, true, &mods).map_err(StepError::Input)?;
                    let waited = delay(a["ms"].as_u64().unwrap_or(0), abort).await;
                    input::key_toggle(key, false, &mods).map_err(StepError::Input)?;
                    waited.map_err(|_| StepError::Aborted)?;
                    Ok(())
                }
                "type-string" => input::type_string(a["text"].as_str().unwrap_or("")),
                "mouse-click" => input::mouse_click(button, a["double"].as_bool().unwrap_or(false)),
                "mouse-down" => input::mouse_toggle(true, button),
                "mouse-up" => input::mouse_toggle(false, button),
                "scroll" => {
                    let clicks = a["clicks"].as_i64().unwrap_or(0) as i32;
                    input::scroll(if a["direction"] == "up" { clicks } else { -clicks })
                }
                "delay" => {
                    delay(a["ms"].as_u64().unwrap_or(0), abort).await.map_err(|_| StepError::Aborted)?;
                    Ok(())
                }
                "random-delay" => {
                    let (min, max) = (a["minMs"].as_u64().unwrap_or(0), a["maxMs"].as_u64().unwrap_or(0));
                    let ms = if max <= min { min } else { min + (uuid::Uuid::new_v4().as_u128() % u128::from(max - min + 1)) as u64 };
                    delay(ms, abort).await.map_err(|_| StepError::Aborted)?;
                    Ok(())
                }
                other => Err(format!("Unknown macro action: {other}")),
            };
            result.map_err(StepError::Input)?;
            delay(0, abort).await.map_err(|_| StepError::Aborted)?;
        }
        Ok(())
    })
}

/// `requestCancel`: the body first, then the end block.
fn request_cancel(state: &AppState) -> bool {
    let execution = state.macros.execution.lock();
    let Some(e) = execution.as_ref() else { return false };
    if e.phase != "end" && !*e.body.borrow() {
        let _ = e.body.send(true);
    } else if !*e.end.borrow() {
        let _ = e.end.send(true);
    }
    true
}

fn set_phase(state: &AppState, phase: &'static str) {
    if let Some(e) = state.macros.execution.lock().as_mut() {
        e.phase = phase;
    }
}

fn register_escape(state: &Arc<AppState>) -> bool {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let weak = Arc::downgrade(state);
    state
        .app
        .global_shortcut()
        .on_shortcut(CANCEL_ACCELERATOR, move |_, _, event| {
            if event.state == ShortcutState::Pressed {
                if let Some(state) = weak.upgrade() {
                    request_cancel(&state);
                }
            }
        })
        .is_ok()
}

/// `MacroExecutorService.run`.
async fn run_macro(state: &Arc<AppState>, m: &Value, place: u64) -> Result<(), String> {
    let id = m["id"].as_str().unwrap_or("").to_owned();
    if let Some(running) = state.macros.execution.lock().as_ref().map(|e| e.macro_id.clone()) {
        return Err(format!("A macro is already running (id={running})"));
    }
    if policy_for_place(state, place).await == "DENY" {
        let message = "Macros are disabled for this game";
        crate::services::app::show_toast(state, &json!({ "title": "Macros disabled", "body": message }));
        emit_execution(state, json!({ "state": "error", "macroId": id, "error": message }));
        return Err(message.into());
    }
    let (body_tx, mut body_rx) = watch::channel(false);
    let (end_tx, mut end_rx) = watch::channel(false);
    {
        let mut execution = state.macros.execution.lock();
        if execution.is_some() {
            return Err("A macro is already running".into());
        }
        *execution = Some(Execution { macro_id: id.clone(), phase: "start", body: body_tx, end: end_tx });
    }
    emit_execution(state, json!({ "state": "running", "macroId": id }));
    let list = |k: &str| m.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let (start, body, end) = (list("startActions"), list("actions"), list("endActions"));
    let looping = m["loop"] == true;
    let repeat = m["repeat"].as_u64().unwrap_or(1).max(1);
    let execution_id = uuid::Uuid::new_v4().to_string();
    let started = now_millis();
    analytics_track(
        state,
        "macro.execute",
        json!({ "execution_id": execution_id, "macro_id": id, "place_id": place, "loop": looping, "repeat": repeat, "action_count": body.len(), "start_action_count": start.len(), "end_action_count": end.len() }),
    );
    tracing::info!(
        "[Macros] Starting execution of macroId={id} (start={}, body={}, end={}, repeat={})",
        start.len(),
        body.len(),
        end.len(),
        if looping { "loop".to_owned() } else { repeat.to_string() }
    );
    let escape = register_escape(state);
    if !escape {
        tracing::warn!("[Macros] Failed to register Escape cancel hotkey; falling back to in-app stop only");
    }
    let (mut keys, mut buttons) = (HashSet::new(), HashSet::new());
    let mut iterations = 0u64;
    let mut body_error = None;
    let body_result = async {
        run_sequence(&start, &mut body_rx, &mut keys, &mut buttons).await?;
        set_phase(state, "body");
        if !body.is_empty() {
            while looping || iterations < repeat {
                if *body_rx.borrow() {
                    break;
                }
                run_sequence(&body, &mut body_rx, &mut keys, &mut buttons).await?;
                iterations += 1;
            }
        } else if looping {
            let _ = body_rx.wait_for(|a| *a).await;
        }
        Ok(())
    }
    .await;
    if let Err(StepError::Input(e)) = body_result {
        body_error = Some(e);
    }
    set_phase(state, "end");
    let end_error = match run_sequence(&end, &mut end_rx, &mut keys, &mut buttons).await {
        Err(StepError::Input(e)) => Some(e),
        _ => None,
    };
    for key in &keys {
        if let Err(error) = input::key_toggle(key, false, &[]) {
            tracing::warn!("[Macros] Failed to force-release key \"{key}\": {error}");
        }
    }
    for button in &buttons {
        if let Err(error) = input::mouse_toggle(false, button) {
            tracing::warn!("[Macros] Failed to force-release mouse button \"{button}\": {error}");
        }
    }
    if escape {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let _ = state.app.global_shortcut().unregister(CANCEL_ACCELERATOR);
    }
    let (body_cancelled, end_cancelled) = {
        let execution = state.macros.execution.lock();
        execution.as_ref().map_or((false, false), |e| (*e.body.borrow(), *e.end.borrow()))
    };
    *state.macros.execution.lock() = None;
    let duration = now_millis() - started;
    let iterations_payload = if body.is_empty() { Value::Null } else { json!(iterations) };
    let fatal = body_error.filter(|_| !body_cancelled).or(end_error.filter(|_| !end_cancelled));
    if let Some(message) = fatal {
        analytics_track(
            state,
            "macro.execution_end",
            json!({
                "execution_id": execution_id, "macro_id": id, "duration_ms": duration, "outcome": "error", "iterations_completed": iterations_payload,
                "error_kind": "input", "error_message": message.chars().take(ANALYTICS_ERROR_MESSAGE_MAX_LENGTH).collect::<String>(),
            }),
        );
        emit_execution(state, json!({ "state": "error", "macroId": id, "error": message }));
        crate::services::app::show_toast(state, &json!({ "title": format!("Macro \"{}\" failed", m["name"].as_str().unwrap_or("")), "body": message }));
        tracing::warn!("[Macros] Execution of macroId={id} ended in error: {message}");
        return Err(message);
    }
    analytics_track(
        state,
        "macro.execution_end",
        json!({
            "execution_id": execution_id, "macro_id": id, "duration_ms": duration,
            "outcome": if body_cancelled || end_cancelled { "cancelled" } else { "completed" }, "iterations_completed": iterations_payload,
        }),
    );
    emit_execution(state, json!({ "state": "idle", "macroId": id }));
    tracing::info!("[Macros] Finished execution of macroId={id}");
    Ok(())
}

fn running_macro_id(state: &AppState) -> Option<String> {
    state.macros.execution.lock().as_ref().map(|e| e.macro_id.clone())
}

// --------------------------------------------------------------- trigger

fn roblox_foreground(state: &AppState) -> bool {
    crate::services::mission_control::foreground_pid().is_some_and(|pid| state.launch.running_pids().contains(&pid))
}

/// `syncRegistrations`.
pub fn sync_registrations(state: &Arc<AppState>) {
    let (active, foreground, recording) = {
        let t = state.macros.trigger.lock();
        (t.started, t.roblox_foreground, t.recording_active)
    };
    if !active {
        return;
    }
    let desired = if foreground && !recording { desired_accelerators(state) } else { HashSet::new() };
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let shortcuts = state.app.global_shortcut();
    let registered: HashSet<String> = state.macros.trigger.lock().registered.clone();
    for accel in registered.difference(&desired) {
        let _ = shortcuts.unregister(accel.as_str());
        state.macros.trigger.lock().registered.remove(accel);
    }
    for accel in desired.difference(&registered) {
        let weak = Arc::downgrade(state);
        let fired = accel.clone();
        let result = shortcuts.on_shortcut(accel.as_str(), move |_, _, event| {
            if event.state == ShortcutState::Pressed {
                if let Some(state) = weak.upgrade() {
                    let accel = fired.clone();
                    tauri::async_runtime::spawn(async move { handle_trigger(&state, &accel).await });
                }
            }
        });
        match result {
            Ok(()) => {
                state.macros.trigger.lock().registered.insert(accel.clone());
            }
            Err(_) => tracing::warn!("[Macros] Failed to register accelerator \"{accel}\""),
        }
    }
}

/// `computeDesiredAccelerators`.
fn desired_accelerators(state: &Arc<AppState>) -> HashSet<String> {
    let mut accelerators = HashSet::new();
    let mut seen = HashSet::new();
    for id in state.launch.running_ids() {
        let Some((place, _)) = crate::services::activity::current_place(state, &id).filter(|(p, _)| *p > 0) else { continue };
        if !seen.insert(place) {
            continue;
        }
        let universe = cached_universe(state, place);
        if lookup(state, &[Some(place), universe]) == "DENY" {
            continue;
        }
        if universe.is_none() {
            if state.macros.trigger.lock().pending_universes.insert(place) {
                let state = Arc::clone(state);
                tauri::async_runtime::spawn(async move {
                    policy_for_place(&state, place).await;
                    state.macros.trigger.lock().pending_universes.remove(&place);
                    if cached_universe(&state, place).is_some() {
                        sync_registrations(&state);
                    }
                });
            }
            continue;
        }
        for entry in list_for_place(state, place) {
            if entry["macro"]["enabled"] == true {
                if let Some(h) = entry["hotkey"].as_str() {
                    accelerators.insert(h.to_owned());
                }
            }
        }
    }
    accelerators
}

fn foreground_place(state: &AppState) -> Option<u64> {
    let pid = crate::services::mission_control::foreground_pid()?;
    let id = state.launch.instance_id_for_pid(pid)?;
    crate::services::activity::current_place(state, &id).map(|(p, _)| p).filter(|p| *p > 0)
}

/// `handleTrigger`.
async fn handle_trigger(state: &Arc<AppState>, accel: &str) {
    let Some(place) = foreground_place(state) else {
        tracing::debug!("[Macros] Trigger {accel} suppressed: no tracked Roblox instance is focused");
        return;
    };
    if !state.macros.trigger.lock().started {
        return;
    }
    let Some(entry) = list_for_place(state, place).into_iter().find(|e| e["hotkey"] == accel) else {
        tracing::debug!("[Macros] Trigger {accel} ignored: focused placeId={place} has no binding");
        return;
    };
    let m = entry["macro"].clone();
    let id = m["id"].as_str().unwrap_or("").to_owned();
    let policy = policy_for_place(state, place).await;
    if !state.macros.trigger.lock().started {
        return;
    }
    if policy == "DENY" {
        tracing::warn!("[Macros] Trigger {accel} for macroId={id} blocked: policy is DENY for placeId={place}");
        analytics_track(state, "macro.trigger_skipped", json!({ "macro_id": id, "place_id": place, "reason": "policy_denied" }));
        sync_registrations(state);
        return;
    }
    match running_macro_id(state) {
        Some(running) if running == id => {
            request_cancel(state);
            analytics_track(state, "macro.trigger_skipped", json!({ "macro_id": id, "place_id": place, "reason": "toggle_cancel" }));
            return;
        }
        Some(_) => {
            analytics_track(state, "macro.trigger_skipped", json!({ "macro_id": id, "place_id": place, "reason": "other_running" }));
            return;
        }
        None => {}
    }
    state.macros.trigger.lock().running_place = Some(place);
    if let Err(error) = run_macro(state, &m, place).await {
        tracing::warn!("[Macros] Macro execution failed for accelerator {accel}: {error}");
    }
    state.macros.trigger.lock().running_place = None;
}

fn cancel_with_toast(state: &AppState, body: &str) {
    {
        let mut t = state.macros.trigger.lock();
        if t.running_place.is_none() || running_macro_id(state).is_none() {
            return;
        }
        t.running_place = None;
    }
    request_cancel(state);
    crate::services::app::show_toast(state, &json!({ "title": MACRO_CANCELLED_TOAST_TITLE, "body": body }));
}

/// Activity changed: cancel a macro whose place was left, then re-sync.
pub fn on_activity_changed(state: &Arc<AppState>) {
    let place = state.macros.trigger.lock().running_place;
    if let Some(place) = place.filter(|_| running_macro_id(state).is_some()) {
        let still = state.launch.running_ids().iter().any(|id| crate::services::activity::current_place(state, id).map(|(p, _)| p) == Some(place));
        if !still {
            cancel_with_toast(state, PLACE_LEFT_BODY);
        }
    }
    sync_registrations(state);
}

fn set_recording_active(state: &Arc<AppState>, active: bool) {
    {
        let mut t = state.macros.trigger.lock();
        if t.recording_active == active {
            return;
        }
        t.recording_active = active;
    }
    sync_registrations(state);
}

// -------------------------------------------------------------- recorder

enum Item {
    Key { down: bool, key: String, t: f64 },
    Mouse { down: bool, button: &'static str, t: f64 },
    Wheel { up: bool, clicks: i64, t: f64 },
}

/// `resolveEvents` + `emitDiscreteActions` + `compressActions`.
fn raw_events_to_actions(events: &[RawEvent]) -> (Vec<Value>, usize, usize) {
    let (mut items, mut unmapped) = (Vec::new(), 0usize);
    let (mut held_keys, mut held_buttons) = (HashSet::new(), HashSet::new());
    let mut wheel = 0.0f64;
    for e in events {
        match e {
            RawEvent::Key { down, vk, t_ms } => {
                let Some(key) = input::key_name(*vk) else {
                    unmapped += 1;
                    continue;
                };
                let changed = if *down { held_keys.insert(key.clone()) } else { held_keys.remove(&key) };
                if key != "escape" && changed {
                    items.push(Item::Key { down: *down, key, t: *t_ms });
                }
            }
            RawEvent::Mouse { down, button, t_ms } => {
                let Some(name) = ["left", "right", "middle"].get(*button as usize).copied() else {
                    unmapped += 1;
                    continue;
                };
                let changed = if *down { held_buttons.insert(name) } else { held_buttons.remove(name) };
                if changed {
                    items.push(Item::Mouse { down: *down, button: name, t: *t_ms });
                }
            }
            RawEvent::Wheel { rotation, t_ms } => {
                wheel += rotation;
                let clicks = wheel.trunc();
                if clicks != 0.0 {
                    wheel -= clicks;
                    items.push(Item::Wheel { up: clicks > 0.0, clicks: clicks.abs() as i64, t: *t_ms });
                }
            }
        }
    }
    let mut actions: Vec<Value> = Vec::new();
    let mut previous: Option<f64> = None;
    for item in &items {
        let t = match item {
            Item::Key { t, .. } | Item::Mouse { t, .. } | Item::Wheel { t, .. } => *t,
        };
        let delta = previous.map_or(0.0, |p| t - p);
        let has_delay = previous.is_some() && delta >= MIN_DELAY_MS;
        if has_delay {
            actions.push(json!({ "type": "delay", "ms": (delta.round() as i64).min(MAX_DELAY_MS) }));
        }
        previous = Some(t);
        match item {
            Item::Wheel { up, clicks, .. } => {
                let direction = if *up { "up" } else { "down" };
                if let Some(last) = actions.last_mut().filter(|l| !has_delay && l["type"] == "scroll" && l["direction"] == direction) {
                    last["clicks"] = json!((last["clicks"].as_i64().unwrap_or(0) + clicks).min(MAX_SCROLL_CLICKS));
                    continue;
                }
                actions.push(json!({ "type": "scroll", "direction": direction, "clicks": (*clicks).min(MAX_SCROLL_CLICKS) }));
            }
            Item::Key { down, key, .. } => actions.push(json!({ "type": if *down { "key-down" } else { "key-up" }, "key": key })),
            Item::Mouse { down, button, .. } => actions.push(json!({ "type": if *down { "mouse-down" } else { "mouse-up" }, "button": button })),
        }
    }
    let mut compressed = Vec::new();
    let mut i = 0;
    while i < actions.len() {
        let (first, second, third) = (&actions[i], actions.get(i + 1), actions.get(i + 2));
        if first["type"] == "key-down" {
            if second.is_some_and(|s| s["type"] == "key-up" && s["key"] == first["key"]) {
                compressed.push(json!({ "type": "key-tap", "key": first["key"] }));
                i += 2;
                continue;
            }
            if let (Some(s), Some(t)) = (second, third) {
                if s["type"] == "delay" && t["type"] == "key-up" && t["key"] == first["key"] {
                    let ms = s["ms"].as_i64().unwrap_or(0);
                    compressed.push(if ms <= TAP_MAX_DWELL_MS { json!({ "type": "key-tap", "key": first["key"] }) } else { json!({ "type": "hold-key", "key": first["key"], "ms": ms }) });
                    i += 3;
                    continue;
                }
            }
        }
        if first["type"] == "mouse-down" {
            if second.is_some_and(|s| s["type"] == "mouse-up" && s["button"] == first["button"]) {
                compressed.push(json!({ "type": "mouse-click", "button": first["button"] }));
                i += 2;
                continue;
            }
            if let (Some(s), Some(t)) = (second, third) {
                if s["type"] == "delay" && s["ms"].as_i64().unwrap_or(0) <= TAP_MAX_DWELL_MS && t["type"] == "mouse-up" && t["button"] == first["button"] {
                    compressed.push(json!({ "type": "mouse-click", "button": first["button"] }));
                    i += 3;
                    continue;
                }
            }
        }
        compressed.push(first.clone());
        i += 1;
    }
    let truncated = compressed.len().saturating_sub(MAX_ACTIONS_PER_RECORDING);
    compressed.truncate(MAX_ACTIONS_PER_RECORDING);
    (compressed, truncated, unmapped)
}

fn emit_recording(state: &AppState, payload: Value) {
    state.events.emit(RECORDING_EVENT_CHANNEL, payload);
}

/// `applyCaptureState`: capture only while Roblox is focused and under the cap.
fn apply_capture(state: &AppState, s: &mut RecorderSession) {
    let desired = s.roblox_foreground && !s.at_limit;
    if desired != s.capturing {
        s.capturing = desired;
        state.macros.capture_enabled.store(desired, Ordering::SeqCst);
    }
}

/// `emitLive`.
fn emit_live(state: &AppState, s: &mut RecorderSession) {
    let (actions, _, unmapped) = raw_events_to_actions(&s.raw);
    if !s.at_limit && actions.len() >= MAX_ACTIONS_PER_RECORDING {
        s.at_limit = true;
        apply_capture(state, s);
        tracing::warn!("[Macros] Recording reached the {MAX_ACTIONS_PER_RECORDING}-action cap; capture paused");
    }
    let preview: Vec<Value> = actions.iter().rev().take(200).rev().cloned().collect();
    let mut payload = json!({ "status": "recording", "actionCount": actions.len(), "capturing": s.capturing, "atLimit": s.at_limit, "preview": preview });
    if unmapped > 0 {
        payload["unmapped"] = json!(unmapped);
    }
    emit_recording(state, payload);
}

async fn start_recording(state: &Arc<AppState>) -> Result<(), String> {
    if state.macros.recorder.lock().is_some() {
        return Err("A recording is already in progress".into());
    }
    if running_macro_id(state).is_some() {
        return Err("Cannot record while a macro is running".into());
    }
    set_recording_active(state, true);
    state.macros.capture_enabled.store(false, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::channel();
    let hooks = match HookRecorder::start(tx) {
        Ok(h) => h,
        Err(error) => {
            set_recording_active(state, false);
            return Err(error);
        }
    };
    let generation = state.macros.generation.fetch_add(1, Ordering::SeqCst) + 1;
    *state.macros.recorder.lock() =
        Some(RecorderSession { raw: Vec::new(), capturing: false, roblox_foreground: false, at_limit: false, dirty: false, finalized: false, hooks: Some(hooks) });
    // Hook events (only while capture is enabled; Escape stops while capturing).
    let sink_state = Arc::clone(state);
    let capture = Arc::clone(&state.macros.capture_enabled);
    std::thread::spawn(move || {
        while let Ok(message) = rx.recv() {
            match message {
                HookMessage::Event(event) if capture.load(Ordering::SeqCst) => {
                    if let Some(s) = sink_state.macros.recorder.lock().as_mut() {
                        s.raw.push(event);
                        s.dirty = true;
                    }
                }
                HookMessage::Escape if capture.load(Ordering::SeqCst) => {
                    let state = Arc::clone(&sink_state);
                    tauri::async_runtime::spawn(async move { stop_recording(&state) });
                }
                _ => {}
            }
        }
    });
    // Foreground gate and live flush.
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        let mut since_poll = Duration::ZERO;
        loop {
            let Some(state) = weak.upgrade() else { return };
            if state.macros.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let foreground = if since_poll.is_zero() { Some(roblox_foreground(&state)) } else { None };
            {
                let mut recorder = state.macros.recorder.lock();
                let Some(s) = recorder.as_mut().filter(|s| !s.finalized) else { return };
                let mut changed = false;
                if let Some(fg) = foreground.filter(|fg| *fg != s.roblox_foreground) {
                    s.roblox_foreground = fg;
                    apply_capture(&state, s);
                    changed = true;
                }
                if s.dirty || changed {
                    s.dirty = false;
                    emit_live(&state, s);
                }
            }
            drop(state);
            tokio::time::sleep(EVENT_FLUSH_INTERVAL).await;
            since_poll += EVENT_FLUSH_INTERVAL;
            if since_poll >= FOREGROUND_POLL {
                since_poll = Duration::ZERO;
            }
        }
    });
    tracing::info!("[Macros] Recording started");
    emit_recording(state, json!({ "status": "recording", "actionCount": 0, "capturing": false, "atLimit": false, "preview": [] }));
    Ok(())
}

/// `stopRecording` → `finalize`.
fn stop_recording(state: &Arc<AppState>) {
    let Some(mut session) = state.macros.recorder.lock().take() else { return };
    if session.finalized {
        return;
    }
    session.finalized = true;
    state.macros.capture_enabled.store(false, Ordering::SeqCst);
    state.macros.generation.fetch_add(1, Ordering::SeqCst);
    if let Some(hooks) = session.hooks.take() {
        hooks.stop();
    }
    let (actions, truncated, unmapped) = raw_events_to_actions(&session.raw);
    if truncated > 0 {
        tracing::warn!("[Macros] Recording truncated: {truncated} actions over the cap were dropped");
    }
    if unmapped > 0 {
        tracing::warn!("[Macros] Recording skipped {unmapped} events with no macro key mapping");
    }
    tracing::info!("[Macros] Recording stopped with {} actions from {} raw events", actions.len(), session.raw.len());
    let mut payload = json!({ "status": "stopped", "actionCount": actions.len(), "actions": actions });
    if truncated > 0 {
        payload["truncated"] = json!(truncated);
    }
    if unmapped > 0 {
        payload["unmapped"] = json!(unmapped);
    }
    set_recording_active(state, false);
    emit_recording(state, payload);
}

// --------------------------------------------------------------- feature

/// `MacrosFeature.start`: policy cache, periodic refresh, triggers.
pub fn start(state: &Arc<AppState>) {
    if !state.feature_flags.is_enabled(FLAG) || state.macros.feature_running.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("[FeatureGate] {FLAG} -> on");
    with_policies(state, |_| ());
    state.macros.trigger.lock().started = true;
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            if !state.macros.feature_running.load(Ordering::SeqCst) {
                return;
            }
            refresh_policies(&state).await;
            drop(state);
            tokio::time::sleep(POLICIES_REFRESH).await;
        }
    });
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            if !state.macros.feature_running.load(Ordering::SeqCst) {
                return;
            }
            let next = roblox_foreground(&state);
            let changed = {
                let mut t = state.macros.trigger.lock();
                let changed = t.roblox_foreground != next;
                t.roblox_foreground = next;
                changed
            };
            if changed {
                tracing::debug!("[Macros] Roblox-foreground state changed to {next}");
                if !next {
                    cancel_with_toast(&state, FOCUS_LOST_BODY);
                }
                sync_registrations(&state);
            }
            drop(state);
            tokio::time::sleep(FOREGROUND_POLL).await;
        }
    });
    tracing::info!("[Macros] Trigger service started");
}

/// `MacrosFeature.stop`.
pub fn stop(state: &Arc<AppState>) {
    if !state.macros.feature_running.swap(false, Ordering::SeqCst) {
        return;
    }
    request_cancel(state);
    stop_recording(state);
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let registered: Vec<String> = {
        let mut t = state.macros.trigger.lock();
        t.started = false;
        t.roblox_foreground = false;
        t.registered.drain().collect()
    };
    for accel in registered {
        let _ = state.app.global_shortcut().unregister(accel.as_str());
    }
    tracing::info!("[Macros] Trigger service stopped");
}

fn feature_disabled() -> Value {
    json!({ "ok": false, "reason": "featureDisabled" })
}

pub fn register(router: &mut Router) {
    router
        .handle("macros:list", |ctx, _| async move { Ok(list(&ctx.state)) })
        .handle("macros:get", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(get(&ctx.state, &id))
        })
        .handle("macros:listForPlace", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            Ok(list_for_place(&ctx.state, place))
        })
        .handle("macros:upsert", |ctx, args| async move {
            let raw: Value = args.get(0)?;
            let state = &ctx.state;
            let (m, created, previous) = match upsert(state, &raw) {
                Ok(r) => r,
                Err(error) => return Ok(json!({ "ok": false, "error": error })),
            };
            let len = |k: &str| m.get(k).and_then(Value::as_array).map_or(0, Vec::len);
            let mut payload = json!({
                "macro_id": m["id"], "action_count": len("actions"), "start_action_count": len("startActions"), "end_action_count": len("endActions"),
                "has_hotkey": !m["defaultHotkey"].is_null(), "loop": m["loop"] == true, "repeat": m["repeat"].as_u64().unwrap_or(1),
            });
            if created {
                analytics_track(state, "macro.create", payload);
            } else {
                payload["hotkey_changed"] = json!(previous.as_ref().map(|p| &p["defaultHotkey"]) != Some(&m["defaultHotkey"]));
                payload["enabled_in_place_count"] = json!(places_for_macro(state, m["id"].as_str().unwrap_or("")).len());
                analytics_track(state, "macro.update", payload);
            }
            Ok(json!({ "ok": true, "value": m }))
        })
        .handle("macros:delete", |ctx, args| async move {
            let id: String = args.get(0)?;
            let count = places_for_macro(&ctx.state, &id).len();
            let removed = delete(&ctx.state, &id);
            if removed {
                analytics_track(&ctx.state, "macro.delete", json!({ "macro_id": id, "enabled_in_place_count": count }));
            }
            Ok(removed)
        })
        .handle("macros:enableForPlace", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            let id: String = args.get(1)?;
            match enable_for_place(&ctx.state, place, &id).await {
                Ok(list) => {
                    analytics_track(&ctx.state, "macro.enable_for_place", json!({ "macro_id": id, "place_id": place }));
                    Ok(json!({ "ok": true, "value": list }))
                }
                Err(error) => Ok(json!({ "ok": false, "error": error })),
            }
        })
        .handle("macros:disableForPlace", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            let id: String = args.get(1)?;
            let state = &ctx.state;
            let bindings = sync_bindings(state, place);
            if bindings.iter().any(|(b, _)| *b == id) {
                set_bindings(state, place, bindings.into_iter().filter(|(b, _)| *b != id).collect());
            }
            analytics_track(state, "macro.disable_for_place", json!({ "macro_id": id, "place_id": place }));
            Ok(list_for_place(state, place))
        })
        .handle("macros:setHotkeyForPlace", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            let id: String = args.get(1)?;
            let hotkey: Option<String> = args.opt(2)?;
            Ok(match set_hotkey_for_place(&ctx.state, place, &id, hotkey) {
                Ok(list) => json!({ "ok": true, "value": list }),
                Err(error) => json!({ "ok": false, "error": error }),
            })
        })
        .handle("macros:getPlacesForMacro", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(places_for_macro(&ctx.state, &id))
        })
        .handle("macros:refresh", |ctx, _| async move {
            *ctx.state.macros.records.lock() = None;
            emit_library_changed(&ctx.state);
            Ok(Value::Null)
        })
        .handle("macros:openFolder", |ctx, _| async move {
            let error = crate::services::roblox_layout::open_path(&macros_dir(&ctx.state));
            if !error.is_empty() {
                return Err(AppError::msg(error));
            }
            Ok(Value::Null)
        })
        .handle("macros:cancel", |ctx, _| async move { Ok(request_cancel(&ctx.state)) })
        .handle("macros:permissionStatus", |_, _| async move { Ok(json!({ "granted": true })) })
        .handle("macros:requestAccessibilityPermission", |_, _| async move { Ok(json!({ "prompted": false, "status": { "granted": true } })) })
        .handle("macros:openAccessibilitySettings", |_, _| async move { Ok(Value::Null) })
        .handle("macros:startRecording", |ctx, _| async move {
            if !ctx.state.feature_flags.is_enabled(FLAG) {
                return Ok(feature_disabled());
            }
            Ok(match start_recording(&ctx.state).await {
                Ok(()) => json!({ "ok": true }),
                Err(error) => json!({ "ok": false, "reason": "failed", "error": error }),
            })
        })
        .handle("macros:stopRecording", |ctx, _| async move {
            stop_recording(&ctx.state);
            Ok(Value::Null)
        })
        .handle("macros:recorderPermissionStatus", |_, _| async move { Ok(json!({ "granted": true })) })
        .handle("macros:requestRecorderPermission", |_, _| async move { Ok(json!({ "prompted": false, "status": { "granted": true } })) })
        .handle("macros:openInputMonitoringSettings", |_, _| async move { Ok(Value::Null) })
        .handle("macros:policyForPlace", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            Ok(policy_for_place(&ctx.state, place).await)
        })
        .handle("macros:exportToFile", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(export_to_file(&ctx.state, &id).await)
        })
        .handle("macros:importFromFilePicker", |ctx, _| async move { Ok(import_from_picker(&ctx.state).await) })
        .handle("macros:confirmImport", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(confirm_import(&ctx.state, &id))
        })
        .handle("macros:cancelImport", |ctx, args| async move {
            let id: String = args.get(0)?;
            drop_staged(&ctx.state, &id);
            Ok(Value::Null)
        })
        .handle("macros:consumePendingImport", |ctx, _| async move { Ok(ctx.state.macros.pending_preview.lock().take()) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_macros() {
        let ok = json!({ "id": "abc", "name": "Jump", "defaultHotkey": "Control+J", "actions": [{ "type": "key-tap", "key": "space" }], "enabled": true });
        assert_eq!(validate_macro(&ok).unwrap()["version"], 1);
        let reserved = json!({ "id": "abc", "name": "x", "defaultHotkey": "Escape", "actions": [], "enabled": true });
        assert!(validate_macro(&reserved).is_err());
        let bad_delay = json!({ "id": "a", "name": "x", "defaultHotkey": null, "actions": [{ "type": "random-delay", "minMs": 5, "maxMs": 1 }], "enabled": true });
        assert!(validate_macro(&bad_delay).is_err());
    }

    #[test]
    fn converts_recordings() {
        let events = vec![
            RawEvent::Key { down: true, vk: 65, t_ms: 0.0 },
            RawEvent::Key { down: false, vk: 65, t_ms: 50.0 },
            RawEvent::Key { down: true, vk: 87, t_ms: 1000.0 },
            RawEvent::Key { down: false, vk: 87, t_ms: 1500.0 },
            RawEvent::Mouse { down: true, button: 0, t_ms: 1505.0 },
            RawEvent::Mouse { down: false, button: 0, t_ms: 1510.0 },
        ];
        let (actions, truncated, unmapped) = raw_events_to_actions(&events);
        assert_eq!((truncated, unmapped), (0, 0));
        assert_eq!(actions[0], json!({ "type": "key-tap", "key": "a" }));
        assert_eq!(actions[1], json!({ "type": "delay", "ms": 950 }));
        assert_eq!(actions[2], json!({ "type": "hold-key", "key": "w", "ms": 500 }));
        assert_eq!(actions[3], json!({ "type": "mouse-click", "button": "left" }));
        assert_eq!(dedupe_name("Jump", &HashSet::from(["jump".to_owned()])), "Jump (2)");
    }
}
