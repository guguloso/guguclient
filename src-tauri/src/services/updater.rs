//! Self-updater (`updater:*`). Port of the Gugu `UpdaterService`: checks the
//! latest GitHub release of `guguloso/guguclient`, downloads its `.exe` with
//! progress, and swaps the binary through a detached script once the user
//! is at the app (window focused) or asked for it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::Manager;
use tokio::io::AsyncWriteExt;

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::state::AppState;
use crate::util::now_iso;

const GITHUB_REPO: &str = "guguloso/guguclient";
const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const PROGRESS_REPORT_EVERY: Duration = Duration::from_millis(150);
const STATE_CHANGED: &str = "updater:stateChanged";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROCESS_NAME: &str = "GuguClient.exe";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    percent: f64,
    bytes_per_second: u64,
    transferred: u64,
    total: u64,
    delta: u64,
}

#[derive(Clone, Serialize)]
pub struct UpdaterErrorInfo {
    code: &'static str,
    name: &'static str,
    message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterState {
    status: &'static str,
    checking: bool,
    available: bool,
    downloaded: bool,
    user_initiated: bool,
    current_version: &'static str,
    target_version: Option<String>,
    channel: &'static str,
    last_checked_at: Option<String>,
    progress: Option<Progress>,
    error: Option<UpdaterErrorInfo>,
}

struct Inner {
    state: UpdaterState,
    checking_lock: bool,
    update_downloaded: bool,
    downloaded_path: Option<PathBuf>,
    user_initiated: bool,
    bypass_focus_check: bool,
    notified_deferred_install: bool,
}

pub struct UpdaterService {
    inner: Mutex<Inner>,
    initialized: AtomicBool,
}

#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

/// `compareSemver`: numeric `major.minor.patch...`, ignores `v` and `-pre`.
fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> {
        let v = v.trim_start_matches(['v', 'V']);
        v.split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

impl UpdaterService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: UpdaterState {
                    status: "idle",
                    checking: false,
                    available: false,
                    downloaded: false,
                    user_initiated: false,
                    current_version: CURRENT_VERSION,
                    target_version: None,
                    channel: "latest",
                    last_checked_at: None,
                    progress: None,
                    error: None,
                },
                checking_lock: false,
                update_downloaded: false,
                downloaded_path: None,
                user_initiated: false,
                bypass_focus_check: true,
                notified_deferred_install: false,
            }),
            initialized: AtomicBool::new(false),
        }
    }

    pub fn state(&self) -> UpdaterState {
        self.inner.lock().state.clone()
    }

    /// `initialize()`: check now and every 15 minutes.
    pub fn initialize(state: &Arc<AppState>) {
        if state.updater.initialized.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            loop {
                check_for_updates(&state, false).await;
                tokio::time::sleep(CHECK_INTERVAL).await;
            }
        });
    }

    /// `setServerChannel` → `syncFeedOptions`: the Gugu feed is always
    /// `latest`, but the state is re-emitted like the original.
    pub fn set_server_channel(&self, state: &AppState) {
        self.patch(state, |s| s.channel = "latest");
    }

    fn patch(&self, state: &AppState, f: impl FnOnce(&mut UpdaterState)) {
        let snapshot = {
            let mut inner = self.inner.lock();
            f(&mut inner.state);
            inner.state.user_initiated = inner.user_initiated;
            inner.state.downloaded = inner.update_downloaded;
            inner.state.clone()
        };
        state.events.emit(STATE_CHANGED, snapshot);
    }

    fn handle_error(&self, state: &AppState, error: &AppError) {
        tracing::error!("[Updater] Error: {error}");
        {
            let mut inner = self.inner.lock();
            inner.checking_lock = false;
            inner.bypass_focus_check = false;
            inner.notified_deferred_install = false;
        }
        let message = error.to_string();
        self.patch(state, |s| {
            s.status = "error";
            s.checking = false;
            s.error = Some(UpdaterErrorInfo {
                code: "unknown",
                name: "Error",
                message,
            });
            s.last_checked_at = Some(now_iso());
        });
        self.inner.lock().user_initiated = false;
    }

    fn on_update_not_available(&self, state: &AppState) {
        {
            let mut inner = self.inner.lock();
            inner.checking_lock = false;
            inner.bypass_focus_check = false;
            inner.notified_deferred_install = false;
        }
        self.patch(state, |s| {
            s.status = "no-update";
            s.checking = false;
            s.available = false;
            s.target_version = None;
            s.progress = None;
            s.error = None;
            s.last_checked_at = Some(now_iso());
        });
        self.inner.lock().user_initiated = false;
        tracing::info!("[Updater] No updates available");
    }

    /// Any window gained focus (`browser-window-focus`).
    pub fn on_window_focus(state: &Arc<AppState>) {
        if state.updater.inner.lock().update_downloaded {
            try_install_downloaded(state);
        }
    }
}

fn window_focused(state: &AppState, label: &str) -> bool {
    state
        .app
        .get_webview_window(label)
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false)
}

