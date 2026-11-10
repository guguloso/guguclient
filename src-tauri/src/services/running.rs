//! Running Roblox instances and their live client logs. Port of
//! `RunningInstancesService`: each Player instance tails the newest
//! `*_Player_*_last.log` written after it started (claimed so two instances
//! never share a file), parses Roblox's log format and fans entries out to
//! Mission Control and the activity tracker.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::services::roblox_layout::instance_data_dir;
use crate::state::AppState;
use crate::util::{iso8601_from_millis, now_millis};

const LOG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const RETAINED_INSTANCES_MAX: usize = 3;
const INSTANCES_CHANGED_CHANNEL: &str = "instances:runningChanged";
const LOG_ENTRY_CHANNEL: &str = "log:entry";
const IGNORED_LOG_MESSAGE_PREFIXES: [&str; 2] = [
    "[FLog::WndProcessCheck] waitForNewPlayerProcess new waiting for mutex result is 0X000102",
    "[FLog::WndProcessCheck] WindowsAppReloadTelemetry MutexWaitResultLoopUpdate",
];

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub timestamp_ms: u64,
    pub level: &'static str,
    pub message: String,
    pub source: &'static str,
}

impl LogEntry {
    pub fn to_json(&self, instance_id: &str) -> Value {
        json!({ "timestamp": iso8601_from_millis(self.timestamp_ms), "level": self.level, "message": self.message, "source": self.source, "instanceId": instance_id })
    }
}

pub type LogListener = Arc<dyn Fn(&LogEntry) + Send + Sync>;

struct Tracked {
    id: String,
    name: String,
    product: String,
    started_ms: u64,
    ended_ms: Option<u64>,
    entries: Vec<LogEntry>,
    current_log: Option<PathBuf>,
    position: u64,
    fragment: String,
    excluded: HashSet<PathBuf>,
}

#[derive(Default)]
pub struct RunningInstances {
    running: Mutex<Vec<Tracked>>,
    retained: Mutex<Vec<Tracked>>,
    claimed: Mutex<HashMap<PathBuf, String>>,
    listeners: Mutex<HashMap<String, Vec<(u64, LogListener)>>>,
    next_listener: AtomicU64,
}

/// `getSystemRobloxLogsDir` (Windows: `%LOCALAPPDATA%\Roblox\logs`).
pub fn system_logs_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Roblox").join("logs"))
}

/// `getInstanceLogsDir`.
fn instance_logs_dir(state: &AppState, id: &str, product: &str) -> Option<PathBuf> {
    let folder = if product == "studio" { "RobloxStudio" } else { "Roblox" };
    instance_data_dir(&state.paths.base, id, product).ok().map(|d| d.join("Library").join("Logs").join(folder))
}

fn list_player_logs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.contains("_Player_") && name.ends_with("_last.log")
        })
        .map(|e| e.path())
        .collect()
}

fn latest_log_in(dir: &Path, after_ms: u64, candidate: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    list_player_logs(dir)
        .into_iter()
        .filter(|p| candidate(p))
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
            (mtime >= after_ms).then_some((p, mtime))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(p, _)| p)
}

/// `readNewLogContent`: bytes appended since `start` (rewinds on truncation).
fn read_new_content(path: &Path, start: u64) -> (String, u64) {
    let Ok(size) = std::fs::metadata(path).map(|m| m.len()) else { return (String::new(), start) };
    if size < start {
        return (String::new(), 0);
    }
    if size == start {
        return (String::new(), start);
    }
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = Vec::with_capacity((size - start) as usize);
        file.take(size - start).read_to_end(&mut buffer)?;
        Ok(buffer)
    };
    match read() {
        Ok(bytes) => {
            let next = start + bytes.len() as u64;
            (String::from_utf8_lossy(&bytes).into_owned(), next)
        }
        Err(_) => (String::new(), start),
    }
}

