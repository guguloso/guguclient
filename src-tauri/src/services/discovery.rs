//! Home feed (`discovery:*`). Port of `DiscoveryService`.
//!
//! Builds the same `{ sections: [...] }` structure from Roblox's
//! omni-recommendation API: game carousels/grids (with grid overflow carried
//! into the next section of the same topic), the friends carousel, layout
//! metadata (subtitles, links, countdowns) and artwork enrichment.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::AppResult;
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints;
use crate::services::{friends, games};

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const CACHE_MAX_ENTRIES: usize = 4;
const ICON_TOPIC_IDS: [i64; 2] = [100_000_001, 100_000_003];
const ICON_TOPIC_NAMES: [&str; 2] = ["continue", "favorites"];
const LINK_START: &str = "{linkStart}";
const LINK_END: &str = "{linkEnd}";
const GRID_COLUMNS: usize = 5;
const MAX_FRIEND_ITEMS: usize = 24;

#[derive(Default)]
pub struct DiscoveryState {
    session_id: Mutex<Option<String>>,
    cache: Mutex<HashMap<String, (Instant, Arc<Value>)>>,
    /// Serializes feed builds so concurrent callers share one result.
    build_lock: tokio::sync::Mutex<()>,
}

// ----------------------------------------------------------------- helpers

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn layout_text(sort: &Value, key: &str) -> Option<String> {
    text(sort.get("topicLayoutData").and_then(|l| l.get(key)))
}

fn layout_flag(sort: &Value, key: &str) -> bool {
    layout_text(sort, key).as_deref() == Some("true")
}

fn resolve_layout_url(path: Option<String>) -> Option<String> {
    let path = path?;
    let base = reqwest::Url::parse(endpoints::WEB).ok()?;
    let url = base.join(&path).ok()?;
    let host = url.host_str()?;
    let roblox = host == "roblox.com" || host.ends_with(".roblox.com");
    (url.scheme() == "https" && roblox).then(|| url.to_string())
}

fn strip_markers(s: &str) -> String {
    s.replace(LINK_START, "").replace(LINK_END, "")
}

/// Port of `splitHomeSubtitleLink`.
fn split_subtitle(raw: Option<String>, url: Option<&str>) -> (Option<String>, Option<Value>) {
    let Some(raw) = raw else { return (None, None) };
    let (start, end) = (raw.find(LINK_START), raw.find(LINK_END));
    let (Some(start), Some(end)) = (start, end) else {
        return (text(Some(&Value::String(strip_markers(&raw)))), None);
    };
    if start >= end {
        return (text(Some(&Value::String(strip_markers(&raw)))), None);
    }
    let prefix = strip_markers(&raw[..start]);
    let link_text = strip_markers(&raw[start + LINK_START.len()..end]);
    let suffix = strip_markers(&raw[end + LINK_END.len()..]);
    let subtitle = text(Some(&Value::String(format!("{prefix}{link_text}{suffix}"))));
    match (url, &subtitle, link_text.trim().is_empty()) {
        (Some(url), Some(_), false) => {
            let link = json!({ "prefix": prefix, "text": link_text, "suffix": suffix, "url": url });
            (subtitle, Some(link))
        }
        _ => (subtitle, None),
    }
}

/// Port of `normalizeHomeSectionLayout`; `undefined` fields are omitted.
fn section_layout(sort: &Value, out: &mut Map<String, Value>) {
    let see_all = resolve_layout_url(layout_text(sort, "linkPath"));
    let subtitle_url =
        resolve_layout_url(layout_text(sort, "subtitleLinkPath")).or_else(|| see_all.clone());

    let (subtitle, link) = split_subtitle(text(sort.get("subtitle")), subtitle_url.as_deref());
    if let Some(s) = subtitle {
        out.insert("subtitle".into(), json!(s));
    }
    if let Some(l) = link {
        out.insert("subtitleLink".into(), l);
    }

    let end_seconds = layout_text(sort, "endTimestamp").and_then(|s| s.parse::<i64>().ok());
    let (cd_subtitle, cd_link) = split_subtitle(
        layout_text(sort, "countdownString"),
        subtitle_url.as_deref(),
    );
    if let (Some(end), Some(subtitle)) = (end_seconds, cd_subtitle) {
        let mut countdown = Map::new();
        countdown.insert("endsAt".into(), json!(end * 1000));
        countdown.insert("subtitle".into(), json!(subtitle));
        if let Some(l) = cd_link {
            countdown.insert("subtitleLink".into(), l);
        }
        out.insert("countdown".into(), Value::Object(countdown));
    }
    out.insert("hideSeeAll".into(), json!(layout_flag(sort, "hideSeeAll")));
    if let Some(info) = layout_text(sort, "infoText") {
        out.insert("infoText".into(), json!(info));
    }
    if let Some(url) = see_all {
        out.insert("seeAllUrl".into(), json!(url));
    }
}

