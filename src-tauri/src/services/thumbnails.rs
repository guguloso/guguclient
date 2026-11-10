//! Thumbnails (`thumbnails:*`) and the `lbx-thumb` image protocol.
//! Port of `ThumbnailService`, `ThumbnailCacheService` and
//! `ThumbnailProtocolService`.
//!
//! - URL cache: in-memory LRU keyed by request shape (no repeated API calls)
//! - Image cache: images are proxied through `http://lbx-thumb.localhost/?src=`
//!   and stored on disk under a SHA-256 name, so reopening a page is instant.
//!
//! Pending: `thumbnails:getSingleWithRetry`.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::try_join_all;
use lru::LruCache;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::error::AppResult;
use crate::ipc::Router;
use crate::roblox::{endpoints, RobloxClient};
use crate::state::AppState;

pub const PROTOCOL: &str = "lbx-thumb";
const LOCAL_PREFIX: &str = "http://lbx-thumb.localhost/?src=";
const URL_CACHE_CAPACITY: usize = 4096;
const BATCH_SIZE: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbnailRequest {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    target_id: u64,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    is_circular: Option<bool>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    /// Cache-bust signal (`request.version`).
    #[serde(default)]
    version: Option<Value>,
}

impl ThumbnailRequest {
    fn cache_key(&self) -> Option<String> {
        if self.token.is_some() || self.alias.is_some() {
            return None;
        }
        Some(format!(
            "{}:{}:{}:{}:{}",
            self.kind,
            self.target_id,
            self.size.as_deref().unwrap_or(""),
            self.format.as_deref().unwrap_or(""),
            self.is_circular.unwrap_or(false)
        ))
    }

    fn to_api(&self, correlation_id: String) -> Value {
        let mut body = json!({
            "requestId": correlation_id,
            "type": self.kind,
            "targetId": self.target_id,
            "size": self.size.as_deref().unwrap_or("150x150"),
            "format": self.format.as_deref().unwrap_or("png"),
            "isCircular": self.is_circular.unwrap_or(false),
        });
        if let Some(token) = &self.token {
            body["token"] = Value::String(token.clone());
        }
        if let Some(alias) = &self.alias {
            body["alias"] = Value::String(alias.clone());
        }
        body
    }
}

/// One entry of a Roblox thumbnails response (`data[]`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbEntry {
    #[serde(default)]
    pub target_id: Option<u64>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ThumbPage {
    #[serde(default)]
    pub data: Vec<ThumbEntry>,
}

/// Identity of an id-based thumbnail query (`ThumbnailCacheService.buildKey`).
pub struct IdQuery<'a> {
    pub kind: &'a str,
    pub size: &'a str,
    pub format: &'a str,
    pub include_profile_frame: Option<bool>,
}

impl IdQuery<'_> {
    fn key(&self, id: u64) -> String {
        format!(
            "{}:{id}:{}:{}:{}",
            self.kind,
            self.size,
            self.format,
            self.include_profile_frame
                .map_or("", |f| if f { "frame" } else { "noframe" })
        )
    }
}

const RETRYABLE_STATES: [&str; 3] = ["Pending", "InReview", "TemporarilyUnavailable"];
const MAX_RETRIES: usize = 3;
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

pub struct ThumbnailService {
    roblox: Arc<RobloxClient>,
    urls: Mutex<LruCache<String, String>>,
    /// id-based cache: key -> remote URL (mapped to the local protocol on read).
    ids: Mutex<LruCache<String, String>>,
    disk_dir: PathBuf,
}

impl ThumbnailService {
    pub fn new(roblox: Arc<RobloxClient>, disk_dir: PathBuf) -> Self {
        let capacity = NonZeroUsize::new(URL_CACHE_CAPACITY).expect("non-zero capacity");
        Self {
            roblox,
            urls: Mutex::new(LruCache::new(capacity)),
            ids: Mutex::new(LruCache::new(capacity)),
            disk_dir,
        }
    }

    /// Port of `ThumbnailCacheService.batchFetch`: serves cached ids, fetches
    /// the rest and retries ids still rendering (`Pending`, ...) up to 3 times.
    /// Returns `{ "<id>": "<local url>" }` like `Object.fromEntries(map)`.
    pub async fn batch_by_id<F, Fut>(
        &self,
        ids: &[u64],
        query: &IdQuery<'_>,
        fetch: F,
    ) -> AppResult<std::collections::BTreeMap<String, String>>
    where
        F: Fn(Vec<u64>) -> Fut,
        Fut: std::future::Future<Output = AppResult<Vec<ThumbEntry>>>,
    {
        let mut result = std::collections::BTreeMap::new();
        let mut pending = Vec::new();
        {
            let mut cache = self.ids.lock();
            for id in endpoints::unique_positive(ids) {
                match cache.get(&query.key(id)) {
                    Some(url) => {
                        result.insert(id.to_string(), to_local_url(url));
                    }
                    None => pending.push(id),
                }
            }
        }

        for attempt in 0..=MAX_RETRIES {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            let entries = match fetch(std::mem::take(&mut pending)).await {
                Ok(entries) => entries,
                Err(error) if result.is_empty() => return Err(error),
                Err(_) => return Ok(result),
            };
            let mut cache = self.ids.lock();
            for entry in entries {
                let Some(target) = entry.target_id else {
                    continue;
                };
                match (entry.state.as_deref(), entry.image_url) {
                    (Some("Completed"), Some(url)) => {
                        result.insert(target.to_string(), to_local_url(&url));
                        cache.put(query.key(target), url);
                    }
                    (Some(state), _) if RETRYABLE_STATES.contains(&state) => pending.push(target),
                    _ => {}
                }
            }
        }
        Ok(result)
    }

