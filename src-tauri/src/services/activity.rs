//! Game session tracking from Roblox client logs (`activity:*`). Port of
//! `ActivityTrackingService` and `ActivityTrackingStore`: joins, teleports,
//! server switches and leaves become per-instance history persisted in
//! `instances/<id>/activity.json`, and feed Discord presence, the
//! close-to-launcher rule and post-game notifications.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};

use crate::ipc::Router;
use crate::services::instances::parse_iso_millis;
use crate::services::roblox_layout::product_paths;
use crate::services::running::LogEntry;
use crate::state::AppState;
use crate::store::JsonStore;
use crate::util::{iso8601_from_millis, now_millis};

const MAX_HISTORY_ENTRIES: usize = 100;
const DISCONNECT_GRACE: Duration = Duration::from_millis(5000);
const UNKNOWN_INSTANCE: &str = "Unknown Instance";

const BLOXSTRAP_RPC_ENTRY: &str = "[FLog::Output] [BloxstrapRPC]";
const GAME_JOINING_ENTRY: &str = "[FLog::Output] ! Joining game";
const GAME_JOINING_PRIVATE_SERVER_ENTRY: &str = "[FLog::GameJoinUtil] GameJoinUtil::joinGamePostPrivateServer";
const GAME_JOINING_RESERVED_SERVER_ENTRY: &str = "[FLog::GameJoinUtil] GameJoinUtil::initiateTeleportToReservedServer";
const GAME_JOINING_UNIVERSE_ENTRY: &str = "[FLog::GameJoinLoadTime] Report game_join_loadtime:";
const GAME_JOINING_UDMUX_ENTRY: &str = "[FLog::Network] UDMUX Address = ";
const GAME_JOINED_ENTRY: &str = "[FLog::Network] serverId:";
const GAME_DISCONNECTED_ENTRY: &str = "[FLog::Network] Time to disconnect replication data:";
const GAME_LEAVING_ENTRY: &str = "[FLog::SingleSurfaceApp] leaveUGCGameInternal";
const RETURN_TO_LUA_APP_ENTRY: &str = "[FLog::SingleSurfaceApp] returnToLuaApp";
const GAME_TELEPORTING_ENTRY_A: &str = "[FLog::GameJoinUtil] GameJoinUtil::initiateTeleportToPlace";
const GAME_TELEPORTING_ENTRY_B: &str = "[FLog::SingleSurfaceApp] initiateTeleport";

static JOINING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)! Joining game '([0-9a-f-]{36})' place ([0-9]+) at ([0-9.]+)").expect("regex"));
static PRIVATE_SERVER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)"accessCode":"([0-9a-f-]{36})""#).expect("regex"));
static UNIVERSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)universeid:([0-9]+).*userid:([0-9]+)").expect("regex"));
static UDMUX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"UDMUX Address = ([0-9.]+), Port = [0-9]+ \| RCC Server Address = ([0-9.]+), Port = [0-9]+").expect("regex"));
static JOINED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"serverId: ([0-9.]+)\|[0-9]+").expect("regex"));
static BLOXSTRAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[BloxstrapRPC\] (.*)").expect("regex"));

#[derive(Clone, Default)]
struct Pending {
    place_id: u64,
    job_id: String,
    access_code: String,
    link_code: String,
    reserved_code: String,
    machine: String,
    universe: Option<u64>,
    user: Option<u64>,
    server_type: String,
    is_teleport: bool,
    joined_at: Option<u64>,
}

impl Pending {
    fn empty() -> Self {
        Self { server_type: "public".into(), ..Self::default() }
    }
}

#[derive(Clone)]
struct History {
    place_id: u64,
    job_id: String,
    machine: String,
    server_type: String,
    is_teleport: bool,
    access_code: Option<String>,
    link_code: Option<String>,
    reserved_code: Option<String>,
    universe: Option<u64>,
    user: Option<u64>,
    joined_at: u64,
    left_at: u64,
    duration_ms: u64,
}

struct Tracking {
    name: String,
    listener: Option<u64>,
    dedupe: HashMap<String, usize>,
    current: Pending,
    in_game: bool,
    teleport_marker: bool,
    reserved_marker: bool,
    history: Vec<History>,
    disconnect_generation: u64,
    disconnect_pending: bool,
}

