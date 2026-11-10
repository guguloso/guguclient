//! Roblox chat (`chat:*`, `chatPoller:*`). Port of `ChatService` and
//! `ChatPollerService`: platform-chat-api calls plus a metadata poller that
//! pushes unread counts, conversation lists and the open thread's messages.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::services::poller::{Ladder, PollJob, Poller, Tier, FOREVER};
use crate::state::AppState;

const IDLE_LADDER: Ladder = &[Tier {
    interval_ms: 5_000,
    duration_ms: FOREVER,
}];
const ACTIVE_LADDER: Ladder = &[
    Tier {
        interval_ms: 500,
        duration_ms: 30_000,
    },
    Tier {
        interval_ms: 1_000,
        duration_ms: 30_000,
    },
    Tier {
        interval_ms: 5_000,
        duration_ms: FOREVER,
    },
];
const BACKGROUND_LADDER: Ladder = &[Tier {
    interval_ms: 300_000,
    duration_ms: FOREVER,
}];
const CONVERSATION_LIST_PAGE_SIZE: u64 = 50;
const MESSAGES_PAGE_SIZE: u64 = 50;
const FAILED_STATUSES: [&str; 6] = [
    "error", "failed", "rejected", "not_sent", "blocked", "denied",
];

// ------------------------------------------------------------------ mapping

fn s(v: &Value, k: &str) -> Value {
    Value::String(v.get(k).and_then(Value::as_str).unwrap_or("").to_owned())
}
fn b(v: &Value, k: &str) -> Value {
    Value::Bool(v.get(k).and_then(Value::as_bool).unwrap_or(false))
}
fn n(v: &Value, k: &str) -> Value {
    v.get(k)
        .filter(|x| x.is_number())
        .cloned()
        .unwrap_or(json!(0))
}
fn opt(out: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v.filter(|v| !v.is_null()) {
        out.insert(key.to_owned(), v.clone());
    }
}

fn is_hidden_system(msg: &Value) -> bool {
    msg.get("type").and_then(Value::as_str) == Some("system")
        && msg.get("visibility").and_then(Value::as_str) == Some("invalid")
}

/// `transformMessage`.
fn transform_message(msg: &Value) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), s(msg, "id"));
    out.insert("content".into(), s(msg, "content"));
    out.insert("senderUserId".into(), n(msg, "sender_user_id"));
    out.insert("createdAt".into(), s(msg, "created_at"));
    out.insert("moderationType".into(), s(msg, "moderation_type"));
    out.insert("type".into(), s(msg, "type"));
    opt(&mut out, "repliesToId", msg.get("replies_to_id"));
    out.insert("isDeleted".into(), b(msg, "is_deleted"));
    out.insert("isPreviewable".into(), b(msg, "is_previewable"));
    out.insert("isBadgeable".into(), b(msg, "is_badgeable"));
    out.insert("visibility".into(), s(msg, "visibility"));
    Value::Object(out)
}

fn visible_messages(list: Option<&Value>) -> Vec<Value> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| !is_hidden_system(m))
        .map(transform_message)
        .collect()
}

