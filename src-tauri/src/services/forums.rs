//! Community forums (`forums:*`). Port of `ForumsService` + controller.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::Method;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::state::AppState;

const PAGE_LIMIT: u64 = 25;
type EmoteNames = Arc<HashMap<String, Value>>;

#[derive(Default)]
pub struct ForumsState {
    emote_names: Mutex<HashMap<u64, EmoteNames>>,
}

fn base(group: u64) -> String {
    format!("{}/groups/{group}", endpoints::GROUPS_V1)
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// `plainTextToSlate`.
fn slate(text: &str) -> String {
    let children: Vec<Value> = text.split('\n').map(|line| json!({ "type": "block-text", "children": [{ "text": line }] })).collect();
    json!({ "type": "document", "children": children }).to_string()
}

fn parse_slate(raw: Option<&Value>) -> Value {
    raw.and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|v| v.get("children").is_some_and(Value::is_array))
        .unwrap_or(Value::Null)
}

fn or_null(v: Option<&Value>) -> Value {
    v.cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null)
}

fn cursor(raw: &Value, key: &str) -> Value {
    raw.get(key).and_then(Value::as_str).filter(|c| !c.is_empty()).map(Value::from).unwrap_or(Value::Null)
}

fn empty_page() -> Value {
    json!({ "data": [], "nextCursor": null, "previousCursor": null })
}

fn map_comment(raw: &Value, emotes: &HashMap<String, Value>) -> Value {
    let creator = raw.get("creatorInfo").filter(|c| c.is_object()).map(|c| {
        json!({ "displayName": c.get("displayName"), "groupRoleName": or_null(c.get("groupRoleName")), "hasVerifiedBadge": c.get("hasVerifiedBadge") })
    });
    let reactions: Vec<Value> = raw
        .get("reactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("reactionCount").and_then(Value::as_u64).unwrap_or(0) > 0)
        .map(|r| {
            let emote = r.get("emoteId").and_then(Value::as_str).unwrap_or("");
            json!({
                "count": r.get("reactionCount"),
                "countsVisible": r.get("areReactionCountsVisible").and_then(Value::as_bool).unwrap_or(true),
                "emoteId": emote,
                "emoteName": emotes.get(emote).cloned().unwrap_or(Value::Null),
                "hasUserReacted": r.get("hasUserAppliedReaction"),
            })
        })
        .collect();
    let thread = raw.get("threadComments");
    json!({
        "id": raw.get("id"),
        "parentId": or_null(raw.get("parentId")),
        "content": { "plainText": or_null(raw.pointer("/content/plainText")), "slate": parse_slate(raw.pointer("/content/slate")) },
        "createdBy": raw.get("createdBy"),
        "creatorDisplayName": or_null(raw.get("creatorDisplayName")),
        "creatorInfo": creator.unwrap_or(Value::Null),
        "createdAt": raw.get("createdAt"),
        "hasMoreReplies": thread.and_then(|t| t.get("hasMore")).and_then(Value::as_bool).unwrap_or(false),
        "reactions": reactions,
        "replies": thread.and_then(|t| t.get("comments")).and_then(Value::as_array).into_iter().flatten()
            .map(|r| map_comment(r, emotes)).collect::<Vec<_>>(),
        "threadId": or_null(raw.get("threadId")),
    })
}

fn map_post(raw: &Value, emotes: &HashMap<String, Value>) -> Value {
    let flag = |k: &str| raw.get(k).and_then(Value::as_bool).unwrap_or(false);
    json!({
        "id": raw.get("id"),
        "shortId": or_null(raw.get("shortId")),
        "categoryId": raw.get("categoryId"),
        "groupId": raw.get("groupId"),
        "name": raw.get("name"),
        "isLocked": flag("isLocked"),
        "isPinned": flag("isPinned"),
        "isUnread": flag("isUnread"),
        "commentCount": raw.get("commentCount").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)),
        "createdBy": raw.get("createdBy"),
        "createdAt": raw.get("createdAt"),
        "firstComment": raw.get("firstComment").filter(|c| c.is_object()).map(|c| map_comment(c, emotes)).unwrap_or(Value::Null),
        "lastComment": raw.get("lastComment").filter(|c| c.is_object()).map(|c| map_comment(c, emotes)).unwrap_or(Value::Null),
    })
}