#[derive(Default)]
pub struct ActivityState {
    tracking: Mutex<HashMap<String, Tracking>>,
    retained: Mutex<HashMap<String, (String, Vec<History>)>>,
}

/// Side effects collected under the lock and run after it is released.
enum Effect {
    Changed,
    Discord,
    SessionEnded(Value),
    Bloxstrap(Value),
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

fn put_opt(out: &mut Value, key: &str, value: &Option<impl serde::Serialize>) {
    if let Some(v) = value {
        out[key] = json!(v);
    }
}

/// `serializeHistorySessionSnapshot`.
fn history_json(id: &str, name: &str, h: &History) -> Value {
    let mut out = json!({ "instanceId": id, "instanceName": name, "placeId": h.place_id, "jobId": h.job_id, "machineAddress": h.machine, "serverType": h.server_type, "isTeleport": h.is_teleport });
    put_opt(&mut out, "accessCode", &h.access_code);
    put_opt(&mut out, "linkCode", &h.link_code);
    put_opt(&mut out, "reservedServerAccessCode", &h.reserved_code);
    put_opt(&mut out, "universeId", &h.universe);
    put_opt(&mut out, "userId", &h.user);
    out["joinedAt"] = json!(iso8601_from_millis(h.joined_at));
    out["leftAt"] = json!(iso8601_from_millis(h.left_at));
    out["durationMs"] = json!(h.duration_ms);
    out
}

/// The persisted shape (`{...entry, joinedAt, leftAt}`).
fn persisted_json(id: &str, h: &History) -> Value {
    let mut out = json!({ "instanceId": id, "placeId": h.place_id, "jobId": h.job_id, "machineAddress": h.machine, "serverType": h.server_type, "isTeleport": h.is_teleport });
    put_opt(&mut out, "accessCode", &h.access_code);
    put_opt(&mut out, "linkCode", &h.link_code);
    put_opt(&mut out, "reservedServerAccessCode", &h.reserved_code);
    put_opt(&mut out, "universeId", &h.universe);
    put_opt(&mut out, "userId", &h.user);
    out["joinedAt"] = json!(iso8601_from_millis(h.joined_at));
    out["leftAt"] = json!(iso8601_from_millis(h.left_at));
    out["durationMs"] = json!(h.duration_ms);
    out
}

/// `PERSISTED_HISTORY_ENTRY_SCHEMA`.
fn parse_persisted(v: &Value) -> Option<History> {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let opt_text = |k: &str| match v.get(k) {
        None => Some(None),
        Some(Value::String(s)) => Some(non_empty(s)),
        _ => None,
    };
    let opt_num = |k: &str| match v.get(k) {
        None => Some(None),
        Some(n) if n.is_number() => Some(n.as_f64().map(|f| f as u64)),
        _ => None,
    };
    let server_type = text("serverType").filter(|s| matches!(s.as_str(), "public" | "private" | "reserved"))?;
    let duration = v.get("durationMs").and_then(Value::as_f64).filter(|d| *d >= 0.0)?;
    Some(History {
        place_id: v.get("placeId").and_then(Value::as_f64)? as u64,
        job_id: text("jobId")?,
        machine: text("machineAddress")?,
        server_type,
        is_teleport: v.get("isTeleport").and_then(Value::as_bool)?,
        access_code: opt_text("accessCode")?,
        link_code: opt_text("linkCode")?,
        reserved_code: opt_text("reservedServerAccessCode")?,
        universe: opt_num("universeId")?,
        user: opt_num("userId")?,
        joined_at: parse_iso_millis(&text("joinedAt")?)?,
        left_at: parse_iso_millis(&text("leftAt")?)?,
        duration_ms: duration as u64,
    })
}

impl ActivityState {
    /// `loadRetainedHistories`: every Player instance's saved history.
    pub fn load(base: &Path) -> Self {
        let dir = product_paths(base, "player").instances_dir;
        let mut retained = HashMap::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path().join("activity.json");
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(raw) = serde_json::from_slice::<Value>(&bytes) else { continue };
            if raw.get("instanceName").is_none() && raw.get("history").is_none() {
                continue;
            }
            let name = raw.get("instanceName").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(UNKNOWN_INSTANCE).to_owned();
            let mut history: Vec<History> = raw.get("history").and_then(Value::as_array).into_iter().flatten().filter_map(parse_persisted).collect();
            history.sort_by_key(|h| std::cmp::Reverse(h.left_at));
            history.truncate(MAX_HISTORY_ENTRIES);
            if !history.is_empty() {
                retained.insert(id, (name, history));
            }
        }
        Self { tracking: Mutex::new(HashMap::new()), retained: Mutex::new(retained) }
    }
}

