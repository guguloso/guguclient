//! Friends list + presence polling (`social:*`, `socialPoller:*`).
//! Port of `SocialPollerService` and `SocialCacheService`.
//!
//! Two pollers keep the sidebar live: the friends poller (friends, requests,
//! headshots) and the presence poller (who is online / in which game). Every
//! change is pushed with the same events the Electron build sent, and a
//! per-account snapshot is persisted to `settings/social-cache.json` so the
//! sidebar paints instantly on the next launch.
//!
//! Pending: the realtime socket (it only switches the ladders below).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::error::AppResult;
use crate::ipc::{EventBus, Router};
use crate::paths::LauncherPaths;
use crate::services::friends;
use crate::services::poller::{Ladder, PollJob, Poller, Tier, FOREVER};
use crate::services::thumbnails::{from_persisted_url, to_persisted_url};
use crate::state::AppState;
use crate::store::JsonStore;
use crate::util::{now_iso, now_millis};

const PRESENCE_TYPE_OFFLINE: u64 = 0;
const PRESENCE_TYPE_IN_GAME: u64 = 2;
const OFFLINE_TICK_RATIO: u64 = 6;
const AVATAR_HEADSHOT_SIZE: &str = "150x150";
const CACHE_STORE_NAME: &str = "social-cache";
const CACHE_WRITE_DEBOUNCE: Duration = Duration::from_millis(1_000);

const FRIENDS_LADDER: Ladder = &[
    Tier {
        interval_ms: 30_000,
        duration_ms: 60_000,
    },
    Tier {
        interval_ms: 300_000,
        duration_ms: FOREVER,
    },
];
const FRIENDS_BACKGROUND_LADDER: Ladder = &[Tier {
    interval_ms: 600_000,
    duration_ms: FOREVER,
}];
const PRESENCE_LADDER: Ladder = &[Tier {
    interval_ms: 30_000,
    duration_ms: FOREVER,
}];
const PRESENCE_BACKGROUND_LADDER: Ladder = &[Tier {
    interval_ms: 180_000,
    duration_ms: FOREVER,
}];

// ------------------------------------------------------------------ cache

/// Partial snapshot update (`updateSnapshot`); `None` keeps the old value.
#[derive(Default)]
struct SnapshotPatch<'a> {
    friends: Option<&'a [Value]>,
    friend_requests: Option<&'a [Value]>,
    last_seen: Option<&'a BTreeMap<u64, Value>>,
    last_games: Option<&'a BTreeMap<u64, Value>>,
    avatars: Option<&'a BTreeMap<u64, String>>,
}

pub struct SocialCache {
    store: JsonStore,
    pending: Mutex<HashMap<u64, Value>>,
    flush_scheduled: AtomicBool,
}

fn merged(previous: Option<&Value>, key: &str, patch: Option<Map<String, Value>>) -> Value {
    let mut base = previous
        .and_then(|p| p.get(key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(patch) = patch {
        base.extend(patch);
    }
    Value::Object(base)
}

fn to_record<V: serde::Serialize>(map: &BTreeMap<u64, V>) -> Map<String, Value> {
    map.iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            )
        })
        .collect()
}

impl SocialCache {
    fn open(paths: &LauncherPaths) -> AppResult<Arc<Self>> {
        let store = JsonStore::open(&paths.settings_dir, CACHE_STORE_NAME)?;
        Ok(Arc::new(Self {
            store,
            pending: Mutex::new(HashMap::new()),
            flush_scheduled: AtomicBool::new(false),
        }))
    }

    fn read_from_disk(&self, user_id: u64) -> Option<Value> {
        self.store
            .read(&format!("byUser.{user_id}"), |v| v.cloned())
    }

    /// Snapshot as stored (Electron URL scheme), pending writes first.
    fn stored_snapshot(&self, user_id: u64) -> Option<Value> {
        if let Some(pending) = self.pending.lock().get(&user_id) {
            return Some(pending.clone());
        }
        self.read_from_disk(user_id)
    }

    /// `getSnapshot`, with avatar URLs the WebView can load.
    pub fn snapshot(&self, user_id: u64) -> Option<Value> {
        let mut snapshot = self.stored_snapshot(user_id)?;
        if let Some(avatars) = snapshot.get_mut("avatars").and_then(Value::as_object_mut) {
            for url in avatars.values_mut() {
                if let Some(s) = url.as_str() {
                    *url = Value::String(from_persisted_url(s));
                }
            }
        }
        Some(snapshot)
    }

