//! `roblox:`/`roblox-player:`/`roblox-studio:` deep links and
//! `lunar-roblox://` app links (`protocol:*`). Port of `ProtocolService`,
//! the URL parsers and the Windows handler registration (with backups of
//! the previous Roblox handler so it can be restored).

use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::Url;
use serde_json::{json, Value};
use tauri::Manager;

use crate::ipc::Router;
use crate::services::registry;
use crate::state::AppState;

pub const LUNAR_SCHEME: &str = "lunar-roblox";
const LUNAR_PREFIX: &str = "lunar-roblox:";
const PLAYER_PREFIX: &str = "roblox-player:";
const ROBLOX_PREFIX: &str = "roblox:";
const STUDIO_PREFIX: &str = "roblox-studio:";
const PLACE_LAUNCHER_KEY: &str = "placelauncherurl:";
const ROBLOX_SCHEMES: [&str; 3] = ["roblox-player", "roblox", "roblox-studio"];
const BACKUP_KEY: &str = r"HKCU\Software\LunarForRoblox\ProtocolHandlerBackup";
const CLASS_PREFIX: &str = r"HKCU\Software\Classes\";
const COMMAND_SUBKEY: &str = r"\shell\open\command";
const EXCLUDED_NAVIGATE_PREFIXES: [&str; 3] = ["/second-screen", "/updater", "/ad-test"];
const TRADE_OFFER_HOST: &str = "trade-offer";
const TRADE_OFFER_MAX_ITEMS: usize = 4;
const MAX_TRADE_ROBUX: u64 = 2_147_483_647;
const REMOTE_MODIFIABLE_SETTINGS: [&str; 18] = [
    "ask-instance-on-deeplink",
    "auto-check-updates",
    "auto-clean-cache",
    "avatar-preview-3d",
    "close-on-launch",
    "close-to-launcher",
    "close-to-tray",
    "disable-roblox-startup",
    "disable-roblox-tray",
    "download-notifications",
    "enable-activity-tracking",
    "enable-animations",
    "enable-discord-rich-presence",
    "open-second-screen-on-launch",
    "os-notifications",
    "post-game-notifications",
    "show-item-values",
    "theme",
];

#[derive(Default)]
pub struct ProtocolService {
    pending_deep_link: Mutex<Option<Value>>,
    pending_app_link: Mutex<Option<Value>>,
    handler_sync: tokio::sync::Mutex<()>,
}

pub fn is_app_link(url: &str) -> bool {
    url.to_lowercase().starts_with(LUNAR_PREFIX)
}

/// `extractProtocolUrl(argv)`.
pub fn extract_protocol_url(argv: &[String]) -> Option<String> {
    argv.iter()
        .find(|a| {
            a.starts_with(PLAYER_PREFIX)
                || a.starts_with(ROBLOX_PREFIX)
                || a.starts_with(STUDIO_PREFIX)
                || is_app_link(a)
        })
        .cloned()
}

/// `parsePositiveInt` (digits only, safe integer, > 0).
fn positive_int(raw: Option<&str>) -> Option<u64> {
    non_negative_int(raw).filter(|v| *v > 0)
}