fn persist(state: &AppState, id: &str, name: &str, history: &[History]) {
    let Ok(dir) = crate::services::roblox_layout::instance_config_dir(&state.paths.base, id, "player") else { return };
    let result = JsonStore::open(&dir, "activity").and_then(|store| {
        store.update(|root| {
            root["instanceName"] = json!(name);
            root["history"] = Value::Array(history.iter().map(|h| persisted_json(id, h)).collect());
        })
    });
    if let Err(error) = result {
        tracing::warn!("[Activity] Failed to persist history for instance {id}: {error}");
    }
}

fn enabled(state: &AppState) -> bool {
    state.settings.get_bool("enable-activity-tracking")
}

/// `getOverview`.
fn overview(state: &AppState) -> Value {
    let running: Vec<String> = state.running.ids();
    let mut recent: Vec<Value> = state
        .activity
        .retained
        .lock()
        .iter()
        .filter(|(id, (_, h))| !h.is_empty() && !running.contains(id))
        .map(|(id, (name, h))| json!({ "instanceId": id, "instanceName": name, "lastSession": history_json(id, name, &h[0]), "endedSessionCount": h.len() }))
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r["lastSession"]["leftAt"].as_str().and_then(parse_iso_millis).unwrap_or(0)));
    json!({ "recentInstances": recent })
}

/// `getRecentSessions`.
fn recent_sessions(state: &AppState, limit: usize) -> Vec<Value> {
    let running: Vec<String> = state.running.ids();
    let mut sessions: Vec<(u64, Value)> = Vec::new();
    for (id, (name, history)) in state.activity.retained.lock().iter() {
        if running.contains(id) {
            continue;
        }
        sessions.extend(history.iter().map(|h| (h.left_at, history_json(id, name, h))));
    }
    sessions.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    sessions.into_iter().take(limit).map(|(_, v)| v).collect()
}

/// `getInstanceTimeline`.
fn timeline(state: &AppState, id: &str) -> Value {
    let running = state.running.info(id);
    let tracking = state.activity.tracking.lock();
    let t = tracking.get(id);
    let retained = state.activity.retained.lock();
    let r = retained.get(id);
    if running.is_none() && t.is_none() && r.is_none() {
        return Value::Null;
    }
    let name = t.map(|t| t.name.clone()).or_else(|| running.as_ref().map(|(n, _)| n.clone())).or_else(|| r.map(|r| r.0.clone())).unwrap_or_else(|| UNKNOWN_INSTANCE.into());
    let current = t.filter(|t| t.in_game && t.current.joined_at.is_some()).map(|t| {
        let c = &t.current;
        let mut out = json!({ "instanceId": id, "instanceName": name, "placeId": c.place_id, "jobId": c.job_id, "machineAddress": c.machine, "serverType": c.server_type, "isTeleport": c.is_teleport });
        put_opt(&mut out, "accessCode", &non_empty(&c.access_code));
        put_opt(&mut out, "linkCode", &non_empty(&c.link_code));
        put_opt(&mut out, "reservedServerAccessCode", &non_empty(&c.reserved_code));
        put_opt(&mut out, "universeId", &c.universe);
        put_opt(&mut out, "userId", &c.user);
        out["joinedAt"] = json!(iso8601_from_millis(c.joined_at.unwrap_or_else(now_millis)));
        out["leftAt"] = Value::Null;
        out["durationMs"] = Value::Null;
        out
    });
    let history = t.map(|t| t.history.clone()).or_else(|| r.map(|r| r.1.clone())).unwrap_or_default();
    json!({
        "instanceId": id,
        "instanceName": name,
        "isRunning": running.is_some(),
        "current": current,
        "recentSessions": history.iter().map(|h| history_json(id, &name, h)).collect::<Vec<_>>(),
    })
}

