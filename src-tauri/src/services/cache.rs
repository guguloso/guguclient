//! `settings/cache.json` key-value cache. Port of `CacheService`.

use serde_json::Value;

use crate::error::AppResult;
use crate::paths::LauncherPaths;
use crate::store::{self, JsonStore};

const CACHE_STORE_NAME: &str = "cache";

pub struct CacheService {
    store: JsonStore,
    /// Top-level keys present at startup, frozen (fresh-install detection).
    top_level_keys: Vec<String>,
}

impl CacheService {
    pub fn open(paths: &LauncherPaths) -> AppResult<Self> {
        let store = JsonStore::open(&paths.settings_dir, CACHE_STORE_NAME)?;
        let top_level_keys = store.top_level_keys();
        let shown = if top_level_keys.is_empty() { "<none>".to_owned() } else { top_level_keys.join(", ") };
        tracing::info!("[Cache] Loaded cache.json (top-level keys: {shown})");
        Ok(Self { store, top_level_keys })
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.store.read(key, |v| v.cloned())
    }

    pub fn set(&self, key: &str, value: Value) -> AppResult<()> {
        self.store.update(|root| store::assign(root, key, value))
    }

    pub fn top_level_keys_at_startup(&self) -> &[String] {
        &self.top_level_keys
    }
}