fn non_negative_int(raw: Option<&str>) -> Option<u64> {
    let raw = raw?;
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>()
        .ok()
        .filter(|v| *v <= 9_007_199_254_740_991)
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

pub fn redact(url: &str) -> String {
    if url.starts_with(PLAYER_PREFIX) {
        "roblox-player:<redacted>".into()
    } else if url.starts_with(STUDIO_PREFIX) {
        "roblox-studio:<redacted>".into()
    } else if url.starts_with(ROBLOX_PREFIX) {
        "roblox:<redacted>".into()
    } else if is_app_link(url) {
        match Url::parse(url) {
            Ok(parsed) => {
                let mut keys: Vec<String> =
                    parsed.query_pairs().map(|(k, _)| k.into_owned()).collect();
                keys.sort();
                keys.dedup();
                let host = parsed.host_str().unwrap_or("").to_lowercase();
                if keys.is_empty() {
                    format!("{LUNAR_PREFIX}//{host}")
                } else {
                    format!("{LUNAR_PREFIX}//{host}?{}", keys.join("&"))
                }
            }
            Err(_) => format!("{LUNAR_PREFIX}<malformed>"),
        }
    } else {
        "unknown:<redacted>".into()
    }
}

/// `parseProtocolUrl`: Roblox URLs → `{ kind, placeId|action/targetId, rawUrl }`.
pub fn parse_deep_link(url: &str) -> Option<Value> {
    if let Some(body) = url.strip_prefix(PLAYER_PREFIX) {
        let _ = body;
        let part = url.split('+').find(|p| p.starts_with(PLACE_LAUNCHER_KEY))?;
        let decoded = urlencoding::decode(&part[PLACE_LAUNCHER_KEY.len()..]).ok()?;
        let parsed = Url::parse(&decoded).ok()?;
        let place = positive_int(query(&parsed, "placeId").as_deref())?;
        return Some(json!({ "kind": "player", "placeId": place, "rawUrl": url }));
    }
    if let Some(body) = url.strip_prefix(STUDIO_PREFIX) {
        let mut pairs = std::collections::HashMap::new();
        for segment in body.split('+') {
            if let Some((k, v)) = segment.split_once(':') {
                pairs.entry(k).or_insert(v);
            }
        }
        let (action, key) = match *pairs.get("task")? {
            "EditPlace" => ("edit-place", "placeId"),
            "EditAsset" => ("edit-asset", "assetId"),
            _ => return None,
        };
        let target = positive_int(pairs.get(key).copied())?;
        return Some(
            json!({ "kind": "studio", "action": action, "targetId": target, "rawUrl": url }),
        );
    }
    if url.starts_with(ROBLOX_PREFIX) {
        let parsed = Url::parse(url).ok()?;
        let from_host = parsed.host_str().and_then(|h| {
            let lower = h.to_lowercase();
            let rest = lower.strip_prefix("placeid=")?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let after = &rest[digits.len()..];
            (after.is_empty() || after.starts_with('&')).then_some(digits)
        });
        let place = positive_int(query(&parsed, "placeId").or(from_host).as_deref())?;
        return Some(json!({ "kind": "player", "placeId": place, "rawUrl": url }));
    }
    None
}

fn action_type(host: &str) -> Option<&'static str> {
    Some(match host.to_lowercase().as_str() {
        "check-for-updates" => "check-for-updates",
        "close-game" => "close-game",
        "close-launcher" => "close-launcher",
        "launch" | "play" => "launch",
        "navigate" => "navigate",
        "set-launcher-stream" => "set-launcher-stream",
        "set-setting" => "set-setting",
        TRADE_OFFER_HOST => TRADE_OFFER_HOST,
        _ => return None,
    })
}

/// `parseAppLinkNavigateUrl`.
fn valid_navigate_target(to: &str) -> bool {
    if !to.starts_with('/')
        || to.starts_with("//")
        || to
            .chars()
            .any(|c| c.is_whitespace() || c == '\\' || c.is_control())
    {
        return false;
    }
    let Ok(url) = Url::parse("lunar-roblox://navigate").and_then(|base| base.join(to)) else {
        return false;
    };
    let path = urlencoding::decode(url.path())
        .map(|p| p.to_lowercase())
        .unwrap_or_default();
    !EXCLUDED_NAVIGATE_PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

fn asset_id_list(raw: Option<&str>) -> Option<Vec<u64>> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return Some(Vec::new());
    };
    let mut ids = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let id = positive_int(Some(token))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    (ids.len() <= TRADE_OFFER_MAX_ITEMS).then_some(ids)
}

fn optional_robux(raw: Option<&str>) -> Option<u64> {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return Some(0);
    }
    non_negative_int(Some(trimmed)).filter(|v| *v <= MAX_TRADE_ROBUX)
}

fn setting_default_kind(state: &AppState, id: &str) -> Option<Value> {
    state.settings.default_value(id)
}

