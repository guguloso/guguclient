//! Launcher settings (`settings:*`). Port of `SettingsService`/`SettingsStore`:
//! same file (`settings/launcher.json`), same keys (`settings.<id>`), same
//! defaults, same "unknown setting" error and same return shapes.
//!
//! Pending for full parity: per-setting `afterSet` side effects (open on
//! startup registry key, Discord RPC toggle, theme broadcast) and analytics.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::paths::LauncherPaths;
use crate::store::{self, JsonStore};

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
            stored.filter(|v| same_kind(v, default)).unwrap_or(default).clone()
        }))
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
            self.store.update(|data| store::assign(data, &format!("{KEY_PREFIX}.{id}"), value))?;
        }
        Ok(self.get_all())
    }

    /// Applies a partial patch; unknown ids are ignored, like Electron.
    pub fn update(&self, patch: Map<String, Value>) -> AppResult<Map<String, Value>> {
        self.store.update(|data| {
            for (id, value) in patch {
                if self.defaults.get(&id).is_some_and(|default| same_kind(&value, default)) {
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
            .unwrap_or(Resolution { width: 1400.0, height: 800.0, maximized: false, fullscreen: false })
    }

    pub fn save_main_resolution(&self, resolution: Resolution) -> AppResult<()> {
        let value = serde_json::json!({
            "width": resolution.width.round(),
            "height": resolution.height.round(),
            "maximized": resolution.maximized,
            "fullscreen": resolution.fullscreen,
        });
        self.store
            .update(|data| store::assign(data, &format!("{KEY_PREFIX}.launcher-resolution-main"), value))
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

pub fn register(router: &mut Router) {
    router
        .handle("settings:get", |ctx, _| async move { Ok(ctx.state.settings.get_all()) })
        .handle("settings:set", |ctx, mut args| async move {
            let id: String = args.get(0)?;
            let value = args.take(1);
            ctx.state.settings.set(&id, value)
        })
        .handle("settings:update", |ctx, mut args| async move {
            // Move the patch out of the args instead of deserializing a copy.
            let Value::Object(patch) = args.take(0) else {
                return Err(AppError::msg("settings:update expects an object"));
            };
            ctx.state.settings.update(patch)
        })
        .handle("settings:reset", |ctx, _| async move { ctx.state.settings.reset() });
}
