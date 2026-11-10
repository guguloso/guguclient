//! Private (VIP) servers (`privateServers:*`). Port of
//! `PrivateServersService` + controller: every channel returns
//! `{ ok: true, value }` or `{ ok: false, error }` with the same error kinds.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::{Method, Url};
use serde_json::{json, Value};

use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::state::AppState;

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const STALE_PRICE_ERROR_CODE: i64 = 15;
const PURCHASES_FLAG: &str = "feature-private-server-purchases";

#[derive(Default)]
pub struct PrivateServersState {
    enabled: Mutex<HashMap<u64, (Instant, bool)>>,
    pricing: Mutex<HashMap<u64, (Instant, Value)>>,
    creating: Mutex<std::collections::HashSet<u64>>,
}

/// `PrivateServerError` / outcome-unknown / purchases-disabled payloads.
enum PsError {
    Request { status: Option<u16>, code: Option<i64>, reason: Option<&'static str> },
    OutcomeUnknown,
    PurchasesDisabled,
}

impl PsError {
    fn payload(&self) -> Value {
        match self {
            PsError::Request { status, code, reason } => json!({ "code": code, "kind": "requestFailed", "reason": reason, "status": status }),
            PsError::OutcomeUnknown => json!({ "kind": "outcomeUnknown" }),
            PsError::PurchasesDisabled => json!({ "kind": "purchasesDisabled" }),
        }
    }
}

fn known_reason(status: u16, code: Option<i64>) -> Option<&'static str> {
    match (status, code) {
        (403, Some(2) | None) => Some("notOwner"),
        (403, Some(11 | 21)) => Some("subscriptionExpired"),
        (403, Some(12)) => Some("friendsOnly"),
        (429, _) => Some("rateLimited"),
        (400, Some(STALE_PRICE_ERROR_CODE)) => Some("priceChanged"),
        (400, Some(32)) => Some("insufficientRobux"),
        (400, Some(6)) => Some("tooManyMembers"),
        _ => None,
    }
}

type PsResult<T> = Result<T, PsError>;

/// `readPrivateServerResponse`.
async fn call(state: &AppState, method: Method, url: &str, body: Option<&Value>, message: &str) -> PsResult<Value> {
    let response = match state.roblox.request(method, url, body).await {
        Ok(r) => r,
        Err(error) => {
            tracing::error!("{message} {error}");
            return Err(PsError::Request { status: None, code: None, reason: None });
        }
    };
    let status = response.status().as_u16();
    if response.status().is_success() {
        return response.json().await.map_err(|_| PsError::Request { status: Some(status), code: None, reason: None });
    }
    let text = response.text().await.unwrap_or_default();
    let code = serde_json::from_str::<Value>(&text).ok().and_then(|b| b.pointer("/errors/0/code").and_then(Value::as_i64));
    tracing::warn!("{message} ({status})");
    Err(PsError::Request { status: Some(status), code, reason: known_reason(status, code) })
}

/// `parsePrivateServerLinkCode`.
fn link_code(link: Option<&str>) -> Value {
    let Some(url) = link.and_then(|l| Url::parse(l).ok()) else { return Value::Null };
    let q = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    if let (Some(code), Some("Server")) = (q("code"), q("type").as_deref()) {
        if !code.is_empty() {
            return json!(code);
        }
    }
    q("privateServerLinkCode").or_else(|| q("linkCode")).map(Value::from).unwrap_or(Value::Null)
}

fn member(u: &Value) -> Value {
    json!({ "id": u.get("id"), "name": u.get("name"), "displayName": u.get("displayName").filter(|v| !v.is_null()).or(u.get("name")) })
}

