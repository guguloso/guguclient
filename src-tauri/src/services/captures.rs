//! Screenshots and clips (`captures:*`). Recording needs the Overwolf runtime
//! (`isOverwolf`), so — exactly like the Electron build outside Overwolf —
//! capture requests report `UNSUPPORTED_PLATFORM`; the library of existing
//! captures (launcher folders plus Roblox's own) is fully managed. Port of
//! `CapturesService`, `CapturesLibraryService`, `CapturesLibrary`,
//! `PosterCache` and `CaptureMediaProtocol` (`lbx-capture` →
//! `http://lbx-capture.localhost/<media|thumb>/<id>`).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tauri::http::{Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::ipc::Router;
use crate::state::AppState;
use crate::util::{iso8601_from_millis, now_iso, now_millis};

pub const PROTOCOL: &str = "lbx-capture";
const MEDIA_ORIGIN: &str = "http://lbx-capture.localhost";
const LIBRARY_INDEX_VERSION: u64 = 3;
const LIBRARY_INDEX_SOURCELESS_VERSION: u64 = 2;
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);
const SCREENSHOTS_SUBDIR: &str = "Screenshots";
const CLIPS_SUBDIR: &str = "Clips";
const DEFAULT_FOLDER_NAME: &str = "Gugu";
const LEGACY_FOLDER_NAME: &str = "Lunar";
const ROBLOX_CAPTURES_SUBDIR: &str = "Roblox";
const SCREENSHOT_EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];
const CLIP_EXTENSIONS: [&str; 3] = ["mp4", "mov", "mkv"];
const UNTRACKED_GAME_TITLE: &str = "Roblox";
const POSTER_WIDTH: u32 = 480;
const POSTER_JPEG_QUALITY: u8 = 80;
const SCREENSHOT_JPEG_QUALITY: u8 = 95;
const POSTER_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const POSTER_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_CAPTURE_FILE_NAME_LENGTH: usize = 200;
const FAILURE_TOAST_THROTTLE_MS: u64 = 10_000;
const CAPTURE_TOAST_DURATION_MS: u64 = 6000;

#[derive(Default)]
pub struct CapturesState {
    entries: Mutex<Option<Map<String, Value>>>,
    scan_cache: Mutex<Option<(String, Scan)>>,
    persist_generation: AtomicU64,
    dirty: std::sync::atomic::AtomicBool,
    failure_toasts: Mutex<HashMap<String, u64>>,
    poster_failures: Mutex<HashSet<PathBuf>>,
}

#[derive(Clone)]
struct Root {
    directory: PathBuf,
    fail_when_missing: bool,
    fail_when_unreadable: bool,
    kind: &'static str,
    source: &'static str,
}

#[derive(Clone)]
struct ScannedFile {
    path: PathBuf,
    kind: &'static str,
    mtime_ms: f64,
    size: u64,
    source: &'static str,
}

#[derive(Clone)]
struct Scan {
    files: Vec<ScannedFile>,
    unavailable_directory: Option<PathBuf>,
    verified: HashSet<String>,
}

fn error_message(code: &str) -> &'static str {
    match code {
        "ALREADY_EXISTS" => "A file with that name already exists.",
        "BUSY" => "A capture is already in progress.",
        "DIRECTORY_UNAVAILABLE" => "The captures folder isn't available right now.",
        "EMPTY_IMAGE" => "The image could not be decoded.",
        "FEATURE_DISABLED" => "Screenshots and clips are turned off in Settings.",
        "GAME_NOT_INJECTED" => "The overlay has not attached to Roblox yet.",
        "INSTANCE_NOT_ACTIVE" => "That instance is not the active Roblox game.",
        "INVALID_NAME" => "That name can't be used.",
        "NO_ACTIVE_GAME" => "No Roblox game is running.",
        "NOT_FOUND" => "That capture no longer exists.",
        "OVERLAY_NOT_READY" => "The in-game overlay is still loading.",
        "RECORDER_NOT_READY" => "The recorder is still loading.",
        "REPLAY_MULTI_INSTANCE" => "Instant replay pauses while more than one Roblox window is open.",
        "REPLAY_NOT_RUNNING" => "Instant replay is not running. Turn it on in Settings to save clips.",
        "REPLAY_STARTING" => "Instant replay is still starting. Try again in a moment.",
        "UNSUPPORTED_KIND" => "That action is only available for screenshots.",
        "UNSUPPORTED_PLATFORM" => "Captures are only available on Windows.",
        _ => "The capture failed.",
    }
}

/// `buildCaptureError`.
fn capture_error(code: &str, details: Option<String>) -> Value {
    let mut e = json!({ "code": code, "message": error_message(code) });
    if let Some(d) = details {
        e["details"] = json!(d);
    }
    e
}

/// `captureFailure`.
fn failure(code: &str, details: Option<String>) -> Value {
    json!({ "ok": false, "error": capture_error(code, details) })
}

fn setting_str(state: &AppState, id: &str) -> String {
    state.settings.get(id).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// `resolveCapturesDirectory`.
fn captures_directory(state: &AppState) -> PathBuf {
    let configured = setting_str(state, "captures-directory");
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return PathBuf::from(trimmed);
    }
    let parent = dirs::video_dir().unwrap_or_else(|| state.paths.base.clone());
    let folder = parent.join(DEFAULT_FOLDER_NAME);
    let legacy = parent.join(LEGACY_FOLDER_NAME);
    if !folder.exists() && legacy.is_dir() {
        // Earlier builds saved into `Videos\Lunar`; keep the library intact.
        if let Err(error) = std::fs::rename(&legacy, &folder) {
            tracing::warn!("Could not move captures from {}: {error}", legacy.display());
            return legacy;
        }
    }
    folder
}

