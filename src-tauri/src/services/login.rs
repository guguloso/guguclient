//! Roblox sign-in window (`auth:login`). Port of `AuthService.openLoginWindow`
//! and `initAuthWindow`: roblox.com/login opens in an isolated WebView2
//! profile (no bridge), its `.ROBLOSECURITY` cookie is polled every 500ms,
//! and once signed in the same window walks the Lunar OAuth consent before
//! closing. Passkeys stay disabled like the original's Permissions-Policy.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::webview::PageLoadEvent;
use tauri::{Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tokio::sync::mpsc;

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::metadata::{gated_installation_id, LunarApi};
use crate::state::AppState;

const LOGIN_WINDOW: &str = "roblox-login";
const LOGIN_URL: &str = "https://www.roblox.com/login";
const POLL_INTERVAL: Duration = Duration::from_millis(500);

type LoginFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

#[derive(Default)]
pub struct LoginState {
    pending: Mutex<Option<LoginFuture>>,
}

enum Event {
    Navigated(String),
    OAuthCallback(String),
    Closed,
}

/// `Permissions-Policy: publickey-credentials-get=(), publickey-credentials-create=()`.
const DISABLE_PASSKEYS: &str = r#"(() => {
  try {
    if (navigator.credentials) {
      const deny = () => Promise.reject(new DOMException('Not allowed', 'NotAllowedError'));
      Object.defineProperty(navigator.credentials, 'get', { value: deny });
      Object.defineProperty(navigator.credentials, 'create', { value: deny });
    }
    if (window.PublicKeyCredential) {
      window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable = () => Promise.resolve(false);
    }
  } catch (_) {}
})();"#;

/// `buildPrefillScript`, run once per window (sessionStorage guard).
fn prefill_script(username: &str) -> String {
    let encoded = serde_json::to_string(username).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(() => {{
  try {{
    if (sessionStorage.getItem('__guguPrefilled')) return;
    const encoded = {encoded};
    const setReactValue = (el, value) => {{
      const proto = window.HTMLInputElement && window.HTMLInputElement.prototype;
      const descriptor = proto && Object.getOwnPropertyDescriptor(proto, 'value');
      const setter = descriptor && descriptor.set;
      if (setter) {{ setter.call(el, value); }} else {{ el.value = value; }}
      el.dispatchEvent(new Event('input', {{ bubbles: true }}));
      el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    }};
    const tryPrefill = () => {{
      const username = document.getElementById('login-username');
      const password = document.getElementById('login-password');
      if (!username || !password) {{ return false; }}
      setReactValue(username, encoded);
      password.focus();
      sessionStorage.setItem('__guguPrefilled', '1');
      return true;
    }};
    if (tryPrefill()) return;
    let attempts = 0;
    const interval = setInterval(() => {{
      attempts += 1;
      if (tryPrefill() || attempts >= 40) clearInterval(interval);
    }}, 100);
  }} catch (err) {{ console.warn('prefill failed', err); }}
}})();"#
    )
}

fn redirect_uri() -> String {
    format!("{}/v1/auth/callback", LunarApi::base_url())
}

/// `extractOAuthCodeFromUrl`: `(code, state)` of the Lunar callback.
fn oauth_code(url: &str) -> Option<(String, String)> {
    if !url.starts_with(&redirect_uri()) {
        return None;
    }
    let parsed = Url::parse(url).ok()?;
    let get = |k: &str| parsed.query_pairs().find(|(name, _)| name == k).map(|(_, v)| v.into_owned());
    Some((get("code").filter(|c| !c.is_empty())?, get("state").unwrap_or_default()))
}