    fn update(self: &Arc<Self>, user_id: u64, patch: SnapshotPatch<'_>) {
        let previous = self.stored_snapshot(user_id);
        let previous = previous.as_ref();
        let list = |key: &str, new: Option<&[Value]>| match new {
            Some(items) => Value::Array(items.to_vec()),
            None => previous
                .and_then(|p| p.get(key))
                .cloned()
                .unwrap_or_else(|| json!([])),
        };
        let next = json!({
            "friends": list("friends", patch.friends),
            "friendRequests": list("friendRequests", patch.friend_requests),
            "lastSeen": merged(previous, "lastSeen", patch.last_seen.map(to_record)),
            "lastGames": merged(previous, "lastGames", patch.last_games.map(to_record)),
            "avatars": merged(previous, "avatars", patch.avatars.map(|m| {
                m.iter().map(|(k, v)| (k.to_string(), Value::String(to_persisted_url(v)))).collect()
            })),
            "savedAt": now_millis(),
        });
        self.pending.lock().insert(user_id, next);
        self.schedule_flush();
    }

    fn schedule_flush(self: &Arc<Self>) {
        if self.flush_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let cache = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(CACHE_WRITE_DEBOUNCE).await;
            cache.flush();
        });
    }

    /// Writes pending snapshots now (also called on account switch / exit).
    pub fn flush(&self) {
        self.flush_scheduled.store(false, Ordering::Release);
        let pending = std::mem::take(&mut *self.pending.lock());
        if pending.is_empty() {
            return;
        }
        let result = self.store.update(|root| {
            if !root.get("byUser").is_some_and(Value::is_object) {
                root["byUser"] = json!({});
            }
            let by_user = root["byUser"].as_object_mut().expect("just ensured object");
            for (user_id, entry) in pending {
                by_user.insert(user_id.to_string(), entry);
            }
        });
        if let Err(error) = result {
            tracing::error!(%error, "[SocialCache] Failed to flush snapshot");
        }
    }
}

// ------------------------------------------------------------------ state

#[derive(Default)]
struct SocialData {
    friends: Vec<Value>,
    friend_requests: Vec<Value>,
    presence: HashMap<u64, Value>,
    last_seen: BTreeMap<u64, Value>,
    last_games: BTreeMap<u64, Value>,
    avatars: BTreeMap<u64, String>,
    /// Signatures of the last broadcast lists (`areFriendListsEqual`).
    last_broadcast_friends: Option<Vec<String>>,
    last_broadcast_requests: Option<Vec<String>>,
    presence_tick: u64,
}

pub struct SocialService {
    cache: Arc<SocialCache>,
    data: Mutex<SocialData>,
    friends_poller: Arc<Poller>,
    presence_poller: Arc<Poller>,
    started: AtomicBool,
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn friend_signature(f: &Value) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        f.get("id").map(Value::to_string).unwrap_or_default(),
        str_field(f, "name"),
        str_field(f, "displayName"),
        u8::from(f.get("isOnline").and_then(Value::as_bool).unwrap_or(false)),
        u8::from(f.get("isDeleted").and_then(Value::as_bool).unwrap_or(false)),
        f.get("presenceType")
            .map(Value::to_string)
            .unwrap_or_default(),
    )
}

fn request_signature(r: &Value) -> String {
    let sender = r.get("sender");
    format!(
        "{}|{}|{}|{}|{}",
        r.get("id").map(Value::to_string).unwrap_or_default(),
        r.get("senderId").map(Value::to_string).unwrap_or_default(),
        sender.map(|s| str_field(s, "name")).unwrap_or_default(),
        sender
            .map(|s| str_field(s, "displayName"))
            .unwrap_or_default(),
        str_field(r, "sentAt"),
    )
}

fn user_id_of(v: &Value) -> Option<u64> {
    v.get("userId").and_then(Value::as_u64)
}

fn presence_type(v: &Value) -> Option<u64> {
    v.get("userPresenceType").and_then(Value::as_u64)
}

fn is_online(presence: Option<&Value>) -> bool {
    presence.is_some_and(|p| presence_type(p) != Some(PRESENCE_TYPE_OFFLINE))
}

const PRESENCE_DIFF_FIELDS: [&str; 7] = [
    "userPresenceType",
    "placeId",
    "universeId",
    "rootPlaceId",
    "gameId",
    "lastLocation",
    "lastOnline",
];

