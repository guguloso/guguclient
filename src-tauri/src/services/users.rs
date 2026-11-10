//! Players (`users:*`). Port of `UsersService` (+ part of `UserInventoryService`).
//!
//! Includes the profile speed-ups made in the JS build: currently-wearing
//! inventory metadata is scanned per asset type in parallel with a page cap.
//!
//! Pending: getInventoryCategories, getInventoryPage.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::future::{join_all, try_join_all};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints;
use crate::services::thumbnails::{IdQuery, ThumbPage};

const SEARCH_RATE_LIMIT: Duration = Duration::from_millis(500);
const SEARCH_LIMITS: [u32; 4] = [10, 25, 50, 100];
const MAX_INVENTORY_PAGES_PER_TYPE: usize = 3;

/// Debounce state for `users:search` (Roblox rate-limits the endpoint hard).
#[derive(Default)]
pub struct UserSearchState {
    last: Mutex<Option<(Instant, String, Value)>>,
}

async fn search(ctx: &Ctx, keyword: &str, limit: u32) -> AppResult<Value> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Ok(json!([]));
    }
    let wait = {
        let last = ctx.state.user_search.last.lock();
        match &*last {
            Some((at, k, cached)) if k == keyword && at.elapsed() < SEARCH_RATE_LIMIT => {
                return Ok(cached.clone());
            }
            Some((at, _, _)) => SEARCH_RATE_LIMIT.checked_sub(at.elapsed()),
            None => None,
        }
    };
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    let limit = SEARCH_LIMITS.iter().copied().find(|l| *l >= limit).unwrap_or(100);
    let url = format!("{}/users/search?keyword={}&limit={limit}", endpoints::USERS_V1, urlencoding::encode(keyword));
    let mut body: Value = ctx.state.roblox.get(&url).await?;
    let data = body.get_mut("data").map(Value::take).unwrap_or_else(|| json!([]));
    *ctx.state.user_search.last.lock() = Some((Instant::now(), keyword.to_owned(), data.clone()));
    Ok(data)
}

async fn social_counts(ctx: &Ctx, user_id: u64) -> AppResult<Value> {
    let base = endpoints::FRIENDS_V1;
    let urls = [
        format!("{base}/users/{user_id}/friends/count"),
        format!("{base}/users/{user_id}/followers/count"),
        format!("{base}/users/{user_id}/followings/count"),
    ];
    let counts = try_join_all(urls.iter().map(|url| async move {
        let body: Value = ctx.state.roblox.get(url).await?;
        AppResult::Ok(body.get("count").and_then(Value::as_u64).unwrap_or(0))
    }))
    .await?;
    Ok(json!({ "friendsCount": counts[0], "followersCount": counts[1], "followingsCount": counts[2] }))
}

/// `getUserProfile`: details + social counts + games run concurrently.
async fn profile(ctx: &Ctx, user_id: u64) -> AppResult<Value> {
    let games_url = format!("{}/users/{user_id}/games?accessFilter=2&sortOrder=Asc&limit=25", endpoints::GAMES_V2);
    let (details, counts, games) = futures::join!(
        ctx.state.auth.user_details(user_id),
        social_counts(ctx, user_id),
        ctx.state.roblox.get::<Value>(&games_url),
    );
    let Value::Object(mut out) = details? else {
        return Err(AppError::msg("unexpected user details shape"));
    };
    let total_visits: u64 = games
        .ok()
        .and_then(|g| g.get("data").and_then(Value::as_array).map(|list| {
            list.iter().filter_map(|game| game.get("placeVisits").or_else(|| game.get("visits"))).filter_map(Value::as_u64).sum()
        }))
        .unwrap_or(0);
    out.insert("socialCounts".into(), counts?);
    out.insert("totalPlaceVisits".into(), json!(total_visits));
    Ok(Value::Object(out))
}