/// `parseAppLinkUrl`: `None` for unsupported links.
fn parse_app_link(state: &AppState, url: &str) -> Option<Value> {
    let parsed = Url::parse(url).ok()?;
    if !parsed.scheme().eq_ignore_ascii_case(LUNAR_SCHEME) {
        return None;
    }
    let kind = action_type(parsed.host_str().unwrap_or(""))?;
    let q = |k: &str| query(&parsed, k);
    match kind {
        "launch" => positive_int(q("placeId").as_deref())
            .map(|place| json!({ "type": "launch", "placeId": place })),
        "navigate" => q("to")
            .filter(|to| valid_navigate_target(to))
            .map(|to| json!({ "type": "navigate", "to": to })),
        "close-game" | "close-launcher" | "check-for-updates" => Some(json!({ "type": kind })),
        "set-launcher-stream" => {
            let stream = q("stream")?.to_lowercase();
            let valid = stream.len() <= 32
                && stream
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && stream
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            valid.then(|| json!({ "type": "set-launcher-stream", "stream": stream }))
        }
        "set-setting" => {
            let id = q("id").filter(|id| REMOTE_MODIFIABLE_SETTINGS.contains(&id.as_str()))?;
            let default = setting_default_kind(state, &id)?;
            if q("reset").as_deref() == Some("true") {
                return Some(json!({ "type": "set-setting", "id": id, "value": default }));
            }
            let raw = q("value")?;
            let value = if default.is_boolean() {
                match raw.as_str() {
                    "true" => json!(true),
                    "false" => json!(false),
                    _ => return None,
                }
            } else if id == "theme" {
                if raw == "dark" || raw == "light" {
                    json!(raw)
                } else {
                    return None;
                }
            } else if id == "auto-check-updates" {
                if ["disabled", "check-only", "check-and-download"].contains(&raw.as_str()) {
                    json!(raw)
                } else {
                    return None;
                }
            } else {
                return None;
            };
            Some(json!({ "type": "set-setting", "id": id, "value": value }))
        }
        TRADE_OFFER_HOST => {
            if positive_int(q("v").as_deref()) != Some(1) {
                return None;
            }
            let from = positive_int(q("from").map(|f| f.trim().to_owned()).as_deref())?;
            let offer = asset_id_list(q("offer").as_deref()).filter(|o| !o.is_empty())?;
            let want = asset_id_list(q("want").as_deref())?;
            let offer_robux = optional_robux(q("offerRobux").as_deref())?;
            let want_robux = optional_robux(q("wantRobux").as_deref())?;
            Some(json!({
                "type": TRADE_OFFER_HOST,
                "offer": {
                    "fromUserId": from, "offerAssetIds": offer, "offerRobux": offer_robux,
                    "version": 1, "wantAssetIds": want, "wantRobux": want_robux,
                },
            }))
        }
        _ => None,
    }
}

