//! Robux purchases (`catalog:purchaseItem`, `catalog:checkoutCart`). Port of
//! `CatalogPurchaser`. A reply that is lost, a 5xx or an unexpected shape is
//! reported as `outcomeUnknown` and refreshes the Robux balance, since Roblox
//! may still have charged.

use reqwest::Method;
use serde_json::{json, Value};

use crate::ipc::Router;
use crate::state::AppState;

const PURCHASES_FLAG: &str = "feature-marketplace-purchases";
const MARKETPLACE_SALES_V1: &str = "https://apis.roblox.com/marketplace-sales/v1";
const CLOUD_V2: &str = "https://apis.roblox.com/cloud/v2";
const ROBUX_CURRENCY_ID: u64 = 1;
const CHECKOUT_LINE_SUCCEEDED: &str = "SUCCEEDED";

/// `notifyRobuxSpent`.
fn notify_robux_spent(state: &AppState) {
    state.events.emit_main("auth:robuxSpent", Value::Null);
}

/// `reportPurchaseFailure` (Sentry breadcrumbs/scopes become log lines).
fn report_failure(surface: &str, reason: &str, context: &Value, status: Option<u16>, roblox_code: Option<i64>, expected: bool) {
    let summary = format!("Purchase failed ({surface}): {reason}");
    if expected {
        tracing::warn!(?status, ?roblox_code, %context, "{summary}");
    } else {
        tracing::error!(?status, ?roblox_code, %context, "{summary}");
    }
}

fn outcome_unknown(surface: &str, reason: &str, context: &Value, status: Option<u16>, roblox_code: Option<i64>) -> Value {
    report_failure(surface, reason, context, status, roblox_code, false);
    json!({ "ok": false, "error": { "kind": "outcomeUnknown" } })
}

fn purchases_enabled(state: &AppState) -> bool {
    if state.feature_flags.is_enabled(PURCHASES_FLAG) {
        return true;
    }
    tracing::info!("Purchase blocked while {PURCHASES_FLAG} is off");
    false
}

/// `failedReply`: a non-2xx answer.
async fn failed_reply(state: &AppState, surface: &str, context: &Value, response: reqwest::Response) -> Value {
    let status = response.status().as_u16();
    let body: Value = serde_json::from_str(&response.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    let error = body.pointer("/errors/0");
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_i64);
    if status >= 500 {
        notify_robux_spent(state);
        return outcome_unknown(surface, "Roblox failed with a server error", context, Some(status), code);
    }
    report_failure(surface, "Roblox rejected the request", context, Some(status), code, matches!(status, 401 | 429));
    let message = error.and_then(|e| e.get("userFacingMessage").or(e.get("message"))).and_then(Value::as_str).filter(|m| !m.is_empty());
    json!({ "ok": false, "error": { "kind": "requestFailed", "message": message } })
}

async fn send(state: &AppState, url: &str, body: &Value) -> Result<reqwest::Response, String> {
    if !state.auth.is_authenticated() {
        return Err("Not authenticated".into());
    }
    state.roblox.request(Method::POST, url, Some(body)).await.map_err(|e| e.to_string())
}

async fn purchase_item(state: &AppState, args: &Value) -> Value {
    if !purchases_enabled(state) {
        return json!({ "ok": false, "error": { "kind": "purchasesDisabled" } });
    }
    let surface = "item";
    let seller_type = args.get("expectedSellerType").filter(|v| !v.is_null()).cloned().unwrap_or(json!("User"));
    let context = json!({
        "collectibleItemId": args.get("collectibleItemId"), "collectibleProductId": args.get("collectibleProductId"),
        "expectedPrice": args.get("expectedPrice"), "expectedSellerType": seller_type,
    });
    let item = args.get("collectibleItemId").and_then(Value::as_str).unwrap_or("");
    let purchaser = match args.get("expectedPurchaserId") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    };
    let body = json!({
        "collectibleItemId": item,
        "collectibleProductId": args.get("collectibleProductId"),
        "expectedCurrency": ROBUX_CURRENCY_ID,
        "expectedPrice": args.get("expectedPrice"),
        "expectedPurchaserId": purchaser,
        "expectedPurchaserType": "User",
        "expectedSellerId": args.get("expectedSellerId"),
        "expectedSellerType": seller_type,
        "idempotencyKey": uuid::Uuid::new_v4().to_string(),
        "rentalOptionDays": null,
    });
    let url = format!("{MARKETPLACE_SALES_V1}/item/{}/purchase-item", urlencoding::encode(item));
    let response = match send(state, &url, &body).await {
        Ok(r) => r,
        Err(error) => {
            tracing::error!("Purchase request did not complete: {error}");
            return outcome_unknown(surface, "request did not complete", &context, None, None);
        }
    };
    if !response.status().is_success() {
        return failed_reply(state, surface, &context, response).await;
    }
    let status = response.status().as_u16();
    let raw: Value = response.json().await.unwrap_or(Value::Null);
    let shape_ok = raw.get("purchased").is_some_and(Value::is_boolean)
        && raw.get("purchaseResult").is_none_or(Value::is_string)
        && raw.get("pending").is_none_or(Value::is_boolean)
        && raw.get("errorMessage").is_none_or(|m| m.is_null() || m.is_string());
    if !shape_ok {
        notify_robux_spent(state);
        return outcome_unknown(surface, "reply did not match the expected shape", &context, None, None);
    }
    let purchased = raw["purchased"].as_bool().unwrap_or(false);
    let pending = raw.get("pending").and_then(Value::as_bool).unwrap_or(false);
    if purchased || pending {
        notify_robux_spent(state);
    } else {
        let mut context = context.clone();
        context["errorMessage"] = raw.get("errorMessage").cloned().unwrap_or(Value::Null);
        context["purchaseResult"] = raw.get("purchaseResult").cloned().unwrap_or(Value::Null);
        report_failure(surface, "Roblox declined the purchase", &context, Some(status), None, true);
    }
    json!({ "ok": true, "value": { "purchased": purchased, "pending": pending, "errorMessage": raw.get("errorMessage").cloned().unwrap_or(Value::Null) } })
}

