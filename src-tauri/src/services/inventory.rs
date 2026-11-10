//! Profile inventory tab (`users:getInventoryCategories`, `getInventoryPage`).
//! Port of `UserInventoryService`: categories, asset/bundle/badge/game pass/
//! private server pages, and price hydration from catalog details.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::services::catalog::{raw_items_details, ItemRef};
use crate::state::AppState;

const PAGE_LIMITS: [u64; 4] = [10, 25, 50, 100];
const BADGE_CATEGORY_ID: i64 = 21;
const GAME_PASS_LABELS: [&str; 3] = ["gamepass", "gamepasses", "passes"];

fn status_of(error: &AppError) -> Option<u16> {
    match error {
        AppError::Api { status, .. } => Some(*status),
        _ => None,
    }
}

/// Maps 400 → empty page and 403 → "Inventory is private" like Electron.
fn page_error(error: AppError, what: &str) -> AppResult<Value> {
    match status_of(&error) {
        Some(400) => Ok(empty_page()),
        Some(403) => Err(AppError::msg("Inventory is private")),
        _ => {
            tracing::error!("Failed to fetch {what}: {error}");
            Err(error)
        }
    }
}

fn empty_page() -> Value {
    json!({ "previousCursor": null, "nextCursor": null, "data": [] })
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn compact(label: &str) -> String {
    label.to_lowercase().replace(' ', "")
}

fn private_servers_tab(labels: &[Option<&str>]) -> Option<&'static str> {
    labels.iter().flatten().find_map(|l| match compact(l).as_str() {
        "myprivateservers" | "privateservers" => Some("MyPrivateServers"),
        "otherprivateservers" => Some("OtherPrivateServers"),
        _ => None,
    })
}

fn is_game_pass(labels: &[Option<&str>]) -> bool {
    labels.iter().flatten().any(|l| GAME_PASS_LABELS.contains(&compact(l).as_str()))
}

fn item_source_type(value: Option<&Value>) -> &'static str {
    let normalized = match value {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    match normalized.as_str() {
        "assettype" | "asset" | "0" => "assetType",
        "bundle" | "1" => "bundleType",
        _ => "unsupported",
    }
}

fn first_non_null<'a>(a: Option<&'a str>, b: Option<&'a str>) -> &'a str {
    a.or(b).unwrap_or("")
}

/// `normalizeInventoryCategoryItem`.
fn normalize_item(item: &Value, category: &Value) -> Option<Value> {
    let name = first_non_null(s(item, "name"), s(item, "displayName"));
    let display = first_non_null(s(item, "displayName"), s(item, "name"));
    if name.is_empty() && display.is_empty() {
        return None;
    }
    let id = item.get("id").and_then(Value::as_i64).unwrap_or(0);
    let category_type = s(item, "categoryType").or(s(category, "categoryType")).unwrap_or("");
    let parent = first_non_null(s(category, "name"), s(category, "displayName"));
    let labels = [s(item, "name"), s(item, "displayName"), Some(category_type), s(item, "filter"), s(category, "name"), s(category, "displayName")];
    let source = if private_servers_tab(&labels).is_some() {
        "privateServers"
    } else if is_game_pass(&labels) {
        "gamePass"
    } else {
        item_source_type(item.get("type"))
    };
    let filter = s(item, "filter");
    let selection_key = [source, &id.to_string(), category_type, filter.unwrap_or(""), name, display, parent].join("|");
    Some(json!({
        "id": id,
        "name": name,
        "displayName": display,
        "parentCategoryName": s(category, "name").unwrap_or(""),
        "parentCategoryDisplayName": s(category, "displayName").unwrap_or(""),
        "selectionKey": selection_key,
        "filter": filter,
        "categoryType": category_type,
        "sourceType": source,
        "isSupported": source == "privateServers" || source == "gamePass" || (id > 0 && source != "unsupported"),
    }))
}

fn normalize_categories(categories: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = categories
        .iter()
        .filter_map(|category| {
            let items: Vec<Value> =
                category.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(|i| normalize_item(i, category)).collect();
            let name = first_non_null(s(category, "name"), s(category, "displayName"));
            let display = first_non_null(s(category, "displayName"), s(category, "name"));
            if items.is_empty() || (name.is_empty() && display.is_empty()) {
                return None;
            }
            Some(json!({ "name": name, "displayName": display, "categoryType": s(category, "categoryType").unwrap_or(""), "items": items }))
        })
        .collect();
    // localeCompare(sensitivity: "base"): case-insensitive ordering.
    let label = |c: &Value| {
        let d = s(c, "displayName").unwrap_or("");
        (if d.is_empty() { s(c, "name").unwrap_or("") } else { d }).to_lowercase()
    };
    out.sort_by_key(label);
    out
}

