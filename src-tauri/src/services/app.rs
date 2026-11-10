//! App-level odds and ends: `app:*` (external links, installation id),
//! `toast:*` (in-app toasts + OS notifications), `licenses:*`,
//! `itemValues:*` and `window:navigateMain`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::Url;
use serde_json::{json, Map, Value};
use tauri::Manager;
use tauri_plugin_notification::NotificationExt;

use crate::error::AppError;
use crate::ipc::Router;
use crate::services::{metadata, protocol, roblox_layout};
use crate::state::AppState;

const ALLOWED_SCHEMES: [&str; 5] = ["http", "https", "roblox-player", "roblox", "roblox-studio"];
const TRUSTED_DOMAINS: [&str; 4] = [
    "lunar.gg",
    "lunarclient.com",
    "moonsworth.com",
    "roblox.com",
];
const TRUSTED_DOMAINS_CACHE_KEY: &str = "external-links.trusted-domains";
const DEFAULT_TOAST_DURATION_MS: u64 = 8_000;
const TOAST_CONTEXT_TTL_BUFFER_MS: u64 = 60_000;
const ITEM_VALUE_TTL: Duration = Duration::from_secs(30 * 60);
const ITEM_VALUE_REFRESH_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub struct AppMisc {
    /// toast id → (expiry, analytics `{ notificationType, payload }`).
    toasts: Mutex<HashMap<String, (Instant, Option<Value>)>>,
    item_values: tokio::sync::Mutex<Option<(Instant, Value)>>,
}

// ------------------------------------------------------------------ external links

fn is_external_open_allowed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| ALLOWED_SCHEMES.contains(&u.scheme().to_lowercase().as_str()))
}

fn external_hostname(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    matches!(parsed.scheme(), "http" | "https")
        .then(|| parsed.host_str().unwrap_or("").to_lowercase())
}

fn in_domains<S: AsRef<str>>(host: &str, domains: &[S]) -> bool {
    domains.iter().any(|d| {
        let d = d.as_ref();
        host == d || host.ends_with(&format!(".{d}"))
    })
}

