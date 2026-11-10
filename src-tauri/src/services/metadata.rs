//! Launcher metadata from the Lunar API (`metadata:*`, `lunarTerms:*`).
//! Port of `LunarApiClient`, `MetadataService` and `LunarTermsService`.
//!
//! One `GET /v1/metadata` drives three things: the updater channel, the
//! Lunar terms the user must acknowledge, and the feature flags. It runs at
//! startup, on every account switch and on the server-chosen interval.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::state::AppState;
use crate::util::now_iso;

const PROD_API_URL: &str = "https://api.lunarrobloxprod.com";
const DEV_API_URL: &str = "https://api.lunarrobloxdev.com";

/// `globalThis.USE_DEV_SERVICES` (the `use-dev-services` setting).
pub static USE_DEV_SERVICES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const USER_AGENT: &str = concat!("lunar-for-roblox/", env!("CARGO_PKG_VERSION"));
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_UPDATER_CHANNEL: &str = "latest";
const DEFAULT_METADATA_INTERVAL_MS: u64 = 21_600_000;
const CAROUSEL_SLIDES_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const NIL_INSTALLATION_ID: &str = "00000000-0000-0000-0000-000000000000";
const INSTALLATION_ID_FILENAME: &str = "installation-id";
const TERMS_PENDING_CHANGED: &str = "lunarTerms:pendingChanged";

// ------------------------------------------------------------------ API

/// `LUNAR_AXIOS_INSTANCE` (10s timeout, JSON, lunar user agent).
pub struct LunarApi {
    http: reqwest::Client,
}

impl LunarApi {
    pub fn new() -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(10))
            .gzip(true)
            .build()?;
        Ok(Self { http })
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> AppResult<T> {
        let base = if USE_DEV_SERVICES.load(std::sync::atomic::Ordering::Relaxed) { DEV_API_URL } else { PROD_API_URL };
        let url = format!("{base}{path}");
        let result = async {
            let response = self
                .http
                .get(&url)
                .query(query)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                return Err(AppError::Api {
                    status: status.as_u16(),
                    url: url.clone(),
                    message: status.to_string(),
                });
            }
            Ok(response.json::<T>().await?)
        }
        .await;
        if let Err(error) = &result {
            tracing::error!("[LunarApi] GET {path} failed: {error}");
        }
        result
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataResponse {
    updater: UpdaterInfo,
    #[serde(default)]
    terms: Vec<TermsDocument>,
    #[serde(default)]
    feature_flags: Option<Value>,
    #[serde(default)]
    refresh_intervals: RefreshIntervals,
}

#[derive(Deserialize)]
struct UpdaterInfo {
    channel: String,
}

#[derive(Deserialize, Default)]
struct RefreshIntervals {
    #[serde(default)]
    metadata: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TermsDocument {
    id: String,
    #[serde(default)]
    name: Value,
    #[serde(default)]
    url: Value,
    hash: String,
    #[serde(default)]
    last_updated: Value,
}

// ------------------------------------------------------------------ installation id

/// `InstallationIdService.getInstallationId`: read or create the UUID file.
fn installation_id(base: &Path) -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let path = base.join(INSTALLATION_ID_FILENAME);
        if let Some(id) = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        {
            tracing::info!("[InstallationId] Loaded: {id}");
            return id;
        }
        let generated = uuid::Uuid::new_v4().to_string();
        if let Err(error) =
            std::fs::create_dir_all(base).and_then(|()| std::fs::write(&path, &generated))
        {
            tracing::error!("[InstallationId] Failed to persist: {error}");
        }
        tracing::info!("[InstallationId] Generated: {generated}");
        generated
    })
}

/// `getGatedInstallationId`: the real id only for verified 13+ accounts.
pub fn gated_installation_id(state: &AppState) -> String {
    let verified = state
        .auth
        .current_user_id()
        .is_some_and(|id| state.auth.is_user_above13(id));
    if verified {
        installation_id(&state.paths.base).to_owned()
    } else {
        NIL_INSTALLATION_ID.to_owned()
    }
}