fn topic_id(sort: &Value) -> Option<i64> {
    sort.get("topicId").and_then(Value::as_i64)
}

fn section_id(sort: &Value, kind: &str, index: usize) -> String {
    let topic_id = topic_id(sort).map_or_else(|| "unknown".to_owned(), |t| t.to_string());
    let base = match text(sort.get("subId")) {
        Some(sub) => format!("{kind}:{topic_id}:{sub}"),
        None => format!(
            "{kind}:{topic_id}:{}",
            text(sort.get("topic")).unwrap_or_else(|| "untitled".into())
        ),
    };
    format!("{base}:{index}")
}

fn group_key(sort: &Value, kind: &str) -> String {
    let topic_id = topic_id(sort).map_or_else(|| "unknown".to_owned(), |t| t.to_string());
    format!(
        "{kind}:{topic_id}:{}",
        text(sort.get("topic")).unwrap_or_else(|| "untitled".into())
    )
}

fn artwork_type(sort: &Value) -> &'static str {
    let topic = text(sort.get("topic")).unwrap_or_default().to_lowercase();
    let by_id = topic_id(sort).is_some_and(|id| ICON_TOPIC_IDS.contains(&id));
    if by_id || ICON_TOPIC_NAMES.contains(&topic.as_str()) {
        "icon"
    } else {
        "thumbnail"
    }
}

fn is_sponsored(recommendation: &Value) -> bool {
    recommendation.get("contentMetadata").is_some_and(|m| {
        ["EncryptedAdTrackingData", "ad_id", "PayerName"]
            .iter()
            .any(|k| m.get(*k).is_some_and(|v| !v.is_null()))
    })
}

fn approval(up: u64, down: u64) -> u64 {
    let total = up + down;
    if total == 0 {
        0
    } else {
        ((up as f64 / total as f64) * 100.0).round() as u64
    }
}

fn game_item(recommendation: &Value, metadata: &Map<String, Value>) -> Option<Value> {
    if recommendation.get("contentType").and_then(Value::as_str) != Some("Game") {
        return None;
    }
    let content_id = recommendation
        .get("contentId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)?;
    let meta = metadata.get(&content_id.to_string())?;
    let universe_id = meta
        .get("universeId")
        .and_then(Value::as_u64)
        .unwrap_or(content_id);
    let place_id = meta.get("rootPlaceId").and_then(Value::as_u64).unwrap_or(0);
    let name = text(meta.get("name")).unwrap_or_default();
    if !(universe_id > 0 && place_id > 0 && !name.is_empty()) {
        return None;
    }
    let up = meta
        .get("totalUpVotes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let down = meta
        .get("totalDownVotes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(json!({
        "universeId": universe_id,
        "placeId": place_id,
        "name": name,
        "playerCount": meta.get("playerCount").and_then(Value::as_u64).unwrap_or(0),
        "rating": approval(up, down),
        "thumbnailUrl": null,
        "minimumAge": meta.get("minimumAge").and_then(Value::as_u64).unwrap_or(0),
        "ageRecommendationDisplayName": text(meta.get("ageRecommendationDisplayName")).unwrap_or_default(),
        "isSponsored": is_sponsored(recommendation),
    }))
}

fn universe_of(item: &Value) -> u64 {
    item.get("universeId").and_then(Value::as_u64).unwrap_or(0)
}

fn collect_items(sort: &Value, metadata: &Map<String, Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    sort.get("recommendationList")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| game_item(r, metadata))
        .filter(|item| seen.insert(universe_of(item)))
        .collect()
}

fn merge_items(primary: Vec<Value>, secondary: Vec<Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    primary
        .into_iter()
        .chain(secondary)
        .filter(|item| seen.insert(universe_of(item)))
        .collect()
}

