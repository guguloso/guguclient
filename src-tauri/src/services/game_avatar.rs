//! Launch avatars (`gameAvatar:*`): an outfit or character picked per game or
//! per instance is worn for the launch and switched back when the game
//! closes. Port of `GameAvatarPreferencesService`, `GameAvatarService`,
//! `GameAvatarJournal`, `LaunchAvatarApplier`, `LaunchAvatarReverter` and
//! `LaunchAvatarNotifier`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use tauri::Manager;
use tokio::sync::oneshot;

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::avatar::AvatarTarget;
use crate::services::instances::{self, Instance};
use crate::services::roblox_layout::{read_json, write_json};
use crate::services::{app as app_service, outfit_wear, outfits};
use crate::state::AppState;
use crate::util::now_iso;

const LAUNCH_AVATAR_FLAG: &str = "feature-launch-avatar";
const ACTIVE_CHANGED_CHANNEL: &str = "gameAvatar:activeChanged";
const LAUNCH_AVATAR_TIMEOUT: Duration = Duration::from_millis(8000);
const RECOVERY_RETRY: Duration = Duration::from_secs(60);
const RECOVERY_MAX_FAILED_PASSES: u32 = 3;
const TOAST_DURATION_MS: u64 = 8000;
const BODY_COLOR3_FIELDS: [&str; 6] = ["headColor3", "leftArmColor3", "leftLegColor3", "rightArmColor3", "rightLegColor3", "torsoColor3"];

#[derive(Default)]
pub struct GameAvatarState {
    holds: Mutex<HashMap<u64, HashSet<u64>>>,
    queues: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    next_hold: AtomicU64,
    failed_passes: AtomicU32,
    last_active: Mutex<String>,
    stopped: AtomicBool,
    recovery_started: AtomicBool,
}

fn arc_of(state: &AppState) -> Option<Arc<AppState>> {
    state.app.try_state::<Arc<AppState>>().map(|s| s.inner().clone())
}

fn positive(v: Option<&Value>) -> Option<u64> {
    v.filter(|v| v.is_u64()).and_then(Value::as_u64).filter(|n| *n > 0)
}

// ------------------------------------------------------------ preferences

/// `gameAvatarChoiceSchema` (`none` allowed) / `launchAvatarSelectionSchema`.
fn parse_choice(v: &Value, allow_none: bool) -> Option<Value> {
    match v.get("kind").and_then(Value::as_str)? {
        "outfit" => {
            let id = v.get("outfitId").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            Some(json!({ "kind": "outfit", "outfitId": id }))
        }
        "character" => Some(json!({ "kind": "character", "robloxOutfitId": positive(v.get("robloxOutfitId"))? })),
        "none" if allow_none => Some(json!({ "kind": "none" })),
        _ => None,
    }
}

/// `storedUniversePreferenceSchema`: the record with its choices normalized.
fn parse_universe_record(v: &Value) -> Option<Value> {
    let map = v.as_object()?;
    positive(map.get("universeId"))?;
    map.get("updatedAt").filter(|u| u.is_string())?;
    let mut record = map.clone();
    if let Some(avatar) = map.get("avatar") {
        let avatar = avatar.as_object()?;
        let by_user = avatar.get("byUser")?.as_object()?;
        let mut parsed = Map::new();
        for (user, choice) in by_user {
            parsed.insert(user.clone(), parse_choice(choice, true)?);
        }
        let mut avatar = avatar.clone();
        avatar.insert("byUser".into(), Value::Object(parsed));
        record.insert("avatar".into(), Value::Object(avatar));
    }
    Some(Value::Object(record))
}

/// `readStore`: drops malformed records (rewriting the store if any).
fn read_store(state: &AppState) -> Map<String, Value> {
    let raw = state.game_preferences.read_universes();
    let valid: Map<String, Value> = raw.iter().filter_map(|(k, v)| Some((k.clone(), parse_universe_record(v)?))).collect();
    if valid.len() != raw.len() {
        tracing::warn!("Dropped malformed game avatar preference records");
        state.game_preferences.write_universes(&valid);
    }
    valid
}