/// `resolveCaptureRoots`.
fn roots(state: &AppState) -> Vec<Root> {
    let directory = captures_directory(state);
    let fail_when_missing = !setting_str(state, "captures-directory").trim().is_empty();
    let mut roots = vec![
        Root { directory: directory.join(SCREENSHOTS_SUBDIR), fail_when_missing, fail_when_unreadable: true, kind: "screenshot", source: "lunar" },
        Root { directory: directory.join(CLIPS_SUBDIR), fail_when_missing, fail_when_unreadable: true, kind: "clip", source: "lunar" },
    ];
    if state.settings.get_bool("captures-include-roblox") {
        if let Some(pictures) = dirs::picture_dir() {
            roots.push(Root { directory: pictures.join(ROBLOX_CAPTURES_SUBDIR), fail_when_missing: false, fail_when_unreadable: false, kind: "screenshot", source: "roblox" });
        }
        if let Some(videos) = dirs::video_dir() {
            roots.push(Root { directory: videos.join(ROBLOX_CAPTURES_SUBDIR), fail_when_missing: false, fail_when_unreadable: false, kind: "clip", source: "roblox" });
        }
    }
    roots
}

fn path_key(path: &Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().to_lowercase()
}

fn extension(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

fn kind_from_extension(path: &Path) -> Option<&'static str> {
    let ext = extension(path);
    if SCREENSHOT_EXTENSIONS.contains(&ext.as_str()) {
        Some("screenshot")
    } else if CLIP_EXTENSIONS.contains(&ext.as_str()) {
        Some("clip")
    } else {
        None
    }
}

/// `isPathInside`: a strict descendant.
fn is_path_inside(child: &Path, parent: &Path) -> bool {
    let (c, p) = (path_key(child), path_key(parent));
    c.len() > p.len() && c.starts_with(&p) && matches!(c.as_bytes()[p.trim_end_matches(['\\', '/']).len()], b'\\' | b'/')
}

fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

fn dir_status(path: &Path) -> &'static str {
    match std::fs::metadata(path) {
        Ok(_) => "read",
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => "missing",
        Err(_) => "unreadable",
    }
}

/// `scanCaptureFiles`.
fn scan(roots: &[Root]) -> Scan {
    let mut files = Vec::new();
    let mut verified = HashSet::new();
    let mut unavailable = None;
    for root in roots {
        let status = match std::fs::read_dir(&root.directory) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if kind_from_extension(&path) != Some(root.kind) {
                        continue;
                    }
                    let Ok(meta) = std::fs::metadata(&path) else { continue };
                    if meta.is_file() {
                        files.push(ScannedFile { mtime_ms: mtime_ms(&meta), size: meta.len(), path, kind: root.kind, source: root.source });
                    }
                }
                "read"
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => "missing",
            Err(_) => "unreadable",
        };
        let parent = root.directory.parent().map_or("missing", dir_status);
        let (is_unavailable, is_verified) = match status {
            "unreadable" => (root.fail_when_unreadable, false),
            "read" => (false, true),
            _ if parent == "read" => (false, true),
            _ => (root.fail_when_missing, false),
        };
        if is_verified {
            verified.insert(path_key(&root.directory));
        }
        if is_unavailable && unavailable.is_none() {
            unavailable = Some(root.directory.clone());
        }
    }
    Scan { files, unavailable_directory: unavailable, verified }
}

fn roots_key(roots: &[Root]) -> String {
    roots.iter().map(|r| format!("{}:{}:{}", r.source, r.kind, path_key(&r.directory))).collect::<Vec<_>>().join("|")
}

fn version_key(mtime_ms: f64, size: u64) -> String {
    format!("{}-{size}", mtime_ms.trunc() as i64)
}

/// `toLibraryItem`.
fn to_item(capture: &Value, mtime: f64, size: u64) -> Value {
    let id = capture["id"].as_str().unwrap_or("");
    let v = version_key(mtime, size);
    let mut item = capture.clone();
    item["mediaUrl"] = json!(format!("{MEDIA_ORIGIN}/media/{}?v={v}", urlencoding::encode(id)));
    item["modifiedAtMs"] = json!(mtime);
    item["sizeBytes"] = json!(size);
    item["thumbnailUrl"] = json!(format!("{MEDIA_ORIGIN}/thumb/{}?v={v}", urlencoding::encode(id)));
    item
}

/// `toItemResult`.
fn item_result(entry: &Value) -> Value {
    match std::fs::metadata(entry["filePath"].as_str().unwrap_or("")) {
        Ok(meta) => json!({ "ok": true, "item": to_item(entry, mtime_ms(&meta), meta.len()) }),
        Err(_) => failure("NOT_FOUND", None),
    }
}

/// Roblox's own file names carry the capture time (`robloxCapturedAtMs`).
fn roblox_captured_at(path: &Path, kind: &str) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let digits = |s: &str| s.parse::<i64>().ok();
    if kind == "clip" {
        // Roblox-YYYY-MM-DDTHH_MM_SS.mmmZ (UTC)
        let rest = name.strip_prefix("Roblox-")?;
        if rest.len() < 24 {
            return None;
        }
        let iso = format!("{}T{}:{}:{}.{}Z", &rest[0..10], &rest[11..13], &rest[14..16], &rest[17..19], &rest[20..23]);
        return crate::services::instances::parse_iso_millis(&iso);
    }
    // RobloxScreenShotYYYYMMDD_HHMMSSmmm (local time)
    let rest = name.strip_prefix("RobloxScreenShot")?;
    if rest.len() < 18 || rest.as_bytes()[8] != b'_' {
        return None;
    }
    let (y, mo, d) = (digits(&rest[0..4])?, digits(&rest[4..6])?, digits(&rest[6..8])?);
    let (h, mi, s, ms) = (digits(&rest[9..11])?, digits(&rest[11..13])?, digits(&rest[13..15])?, digits(&rest[15..18])?);
    let utc = crate::services::instances::parse_iso_millis(&format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z"))?;
    let offset_ms = local_offset_ms(utc);
    u64::try_from(utc as i64 - offset_ms).ok()
}

