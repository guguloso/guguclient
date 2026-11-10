//! Search, Roblox notifications, inbox messages and virtual events.
//! Ports of `SearchService` (`search:*`), `NotificationsService`
//! (`notifications:*`), `PrivateMessagesService` (`privateMessages:*`) and
//! `EventsService` (`events:*`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::Url;
use serde_json::{json, Value};

use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints;
use crate::services::games;
use crate::state::AppState;
use crate::util::now_iso;

const LANDING_TTL: Duration = Duration::from_secs(2 * 60);
const DEFAULT_PAGE_SIZE: u64 = 20;
const RSVP_STATUSES: [&str; 4] = ["going", "maybeGoing", "none", "notGoing"];

#[derive(Default)]
pub struct FeedsState {
    landing: Mutex<HashMap<String, (Instant, Value)>>,
    omni_session: Mutex<Option<String>>,
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

// ------------------------------------------------------------------ search

async fn landing_page(ctx: &Ctx) -> Value {
    let key = ctx
        .state
        .auth
        .current_user_id()
        .map_or_else(|| "guest".to_owned(), |id| format!("user:{id}"));
    if let Some((at, cached)) = ctx.state.feeds.landing.lock().get(&key) {
        if at.elapsed() < LANDING_TTL {
            return cached.clone();
        }
    }
    let url = format!(
        "{}?sessionId={}",
        endpoints::SEARCH_LANDING_PAGE,
        uuid::Uuid::new_v4()
    );
    let raw: Value = match ctx.state.roblox.get(&url).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::error!("Failed to fetch search landing page: {error}");
            return json!({ "recentlyVisited": [], "trendingQueries": [] });
        }
    };
    let sorts = raw
        .get("sorts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let recent = sorts
        .iter()
        .find(|s| s.get("sortId").and_then(Value::as_str) == Some("RecentlyVisited"));
    let trending = sorts
        .iter()
        .find(|s| s.get("contentType").and_then(Value::as_str) == Some("TrendingSearches"));
    let queries: Vec<Value> = trending
        .and_then(|t| t.get("queries"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|q| q.get("query").cloned().unwrap_or(Value::Null))
        .collect();
    let games_raw = recent
        .and_then(|r| r.get("games"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ids: Vec<u64> = games_raw.iter().map(|g| n(g, "universeId")).collect();
    let thumbs = games::wide_thumbnails(ctx, &ids, "576x324")
        .await
        .unwrap_or_default();
    let recently: Vec<Value> = games_raw
        .iter()
        .map(|g| {
            let id = n(g, "universeId");
            json!({
                "universeId": id,
                "placeId": g.get("rootPlaceId"),
                "name": g.get("name"),
                "playerCount": g.get("playerCount"),
                "rating": games::approval_percentage(n(g, "totalUpVotes"), n(g, "totalDownVotes")),
                "thumbnailUrl": thumbs.get(&id.to_string()),
            })
        })
        .collect();
    let result = json!({ "recentlyVisited": recently, "trendingQueries": queries });
    ctx.state
        .feeds
        .landing
        .lock()
        .insert(key, (Instant::now(), result.clone()));
    result
}

async fn autocomplete(state: &AppState, prefix: &str, max: usize) -> Value {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return json!([]);
    }
    let url = format!(
        "{}/get-suggestion/{}",
        endpoints::GAMES_AUTOCOMPLETE,
        urlencoding::encode(trimmed)
    );
    match state.roblox.get::<Value>(&url).await {
        Ok(raw) => Value::Array(
            raw.get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|e| e.get("type").and_then(Value::as_i64) == Some(1))
                .take(max)
                .map(|e| json!({ "searchQuery": e.get("searchQuery"), "type": "experience" }))
                .collect(),
        ),
        Err(error) => {
            tracing::error!("Failed to fetch autocomplete suggestions: {error}");
            json!([])
        }
    }
}

async fn marketplace_autocomplete(state: &AppState, prefix: &str, limit: u64) -> Value {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return json!([]);
    }
    let enc = urlencoding::encode(trimmed);
    let url = format!(
        "{}/suggest?prefix={enc}&limit={limit}&lang=en&q={enc}",
        endpoints::MARKETPLACE_AUTOCOMPLETE
    );
    match state.roblox.get::<Value>(&url).await {
        Ok(raw) => Value::Array(
            raw.get("Data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|e| e.get("Query").cloned().unwrap_or(Value::Null))
                .collect(),
        ),
        Err(error) => {
            tracing::error!("Failed to fetch marketplace autocomplete: {error}");
            json!([])
        }
    }
}

async fn omni_search(ctx: &Ctx, query: &str, page_token: Option<String>) -> Value {
    let empty = json!({ "games": [], "nextPageToken": "", "hasMore": false });
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return empty;
    }
    let session = {
        let mut session = ctx.state.feeds.omni_session.lock();
        if page_token.as_deref().unwrap_or("").is_empty() || session.is_none() {
            *session = Some(uuid::Uuid::new_v4().to_string());
        }
        session.clone().unwrap_or_default()
    };
    let url = format!(
        "{}/omni-search?searchQuery={}&pageToken={}&pageType=all&sessionId={session}",
        endpoints::SEARCH_API,
        urlencoding::encode(trimmed),
        urlencoding::encode(page_token.as_deref().unwrap_or(""))
    );
    let raw: Value = match ctx.state.roblox.get(&url).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::error!("Failed to perform omni-search: {error}");
            return empty;
        }
    };
    let rows: Vec<Value> = raw
        .get("searchResults")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|g| g.get("contentGroupType").and_then(Value::as_str) == Some("Game"))
        .flat_map(|g| {
            g.get("contents")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let ids: Vec<u64> = rows.iter().map(|g| n(g, "universeId")).collect();
    let thumbs = games::icons(ctx, &ids, "512x512").await.unwrap_or_default();
    let games: Vec<Value> = rows
        .iter()
        .map(|g| {
            let id = n(g, "universeId");
            json!({
                "universeId": id,
                "placeId": g.get("rootPlaceId"),
                "name": g.get("name"),
                "description": s(g, "description").unwrap_or(""),
                "playerCount": g.get("playerCount"),
                "rating": games::approval_percentage(n(g, "totalUpVotes"), n(g, "totalDownVotes")),
                "thumbnailUrl": thumbs.get(&id.to_string()),
                "isSponsored": g.get("isSponsored").and_then(Value::as_bool).unwrap_or(false),
                "creatorName": s(g, "creatorName").unwrap_or(""),
                "creatorHasVerifiedBadge": g.get("creatorHasVerifiedBadge").and_then(Value::as_bool).unwrap_or(false),
                "minimumAge": g.get("minimumAge").and_then(Value::as_u64).unwrap_or(0),
                "ageRecommendationDisplayName": s(g, "ageRecommendationDisplayName").unwrap_or(""),
            })
        })
        .collect();
    let next = s(&raw, "nextPageToken").unwrap_or("").to_owned();
    json!({ "games": games, "hasMore": !next.is_empty(), "nextPageToken": next })
}

// ------------------------------------------------------------------ notifications

fn deeplink_action(path: &str) -> Option<Value> {
    let Ok(url) = Url::parse(path) else {
        return (!path.is_empty()).then(|| json!({ "type": "open-url", "url": path }));
    };
    let q = |k: &str| {
        url.query_pairs()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.into_owned())
    };
    let route = url.path().trim_start_matches('/');
    Some(match route {
        "profile" => {
            let id = q("userId").unwrap_or_default().parse::<f64>().ok()?;
            json!({ "type": "open-profile", "userId": id as u64 })
        }
        "catalog" => json!({ "route": "marketplace", "type": "open-route" }),
        "chat" => json!({ "route": "chat", "type": "open-route" }),
        "messages" => json!({ "route": "inbox", "type": "open-route" }),
        "group" => {
            json!({ "communityId": q("groupId").filter(|g| !g.is_empty())?, "type": "open-community" })
        }
        "event_details" => {
            json!({ "eventId": q("eventId").filter(|e| !e.is_empty())?, "type": "open-event" })
        }
        "account_info" => json!({ "type": "open-url", "url": "https://www.roblox.com/my/account" }),
        "notification_settings" => {
            json!({ "type": "open-url", "url": "https://www.roblox.com/my/account#!/notifications" })
        }
        "external_web_link" => {
            json!({ "type": "open-url", "url": format!("https://help.roblox.com/hc/articles/{}", q("articleId").unwrap_or_default()) })
        }
        _ => json!({ "type": "open-url", "url": path }),
    })
}