async fn badges(ctx: &Ctx, user_id: u64, limit: Option<f64>) -> AppResult<Vec<Value>> {
    let limit = match limit {
        Some(l) if l.is_finite() => (l.floor() as i64).clamp(1, 100) as usize,
        _ => 100,
    };
    let mut out: Vec<Value> = Vec::with_capacity(limit);
    let mut cursor = String::new();
    let mut last_error = None;
    while out.len() < limit {
        let page_limit = (limit - out.len()).min(100);
        let mut url = format!("{}/users/{user_id}/badges?limit={page_limit}&sortOrder=Desc", endpoints::BADGES_V1);
        if !cursor.is_empty() {
            url.push_str("&cursor=");
            url.push_str(&urlencoding::encode(&cursor));
        }
        let mut page: Value = match ctx.state.roblox.get(&url).await {
            Ok(page) => page,
            Err(error) => {
                last_error = Some(error);
                break;
            }
        };
        if let Some(Value::Array(list)) = page.get_mut("data").map(Value::take) {
            out.extend(list);
        }
        match page.get("nextPageCursor").and_then(Value::as_str) {
            Some(next) if !next.is_empty() => cursor = next.to_owned(),
            _ => break,
        }
    }
    if out.is_empty() {
        return Err(last_error.unwrap_or_else(|| AppError::msg("Failed to fetch user badges")));
    }
    out.truncate(limit);
    Ok(out)
}

async fn badge_thumbnails(ctx: &Ctx, badge_ids: Vec<u64>, size: String) -> AppResult<std::collections::BTreeMap<String, String>> {
    let query = IdQuery { kind: "BadgeIcon", size: &size, format: "Png", include_profile_frame: None };
    let roblox = &ctx.state.roblox;
    let size_ref = size.as_str();
    ctx.state
        .thumbnails
        .batch_by_id(&badge_ids, &query, |missing| async move {
            let url = format!(
                "{}/badges/icons?badgeIds={}&size={size_ref}&format=Png&isCircular=false",
                endpoints::THUMBNAILS_V1,
                endpoints::join_ids(&missing)
            );
            match roblox.get::<ThumbPage>(&url).await {
                Ok(page) => Ok(page.data),
                Err(error) => {
                    tracing::warn!(%error, count = missing.len(), "Failed to fetch badge thumbnails");
                    Ok(Vec::new())
                }
            }
        })
        .await
}