/// Local UTC offset at a moment (Windows time zone rules).
fn local_offset_ms(_utc_ms: u64) -> i64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        let mode = unsafe { GetTimeZoneInformation(&mut tz) };
        let bias = tz.Bias + if mode == 2 { tz.DaylightBias } else { tz.StandardBias };
        -(bias as i64) * 60_000
    }
    #[cfg(not(windows))]
    0
}

/// `titleFromFileName`: the stem without the ` YYYY-MM-DD HH-MM-SS (n)` suffix.
fn title_from_file_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    static SUFFIX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r" \d{4}-\d{2}-\d{2} \d{2}-\d{2}-\d{2}(?: \(\d+\))?$").expect("regex"));
    let title = SUFFIX.replace(stem, "").trim().to_owned();
    if title.is_empty() {
        UNTRACKED_GAME_TITLE.to_owned()
    } else {
        title
    }
}

fn synthesize(file: &ScannedFile) -> Value {
    let roblox = file.source == "roblox";
    let captured = (if roblox { roblox_captured_at(&file.path, file.kind) } else { None }).unwrap_or(file.mtime_ms as u64);
    json!({
        "capturedAt": iso8601_from_millis(captured), "favorite": false,
        "fileName": file.path.file_name().and_then(|n| n.to_str()).unwrap_or(""), "filePath": file.path.to_string_lossy(),
        "gameTitle": if roblox { UNTRACKED_GAME_TITLE.to_owned() } else { title_from_file_name(&file.path) },
        "id": uuid::Uuid::new_v4().to_string(), "instanceId": null, "kind": file.kind, "placeId": null, "source": file.source, "universeId": null,
    })
}

fn library_file(state: &AppState) -> PathBuf {
    state.paths.base.join("captures").join("library.json")
}

fn posters_dir(state: &AppState) -> PathBuf {
    state.paths.base.join("cache").join("capture-thumbnails")
}

/// `loadEntries` with the v2 → v3 migration.
fn with_entries<R>(state: &AppState, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
    let mut guard = state.captures.entries.lock();
    if guard.is_none() {
        let mut index = std::fs::read(library_file(state)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        match index.as_ref().and_then(|i| i["version"].as_u64()) {
            Some(LIBRARY_INDEX_VERSION) | None => {}
            Some(LIBRARY_INDEX_SOURCELESS_VERSION) => {
                if let Some(entries) = index.as_mut().and_then(|i| i["entries"].as_object_mut()) {
                    for entry in entries.values_mut() {
                        if entry.get("source").is_none_or(Value::is_null) {
                            entry["source"] = json!("lunar");
                        }
                    }
                }
                tracing::info!("[Captures/Library] Migrated library index v2 to v3");
            }
            Some(v) => {
                tracing::info!("[Captures/Library] Discarding library index v{v}; expected v3");
                index = None;
            }
        }
        *guard = Some(index.and_then(|i| i["entries"].as_object().cloned()).unwrap_or_default());
    }
    f(guard.as_mut().expect("loaded"))
}

fn write_index(state: &AppState) {
    let snapshot = with_entries(state, |e| json!({ "entries": e.clone(), "version": LIBRARY_INDEX_VERSION }));
    let path = library_file(state);
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let result = std::fs::create_dir_all(path.parent().expect("parent"))
        .and_then(|_| std::fs::write(&tmp, serde_json::to_string_pretty(&snapshot).unwrap_or_default()))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("[Captures/Library] Failed to persist library index: {error}");
    }
}

/// `LibraryIndexPersister.schedule` (500ms debounce).
fn persist(state: &AppState) {
    state.captures.dirty.store(true, Ordering::SeqCst);
    let generation = state.captures.persist_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(state) = state.arc() else { return };
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PERSIST_DEBOUNCE).await;
        if state.captures.persist_generation.load(Ordering::SeqCst) == generation && state.captures.dirty.swap(false, Ordering::SeqCst) {
            write_index(&state);
        }
    });
}

/// `flushSync` at quit.
pub fn flush(state: &AppState) {
    if state.captures.dirty.swap(false, Ordering::SeqCst) {
        write_index(state);
    }
}

fn set_entry(state: &AppState, entry: Value) {
    let id = entry["id"].as_str().unwrap_or("").to_owned();
    let moved = with_entries(state, |entries| {
        let moved = entries.get(&id).is_some_and(|p| p["filePath"] != entry["filePath"]);
        entries.insert(id, entry);
        moved
    });
    if moved {
        *state.captures.scan_cache.lock() = None;
    }
}

fn broadcast_library_changed(state: &AppState) {
    state.events.emit("captures:libraryChanged", Value::Null);
}