/// `finalizeCurrentActivity`.
fn finalize(state: &AppState, id: &str, t: &mut Tracking, reason: &str, replaying: bool, effects: &mut Vec<Effect>) {
    t.disconnect_pending = false;
    t.disconnect_generation += 1;
    let left_at = now_millis();
    let joined_at = t.current.joined_at.unwrap_or(left_at);
    let c = &t.current;
    let entry = History {
        place_id: c.place_id,
        job_id: c.job_id.clone(),
        machine: c.machine.clone(),
        server_type: c.server_type.clone(),
        is_teleport: c.is_teleport,
        access_code: non_empty(&c.access_code),
        link_code: non_empty(&c.link_code),
        reserved_code: non_empty(&c.reserved_code),
        universe: c.universe,
        user: c.user,
        joined_at,
        left_at,
        duration_ms: left_at.saturating_sub(joined_at),
    };
    t.history.insert(0, entry.clone());
    t.history.truncate(MAX_HISTORY_ENTRIES);
    state.activity.retained.lock().insert(id.to_owned(), (t.name.clone(), t.history.clone()));
    persist(state, id, &t.name, &t.history);
    tracing::info!("[Activity] {id}: left placeId={} jobId={} durationMs={} reason={reason}", entry.place_id, entry.job_id, entry.duration_ms);
    t.current = Pending::empty();
    t.in_game = false;
    effects.push(Effect::Discord);
    effects.push(Effect::Changed);
    if !replaying {
        let mut payload = history_json(id, &t.name, &entry);
        payload["reason"] = json!(reason);
        effects.push(Effect::SessionEnded(payload));
    }
}

fn is_join_start(message: &str) -> bool {
    message.starts_with(GAME_JOINING_ENTRY) || message.starts_with(GAME_JOINING_PRIVATE_SERVER_ENTRY)
}

/// `processPreJoinLogEntry`.
fn pre_join(id: &str, t: &mut Tracking, message: &str) {
    if message.starts_with(GAME_JOINING_PRIVATE_SERVER_ENTRY) {
        t.current.server_type = "private".into();
        if let Some(code) = PRIVATE_SERVER.captures(message).and_then(|c| c.get(1)) {
            t.current.access_code = code.as_str().to_owned();
        }
        return;
    }
    if !message.starts_with(GAME_JOINING_ENTRY) {
        return;
    }
    let Some(caps) = JOINING.captures(message) else {
        tracing::debug!("[Activity] {id}: unable to parse join line");
        return;
    };
    t.current.place_id = caps[2].parse().unwrap_or(0);
    t.current.job_id = caps[1].to_owned();
    t.current.machine = caps[3].to_owned();
    if t.teleport_marker {
        t.current.is_teleport = true;
        t.teleport_marker = false;
    }
    if t.reserved_marker {
        t.current.server_type = "reserved".into();
        t.reserved_marker = false;
    }
    tracing::info!("[Activity] {id}: joining placeId={} jobId={}", t.current.place_id, t.current.job_id);
}