/// `parseLogLevel`.
fn parse_level(level: &str, severity: Option<&str>) -> &'static str {
    match severity.map(str::to_lowercase).as_deref() {
        Some("error" | "critical") => return "error",
        Some("warning") => return "warn",
        Some("info") => return "info",
        Some("debug") => return "debug",
        _ => {}
    }
    match level.parse::<i64>() {
        Ok(n) if n >= 12 => "error",
        Ok(n) if n >= 8 => "warn",
        _ => "info",
    }
}

/// `ROBLOX_LOG_REGEX`:
/// `^(ISO),[\d.]+,[a-f0-9]+,(\d+)(?:,(\w+))?\s*\[([^\]]+)\]\s*(.*)$`.
fn parse_roblox_line(line: &str) -> Option<(u64, &'static str, String)> {
    let (timestamp, rest) = line.split_once(',')?;
    let valid_ts = timestamp.len() >= 11
        && timestamp.ends_with('Z')
        && timestamp.as_bytes()[..10].iter().enumerate().all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() })
        && timestamp.as_bytes().get(10) == Some(&b'T')
        && timestamp[11..timestamp.len() - 1].bytes().all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
        && timestamp.len() > 12;
    if !valid_ts {
        return None;
    }
    let (num, rest) = rest.split_once(',')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let (hex, rest) = rest.split_once(',')?;
    if hex.is_empty() || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let digits_end = rest.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    let (level, mut rest) = rest.split_at(digits_end);
    let mut severity = None;
    if let Some(after) = rest.strip_prefix(',') {
        let word_end = after.bytes().position(|b| !(b.is_ascii_alphanumeric() || b == b'_')).unwrap_or(after.len());
        if word_end > 0 {
            severity = Some(&after[..word_end]);
            rest = &after[word_end..];
        }
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    if close == 0 {
        return None;
    }
    let category = &rest[..close];
    let message = rest[close + 1..].trim_start();
    let millis = crate::services::instances::parse_iso_millis(timestamp)?;
    Some((millis, parse_level(level, severity), format!("[{category}] {message}")))
}

/// `parseLogLine`: unparseable lines keep their raw text at `info`.
fn parse_line(line: &str) -> Option<LogEntry> {
    if line.trim().is_empty() {
        return None;
    }
    Some(match parse_roblox_line(line) {
        Some((timestamp_ms, level, message)) => LogEntry { timestamp_ms, level, message, source: "file" },
        None => LogEntry { timestamp_ms: now_millis(), level: "info", message: line.to_owned(), source: "file" },
    })
}

fn entry_json(t: &Tracked, has_second_screen: bool) -> Value {
    json!({
        "instanceId": t.id,
        "instanceName": t.name,
        "product": t.product,
        "hasSecondScreen": has_second_screen,
        "startedAt": iso8601_from_millis(t.started_ms),
        "endedAt": t.ended_ms.map(iso8601_from_millis),
        "logCount": t.entries.len(),
    })
}

impl RunningInstances {
    /// `snapshotPlayerLogFiles`: logs that already exist before a launch.
    pub fn snapshot_player_log_files(&self, state: &AppState, id: &str) -> Vec<PathBuf> {
        [instance_logs_dir(state, id, "player"), system_logs_dir()].into_iter().flatten().flat_map(|d| list_player_logs(&d)).collect()
    }

    /// `register`: tracks the instance and, for Player, starts the log watcher.
    pub fn register(&self, state: &Arc<AppState>, id: &str, name: &str, product: &str, excluded: Vec<PathBuf>) {
        {
            let mut running = self.running.lock();
            if running.iter().any(|t| t.id == id) {
                tracing::debug!("Instance {id} already registered as running");
                return;
            }
            running.push(Tracked {
                id: id.to_owned(),
                name: name.to_owned(),
                product: product.to_owned(),
                started_ms: now_millis(),
                ended_ms: None,
                entries: Vec::new(),
                current_log: None,
                position: 0,
                fragment: String::new(),
                excluded: excluded.into_iter().collect(),
            });
        }
        self.retained.lock().retain(|t| t.id != id);
        tracing::info!("Registered running instance: {name}");
        if product == "player" {
            self.start_log_watcher(state, id, product);
        }
        self.notify_instances_changed(state);
    }