/// `diffPresence`: changed records plus users who just went offline.
fn diff_presence(
    previous: &HashMap<u64, Value>,
    incoming: &[Value],
) -> (Vec<Value>, BTreeMap<u64, Value>) {
    let mut changes = Vec::new();
    let mut newly_offline = BTreeMap::new();
    for presence in incoming {
        let Some(user_id) = user_id_of(presence) else {
            continue;
        };
        let Some(prior) = previous.get(&user_id) else {
            changes.push(json!({ "userId": user_id, "presence": presence }));
            continue;
        };
        if PRESENCE_DIFF_FIELDS
            .iter()
            .any(|k| prior.get(k) != presence.get(k))
        {
            changes.push(json!({ "userId": user_id, "presence": presence }));
        }
        let went_offline = presence_type(prior) != Some(PRESENCE_TYPE_OFFLINE)
            && presence_type(presence) == Some(PRESENCE_TYPE_OFFLINE);
        if let (true, Some(last_online)) = (
            went_offline,
            presence.get("lastOnline").filter(|v| truthy(v)),
        ) {
            newly_offline.insert(user_id, last_online.clone());
        }
    }
    (changes, newly_offline)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Bool(b) => *b,
        _ => true,
    }
}

/// `captureLastGames`: new in-game sessions keyed by user.
fn capture_last_games(
    incoming: &[Value],
    last_games: &BTreeMap<u64, Value>,
    now: &str,
) -> BTreeMap<u64, Value> {
    let mut updates = BTreeMap::new();
    for presence in incoming {
        let Some(user_id) = user_id_of(presence) else {
            continue;
        };
        let universe = presence.get("universeId").filter(|v| !v.is_null());
        let (Some(universe), Some(PRESENCE_TYPE_IN_GAME)) = (universe, presence_type(presence))
        else {
            continue;
        };
        if last_games.get(&user_id).and_then(|g| g.get("universeId")) == Some(universe) {
            continue;
        }
        let location = presence
            .get("lastLocation")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let name = if location.is_empty() {
            format!("Game {universe}")
        } else {
            location.to_owned()
        };
        let or_null = |key: &str| presence.get(key).cloned().unwrap_or(Value::Null);
        updates.insert(
            user_id,
            json!({
                "name": name,
                "placeId": or_null("placeId"),
                "playedAt": now,
                "rootPlaceId": or_null("rootPlaceId"),
                "universeId": universe,
            }),
        );
    }
    updates
}

impl SocialService {
    pub fn new(paths: &LauncherPaths) -> AppResult<Self> {
        Ok(Self {
            cache: SocialCache::open(paths)?,
            data: Mutex::new(SocialData::default()),
            friends_poller: Poller::new(
                "social-friends",
                FRIENDS_LADDER,
                Some(FRIENDS_BACKGROUND_LADDER),
            ),
            presence_poller: Poller::new(
                "social-presence",
                PRESENCE_LADDER,
                Some(PRESENCE_BACKGROUND_LADDER),
            ),
            started: AtomicBool::new(false),
        })
    }

    /// `start()`: hydrate from disk, start both pollers, follow account switches.
    pub fn start(state: &Arc<AppState>) {
        let social = &state.social;
        if social.started.swap(true, Ordering::AcqRel) {
            return;
        }
        social.hydrate(state.auth.current_user_id());
        social.start_pollers(state);

        let weak = Arc::downgrade(state);
        let mut accounts = state.auth.subscribe_account_changes();
        tauri::async_runtime::spawn(async move {
            while accounts.changed().await.is_ok() {
                let Some(state) = weak.upgrade() else { break };
                let social = &state.social;
                social.cache.flush();
                *social.data.lock() = SocialData::default();
                social.friends_poller.stop();
                social.presence_poller.stop();
                social.start_pollers(&state);
            }
        });
        tracing::info!("[SocialPoller] Started");
    }

    fn start_pollers(&self, state: &Arc<AppState>) {
        let weak = Arc::downgrade(state);
        self.friends_poller
            .start(Arc::new(FriendsJob(weak.clone())));
        self.presence_poller.start(Arc::new(PresenceJob(weak)));
    }

    pub fn stop(&self) {
        if !self.started.swap(false, Ordering::AcqRel) {
            return;
        }
        self.friends_poller.stop();
        self.presence_poller.stop();
        self.cache.flush();
        tracing::info!("[SocialPoller] Stopped");
    }

