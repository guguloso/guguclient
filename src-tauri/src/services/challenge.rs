//! Roblox generic challenges (`challenge:*`): captcha, two-step verification
//! and proof-of-work raised by any Roblox API call. Port of
//! `ChallengeService`, `ChallengeController`, `ProofOfWorkService` and
//! `CaptchaService`. Proof-of-work is solved locally; captchas on mutating
//! requests are replayed from a roblox.com popup; everything else is shown
//! by the renderer (`challenge:requested`) and answered through
//! `challenge:solve` / `challenge:dismiss`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tauri::webview::{Cookie, PageLoadEvent};
use tauri::{Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tokio::sync::{mpsc, oneshot};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::{Challenge, ChallengeOutcome, ReplayRequest};
use crate::services::app as app_service;
use crate::state::AppState;
use crate::util::now_millis;

const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MUTATING_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];
const CHALLENGE_CONTINUE_URL: &str = "https://apis.roblox.com/challenge/v1/continue";
const POW_PUZZLE_URL: &str = "https://apis.roblox.com/proof-of-work-service/v1/pow-puzzle";
const CAPTCHA_WINDOW: &str = "roblox-captcha";
const CAPTCHA_DONE_PREFIX: &str = "https://www.roblox.com/lunar-captcha-done";
const REPLAY_DONE_PREFIX: &str = "https://www.roblox.com/lunar-replay-done";
const ARKOSE_SCRIPT_URL: &str = "https://arkoselabs.roblox.com/v2/A2A14B1D-1AF3-C791-9BBC-EE33CC7A0A6F/api.js";
const ARKOSE_SCRIPT_URL_TEMPLATE: &str = "https://arkoselabs.roblox.com/v2/{publicKey}/api.js";
const CAPTCHA_METADATA_URL: &str = "https://apis.rbxcdn.com/captcha/v1/metadata";
const FALLBACK_FUNCAPTCHA_ACTION_TYPE: &str = "ACTION_TYPE_GENERIC_CHALLENGE";
const CAPTCHA_WARMUP_DELAY: Duration = Duration::from_millis(1500);
const REPLAY_BLOCKED_HEADERS: [&str; 10] =
    ["accept-encoding", "accept-language", "cookie", "connection", "content-length", "host", "origin", "referer", "user-agent", "x-bound-auth-token"];
const CAPTCHA_INJECT_JS: &str = include_str!("challenge_scripts/captcha-inject.js");
const CAPTCHA_REPLAY_JS: &str = include_str!("challenge_scripts/captcha-replay.js");
const TWO_STEP_ACTION_TYPES: [&str; 9] = ["Unknown", "Login", "RobuxSpend", "ItemTrade", "Resale", "PasswordReset", "RevertAccount", "Generic", "GenericWithRecoveryCodes"];

struct Pending {
    challenge: Challenge,
    action_type: Value,
    shown_at: u64,
    reply: oneshot::Sender<Option<Challenge>>,
}

#[derive(Default)]
pub struct ChallengeState {
    pending: Mutex<HashMap<String, Pending>>,
    funcaptcha_keys: Mutex<Option<Value>>,
}

fn b64_decode(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.decode(value.trim()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn b64_encode(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
}

/// `normalizeActionType`.
fn normalize_action_type(value: Option<&Value>) -> &'static str {
    match value {
        Some(Value::Number(n)) => n.as_u64().and_then(|i| TWO_STEP_ACTION_TYPES.get(i as usize).copied()).unwrap_or("Generic"),
        Some(Value::String(s)) => {
            let s = s.trim();
            if let Some(found) = TWO_STEP_ACTION_TYPES.iter().find(|t| **t == s) {
                return found;
            }
            s.parse::<usize>().ok().and_then(|i| TWO_STEP_ACTION_TYPES.get(i).copied()).unwrap_or("Generic")
        }
        _ => "Generic",
    }
}

/// `extractActionType`.
fn extract_action_type(c: &Challenge) -> Value {
    if c.metadata.is_empty() {
        return Value::Null;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&b64_decode(&c.metadata)) else { return Value::Null };
    match parsed.get("actionType") {
        None | Some(Value::Null) => Value::Null,
        Some(v) if c.kind == "captcha" => json!(v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())),
        Some(v) => json!(normalize_action_type(Some(v))),
    }
}