fn emotes_list(raw: &Value) -> Vec<(String, Value)> {
    raw.get("emoteSets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|set| set.get("emotes").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e.get("name").cloned().unwrap_or(Value::Null))))
        .collect()
}

/// `fetchEmoteNameById`: cached per community; failures are not cached.
async fn emote_names(state: &AppState, group: u64) -> EmoteNames {
    if let Some(cached) = state.forums.emote_names.lock().get(&group) {
        return Arc::clone(cached);
    }
    match state.roblox.get::<Value>(&format!("{}/emotes", base(group))).await {
        Ok(raw) => {
            let names: EmoteNames = Arc::new(emotes_list(&raw).into_iter().collect());
            state.forums.emote_names.lock().insert(group, Arc::clone(&names));
            names
        }
        Err(error) => {
            tracing::warn!("Failed to fetch forum emotes: {error}");
            Arc::new(HashMap::new())
        }
    }
}

fn page_of(raw: &Value, map: impl Fn(&Value) -> Value) -> Value {
    json!({
        "data": raw.get("data").and_then(Value::as_array).into_iter().flatten().map(map).collect::<Vec<_>>(),
        "nextCursor": cursor(raw, "nextPageCursor"),
        "previousCursor": cursor(raw, "previousPageCursor"),
    })
}

async fn reaction(state: &AppState, method: Method, args: &[Value]) -> AppResult<()> {
    let s = |i: usize| args.get(i).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let group = args.first().and_then(Value::as_u64).unwrap_or(0);
    let (channel, post, comment, emote) = (s(1), s(2), s(3), s(4));
    let is_post_comment = args.get(5).and_then(Value::as_bool).unwrap_or(false);
    let url = format!(
        "{}/forums/channels/{}/comments/{}/reactions/{}?categoryId={}&postId={}&isPostComment={is_post_comment}",
        base(group),
        enc(&post),
        enc(&comment),
        enc(&emote),
        enc(&channel),
        enc(&post)
    );
    let response = state.roblox.request(method.clone(), &url, None).await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let detail: String = response.text().await.unwrap_or_default().chars().take(500).collect();
        let verb = if method == Method::POST { "add" } else { "remove" };
        tracing::error!("Failed to {verb} forum reaction ({status}): {detail}");
        return Err(AppError::msg(format!("Failed to {verb} reaction ({status})")));
    }
    state.track(
        "forum.reaction",
        json!({ "community_id": group, "action": if method == Method::POST { "add" } else { "remove" }, "emote_id": emote, "is_post_comment": is_post_comment }),
    );
    Ok(())
}

