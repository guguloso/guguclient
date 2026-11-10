//! Disk maintenance for Roblox builds: `versions:cleanup` (with the startup
//! and post-download triggers), `versions:getStorageBreakdown` and the
//! background version update scheduler. Port of `VersionsCleanupService`,
//! `VersionsService.cleanupCache`, `getStorageBreakdown` and
//! `BackgroundVersionUpdateService`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::ipc::Router;
use crate::services::instances::{product_of, version_hash_of};
use crate::services::roblox_layout::{self as layout, product_paths};
use crate::services::versions::{self, DOWNLOAD_PROGRESS};
use crate::state::AppState;
use crate::util::now_iso;

const VERSION_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const KEEP_PER_CHANNEL: usize = 2;
const SCAN_MAX_DEPTH: usize = 16;
const SCAN_MAX_ENTRIES: usize = 20_000;
const BACKGROUND_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const PRODUCTS: [&str; 2] = ["player", "studio"];

type CleanupFuture = Shared<BoxFuture<'static, Value>>;

#[derive(Default)]
pub struct MaintenanceState {
    cleanup_in_flight: Mutex<Option<CleanupFuture>>,
    background_run: AtomicBool,
    scheduler_generation: AtomicU64,
}

/// `getPathSizeBytes`: bounded iterative scan; `(bytes, truncated)`.
fn path_size(target: &Path, on_limit: &mut dyn FnMut(String)) -> (u64, bool) {
    let mut pending = vec![(target.to_path_buf(), 0usize)];
    let (mut total, mut visited) = (0u64, 0usize);
    let (mut warned_depth, mut truncated) = (false, false);
    while let Some((path, depth)) = pending.pop() {
        visited += 1;
        if visited > SCAN_MAX_ENTRIES {
            on_limit(format!("Size scan reached entry limit ({SCAN_MAX_ENTRIES}) for {}; reported size may be partial.", target.display()));
            truncated = true;
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_file() || meta.file_type().is_symlink() {
            total += meta.len();
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        if depth >= SCAN_MAX_DEPTH {
            if !warned_depth {
                on_limit(format!("Size scan reached depth limit ({SCAN_MAX_DEPTH}) for {}; reported size may be partial.", target.display()));
                warned_depth = true;
                truncated = true;
            }
            continue;
        }
        match std::fs::read_dir(&path) {
            Ok(entries) => pending.extend(entries.flatten().map(|e| (e.path(), depth + 1))),
            Err(error) => tracing::warn!("[StorageBreakdown] Failed to size {}: {error}", path.display()),
        }
    }
    (total, truncated)
}

fn size_of(target: &Path) -> u64 {
    path_size(target, &mut |message| tracing::warn!("[CacheCleanup] {message}")).0
}

/// `getStorageBreakdown`: bytes per category, largest first.
fn storage_breakdown(state: &AppState) -> Vec<Value> {
    let base = &state.paths.base;
    let player = product_paths(base, "player");
    let studio = product_paths(base, "studio");
    let categories: Vec<(&str, Vec<PathBuf>)> = vec![
        ("versions", vec![player.versions_dir.clone(), studio.versions_dir.clone()]),
        ("instanceData", vec![player.data_dir.clone(), studio.data_dir.clone()]),
        ("instanceConfigs", vec![player.instances_dir.clone(), studio.instances_dir.clone()]),
        ("thumbnailsCache", vec![state.paths.thumbnail_cache_dir.clone()]),
        ("avatarCache", vec![base.join("cache").join("assets")]),
        ("logs", vec![state.paths.logs_dir.clone(), base.join("main.log"), base.join("main.old.log")]),
        ("macros", vec![base.join("macros")]),
        ("outfits", vec![base.join("outfits")]),
        ("fastflags", vec![base.join("fastflags")]),
        ("temp", vec![player.temp_dir.clone(), studio.temp_dir.clone()]),
        ("settings", vec![state.paths.settings_dir.clone(), player.installed_versions_file.clone(), studio.installed_versions_file.clone()]),
    ];
    let mut entries: Vec<Value> = categories
        .into_iter()
        .map(|(category, paths)| {
            let mut truncated = false;
            let bytes: u64 = paths
                .iter()
                .map(|p| {
                    let (bytes, cut) = path_size(p, &mut |message| tracing::warn!("[StorageBreakdown] {message}"));
                    truncated |= cut;
                    bytes
                })
                .sum();
            let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            json!({ "bytes": bytes, "category": category, "paths": paths, "truncated": truncated })
        })
        .collect();
    entries.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
    entries
}

struct CleanupResult {
    trigger: String,
    started_at: String,
    started: SystemTime,
    bytes_freed: u64,
    removed_temp_entries: u64,
    removed_unused_versions: u64,
    removed_orphan_version_dirs: u64,
    removed_missing_metadata_entries: u64,
    cleaned_instance_caches: u64,
    skipped_instance_caches_running: u64,
    errors: Vec<String>,
}

impl CleanupResult {
    fn error(&mut self, message: String) {
        tracing::warn!("[CacheCleanup] {message}");
        self.errors.push(message);
    }
}

/// `ensureSafeCleanupPath`: only strict descendants of the launcher base.
fn ensure_safe(state: &AppState, kind: &str, target: &Path) -> Result<(), String> {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().trim_end_matches(['\\', '/']).to_owned();
    let base = norm(&state.paths.base);
    let target_s = norm(target);
    if target_s == base {
        return Err(format!("Unsafe {kind} {}: refusing to operate on launcher base directory directly", target.display()));
    }
    if !target_s.starts_with(&format!("{base}{}", std::path::MAIN_SEPARATOR)) {
        return Err(format!("Unsafe {kind} {}: path resolves outside launcher base directory", target.display()));
    }
    Ok(())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn parse_millis(iso: Option<&Value>) -> Option<u64> {
    iso.and_then(Value::as_str).and_then(crate::services::instances::parse_iso_millis)
}

/// `performCleanupCache` for one product.
async fn cleanup_product(state: &AppState, product: &str, trigger: &str, running: &HashSet<String>) -> CleanupResult {
    let started = SystemTime::now();
    let mut result = CleanupResult {
        trigger: trigger.to_owned(),
        started_at: now_iso(),
        started,
        bytes_freed: 0,
        removed_temp_entries: 0,
        removed_unused_versions: 0,
        removed_orphan_version_dirs: 0,
        removed_missing_metadata_entries: 0,
        cleaned_instance_caches: 0,
        skipped_instance_caches_running: 0,
        errors: Vec::new(),
    };
    if versions::is_downloading(state, None) {
        tracing::info!("[CacheCleanup] Waiting for active downloads to finish before cleanup.");
        while versions::is_downloading(state, None) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let base = state.paths.base.clone();
    let paths = product_paths(&base, product);

    // Temp entries.
    let _ = std::fs::create_dir_all(&paths.temp_dir);
    match std::fs::read_dir(&paths.temp_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let target = entry.path();
                let bytes = size_of(&target);
                if let Err(message) = ensure_safe(state, "temp cleanup path", &target) {
                    result.error(format!("Temp cleanup failed: {message}"));
                    break;
                }
                if !target.exists() {
                    continue;
                }
                match remove_path(&target) {
                    Ok(()) => {
                        result.removed_temp_entries += 1;
                        result.bytes_freed += bytes;
                    }
                    Err(error) => result.error(format!("Failed to remove temp entry {}: {error}", target.display())),
                }
            }
            let _ = std::fs::create_dir_all(&paths.temp_dir);
        }
        Err(error) => result.error(format!("Temp cleanup failed: {error}")),
    }

    // Orphan version directories (not in the registry).
    let metadata = versions::registry_entries(state, product);
    let known: HashSet<String> = metadata.iter().filter_map(|v| v.get("versionHash").and_then(Value::as_str).map(str::to_owned)).collect();
    if paths.versions_dir.exists() {
        for entry in std::fs::read_dir(&paths.versions_dir).into_iter().flatten().flatten() {
            if !entry.path().is_dir() || known.contains(entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let orphan = entry.path();
            if let Err(message) = ensure_safe(state, "orphan version path", &orphan) {
                result.error(format!("Orphan version cleanup failed: {message}"));
                break;
            }
            let bytes = size_of(&orphan);
            match std::fs::remove_dir_all(&orphan) {
                Ok(()) => {
                    result.removed_orphan_version_dirs += 1;
                    result.bytes_freed += bytes;
                }
                Err(error) => result.error(format!("Failed to remove orphan version dir {}: {error}", orphan.display())),
            }
        }
    }

    // Registry entries whose build is gone.
    let metadata = versions::registry_entries(state, product);
    let existing: Vec<Value> = metadata
        .iter()
        .filter(|v| layout::is_version_installed(&base, v.get("versionHash").and_then(Value::as_str).unwrap_or(""), product))
        .cloned()
        .collect();
    result.removed_missing_metadata_entries = (metadata.len() - existing.len()) as u64;
    if result.removed_missing_metadata_entries > 0 {
        versions::save_registry_entries(state, product, &existing);
    }

    // Unused versions past retention, keeping the newest per channel.
    let installed = versions::installed_versions(state, product);
    let used: HashSet<String> =
        state.instances.all(&base).iter().filter(|i| product_of(i) == product).map(|i| version_hash_of(i).to_owned()).collect();
    let mut by_channel: HashMap<String, Vec<&Value>> = HashMap::new();
    for v in &installed {
        by_channel.entry(v.get("channel").and_then(Value::as_str).unwrap_or("").to_owned()).or_default().push(v);
    }
    let mut protected = HashSet::new();
    for list in by_channel.values_mut() {
        list.sort_by_key(|v| std::cmp::Reverse(parse_millis(v.get("installedAt")).unwrap_or(0)));
        protected.extend(list.iter().take(KEEP_PER_CHANNEL).filter_map(|v| v.get("versionHash").and_then(Value::as_str).map(str::to_owned)));
    }
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let cutoff = now.saturating_sub(VERSION_RETENTION.as_millis() as u64);
    for v in &installed {
        let hash = v.get("versionHash").and_then(Value::as_str).unwrap_or("");
        if used.contains(hash) || protected.contains(hash) || v.get("source").and_then(Value::as_str) == Some("manual") {
            continue;
        }
        let Some(last) = parse_millis(v.get("lastUsedAt").filter(|l| !l.is_null()).or(v.get("installedAt"))) else { continue };
        if last >= cutoff {
            continue;
        }
        let Ok(dir) = layout::version_install_dir(&base, hash, product) else { continue };
        if let Err(message) = ensure_safe(state, "version prune path", &dir) {
            result.error(format!("Unused version pruning failed: {message}"));
            break;
        }
        let bytes = size_of(&dir);
        match versions::remove_version(state, hash, product).await {
            Ok(()) => {
                result.removed_unused_versions += 1;
                result.bytes_freed += bytes;
            }
            Err(error) => result.error(format!("Failed to prune version {hash}: {error}")),
        }
    }

    // Instance caches (skipped at startup: running state may be incomplete).
    let instances: Vec<_> = state.instances.all(&base).into_iter().filter(|i| product_of(i) == product).collect();
    if trigger == "startup" {
        result.skipped_instance_caches_running = instances.len() as u64;
        tracing::info!("[CacheCleanup] Skipped instance cache cleanup on startup because running instance state may be incomplete.");
    } else {
        let bundle = if product == "studio" { "com.roblox.RobloxStudio" } else { "com.roblox.Roblox" };
        for instance in instances {
            let id = instance.get("id").and_then(Value::as_str).unwrap_or("");
            if running.contains(id) {
                result.skipped_instance_caches_running += 1;
                continue;
            }
            let Ok(data_dir) = layout::instance_data_dir(&base, id, product) else { continue };
            let cache_dir = data_dir.join("Library").join("Caches").join(bundle);
            if let Err(message) = ensure_safe(state, "instance cache path", &cache_dir) {
                result.error(format!("Instance cache cleanup failed: {message}"));
                break;
            }
            let bytes = size_of(&cache_dir);
            let cleaned = (|| {
                if cache_dir.exists() {
                    std::fs::remove_dir_all(&cache_dir)?;
                }
                std::fs::create_dir_all(&cache_dir)
            })();
            match cleaned {
                Ok(()) => {
                    if bytes > 0 {
                        result.cleaned_instance_caches += 1;
                    }
                    result.bytes_freed += bytes;
                }
                Err(error) => result.error(format!("Failed to clean cache for instance {id}: {error}")),
            }
        }
    }
    tracing::info!(
        "[CacheCleanup] trigger={}, bytesFreed={}, removedTempEntries={}, removedUnusedVersions={}, removedOrphanVersionDirs={}, removedMissingMetadataEntries={}, cleanedInstanceCaches={}, skippedInstanceCachesRunning={}, errors={}",
        result.trigger,
        result.bytes_freed,
        result.removed_temp_entries,
        result.removed_unused_versions,
        result.removed_orphan_version_dirs,
        result.removed_missing_metadata_entries,
        result.cleaned_instance_caches,
        result.skipped_instance_caches_running,
        result.errors.len()
    );
    result
}

/// `combineCleanupResults`.
fn combine(player: CleanupResult, studio: CleanupResult) -> Value {
    let finished_at = now_iso();
    let started = player.started.min(studio.started);
    let started_at = if player.started <= studio.started { player.started_at.clone() } else { studio.started_at.clone() };
    let duration = SystemTime::now().duration_since(started).unwrap_or_default().as_millis() as u64;
    let mut errors = player.errors;
    errors.extend(studio.errors);
    json!({
        "trigger": player.trigger,
        "startedAt": started_at,
        "finishedAt": finished_at,
        "durationMs": duration,
        "bytesFreed": player.bytes_freed + studio.bytes_freed,
        "removedTempEntries": player.removed_temp_entries + studio.removed_temp_entries,
        "removedUnusedVersions": player.removed_unused_versions + studio.removed_unused_versions,
        "removedOrphanVersionDirs": player.removed_orphan_version_dirs + studio.removed_orphan_version_dirs,
        "removedMissingMetadataEntries": player.removed_missing_metadata_entries + studio.removed_missing_metadata_entries,
        "cleanedInstanceCaches": player.cleaned_instance_caches + studio.cleaned_instance_caches,
        "skippedInstanceCachesRunning": player.skipped_instance_caches_running + studio.skipped_instance_caches_running,
        "errors": errors,
    })
}

/// `cleanupCache`: concurrent callers share the run in flight.
pub async fn cleanup(state: &Arc<AppState>, trigger: &str) -> Value {
    let shared = {
        let mut slot = state.version_maintenance.cleanup_in_flight.lock();
        match slot.as_ref() {
            Some(running) => running.clone(),
            None => {
                let running: HashSet<String> = state.launch.running_ids().into_iter().collect();
                let state = Arc::clone(state);
                let trigger = trigger.to_owned();
                let future: BoxFuture<'static, Value> = async move {
                    let (player, studio) =
                        futures::join!(cleanup_product(&state, PRODUCTS[0], &trigger, &running), cleanup_product(&state, PRODUCTS[1], &trigger, &running));
                    let combined = combine(player, studio);
                    *state.version_maintenance.cleanup_in_flight.lock() = None;
                    combined
                }
                .boxed();
                let shared = future.shared();
                *slot = Some(shared.clone());
                shared
            }
        }
    };
    shared.await
}

/// `downloadVersion` waits for a cleanup in flight before downloading.
pub async fn wait_for_cleanup(state: &AppState) {
    let running = state.version_maintenance.cleanup_in_flight.lock().clone();
    if let Some(running) = running {
        running.await;
    }
}

/// `triggerPostDownloadCleanupIfEnabled`.
pub fn cleanup_after_download(state: &Arc<AppState>) {
    if !state.settings.get_bool("auto-clean-cache") {
        return;
    }
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        cleanup(&state, "post-download").await;
    });
}

