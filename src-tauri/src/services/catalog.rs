//! Avatar marketplace (`catalog:*`, `cart:*`). Port of `CatalogService`,
//! `CatalogItemDetailsFetcher`, `ItemDetailsCacheService` and `CartService`.
//!
//! Purchases live in `purchases.rs`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::future::join_all;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints::{self, join_ids};
use crate::services::metadata::gated_installation_id;
use crate::services::thumbnails::{IdQuery, ThumbPage};
use crate::state::AppState;

const NAVIGATION_TTL: Duration = Duration::from_secs(30 * 60);
const PROMOTED_PAGES_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const ITEM_DETAILS_TTL: Duration = Duration::from_secs(5 * 60);
const ITEM_DETAILS_MAX: usize = 500;
const VALID_LIMITS: [u64; 5] = [10, 28, 30, 60, 120];
const DETAILS_BATCH: usize = 120;
const THUMBNAIL_BATCH: usize = 100;
const RICH_DETAILS_CONCURRENCY: usize = 4;
const RICH_DETAILS_MAX_ITEMS: usize = 30;
const COLLECTIBLE_BATCH: usize = 30;
const CART_MAX_ITEMS: usize = 100;

const ITEM_RESTRICTIONS: [(u64, &str); 9] = [
    (1, "ThirteenPlus"),
    (2, "LimitedUnique"),
    (3, "Limited"),
    (4, "Rthro"),
    (5, "Live"),
    (6, "Collectible"),
    (7, "BuildersClub"),
    (8, "TurboBuildersClub"),
    (9, "OutrageousBuildersClub"),
];
const ITEM_STATUSES: [(u64, &str); 3] = [(1, "New"), (2, "Sale"), (7, "XboxExclusive")];

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub id: u64,
    pub item_type: String,
}

impl ItemRef {
    pub fn key(&self) -> String {
        format!("{}:{}", self.item_type, self.id)
    }
    fn is_bundle(&self) -> bool {
        self.item_type == "Bundle"
    }
}

#[derive(Default)]
pub struct CatalogState {
    navigation: tokio::sync::Mutex<Option<(Instant, Value)>>,
    promoted_pages: tokio::sync::Mutex<Option<(Instant, Value)>>,
    item_details: Mutex<HashMap<String, (Instant, Value)>>,
}

impl CatalogState {
    pub fn clear_cache(&self) {
        self.item_details.lock().clear();
    }
}

// ------------------------------------------------------------------ mapping

fn enum_names(raw: Option<&Value>, names: &[(u64, &str)]) -> Value {
    let list = raw
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Value::Array(
        list.iter()
            .filter_map(|v| match v {
                Value::Number(n) => {
                    let n = n.as_u64()?;
                    names
                        .iter()
                        .find(|(k, _)| *k == n)
                        .map(|(_, name)| Value::from(*name))
                }
                Value::String(s) => Some(Value::String(s.clone())),
                _ => None,
            })
            .collect(),
    )
}

fn or_null(raw: &Value, key: &str) -> Value {
    raw.get(key)
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(Value::Null)
}

fn or_default(raw: &Value, key: &str, default: Value) -> Value {
    raw.get(key)
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or(default)
}

