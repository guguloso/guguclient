//! Mission Control, the second-screen window (`secondScreen:*`). Port of
//! `MissionControlWindow` and the `SecondScreenController`: 1400×900
//! (min 600×400), placed where it was last moved or centred on a
//! secondary display, loading the renderer at `#/second-screen`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::webview::PageLoadEvent;
use tauri::{Manager, Monitor, WebviewWindow, WindowEvent};

use crate::ipc::Router;
use crate::state::AppState;

pub const MISSION_CONTROL_WINDOW: &str = "mission-control";
const DEFAULT_WIDTH: f64 = 1400.0;
const DEFAULT_HEIGHT: f64 = 900.0;
const MIN_WIDTH: f64 = 600.0;
const MIN_HEIGHT: f64 = 400.0;
const TITLE: &str = "Gugu Mission Control";
const ROUTE: &str = "/second-screen";
const FOCUS_INSTANCE_CHANNEL: &str = "secondScreen:focusInstance";
const MOVE_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

static MOVE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn window(state: &AppState) -> Option<WebviewWindow> {
    state.app.get_webview_window(MISSION_CONTROL_WINDOW)
}

pub fn is_open(state: &AppState) -> bool {
    window(state).is_some()
}

/// `missionControlWindow.send`.
pub fn send(state: &AppState, channel: &str, payload: Value) {
    if is_open(state) {
        state.events.emit_to(MISSION_CONTROL_WINDOW, channel, payload);
    }
}

fn focus_instance(state: &AppState, id: &str) {
    send(state, FOCUS_INSTANCE_CHANNEL, json!({ "instanceId": id }));
}

/// Logical work area of a monitor: `(x, y, width, height)`.
fn work_area(monitor: &Monitor) -> (f64, f64, f64, f64) {
    let scale = monitor.scale_factor();
    let pos = monitor.position().to_logical::<f64>(scale);
    let size = monitor.size().to_logical::<f64>(scale);
    (pos.x, pos.y, size.width, size.height)
}

/// `resolveInitialPosition`.
fn initial_position(state: &AppState) -> (f64, f64) {
    let monitors = state.app.available_monitors().unwrap_or_default();
    let placement = state.settings.get("second-screen-placement").unwrap_or(Value::Null);
    if let (Some(x), Some(y)) = (placement.get("x").and_then(Value::as_f64), placement.get("y").and_then(Value::as_f64)) {
        let (cx, cy) = (x + DEFAULT_WIDTH / 2.0, y + DEFAULT_HEIGHT / 2.0);
        let on_screen = monitors.iter().map(work_area).any(|(mx, my, mw, mh)| cx >= mx && cx < mx + mw && cy >= my && cy < my + mh);
        if on_screen {
            return (x, y);
        }
    }
    let primary = state.app.primary_monitor().ok().flatten();
    let display = monitors
        .iter()
        .find(|m| primary.as_ref().is_none_or(|p| m.position() != p.position()))
        .cloned()
        .or_else(|| state.app.get_webview_window(crate::MAIN_WINDOW).and_then(|w| w.current_monitor().ok().flatten()))
        .or(primary);
    match display {
        Some(display) => {
            let (x, y, w, h) = work_area(&display);
            ((x + (w - DEFAULT_WIDTH) / 2.0).floor(), (y + (h - DEFAULT_HEIGHT) / 2.0).floor())
        }
        None => (0.0, 0.0),
    }
}

/// `savePlacement`: remembers where the window was moved (not when maximized).
fn save_placement(state: &AppState, window: &WebviewWindow) {
    if window.is_maximized().unwrap_or(false) || window.is_fullscreen().unwrap_or(false) {
        return;
    }
    let (Ok(pos), Ok(scale)) = (window.outer_position(), window.scale_factor()) else { return };
    let logical = pos.to_logical::<f64>(scale);
    let (x, y) = (logical.x.round(), logical.y.round());
    let previous = state.settings.get("second-screen-placement").unwrap_or(Value::Null);
    if previous.get("x").and_then(Value::as_f64) == Some(x) && previous.get("y").and_then(Value::as_f64) == Some(y) {
        return;
    }
    if let Err(error) = state.settings.set("second-screen-placement", json!({ "x": x, "y": y })) {
        tracing::warn!("Failed to save Mission Control placement: {error}");
    }
}