/// `readLogEntry`: the session state machine.
fn read_log(state: &Arc<AppState>, id: &str, t: &mut Tracking, message: &str, replaying: bool, effects: &mut Vec<Effect>) {
    if message.starts_with(GAME_LEAVING_ENTRY) || message.starts_with(RETURN_TO_LUA_APP_ENTRY) {
        if t.current.place_id != 0 && t.in_game {
            finalize(state, id, t, "leave", replaying, effects);
        } else if t.current.place_id != 0 {
            tracing::debug!("[Activity] {id}: join cancelled before completion");
            t.current = Pending::empty();
            t.teleport_marker = false;
            t.reserved_marker = false;
        }
        return;
    }
    if !t.in_game && t.current.place_id == 0 {
        pre_join(id, t, message);
        return;
    }
    if !t.in_game {
        if message.starts_with(GAME_JOINING_UNIVERSE_ENTRY) {
            if let Some(caps) = UNIVERSE.captures(message) {
                t.current.universe = caps[1].parse().ok();
                t.current.user = caps[2].parse().ok();
            }
            return;
        }
        if message.starts_with(GAME_JOINING_UDMUX_ENTRY) {
            if let Some(caps) = UDMUX.captures(message) {
                if caps[2] == t.current.machine {
                    t.current.machine = caps[1].to_owned();
                }
            }
            return;
        }
        if !message.starts_with(GAME_JOINED_ENTRY) {
            return;
        }
        let Some(caps) = JOINED.captures(message) else { return };
        if caps[1] != t.current.machine {
            tracing::debug!("[Activity] {id}: joined server mismatch (expected={}, actual={})", t.current.machine, &caps[1]);
            return;
        }
        t.in_game = true;
        t.current.joined_at = Some(now_millis());
        tracing::info!("[Activity] {id}: joined placeId={} jobId={} serverType={}", t.current.place_id, t.current.job_id, t.current.server_type);
        effects.push(Effect::Discord);
        effects.push(Effect::Changed);
        return;
    }
    if is_join_start(message) {
        let reason = if t.teleport_marker { "teleport" } else { "server_switch" };
        finalize(state, id, t, reason, replaying, effects);
        pre_join(id, t, message);
        return;
    }
    if message.starts_with(GAME_DISCONNECTED_ENTRY) {
        t.disconnect_generation += 1;
        t.disconnect_pending = true;
        let generation = t.disconnect_generation;
        let weak = Arc::downgrade(state);
        let owner = id.to_owned();
        tauri::async_runtime::spawn(async move {
            let id = owner;
            tokio::time::sleep(DISCONNECT_GRACE).await;
            let Some(state) = weak.upgrade() else { return };
            let mut effects = Vec::new();
            {
                let mut tracking = state.activity.tracking.lock();
                let Some(t) = tracking.get_mut(&id) else { return };
                if !t.disconnect_pending || t.disconnect_generation != generation {
                    return;
                }
                t.disconnect_pending = false;
                if t.in_game && t.current.joined_at.is_some() {
                    finalize(&state, &id, t, "disconnect", false, &mut effects);
                }
            }
            run_effects(&state, effects);
        });
        tracing::debug!("[Activity] {id}: disconnect detected, waiting {}ms for possible server transition", DISCONNECT_GRACE.as_millis());
        return;
    }
    if message.starts_with(GAME_TELEPORTING_ENTRY_A) || message.starts_with(GAME_TELEPORTING_ENTRY_B) {
        t.teleport_marker = true;
        tracing::debug!("[Activity] {id}: teleport initiated for {}/{}", t.current.place_id, t.current.job_id);
        return;
    }
    if message.starts_with(GAME_JOINING_RESERVED_SERVER_ENTRY) {
        t.teleport_marker = true;
        t.reserved_marker = true;
    }
    if message.starts_with(BLOXSTRAP_RPC_ENTRY) {
        if let Some(parsed) = BLOXSTRAP.captures(message).and_then(|c| serde_json::from_str::<Value>(&c[1]).ok()) {
            match validate_bloxstrap(&parsed) {
                Some(valid) => {
                    tracing::info!("[Activity] Received BloxstrapRPC command: {}", valid["command"].as_str().unwrap_or(""));
                    effects.push(Effect::Bloxstrap(valid));
                }
                None => tracing::debug!("[Activity] Invalid BloxstrapRPC message"),
            }
        } else {
            tracing::debug!("[Activity] Failed to parse BloxstrapRPC JSON");
        }
    }
}

/// `bloxstrapRPCMessageSchema`.
fn validate_bloxstrap(v: &Value) -> Option<Value> {
    let image_ok = |i: Option<&Value>| match i {
        None => true,
        Some(i) => {
            i.is_object()
                && i.get("assetId").is_none_or(|a| a.is_string() || a.as_u64().is_some_and(|n| n > 0))
                && i.get("hoverText").is_none_or(Value::is_string)
                && i.get("clear").is_none_or(Value::is_boolean)
                && i.get("reset").is_none_or(Value::is_boolean)
        }
    };
    match v.get("command").and_then(Value::as_str)? {
        "SetLaunchData" => v.get("data").and_then(Value::as_str).filter(|d| d.chars().count() <= 200).map(|_| v.clone()),
        "SetRichPresence" => {
            let d = v.get("data").filter(|d| d.is_object())?;
            let text_ok = |k: &str| d.get(k).is_none_or(|t| t.as_str().is_some_and(|s| s.chars().count() <= 128));
            let time_ok = |k: &str| d.get(k).is_none_or(|t| t.is_u64());
            (text_ok("details") && text_ok("state") && time_ok("timeStart") && time_ok("timeEnd") && image_ok(d.get("smallImage")) && image_ok(d.get("largeImage")))
                .then(|| v.clone())
        }
        _ => None,
    }
}