fn require_auth(state: &AppState) -> AppResult<()> {
    if state.auth.is_authenticated() {
        Ok(())
    } else {
        Err(AppError::msg("Not authenticated"))
    }
}

async fn categories(state: &AppState, user_id: u64) -> AppResult<Value> {
    require_auth(state)?;
    let url = format!("{}/users/{user_id}/categories", endpoints::INVENTORY_V1);
    match state.roblox.get::<Value>(&url).await {
        Ok(r) => Ok(Value::Array(normalize_categories(r.get("categories").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()))),
        Err(error) if status_of(&error) == Some(403) => Err(AppError::msg("Inventory is private")),
        Err(error) => {
            tracing::error!("Failed to fetch inventory categories for user {user_id}: {error}");
            Err(error)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageQuery {
    user_id: u64,
    category_item: Value,
    limit: Option<f64>,
    sort_order: Option<String>,
    cursor: Option<String>,
}

fn opt_put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value.filter(|v| !v.is_null()) {
        map.insert(key.into(), v.clone());
    }
}

fn entry(id: u64, name: &str, item_type: &str, thumb: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(id));
    m.insert("name".into(), json!(name));
    m.insert("itemType".into(), json!(item_type));
    m.insert("thumbnailType".into(), json!(thumb));
    m
}

fn cursors(r: &Value, data: Vec<Value>) -> Value {
    json!({
        "previousCursor": r.get("previousPageCursor").cloned().unwrap_or(Value::Null),
        "nextCursor": r.get("nextPageCursor").cloned().unwrap_or(Value::Null),
        "data": data,
    })
}

/// `hydrateInventoryEntries`: price, status, limited flags from details.
async fn hydrate(state: &AppState, entries: Vec<Map<String, Value>>) -> Vec<Value> {
    let refs: Vec<ItemRef> = entries
        .iter()
        .filter(|e| !matches!(e.get("thumbnailType").and_then(Value::as_str), Some("BadgeIcon" | "GamePass")))
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_u64).filter(|id| *id > 0)?;
            let kind = if e.get("itemType").and_then(Value::as_str) == Some("Bundle") { "Bundle" } else { "Asset" };
            Some(ItemRef { id, item_type: kind.into() })
        })
        .collect();
    if refs.is_empty() {
        return entries.into_iter().map(Value::Object).collect();
    }
    let details = match raw_items_details(state, &refs, true).await {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!("Failed to hydrate inventory entry details for {} item(s): {error}", entries.len());
            return entries.into_iter().map(Value::Object).collect();
        }
    };
    let by_key: std::collections::HashMap<String, &Value> = details
        .iter()
        .filter_map(|d| {
            let id = d.get("id").and_then(Value::as_u64).filter(|id| *id > 0)?;
            let kind = if s(d, "itemType") == Some("Bundle") { "Bundle" } else { "Asset" };
            Some((format!("{kind}:{id}"), d))
        })
        .collect();
    entries
        .into_iter()
        .map(|mut e| {
            let key = format!("{}:{}", e.get("itemType").and_then(Value::as_str).unwrap_or(""), e.get("id").map(Value::to_string).unwrap_or_default());
            let Some(detail) = by_key.get(&key) else { return Value::Object(e) };
            let num = |k: &str| detail.get(k).filter(|v| !v.is_null()).cloned();
            let off_sale = detail.get("isOffSale").and_then(Value::as_bool).unwrap_or(false);
            let collectible = e.get("collectibleItemId").is_some_and(|v| !v.is_null());
            let price = if collectible && detail.get("lowestResalePrice").is_some() {
                detail.get("lowestResalePrice").cloned().unwrap_or(Value::Null)
            } else if num("price") == Some(json!(0)) && off_sale {
                num("lowestPrice").unwrap_or(Value::Null)
            } else {
                num("price").or_else(|| num("lowestPrice")).unwrap_or(Value::Null)
            };
            let normalized = s(detail, "priceStatus").map(str::trim).filter(|s| !s.is_empty());
            let status = if collectible && price.is_null() {
                Some("Off Sale")
            } else if price == json!(0) && (off_sale || normalized.is_some_and(|n| n.eq_ignore_ascii_case("off sale"))) {
                Some("Off Sale")
            } else if price == json!(0) {
                Some("Free")
            } else if price.is_null() && off_sale {
                Some("Off Sale")
            } else {
                normalized
            };
            let restrictions: Vec<i64> =
                detail.get("itemRestrictions").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_i64).collect();
            let serial = e.get("serialNumber").and_then(Value::as_i64).unwrap_or(0);
            let limited = if restrictions.contains(&2) || serial > 0 {
                Some("limitedUnique")
            } else if restrictions.contains(&3) {
                Some("limited")
            } else {
                None
            };
            if let Some(name) = s(detail, "name") {
                e.insert("name".into(), json!(name));
            }
            if let Some(creator) = s(detail, "creatorName") {
                e.insert("creatorName".into(), json!(creator));
            }
            e.insert("price".into(), price);
            if let Some(status) = status {
                e.insert("priceStatus".into(), json!(status));
            }
            match limited {
                Some(l) => e.insert("limitedType".into(), json!(l)),
                None => e.remove("limitedType"),
            };
            Value::Object(e)
        })
        .collect()
}

