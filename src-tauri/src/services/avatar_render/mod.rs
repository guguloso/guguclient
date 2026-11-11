//! `avatarRender:*`. Port of `AvatarRenderService`: downloads avatar assets,
//! parses meshes / models / animations in the main process and hands the
//! renderer ready-to-upload geometry and image bytes.

mod binary;
mod delivery;
mod extract;
mod mesh;
mod payload;
mod rbxm;

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::ipc::Router;
use crate::state::AppState;
use delivery::{AssetBytes, AssetDelivery, AssetRequest};
use payload::ParseContext;

const PARSED_CACHE_MAX: usize = 128;
const PARSED_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const PARSED_MIN_SIZE_BYTES: usize = 1024;
const PARSED_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

struct ParsedEntry {
    payload: Arc<Value>,
    missing: Option<Arc<Value>>,
    size: usize,
    expires: Instant,
}

/// LRU bounded by entry count, total size and TTL (`lru-cache` options).
struct ParsedCache {
    entries: lru::LruCache<String, ParsedEntry>,
    total: usize,
}

impl ParsedCache {
    fn new() -> Self {
        Self { entries: lru::LruCache::unbounded(), total: 0 }
    }

    fn get(&mut self, key: &str) -> Option<(Arc<Value>, Option<Arc<Value>>)> {
        let expired = self.entries.peek(key)?.expires <= Instant::now();
        if expired {
            if let Some(entry) = self.entries.pop(key) {
                self.total -= entry.size;
            }
            return None;
        }
        self.entries.get(key).map(|e| (Arc::clone(&e.payload), e.missing.clone()))
    }

    fn set(&mut self, key: String, payload: Arc<Value>, missing: Option<Arc<Value>>) {
        let size = payload::typed_bytes(&payload).max(PARSED_MIN_SIZE_BYTES);
        if size > PARSED_CACHE_MAX_BYTES {
            return;
        }
        if let Some(old) = self.entries.pop(&key) {
            self.total -= old.size;
        }
        self.entries.put(key, ParsedEntry { payload, missing, size, expires: Instant::now() + PARSED_CACHE_TTL });
        self.total += size;
        while self.entries.len() > PARSED_CACHE_MAX || self.total > PARSED_CACHE_MAX_BYTES {
            let Some((_, evicted)) = self.entries.pop_lru() else { break };
            self.total -= evicted.size;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }
}

struct AvatarRender {
    delivery: Mutex<Option<Arc<AssetDelivery>>>,
    parsed: Mutex<ParsedCache>,
    defaults: tokio::sync::Mutex<Option<Value>>,
}

static SERVICE: LazyLock<AvatarRender> =
    LazyLock::new(|| AvatarRender { delivery: Mutex::new(None), parsed: Mutex::new(ParsedCache::new()), defaults: tokio::sync::Mutex::new(None) });

fn delivery(state: &AppState) -> Arc<AssetDelivery> {
    Arc::clone(SERVICE.delivery.lock().get_or_insert_with(|| Arc::new(AssetDelivery::new(&state.paths.base))))
}

/// `toFetchRequest`: dynamic heads are fetched in their MeshPart head format.
fn to_fetch_request(request: &AssetRequest) -> AssetRequest {
    let mut fetch = request.clone();
    if request.asset_type_id == payload::DYNAMIC_HEAD_ASSET_TYPE_ID {
        fetch.asset_format = Some(payload::DYNAMIC_HEAD_ASSET_FORMAT.to_owned());
    }
    fetch
}

fn ok_result(asset_id: i64, payload: &Value, missing: Option<&Value>) -> Value {
    let mut out = json!({ "status": "ok", "assetId": asset_id, "payload": payload });
    if let Some(missing) = missing {
        out["missingSubAssets"] = missing.clone();
    }
    out
}

/// `toAssetResult`.
async fn to_asset_result(state: &Arc<AppState>, delivery: &Arc<AssetDelivery>, result: AssetBytes, request: &AssetRequest) -> Value {
    let (asset_id, bytes, hash) = match result {
        AssetBytes::Failed { asset_id, failure } => return AssetBytes::failure_json(asset_id, &failure),
        AssetBytes::Ok { asset_id, bytes, content_hash } => (asset_id, bytes, content_hash),
    };
    let key = format!("{}:{hash}", request.asset_type_id);
    if let Some((payload, missing)) = SERVICE.parsed.lock().get(&key) {
        return ok_result(asset_id, &payload, missing.as_deref());
    }
    let ctx = ParseContext::new(state, delivery);
    let parsed = match payload::parse_payload(&ctx, &bytes, request).await {
        Ok(parsed) => parsed,
        Err(message) => {
            tracing::warn!("Asset {asset_id} parse failed: {message}");
            return json!({ "status": "failed", "assetId": asset_id, "reason": "parseError", "message": message });
        }
    };
    let Some(parsed) = parsed else {
        return json!({
            "status": "failed",
            "assetId": asset_id,
            "reason": "unsupportedFormat",
            "message": format!("No parser for payload kind \"{}\" and asset type {}", payload::classify(&bytes), request.asset_type_id),
        });
    };
    let misses = std::mem::take(&mut *ctx.misses.lock());
    let missing = (!misses.is_empty()).then(|| Arc::new(Value::Array(misses)));
    let result = ok_result(asset_id, &parsed, missing.as_deref());
    SERVICE.parsed.lock().set(key, Arc::new(parsed), missing);
    result
}

/// `getAssets`: one result per reference, in order.
async fn get_assets(state: &Arc<AppState>, refs: Vec<AssetRequest>) -> Vec<Value> {
    let delivery = delivery(state);
    let results = delivery.get_assets_bytes(state, refs.iter().map(to_fetch_request).collect()).await;
    futures::future::join_all(results.into_iter().zip(refs.iter()).map(|(result, request)| to_asset_result(state, &delivery, result, request))).await
}

/// `getDefaults`: memoized only once a load is complete.
async fn get_defaults(state: &Arc<AppState>) -> Value {
    let mut cached = SERVICE.defaults.lock().await;
    if let Some(value) = cached.as_ref() {
        return value.clone();
    }
    let (value, complete) = payload::load_defaults(state, &delivery(state)).await;
    if complete {
        *cached = Some(value.clone());
    }
    value
}

pub fn register(router: &mut Router) {
    router
        .handle("avatarRender:getAssets", |ctx, args| async move {
            let refs: Vec<AssetRequest> = args.get(0)?;
            Ok(get_assets(&ctx.state, refs).await)
        })
        .handle("avatarRender:prefetch", |ctx, args| async move {
            let refs: Vec<AssetRequest> = args.get(0)?;
            let results = get_assets(&ctx.state, refs).await;
            let failed = results.iter().filter(|r| r.get("status").and_then(Value::as_str) == Some("failed")).count();
            if failed > 0 {
                tracing::debug!("Avatar asset prefetch: {failed}/{} failed", results.len());
            }
            Ok(Value::Null)
        })
        .handle("avatarRender:getDefaults", |ctx, _args| async move { Ok(get_defaults(&ctx.state).await) })
        .handle("avatarRender:clearCache", |ctx, _args| async move {
            SERVICE.parsed.lock().clear();
            *SERVICE.defaults.lock().await = None;
            delivery(&ctx.state).clear_cache().await;
            Ok(Value::Null)
        });
}
