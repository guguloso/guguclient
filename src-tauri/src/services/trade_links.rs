//! Trade offer links waiting on a friendship or saved for later
//! (`tradeLinks:*`). Port of `TradeLinksService`; one file per account at
//! `trade-links/pending-<userId>.json` (`{ pending: [...] }`).

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::ipc::Router;
use crate::services::instances::parse_iso_millis;
use crate::services::roblox_layout::{read_json, write_json};
use crate::state::AppState;
use crate::util::{iso8601_from_millis, now_iso, now_millis};

const MAX_PENDING: usize = 500;
const MAX_NAME_LENGTH: usize = 64;
const MAX_ITEMS_PER_SIDE: usize = 4;
const MAX_TRADE_ROBUX: u64 = 2_147_483_647;
const REASONS: [&str; 2] = ["friendRequest", "saved"];

fn path(state: &AppState, user_id: u64) -> PathBuf {
    state.paths.base.join("trade-links").join(format!("pending-{user_id}.json"))
}

fn is_iso(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).and_then(parse_iso_millis).is_some()
}

/// `tradeOfferLinkSchema` + `validateTradeOfferLink`.
fn valid_link(link: &Value) -> bool {
    let ids = |k: &str| -> Option<Vec<u64>> {
        let list = link.get(k)?.as_array()?;
        let ids: Option<Vec<u64>> = list.iter().map(|v| v.as_u64().filter(|id| *id > 0)).collect();
        ids.filter(|ids| ids.len() <= MAX_ITEMS_PER_SIDE && ids.iter().collect::<HashSet<_>>().len() == ids.len())
    };
    let robux = |k: &str| link.get(k).and_then(Value::as_u64).is_some_and(|r| r <= MAX_TRADE_ROBUX);
    link.get("version").and_then(Value::as_u64) == Some(1)
        && link.get("fromUserId").and_then(Value::as_u64).is_some_and(|id| id > 0)
        && ids("offerAssetIds").is_some_and(|ids| !ids.is_empty())
        && ids("wantAssetIds").is_some()
        && robux("offerRobux")
        && robux("wantRobux")
}

fn valid_name(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| s.chars().count() <= MAX_NAME_LENGTH)
}

/// `pendingTradeLinkSchema`.
fn valid_record(r: &Value) -> bool {
    is_iso(r.get("createdAt"))
        && r.get("id").is_some_and(Value::is_string)
        && r.get("link").is_some_and(valid_link)
        && (r.get("notifiedAt").is_none() || is_iso(r.get("notifiedAt")))
        && r.get("reason").and_then(Value::as_str).is_some_and(|x| REASONS.contains(&x))
        && valid_name(r.get("targetDisplayName"))
        && valid_name(r.get("targetName"))
}

fn read_pending(state: &AppState, user_id: u64) -> Vec<Value> {
    let records = read_json(&path(state, user_id))
        .and_then(|f| f.get("pending").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let valid: Vec<Value> = records.iter().filter(|r| valid_record(r)).cloned().collect();
    if valid.len() != records.len() {
        tracing::warn!("Ignoring {} malformed pending trade link(s) for user {user_id}", records.len() - valid.len());
    }
    valid
}

fn write_pending(state: &AppState, user_id: u64, pending: &[Value]) -> bool {
    if let Err(error) = write_json(&path(state, user_id), &json!({ "pending": pending })) {
        tracing::warn!("Failed to write pending trade links for user {user_id}: {error}");
        return false;
    }
    state.events.emit_main("tradeLinks:changed", Value::Null);
    true
}

fn from_user(r: &Value) -> Option<u64> {
    r.pointer("/link/fromUserId").and_then(Value::as_u64)
}

fn save_pending(state: &AppState, mut input: serde_json::Map<String, Value>) -> Value {
    let account = input.remove("accountId").and_then(|v| v.as_u64()).unwrap_or(0);
    if !state.auth.has_account(account) {
        tracing::warn!("Refused to save a pending trade link for unknown account {account}");
        return Value::Null;
    }
    let created_raw = input.remove("createdAt");
    let now = now_millis();
    let created_at = created_raw
        .as_ref()
        .and_then(Value::as_str)
        .and_then(parse_iso_millis)
        .filter(|t| *t <= now)
        .map_or_else(now_iso, iso8601_from_millis);
    input.insert("createdAt".into(), json!(created_at));
    input.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
    input.remove("notifiedAt").filter(|n| is_iso(Some(n))).map(|n| input.insert("notifiedAt".into(), n));
    let record = Value::Object(input);
    if !valid_record(&record) {
        tracing::warn!("Refused to save a malformed pending trade link");
        return Value::Null;
    }
    let author = from_user(&record);
    let mut next: Vec<Value> = read_pending(state, account).into_iter().filter(|r| from_user(r) != author).collect();
    next.push(record.clone());
    if next.len() > MAX_PENDING {
        next.drain(..next.len() - MAX_PENDING);
    }
    if write_pending(state, account, &next) {
        record
    } else {
        Value::Null
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("tradeLinks:listPending", |ctx, _| async move {
            Ok(ctx.state.auth.current_user_id().map(|id| read_pending(&ctx.state, id)).unwrap_or_default())
        })
        .handle("tradeLinks:savePending", |ctx, args| async move {
            let props: serde_json::Map<String, Value> = args.get(0)?;
            Ok(save_pending(&ctx.state, props))
        })
        .handle("tradeLinks:removePending", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let account = props.get("accountId").and_then(Value::as_u64).unwrap_or(0);
            let author = props.get("authorUserId").and_then(Value::as_u64);
            if !ctx.state.auth.has_account(account) {
                return Ok(Vec::new());
            }
            let current = read_pending(&ctx.state, account);
            let next: Vec<Value> = current.iter().filter(|r| from_user(r) != author).cloned().collect();
            if next.len() != current.len() {
                write_pending(&ctx.state, account, &next);
            }
            Ok(next)
        })
        .handle("tradeLinks:markNotified", |ctx, args| async move {
            let props: Value = args.get(0)?;
            let account = props.get("accountId").and_then(Value::as_u64).unwrap_or(0);
            if !ctx.state.auth.has_account(account) {
                return Ok(false);
            }
            let Some(ids) = props.get("ids").and_then(Value::as_array).filter(|ids| ids.len() <= MAX_PENDING && ids.iter().all(Value::is_string))
            else {
                tracing::warn!("Refused malformed pending trade link ids");
                return Ok(false);
            };
            let wanted: HashSet<&str> = ids.iter().filter_map(Value::as_str).collect();
            let notified_at = now_iso();
            let mut changed = false;
            let next: Vec<Value> = read_pending(&ctx.state, account)
                .into_iter()
                .map(|mut r| {
                    let id = r.get("id").and_then(Value::as_str).unwrap_or("");
                    if wanted.contains(id) && r.get("notifiedAt").is_none() {
                        r["notifiedAt"] = json!(notified_at);
                        changed = true;
                    }
                    r
                })
                .collect();
            Ok(if changed { write_pending(&ctx.state, account, &next) } else { true })
        });
}