/// The startup cleanup run when `auto-clean-cache` is on.
pub fn cleanup_on_startup(state: &Arc<AppState>) {
    if state.settings.get_bool("auto-clean-cache") {
        let state = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            cleanup(&state, "startup").await;
        });
    }
}

// ------------------------------------------------- background version updates

fn auto_check_mode(state: &AppState) -> String {
    match state.settings.get("auto-check-updates") {
        Some(Value::Bool(true)) => "check-only".into(),
        Some(Value::Bool(false)) => "disabled".into(),
        Some(Value::String(s)) => s,
        _ => "disabled".into(),
    }
}

/// `BackgroundVersionUpdateService.startStop`: (re)starts the 6-hour
/// scheduler with an immediate run, or stops it when disabled.
pub fn start_stop_background_updates(state: &Arc<AppState>) {
    let generation = state.version_maintenance.scheduler_generation.fetch_add(1, Ordering::SeqCst) + 1;
    if auto_check_mode(state) == "disabled" {
        tracing::debug!("[BackgroundVersionUpdate] Scheduler disabled by settings");
        return;
    }
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        let mut trigger = "startup";
        loop {
            let Some(state) = weak.upgrade() else { return };
            if state.version_maintenance.scheduler_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            schedule_run(&state, trigger);
            drop(state);
            tokio::time::sleep(BACKGROUND_CHECK_INTERVAL).await;
            trigger = "interval";
        }
    });
}

