//! Roblox terms the active account still has to accept (`userAgreements:*`).
//! Port of `UserAgreementsService`.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::state::AppState;

const PENDING_CHANGED: &str = "userAgreements:pendingChanged";
const SUPPORTED_TYPES: [&str; 4] = ["TermsOfService", "PrivacyPolicy", "CookiePolicy", "EULA"];

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAgreement {
    id: String,
    display_url: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAgreement {
    id: String,
    #[serde(default)]
    display_url: String,
    #[serde(default)]
    agreement_type: Option<String>,
}

#[derive(Default)]
struct Cached {
    user_id: Option<u64>,
    pending: Vec<PendingAgreement>,
}

#[derive(Default)]
pub struct UserAgreementsService {
    cached: Mutex<Cached>,
    /// Serializes refreshes: concurrent callers wait and share the result.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl UserAgreementsService {
    /// `initialize()`: refresh on every account change, clear on logout.
    pub fn initialize(state: &Arc<AppState>) {
        let weak = Arc::downgrade(state);
        let mut accounts = state.auth.subscribe_account_changes();
        tauri::async_runtime::spawn(async move {
            while accounts.changed().await.is_ok() {
                let user_id = *accounts.borrow_and_update();
                let Some(state) = weak.upgrade() else { break };
                match user_id {
                    None => state.agreements.clear(&state),
                    Some(_) => {
                        tauri::async_runtime::spawn(async move {
                            refresh(&state).await;
                        });
                    }
                }
            }
        });
    }

    fn cached_pending(&self, active: Option<u64>) -> Vec<PendingAgreement> {
        let cached = self.cached.lock();
        match active {
            Some(id) if cached.user_id == Some(id) => cached.pending.clone(),
            _ => Vec::new(),
        }
    }

    fn clear(&self, state: &AppState) {
        let had_pending = {
            let mut cached = self.cached.lock();
            let had = !cached.pending.is_empty();
            *cached = Cached::default();
            had
        };
        if had_pending {
            state.events.emit_main(PENDING_CHANGED, Vec::<PendingAgreement>::new());
        }
    }

    /// Stores a fresh snapshot; emits when the id set differs.
    fn commit(&self, state: &AppState, user_id: u64, pending: Vec<PendingAgreement>) {
        let changed = {
            let mut cached = self.cached.lock();
            let same = cached.user_id == Some(user_id) && cached.pending.len() == pending.len() && {
                let ids: HashSet<&str> = cached.pending.iter().map(|a| a.id.as_str()).collect();
                pending.iter().all(|a| ids.contains(a.id.as_str()))
            };
            cached.user_id = Some(user_id);
            cached.pending = pending.clone();
            !same
        };
        if changed {
            state.events.emit_main(PENDING_CHANGED, pending);
        }
    }
}

fn normalize(raw: Vec<RawAgreement>) -> Vec<PendingAgreement> {
    raw.into_iter()
        .filter_map(|entry| {
            let kind = entry.agreement_type.filter(|t| SUPPORTED_TYPES.contains(&t.as_str()))?;
            Some(PendingAgreement { id: entry.id, display_url: entry.display_url, kind })
        })
        .collect()
}

async fn web_request<T: serde::de::DeserializeOwned>(state: &AppState, method: Method, url: &str, body: Option<&Value>) -> AppResult<T> {
    let context = state.auth.web_context().await;
    let context = context.as_ref().map(|(cookie, tracker)| (cookie.as_str(), tracker.as_str()));
    state.roblox.request_web(method, url, body, context).await
}

pub async fn refresh(state: &AppState) -> Vec<PendingAgreement> {
    let Some(user_id) = state.auth.current_user_id() else {
        state.agreements.clear(state);
        return Vec::new();
    };
    let _guard = state.agreements.refresh_lock.lock().await;
    let url = format!("{}/agreements-resolution/web", endpoints::USER_AGREEMENTS_V1);
    match web_request::<Vec<RawAgreement>>(state, Method::GET, &url, None).await {
        Ok(raw) => {
            if state.auth.current_user_id() != Some(user_id) {
                return state.agreements.cached_pending(state.auth.current_user_id());
            }
            let pending = normalize(raw);
            state.agreements.commit(state, user_id, pending.clone());
            pending
        }
        Err(error) => {
            tracing::error!("UserAgreements: Failed to refresh for {user_id}: {error}");
            let stale = state.agreements.cached.lock().user_id != Some(user_id);
            if stale {
                state.agreements.commit(state, user_id, Vec::new());
            }
            state.agreements.cached.lock().pending.clone()
        }
    }
}

async fn get_pending(state: &AppState) -> Vec<PendingAgreement> {
    let Some(user_id) = state.auth.current_user_id() else { return Vec::new() };
    {
        let cached = state.agreements.cached.lock();
        if cached.user_id == Some(user_id) {
            return cached.pending.clone();
        }
    }
    refresh(state).await
}

async fn accept(state: &AppState, ids: Vec<String>) -> AppResult<Value> {
    if ids.is_empty() {
        return Ok(json!([]));
    }
    if state.auth.current_user_id().is_none() {
        return Err(AppError::Message("Cannot accept agreements without an active account".into()));
    }
    let url = format!("{}/acceptances", endpoints::USER_AGREEMENTS_V1);
    let body = json!({ "acceptances": ids.iter().map(|id| json!({ "agreementId": id })).collect::<Vec<_>>() });
    let response: Value = web_request(state, Method::POST, &url, Some(&body)).await?;
    let results: Vec<Value> = response
        .get("results")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|r| json!({ "agreementId": r.get("agreementId"), "errorCode": r.get("errorCode") }))
                .collect()
        })
        .unwrap_or_default();
    refresh(state).await;
    Ok(Value::Array(results))
}

pub fn register(router: &mut Router) {
    router
        .handle("userAgreements:getPending", |ctx, _| async move { Ok(get_pending(&ctx.state).await) })
        .handle("userAgreements:refresh", |ctx, _| async move { Ok(refresh(&ctx.state).await) })
        .handle("userAgreements:accept", |ctx, args| async move {
            let ids: Vec<String> = args.get(0)?;
            accept(&ctx.state, ids).await
        });
}
