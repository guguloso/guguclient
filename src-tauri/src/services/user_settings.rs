//! Roblox account privacy settings (`userSettings:*`). Port of
//! `UserSettingsService`: user-settings-api plus `/my/settings/json`, the
//! latter also refreshing the stored age/premium flags on account switches.

use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints;
use crate::state::AppState;

#[derive(Default)]
struct Cached {
    settings: Option<(u64, Value)>,
    account_info: Option<(u64, Value)>,
}

#[derive(Default)]
pub struct UserSettingsService {
    cached: Mutex<Cached>,
    refresh_lock: tokio::sync::Mutex<()>,
}

/// `mapMySettingsToAccountInfo`.
fn map_account_info(raw: &Value) -> Value {
    let mut info = Map::new();
    for (to, from) in [
        ("userAbove13", "UserAbove13"),
        ("ageBracket", "AgeBracket"),
        ("isBirthdateLocked", "IsBirthdateLocked"),
        ("isPremium", "IsPremium"),
        ("hasRobloxSubscription", "HasRobloxSubscription"),
        ("isBcRenewalMembership", "IsBcRenewalMembership"),
        ("premiumFeatureId", "PremiumFeatureId"),
        ("accountAgeInDays", "AccountAgeInDays"),
        ("canTrade", "CanTrade"),
        ("canHideInventory", "CanHideInventory"),
        ("isAdmin", "IsAdmin"),
        ("useSuperSafePrivacyMode", "UseSuperSafePrivacyMode"),
        ("missingParentEmail", "MissingParentEmail"),
        ("isAppChatSettingEnabled", "IsAppChatSettingEnabled"),
        ("isGameChatSettingEnabled", "IsGameChatSettingEnabled"),
    ] {
        if let Some(v) = raw.get(from) {
            info.insert(to.to_owned(), v.clone());
        }
    }
    if let Some(sec) = raw.get("MyAccountSecurityModel").filter(|v| v.is_object()) {
        let flag = |k: &str| sec.get(k).and_then(Value::as_bool).unwrap_or(false);
        info.insert(
            "security".into(),
            json!({
                "isEmailSet": flag("IsEmailSet"),
                "isEmailVerified": flag("IsEmailVerified"),
                "isTwoStepEnabled": flag("IsTwoStepEnabled"),
                "showSignOutFromAllSessions": flag("ShowSignOutFromAllSessions"),
            }),
        );
    }
    Value::Object(info)
}

async fn web_get(state: &AppState, url: &str) -> AppResult<Value> {
    let context = state.auth.web_context().await;
    let context = context.as_ref().map(|(c, t)| (c.as_str(), t.as_str()));
    state.roblox.request_web(Method::GET, url, None, context).await
}

impl UserSettingsService {
    /// `initialize()`: on every account switch refresh and persist flags.
    pub fn initialize(state: &Arc<AppState>) {
        let weak = Arc::downgrade(state);
        let mut accounts = state.auth.subscribe_account_changes();
        tauri::async_runtime::spawn(async move {
            while accounts.changed().await.is_ok() {
                let user_id = *accounts.borrow_and_update();
                let Some(state) = weak.upgrade() else { break };
                match user_id {
                    None => *state.user_settings.cached.lock() = Cached::default(),
                    Some(user_id) => {
                        tauri::async_runtime::spawn(async move {
                            let (_, info) = refresh(&state).await;
                            if let Some(info) = info {
                                let flag = |k: &str| info.get(k).and_then(Value::as_bool);
                                state.auth.update_stored_account_flags(
                                    user_id,
                                    flag("userAbove13"),
                                    flag("isPremium"),
                                    flag("hasRobloxSubscription"),
                                );
                            }
                        });
                    }
                }
            }
        });
    }

    fn cached_settings(&self, active: Option<u64>) -> Option<Value> {
        let cached = self.cached.lock();
        cached.settings.as_ref().filter(|(id, _)| Some(*id) == active).map(|(_, v)| v.clone())
    }

    fn cached_account_info(&self, active: Option<u64>) -> Option<Value> {
        let cached = self.cached.lock();
        cached.account_info.as_ref().filter(|(id, _)| Some(*id) == active).map(|(_, v)| v.clone())
    }
}