fn notification_action(action: &Value, name: &str, payload: Option<&Value>) -> Option<Value> {
    match s(action, "actionType") {
        Some("deeplink") => {
            let deeplink = deeplink_action(s(action, "path").unwrap_or(""));
            if deeplink.as_ref().and_then(|d| s(d, "type")) == Some("open-event") {
                return deeplink;
            }
            let num = |k: &str| {
                payload.and_then(|p| p.get(k)).and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .or_else(|| v.as_u64())
                })
            };
            match (num("universe_id"), num("place_id")) {
                (Some(u), Some(p)) if u > 0 && p > 0 => Some(
                    json!({ "name": name, "placeId": p, "type": "open-game", "universeId": u }),
                ),
                _ => deeplink,
            }
        }
        Some("notificationAPI") => s(action, "path")
            .filter(|p| !p.is_empty())
            .map(|p| json!({ "actionName": p, "type": "server-action" })),
        Some("dismiss") => Some(json!({ "type": "dismiss" })),
        _ => None,
    }
}

/// `normalizeNotification`.
fn normalize_notification(entry: &Value) -> Value {
    let content = entry.get("content");
    let visual = content.and_then(|c| {
        let states = c.get("states")?;
        let key = c
            .get("currentState")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .unwrap_or("default");
        states
            .get(key)
            .or_else(|| states.get("default"))?
            .get("visualItems")
            .cloned()
    });
    let text = visual.as_ref().and_then(|v| v.pointer("/textBody/0"));
    let title_text = text
        .and_then(|t| t.pointer("/title/text"))
        .and_then(Value::as_str);
    let label_text = text
        .and_then(|t| t.pointer("/label/text"))
        .and_then(Value::as_str);
    let title = title_text
        .or(label_text)
        .unwrap_or("Notification")
        .to_owned();
    let subtitle = if title_text.is_some() {
        label_text.map(Value::from).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let thumbnail = visual
        .as_ref()
        .and_then(|v| v.pointer("/thumbnail/0"))
        .and_then(|t| {
            let id = t.get("id").and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| v.as_u64())
            })?;
            match s(t, "idType") {
                Some("userThumbnail") => Some(json!({ "type": "user", "userId": id })),
                Some("gameThumbnail") => Some(json!({ "type": "game", "universeId": id })),
                _ => None,
            }
        });
    let buttons: Vec<Value> = visual
        .as_ref()
        .and_then(|v| v.get("button"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|button| {
            let actions: Vec<Value> = button
                .get("actions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| notification_action(a, &title, button.get("clientEventsPayload")))
                .collect();
            json!({
                "actions": actions,
                "label": button.pointer("/label/text").and_then(Value::as_str).unwrap_or(""),
                "style": if s(button, "buttonStyle") == Some("Secondary") { "secondary" } else { "primary" },
            })
        })
        .collect();
    let kind_type = content
        .and_then(|c| s(c, "notificationType"))
        .filter(|t| !t.is_empty())
        .unwrap_or("Unknown");
    let kind = match kind_type {
        "Experience" => "experience",
        "FriendRequestAccepted" => "friend-request-accepted",
        _ => "unknown",
    };
    json!({
        "buttons": buttons,
        "eventDate": entry.get("eventDate").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!(now_iso())),
        "id": s(entry, "id").unwrap_or(""),
        "isInteracted": entry.get("isInteracted").and_then(Value::as_bool).unwrap_or(false),
        "kind": kind,
        "notificationType": kind_type,
        "subtitle": subtitle,
        "thumbnail": thumbnail.unwrap_or_else(|| json!({ "type": "static" })),
        "title": title,
    })
}