// ------------------------------------------------------------------ terms

#[derive(Default)]
struct TermsState {
    items: Vec<Value>,
    reconciled_once: bool,
}

fn pending_ids(items: &[Value]) -> HashSet<String> {
    items
        .iter()
        .filter(|i| i.get("newVersion").is_some())
        .filter_map(|i| i.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn pending_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter(|i| i.get("newVersion").is_some())
        .cloned()
        .collect()
}

fn stored_version(stored: &Value) -> Value {
    json!({ "acceptedAt": stored.get("acceptedAt"), "hash": stored.get("hash"), "updatedAt": stored.get("updatedAt") })
}

fn reconcile(state: &AppState, doc: &TermsDocument) -> Value {
    let stored = state.cache.get(&format!("terms.{}", doc.id));
    match &stored {
        Some(s) if s.get("hash").and_then(Value::as_str) == Some(doc.hash.as_str()) => json!({
            "currentVersion": stored_version(s),
            "id": doc.id,
            "name": doc.name,
            "url": doc.url,
        }),
        _ => {
            let mut item = json!({
                "id": doc.id,
                "name": doc.name,
                "newVersion": { "hash": doc.hash, "updatedAt": doc.last_updated },
                "url": doc.url,
            });
            if let Some(s) = &stored {
                item["currentVersion"] = stored_version(s);
            }
            item
        }
    }
}

// ------------------------------------------------------------------ service

/// `TtlRequestCache` for one value: concurrent callers share the lock (and
/// so the in-flight request); failures are not cached.
struct TtlCache {
    slot: tokio::sync::Mutex<Option<(Instant, Value)>>,
    ttl: Duration,
}

pub struct MetadataService {
    api: LunarApi,
    initialized: AtomicBool,
    latest_request: AtomicU64,
    last_channel: Mutex<Option<String>>,
    interval_ms: AtomicU64,
    interval_changed: Notify,
    carousel: TtlCache,
    terms: Mutex<TermsState>,
}

impl MetadataService {
    pub fn new() -> AppResult<Self> {
        Ok(Self {
            api: LunarApi::new()?,
            initialized: AtomicBool::new(false),
            latest_request: AtomicU64::new(0),
            last_channel: Mutex::new(None),
            interval_ms: AtomicU64::new(DEFAULT_METADATA_INTERVAL_MS),
            interval_changed: Notify::new(),
            carousel: TtlCache {
                slot: tokio::sync::Mutex::new(None),
                ttl: CAROUSEL_SLIDES_TTL,
            },
            terms: Mutex::new(TermsState::default()),
        })
    }

    pub fn api(&self) -> &LunarApi {
        &self.api
    }

    /// `initialize()`: fetch now, on account switches and on the interval.
    pub fn initialize(state: &Arc<AppState>) {
        if state.metadata.initialized.swap(true, Ordering::AcqRel) {
            return;
        }
        tracing::info!("[Metadata] Initializing metadata service");
        let s = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            fetch_metadata(&s, None).await;
        });

        let weak = Arc::downgrade(state);
        let mut accounts = state.auth.subscribe_account_changes();
        tauri::async_runtime::spawn(async move {
            while accounts.changed().await.is_ok() {
                let Some(state) = weak.upgrade() else { break };
                tracing::info!("[Metadata] Active account changed, refetching metadata");
                tauri::async_runtime::spawn(async move {
                    fetch_metadata(&state, None).await;
                });
            }
        });

        // `setInterval`, restarted whenever the server changes the period.
        let state = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            loop {
                let interval =
                    Duration::from_millis(state.metadata.interval_ms.load(Ordering::Acquire));
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        tracing::info!("[Metadata] Fetching metadata (interval refresh)");
                        let state = Arc::clone(&state);
                        tauri::async_runtime::spawn(async move {
                            fetch_metadata(&state, None).await;
                        });
                    }
                    _ = state.metadata.interval_changed.notified() => {}
                }
            }
        });
    }

    fn reschedule_if_needed(&self, server_ms: u64) {
        let current = self.interval_ms.load(Ordering::Acquire);
        if server_ms > 0 && server_ms != current {
            tracing::info!(
                "[Metadata] Server changed refresh interval: {current}ms → {server_ms}ms"
            );
            self.interval_ms.store(server_ms, Ordering::Release);
            self.interval_changed.notify_waiters();
        }
    }

    pub fn pending_terms(&self) -> Vec<Value> {
        pending_items(&self.terms.lock().items)
    }
}

