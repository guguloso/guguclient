//! Charts, servers and per-game social state (`games:*`, second half).
//! Port of the rest of `GamesService`: explore sorts (the Charts page),
//! sort content, promoted games, playability, votes, follow notifications,
//! friends playing, server lists and social links.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::future::try_join_all;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints;
use crate::services::games;
use crate::services::metadata::gated_installation_id;

const SORTS_TTL: Duration = Duration::from_secs(5 * 60);
const SORT_CONTENT_TTL: Duration = Duration::from_secs(10 * 60);
const FRIENDS_PLAYING_TTL: Duration = Duration::from_secs(60);
const SOCIAL_LINKS_TTL: Duration = Duration::from_secs(5 * 60);
const SOCIAL_LINKS_MAX: usize = 100;
const PROMOTED_GAMES_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_EXPLORE_SORT_PAGES: usize = 25;
const PLAYABILITY_BATCH: usize = 50;
/// `EXPLORE_SORT_CONTENT_CONTEXT`.
const SORT_CONTENT_CONTEXT: [(&str, &str); 7] = [
    ("age", "all"),
    ("country", "us"),
    ("device", "computer"),
    ("page", "gamesPage"),
    ("position", "1"),
    ("cpuCores", "16"),
    ("maxResolution", "1920x1080"),
];

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExploreGame {
    universe_id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    root_place_id: u64,
    #[serde(default)]
    player_count: u64,
    total_up_votes: Option<u64>,
    total_down_votes: Option<u64>,
    is_sponsored: Option<bool>,
    native_ad_data: Option<String>,
    minimum_age: Option<u64>,
    age_recommendation_display_name: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExploreSort {
    sort_id: String,
    #[serde(default)]
    sort_display_name: String,
    subtitle: Option<String>,
    #[serde(default)]
    games: Vec<ExploreGame>,
    game_set_target_id: Option<u64>,
    game_set_type_id: Option<u64>,
    treatment_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SortsPage {
    #[serde(default)]
    sorts: Vec<Value>,
    next_sorts_page_token: Option<String>,
}

type Cached<T> = Mutex<Option<(Instant, T)>>;

#[derive(Default)]
pub struct GameListsState {
    session_id: Mutex<Option<String>>,
    sorts: Cached<Vec<ExploreSort>>,
    sort_content: Mutex<HashMap<String, (Instant, Value)>>,
    promoted: tokio::sync::Mutex<Option<(Instant, Value)>>,
    friends_playing: Mutex<HashMap<u64, (Instant, Value)>>,
    social_links: Mutex<HashMap<u64, (Instant, Value)>>,
}

impl GameListsState {
    pub fn clear_cache(&self) {
        *self.sorts.lock() = None;
        self.sort_content.lock().clear();
    }
}

fn session_id(ctx: &Ctx) -> String {
    ctx.state
        .game_lists
        .session_id
        .lock()
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn fresh<T: Clone>(slot: &Cached<T>, ttl: Duration) -> Option<T> {
    slot.lock()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, v)| v.clone())
}

fn query_string(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `fetchExploreSorts`: every page, minus "filters", de-duplicated.
async fn explore_sorts(ctx: &Ctx) -> AppResult<Vec<ExploreSort>> {
    if let Some(sorts) = fresh(&ctx.state.game_lists.sorts, SORTS_TTL) {
        return Ok(sorts);
    }
    let session = session_id(ctx);
    let mut collected = Vec::new();
    let mut seen = HashSet::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_EXPLORE_SORT_PAGES {
        let mut params = vec![("sessionId", session.clone())];
        if let Some(token) = &token {
            params.push(("sortsPageToken", token.clone()));
        }
        let url = format!(
            "{}/get-sorts?{}",
            endpoints::EXPLORE_API,
            query_string(&params)
        );
        let page: SortsPage = ctx.state.roblox.get(&url).await?;
        for raw in page.sorts {
            let Ok(sort) = serde_json::from_value::<ExploreSort>(raw) else {
                continue;
            };
            if sort.sort_id == "filters" || !seen.insert(sort.sort_id.clone()) {
                continue;
            }
            collected.push(sort);
        }
        token = page.next_sorts_page_token.filter(|t| !t.is_empty());
        if token.is_none() {
            break;
        }
    }
    *ctx.state.game_lists.sorts.lock() = Some((Instant::now(), collected.clone()));
    Ok(collected)
}

async fn sort_content(
    ctx: &Ctx,
    sort_id: &str,
    sort: Option<&ExploreSort>,
) -> Option<Vec<ExploreGame>> {
    let session = session_id(ctx);
    let mut params = vec![
        ("sessionId", session.clone()),
        ("sortId", sort_id.to_owned()),
        ("discoverPageSessionInfo", session),
    ];
    if let Some(sort) = sort {
        if let Some(id) = sort.game_set_target_id {
            params.push(("gameSetTargetId", id.to_string()));
        }
        if let Some(id) = sort.game_set_type_id {
            params.push(("gameSetTypeId", id.to_string()));
        }
        if let Some(t) = &sort.treatment_type {
            params.push(("treatmentType", t.clone()));
        }
    }
    params.extend(
        SORT_CONTENT_CONTEXT
            .iter()
            .map(|(k, v)| (*k, (*v).to_owned())),
    );
    let url = format!(
        "{}/get-sort-content?{}",
        endpoints::EXPLORE_API,
        query_string(&params)
    );
    match ctx.state.roblox.get::<ExploreSort>(&url).await {
        Ok(sort) => Some(sort.games),
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch full sort content");
            None
        }
    }
}

/// `convertExploreGamesToListItems`.
fn to_list_item(game: &ExploreGame) -> Value {
    let mut item = json!({
        "creatorId": 0,
        "creatorName": "",
        "creatorType": "",
        "creatorHasVerifiedBadge": false,
        "universeId": game.universe_id,
        "name": game.name,
        "placeId": game.root_place_id,
        "playerCount": game.player_count,
        "imageToken": "",
        "price": null,
        "analyticsIdentifier": null,
        "gameDescription": "",
        "genre": "",
    });
    let fields: [(&str, Option<Value>); 7] = [
        ("totalUpVotes", game.total_up_votes.map(Value::from)),
        ("totalDownVotes", game.total_down_votes.map(Value::from)),
        ("isSponsored", game.is_sponsored.map(Value::from)),
        ("nativeAdData", game.native_ad_data.clone().map(Value::from)),
        ("isShowSponsoredLabel", game.is_sponsored.map(Value::from)),
        ("minimumAge", game.minimum_age.map(Value::from)),
        (
            "ageRecommendationDisplayName",
            game.age_recommendation_display_name
                .clone()
                .map(Value::from),
        ),
    ];
    for (key, value) in fields {
        if let Some(value) = value {
            item[key] = value;
        }
    }
    item
}

async fn games_by_sort(ctx: &Ctx, sort_id: String, limit: usize) -> AppResult<Value> {
    let key = format!("{sort_id}:{limit}");
    if let Some((at, games)) = ctx.state.game_lists.sort_content.lock().get(&key) {
        if at.elapsed() < SORT_CONTENT_TTL {
            return Ok(games.clone());
        }
    }
    let sorts = explore_sorts(ctx).await?;
    let sort = sorts.iter().find(|s| s.sort_id == sort_id);
    let full = sort_content(ctx, &sort_id, sort).await;
    let source = full
        .as_deref()
        .or(sort.map(|s| s.games.as_slice()))
        .unwrap_or_default();
    let games: Vec<Value> = source.iter().take(limit).map(to_list_item).collect();
    let games = Value::Array(games);
    if games.as_array().is_some_and(|g| !g.is_empty()) {
        ctx.state
            .game_lists
            .sort_content
            .lock()
            .insert(key, (Instant::now(), games.clone()));
    }
    Ok(games)
}

async fn promoted_games(ctx: &Ctx, force_refresh: bool) -> AppResult<Value> {
    let mut slot = ctx.state.game_lists.promoted.lock().await;
    if !force_refresh {
        if let Some((at, games)) = slot.as_ref() {
            if at.elapsed() < PROMOTED_GAMES_TTL {
                return Ok(games.clone());
            }
        }
    }
    let installation_id = gated_installation_id(&ctx.state);
    let query = [
        ("installation_id", installation_id.as_str()),
        ("version", env!("CARGO_PKG_VERSION")),
    ];
    let mut response: Value = ctx
        .state
        .metadata
        .api()
        .get("/v1/games/promoted", &query)
        .await?;
    let games = response
        .get_mut("games")
        .map(Value::take)
        .unwrap_or(Value::Null);
    *slot = Some((Instant::now(), games.clone()));
    Ok(games)
}

async fn playability(ctx: &Ctx, universe_ids: Vec<u64>) -> AppResult<Value> {
    let ids: Vec<u64> = universe_ids.into_iter().filter(|id| *id > 0).collect();
    let pages = try_join_all(ids.chunks(PLAYABILITY_BATCH).map(|chunk| {
        let url = format!(
            "{}/games/multiget-playability-status?{}",
            endpoints::GAMES_V1,
            endpoints::repeat_param("universeIds", chunk)
        );
        async move { ctx.state.roblox.get::<Vec<Value>>(&url).await }
    }))
    .await?;
    let mut out = serde_json::Map::new();
    for entry in pages.into_iter().flatten() {
        let Some(id) = entry.get("universeId").and_then(Value::as_u64) else {
            continue;
        };
        out.insert(
            id.to_string(),
            json!({
                "universeId": id,
                "isPlayable": entry.get("isPlayable"),
                "playabilityStatus": entry.get("playabilityStatus"),
                "unplayableDisplayText": entry.get("unplayableDisplayText"),
            }),
        );
    }
    Ok(Value::Object(out))
}

/// `normalizeVote`.
fn normalize_vote(raw: &Value) -> Value {
    let model = raw.get("model").filter(|m| !m.is_null()).unwrap_or(raw);
    let user_vote = if model.get("hasVoted") == Some(&Value::Bool(false)) {
        Value::Null
    } else {
        model.get("userVote").cloned().unwrap_or(Value::Null)
    };
    json!({
        "canVote": model.get("canVote").and_then(Value::as_bool).unwrap_or(false),
        "reasonForNotVoteable": model.get("reasonForNotVoteable").and_then(Value::as_str).unwrap_or(""),
        "userVote": user_vote,
    })
}

async fn get_vote(ctx: &Ctx, universe_id: u64) -> Option<Value> {
    if universe_id == 0 {
        return None;
    }
    let url = format!("{}/games/{universe_id}/votes/user", endpoints::GAMES_V1);
    match ctx.state.roblox.get::<Value>(&url).await {
        Ok(raw) => Some(normalize_vote(&raw)),
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch game vote");
            None
        }
    }
}