fn show_main_window(state: &AppState) {
    if let Some(window) = state.app.get_webview_window(crate::MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn analytics_source(initiator: &str) -> &str {
    if initiator == "second_instance" || initiator == "external_open" {
        initiator
    } else {
        "app_open"
    }
}

fn dispatch_deep_link(state: &AppState, deep_link: Value, initiator: &str) {
    let kind = deep_link.get("kind").and_then(Value::as_str).unwrap_or("");
    let studio = kind == "studio";
    tracing::info!("Parsed deep link: {kind}");
    state.track(
        "launcher.deeplink",
        json!({
            "kind": kind,
            "place_id": if studio { Value::Null } else { deep_link.get("placeId").cloned().unwrap_or(Value::Null) },
            "studio_action": if studio { deep_link.get("action").cloned().unwrap_or(Value::Null) } else { Value::Null },
            "studio_target_id": if studio { deep_link.get("targetId").cloned().unwrap_or(Value::Null) } else { Value::Null },
            "source": analytics_source(initiator),
        }),
    );
    *state.protocol.pending_deep_link.lock() = Some(deep_link.clone());
    show_main_window(state);
    state.events.emit_main("protocol:deepLink", deep_link);
}

fn dispatch_app_link(state: &AppState, action: Value) {
    let request = json!({ "action": action, "id": uuid::Uuid::new_v4().to_string() });
    *state.protocol.pending_app_link.lock() = Some(request.clone());
    show_main_window(state);
    state.events.emit_main("protocol:appLink", request);
}

/// `handleProtocolUrl`.
pub async fn handle_url(state: &Arc<AppState>, url: &str, initiator: &str) {
    tracing::info!("Received protocol URL: {}", redact(url));
    if is_app_link(url) {
        let action = parse_app_link(state, url);
        let parsed = Url::parse(url).ok();
        let utm = [
            "utm_source",
            "utm_medium",
            "utm_campaign",
            "utm_content",
            "utm_term",
        ];
        let mut payload = json!({
            "action": action.as_ref().and_then(|a| a.get("type").cloned())
                .or_else(|| parsed.as_ref().and_then(|p| action_type(p.host_str().unwrap_or("")).map(Value::from)))
                .unwrap_or(Value::Null),
            "handled": action.is_some(),
            "initiator": initiator,
        });
        for key in utm {
            payload[key] = parsed
                .as_ref()
                .and_then(|p| query(p, key))
                .map(Value::from)
                .unwrap_or(Value::Null);
        }
        state.track("launcher.app_link", payload);
        let Some(action) = action else {
            tracing::warn!("Ignoring unsupported app link: {}", redact(url));
            dispatch_app_link(state, json!({ "type": "unsupported" }));
            return;
        };
        let kind = action
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        tracing::info!("Parsed app link action: {kind}");
        match kind.as_str() {
            "launch" => {
                let link =
                    json!({ "kind": "place", "placeId": action.get("placeId"), "rawUrl": url });
                dispatch_deep_link(state, link, initiator);
            }
            "close-launcher" => {
                tracing::info!("Quitting launcher on app link request");
                state.app.exit(0);
            }
            "check-for-updates" => {
                show_main_window(state);
                crate::services::updater::check_for_updates(state, true).await;
            }
            _ => dispatch_app_link(state, action),
        }
        return;
    }
    match parse_deep_link(url) {
        Some(link) => dispatch_deep_link(state, link, initiator),
        None => tracing::warn!("Failed to parse protocol URL: {}", redact(url)),
    }
}

// ------------------------------------------------------------------ handler registration

fn self_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" \"%1\"", exe.display()))
}

/// `isSelfProtocolCommand`: the registered command launches this exe.
fn is_self_command(command: &str) -> bool {
    let exe_in_command = command
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
        .or_else(|| command.split_whitespace().next())
        .map(str::to_lowercase);
    let Some(exe_in_command) = exe_in_command else {
        return false;
    };
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let current = current.display().to_string().to_lowercase();
    exe_in_command == current
        || std::path::Path::new(&exe_in_command).file_name()
            == std::path::Path::new(&current).file_name()
}

fn command_key(scheme: &str) -> String {
    format!("{CLASS_PREFIX}{scheme}{COMMAND_SUBKEY}")
}

/// `app.setAsDefaultProtocolClient` on Windows.
fn set_protocol_client(scheme: &str) -> bool {
    let Some(command) = self_command() else {
        return false;
    };
    let class_key = format!("{CLASS_PREFIX}{scheme}");
    registry::set(&class_key, None, "REG_SZ", &format!("URL:{scheme}")).is_ok()
        && registry::set(&class_key, Some("URL Protocol"), "REG_SZ", "").is_ok()
        && registry::set(&command_key(scheme), None, "REG_SZ", &command).is_ok()
}

fn is_protocol_client(scheme: &str) -> bool {
    matches!(registry::query(&command_key(scheme), None), registry::Query::Found { data, .. } if is_self_command(&data))
}

/// `captureSchemeBackup`: `false` when the current handler is unreadable.
fn capture_backup(scheme: &str) -> bool {
    match registry::query(&command_key(scheme), None) {
        registry::Query::Error(error) => {
            tracing::warn!("Could not query the {scheme} handler before registering: {error}");
            false
        }
        registry::Query::Found { data, kind } if data.contains("%1") && !is_self_command(&data) => {
            let ok = registry::set(BACKUP_KEY, Some(scheme), &kind, &data).is_ok()
                && registry::set(BACKUP_KEY, Some(&format!("{scheme}.type")), "REG_SZ", &kind)
                    .is_ok();
            if ok {
                tracing::info!("Backed up the existing {scheme} handler");
            } else {
                tracing::warn!("Could not back up the {scheme} handler");
            }
            ok
        }
        _ => true,
    }
}

fn delete_backup(scheme: &str) {
    let _ = registry::delete(BACKUP_KEY, Some(scheme));
    let _ = registry::delete(BACKUP_KEY, Some(&format!("{scheme}.type")));
}