    pub fn set_backgrounded(&self, backgrounded: bool) {
        self.friends_poller.set_backgrounded(backgrounded);
        self.presence_poller.set_backgrounded(backgrounded);
    }

    /// Boosts the friends poller after a user-driven mutation.
    pub fn on_user_acted(&self) {
        self.friends_poller.trigger();
    }

    fn hydrate(&self, user_id: Option<u64>) {
        let Some(snapshot) = user_id.and_then(|id| self.cache.snapshot(id)) else {
            return;
        };
        let list = |key: &str| {
            snapshot
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let record = |key: &str| -> BTreeMap<u64, Value> {
            snapshot
                .get(key)
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut data = self.data.lock();
        data.friends = list("friends");
        data.friend_requests = list("friendRequests");
        data.avatars = record("avatars")
            .into_iter()
            .filter_map(|(k, v)| Some((k, v.as_str()?.to_owned())))
            .collect();
        data.last_seen = record("lastSeen");
        data.last_games = record("lastGames");
    }
}

// ------------------------------------------------------------------ jobs

struct FriendsJob(Weak<AppState>);
struct PresenceJob(Weak<AppState>);

impl PollJob for FriendsJob {
    fn should_run(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|s| s.auth.current_user_id().is_some())
    }

    async fn run(&self) -> AppResult<()> {
        let Some(state) = self.0.upgrade() else {
            return Ok(());
        };
        let Some(user_id) = state.auth.current_user_id() else {
            return Ok(());
        };
        let (friends, requests) = futures::join!(
            friends::list_friends(&state, user_id),
            friends::get_friend_requests(&state)
        );

        let missing: Vec<u64> = {
            let data = state.social.data.lock();
            let mut seen = HashSet::new();
            friends
                .iter()
                .filter_map(|f| f.get("id").and_then(Value::as_u64))
                .chain(requests.iter().filter_map(|r| {
                    r.get("senderId")
                        .and_then(Value::as_u64)
                        .or_else(|| r.pointer("/sender/id").and_then(Value::as_u64))
                }))
                .filter(|id| *id > 0 && seen.insert(*id) && !data.avatars.contains_key(id))
                .collect()
        };
        let mut avatars = BTreeMap::new();
        if !missing.is_empty() {
            match state
                .auth
                .headshots(&state.thumbnails, &missing, AVATAR_HEADSHOT_SIZE, true)
                .await
            {
                Ok(map) => avatars.extend(
                    map.into_iter()
                        .filter_map(|(k, v)| Some((k.parse().ok()?, v))),
                ),
                Err(error) => {
                    tracing::warn!(%error, "[SocialPoller] Failed to fetch avatar headshots")
                }
            }
        }
        handle_friends_result(&state, user_id, friends, requests, avatars);
        Ok(())
    }
}

fn handle_friends_result(
    state: &AppState,
    user_id: u64,
    friends: Vec<Value>,
    requests: Vec<Value>,
    avatars: BTreeMap<u64, String>,
) {
    if state.auth.current_user_id() != Some(user_id) {
        return;
    }
    let social = &state.social;
    let events: &EventBus = &state.events;
    let mut data = social.data.lock();

    let incoming: HashSet<u64> = friends
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_u64))
        .collect();
    let previous: Vec<u64> = data
        .friends
        .iter()
        .filter_map(|f| f.get("id").and_then(Value::as_u64))
        .collect();
    data.friends = friends;
    data.friend_requests = requests;

    let mut avatars_changed = false;
    for (id, url) in avatars {
        if data.avatars.get(&id) != Some(&url) {
            data.avatars.insert(id, url);
            avatars_changed = true;
        }
    }
    let mut presence_changed = false;
    for id in previous {
        if !incoming.contains(&id) && data.presence.remove(&id).is_some() {
            presence_changed = true;
        }
    }

    let friend_sigs: Vec<String> = data.friends.iter().map(friend_signature).collect();
    if data.last_broadcast_friends.as_ref() != Some(&friend_sigs) {
        data.last_broadcast_friends = Some(friend_sigs);
        events.emit("social:friendsChanged", json!({ "friends": data.friends }));
    }
    let request_sigs: Vec<String> = data.friend_requests.iter().map(request_signature).collect();
    if data.last_broadcast_requests.as_ref() != Some(&request_sigs) {
        data.last_broadcast_requests = Some(request_sigs);
        events.emit(
            "social:friendRequestsChanged",
            json!({ "requests": data.friend_requests }),
        );
    }
    if avatars_changed {
        events.emit("social:avatarsChanged", json!({ "avatars": data.avatars }));
    }
    social.cache.update(
        user_id,
        SnapshotPatch {
            friends: Some(&data.friends),
            friend_requests: Some(&data.friend_requests),
            avatars: avatars_changed.then_some(&data.avatars),
            ..SnapshotPatch::default()
        },
    );
    drop(data);
    if presence_changed {
        social.presence_poller.trigger();
    }
}