fn track_failed(state: &AppState, c: &Challenge, action_type: &Value, path: &str, shown_at: u64, reason: &str, status: Option<u16>) {
    state.track(
        "challenge.failed",
        json!({ "action_type": action_type, "challenge_id": c.id, "challenge_type": c.kind, "duration_ms": now_millis().saturating_sub(shown_at), "path": path, "reason": reason, "response_status": status }),
    );
}

fn track_popup_failed(state: &AppState, challenge_id: &str, path: &str, stage: &str, description: Option<String>) {
    state.track(
        "challenge.popup_failed",
        json!({ "challenge_id": challenge_id, "error_code": null, "error_description": description, "path": path, "stage": stage }),
    );
}

/// Installs the handler on the Roblox client (`setHandleGenericChallenge`).
pub fn install(state: &Arc<AppState>) {
    let weak = Arc::downgrade(state);
    state.roblox.set_challenge_handler(Arc::new(move |challenge, replay| {
        let weak = weak.clone();
        Box::pin(async move {
            let state = weak.upgrade()?;
            handle_challenge(&state, challenge, replay).await
        })
    }));
}

/// `handleChallenge`.
async fn handle_challenge(state: &Arc<AppState>, challenge: Challenge, replay: ReplayRequest) -> Option<ChallengeOutcome> {
    tracing::info!("Challenge received: type={}, id={}", challenge.kind, challenge.id);
    if challenge.kind == "proofofwork" {
        return solve_proof_of_work(state, &challenge).await.map(ChallengeOutcome::Retry);
    }
    let Some(main) = state.app.get_webview_window(crate::MAIN_WINDOW) else {
        tracing::warn!("Cannot show challenge — main window is not available");
        return None;
    };
    if main.is_minimized().unwrap_or(false) {
        let _ = main.unminimize();
    }
    let _ = main.set_focus();
    let action_type = extract_action_type(&challenge);
    let replay_supported = challenge.kind == "captcha" && replay.replay_supported;
    if replay_supported && MUTATING_METHODS.contains(&replay.method.to_uppercase().as_str()) {
        tracing::info!("Handling captcha challenge via roblox.com replay popup: id={}", challenge.id);
        let shown_at = now_millis();
        state.track(
            "challenge.shown",
            json!({ "action_type": action_type, "challenge_id": challenge.id, "challenge_type": challenge.kind, "path": "replay_popup", "replay_supported": true, "request_method": replay.method.to_uppercase() }),
        );
        let response = open_replay_window(state, &replay, &challenge).await;
        let status = response["status"].as_u64().unwrap_or(500) as u16;
        if (200..300).contains(&status) {
            state.track(
                "challenge.solved",
                json!({ "action_type": action_type, "challenge_id": challenge.id, "challenge_type": challenge.kind, "duration_ms": now_millis() - shown_at, "path": "replay_popup" }),
            );
        } else {
            track_failed(state, &challenge, &action_type, "replay_popup", shown_at, "replay_non_2xx", Some(status));
        }
        return Some(ChallengeOutcome::Respond(to_response(&response)));
    }
    let (tx, rx) = oneshot::channel();
    let shown_at = now_millis();
    state.challenge.pending.lock().insert(challenge.id.clone(), Pending { challenge: challenge.clone(), action_type: action_type.clone(), shown_at, reply: tx });
    let timeout_state = Arc::downgrade(state);
    let id = challenge.id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(CHALLENGE_TIMEOUT).await;
        let Some(state) = timeout_state.upgrade() else { return };
        let Some(entry) = state.challenge.pending.lock().remove(&id) else { return };
        tracing::warn!("Challenge timed out: id={id}");
        track_failed(&state, &entry.challenge, &entry.action_type, "modal", entry.shown_at, "timed_out", None);
        let _ = entry.reply.send(None);
    });
    state.track(
        "challenge.shown",
        json!({ "action_type": action_type, "challenge_id": challenge.id, "challenge_type": challenge.kind, "path": "modal", "replay_supported": replay_supported, "request_method": null }),
    );
    state.events.emit_main(
        "challenge:requested",
        json!({ "challengeId": challenge.id, "challengeType": challenge.kind, "challengeBase64Metadata": challenge.metadata }),
    );
    rx.await.ok().flatten().map(ChallengeOutcome::Retry)
}