/// Fields shared by `toCatalogItem` and `toCatalogItemFromItemDetails`.
fn base_item(raw: &Value) -> Map<String, Value> {
    let is_bundle = raw.get("itemType").and_then(Value::as_str) == Some("Bundle");
    let is_group = raw.get("creatorType").and_then(Value::as_str) == Some("Group");
    let mut item = Map::new();
    let mut put = |k: &str, v: Value| {
        item.insert(k.to_owned(), v);
    };
    put("id", raw.get("id").cloned().unwrap_or(Value::Null));
    put(
        "itemType",
        Value::from(if is_bundle { "Bundle" } else { "Asset" }),
    );
    put("assetType", or_null(raw, "assetType"));
    put("bundleType", or_null(raw, "bundleType"));
    put("name", or_default(raw, "name", json!("")));
    put("description", or_default(raw, "description", json!("")));
    put("productId", or_null(raw, "productId"));
    put("collectibleItemId", or_null(raw, "collectibleItemId"));
    put(
        "creatorType",
        Value::from(if is_group { "Group" } else { "User" }),
    );
    put(
        "creatorTargetId",
        or_default(raw, "creatorTargetId", json!(0)),
    );
    put("creatorName", or_default(raw, "creatorName", json!("")));
    put(
        "creatorHasVerifiedBadge",
        Value::Bool(
            raw.get("creatorHasVerifiedBadge")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    put("price", or_null(raw, "price"));
    put("lowestPrice", or_null(raw, "lowestPrice"));
    put("lowestResalePrice", or_null(raw, "lowestResalePrice"));
    put("favoriteCount", or_default(raw, "favoriteCount", json!(0)));
    put(
        "unitsAvailableForConsumption",
        or_null(raw, "unitsAvailableForConsumption"),
    );
    put("totalQuantity", or_null(raw, "totalQuantity"));
    put(
        "itemRestrictions",
        enum_names(raw.get("itemRestrictions"), &ITEM_RESTRICTIONS),
    );
    put(
        "itemStatus",
        enum_names(raw.get("itemStatus"), &ITEM_STATUSES),
    );
    put(
        "hasResellers",
        Value::Bool(
            raw.get("hasResellers")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    put("saleLocationType", or_null(raw, "saleLocationType"));
    put("offSaleDeadline", or_null(raw, "offSaleDeadline"));
    put("taxonomy", or_default(raw, "taxonomy", json!([])));
    put("quantityLimitPerUser", or_null(raw, "quantityLimitPerUser"));
    // `undefined` in JS: present only when Roblox sent them.
    for key in ["isOffSale", "itemCreatedUtc"] {
        if let Some(v) = raw.get(key) {
            item.insert(key.to_owned(), v.clone());
        }
    }
    item
}

/// `toCatalogItem` (search results and batch details).
fn to_catalog_item(raw: &Value) -> Value {
    let mut item = base_item(raw);
    if let Some(v) = raw.get("timedOptions") {
        item.insert("timedOptions".into(), v.clone());
    }
    Value::Object(item)
}

/// `toCatalogItemFromItemDetails` (single item details endpoint).
fn to_catalog_item_from_details(raw: &Value) -> Value {
    let mut item = base_item(raw);
    item.insert("collectibleProductId".into(), Value::Null);
    item.insert("expectedSellerId".into(), or_null(raw, "expectedSellerId"));
    for key in ["isPBR", "isPurchasable", "owned"] {
        if let Some(v) = raw.get(key) {
            item.insert(key.to_owned(), v.clone());
        }
    }
    Value::Object(item)
}

fn apply_collectible(mut item: Value, collectibles: Option<&HashMap<String, Value>>) -> Value {
    let (Some(id), Some(collectibles)) = (
        item.get("collectibleItemId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        collectibles,
    ) else {
        return item;
    };
    let detail = collectibles.get(&id);
    item["collectibleProductId"] = detail
        .map(|d| or_null(d, "collectibleProductId"))
        .unwrap_or(Value::Null);
    item["resaleRestriction"] = detail
        .map(|d| or_null(d, "resaleRestriction"))
        .unwrap_or(Value::Null);
    item
}

/// `toMarketplaceNavigationMenu`.
fn to_navigation_menu(raw: &Value) -> Value {
    let list = |v: Option<&Value>| v.and_then(Value::as_array).cloned().unwrap_or_default();
    let ids = |v: &Value, key: &str| or_default(v, key, json!([]));
    let categories: Vec<Value> = list(raw.get("categories"))
        .iter()
        .map(|cat| {
            let subcategories: Vec<Value> = list(cat.get("subcategories"))
                .iter()
                .map(|sub| {
                    json!({
                        "name": sub.get("name"),
                        "shortName": or_null(sub, "shortName"),
                        "subcategory": or_null(sub, "subcategory"),
                        "taxonomy": sub.get("taxonomy"),
                        "subcategoryId": or_null(sub, "subcategoryId"),
                        "assetTypeIds": ids(sub, "assetTypeIds"),
                        "bundleTypeIds": ids(sub, "bundleTypeIds"),
                    })
                })
                .collect();
            json!({
                "name": cat.get("name"),
                "category": or_null(cat, "category"),
                "taxonomy": cat.get("taxonomy"),
                "categoryId": or_null(cat, "categoryId"),
                "orderIndex": cat.get("orderIndex"),
                "assetTypeIds": ids(cat, "assetTypeIds"),
                "bundleTypeIds": ids(cat, "bundleTypeIds"),
                "isSearchable": cat.get("isSearchable").and_then(Value::as_bool).unwrap_or(false),
                "subcategories": subcategories,
            })
        })
        .collect();
    let sort_menu = raw.get("sortMenu");
    json!({
        "categories": categories,
        "genres": list(raw.get("genres")).iter()
            .filter(|g| g.get("genre").is_some_and(Value::is_number))
            .map(|g| json!({ "genre": g.get("genre"), "name": g.get("name") }))
            .collect::<Vec<_>>(),
        "sortOptions": list(sort_menu.and_then(|s| s.get("sortOptions"))).iter()
            .map(|o| json!({
                "sortType": o.get("sortType"),
                "sortOrder": o.get("sortOrder"),
                "name": o.get("name"),
                "isPriceRelated": o.get("isPriceRelated").and_then(Value::as_bool).unwrap_or(false),
            }))
            .collect::<Vec<_>>(),
        "sortAggregations": list(sort_menu.and_then(|s| s.get("sortAggregations"))).iter()
            .map(|a| json!({ "sortAggregation": a.get("sortAggregation"), "name": a.get("name") }))
            .collect::<Vec<_>>(),
        "salesTypeFilters": list(raw.get("salesTypeFilters")).iter()
            .map(|f| json!({ "filter": f.get("filter"), "name": f.get("name") }))
            .collect::<Vec<_>>(),
        "defaults": {
            "categoryId": raw.get("defaultCategory"),
            "sortType": raw.get("defaultSortType"),
            "sortAggregation": raw.get("defaultSortAggregation"),
            "currency": raw.get("defaultCurrency"),
        },
    })
}

// ------------------------------------------------------------------ queries

async fn navigation_menu(ctx: &Ctx, force: bool) -> AppResult<Value> {
    let mut slot = ctx.state.catalog.navigation.lock().await;
    if !force {
        if let Some((_, menu)) = slot
            .as_ref()
            .filter(|(at, _)| at.elapsed() < NAVIGATION_TTL)
        {
            return Ok(menu.clone());
        }
    }
    let url = format!("{}/search/navigation-menu-items", endpoints::CATALOG_V1);
    let raw: Value = ctx.state.roblox.get(&url).await?;
    let menu = to_navigation_menu(&raw);
    *slot = Some((Instant::now(), menu.clone()));
    Ok(menu)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SearchQuery {
    keyword: Option<String>,
    taxonomy: Option<String>,
    asset_type_ids: Option<Vec<u64>>,
    bundle_type_ids: Option<Vec<u64>>,
    category_filter: Option<Value>,
    subcategory_id: Option<Value>,
    sort_type: Option<Value>,
    sort_aggregation: Option<Value>,
    min_price: Option<Value>,
    max_price: Option<Value>,
    sales_type_filter: Option<Value>,
    creator_type: Option<Value>,
    creator_target_id: Option<Value>,
    creator_name: Option<String>,
    include_not_for_sale: Option<Value>,
    topics: Option<Vec<String>>,
    limit: Option<u64>,
    cursor: Option<String>,
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// `normalizeCreatorName`: trimmed, upper-cased, spaces as `+`.
fn normalize_creator_name(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_uppercase().replace(' ', "+"))
}

async fn search_items(ctx: &Ctx, q: SearchQuery) -> AppResult<Value> {
    let limit = q.limit.filter(|l| VALID_LIMITS.contains(l)).unwrap_or(30);
    let mut params: Vec<(&str, String)> = Vec::new();
    let mut push = |k: &'static str, v: Option<String>| {
        if let Some(v) = v {
            params.push((k, v));
        }
    };
    push("Keyword", q.keyword.clone());
    push("Taxonomy", q.taxonomy.clone().filter(|t| !t.is_empty()));
    push(
        "CategoryFilter",
        q.category_filter.as_ref().and_then(scalar),
    );
    push("Subcategory", q.subcategory_id.as_ref().and_then(scalar));
    push("SortType", q.sort_type.as_ref().and_then(scalar));
    push(
        "SortAggregation",
        q.sort_aggregation.as_ref().and_then(scalar),
    );
    push("MinPrice", q.min_price.as_ref().and_then(scalar));
    push("MaxPrice", q.max_price.as_ref().and_then(scalar));
    push(
        "SalesTypeFilter",
        q.sales_type_filter.as_ref().and_then(scalar),
    );
    push("CreatorType", q.creator_type.as_ref().and_then(scalar));
    push(
        "CreatorTargetId",
        q.creator_target_id.as_ref().and_then(scalar),
    );
    push(
        "CreatorName",
        normalize_creator_name(q.creator_name.as_deref()),
    );
    push(
        "IncludeNotForSale",
        q.include_not_for_sale.as_ref().and_then(scalar),
    );
    if q.topics.is_some() {
        push("TriggeredByTopicDiscovery", Some("true".into()));
    }
    push("limit", Some(limit.to_string()));
    push("cursor", q.cursor.clone().filter(|c| !c.is_empty()));
    for (name, ids) in [
        ("AssetTypeIds", &q.asset_type_ids),
        ("BundleTypeIds", &q.bundle_type_ids),
    ] {
        for id in ids.iter().flatten() {
            params.push((name, id.to_string()));
        }
    }
    for topic in q.topics.iter().flatten() {
        params.push(("Topics", topic.clone()));
    }
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect();
    let url = format!(
        "{}/search/items/details?{}",
        endpoints::CATALOG_V2,
        query.join("&")
    );
    let raw: Value = ctx.state.roblox.get(&url).await?;
    let items: Vec<Value> = raw
        .get("data")
        .and_then(Value::as_array)
        .map(|d| d.iter().map(to_catalog_item).collect())
        .unwrap_or_default();
    Ok(json!({
        "keyword": or_null(&raw, "keyword"),
        "previousPageCursor": or_null(&raw, "previousPageCursor"),
        "nextPageCursor": or_null(&raw, "nextPageCursor"),
        "items": items,
    }))
}

/// `lookupItemsDetails` through the 5-minute item details cache.
async fn items_details(state: &AppState, items: &[ItemRef], strict: bool) -> AppResult<Vec<Value>> {
    Ok(raw_items_details(state, items, strict)
        .await?
        .iter()
        .map(to_catalog_item)
        .collect())
}

/// `itemDetailsCache.batchFetch` + `postCatalogItemsDetails`: raw entries.
pub async fn raw_items_details(
    state: &AppState,
    items: &[ItemRef],
    strict: bool,
) -> AppResult<Vec<Value>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut result: Vec<(String, Value)> = Vec::new();
    let mut missing = Vec::new();
    {
        let mut seen = HashSet::new();
        let cache = state.catalog.item_details.lock();
        for item in items {
            let key = item.key();
            if !seen.insert(key.clone()) {
                continue;
            }
            match cache
                .get(&key)
                .filter(|(at, _)| at.elapsed() < ITEM_DETAILS_TTL)
            {
                Some((_, v)) => result.push((key, v.clone())),
                None => missing.push(item.clone()),
            }
        }
    }
    let url = format!("{}/catalog/items/details", endpoints::CATALOG_V1);
    for batch in missing.chunks(DETAILS_BATCH) {
        let body = json!({ "items": batch.iter().map(|i| json!({ "id": i.id, "itemType": i.item_type })).collect::<Vec<_>>() });
        match state.roblox.post::<_, Value>(&url, &body).await {
            Ok(raw) => {
                let mut cache = state.catalog.item_details.lock();
                for entry in raw
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(id) = entry.get("id").and_then(Value::as_u64).filter(|id| *id > 0)
                    else {
                        continue;
                    };
                    let kind = if entry.get("itemType").and_then(Value::as_str) == Some("Bundle") {
                        "Bundle"
                    } else {
                        "Asset"
                    };
                    let key = format!("{kind}:{id}");
                    if cache.len() >= ITEM_DETAILS_MAX {
                        if let Some(oldest) = cache
                            .iter()
                            .min_by_key(|(_, (at, _))| *at)
                            .map(|(k, _)| k.clone())
                        {
                            cache.remove(&oldest);
                        }
                    }
                    cache.insert(key.clone(), (Instant::now(), entry.clone()));
                    if let Some(slot) = result.iter_mut().find(|(k, _)| *k == key) {
                        slot.1 = entry.clone();
                    } else {
                        result.push((key, entry.clone()));
                    }
                }
            }
            Err(error) if strict => return Err(error),
            Err(error) => tracing::warn!(%error, "Failed to fetch catalog item details batch"),
        }
    }
    Ok(result.into_iter().map(|(_, raw)| raw).collect())
}

enum Lookup {
    Found(Value),
    NotFound,
    Failed,
}

/// `fetchItemDetailsBase`: one item, distinguishing "gone" from "failed".
async fn item_details_base(state: &AppState, item: &ItemRef, cookie: Option<&str>) -> Lookup {
    let url = format!(
        "{}/catalog/items/{}/details?itemType={}",
        endpoints::CATALOG_V1,
        item.id,
        item.item_type
    );
    let response = match state.roblox.request_for(reqwest::Method::GET, &url, None, cookie).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch catalog item details");
            return Lookup::Failed;
        }
    };
    let status = response.status().as_u16();
    if response.status().is_success() {
        return match response.json::<Value>().await {
            Ok(raw) => Lookup::Found(to_catalog_item_from_details(&raw)),
            Err(error) => {
                tracing::warn!(%error, "Failed to parse catalog item details");
                Lookup::Failed
            }
        };
    }
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let code = body.pointer("/errors/0/code").and_then(Value::as_u64);
    tracing::warn!(
        status,
        ?code,
        "Catalog item details for {} {} were refused",
        item.item_type,
        item.id
    );
    if status == 404 || (status == 400 && matches!(code, Some(11) | Some(21))) {
        Lookup::NotFound
    } else {
        Lookup::Failed
    }
}

/// `fetchCollectibleDetails`: `None` when any batch failed.
async fn collectible_details(state: &AppState, ids: Vec<String>) -> Option<HashMap<String, Value>> {
    if ids.is_empty() {
        return Some(HashMap::new());
    }
    let url = format!("{}/items/details", endpoints::MARKETPLACE_ITEMS_V1);
    let pages = futures::future::try_join_all(ids.chunks(COLLECTIBLE_BATCH).map(|chunk| {
        let body = json!({ "itemIds": chunk });
        let url = &url;
        async move { state.roblox.post::<_, Vec<Value>>(url, &body).await }
    }))
    .await;
    match pages {
        Ok(pages) => Some(
            pages
                .into_iter()
                .flatten()
                .filter_map(|d| Some((d.get("collectibleItemId")?.as_str()?.to_owned(), d)))
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch collectible details");
            None
        }
    }
}

async fn item_details(state: &AppState, item: ItemRef) -> Option<Value> {
    let Lookup::Found(found) = item_details_base(state, &item, None).await else {
        return None;
    };
    let Some(collectible) = found
        .get("collectibleItemId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Some(found);
    };
    let collectibles = collectible_details(state, vec![collectible]).await;
    Some(apply_collectible(found, collectibles.as_ref()))
}

/// `getItemsDetailsRich`: up to 30 unique items, 4 at a time.
async fn items_details_rich(state: &AppState, items: Vec<ItemRef>) -> Vec<Value> {
    lookup_items_details_rich(state, items, None).await.0
}

/// `lookupItemsDetailsRich`: the resolved details plus the refs Roblox
/// reported as gone, acting as `as_user` (a stored account) when given.
pub async fn lookup_items_details_rich(state: &AppState, items: Vec<ItemRef>, as_user: Option<u64>) -> (Vec<Value>, Vec<ItemRef>) {
    let mut seen = HashSet::new();
    let refs: Vec<ItemRef> = items.into_iter().filter(|i| seen.insert(i.key())).take(RICH_DETAILS_MAX_ITEMS).collect();
    if refs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let user_id = state.auth.current_user_id();
    let scoped = as_user.filter(|u| Some(*u) != user_id);
    if let Some(user) = scoped {
        if state.auth.request_cookie_for_user(user).is_none() {
            tracing::warn!("No stored session for account {user}; skipping its item details");
            return (Vec::new(), Vec::new());
        }
    }
    let mut details = Vec::new();
    let mut not_found = Vec::new();
    for chunk in refs.chunks(RICH_DETAILS_CONCURRENCY) {
        let lookups = join_all(chunk.iter().map(|r| async move {
            match scoped {
                Some(user) => match state.auth.request_cookie_for_user(user) {
                    Some(cookie) => item_details_base(state, r, Some(&cookie)).await,
                    None => Lookup::Failed,
                },
                None => item_details_base(state, r, None).await,
            }
        }))
        .await;
        for (r, lookup) in chunk.iter().zip(lookups) {
            match lookup {
                Lookup::Found(item) => details.push(item),
                Lookup::NotFound => not_found.push(r.clone()),
                Lookup::Failed => {}
            }
        }
    }
    let ids = details.iter().filter_map(|d| d.get("collectibleItemId").and_then(Value::as_str).map(str::to_owned)).collect();
    let collectibles = collectible_details(state, ids).await;
    if scoped.is_none() && state.auth.current_user_id() != user_id {
        tracing::warn!("Account changed while fetching rich item details; discarding them");
        return (Vec::new(), Vec::new());
    }
    let resolved = match collectibles {
        Some(c) => details.into_iter().map(|d| apply_collectible(d, Some(&c))).collect(),
        None => details.into_iter().filter(|d| d.get("collectibleItemId").is_none_or(Value::is_null)).collect(),
    };
    (resolved, not_found)
}

const BODY_PART_TYPES: [u64; 7] = [17, 27, 28, 29, 30, 31, 79];

fn has_body_parts(assets: &[Value]) -> bool {
    assets.iter().any(|a| a.pointer("/assetType/id").and_then(Value::as_u64).is_some_and(|t| BODY_PART_TYPES.contains(&t)))
}

/// `getBundleContents`: a bundle expanded into typed assets; a character
/// bundle's saved outfit also yields its scales and rig.
pub async fn bundle_contents(state: &AppState, bundle_id: u64) -> Option<Value> {
    let url = format!("{}/bundles/{bundle_id}/details", endpoints::CATALOG_V1);
    let details: Value = match state.roblox.get(&url).await {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!("Failed to fetch bundle contents: {error}");
            return None;
        }
    };
    let name = details.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
    let members: Vec<Value> = details.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let outfit = members
        .iter()
        .find(|m| m.get("type").and_then(Value::as_str) == Some("UserOutfit"))
        .and_then(|m| m.get("id"))
        .and_then(Value::as_u64);
    if let Some(outfit) = outfit {
        if let Some(contents) = outfit_bundle_contents(state, bundle_id, &name, outfit).await {
            return Some(contents);
        }
    }
    asset_bundle_contents(state, bundle_id, &name, &members).await
}

