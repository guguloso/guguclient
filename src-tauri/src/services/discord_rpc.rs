//! Discord Rich Presence. Port of `DiscordRpcService` over Discord's local
//! IPC (`\\.\pipe\discord-ipc-N`, `@xhayper/discord-rpc` framing): shows the
//! launcher or the current game, follows BloxstrapRPC messages from game
//! logs, and reconnects every 30s while Discord is closed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::AppState;
use crate::util::now_millis;

const CLIENT_ID: &str = "1569564533830324315";
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const LAUNCHER_IMAGE: &str = "logo";
const GAME_IMAGE: &str = "playing";
const SMALL_IMAGE: &str = "logo2";
const FAILURE_LOG_INTERVAL_MS: u64 = 30_000;
const BLOXSTRAP_RATE_LIMIT_MS: u64 = 1000;
const BLOXSTRAP_THUMBNAIL_CACHE_SIZE: usize = 20;
const BLOXSTRAP_RESET_TOKEN: &str = "<reset>";
const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

type Writer = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>;

#[derive(Default)]
struct Inner {
    prevent_reconnect: bool,
    connected: bool,
    current: Option<Map<String, Value>>,
    original: Option<Map<String, Value>>,
    launch_ts: Option<u64>,
    in_game: bool,
    place_id: Option<u64>,
    queue: Vec<Value>,
    last_bloxstrap: u64,
    thumbnails: Vec<(u64, String)>,
    launch_data: Option<String>,
    revision: u64,
    game_info: HashMap<u64, (String, String)>,
    universes: HashMap<u64, u64>,
    last_failure_log: u64,
    fallback_nonce: Option<String>,
    activity_nonce: Option<String>,
}

#[derive(Default)]
pub struct DiscordRpc {
    inner: Mutex<Inner>,
    writer: tokio::sync::Mutex<Option<Writer>>,
    generation: AtomicU64,
}

fn enabled(state: &AppState) -> bool {
    state.settings.get_bool("enable-discord-rich-presence")
}

/// `startStop`.
pub fn start_stop(state: &Arc<AppState>) {
    if enabled(state) {
        initialize(state);
    } else {
        shutdown(state);
    }
}

fn initialize(state: &Arc<AppState>) {
    {
        let mut inner = state.discord.inner.lock();
        if !enabled(state) {
            inner.prevent_reconnect = true;
            return;
        }
        if state.discord.generation.load(Ordering::SeqCst) % 2 == 1 {
            return; // a client loop is already running
        }
        inner.prevent_reconnect = false;
    }
    let generation = state.discord.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else { return };
            if state.discord.generation.load(Ordering::SeqCst) != generation || state.discord.inner.lock().prevent_reconnect {
                return;
            }
            match connect().await {
                Ok(pipe) => run_session(&state, pipe, generation).await,
                Err(error) => tracing::debug!("Discord RPC initial connection failed (Discord may not be running): {error}"),
            }
            if state.discord.inner.lock().prevent_reconnect || state.discord.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            drop(state);
            tokio::time::sleep(RETRY_INTERVAL).await;
            tracing::debug!("Discord RPC attempting reconnect...");
        }
    });
}

/// `shutdown`: clears the presence and closes the pipe.
pub fn shutdown(state: &AppState) {
    {
        let mut inner = state.discord.inner.lock();
        inner.prevent_reconnect = true;
        inner.connected = false;
    }
    if state.discord.generation.load(Ordering::SeqCst) % 2 == 1 {
        state.discord.generation.fetch_add(1, Ordering::SeqCst);
    }
    let Some(state) = arc_of(state) else { return };
    tauri::async_runtime::spawn(async move {
        let mut writer = state.discord.writer.lock().await;
        if let Some(w) = writer.as_mut() {
            let _ = write_frame(w, OP_FRAME, &json!({ "cmd": "SET_ACTIVITY", "args": { "pid": std::process::id() }, "nonce": uuid::Uuid::new_v4().to_string() })).await;
            let _ = w.shutdown().await;
        }
        *writer = None;
    });
}

fn arc_of(state: &AppState) -> Option<Arc<AppState>> {
    use tauri::Manager;
    state.app.try_state::<Arc<AppState>>().map(|s| s.inner().clone())
}

async fn connect() -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "Could not connect");
    for i in 0..10 {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(format!(r"\\.\pipe\discord-ipc-{i}")) {
            Ok(pipe) => return Ok(pipe),
            Err(error) => last = error,
        }
    }
    Err(last)
}

async fn write_frame(w: &mut Writer, op: u32, payload: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(payload)?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<(u32, Value)> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header).await?;
    let op = u32::from_le_bytes(header[..4].try_into().unwrap_or_default());
    let len = u32::from_le_bytes(header[4..].try_into().unwrap_or_default()) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// One connected session: handshake, READY, then responses until the pipe