    /// `unregister`: Player instances keep their log for the recent list.
    pub fn unregister(&self, state: &AppState, id: &str) {
        let Some(mut tracked) = ({
            let mut running = self.running.lock();
            running.iter().position(|t| t.id == id).map(|pos| running.remove(pos))
        }) else {
            return;
        };
        if let Some(log) = &tracked.current_log {
            let mut claimed = self.claimed.lock();
            if claimed.get(log).map(String::as_str) == Some(id) {
                claimed.remove(log);
            }
        }
        self.listeners.lock().remove(id);
        if tracked.product == "player" {
            tracked.ended_ms = Some(now_millis());
            let mut retained = self.retained.lock();
            retained.retain(|t| t.id != id);
            retained.push(tracked);
            while retained.len() > RETAINED_INSTANCES_MAX {
                retained.remove(0);
            }
        }
        tracing::info!("Unregistered running instance: {id}");
        self.notify_instances_changed(state);
    }

    pub fn is_registered(&self, id: &str) -> bool {
        self.running.lock().iter().any(|t| t.id == id)
    }

    /// Name and product of a running instance (`get`).
    pub fn info(&self, id: &str) -> Option<(String, String)> {
        self.running.lock().iter().find(|t| t.id == id).map(|t| (t.name.clone(), t.product.clone()))
    }

    pub fn ids(&self) -> Vec<String> {
        self.running.lock().iter().map(|t| t.id.clone()).collect()
    }

    /// `list`.
    pub fn list(&self, state: &AppState) -> Vec<Value> {
        let open = crate::services::mission_control::is_open(state);
        self.running.lock().iter().map(|t| entry_json(t, open)).collect()
    }

    /// `listRecent`: newest first.
    pub fn list_recent(&self, state: &AppState) -> Vec<Value> {
        let open = crate::services::mission_control::is_open(state);
        self.retained.lock().iter().rev().map(|t| entry_json(t, open)).collect()
    }

    /// `getLogEntries`: a running instance's log, else a retained one's.
    pub fn log_entries(&self, id: &str) -> Vec<LogEntry> {
        if let Some(t) = self.running.lock().iter().find(|t| t.id == id) {
            return t.entries.clone();
        }
        self.retained.lock().iter().find(|t| t.id == id).map(|t| t.entries.clone()).unwrap_or_default()
    }

    /// `subscribeToLogEntries`: `None` for an instance that is not running.
    pub fn subscribe(&self, id: &str, listener: LogListener) -> Option<u64> {
        if !self.is_registered(id) {
            tracing::debug!("Ignoring log subscription for unknown instance {id}");
            return None;
        }
        let token = self.next_listener.fetch_add(1, Ordering::SeqCst);
        self.listeners.lock().entry(id.to_owned()).or_default().push((token, listener));
        Some(token)
    }

    pub fn unsubscribe(&self, id: &str, token: u64) {
        let mut listeners = self.listeners.lock();
        if let Some(list) = listeners.get_mut(id) {
            list.retain(|(t, _)| *t != token);
            if list.is_empty() {
                listeners.remove(id);
            }
        }
    }

    /// `notifyInstancesChanged` → `broadcastInstancesChanged` (+ the
    /// backgrounded-state refresh the main process runs on every change).
    pub fn notify_instances_changed(&self, state: &AppState) {
        state.events.emit(INSTANCES_CHANGED_CHANNEL, self.list(state));
        crate::refresh_backgrounded(state);
    }