async fn outfit_bundle_contents(state: &AppState, bundle_id: u64, name: &str, outfit: u64) -> Option<Value> {
    let details: Value = match state.roblox.get(&format!("https://avatar.roblox.com/v3/outfits/{outfit}/details")).await {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!("Failed to read bundle outfit details, falling back to members: {error}");
            return None;
        }
    };
    let raw_assets = details.get("assets").and_then(Value::as_array).filter(|a| !a.is_empty())?;
    let assets: Vec<Value> = raw_assets
        .iter()
        .map(|a| {
            let type_id = a.pointer("/assetType/id").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0));
            let type_name = a.pointer("/assetType/name").and_then(Value::as_str).unwrap_or("");
            let mut asset = json!({
                "id": a.get("id"),
                "name": a.get("name").and_then(Value::as_str).unwrap_or(name),
                "assetType": { "id": type_id, "name": type_name },
            });
            if let Some(meta) = a.get("meta").filter(|m| !m.is_null()) {
                asset["meta"] = meta.clone();
            }
            asset
        })
        .collect();
    let character = has_body_parts(&assets);
    let mut contents = json!({ "assets": assets, "bundleId": bundle_id, "name": name, "isCharacterBundle": character, "unresolvedAssetIds": [] });
    match details.get("playerAvatarType").and_then(Value::as_str) {
        Some("R6") => contents["playerAvatarType"] = json!(1),
        Some("R15") => contents["playerAvatarType"] = json!(3),
        _ => {}
    }
    if let Some(scale) = details.get("scale").and_then(Value::as_object) {
        let mut scales = Map::new();
        for key in ["height", "width", "head", "depth", "proportion", "bodyType"] {
            if let Some(v) = scale.get(key).filter(|v| !v.is_null()) {
                scales.insert(key.into(), v.clone());
            }
        }
        if !scales.is_empty() {
            contents["scales"] = Value::Object(scales);
        }
    }
    Some(contents)
}

