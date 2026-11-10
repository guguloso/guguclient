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
const MAIN_WINDOW: &str = "main";
const MIN_WIDTH: f64 = 1120.0;
const MIN_HEIGHT: f64 = 760.0;
const MAXIMIZED_CHANGED: &str = "window:maximizedChanged";

pub fn run() {
    let paths = LauncherPaths::resolve().expect("failed to resolve launcher directories");
    let _log_guard = init_logging(&paths);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Application starting (Rust runtime)");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| reveal_main_window(app)))
        .register_asynchronous_uri_scheme_protocol(services::thumbnails::PROTOCOL, services::thumbnails::protocol_handler)
        .invoke_handler(tauri::generate_handler![ipc::ipc_invoke])
        .setup(move |app| {
            let state = Arc::new(AppState::initialize(app.handle().clone(), paths)?);
            let router = Arc::new(Router::build());
            tracing::info!(channels = router.len(), "IPC router ready");
            app.manage(Arc::clone(&state));
            app.manage(router);

            build_tray(app.handle())?;
            create_main_window(app.handle(), &state)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Gugu Client");
}

fn init_logging(paths: &LauncherPaths) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env("GUGU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
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
    let mut builder = WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()));
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
        .inner_size(resolution.width.max(MIN_WIDTH), resolution.height.max(MIN_HEIGHT))
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        .decorations(false)
        .visible(false)
        .background_color(Color(5, 5, 5, 255))
        .initialization_script(BRIDGE_JS)
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
        }
        WindowEvent::CloseRequested { api, .. } => {
            save_resolution(&handle, &state);
            if state.settings.get_bool("close-to-tray") {
                api.prevent_close();
                let _ = handle.hide();
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
            resolution = Resolution { width: logical.width, height: logical.height, ..resolution };
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
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                reveal_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}
