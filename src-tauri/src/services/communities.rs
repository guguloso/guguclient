//! Roblox communities/groups (`communities:*`). Port of `CommunitiesService`,
//! `GameCommunitiesService` and `GameCommunityIndexService` (the
//! `game-communities.json` index linking experiences to their community).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::AppResult;
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints::{self, join_ids};
use crate::services::roblox_layout::{read_json, write_json};
use crate::services::thumbnails::{IdQuery, ThumbPage};
use crate::state::AppState;
use crate::util::now_millis;

const PAGE_LIMIT: u64 = 25;
const ICON_SIZE: &str = "150x150";
const ICON_BATCH: usize = 100;
const ALLIES_PAGE_SIZE: u64 = 24;
const MAX_RECENT_GAMES_CONSIDERED: usize = 30;
const MIN_SUGGESTIONS: usize = 4;
const MAX_SUGGESTIONS: usize = 12;
const UNRESOLVED_RECHECK_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const OWNER_RANK: u64 = 255;
const MIN_NAME_MATCH_LENGTH: usize = 4;
const INDEX_VERSION: u64 = 1;
const MAX_INDEX_ENTRIES: usize = 2000;
const ENTRY_REFRESH_MS: u64 = 24 * 60 * 60 * 1000;
const SOURCES: [&str; 4] = ["socialLink", "creator", "description", "creatorOwned"];

// ------------------------------------------------------------------ mapping

fn user(raw: Option<&Value>) -> Value {
    match raw.filter(|v| v.is_object()) {
        Some(r) => {
            json!({ "displayName": r.get("displayName"), "hasVerifiedBadge": r.get("hasVerifiedBadge"), "userId": r.get("userId"), "username": r.get("username") })
        }
        None => Value::Null,
    }
}

fn role(raw: &Value) -> Value {
    json!({ "id": raw.get("id"), "memberCount": raw.get("memberCount").cloned().unwrap_or(Value::Null), "name": raw.get("name"), "rank": raw.get("rank") })
}

pub fn map_community(raw: &Value) -> Value {
    let shout = match raw.get("shout").filter(|s| s.is_object()) {
        Some(s) => {
            json!({ "body": s.get("body"), "created": s.get("created"), "poster": user(s.get("poster")), "updated": s.get("updated") })
        }
        None => Value::Null,
    };
    json!({
        "description": raw.get("description"),
        "hasVerifiedBadge": raw.get("hasVerifiedBadge"),
        "id": raw.get("id"),
        "isLocked": raw.get("isLocked").and_then(Value::as_bool).unwrap_or(false),
        "memberCount": raw.get("memberCount"),
        "name": raw.get("name"),
        "owner": user(raw.get("owner")),
        "publicEntryAllowed": raw.get("publicEntryAllowed"),
        "shout": shout,
    })
}

fn user_community(raw: &Value) -> Value {
    json!({ "community": map_community(raw.get("group").unwrap_or(&Value::Null)), "role": role(raw.get("role").unwrap_or(&Value::Null)) })
}

fn cursor(raw: &Value, key: &str) -> Value {
    raw.get(key)
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(Value::from)
        .unwrap_or(Value::Null)
}

fn page(raw: &Value, map: impl Fn(&Value) -> Value) -> Value {
    json!({
        "data": raw.get("data").and_then(Value::as_array).into_iter().flatten().map(map).collect::<Vec<_>>(),
        "nextCursor": cursor(raw, "nextPageCursor"),
        "previousCursor": cursor(raw, "previousPageCursor"),
    })
}