/// `refresh()`: `(userSettings, accountInfo)`, each `None` on failure.
pub async fn refresh(state: &AppState) -> (Option<Value>, Option<Value>) {
    let Some(user_id) = state.auth.current_user_id() else {
        *state.user_settings.cached.lock() = Cached::default();
        return (None, None);
    };
    let _guard = state.user_settings.refresh_lock.lock().await;
    let settings_url = format!("{}/v1/user-settings", endpoints::USER_SETTINGS_API);
    let info_url = format!("{}/my/settings/json", endpoints::WEB);
    let (settings, info) = futures::join!(web_get(state, &settings_url), web_get(state, &info_url));
    let service = &state.user_settings;
    let still_active = state.auth.current_user_id() == Some(user_id);

    let settings = match settings {
        Ok(v) if still_active => {
            service.cached.lock().settings = Some((user_id, v.clone()));
            Some(v)
        }
        Ok(_) => None,
        Err(error) => {
            tracing::error!("UserSettings: Failed to refresh user-settings for {user_id}: {error}");
            let mut cached = service.cached.lock();
            if cached.settings.as_ref().is_some_and(|(id, _)| *id != user_id) {
                cached.settings = None;
            }
            None
        }
    };
    let info = match info {
        Ok(raw) if still_active => {
            let info = map_account_info(&raw);
            service.cached.lock().account_info = Some((user_id, info.clone()));
            Some(info)
        }
        Ok(_) => None,
        Err(error) => {
            tracing::error!("UserSettings: Failed to refresh account-info for {user_id}: {error}");
            let mut cached = service.cached.lock();
            if cached.account_info.as_ref().is_some_and(|(id, _)| *id != user_id) {
                cached.account_info = None;
            }
            None
        }
    };
    (settings, info)
}

async fn update(state: &AppState, patch: Value) -> AppResult<Value> {
    let user_id = state
        .auth
        .current_user_id()
        .ok_or_else(|| AppError::msg("Cannot update user settings without an active account"))?;
    let url = format!("{}/v1/user-settings", endpoints::USER_SETTINGS_API);
    let context = state.auth.web_context().await;
    let context = context.as_ref().map(|(c, t)| (c.as_str(), t.as_str()));
    state.roblox.request_web::<Value>(Method::POST, &url, Some(&patch), context).await?;
    let mut next = state.user_settings.cached_settings(Some(user_id)).unwrap_or_else(|| json!({}));
    if let (Some(next), Some(patch)) = (next.as_object_mut(), patch.as_object()) {
        next.extend(patch.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    if state.auth.current_user_id() == Some(user_id) {
        state.user_settings.cached.lock().settings = Some((user_id, next.clone()));
    }
    tracing::info!("UserSettings: Updated for user {user_id}");
    Ok(next)
}

pub fn register(router: &mut Router) {
    router
        .handle("userSettings:get", |ctx, _| async move {
            let state = &ctx.state;
            if let Some(cached) = state.user_settings.cached_settings(state.auth.current_user_id()) {
                return Ok(Some(cached));
            }
            Ok(refresh(state).await.0)
        })
        .handle("userSettings:getCached", |ctx, _| async move {
            Ok(ctx.state.user_settings.cached_settings(ctx.state.auth.current_user_id()))
        })
        .handle("userSettings:refresh", |ctx, _| async move { Ok(refresh(&ctx.state).await.0) })
        .handle("userSettings:update", |ctx, args| async move {
            let patch: Value = args.get(0)?;
            update(&ctx.state, patch).await
        })
        .handle("userSettings:getAccountInfo", |ctx, _| async move {
            let state = &ctx.state;
            if let Some(cached) = state.user_settings.cached_account_info(state.auth.current_user_id()) {
                return Ok(Some(cached));
            }
            Ok(refresh(state).await.1)
        })
        .handle("userSettings:getCachedAccountInfo", |ctx, _| async move {
            Ok(ctx.state.user_settings.cached_account_info(ctx.state.auth.current_user_id()))
        });
}