fn selection_exists(state: &AppState, user: u64, selection: &Value) -> bool {
    selection.get("kind").and_then(Value::as_str) != Some("outfit")
        || selection.get("outfitId").and_then(Value::as_str).is_some_and(|id| outfits::find_for_user(state, user, id).is_some())
}

/// `writeChoice`: sets or (with `None`) removes one account's pick.
fn write_choice(state: &AppState, universe: u64, user: u64, choice: Option<Value>) {
    let mut store = read_store(state);
    let key = universe.to_string();
    let existing = store.get(&key).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut by_user = existing.get("avatar").and_then(|a| a.get("byUser")).and_then(Value::as_object).cloned().unwrap_or_default();
    match choice {
        Some(c) => {
            by_user.insert(user.to_string(), c);
        }
        None => {
            by_user.remove(&user.to_string());
        }
    }
    let mut next = existing.clone();
    next.insert("universeId".into(), json!(universe));
    next.insert("updatedAt".into(), json!(now_iso()));
    if by_user.is_empty() {
        next.remove("avatar");
    } else {
        let mut avatar = existing.get("avatar").and_then(Value::as_object).cloned().unwrap_or_default();
        avatar.insert("byUser".into(), Value::Object(by_user));
        next.insert("avatar".into(), Value::Object(avatar));
    }
    if next.iter().any(|(k, v)| k != "universeId" && k != "updatedAt" && !v.is_null()) {
        store.insert(key, Value::Object(next));
    } else {
        store.remove(&key);
    }
    state.game_preferences.write_universes(&store);
}

/// `getChoice`.
fn get_choice(state: &AppState, universe: u64, user: u64) -> Option<Value> {
    let store = read_store(state);
    let choice = store.get(&universe.to_string())?.get("avatar")?.get("byUser")?.get(user.to_string())?.clone();
    if choice["kind"] != "none" && !selection_exists(state, user, &choice) {
        write_choice(state, universe, user, None);
        tracing::info!("Removed stale game avatar for universeId={universe} userId={user}");
        return None;
    }
    Some(choice)
}

/// `setChoice`.
fn set_choice(state: &AppState, universe: Option<u64>, user: Option<u64>, choice: &Value) -> AppResult<Value> {
    let (Some(universe), Some(user)) = (universe, user) else {
        return Err(AppError::msg(format!(
            "Invalid game avatar key universeId={} userId={}",
            universe.map_or_else(|| "undefined".into(), |u| u.to_string()),
            user.map_or_else(|| "undefined".into(), |u| u.to_string())
        )));
    };
    let parsed = parse_choice(choice, true).ok_or_else(|| AppError::msg("Invalid game avatar choice"))?;
    if !state.auth.has_account(user) {
        return Err(AppError::msg(format!("Account not found: {user}")));
    }
    if parsed["kind"] != "none" && !selection_exists(state, user, &parsed) {
        return Err(AppError::msg(format!("Outfit not found for account {user}")));
    }
    write_choice(state, universe, user, Some(parsed.clone()));
    tracing::info!("Saved game avatar for universeId={universe} userId={user} -> {}", parsed["kind"].as_str().unwrap_or(""));
    state.track("launch.avatar.choice", json!({ "kind": parsed["kind"], "scope": "game" }));
    Ok(parsed)
}

/// `clearChoice`.
fn clear_choice(state: &AppState, universe: u64, user: u64) -> bool {
    let store = read_store(state);
    if store.get(&universe.to_string()).and_then(|r| r.get("avatar")).and_then(|a| a.get("byUser")).and_then(|b| b.get(user.to_string())).is_none() {
        return false;
    }
    write_choice(state, universe, user, None);
    tracing::info!("Cleared game avatar for universeId={universe} userId={user}");
    state.track("launch.avatar.choice", json!({ "kind": "cleared", "scope": "game" }));
    true
}

