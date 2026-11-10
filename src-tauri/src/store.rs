//! `electron-store` compatible JSON file store.
//!
//! Same on-disk format (tab-indented JSON, dot-path keys, `__internal__`
//! preserved) so the Electron and Rust builds can read each other's files.
//! Writes are atomic (temp file + rename) and happen outside the lock.

use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde_json::{Map, Value};

use crate::error::AppResult;

pub struct JsonStore {
    path: PathBuf,
    data: RwLock<Value>,
}

impl JsonStore {
    /// Opens `<dir>/<name>.json`. An unreadable or invalid file starts empty,
    /// matching electron-store's `clearInvalidConfig: true`.
    pub fn open(dir: &Path, name: &str) -> AppResult<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{name}.json"));
        let data = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        Ok(Self { path, data: RwLock::new(data) })
    }

    /// Runs `f` against the value at `key` without cloning the tree.
    pub fn read<R>(&self, key: &str, f: impl FnOnce(Option<&Value>) -> R) -> R {
        let data = self.data.read();
        f(lookup(&data, key))
    }

    /// Keys of the root object, in file order.
    pub fn top_level_keys(&self) -> Vec<String> {
        self.data.read().as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
    }

    /// Mutates the whole document under the write lock, then persists once.
    pub fn update<R>(&self, f: impl FnOnce(&mut Value) -> R) -> AppResult<R> {
        let (result, snapshot) = {
            let mut data = self.data.write();
            let result = f(&mut data);
            (result, serialize(&data)?)
        };
        self.persist(&snapshot)?;
        Ok(result)
    }

    fn persist(&self, bytes: &[u8]) -> AppResult<()> {
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Resolves a dot-separated path (`"settings.close-to-tray"`).
pub fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(root, |node, part| node.get(part))
}

/// Sets a dot-separated path, creating intermediate objects as needed.
pub fn assign(root: &mut Value, key: &str, value: Value) {
    let mut node = root;
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        let map = node.as_object_mut().expect("just ensured object");
        if parts.peek().is_none() {
            map.insert(part.to_owned(), value);
            return;
        }
        node = map.entry(part.to_owned()).or_insert_with(|| Value::Object(Map::new()));
    }
}

/// Removes a dot-separated path if present.
pub fn remove(root: &mut Value, key: &str) {
    match key.rsplit_once('.') {
        Some((parent, leaf)) => {
            if let Some(Value::Object(map)) = lookup_mut(root, parent) {
                map.remove(leaf);
            }
        }
        None => {
            if let Value::Object(map) = root {
                map.remove(key);
            }
        }
    }
}

fn lookup_mut<'a>(root: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    key.split('.').try_fold(root, |node, part| node.get_mut(part))
}

fn serialize(value: &Value) -> AppResult<Vec<u8>> {
    let mut out = Vec::with_capacity(1024);
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(value, &mut serializer)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dot_paths_round_trip() {
        let mut root = json!({});
        assign(&mut root, "settings.close-to-tray", json!(false));
        assert_eq!(lookup(&root, "settings.close-to-tray"), Some(&json!(false)));
        remove(&mut root, "settings.close-to-tray");
        assert_eq!(lookup(&root, "settings.close-to-tray"), None);
        assert!(root.get("settings").is_some());
    }
}