/// `createPopupReplayResponse`.
fn to_response(serialized: &Value) -> reqwest::Response {
    let status = serialized["status"].as_u64().and_then(|s| u16::try_from(s).ok()).unwrap_or(500);
    let mut builder = http::Response::builder().status(status);
    for pair in serialized["headers"].as_array().into_iter().flatten() {
        if let (Some(k), Some(v)) = (pair.get(0).and_then(Value::as_str), pair.get(1).and_then(Value::as_str)) {
            builder = builder.header(k, v);
        }
    }
    let body = serialized["bodyText"].as_str().unwrap_or("").to_owned();
    match builder.body(body) {
        Ok(r) => reqwest::Response::from(r),
        Err(_) => reqwest::Response::from(http::Response::new(String::new())),
    }
}

fn failure_response(status: u16, status_text: &str, body: &str, url: &str) -> Value {
    json!({ "status": status, "statusText": status_text, "url": url, "headers": [["content-type", "text/plain; charset=utf-8"]], "bodyText": body })
}

// ------------------------------------------------------------ proof of work

/// `solveProofOfWorkPuzzle`: `A^(2^T) mod N`.
fn solve_puzzle(a: &str, n: &str, t: u64) -> Option<String> {
    use num_bigint::BigUint;
    let modulus: BigUint = n.parse().ok()?;
    let mut value: BigUint = a.parse::<BigUint>().ok()? % &modulus;
    for _ in 0..t {
        value = (&value * &value) % &modulus;
    }
    Some(value.to_string())
}

fn decimal(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => Some(s.clone()),
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        _ => None,
    }
}

/// `ProofOfWorkService.solve`.
async fn solve_proof_of_work(state: &AppState, c: &Challenge) -> Option<Challenge> {
    let result = async {
        let meta: Value = serde_json::from_str(&b64_decode(&c.metadata)).map_err(|e| AppError::msg(e.to_string()))?;
        let session = meta["sessionId"].as_str().unwrap_or("").to_owned();
        let puzzle: Value = state.roblox.get(&format!("{POW_PUZZLE_URL}?sessionID={}", urlencoding::encode(&session))).await?;
        let artifacts: Value = serde_json::from_str(puzzle["artifacts"].as_str().unwrap_or("{}")).map_err(|e| AppError::msg(e.to_string()))?;
        let (a, n) = (decimal(artifacts.get("A")).ok_or_else(|| AppError::msg("bad A"))?, decimal(artifacts.get("N")).ok_or_else(|| AppError::msg("bad N"))?);
        let t: u64 = decimal(artifacts.get("T")).and_then(|t| t.parse().ok()).ok_or_else(|| AppError::msg("bad T"))?;
        let solution = tauri::async_runtime::spawn_blocking(move || solve_puzzle(&a, &n, t)).await.ok().flatten().ok_or_else(|| AppError::msg("puzzle"))?;
        let answer: Value = state.roblox.post(POW_PUZZLE_URL, &json!({ "sessionID": session, "solution": solution })).await?;
        let token = answer["redemptionToken"].as_str().filter(|_| answer["answerCorrect"] == true).map(str::to_owned);
        let Some(token) = token else {
            tracing::warn!("Proof-of-work puzzle rejected for challenge {}", c.id);
            return Ok(None);
        };
        let redemption = json!({ "redemptionToken": token, "sessionId": session }).to_string();
        let _: Value = state.roblox.post(CHALLENGE_CONTINUE_URL, &json!({ "challengeId": c.id, "challengeType": c.kind, "challengeMetadata": redemption })).await?;
        tracing::info!("Solved proof-of-work challenge {}", c.id);
        Ok::<_, AppError>(Some(Challenge { id: c.id.clone(), kind: c.kind.clone(), metadata: b64_encode(&redemption) }))
    }
    .await;
    result.unwrap_or_else(|error| {
        tracing::warn!("Failed to solve proof-of-work challenge {}: {error}", c.id);
        None
    })
}