/// `readInstanceAvatar`: the instance's pick, dropped when it is gone.
fn read_instance_avatar(state: &AppState, instance: &Instance) -> Option<(u64, Value)> {
    let avatar = instance.get("launchAvatar").filter(|a| a.is_object())?;
    let user = positive(avatar.get("userId"))?;
    let selection = parse_choice(avatar.get("selection")?, false)?;
    if !selection_exists(state, user, &selection) {
        let id = instance.get("id").and_then(Value::as_str).unwrap_or("");
        let mut updates = Map::new();
        updates.insert("launchAvatar".into(), Value::Null);
        instances::update(state, id, updates);
        tracing::info!("Removed stale launch avatar from instance {id}");
        return None;
    }
    Some((user, selection))
}

/// `resolve`: which pick applies to a launch and where it came from.
fn resolve(state: &AppState, instance: Option<&Instance>, universe: Option<u64>) -> Value {
    let nothing = |user: Option<u64>| json!({ "gameChoice": null, "selection": null, "source": "none", "userId": user });
    if instance.is_some_and(|i| i.get("product").and_then(Value::as_str) != Some("player")) {
        return nothing(None);
    }
    let user = instance.and_then(|i| positive(i.get("accountUserId"))).or_else(|| state.auth.current_user_id());
    let Some(user) = user.filter(|u| state.auth.has_account(*u)) else { return nothing(None) };
    let game_choice = universe.and_then(|u| get_choice(state, u, user));
    if let Some(choice) = game_choice {
        if choice["kind"] == "none" {
            return json!({ "gameChoice": choice, "selection": null, "source": "none", "userId": user });
        }
        return json!({ "gameChoice": choice, "selection": choice, "source": "game", "userId": user });
    }
    if let Some((owner, selection)) = instance.and_then(|i| read_instance_avatar(state, i)) {
        if owner == user {
            return json!({ "gameChoice": null, "selection": selection, "source": "instance", "userId": user });
        }
    }
    nothing(Some(user))
}

// ---------------------------------------------------------------- journal

fn journal_dir(state: &AppState) -> PathBuf {
    state.paths.base.join("game-avatar")
}

fn journal_path(state: &AppState, user: u64) -> PathBuf {
    journal_dir(state).join(format!("{user}.json"))
}

fn valid_entry(v: &Value, user: u64) -> bool {
    let colors_ok = match v.pointer("/snapshot/bodyColor3s") {
        Some(Value::Null) => true,
        Some(c) => BODY_COLOR3_FIELDS.iter().all(|f| c.get(*f).is_some_and(Value::is_string)),
        None => false,
    };
    v.get("applied").is_some_and(|a| a.is_null() || a.is_string())
        && v.get("createdAt").is_some_and(Value::is_string)
        && v.pointer("/snapshot/avatar").is_some_and(Value::is_object)
        && colors_ok
        && positive(v.get("userId")) == Some(user)
}

fn journal_list(state: &AppState) -> Vec<u64> {
    std::fs::read_dir(journal_dir(state))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json")?.parse::<u64>().ok().filter(|id| *id > 0))
        .collect()
}

fn journal_read(state: &AppState, user: u64) -> Option<Value> {
    let path = journal_path(state, user);
    if !path.exists() {
        return None;
    }
    match read_json(&path) {
        Some(entry) if valid_entry(&entry, user) => Some(entry),
        _ => {
            tracing::warn!("Dropped an unreadable avatar revert record for account {user}");
            journal_delete(state, user);
            None
        }
    }
}

fn journal_write(state: &AppState, entry: &Value) {
    let Some(user) = positive(entry.get("userId")) else { return };
    let _ = std::fs::create_dir_all(journal_dir(state));
    if let Err(error) = write_json(&journal_path(state, user), entry) {
        tracing::warn!("Failed to write the avatar revert record for account {user}: {error}");
    }
}

fn journal_delete(state: &AppState, user: u64) {
    let path = journal_path(state, user);
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

// ------------------------------------------------------------- snapshots

async fn read_snapshot(state: &AppState, target: &AvatarTarget) -> AppResult<Value> {
    let (avatar, colors) = futures::join!(target.get_avatar(state), target.get_body_color3s(state));
    Ok(json!({ "avatar": avatar?, "bodyColor3s": colors? }))
}

/// `sortKeys`, with integral floats written the way JSON.stringify does.
fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), sort_keys(&o[k]))).collect())
        }
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9.0e15 => json!(f as i64),
            _ => v.clone(),
        },
        _ => v.clone(),
    }
}

