//! Experiences (`games:*`). Port of `GamesService`.
//!
//! Ported: getDetails, getVotes, getThumbnails, getWideThumbnails,
//! getUniverseByPlace, getUserGames.
//! Pending: sorts/charts, servers, playability, social links, vote and
//! notification mutations, friends playing, promoted games.

use std::collections::BTreeMap;

use futures::future::try_join_all;
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppResult;
use crate::ipc::{Ctx, Router};
use crate::roblox::endpoints::{self, join_ids};
use crate::services::thumbnails::{IdQuery, ThumbEntry, ThumbPage};

const THUMBNAIL_BATCH_SIZE: usize = 100;

#[derive(Deserialize)]
struct DataEnvelope<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoteEntry {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    up_votes: u64,
    #[serde(default)]
    down_votes: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniverseThumbnails {
    universe_id: u64,
    #[serde(default)]
    thumbnails: Vec<ThumbEntry>,
}

/// Same rounding as `computeApprovalPercentage`.
pub fn approval_percentage(up: u64, down: u64) -> u64 {
    let total = up + down;
    if total == 0 {
        0
    } else {
        ((up as f64 / total as f64) * 100.0).round() as u64
    }
}

pub async fn details(ctx: &Ctx, universe_ids: &[u64]) -> AppResult<Vec<Value>> {
    if universe_ids.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/games?universeIds={}", endpoints::GAMES_V1, join_ids(universe_ids));
    let envelope: DataEnvelope<Value> = ctx.state.roblox.get(&url).await?;
    Ok(envelope.data)
}

/// `Object.fromEntries(Map<universeId, {upVotes, downVotes, rating}>)`.
async fn votes(ctx: &Ctx, universe_ids: &[u64]) -> AppResult<BTreeMap<String, Value>> {
    if universe_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let url = format!("{}/games/votes?universeIds={}", endpoints::GAMES_V1, join_ids(universe_ids));
    let envelope: DataEnvelope<VoteEntry> = ctx.state.roblox.get(&url).await?;
    Ok(envelope
        .data
        .into_iter()
        .map(|v| {
            let rating = approval_percentage(v.up_votes, v.down_votes);
            let value = serde_json::json!({ "upVotes": v.up_votes, "downVotes": v.down_votes, "rating": rating });
            (v.id.to_string(), value)
        })
        .collect())
}

/// Square game icons, cached (`getGameThumbnails`).
pub async fn icons(ctx: &Ctx, universe_ids: &[u64], size: &str) -> AppResult<BTreeMap<String, String>> {
    let query = IdQuery { kind: "GameIcon", size, format: "Png", include_profile_frame: None };
    let roblox = &ctx.state.roblox;
    ctx.state
        .thumbnails
        .batch_by_id(universe_ids, &query, |missing| async move {
            let pages = try_join_all(missing.chunks(THUMBNAIL_BATCH_SIZE).map(|chunk| {
                let url = format!(
                    "{}/games/icons?universeIds={}&size={size}&format=Png&isCircular=false",
                    endpoints::THUMBNAILS_V1,
                    join_ids(chunk)
                );
                async move { roblox.get::<ThumbPage>(&url).await }
            }))
            .await?;
            Ok(pages.into_iter().flat_map(|p| p.data).collect())
        })
        .await
}

/// 16:9 game artwork, cached (`getGameWideThumbnails`).
pub async fn wide_thumbnails(ctx: &Ctx, universe_ids: &[u64], size: &str) -> AppResult<BTreeMap<String, String>> {
    let query = IdQuery { kind: "GameThumbnail", size, format: "Webp", include_profile_frame: None };
    let roblox = &ctx.state.roblox;
    ctx.state
        .thumbnails
        .batch_by_id(universe_ids, &query, |missing| async move {
            let pages = try_join_all(missing.chunks(THUMBNAIL_BATCH_SIZE).map(|chunk| {
                let url = format!(
                    "{}/games/multiget/thumbnails?universeIds={}&countPerUniverse=1&defaults=false&size={size}&format=Webp",
                    endpoints::THUMBNAILS_V1,
                    join_ids(chunk)
                );
                async move { roblox.get::<DataEnvelope<UniverseThumbnails>>(&url).await }
            }))
            .await?;
            Ok(pages
                .into_iter()
                .flat_map(|p| p.data)
                .map(|u| {
                    let first = u.thumbnails.into_iter().next();
                    ThumbEntry {
                        target_id: Some(u.universe_id),
                        state: first.as_ref().and_then(|t| t.state.clone()),
                        image_url: first.and_then(|t| t.image_url),
                    }
                })
                .collect())
        })
        .await
}

async fn universe_by_place(ctx: &Ctx, place_id: u64) -> Option<u64> {
    let url = format!("{}/places/{place_id}/universe", endpoints::UNIVERSES_V1);
    match ctx.state.roblox.get::<Value>(&url).await {
        Ok(body) => body.get("universeId").and_then(Value::as_u64),
        Err(error) => {
            tracing::warn!(%error, "Failed to resolve universe for place");
            None
        }
    }
}

async fn user_games(ctx: &Ctx, user_id: u64) -> AppResult<Vec<Value>> {
    let url = format!("{}/users/{user_id}/games?accessFilter=2&sortOrder=Asc&limit=25", endpoints::GAMES_V2);
    let envelope: DataEnvelope<Value> = ctx.state.roblox.get(&url).await?;
    Ok(envelope.data)
}

pub fn register(router: &mut Router) {
    router
        .handle("games:getDetails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            details(&ctx, &ids).await
        })
        .handle("games:getVotes", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            votes(&ctx, &ids).await
        })
        .handle("games:getThumbnails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size = args.opt::<String>(1)?.unwrap_or_else(|| "512x512".to_owned());
            icons(&ctx, &ids, &size).await
        })
        .handle("games:getWideThumbnails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size = args.opt::<String>(1)?.unwrap_or_else(|| "576x324".to_owned());
            wide_thumbnails(&ctx, &ids, &size).await
        })
        .handle("games:getUniverseByPlace", |ctx, args| async move {
            let place_id: u64 = args.get(0)?;
            Ok(universe_by_place(&ctx, place_id).await)
        })
        .handle("games:getUserGames", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            user_games(&ctx, user_id).await
        });
}

#[cfg(test)]
mod tests {
    use super::approval_percentage;

    #[test]
    fn approval_rounding_matches_js() {
        assert_eq!(approval_percentage(0, 0), 0);
        assert_eq!(approval_percentage(95, 5), 95);
        assert_eq!(approval_percentage(2, 1), 67);
    }
}
