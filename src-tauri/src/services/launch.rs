//! Starting Roblox (`launch:*`). Port of `LaunchService` + `LaunchController`
//! for Windows: auto-update the pinned build, write the instance's
//! FastFlags, mint an authentication ticket, spawn `RobloxPlayerBeta.exe`
//! with a `roblox-player:` URL and watch the process until it exits.
//!
//! Pending: modded runtimes (instances with enabled mods or a custom font),
//! launch avatars, activity tracking, opening the second screen on launch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{Manager, WebviewWindow};

use crate::error::AppError;
use crate::ipc::Router;
use crate::services::game_avatar;
use crate::services::instances::{self, product_of, version_hash_of, Instance};
use crate::services::roblox_layout::{
    self as layout, ensure_instance_data_dir, is_version_installed, matches_process_name,
    platform_config, process_list, read_json, version_install_dir, SingletonMutex, PRODUCTS,
};
use crate::services::versions::{self, DOWNLOAD_PROGRESS};
use crate::state::AppState;

const PROCESS_EXIT_WAIT: Duration = Duration::from_secs(5);
const PROCESS_EXIT_POLL: Duration = Duration::from_millis(100);
const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_RUN_VALUE: &str = "RobloxPlayerBeta";
const DEFAULT_INSTANCE_NAMES: [(&str, &str); 2] =
    [("player", "My Roblox"), ("studio", "My Studio")];

/// `LaunchServiceError`: serialized to the renderer as `{ code, details }`.
#[derive(Debug)]
pub struct LaunchError {
    code: &'static str,
    details: String,
}

impl LaunchError {
    fn new(code: &'static str, details: impl Into<String>) -> Self {
        Self {
            code,
            details: details.into(),
        }
    }
    fn unknown(error: impl std::fmt::Display) -> Self {
        Self::new("UNKNOWN", error.to_string())
    }
}

impl From<AppError> for LaunchError {
    fn from(error: AppError) -> Self {
        Self::unknown(error)
    }
}

type LaunchResult<T> = Result<T, LaunchError>;

struct Running {
    avatar: Option<Arc<game_avatar::LaunchAvatar>>,
    pid: Option<u32>,
    place_id: Option<u64>,
    product: String,
    version_hash: String,
}

#[derive(Default)]
pub struct LaunchService {
    running: Mutex<HashMap<String, Running>>,
    mutex: SingletonMutex,
    /// Serializes single-instance close-and-spawn steps.
    single_instance_chain: tokio::sync::Mutex<()>,
    updating: Mutex<(usize, Vec<String>)>,
}

impl LaunchService {
    pub fn running_ids(&self) -> Vec<String> {
        self.running.lock().keys().cloned().collect()
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.lock().contains_key(id)
    }

    pub fn running_pids(&self) -> Vec<u32> {
        self.running.lock().values().filter_map(|r| r.pid).collect()
    }

    #[allow(dead_code)] // used by captures (activeGame) later
    pub fn instance_id_for_pid(&self, pid: u32) -> Option<String> {
        self.running
            .lock()
            .iter()
            .find(|(_, r)| r.pid == Some(pid))
            .map(|(id, _)| id.clone())
    }

}

fn multi_instance_enabled(state: &AppState) -> bool {
    state.feature_flags.is_enabled("feature-multi-instance")
        && state.settings.get_bool("enable-multi-instance")
}

fn closes_other_players(state: &AppState, product: &str) -> bool {
    product == "player" && !multi_instance_enabled(state)
}

fn find_product_pids(products: &[&str]) -> Vec<(u32, &'static str)> {
    let names: Vec<&'static str> = products
        .iter()
        .map(|p| platform_config(p).process_name)
        .collect();
    process_list()
        .into_iter()
        .filter_map(|p| {
            names
                .iter()
                .find(|n| matches_process_name(&p.name, n))
                .map(|n| (p.pid, *n))
        })
        .collect()
}

/// `isAnyRobloxProcessRunning`.
pub fn is_any_roblox_running(state: &AppState) -> bool {
    !state.launch.running.lock().is_empty() || !find_product_pids(&PRODUCTS).is_empty()
}

fn untracked_player_pids(state: &AppState) -> Vec<u32> {
    let tracked = state.launch.running_pids();
    find_product_pids(&["player"])
        .into_iter()
        .map(|(pid, _)| pid)
        .filter(|pid| !tracked.contains(pid))
        .collect()
}

fn other_running_player_ids(state: &AppState, id: &str) -> Vec<String> {
    state
        .launch
        .running
        .lock()
        .iter()
        .filter(|(k, r)| *k != id && r.product == "player")
        .map(|(k, _)| k.clone())
        .collect()
}

async fn wait_for_pid_exit(pid: u32) -> bool {
    let deadline = tokio::time::Instant::now() + PROCESS_EXIT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if !layout::is_pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(PROCESS_EXIT_POLL).await;
    }
    tracing::warn!(
        "PID {pid} still present after {}ms",
        PROCESS_EXIT_WAIT.as_millis()
    );
    false
}