    /// `thumbnails:getBatch` — same response shape and ordering as Electron.
    pub async fn get_batch(&self, requests: Vec<ThumbnailRequest>) -> AppResult<Vec<Value>> {
        let mut responses = vec![Value::Null; requests.len()];
        let mut misses: Vec<(usize, Option<String>)> = Vec::new();
        {
            let mut cache = self.urls.lock();
            for (index, request) in requests.iter().enumerate() {
                let key = request.cache_key();
                if request.version.is_some() {
                    if let Some(key) = &key {
                        cache.pop(key);
                    }
                }
                match key.as_ref().and_then(|k| cache.get(k)) {
                    Some(url) => {
                        responses[index] = json!({
                            "requestId": request.request_id,
                            "targetId": request.target_id,
                            "state": "Completed",
                            "imageUrl": url,
                        });
                    }
                    None => misses.push((index, key)),
                }
            }
        }
        if misses.is_empty() {
            return Ok(responses);
        }

        let body: Vec<Value> = misses
            .iter()
            .map(|(index, _)| requests[*index].to_api(format!("__lbx_cache_{index}")))
            .collect();
        let url = format!("{}/batch", endpoints::THUMBNAILS_V1);
        let pages = try_join_all(body.chunks(BATCH_SIZE).map(|chunk| async {
            let page: Value = self.roblox.post(&url, chunk).await?;
            AppResult::Ok(page)
        }))
        .await?;

        let mut by_correlation: HashMap<String, Map<String, Value>> =
            HashMap::with_capacity(misses.len());
        for mut page in pages {
            // Move the entries out of the response instead of cloning them.
            if let Some(Value::Array(entries)) = page.get_mut("data").map(Value::take) {
                for entry in entries {
                    if let Value::Object(entry) = entry {
                        if let Some(id) = entry.get("requestId").and_then(Value::as_str) {
                            by_correlation.insert(id.to_owned(), entry);
                        }
                    }
                }
            }
        }

        let mut cache = self.urls.lock();
        for (index, key) in misses {
            let request = &requests[index];
            let Some(mut entry) = by_correlation.remove(&format!("__lbx_cache_{index}")) else {
                responses[index] = json!({ "requestId": request.request_id });
                continue;
            };
            let completed = entry.get("state").and_then(Value::as_str) == Some("Completed");
            if let Some(Value::String(remote)) = entry.get("imageUrl") {
                let local = to_local_url(remote);
                if completed {
                    if let Some(key) = key {
                        cache.put(key, local.clone());
                    }
                }
                entry.insert("imageUrl".into(), Value::String(local));
            }
            entry.insert("requestId".into(), json!(request.request_id));
            responses[index] = Value::Object(entry);
        }
        Ok(responses)
    }