async fn asset_bundle_contents(state: &AppState, bundle_id: u64, name: &str, members: &[Value]) -> Option<Value> {
    let asset_members: Vec<&Value> = members.iter().filter(|m| m.get("type").and_then(Value::as_str) == Some("Asset")).collect();
    let missing: Vec<ItemRef> = asset_members
        .iter()
        .filter(|m| m.get("assetType").is_none_or(Value::is_null))
        .filter_map(|m| m.get("id").and_then(Value::as_u64))
        .map(|id| ItemRef { id, item_type: "Asset".into() })
        .collect();
    let mut resolved: HashMap<u64, Value> = HashMap::new();
    if !missing.is_empty() {
        match items_details(state, &missing, true).await {
            Ok(details) => {
                for d in details {
                    if let Some(id) = d.get("id").and_then(Value::as_u64) {
                        resolved.insert(id, d);
                    }
                }
            }
            Err(error) => {
                tracing::warn!("Failed to fetch bundle contents: {error}");
                return None;
            }
        }
    }
    let mut assets = Vec::new();
    let mut unresolved = Vec::new();
    for m in asset_members {
        let id = m.get("id").cloned().unwrap_or(Value::Null);
        let type_id = m.get("assetType").filter(|t| !t.is_null()).cloned().or_else(|| {
            id.as_u64().and_then(|i| resolved.get(&i)).and_then(|d| d.get("assetType")).filter(|t| !t.is_null()).cloned()
        });
        let Some(type_id) = type_id else {
            unresolved.push(id);
            continue;
        };
        let member_name = m.get("name").and_then(Value::as_str).unwrap_or(name);
        assets.push(json!({ "id": id, "name": member_name, "assetType": { "id": type_id, "name": "" } }));
    }
    let character = has_body_parts(&assets);
    Some(json!({ "assets": assets, "bundleId": bundle_id, "isCharacterBundle": character, "name": name, "unresolvedAssetIds": unresolved }))
}
async fn item_thumbnails(
    ctx: &Ctx,
    items: Vec<ItemRef>,
    size: String,
) -> AppResult<BTreeMap<String, String>> {
    let assets: Vec<u64> = items
        .iter()
        .filter(|i| !i.is_bundle())
        .map(|i| i.id)
        .collect();
    let bundles: Vec<u64> = items
        .iter()
        .filter(|i| i.is_bundle())
        .map(|i| i.id)
        .collect();
    let bundle_size = if size == "150x150" {
        "150x150"
    } else {
        "420x420"
    };
    let thumbs = &ctx.state.thumbnails;
    let asset_query = IdQuery {
        kind: "Asset",
        size: &size,
        format: "Webp",
        include_profile_frame: None,
    };
    let bundle_query = IdQuery {
        kind: "BundleThumbnail",
        size: bundle_size,
        format: "Png",
        include_profile_frame: None,
    };
    let fetch = |path: &'static str, size: String, format: &'static str| {
        move |missing: Vec<u64>| {
            let size = size.clone();
            async move {
                let mut out = Vec::new();
                for chunk in missing.chunks(THUMBNAIL_BATCH) {
                    let (param, base) = if path == "assets" {
                        ("assetIds", "assets")
                    } else {
                        ("bundleIds", "bundles/thumbnails")
                    };
                    let url = format!(
                        "{}/{base}?{param}={}&size={size}&format={format}",
                        endpoints::THUMBNAILS_V1,
                        join_ids(chunk)
                    );
                    match ctx.state.roblox.get::<ThumbPage>(&url).await {
                        Ok(page) => out.extend(page.data),
                        Err(error) => {
                            tracing::warn!(%error, "Failed to fetch {path} thumbnails");
                            return Ok(Vec::new());
                        }
                    }
                }
                Ok(out)
            }
        }
    };
    let (a, b) = futures::join!(
        thumbs.batch_by_id(&assets, &asset_query, fetch("assets", size.clone(), "Webp")),
        thumbs.batch_by_id(
            &bundles,
            &bundle_query,
            fetch("bundles", bundle_size.to_owned(), "Png")
        ),
    );
    let mut out = a?;
    out.extend(b?);
    Ok(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResellersQuery {
    collectible_item_id: String,
    cursor: Option<String>,
    limit: Option<u64>,
}

async fn resellers(ctx: &Ctx, q: ResellersQuery) -> AppResult<Value> {
    let mut url = format!(
        "{}/item/{}/resellers?limit={}",
        endpoints::MARKETPLACE_SALES_V1,
        urlencoding::encode(&q.collectible_item_id),
        q.limit.unwrap_or(100)
    );
    if let Some(cursor) = q.cursor {
        url.push_str(&format!("&cursor={}", urlencoding::encode(&cursor)));
    }
    let raw: Value = ctx.state.roblox.get(&url).await?;
    let data: Vec<Value> = raw
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            let seller = e.get("seller").cloned().unwrap_or(Value::Null);
            json!({
                "collectibleProductId": e.get("collectibleProductId"),
                "collectibleItemInstanceId": e.get("collectibleItemInstanceId"),
                "seller": {
                    "sellerId": seller.get("sellerId"),
                    "sellerType": seller.get("sellerType"),
                    "name": seller.get("name"),
                    "hasVerifiedBadge": seller.get("hasVerifiedBadge"),
                },
                "price": e.get("price"),
                "serialNumber": or_null(e, "serialNumber"),
            })
        })
        .collect();
    Ok(json!({
        "data": data,
        "previousPageCursor": or_null(&raw, "previousPageCursor"),
        "nextPageCursor": or_null(&raw, "nextPageCursor"),
    }))
}