// ----------------------------------------------------------- controller

/// `resolveTwoStepChallengeMetadata`.
async fn two_step_metadata(state: &AppState, id: &str, kind: &str, metadata: &str) -> AppResult<String> {
    if kind != "twostepverification" && kind != "forcetwostepverification" {
        return Ok(metadata.to_owned());
    }
    let pending = state.challenge.pending.lock().get(id).map(|p| p.challenge.clone());
    let pending = pending.ok_or_else(|| AppError::msg(format!("No pending challenge metadata available for {id}")))?;
    let decoded: Value = serde_json::from_str(&b64_decode(&pending.metadata)).unwrap_or_else(|_| json!({}));
    let user = state.auth.current_user_id().ok_or_else(|| AppError::msg("Cannot solve 2FA challenge without an active user"))?;
    let actual_id = decoded.get("challengeId").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(id).to_owned();
    let action_type = normalize_action_type(decoded.get("actionType"));
    let remember = decoded.get("rememberDevice").cloned().filter(|v| !v.is_null()).unwrap_or(json!(false));
    let raw_method = ["mediaType", "verificationMethod", "challengeType"].iter().find_map(|k| decoded.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()));
    let method = match raw_method.map(str::to_lowercase).as_deref() {
        Some("email") => "email",
        Some("password") => "password",
        Some("recoverycode" | "recovery-code" | "recovery_codes" | "recoverycodes") => "recovery-codes",
        Some("sms") => "sms",
        _ => "authenticator",
    };
    let url = format!("https://twostepverification.roblox.com/v1/users/{user}/challenges/{method}/verify");
    let code = b64_decode(metadata).trim().to_owned();
    tracing::info!("Verifying 2FA challenge: type={kind}, id={id}, method={method}, actionType={action_type}");
    let verified: Value = state.roblox.post(&url, &json!({ "challengeId": actual_id, "actionType": action_type, "code": code })).await?;
    Ok(b64_encode(&json!({ "actionType": action_type, "challengeId": actual_id, "rememberDevice": remember, "verificationToken": verified.get("verificationToken") }).to_string()))
}

fn resolve(state: &AppState, id: &str, kind: &str, metadata: String) {
    let Some(entry) = state.challenge.pending.lock().remove(id) else {
        tracing::warn!("No pending challenge found for id={id}");
        return;
    };
    tracing::info!("Challenge solved: id={id}");
    state.track(
        "challenge.solved",
        json!({ "action_type": entry.action_type, "challenge_id": id, "challenge_type": entry.challenge.kind, "duration_ms": now_millis() - entry.shown_at, "path": "modal" }),
    );
    let _ = entry.reply.send(Some(Challenge { id: id.to_owned(), kind: kind.to_owned(), metadata }));
}

/// `dismissChallenge`.
fn dismiss(state: &AppState, id: &str, reason: &str) {
    let Some(entry) = state.challenge.pending.lock().remove(id) else { return };
    tracing::info!("Challenge dismissed: id={id}, reason={reason}");
    track_failed(state, &entry.challenge, &entry.action_type, "modal", entry.shown_at, reason, None);
    let _ = entry.reply.send(None);
}

async fn solve(state: &AppState, id: &str, kind: &str, metadata: &str) -> bool {
    let result = async {
        let resolved = if kind == "captcha" { metadata.to_owned() } else { two_step_metadata(state, id, kind, metadata).await? };
        let raw = b64_decode(&resolved);
        let _: Value = state.roblox.post(CHALLENGE_CONTINUE_URL, &json!({ "challengeId": id, "challengeType": kind, "challengeMetadata": raw })).await?;
        Ok::<_, AppError>(resolved)
    }
    .await;
    match result {
        Ok(resolved) => {
            resolve(state, id, kind, resolved);
            true
        }
        Err(error) => {
            tracing::warn!("Challenge solve failed: {error}");
            dismiss(state, id, "solve_rejected");
            false
        }
    }
}