/// `avatarFingerprint`.
fn fingerprint(snapshot: &Value) -> String {
    let avatar = &snapshot["avatar"];
    let colors = match snapshot.get("bodyColor3s").and_then(Value::as_object) {
        Some(c) => Value::Object(c.iter().map(|(k, v)| (k.clone(), json!(v.as_str().unwrap_or("").trim_start_matches('#').to_uppercase()))).collect()),
        None => avatar.get("bodyColors").cloned().unwrap_or(Value::Null),
    };
    let mut assets: Vec<Value> = avatar
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| {
            let mut out = json!({ "id": a.get("id") });
            if let Some(meta) = a.get("meta") {
                out["meta"] = meta.clone();
            }
            out
        })
        .collect();
    assets.sort_by(|a, b| a["id"].as_f64().unwrap_or(0.0).total_cmp(&b["id"].as_f64().unwrap_or(0.0)));
    let mut model = json!({ "assets": assets, "colors": colors });
    if let Some(rig) = avatar.get("playerAvatarType") {
        model["rig"] = rig.clone();
    }
    if let Some(scales) = avatar.get("scales") {
        model["scales"] = scales.clone();
    }
    sort_keys(&model).to_string()
}

fn describe_failure(result: &Value) -> String {
    let error = result["error"].as_str().unwrap_or("");
    match result.get("failedStage").and_then(Value::as_str) {
        Some(stage) => format!("{error}/{stage}"),
        None => error.to_owned(),
    }
}

// ---------------------------------------------------------------- notifier

fn toast(state: &AppState, title: &str, body: &str, outcome: &str) {
    app_service::show_toast(
        state,
        &json!({
            "analytics": { "notificationType": "launch_avatar", "payload": { "outcome": outcome } },
            "body": body, "duration": TOAST_DURATION_MS, "isDismissable": true, "title": title,
        }),
    );
}

fn notify_applied(state: &AppState, report: &Value) {
    state.track("launch.avatar.apply", report.clone());
    match report["outcome"].as_str() {
        Some("missing") => toast(
            state,
            "Launch outfit removed",
            "The outfit picked for this launch no longer exists, so it was removed. Your avatar was left as it was.",
            "missing",
        ),
        Some("failed") => toast(state, "Couldn't put on your launch outfit", "Your avatar was left as it was for this launch.", "failed"),
        _ => {}
    }
}

fn notify_reverted(state: &AppState, outcome: &str, recovered: bool) {
    state.track("launch.avatar.revert", json!({ "outcome": outcome, "recovered": recovered }));
    match outcome {
        "kept_change" => toast(
            state,
            "Kept your avatar changes",
            "You changed your avatar while playing, so Gugu Client for Roblox kept your changes instead of switching it back.",
            outcome,
        ),
        "failed" => toast(state, "Couldn't switch your avatar back", "Gugu Client for Roblox will try again the next time it starts.", outcome),
        "restored" if recovered => {
            toast(state, "Switched your avatar back", "A game launched last time had changed your avatar. It's been switched back.", outcome)
        }
        _ => {}
    }
}

// ----------------------------------------------------------------- applier

/// `wearSelection`: `applied`, `missing` or `failed`.
async fn wear_selection(state: &AppState, selection: &Value, user: u64) -> &'static str {
    if selection["kind"] == "outfit" {
        let id = selection["outfitId"].as_str().unwrap_or("");
        let result = outfit_wear::wear_for_account(state, id, user).await;
        if result["ok"] == true {
            return "applied";
        }
        tracing::warn!("Launch outfit {id} for account {user} was not worn: {}", describe_failure(&result));
        return if result["error"] == "outfitMissing" { "missing" } else { "failed" };
    }
    let id = selection["robloxOutfitId"].as_u64().unwrap_or(0);
    let result = outfit_wear::wear_character_for_account(state, id, user).await;
    if result["ok"] == true {
        return "applied";
    }
    tracing::warn!("Launch character {id} for account {user} was not worn: {}", describe_failure(&result));
    if result["error"] == "characterMissing" {
        "missing"
    } else {
        "failed"
    }
}