/// closes (`disconnected`).
async fn run_session(state: &Arc<AppState>, pipe: tokio::net::windows::named_pipe::NamedPipeClient, generation: u64) {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    if let Err(error) = write_frame(&mut writer, OP_HANDSHAKE, &json!({ "v": 1, "client_id": CLIENT_ID })).await {
        tracing::debug!("Discord RPC initial connection failed (Discord may not be running): {error}");
        return;
    }
    *state.discord.writer.lock().await = Some(writer);
    loop {
        let (op, message) = match read_frame(&mut reader).await {
            Ok(frame) => frame,
            Err(error) => {
                tracing::debug!("Discord RPC socket error (Discord likely closed the pipe): {error}");
                break;
            }
        };
        if state.discord.generation.load(Ordering::SeqCst) != generation {
            break;
        }
        if op != OP_FRAME {
            if op == 2 {
                break; // CLOSE
            }
            continue;
        }
        if message["evt"] == "READY" {
            let was_in_game = {
                let mut inner = state.discord.inner.lock();
                inner.connected = true;
                inner.in_game
            };
            tracing::info!("Discord RPC connected");
            if !was_in_game {
                set_launcher_activity(&mut state.discord.inner.lock());
            }
            apply_activity(state);
        } else if message["evt"] == "ERROR" {
            on_request_error(state, &message);
        }
    }
    state.discord.inner.lock().connected = false;
    *state.discord.writer.lock().await = None;
    tracing::info!("Discord RPC disconnected");
}

fn log_failure(inner: &mut Inner, message: &str, detail: &Value) {
    let now = now_millis();
    if now - inner.last_failure_log < FAILURE_LOG_INTERVAL_MS {
        return;
    }
    inner.last_failure_log = now;
    tracing::debug!("{message}: {detail}");
}

fn on_request_error(state: &Arc<AppState>, message: &Value) {
    let nonce = message["nonce"].as_str().map(str::to_owned);
    let fallback = {
        let mut inner = state.discord.inner.lock();
        if nonce.is_some() && nonce == inner.fallback_nonce {
            log_failure(&mut inner, "Discord RPC failed to set minimal fallback activity", &message["data"]);
            return;
        }
        if nonce.is_none() || nonce != inner.activity_nonce {
            return;
        }
        log_failure(&mut inner, "Discord RPC failed to set activity, applying minimal fallback", &message["data"]);
        if !inner.connected {
            return;
        }
        let ts = inner.launch_ts.unwrap_or_else(now_millis);
        let mut fallback = Map::new();
        if inner.in_game {
            fallback.insert("details".into(), json!("Playing Gugu Client for Roblox"));
            fallback.insert("startTimestamp".into(), json!(ts));
        } else {
            fallback.insert("details".into(), json!("Active in Launcher"));
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        inner.fallback_nonce = Some(nonce.clone());
        (fallback, nonce)
    };
    send_activity(state, fallback.0, fallback.1);
}

/// `ClientUser.setActivity` formatting.
fn format_activity(a: &Map<String, Value>) -> Value {
    let mut out = json!({ "type": 0, "created_at": now_millis(), "instance": false });
    let text = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(d) = text("details") {
        out["details"] = json!(d);
    }
    if let Some(s) = text("state") {
        out["state"] = json!(s);
    }
    let start = a.get("startTimestamp").and_then(Value::as_u64).filter(|t| *t > 0);
    let end = a.get("endTimestamp").and_then(Value::as_u64).filter(|t| *t > 0);
    if start.is_some() || end.is_some() {
        let mut ts = Map::new();
        if let Some(s) = start {
            ts.insert("start".into(), json!(s));
        }
        if let Some(e) = end {
            ts.insert("end".into(), json!(e));
        }
        out["timestamps"] = Value::Object(ts);
    }
    let mut assets = Map::new();
    for (key, field) in [("largeImageKey", "large_image"), ("smallImageKey", "small_image"), ("largeImageText", "large_text"), ("smallImageText", "small_text")] {
        if let Some(v) = text(key) {
            assets.insert(field.into(), json!(v));
        }
    }
    if !assets.is_empty() {
        out["assets"] = Value::Object(assets);
    }
    if let Some(buttons) = a.get("buttons").and_then(Value::as_array).filter(|b| !b.is_empty()) {
        out["buttons"] = Value::Array(buttons.clone());
    }
    out
}

fn send_activity(state: &Arc<AppState>, activity: Map<String, Value>, nonce: String) {
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        let payload = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": std::process::id(), "activity": format_activity(&activity) }, "nonce": nonce });
        let mut writer = state.discord.writer.lock().await;
        if let Some(w) = writer.as_mut() {
            if let Err(error) = write_frame(w, OP_FRAME, &payload).await {
                tracing::debug!("Discord RPC failed to set activity: {error}");
            }
        }
    });
}