/// `normalizeVipServer`.
fn normalize(raw: &Value) -> Value {
    let sub = raw.get("subscription").cloned().unwrap_or(Value::Null);
    let perms = raw.get("permissions").cloned().unwrap_or(Value::Null);
    let b = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let n = |v: &Value, k: &str| v.get(k).cloned().filter(|x| !x.is_null()).unwrap_or(Value::Null);
    let link = raw.get("link").and_then(Value::as_str);
    json!({
        "id": raw.get("id"),
        "name": raw.get("name"),
        "game": { "universeId": raw.pointer("/game/id"), "rootPlaceId": raw.pointer("/game/rootPlace/id"), "name": raw.pointer("/game/name") },
        "joinCode": raw.get("joinCode").and_then(Value::as_str).unwrap_or(""),
        "active": raw.get("active"),
        "subscription": {
            "active": sub.get("active"), "expired": sub.get("expired"),
            "expirationDate": n(&sub, "expirationDate"), "price": n(&sub, "price"),
            "canRenew": b(&sub, "canRenew"), "hasInsufficientFunds": b(&sub, "hasInsufficientFunds"),
            "hasRecurringProfile": b(&sub, "hasRecurringProfile"), "hasPriceChanged": b(&sub, "hasPriceChanged"),
        },
        "permissions": {
            "friendsAllowed": b(&perms, "friendsAllowed"), "clanAllowed": b(&perms, "clanAllowed"),
            "enemyClanId": n(&perms, "enemyClanId"),
            "members": perms.get("users").and_then(Value::as_array).into_iter().flatten().map(member).collect::<Vec<_>>(),
        },
        "voiceEnabled": raw.pointer("/voiceSettings/enabled").and_then(Value::as_bool).unwrap_or(false),
        "link": link.unwrap_or(""),
        "linkCode": link_code(link),
    })
}

fn purchases_enabled(state: &AppState) -> PsResult<()> {
    if state.feature_flags.is_enabled(PURCHASES_FLAG) {
        Ok(())
    } else {
        tracing::info!("Private server purchase blocked while {PURCHASES_FLAG} is off");
        Err(PsError::PurchasesDisabled)
    }
}

/// `toPurchaseError`: 5xx and transport failures leave the outcome unknown.
fn purchase_error(state: &AppState, error: PsError, surface: &str) -> PsError {
    match error {
        PsError::Request { status: Some(status), code, reason } if status < 500 => {
            tracing::warn!("Purchase failed ({surface}): Roblox rejected the request (status {status}, code {code:?})");
            PsError::Request { status: Some(status), code, reason }
        }
        PsError::Request { status, .. } => {
            tracing::error!("Purchase failed ({surface}): {}", if status.is_some() { "Roblox failed with a server error" } else { "request did not complete" });
            state.events.emit_main("auth:robuxSpent", Value::Null);
            PsError::OutcomeUnknown
        }
        other => other,
    }
}

fn wrap(result: PsResult<Value>) -> Value {
    match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error.payload() }),
    }
}

fn vip(path: &str) -> String {
    format!("{}/vip-servers/{path}", endpoints::GAMES_V1)
}

async fn config(state: &AppState, id: u64) -> PsResult<Value> {
    Ok(normalize(&call(state, Method::GET, &vip(&id.to_string()), None, "Failed to load this private server.").await?))
}

async fn enabled(state: &AppState, universe: u64) -> PsResult<Value> {
    if let Some((at, value)) = state.private_servers.enabled.lock().get(&universe) {
        if at.elapsed() < CACHE_TTL {
            return Ok(json!(value));
        }
    }
    let url = format!("{}/private-servers/enabled-in-universe/{universe}", endpoints::GAMES_V1);
    let raw = call(state, Method::GET, &url, None, "Failed to check private server availability.").await?;
    let value = raw.get("privateServersEnabled").and_then(Value::as_bool) == Some(true);
    state.private_servers.enabled.lock().insert(universe, (Instant::now(), value));
    Ok(json!(value))
}

async fn pricing(state: &AppState, place: u64) -> Value {
    if let Some((at, value)) = state.private_servers.pricing.lock().get(&place) {
        if at.elapsed() < CACHE_TTL {
            return value.clone();
        }
    }
    let fallback = json!({ "canCreate": false, "priceInRobux": null, "productId": null });
    let response = state
        .roblox
        .http()
        .get(format!("{}/games/{place}", endpoints::WEB))
        .header("Accept", "text/html,application/xhtml+xml")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .send()
        .await;
    let html = match response {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(error) => {
            tracing::warn!("Failed to scrape create pricing for place {place}: {error}");
            return fallback;
        }
    };
    let attr = |name: &str| -> Option<String> {
        let marker = format!("{name}=\"");
        let start = html.find(&marker)? + marker.len();
        Some(html[start..].split('"').next()?.to_owned())
    };
    let price = attr("data-private-server-price").filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<u64>().ok());
    let product = attr("data-private-server-product-id").filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let can_create = attr("data-can-create-server").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    tracing::info!("Private server create pricing for place {place}: price={price:?}, productId={product:?}, canCreate={can_create}");
    let value = json!({ "canCreate": can_create, "priceInRobux": price, "productId": product });
    if price.is_some() {
        state.private_servers.pricing.lock().insert(place, (Instant::now(), value.clone()));
    } else {
        tracing::warn!("Private server price not parsed for place {place}");
    }
    value
}

