//! Accounts and session (`auth:*`). Port of `AuthService` + `AuthController`.
//!
//! Reads/writes the same `~/.lunar-for-roblox/auth.json` and encrypts cookies
//! with the same safeStorage key, so the Electron and Rust builds share logins.
//!
//! Pending: `auth:login` (embedded roblox.com login window with captcha),
//! rotated-cookie persistence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::watch;

use crate::crypto::SafeStorage;
use crate::error::{AppError, AppResult};
use crate::ipc::{EventBus, Router};
use crate::roblox::endpoints::{self, join_ids, unique_positive};
use crate::roblox::RobloxClient;
use crate::services::thumbnails::{IdQuery, ThumbPage};

const ACTIVE_ACCOUNT_CHANGED: &str = "auth:activeAccountChanged";
const FAVORITE_GAMES_TTL: Duration = Duration::from_secs(10 * 60);
const PRESENCE_BATCH: usize = 100;
const HEADSHOT_BATCH: usize = 100;
const BROWSER_TRACKER_FIELD: &str = "browserTrackerId";

/// `RBXEventTrackerV2=CreateDate=...&rbxid=...&browserid=<id>` → `<id>`.
fn browser_tracker_from_set_cookie(header: &str) -> Option<String> {
    let pair = header.split(';').next()?;
    let (name, value) = pair.split_once('=')?;
    if name.trim() != "RBXEventTrackerV2" {
        return None;
    }
    let raw = value
        .trim()
        .split('&')
        .find_map(|part| {
            part.split_once('=')
                .filter(|(k, _)| k.eq_ignore_ascii_case("browserid"))
                .map(|(_, v)| v)
        })
        .filter(|v| !v.is_empty())?;
    Some(
        urlencoding::decode(raw)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| raw.to_owned()),
    )
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthFile {
    #[serde(default)]
    accounts: Vec<StoredAccount>,
    #[serde(default)]
    active_user_id: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccount {
    user_id: u64,
    username: String,
    display_name: String,
    /// safeStorage ciphertext (base64), never plaintext on disk.
    cookie: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_invalid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_above13: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_premium: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    has_roblox_subscription: Option<bool>,
    /// Unknown fields (e.g. `browserTrackerId`) survive a round-trip.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Shape returned by `auth:getAccounts` (no cookie, ever).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountSummary<'a> {
    user_id: u64,
    username: &'a str,
    display_name: &'a str,
    user_above13: Option<bool>,
    is_premium: Option<bool>,
    has_roblox_subscription: Option<bool>,
    session_invalid: Option<bool>,
}

/// `{ id, name, displayName, ... }` from `users/authenticated`.
struct ValidatedUser {
    user: Map<String, Value>,
}

impl ValidatedUser {
    fn id(&self) -> u64 {
        self.user.get("id").and_then(Value::as_u64).unwrap_or(0)
    }
    fn str_field(&self, key: &str) -> String {
        self.user
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteGame {
    id: u64,
    name: String,
    place_id: u64,
    root_place_id: u64,
    description: String,
}

pub struct AuthService {
    file: PathBuf,
    data: RwLock<AuthFile>,
    crypto: SafeStorage,
    roblox: Arc<RobloxClient>,
    events: EventBus,
    favorites: Mutex<HashMap<u64, (Instant, Arc<[FavoriteGame]>)>>,
    /// `onActiveAccountChanged` subscribers (social poller, metadata, ...).
    account_tx: watch::Sender<Option<u64>>,
}

impl AuthService {
    pub fn load(
        file: PathBuf,
        crypto: SafeStorage,
        roblox: Arc<RobloxClient>,
        events: EventBus,
    ) -> Self {
        let data = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<AuthFile>(&bytes).ok())
            .unwrap_or_default();
        tracing::info!(accounts = data.accounts.len(), "auth data loaded");
        let (account_tx, _) = watch::channel(data.active_user_id);
        Self {
            file,
            data: RwLock::new(data),
            crypto,
            roblox,
            events,
            favorites: Mutex::new(HashMap::new()),
            account_tx,
        }
    }

    // ------------------------------------------------------------- state

    pub fn current_user_id(&self) -> Option<u64> {
        self.data.read().active_user_id
    }

    pub fn is_authenticated(&self) -> bool {
        let data = self.data.read();
        data.active_user_id
            .is_some_and(|id| data.accounts.iter().any(|a| a.user_id == id))
    }

    /// `onActiveAccountChanged`: fires on every activation, logout included.
    pub fn subscribe_account_changes(&self) -> watch::Receiver<Option<u64>> {
        self.account_tx.subscribe()
    }

    /// `getStoredAccountFlags(userId).userAbove13 === true`.
    pub fn is_user_above13(&self, user_id: u64) -> bool {
        let data = self.data.read();
        data.accounts
            .iter()
            .any(|a| a.user_id == user_id && a.user_above13 == Some(true))
    }

    /// `updateStoredAccountFlags`: merges age/premium flags, writing and
    /// notifying (`auth:accountsChanged`) only when something changed.
    pub fn update_stored_account_flags(
        &self,
        user_id: u64,
        user_above13: Option<bool>,
        is_premium: Option<bool>,
        has_roblox_subscription: Option<bool>,
    ) {
        let changed = {
            let data = self.data.read();
            let Some(account) = data.accounts.iter().find(|a| a.user_id == user_id) else {
                return;
            };
            user_above13.or(account.user_above13) != account.user_above13
                || is_premium.or(account.is_premium) != account.is_premium
                || has_roblox_subscription.or(account.has_roblox_subscription)
                    != account.has_roblox_subscription
        };
        if !changed {
            return;
        }
        let result = self.save(|data| {
            if let Some(account) = data.accounts.iter_mut().find(|a| a.user_id == user_id) {
                account.user_above13 = user_above13.or(account.user_above13);
                account.is_premium = is_premium.or(account.is_premium);
                account.has_roblox_subscription =
                    has_roblox_subscription.or(account.has_roblox_subscription);
            }
        });
        match result {
            Ok(()) => self.events.emit_main("auth:accountsChanged", Value::Null),
            Err(error) => tracing::warn!(%error, "Auth: failed to persist account flags"),
        }
    }

    /// `fetchApiWithWebContext` credentials: the active cookie and its
    /// browser tracker id, provisioned from `roblox.com/home` when missing.
    pub async fn web_context(&self) -> Option<(String, String)> {
        let user_id = self.current_user_id()?;
        let cookie = self.cookie_for(user_id)?;
        let stored = {
            let data = self.data.read();
            data.accounts
                .iter()
                .find(|a| a.user_id == user_id)
                .and_then(|a| a.extra.get(BROWSER_TRACKER_FIELD))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if let Some(tracker) = stored.and_then(|encrypted| self.decrypt(&encrypted)) {
            return Some((cookie, tracker));
        }
        match self.fetch_browser_tracker_id(&cookie).await {
            Ok(Some(tracker)) => {
                self.persist_browser_tracker_id(user_id, &tracker);
                Some((cookie, tracker))
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(user_id, %error, "Auth: Failed to fetch browser tracker ID");
                None
            }
        }
    }

    async fn fetch_browser_tracker_id(&self, cookie: &str) -> AppResult<Option<String>> {
        let url = format!("{}/home", endpoints::WEB);
        let response = self.roblox.get_as(&url, cookie).await?;
        Ok(response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(browser_tracker_from_set_cookie))
    }

    fn persist_browser_tracker_id(&self, user_id: u64, tracker: &str) {
        let encrypted = match self.crypto.encrypt_base64(tracker) {
            Ok(encrypted) => encrypted,
            Err(error) => {
                tracing::warn!(%error, "Auth: failed to encrypt browser tracker ID");
                return;
            }
        };
        let result = self.save(|data| {
            if let Some(account) = data.accounts.iter_mut().find(|a| a.user_id == user_id) {
                account
                    .extra
                    .insert(BROWSER_TRACKER_FIELD.to_owned(), Value::String(encrypted));
            }
        });
        match result {
            Ok(()) => tracing::info!(user_id, "Auth: Stored browser tracker ID"),
            Err(error) => tracing::warn!(%error, "Auth: failed to persist browser tracker ID"),
        }
    }

    fn accounts_json(&self) -> AppResult<Value> {
        let data = self.data.read();
        let list: Vec<AccountSummary<'_>> = data
            .accounts
            .iter()
            .map(|a| AccountSummary {
                user_id: a.user_id,
                username: &a.username,
                display_name: &a.display_name,
                user_above13: a.user_above13,
                is_premium: a.is_premium,
                has_roblox_subscription: a.has_roblox_subscription,
                session_invalid: a.session_invalid,
            })
            .collect();
        Ok(serde_json::to_value(list)?)
    }

    fn decrypt(&self, encrypted: &str) -> Option<String> {
        match self.crypto.decrypt_base64(encrypted) {
            Ok(cookie) if !cookie.is_empty() => Some(cookie),
            Ok(_) => None,
            Err(error) => {
                tracing::error!(%error, "Auth: failed to decrypt cookie");
                None
            }
        }
    }

    /// Decrypted cookie for `user_id` (owned so it outlives the lock).
    fn cookie_for(&self, user_id: u64) -> Option<String> {
        let encrypted = {
            let data = self.data.read();
            data.accounts
                .iter()
                .find(|a| a.user_id == user_id)?
                .cookie
                .clone()
        };
        self.decrypt(&encrypted)
    }

    /// `getStoredCookie` / `getStoredCookieForUser`.
    pub fn stored_cookie(&self, user_id: Option<u64>) -> Option<String> {
        self.cookie_for(user_id.or_else(|| self.current_user_id())?)
    }

    /// `getRequestHeadersForUser`: the stored account's cookie unless its
    /// session was marked invalid.
    pub fn request_cookie_for_user(&self, user_id: u64) -> Option<String> {
        let valid = self.data.read().accounts.iter().any(|a| a.user_id == user_id && a.session_invalid != Some(true));
        if valid { self.cookie_for(user_id) } else { None }
    }

    pub fn has_account(&self, user_id: u64) -> bool {
        self.data
            .read()
            .accounts
            .iter()
            .any(|a| a.user_id == user_id)
    }

    /// `persistCookieForUser`: stores a rotated `.ROBLOSECURITY`.
    fn persist_cookie(&self, user_id: u64, cookie: &str) {
        if self.cookie_for(user_id).as_deref() == Some(cookie) {
            return;
        }
        let Ok(encrypted) = self.crypto.encrypt_base64(cookie) else {
            return;
        };
        let result = self.save(|data| {
            if let Some(account) = data.accounts.iter_mut().find(|a| a.user_id == user_id) {
                account.cookie = encrypted;
            }
        });
        match result {
            Ok(()) => tracing::info!("Auth: Refreshed .ROBLOSECURITY cookie for user {user_id}"),
            Err(error) => tracing::warn!(%error, "Auth: failed to persist rotated cookie"),
        }
    }

    /// `invalidateAccountWithNotification`.
    fn invalidate_with_notification(&self, user_id: u64) {
        let (display_name, was_active) = {
            let data = self.data.read();
            let Some(account) = data.accounts.iter().find(|a| a.user_id == user_id) else {
                return;
            };
            if account.session_invalid == Some(true) {
                return;
            }
            (
                account.display_name.clone(),
                data.active_user_id == Some(user_id),
            )
        };
        if was_active {
            self.roblox.set_cookie(None);
        }
        let result = self.save(|data| {
            mark_invalid(data, user_id, true);
            if was_active {
                data.active_user_id = None;
            }
        });
        if let Err(error) = result {
            tracing::warn!(%error, "Auth: failed to mark session invalid");
            return;
        }
        self.events.emit_main(
            "auth:invalidated",
            json!({ "userId": user_id, "displayName": display_name }),
        );
        if was_active {
            self.notify_active_account_changed(None);
        }
    }

    /// `getAuthTicket`: one-time ticket the Roblox client logs in with.
    pub async fn get_auth_ticket(&self, user_id: Option<u64>) -> AppResult<String> {
        let target = user_id.or_else(|| self.current_user_id());
        let cookie = match user_id {
            Some(id) => self
                .cookie_for(id)
                .ok_or_else(|| AppError::msg(format!("Pinned account {id} was not found")))?,
            None => self
                .stored_cookie(None)
                .ok_or_else(|| AppError::msg("Not authenticated - cannot generate auth ticket"))?,
        };
        let url = format!("{}/authentication-ticket/", endpoints::AUTH_V1);
        let response = self.roblox.post_web_as(&url, &cookie).await?;
        let rotated = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                v.split(';')
                    .next()?
                    .trim()
                    .strip_prefix(".ROBLOSECURITY=")
                    .map(str::to_owned)
            })
            .filter(|c| !c.is_empty() && *c != cookie);
        if let (Some(target), Some(rotated)) = (target, rotated) {
            if self.has_account(target) {
                self.persist_cookie(target, &rotated);
                if self.current_user_id() == Some(target) {
                    self.roblox.set_cookie(Some(&rotated));
                }
            }
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::error!(status = status.as_u16(), body = %body.chars().take(300).collect::<String>(), "Auth ticket request failed!");
            if status.as_u16() == 401 {
                if let Some(id) = user_id {
                    self.invalidate_with_notification(id);
                }
                return Err(AppError::msg("Authentication expired - please login again"));
            }
            return Err(AppError::msg(format!(
                "Failed to generate auth ticket: {status}"
            )));
        }
        let ticket = response
            .headers()
            .get("rbx-authentication-ticket")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                tracing::error!("No authentication ticket in response!");
                AppError::msg("No authentication ticket in response - Roblox API may have changed")
            })?;
        tracing::debug!("Successfully generated authentication ticket!");
        Ok(ticket)
    }

    /// Applies `f` to the auth data and writes `auth.json` once.
    fn save(&self, f: impl FnOnce(&mut AuthFile)) -> AppResult<()> {
        let bytes = {
            let mut data = self.data.write();
            f(&mut data);
            serde_json::to_vec_pretty(&*data)?
        };
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    fn notify_active_account_changed(&self, user_id: Option<u64>) {
        self.favorites.lock().clear();
        self.events
            .emit(ACTIVE_ACCOUNT_CHANGED, json!({ "userId": user_id }));
        self.account_tx.send_replace(user_id);
    }

    // ------------------------------------------------------------- session

    /// `validateCookie`: `Ok(None)` when Roblox rejects the cookie (401/403),
    /// `Err` on transient failures (network, 5xx).
    async fn validate_cookie(&self, cookie: &str) -> AppResult<Option<ValidatedUser>> {
        let url = format!("{}/users/authenticated", endpoints::USERS_V1);
        let response = self.roblox.get_as(&url, cookie).await?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Ok(None);
        }
        let mut user: Map<String, Value> = RobloxClient::decode(&url, response).await?;
        if !(user.contains_key("id") && user.contains_key("name")) {
            return Ok(None);
        }
        let id = user.get("id").and_then(Value::as_u64).unwrap_or(0);
        user.extend(self.membership(id, Some(cookie)).await);
        Ok(Some(ValidatedUser { user }))
    }

    /// `getAuthenticatedUser`.
    pub async fn authenticated_user(&self) -> AppResult<Option<Value>> {
        let Some(user_id) = self.current_user_id() else {
            return Ok(None);
        };
        let Some(cookie) = self.cookie_for(user_id) else {
            return Ok(None);
        };
        match self.validate_cookie(&cookie).await {
            Ok(Some(validated)) => {
                self.roblox.set_cookie(Some(&cookie));
                Ok(Some(Value::Object(validated.user)))
            }
            Ok(None) => {
                tracing::warn!(user_id, "Auth: cookie rejected");
                self.save(|data| mark_invalid(data, user_id, true))?;
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(user_id, %error, "Auth: transient validation failure");
                Ok(None)
            }
        }
    }

    async fn set_cookie(&self, raw: &str) -> AppResult<Value> {
        let cookie = normalize_roblosecurity(raw);
        let validated = self.validate_cookie(&cookie).await.map_err(|_| {
            AppError::msg("Could not reach Roblox to validate this cookie. Check your connection and try again.")
        })?;
        let validated = validated.ok_or_else(|| AppError::msg("Invalid cookie"))?;
        let user_id = validated.id();
        let encrypted = self.crypto.encrypt_base64(&cookie)?;
        let account = StoredAccount {
            user_id,
            username: validated.str_field("name"),
            display_name: validated.str_field("displayName"),
            cookie: encrypted,
            session_invalid: None,
            user_above13: None,
            is_premium: None,
            has_roblox_subscription: None,
            extra: Map::new(),
        };
        self.save(|data| {
            data.accounts.retain(|a| a.user_id != user_id);
            data.accounts.push(account);
            data.active_user_id = Some(user_id);
        })?;
        self.roblox.set_cookie(Some(&cookie));
        self.notify_active_account_changed(Some(user_id));
        Ok(Value::Object(validated.user))
    }

    fn logout(&self) -> AppResult<()> {
        if self.current_user_id().is_none() {
            return Ok(());
        }
        self.roblox.set_cookie(None);
        self.save(|data| data.active_user_id = None)?;
        self.notify_active_account_changed(None);
        Ok(())
    }

    /// `initialize()`: restores the stored session before any poller runs.
    pub async fn initialize(&self) {
        if let Some(user_id) = self.current_user_id() {
            match self.switch_account(user_id).await {
                Ok(()) => return,
                Err(error) => {
                    tracing::error!(user_id, %error, "Failed to restore session");
                    self.roblox.set_cookie(None);
                    if let Err(error) = self.save(|data| data.active_user_id = None) {
                        tracing::warn!(%error, "Auth: failed to persist session reset");
                    }
                }
            }
        }
        self.notify_active_account_changed(None);
    }

    async fn switch_account(&self, user_id: u64) -> AppResult<()> {
        let cookie = self
            .cookie_for(user_id)
            .ok_or_else(|| AppError::msg("Account not found"))?;
        let validated = self.validate_cookie(&cookie).await.map_err(|_| {
            AppError::msg("Could not reach Roblox to validate this account. Check your connection and try again.")
        })?;
        let Some(validated) = validated else {
            let was_active = self.current_user_id() == Some(user_id);
            self.save(|data| mark_invalid(data, user_id, true))?;
            if was_active {
                self.notify_active_account_changed(None);
            }
            return Err(AppError::msg("Session expired"));
        };
        self.save(|data| {
            mark_invalid(data, user_id, false);
            data.active_user_id = Some(user_id);
        })?;
        self.roblox.set_cookie(Some(&cookie));
        tracing::info!(user = %validated.str_field("name"), "Auth: switched account");
        self.notify_active_account_changed(Some(user_id));
        Ok(())
    }

    async fn remove_account(&self, user_id: u64) -> AppResult<()> {
        let (was_active, next) = {
            let data = self.data.read();
            let next = data
                .accounts
                .iter()
                .find(|a| a.user_id != user_id)
                .map(|a| a.user_id);
            (data.active_user_id == Some(user_id), next)
        };
        self.save(|data| data.accounts.retain(|a| a.user_id != user_id))?;
        if !was_active {
            self.notify_active_account_changed(self.current_user_id());
            return Ok(());
        }

        let mut active = None;
        if let Some(next_id) = next {
            let cookie = self.cookie_for(next_id);
            let validation = match &cookie {
                Some(cookie) => self.validate_cookie(cookie).await,
                None => Ok(None),
            };
            match validation {
                Ok(Some(_)) => {
                    self.roblox.set_cookie(cookie.as_deref());
                    self.save(|data| mark_invalid(data, next_id, false))?;
                    active = Some(next_id);
                }
                Ok(None) => self.save(|data| mark_invalid(data, next_id, true))?,
                Err(error) => {
                    tracing::warn!(%error, "Auth: transient validation failure after removal")
                }
            }
        }
        if active.is_none() {
            self.roblox.set_cookie(None);
        }
        self.save(|data| data.active_user_id = active)?;
        self.notify_active_account_changed(active);
        Ok(())
    }

    // ------------------------------------------------------------- lookups

    /// `getUserMembership` + `resolveMembership`; both lookups run concurrently.
    async fn membership(&self, user_id: u64, cookie: Option<&str>) -> Map<String, Value> {
        let member_url = format!(
            "{}/users/{user_id}/validate-membership",
            endpoints::PREMIUM_FEATURES_V1
        );
        let header_url = format!("{}/profiles/get", endpoints::PROFILE_PLATFORM_API);
        let header_body = json!({
            "profileId": user_id.to_string(),
            "profileType": "User",
            "components": [{ "component": "UserProfileHeader" }],
        });
        let (is_member, header) = futures::future::join(
            async {
                match cookie {
                    Some(c) => {
                        let response = self.roblox.get_as(&member_url, c).await.ok()?;
                        RobloxClient::decode::<bool>(&member_url, response)
                            .await
                            .ok()
                    }
                    None => self.roblox.get::<bool>(&member_url).await.ok(),
                }
            },
            async {
                let profile: Value = match cookie {
                    Some(c) => {
                        let response = self
                            .roblox
                            .post_as(&header_url, &header_body, c)
                            .await
                            .ok()?;
                        RobloxClient::decode(&header_url, response).await.ok()?
                    }
                    None => self.roblox.post(&header_url, &header_body).await.ok()?,
                };
                profile.pointer("/components/UserProfileHeader").cloned()
            },
        )
        .await;
        resolve_membership(header.as_ref(), is_member)
    }

    pub async fn user_details(&self, user_id: u64) -> AppResult<Value> {
        let url = format!("{}/users/{user_id}", endpoints::USERS_V1);
        let (user, membership) = futures::future::join(
            self.roblox.get::<Map<String, Value>>(&url),
            self.membership(user_id, None),
        )
        .await;
        let mut user = user?;
        user.extend(membership);
        Ok(Value::Object(user))
    }

    async fn robux_balance(&self) -> u64 {
        let Some(user_id) = self.current_user_id() else {
            return 0;
        };
        let url = format!("{}/users/{user_id}/currency", endpoints::ECONOMY_V1);
        match self.roblox.get::<Value>(&url).await {
            Ok(body) => body.get("robux").and_then(Value::as_u64).unwrap_or(0),
            Err(error) => {
                tracing::warn!(%error, "[Auth] Failed to fetch Robux balance");
                0
            }
        }
    }

    /// Presence for up to N users, batched by 100 and fetched concurrently.
    pub async fn presence(&self, user_ids: &[u64]) -> AppResult<Vec<Value>> {
        let ids = unique_positive(user_ids);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/presence/users", endpoints::PRESENCE_V1);
        let pages = futures::future::try_join_all(ids.chunks(PRESENCE_BATCH).map(|chunk| {
            let body = json!({ "userIds": chunk });
            let url = &url;
            async move { self.roblox.post::<_, Value>(url, &body).await }
        }))
        .await?;
        Ok(pages
            .into_iter()
            .flat_map(
                |mut page| match page.get_mut("userPresences").map(Value::take) {
                    Some(Value::Array(list)) => list,
                    _ => Vec::new(),
                },
            )
            .collect())
    }

    pub async fn headshots(
        &self,
        thumbnails: &crate::services::thumbnails::ThumbnailService,
        user_ids: &[u64],
        size: &str,
        include_profile_frame: bool,
    ) -> AppResult<std::collections::BTreeMap<String, String>> {
        let query = IdQuery {
            kind: "AvatarHeadShot",
            size,
            format: "Png",
            include_profile_frame: Some(include_profile_frame),
        };
        thumbnails
            .batch_by_id(user_ids, &query, |missing| async move {
                let pages = futures::future::try_join_all(missing.chunks(HEADSHOT_BATCH).map(|chunk| {
                    let url = format!(
                        "{}/users/avatar-headshot?userIds={}&size={size}&format=Png&isCircular=false&includeProfileFrame={include_profile_frame}",
                        endpoints::THUMBNAILS_V1,
                        join_ids(chunk)
                    );
                    async move { self.roblox.get::<ThumbPage>(&url).await }
                }))
                .await?;
                Ok(pages.into_iter().flat_map(|p| p.data).collect())
            })
            .await
    }

    async fn favorite_games(&self, user_id: u64) -> AppResult<Arc<[FavoriteGame]>> {
        let cached = self
            .favorites
            .lock()
            .get(&user_id)
            .filter(|(at, _)| at.elapsed() < FAVORITE_GAMES_TTL)
            .map(|(_, games)| Arc::clone(games));
        if let Some(games) = cached {
            return Ok(games);
        }
        let url = format!(
            "{}/users/{user_id}/favorite/games?sortOrder=Desc&limit=50",
            endpoints::GAMES_V2
        );
        let page: Value = self.roblox.get(&url).await?;
        let games: Arc<[FavoriteGame]> = page
            .get("data")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|g| {
                        let root = g
                            .pointer("/rootPlace/id")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        FavoriteGame {
                            id: g.get("id").and_then(Value::as_u64).unwrap_or(0),
                            name: g
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            place_id: root,
                            root_place_id: root,
                            description: g
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.favorites
            .lock()
            .insert(user_id, (Instant::now(), Arc::clone(&games)));
        Ok(games)
    }

    async fn toggle_favorite_game(&self, universe_id: u64, is_favorite: bool) -> AppResult<bool> {
        let url = format!("{}/games/{universe_id}/favorites", endpoints::GAMES_V1);
        self.roblox
            .post::<_, Value>(&url, &json!({ "isFavorited": is_favorite }))
            .await?;
        self.favorites.lock().clear();
        Ok(true)
    }
}

fn mark_invalid(data: &mut AuthFile, user_id: u64, invalid: bool) {
    if let Some(account) = data.accounts.iter_mut().find(|a| a.user_id == user_id) {
        account.session_invalid = if invalid {
            Some(true)
        } else {
            account.session_invalid.map(|_| false)
        };
    }
}

/// Port of `normalizeRoblosecurityCookie`: accepts a raw value, a
/// `.ROBLOSECURITY=...` pair or a full `Set-Cookie` header.
fn normalize_roblosecurity(input: &str) -> String {
    let mut s = input.trim();
    if let Some(rest) = s
        .strip_prefix("Set-Cookie:")
        .or_else(|| s.strip_prefix("set-cookie:"))
    {
        s = rest.trim();
    }
    if let Some(pos) = s.find(".ROBLOSECURITY=") {
        s = &s[pos + ".ROBLOSECURITY=".len()..];
    }
    let value = s.split(';').next().unwrap_or_default().trim();
    value.trim_matches('"').to_owned()
}

/// Exact port of `resolveMembership` from the Electron build.
fn resolve_membership(header: Option<&Value>, is_member: Option<bool>) -> Map<String, Value> {
    let is_premium = header
        .and_then(|h| h.get("isPremium"))
        .and_then(Value::as_bool)
        == Some(true);
    let header_plus = header
        .and_then(|h| h.get("isRobloxPlus"))
        .and_then(Value::as_bool);

    let has_premium = match (is_member, is_premium, header_plus) {
        (Some(member), _, _) => Some(member),
        (None, true, _) | (None, _, Some(true)) => Some(true),
        (None, _, Some(false)) => Some(false),
        _ => None,
    };
    let has_plus = match (header_plus, has_premium) {
        (Some(plus), _) => Some(plus),
        (None, Some(false)) => Some(false),
        (None, Some(true)) if header.is_some() => Some(!is_premium),
        _ => None,
    };

    let mut out = Map::new();
    if let Some(v) = has_premium {
        out.insert("hasPremiumMembership".into(), Value::Bool(v));
    }
    if let Some(v) = has_plus {
        out.insert("hasRobloxPlus".into(), Value::Bool(v));
    }
    out
}

pub fn register(router: &mut Router) {
    router
        .handle("auth:isAuthenticated", |ctx, _| async move {
            Ok(ctx.state.auth.is_authenticated())
        })
        .handle("auth:getUser", |ctx, _| async move {
            ctx.state.auth.authenticated_user().await
        })
        .handle("auth:getAccounts", |ctx, _| async move {
            ctx.state.auth.accounts_json()
        })
        .handle("auth:getRobuxBalance", |ctx, _| async move {
            Ok(ctx.state.auth.robux_balance().await)
        })
        .handle("auth:getUserDetails", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            ctx.state.auth.user_details(user_id).await
        })
        .handle("auth:getPresence", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            ctx.state.auth.presence(&ids).await
        })
        .handle("auth:getAvatarHeadshots", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size = args
                .opt::<String>(1)?
                .unwrap_or_else(|| "150x150".to_owned());
            let frame = args.opt::<bool>(2)?.unwrap_or(true);
            ctx.state
                .auth
                .headshots(&ctx.state.thumbnails, &ids, &size, frame)
                .await
        })
        .handle("auth:getFavoriteGames", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            ctx.state.auth.favorite_games(user_id).await
        })
        .handle("auth:toggleFavoriteGame", |ctx, args| async move {
            let universe_id: u64 = args.get(0)?;
            let is_favorite: bool = args.get(1)?;
            ctx.state
                .auth
                .toggle_favorite_game(universe_id, is_favorite)
                .await
        })
        .handle("auth:setCookie", |ctx, args| async move {
            let cookie: String = args.get(0)?;
            ctx.state.auth.set_cookie(&cookie).await
        })
        .handle("auth:logout", |ctx, _| async move {
            ctx.state.auth.logout()?;
            Ok(true)
        })
        .handle("auth:switchAccount", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            ctx.state.auth.switch_account(user_id).await
        })
        .handle("auth:removeAccount", |ctx, args| async move {
            let user_id: u64 = args.get(0)?;
            ctx.state.auth.remove_account(user_id).await
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_matches_electron_rules() {
        let premium = json!({ "isPremium": true });
        let out = resolve_membership(Some(&premium), None);
        assert_eq!(out.get("hasPremiumMembership"), Some(&Value::Bool(true)));
        assert_eq!(out.get("hasRobloxPlus"), Some(&Value::Bool(false)));
        assert!(resolve_membership(None, None).is_empty());
        let out = resolve_membership(None, Some(false));
        assert_eq!(out.get("hasRobloxPlus"), Some(&Value::Bool(false)));
    }

    #[test]
    fn cookie_normalization() {
        assert_eq!(normalize_roblosecurity("  abc  "), "abc");
        assert_eq!(normalize_roblosecurity(".ROBLOSECURITY=abc; path=/"), "abc");
        assert_eq!(
            normalize_roblosecurity("Set-Cookie: .ROBLOSECURITY=\"abc\"; domain=.roblox.com"),
            "abc"
        );
    }
}