/// `applyActivity`.
fn apply_activity(state: &Arc<AppState>) {
    let (activity, nonce) = {
        let mut inner = state.discord.inner.lock();
        let Some(current) = inner.current.clone().filter(|_| inner.connected) else { return };
        let nonce = uuid::Uuid::new_v4().to_string();
        inner.activity_nonce = Some(nonce.clone());
        (current, nonce)
    };
    send_activity(state, activity, nonce);
}

fn set_launcher_activity(inner: &mut Inner) {
    let mut a = Map::new();
    a.insert("details".into(), json!("Active in Launcher"));
    a.insert("largeImageKey".into(), json!(LAUNCHER_IMAGE));
    a.insert("largeImageText".into(), json!("Gugu For Roblox"));
    inner.current = Some(a);
}

fn game_buttons(inner: &Inner) -> Option<Value> {
    let place = inner.place_id.filter(|p| *p > 0)?;
    let mut buttons = vec![json!({ "label": "See game page", "url": format!("https://www.roblox.com/games/{place}") })];
    if let Some(data) = &inner.launch_data {
        buttons.push(json!({ "label": "Join Server", "url": format!("roblox://experiences/start?placeId={place}&launchData={}", urlencoding::encode(data)) }));
    }
    Some(Value::Array(buttons))
}

async fn resolve_game_info(state: &AppState, place: u64) -> Option<(String, String)> {
    if let Some(info) = state.discord.inner.lock().game_info.get(&place).cloned() {
        return Some(info);
    }
    let cached = state.discord.inner.lock().universes.get(&place).copied();
    let universe = match cached {
        Some(u) => u,
        None => {
            let u = crate::services::games::universe_by_place(state, place).await?;
            state.discord.inner.lock().universes.insert(place, u);
            u
        }
    };
    let details: Value = state.roblox.get(&format!("https://games.roblox.com/v1/games?universeIds={universe}")).await.ok()?;
    let game = details.pointer("/data/0")?;
    let info = (
        game.get("name").and_then(Value::as_str).unwrap_or("Unknown Game").to_owned(),
        game.pointer("/creator/name").and_then(Value::as_str).unwrap_or("Unknown").to_owned(),
    );
    state.discord.inner.lock().game_info.insert(place, info.clone());
    Some(info)
}

/// `updateGameActivity`: the game being played (or the instance name).
pub async fn update_game_activity(state: &Arc<AppState>, instance_id: &str, place: u64) {
    let launch_ts = now_millis();
    {
        let mut inner = state.discord.inner.lock();
        inner.in_game = true;
        inner.launch_ts = Some(launch_ts);
        inner.place_id = Some(place);
        inner.revision += 1;
    }
    let info = if place > 0 { resolve_game_info(state, place).await } else { None };
    if place > 0 && info.is_none() {
        tracing::warn!("Failed to resolve game info for Discord RPC");
    }
    let fallback_name = state.instances.get(&state.paths.base, instance_id).and_then(|i| i.get("name").and_then(Value::as_str).map(str::to_owned));
    {
        let mut inner = state.discord.inner.lock();
        let mut a = Map::new();
        match info {
            Some((name, creator)) => {
                a.insert("details".into(), json!(name));
                a.insert("state".into(), json!(format!("by {creator}")));
                a.insert("startTimestamp".into(), json!(launch_ts));
                a.insert("largeImageKey".into(), json!(GAME_IMAGE));
                a.insert("largeImageText".into(), json!(name));
                a.insert("smallImageKey".into(), json!(SMALL_IMAGE));
                a.insert("smallImageText".into(), json!("Gugu For Roblox"));
                if let Some(buttons) = game_buttons(&inner) {
                    a.insert("buttons".into(), buttons);
                }
            }
            None => {
                a.insert("details".into(), json!(fallback_name.unwrap_or_else(|| "Unknown Instance".into())));
                a.insert("state".into(), json!("Playing Gugu For Roblox"));
                a.insert("startTimestamp".into(), json!(inner.launch_ts.unwrap_or_else(now_millis)));
                a.insert("largeImageKey".into(), json!(GAME_IMAGE));
                a.insert("largeImageText".into(), json!("Gugu For Roblox"));
                a.insert("smallImageKey".into(), json!(SMALL_IMAGE));
                a.insert("smallImageText".into(), json!("Gugu For Roblox"));
            }
        }
        inner.original = Some(a.clone());
        inner.current = Some(a);
        let queued = std::mem::take(&mut inner.queue);
        for message in queued {
            apply_bloxstrap(state, &mut inner, &message);
        }
    }
    apply_activity(state);
}