// ------------------------------------------------------------- popups

enum PopupEvent {
    Done(String),
    Closed,
}

fn close_captcha_window(state: &AppState) {
    if let Some(window) = state.app.get_webview_window(CAPTCHA_WINDOW) {
        let _ = window.destroy();
    }
}

/// `openExternalFromCaptcha`.
fn open_external_from_captcha(state: &AppState, url: &str) {
    if !app_service::is_external_open_allowed(url) {
        tracing::warn!("[Captcha] Blocked external open with disallowed protocol: {url}");
        return;
    }
    if let Some(domain) = app_service::domain_requiring_confirmation(state, url) {
        tracing::warn!("[Captcha] Blocked external open of unconfirmed domain {domain} from captcha popup");
        return;
    }
    app_service::perform_external_open(state, url, None);
}

/// A roblox.com popup in a throwaway profile seeded with the session cookie.
async fn open_popup(
    state: &Arc<AppState>,
    title: &str,
    size: (f64, f64),
    done_prefix: &'static str,
    block_after_load: Arc<AtomicBool>,
) -> AppResult<(WebviewWindow, mpsc::UnboundedReceiver<PopupEvent>, mpsc::UnboundedReceiver<()>)> {
    close_captcha_window(state);
    let (tx, rx) = mpsc::unbounded_channel();
    let (loaded_tx, loaded_rx) = mpsc::unbounded_channel();
    let profile = std::env::temp_dir().join(format!("gugu-captcha-{}", uuid::Uuid::new_v4()));
    let nav_tx = tx.clone();
    let external_state = Arc::clone(state);
    let mut builder = WebviewWindowBuilder::new(&state.app, CAPTCHA_WINDOW, WebviewUrl::External(Url::parse("about:blank").map_err(|e| AppError::msg(e.to_string()))?))
        .title(title)
        .inner_size(size.0, size.1)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .visible(false)
        .data_directory(profile.clone())
        .on_navigation(move |url| {
            let url = url.to_string();
            if url.starts_with(done_prefix) {
                let _ = nav_tx.send(PopupEvent::Done(url));
                return false;
            }
            !block_after_load.load(Ordering::SeqCst)
        })
        .on_new_window(move |url, _| {
            open_external_from_captcha(&external_state, url.as_str());
            tauri::webview::NewWindowResponse::Deny
        })
        .on_page_load(move |_, payload| {
            if payload.event() == PageLoadEvent::Finished && payload.url().as_str().starts_with("https://www.roblox.com") {
                let _ = loaded_tx.send(());
            }
        });
    if let Some(main) = state.app.get_webview_window(crate::MAIN_WINDOW) {
        builder = builder.parent(&main)?;
    }
    let window = builder.build()?;
    window.on_window_event(move |event| {
        if let WindowEvent::Destroyed = event {
            let _ = tx.send(PopupEvent::Closed);
            let profile = profile.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                let _ = std::fs::remove_dir_all(profile);
            });
        }
    });
    if let Some(cookie) = state.roblox.session_cookie() {
        let seeded = Cookie::build((".ROBLOSECURITY", cookie)).domain(".roblox.com").path("/").secure(true).http_only(true).build();
        if let Err(error) = window.set_cookie(seeded) {
            tracing::warn!("[Captcha] Could not seed the popup session: {error}");
        }
    }
    window.navigate(Url::parse("https://www.roblox.com/").map_err(|e| AppError::msg(e.to_string()))?)?;
    Ok((window, rx, loaded_rx))
}

/// `warmCaptchaSession`.
async fn warm(window: &WebviewWindow) {
    tokio::time::sleep(CAPTCHA_WARMUP_DELAY).await;
    let count = Url::parse("https://www.roblox.com").ok().and_then(|u| window.cookies_for_url(u).ok()).map_or(0, |c| c.len());
    tracing::info!("[Captcha] Warmed Roblox session with {count} cookie(s)");
}