// ------------------------------------------------------------------ events

fn map_event(raw: &Value) -> Value {
    let mut thumbs: Vec<&Value> = raw
        .get("thumbnails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    thumbs.sort_by_key(|t| t.get("rank").and_then(Value::as_i64).unwrap_or(0));
    let media = thumbs
        .iter()
        .find_map(|t| t.get("mediaId").filter(|m| m.is_number()).cloned())
        .unwrap_or(Value::Null);
    let or_null = |v: Option<&Value>| v.cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null);
    let rsvp = s(raw, "userRsvpStatus")
        .filter(|r| RSVP_STATUSES.contains(r))
        .unwrap_or("none");
    json!({
        "categories": raw.get("eventCategories").and_then(Value::as_array).into_iter().flatten()
            .filter_map(|e| e.get("category").filter(|c| c.as_str().is_some_and(|c| !c.is_empty())).cloned()).collect::<Vec<_>>(),
        "description": s(raw, "displayDescription").or(s(raw, "description")).unwrap_or(""),
        "endUtc": or_null(raw.pointer("/eventTime/endUtc")),
        "host": {
            "hasVerifiedBadge": raw.pointer("/host/hasVerifiedBadge").and_then(Value::as_bool).unwrap_or(false),
            "hostId": or_null(raw.pointer("/host/hostId")),
            "hostName": or_null(raw.pointer("/host/hostName")),
            "hostType": or_null(raw.pointer("/host/hostType")),
        },
        "id": raw.get("id"),
        "placeId": or_null(raw.get("placeId")),
        "startUtc": or_null(raw.pointer("/eventTime/startUtc")),
        "status": or_null(raw.get("eventStatus")),
        "subtitle": or_null(raw.get("displaySubtitle").filter(|v| !v.is_null()).or(raw.get("subtitle"))),
        "thumbnailMediaId": media,
        "title": s(raw, "displayTitle").or(s(raw, "title")).unwrap_or(""),
        "universeId": or_null(raw.get("universeId")),
        "userRsvpStatus": rsvp,
        "visibility": or_null(raw.get("eventVisibility")),
    })
}