/// `killInstance`.
pub fn kill_instance(state: &AppState, id: &str) -> bool {
    let pid = match state.launch.running.lock().get(id) {
        Some(r) => r.pid,
        None => return false,
    };
    let Some(pid) = pid else {
        handle_exit(state, id, None);
        return false;
    };
    let result = layout::kill_pid(pid);
    handle_exit(state, id, None);
    match result {
        Ok(_) => {
            tracing::info!("Killed instance {id}");
            true
        }
        Err(error) => {
            tracing::error!("Failed to kill instance {id}: {error}");
            false
        }
    }
}

async fn ensure_stopped(state: &AppState, id: &str) -> bool {
    let pid = match state.launch.running.lock().get(id) {
        Some(r) => r.pid,
        None => return true,
    };
    tracing::info!("Instance '{id}' already running; closing its window before launch");
    kill_instance(state, id);
    match pid {
        None => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            true
        }
        Some(pid) => wait_for_pid_exit(pid).await,
    }
}

async fn ensure_untracked_stopped(pid: u32) -> bool {
    tracing::info!("Untracked Roblox Player (pid {pid}) running; closing it before launch");
    match layout::kill_pid(pid) {
        Ok(false) => true,
        Ok(true) => wait_for_pid_exit(pid).await,
        Err(error) => {
            tracing::error!("Failed to kill untracked Roblox Player (pid {pid}): {error}");
            false
        }
    }
}