/// `splitHomeGridItems` / `limitHomeGridItems`: rows * 5 visible, rest carried.
fn split_grid(mut items: Vec<Value>, rows: Option<&Value>) -> (Vec<Value>, Vec<Value>) {
    let rows = match rows {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    match rows {
        Some(r) if r >= 0 => {
            let visible = (r as usize) * GRID_COLUMNS;
            if visible >= items.len() {
                (items, Vec::new())
            } else {
                let overflow = items.split_off(visible);
                (items, overflow)
            }
        }
        _ => (items, Vec::new()),
    }
}

fn friend_status(presence_type: u64) -> &'static str {
    match presence_type {
        2 => "in-game",
        3 => "in-studio",
        1 => "online",
        _ => "offline",
    }
}

fn friend_weight(status: &str) -> u8 {
    match status {
        "in-game" => 4,
        "in-studio" => 3,
        "online" => 2,
        _ => 1,
    }
}

// ----------------------------------------------------------------- service

impl DiscoveryState {
    fn session_id(&self, reset: bool) -> String {
        let mut slot = self.session_id.lock();
        if reset || slot.is_none() {
            *slot = Some(uuid::Uuid::new_v4().to_string());
        }
        slot.clone().unwrap_or_default()
    }

    fn cached(&self, key: &str) -> Option<Arc<Value>> {
        let cache = self.cache.lock();
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, feed)| Arc::clone(feed))
    }

    fn store(&self, key: String, feed: Arc<Value>) {
        let mut cache = self.cache.lock();
        cache.insert(key, (Instant::now(), feed));
        while cache.len() > CACHE_MAX_ENTRIES {
            let oldest = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => cache.remove(&k),
                None => break,
            };
        }
    }
}

async fn home_feed(ctx: &Ctx, force_refresh: bool) -> AppResult<Arc<Value>> {
    let discovery = &ctx.state.discovery;
    let key = ctx
        .state
        .auth
        .current_user_id()
        .map_or_else(|| "guest".to_owned(), |id| format!("user:{id}"));
    if !force_refresh {
        if let Some(feed) = discovery.cached(&key) {
            return Ok(feed);
        }
    }
    let _guard = discovery.build_lock.lock().await;
    if !force_refresh {
        if let Some(feed) = discovery.cached(&key) {
            return Ok(feed);
        }
    }
    let feed = Arc::new(build_home_feed(ctx).await?);
    discovery.store(key, Arc::clone(&feed));
    Ok(feed)
}

async fn fetch_discovery(ctx: &Ctx) -> AppResult<Value> {
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let resolution = ctx
        .window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let size = m.size().to_logical::<f64>(m.scale_factor());
            format!("{}x{}", size.width.round(), size.height.round())
        })
        .unwrap_or_else(|| "1920x1080".to_owned());
    let url = format!("{}/omni-recommendation", endpoints::DISCOVERY_API);
    let body = json!({
        "cpuCores": cpu,
        "maxResolution": resolution,
        "pageType": "Home",
        "sduiTreatmentTypes": ["Carousel", "HeroUnit"],
        "sessionId": ctx.state.discovery.session_id(false),
        "supportedTreatmentTypes": ["SortlessGrid"],
    });
    let response: Value = ctx.state.roblox.post(&url, &body).await?;
    if response.get("isSessionExpired").and_then(Value::as_bool) == Some(true) {
        ctx.state.discovery.session_id(true);
    }
    Ok(response)
}