async fn set_vote(ctx: &Ctx, place_id: u64, vote: Option<bool>) -> AppResult<Option<Value>> {
    if place_id == 0 {
        return Ok(None);
    }
    let param = match vote {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
    };
    let url = format!(
        "{}/vote/asset/{place_id}?vote={param}",
        endpoints::VOTING_API
    );
    let raw: Value = ctx.state.roblox.post(&url, &json!({})).await?;
    let action = match vote {
        Some(true) => "like",
        Some(false) => "dislike",
        None => "none",
    };
    ctx.state
        .track("game.vote", json!({ "place_id": place_id, "vote": action }));
    Ok(Some(normalize_vote(&raw)))
}

const FOLLOWINGS_V1: &str = "https://followings.roblox.com/v1";

async fn notification_status(ctx: &Ctx, universe_id: u64) -> Option<Value> {
    let user_id = ctx.state.auth.current_user_id()?;
    if universe_id == 0 {
        return None;
    }
    let url = format!("{FOLLOWINGS_V1}/users/{user_id}/universes/{universe_id}/status");
    match ctx.state.roblox.get::<Value>(&url).await {
        Ok(raw) => Some(
            json!({ "canFollow": raw.get("CanFollow"), "isFollowing": raw.get("IsFollowing") }),
        ),
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch game notification status");
            None
        }
    }
}

