//! Friends and follows (`friends:*`). Port of `FriendsService`.
//! All 14 channels; response shapes match the Electron mapping exactly.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::error::AppResult;
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::state::AppState;

const FRIEND_STATUSES: [&str; 4] = ["NotFriends", "Friends", "RequestSent", "RequestReceived"];

struct Names {
    name: String,
    display_name: String,
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn data_array(mut body: Value, key: &str) -> Vec<Value> {
    match body.get_mut(key).map(Value::take) {
        Some(Value::Array(list)) => list,
        _ => Vec::new(),
    }
}

/// `fetchUserDetails`: resolves usernames/display names in one call.
async fn fetch_user_details(state: &AppState, user_ids: Vec<u64>) -> HashMap<u64, Names> {
    if user_ids.is_empty() {
        return HashMap::new();
    }
    let url = format!("{}/user/profiles/get-profiles", endpoints::USER_PROFILE_API);
    let body = json!({
        "userIds": user_ids,
        "fields": ["names.combinedName", "names.username", "isVerified", "isDeleted"],
    });
    let response: Value = match state.roblox.post(&url, &body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "Failed to fetch user details");
            return HashMap::new();
        }
    };
    response
        .get("profileDetails")
        .and_then(Value::as_array)
        .map(|profiles| {
            profiles
                .iter()
                .filter_map(|p| {
                    let id = u64_of(p, "userId")?;
                    let names = p.get("names");
                    Some((
                        id,
                        Names {
                            name: names
                                .map(|n| str_of(n, "username"))
                                .unwrap_or_default()
                                .to_owned(),
                            display_name: names
                                .map(|n| str_of(n, "combinedName"))
                                .unwrap_or_default()
                                .to_owned(),
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn needs_names(f: &Value) -> Option<u64> {
    let id = u64_of(f, "id")?;
    let (name, display) = (str_of(f, "name"), str_of(f, "displayName"));
    (name.is_empty() || display.is_empty()).then_some(id)
}

fn first_non_empty<'a>(options: &[&'a str]) -> &'a str {
    options
        .iter()
        .copied()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

pub async fn list_friends(state: &AppState, user_id: u64) -> Vec<Value> {
    let url = format!("{}/users/{user_id}/friends", endpoints::FRIENDS_V1);
    let friends = match state.roblox.get::<Value>(&url).await {
        Ok(body) => data_array(body, "data"),
        Err(error) => {
            tracing::error!(user_id, %error, "Failed to get friends");
            return Vec::new();
        }
    };
    let details = fetch_user_details(state, friends.iter().filter_map(needs_names).collect()).await;
    friends
        .iter()
        .map(|f| {
            let id = u64_of(f, "id").unwrap_or(0);
            let d = details.get(&id);
            let name = first_non_empty(&[
                d.map_or("", |d| d.name.as_str()),
                str_of(f, "name"),
                str_of(f, "displayName"),
            ]);
            let display = first_non_empty(&[
                d.map_or("", |d| d.display_name.as_str()),
                str_of(f, "displayName"),
            ]);
            let online = f.get("isOnline").and_then(Value::as_bool).unwrap_or(false);
            json!({
                "id": id,
                "name": name,
                "displayName": display,
                "isOnline": online,
                "isDeleted": f.get("isDeleted").and_then(Value::as_bool).unwrap_or(false),
                "description": str_of(f, "description"),
                "created": str_of(f, "created"),
                "externalAppDisplayName": f.get("externalAppDisplayName"),
                "presenceType": if online { 1 } else { 0 },
                "friendFrequentScore": 0,
                "friendFrequentRank": 0,
            })
        })
        .collect()
}

async fn get_online_friends(state: &AppState, user_id: u64) -> Vec<Value> {
    let url = format!("{}/users/{user_id}/friends/online", endpoints::FRIENDS_V1);
    let friends = match state.roblox.get::<Value>(&url).await {
        Ok(body) => data_array(body, "data"),
        Err(error) => {
            tracing::error!(user_id, %error, "Failed to get online friends");
            return Vec::new();
        }
    };
    let details = fetch_user_details(state, friends.iter().filter_map(needs_names).collect()).await;
    friends
        .iter()
        .map(|f| {
            let id = u64_of(f, "id").unwrap_or(0);
            let d = details.get(&id);
            json!({
                "id": id,
                "name": first_non_empty(&[str_of(f, "name"), d.map_or("", |d| d.name.as_str()), str_of(f, "displayName")]),
                "displayName": first_non_empty(&[str_of(f, "displayName"), d.map_or("", |d| d.display_name.as_str())]),
                "isOnline": true,
                "presenceType": 1,
                "isDeleted": false,
                "friendFrequentScore": 0,
                "friendFrequentRank": 0,
            })
        })
        .collect()
}

pub async fn get_friend_requests(state: &AppState) -> Vec<Value> {
    let url = format!("{}/my/friends/requests?limit=100", endpoints::FRIENDS_V1);
    let requests = match state.roblox.get::<Value>(&url).await {
        Ok(body) => data_array(body, "data"),
        Err(error) => {
            tracing::error!(%error, "Failed to get friend requests");
            return Vec::new();
        }
    };
    let missing = requests
        .iter()
        .filter(|r| str_of(r, "name").is_empty() && str_of(r, "displayName").is_empty())
        .filter_map(|r| u64_of(r, "id"))
        .collect();
    let details = fetch_user_details(state, missing).await;
    let now = crate::util::now_iso();
    requests
        .iter()
        .map(|r| {
            let fr = r.get("friendRequest");
            let sender = u64_of(r, "id").or_else(|| fr.and_then(|f| u64_of(f, "senderId"))).unwrap_or(0);
            let d = details.get(&sender);
            let name = first_non_empty(&[str_of(r, "name"), d.map_or("", |d| d.name.as_str()), str_of(r, "displayName")]);
            let display = first_non_empty(&[str_of(r, "displayName"), d.map_or("", |d| d.display_name.as_str())]);
            json!({
                "id": sender,
                "senderId": sender,
                "sourceUniverseId": fr.and_then(|f| f.get("sourceUniverseId")),
                "contactName": fr.and_then(|f| f.get("contactName")),
                "senderNickname": fr.and_then(|f| f.get("senderNickname")),
                "sentAt": fr.and_then(|f| f.get("sentAt")).and_then(Value::as_str).unwrap_or(now.as_str()),
                "sender": {
                    "id": sender,
                    "name": name,
                    "displayName": display,
                    "hasVerifiedBadge": r.get("hasVerifiedBadge").and_then(Value::as_bool).unwrap_or(false),
                },
                "mutualFriendsList": r.get("mutualFriendsList").cloned().unwrap_or_else(|| json!([])),
            })
        })
        .collect()
}

async fn check_friend_status(state: &AppState, user_id: u64) -> Option<String> {
    let me = state.auth.current_user_id()?;
    let url = format!(
        "{}/users/{me}/friends/statuses?userIds={user_id}",
        endpoints::FRIENDS_V1
    );
    match state.roblox.get::<Value>(&url).await {
        Ok(body) => body
            .pointer("/data/0/status")
            .and_then(Value::as_str)
            .filter(|s| FRIEND_STATUSES.contains(s))
            .map(str::to_owned),
        Err(error) => {
            tracing::error!(user_id, %error, "Failed to get friend status");
            None
        }
    }
}

async fn is_following(state: &AppState, user_id: u64) -> bool {
    let url = format!("{}/user/following-exists", endpoints::FRIENDS_V1);
    match state
        .roblox
        .post::<_, Value>(&url, &json!({ "targetUserIds": [user_id] }))
        .await
    {
        Ok(body) => body
            .pointer("/followings/0/isFollowing")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        Err(error) => {
            tracing::error!(user_id, %error, "Failed to get following status");
            false
        }
    }
}

async fn search_friends(state: &AppState, user_id: u64, query: &str, limit: u32) -> Vec<Value> {
    let url = format!(
        "{}/users/{user_id}/friends/search?query={}&limit={limit}",
        endpoints::FRIENDS_V1,
        urlencoding::encode(query)
    );
    let friends = match state.roblox.get::<Value>(&url).await {
        Ok(body) => data_array(body, "PageItems"),
        Err(error) => {
            tracing::error!(user_id, %error, "Failed to search friends");
            return Vec::new();
        }
    };
    let ids: Vec<u64> = friends.iter().filter_map(|f| u64_of(f, "id")).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let details = fetch_user_details(state, ids).await;
    friends
        .iter()
        .map(|f| {
            let id = u64_of(f, "id").unwrap_or(0);
            let d = details.get(&id);
            json!({
                "id": id,
                "name": d.map_or("", |d| d.name.as_str()),
                "displayName": d.map_or("", |d| d.display_name.as_str()),
                "isOnline": false,
                "isDeleted": false,
                "description": "",
                "created": "",
                "externalAppDisplayName": null,
                "presenceType": 0,
                "friendFrequentScore": 0,
                "friendFrequentRank": 0,
            })
        })
        .collect()
}

/// Mutations propagate errors (the UI shows a toast), like the original.
async fn post_action(state: &AppState, path: String, body: Option<Value>) -> AppResult<()> {
    let url = format!("{}{path}", endpoints::FRIENDS_V1);
    match body {
        Some(body) => state.roblox.post::<_, Value>(&url, &body).await?,
        None => state.roblox.post_empty::<Value>(&url).await?,
    };
    tracing::info!(%path, "friends action ok");
    state.social.on_user_acted();
    Ok(())
}

pub fn register(router: &mut Router) {
    router
        .handle("friends:getFriends", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            Ok(list_friends(&ctx.state, user_id).await)
        })
        .handle("friends:getOnlineFriends", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            Ok(get_online_friends(&ctx.state, user_id).await)
        })
        .handle("friends:getFriendRequests", |ctx, _| async move {
            Ok(get_friend_requests(&ctx.state).await)
        })
        .handle("friends:acceptFriendRequest", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(
                &ctx.state,
                format!("/users/{id}/accept-friend-request"),
                None,
            )
            .await
        })
        .handle("friends:declineFriendRequest", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(
                &ctx.state,
                format!("/users/{id}/decline-friend-request"),
                None,
            )
            .await
        })
        .handle("friends:declineAllFriendRequests", |ctx, _| async move {
            post_action(
                &ctx.state,
                "/user/friend-requests/decline-all".to_owned(),
                Some(json!({})),
            )
            .await
        })
        .handle("friends:sendFriendRequest", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(
                &ctx.state,
                format!("/users/{id}/request-friendship"),
                Some(json!({})),
            )
            .await
        })
        .handle("friends:unfriend", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(&ctx.state, format!("/users/{id}/unfriend"), None).await
        })
        .handle("friends:follow", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(&ctx.state, format!("/users/{id}/follow"), None).await
        })
        .handle("friends:unfollow", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            post_action(&ctx.state, format!("/users/{id}/unfollow"), None).await
        })
        .handle("friends:isFollowing", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(is_following(&ctx.state, id).await)
        })
        .handle("friends:checkFriendStatus", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(check_friend_status(&ctx.state, id).await)
        })
        .handle("friends:getFriendStatus", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(check_friend_status(&ctx.state, id)
                .await
                .unwrap_or_else(|| "NotFriends".to_owned()))
        })
        .handle("friends:searchFriends", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            let query: String = args.get(1)?;
            let limit = args.opt::<u32>(2)?.unwrap_or(50);
            Ok(search_friends(&ctx.state, user_id, &query, limit).await)
        });
}