fn map_event_page(raw: &Value) -> Value {
    let cursor = |k: &str| {
        s(raw, k)
            .filter(|c| !c.is_empty())
            .map(Value::from)
            .unwrap_or(Value::Null)
    };
    json!({
        "data": raw.get("data").and_then(Value::as_array).into_iter().flatten().map(map_event).collect::<Vec<_>>(),
        "nextCursor": cursor("nextPageCursor"),
        "previousCursor": cursor("previousPageCursor"),
    })
}

fn empty_event_page() -> Value {
    json!({ "data": [], "nextCursor": null, "previousCursor": null })
}

async fn active_universe_events(state: &AppState, universe: u64) -> Value {
    let from = now_iso();
    let mut events = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut url = format!(
            "{}/universes/{universe}/virtual-events?fromUtc={}",
            endpoints::VIRTUAL_EVENTS_V1,
            urlencoding::encode(&from)
        );
        if let Some(c) = &cursor {
            url.push_str(&format!("&cursor={}", urlencoding::encode(c)));
        }
        let raw: Value = match state.roblox.get(&url).await {
            Ok(raw) => raw,
            Err(error) => {
                tracing::error!("Failed to fetch events for universe {universe}: {error}");
                return json!({ "status": "error" });
            }
        };
        let page = map_event_page(&raw);
        events.extend(page["data"].as_array().cloned().unwrap_or_default());
        cursor = page["nextCursor"]
            .as_str()
            .map(str::to_owned)
            .filter(|c| seen.insert(c.clone()));
        if cursor.is_none() {
            break;
        }
    }
    json!({ "status": "ok", "events": events })
}