/// `transformConversation`.
fn transform_conversation(conv: &Value) -> Value {
    let mut user_data = Map::new();
    if let Some(map) = conv.get("user_data").and_then(Value::as_object) {
        for (key, value) in map {
            let fallback: u64 = key.parse().unwrap_or(0);
            user_data.insert(
                fallback.to_string(),
                json!({
                    "id": value.get("id").filter(|v| v.is_number()).cloned().unwrap_or(json!(fallback)),
                    "name": s(value, "name"),
                    "displayName": s(value, "display_name"),
                    "combinedName": s(value, "combined_name"),
                    "isVerified": b(value, "is_verified"),
                }),
            );
        }
    }
    let mut out = Map::new();
    let source = if conv.get("source").and_then(Value::as_str) == Some("channels") {
        "channels"
    } else {
        "friends"
    };
    out.insert("source".into(), json!(source));
    out.insert("id".into(), conv.get("id").cloned().unwrap_or(Value::Null));
    out.insert("type".into(), s(conv, "type"));
    out.insert("name".into(), s(conv, "name"));
    out.insert("isDefaultName".into(), b(conv, "is_default_name"));
    opt(&mut out, "createdBy", conv.get("created_by"));
    out.insert(
        "participantUserIds".into(),
        conv.get("participant_user_ids")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or(json!([])),
    );
    out.insert("userData".into(), Value::Object(user_data));
    out.insert(
        "messages".into(),
        Value::Array(visible_messages(conv.get("messages"))),
    );
    out.insert("unreadMessageCount".into(), n(conv, "unread_message_count"));
    out.insert("updatedAt".into(), s(conv, "updated_at"));
    out.insert("createdAt".into(), s(conv, "created_at"));
    out.insert("sortIndex".into(), n(conv, "sort_index"));
    out.insert(
        "participantPendingStatus".into(),
        s(conv, "participant_pending_status"),
    );
    out.insert(
        "osaAcknowledgementStatus".into(),
        s(conv, "osa_acknowledgement_status"),
    );
    opt(&mut out, "moderationType", conv.get("moderation_type"));
    opt(
        &mut out,
        "userPendingStatus",
        conv.get("user_pending_status"),
    );
    if let Some(preview) = conv
        .get("preview_message")
        .filter(|p| p.is_object() && !is_hidden_system(p))
    {
        out.insert("previewMessage".into(), transform_message(preview));
    }
    Value::Object(out)
}

/// `classifyChatUnavailableReason`.
fn classify_unavailable(metadata: &Value, info: Option<&Value>, settings: Option<&Value>) -> Value {
    let info_flag = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_bool);
    let meta_str = |k: &str| metadata.get(k).and_then(Value::as_str);
    if info_flag("missingParentEmail") == Some(true) {
        return json!({ "tag": "parental-restricted" });
    }
    if info_flag("userAbove13") == Some(false) {
        return json!({ "tag": "under-13" });
    }
    if meta_str("isChatEnabledByGlobalRules") == Some("disabled") {
        return json!({ "tag": "account-restricted" });
    }
    if info_flag("isAppChatSettingEnabled") == Some(false) {
        return json!({ "tag": "app-chat-disabled" });
    }
    let setting = |k: &str| settings.and_then(|s| s.get(k)).and_then(Value::as_str);
    if meta_str("isChatEnabledByPrivacySetting") == Some("disabled")
        || setting("whoCanChatWithMeInApp") == Some("NoOne")
    {
        return json!({ "tag": "privacy-restricted" });
    }
    if !metadata
        .get("isChatUserMessagesEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return json!({ "tag": "messages-disabled" });
    }
    if setting("allowFacialAgeEstimation") == Some("Disabled") {
        return json!({ "tag": "account-restricted" });
    }
    json!({ "tag": "generic", "robloxMessage": metadata.get("languageForPrivacySettingUnavailable") })
}

fn api(path: &str) -> String {
    format!("{}{path}", endpoints::PLATFORM_CHAT_API)
}

// ------------------------------------------------------------------ service calls