impl PollJob for PresenceJob {
    fn should_run(&self) -> bool {
        self.0.upgrade().is_some_and(|s| {
            s.auth.current_user_id().is_some() && !s.social.data.lock().friends.is_empty()
        })
    }

    async fn run(&self) -> AppResult<()> {
        let Some(state) = self.0.upgrade() else {
            return Ok(());
        };
        let ids = {
            let mut data = state.social.data.lock();
            if data.friends.is_empty() {
                return Ok(());
            }
            let all: Vec<u64> = data
                .friends
                .iter()
                .filter_map(|f| f.get("id").and_then(Value::as_u64))
                .collect();
            let include_offline = data.presence_tick % OFFLINE_TICK_RATIO == 0;
            data.presence_tick += 1;
            if include_offline {
                all
            } else {
                all.into_iter()
                    .filter(|id| is_online(data.presence.get(id)))
                    .collect()
            }
        };
        if ids.is_empty() {
            return Ok(());
        }
        let incoming = state.auth.presence(&ids).await?;
        handle_presence_result(&state, &incoming);
        Ok(())
    }
}

fn handle_presence_result(state: &AppState, incoming: &[Value]) {
    let Some(user_id) = state.auth.current_user_id() else {
        return;
    };
    let social = &state.social;
    let mut data = social.data.lock();
    let now = now_iso();
    let (changes, newly_offline) = diff_presence(&data.presence, incoming);
    let last_game_updates = capture_last_games(incoming, &data.last_games, &now);
    for presence in incoming {
        if let Some(id) = user_id_of(presence) {
            data.presence.insert(id, presence.clone());
        }
    }
    if !changes.is_empty() {
        state
            .events
            .emit("social:presenceChanged", json!({ "changes": changes }));
    }
    if !newly_offline.is_empty() {
        data.last_seen
            .extend(newly_offline.iter().map(|(k, v)| (*k, v.clone())));
        state.events.emit(
            "social:lastSeenChanged",
            json!({ "lastSeen": data.last_seen }),
        );
        social.cache.update(
            user_id,
            SnapshotPatch {
                last_seen: Some(&newly_offline),
                ..SnapshotPatch::default()
            },
        );
    }
    if !last_game_updates.is_empty() {
        data.last_games
            .extend(last_game_updates.iter().map(|(k, v)| (*k, v.clone())));
        state.events.emit(
            "social:lastGamesChanged",
            json!({ "lastGames": data.last_games }),
        );
        social.cache.update(
            user_id,
            SnapshotPatch {
                last_games: Some(&last_game_updates),
                ..SnapshotPatch::default()
            },
        );
    }
}

// ------------------------------------------------------------------ IPC

pub fn register(router: &mut Router) {
    router
        .handle("social:getCachedSnapshot", |ctx, _| async move {
            Ok(ctx
                .state
                .auth
                .current_user_id()
                .and_then(|id| ctx.state.social.cache.snapshot(id)))
        })
        .handle("socialPoller:userActed", |ctx, _| async move {
            ctx.state.social.on_user_acted();
            Ok(Value::Null)
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_diff_and_last_games() {
        let mut previous = HashMap::new();
        previous.insert(
            1,
            json!({ "userId": 1, "userPresenceType": 2, "universeId": 10, "lastOnline": "a" }),
        );
        let incoming = [
            json!({ "userId": 1, "userPresenceType": 0, "lastOnline": "b" }),
            json!({ "userId": 2, "userPresenceType": 2, "universeId": 20, "lastLocation": "  Obby " }),
        ];
        let (changes, offline) = diff_presence(&previous, &incoming);
        assert_eq!(changes.len(), 2);
        assert_eq!(offline.get(&1), Some(&json!("b")));

        let games = capture_last_games(&incoming, &BTreeMap::new(), "now");
        assert_eq!(games.len(), 1);
        assert_eq!(games[&2]["name"], "Obby");
        assert_eq!(games[&2]["placeId"], Value::Null);
    }
}