pub fn register(router: &mut Router) {
    router
        .handle("search:getLandingPage", |ctx, _| async move { Ok(landing_page(&ctx).await) })
        .handle("search:getAutocompleteSuggestions", |ctx, args| async move {
            let prefix: String = args.get(0)?;
            let max: Option<usize> = args.opt(1)?;
            Ok(autocomplete(&ctx.state, &prefix, max.unwrap_or(5)).await)
        })
        .handle("search:getMarketplaceAutocomplete", |ctx, args| async move {
            let prefix: String = args.get(0)?;
            let limit: Option<u64> = args.opt(1)?;
            Ok(marketplace_autocomplete(&ctx.state, &prefix, limit.unwrap_or(10)).await)
        })
        .handle("search:search", |ctx, args| async move {
            let query: String = args.get(0)?;
            let token: Option<String> = args.opt(1)?;
            Ok(omni_search(&ctx, &query, token).await)
        })
        .handle("notifications:getRecent", |ctx, args| async move {
            let max_rows: Option<u64> = args.opt(0)?;
            let start: Option<u64> = args.opt(1)?;
            let url = format!(
                "{}/stream-notifications/get-recent?maxRows={}&startIndex={}",
                endpoints::NOTIFICATIONS_V2,
                max_rows.unwrap_or(20),
                start.unwrap_or(0)
            );
            match ctx.state.roblox.get::<Vec<Value>>(&url).await {
                Ok(list) => Ok(list
                    .iter()
                    .filter(|e| e.get("id").is_some_and(|id| !id.is_null()) && e.get("content").is_some_and(|c| !c.is_null()))
                    .map(normalize_notification)
                    .collect::<Vec<_>>()),
                Err(error) => {
                    tracing::error!("[Notifications] Failed to get notifications: {error}");
                    Ok(Vec::new())
                }
            }
        })
        .handle("notifications:getUnreadCount", |ctx, _| async move {
            let url = format!("{}/stream-notifications/unread-count", endpoints::NOTIFICATIONS_V2);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(r) => r.get("unreadNotifications").and_then(Value::as_u64).unwrap_or(0),
                Err(error) => {
                    tracing::error!("[Notifications] Failed to get unread count: {error}");
                    0
                }
            })
        })
        .handle("notifications:markAllRead", |ctx, _| async move {
            let url = format!("{}/stream-notifications/clear-unread", endpoints::NOTIFICATIONS_V2);
            if let Err(error) = ctx.state.roblox.post_empty::<Value>(&url).await {
                tracing::error!("[Notifications] Failed to clear unread notifications: {error}");
            }
            Ok(Value::Null)
        })
        .handle("notifications:performAction", |ctx, args| async move {
            let id: String = args.get(0)?;
            let action: String = args.get(1)?;
            let url = format!(
                "{}/stream-notifications/action/{}/{}",
                endpoints::NOTIFICATIONS_V2,
                urlencoding::encode(&id),
                urlencoding::encode(&action)
            );
            if let Err(error) = ctx.state.roblox.post_empty::<Value>(&url).await {
                tracing::error!("[Notifications] Failed to perform action: {error}");
            }
            Ok(Value::Null)
        })
        .handle("privateMessages:getMessages", |ctx, args| async move {
            let tab: String = args.get(0)?;
            let page: Option<u64> = args.opt(1)?;
            let size: Option<u64> = args.opt(2)?;
            let url = format!(
                "{}/messages?messageTab={}&pageNumber={}&pageSize={}",
                endpoints::PRIVATE_MESSAGES_V1,
                urlencoding::encode(&tab),
                page.unwrap_or(0),
                size.unwrap_or(DEFAULT_PAGE_SIZE)
            );
            let user = |u: Option<&Value>| {
                let u = u.cloned().unwrap_or(Value::Null);
                json!({ "displayName": u.get("displayName"), "hasVerifiedBadge": u.get("hasVerifiedBadge"), "id": u.get("id"), "name": u.get("name") })
            };
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => json!({
                    "messages": raw.get("collection").and_then(Value::as_array).into_iter().flatten().map(|m| json!({
                        "body": m.get("body"), "created": m.get("created"), "id": m.get("id"), "isRead": m.get("isRead"),
                        "isReportAbuseDisplayed": m.get("isReportAbuseDisplayed"), "isSystemMessage": m.get("isSystemMessage"),
                        "recipient": user(m.get("recipient")), "sender": user(m.get("sender")), "subject": m.get("subject"),
                    })).collect::<Vec<_>>(),
                    "pageNumber": raw.get("pageNumber"),
                    "totalCount": raw.get("totalCollectionSize"),
                    "totalPages": raw.get("totalPages"),
                }),
                Err(error) => {
                    tracing::error!("[PrivateMessages] Failed to get messages: {error}");
                    json!({ "messages": [], "pageNumber": 0, "totalCount": 0, "totalPages": 0 })
                }
            })
        })
        .handle("privateMessages:getAnnouncements", |ctx, _| async move {
            let url = format!("{}/announcements", endpoints::PRIVATE_MESSAGES_V1);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => raw
                    .get("collection")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|a| json!({
                        "body": s(a, "body").unwrap_or(""),
                        "created": s(a, "created"),
                        "id": a.get("id").filter(|v| v.is_number()).cloned().unwrap_or(json!(0)),
                        "subject": s(a, "subject").unwrap_or("Announcement"),
                    }))
                    .collect::<Vec<_>>(),
                Err(error) => {
                    tracing::error!("[PrivateMessages] Failed to get announcements: {error}");
                    Vec::new()
                }
            })
        })
        .handle("privateMessages:getUnreadCount", |ctx, _| async move {
            let url = format!("{}/messages/unread/count", endpoints::PRIVATE_MESSAGES_V1);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(r) => r.get("count").and_then(Value::as_u64).unwrap_or(0),
                Err(error) => {
                    tracing::error!("[PrivateMessages] Failed to get unread count: {error}");
                    0
                }
            })
        })
        .handle("privateMessages:archive", |ctx, args| async move { message_action(&ctx.state, args.get(0)?, "archive", "archive").await })
        .handle("privateMessages:markRead", |ctx, args| async move { message_action(&ctx.state, args.get(0)?, "mark-read", "mark read").await })
        .handle("privateMessages:markUnread", |ctx, args| async move {
            message_action(&ctx.state, args.get(0)?, "mark-unread", "mark unread").await
        })
        .handle("events:getGroupEvents", |ctx, args| async move {
            let community: Value = args.get(0)?;
            let cursor: Option<String> = args.opt(1)?;
            let id = community.as_str().map(str::to_owned).unwrap_or_else(|| community.to_string());
            let mut url = format!("{}/virtual-events/groups/{id}", endpoints::VIRTUAL_EVENTS_V1);
            if let Some(c) = cursor.filter(|c| !c.is_empty()) {
                url.push_str(&format!("?cursor={}", urlencoding::encode(&c)));
            }
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => map_event_page(&raw),
                Err(error) => {
                    tracing::error!("Failed to fetch community events: {error}");
                    empty_event_page()
                }
            })
        })
        .handle("events:getActiveUniverseEvents", |ctx, args| async move {
            let universe: u64 = args.get(0)?;
            Ok(active_universe_events(&ctx.state, universe).await)
        })
        .handle("events:getEvent", |ctx, args| async move {
            let id: String = args.get(0)?;
            let url = format!("{}/virtual-events/{}", endpoints::VIRTUAL_EVENTS_V1, urlencoding::encode(&id));
            let response = match ctx.state.roblox.get_raw(&url).await {
                Ok(r) => r,
                Err(error) => {
                    tracing::error!("Failed to fetch event {id}: {error}");
                    return Ok(json!({ "status": "error" }));
                }
            };
            let status = response.status().as_u16();
            if !response.status().is_success() {
                let kind = match status {
                    401 => "unauthenticated",
                    404 => "notFound",
                    _ => "error",
                };
                if kind == "error" {
                    tracing::error!("Failed to fetch event {id} ({status})");
                } else {
                    tracing::warn!("Event {id} unavailable ({status}): {kind}");
                }
                return Ok(json!({ "status": kind }));
            }
            Ok(match response.json::<Value>().await {
                Ok(raw) => json!({ "status": "ok", "event": map_event(&raw) }),
                Err(_) => json!({ "status": "error" }),
            })
        })
        .handle("events:getRsvpCounters", |ctx, args| async move {
            let id: String = args.get(0)?;
            let url = format!("{}/virtual-events/{}/rsvps/counters", endpoints::VIRTUAL_EVENTS_V1, urlencoding::encode(&id));
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => json!({
                    "going": raw.pointer("/counters/going").cloned().unwrap_or(json!(0)),
                    "maybeGoing": raw.pointer("/counters/maybeGoing").cloned().unwrap_or(json!(0)),
                    "none": raw.pointer("/counters/none").cloned().unwrap_or(json!(0)),
                    "notGoing": raw.pointer("/counters/notGoing").cloned().unwrap_or(json!(0)),
                }),
                Err(error) => {
                    tracing::error!("Failed to fetch RSVP counters for event {id}: {error}");
                    Value::Null
                }
            })
        })
        .handle("events:setRsvp", |ctx, args| async move {
            let id: String = args.get(0)?;
            let status: String = args.get(1)?;
            let url = format!("{}/virtual-events/{}/rsvps", endpoints::VIRTUAL_EVENTS_V1, urlencoding::encode(&id));
            match ctx.state.roblox.post_raw(&url, &json!({ "rsvpStatus": status })).await {
                Ok(code) if (200..300).contains(&code) => Ok(json!({ "ok": true })),
                Ok(code) => {
                    let error = match code {
                        401 => "unauthenticated",
                        403 => "forbidden",
                        429 => "rateLimited",
                        _ => "error",
                    };
                    if code == 401 {
                        tracing::warn!("RSVP to event {id} rejected: signed out");
                    } else {
                        tracing::error!("Failed to RSVP to event {id} ({code}): {error}");
                    }
                    Ok(json!({ "ok": false, "error": error }))
                }
                Err(error) => {
                    tracing::error!("Failed to RSVP to event {id}: {error}");
                    Ok(json!({ "ok": false, "error": "error" }))
                }
            }
        });
}

async fn message_action(
    state: &AppState,
    ids: Vec<u64>,
    path: &str,
    label: &str,
) -> crate::error::AppResult<Value> {
    if ids.is_empty() {
        return Ok(Value::Null);
    }
    let url = format!("{}/messages/{path}", endpoints::PRIVATE_MESSAGES_V1);
    if let Err(error) = state
        .roblox
        .post::<_, Value>(&url, &json!({ "messageIds": ids }))
        .await
    {
        tracing::error!("[PrivateMessages] Failed to {label} messages: {error}");
    }
    Ok(Value::Null)
}
