//! `AssetDeliveryService`: resolves asset locations through the batched
//! assetdelivery endpoint, downloads them, and keeps a content-addressed
//! disk cache under `cache/assets` with a JSON index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::state::AppState;

const INDEX_FILE: &str = "index.json";
const TMP_SUFFIX: &str = ".tmp";
const RESOLUTION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;
const EVICTION_TARGET_RATIO: f64 = 0.8;
const BATCH_CHUNK_SIZE: usize = 100;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_MAX_BYTES: usize = 64 * 1024 * 1024;
const NO_BATCH_RESPONSE_MESSAGE: &str = "No response for asset in batch";
const NEGATIVE_CACHE_MAX: usize = 4096;
const BATCH_URL: &str = "https://assetdelivery.roblox.com/v2/assets/batch";
/// `setImmediate` stand-in: concurrent fetches queued within this window
/// share one batch round-trip.
const FLUSH_DELAY: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRequest {
    pub asset_id: i64,
    #[serde(default)]
    pub asset_type_id: i64,
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub asset_format: Option<String>,
}

impl AssetRequest {
    pub fn new(asset_id: i64, asset_type_id: i64) -> Self {
        Self { asset_id, asset_type_id, version: None, asset_format: None }
    }

    /// `buildAssetKey`.
    fn key(&self) -> String {
        let version = self.version.map(|v| format!("@{v}")).unwrap_or_default();
        let format = self.asset_format.as_deref().filter(|f| !f.is_empty()).map(|f| format!(":{f}")).unwrap_or_default();
        format!("{}{version}{format}", self.asset_id)
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub reason: &'static str,
    pub message: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AssetBytes {
    Ok { asset_id: i64, bytes: Arc<Vec<u8>>, content_hash: String },
    Failed { asset_id: i64, failure: Failure },
}

impl AssetBytes {
    /// The renderer-facing failure object (`{ status, assetId, reason, message? }`).
    pub fn failure_json(asset_id: i64, failure: &Failure) -> Value {
        let mut value = json!({ "status": "failed", "assetId": asset_id, "reason": failure.reason });
        if let Some(message) = &failure.message {
            value["message"] = json!(message);
        }
        value
    }
}

enum Resolved {
    Location(String),
    Failed(Failure),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    hash: String,
    size_bytes: u64,
    last_access_at: u64,
    resolved_at: u64,
}

type Index = HashMap<String, IndexEntry>;
type Pending = Vec<(AssetRequest, oneshot::Sender<Resolved>)>;

pub struct AssetDelivery {
    dir: PathBuf,
    in_flight: Mutex<HashMap<String, Shared<BoxFuture<'static, AssetBytes>>>>,
    pending: Mutex<HashMap<String, Pending>>,
    pending_order: Mutex<Vec<String>>,
    flush_scheduled: AtomicBool,
    index: Mutex<Option<Index>>,
    epoch: AtomicU64,
    negative: Mutex<lru::LruCache<String, (Failure, Instant)>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn negative_ttl(reason: &str) -> Option<Duration> {
    match reason {
        "moderated" | "notFound" => Some(Duration::from_secs(6 * 60 * 60)),
        "unauthorized" => Some(Duration::from_secs(5 * 60)),
        _ => None,
    }
}

/// `mapAssetErrorToReason`.
fn error_reason(custom: Option<i64>, code: Option<i64>) -> &'static str {
    if custom == Some(14) {
        return "notFound";
    }
    match code {
        Some(401 | 403) => "unauthorized",
        Some(404 | 410) => "notFound",
        _ => "network",
    }
}

fn is_cache_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl AssetDelivery {
    pub fn new(base: &Path) -> Self {
        Self {
            dir: base.join("cache").join("assets"),
            in_flight: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            pending_order: Mutex::new(Vec::new()),
            flush_scheduled: AtomicBool::new(false),
            index: Mutex::new(None),
            epoch: AtomicU64::new(0),
            negative: Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(NEGATIVE_CACHE_MAX).expect("non-zero"))),
        }
    }

    /// `getAssetsBytes`: one result per request, in order.
    pub async fn get_assets_bytes(self: &Arc<Self>, state: &Arc<AppState>, requests: Vec<AssetRequest>) -> Vec<AssetBytes> {
        futures::future::join_all(requests.into_iter().map(|r| self.get_single(state, r))).await
    }

    pub async fn get_one(self: &Arc<Self>, state: &Arc<AppState>, request: AssetRequest) -> AssetBytes {
        self.get_single(state, request).await
    }