async fn inventory_visibility(ctx: &Ctx, user_id: u64) -> AppResult<bool> {
    if !ctx.state.auth.is_authenticated() {
        return Err(AppError::msg("Not authenticated"));
    }
    let url = format!("{}/users/{user_id}/can-view-inventory", endpoints::INVENTORY_V1);
    match ctx.state.roblox.get::<Value>(&url).await {
        Ok(body) => Ok(body.get("canView").and_then(Value::as_bool).unwrap_or(false)),
        Err(AppError::Api { status: 403, .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn update_description(ctx: &Ctx, description: &str) -> AppResult<bool> {
    let url = format!("{}/description", endpoints::USERS_V1);
    ctx.state.roblox.post::<_, Value>(&url, &json!({ "description": description })).await?;
    Ok(true)
}

/// `getCurrentlyWearing`: profile component + catalog details + limited
/// metadata from the owner's inventory (parallel, capped).
async fn currently_wearing(ctx: &Ctx, user_id: u64) -> AppResult<Vec<Value>> {
    let url = format!("{}/profiles/get", endpoints::PROFILE_PLATFORM_API);
    let body = json!({
        "profileId": user_id.to_string(),
        "profileType": "User",
        "components": [{ "component": "CurrentlyWearing" }],
    });
    let mut profile: Value = ctx.state.roblox.post(&url, &body).await?;
    let assets = match profile.pointer_mut("/components/CurrentlyWearing/assets").map(Value::take) {
        Some(Value::Array(list)) if !list.is_empty() => list,
        _ => return Ok(Vec::new()),
    };
    let asset_ids: Vec<u64> = assets.iter().filter_map(|a| a.get("assetId").and_then(Value::as_u64)).collect();

    let details_url = format!("{}/catalog/items/details", endpoints::CATALOG_V1);
    let items: Vec<Value> = asset_ids.iter().map(|id| json!({ "id": id, "itemType": "Asset" })).collect();
    let details: HashMap<u64, Value> = match ctx.state.roblox.post::<_, Value>(&details_url, &json!({ "items": items })).await {
        Ok(mut response) => match response.get_mut("data").map(Value::take) {
            Some(Value::Array(list)) => list
                .into_iter()
                .filter_map(|d| Some((d.get("id").and_then(Value::as_u64)?, d)))
                .collect(),
            _ => HashMap::new(),
        },
        Err(error) => return Err(error),
    };

    // Group target asset ids by asset type, then scan each type concurrently.
    let mut by_type: HashMap<u64, HashSet<u64>> = HashMap::new();
    for id in &asset_ids {
        if let Some(kind) = details.get(id).and_then(|d| d.get("assetType")).and_then(Value::as_u64).filter(|k| *k > 0) {
            by_type.entry(kind).or_default().insert(*id);
        }
    }
    let scans = by_type.iter().map(|(kind, targets)| scan_inventory_type(ctx, user_id, *kind, targets));
    let mut meta: HashMap<u64, (Option<Value>, Option<u64>)> = HashMap::new();
    for found in join_all(scans).await {
        meta.extend(found);
    }

    Ok(assets
        .iter()
        .map(|asset| {
            let id = asset.get("assetId").and_then(Value::as_u64).unwrap_or(0);
            let d = details.get(&id);
            let field = |k: &str| d.and_then(|d| d.get(k)).cloned().unwrap_or(Value::Null);
            let (collectible, serial) = meta.get(&id).cloned().unwrap_or((None, None));
            let type_name = d
                .and_then(|d| d.pointer("/taxonomy/0/taxonomyName"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Accessory");
            let mut out = Map::new();
            out.insert("id".into(), json!(id));
            out.insert("name".into(), d.and_then(|d| d.get("name")).cloned().unwrap_or_else(|| json!("")));
            out.insert("itemType".into(), json!("Asset"));
            out.insert("thumbnailType".into(), json!("Asset"));
            if let Some(c) = collectible {
                out.insert("collectibleItemId".into(), c);
            }
            if let Some(s) = serial {
                out.insert("serialNumber".into(), json!(s));
            }
            out.insert("creatorName".into(), field("creatorName"));
            out.insert("price".into(), field("price"));
            out.insert("priceStatus".into(), field("priceStatus"));
            out.insert(
                "assetType".into(),
                json!({ "id": d.and_then(|d| d.get("assetType")).and_then(Value::as_u64).unwrap_or(0), "name": type_name }),
            );
            Value::Object(out)
        })
        .collect())
}

async fn scan_inventory_type(
    ctx: &Ctx,
    user_id: u64,
    asset_type: u64,
    targets: &HashSet<u64>,
) -> HashMap<u64, (Option<Value>, Option<u64>)> {
    let mut found: HashMap<u64, (Option<Value>, Option<u64>)> = HashMap::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_INVENTORY_PAGES_PER_TYPE {
        let mut url = format!(
            "{}/users/{user_id}/inventory/{asset_type}?limit=100&sortOrder=Desc",
            endpoints::INVENTORY_V2
        );
        if let Some(c) = &cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding::encode(c));
        }
        let Ok(page) = ctx.state.roblox.get::<Value>(&url).await else { break };
        for item in page.get("data").and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = item.get("assetId").and_then(Value::as_u64).filter(|id| targets.contains(id)) else { continue };
            let collectible = item.get("collectibleItemId").filter(|v| !v.is_null()).cloned();
            let serial = item.get("serialNumber").and_then(Value::as_u64);
            let replace = match found.get(&id) {
                None => true,
                Some((_, existing)) => serial.is_some_and(|s| s > 0) && existing.is_none(),
            };
            if replace {
                found.insert(id, (collectible, serial));
            }
        }
        cursor = page.get("nextPageCursor").and_then(Value::as_str).map(str::to_owned);
        if cursor.is_none() || targets.iter().all(|id| found.contains_key(id)) {
            break;
        }
    }
    found
}

pub fn register(router: &mut Router) {
    router
        .handle("users:search", |ctx, args| async move {
            let keyword: String = args.get(0)?;
            let limit = args.opt::<u32>(1)?.unwrap_or(10);
            search(&ctx, &keyword, limit).await
        })
        .handle("users:getProfile", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            profile(&ctx, user_id).await
        })
        .handle("users:getBadges", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            let limit = args.opt::<f64>(1)?;
            badges(&ctx, user_id, limit).await
        })
        .handle("users:getBadgeThumbnails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size = args.opt::<String>(1)?.unwrap_or_else(|| "150x150".to_owned());
            badge_thumbnails(&ctx, ids, size).await
        })
        .handle("users:getSocialCounts", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            social_counts(&ctx, user_id).await
        })
        .handle("users:getInventoryVisibility", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            inventory_visibility(&ctx, user_id).await
        })
        .handle("users:updateDescription", |ctx, args| async move {
            let description: String = args.get(0)?;
            update_description(&ctx, &description).await
        })
        .handle("users:getCurrentlyWearing", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            currently_wearing(&ctx, user_id).await
        });
}