/// `fetchMetadata`: returns the resolved channel, or `None` when the request
/// failed or was superseded.
pub async fn fetch_metadata(state: &AppState, switch_to: Option<&str>) -> Option<String> {
    let metadata = &state.metadata;
    let request_id = metadata.latest_request.fetch_add(1, Ordering::AcqRel) + 1;
    let installation_id = gated_installation_id(state);
    let stream = switch_to
        .filter(|s| !s.is_empty())
        .unwrap_or(BUILD_UPDATER_CHANNEL);
    let roblox_id = state.auth.current_user_id();
    let player_id = roblox_id.map(|id| id.to_string());

    let mut query = vec![
        ("installation_id", installation_id.as_str()),
        ("version", APP_VERSION),
        ("launcher_update_stream", stream),
    ];
    if let Some(player_id) = &player_id {
        query.push(("player_id", player_id.as_str()));
    }
    let response: MetadataResponse = match metadata.api.get("/v1/metadata", &query).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!("[Metadata] Failed to fetch metadata: {error}");
            return None;
        }
    };
    if installation_id != gated_installation_id(state) || roblox_id != state.auth.current_user_id()
    {
        tracing::info!("[Metadata] Discarding metadata for a superseded account");
        return None;
    }
    if switch_to.is_none() && request_id != metadata.latest_request.load(Ordering::Acquire) {
        tracing::info!("[Metadata] Discarding metadata from a superseded request");
        return None;
    }

    process_updater(state, &response.updater.channel);
    process_incoming_terms(state, &response.terms);
    state.feature_flags.process_incoming(
        response.feature_flags.as_ref(),
        &state.cache,
        &state.events,
    );
    metadata.reschedule_if_needed(response.refresh_intervals.metadata);
    Some(response.updater.channel)
}

fn process_updater(state: &AppState, channel: &str) {
    let mut last = state.metadata.last_channel.lock();
    if last.as_deref() == Some(channel) {
        tracing::debug!("[Metadata] Updater channel unchanged, skipping");
        return;
    }
    if let Some(previous) = last.as_deref() {
        tracing::info!("[Metadata] Updater channel changed: {previous} → {channel}");
    }
    *last = Some(channel.to_owned());
    drop(last);
    state.updater.set_server_channel(state);
}

/// `processIncomingTerms`: the first reconcile on a fresh install (no
/// `terms` key in the cache at startup) silently accepts everything.
fn process_incoming_terms(state: &AppState, documents: &[TermsDocument]) {
    let mut terms = state.metadata.terms.lock();
    let previous = pending_ids(&terms.items);
    terms.items = documents.iter().map(|doc| reconcile(state, doc)).collect();
    if !terms.reconciled_once {
        terms.reconciled_once = true;
        let fresh_install = !state
            .cache
            .top_level_keys_at_startup()
            .iter()
            .any(|k| k == "terms");
        let pending = pending_items(&terms.items);
        if fresh_install && !pending.is_empty() {
            tracing::info!(
                "[LunarTerms] Fresh install detected, silently accepting {} term(s)",
                pending.len()
            );
            accept_pending(state, &mut terms, "automatic", &pending);
            return;
        }
    }
    if previous != pending_ids(&terms.items) {
        state
            .events
            .emit_main(TERMS_PENDING_CHANGED, pending_items(&terms.items));
    }
}