    /// `clearCache`.
    pub async fn clear_cache(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        *self.index.lock() = Some(Index::new());
        self.negative.lock().clear();
        let in_flight: Vec<_> = self.in_flight.lock().values().cloned().collect();
        futures::future::join_all(in_flight).await;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            }
        } else {
            let _ = std::fs::create_dir_all(&self.dir);
        }
        tracing::info!("Asset cache cleared");
    }

    fn get_single(self: &Arc<Self>, state: &Arc<AppState>, request: AssetRequest) -> Shared<BoxFuture<'static, AssetBytes>> {
        let key = request.key();
        let mut in_flight = self.in_flight.lock();
        if let Some(existing) = in_flight.get(&key) {
            return existing.clone();
        }
        let (this, state, owned_key) = (Arc::clone(self), Arc::clone(state), key.clone());
        let future = async move {
            let result = this.fetch_single(&state, request, &owned_key).await;
            this.in_flight.lock().remove(&owned_key);
            result
        }
        .boxed()
        .shared();
        in_flight.insert(key, future.clone());
        future
    }

    async fn fetch_single(self: &Arc<Self>, state: &Arc<AppState>, request: AssetRequest, key: &str) -> AssetBytes {
        let asset_id = request.asset_id;
        {
            let mut negative = self.negative.lock();
            match negative.get(key) {
                Some((failure, expires)) if *expires > Instant::now() => {
                    return AssetBytes::Failed { asset_id, failure: failure.clone() };
                }
                Some(_) => {
                    negative.pop(key);
                }
                None => {}
            }
        }
        if let Some((bytes, hash)) = self.read_from_cache(key) {
            return AssetBytes::Ok { asset_id, bytes: Arc::new(bytes), content_hash: hash };
        }
        let location = match self.queue_resolve(state, request, key).await {
            Resolved::Location(location) => location,
            Resolved::Failed(failure) => {
                if let Some(ttl) = negative_ttl(failure.reason) {
                    self.negative.lock().put(key.to_owned(), (failure.clone(), Instant::now() + ttl));
                }
                return AssetBytes::Failed { asset_id, failure };
            }
        };
        match self.download(state, &location).await {
            Ok(bytes) => {
                let hash = format!("{:x}", Sha256::digest(location.as_bytes()));
                if let Err(error) = self.write_to_cache(key, &hash, &bytes) {
                    tracing::warn!("Asset {key} fetch failed: {error}");
                    return AssetBytes::Failed { asset_id, failure: Failure { reason: "network", message: Some(error) } };
                }
                AssetBytes::Ok { asset_id, bytes: Arc::new(bytes), content_hash: hash }
            }
            Err(error) => {
                tracing::warn!("Asset {key} fetch failed: {error}");
                AssetBytes::Failed { asset_id, failure: Failure { reason: "network", message: Some(error) } }
            }
        }
    }

    async fn queue_resolve(self: &Arc<Self>, state: &Arc<AppState>, request: AssetRequest, key: &str) -> Resolved {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            match pending.get_mut(key) {
                Some(queued) => queued.push((request, tx)),
                None => {
                    pending.insert(key.to_owned(), vec![(request, tx)]);
                    self.pending_order.lock().push(key.to_owned());
                }
            }
        }
        if !self.flush_scheduled.swap(true, Ordering::SeqCst) {
            let (this, state) = (Arc::clone(self), Arc::clone(state));
            tokio::spawn(async move {
                tokio::time::sleep(FLUSH_DELAY).await;
                this.flush_scheduled.store(false, Ordering::SeqCst);
                this.flush_resolves(&state).await;
            });
        }
        rx.await.unwrap_or(Resolved::Failed(Failure { reason: "network", message: Some(NO_BATCH_RESPONSE_MESSAGE.into()) }))
    }

    async fn flush_resolves(&self, state: &Arc<AppState>) {
        let mut batch = std::mem::take(&mut *self.pending.lock());
        let keys = std::mem::take(&mut *self.pending_order.lock());
        if batch.is_empty() {
            return;
        }
        let chunks: Vec<Vec<String>> = keys.chunks(BATCH_CHUNK_SIZE).map(<[String]>::to_vec).collect();
        let requests: Vec<Vec<(String, AssetRequest)>> = chunks
            .iter()
            .map(|chunk| chunk.iter().filter_map(|k| Some((k.clone(), batch.get(k)?.first()?.0.clone()))).collect())
            .collect();
        let outcomes = futures::future::join_all(requests.iter().map(|chunk| resolve_chunk(state, chunk))).await;
        for (chunk, mut outcome) in chunks.into_iter().zip(outcomes) {
            for key in chunk {
                let resolved = outcome.remove(&key);
                for (_, sender) in batch.remove(&key).unwrap_or_default() {
                    let value = match &resolved {
                        Some(Resolved::Location(l)) => Resolved::Location(l.clone()),
                        Some(Resolved::Failed(f)) => Resolved::Failed(f.clone()),
                        None => Resolved::Failed(Failure { reason: "network", message: Some(NO_BATCH_RESPONSE_MESSAGE.into()) }),
                    };
                    let _ = sender.send(value);
                }
            }
        }
    }

    async fn download(&self, state: &AppState, location: &str) -> Result<Vec<u8>, String> {
        let response = state.roblox.http().get(location).timeout(DOWNLOAD_TIMEOUT).send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("CDN download failed: {} {}", status.as_u16(), status.canonical_reason().unwrap_or("")));
        }
        if let Some(declared) = response.content_length().filter(|l| *l as usize > DOWNLOAD_MAX_BYTES) {
            return Err(format!("Asset body declares {declared} bytes, above the {DOWNLOAD_MAX_BYTES} byte cap"));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > DOWNLOAD_MAX_BYTES {
                return Err(format!("Asset body exceeded the {DOWNLOAD_MAX_BYTES} byte download cap"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    // ---- disk cache ----

    fn with_index<T>(&self, f: impl FnOnce(&mut Index) -> T) -> T {
        let mut guard = self.index.lock();
        let index = guard.get_or_insert_with(|| self.read_index_from_disk());
        f(index)
    }

    fn read_from_cache(&self, key: &str) -> Option<(Vec<u8>, String)> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let entry = self.with_index(|index| index.get(key).cloned())?;
        if now_ms().saturating_sub(entry.resolved_at) > RESOLUTION_TTL_MS {
            self.with_index(|index| {
                index.remove(key);
                self.remove_unreferenced(index, &entry.hash);
                if epoch == self.epoch.load(Ordering::SeqCst) {
                    self.persist(index);
                }
            });
            return None;
        }
        let Ok(bytes) = std::fs::read(self.dir.join(&entry.hash)) else {
            if epoch == self.epoch.load(Ordering::SeqCst) {
                self.with_index(|index| {
                    index.remove(key);
                    self.persist(index);
                });
            }
            return None;
        };
        if epoch != self.epoch.load(Ordering::SeqCst) {
            return None;
        }
        self.with_index(|index| {
            if let Some(e) = index.get_mut(key) {
                e.last_access_at = now_ms();
            }
            self.persist(index);
        });
        tracing::debug!("Asset cache hit for {key} ({})", entry.hash);
        Some((bytes, entry.hash))
    }

    fn write_to_cache(&self, key: &str, hash: &str, bytes: &[u8]) -> Result<(), String> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let final_path = self.dir.join(hash);
        let tmp = self.dir.join(format!("{hash}.{}{TMP_SUFFIX}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &final_path).map_err(|e| e.to_string())?;
        if epoch != self.epoch.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&final_path);
            return Ok(());
        }
        self.with_index(|index| {
            let now = now_ms();
            index.insert(key.to_owned(), IndexEntry { hash: hash.to_owned(), size_bytes: bytes.len() as u64, last_access_at: now, resolved_at: now });
            self.evict_if_over_cap(index);
            if epoch == self.epoch.load(Ordering::SeqCst) {
                self.persist(index);
            }
        });
        Ok(())
    }

    fn evict_if_over_cap(&self, index: &mut Index) {
        let mut total: u64 = index.values().map(|e| e.size_bytes).sum();
        if total <= CACHE_MAX_BYTES {
            return;
        }
        let target = CACHE_MAX_BYTES as f64 * EVICTION_TARGET_RATIO;
        let mut entries: Vec<(String, IndexEntry)> = index.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by_key(|(_, e)| e.last_access_at);
        let mut evicted = Vec::new();
        for (key, entry) in entries {
            if total as f64 <= target {
                break;
            }
            index.remove(&key);
            total -= entry.size_bytes;
            evicted.push(entry.hash);
        }
        for hash in &evicted {
            self.remove_unreferenced(index, hash);
        }
        tracing::info!("Asset cache evicted {} entries down to size cap", evicted.len());
    }

    fn remove_unreferenced(&self, index: &Index, hash: &str) {
        if index.values().any(|e| e.hash == hash) {
            return;
        }
        let path = self.dir.join(hash);
        if let Err(error) = std::fs::remove_file(&path) {
            if path.exists() {
                tracing::warn!("Failed to evict cached asset {hash}: {error}");
            }
        }
    }

    fn read_index_from_disk(&self) -> Index {
        let path = self.dir.join(INDEX_FILE);
        let mut index: Index = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!("Asset cache index unreadable, starting fresh: {error}");
                Index::new()
            }),
            Err(_) => Index::new(),
        };
        index.retain(|_, e| is_cache_hash(&e.hash));
        self.reconcile(&index);
        index
    }

    /// `reconcileCacheDir`: drop files the index does not reference.
    fn reconcile(&self, index: &Index) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let referenced: std::collections::HashSet<&str> = index.values().map(|e| e.hash.as_str()).collect();
        let orphaned: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name != INDEX_FILE && !referenced.contains(name.as_ref())
            })
            .map(|e| e.path())
            .collect();
        if orphaned.is_empty() {
            return;
        }
        for path in &orphaned {
            let removed = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
            if let Err(error) = removed {
                tracing::warn!("Failed to remove orphaned cache file {}: {error}", path.display());
            }
        }
        tracing::info!("Asset cache reconcile removed {} unreferenced files", orphaned.len());
    }

    /// `persistIndex`: tmp file renamed over the index (runs under the index lock, so writes never interleave).
    fn persist(&self, index: &Index) {
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            let path = self.dir.join(INDEX_FILE);
            let tmp = self.dir.join(format!("{INDEX_FILE}{TMP_SUFFIX}"));
            std::fs::write(&tmp, serde_json::to_vec(index).unwrap_or_default())?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(error) = write() {
            tracing::warn!("Failed to persist asset cache index: {error}");
        }
    }
}