async fn set_notification(ctx: &Ctx, universe_id: u64, enabled: bool) -> AppResult<bool> {
    let user_id = ctx
        .state
        .auth
        .current_user_id()
        .ok_or_else(|| AppError::msg("No active account to update notifications for."))?;
    let url = format!("{FOLLOWINGS_V1}/users/{user_id}/universes/{universe_id}");
    if enabled {
        ctx.state.roblox.post_empty::<Value>(&url).await?;
    } else {
        ctx.state.roblox.delete::<Value>(&url).await?;
    }
    ctx.state.track(
        "game.notify",
        json!({ "universe_id": universe_id, "enabled": enabled }),
    );
    Ok(true)
}

async fn friends_playing(ctx: &Ctx, user_id: u64) -> AppResult<Value> {
    if let Some((at, entries)) = ctx.state.game_lists.friends_playing.lock().get(&user_id) {
        if at.elapsed() < FRIENDS_PLAYING_TTL {
            return Ok(entries.clone());
        }
    }
    let url = format!("{}/users/{user_id}/friends", endpoints::FRIENDS_V1);
    let mut body: Value = ctx.state.roblox.get(&url).await?;
    let friends = match body.get_mut("data").map(Value::take) {
        Some(Value::Array(list)) => list,
        _ => Vec::new(),
    };
    let mut entries = Vec::new();
    if !friends.is_empty() {
        let ids: Vec<u64> = friends
            .iter()
            .filter_map(|f| f.get("id").and_then(Value::as_u64))
            .collect();
        let presences = ctx.state.auth.presence(&ids).await?;
        let in_game: Vec<&Value> = presences
            .iter()
            .filter(|p| {
                p.get("userPresenceType").and_then(Value::as_u64) == Some(2)
                    && p.get("universeId")
                        .and_then(Value::as_u64)
                        .is_some_and(|u| u > 0)
            })
            .collect();
        if !in_game.is_empty() {
            let mut universes = Vec::new();
            let mut seen = HashSet::new();
            for p in &in_game {
                if let Some(u) = p.get("universeId").and_then(Value::as_u64) {
                    if seen.insert(u) {
                        universes.push(u);
                    }
                }
            }
            let details = games::details(ctx, &universes).await?;
            let by_id: HashMap<u64, &Value> = details
                .iter()
                .filter_map(|g| Some((g.get("id")?.as_u64()?, g)))
                .collect();
            let friends_by_id: HashMap<u64, &Value> = friends
                .iter()
                .filter_map(|f| Some((f.get("id")?.as_u64()?, f)))
                .collect();
            for presence in in_game {
                let (Some(uid), Some(universe)) = (
                    presence.get("userId").and_then(Value::as_u64),
                    presence.get("universeId").and_then(Value::as_u64),
                ) else {
                    continue;
                };
                if let Some(friend) = friends_by_id.get(&uid) {
                    entries.push(json!({
                        "friend": friend,
                        "presence": presence,
                        "game": by_id.get(&universe).copied().cloned().unwrap_or(Value::Null),
                    }));
                }
            }
        }
    }
    let entries = Value::Array(entries);
    ctx.state
        .game_lists
        .friends_playing
        .lock()
        .insert(user_id, (Instant::now(), entries.clone()));
    Ok(entries)
}

