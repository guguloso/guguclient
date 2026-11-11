//! Launcher settings (`settings:*`). Port of `SettingsService`/`SettingsStore`:
//! same file (`settings/launcher.json`), same keys (`settings.<id>`), same
//! defaults, same "unknown setting" error and same return shapes.
//!
//! Per-setting `afterSet` side effects run from the IPC handlers.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::paths::LauncherPaths;
use crate::state::AppState;
use crate::store::{self, JsonStore};
use std::sync::Arc;

const DEFAULTS_JSON: &str = include_str!("settings_defaults.json");
const KEY_PREFIX: &str = "settings";

pub struct SettingsService {
    store: JsonStore,
    defaults: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Resolution {
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

impl SettingsService {
    pub fn open(paths: &LauncherPaths) -> AppResult<Self> {
        let defaults: Map<String, Value> = serde_json::from_str(DEFAULTS_JSON)?;
        let store = JsonStore::open(&paths.settings_dir, "launcher")?;
        let dev = store.read("settings.use-dev-services", |v| v.and_then(Value::as_bool)) == Some(true);
        crate::services::metadata::USE_DEV_SERVICES.store(dev, std::sync::atomic::Ordering::Relaxed);
        Ok(Self { store, defaults })
    }

    /// Every known setting with its stored value, falling back to the default
    /// when missing or of the wrong JSON type (Electron's `transformSet` → null).
    pub fn get_all(&self) -> Map<String, Value> {
        self.store.read(KEY_PREFIX, |stored| {
            self.defaults
                .iter()
                .map(|(id, default)| {
                    let value = stored
                        .and_then(|s| s.get(id))
                        .filter(|v| same_kind(v, default))
                        .unwrap_or(default);
                    (id.clone(), value.clone())
                })
                .collect()
        })
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        let default = self.defaults.get(id)?;
        Some(self.store.read(&format!("{KEY_PREFIX}.{id}"), |stored| {
            stored
                .filter(|v| same_kind(v, default))
                .unwrap_or(default)
                .clone()
        }))
    }

    /// `STATIC_SETTING_DEFAULTS[id]`.
    pub fn default_value(&self, id: &str) -> Option<Value> {
        self.defaults.get(id).cloned()
    }

    pub fn get_bool(&self, id: &str) -> bool {
        self.get(id).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    pub fn set(&self, id: &str, value: Value) -> AppResult<Map<String, Value>> {
        let default = self
            .defaults
            .get(id)
            .ok_or_else(|| AppError::msg(format!("Unknown setting \"{id}\"")))?;
        if same_kind(&value, default) {
            self.store
                .update(|data| store::assign(data, &format!("{KEY_PREFIX}.{id}"), value))?;
        }
        Ok(self.get_all())
    }

    /// Applies a partial patch; unknown ids are ignored, like Electron.
    pub fn update(&self, patch: Map<String, Value>) -> AppResult<Map<String, Value>> {
        self.store.update(|data| {
            for (id, value) in patch {
                if self
                    .defaults
                    .get(&id)
                    .is_some_and(|default| same_kind(&value, default))
                {
                    store::assign(data, &format!("{KEY_PREFIX}.{id}"), value);
                }
            }
        })?;
        Ok(self.get_all())
    }

    pub fn reset(&self) -> AppResult<Map<String, Value>> {
        self.store.update(|data| {
            for id in self.defaults.keys() {
                store::remove(data, &format!("{KEY_PREFIX}.{id}"));
            }
        })?;
        Ok(self.get_all())
    }

    pub fn main_resolution(&self) -> Resolution {
        self.get("launcher-resolution-main")
            .and_then(|v| Resolution::deserialize(&v).ok())
            .unwrap_or(Resolution {
                width: 1400.0,
                height: 800.0,
                maximized: false,
                fullscreen: false,
            })
    }

    pub fn save_main_resolution(&self, resolution: Resolution) -> AppResult<()> {
        let value = serde_json::json!({
            "width": resolution.width.round(),
            "height": resolution.height.round(),
            "maximized": resolution.maximized,
            "fullscreen": resolution.fullscreen,
        });
        self.store.update(|data| {
            store::assign(
                data,
                &format!("{KEY_PREFIX}.launcher-resolution-main"),
                value,
            )
        })
    }
}

/// Same JSON type as the default (nullable object fields are allowed).
fn same_kind(value: &Value, default: &Value) -> bool {
    matches!(
        (value, default),
        (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Object(_), Value::Object(_))
            | (Value::Array(_), Value::Array(_))
    )
}

/// The per-setting `afterSet` side effects, run after every set/update/reset.
fn after_set(state: &Arc<AppState>, id: &str, value: &Value, previous: &Value) {
    let changed = value != previous;
    match id {
        "allow-analytics-collection" if changed => sync_analytics_opt_out(value.as_bool().unwrap_or(true)),
        "auto-check-updates" => crate::services::version_maintenance::start_stop_background_updates(state),
        "disable-roblox-startup" => {
            if value.as_bool() == Some(false) && previous.as_bool() == Some(true) {
                crate::services::launch::restore_startup_registration(state);
            }
        }
        "enable-activity-tracking" => crate::services::activity::start_stop(state),
        "enable-discord-rich-presence" => crate::services::discord_rpc::start_stop(state),
        "handle-roblox-urls" if changed => {
            let state = Arc::clone(state);
            tauri::async_runtime::spawn(async move { crate::services::protocol::sync_handlers(&state).await });
        }
        "mission-control-hotkey" => crate::services::mission_control::sync_hotkey(state),
        "open-on-startup" if changed => sync_open_on_startup(value.as_bool().unwrap_or(false)),
        id if id.starts_with("captures-") => crate::services::captures::broadcast_status(state),
        "use-dev-services" => {
            crate::services::metadata::USE_DEV_SERVICES.store(value.as_bool() == Some(true), std::sync::atomic::Ordering::Relaxed);
            if changed {
                state.updater.reset_environment_state(state);
                let state = Arc::clone(state);
                tauri::async_runtime::spawn(async move {
                    crate::services::metadata::fetch_metadata(&state, None).await;
                });
            }
        }
        _ => {}
    }
}

/// `syncAnalyticsOptOutMarker` with the age gate still pending: only an
/// opt-out writes the uninstaller's marker.
fn sync_analytics_opt_out(enabled: bool) {
    if enabled {
        return;
    }
    if let Err(error) = crate::services::registry::set(r"HKCU\Software\LunarForRoblox", Some("AnalyticsOptOut"), "REG_SZ", "1") {
        tracing::warn!("[Analytics] Could not sync the uninstall opt-out marker: {error}");
    }
}

/// `syncOpenOnStartup` (`app.setLoginItemSettings`, packaged builds only).
fn sync_open_on_startup(enabled: bool) {
    if cfg!(debug_assertions) {
        return;
    }
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "com.moonsworth.roblox";
    let result = match std::env::current_exe() {
        Ok(exe) if enabled => crate::services::registry::set(RUN_KEY, Some(NAME), "REG_SZ", &format!("\"{}\" --launch-to-tray", exe.display())),
        Ok(_) => crate::services::registry::delete(RUN_KEY, Some(NAME)),
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = result {
        tracing::warn!("[LoginItem] Could not update the login item: {error}");
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("settings:get", |ctx, _| async move {
            Ok(ctx.state.settings.get_all())
        })
        .handle("settings:set", |ctx, mut args| async move {
            let id: String = args.get(0)?;
            let value = args.take(1);
            let previous = ctx.state.settings.get(&id);
            let all = ctx.state.settings.set(&id, value)?;
            if let (Some(previous), Some(next)) = (previous, all.get(&id)) {
                after_set(&ctx.state, &id, next, &previous);
            }
            Ok(all)
        })
        .handle("settings:update", |ctx, mut args| async move {
            // Move the patch out of the args instead of deserializing a copy.
            let Value::Object(patch) = args.take(0) else {
                return Err(AppError::msg("settings:update expects an object"));
            };
            let before = ctx.state.settings.get_all();
            let ids: Vec<String> = patch.keys().cloned().collect();
            let all = ctx.state.settings.update(patch)?;
            for id in ids {
                if let (Some(previous), Some(next)) = (before.get(&id), all.get(&id)) {
                    after_set(&ctx.state, &id, next, previous);
                }
            }
            Ok(all)
        })
        .handle("settings:reset", |ctx, _| async move {
            let before = ctx.state.settings.get_all();
            let all = ctx.state.settings.reset()?;
            for (id, previous) in &before {
                if let Some(next) = all.get(id) {
                    after_set(&ctx.state, id, next, previous);
                }
            }
            Ok(all)
        });
}
