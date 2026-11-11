//! Server-driven feature flags (`featureFlags:*`). Port of `FeatureFlagService`.
//!
//! Resolution order per key: dev override (`GUGU_FEATURE_FLAGS`, debug
//! builds only) → server value → registry default. The last server map is
//! cached in `cache.json` (`featureFlags.server`) for offline boots.

use parking_lot::RwLock;
use serde_json::{json, Map, Value};

use crate::ipc::{EventBus, Router};
use crate::services::cache::CacheService;
use crate::util::now_millis;

const CACHE_KEY: &str = "featureFlags.server";
const CHANGED_EVENT: &str = "featureFlags:changed";
#[cfg(debug_assertions)]
const OVERRIDE_ENV: &str = "GUGU_FEATURE_FLAGS";

/// `FEATURE_FLAG_DEFAULTS`, in registry order.
const DEFAULTS: [(&str, bool); 7] = [
    ("page-macros", true),
    ("feature-macros", true),
    ("feature-ads", true),
    ("feature-multi-instance", true),
    ("feature-marketplace-purchases", true),
    ("feature-private-server-purchases", true),
    ("feature-launch-avatar", true),
];

type Flags = [bool; DEFAULTS.len()];

pub struct FeatureFlagService {
    resolved: RwLock<Flags>,
    overrides: [Option<bool>; DEFAULTS.len()],
}

/// `normalizeServerFeatureFlags`: keeps boolean entries of an object.
fn normalize(input: Option<&Value>) -> Map<String, Value> {
    input
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.is_boolean())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn resolve(server: &Map<String, Value>, overrides: &[Option<bool>; DEFAULTS.len()]) -> Flags {
    std::array::from_fn(|i| {
        let (key, default) = DEFAULTS[i];
        overrides[i]
            .or_else(|| server.get(key).and_then(Value::as_bool))
            .unwrap_or(default)
    })
}

fn summarize(flags: &Flags) -> String {
    let off: Vec<&str> = DEFAULTS
        .iter()
        .zip(flags)
        .filter(|(_, on)| !**on)
        .map(|((k, _), _)| *k)
        .collect();
    if off.is_empty() {
        "all on".to_owned()
    } else {
        format!("off: {}", off.join(", "))
    }
}

#[cfg(debug_assertions)]
fn load_dev_overrides() -> [Option<bool>; DEFAULTS.len()] {
    let mut overrides = [None; DEFAULTS.len()];
    let Ok(raw) = std::env::var(OVERRIDE_ENV) else {
        return overrides;
    };
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((key, value)) = entry.split_once('=') else {
            tracing::warn!("[FeatureFlags] Ignoring invalid override entries: {entry}");
            continue;
        };
        let value = match value.trim().to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => {
                tracing::warn!("[FeatureFlags] Ignoring invalid override entries: {entry}");
                continue;
            }
        };
        match DEFAULTS.iter().position(|(k, _)| *k == key.trim()) {
            Some(i) => overrides[i] = Some(value),
            None => tracing::warn!(
                "[FeatureFlags] Ignoring override keys outside the registry: {}",
                key.trim()
            ),
        }
    }
    overrides
}

#[cfg(not(debug_assertions))]
fn load_dev_overrides() -> [Option<bool>; DEFAULTS.len()] {
    [None; DEFAULTS.len()]
}

fn to_json(flags: &Flags) -> Value {
    Value::Object(
        DEFAULTS
            .iter()
            .zip(flags)
            .map(|((k, _), v)| ((*k).to_owned(), Value::Bool(*v)))
            .collect(),
    )
}

impl FeatureFlagService {
    /// `initialize()`: rehydrates the cached server map synchronously.
    pub fn initialize(cache: &CacheService) -> Self {
        let entry = cache.get(CACHE_KEY);
        let server = normalize(entry.as_ref().and_then(|e| e.get("flags")));
        let overrides = load_dev_overrides();
        let resolved = resolve(&server, &overrides);
        let source = if entry.is_some() { "cache" } else { "defaults" };
        tracing::info!(
            "[FeatureFlags] Initialized from {source}: {}",
            summarize(&resolved)
        );
        Self {
            resolved: RwLock::new(resolved),
            overrides,
        }
    }

    pub fn all(&self) -> Value {
        to_json(&self.resolved.read())
    }

    #[allow(dead_code)] // read by gated services in later phases
    pub fn is_enabled(&self, key: &str) -> bool {
        let flags = self.resolved.read();
        DEFAULTS
            .iter()
            .position(|(k, _)| *k == key)
            .is_some_and(|i| flags[i])
    }

    /// `processIncomingFlags`: `None` (server failed to evaluate) keeps the
    /// last map; otherwise persist, re-resolve and notify on change.
    pub fn process_incoming(&self, flags: Option<&Value>, cache: &CacheService, events: &EventBus) {
        let Some(flags) = flags.filter(|f| !f.is_null()) else {
            tracing::warn!("[FeatureFlags] Server could not evaluate flags, keeping the last map");
            return;
        };
        let server = normalize(Some(flags));
        if let Err(error) = cache.set(
            CACHE_KEY,
            json!({ "fetchedAt": now_millis(), "flags": server }),
        ) {
            tracing::warn!(%error, "[FeatureFlags] Failed to persist the server map");
        }
        let next = resolve(&server, &self.overrides);
        let mut resolved = self.resolved.write();
        if *resolved == next {
            return;
        }
        let changes: Vec<String> = DEFAULTS
            .iter()
            .zip(resolved.iter().zip(&next))
            .filter(|(_, (a, b))| a != b)
            .map(|((k, _), (_, b))| format!("{k}={b}"))
            .collect();
        tracing::info!("[FeatureFlags] Changed: {}", changes.join(", "));
        *resolved = next;
        let payload = to_json(&resolved);
        drop(resolved);
        events.emit(CHANGED_EVENT, payload);
    }
}

pub fn register(router: &mut Router) {
    router.handle("featureFlags:getAll", |ctx, _| async move {
        Ok(ctx.state.feature_flags.all())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_beats_server_beats_default() {
        let server = normalize(Some(
            &json!({ "feature-ads": false, "page-macros": "nope", "unknown": false }),
        ));
        let mut overrides = [None; DEFAULTS.len()];
        overrides[1] = Some(false);
        let flags = resolve(&server, &overrides);
        assert_eq!(flags, [true, false, false, true, true, true, true]);
        assert_eq!(summarize(&flags), "off: feature-macros, feature-ads");
    }
}