/// `openCaptchaWindow`: the Arkose token, or `None` when abandoned.
async fn open_captcha_window(state: &Arc<AppState>, blob: &str, challenge_id: &str) -> Option<String> {
    let injected = Arc::new(AtomicBool::new(false));
    let (window, mut events, mut loaded) = match open_popup(state, "Complete Roblox Captcha", (500.0, 600.0), CAPTCHA_DONE_PREFIX, Arc::clone(&injected)).await {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!("Failed during Roblox captcha warmup/injection: {error}");
            track_popup_failed(state, challenge_id, "modal", "warmup_inject", Some(error.to_string()));
            return None;
        }
    };
    let finish = |window: &WebviewWindow, token: Option<String>| {
        let _ = window.destroy();
        token
    };
    tokio::select! {
        _ = loaded.recv() => {}
        event = events.recv() => {
            return match event {
                Some(PopupEvent::Done(url)) => finish(&window, token_from(&url)),
                _ => finish(&window, None),
            };
        }
    }
    tracing::info!("[Captcha] Warmup page loaded: {}", window.url().map(|u| u.to_string()).unwrap_or_default());
    let _ = window.show();
    let _ = window.set_focus();
    warm(&window).await;
    injected.store(true, Ordering::SeqCst);
    let config = json!({ "dataExchangeBlob": blob, "arkoseScriptUrl": ARKOSE_SCRIPT_URL, "donePrefix": CAPTCHA_DONE_PREFIX });
    if let Err(error) = window.eval(&format!("globalThis.__lbxCaptchaInject = {config};\n{CAPTCHA_INJECT_JS}")) {
        tracing::warn!("Failed during Roblox captcha warmup/injection: {error}");
        track_popup_failed(state, challenge_id, "modal", "warmup_inject", Some(error.to_string()));
        return finish(&window, None);
    }
    match events.recv().await {
        Some(PopupEvent::Done(url)) => finish(&window, token_from(&url)),
        _ => finish(&window, None),
    }
}

fn token_from(url: &str) -> Option<String> {
    Url::parse(url).ok()?.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned())
}

/// `getFunCaptchaPublicKeys`.
async fn funcaptcha_keys(state: &AppState) -> Value {
    let fallback = json!({
        "ACTION_TYPE_ASSET_COMMENT": "63E4117F-E727-42B4-6DAA-C8448E9B137F", "ACTION_TYPE_CLOTHING_ASSET_UPLOAD": "63E4117F-E727-42B4-6DAA-C8448E9B137F",
        "ACTION_TYPE_FOLLOW_USER": "63E4117F-E727-42B4-6DAA-C8448E9B137F", "ACTION_TYPE_GENERIC_CHALLENGE": "CC30DB96-0C88-4DEB-86E5-6601927ACBB4",
        "ACTION_TYPE_GROUP_JOIN": "63E4117F-E727-42B4-6DAA-C8448E9B137F", "ACTION_TYPE_GROUP_WALL_POST": "63E4117F-E727-42B4-6DAA-C8448E9B137F",
        "ACTION_TYPE_MARKETPLACE": "63E4117F-E727-42B4-6DAA-C8448E9B137F", "ACTION_TYPE_PASSIVE": "EA7F44A3-D0DF-460D-A66F-1775B978E5E1",
        "ACTION_TYPE_SUPPORT_REQUEST": "63E4117F-E727-42B4-6DAA-C8448E9B137F", "ACTION_TYPE_WEB_GAMECARD_REDEMPTION": "1B154715-ACB4-2706-19ED-0DC7E3F7D855",
        "ACTION_TYPE_WEB_LOGIN": "476068BF-9607-4799-B53D-966BE98E2B81", "ACTION_TYPE_WEB_RESET_PASSWORD": "476068BF-9607-4799-B53D-966BE98E2B81",
        "ACTION_TYPE_WEB_SIGNUP": "A2A14B1D-1AF3-C791-9BBC-EE33CC7A0A6F",
    });
    if let Some(cached) = state.challenge.funcaptcha_keys.lock().clone() {
        return cached;
    }
    let fetched = state.roblox.http().get(CAPTCHA_METADATA_URL).send().await.ok();
    let metadata: Option<Value> = match fetched {
        Some(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    };
    match metadata.as_ref().and_then(|m| m.get("funCaptchaPublicKeys")).and_then(Value::as_object) {
        Some(keys) => {
            let mut merged = fallback.as_object().cloned().unwrap_or_default();
            merged.extend(keys.clone());
            let merged = Value::Object(merged);
            *state.challenge.funcaptcha_keys.lock() = Some(merged.clone());
            merged
        }
        None => {
            tracing::warn!("Failed to fetch captcha public key metadata; using snapshot");
            fallback
        }
    }
}

/// `sanitizePopupReplayRequest` + the rozod request context shape.
fn replay_request_json(replay: &ReplayRequest) -> Value {
    let headers: Map<String, Value> = replay
        .headers
        .iter()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !REPLAY_BLOCKED_HEADERS.contains(&k.as_str()) && !k.starts_with("sec-")
        })
        .map(|(k, v)| (k.to_lowercase(), json!(v)))
        .collect();
    let body = replay.body.as_ref().map(|b| json!({ "type": "text", "value": b })).unwrap_or(Value::Null);
    json!({ "url": replay.url, "method": replay.method, "headers": headers, "body": body, "replaySupported": replay.replay_supported })
}