/// `consumeLogEntry`: only file lines that carry an `[FLog::` message.
fn consume(state: &Arc<AppState>, id: &str, t: &mut Tracking, entry: &LogEntry, replaying: bool, effects: &mut Vec<Effect>) {
    if entry.source != "file" {
        return;
    }
    let message = entry.message.as_str();
    let Some(start) = message.find("[FLog::") else { return };
    read_log(state, id, t, &message[start..], replaying, effects);
}

fn dedupe_key(e: &LogEntry) -> String {
    format!("{}|{}|{}|{}", e.timestamp_ms, e.source, e.level, e.message)
}

/// `getMostRecentActiveState` → Discord presence.
fn update_discord(state: &Arc<AppState>) {
    let active = state
        .activity
        .tracking
        .lock()
        .iter()
        .filter(|(_, t)| t.in_game && t.current.place_id > 0 && t.current.joined_at.is_some())
        .max_by_key(|(_, t)| t.current.joined_at)
        .map(|(id, t)| (id.clone(), t.current.place_id));
    match active {
        Some((id, place)) => {
            let state = Arc::clone(state);
            tauri::async_runtime::spawn(async move { crate::services::discord_rpc::update_game_activity(&state, &id, place).await });
        }
        None => crate::services::discord_rpc::clear_game_activity(state),
    }
}

fn run_effects(state: &Arc<AppState>, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Changed => {
                state.events.emit("activity:changed", overview(state));
                crate::services::macros::on_activity_changed(state);
            }
            Effect::Discord => update_discord(state),
            Effect::Bloxstrap(message) => crate::services::discord_rpc::process_bloxstrap_message(state, message),
            Effect::SessionEnded(payload) => {
                crate::services::launch::on_session_ended(state, &payload);
                crate::services::launcher_notifications::on_session_ended(state, &payload);
                state.events.emit("activity:sessionEnded", payload);
            }
        }
    }
}

/// `buildActivityLaunchContext`.
pub fn launch_context(options: Option<&Value>) -> Option<Value> {
    let o = options?;
    match o.get("type").and_then(Value::as_str)? {
        "privateServer" => Some(json!({ "serverType": "private", "accessCode": o.get("accessCode") })),
        "linkCode" => Some(json!({ "serverType": "private", "linkCode": o.get("linkCode") })),
        "reservedServer" => Some(json!({ "serverType": "reserved", "reservedServerAccessCode": o.get("reservedServerAccessCode") })),
        _ => None,
    }
}