/// `LaunchAvatarApplier.apply`: the report to announce, if a pick applied.
async fn apply(state: &AppState, instance: &Instance, place_id: Option<u64>, user: u64) -> Option<Value> {
    let universe = match place_id {
        Some(place) => crate::services::games::universe_by_place(state, place).await,
        None => None,
    };
    let effective = resolve(state, Some(instance), universe);
    let selection = effective.get("selection").filter(|s| s.is_object())?.clone();
    if effective["userId"].as_u64() != Some(user) {
        return None;
    }
    let source = effective["source"].as_str().unwrap_or("none").to_owned();
    let report = |outcome: &str| {
        json!({
            "outcome": outcome,
            "pinned_account": instance.get("accountUserId").is_some_and(Value::is_number),
            "selection_kind": selection["kind"],
            "source": if source == "game" { "game" } else { "instance" },
        })
    };
    let Some(target) = AvatarTarget::for_account(state, user) else {
        tracing::warn!("No usable session for account {user}; launching with its current avatar");
        return Some(report("failed"));
    };
    let pending = journal_read(state, user);
    let entry = match &pending {
        Some(e) => e.clone(),
        None => {
            let snapshot = match read_snapshot(state, &target).await {
                Ok(s) => s,
                Err(error) => {
                    tracing::warn!("Could not snapshot the avatar of {user}; launching with it unchanged: {error}");
                    return Some(report("failed"));
                }
            };
            let entry = json!({ "applied": null, "createdAt": now_iso(), "snapshot": snapshot, "userId": user });
            journal_write(state, &entry);
            entry
        }
    };
    let outcome = wear_selection(state, &selection, user).await;
    if outcome == "missing" {
        if source == "game" {
            if let Some(universe) = universe {
                clear_choice(state, universe, user);
            }
        } else if source == "instance" {
            let mut updates = Map::new();
            updates.insert("launchAvatar".into(), Value::Null);
            instances::update(state, instance.get("id").and_then(Value::as_str).unwrap_or(""), updates);
        }
        tracing::info!("Removed a {source} launch avatar for account {user} that no longer exists");
    }
    let current = match read_snapshot(state, &target).await {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!("Could not read the avatar of {user} after the launch change: {error}");
            return Some(report(outcome));
        }
    };
    let applied = fingerprint(&current);
    if pending.is_none() && applied == fingerprint(&entry["snapshot"]) {
        journal_delete(state, user);
        return Some(report(outcome));
    }
    let mut entry = entry;
    entry["applied"] = json!(applied);
    journal_write(state, &entry);
    tracing::info!("Launch avatar for account {user}: {} from the {source} ({outcome})", selection["kind"].as_str().unwrap_or(""));
    Some(report(outcome))
}

// ---------------------------------------------------------------- reverter

fn is_held(state: &AppState, user: u64) -> bool {
    state.game_avatar.holds.lock().get(&user).is_some_and(|h| !h.is_empty())
}

/// `revertIfIdle`: puts the snapshot back once no launch holds the account.
async fn revert_if_idle(state: &AppState, user: u64) -> Option<&'static str> {
    if is_held(state, user) {
        return None;
    }
    let entry = journal_read(state, user)?;
    if !state.auth.has_account(user) {
        journal_delete(state, user);
        tracing::warn!("Account {user} was removed; its avatar cannot be put back");
        return Some("account_removed");
    }
    let Some(target) = AvatarTarget::for_account(state, user) else {
        tracing::warn!("No usable session for account {user}; its avatar is put back on a later start");
        return Some("failed");
    };
    let current = match read_snapshot(state, &target).await {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!("Could not read the avatar of {user} to put it back: {error}");
            return Some("failed");
        }
    };
    let now = fingerprint(&current);
    if let Some(applied) = entry["applied"].as_str() {
        if now != applied {
            journal_delete(state, user);
            tracing::info!("Account {user} changed its avatar during the session; keeping it");
            return Some("kept_change");
        }
    }
    if now == fingerprint(&entry["snapshot"]) {
        journal_delete(state, user);
        return Some("restored");
    }
    let result = outfit_wear::restore_for_account(state, &entry["snapshot"], user).await;
    if result["ok"] == true {
        journal_delete(state, user);
        tracing::info!("Put back the avatar of account {user}");
        return Some("restored");
    }
    tracing::warn!("Putting back the avatar of account {user} failed ({}); retrying on a later start", describe_failure(&result));
    Some("failed")
}