/// `acceptPending`: persist, track, promote `newVersion`, re-emit.
fn accept_pending(state: &AppState, terms: &mut TermsState, initiator: &str, pending: &[Value]) {
    if pending.is_empty() {
        return;
    }
    let accepted_at = now_iso();
    let mut accepted = HashSet::new();
    for item in pending {
        let (Some(id), Some(next)) = (
            item.get("id").and_then(Value::as_str),
            item.get("newVersion"),
        ) else {
            continue;
        };
        let record = json!({ "acceptedAt": accepted_at, "hash": next.get("hash"), "updatedAt": next.get("updatedAt") });
        if let Err(error) = state.cache.set(&format!("terms.{id}"), record) {
            tracing::warn!(%error, "[LunarTerms] failed to persist acceptance");
        }
        let previous = item
            .get("currentVersion")
            .filter(|p| p.get("acceptedAt").is_some_and(|a| !a.is_null()));
        let mut payload = json!({
            "id": id,
            "initiator": initiator,
            "new_version": { "accepted_at": accepted_at, "hash": next.get("hash"), "updated_at": next.get("updatedAt") },
        });
        if let Some(previous) = previous {
            payload["previous_version"] = json!({
                "accepted_at": previous.get("acceptedAt"),
                "hash": previous.get("hash"),
                "updated_at": previous.get("updatedAt"),
            });
        }
        state.track("terms.accept", payload);
        accepted.insert(id.to_owned());
    }
    for item in &mut terms.items {
        let is_accepted = item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| accepted.contains(id));
        if !is_accepted {
            continue;
        }
        if let Some(next) = item.as_object_mut().and_then(|m| m.remove("newVersion")) {
            item["currentVersion"] = json!({ "acceptedAt": accepted_at, "hash": next.get("hash"), "updatedAt": next.get("updatedAt") });
        }
    }
    state
        .events
        .emit_main(TERMS_PENDING_CHANGED, pending_items(&terms.items));
}

async fn carousel_slides(state: &AppState, force_refresh: bool) -> AppResult<Value> {
    let cache = &state.metadata.carousel;
    let mut slot = cache.slot.lock().await;
    if !force_refresh {
        if let Some((at, value)) = slot.as_ref() {
            if at.elapsed() < cache.ttl {
                return Ok(value.clone());
            }
        }
    }
    let installation_id = gated_installation_id(state);
    let query = [
        ("installation_id", installation_id.as_str()),
        ("version", APP_VERSION),
    ];
    let result: AppResult<Value> = state
        .metadata
        .api
        .get("/v1/metadata/carousel", &query)
        .await;
    match result {
        Ok(mut response) => {
            let slides = response
                .get_mut("slides")
                .map(Value::take)
                .unwrap_or(Value::Null);
            *slot = Some((Instant::now(), slides.clone()));
            Ok(slides)
        }
        Err(error) => {
            tracing::warn!("[Metadata] Failed to fetch carousel slides: {error}");
            Err(error)
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("metadata:getCarouselSlides", |ctx, args| async move {
            let force: Option<bool> = args.opt(0)?;
            carousel_slides(&ctx.state, force.unwrap_or(false)).await
        })
        .handle("lunarTerms:getPending", |ctx, _| async move {
            Ok(ctx.state.metadata.pending_terms())
        })
        .handle("lunarTerms:accept", |ctx, _| async move {
            let state = &ctx.state;
            let mut terms = state.metadata.terms.lock();
            let pending = pending_items(&terms.items);
            accept_pending(state, &mut terms, "modal", &pending);
            Ok(Value::Null)
        })
        .handle("updater:switchStream", |ctx, args| async move {
            let stream: String = args.get(0)?;
            tracing::info!("[Metadata] Switching stream to '{stream}'");
            Ok(fetch_metadata(&ctx.state, Some(&stream))
                .await
                .unwrap_or(stream))
        });
}

/// `InstallationIdService.getInstallationId` for `app:getInstallationId`.
pub fn installation_id_value(base: &Path) -> String {
    installation_id(base).to_owned()
}
