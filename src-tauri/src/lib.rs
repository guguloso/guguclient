//! Gugu Client for Roblox — Rust runtime.
//!
//! The React renderer is loaded unchanged; a generated bridge recreates
//! `window.api` on top of Tauri IPC and this crate answers the same channels
//! the Electron main process did.

mod crypto;
mod error;
mod ipc;
mod paths;
mod roblox;
mod services;
mod state;
mod store;
mod util;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::ipc::Router;
use crate::paths::LauncherPaths;
use crate::services::settings::Resolution;
use crate::state::AppState;

/// `window.api` bridge generated from the Electron preload.
const BRIDGE_JS: &str = include_str!("../generated/bridge.js");
pub(crate) const MAIN_WINDOW: &str = "main";
pub(crate) const UPDATER_WINDOW: &str = "updater";
const MIN_WIDTH: f64 = 1120.0;
const MIN_HEIGHT: f64 = 760.0;
const MAXIMIZED_CHANGED: &str = "window:maximizedChanged";

pub fn run() {
    let paths = LauncherPaths::resolve().expect("failed to resolve launcher directories");
    let _log_guard = init_logging(&paths);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Application starting (Rust runtime)"
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            on_second_instance(app, argv)
        }))
        .plugin(tauri_plugin_notification::init())
        .register_asynchronous_uri_scheme_protocol(
            services::thumbnails::PROTOCOL,
            services::thumbnails::protocol_handler,
        )
        .invoke_handler(tauri::generate_handler![ipc::ipc_invoke])
        .setup(move |app| {
            let state = Arc::new(AppState::initialize(app.handle().clone(), paths)?);
            let router = Arc::new(Router::build());
            tracing::info!(channels = router.len(), "IPC router ready");
            app.manage(Arc::clone(&state));
            app.manage(router);

            state.start_background();
            build_tray(app.handle())?;
            create_main_window(app.handle(), &state)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Gugu Client")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // `before-quit`: persist debounced caches.
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    state.social.stop();
                    state.game_communities.flush(&state);
                }
            }
        });
}

/// Builds a frameless app window running the renderer with the bridge.
/// `route` is the initial hash route (Electron's `loadFile({ hash })`).
fn app_window_builder<'a>(
    app: &'a AppHandle,
    label: &'a str,
    route: Option<&str>,
) -> WebviewWindowBuilder<'a, tauri::Wry, AppHandle> {
    let mut script = String::with_capacity(BRIDGE_JS.len() + 128);
    if let Some(route) = route {
        script.push_str(&format!(
            "if(!location.hash)history.replaceState(null,'','#{route}');\n"
        ));
    }
    script.push_str(BRIDGE_JS);
    WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .decorations(false)
        .visible(false)
        .background_color(Color(5, 5, 5, 255))
        .initialization_script(&script)
}

/// `UpdaterWindowService.showUpdaterWindow`: 560×470, fixed size, frameless.
pub(crate) fn show_updater_window(app: &AppHandle) -> tauri::Result<()> {
    let window = match app.get_webview_window(UPDATER_WINDOW) {
        Some(window) => window,
        None => {
            let window = app_window_builder(app, UPDATER_WINDOW, Some("/updater"))
                .title("Gugu Client for Roblox")
                .inner_size(560.0, 470.0)
                .resizable(false)
                .build()?;
            let state = app.state::<Arc<AppState>>().inner().clone();
            window.on_window_event(move |event| {
                if let WindowEvent::Focused(true) = event {
                    services::updater::UpdaterService::on_window_focus(&state);
                }
            });
            tracing::info!("[UpdaterWindow] Created updater window");
            window
        }
    };
    window.show()?;
    window.set_focus()?;
    Ok(())
}

/// `isMainWindowStashed()`: minimized or hidden (to tray).
fn update_backgrounded(window: &WebviewWindow, state: &AppState) {
    let stashed = window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(true);
    state.social.set_backgrounded(stashed);
    state.chat.set_backgrounded(stashed);
}

fn init_logging(paths: &LauncherPaths) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env("GUGU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if std::fs::create_dir_all(&paths.logs_dir).is_err() {
        builder.init();
        return None;
    }
    let appender = tracing_appender::rolling::daily(&paths.logs_dir, "gugu-rust.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    builder.with_ansi(false).with_writer(writer).init();
    Some(guard)
}

fn create_main_window(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<()> {
    let resolution = state.settings.main_resolution();
    #[allow(unused_mut)]
    let mut builder = app_window_builder(app, MAIN_WINDOW, None);
    // Debug builds only: `GUGU_DEBUG_PORT=9333` exposes the WebView2 DevTools
    // protocol for automated UI checks. Keeps Tauri's default feature flags.
    #[cfg(debug_assertions)]
    if let Ok(port) = std::env::var("GUGU_DEBUG_PORT") {
        builder = builder.additional_browser_args(&format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
        ));
    }
    let window = builder
        .title("Gugu Client for Roblox")
        .inner_size(
            resolution.width.max(MIN_WIDTH),
            resolution.height.max(MIN_HEIGHT),
        )
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        // Show only once painted: no white flash on startup.
        .on_page_load(|window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.show();
                let _ = window.set_focus();
            }
        })
        .build()?;

    if resolution.maximized {
        window.maximize()?;
    }
    attach_window_events(&window, Arc::clone(state));
    Ok(())
}

fn attach_window_events(window: &WebviewWindow, state: Arc<AppState>) {
    let was_maximized = AtomicBool::new(window.is_maximized().unwrap_or(false));
    let handle = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Resized(_) => {
            let maximized = handle.is_maximized().unwrap_or(false);
            if was_maximized.swap(maximized, Ordering::Relaxed) != maximized {
                state.events.emit(MAXIMIZED_CHANGED, maximized);
                save_resolution(&handle, &state);
            }
            update_backgrounded(&handle, &state);
        }
        WindowEvent::Focused(true) => {
            update_backgrounded(&handle, &state);
            services::updater::UpdaterService::on_window_focus(&state);
        }
        WindowEvent::CloseRequested { api, .. } => {
            save_resolution(&handle, &state);
            if state.settings.get_bool("close-to-tray") {
                api.prevent_close();
                let _ = handle.hide();
                update_backgrounded(&handle, &state);
            }
        }
        _ => {}
    });
}

/// Persists size in logical pixels (Electron DIPs) like `saveResolution`.
fn save_resolution(window: &WebviewWindow, state: &AppState) {
    let maximized = window.is_maximized().unwrap_or(false);
    let mut resolution = state.settings.main_resolution();
    if !maximized {
        if let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) {
            let logical = size.to_logical::<f64>(scale);
            resolution = Resolution {
                width: logical.width,
                height: logical.height,
                ..resolution
            };
        }
    }
    resolution.maximized = maximized;
    resolution.fullscreen = window.is_fullscreen().unwrap_or(false);
    if let Err(error) = state.settings.save_main_resolution(resolution) {
        tracing::warn!(%error, "failed to save window resolution");
    }
}

fn reveal_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Gugu Client", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Gugu Client for Roblox")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => reveal_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// `second-instance`: forward a protocol URL from the new process, or just
/// bring the window back.
fn on_second_instance(app: &AppHandle, argv: Vec<String>) {
    match services::protocol::extract_protocol_url(&argv) {
        Some(url) => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                let state = Arc::clone(&state);
                tauri::async_runtime::spawn(async move {
                    services::protocol::handle_url(&state, &url, "second_instance").await;
                });
            }
        }
        None => reveal_main_window(app),
    }
}