fn trimmed(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `normalizeServer`.
fn normalize_server(server: &Value, source: &str, place_id: u64) -> Value {
    let playing = server.get("playing").and_then(Value::as_u64).unwrap_or(0);
    let max_players = server
        .get("maxPlayers")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let vip = server.get("vipServerId").filter(|v| !v.is_null());
    let access_code = trimmed(server.get("accessCode"));
    let id = trimmed(server.get("id")).unwrap_or_else(|| match (vip, &access_code) {
        (Some(vip), _) if vip.as_u64() != Some(0) => format!("vip-{vip}"),
        (_, Some(code)) => format!(
            "code-{}",
            server
                .get("accessCode")
                .and_then(Value::as_str)
                .unwrap_or(code)
        ),
        _ => format!("server-{source}-{place_id}-{}", uuid::Uuid::new_v4()),
    });
    let mut out = json!({
        "id": id,
        "source": source,
        "playing": playing,
        "maxPlayers": max_players,
        "isFull": max_players > 0 && playing >= max_players,
        "ping": server.get("ping").cloned().unwrap_or(Value::Null),
        "fps": server.get("fps").cloned().unwrap_or(Value::Null),
    });
    if let Some(name) = trimmed(server.get("name")) {
        out["name"] = Value::String(name);
    }
    if let Some(vip) = vip {
        out["vipServerId"] = vip.clone();
    }
    if let Some(code) = access_code {
        out["accessCode"] = Value::String(code);
    }
    if let Some(owner) = server.get("owner") {
        let name = trimmed(owner.get("name"));
        if let (Some(id), Some(name)) = (
            owner
                .get("id")
                .filter(|v| v.as_u64().is_some_and(|i| i > 0)),
            name,
        ) {
            out["owner"] = json!({
                "id": id,
                "displayName": trimmed(owner.get("displayName")).unwrap_or_else(|| name.clone()),
                "name": name,
                "hasVerifiedBadge": owner.get("hasVerifiedBadge").and_then(Value::as_bool).unwrap_or(false),
            });
        }
    }
    let tokens: Vec<Option<String>> = server
        .get("playerTokens")
        .and_then(Value::as_array)
        .map(|t| t.iter().map(|v| trimmed(Some(v))).collect())
        .unwrap_or_default();
    let players: Vec<Value> = server
        .get("players")
        .and_then(Value::as_array)
        .map(|players| {
            players
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.get("id")
                        .is_some_and(|id| id.as_u64().is_some_and(|i| i > 0))
                })
                .map(|(i, p)| {
                    let name = trimmed(p.get("name")).unwrap_or_default();
                    let display = trimmed(p.get("displayName")).unwrap_or_default();
                    let fallback = [display.as_str(), name.as_str()]
                        .into_iter()
                        .find(|s| !s.is_empty())
                        .unwrap_or("Player")
                        .to_owned();
                    let token =
                        trimmed(p.get("playerToken")).or_else(|| tokens.get(i).cloned().flatten());
                    let mut entry = json!({
                        "id": p.get("id"),
                        "name": if name.is_empty() { fallback.clone() } else { name },
                        "displayName": if display.is_empty() { fallback } else { display },
                    });
                    if let Some(token) = token {
                        entry["playerToken"] = Value::String(token);
                    }
                    entry
                })
                .collect()
        })
        .unwrap_or_default();
    if !players.is_empty() {
        out["previewPlayers"] = Value::Array(players);
    }
    let mut seen = HashSet::new();
    let unique: Vec<Value> = tokens
        .into_iter()
        .flatten()
        .filter(|t| seen.insert(t.clone()))
        .map(Value::String)
        .collect();
    if !unique.is_empty() {
        out["previewPlayerTokens"] = Value::Array(unique);
    }
    out
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerOptions {
    source: String,
    cursor: Option<String>,
    limit: Option<u64>,
    exclude_full_games: Option<bool>,
    sort_order: Option<String>,
}