async fn topics(ctx: &Ctx, request: Option<Value>) -> Value {
    let avatar_url = format!("{}/avatar", endpoints::AVATAR_V1);
    let avatar_items: Vec<Value> = match ctx.state.roblox.get::<Value>(&avatar_url).await {
        Ok(avatar) => avatar
            .get("assets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| json!({ "TargetId": a.get("id"), "ItemType": "Asset" }))
            .collect(),
        Err(_) => Vec::new(),
    };
    let request = request.unwrap_or(Value::Null);
    let body = json!({
        "items": avatar_items,
        "selectTopics": [],
        "inputQuery": request.get("inputQuery").and_then(Value::as_str).unwrap_or(""),
        "maxResult": request.get("maxResult").and_then(Value::as_u64).unwrap_or(20),
        "genderType": "Unknown",
    });
    let url = format!("{}/topic/get-topics", endpoints::CATALOG_V1);
    match ctx.state.roblox.post::<_, Value>(&url, &body).await {
        Ok(raw) => {
            let mut seen = HashSet::new();
            let list: Vec<Value> = raw
                .get("topics")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    let display = t
                        .get("displayName")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())?;
                    let original = t
                        .get("originalTopicName")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let key = if original.is_empty() {
                        display
                    } else {
                        original
                    };
                    seen.insert(key.to_owned())
                        .then(|| json!({ "displayName": display, "originalTopicName": original }))
                })
                .collect();
            Value::Array(list)
        }
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch catalog topics");
            json!([])
        }
    }
}