async fn revert_and_report(state: &AppState, user: u64, recovered: bool) {
    let outcome = revert_if_idle(state, user).await;
    let passes = state.game_avatar.failed_passes.load(Ordering::SeqCst);
    let will_retry = recovered && outcome == Some("failed") && passes + 1 < RECOVERY_MAX_FAILED_PASSES;
    if let Some(outcome) = outcome.filter(|_| !will_retry) {
        notify_reverted(state, outcome, recovered);
    }
}

// ----------------------------------------------------------------- service

/// `listActive`: accounts currently wearing a launch avatar.
fn list_active(state: &AppState) -> Vec<u64> {
    let mut held: Vec<u64> = state.game_avatar.holds.lock().keys().copied().collect();
    held.sort_unstable();
    held.into_iter().filter(|u| journal_read(state, *u).is_some()).collect()
}

fn publish_active(state: &AppState) {
    let active = list_active(state);
    let key = active.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    {
        let mut last = state.game_avatar.last_active.lock();
        if *last == key {
            return;
        }
        *last = key;
    }
    state.events.emit(ACTIVE_CHANGED_CHANNEL, json!(active));
}

/// `enqueue`: runs `task` after the account's earlier tasks.
fn enqueue<F, Fut>(state: &Arc<AppState>, user: u64, task: F) -> oneshot::Receiver<()>
where
    F: FnOnce(Arc<AppState>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let lock = state.game_avatar.queues.lock().entry(user).or_default().clone();
    let state = Arc::clone(state);
    let (tx, rx) = oneshot::channel();
    tauri::async_runtime::spawn(async move {
        let _turn = lock.lock().await;
        task(Arc::clone(&state)).await;
        publish_active(&state);
        let _ = tx.send(());
    });
    rx
}

/// A launch's claim on its account's avatar (`prepareForLaunch`).
pub struct LaunchAvatar {
    user: u64,
    hold: u64,
    released: AtomicBool,
    applying: Mutex<Option<oneshot::Receiver<()>>>,
}

impl LaunchAvatar {
    /// `waitForApply`: waits up to 8s for the pick to be worn.
    pub async fn ready(&self) {
        let Some(applying) = self.applying.lock().take() else { return };
        if tokio::time::timeout(LAUNCH_AVATAR_TIMEOUT, applying).await.is_err() {
            tracing::warn!("Launch avatar for account {} took over {}ms; launching without waiting", self.user, LAUNCH_AVATAR_TIMEOUT.as_millis());
        }
    }

    /// `release` / `releaseHold`: the last hold switches the avatar back.
    pub fn release(&self, state: &AppState) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut holds = state.game_avatar.holds.lock();
            if let Some(set) = holds.get_mut(&self.user) {
                set.remove(&self.hold);
                if !set.is_empty() {
                    return;
                }
            }
            holds.remove(&self.user);
        }
        let Some(state) = arc_of(state) else { return };
        let user = self.user;
        drop(enqueue(&state, user, move |state| async move { revert_and_report(&state, user, false).await }));
    }
}

/// `prepareForLaunch`: `None` when the launch keeps the current avatar.
pub fn prepare_for_launch(state: &Arc<AppState>, instance: &Instance, place_id: Option<u64>) -> Option<Arc<LaunchAvatar>> {
    if instance.get("product").and_then(Value::as_str) != Some("player") || !state.feature_flags.is_enabled(LAUNCH_AVATAR_FLAG) {
        return None;
    }
    let user = positive(instance.get("accountUserId")).or_else(|| state.auth.current_user_id())?;
    let hold = state.game_avatar.next_hold.fetch_add(1, Ordering::SeqCst);
    state.game_avatar.holds.lock().entry(user).or_default().insert(hold);
    let instance = instance.clone();
    let applying = enqueue(state, user, move |state| async move {
        if let Some(report) = apply(&state, &instance, place_id, user).await {
            notify_applied(&state, &report);
        }
    });
    Some(Arc::new(LaunchAvatar { user, hold, released: AtomicBool::new(false), applying: Mutex::new(Some(applying)) }))
}