async fn close_other_players(state: &AppState, id: &str) -> LaunchResult<()> {
    let mut stopped = Vec::new();
    for other in other_running_player_ids(state, id) {
        stopped.push(ensure_stopped(state, &other).await);
    }
    for pid in untracked_player_pids(state) {
        stopped.push(ensure_untracked_stopped(pid).await);
    }
    if stopped.contains(&false) {
        return Err(LaunchError::new(
            "PLAYER_CLOSE_FAILED",
            "A running Roblox Player did not exit before the single-instance launch",
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------ updating notice

fn begin_updating(state: &AppState, product: &str, initiator: Option<&WebviewWindow>) {
    let mut updating = state.launch.updating.lock();
    updating.0 += 1;
    let label = initiator
        .map(|w| w.label().to_owned())
        .unwrap_or_else(|| crate::MAIN_WINDOW.to_owned());
    if !updating.1.contains(&label) {
        if let Some(window) = state.app.get_webview_window(&label) {
            let _ = tauri::Emitter::emit(
                &window,
                "gugu:ipc-event",
                json!({ "channel": "launch:updating", "payload": { "product": product } }),
            );
        }
        updating.1.push(label);
    }
}

fn end_updating(state: &AppState) {
    let mut updating = state.launch.updating.lock();
    updating.0 = updating.0.saturating_sub(1);
    if updating.0 != 0 {
        return;
    }
    for label in updating.1.drain(..) {
        if let Some(window) = state.app.get_webview_window(&label) {
            let _ = tauri::Emitter::emit(
                &window,
                "gugu:ipc-event",
                json!({ "channel": "launch:updating", "payload": null }),
            );
        }
    }
}

fn finish_download_progress(state: &AppState) {
    if !versions::is_downloading(state, None) {
        state.events.emit(DOWNLOAD_PROGRESS, Value::Null);
    }
}

/// `autoUpdateInstance`.
async fn auto_update(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    initiator: Option<&WebviewWindow>,
) -> LaunchResult<Instance> {
    if !instance
        .get("autoUpdate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(instance);
    }
    let product = product_of(&instance).to_owned();
    let channel = instance
        .get("version")
        .and_then(|v| v.get("channel"))
        .and_then(Value::as_str)
        .unwrap_or("LIVE")
        .to_owned();
    let latest = versions::latest_version(state, &channel, &product).await?;
    let current = version_hash_of(&instance).to_owned();
    if current == latest.client_version_upload {
        return Ok(instance);
    }
    tracing::info!(
        "Update available for instance '{}' ({current} -> {}), downloading...",
        instance_name(&instance),
        latest.client_version_upload
    );
    begin_updating(state, &product, initiator);
    let result = async {
        let installed = versions::download_version(state, latest, "automatic").await?;
        let mut patch = serde_json::Map::new();
        patch.insert(
            "version".into(),
            json!({
                "versionHash": installed.get("versionHash"),
                "displayVersion": installed.get("displayVersion"),
                "channel": installed.get("channel"),
            }),
        );
        instances::update(state, id, patch);
        state
            .instances
            .get(&state.paths.base, id)
            .ok_or_else(|| AppError::msg(format!("Instance '{id}' not found after update")))
    }
    .await;
    end_updating(state);
    finish_download_progress(state);
    match result {
        Ok(updated) => Ok(updated),
        Err(error) => {
            tracing::warn!("Auto-update failed, launching with current version: {error}");
            if !is_version_installed(&state.paths.base, &current, &product) {
                return Err(error.into());
            }
            Ok(instance)
        }
    }
}

// ------------------------------------------------------------------ launch pieces

struct AuthContext {
    pinned: Option<u64>,
    has_cookie: bool,
    precomputed_ticket: Option<String>,
}

fn describe(auth: &AuthContext) -> String {
    match auth.pinned {
        None => "active account".to_owned(),
        Some(id) => format!("pinned account {id}"),
    }
}

async fn resolve_auth(
    state: &AppState,
    instance: &Instance,
    options: Option<&Value>,
) -> LaunchResult<AuthContext> {
    if product_of(instance) != "player" {
        return Ok(AuthContext {
            pinned: None,
            has_cookie: false,
            precomputed_ticket: None,
        });
    }
    let pinned = instance.get("accountUserId").and_then(Value::as_u64);
    let has_cookie = state.auth.stored_cookie(pinned).is_some();
    let mut context = AuthContext {
        pinned,
        has_cookie,
        precomputed_ticket: None,
    };
    let Some(pinned) = pinned else {
        return Ok(context);
    };
    tracing::info!("Instance is pinned to account {pinned} for launch");
    if !has_cookie {
        return Err(LaunchError::new(
            "ACCOUNT_NOT_FOUND",
            format!("Pinned account {pinned} is missing or has no stored session"),
        ));
    }
    if options.is_some() {
        return Ok(context);
    }
    match state.auth.get_auth_ticket(Some(pinned)).await {
        Ok(ticket) => {
            context.precomputed_ticket = Some(ticket);
            Ok(context)
        }
        Err(error) => {
            tracing::warn!("Pinned account auth preflight failed for user {pinned}: {error}");
            Err(LaunchError::new("ACCOUNT_AUTH_FAILED", error.to_string()))
        }
    }
}

struct LaunchTarget {
    version_hash: String,
    binary_path: PathBuf,
    client_settings_dir: PathBuf,
}

/// `validateLaunchTarget` (modded runtimes are not ported yet).
fn validate_target(state: &AppState, instance: &Instance) -> LaunchResult<LaunchTarget> {
    let base = &state.paths.base;
    let hash = version_hash_of(instance).to_owned();
    let product = product_of(instance);
    if !is_version_installed(base, &hash, product) {
        let display = instance
            .get("version")
            .and_then(|v| v.get("displayVersion"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(LaunchError::new(
            "VERSION_NOT_INSTALLED",
            format!("Version '{display}' ({hash}) is not installed. Please download it first."),
        ));
    }
    let mut install_dir = version_install_dir(base, &hash, product)?;
    if product == "player" {
        match crate::services::mods::prepare_runtime(state, instance) {
            Ok(Some(runtime)) => install_dir = runtime,
            Ok(None) => {}
            Err(error) => return Err(LaunchError::new("MOD_RUNTIME_PREP_FAILED", error)),
        }
    }
    let binary_path = layout::binary_path(&install_dir, product);
    if !binary_path.exists() {
        return Err(LaunchError::new(
            "BINARY_NOT_FOUND",
            format!("Roblox binary not found at: {}", binary_path.display()),
        ));
    }
    Ok(LaunchTarget {
        version_hash: hash,
        binary_path,
        client_settings_dir: install_dir.join("ClientSettings"),
    })
}

/// `applyInstanceFastFlags`: `ClientSettings/ClientAppSettings.json`.
fn apply_fast_flags(instance: &Instance, dir: &std::path::Path) -> std::io::Result<()> {
    if product_of(instance) != "player" {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let mut settings = serde_json::Map::new();
    let fps = instance
        .get("fpsCap")
        .filter(|v| v.as_f64().is_some_and(|f| f != 0.0))
        .cloned()
        .unwrap_or(json!(60));
    settings.insert("DFIntTaskSchedulerTargetFps".into(), fps);
    if let Some(flags) = instance.get("fastFlags").and_then(Value::as_object) {
        settings.extend(flags.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    std::fs::write(
        dir.join("ClientAppSettings.json"),
        serde_json::to_vec_pretty(&Value::Object(settings))?,
    )
}

fn hidden_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// `queryStartupRegistration` via `reg query`: `(command, type)`.
fn query_startup_registration() -> Option<(String, String)> {
    let output = hidden_command("reg")
        .args(["query", STARTUP_RUN_KEY, "/v", STARTUP_RUN_VALUE])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!("No Roblox startup registration present");
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with(STARTUP_RUN_VALUE))?;
    let rest = line.trim_start()[STARTUP_RUN_VALUE.len()..].trim_start();
    let (kind, data) = rest.split_once(char::is_whitespace)?;
    if !kind.starts_with("REG_") || !kind.ends_with("SZ") {
        return Some((String::new(), "REG_SZ".into()));
    }
    Some((data.trim().to_owned(), kind.to_owned()))
}

/// `removeStartupRegistration`: backs the value up, then deletes it.
fn remove_startup_registration(state: &AppState) {
    let Some((command, kind)) = query_startup_registration() else {
        return;
    };
    if !command.is_empty() {
        let backup =
            json!({ "command": command, "type": kind, "capturedAt": crate::util::now_iso() });
        if let Err(error) = layout::write_json(
            &state.paths.settings_dir.join("roblox-startup-backup.json"),
            &backup,
        ) {
            tracing::warn!("Could not back up Roblox startup registration: {error}");
        }
    }
    match hidden_command("reg")
        .args(["delete", STARTUP_RUN_KEY, "/v", STARTUP_RUN_VALUE, "/f"])
        .status()
    {
        Ok(status) if status.success() => {
            tracing::info!("Removed Roblox launch-at-startup registration")
        }
        Ok(status) => tracing::warn!("Failed to remove Roblox startup registration: {status}"),
        Err(error) => tracing::warn!("Failed to remove Roblox startup registration: {error}"),
    }
}

/// `applyAppStorageOverrides`: keeps Roblox out of the tray and startup.
fn apply_app_storage_overrides(state: &AppState, instance: &Instance) {
    if product_of(instance) != "player" {
        return;
    }
    let disable_tray = state.settings.get_bool("disable-roblox-tray");
    let disable_startup = state.settings.get_bool("disable-roblox-startup");
    let mut overrides: Vec<(&str, &str)> = Vec::new();
    if disable_tray {
        overrides.extend([
            ("MinimizeToTray", "false"),
            ("SystemTrayModalShown", "true"),
        ]);
    }
    if disable_startup {
        overrides.push(("LaunchAtStartup", "false"));
    }
    if !overrides.is_empty() {
        patch_app_storage(&overrides);
    }
    if disable_startup {
        remove_startup_registration(state);
    }
}

/// `patchAppStorageFile` on the system `appStorage.json`.
fn patch_app_storage(overrides: &[(&str, &str)]) {
    let Some(path) = layout::system_app_storage() else { return };
    {
        let mut storage = match read_json(&path) {
            Some(Value::Object(map)) => map,
            Some(_) => {
                tracing::warn!(
                    "Non-object appStorage.json at {}; resetting",
                    path.display()
                );
                serde_json::Map::new()
            }
            None => serde_json::Map::new(),
        };
        let mut changed = false;
        for &(key, value) in overrides {
            if storage.get(key).and_then(Value::as_str) != Some(value) {
                storage.insert(key.to_owned(), Value::String(value.to_owned()));
                changed = true;
            }
        }
        if changed {
            let bytes = serde_json::to_vec(&Value::Object(storage)).unwrap_or_default();
            let tmp = path.with_extension("json.lunar-for-roblox.tmp");
            let result = std::fs::create_dir_all(path.parent().expect("appStorage has a parent"))
                .and_then(|()| std::fs::write(&tmp, bytes))
                .and_then(|()| std::fs::rename(&tmp, &path));
            match result {
                Ok(()) => {
                    tracing::debug!("Patched Roblox appStorage overrides at {}", path.display())
                }
                Err(error) => tracing::warn!(
                    "Failed to apply appStorage overrides at {}: {error}",
                    path.display()
                ),
            }
        }
    }
}

/// `restoreStartupRunValue`: puts Roblox's launch-at-startup entry back
/// (from the backup, or the newest system install).
pub fn restore_startup_registration(state: &AppState) {
    if query_startup_registration().is_some() {
        tracing::debug!("Roblox startup registration already present");
        return;
    }
    patch_app_storage(&[("LaunchAtStartup", "true")]);
    let backup = read_json(&state.paths.settings_dir.join("roblox-startup-backup.json")).filter(|b| {
        b.get("command").and_then(Value::as_str).is_some_and(|c| !c.is_empty())
            && matches!(b.get("type").and_then(Value::as_str), Some("REG_SZ" | "REG_EXPAND_SZ"))
    });
    let mut entry = None;
    if let Some(backup) = backup {
        let command = backup["command"].as_str().unwrap_or("").to_owned();
        let exe = match command.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or("").to_owned(),
            None => command.split_whitespace().next().unwrap_or("").to_owned(),
        };
        if !exe.is_empty() && std::path::Path::new(&exe).exists() {
            entry = Some((command, backup["type"].as_str().unwrap_or("REG_SZ").to_owned()));
        } else {
            tracing::debug!("Saved Roblox startup command is stale; using newest install");
        }
    }
    if entry.is_none() {
        let versions = dirs::data_local_dir().map(|d| d.join("Roblox").join("Versions"));
        let newest = versions
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("version-"))
            .map(|e| e.path().join("RobloxPlayerBeta.exe"))
            .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
            .max_by_key(|(t, _)| *t)
            .map(|(_, p)| p);
        entry = newest.map(|p| (format!("\"{}\" --launch-to-tray", p.display()), "REG_SZ".to_owned()));
    }
    let Some((command, kind)) = entry else {
        tracing::info!("No Roblox startup command to restore; re-enabled the json flag only");
        return;
    };
    match hidden_command("reg").args(["add", STARTUP_RUN_KEY, "/v", STARTUP_RUN_VALUE, "/t", &kind, "/d", &command, "/f"]).status() {
        Ok(status) if status.success() => tracing::info!("Restored Roblox launch-at-startup registration"),
        Ok(status) => tracing::warn!("Failed to restore Roblox startup registration: {status}"),
        Err(error) => tracing::warn!("Failed to restore Roblox startup registration: {error}"),
    }
}

fn str_opt(options: &Value, key: &str) -> String {
    match options.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    }
}

/// `buildPlaceLauncherUrl`.
fn place_launcher_url(options: &Value) -> String {
    let base = "https://assetgame.roblox.com/game/PlaceLauncher.ashx";
    let place = str_opt(options, "placeId");
    match options.get("type").and_then(Value::as_str).unwrap_or("") {
        "followUser" => format!(
            "{base}?request=RequestFollowUser&placeId={place}&userId={}",
            str_opt(options, "userId")
        ),
        "gameJob" => format!(
            "{base}?request=RequestGameJob&placeId={place}&gameId={}",
            str_opt(options, "gameInstanceId")
        ),
        "privateServer" => format!(
            "{base}?request=RequestPrivateGame&placeId={place}&accessCode={}",
            str_opt(options, "accessCode")
        ),
        "reservedServer" => format!(
            "{base}?request=RequestReservedGame&placeId={place}&reservedServerAccessCode={}",
            str_opt(options, "reservedServerAccessCode")
        ),
        "linkCode" => format!(
            "{base}?request=RequestPrivateGame&placeId={place}&linkCode={}",
            str_opt(options, "linkCode")
        ),
        _ => format!("{base}?request=RequestGame&placeId={place}&isPlayTogetherGame=false"),
    }
}

/// `buildFallbackUrl`.
fn fallback_url(options: &Value) -> String {
    let place = str_opt(options, "placeId");
    let start = format!("roblox://experiences/start?placeId={place}");
    match options.get("type").and_then(Value::as_str).unwrap_or("") {
        "deepLink" => str_opt(options, "rawUrl"),
        "followUser" => format!("{start}&userId={}", str_opt(options, "userId")),
        "gameJob" => format!(
            "{start}&gameInstanceId={}",
            str_opt(options, "gameInstanceId")
        ),
        "privateServer" => format!("{start}&accessCode={}", str_opt(options, "accessCode")),
        "reservedServer" => format!(
            "{start}&reservedServerAccessCode={}",
            str_opt(options, "reservedServerAccessCode")
        ),
        "linkCode" => format!("{start}&linkCode={}", str_opt(options, "linkCode")),
        _ => start,
    }
}

/// `replaceGameinfo`: swaps the ticket inside a `roblox-player:` deep link.
fn replace_gameinfo(url: &str, ticket: &str) -> String {
    url.split('+')
        .map(|part| {
            if part.starts_with("gameinfo:") {
                format!("gameinfo:{ticket}")
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

async fn build_launch_url(
    state: &AppState,
    instance: &Instance,
    options: Option<&Value>,
    auth: &AuthContext,
) -> LaunchResult<Option<String>> {
    let kind = options.and_then(|o| o.get("type")).and_then(Value::as_str);
    if product_of(instance) == "studio" {
        let Some(options) = options else {
            return Ok(None);
        };
        return match kind {
            Some("studioEditPlace" | "studioEditAsset") => {
                build_studio_url(state, options, auth).await
            }
            other => Err(LaunchError::new(
                "STUDIO_LAUNCH_MODE_UNSUPPORTED",
                format!(
                    "Unsupported Studio launch option: {}",
                    other.unwrap_or("undefined")
                ),
            )),
        };
    }
    if let Some(k @ ("studioEditPlace" | "studioEditAsset")) = kind {
        return Err(LaunchError::new(
            "STUDIO_OPTIONS_ON_PLAYER",
            format!("Player instance received Studio launch option: {k}"),
        ));
    }
    let now = crate::util::now_millis();
    if let Some(options) = options {
        let ticket = match state.auth.get_auth_ticket(auth.pinned).await {
            Ok(ticket) => ticket,
            Err(error) if !auth.has_cookie => {
                tracing::warn!("Auth ticket failed, falling back: {error}");
                return Ok(Some(fallback_url(options)));
            }
            Err(error) => {
                tracing::warn!("Launch auth failed for {}: {error}", describe(auth));
                return Err(LaunchError::new("ACCOUNT_AUTH_FAILED", error.to_string()));
            }
        };
        let place = str_opt(options, "placeId");
        if kind == Some("deepLink") {
            tracing::info!("Launching game with placeId: {place} (deep link, re-authenticated)");
            return Ok(Some(replace_gameinfo(&str_opt(options, "rawUrl"), &ticket)));
        }
        tracing::info!(
            "Launching {} with placeId: {place} (authenticated)",
            kind.unwrap_or("game")
        );
        let encoded = urlencoding::encode(&place_launcher_url(options)).into_owned();
        return Ok(Some(
            [
                "roblox-player:1".to_owned(),
                "launchmode:play".to_owned(),
                format!("gameinfo:{ticket}"),
                format!("launchtime:{now}"),
                format!("placelauncherurl:{encoded}"),
                "robloxLocale:en_us".to_owned(),
                "gameLocale:en_us".to_owned(),
            ]
            .join("+"),
        ));
    }
    if !auth.has_cookie {
        return Ok(None);
    }
    let ticket = match &auth.precomputed_ticket {
        Some(ticket) => ticket.clone(),
        None => state
            .auth
            .get_auth_ticket(auth.pinned)
            .await
            .map_err(|error| {
                tracing::warn!("Launch auth failed for {}: {error}", describe(auth));
                LaunchError::new("ACCOUNT_AUTH_FAILED", error.to_string())
            })?,
    };
    Ok(Some(
        [
            "roblox-player:1".to_owned(),
            "launchmode:app".to_owned(),
            format!("gameinfo:{ticket}"),
            format!("launchtime:{now}"),
            "robloxLocale:en_us".to_owned(),
            "gameLocale:en_us".to_owned(),
        ]
        .join("+"),
    ))
}

async fn build_studio_url(
    state: &AppState,
    options: &Value,
    auth: &AuthContext,
) -> LaunchResult<Option<String>> {
    let ticket = match state.auth.get_auth_ticket(auth.pinned).await {
        Ok(ticket) => ticket,
        Err(error) if !auth.has_cookie => {
            tracing::warn!("Studio auth ticket failed, launching without URL: {error}");
            return Ok(None);
        }
        Err(error) => return Err(LaunchError::new("ACCOUNT_AUTH_FAILED", error.to_string())),
    };
    let edit_place = options.get("type").and_then(Value::as_str) == Some("studioEditPlace");
    let pairs = if edit_place {
        let place: u64 = options.get("placeId").and_then(Value::as_u64).unwrap_or(0);
        let url = format!("https://apis.roblox.com/universes/v1/places/{place}/universe");
        match state
            .roblox
            .get::<Value>(&url)
            .await
            .ok()
            .and_then(|r| r.get("universeId").and_then(Value::as_u64))
        {
            Some(universe) => vec![format!("placeId:{place}"), format!("universeId:{universe}")],
            None => {
                tracing::warn!("Studio EditPlace aborted: no universe for placeId={place}");
                return Ok(None);
            }
        }
    } else {
        vec![format!("assetId:{}", str_opt(options, "assetId"))]
    };
    let task = if edit_place { "EditPlace" } else { "EditAsset" };
    let mut parts = vec![
        "roblox-studio:1".to_owned(),
        "launchmode:edit".to_owned(),
        format!("task:{task}"),
        format!("gameinfo:{ticket}"),
        format!("launchtime:{}", crate::util::now_millis()),
    ];
    parts.extend(pairs);
    parts.extend([
        "robloxLocale:en_us".to_owned(),
        "gameLocale:en_us".to_owned(),
    ]);
    Ok(Some(parts.join("+")))
}

// ------------------------------------------------------------------ process

fn spawn_roblox(
    binary: &std::path::Path,
    url: Option<&str>,
    env: &[(String, String)],
) -> std::io::Result<u32> {
    let mut command = std::process::Command::new(binary);
    if let Some(url) = url {
        command.arg(url);
    }
    command.current_dir(binary.parent().unwrap_or(std::path::Path::new(".")));
    command.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    Ok(command.spawn()?.id())
}

/// `handleProcessExit` + the `onExit` callback registered at startup.
fn handle_exit(state: &AppState, id: &str, code: Option<i32>) {
    let Some(running) = state.launch.running.lock().remove(id) else {
        return;
    };
    if let Some(avatar) = &running.avatar {
        avatar.release(state);
    }
    instances::mark_stopped(state, id);
    state.running.unregister(state, id);
    if running.product == "player" {
        if let Some(state) = state.arc() {
            crate::services::activity::stop_tracking(&state, id);
        }
    }
    let code = code.unwrap_or(-1);
    tracing::info!(
        "Roblox exited: instance={id}, product={}, version={}, code={code}",
        running.product,
        running.version_hash
    );
    state.track(
        "launch.exit",
        json!({ "instance_id": id, "product": running.product, "version_hash": running.version_hash, "exit_code": code }),
    );
    let _ = running.place_id;
    state.events.emit_main(
        "launch:exited",
        json!({ "instanceId": id, "product": running.product, "versionHash": running.version_hash, "code": code }),
    );
}

fn monitor(state: &Arc<AppState>, id: String, pid: Option<u32>) {
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(MONITOR_INTERVAL).await;
            let Some(state) = weak.upgrade() else { break };
            if !state.launch.is_running(&id) {
                break;
            }
            let alive = pid.is_some_and(layout::is_pid_alive);
            if !alive {
                handle_exit(&state, &id, None);
                break;
            }
        }
    });
}

async fn launch_instance(
    state: &Arc<AppState>,
    id: &str,
    options: Option<Value>,
    initiator: Option<WebviewWindow>,
) -> LaunchResult<()> {
    let base = state.paths.base.clone();
    let found = state.instances.get(&base, id).ok_or_else(|| {
        LaunchError::new("INSTANCE_NOT_FOUND", format!("Instance '{id}' not found"))
    })?;
    let instance = auto_update(state, id, found, initiator.as_ref()).await?;
    let product = product_of(&instance).to_owned();
    let options = options.or_else(|| {
        let place = instance
            .get("autoLaunchPlaceId")
            .filter(|v| !v.is_null() && *v != &json!(0))?;
        (product == "player").then(|| json!({ "type": "game", "placeId": place }))
    });
    let auth = resolve_auth(state, &instance, options.as_ref()).await?;
    let place_id = options
        .as_ref()
        .and_then(|o| o.get("placeId"))
        .and_then(Value::as_u64);
    let avatar = game_avatar::prepare_for_launch(state, &instance, place_id);
    let result = prepare_and_spawn(state, id, &instance, options.as_ref(), &auth, place_id, avatar.clone()).await;
    if let Err(error) = result {
        if let Some(avatar) = &avatar {
            avatar.release(state);
        }
        return Err(error);
    }
    if state.settings.get_bool("close-on-launch") {
        if let Some(window) = state.app.get_webview_window(crate::MAIN_WINDOW) {
            let _ = window.close();
        }
    }
    Ok(())
}

/// `prepareAndSpawn`: runtime, version and auth, the launch avatar, then the process.
async fn prepare_and_spawn(
    state: &Arc<AppState>,
    id: &str,
    instance: &Instance,
    options: Option<&Value>,
    auth: &AuthContext,
    place_id: Option<u64>,
    avatar: Option<Arc<game_avatar::LaunchAvatar>>,
) -> LaunchResult<()> {
    let base = state.paths.base.clone();
    let product = product_of(instance).to_owned();
    ensure_stopped(state, id).await;
    let target = validate_target(state, instance)?;
    if let Err(error) = apply_fast_flags(instance, &target.client_settings_dir) {
        return Err(LaunchError::unknown(error));
    }
    apply_app_storage_overrides(state, instance);

    let mut env: Vec<(String, String)> = Vec::new();
    if instance
        .get("isolated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let fake_home = ensure_instance_data_dir(&base, id, &product)?;
        let tmp = format!("{}/tmp", fake_home.display());
        std::fs::create_dir_all(&tmp).map_err(LaunchError::unknown)?;
        env.push(("HOME".into(), fake_home.display().to_string()));
        env.push(("TMPDIR".into(), tmp));
        tracing::info!(
            "Launching instance '{}' with HOME={}",
            instance_name(instance),
            fake_home.display()
        );
    }
    let launch_url = build_launch_url(state, instance, options, auth).await?;
    if let Some(avatar) = &avatar {
        avatar.ready().await;
    }

    let single = closes_other_players(state, &product);
    let spawn = || async {
        if single {
            close_other_players(state, id).await?;
        }
        if single {
            state.launch.mutex.release();
        } else if product == "player" && !state.launch.mutex.acquire() {
            return Err(LaunchError::new(
                "MUTEX_ACQUIRE_FAILED",
                "Failed to acquire ROBLOX singleton mutex on Windows",
            ));
        }
        let excluded = if product == "player" { state.running.snapshot_player_log_files(state, id) } else { Vec::new() };
        // CreateProcess blocks ~15s while Roblox's protected binary loads
        // (same in the Electron build), so keep it off the async workers.
        let (binary, url, child_env) =
            (target.binary_path.clone(), launch_url.clone(), env.clone());
        let pid = tauri::async_runtime::spawn_blocking(move || {
            spawn_roblox(&binary, url.as_deref(), &child_env)
        })
        .await
        .map_err(LaunchError::unknown)?
        .map_err(|error| {
            tracing::error!("Failed to launch Roblox: {error}");
            LaunchError::unknown(error)
        })?;
        instances::mark_launched(state, id);
        versions::mark_version_used(state, &product, &target.version_hash);
        let name = instance
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        state.running.register(state, id, &name, &product, excluded);
        state.launch.running.lock().insert(
            id.to_owned(),
            Running {
                avatar: avatar.clone(),
                pid: Some(pid),
                place_id,
                product: product.clone(),
                version_hash: target.version_hash.clone(),
            },
        );
        monitor(state, id.to_owned(), Some(pid));
        if product == "player" {
            crate::services::activity::start_tracking(state, id, crate::services::activity::launch_context(options));
        }
        if product == "player" && state.settings.get_bool("open-second-screen-on-launch") {
            crate::services::mission_control::open(state, Some(id));
        }
        tracing::info!("Successfully launched instance '{name}' ({id})");
        Ok(())
    };
    if single {
        let _guard = state.launch.single_instance_chain.lock().await;
        spawn().await?;
    } else {
        spawn().await?;
    }
    Ok(())
}

async fn create_default_instance(
    state: &Arc<AppState>,
    product: &str,
    initiator: Option<&WebviewWindow>,
) -> Result<Instance, AppError> {
    let latest = versions::latest_version(state, "LIVE", product).await?;
    if !is_version_installed(&state.paths.base, &latest.client_version_upload, product) {
        begin_updating(state, product, initiator);
        let result = versions::download_version(state, latest.clone(), "automatic").await;
        end_updating(state);
        finish_download_progress(state);
        result?;
    }
    let name = DEFAULT_INSTANCE_NAMES
        .iter()
        .find(|(p, _)| *p == product)
        .map_or("My Roblox", |(_, n)| n);
    instances::create(
        state,
        instances::CreateOptions {
            name: name.to_owned(),
            product: product.to_owned(),
            version: json!({ "versionHash": latest.client_version_upload, "displayVersion": latest.version, "channel": "LIVE" }),
            auto_update: Some(true),
            account_user_id: None,
            description: None,
            icon_color: None,
            copy_from_instance_id: None,
        },
    )
}

fn kill_products(state: &AppState, products: &[&str]) -> bool {
    let mut failed = Vec::new();
    for (pid, name) in find_product_pids(products) {
        if let Err(error) = layout::kill_pid(pid) {
            tracing::error!("Failed to kill {name}: {error}");
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        return false;
    }
    for id in state.launch.running_ids() {
        handle_exit(state, &id, None);
    }
    true
}

pub fn register(router: &mut Router) {
    router
        .handle("launch:instance", |ctx, args| async move {
            let id: String = args.get(0)?;
            let options: Option<Value> = args.opt(1)?;
            let state = &ctx.state;
            let launch_id = uuid::Uuid::new_v4().to_string();
            let started = std::time::Instant::now();
            let place_id = options.as_ref().and_then(|o| o.get("placeId")).cloned().unwrap_or(Value::Null);
            state.track("launch.init", json!({ "instance_id": id, "launch_id": launch_id, "place_id": place_id }));
            match launch_instance(state, &id, options, Some(ctx.window.clone())).await {
                Ok(()) => {
                    state.track(
                        "launch.success",
                        json!({ "instance_id": id, "launch_id": launch_id, "duration_ms": started.elapsed().as_millis() as u64 }),
                    );
                    Ok(json!({ "ok": true }))
                }
                Err(error) => {
                    tracing::error!("Failed to launch Roblox: {} {}", error.code, error.details);
                    state.track("launch.failure", json!({ "instance_id": id, "launch_id": launch_id, "exit_code": null }));
                    Ok(json!({ "ok": false, "error": { "code": error.code, "details": error.details } }))
                }
            }
        })
        .handle("launch:createDefaultInstance", |ctx, args| async move {
            let product: String = args.get(0)?;
            let from_deeplink: Option<Value> = args.opt(1)?;
            let instance = create_default_instance(&ctx.state, &product, Some(&ctx.window)).await?;
            ctx.state.track(
                "instance.create_default",
                json!({ "instance_id": instance.get("id"), "product": product, "from_deeplink": from_deeplink }),
            );
            Ok(instance)
        })
        .handle("launch:isRunning", |ctx, _| async move { Ok(!ctx.state.launch.running.lock().is_empty()) })
        .handle("launch:isAnyRunning", |ctx, _| async move {
            Ok(!ctx.state.launch.running.lock().is_empty() || !find_product_pids(&PRODUCTS).is_empty())
        })
        .handle("launch:getRunningConflict", |ctx, args| async move {
            let id: String = args.get(0)?;
            let state = &ctx.state;
            let Some(instance) = state.instances.get(&state.paths.base, &id) else { return Ok(Value::Null) };
            if closes_other_players(state, product_of(&instance))
                && (!other_running_player_ids(state, &id).is_empty() || !untracked_player_pids(state).is_empty())
            {
                return Ok(json!("single-instance"));
            }
            Ok(if state.launch.is_running(&id) { json!("instance-running") } else { Value::Null })
        })
        .handle("launch:getRunningInstanceIds", |ctx, _| async move { Ok(ctx.state.launch.running_ids()) })
        .handle("launch:getRunningInstanceId", |ctx, _| async move { Ok(ctx.state.launch.running_ids().into_iter().next()) })
        .handle("launch:getRunningVersion", |ctx, _| async move {
            let running = ctx.state.launch.running.lock();
            Ok(running.values().next().map(|r| json!({ "product": r.product, "versionHash": r.version_hash })))
        })
        .handle("launch:kill", |ctx, _| async move {
            let state = &ctx.state;
            let products: Vec<String> = {
                let running = state.launch.running.lock();
                if running.is_empty() {
                    return Ok(false);
                }
                let mut products: Vec<String> = running.values().map(|r| r.product.clone()).collect();
                products.dedup();
                products
            };
            let refs: Vec<&str> = products.iter().map(String::as_str).collect();
            let ok = kill_products(state, &refs);
            if ok {
                tracing::info!("Killed Roblox process(es)");
            }
            Ok(ok)
        })
        .handle("launch:killInstance", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(kill_instance(&ctx.state, &id))
        })
        .handle("launch:forceQuit", |ctx, _| async move {
            let ok = kill_products(&ctx.state, &PRODUCTS);
            if ok {
                tracing::info!("Force-quit all Roblox process(es)");
            }
            Ok(ok)
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_urls_match_electron() {
        let game = json!({ "type": "game", "placeId": 920587237 });
        assert_eq!(
            place_launcher_url(&game),
            "https://assetgame.roblox.com/game/PlaceLauncher.ashx?request=RequestGame&placeId=920587237&isPlayTogetherGame=false"
        );
        assert_eq!(
            fallback_url(&json!({ "type": "followUser", "placeId": 1, "userId": 2 })),
            "roblox://experiences/start?placeId=1&userId=2"
        );
        assert_eq!(
            replace_gameinfo("roblox-player:1+gameinfo:old+launchmode:play", "new"),
            "roblox-player:1+gameinfo:new+launchmode:play"
        );
    }
}

fn instance_name(instance: &Instance) -> &str {
    instance.get("name").and_then(Value::as_str).unwrap_or("")
}


/// `handleTrackedSessionEnded`: `close-to-launcher` closes the game when the
/// player leaves it.
pub fn on_session_ended(state: &Arc<AppState>, session: &Value) {
    let id = session["instanceId"].as_str().unwrap_or("");
    if session["reason"] != "leave" || !state.settings.get_bool("close-to-launcher") || !state.launch.is_running(id) {
        return;
    }
    kill_instance(state, id);
}