pub async fn check_for_updates(state: &Arc<AppState>, user_initiated: bool) {
    let updater = &state.updater;
    updater.set_server_channel(state);
    let (downloaded, already_checking) = {
        let mut inner = updater.inner.lock();
        if user_initiated {
            inner.user_initiated = true;
        }
        (inner.update_downloaded, inner.checking_lock)
    };
    if downloaded {
        try_install_downloaded(state);
        return;
    }
    if already_checking {
        updater.patch(state, |s| {
            s.status = "checking";
            s.checking = true;
        });
        tracing::info!("[Updater] Update check already running");
        return;
    }
    {
        let mut inner = updater.inner.lock();
        inner.checking_lock = true;
        inner.notified_deferred_install = false;
    }
    updater.patch(state, |s| {
        s.status = "checking";
        s.checking = true;
        s.error = None;
    });

    match find_update(state).await {
        Ok(None) => updater.on_update_not_available(state),
        Ok(Some((version, url, size))) => on_update_available(state, version, url, size),
        Err(error) => updater.handle_error(state, &error),
    }
}

/// Queries the latest release; `Some((version, exe_url, size))` when newer.
async fn find_update(state: &AppState) -> AppResult<Option<(String, String, u64)>> {
    let api_url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    tracing::info!("[Updater] Checking GitHub releases at {api_url}");
    let response = state
        .roblox
        .http()
        .get(&api_url)
        .header("User-Agent", format!("GuguClient/{CURRENT_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .header("Cache-Control", "no-cache")
        .send()
        .await?;
    let status = response.status();
    if status.as_u16() == 404 {
        tracing::info!("[Updater] No releases published on GitHub yet (404)");
        return Ok(None);
    }
    if !status.is_success() {
        return Err(AppError::Message(format!("GitHub API returned {status}")));
    }
    let release: Release = response.json().await?;
    let raw_tag = release.tag_name.or(release.name).unwrap_or_default();
    let raw_tag = raw_tag.trim();
    let latest = raw_tag.trim_start_matches(['v', 'V']).trim();
    if latest.is_empty() || compare_semver(latest, CURRENT_VERSION).is_le() {
        let shown = if latest.is_empty() { "none" } else { latest };
        tracing::info!("[Updater] Up to date (current={CURRENT_VERSION}, latest={shown})");
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name.to_lowercase().ends_with(".exe") && a.browser_download_url.is_some())
        .ok_or_else(|| {
            AppError::Message(format!(
                "Release {raw_tag} does not include an .exe update asset"
            ))
        })?;
    let url = asset.browser_download_url.unwrap_or_default();
    Ok(Some((latest.to_owned(), url, asset.size.unwrap_or(0))))
}

fn on_update_available(state: &Arc<AppState>, version: String, url: String, size: u64) {
    let updater = &state.updater;
    updater.inner.lock().checking_lock = false;
    tracing::info!("[Updater] Update available: {version} (current={CURRENT_VERSION})");
    let target = version.clone();
    updater.patch(state, |s| {
        s.status = "available";
        s.checking = false;
        s.available = true;
        s.target_version = Some(target);
        s.last_checked_at = Some(now_iso());
        s.error = None;
    });
    let (bypass, user_initiated) = {
        let inner = updater.inner.lock();
        (inner.bypass_focus_check, inner.user_initiated)
    };
    if window_focused(state, crate::MAIN_WINDOW) || bypass || user_initiated {
        if let Err(error) = crate::show_updater_window(&state.app) {
            tracing::error!(%error, "[UpdaterWindow] Failed to show");
        }
    }
    updater.patch(state, |s| s.status = "downloading");
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        match download(&state, &url, &version, size).await {
            Ok(path) => on_update_downloaded(&state, path),
            Err(error) => state.updater.handle_error(&state, &error),
        }
    });
}

async fn download(state: &AppState, url: &str, version: &str, expected: u64) -> AppResult<PathBuf> {
    let dest = std::env::temp_dir().join(format!("GuguClient-Update-{version}.exe"));
    tracing::info!("[Updater] Downloading {url} -> {}", dest.display());
    let mut response = state
        .roblox
        .http()
        .get(url)
        .header("User-Agent", format!("GuguClient/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(30 * 60))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(AppError::Message(format!(
            "Failed to download update: HTTP {}",
            response.status()
        )));
    }
    let total = response
        .content_length()
        .filter(|n| *n > 0)
        .unwrap_or(expected);
    let mut file = tokio::fs::File::create(&dest).await?;
    let (mut transferred, mut last_bytes, mut last_time) = (0u64, 0u64, Instant::now());
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
        transferred += chunk.len() as u64;
        let elapsed = last_time.elapsed();
        if elapsed >= PROGRESS_REPORT_EVERY || (total > 0 && transferred >= total) {
            let elapsed_ms = elapsed.as_millis() as u64;
            let progress = Progress {
                percent: if total > 0 {
                    (transferred as f64 / total as f64 * 100.0).min(100.0)
                } else {
                    0.0
                },
                bytes_per_second: if elapsed_ms > 0 {
                    (transferred - last_bytes) * 1000 / elapsed_ms
                } else {
                    0
                },
                transferred,
                total,
                delta: transferred - last_bytes,
            };
            state.updater.patch(state, |s| {
                s.status = "downloading";
                s.progress = Some(progress);
            });
            last_time = Instant::now();
            last_bytes = transferred;
        }
    }
    file.flush().await?;
    Ok(dest)
}