    fn append(&self, state: &AppState, id: &str, entry: LogEntry) {
        {
            let mut running = self.running.lock();
            let Some(t) = running.iter_mut().find(|t| t.id == id) else { return };
            t.entries.push(entry.clone());
        }
        crate::services::mission_control::send(state, LOG_ENTRY_CHANNEL, entry.to_json(id));
        let listeners: Vec<LogListener> = self.listeners.lock().get(id).map(|l| l.iter().map(|(_, f)| Arc::clone(f)).collect()).unwrap_or_default();
        for listener in listeners {
            listener(&entry);
        }
    }

    fn start_log_watcher(&self, state: &Arc<AppState>, id: &str, product: &str) {
        let Some(logs_dir) = instance_logs_dir(state, id, product) else { return };
        tracing::debug!("Starting log watcher for {}", logs_dir.display());
        let weak = Arc::downgrade(state);
        let id = id.to_owned();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(LOG_POLL_INTERVAL).await;
                let Some(state) = weak.upgrade() else { return };
                if !state.running.poll(&state, &id, &logs_dir) {
                    return;
                }
            }
        });
    }

    /// `pollLogFile`; `false` once the instance is gone.
    fn poll(&self, state: &AppState, id: &str, logs_dir: &Path) -> bool {
        let (path, position, started, excluded) = {
            let running = self.running.lock();
            let Some(t) = running.iter().find(|t| t.id == id) else { return false };
            (t.current_log.clone(), t.position, t.started_ms, t.excluded.clone())
        };
        let path = match path {
            Some(p) => p,
            None => {
                let claimed: HashSet<PathBuf> = self.claimed.lock().keys().cloned().collect();
                let candidate = |p: &Path| !(excluded.contains(p) || claimed.contains(p));
                let found = latest_log_in(logs_dir, started, &candidate).or_else(|| system_logs_dir().and_then(|d| latest_log_in(&d, started, &candidate)));
                let Some(found) = found else { return true };
                self.claimed.lock().insert(found.clone(), id.to_owned());
                tracing::debug!("Watching log file: {}", found.display());
                let mut running = self.running.lock();
                if let Some(t) = running.iter_mut().find(|t| t.id == id) {
                    t.current_log = Some(found.clone());
                    t.position = 0;
                    t.fragment.clear();
                }
                found
            }
        };
        let (content, next) = read_new_content(&path, if path.exists() { position } else { position });
        let lines = {
            let mut running = self.running.lock();
            let Some(t) = running.iter_mut().find(|t| t.id == id) else { return false };
            t.position = next;
            if content.is_empty() {
                return true;
            }
            let combined = std::mem::take(&mut t.fragment) + &content;
            let mut segments: Vec<String> = combined.split('\n').map(|s| s.strip_suffix('\r').unwrap_or(s).to_owned()).collect();
            t.fragment = segments.pop().unwrap_or_default();
            segments
        };
        for line in lines {
            let trimmed = line.trim_start();
            if IGNORED_LOG_MESSAGE_PREFIXES.iter().any(|p| trimmed.contains(p)) {
                continue;
            }
            if let Some(entry) = parse_line(&line) {
                self.append(state, id, entry);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_roblox_log_lines() {
        let (ms, level, message) = parse_roblox_line("2024-05-01T10:20:30.123Z,12.5,1a2b,6 [FLog::Output] Hello world").unwrap();
        assert_eq!(ms, 1_714_558_830_123);
        assert_eq!(level, "info");
        assert_eq!(message, "[FLog::Output] Hello world");
        let (_, level, _) = parse_roblox_line("2024-05-01T10:20:30.123Z,1.0,ff,1,Warning [DFLog::X] y").unwrap();
        assert_eq!(level, "warn");
        let (_, level, _) = parse_roblox_line("2024-05-01T10:20:30.123Z,1.0,ff,13 [X] boom").unwrap();
        assert_eq!(level, "error");
        assert!(parse_roblox_line("plain text").is_none());
        assert_eq!(parse_line("plain text").unwrap().message, "plain text");
    }
}