pub fn register(router: &mut Router) {
    router
        .handle("forums:getFeatures", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            Ok(match ctx.state.roblox.get::<Value>(&format!("{}/product-features", base(group))).await {
                Ok(raw) => {
                    let read = raw.get("ForumsRichTextRead").and_then(Value::as_bool).unwrap_or(false);
                    let write = raw.get("ForumsRichTextWrite").and_then(Value::as_bool).unwrap_or(false);
                    json!({ "enabled": read || write, "richTextRead": read, "richTextWrite": write, "ageCheck": raw.get("ForumsAgeCheck").and_then(Value::as_bool).unwrap_or(false) })
                }
                Err(error) => {
                    tracing::error!("Failed to fetch forum features: {error}");
                    json!({ "ageCheck": false, "enabled": false, "richTextRead": false, "richTextWrite": false })
                }
            })
        })
        .handle("forums:getChannels", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let c: Option<String> = args.opt(1)?;
            let url = match c.filter(|c| !c.is_empty()) {
                Some(c) => format!("{}/forums?cursor={}", base(group), enc(&c)),
                None => format!("{}/forums", base(group)),
            };
            Ok(match ctx.state.roblox.get::<Value>(&url).await {
                Ok(raw) => page_of(&raw, |ch| {
                    json!({
                        "id": ch.get("id"), "groupId": ch.get("groupId"), "name": ch.get("name"),
                        "description": or_null(ch.get("description")), "parentChannelId": or_null(ch.get("parentChannelId")),
                        "rank": or_null(ch.get("rank")), "isRestricted": ch.get("isRestricted").and_then(Value::as_bool).unwrap_or(false),
                    })
                }),
                Err(error) => {
                    tracing::error!("Failed to fetch forum channels: {error}");
                    empty_page()
                }
            })
        })
        .handle("forums:getPosts", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let c: Option<String> = args.opt(2)?;
            let limit: Option<u64> = args.opt(3)?;
            let state = &ctx.state;
            let mut url = format!("{}/forums/{}/posts?includeCommentCount=true&limit={}", base(group), enc(&channel), limit.unwrap_or(PAGE_LIMIT));
            if let Some(c) = c.filter(|c| !c.is_empty()) {
                url.push_str(&format!("&cursor={}", enc(&c)));
            }
            let (response, emotes) = futures::join!(state.roblox.request(Method::GET, &url, None), emote_names(state, group));
            let response = match response {
                Ok(r) => r,
                Err(error) => {
                    tracing::error!("Failed to fetch forum posts: {error}");
                    return Ok(empty_page());
                }
            };
            if response.status().as_u16() == 403 {
                let mut page = empty_page();
                page["accessDenied"] = json!(true);
                return Ok(page);
            }
            if !response.status().is_success() {
                return Ok(empty_page());
            }
            let raw: Value = response.json().await.unwrap_or(Value::Null);
            Ok(page_of(&raw, |p| map_post(p, &emotes)))
        })
        .handle("forums:getPinnedPosts", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let state = &ctx.state;
            let url = format!("{}/forums/{}/posts/pinned?includeCommentCount=true", base(group), enc(&channel));
            let (raw, emotes) = futures::join!(state.roblox.get::<Value>(&url), emote_names(state, group));
            Ok(match raw {
                Ok(raw) => raw.get("data").and_then(Value::as_array).into_iter().flatten().map(|p| map_post(p, &emotes)).collect(),
                Err(error) => {
                    tracing::error!("Failed to fetch pinned forum posts: {error}");
                    Vec::new()
                }
            })
        })
        .handle("forums:getPost", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let short: String = args.get(2)?;
            let state = &ctx.state;
            let url = format!("{}/forums/{}/posts?postIds={}", base(group), enc(&channel), enc(&short));
            let (raw, emotes) = futures::join!(state.roblox.get::<Value>(&url), emote_names(state, group));
            Ok(match raw {
                Ok(raw) => raw.pointer("/data/0").map(|p| map_post(p, &emotes)).unwrap_or(Value::Null),
                Err(error) => {
                    tracing::error!("Failed to fetch forum post: {error}");
                    Value::Null
                }
            })
        })
        .handle("forums:getComments", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let post: String = args.get(2)?;
            let comment: Option<String> = args.opt(3)?;
            let c: Option<String> = args.opt(4)?;
            let limit: Option<u64> = args.opt(5)?;
            let state = &ctx.state;
            let mut url = format!("{}/forums/{}/posts/{}/comments?limit={}", base(group), enc(&channel), enc(&post), limit.unwrap_or(PAGE_LIMIT));
            if let Some(id) = comment.filter(|c| !c.is_empty()) {
                url.push_str(&format!("&commentId={}", enc(&id)));
            }
            if let Some(c) = c.filter(|c| !c.is_empty()) {
                url.push_str(&format!("&cursor={}", enc(&c)));
            }
            let (raw, emotes) = futures::join!(state.roblox.get::<Value>(&url), emote_names(state, group));
            Ok(match raw {
                Ok(raw) => page_of(&raw, |c| map_comment(c, &emotes)),
                Err(error) => {
                    tracing::error!("Failed to fetch forum comments: {error}");
                    empty_page()
                }
            })
        })
        .handle("forums:markRead", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let post: String = args.get(2)?;
            let comment: String = args.get(3)?;
            let url = format!("{}/forums/{}/posts/{}/comments/{}/read", base(group), enc(&channel), enc(&post), enc(&comment));
            if let Err(error) = ctx.state.roblox.request(Method::PATCH, &url, Some(&json!({}))).await {
                tracing::warn!("Failed to mark forum thread read: {error}");
            }
            Ok(Value::Null)
        })
        .handle("forums:getEmotes", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            Ok(match ctx.state.roblox.get::<Value>(&format!("{}/emotes", base(group))).await {
                Ok(raw) => emotes_list(&raw).into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect(),
                Err(error) => {
                    tracing::error!("Failed to fetch forum emotes: {error}");
                    Vec::new()
                }
            })
        })
        .handle("forums:createPost", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let title: String = args.get(2)?;
            let body: String = args.get(3)?;
            let url = format!("{}/forums/{}/posts", base(group), enc(&channel));
            let raw: Value = ctx.state.roblox.post(&url, &json!({ "title": title, "slate": slate(&body) })).await?;
            ctx.state.track("forum.post.create", json!({ "community_id": group, "channel_id": channel }));
            Ok(map_post(&raw, &HashMap::new()))
        })
        .handle("forums:createComment", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let post: String = args.get(2)?;
            let body: String = args.get(3)?;
            let reply: Option<String> = args.opt(4)?;
            let mut payload = json!({ "slate": slate(&body) });
            if let Some(reply) = reply.as_ref().filter(|r| !r.is_empty()) {
                payload["repliesToPostCommentId"] = json!(reply);
            }
            let url = format!("{}/forums/{}/posts/{}/comments", base(group), enc(&channel), enc(&post));
            let raw: Value = ctx.state.roblox.post(&url, &payload).await?;
            ctx.state.track("forum.comment.create", json!({ "community_id": group, "channel_id": channel, "is_reply": reply.is_some() }));
            Ok(map_comment(&raw, &HashMap::new()))
        })
        .handle("forums:editComment", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let parent: String = args.get(2)?;
            let comment: String = args.get(3)?;
            let body: String = args.get(4)?;
            let url = format!("{}/forums/{}/posts/{}/comments/{}", base(group), enc(&channel), enc(&parent), enc(&comment));
            let raw: Value = ctx.state.roblox.patch(&url, &json!({ "slate": slate(&body) })).await?;
            Ok(map_comment(&raw, &HashMap::new()))
        })
        .handle("forums:deleteComment", |ctx, args| async move {
            let group: u64 = args.get(0)?;
            let channel: String = args.get(1)?;
            let parent: String = args.get(2)?;
            let comment: String = args.get(3)?;
            let url = format!("{}/forums/{}/posts/{}/comments/{}", base(group), enc(&channel), enc(&parent), enc(&comment));
            ctx.state.roblox.delete::<Value>(&url).await?;
            Ok(Value::Null)
        })
        .handle("forums:addReaction", |ctx, args| async move {
            let list: Vec<Value> = (0..6).map(|i| args.opt::<Value>(i).ok().flatten().unwrap_or(Value::Null)).collect();
            reaction(&ctx.state, Method::POST, &list).await?;
            Ok(Value::Null)
        })
        .handle("forums:removeReaction", |ctx, args| async move {
            let list: Vec<Value> = (0..6).map(|i| args.opt::<Value>(i).ok().flatten().unwrap_or(Value::Null)).collect();
            reaction(&ctx.state, Method::DELETE, &list).await?;
            Ok(Value::Null)
        });
}