/// `CapturesLibrary.list`.
fn list_library(state: &AppState, rescan: bool) -> Value {
    let roots = roots(state);
    let key = roots_key(&roots);
    let cached = state.captures.scan_cache.lock().as_ref().filter(|(k, _)| *k == key).map(|(_, s)| s.clone());
    let scanned = match cached {
        Some(s) if !rescan => s,
        _ => scan(&roots),
    };
    *state.captures.scan_cache.lock() = scanned.unavailable_directory.is_none().then(|| (key, scanned.clone()));
    if let Some(dir) = &scanned.unavailable_directory {
        tracing::warn!("[Captures/Library] Captures directory unavailable: {}", dir.display());
        return json!({ "ok": false, "error": capture_error("DIRECTORY_UNAVAILABLE", Some(dir.to_string_lossy().into_owned())) });
    }
    let mut changed = false;
    let mut items: Vec<Value> = Vec::new();
    with_entries(state, |entries| {
        let by_path: HashMap<String, String> = entries.values().filter_map(|e| Some((path_key(Path::new(e["filePath"].as_str()?)), e["id"].as_str()?.to_owned()))).collect();
        for file in &scanned.files {
            let known = by_path.get(&path_key(&file.path)).and_then(|id| entries.get(id)).cloned();
            let capture = match known {
                Some(mut known) => {
                    if known["source"] != file.source {
                        known["source"] = json!(file.source);
                        entries.insert(known["id"].as_str().unwrap_or("").to_owned(), known.clone());
                        changed = true;
                    }
                    known
                }
                None => {
                    let synthesized = synthesize(file);
                    entries.insert(synthesized["id"].as_str().unwrap_or("").to_owned(), synthesized.clone());
                    changed = true;
                    synthesized
                }
            };
            items.push(to_item(&capture, file.mtime_ms, file.size));
        }
        let present: HashSet<String> = items.iter().filter_map(|i| i["id"].as_str().map(str::to_owned)).collect();
        let gone: Vec<String> = entries
            .iter()
            .filter(|(id, e)| {
                !present.contains(*id)
                    && Path::new(e["filePath"].as_str().unwrap_or("")).parent().is_some_and(|p| scanned.verified.contains(&path_key(p)))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            entries.remove(&id);
            changed = true;
        }
    });
    if changed {
        persist(state);
    }
    items.sort_by(|a, b| b["capturedAt"].as_str().cmp(&a["capturedAt"].as_str()));
    json!({ "ok": true, "items": items })
}

fn resolve(state: &AppState, id: &str) -> Option<Value> {
    with_entries(state, |e| e.get(id).cloned())
}

/// `CaptureEntryResolver.insideDirectory`.
fn inside_directory(state: &AppState, id: &str) -> Option<Value> {
    let entry = resolve(state, id)?;
    let path = PathBuf::from(entry["filePath"].as_str().unwrap_or(""));
    if !roots(state).iter().any(|r| is_path_inside(&path, &r.directory)) {
        tracing::warn!("[Captures/Library] Refused path outside every capture root: {}", path.display());
        return None;
    }
    Some(entry)
}

/// `insideLibrary`: also requires the file to still exist.
fn inside_library(state: &AppState, id: &str) -> Option<Value> {
    inside_directory(state, id).filter(|e| Path::new(e["filePath"].as_str().unwrap_or("")).exists())
}

// ------------------------------------------------------------------ posters

fn poster_prefix(id: &str) -> String {
    format!("{id}-")
}

fn remove_posters(state: &AppState, id: &str, keep: Option<&str>) {
    let prefix = poster_prefix(id);
    for entry in std::fs::read_dir(posters_dir(state)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && Some(name.as_str()) != keep && !name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `decodePoster` for images: width capped at 480, JPEG q80.
fn image_poster(source: &Path) -> Option<Vec<u8>> {
    let image = image::open(source).ok()?;
    let image = if image.width() > POSTER_WIDTH { image.resize(POSTER_WIDTH, u32::MAX, image::imageops::FilterType::Triangle) } else { image };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, POSTER_JPEG_QUALITY).encode_image(&image.to_rgb8()).ok()?;
    Some(out)
}

/// `PosterCache.ensure`: the poster path, generated on demand.
fn ensure_poster(state: &AppState, capture: &Value) -> Option<PathBuf> {
    let file = PathBuf::from(capture["filePath"].as_str()?);
    let meta = std::fs::metadata(&file).ok()?;
    let id = capture["id"].as_str()?;
    let destination = posters_dir(state).join(format!("{}{}.jpg", poster_prefix(id), version_key(mtime_ms(&meta), meta.len())));
    if let Ok(meta) = std::fs::metadata(&destination) {
        if meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > POSTER_TOUCH_INTERVAL) {
            let _ = std::fs::File::options().write(true).open(&destination).and_then(|f| f.set_modified(SystemTime::now()));
        }
        return Some(destination);
    }
    if state.captures.poster_failures.lock().contains(&destination) {
        return None;
    }
    let _ = std::fs::create_dir_all(posters_dir(state));
    // Clips need ffmpeg, which only ships with the Overwolf recorder.
    let bytes = if capture["kind"] == "clip" { None } else { image_poster(&file) };
    let Some(bytes) = bytes else {
        if capture["kind"] == "screenshot" {
            state.captures.poster_failures.lock().insert(destination);
        }
        return None;
    };
    let tmp = destination.with_extension(format!("jpg.{}.tmp", uuid::Uuid::new_v4()));
    if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &destination)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    remove_posters(state, id, destination.file_name().and_then(|n| n.to_str()));
    Some(destination)
}