fn schedule_run(state: &Arc<AppState>, trigger: &'static str) {
    if state.version_maintenance.background_run.swap(true, Ordering::SeqCst) {
        tracing::debug!("[BackgroundVersionUpdate] Skipping {trigger} tick; run already in progress");
        return;
    }
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        run_once(&state, trigger).await;
        state.version_maintenance.background_run.store(false, Ordering::SeqCst);
    });
}

async fn run_once(state: &Arc<AppState>, trigger: &str) {
    let mode = auto_check_mode(state);
    if mode == "disabled" {
        tracing::debug!("[BackgroundVersionUpdate] Skipping {trigger} run; mode is disabled");
        return;
    }
    let mut channels: Vec<(String, String)> = Vec::new();
    for instance in state.instances.all(&state.paths.base) {
        let channel = instance.get("version").and_then(|v| v.get("channel")).and_then(Value::as_str).unwrap_or("").trim().to_owned();
        if channel.is_empty() {
            continue;
        }
        let entry = (product_of(&instance).to_owned(), channel);
        if let Some(pos) = channels.iter().position(|c| *c == entry) {
            channels.remove(pos);
        }
        channels.push(entry);
    }
    if channels.is_empty() {
        tracing::debug!("[BackgroundVersionUpdate] {trigger} run skipped; no instance channels");
        return;
    }
    let (mut found, mut attempted, mut succeeded, mut failures) = (0, 0, 0, 0);
    for (product, channel) in &channels {
        let result = match versions::check_for_update(state, channel, product).await {
            Ok(r) => r,
            Err(error) => {
                failures += 1;
                tracing::warn!("[BackgroundVersionUpdate] Failed processing channel {product}/{channel}: {error}");
                continue;
            }
        };
        if result["status"] == "channelUnavailable" {
            tracing::warn!("[BackgroundVersionUpdate] Skipping {product}/{channel}: channel unavailable");
            continue;
        }
        if result["hasUpdate"] != true {
            continue;
        }
        found += 1;
        if mode == "check-only" {
            tracing::info!("[BackgroundVersionUpdate] Update available for {product}/{channel}: {}", result["latestVersion"]["version"].as_str().unwrap_or(""));
            continue;
        }
        attempted += 1;
        let info = match serde_json::from_value(result["latestVersion"].clone()) {
            Ok(info) => info,
            Err(error) => {
                failures += 1;
                tracing::warn!("[BackgroundVersionUpdate] Failed processing channel {product}/{channel}: {error}");
                continue;
            }
        };
        let downloaded = versions::download_version(state, info, "automatic").await;
        if !versions::is_downloading(state, None) {
            state.events.emit_main(DOWNLOAD_PROGRESS, Value::Null);
        }
        match downloaded {
            Ok(_) => {
                succeeded += 1;
                cleanup_after_download(state);
            }
            Err(error) => {
                failures += 1;
                tracing::warn!("[BackgroundVersionUpdate] Failed processing channel {product}/{channel}: {error}");
            }
        }
    }
    tracing::info!(
        "[BackgroundVersionUpdate] {trigger} run complete (channels={}, mode={mode}, updates={found}, downloadsAttempted={attempted}, downloadsSucceeded={succeeded}, failures={failures})",
        channels.len()
    );
}

/// `shutdown`.
pub fn stop_background_updates(state: &AppState) {
    state.version_maintenance.scheduler_generation.fetch_add(1, Ordering::SeqCst);
}

pub fn register(router: &mut Router) {
    router
        .handle("versions:cleanup", |ctx, _| async move { Ok(cleanup(&ctx.state, "manual").await) })
        .handle("versions:getStorageBreakdown", |ctx, _| async move {
            let state = Arc::clone(&ctx.state);
            Ok(tauri::async_runtime::spawn_blocking(move || storage_breakdown(&state)).await.unwrap_or_default())
        });
}