/// The replay orchestration reports its result by navigating to
/// `REPLAY_DONE_PREFIX#<json>` (Electron read `executeJavaScript`'s value).
fn replay_script(config: &Value) -> String {
    let body = CAPTCHA_REPLAY_JS.replacen("\n(async () => {", "\nglobalThis.__lbxReplayPromise = (async () => {", 1);
    format!(
        "globalThis.__lbxCaptchaReplay = {config};\n{body}\nPromise.resolve(globalThis.__lbxReplayPromise).then((r) => {{ window.location.href = '{REPLAY_DONE_PREFIX}#' + encodeURIComponent(JSON.stringify(r === undefined ? null : r)); }}, (e) => {{ window.location.href = '{REPLAY_DONE_PREFIX}#' + encodeURIComponent(JSON.stringify({{ __lunarReplayError: true, message: String(e && e.message || e) }})); }});"
    )
}

fn is_serialized_response(v: &Value) -> bool {
    v["status"].is_number() && v["statusText"].is_string() && v["url"].is_string() && v["headers"].is_array() && v["bodyText"].is_string()
}

/// `openCaptchaReplayWindow`: the terminal response of the replayed request.
async fn open_replay_window(state: &Arc<AppState>, replay: &ReplayRequest, challenge: &Challenge) -> Value {
    let block = Arc::new(AtomicBool::new(false));
    let (window, mut events, mut loaded) = match open_popup(state, "Roblox Verification", (520.0, 640.0), REPLAY_DONE_PREFIX, Arc::clone(&block)).await {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!("Failed during Roblox captcha replay popup execution: {error}");
            track_popup_failed(state, &challenge.id, "replay_popup", "replay_execute", Some(error.to_string()));
            return failure_response(500, "Captcha Replay Failed", "Roblox captcha replay failed before a terminal response was captured.", &replay.url);
        }
    };
    let cancelled = || failure_response(499, "Captcha Replay Cancelled", "Captcha replay window was closed before Roblox returned a terminal response.", &replay.url);
    let finish = |window: &WebviewWindow, response: Value| {
        // `syncPopupAuthCookie`: keep a cookie the popup rotated.
        let latest = Url::parse("https://www.roblox.com")
            .ok()
            .and_then(|u| window.cookies_for_url(u).ok())
            .and_then(|c| c.iter().find(|c| c.name() == ".ROBLOSECURITY").map(|c| c.value().to_owned()));
        if let Some(latest) = latest.filter(|l| Some(l.as_str()) != state.roblox.session_cookie().as_deref()) {
            state.auth.adopt_session_cookie(&latest);
        }
        let _ = window.destroy();
        response
    };
    tokio::select! {
        _ = loaded.recv() => {}
        _ = events.recv() => return cancelled(),
    }
    block.store(true, Ordering::SeqCst);
    let _ = window.show();
    let _ = window.set_focus();
    warm(&window).await;
    let config = json!({
        "replayRequest": replay_request_json(replay),
        "initialChallenge": { "challengeId": challenge.id, "challengeType": challenge.kind, "challengeBase64Metadata": challenge.metadata },
        "arkoseScriptUrlTemplate": ARKOSE_SCRIPT_URL_TEMPLATE,
        "challengeContinueUrl": CHALLENGE_CONTINUE_URL,
        "funCaptchaPublicKeys": funcaptcha_keys(state).await,
        "actionTypeToFunCaptchaKey": { "FollowUser": "ACTION_TYPE_FOLLOW_USER", "JoinGroup": "ACTION_TYPE_GROUP_JOIN", "Login": "ACTION_TYPE_WEB_LOGIN", "Signup": "ACTION_TYPE_WEB_SIGNUP" },
        "fallbackFunCaptchaActionType": FALLBACK_FUNCAPTCHA_ACTION_TYPE,
    });
    if let Err(error) = window.eval(&replay_script(&config)) {
        tracing::warn!("Failed during Roblox captcha replay popup execution: {error}");
        track_popup_failed(state, &challenge.id, "replay_popup", "replay_execute", Some(error.to_string()));
        return finish(&window, failure_response(500, "Captcha Replay Failed", "Roblox captcha replay failed before a terminal response was captured.", &replay.url));
    }
    let Some(PopupEvent::Done(url)) = events.recv().await else { return cancelled() };
    let encoded = url.split_once('#').map(|(_, f)| f).unwrap_or("");
    let result: Value = urlencoding::decode(encoded).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    if is_serialized_response(&result) {
        let still = result["headers"].as_array().into_iter().flatten().any(|h| h[0].as_str().is_some_and(|n| n.eq_ignore_ascii_case("rblx-challenge-id")));
        tracing::info!("[Captcha Replay] terminal response: status={} challengeStillPresent={still}", result["status"]);
        return finish(&window, result);
    }
    let message = result.get("message").and_then(Value::as_str).filter(|_| result["__lunarReplayError"] == true).unwrap_or("Roblox replay did not return a usable HTTP response.");
    finish(&window, failure_response(500, "Captcha Replay Failed", message, &replay.url))
}