/// `startTracking`: replays the buffered log, then follows it live.
pub fn start_tracking(state: &Arc<AppState>, id: &str, context: Option<Value>) {
    let info = state.running.info(id);
    if info.as_ref().is_some_and(|(_, p)| p == "studio") || !enabled(state) {
        return;
    }
    let mut effects = Vec::new();
    {
        let mut tracking = state.activity.tracking.lock();
        if tracking.contains_key(id) {
            tracing::debug!("[Activity] Tracking already active for instance {id}");
            return;
        }
        let retained = state.activity.retained.lock().get(id).cloned();
        let mut t = Tracking {
            name: info.map(|(n, _)| n).or_else(|| retained.as_ref().map(|r| r.0.clone())).unwrap_or_else(|| UNKNOWN_INSTANCE.into()),
            listener: None,
            dedupe: HashMap::new(),
            current: Pending::empty(),
            in_game: false,
            teleport_marker: false,
            reserved_marker: false,
            history: retained.map(|r| r.1).unwrap_or_default(),
            disconnect_generation: 0,
            disconnect_pending: false,
        };
        if let Some(ctx) = &context {
            if let Some(s) = ctx.get("serverType").and_then(Value::as_str) {
                t.current.server_type = s.to_owned();
            }
            for (key, field) in [("accessCode", &mut t.current.access_code), ("linkCode", &mut t.current.link_code), ("reservedServerAccessCode", &mut t.current.reserved_code)] {
                if let Some(v) = ctx.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    *field = v.to_owned();
                }
            }
        }
        let weak = Arc::downgrade(state);
        let owner = id.to_owned();
        t.listener = state.running.subscribe(
            id,
            Arc::new(move |entry: &LogEntry| {
                let Some(state) = weak.upgrade() else { return };
                let mut effects = Vec::new();
                {
                    let mut tracking = state.activity.tracking.lock();
                    let Some(t) = tracking.get_mut(&owner) else { return };
                    if !t.dedupe.is_empty() {
                        let key = dedupe_key(entry);
                        match t.dedupe.get_mut(&key) {
                            Some(count) if *count > 0 => {
                                *count -= 1;
                                return;
                            }
                            _ => t.dedupe.clear(),
                        }
                    }
                    consume(&state, &owner, t, entry, false, &mut effects);
                }
                run_effects(&state, effects);
            }),
        );
        let buffered = state.running.log_entries(id);
        for entry in &buffered {
            *t.dedupe.entry(dedupe_key(entry)).or_default() += 1;
            consume(state, id, &mut t, entry, true, &mut effects);
        }
        tracking.insert(id.to_owned(), t);
        tracing::info!("[Activity] Started tracking for instance {id} (replayedLines={}, pendingLive=0)", buffered.len());
    }
    effects.retain(|e| !matches!(e, Effect::SessionEnded(_)));
    effects.push(Effect::Changed);
    run_effects(state, effects);
}

/// `stopTracking`: an in-progress session ends with `process_exit`.
pub fn stop_tracking(state: &Arc<AppState>, id: &str) {
    let mut effects = Vec::new();
    {
        let mut tracking = state.activity.tracking.lock();
        let Some(mut t) = tracking.remove(id) else { return };
        if let Some(token) = t.listener.take() {
            state.running.unsubscribe(id, token);
        }
        t.disconnect_pending = false;
        if t.in_game && t.current.joined_at.is_some() {
            finalize(state, id, &mut t, "process_exit", false, &mut effects);
        }
        tracing::info!("[Activity] Stopped tracking for instance {id} (historyEntries={})", t.history.len());
    }
    effects.push(Effect::Changed);
    run_effects(state, effects);
}

/// `startStop` (setting toggled).
pub fn start_stop(state: &Arc<AppState>) {
    if enabled(state) {
        for id in state.running.ids() {
            start_tracking(state, &id, None);
        }
        return;
    }
    let ids: Vec<String> = state.activity.tracking.lock().keys().cloned().collect();
    for id in ids {
        stop_tracking(state, &id);
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("activity:getOverview", |ctx, _| async move { Ok(overview(&ctx.state)) })
        .handle("activity:getRecentSessions", |ctx, args| async move {
            let limit: Option<usize> = args.opt(0)?;
            Ok(recent_sessions(&ctx.state, limit.unwrap_or(50)))
        })
        .handle("activity:getInstanceTimeline", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(timeline(&ctx.state, &id))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_patterns() {
        let caps = JOINING.captures("[FLog::Output] ! Joining game '0123abcd-0123-0123-0123-0123456789ab' place 189707 at 10.0.0.1").unwrap();
        assert_eq!(&caps[2], "189707");
        assert_eq!(&caps[3], "10.0.0.1");
        assert!(JOINED.is_match("[FLog::Network] serverId: 10.0.0.1|53640"));
        assert!(validate_bloxstrap(&json!({ "command": "SetLaunchData", "data": "x" })).is_some());
        assert!(validate_bloxstrap(&json!({ "command": "SetRichPresence", "data": { "timeStart": -1 } })).is_none());
    }
}

/// `getCurrentActivity`: `(placeId, universeId)` while the instance is in a game.
pub fn current_place(state: &AppState, id: &str) -> Option<(u64, Option<u64>)> {
    let tracking = state.activity.tracking.lock();
    let t = tracking.get(id).filter(|t| t.in_game && t.current.joined_at.is_some())?;
    Some((t.current.place_id, t.current.universe))
}