async fn create(state: &AppState, props: &Value) -> PsResult<Value> {
    purchases_enabled(state)?;
    let universe = props.get("universeId").and_then(Value::as_u64).unwrap_or(0);
    if !state.private_servers.creating.lock().insert(universe) {
        tracing::warn!("Ignoring duplicate private server create for universe {universe}; a purchase is already in flight");
        return Err(PsError::OutcomeUnknown);
    }
    let url = format!("{}/games/vip-servers/{universe}", endpoints::GAMES_V1);
    let body = json!({ "name": props.get("name"), "expectedPrice": props.get("expectedPrice") });
    let result = call(state, Method::POST, &url, Some(&body), "Failed to create the private server.").await;
    state.private_servers.creating.lock().remove(&universe);
    match result {
        Ok(raw) => {
            state.private_servers.pricing.lock().clear();
            state.events.emit_main("auth:robuxSpent", Value::Null);
            Ok(json!({
                "vipServerId": raw.get("vipServerId").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
                "accessCode": raw.get("accessCode").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null),
                "name": raw.get("name").cloned().filter(|v| !v.is_null()).or_else(|| props.get("name").cloned()).unwrap_or(Value::Null),
            }))
        }
        Err(error) => {
            let stale = matches!(error, PsError::Request { code: Some(STALE_PRICE_ERROR_CODE), .. });
            let mapped = purchase_error(state, error, "private_server.create");
            if stale || matches!(mapped, PsError::OutcomeUnknown) {
                state.private_servers.pricing.lock().clear();
            }
            Err(mapped)
        }
    }
}

async fn update_subscription(state: &AppState, props: &Value) -> PsResult<Value> {
    let active = props.get("active").and_then(Value::as_bool).unwrap_or(false);
    if active {
        purchases_enabled(state)?;
    }
    let id = props.get("privateServerId").and_then(Value::as_u64).unwrap_or(0);
    let mut body = json!({ "active": active });
    if let Some(price) = props.get("price").filter(|p| !p.is_null()) {
        body["price"] = price.clone();
    }
    let raw = match call(state, Method::PATCH, &vip(&format!("{id}/subscription")), Some(&body), "Failed to update the subscription.").await {
        Ok(raw) => raw,
        Err(error) if active => return Err(purchase_error(state, error, "private_server.renew")),
        Err(error) => return Err(error),
    };
    if active {
        state.events.emit_main("auth:robuxSpent", Value::Null);
    }
    let b = |k: &str| raw.get(k).and_then(Value::as_bool).unwrap_or(false);
    Ok(json!({
        "active": raw.get("active"),
        "expired": b("expired"),
        "expirationDate": raw.get("expirationDate").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null),
        "price": raw.get("price").cloned().filter(|v| !v.is_null()).or_else(|| props.get("price").cloned()).unwrap_or(Value::Null),
        "canRenew": b("canRenew"),
        "hasInsufficientFunds": b("hasInsufficientFunds"),
        "hasRecurringProfile": b("hasRecurringProfile"),
        "hasPriceChanged": b("hasPriceChanged"),
    }))
}

async fn update_permissions(state: &AppState, props: &Value) -> PsResult<Value> {
    let id = props.get("privateServerId").and_then(Value::as_u64).unwrap_or(0);
    let body = json!({
        "clanAllowed": props.get("clanAllowed"), "enemyClanId": props.get("enemyClanId"), "friendsAllowed": props.get("friendsAllowed"),
        "usersToAdd": props.get("usersToAdd"), "usersToRemove": props.get("usersToRemove"),
    });
    let raw = call(state, Method::PATCH, &vip(&format!("{id}/permissions")), Some(&body), "Failed to update the invite list.").await?;
    let Some(users) = raw.get("users").and_then(Value::as_array) else {
        return Ok(config(state, id).await?["permissions"].clone());
    };
    Ok(json!({
        "friendsAllowed": raw.get("friendsAllowed").cloned().filter(|v| !v.is_null()).or_else(|| props.get("friendsAllowed").cloned()),
        "clanAllowed": raw.get("clanAllowed").cloned().filter(|v| !v.is_null()).or_else(|| props.get("clanAllowed").cloned()),
        "enemyClanId": raw.get("enemyClanId").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null),
        "members": users.iter().map(member).collect::<Vec<_>>(),
    }))
}