/// `resolveChunk`: one POST to the batch endpoint.
async fn resolve_chunk(state: &AppState, chunk: &[(String, AssetRequest)]) -> HashMap<String, Resolved> {
    let mut outcomes = HashMap::new();
    let body: Vec<Value> = chunk
        .iter()
        .map(|(key, request)| {
            let mut item = json!({ "requestId": key, "assetId": request.asset_id });
            if let Some(version) = request.version {
                item["version"] = json!(version);
            }
            if let Some(format) = &request.asset_format {
                item["assetFormat"] = json!(format);
            }
            item
        })
        .collect();
    let formats: HashMap<&str, Option<&str>> = chunk.iter().map(|(k, r)| (k.as_str(), r.asset_format.as_deref())).collect();
    let response: Result<Vec<Value>, String> = async {
        let response = state.roblox.request(reqwest::Method::POST, BATCH_URL, Some(&Value::Array(body))).await.map_err(|e| e.to_string())?;
        crate::roblox::RobloxClient::decode::<Vec<Value>>(BATCH_URL, response).await.map_err(|e| e.to_string())
    }
    .await;
    let items = match response {
        Ok(items) => items,
        Err(message) => {
            tracing::warn!("Asset batch resolve failed: {message}");
            for (key, _) in chunk {
                outcomes.insert(key.clone(), Resolved::Failed(Failure { reason: "network", message: Some(message.clone()) }));
            }
            return outcomes;
        }
    };
    for item in items {
        let Some(key) = item.get("requestId").and_then(Value::as_str).filter(|k| !k.is_empty()) else { continue };
        let requested = formats.get(key).copied().flatten();
        let locations: Vec<&Value> = item
            .get("locations")
            .and_then(Value::as_array)
            .map(|l| l.iter().filter(|e| e.get("location").and_then(Value::as_str).is_some_and(|s| !s.is_empty())).collect())
            .unwrap_or_default();
        let matched = match requested {
            Some(format) => locations.iter().find(|e| e.get("assetFormat").and_then(Value::as_str) == Some(format)).copied(),
            None => locations.first().copied(),
        };
        let chosen = matched.or_else(|| locations.first().copied());
        if let (Some(chosen), Some(format), None) = (chosen, requested, matched) {
            let used = chosen.get("assetFormat").and_then(|f| f.as_str()).unwrap_or("");
            tracing::warn!("Asset {key} resolved without requested format {format}; using {used}");
        }
        if let Some(location) = chosen.and_then(|c| c.get("location")).and_then(Value::as_str) {
            outcomes.insert(key.to_owned(), Resolved::Location(location.to_owned()));
            continue;
        }
        let first_error = item.get("errors").and_then(Value::as_array).and_then(|e| e.first());
        let field = |name: &str| first_error.and_then(|e| e.get(name)).and_then(Value::as_i64);
        outcomes.insert(
            key.to_owned(),
            Resolved::Failed(Failure {
                reason: error_reason(field("customErrorCode"), field("code")),
                message: first_error.and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_owned),
            }),
        );
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_reasons() {
        let mut request = AssetRequest::new(5, 79);
        assert_eq!(request.key(), "5");
        request.version = Some(2);
        request.asset_format = Some("avatar_meshpart_head".into());
        assert_eq!(request.key(), "5@2:avatar_meshpart_head");
        assert_eq!(error_reason(Some(14), Some(500)), "notFound");
        assert_eq!(error_reason(None, Some(403)), "unauthorized");
        assert_eq!(error_reason(None, None), "network");
    }
}