fn user_trusted_domains(state: &AppState) -> Vec<String> {
    state
        .cache
        .get(TRUSTED_DOMAINS_CACHE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn domain_requiring_confirmation(state: &AppState, url: &str) -> Option<String> {
    if !state.settings.get_bool("confirm-external-links") {
        return None;
    }
    let host = external_hostname(url)?;
    if in_domains(&host, &TRUSTED_DOMAINS) || in_domains(&host, &user_trusted_domains(state)) {
        return None;
    }
    Some(host)
}

/// `shell.openExternal`.
pub fn open_external_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `explorer <url>` hands the URL to its registered handler without a shell.
        let _ = std::process::Command::new("explorer.exe")
            .arg(url)
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = url;
}

fn perform_external_open(state: &AppState, url: &str, source_route: Option<Value>) {
    state.track(
        "external.open",
        json!({ "url": url, "source_route": source_route }),
    );
    open_external_url(url);
}

// ------------------------------------------------------------------ toasts

/// `ToastService.showToast` (requests from the renderer carry no callbacks).
pub fn show_toast(state: &AppState, options: &Value) -> String {
    let id = options
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let duration = options
        .get("duration")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TOAST_DURATION_MS);
    let placement = options
        .get("placement")
        .and_then(Value::as_str)
        .unwrap_or("both");
    let analytics = options.get("analytics").filter(|a| !a.is_null()).cloned();
    {
        let mut toasts = state.misc.toasts.lock();
        let now = Instant::now();
        toasts.retain(|_, (expires, _)| *expires > now);
        toasts.insert(
            id.clone(),
            (
                now + Duration::from_millis(duration + TOAST_CONTEXT_TTL_BUFFER_MS),
                analytics.clone(),
            ),
        );
    }
    let mut payload = Map::new();
    if let Some(actions) = options.get("actions").and_then(Value::as_array) {
        payload.insert(
            "actions".into(),
            Value::Array(
                actions
                    .iter()
                    .map(|a| json!({ "id": a.get("id"), "text": a.get("text") }))
                    .collect(),
            ),
        );
    }
    for key in [
        "body",
        "image",
        "isDismissable",
        "title",
        "variant",
        "variantData",
    ] {
        if let Some(v) = options.get(key) {
            payload.insert(key.into(), v.clone());
        }
    }
    payload.insert("duration".into(), json!(duration));
    payload.insert("id".into(), json!(id));
    let payload = Value::Object(payload);
    let track = |interaction: &str, placement: &str| {
        if let Some(analytics) = &analytics {
            let mut event = analytics
                .get("payload")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            event.insert("interactionType".into(), json!(interaction));
            event.insert("notificationId".into(), json!(id));
            event.insert(
                "notificationType".into(),
                analytics
                    .get("notificationType")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            event.insert("placement".into(), json!(placement));
            if placement == "native" {
                event.insert("actionId".into(), Value::Null);
                event.insert("extra".into(), Value::Null);
            }
            state.track("notification.interaction", Value::Object(event));
        }
    };
    if placement != "native" {
        if state.app.get_webview_window(crate::MAIN_WINDOW).is_some() {
            state.events.emit_main("toast:show", payload.clone());
            track("impression", "toast");
        } else {
            tracing::warn!("[ToastService] Main window unavailable, skipping toast");
        }
    }
    if placement != "toast" && state.settings.get_bool("os-notifications") {
        let force = placement == "native";
        let focused = state
            .app
            .get_webview_window(crate::MAIN_WINDOW)
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if force || !focused {
            let title = payload.get("title").and_then(Value::as_str).unwrap_or("");
            let body = payload.get("body").and_then(Value::as_str).unwrap_or("");
            match state
                .app
                .notification()
                .builder()
                .title(title)
                .body(body)
                .show()
            {
                Ok(()) => track("impression", "native"),
                Err(error) => {
                    tracing::warn!("[ToastService] Failed to show OS notification: {error}")
                }
            }
        }
    }
    id
}

fn handle_toast_interaction(state: &AppState, interaction: &Value) -> bool {
    let id = interaction
        .get("toastId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let analytics = match state.misc.toasts.lock().get(id) {
        Some((_, analytics)) => analytics.clone(),
        None => {
            tracing::warn!("[ToastService] No context for toast {id}");
            return false;
        }
    };
    if let Some(analytics) = analytics {
        let mut event = analytics
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        event.insert(
            "actionId".into(),
            interaction.get("actionId").cloned().unwrap_or(Value::Null),
        );
        event.insert(
            "extra".into(),
            interaction.get("extra").cloned().unwrap_or(Value::Null),
        );
        event.insert(
            "interactionType".into(),
            interaction.get("type").cloned().unwrap_or(Value::Null),
        );
        event.insert("notificationId".into(), json!(id));
        event.insert(
            "notificationType".into(),
            analytics
                .get("notificationType")
                .cloned()
                .unwrap_or(Value::Null),
        );
        event.insert("placement".into(), json!("toast"));
        state.track("notification.interaction", Value::Object(event));
    }
    true
}

// ------------------------------------------------------------------ item values

async fn item_values(state: &AppState, force: bool) -> Result<Value, AppError> {
    let mut slot = state.misc.item_values.lock().await;
    let max_age = if force {
        ITEM_VALUE_REFRESH_COOLDOWN
    } else {
        ITEM_VALUE_TTL
    };
    if let Some((fetched, cache)) = slot.as_ref() {
        if fetched.elapsed() < max_age {
            return Ok(cache.clone());
        }
    }
    tracing::debug!("[ItemValues] Fetching value map");
    let installation_id = metadata::gated_installation_id(state);
    let query = [
        ("installation_id", installation_id.as_str()),
        ("version", env!("CARGO_PKG_VERSION")),
    ];
    let mut response: Value = state
        .metadata
        .api()
        .get("/v1/economy/items/rolimons", &query)
        .await?;
    let map = response
        .get_mut("items")
        .map(Value::take)
        .unwrap_or(Value::Null);
    let cache = json!({ "fetchedAt": crate::util::now_millis(), "map": map });
    *slot = Some((Instant::now(), cache.clone()));
    Ok(cache)
}

pub fn register(router: &mut Router) {
    router
        .handle("app:openApplicationFolder", |ctx, _| async move { Ok(roblox_layout::open_path(&ctx.state.paths.base)) })
        .handle("app:getInstallationId", |ctx, _| async move { Ok(metadata::installation_id_value(&ctx.state.paths.base)) })
        .handle("app:openExternal", |ctx, args| async move {
            let url: String = args.get(0)?;
            let source_route: Option<Value> = args.opt(1)?;
            let state: &Arc<AppState> = &ctx.state;
            if protocol::is_app_link(&url) {
                protocol::handle_url(state, &url, "external_open").await;
                return Ok(Value::Null);
            }
            if !is_external_open_allowed(&url) {
                tracing::warn!("[App] Blocked external open with disallowed protocol: {url}");
                return Ok(Value::Null);
            }
            if let Some(domain) = domain_requiring_confirmation(state, &url) {
                let _ = tauri::Emitter::emit(
                    &ctx.window,
                    "gugu:ipc-event",
                    json!({ "channel": "app:externalOpenConfirmRequested", "payload": { "url": url, "domain": domain } }),
                );
                return Ok(Value::Null);
            }
            perform_external_open(state, &url, source_route);
            Ok(Value::Null)
        })
        .handle("app:confirmExternalOpen", |ctx, args| async move {
            let url: String = args.get(0)?;
            let remember: Option<bool> = args.opt(1)?;
            if !is_external_open_allowed(&url) {
                tracing::warn!("[App] Blocked external open with disallowed protocol: {url}");
                return Ok(Value::Null);
            }
            if let (Some(true), Some(host)) = (remember, external_hostname(&url)) {
                let mut trusted = user_trusted_domains(&ctx.state);
                if !trusted.contains(&host) {
                    trusted.push(host);
                    ctx.state.cache.set(TRUSTED_DOMAINS_CACHE_KEY, json!(trusted))?;
                }
            }
            perform_external_open(&ctx.state, &url, None);
            Ok(Value::Null)
        })
        .handle("toast:showToast", |ctx, args| async move {
            let request: Value = args.get(0)?;
            Ok(show_toast(&ctx.state, &request))
        })
        .handle("toast:handleInteraction", |ctx, args| async move {
            let interaction: Value = args.get(0)?;
            Ok(handle_toast_interaction(&ctx.state, &interaction))
        })
        // No bundled licenses file ships with this build; Electron returns [] too.
        .handle("licenses:getDirect", |_, _| async move { Ok(Vec::<Value>::new()) })
        .handle("licenses:openFolder", |ctx, _| async move {
            let dir = ctx.state.paths.base.join("licenses");
            std::fs::create_dir_all(&dir)?;
            let result = roblox_layout::open_path(&dir);
            if result.is_empty() {
                Ok(Value::Null)
            } else {
                Err(AppError::msg(result))
            }
        })
        .handle("itemValues:getValueMap", |ctx, args| async move {
            let props: Option<Value> = args.opt(0)?;
            let force = props.and_then(|p| p.get("force").and_then(Value::as_bool)).unwrap_or(false);
            item_values(&ctx.state, force).await
        })
        .handle("window:navigateMain", |ctx, args| async move {
            let target: Value = args.get(0)?;
            if let Some(window) = ctx.state.app.get_webview_window(crate::MAIN_WINDOW) {
                let _ = window.show();
                ctx.state.events.emit_main("window:mainNavigate", target);
            }
            Ok(Value::Null)
        });
}
