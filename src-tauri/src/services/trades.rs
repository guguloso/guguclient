//! Limited item trading (`trades:*`). Port of `TradesService` + controller.

use serde_json::{json, Value};

use crate::ipc::Router;
use crate::state::AppState;

const TRADES: &str = "https://trades.roblox.com";
const RATIO_PRECISION: f64 = 10_000.0;

fn eligibility_reason(key: &str) -> &'static str {
    match key {
        "Eligible" => "",
        "IneligibleTradeSystemDisabled" => "The trading system is currently disabled.",
        "IneligibleCannotTradeWithRoblox" => "You cannot trade with Roblox accounts.",
        "IneligibleUserNotFound" => "User not found.",
        "IneligibleMissingPremiumMembership" => "You need Roblox Premium or Roblox Plus to trade with other players.",
        "IneligibleLegalOrRegulatoryRestrictions" => "Trading is restricted in your region due to legal or regulatory requirements.",
        _ => "Unable to determine trade eligibility.",
    }
}

fn mutual_reason(key: &str) -> &'static str {
    match key {
        "Eligible" => "",
        "CallingUserIneligible" => "You are not eligible to trade.",
        "TargetUserIneligible" => "This user is not eligible to trade.",
        "CannotTradeWithSelf" => "You cannot trade with yourself.",
        "CallingUserPrivacySettingsRestricted" => "Your privacy settings prevent trading with this user.",
        _ => "Unable to determine trade eligibility.",
    }
}

fn or<'a>(v: Option<&'a Value>, default: Value) -> Value {
    v.cloned().filter(|v| !v.is_null()).unwrap_or(default)
}

fn map_user(u: Option<&Value>) -> Value {
    let u = u.cloned().unwrap_or(Value::Null);
    json!({ "id": or(u.get("id"), json!(0)), "name": or(u.get("name"), json!("")), "displayName": or(u.get("displayName"), json!("")) })
}

fn map_target(t: Option<&Value>) -> Value {
    let t = t.cloned().unwrap_or(Value::Null);
    json!({ "itemType": or(t.get("itemType"), json!("Unknown")), "targetId": or(t.get("targetId"), json!("")) })
}

fn map_instance(i: &Value) -> Value {
    json!({
        "collectibleItemInstanceId": or(i.get("collectibleItemInstanceId"), json!("")),
        "itemName": or(i.get("itemName"), json!("")),
        "serialNumber": or(i.get("serialNumber"), Value::Null),
        "originalPrice": or(i.get("originalPrice"), Value::Null),
        "recentAveragePrice": or(i.get("recentAveragePrice"), Value::Null),
        "assetStock": or(i.get("assetStock"), Value::Null),
        "isOnHold": i.get("isOnHold").and_then(Value::as_bool).unwrap_or(false),
        "itemTarget": map_target(i.get("itemTarget")),
    })
}

fn map_offer(o: Option<&Value>) -> Value {
    let o = o.cloned().unwrap_or(Value::Null);
    json!({
        "user": map_user(o.get("user")),
        "robux": or(o.get("robux"), json!(0)),
        "items": o.get("items").and_then(Value::as_array).into_iter().flatten().map(map_instance).collect::<Vec<_>>(),
    })
}

fn offer_body(request: &Value) -> Value {
    let side = |k: &str| {
        let o = request.get(k).cloned().unwrap_or(Value::Null);
        json!({ "userId": o.get("userId"), "robux": o.get("robux"), "collectibleItemInstanceIds": o.get("collectibleItemInstanceIds") })
    };
    json!({ "senderOffer": side("senderOffer"), "recipientOffer": side("recipientOffer") })
}

fn round_ratio(v: f64) -> f64 {
    (v * RATIO_PRECISION).round() / RATIO_PRECISION
}

async fn get(state: &AppState, path: &str) -> crate::error::AppResult<Value> {
    state.roblox.get(&format!("{TRADES}{path}")).await
}