/// `PosterCache.sweep`: drops posters untouched for 30 days.
fn sweep_posters(state: &AppState) {
    let mut count = 0;
    for entry in std::fs::read_dir(posters_dir(state)).into_iter().flatten().flatten() {
        let old = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).is_some_and(|age| age > POSTER_MAX_AGE);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            count += 1;
        }
    }
    if count > 0 {
        tracing::info!("[Captures/Posters] Swept {count} stale posters");
    }
    // `sweepTempFiles` of the library index.
    let file = library_file(state);
    let prefix = format!("{}.", file.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    for entry in std::fs::read_dir(file.parent().expect("parent")).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ------------------------------------------------------------- protocol

fn content_type(path: &Path) -> Option<&'static str> {
    Some(match extension(path).as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "mp4" => "video/mp4",
        "png" => "image/png",
        _ => return None,
    })
}

fn plain(status: StatusCode, text: &str) -> Response<Vec<u8>> {
    Response::builder().status(status).body(text.as_bytes().to_vec()).unwrap_or_default()
}

/// `parseRangeHeader`.
fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(header) = header else { return Ok(None) };
    let spec = header.trim().strip_prefix("bytes=").ok_or(())?;
    let (start, end) = spec.split_once('-').ok_or(())?;
    if !start.bytes().all(|b| b.is_ascii_digit()) || !end.bytes().all(|b| b.is_ascii_digit()) || (start.is_empty() && end.is_empty()) {
        return Err(());
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(Some((size.saturating_sub(suffix), size.saturating_sub(1))));
    }
    let start: u64 = start.parse().map_err(|_| ())?;
    let end = if end.is_empty() { size.saturating_sub(1) } else { end.parse::<u64>().map_err(|_| ())?.min(size.saturating_sub(1)) };
    if start >= size || start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// `createFileResponse`.
fn file_response(path: &Path, content_type: &str, cache: &str, range: Option<&str>) -> Response<Vec<u8>> {
    let Ok(size) = std::fs::metadata(path).map(|m| m.len()) else { return plain(StatusCode::NOT_FOUND, "Not found") };
    let range = match parse_range(range, size) {
        Ok(r) => r,
        Err(()) => {
            return Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE).header("Content-Range", format!("bytes */{size}")).body(Vec::new()).unwrap_or_default();
        }
    };
    let (start, end) = range.unwrap_or((0, size.saturating_sub(1)));
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = Vec::with_capacity((end + 1 - start) as usize);
        file.take(end + 1 - start).read_to_end(&mut buffer)?;
        Ok(buffer)
    };
    let Ok(body) = read() else { return plain(StatusCode::NOT_FOUND, "Not found") };
    let mut builder = Response::builder()
        .status(if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header("Accept-Ranges", "bytes")
        .header("Cache-Control", cache)
        .header("Content-Length", body.len().to_string())
        .header("Content-Type", content_type);
    if range.is_some() {
        builder = builder.header("Content-Range", format!("bytes {start}-{end}/{size}"));
    }
    builder.body(body).unwrap_or_default()
}

fn handle_media(state: &AppState, request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    let mut segments = request.uri().path().trim_start_matches('/').splitn(2, '/');
    let host = segments.next().unwrap_or("");
    let id = segments.next().and_then(|s| urlencoding::decode(s).ok()).map(|s| s.into_owned()).unwrap_or_default();
    if id.is_empty() || !(host == "media" || host == "thumb") {
        return plain(StatusCode::BAD_REQUEST, "Bad request");
    }
    let Some(capture) = resolve(state, &id) else { return plain(StatusCode::NOT_FOUND, "Not found") };
    let file = PathBuf::from(capture["filePath"].as_str().unwrap_or(""));
    let Some(kind) = content_type(&file) else { return plain(StatusCode::FORBIDDEN, "Unsupported file type") };
    let Ok(real) = std::fs::canonicalize(&file) else { return plain(StatusCode::NOT_FOUND, "Not found") };
    let inside = roots(state).iter().filter_map(|r| std::fs::canonicalize(&r.directory).ok()).any(|dir| is_path_inside(&real, &dir));
    if !inside {
        tracing::warn!("[Captures/Media] Refused path outside every capture root: {}", file.display());
        return plain(StatusCode::FORBIDDEN, "Forbidden");
    }
    if host == "thumb" {
        return match ensure_poster(state, &capture) {
            Some(poster) => file_response(&poster, "image/jpeg", "private, max-age=31536000, immutable", None),
            None => plain(StatusCode::NOT_FOUND, "No thumbnail"),
        };
    }
    let range = request.headers().get("range").and_then(|v| v.to_str().ok());
    file_response(&real, kind, "private, max-age=3600", range)
}

pub fn protocol_handler<R: Runtime>(ctx: UriSchemeContext<'_, R>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let state = Arc::clone(ctx.app_handle().state::<Arc<AppState>>().inner());
    tauri::async_runtime::spawn_blocking(move || responder.respond(handle_media(&state, &request)));
}

// --------------------------------------------------------------- service

/// `getStatus`: no Overwolf runtime, so nothing can record.
fn status(state: &AppState) -> Value {
    json!({
        "activeGame": null,
        "clipsAvailable": false,
        "directory": captures_directory(state).to_string_lossy(),
        "enabled": false,
        "hotkeys": {
            "clip": state.settings.get("captures-clip-hotkey"),
            "error": null,
            "registered": false,
            "screenshot": state.settings.get("captures-screenshot-hotkey"),
        },
        "overlayReady": false,
        "recorderReady": false,
        "replay": { "active": false, "bufferSeconds": state.settings.get("captures-replay-seconds"), "captureMode": null, "enabled": false, "lastError": null, "pid": null },
        "screenshotsAvailable": false,
        "supported": false,
    })
}