fn valid_order(order: &Value) -> bool {
    let line_ok = |l: &Value| {
        l.get("collectibleProductId").is_some_and(Value::is_string)
            && l.get("agreedPriceRobux").is_some_and(Value::is_number)
            && l.get("assetId").is_none_or(Value::is_string)
            && l.get("status").is_some_and(Value::is_string)
            && l.get("errorReason").is_none_or(Value::is_string)
    };
    let group_ok = |g: &Value| {
        g.get("lineItems").and_then(Value::as_array).is_some_and(|l| l.iter().all(line_ok))
            && g.get("status").is_some_and(Value::is_string)
            && g.get("strategy").is_none_or(Value::is_string)
    };
    let receipt_ok = order
        .get("receipt")
        .is_none_or(|r| r.get("purchasedItemsCount").is_some_and(Value::is_number) && r.get("totalSpentRobux").is_some_and(Value::is_number));
    order.get("fulfillmentGroups").and_then(Value::as_array).is_some_and(|g| g.iter().all(group_ok))
        && order.get("status").is_some_and(Value::is_string)
        && receipt_ok
}

async fn checkout_cart(state: &AppState, args: &Value) -> Value {
    if !purchases_enabled(state) {
        return json!({ "ok": false, "error": { "kind": "purchasesDisabled" } });
    }
    let surface = "checkout";
    let lines: Vec<Value> = args.get("lineItems").and_then(Value::as_array).cloned().unwrap_or_default();
    let total: f64 = lines.iter().filter_map(|l| l.get("agreedPriceRobux").and_then(Value::as_f64)).sum();
    let context = json!({ "lineCount": lines.len(), "totalRobux": total });
    let url = format!("{CLOUD_V2}/avatar-marketplace-orders?idempotencyKey.key={}", uuid::Uuid::new_v4());
    let user = match args.get("purchasingUserId") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    };
    let body = json!({
        "context": { "productSurface": "SHOPPING_CART_WEB" },
        "fulfillmentGroups": [{
            "strategy": "BEST_EFFORT",
            "lineItems": lines.iter().map(|l| json!({ "collectibleProductId": l.get("collectibleProductId"), "agreedPriceRobux": l.get("agreedPriceRobux") })).collect::<Vec<_>>(),
        }],
        "purchasingUser": format!("users/{user}"),
    });
    let response = match send(state, &url, &body).await {
        Ok(r) => r,
        Err(error) => {
            tracing::error!("Checkout request did not complete: {error}");
            return outcome_unknown(surface, "request did not complete", &context, None, None);
        }
    };
    if !response.status().is_success() {
        return failed_reply(state, surface, &context, response).await;
    }
    let status = response.status().as_u16();
    let order: Value = response.json().await.unwrap_or(Value::Null);
    if !valid_order(&order) {
        notify_robux_spent(state);
        return outcome_unknown(surface, "reply did not match the expected shape", &context, None, None);
    }
    let line_items: Vec<Value> = order["fulfillmentGroups"].as_array().into_iter().flatten().flat_map(|g| g["lineItems"].as_array().cloned().unwrap_or_default()).collect();
    let purchased = order.pointer("/receipt/purchasedItemsCount").cloned().unwrap_or(json!(0));
    if purchased.as_f64().unwrap_or(0.0) > 0.0 {
        notify_robux_spent(state);
    }
    let failed: Vec<&Value> = line_items.iter().filter(|l| l["status"] != CHECKOUT_LINE_SUCCEEDED).collect();
    if !failed.is_empty() {
        let mut reasons: Vec<String> = Vec::new();
        for l in &failed {
            let reason = l.get("errorReason").or(l.get("status")).and_then(Value::as_str).unwrap_or("").to_owned();
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
        let mut context = context.clone();
        context["failedCount"] = json!(failed.len());
        context["reasons"] = json!(reasons.join(", "));
        report_failure(surface, "some checkout lines were not bought", &context, Some(status), None, true);
    }
    let lines: Vec<Value> = line_items
        .iter()
        .map(|l| {
            let mut out = json!({ "collectibleProductId": l.get("collectibleProductId"), "agreedPriceRobux": l.get("agreedPriceRobux"), "status": l.get("status") });
            for key in ["assetId", "errorReason"] {
                if let Some(v) = l.get(key) {
                    out[key] = v.clone();
                }
            }
            out
        })
        .collect();
    json!({
        "ok": true,
        "value": {
            "status": order["status"], "lineItems": lines, "purchasedItemsCount": purchased,
            "totalSpentRobux": order.pointer("/receipt/totalSpentRobux").cloned().unwrap_or(json!(0)),
        },
    })
}

pub fn register(router: &mut Router) {
    router
        .handle("catalog:purchaseItem", |ctx, args| async move {
            let request: Value = args.get(0)?;
            Ok(purchase_item(&ctx.state, &request).await)
        })
        .handle("catalog:checkoutCart", |ctx, args| async move {
            let request: Value = args.get(0)?;
            Ok(checkout_cart(&ctx.state, &request).await)
        });
}