async fn conversation_metadata(state: &AppState) -> (u64, u64) {
    match state
        .roblox
        .get::<Value>(&api("/get-conversation-metadata"))
        .await
    {
        Ok(r) => (
            r.get("global_unread_message_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            r.get("global_unread_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        Err(error) => {
            tracing::error!("Failed to fetch conversation metadata: {error}");
            (0, 0)
        }
    }
}

async fn chat_availability(state: &AppState, user_id: u64) -> Value {
    let metadata = match state
        .roblox
        .get::<Value>(&api(&format!("/metadata?userId={user_id}")))
        .await
    {
        Ok(m) => m,
        Err(error) => {
            tracing::error!("Failed to fetch chat settings metadata: {error}");
            return json!({ "available": false, "reason": { "tag": "generic" } });
        }
    };
    let flag = |k: &str| metadata.get(k).and_then(Value::as_bool).unwrap_or(false);
    if flag("isChatEnabled") && flag("isChatVisible") && flag("isChatUserMessagesEnabled") {
        return json!({ "available": true });
    }
    let (settings, info) = crate::services::user_settings::refresh(state).await;
    json!({ "available": false, "reason": classify_unavailable(&metadata, info.as_ref(), settings.as_ref()) })
}

async fn user_conversations(state: &AppState, cursor: Option<String>, page_size: u64) -> Value {
    let mut url = api(&format!(
        "/get-user-conversations?include_user_data=true&pageSize={page_size}"
    ));
    if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
        url.push_str(&format!("&cursor={}", urlencoding::encode(&cursor)));
    }
    match state.roblox.get::<Value>(&url).await {
        Ok(r) => json!({
            "conversations": r.get("conversations").and_then(Value::as_array).into_iter().flatten()
                .map(transform_conversation).collect::<Vec<_>>(),
            "nextCursor": r.get("next_cursor").and_then(Value::as_str).unwrap_or(""),
        }),
        Err(error) => {
            tracing::error!("Failed to fetch user conversations: {error}");
            json!({ "conversations": [], "nextCursor": "" })
        }
    }
}

async fn conversation_messages(
    state: &AppState,
    id: &str,
    page_size: Option<u64>,
    cursor: Option<String>,
) -> Value {
    let mut url = api(&format!(
        "/get-conversation-messages?conversation_id={}",
        urlencoding::encode(id)
    ));
    if let Some(size) = page_size {
        url.push_str(&format!("&page_size={size}"));
    }
    if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
        url.push_str(&format!("&cursor={}", urlencoding::encode(&cursor)));
    }
    match state.roblox.get::<Value>(&url).await {
        Ok(r) => {
            let mut out = Map::new();
            out.insert(
                "messages".into(),
                Value::Array(visible_messages(r.get("messages"))),
            );
            opt(&mut out, "nextCursor", r.get("next_cursor"));
            opt(&mut out, "previousCursor", r.get("previous_cursor"));
            Value::Object(out)
        }
        Err(error) => {
            tracing::error!("Failed to fetch conversation messages: {error}");
            json!({ "messages": [] })
        }
    }
}

async fn send_message(state: &AppState, id: String, content: String) -> AppResult<Value> {
    let body = json!({ "conversation_id": id, "messages": [{ "content": content }] });
    let result: AppResult<Value> = async {
        let r: Value = state.roblox.post(&api("/send-messages"), &body).await?;
        let messages: Vec<Value> = r
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|m| {
                let mut out = Map::new();
                out.insert("id".into(), s(m, "id"));
                out.insert("content".into(), s(m, "content"));
                out.insert("senderUserId".into(), n(m, "sender_user_id"));
                out.insert("createdAt".into(), s(m, "created_at"));
                out.insert("moderationType".into(), s(m, "moderation_type"));
                out.insert("type".into(), s(m, "type"));
                opt(&mut out, "repliesToId", m.get("replies_to"));
                out.insert("isDeleted".into(), b(m, "is_deleted"));
                out.insert("isPreviewable".into(), b(m, "is_previewable"));
                out.insert("isBadgeable".into(), b(m, "is_badgeable"));
                out.insert("visibility".into(), s(m, "visibility"));
                opt(&mut out, "status", m.get("status"));
                Value::Object(out)
            })
            .collect();
        let failed = messages.iter().any(|m| {
            m.get("status")
                .and_then(Value::as_str)
                .is_some_and(|st| FAILED_STATUSES.contains(&st.to_lowercase().as_str()))
        });
        if messages.is_empty() || failed {
            return Err(AppError::msg("Chat did not accept the message"));
        }
        Ok(json!({ "messages": messages }))
    }
    .await;
    if let Err(error) = &result {
        tracing::error!(conversation_id = %body["conversation_id"], "Failed to send message: {error}");
    }
    result
}

async fn status_success(state: &AppState, path: &str, body: Value, what: &str) -> bool {
    match state.roblox.post::<_, Value>(&api(path), &body).await {
        Ok(r) => r.get("status").and_then(Value::as_str) == Some("success"),
        Err(error) => {
            tracing::error!("Failed to {what}: {error}");
            false
        }
    }
}