pub fn broadcast_status(state: &AppState) {
    state.events.emit("captures:statusChanged", status(state));
}

/// `CaptureNotifier.failCapture` for the platform gate.
fn fail_capture(state: &AppState, kind: &str, trigger: &str) -> Value {
    let code = "UNSUPPORTED_PLATFORM";
    let error = capture_error(code, None);
    tracing::warn!("[Captures] {kind} failed ({code}): {}", error_message(code));
    state.track("capture.failure", json!({ "error_code": code, "error_message": error_message(code), "kind": kind, "stage": kind, "trigger": trigger }));
    if trigger == "hotkey" {
        let now = now_millis();
        let mut toasts = state.captures.failure_toasts.lock();
        if now - toasts.get(code).copied().unwrap_or(0) >= FAILURE_TOAST_THROTTLE_MS {
            toasts.insert(code.into(), now);
            let title = if kind == "clip" { "Clip not saved" } else { "Screenshot not taken" };
            crate::services::app::show_toast(state, &json!({ "body": error_message(code), "duration": CAPTURE_TOAST_DURATION_MS, "isDismissable": true, "title": title }));
        }
    }
    json!({ "ok": false, "error": error })
}

/// `buildRenamedPath`.
fn renamed_path(original: &Path, raw: &str) -> Option<PathBuf> {
    let name = raw.trim();
    let forbidden = name.chars().any(|c| (c as u32) < 32 || "<>:\"/\\|?*".contains(c));
    if name.is_empty() || name.starts_with('.') || forbidden {
        return None;
    }
    let ext = original.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    let stem = if name.to_lowercase().ends_with(&ext.to_lowercase()) { &name[..name.len() - ext.len()] } else { name };
    let reserved = {
        let base = stem.split('.').next().unwrap_or("").to_lowercase();
        matches!(base.as_str(), "con" | "prn" | "aux" | "nul") || ((base.starts_with("com") || base.starts_with("lpt")) && base.len() == 4 && base.as_bytes()[3].is_ascii_digit())
    };
    if stem.is_empty() || stem.ends_with(['.', ' ']) || reserved {
        return None;
    }
    let file_name = format!("{stem}{ext}");
    if file_name.chars().count() > MAX_CAPTURE_FILE_NAME_LENGTH || Some(file_name.as_str()) == original.file_name().and_then(|n| n.to_str()) {
        return None;
    }
    Some(original.with_file_name(file_name))
}

/// `renameWithoutClobber`.
fn rename_without_clobber(from: &Path, to: &Path) -> std::io::Result<bool> {
    if !to.exists() {
        std::fs::rename(from, to)?;
        return Ok(true);
    }
    // Case-only renames on Windows: stage through a temporary name.
    let staging = from.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::rename(from, &staging)?;
    if to.exists() {
        std::fs::rename(&staging, from)?;
        return Ok(false);
    }
    if let Err(error) = std::fs::rename(&staging, to) {
        let _ = std::fs::rename(&staging, from);
        return Err(error);
    }
    Ok(true)
}

fn decode_data_url(data_url: &str) -> Option<image::DynamicImage> {
    let (_, encoded) = data_url.strip_prefix("data:")?.split_once(";base64,")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    image::load_from_memory(&bytes).ok()
}

fn encode_for(image: &image::DynamicImage, path: &Path) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if matches!(extension(path).as_str(), "jpg" | "jpeg") {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, SCREENSHOT_JPEG_QUALITY).encode_image(&image.to_rgb8()).ok()?;
    } else {
        image.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    }
    Some(out)
}

fn copy_to_clipboard(image: &image::DynamicImage) -> Result<(), String> {
    let rgba = image.to_rgba8();
    let data = arboard::ImageData { width: rgba.width() as usize, height: rgba.height() as usize, bytes: std::borrow::Cow::Owned(rgba.into_raw()) };
    arboard::Clipboard::new().and_then(|mut c| c.set_image(data)).map_err(|e| e.to_string())
}

fn save_edited(state: &AppState, request: &Value) -> Value {
    let Some(entry) = inside_library(state, request["captureId"].as_str().unwrap_or("")) else { return failure("NOT_FOUND", None) };
    if entry["kind"] != "screenshot" {
        return failure("UNSUPPORTED_KIND", None);
    }
    let Some(image) = request["dataUrl"].as_str().and_then(decode_data_url) else { return failure("EMPTY_IMAGE", None) };
    let original = PathBuf::from(entry["filePath"].as_str().unwrap_or(""));
    let copy = request["mode"] == "copy";
    let target = if copy {
        let directory = if entry["source"] == "roblox" {
            let dir = captures_directory(state).join(SCREENSHOTS_SUBDIR);
            let _ = std::fs::create_dir_all(&dir);
            dir
        } else {
            original.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let stem = original.file_stem().and_then(|s| s.to_str()).unwrap_or("capture");
        let mut candidate = directory.join(format!("{stem}-edited.png"));
        let mut counter = 2;
        while candidate.exists() {
            candidate = directory.join(format!("{stem}-edited-{counter}.png"));
            counter += 1;
        }
        candidate
    } else {
        original.clone()
    };
    let Some(bytes) = encode_for(&image, &target) else { return failure("CAPTURE_FAILED", Some("Could not encode the image.".into())) };
    let tmp = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    if let Err(error) = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &target)) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("[Captures/Library] Failed to save edited {}: {error}", entry["fileName"].as_str().unwrap_or(""));
        return failure("CAPTURE_FAILED", Some(error.to_string()));
    }
    let (width, height) = (image.width(), image.height());
    let updated = if copy {
        json!({
            "capturedAt": now_iso(), "favorite": false, "fileName": target.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "filePath": target.to_string_lossy(), "gameTitle": entry["gameTitle"], "height": height, "id": uuid::Uuid::new_v4().to_string(),
            "instanceId": entry["instanceId"], "kind": "screenshot", "placeId": entry["placeId"], "source": "lunar", "universeId": entry["universeId"], "width": width,
        })
    } else {
        let mut e = entry.clone();
        e["height"] = json!(height);
        e["width"] = json!(width);
        e
    };
    if copy {
        set_entry(state, updated.clone());
        *state.captures.scan_cache.lock() = None;
    } else {
        remove_posters(state, entry["id"].as_str().unwrap_or(""), None);
        set_entry(state, updated.clone());
        *state.captures.scan_cache.lock() = None;
    }
    persist(state);
    state.track("capture.edit_save", json!({ "mode": request["mode"] }));
    broadcast_library_changed(state);
    item_result(&updated)
}