/// `clearGameActivity`: back to the launcher presence.
pub fn clear_game_activity(state: &Arc<AppState>) {
    {
        let mut inner = state.discord.inner.lock();
        inner.in_game = false;
        inner.launch_ts = None;
        inner.place_id = None;
        inner.original = None;
        inner.launch_data = None;
        inner.revision += 1;
        inner.queue.clear();
        set_launcher_activity(&mut inner);
    }
    apply_activity(state);
}

/// `processBloxstrapRPCMessage` (already validated by the activity tracker).
pub fn process_bloxstrap_message(state: &Arc<AppState>, message: Value) {
    {
        let mut inner = state.discord.inner.lock();
        if inner.current.is_none() || inner.original.is_none() {
            inner.queue.push(message);
            tracing::debug!("Discord RPC: Bloxstrap message queued (presence not ready)");
            return;
        }
        let now = now_millis();
        if now - inner.last_bloxstrap < BLOXSTRAP_RATE_LIMIT_MS {
            tracing::debug!("Discord RPC: Bloxstrap RPC rate-limited, dropping message");
            return;
        }
        inner.last_bloxstrap = now;
        apply_bloxstrap(state, &mut inner, &message);
    }
    apply_activity(state);
}

fn apply_bloxstrap(state: &Arc<AppState>, inner: &mut Inner, message: &Value) {
    if message["command"] == "SetLaunchData" {
        inner.launch_data = message["data"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let buttons = game_buttons(inner);
        if let Some(current) = inner.current.as_mut() {
            match buttons {
                Some(b) => current.insert("buttons".into(), b),
                None => current.remove("buttons"),
            };
        }
        return;
    }
    let data = &message["data"];
    let (Some(current), Some(original)) = (inner.current.as_mut(), inner.original.clone()) else { return };
    for key in ["details", "state"] {
        if let Some(v) = data.get(key).and_then(Value::as_str) {
            if v == BLOXSTRAP_RESET_TOKEN {
                match original.get(key) {
                    Some(o) => current.insert(key.into(), o.clone()),
                    None => current.remove(key),
                };
            } else if v.is_empty() {
                current.remove(key);
            } else {
                current.insert(key.into(), json!(v));
            }
        }
    }
    for (field, key) in [("timeStart", "startTimestamp"), ("timeEnd", "endTimestamp")] {
        match data.get(field).and_then(Value::as_u64) {
            Some(0) => {
                current.remove(key);
            }
            Some(t) => {
                current.insert(key.into(), json!(t * 1000));
            }
            None => {}
        }
    }
    let mut fetches = Vec::new();
    for (slot, image_key, text_key) in [("smallImage", "smallImageKey", "smallImageText"), ("largeImage", "largeImageKey", "largeImageText")] {
        let Some(image) = data.get(slot).filter(|i| i.is_object()) else { continue };
        if image.get("clear") == Some(&json!(true)) {
            current.remove(image_key);
            continue;
        }
        if image.get("reset") == Some(&json!(true)) {
            for k in [image_key, text_key] {
                match original.get(k) {
                    Some(o) => current.insert(k.into(), o.clone()),
                    None => current.remove(k),
                };
            }
            continue;
        }
        let asset = match image.get("assetId") {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.strip_prefix("rbxassetid://").filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())).and_then(|d| d.parse().ok()).filter(|id| *id > 0),
            _ => None,
        };
        if let Some(asset) = asset {
            match inner.thumbnails.iter().find(|(id, _)| *id == asset) {
                Some((_, url)) => {
                    current.insert(image_key.into(), json!(url));
                }
                None => fetches.push((asset, image_key)),
            }
        }
        if let Some(text) = image.get("hoverText").and_then(Value::as_str) {
            current.insert(text_key.into(), json!(text));
        }
    }
    let revision = inner.revision;
    for (asset, image_key) in fetches {
        let state = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            let url = format!("https://thumbnails.roblox.com/v1/assets?assetIds={asset}&size=512x512&format=Png");
            let Ok(r) = state.roblox.get::<Value>(&url).await else {
                tracing::debug!("Discord RPC: Failed to fetch asset thumbnail for {asset}");
                return;
            };
            let entry = r.pointer("/data/0");
            let Some(image) = entry.filter(|e| e["state"] == "Completed").and_then(|e| e["imageUrl"].as_str()).filter(|u| !u.is_empty()).map(str::to_owned) else { return };
            {
                let mut inner = state.discord.inner.lock();
                if inner.current.is_none() || inner.revision != revision {
                    return;
                }
                if inner.thumbnails.len() >= BLOXSTRAP_THUMBNAIL_CACHE_SIZE {
                    inner.thumbnails.remove(0);
                }
                inner.thumbnails.push((asset, image.clone()));
                if let Some(current) = inner.current.as_mut() {
                    current.insert(image_key.into(), json!(image));
                }
            }
            apply_activity(&state);
        });
    }
}