// ------------------------------------------------------------------ poller

#[derive(Default)]
struct PollerData {
    last_global_unread: u64,
    last_conversation_unread: HashMap<String, u64>,
    last_conversations: Option<Vec<String>>,
    last_messages: Option<(String, Vec<String>)>,
    viewed: Option<String>,
    route_active: bool,
}

pub struct ChatPoller {
    poller: Arc<Poller>,
    data: Mutex<PollerData>,
    started: AtomicBool,
    fetching_messages: AtomicBool,
}

fn conversation_signature(c: &Value) -> String {
    let str_of = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_owned();
    let preview = c.get("previewMessage");
    let ids: Vec<String> = c
        .get("participantUserIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(Value::to_string)
        .collect();
    [
        str_of(c.get("id")),
        str_of(c.get("updatedAt")),
        c.get("unreadMessageCount")
            .map(Value::to_string)
            .unwrap_or_default(),
        str_of(preview.and_then(|p| p.get("id"))),
        str_of(preview.and_then(|p| p.get("content"))),
        str_of(c.get("name")),
        ids.join(","),
    ]
    .join("|")
}

fn message_signature(m: &Value) -> String {
    let str_of = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let deleted = if m.get("isDeleted").and_then(Value::as_bool).unwrap_or(false) {
        "1"
    } else {
        "0"
    };
    [
        str_of("id"),
        str_of("content"),
        str_of("moderationType"),
        deleted.to_owned(),
        str_of("status"),
        str_of("visibility"),
    ]
    .join("|")
}

struct MetadataJob(Weak<AppState>);

impl PollJob for MetadataJob {
    fn should_run(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|s| s.auth.current_user_id().is_some())
    }

    async fn run(&self) -> AppResult<()> {
        let Some(state) = self.0.upgrade() else {
            return Ok(());
        };
        let (unread, _) = conversation_metadata(&state).await;
        let (increased, viewing) = {
            let d = state.chat.data.lock();
            (unread > d.last_global_unread, d.viewed.is_some())
        };
        let conversations = if increased || viewing {
            let r = user_conversations(&state, None, CONVERSATION_LIST_PAGE_SIZE).await;
            r.get("conversations").and_then(Value::as_array).cloned()
        } else {
            None
        };
        handle_metadata(&state, unread, conversations);
        Ok(())
    }
}

fn handle_metadata(state: &Arc<AppState>, unread: u64, conversations: Option<Vec<Value>>) {
    let refetch = {
        let mut d = state.chat.data.lock();
        if unread != d.last_global_unread {
            d.last_global_unread = unread;
            state
                .events
                .emit("chat:unreadCountChanged", json!({ "count": unread }));
        }
        let Some(conversations) = conversations else {
            return;
        };
        let sigs: Vec<String> = conversations.iter().map(conversation_signature).collect();
        if d.last_conversations.as_ref() != Some(&sigs) {
            d.last_conversations = Some(sigs);
            state.events.emit(
                "chat:conversationsChanged",
                json!({ "conversations": conversations }),
            );
        }
        let viewed = d.viewed.clone();
        let mut increased = false;
        for conv in &conversations {
            let Some(key) = conv.get("id").and_then(Value::as_str) else {
                continue;
            };
            let current = conv
                .get("unreadMessageCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let previous = d
                .last_conversation_unread
                .insert(key.to_owned(), current)
                .unwrap_or(0);
            if current > previous && viewed.as_deref() == Some(key) {
                increased = true;
            }
        }
        increased.then_some(viewed).flatten()
    };
    if let Some(id) = refetch {
        fetch_messages(state, id);
    }
}

/// `fetchMessages`: one in flight at a time, dropped if the view changed.
fn fetch_messages(state: &Arc<AppState>, conversation_id: String) {
    if state.chat.fetching_messages.swap(true, Ordering::AcqRel) {
        return;
    }
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        let r =
            conversation_messages(&state, &conversation_id, Some(MESSAGES_PAGE_SIZE), None).await;
        state.chat.fetching_messages.store(false, Ordering::Release);
        let messages = r
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut d = state.chat.data.lock();
        if d.viewed.as_deref() != Some(conversation_id.as_str()) {
            return;
        }
        let sigs: Vec<String> = messages.iter().map(message_signature).collect();
        if d.last_messages
            .as_ref()
            .is_some_and(|(id, prev)| *id == conversation_id && *prev == sigs)
        {
            return;
        }
        d.last_messages = Some((conversation_id.clone(), sigs));
        state.events.emit(
            "chat:messagesChanged",
            json!({ "conversationId": conversation_id, "messages": messages }),
        );
    });
}