fn set_favorite(state: &AppState, id: &str, favorite: bool) -> Value {
    let Some(mut entry) = inside_library(state, id) else { return failure("NOT_FOUND", None) };
    entry["favorite"] = json!(favorite);
    set_entry(state, entry.clone());
    persist(state);
    state.track("capture.favorite", json!({ "favorite": favorite, "kind": entry["kind"] }));
    item_result(&entry)
}

pub fn start(state: &Arc<AppState>) {
    let state = Arc::clone(state);
    tauri::async_runtime::spawn_blocking(move || sweep_posters(&state));
}

pub fn register(router: &mut Router) {
    router
        .handle("captures:getStatus", |ctx, _| async move { Ok(status(&ctx.state)) })
        .handle("captures:takeScreenshot", |ctx, args| async move {
            let props: Value = args.opt(0)?.unwrap_or(Value::Null);
            Ok(fail_capture(&ctx.state, "screenshot", props["trigger"].as_str().unwrap_or("")))
        })
        .handle("captures:saveClip", |ctx, args| async move {
            let props: Value = args.opt(0)?.unwrap_or(Value::Null);
            Ok(fail_capture(&ctx.state, "clip", props["trigger"].as_str().unwrap_or("")))
        })
        .handle("captures:listLibrary", |ctx, args| async move {
            let props: Value = args.opt(0)?.unwrap_or(Value::Null);
            let state = Arc::clone(&ctx.state);
            let rescan = props["rescan"].as_bool().unwrap_or(false);
            Ok(tauri::async_runtime::spawn_blocking(move || list_library(&state, rescan)).await.unwrap_or(Value::Null))
        })
        .handle("captures:deleteCapture", |ctx, args| async move {
            let id: String = args.get(0)?;
            let state = &ctx.state;
            let Some(entry) = inside_directory(state, &id) else { return Ok(failure("NOT_FOUND", None)) };
            let path = PathBuf::from(entry["filePath"].as_str().unwrap_or(""));
            if path.exists() {
                if let Err(error) = trash::delete(&path) {
                    tracing::warn!("[Captures/Library] Failed to trash {}: {error}", path.display());
                    return Ok(failure("CAPTURE_FAILED", Some(error.to_string())));
                }
            }
            with_entries(state, |e| e.remove(&id));
            *state.captures.scan_cache.lock() = None;
            persist(state);
            remove_posters(state, &id, None);
            state.captures.poster_failures.lock().retain(|p| !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&poster_prefix(&id))));
            state.track("capture.delete", json!({ "kind": entry["kind"], "source": entry["source"] }));
            broadcast_library_changed(state);
            Ok(json!({ "ok": true }))
        })
        .handle("captures:revealCapture", |ctx, args| async move {
            let id: String = args.get(0)?;
            let Some(entry) = inside_library(&ctx.state, &id) else { return Ok(failure("NOT_FOUND", None)) };
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("explorer.exe")
                    .raw_arg(format!("/select,\"{}\"", entry["filePath"].as_str().unwrap_or("")))
                    .spawn();
            }
            Ok(json!({ "ok": true }))
        })
        .handle("captures:openCapturesFolder", |ctx, _| async move {
            let directory = captures_directory(&ctx.state);
            if let Err(error) = std::fs::create_dir_all(&directory) {
                tracing::warn!("[Captures] Failed to create captures directory {}: {error}", directory.display());
                return Ok(failure("CAPTURE_FAILED", Some(error.to_string())));
            }
            let error = crate::services::roblox_layout::open_path(&directory);
            if !error.is_empty() {
                return Ok(failure("CAPTURE_FAILED", Some(error)));
            }
            Ok(json!({ "ok": true }))
        })
        .handle("captures:chooseCapturesDirectory", |ctx, _| async move {
            let picked = rfd::AsyncFileDialog::new().set_title("Choose captures folder").set_directory(captures_directory(&ctx.state)).pick_folder().await;
            Ok(picked.map(|p| json!(p.path().to_string_lossy())).unwrap_or(Value::Null))
        })
        .handle("captures:setFavorite", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let result = set_favorite(&ctx.state, props["captureId"].as_str().unwrap_or(""), props["favorite"].as_bool().unwrap_or(false));
            if result["ok"] == true {
                broadcast_library_changed(&ctx.state);
            }
            Ok(result)
        })
        .handle("captures:setFavorites", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let favorite = props["favorite"].as_bool().unwrap_or(false);
            let (mut items, mut failed) = (Vec::new(), Vec::new());
            for id in props["captureIds"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let result = set_favorite(&ctx.state, id, favorite);
                if result["ok"] == true {
                    items.push(result["item"].clone());
                } else {
                    failed.push(json!(id));
                }
            }
            if !items.is_empty() {
                broadcast_library_changed(&ctx.state);
            }
            Ok(json!({ "failedIds": failed, "items": items }))
        })
        .handle("captures:renameCapture", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let state = &ctx.state;
            let Some(entry) = inside_library(state, props["captureId"].as_str().unwrap_or("")) else { return Ok(failure("NOT_FOUND", None)) };
            let original = PathBuf::from(entry["filePath"].as_str().unwrap_or(""));
            let Some(target) = renamed_path(&original, props["newName"].as_str().unwrap_or("")) else { return Ok(failure("INVALID_NAME", None)) };
            match rename_without_clobber(&original, &target) {
                Ok(false) => return Ok(failure("ALREADY_EXISTS", None)),
                Ok(true) => {}
                Err(error) => {
                    tracing::warn!("[Captures/Library] Failed to rename {} to {}: {error}", original.display(), target.display());
                    return Ok(failure("CAPTURE_FAILED", Some(error.to_string())));
                }
            }
            let mut updated = entry.clone();
            updated["fileName"] = json!(target.file_name().and_then(|n| n.to_str()).unwrap_or(""));
            updated["filePath"] = json!(target.to_string_lossy());
            set_entry(state, updated.clone());
            persist(state);
            state.track("capture.rename", json!({ "kind": entry["kind"] }));
            broadcast_library_changed(state);
            Ok(item_result(&updated))
        })
        .handle("captures:getCaptureInfo", |ctx, args| async move {
            let id: String = args.get(0)?;
            let state = &ctx.state;
            let Some(entry) = inside_library(state, &id) else { return Ok(failure("NOT_FOUND", None)) };
            let needs_duration = entry["kind"] == "clip" && !entry["durationMs"].is_number();
            if entry["width"].is_number() && entry["height"].is_number() && !needs_duration {
                return Ok(item_result(&entry));
            }
            // Clips need ffprobe (Overwolf recorder); screenshots read their header.
            if entry["kind"] != "screenshot" {
                return Ok(item_result(&entry));
            }
            match image::image_dimensions(entry["filePath"].as_str().unwrap_or("")) {
                Ok((width, height)) => {
                    let mut updated = entry.clone();
                    updated["width"] = json!(width);
                    updated["height"] = json!(height);
                    set_entry(state, updated.clone());
                    persist(state);
                    Ok(item_result(&updated))
                }
                Err(_) => Ok(item_result(&entry)),
            }
        })
        .handle("captures:readCaptureImage", |ctx, args| async move {
            let id: String = args.get(0)?;
            let Some(entry) = inside_library(&ctx.state, &id) else { return Ok(failure("NOT_FOUND", None)) };
            if entry["kind"] != "screenshot" {
                return Ok(failure("UNSUPPORTED_KIND", None));
            }
            let path = PathBuf::from(entry["filePath"].as_str().unwrap_or(""));
            let data = content_type(&path).and_then(|kind| std::fs::read(&path).ok().map(|b| format!("data:{kind};base64,{}", base64::engine::general_purpose::STANDARD.encode(b))));
            Ok(match data {
                Some(data_url) => json!({ "ok": true, "dataUrl": data_url }),
                None => failure("CAPTURE_FAILED", Some("Could not read the file.".into())),
            })
        })
        .handle("captures:saveEditedCapture", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let state = Arc::clone(&ctx.state);
            Ok(tauri::async_runtime::spawn_blocking(move || save_edited(&state, &props)).await.unwrap_or(Value::Null))
        })
        .handle("captures:copyImage", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let image = if props["source"] == "editor" {
                match props["dataUrl"].as_str().and_then(decode_data_url) {
                    Some(i) => i,
                    None => return Ok(failure("EMPTY_IMAGE", None)),
                }
            } else {
                let Some(entry) = inside_library(&ctx.state, props["captureId"].as_str().unwrap_or("")) else { return Ok(failure("NOT_FOUND", None)) };
                if entry["kind"] != "screenshot" {
                    return Ok(failure("UNSUPPORTED_KIND", None));
                }
                let path = entry["filePath"].as_str().unwrap_or("").to_owned();
                match image::open(&path) {
                    Ok(i) => i,
                    Err(_) => return Ok(failure("EMPTY_IMAGE", Some(path))),
                }
            };
            if let Err(error) = copy_to_clipboard(&image) {
                tracing::warn!("[Captures] Failed to copy image: {error}");
                return Ok(failure("EMPTY_IMAGE", None));
            }
            ctx.state.track("capture.copy_image", json!({ "source": props["source"] }));
            Ok(json!({ "ok": true }))
        })
        .handle("captures:listMicrophones", |_, _| async move {
            Ok(json!({ "ok": false, "error": { "code": "RECORDER_NOT_READY", "message": "The recorder is not ready yet." } }))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_rules() {
        let original = Path::new(r"C:\x\shot.png");
        assert_eq!(renamed_path(original, "new name").unwrap(), Path::new(r"C:\x\new name.png"));
        assert!(renamed_path(original, "con").is_none());
        assert!(renamed_path(original, "a:b").is_none());
        assert!(renamed_path(original, ".hidden").is_none());
        assert!(renamed_path(original, "shot.png").is_none());
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Ok(Some((0, 9))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert!(parse_range(Some("bytes=200-"), 100).is_err());
        assert_eq!(title_from_file_name(Path::new("Doors 2024-01-02 03-04-05 (2).png")), "Doors");
    }
}