/// `MissionControlWindow.open`: `false` when the instance cannot have one.
pub fn open(state: &Arc<AppState>, instance_id: Option<&str>) -> bool {
    let info = instance_id.and_then(|id| state.running.info(id));
    if let Some(id) = instance_id {
        let Some((_, product)) = &info else {
            tracing::warn!("Cannot open Mission Control - instance {id} not running");
            return false;
        };
        if product != "player" {
            tracing::warn!("Cannot open Mission Control for non-player instance {id}");
            return false;
        }
    }
    if let Some(existing) = window(state) {
        if existing.is_minimized().unwrap_or(false) {
            let _ = existing.unminimize();
        }
        let _ = existing.show();
        let _ = existing.set_focus();
        if let Some(id) = instance_id {
            focus_instance(state, id);
        }
        return true;
    }
    let (x, y) = initial_position(state);
    let focus = instance_id.map(str::to_owned);
    let loaded_state = Arc::clone(state);
    let built = crate::app_window_builder(&state.app, MISSION_CONTROL_WINDOW, Some(ROUTE))
        .title(TITLE)
        .inner_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        .position(x, y)
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.show();
                let _ = window.set_focus();
                if let Some(id) = &focus {
                    focus_instance(&loaded_state, id);
                }
            }
        })
        .build();
    let window = match built {
        Ok(w) => w,
        Err(error) => {
            tracing::error!("Failed to open Mission Control: {error}");
            return false;
        }
    };
    let events_state = Arc::clone(state);
    let handle = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Moved(_) => {
            let generation = MOVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            let state = Arc::clone(&events_state);
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(MOVE_SAVE_DEBOUNCE).await;
                if MOVE_GENERATION.load(Ordering::SeqCst) == generation {
                    save_placement(&state, &handle);
                }
            });
        }
        WindowEvent::Destroyed => {
            MOVE_GENERATION.fetch_add(1, Ordering::SeqCst);
            let state = Arc::clone(&events_state);
            // The window is gone from the registry only after this event.
            tauri::async_runtime::spawn(async move {
                state.running.notify_instances_changed(&state);
            });
        }
        _ => {}
    });
    match &info {
        Some((name, _)) => tracing::info!("Opened Mission Control for: {name}"),
        None => tracing::info!("Opened Mission Control"),
    }
    state.running.notify_instances_changed(state);
    true
}

pub fn close(state: &AppState) {
    if let Some(window) = window(state) {
        let _ = window.close();
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("secondScreen:open", |ctx, args| async move {
            let id: Option<String> = args.opt(0)?;
            Ok(open(&ctx.state, id.as_deref()))
        })
        .handle("secondScreen:close", |ctx, _| async move {
            close(&ctx.state);
            Ok(true)
        })
        .handle("secondScreen:isOpen", |ctx, _| async move { Ok(is_open(&ctx.state)) })
        .handle("secondScreen:getLogEntries", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(ctx.state.running.log_entries(&id).iter().map(|e| e.to_json(&id)).collect::<Vec<_>>())
        })
        .handle("secondScreen:getRunningInstances", |ctx, _| async move { Ok(ctx.state.running.list(&ctx.state)) })
        .handle("secondScreen:getRecentInstances", |ctx, _| async move { Ok(ctx.state.running.list_recent(&ctx.state)) });
}

// ------------------------------------------------------------------ hotkey

/// `MissionControlHotkey`: the accelerator is bound only while the launcher
/// or a tracked Roblox window is in the foreground.
#[derive(Default)]
pub struct HotkeyState {
    accelerator: parking_lot::Mutex<Option<String>>,
    registered: std::sync::atomic::AtomicBool,
    generation: AtomicU64,
}

/// `ForegroundWindowService`: pid of the foreground window.
fn foreground_pid() -> Option<u32> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (pid != 0).then_some(pid)
    }
    #[cfg(not(windows))]
    None
}

fn apply_registration(state: &Arc<AppState>, foreground: bool) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let Some(accelerator) = state.hotkey.accelerator.lock().clone() else { return };
    let shortcuts = state.app.global_shortcut();
    if !foreground {
        if state.hotkey.registered.swap(false, Ordering::SeqCst) {
            let _ = shortcuts.unregister(accelerator.as_str());
        }
        return;
    }
    if state.hotkey.registered.load(Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(state);
    let result = shortcuts.on_shortcut(accelerator.as_str(), move |_, _, event| {
        if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
            if let Some(state) = weak.upgrade() {
                open(&state, None);
            }
        }
    });
    match result {
        Ok(()) => state.hotkey.registered.store(true, Ordering::SeqCst),
        Err(error) => tracing::warn!("[MissionControl] Failed to register hotkey \"{accelerator}\": {error}"),
    }
}

fn clear_hotkey(state: &Arc<AppState>) {
    state.hotkey.generation.fetch_add(1, Ordering::SeqCst);
    apply_registration(state, false);
    *state.hotkey.accelerator.lock() = None;
}

/// `syncMissionControlHotkey`.
pub fn sync_hotkey(state: &Arc<AppState>) {
    let configured = state.settings.get("mission-control-hotkey").and_then(|v| v.as_str().map(str::to_owned)).filter(|s| !s.is_empty());
    if *state.hotkey.accelerator.lock() == configured {
        return;
    }
    clear_hotkey(state);
    let Some(accelerator) = configured else { return };
    *state.hotkey.accelerator.lock() = Some(accelerator.clone());
    tracing::info!("[MissionControl] Hotkey set to \"{accelerator}\"");
    let generation = state.hotkey.generation.load(Ordering::SeqCst);
    let weak = Arc::downgrade(state);
    tauri::async_runtime::spawn(async move {
        let mut foreground = false;
        loop {
            let Some(state) = weak.upgrade() else { return };
            if state.hotkey.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let pid = foreground_pid();
            let next = pid.is_some_and(|p| p == std::process::id() || state.launch.running_pids().contains(&p));
            if next != foreground {
                foreground = next;
                apply_registration(&state, foreground);
            }
            drop(state);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}