/// `generatePkce`.
fn pkce() -> (String, String) {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn is_login_success_url(url: &str) -> bool {
    url.contains("/home") || url.contains("/discover") || url == "https://www.roblox.com/"
}

fn open_window(state: &AppState, prefill: Option<String>, events: mpsc::UnboundedSender<Event>) -> AppResult<WebviewWindow> {
    let profile = std::env::temp_dir().join(format!("gugu-auth-{}", uuid::Uuid::new_v4()));
    let mut script = DISABLE_PASSKEYS.to_owned();
    if let Some(username) = prefill.filter(|u| !u.trim().is_empty()) {
        script.push('\n');
        script.push_str(&prefill_script(username.trim()));
    }
    let nav_events = events.clone();
    let url = Url::parse(LOGIN_URL).map_err(|e| AppError::msg(e.to_string()))?;
    let window = WebviewWindowBuilder::new(&state.app, LOGIN_WINDOW, WebviewUrl::External(url))
        .title("Log in to Roblox")
        .inner_size(510.0, 600.0)
        .resizable(false)
        .maximizable(false)
        .visible(false)
        .data_directory(profile.clone())
        .initialization_script(&script)
        .on_navigation(move |url| {
            let url = url.to_string();
            if oauth_code(&url).is_some() {
                let _ = nav_events.send(Event::OAuthCallback(url));
                return false;
            }
            let _ = nav_events.send(Event::Navigated(url));
            true
        })
        .on_page_load(|window, payload| {
            if payload.event() == PageLoadEvent::Finished && !window.is_visible().unwrap_or(true) {
                let _ = window.show();
                let _ = window.set_always_on_top(true);
            }
        })
        .build()?;
    window.on_window_event(move |event| {
        if let WindowEvent::Destroyed = event {
            let _ = events.send(Event::Closed);
            let profile = profile.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                let _ = std::fs::remove_dir_all(profile);
            });
        }
    });
    Ok(window)
}

/// Polls the window's cookies until Roblox accepts one.
async fn check_for_login(state: &AppState, window: &WebviewWindow) -> AppResult<Option<Value>> {
    let url = Url::parse("https://www.roblox.com/").map_err(|e| AppError::msg(e.to_string()))?;
    let cookies = window.cookies_for_url(url)?;
    let Some(cookie) = cookies.iter().find(|c| c.name() == ".ROBLOSECURITY").map(|c| c.value().to_owned()) else { return Ok(None) };
    let tracker = cookies.iter().find(|c| c.name() == "RBXEventTrackerV2").map(|c| c.value().to_owned());
    state.auth.persist_login(&cookie, tracker.as_deref()).await
}

/// `resolveCurrentUserAbove13` (Roblox web settings).
async fn current_user_above13(state: &AppState) -> Option<bool> {
    let context = state.auth.web_context().await;
    let settings: AppResult<Value> = state
        .roblox
        .request_web(reqwest::Method::GET, "https://www.roblox.com/my/settings/json", None, context.as_ref().map(|(c, t)| (c.as_str(), t.as_str())))
        .await;
    match settings {
        Ok(s) => {
            let above = s.get("UserAbove13").and_then(Value::as_bool);
            if above.is_none() {
                tracing::warn!("Auth: Roblox settings response omitted 13+ status before OAuth consent");
            }
            above
        }
        Err(error) => {
            tracing::warn!("Auth: Failed to resolve 13+ status before OAuth consent: {error}");
            None
        }
    }
}