async fn recommendations(ctx: &Ctx, request: Value) -> Value {
    let Some(asset_id) = request.get("assetId").and_then(Value::as_u64) else {
        return json!([]);
    };
    let mut url = format!(
        "{}/recommendations/assets?assetId={asset_id}&numItems={}&details=false",
        endpoints::CATALOG_V2,
        request.get("numItems").and_then(Value::as_u64).unwrap_or(7)
    );
    if let Some(type_id) = request.get("assetTypeId").and_then(Value::as_u64) {
        url.push_str(&format!("&assetTypeId={type_id}"));
    }
    let ids: Vec<u64> = match ctx.state.roblox.get::<Value>(&url).await {
        Ok(raw) => raw
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch recommendations");
            return json!([]);
        }
    };
    if ids.is_empty() {
        return json!([]);
    }
    let refs: Vec<ItemRef> = ids
        .into_iter()
        .map(|id| ItemRef {
            id,
            item_type: "Asset".into(),
        })
        .collect();
    match items_details(&ctx.state, &refs, false).await {
        Ok(items) => Value::Array(items),
        Err(_) => json!([]),
    }
}

async fn promoted_shop_pages(ctx: &Ctx, force: bool) -> AppResult<Value> {
    let mut slot = ctx.state.catalog.promoted_pages.lock().await;
    if !force {
        if let Some((_, pages)) = slot
            .as_ref()
            .filter(|(at, _)| at.elapsed() < PROMOTED_PAGES_TTL)
        {
            return Ok(pages.clone());
        }
    }
    let installation_id = gated_installation_id(&ctx.state);
    let query = [
        ("installation_id", installation_id.as_str()),
        ("version", env!("CARGO_PKG_VERSION")),
    ];
    let mut raw: Value = ctx
        .state
        .metadata
        .api()
        .get("/v1/marketplace/promoted-pages", &query)
        .await?;
    let pages = raw.get_mut("pages").map(Value::take).unwrap_or(Value::Null);
    *slot = Some((Instant::now(), pages.clone()));
    Ok(pages)
}