    /// `getSingleWithRetry`: one request, polled while Roblox renders it.
    pub async fn get_single_with_retry(
        &self,
        request: ThumbnailRequest,
        max_retries: u32,
        retry_delay_ms: u64,
    ) -> AppResult<Value> {
        let key = request.cache_key();
        if let Some(key) = &key {
            let mut cache = self.urls.lock();
            if request.version.is_some() {
                cache.pop(key);
            } else if let Some(url) = cache.get(key) {
                return Ok(json!({
                    "requestId": request.request_id,
                    "targetId": request.target_id,
                    "state": "Completed",
                    "imageUrl": url,
                }));
            }
        }
        let url = format!("{}/batch", endpoints::THUMBNAILS_V1);
        let body = [request.to_api(request.request_id.clone().unwrap_or_default())];
        let mut retries = 0;
        loop {
            let mut page: Value = self.roblox.post(&url, &body).await?;
            let Some(Value::Object(mut entry)) = page
                .get_mut("data")
                .and_then(|d| d.get_mut(0))
                .map(Value::take)
            else {
                return Ok(Value::Null);
            };
            let state = entry
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if state == "Pending" && retries < max_retries {
                retries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(retry_delay_ms)).await;
                continue;
            }
            if let Some(Value::String(remote)) = entry.get("imageUrl") {
                let local = to_local_url(remote);
                if let (true, Some(key)) = (state == "Completed", &key) {
                    self.urls.lock().put(key.clone(), local.clone());
                }
                entry.insert("imageUrl".into(), Value::String(local));
            }
            return Ok(Value::Object(entry));
        }
    }

    /// Serves `http://lbx-thumb.localhost/?src=<url>` from disk, fetching once.
    async fn serve(&self, uri: &str) -> Response<Vec<u8>> {
        let Some(source) = uri
            .split_once("src=")
            .and_then(|(_, encoded)| urlencoding::decode(encoded).ok())
            .map(|s| s.into_owned())
            .filter(|s| is_allowed_source(s))
        else {
            return plain(StatusCode::BAD_REQUEST);
        };

        let file = self.disk_dir.join(hex_sha256(&source));
        if let Ok(bytes) = tokio::fs::read(&file).await {
            return image(bytes);
        }
        let fetched = async {
            let response = self
                .roblox
                .http()
                .get(&source)
                .send()
                .await?
                .error_for_status()?;
            Ok::<_, reqwest::Error>(response.bytes().await?.to_vec())
        }
        .await;
        match fetched {
            Ok(bytes) => {
                if let Err(error) = tokio::fs::write(&file, &bytes).await {
                    tracing::debug!(%error, "thumbnail disk cache write failed");
                }
                image(bytes)
            }
            Err(error) => {
                tracing::debug!(%error, "thumbnail fetch failed");
                plain(StatusCode::BAD_GATEWAY)
            }
        }
    }
}

/// Async URI scheme handler registered on the Tauri builder.
pub fn protocol_handler<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let state = Arc::clone(ctx.app_handle().state::<Arc<AppState>>().inner());
    let uri = request.uri().to_string();
    tauri::async_runtime::spawn(async move {
        responder.respond(state.thumbnails.serve(&uri).await);
    });
}

pub fn to_local_url(remote: &str) -> String {
    if remote.starts_with(LOCAL_PREFIX) || !is_allowed_source(remote) {
        return remote.to_owned();
    }
    format!("{LOCAL_PREFIX}{}", urlencoding::encode(remote))
}

/// Electron's on-disk form of a proxied thumbnail (`lbx-thumb://image/?src=`).
const ELECTRON_PREFIX: &str = "lbx-thumb://image/?src=";

/// Persisted URL → URL the WebView can load (Electron caches use the
/// `lbx-thumb://` scheme, which WebView2 cannot resolve).
pub fn from_persisted_url(url: &str) -> String {
    match url.strip_prefix(ELECTRON_PREFIX) {
        Some(encoded) => format!("{LOCAL_PREFIX}{encoded}"),
        None => url.to_owned(),
    }
}

/// Local URL → Electron's persisted form, so both builds share caches.
pub fn to_persisted_url(url: &str) -> String {
    match url.strip_prefix(LOCAL_PREFIX) {
        Some(encoded) => format!("{ELECTRON_PREFIX}{encoded}"),
        None => url.to_owned(),
    }
}

fn is_allowed_source(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host == "rbxcdn.com" || host.ends_with(".rbxcdn.com"))
}

fn hex_sha256(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sniff_content_type(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'G', b'I', b'F', ..] => "image/gif",
        _ => "application/octet-stream",
    }
}

fn image(bytes: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, sniff_content_type(&bytes))
        .header(header::CACHE_CONTROL, "public, max-age=604800, immutable")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(bytes)
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR))
}

fn plain(status: StatusCode) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = status;
    response
}

pub fn register(router: &mut Router) {
    router
        .handle("thumbnails:getBatch", |ctx, args| async move {
            let requests: Vec<ThumbnailRequest> = args.get(0)?;
            ctx.state.thumbnails.get_batch(requests).await
        })
        .handle("thumbnails:getSingleWithRetry", |ctx, args| async move {
            let request: ThumbnailRequest = args.get(0)?;
            let max_retries: Option<u32> = args.opt(1)?;
            let delay: Option<u64> = args.opt(2)?;
            ctx.state
                .thumbnails
                .get_single_with_retry(request, max_retries.unwrap_or(5), delay.unwrap_or(1000))
                .await
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_only_for_roblox_cdn() {
        assert!(
            to_local_url("https://tr.rbxcdn.com/abc/150/150/Image/Png").starts_with(LOCAL_PREFIX)
        );
        assert_eq!(
            to_local_url("https://evil.example/x.png"),
            "https://evil.example/x.png"
        );
        let local = to_local_url("https://tr.rbxcdn.com/abc/150/150/Image/Png");
        let persisted = to_persisted_url(&local);
        assert!(persisted.starts_with("lbx-thumb://image/?src="));
        assert_eq!(from_persisted_url(&persisted), local);
        assert_eq!(sniff_content_type(&[0x89, b'P', b'N', b'G']), "image/png");
    }
}