async fn resolve_share_link(state: &AppState, code: &str) -> PsResult<Value> {
    let url = format!("{}/resolve-link", endpoints::SHARE_LINKS_V1);
    let raw = call(state, Method::POST, &url, Some(&json!({ "linkId": code, "linkType": "Server" })), "Failed to resolve the private server link.").await?;
    let invite = raw.get("privateServerInviteData").cloned().unwrap_or(Value::Null);
    let status = invite.get("status").and_then(Value::as_str);
    let link = invite.get("linkCode").and_then(Value::as_str).filter(|l| !l.is_empty());
    let place = invite.get("placeId").and_then(Value::as_u64).filter(|p| *p > 0);
    let universe = invite.get("universeId").and_then(Value::as_u64).filter(|u| *u > 0);
    match (status.is_none_or(|s| s == "Valid"), link, place, universe) {
        (true, Some(link), Some(place), Some(universe)) => Ok(json!({ "linkCode": link, "placeId": place, "universeId": universe })),
        _ => {
            tracing::warn!("Share link resolution returned no usable invite (status={status:?})");
            Err(PsError::Request { status: None, code: None, reason: Some("invalidLink") })
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("privateServers:getEnabled", |ctx, args| async move {
            let universe: u64 = args.get(0)?;
            Ok(wrap(enabled(&ctx.state, universe).await))
        })
        .handle("privateServers:getConfig", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(wrap(config(&ctx.state, id).await))
        })
        .handle("privateServers:update", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let id = props.get("privateServerId").and_then(Value::as_u64).unwrap_or(0);
            let body = json!({
                "name": props.get("name"), "active": props.get("active"),
                "newJoinCode": props.get("regenerateJoinCode").and_then(Value::as_bool).unwrap_or(false),
            });
            let state = &ctx.state;
            Ok(wrap(
                call(state, Method::PATCH, &vip(&id.to_string()), Some(&body), "Failed to update this private server.").await.map(|raw| normalize(&raw)),
            ))
        })
        .handle("privateServers:updateSubscription", |ctx, args| async move {
            let props: Value = args.get(0)?;
            Ok(wrap(update_subscription(&ctx.state, &props).await))
        })
        .handle("privateServers:updatePermissions", |ctx, args| async move {
            let props: Value = args.get(0)?;
            Ok(wrap(update_permissions(&ctx.state, &props).await))
        })
        .handle("privateServers:resolveShareLink", |ctx, args| async move {
            let code: String = args.get(0)?;
            Ok(wrap(resolve_share_link(&ctx.state, &code).await))
        })
        .handle("privateServers:canInvite", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            let url = format!("{}/vip-server/can-invite/{user}", endpoints::GAMES_V1);
            let result = call(&ctx.state, Method::GET, &url, None, "Failed to check whether this player can be invited.").await.map(|r| {
                json!({
                    "canInvite": r.get("canInvite"),
                    "restrictedByPrivacy": r.get("doesOwnerPrivacyRestrictJoins").and_then(Value::as_bool).unwrap_or(false),
                    "inviteResponseType": r.get("inviteResponseType").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
                })
            });
            Ok(wrap(result))
        })
        .handle("privateServers:getCreatePricing", |ctx, args| async move {
            let place: u64 = args.get(0)?;
            Ok(wrap(Ok(pricing(&ctx.state, place).await)))
        })
        .handle("privateServers:create", |ctx, args| async move {
            let props: Value = args.get(0)?;
            Ok(wrap(create(&ctx.state, &props).await))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_codes_and_reasons() {
        assert_eq!(link_code(Some("https://www.roblox.com/share?code=abc&type=Server")), json!("abc"));
        assert_eq!(link_code(Some("https://www.roblox.com/games/1?privateServerLinkCode=xyz")), json!("xyz"));
        assert_eq!(known_reason(400, Some(32)), Some("insufficientRobux"));
        assert_eq!(known_reason(403, None), Some("notOwner"));
    }
}