fn favorite_url(user_id: u64, item: &ItemRef) -> String {
    let kind = if item.is_bundle() {
        "bundles"
    } else {
        "assets"
    };
    format!(
        "{}/favorites/users/{user_id}/{kind}/{}/favorite",
        endpoints::CATALOG_V1,
        item.id
    )
}

// ------------------------------------------------------------------ cart

fn cart_path(state: &AppState, user_id: u64) -> PathBuf {
    state
        .paths
        .base
        .join("marketplace-carts")
        .join(format!("cart-{user_id}.json"))
}

fn cart_key(item: &Value) -> String {
    format!(
        "{}:{}",
        item.get("itemType").and_then(Value::as_str).unwrap_or(""),
        item.get("itemId").map(Value::to_string).unwrap_or_default()
    )
}

fn read_cart(state: &AppState, user_id: u64) -> Option<Vec<Value>> {
    let bytes = std::fs::read(cart_path(state, user_id)).ok()?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(cart) => Some(
            cart.get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        Err(error) => {
            tracing::warn!(user_id, %error, "Failed to read cart");
            None
        }
    }
}

fn write_cart(state: &AppState, user_id: u64, items: &[Value]) {
    let path = cart_path(state, user_id);
    let result = (|| -> AppResult<()> {
        std::fs::create_dir_all(path.parent().expect("cart file has a parent"))?;
        let tmp = path.with_extension("json.gugu-for-roblox.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&json!({ "items": items }))?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(user_id, %error, "Failed to write cart");
    }
}

fn cart(items: Vec<Value>) -> Value {
    json!({ "items": items })
}

pub fn register(router: &mut Router) {
    router
        .handle("catalog:getNavigationMenu", |ctx, args| async move {
            let force: Option<bool> = args.opt(0)?;
            navigation_menu(&ctx, force.unwrap_or(false)).await
        })
        .handle("catalog:searchItems", |ctx, args| async move {
            let query: SearchQuery = args.opt(0)?.unwrap_or_default();
            search_items(&ctx, query).await
        })
        .handle("catalog:getItemsDetails", |ctx, args| async move {
            let items: Vec<ItemRef> = args.get(0)?;
            items_details(&ctx.state, &items, false).await
        })
        .handle("catalog:getItemDetails", |ctx, args| async move {
            let item: ItemRef = args.get(0)?;
            Ok(item_details(&ctx.state, item).await)
        })
        .handle("catalog:getItemsDetailsRich", |ctx, args| async move {
            let items: Vec<ItemRef> = args.get(0)?;
            Ok(items_details_rich(&ctx.state, items).await)
        })
        .handle("catalog:getBundleContents", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(bundle_contents(&ctx.state, id).await)
        })
        .handle("catalog:getItemThumbnails", |ctx, args| async move {
            let items: Vec<ItemRef> = args.get(0)?;
            let size: Option<String> = args.opt(1)?;
            if items.is_empty() {
                return Ok(BTreeMap::new());
            }
            item_thumbnails(&ctx, items, size.unwrap_or_else(|| "420x420".into())).await
        })
        .handle("catalog:getResellers", |ctx, args| async move {
            let query: ResellersQuery = args.get(0)?;
            resellers(&ctx, query).await
        })
        .handle("catalog:getTopics", |ctx, args| async move {
            let request: Option<Value> = args.opt(0)?;
            Ok(topics(&ctx, request).await)
        })
        .handle("catalog:getRecommendations", |ctx, args| async move {
            let request: Value = args.get(0)?;
            Ok(recommendations(&ctx, request).await)
        })
        .handle("catalog:getPromotedShopPages", |ctx, args| async move {
            let force: Option<bool> = args.opt(0)?;
            promoted_shop_pages(&ctx, force.unwrap_or(false)).await
        })
        .handle("catalog:getFavoriteStatus", |ctx, args| async move {
            let item: ItemRef = args.get(0)?;
            let Some(user_id) = ctx.state.auth.current_user_id() else {
                return Ok(false);
            };
            let raw: Value = ctx.state.roblox.get(&favorite_url(user_id, &item)).await?;
            Ok(!raw.is_null())
        })
        .handle("catalog:setFavorite", |ctx, args| async move {
            let item: ItemRef = args.get(0)?;
            let favorite: bool = args.get(1)?;
            let user_id = ctx
                .state
                .auth
                .current_user_id()
                .ok_or_else(|| AppError::msg("Not logged in"))?;
            let url = favorite_url(user_id, &item);
            if favorite {
                ctx.state.roblox.post_empty::<Value>(&url).await?;
            } else {
                ctx.state.roblox.delete::<Value>(&url).await?;
            }
            Ok(favorite)
        })
        .handle("cart:get", |ctx, _| async move {
            let Some(user_id) = ctx.state.auth.current_user_id() else {
                return Ok(cart(Vec::new()));
            };
            Ok(cart(read_cart(&ctx.state, user_id).unwrap_or_default()))
        })
        .handle("cart:add", |ctx, args| async move {
            let item: Value = args.get(0)?;
            let Some(user_id) = ctx.state.auth.current_user_id() else {
                return Ok(cart(Vec::new()));
            };
            let mut items = read_cart(&ctx.state, user_id).unwrap_or_default();
            let key = cart_key(&item);
            if items.iter().any(|existing| cart_key(existing) == key) {
                return Ok(cart(items));
            }
            items.push(item);
            if items.len() > CART_MAX_ITEMS {
                items.drain(..items.len() - CART_MAX_ITEMS);
            }
            write_cart(&ctx.state, user_id, &items);
            Ok(cart(items))
        })
        .handle("cart:remove", |ctx, args| async move {
            let item: ItemRef = args.get(0)?;
            let Some(user_id) = ctx.state.auth.current_user_id() else {
                return Ok(cart(Vec::new()));
            };
            let Some(items) = read_cart(&ctx.state, user_id) else {
                return Ok(cart(Vec::new()));
            };
            let key = item.key();
            let items: Vec<Value> = items.into_iter().filter(|e| cart_key(e) != key).collect();
            write_cart(&ctx.state, user_id, &items);
            Ok(cart(items))
        })
        .handle("cart:clear", |ctx, _| async move {
            let Some(user_id) = ctx.state.auth.current_user_id() else {
                return Ok(cart(Vec::new()));
            };
            write_cart(&ctx.state, user_id, &[]);
            Ok(cart(Vec::new()))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_item_mapping() {
        let item = to_catalog_item(&json!({
            "id": 5, "itemType": "Bundle", "creatorType": "Group", "itemRestrictions": [3, "Collectible", 99],
        }));
        assert_eq!(item["itemType"], "Bundle");
        assert_eq!(item["creatorType"], "Group");
        assert_eq!(item["itemRestrictions"], json!(["Limited", "Collectible"]));
        assert_eq!(item["price"], Value::Null);
        assert!(item.get("isOffSale").is_none());
        assert_eq!(
            normalize_creator_name(Some("  roblox user ")),
            Some("ROBLOX+USER".into())
        );
    }
}