/// `startRecovery`: puts back avatars a previous session left changed.
pub fn start_recovery(state: &Arc<AppState>) {
    if state.game_avatar.recovery_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        loop {
            match recover(&state).await {
                Some(delay) => tokio::time::sleep(delay).await,
                None => break,
            }
        }
    });
}

/// One recovery pass; `Some(delay)` schedules another.
async fn recover(state: &Arc<AppState>) -> Option<Duration> {
    if state.game_avatar.stopped.load(Ordering::SeqCst) {
        return None;
    }
    let pending: Vec<u64> = journal_list(state).into_iter().filter(|u| !is_held(state, *u)).collect();
    if pending.is_empty() {
        return None;
    }
    if crate::services::launch::is_any_roblox_running(state) {
        return Some(RECOVERY_RETRY);
    }
    let waits: Vec<_> = pending.into_iter().map(|user| enqueue(state, user, move |state| async move { revert_and_report(&state, user, true).await })).collect();
    futures::future::join_all(waits).await;
    let failed = journal_list(state).into_iter().any(|u| !is_held(state, u));
    if !failed || state.game_avatar.stopped.load(Ordering::SeqCst) {
        return None;
    }
    let passes = state.game_avatar.failed_passes.fetch_add(1, Ordering::SeqCst) + 1;
    (passes < RECOVERY_MAX_FAILED_PASSES).then_some(RECOVERY_RETRY)
}

/// `stop` (app quit).
pub fn stop(state: &AppState) {
    state.game_avatar.stopped.store(true, Ordering::SeqCst);
}

pub fn register(router: &mut Router) {
    router
        .handle("gameAvatar:get", |ctx, args| async move {
            let universe: u64 = args.get(0)?;
            let user: u64 = args.get(1)?;
            Ok(get_choice(&ctx.state, universe, user))
        })
        .handle("gameAvatar:set", |ctx, args| async move {
            let universe: Value = args.get(0)?;
            let user: Value = args.get(1)?;
            let choice: Value = args.opt(2)?.unwrap_or(Value::Null);
            set_choice(&ctx.state, positive(Some(&universe)), positive(Some(&user)), &choice)
        })
        .handle("gameAvatar:clear", |ctx, args| async move {
            let universe: u64 = args.get(0)?;
            let user: u64 = args.get(1)?;
            Ok(clear_choice(&ctx.state, universe, user))
        })
        .handle("gameAvatar:listActive", |ctx, _| async move { Ok(list_active(&ctx.state)) })
        .handle("gameAvatar:resolve", |ctx, args| async move {
            let universe: Option<u64> = args.opt(0)?;
            let instance_id: Option<String> = args.opt(1)?;
            let state = &ctx.state;
            let instance = instance_id.and_then(|id| state.instances.get(&state.paths.base, &id));
            Ok(resolve(state, instance.as_ref(), universe))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_js_shape() {
        let snapshot = json!({
            "avatar": { "assets": [{ "id": 5 }, { "id": 2, "meta": { "order": 1 } }], "playerAvatarType": 3, "scales": { "height": 1.0, "width": 0.95 } },
            "bodyColor3s": { "headColor3": "#ffcc00" },
        });
        assert_eq!(
            fingerprint(&snapshot),
            r#"{"assets":[{"id":2,"meta":{"order":1}},{"id":5}],"colors":{"headColor3":"FFCC00"},"rig":3,"scales":{"height":1,"width":0.95}}"#
        );
    }

    #[test]
    fn choices_are_validated() {
        assert!(parse_choice(&json!({ "kind": "none" }), false).is_none());
        assert_eq!(parse_choice(&json!({ "kind": "character", "robloxOutfitId": 7, "x": 1 }), true), Some(json!({ "kind": "character", "robloxOutfitId": 7 })));
        assert!(parse_choice(&json!({ "kind": "outfit", "outfitId": "" }), true).is_none());
    }
}