impl ChatPoller {
    pub fn new() -> Self {
        Self {
            poller: Poller::new("chat-metadata", IDLE_LADDER, Some(BACKGROUND_LADDER)),
            data: Mutex::new(PollerData::default()),
            started: AtomicBool::new(false),
            fetching_messages: AtomicBool::new(false),
        }
    }

    pub fn start(state: &Arc<AppState>) {
        let chat = &state.chat;
        if chat.started.swap(true, Ordering::AcqRel) {
            return;
        }
        chat.poller
            .start(Arc::new(MetadataJob(Arc::downgrade(state))));
        let weak = Arc::downgrade(state);
        let mut accounts = state.auth.subscribe_account_changes();
        tauri::async_runtime::spawn(async move {
            while accounts.changed().await.is_ok() {
                let Some(state) = weak.upgrade() else { break };
                {
                    let mut d = state.chat.data.lock();
                    d.last_global_unread = 0;
                    d.last_conversation_unread.clear();
                    d.last_conversations = None;
                    d.last_messages = None;
                }
                state.chat.poller.stop();
                state
                    .chat
                    .poller
                    .start(Arc::new(MetadataJob(Arc::downgrade(&state))));
            }
        });
        tracing::info!("[ChatPoller] Started");
    }

    pub fn set_backgrounded(&self, backgrounded: bool) {
        self.poller.set_backgrounded(backgrounded);
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("chat:getConversationMetadata", |ctx, _| async move {
            let (messages, count) = conversation_metadata(&ctx.state).await;
            Ok(json!({ "globalUnreadMessageCount": messages, "globalUnreadCount": count }))
        })
        .handle("chat:getChatAvailability", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            Ok(chat_availability(&ctx.state, user_id).await)
        })
        .handle("chat:getUserConversations", |ctx, args| async move {
            let cursor: Option<String> = args.opt(0)?;
            let page_size: Option<u64> = args.opt(1)?;
            Ok(user_conversations(&ctx.state, cursor, page_size.unwrap_or(20)).await)
        })
        .handle("chat:getConversationMessages", |ctx, args| async move {
            let id: String = args.get(0)?;
            let page_size: Option<u64> = args.opt(1)?;
            let cursor: Option<String> = args.opt(2)?;
            Ok(conversation_messages(&ctx.state, &id, page_size, cursor).await)
        })
        .handle("chat:sendMessage", |ctx, args| async move {
            let id: String = args.get(0)?;
            let content: String = args.get(1)?;
            send_message(&ctx.state, id, content).await
        })
        .handle("chat:markAsRead", |ctx, args| async move {
            let ids: Vec<String> = args.get(0)?;
            match ctx.state.roblox.post::<_, Value>(&api("/mark-conversations"), &json!({ "conversation_ids": ids })).await {
                Ok(r) => Ok(r),
                Err(error) => {
                    tracing::error!("Failed to mark conversations as read: {error}");
                    Ok(json!({ "results": ids.iter().map(|id| json!({ "conversation_id": id, "status": "error" })).collect::<Vec<_>>() }))
                }
            }
        })
        .handle("chat:removeFromConversation", |ctx, args| async move {
            let id: String = args.get(0)?;
            let user_id: u64 = args.get(1)?;
            let body = json!({ "conversation_id": id, "user_ids": [user_id] });
            Ok(status_success(&ctx.state, "/remove-users", body, "remove from conversation").await)
        })
        .handle("chat:renameGroup", |ctx, args| async move {
            let id: String = args.get(0)?;
            let name: String = args.get(1)?;
            let body = json!({ "conversations": [{ "id": id, "name": name }] });
            let r: Value = ctx.state.roblox.post(&api("/update-conversations"), &body).await.inspect_err(|error| {
                tracing::error!("Failed to rename group chat: {error}");
            })?;
            let first = r.pointer("/conversations/0").filter(|c| c.get("status").and_then(Value::as_str) == Some("success"));
            let first = first.ok_or_else(|| AppError::msg("Failed to rename group chat"))?;
            Ok(transform_conversation(first))
        })
        .handle("chat:addUsersToConversation", |ctx, args| async move {
            let id: String = args.get(0)?;
            let user_ids: Vec<u64> = args.get(1)?;
            let body = json!({ "conversation_id": id, "user_ids": user_ids });
            Ok(status_success(&ctx.state, "/add-users", body, "add users to conversation").await)
        })
        .handle("chat:createConversation", |ctx, args| async move {
            let user_ids: Vec<u64> = args.get(0)?;
            let kind = if user_ids.len() == 1 { "one_to_one" } else { "group" };
            let body = json!({
                "conversations": [{ "type": kind, "participant_user_ids": user_ids }],
                "include_user_data": true,
            });
            match ctx.state.roblox.post::<_, Value>(&api("/create-conversations"), &body).await {
                Ok(r) => Ok(r.pointer("/conversations/0").map(transform_conversation)),
                Err(error) => {
                    tracing::error!("Failed to create conversation: {error}");
                    Ok(None)
                }
            }
        })
        .handle("chatPoller:setActiveConversation", |ctx, args| async move {
            let id: Option<String> = args.opt(0)?;
            let changed = {
                let mut d = ctx.state.chat.data.lock();
                let changed = d.viewed != id;
                d.viewed = id.clone();
                changed
            };
            if let (true, Some(id)) = (changed, id) {
                fetch_messages(&ctx.state, id);
            }
            Ok(Value::Null)
        })
        .handle("chatPoller:setChatRouteActive", |ctx, args| async move {
            let active: bool = args.get(0)?;
            let chat = &ctx.state.chat;
            let changed = {
                let mut d = chat.data.lock();
                std::mem::replace(&mut d.route_active, active) != active
            };
            if changed {
                chat.poller.set_ladder(if active { ACTIVE_LADDER } else { IDLE_LADDER }, Some(BACKGROUND_LADDER));
                if active {
                    chat.poller.trigger();
                }
            }
            Ok(Value::Null)
        })
        .handle("chatPoller:userSentMessage", |ctx, _| async move {
            ctx.state.chat.poller.trigger();
            let viewed = ctx.state.chat.data.lock().viewed.clone();
            if let Some(id) = viewed {
                fetch_messages(&ctx.state, id);
            }
            Ok(Value::Null)
        })
        .handle("chatPoller:markedAsRead", |ctx, args| async move {
            let id: String = args.get(0)?;
            ctx.state.chat.data.lock().last_conversation_unread.remove(&id);
            ctx.state.chat.poller.trigger();
            Ok(Value::Null)
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_mapping_hides_invalid_system_messages() {
        let conv = transform_conversation(&json!({
            "id": "c1", "source": "x", "user_data": { "7": { "display_name": "D" } },
            "messages": [{ "id": "m1", "type": "system", "visibility": "invalid" }, { "id": "m2", "content": "hi" }],
            "preview_message": { "type": "system", "visibility": "invalid" },
        }));
        assert_eq!(conv["source"], "friends");
        assert_eq!(conv["messages"].as_array().unwrap().len(), 1);
        assert_eq!(conv["userData"]["7"]["id"], 7);
        assert!(conv.get("previewMessage").is_none());
        assert_eq!(
            classify_unavailable(
                &json!({ "isChatEnabledByGlobalRules": "disabled" }),
                None,
                None
            )["tag"],
            "account-restricted"
        );
    }
}