pub fn register(router: &mut Router) {
    router
        .handle("challenge:solve", |ctx, args| async move {
            let id: String = args.get(0)?;
            let kind: String = args.get(1)?;
            let metadata: String = args.get(2)?;
            Ok(solve(&ctx.state, &id, &kind, &metadata).await)
        })
        .handle("challenge:dismiss", |ctx, args| async move {
            let id: String = args.get(0)?;
            let reason: Option<String> = args.opt(1)?;
            dismiss(&ctx.state, &id, reason.as_deref().unwrap_or("user_dismissed"));
            Ok(true)
        })
        .handle("challenge:openCaptchaWindow", |ctx, args| async move {
            let blob: String = args.get(0)?;
            let id: Option<String> = args.opt(1)?;
            Ok(open_captcha_window(&ctx.state, &blob, id.as_deref().unwrap_or("")).await)
        })
        .handle("challenge:closeCaptchaWindow", |ctx, _| async move {
            close_captcha_window(&ctx.state);
            Ok(true)
        })
        .handle("challenge:triggerTest", |ctx, args| async move {
            if !cfg!(debug_assertions) {
                return Ok(Value::Null);
            }
            let challenge = Challenge { id: args.get(0)?, kind: args.get(1)?, metadata: args.get(2)? };
            let state = Arc::clone(&ctx.state);
            tauri::async_runtime::spawn(async move {
                let replay = ReplayRequest { url: String::new(), method: "GET".into(), headers: Vec::new(), body: None, replay_supported: false };
                handle_challenge(&state, challenge, replay).await;
            });
            Ok(Value::Null)
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn proof_of_work() {
        // 3^(2^3) mod 7 = 3^8 mod 7 = 2
        assert_eq!(super::solve_puzzle("3", "7", 3).as_deref(), Some("2"));
        assert_eq!(super::normalize_action_type(Some(&serde_json::json!(2))), "RobuxSpend");
        assert_eq!(super::normalize_action_type(Some(&serde_json::json!("nope"))), "Generic");
    }
}