fn data(raw: &Value) -> Vec<Value> {
    raw.get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ------------------------------------------------------------------ service calls

pub async fn community_details(state: &AppState, id: u64) -> AppResult<Value> {
    let raw: Value = state
        .roblox
        .get(&format!("{}/groups/{id}", endpoints::GROUPS_V1))
        .await?;
    Ok(map_community(&raw))
}

pub async fn fetch_user_communities(state: &AppState, user_id: u64) -> AppResult<Vec<Value>> {
    let raw: Value = state
        .roblox
        .get(&format!(
            "{}/users/{user_id}/groups/roles",
            endpoints::GROUPS_V1
        ))
        .await?;
    Ok(data(&raw).iter().map(user_community).collect())
}

pub async fn community_icons(
    state: &AppState,
    ids: &[u64],
    size: &str,
) -> AppResult<BTreeMap<String, String>> {
    let ids: Vec<u64> = ids.iter().copied().filter(|id| *id > 0).collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let query = IdQuery {
        kind: "GroupIcon",
        size,
        format: "Png",
        include_profile_frame: None,
    };
    state
        .thumbnails
        .batch_by_id(&ids, &query, |missing| async move {
            let pages = futures::future::try_join_all(missing.chunks(ICON_BATCH).map(|chunk| {
                let url = format!(
                    "{}/groups/icons?groupIds={}&size={size}&format=Png",
                    endpoints::THUMBNAILS_V1,
                    join_ids(chunk)
                );
                async move { state.roblox.get::<ThumbPage>(&url).await }
            }))
            .await?;
            Ok(pages.into_iter().flat_map(|p| p.data).collect())
        })
        .await
}

fn qp(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ------------------------------------------------------------------ game community index

#[derive(Default)]
pub struct GameCommunityIndex {
    entries: Mutex<Option<HashMap<u64, (Map<String, Value>, u64)>>>,
    flush_scheduled: AtomicBool,
}

fn community_from_url(url: &str) -> Option<u64> {
    let lower = url.to_lowercase();
    ["groups/", "communities/"].iter().find_map(|p| {
        let idx = lower.find(p)?;
        let digits: String = lower[idx + p.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

fn communities_in_text(text: &str) -> Vec<u64> {
    let lower = text.to_lowercase();
    let mut ids = Vec::new();
    for marker in ["roblox.com/groups/", "roblox.com/communities/"] {
        let mut rest = lower.as_str();
        while let Some(idx) = rest.find(marker) {
            rest = &rest[idx + marker.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(id) = digits.parse::<u64>() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

impl GameCommunityIndex {
    fn path(state: &AppState) -> std::path::PathBuf {
        state.paths.base.join("game-communities.json")
    }

    fn with_entries<R>(
        &self,
        state: &AppState,
        f: impl FnOnce(&mut HashMap<u64, (Map<String, Value>, u64)>) -> R,
    ) -> R {
        let mut guard = self.entries.lock();
        let entries = guard.get_or_insert_with(|| {
            let mut map = HashMap::new();
            let Some(raw) = read_json(&Self::path(state)) else {
                return map;
            };
            if raw.get("version").and_then(Value::as_u64) != Some(INDEX_VERSION) {
                return map;
            }
            for (key, entry) in raw
                .get("entries")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let (Ok(id), Some(findings), Some(updated)) = (
                    key.parse::<u64>(),
                    entry.get("findings").and_then(Value::as_object),
                    entry.get("updatedAt").and_then(Value::as_u64),
                ) else {
                    continue;
                };
                let valid = findings.iter().all(|(k, v)| {
                    SOURCES.contains(&k.as_str()) && v.as_u64().is_some_and(|v| v > 0)
                });
                if id > 0 && valid {
                    map.insert(id, (findings.clone(), updated));
                }
            }
            map
        });
        f(entries)
    }

    pub fn community_id(&self, state: &AppState, universe: u64) -> Option<u64> {
        self.with_entries(state, |e| {
            let (findings, _) = e.get(&universe)?;
            SOURCES
                .iter()
                .find_map(|s| findings.get(*s).and_then(Value::as_u64))
        })
    }

    fn should_resolve(&self, state: &AppState, universe: u64) -> bool {
        self.with_entries(state, |e| match e.get(&universe) {
            None => true,
            Some((findings, updated)) => {
                findings.is_empty()
                    && now_millis().saturating_sub(*updated) >= UNRESOLVED_RECHECK_MS
            }
        })
    }

    fn mark_unresolved(self: &Arc<Self>, state: &Arc<AppState>, universe: u64) {
        if self.community_id(state, universe).is_some() {
            return;
        }
        self.with_entries(state, |e| e.insert(universe, (Map::new(), now_millis())));
        self.schedule_persist(state);
    }

    /// `applyGameCommunityFinding` for each finding.
    fn apply(self: &Arc<Self>, state: &Arc<AppState>, findings: &[(u64, &str, Option<u64>)]) {
        let now = now_millis();
        let changed = self.with_entries(state, |entries| {
            let mut changed = false;
            for (universe, source, community) in findings {
                let existing = entries.get(universe).cloned();
                let previous = existing
                    .as_ref()
                    .and_then(|(f, _)| f.get(*source))
                    .and_then(Value::as_u64);
                match community {
                    None => {
                        let Some((mut f, _)) = existing.filter(|_| previous.is_some()) else {
                            continue;
                        };
                        f.remove(*source);
                        if f.is_empty() {
                            entries.remove(universe);
                        } else {
                            entries.insert(*universe, (f, now));
                        }
                        changed = true;
                    }
                    Some(id) => {
                        let fresh = previous == Some(*id)
                            && existing
                                .as_ref()
                                .is_some_and(|(_, u)| now.saturating_sub(*u) < ENTRY_REFRESH_MS);
                        if fresh {
                            continue;
                        }
                        let mut f = existing.map(|(f, _)| f).unwrap_or_default();
                        f.insert((*source).to_owned(), json!(id));
                        entries.insert(*universe, (f, now));
                        changed = true;
                    }
                }
            }
            changed
        });
        if changed {
            self.schedule_persist(state);
        }
    }

    /// `recordGames` (called whenever game details are fetched).
    pub fn record_games(self: &Arc<Self>, state: &Arc<AppState>, games: &[Value]) {
        let mut findings = Vec::new();
        for game in games {
            let Some(id) = game.get("id").and_then(Value::as_u64).filter(|id| *id > 0) else {
                continue;
            };
            let creator = game
                .pointer("/creator/id")
                .and_then(Value::as_u64)
                .filter(|c| *c > 0);
            let group_owned =
                game.pointer("/creator/type").and_then(Value::as_str) == Some("Group");
            let linked = communities_in_text(
                game.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
            .first()
            .copied();
            findings.push((id, "creator", if group_owned { creator } else { None }));
            findings.push((id, "description", linked));
        }
        self.apply(state, &findings);
    }

    /// `recordSocialLinks`.
    pub fn record_social_links(
        self: &Arc<Self>,
        state: &Arc<AppState>,
        universe: u64,
        links: &[Value],
    ) {
        let linked = links
            .iter()
            .filter(|l| l.get("type").and_then(Value::as_str) == Some("RobloxGroup"))
            .find_map(|l| community_from_url(l.get("url").and_then(Value::as_str).unwrap_or("")));
        self.apply(state, &[(universe, "socialLink", linked)]);
    }

    fn schedule_persist(self: &Arc<Self>, state: &Arc<AppState>) {
        if self.flush_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let (index, state) = (Arc::clone(self), Arc::clone(state));
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            index.flush(&state);
        });
    }

    pub fn flush(&self, state: &AppState) {
        self.flush_scheduled.store(false, Ordering::Release);
        let snapshot = self.with_entries(state, |e| {
            let mut kept: Vec<_> = e.iter().map(|(k, (f, u))| (*k, f.clone(), *u)).collect();
            kept.sort_by(|a, b| b.2.cmp(&a.2));
            kept.truncate(MAX_INDEX_ENTRIES);
            kept
        });
        let entries: Map<String, Value> = snapshot
            .into_iter()
            .map(|(k, f, u)| (k.to_string(), json!({ "findings": f, "updatedAt": u })))
            .collect();
        if let Err(error) = write_json(
            &Self::path(state),
            &json!({ "entries": entries, "version": INDEX_VERSION }),
        ) {
            tracing::warn!("Failed to write game community index: {error}");
        }
    }
}

fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    let mut depth_sq = 0;
    let mut depth_paren = 0;
    for c in name.chars() {
        match c {
            '[' => depth_sq += 1,
            ']' if depth_sq > 0 => depth_sq -= 1,
            '(' => depth_paren += 1,
            ')' if depth_paren > 0 => depth_paren -= 1,
            _ if depth_sq == 0 && depth_paren == 0 && c.is_alphanumeric() => {
                out.extend(c.to_lowercase())
            }
            _ => {}
        }
    }
    out
}

async fn resolve_game(ctx: &Ctx, universe: u64) {
    let state = &ctx.state;
    let index = &state.game_communities;
    let ids = [universe];
    let result: AppResult<()> = async {
        let (details, _) = futures::join!(
            crate::services::games::details(ctx, &ids),
            crate::services::game_lists::social_links_for(ctx, universe)
        );
        let Some(game) = details?.into_iter().next() else {
            index.mark_unresolved(state, universe);
            return Ok(());
        };
        if index.community_id(state, universe).is_some() {
            return Ok(());
        }
        let creator = game
            .pointer("/creator/id")
            .and_then(Value::as_u64)
            .filter(|c| *c > 0);
        if let (Some("User"), Some(creator)) = (
            game.pointer("/creator/type").and_then(Value::as_str),
            creator,
        ) {
            let memberships = fetch_user_communities(state, creator).await?;
            let game_name = normalize_name(
                game.get("sourceName")
                    .or(game.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let found = (game_name.chars().count() >= MIN_NAME_MATCH_LENGTH)
                .then(|| {
                    memberships.iter().find(|m| {
                        m.pointer("/role/rank").and_then(Value::as_u64) == Some(OWNER_RANK)
                            && normalize_name(
                                m.pointer("/community/name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                            )
                            .contains(&game_name)
                    })
                })
                .flatten()
                .and_then(|m| m.pointer("/community/id").and_then(Value::as_u64));
            index.apply(state, &[(universe, "creatorOwned", found)]);
            if found.is_some() {
                return Ok(());
            }
        }
        index.mark_unresolved(state, universe);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!("Failed to look up the community for universe {universe}: {error}");
    }
}

async fn load_community(state: &AppState, id: u64) -> Value {
    match community_details(state, id).await {
        Ok(c) if !c.get("isLocked").and_then(Value::as_bool).unwrap_or(false) => c,
        Ok(_) => Value::Null,
        Err(error) => {
            tracing::warn!("Failed to load game community {id}: {error}");
            Value::Null
        }
    }
}

async fn game_community(ctx: &Ctx, universe: u64) -> Value {
    if universe == 0 {
        return Value::Null;
    }
    let state = &ctx.state;
    if state.game_communities.should_resolve(state, universe) {
        resolve_game(ctx, universe).await;
    } else {
        crate::services::game_lists::social_links_for(ctx, universe).await;
    }
    match state.game_communities.community_id(state, universe) {
        Some(id) => load_community(state, id).await,
        None => Value::Null,
    }
}

async fn game_suggestions(ctx: &Ctx) -> Value {
    let state = &ctx.state;
    let Some(user_id) = state.auth.current_user_id() else {
        return json!([]);
    };
    let result: AppResult<Vec<Value>> = async {
        let (recent, memberships) = futures::join!(
            crate::services::discovery::recent_games(ctx, false),
            fetch_user_communities(state, user_id)
        );
        let (recent, memberships) = (recent?, memberships?);
        let excluded: HashSet<u64> = memberships
            .iter()
            .filter_map(|m| m.pointer("/community/id").and_then(Value::as_u64))
            .collect();
        let mut candidates: Vec<(u64, Vec<String>)> = Vec::new();
        for game in recent
            .as_array()
            .into_iter()
            .flatten()
            .take(MAX_RECENT_GAMES_CONSIDERED)
        {
            let Some(universe) = game.get("universeId").and_then(Value::as_u64) else {
                continue;
            };
            let Some(id) = state
                .game_communities
                .community_id(state, universe)
                .filter(|id| !excluded.contains(id))
            else {
                continue;
            };
            let name = game
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            match candidates.iter_mut().find(|(c, _)| *c == id) {
                Some((_, names)) if !names.contains(&name) => names.push(name),
                Some(_) => {}
                None => candidates.push((id, vec![name])),
            }
        }
        if candidates.len() < MIN_SUGGESTIONS {
            return Ok(Vec::new());
        }
        let mut shown = Vec::new();
        let mut next = 0;
        while shown.len() < MAX_SUGGESTIONS && next < candidates.len() {
            let take = MAX_SUGGESTIONS - shown.len();
            let batch: Vec<_> = candidates[next..(next + take).min(candidates.len())].to_vec();
            next += batch.len();
            let loaded =
                futures::future::join_all(batch.iter().map(|(id, _)| load_community(state, *id)))
                    .await;
            for (community, (_, names)) in loaded.into_iter().zip(batch) {
                if !community.is_null() {
                    shown.push(json!({ "community": community, "gameNames": names }));
                }
            }
        }
        if state.auth.current_user_id() != Some(user_id) || shown.len() < MIN_SUGGESTIONS {
            return Ok(Vec::new());
        }
        Ok(shown)
    }
    .await;
    match result {
        Ok(list) => Value::Array(list),
        Err(error) => {
            tracing::warn!("Failed to build game community suggestions: {error}");
            json!([])
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("communities:getDetails", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            community_details(&ctx.state, id).await
        })
        .handle("communities:getUserCommunities", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            Ok(fetch_user_communities(&ctx.state, user_id).await.unwrap_or_else(|error| {
                tracing::error!("Failed to fetch user communities: {error}");
                Vec::new()
            }))
        })
        .handle("communities:getAllies", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let start: Option<u64> = args.opt(1)?;
            let start = start.unwrap_or(0);
            let url = format!(
                "{}/groups/{id}/relationships/Allies?StartRowIndex={start}&MaxRows={ALLIES_PAGE_SIZE}",
                endpoints::GROUPS_V1
            );
            let raw: Value = ctx.state.roblox.get(&url).await?;
            let related = raw.get("relatedGroups").and_then(Value::as_array).cloned().unwrap_or_default();
            let total = raw.get("totalGroupCount").and_then(Value::as_u64).unwrap_or(0);
            let next = start + related.len() as u64;
            Ok(json!({
                "data": related.iter().map(map_community).collect::<Vec<_>>(),
                "nextRowIndex": if next < total { json!(next) } else { Value::Null },
                "totalCount": total,
            }))
        })
        .handle("communities:getProfileInfo", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let body = json!({
                "profileId": id.to_string(),
                "profileType": "Community",
                "components": [{ "component": "CoverPhoto" }, { "component": "Announcements" }],
            });
            let url = format!("{}/profiles/get", endpoints::PROFILE_PLATFORM_API);
            Ok(match ctx.state.roblox.post::<_, Value>(&url, &body).await {
                Ok(raw) => json!({
                    "coverPhotoAssetId": raw.pointer("/components/CoverPhoto/coverPhotoId").cloned().unwrap_or(Value::Null),
                    "latestAnnouncementId": raw.pointer("/components/Announcements/id").cloned().unwrap_or(Value::Null),
                }),
                Err(error) => {
                    tracing::error!("Failed to fetch community profile info: {error}");
                    json!({ "coverPhotoAssetId": null, "latestAnnouncementId": null })
                }
            })
        })
        .handle("communities:getAnnouncement", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let announcement_id: String = args.get(1)?;
            let state = &ctx.state;
            let a_url = format!("{}/groups/{id}/announcements?announcementIds={}", endpoints::GROUPS_V1, urlencoding::encode(&announcement_id));
            let e_url = format!("{}/groups/{id}/emotes", endpoints::GROUPS_V1);
            let (announcements, emotes) = futures::join!(state.roblox.get::<Value>(&a_url), state.roblox.get::<Value>(&e_url));
            let (announcements, emotes) = match (announcements, emotes) {
                (Ok(a), Ok(e)) => (a, e),
                (Err(error), _) | (_, Err(error)) => {
                    tracing::error!("Failed to fetch community announcement: {error}");
                    return Ok(Value::Null);
                }
            };
            let Some(raw) = announcements.pointer("/data/0") else { return Ok(Value::Null) };
            let names: HashMap<String, Value> = emotes
                .get("emoteSets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|set| set.get("emotes").and_then(Value::as_array).cloned().unwrap_or_default())
                .filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e.get("name").cloned().unwrap_or(Value::Null))))
                .collect();
            let media = raw.pointer("/message/media");
            let approved = media.and_then(|m| m.get("isApproved")).and_then(Value::as_bool).unwrap_or(false);
            let reactions: Vec<Value> = raw
                .pointer("/message/reactions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|r| r.get("reactionCount").and_then(Value::as_u64).unwrap_or(0) > 0)
                .map(|r| {
                    let emote = r.get("emoteId").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "count": r.get("reactionCount"),
                        "emoteId": emote,
                        "emoteName": names.get(emote).cloned().unwrap_or(Value::Null),
                        "hasUserReacted": r.get("hasUserAppliedReaction"),
                    })
                })
                .collect();
            Ok(json!({
                "authorDisplayName": raw.pointer("/creatorInfo/displayName").and_then(Value::as_str).unwrap_or(""),
                "authorRoleName": raw.pointer("/creatorInfo/groupRoleName").and_then(Value::as_str).unwrap_or(""),
                "authorUserId": raw.get("createdBy"),
                "body": raw.pointer("/message/content/plainText").and_then(Value::as_str).unwrap_or(""),
                "categoryId": raw.get("categoryId").cloned().unwrap_or(Value::Null),
                "createdAt": raw.get("createdAt"),
                "id": raw.get("id"),
                "imageAssetId": if approved { media.and_then(|m| m.get("assetId")).cloned().unwrap_or(Value::Null) } else { Value::Null },
                "messageId": raw.pointer("/message/id").cloned().unwrap_or(Value::Null),
                "reactions": reactions,
                "title": raw.get("name"),
            }))
        })
        .handle("communities:getLinkedShout", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let url = format!("{}/groups/{id}/shout", endpoints::COMMUNITY_LINKS_V1);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) if raw.get("id").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())) => json!({
                    "content": raw.get("content").and_then(Value::as_str).unwrap_or(""),
                    "createdAt": raw.get("createdAt"),
                    "createdBy": raw.get("createdBy"),
                    "id": raw.get("id"),
                    "imageAssetId": raw.get("imageAssetId").cloned().unwrap_or(Value::Null),
                    "imageUrl": raw.get("imageURL").cloned().unwrap_or(Value::Null),
                    "likeCount": raw.get("likeCount").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
                    "title": raw.get("title").and_then(Value::as_str).unwrap_or(""),
                }),
                Ok(_) => Value::Null,
                Err(error) => {
                    tracing::warn!("No linked community shout available: {error}");
                    Value::Null
                }
            })
        })
        .handle("communities:getMembership", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let url = format!("{}/groups/{id}/membership?includeNotificationPreferences=false", endpoints::GROUPS_V1);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => {
                    let r = raw.pointer("/userRole/role").cloned().unwrap_or(Value::Null);
                    let member = r.get("rank").and_then(Value::as_u64).unwrap_or(0) > 0;
                    json!({ "isMember": member, "isPendingJoin": raw.get("isPendingJoin"), "role": if member { role(&r) } else { Value::Null } })
                }
                Err(error) => {
                    tracing::error!("Failed to fetch community membership: {error}");
                    Value::Null
                }
            })
        })
        .handle("communities:join", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            ctx.state.roblox.post::<_, Value>(&format!("{}/groups/{id}/users", endpoints::GROUPS_V1), &json!({})).await?;
            ctx.state.track("community.membership", json!({ "community_id": id, "action": "join" }));
            Ok(Value::Null)
        })
        .handle("communities:leave", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let user_id: u64 = args.get(1)?;
            ctx.state.roblox.delete::<Value>(&format!("{}/groups/{id}/users/{user_id}", endpoints::GROUPS_V1)).await?;
            ctx.state.track("community.membership", json!({ "community_id": id, "action": "leave" }));
            Ok(Value::Null)
        })
        .handle("communities:getSocialLinks", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(match ctx.state.roblox.get::<Value>(&format!("{}/groups/{id}/social-links", endpoints::GROUPS_V1)).await {
                Ok(raw) => data(&raw).iter().map(|l| json!({ "id": l.get("id"), "title": l.get("title"), "type": l.get("type"), "url": l.get("url") })).collect(),
                Err(error) => {
                    tracing::error!("Failed to fetch community social links: {error}");
                    Vec::new()
                }
            })
        })
        .handle("communities:getMetadata", |ctx, _| async move {
            let raw: Value = ctx.state.roblox.get(&format!("{}/groups/metadata", endpoints::GROUPS_V1)).await?;
            Ok(json!({ "communityLimit": raw.get("groupLimit"), "currentCommunityCount": raw.get("currentGroupCount") }))
        })
        .handle("communities:getSearchMetadata", |ctx, _| async move {
            Ok(match ctx.state.roblox.get::<Value>(&format!("{}/groups/search/metadata", endpoints::GROUPS_V1)).await {
                Ok(raw) => json!({ "suggestedKeywords": raw.get("SuggestedGroupKeywords") }),
                Err(error) => {
                    tracing::error!("Failed to fetch community search metadata: {error}");
                    json!({ "suggestedKeywords": [] })
                }
            })
        })
        .handle("communities:getFriendsCommunities", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            let url = format!("{}/users/{user_id}/friends/groups/roles", endpoints::GROUPS_V1);
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => data(&raw)
                    .iter()
                    .map(|f| {
                        let u = f.get("user").cloned().unwrap_or(Value::Null);
                        json!({
                            "communities": f.get("groups").and_then(Value::as_array).into_iter().flatten().map(user_community).collect::<Vec<_>>(),
                            "friend": { "displayName": u.get("displayName"), "hasVerifiedBadge": u.get("hasVerifiedBadge"), "userId": u.get("userId"), "username": u.get("username") },
                        })
                    })
                    .collect(),
                Err(error) => {
                    tracing::error!("Failed to fetch friends' communities: {error}");
                    Vec::new()
                }
            })
        })
        .handle("communities:getGameCommunity", |ctx, args| async move {
            let universe: u64 = args.get(0)?;
            Ok(game_community(&ctx, universe).await)
        })
        .handle("communities:getGameSuggestions", |ctx, _| async move { Ok(game_suggestions(&ctx).await) })
        .handle("communities:getMembers", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let cursor: Option<String> = args.opt(1)?;
            let limit: Option<u64> = args.opt(2)?;
            let mut params = vec![("limit", limit.unwrap_or(PAGE_LIMIT).to_string())];
            if let Some(c) = cursor.filter(|c| !c.is_empty()) {
                params.push(("cursor", c));
            }
            let raw: Value = ctx.state.roblox.get(&format!("{}/groups/{id}/users?{}", endpoints::GROUPS_V1, qp(&params))).await?;
            Ok(page(&raw, |m| {
                let u = m.get("user").cloned().unwrap_or(Value::Null);
                json!({
                    "displayName": u.get("displayName"),
                    "hasVerifiedBadge": u.get("hasVerifiedBadge"),
                    "role": role(m.get("role").unwrap_or(&Value::Null)),
                    "userId": u.get("userId"),
                    "username": u.get("username"),
                })
            }))
        })
        .handle("communities:getExperiences", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let cursor: Option<String> = args.opt(1)?;
            let limit: Option<u64> = args.opt(2)?;
            let mut params = vec![
                ("accessFilter", "2".to_owned()),
                ("limit", limit.unwrap_or(PAGE_LIMIT).to_string()),
                ("sortOrder", "Desc".to_owned()),
            ];
            if let Some(c) = cursor.filter(|c| !c.is_empty()) {
                params.push(("cursor", c));
            }
            let raw: Value = ctx.state.roblox.get(&format!("{}/groups/{id}/gamesV2?{}", endpoints::GAMES_V2, qp(&params))).await?;
            Ok(page(&raw, |g| {
                json!({
                    "description": g.get("description").and_then(Value::as_str).unwrap_or(""),
                    "name": g.get("name"),
                    "placeVisits": g.get("placeVisits"),
                    "rootPlaceId": g.pointer("/rootPlace/id").cloned().unwrap_or(Value::Null),
                    "universeId": g.get("id"),
                })
            }))
        })
        .handle("communities:search", |ctx, args| async move {
            let keyword: String = args.get(0)?;
            let cursor: Option<String> = args.opt(1)?;
            let limit: Option<u64> = args.opt(2)?;
            let empty = json!({ "data": [], "nextCursor": null, "previousCursor": null });
            let trimmed = keyword.trim();
            if trimmed.is_empty() {
                return Ok(empty);
            }
            let mut params = vec![("keyword", trimmed.to_owned()), ("limit", limit.unwrap_or(PAGE_LIMIT).to_string())];
            if let Some(c) = cursor.filter(|c| !c.is_empty()) {
                params.push(("cursor", c));
            }
            Ok(match ctx.state.roblox.get::<Value>(&format!("{}/groups/search?{}", endpoints::GROUPS_V1, qp(&params))).await {
                Ok(raw) => page(&raw, |g| {
                    json!({
                        "description": g.get("description"), "hasVerifiedBadge": g.get("hasVerifiedBadge"), "id": g.get("id"),
                        "memberCount": g.get("memberCount"), "name": g.get("name"), "publicEntryAllowed": g.get("publicEntryAllowed"),
                    })
                }),
                Err(error) => {
                    tracing::error!("Failed to search communities: {error}");
                    empty
                }
            })
        })
        .handle("communities:getIcons", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size: Option<String> = args.opt(1)?;
            community_icons(&ctx.state, &ids, size.as_deref().unwrap_or(ICON_SIZE)).await
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_link_parsing() {
        assert_eq!(
            community_from_url("https://www.roblox.com/communities/123/name"),
            Some(123)
        );
        assert_eq!(
            communities_in_text("join roblox.com/groups/55 and roblox.com/groups/55"),
            vec![55]
        );
        assert_eq!(normalize_name("[UPD] Pet Sim (Beta)!"), "petsim");
    }
}