/// `startOAuthConsent`: links the account to the Lunar API, best effort.
async fn oauth_consent(state: &Arc<AppState>, window: &WebviewWindow, events: &mut mpsc::UnboundedReceiver<Event>) {
    let report = |outcome: &str| state.track("oauth.consent_result", json!({ "outcome": outcome }));
    if current_user_above13(state).await == Some(false) {
        tracing::info!("Auth: Skipping OAuth consent for account not confirmed 13+");
        return;
    }
    let (verifier, challenge) = pkce();
    let csrf = uuid::Uuid::new_v4().simple().to_string();
    let installation = gated_installation_id(state);
    let version = env!("CARGO_PKG_VERSION");
    let query = [("installation_id", installation.as_str()), ("version", version), ("code_challenge", challenge.as_str()), ("state", csrf.as_str())];
    let authorize: AppResult<Value> = state.metadata.api().get("/v1/auth/authorize-url", &query).await;
    let Some(url) = authorize.ok().and_then(|r| r.get("url").and_then(Value::as_str).map(str::to_owned)) else {
        tracing::warn!("Auth: Failed to get OAuth authorize URL");
        report("authorize_url_failed");
        return;
    };
    tracing::info!("Auth: Redirecting to OAuth consent screen");
    let Ok(parsed) = Url::parse(&url) else {
        report("load_failed");
        return;
    };
    if window.navigate(parsed).is_err() {
        report("load_failed");
        return;
    }
    while let Some(event) = events.recv().await {
        match event {
            Event::Closed => {
                tracing::info!("Auth: OAuth consent window closed, proceeding without API registration");
                report("user_cancelled");
                return;
            }
            Event::OAuthCallback(url) => {
                let Some((code, returned_state)) = oauth_code(&url) else { continue };
                if returned_state != csrf {
                    tracing::warn!("Auth: OAuth state mismatch, rejecting callback (possible CSRF attempt)");
                    report("state_mismatch");
                    return;
                }
                tracing::info!("Auth: OAuth consent completed, exchanging code");
                let state = Arc::clone(state);
                tauri::async_runtime::spawn(async move {
                    let installation = gated_installation_id(&state);
                    let query = [("installation_id", installation.as_str()), ("version", env!("CARGO_PKG_VERSION"))];
                    let body = json!({ "code": code, "code_verifier": verifier });
                    match state.metadata.api().post("/v1/auth", &query, &body).await {
                        Ok(()) => {
                            tracing::info!("Auth: Lunar API user verification successful");
                            state.track("oauth.consent_result", json!({ "outcome": "success" }));
                        }
                        Err(error) => {
                            tracing::warn!("Auth: Lunar API verification failed: {error}");
                            state.track("oauth.consent_result", json!({ "outcome": "exchange_failed" }));
                        }
                    }
                });
                return;
            }
            Event::Navigated(_) => {}
        }
    }
}

/// `createLoginPromise`.
async fn run_login(state: Arc<AppState>, prefill: Option<String>) -> Result<Value, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let window = open_window(&state, prefill, tx).map_err(|error| {
        tracing::error!("Auth: Failed to initialize login window: {error}");
        error.to_string()
    })?;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    let user = loop {
        let check = tokio::select! {
            _ = ticker.tick() => true,
            event = rx.recv() => match event {
                Some(Event::Navigated(url)) => is_login_success_url(&url),
                Some(Event::OAuthCallback(_)) => false,
                Some(Event::Closed) | None => return Err("Login cancelled".into()),
            },
        };
        if !check {
            continue;
        }
        match check_for_login(&state, &window).await {
            Ok(Some(user)) => break user,
            Ok(None) => {}
            Err(error) => {
                tracing::error!("Auth: Login cookie polling failed: {error}");
                let _ = window.destroy();
                return Err(error.to_string());
            }
        }
    };
    oauth_consent(&state, &window, &mut rx).await;
    let _ = window.close();
    Ok(user)
}

/// `openLoginWindow`: concurrent calls share the open window.
async fn open_login(state: &Arc<AppState>, prefill: Option<String>) -> AppResult<Value> {
    let shared = {
        let mut pending = state.login.pending.lock();
        match pending.as_ref() {
            Some(running) => {
                if let Some(window) = state.app.get_webview_window(LOGIN_WINDOW) {
                    let _ = window.set_focus();
                }
                running.clone()
            }
            None => {
                let owner = Arc::clone(state);
                let future: BoxFuture<'static, Result<Value, String>> = async move {
                    let result = run_login(Arc::clone(&owner), prefill).await;
                    *owner.login.pending.lock() = None;
                    result
                }
                .boxed();
                let shared = future.shared();
                *pending = Some(shared.clone());
                shared
            }
        }
    };
    shared.await.map_err(AppError::msg)
}

pub fn register(router: &mut Router) {
    router.handle("auth:login", |ctx, args| async move {
        let options: Option<Value> = args.opt(0)?;
        let prefill = options.as_ref().and_then(|o| o.get("prefillUsername")).and_then(Value::as_str).map(str::to_owned);
        let user = open_login(&ctx.state, prefill).await?;
        ctx.state.track("launcher.auth", json!({ "action": "login" }));
        Ok(user)
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn oauth_callback() {
        let url = format!("{}?code=abc&state=xyz", super::redirect_uri());
        assert_eq!(super::oauth_code(&url), Some(("abc".into(), "xyz".into())));
        assert!(super::oauth_code("https://www.roblox.com/home").is_none());
    }
}