fn is_badge_category(item: &Value) -> bool {
    if item.get("id").and_then(Value::as_i64) == Some(BADGE_CATEGORY_ID) {
        return true;
    }
    ["displayName", "name", "parentCategoryDisplayName", "parentCategoryName", "categoryType", "filter"].iter().any(|k| {
        s(item, k).is_some_and(|label| {
            label.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "badge" || w == "badges")
        })
    })
}

async fn page(state: &AppState, q: PageQuery) -> AppResult<Value> {
    require_auth(state)?;
    let limit = match q.limit {
        Some(l) if l.is_finite() => PAGE_LIMITS.into_iter().find(|a| (*a as f64) >= l).unwrap_or(100),
        _ => 50,
    };
    let sort = if q.sort_order.as_deref() == Some("Asc") { "Asc" } else { "Desc" };
    let cursor = q.cursor.filter(|c| !c.is_empty());
    let item = &q.category_item;
    let user = q.user_id;
    let cursor_param = |name: &str| cursor.as_ref().map(|c| format!("&{name}={}", urlencoding::encode(c))).unwrap_or_default();

    if is_badge_category(item) {
        let url = format!("{}/users/{user}/badges?limit={limit}&sortOrder={sort}{}", endpoints::BADGES_V1, cursor_param("cursor"));
        return match state.roblox.get::<Value>(&url).await {
            Ok(r) => {
                let data = r
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|b| {
                        let id = b.get("id").and_then(Value::as_u64).filter(|id| *id > 0)?;
                        let name = s(b, "displayName").or(s(b, "name")).unwrap_or("");
                        if name.is_empty() {
                            return None;
                        }
                        let mut e = entry(id, name, "Asset", "BadgeIcon");
                        e.insert("assetId".into(), json!(id));
                        opt_put(&mut e, "creatorName", b.pointer("/creator/name"));
                        e.insert("priceStatus".into(), json!("Badge"));
                        opt_put(&mut e, "created", b.get("created"));
                        opt_put(&mut e, "updated", b.get("updated"));
                        Some(Value::Object(e))
                    })
                    .collect();
                Ok(cursors(&r, data))
            }
            Err(error) => page_error(error, &format!("badge inventory page for user {user}")),
        };
    }

    let source = s(item, "sourceType").unwrap_or("");
    if source == "privateServers" {
        if Some(user) != state.auth.current_user_id() {
            return Ok(empty_page());
        }
        let labels = ["name", "displayName", "categoryType", "filter", "parentCategoryName", "parentCategoryDisplayName"].map(|k| s(item, k));
        let tab = private_servers_tab(&labels).unwrap_or("MyPrivateServers");
        let url = format!(
            "{}/private-servers/my-private-servers?privateServersTab={tab}&itemsPerPage={limit}{}",
            endpoints::GAMES_V1,
            cursor_param("cursor")
        );
        return match state.roblox.get::<Value>(&url).await {
            Ok(r) => {
                let data = r
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        let id = row.get("privateServerId").and_then(Value::as_u64).filter(|id| *id > 0)?;
                        let name = s(row, "name").filter(|n| !n.is_empty())?;
                        let mut e = entry(id, name, "Asset", "GameIcon");
                        opt_put(&mut e, "assetId", row.get("placeId"));
                        opt_put(&mut e, "universeId", row.get("universeId"));
                        opt_put(&mut e, "universeName", row.get("universeName"));
                        e.insert("price".into(), row.get("priceInRobux").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)));
                        Some(Value::Object(e))
                    })
                    .collect();
                Ok(cursors(&r, data))
            }
            Err(error) => page_error(error, &format!("private servers inventory page for user {user}")),
        };
    }
    if source == "gamePass" {
        let start = cursor.as_deref().and_then(|c| c.parse::<u64>().ok()).filter(|v| *v > 0);
        let url = format!(
            "https://apis.roblox.com/game-passes/v1/users/{user}/game-passes?count={limit}{}",
            start.map(|s| format!("&exclusiveStartId={s}")).unwrap_or_default()
        );
        return match state.roblox.get::<Value>(&url).await {
            Ok(r) => {
                let rows = r.get("gamePasses").and_then(Value::as_array).cloned().unwrap_or_default();
                let last = rows.last().and_then(|r| r.get("gamePassId")).and_then(Value::as_u64).unwrap_or(0);
                let data: Vec<Value> = rows
                    .iter()
                    .filter_map(|row| {
                        let id = row.get("gamePassId").and_then(Value::as_u64).filter(|id| *id > 0)?;
                        let name = s(row, "name").filter(|n| !n.is_empty())?;
                        let mut e = entry(id, name, "Asset", "GamePass");
                        opt_put(&mut e, "creatorName", row.pointer("/creator/name"));
                        let for_sale = row.get("isForSale").and_then(Value::as_bool) != Some(false);
                        e.insert("price".into(), if for_sale { row.get("price").cloned().unwrap_or(Value::Null) } else { Value::Null });
                        if !for_sale {
                            e.insert("priceStatus".into(), json!("Off Sale"));
                        }
                        Some(Value::Object(e))
                    })
                    .collect();
                let next = (rows.len() as u64 >= limit && last > 0).then(|| last.to_string());
                Ok(json!({ "previousCursor": null, "nextCursor": next, "data": data }))
            }
            Err(error) => page_error(error, &format!("game pass inventory page for user {user}")),
        };
    }

    let category_id = item.get("id").and_then(Value::as_i64).unwrap_or(0);
    if category_id <= 0 {
        return Ok(empty_page());
    }
    if source == "assetType" {
        let url = format!(
            "{}/users/{user}/inventory/{category_id}?limit={limit}&sortOrder={sort}{}",
            endpoints::INVENTORY_V2,
            cursor_param("cursor")
        );
        return match state.roblox.get::<Value>(&url).await {
            Ok(r) => {
                let entries: Vec<Map<String, Value>> = r
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|it| {
                        let id = it.get("assetId").and_then(Value::as_u64).filter(|id| *id > 0)?;
                        let name = s(it, "assetName").filter(|n| !n.is_empty())?;
                        let mut e = entry(id, name, "Asset", "Asset");
                        for (to, from) in [
                            ("userAssetId", "userAssetId"),
                            ("assetId", "assetId"),
                            ("collectibleItemId", "collectibleItemId"),
                            ("collectibleItemInstanceId", "collectibleItemInstanceId"),
                            ("serialNumber", "serialNumber"),
                            ("created", "created"),
                            ("updated", "updated"),
                        ] {
                            opt_put(&mut e, to, it.get(from));
                        }
                        Some(e)
                    })
                    .collect();
                let data = hydrate(state, entries).await;
                Ok(cursors(&r, data))
            }
            Err(error) => page_error(error, &format!("asset inventory page for user {user}, assetType {category_id}")),
        };
    }
    if source == "bundleType" {
        if !(1..=4).contains(&category_id) {
            return Ok(empty_page());
        }
        let url = format!(
            "{}/users/{user}/bundles/{category_id}?cursor={}&limit={limit}",
            endpoints::CATALOG_V1,
            urlencoding::encode(cursor.as_deref().unwrap_or(""))
        );
        return match state.roblox.get::<Value>(&url).await {
            Ok(r) => {
                let entries: Vec<Map<String, Value>> = r
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|b| {
                        let id = b.get("id").and_then(Value::as_u64).filter(|id| *id > 0)?;
                        let name = s(b, "name").filter(|n| !n.is_empty())?;
                        let mut e = entry(id, name, "Bundle", "BundleThumbnail");
                        opt_put(&mut e, "bundleType", b.get("bundleType"));
                        opt_put(&mut e, "creatorName", b.pointer("/creator/name"));
                        Some(e)
                    })
                    .collect();
                let data = hydrate(state, entries).await;
                Ok(cursors(&r, data))
            }
            Err(error) => page_error(error, &format!("bundle inventory page for user {user}, bundle category {category_id}")),
        };
    }
    Ok(empty_page())
}

pub fn register(router: &mut Router) {
    router
        .handle("users:getInventoryCategories", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            categories(&ctx.state, user_id).await
        })
        .handle("users:getInventoryPage", |ctx, args| async move {
            let query: PageQuery = args.get(0)?;
            page(&ctx.state, query).await
        });
}