async fn friend_items(ctx: &Ctx) -> Vec<Value> {
    let Ok(Some(user)) = ctx.state.auth.authenticated_user().await else {
        return Vec::new();
    };
    let Some(me) = user.get("id").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let friends = friends::list_friends(&ctx.state, me).await;
    if friends.is_empty() {
        return Vec::new();
    }
    let ids: Vec<u64> = friends
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_u64))
        .collect();
    let (presences, avatars) = futures::join!(
        ctx.state.auth.presence(&ids),
        ctx.state
            .auth
            .headshots(&ctx.state.thumbnails, &ids, "150x150", true),
    );
    let presence_by_user: HashMap<u64, Value> = presences
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "Failed to load Home friend presence data");
            Vec::new()
        })
        .into_iter()
        .filter_map(|p| Some((p.get("userId").and_then(Value::as_u64)?, p)))
        .collect();
    let avatars = avatars.unwrap_or_default();

    let mut items: Vec<(u8, String, Value)> = friends
        .iter()
        .filter_map(|friend| {
            if friend.get("isDeleted").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let id = friend.get("id").and_then(Value::as_u64)?;
            let name = text(friend.get("name")).unwrap_or_default();
            let display = text(friend.get("displayName")).unwrap_or_else(|| name.clone());
            if name.is_empty() && display.is_empty() {
                return None;
            }
            let presence = presence_by_user.get(&id);
            let presence_type = presence
                .and_then(|p| p.get("userPresenceType"))
                .or_else(|| friend.get("presenceType"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let status = friend_status(presence_type);
            let location = presence.and_then(|p| text(p.get("lastLocation")));
            let status_text = match status {
                "in-game" => location.unwrap_or_else(|| "In Game".into()),
                "in-studio" => location.unwrap_or_else(|| "In Studio".into()),
                "online" => location.unwrap_or_else(|| "Online".into()),
                _ => "Offline".into(),
            };
            let mut item = Map::new();
            item.insert("userId".into(), json!(id));
            item.insert(
                "name".into(),
                json!(if name.is_empty() { &display } else { &name }),
            );
            item.insert(
                "displayName".into(),
                json!(if display.is_empty() { &name } else { &display }),
            );
            item.insert(
                "avatarUrl".into(),
                avatars
                    .get(&id.to_string())
                    .map_or(Value::Null, |u| json!(u)),
            );
            item.insert("status".into(), json!(status));
            item.insert("statusText".into(), json!(status_text));
            if let Some(u) = presence
                .and_then(|p| p.get("universeId"))
                .filter(|v| !v.is_null())
            {
                item.insert("universeId".into(), u.clone());
            }
            if let Some(p) = presence
                .and_then(|p| {
                    p.get("placeId")
                        .filter(|v| !v.is_null())
                        .or_else(|| p.get("rootPlaceId"))
                })
                .filter(|v| !v.is_null())
            {
                item.insert("placeId".into(), p.clone());
            }
            Some((friend_weight(status), display, Value::Object(item)))
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    items
        .into_iter()
        .take(MAX_FRIEND_ITEMS)
        .map(|(_, _, item)| item)
        .collect()
}

async fn build_home_feed(ctx: &Ctx) -> AppResult<Value> {
    // Friends load eagerly, in parallel with the recommendation request.
    let authenticated = ctx.state.auth.is_authenticated();
    let (friends, discovery) = futures::join!(
        async {
            if authenticated {
                friend_items(ctx).await
            } else {
                Vec::new()
            }
        },
        fetch_discovery(ctx),
    );
    let mut discovery = discovery?;
    let sorts = match discovery.get_mut("sorts").map(Value::take) {
        Some(Value::Array(list)) => list,
        _ => Vec::new(),
    };
    let metadata = match discovery
        .pointer_mut("/contentMetadata/Game")
        .map(Value::take)
    {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };

    let mut sections: Vec<Value> = Vec::new();
    let mut overflow: HashMap<String, Vec<Value>> = HashMap::new();
    for (index, sort) in sorts.iter().enumerate() {
        match sort.get("treatmentType").and_then(Value::as_str) {
            Some(treatment @ ("Carousel" | "SortlessGrid")) => {
                let kind = if treatment == "Carousel" {
                    "games-carousel"
                } else {
                    "games-grid"
                };
                let key = group_key(sort, kind);
                let own = collect_items(sort, &metadata);
                let source = match overflow.remove(&key) {
                    Some(carried) if !carried.is_empty() => merge_items(carried, own),
                    _ => own,
                };
                let (visible, rest) = if kind == "games-grid" {
                    split_grid(source, sort.get("numberOfRows"))
                } else {
                    (source, Vec::new())
                };
                if !rest.is_empty() {
                    overflow.insert(key, rest);
                }
                if visible.is_empty() {
                    continue;
                }
                let mut section = Map::new();
                section.insert("id".into(), json!(section_id(sort, kind, index)));
                section.insert("kind".into(), json!(kind));
                section.insert(
                    "title".into(),
                    json!(text(sort.get("topic")).unwrap_or_else(|| "Recommended".into())),
                );
                section_layout(sort, &mut section);
                section.insert("artworkType".into(), json!(artwork_type(sort)));
                section.insert(
                    "hideTileMetadata".into(),
                    json!(layout_flag(sort, "hideTileMetadata")),
                );
                section.insert("items".into(), Value::Array(visible));
                sections.push(Value::Object(section));
            }
            Some("FriendCarousel") if !friends.is_empty() => {
                let mut section = Map::new();
                section.insert(
                    "id".into(),
                    json!(section_id(sort, "friends-carousel", index)),
                );
                section.insert("kind".into(), json!("friends-carousel"));
                section.insert(
                    "title".into(),
                    json!(text(sort.get("topic")).unwrap_or_else(|| "Friends".into())),
                );
                section_layout(sort, &mut section);
                section.insert("items".into(), Value::Array(friends.clone()));
                sections.push(Value::Object(section));
            }
            other => {
                tracing::debug!(treatment = ?other, "Skipping unsupported Home treatment type")
            }
        }
    }

    with_artwork(ctx, &mut sections).await;
    Ok(json!({ "sections": sections }))
}

/// `withHomeGameThumbnails`: icons for icon sections, wide art (falling back
/// to icons) for the rest. All three lookups run concurrently.
async fn with_artwork(ctx: &Ctx, sections: &mut [Value]) {
    let mut icon_ids = Vec::new();
    let mut wide_ids = Vec::new();
    for section in sections.iter() {
        if section.get("kind").and_then(Value::as_str) == Some("friends-carousel") {
            continue;
        }
        let icon = section.get("artworkType").and_then(Value::as_str) == Some("icon");
        let target = if icon { &mut icon_ids } else { &mut wide_ids };
        for item in section
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            target.push(universe_of(item));
        }
    }
    if icon_ids.is_empty() && wide_ids.is_empty() {
        return;
    }
    let (icons, wide, fallback) = futures::join!(
        games::icons(ctx, &icon_ids, "512x512"),
        games::wide_thumbnails(ctx, &wide_ids, "576x324"),
        games::icons(ctx, &wide_ids, "512x512"),
    );
    let icons = icons.unwrap_or_default();
    let wide = wide.unwrap_or_default();
    let fallback = fallback.unwrap_or_default();
    for section in sections.iter_mut() {
        if section.get("kind").and_then(Value::as_str) == Some("friends-carousel") {
            continue;
        }
        let icon = section.get("artworkType").and_then(Value::as_str) == Some("icon");
        let Some(Value::Array(items)) = section.get_mut("items") else {
            continue;
        };
        for item in items.iter_mut() {
            let id = universe_of(item).to_string();
            let url = if icon {
                icons.get(&id)
            } else {
                wide.get(&id).or_else(|| fallback.get(&id))
            };
            if let Some(url) = url {
                item["thumbnailUrl"] = json!(url);
            }
        }
    }
}

fn is_continue_section(section: &Value) -> bool {
    if section.get("kind").and_then(Value::as_str) == Some("friends-carousel") {
        return false;
    }
    let haystack = format!(
        "{} {}",
        section
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        section
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_lowercase();
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "continue")
}

pub fn register(router: &mut Router) {
    router
        .handle("discovery:getHomeFeed", |ctx, args| async move {
            let force = args.opt::<bool>(0)?.unwrap_or(false);
            home_feed(&ctx, force).await
        })
        .handle("discovery:getRecentGames", |ctx, args| async move {
            let force = args.opt::<bool>(0)?.unwrap_or(false);
            recent_games(&ctx, force).await
        });
}

/// `getRecentGames`: the "Continue" section of the home feed.
pub async fn recent_games(ctx: &Ctx, force: bool) -> AppResult<Value> {
    if !ctx.state.auth.is_authenticated() {
        return Ok(json!([]));
    }
    let feed = home_feed(ctx, force).await?;
    let items = feed
        .get("sections")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find(|s| is_continue_section(s)))
        .and_then(|s| s.get("items"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_link_split() {
        let (subtitle, link) = split_subtitle(
            Some("Play {linkStart}now{linkEnd}!".into()),
            Some("https://www.roblox.com/x"),
        );
        assert_eq!(subtitle.as_deref(), Some("Play now!"));
        assert_eq!(link.unwrap()["text"], "now");
    }

    #[test]
    fn grid_overflow() {
        let items: Vec<Value> = (1..=12).map(|i| json!({ "universeId": i })).collect();
        let (visible, rest) = split_grid(items, Some(&json!(2)));
        assert_eq!((visible.len(), rest.len()), (10, 2));
    }

    #[test]
    fn only_roblox_links() {
        assert!(resolve_layout_url(Some("/games".into()))
            .unwrap()
            .starts_with("https://www.roblox.com/"));
        assert!(resolve_layout_url(Some("https://evil.example".into())).is_none());
    }
}