async fn servers(ctx: &Ctx, place_id: u64, options: ServerOptions) -> AppResult<Value> {
    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(cursor) = options.cursor.filter(|c| !c.is_empty()) {
        params.push(("cursor", cursor));
    }
    if let Some(limit) = options.limit {
        params.push(("limit", limit.to_string()));
    }
    let url = if options.source == "private" {
        format!(
            "{}/games/{place_id}/private-servers?{}",
            endpoints::GAMES_V1,
            query_string(&params)
        )
    } else {
        let server_type = if options.source == "public" { 0 } else { 1 };
        if let Some(exclude) = options.exclude_full_games {
            params.push(("excludeFullGames", exclude.to_string()));
        }
        match options.sort_order.as_deref() {
            Some("asc") => params.push(("sortOrder", "1".into())),
            Some("desc") => params.push(("sortOrder", "2".into())),
            _ => {}
        }
        format!(
            "{}/games/{place_id}/servers/{server_type}?{}",
            endpoints::GAMES_V1,
            query_string(&params)
        )
    };
    let response: Value = ctx.state.roblox.get(&url).await?;
    let data: Vec<Value> = response
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|s| normalize_server(s, &options.source, place_id))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "data": data,
        "previousCursor": response.get("previousPageCursor").cloned().unwrap_or(Value::Null),
        "nextCursor": response.get("nextPageCursor").cloned().unwrap_or(Value::Null),
    }))
}