fn on_update_downloaded(state: &Arc<AppState>, path: PathBuf) {
    tracing::info!("[Updater] Update downloaded: {}", path.display());
    {
        let mut inner = state.updater.inner.lock();
        inner.update_downloaded = true;
        inner.downloaded_path = Some(path);
        inner.checking_lock = false;
        inner.notified_deferred_install = false;
    }
    state.updater.patch(state, |s| {
        s.status = "downloaded";
        s.checking = false;
        s.available = true;
        if let Some(progress) = &mut s.progress {
            progress.percent = 100.0;
        }
    });
    try_install_downloaded(state);
}

fn try_install_downloaded(state: &Arc<AppState>) {
    let updater = &state.updater;
    let (downloaded, bypass, user_initiated) = {
        let inner = updater.inner.lock();
        (
            inner.update_downloaded,
            inner.bypass_focus_check,
            inner.user_initiated,
        )
    };
    if !downloaded {
        return;
    }
    let can_update = bypass
        || window_focused(state, crate::UPDATER_WINDOW)
        || window_focused(state, crate::MAIN_WINDOW);
    if !(can_update || user_initiated) {
        updater.inner.lock().notified_deferred_install = true;
        return;
    }
    updater.inner.lock().user_initiated = false;
    quit_and_install(state);
}

/// Hands the downloaded exe to a detached script that waits for this
/// process to exit, optionally replaces the portable exe, and relaunches.
fn quit_and_install(state: &AppState) {
    state.updater.patch(state, |s| {
        s.status = "installing";
        s.checking = false;
        s.error = None;
    });
    tracing::info!("[Updater] Quitting and installing downloaded update");
    if let Some(window) = state.app.get_webview_window(crate::UPDATER_WINDOW) {
        let _ = window.hide();
    }
    let Some(exe) = state.updater.inner.lock().downloaded_path.clone() else {
        tracing::error!("[Updater] No downloaded file found for installation");
        return;
    };
    if let Err(error) = launch_install_script(&exe) {
        tracing::error!(%error, "[Updater] Failed to launch update installer script");
    }
    state.app.exit(0);
}

fn launch_install_script(exe: &std::path::Path) -> std::io::Result<()> {
    let exe = exe.display();
    let millis = crate::util::now_millis();
    let script = std::env::temp_dir().join(format!("gugu-apply-update-{millis}.cmd"));
    let copy_lines = match std::env::var("GUGU_PORTABLE_EXE") {
        Ok(portable) if std::path::Path::new(&portable).exists() => format!(
            "copy /y \"{exe}\" \"{portable}\" >nul 2>&1\r\nif not errorlevel 1 set \"LAUNCH_TARGET={portable}\"\r\n"
        ),
        _ => String::new(),
    };
    let content = format!(
        "@echo off\r\nset \"LAUNCH_TARGET={exe}\"\r\n:waitloop\r\ntimeout /t 1 /nobreak >nul\r\n\
         tasklist /fi \"imagename eq {PROCESS_NAME}\" 2>nul | find /i \"{PROCESS_NAME}\" >nul\r\n\
         if not errorlevel 1 goto waitloop\r\n{copy_lines}start \"\" \"%LAUNCH_TARGET%\"\r\ndel \"%~f0\"\r\n"
    );
    std::fs::write(&script, content)?;
    let mut command = std::process::Command::new("cmd.exe");
    command.arg("/c").arg(&script);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    command.spawn().map(drop)
}

pub fn register(router: &mut Router) {
    router
        .handle("updater:getState", |ctx, _| async move {
            Ok(ctx.state.updater.state())
        })
        .handle("updater:checkForUpdates", |ctx, _| async move {
            check_for_updates(&ctx.state, true).await;
            Ok(Value::Null)
        })
        .handle("updater:quitAndInstall", |ctx, _| async move {
            quit_and_install(&ctx.state);
            Ok(Value::Null)
        });
}

#[cfg(test)]
mod tests {
    use super::compare_semver;
    use std::cmp::Ordering::*;

    #[test]
    fn semver_like_electron() {
        assert_eq!(compare_semver("1.0.17", "1.0.16"), Greater);
        assert_eq!(compare_semver("v1.0.16", "1.0.16"), Equal);
        assert_eq!(compare_semver("1.1", "1.0.99"), Greater);
        assert_eq!(compare_semver("1.0.16-beta", "1.0.16"), Equal);
    }
}