pub fn register(router: &mut Router) {
    router
        .handle("trades:checkEligibility", |ctx, _| async move {
            Ok(match get(&ctx.state, "/v2/users/me/can-trade").await {
                Ok(r) => json!({
                    "canTrade": r.get("canTrade").and_then(Value::as_bool).unwrap_or(false),
                    "reason": eligibility_reason(r.get("tradeEligibility").and_then(Value::as_str).unwrap_or("Unknown")),
                }),
                Err(error) => {
                    tracing::error!("Failed to check trade eligibility: {error}");
                    json!({ "canTrade": false, "reason": "Failed to check trade eligibility." })
                }
            })
        })
        .handle("trades:getTradeList", |ctx, args| async move {
            let status: String = args.get(0)?;
            let cursor: Option<String> = args.opt(1)?;
            if !["Inbound", "Outbound", "Completed", "Inactive"].contains(&status.as_str()) {
                tracing::error!("Failed to get trade list for {status}");
                return Ok(json!({ "ok": false }));
            }
            let mut path = format!("/v1/trades/{status}?limit=25&sortOrder=Desc");
            if let Some(c) = cursor.filter(|c| !c.is_empty()) {
                path.push_str(&format!("&cursor={}", urlencoding::encode(&c)));
            }
            Ok(match get(&ctx.state, &path).await {
                Ok(r) => json!({
                    "ok": true,
                    "page": {
                        "previousCursor": or(r.get("previousPageCursor"), Value::Null),
                        "nextCursor": or(r.get("nextPageCursor"), Value::Null),
                        "data": r.get("data").and_then(Value::as_array).into_iter().flatten().map(|t| json!({
                            "id": or(t.get("id"), json!(0)),
                            "user": map_user(t.get("user")),
                            "created": or(t.get("created"), json!("")),
                            "expiration": or(t.get("expiration"), json!("")),
                            "status": or(t.get("status"), json!("Unknown")),
                            "isActive": t.get("isActive").and_then(Value::as_bool).unwrap_or(false),
                        })).collect::<Vec<_>>(),
                    },
                }),
                Err(error) => {
                    tracing::error!("Failed to get trade list for {status}: {error}");
                    json!({ "ok": false })
                }
            })
        })
        .handle("trades:getTradeCount", |ctx, args| async move {
            let status: String = args.get(0)?;
            if status != "Inbound" {
                return Ok(0);
            }
            Ok(match get(&ctx.state, "/v1/trades/Inbound/count").await {
                Ok(r) => r.get("count").and_then(Value::as_u64).unwrap_or(0),
                Err(error) => {
                    tracing::error!("Failed to get trade count for {status}: {error}");
                    0
                }
            })
        })
        .handle("trades:getTradeDetails", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(match get(&ctx.state, &format!("/v2/trades/{id}")).await {
                Ok(r) => json!({
                    "tradeId": or(r.get("tradeId"), json!(id)),
                    "status": or(r.get("status"), json!("Unknown")),
                    "senderOffer": map_offer(r.get("participantAOffer")),
                    "receiverOffer": map_offer(r.get("participantBOffer")),
                }),
                Err(error) => {
                    tracing::error!("Failed to get trade details for {id}: {error}");
                    Value::Null
                }
            })
        })
        .handle("trades:getMetadata", |ctx, _| async move {
            Ok(match get(&ctx.state, "/v1/trades/metadata").await {
                Ok(r) => json!({
                    "maxItemsPerSide": or(r.get("maxItemsPerSide"), json!(4)),
                    "minValueRatio": or(r.get("minValueRatio"), json!(0)),
                    "tradeSystemMaxRobuxPercent": round_ratio(r.get("tradeSystemMaxRobuxPercent").and_then(Value::as_f64).unwrap_or(0.5)),
                    "tradeSystemRobuxFee": round_ratio(r.get("tradeSystemRobuxFee").and_then(Value::as_f64).unwrap_or(0.3)),
                }),
                Err(error) => {
                    tracing::error!("Failed to get trade metadata: {error}");
                    json!({ "maxItemsPerSide": 4, "minValueRatio": 0, "tradeSystemMaxRobuxPercent": 0.5, "tradeSystemRobuxFee": 0.3 })
                }
            })
        })
        .handle("trades:getTradableItems", |ctx, args| async move {
            let q: Value = args.get(0)?;
            let user = q.get("userId").cloned().unwrap_or(Value::Null);
            let mut params = vec![format!("limit={}", q.get("limit").and_then(Value::as_u64).unwrap_or(25))];
            for key in ["sortBy", "sortOrder", "search", "cursor"] {
                if let Some(v) = q.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    params.push(format!("{key}={}", urlencoding::encode(v)));
                }
            }
            let path = format!("/v2/users/{user}/tradableItems?{}", params.join("&"));
            Ok(match get(&ctx.state, &path).await {
                Ok(r) => json!({
                    "userId": or(r.get("userId"), user.clone()),
                    "items": r.get("items").and_then(Value::as_array).into_iter().flatten().map(|i| json!({
                        "collectibleItemId": or(i.get("collectibleItemId"), json!("")),
                        "itemName": or(i.get("itemName"), json!("")),
                        "originalPrice": or(i.get("originalPrice"), Value::Null),
                        "recentAveragePrice": or(i.get("recentAveragePrice"), Value::Null),
                        "assetStock": or(i.get("assetStock"), Value::Null),
                        "itemTarget": map_target(i.get("itemTarget")),
                        "instances": i.get("instances").and_then(Value::as_array).into_iter().flatten().map(map_instance).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                    "nextPageCursor": or(r.get("nextPageCursor"), Value::Null),
                }),
                Err(error) => {
                    tracing::error!("Failed to get tradable items for user {user}: {error}");
                    json!({ "userId": user, "items": [], "nextPageCursor": null })
                }
            })
        })
        .handle("trades:checkCanTradeWith", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            Ok(match get(&ctx.state, &format!("/v2/users/{user}/can-trade-with")).await {
                Ok(r) => {
                    let eligibility = r.get("mutualTradeEligibility").and_then(Value::as_str).unwrap_or("Unknown");
                    let can = r.get("canTrade").and_then(Value::as_bool).unwrap_or(false);
                    let blocked = match eligibility {
                        "CallingUserIneligible" | "CallingUserPrivacySettingsRestricted" | "CannotTradeWithSelf" => "viewer",
                        "TargetUserIneligible" => "target",
                        _ => "unknown",
                    };
                    let mut out = json!({ "canTrade": can, "reason": mutual_reason(eligibility) });
                    if !can {
                        out["blockedBy"] = json!(blocked);
                    }
                    out
                }
                Err(error) => {
                    tracing::error!("Failed to check trade eligibility with user {user}: {error}");
                    json!({ "blockedBy": "unknown", "canTrade": false, "reason": "Failed to check trade eligibility." })
                }
            })
        })
        .handle("trades:sendTrade", |ctx, args| async move {
            let request: Value = args.get(0)?;
            let r: Value = ctx.state.roblox.post(&format!("{TRADES}/v2/trades/send"), &offer_body(&request)).await?;
            Ok(or(r.get("tradeId"), json!(0)))
        })
        .handle("trades:counterTrade", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let request: Value = args.get(1)?;
            let r: Value = ctx.state.roblox.post(&format!("{TRADES}/v2/trades/{id}/counter"), &offer_body(&request)).await?;
            Ok(or(r.get("tradeId"), json!(0)))
        })
        .handle("trades:acceptTrade", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            ctx.state.roblox.post_empty::<Value>(&format!("{TRADES}/v1/trades/{id}/accept")).await?;
            tracing::info!("Accepted trade {id}");
            ctx.state.events.emit_main("auth:robuxSpent", Value::Null);
            Ok(Value::Null)
        })
        .handle("trades:declineTrade", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            ctx.state.roblox.post_empty::<Value>(&format!("{TRADES}/v1/trades/{id}/decline")).await?;
            tracing::info!("Declined trade {id}");
            Ok(Value::Null)
        });
}