fn restore_handler(scheme: &str) {
    let key = command_key(scheme);
    match registry::query(&key, None) {
        registry::Query::Error(error) => {
            tracing::warn!("Could not query the {scheme} handler for restore: {error}")
        }
        registry::Query::Missing => delete_backup(scheme),
        registry::Query::Found { data, .. } if data.contains("%1") => {
            if !is_self_command(&data) {
                delete_backup(scheme);
                return;
            }
            match registry::query(BACKUP_KEY, Some(scheme)) {
                registry::Query::Error(error) => {
                    tracing::warn!("Could not read the {scheme} handler backup: {error}")
                }
                registry::Query::Found { data, kind } => {
                    if registry::set(&key, None, &kind, &data).is_ok() {
                        tracing::info!("Restored the original {scheme} handler");
                        delete_backup(scheme);
                    }
                }
                registry::Query::Missing => {
                    let class_key = format!("{CLASS_PREFIX}{scheme}");
                    let _ = registry::delete(&key, None);
                    let _ = registry::delete(&class_key, Some("URL Protocol"));
                    let _ = registry::delete(&class_key, None);
                    tracing::info!("Removed the {scheme} handler (no prior owner recorded)");
                    delete_backup(scheme);
                }
            }
        }
        registry::Query::Found { .. } => {}
    }
}

/// `ensureAppSchemeRegistered` + `syncRobloxUrlHandlers` (startup and
/// whenever `handle-roblox-urls` changes).
pub async fn sync_handlers(state: &AppState) {
    let _guard = state.protocol.handler_sync.lock().await;
    if !is_protocol_client(LUNAR_SCHEME) {
        if set_protocol_client(LUNAR_SCHEME) {
            tracing::info!("Registered as default protocol handler for {LUNAR_SCHEME}:// URLs");
        } else {
            tracing::warn!("Protocol registration failed for: {LUNAR_SCHEME}");
        }
    }
    if state.settings.get_bool("handle-roblox-urls") {
        let safe: Vec<&str> = ROBLOX_SCHEMES
            .into_iter()
            .filter(|s| capture_backup(s))
            .collect();
        let skipped: Vec<&str> = ROBLOX_SCHEMES
            .into_iter()
            .filter(|s| !safe.contains(s))
            .collect();
        if !skipped.is_empty() {
            tracing::warn!(
                "Skipping takeover of unreadable handlers: {}",
                skipped.join(", ")
            );
        }
        let failures: Vec<&str> = safe
            .iter()
            .copied()
            .filter(|s| !set_protocol_client(s))
            .collect();
        if !failures.is_empty() {
            tracing::warn!("Protocol registration failed for: {}", failures.join(", "));
        } else if skipped.is_empty() {
            tracing::info!("Registered as default protocol handler for roblox URLs");
        }
    } else if ROBLOX_SCHEMES.iter().any(|s| is_protocol_client(s)) {
        for scheme in ROBLOX_SCHEMES {
            restore_handler(scheme);
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("protocol:consumePendingDeepLink", |ctx, _| async move {
            Ok(ctx.state.protocol.pending_deep_link.lock().take())
        })
        .handle("protocol:consumePendingAppLink", |ctx, _| async move {
            Ok(ctx.state.protocol.pending_app_link.lock().take())
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_links_parse_like_electron() {
        let player = "roblox-player:1+launchmode:play+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestGame%26placeId%3D920587237";
        assert_eq!(parse_deep_link(player).unwrap()["placeId"], 920587237);
        assert_eq!(
            parse_deep_link("roblox://experiences/start?placeId=42").unwrap()["placeId"],
            42
        );
        assert_eq!(
            parse_deep_link("roblox://placeId=77&x=1").unwrap()["placeId"],
            77
        );
        let studio = parse_deep_link("roblox-studio:1+task:EditPlace+placeId:5").unwrap();
        assert_eq!(studio["action"], "edit-place");
        assert!(valid_navigate_target("/games/charts"));
        assert!(!valid_navigate_target("/updater"));
        assert!(!valid_navigate_target("//evil"));
        assert_eq!(
            redact("lunar-roblox://launch?placeId=1&b=2"),
            "lunar-roblox://launch?b&placeId"
        );
    }
}