async fn social_links(ctx: &Ctx, universe_id: u64) -> Value {
    if universe_id == 0 {
        return json!([]);
    }
    if let Some((at, links)) = ctx.state.game_lists.social_links.lock().get(&universe_id) {
        if at.elapsed() < SOCIAL_LINKS_TTL {
            return links.clone();
        }
    }
    let url = format!(
        "{}/games/{universe_id}/social-links/list",
        endpoints::GAMES_V1
    );
    match ctx.state.roblox.get::<Value>(&url).await {
        Ok(response) => {
            let links: Vec<Value> = response
                .get("data")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|l| json!({ "id": l.get("id"), "title": l.get("title"), "type": l.get("type"), "url": l.get("url") }))
                        .collect()
                })
                .unwrap_or_default();
            let links = Value::Array(links);
            ctx.state.game_communities.record_social_links(
                &ctx.state,
                universe_id,
                links.as_array().map(Vec::as_slice).unwrap_or_default(),
            );
            let mut cache = ctx.state.game_lists.social_links.lock();
            if cache.len() >= SOCIAL_LINKS_MAX {
                if let Some(oldest) = cache.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| *k) {
                    cache.remove(&oldest);
                }
            }
            cache.insert(universe_id, (Instant::now(), links.clone()));
            links
        }
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch game social links");
            json!([])
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("games:getSorts", |ctx, _| async move {
            let sorts = explore_sorts(&ctx).await?;
            Ok(sorts
                .iter()
                .map(|s| {
                    let mut entry = json!({
                        "sortId": s.sort_id,
                        "displayName": s.sort_display_name,
                        "token": s.sort_id,
                        "name": s.sort_display_name,
                    });
                    if let Some(subtitle) = &s.subtitle {
                        entry["subtitle"] = Value::String(subtitle.clone());
                    }
                    entry
                })
                .collect::<Vec<_>>())
        })
        .handle("games:getBySort", |ctx, args| async move {
            let sort: String = args.get(0)?;
            let limit: Option<usize> = args.opt(1)?;
            games_by_sort(&ctx, sort, limit.unwrap_or(100)).await
        })
        .handle("games:getPromotedGames", |ctx, args| async move {
            let force: Option<bool> = args.opt(0)?;
            promoted_games(&ctx, force.unwrap_or(false)).await
        })
        .handle("games:getPlayability", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            playability(&ctx, ids).await
        })
        .handle("games:getSocialLinks", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(social_links(&ctx, id).await)
        })
        .handle("games:getVote", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(get_vote(&ctx, id).await)
        })
        .handle("games:setVote", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            let vote: Option<bool> = args.opt(1)?;
            set_vote(&ctx, place_id, vote).await
        })
        .handle("games:getNotificationStatus", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(notification_status(&ctx, id).await)
        })
        .handle("games:setNotification", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let enabled: bool = args.get(1)?;
            set_notification(&ctx, id, enabled).await
        })
        .handle("games:getFriendsPlaying", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            friends_playing(&ctx, user_id).await
        })
        .handle("games:getServers", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            let options: ServerOptions = args.get(1)?;
            servers(&ctx, place_id, options).await
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vote_and_server_normalization() {
        let vote = normalize_vote(
            &json!({ "model": { "canVote": true, "hasVoted": false, "userVote": true } }),
        );
        assert_eq!(
            vote,
            json!({ "canVote": true, "reasonForNotVoteable": "", "userVote": null })
        );

        let server = normalize_server(
            &json!({ "id": " abc ", "playing": 10, "maxPlayers": 10, "playerTokens": ["t1", "t1", " "] }),
            "public",
            1,
        );
        assert_eq!(server["id"], "abc");
        assert_eq!(server["isFull"], true);
        assert_eq!(server["previewPlayerTokens"], json!(["t1"]));
    }
}

/// `fetchGameSocialLinks` for other services (community lookups).
pub async fn social_links_for(ctx: &Ctx, universe_id: u64) -> Value {
    social_links(ctx, universe_id).await
}
