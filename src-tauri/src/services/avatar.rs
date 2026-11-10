//! Avatar editor (`avatar:*`). Port of `AvatarService`, `AvatarAccountClient`,
//! `ProfileFrameService` and the thumbnail helpers of `AvatarController`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::roblox::endpoints::{self, join_ids};
use crate::services::catalog::{raw_items_details, ItemRef};
use crate::services::thumbnails::to_local_url;
use crate::state::AppState;

const AVATAR: &str = "https://avatar.roblox.com";
const RULES_TTL: Duration = Duration::from_secs(60 * 60);
const EMOTE_SLOTS: i64 = 8;
const R6_ID: i64 = 1;
const R15_ID: i64 = 3;

#[derive(Default)]
pub struct AvatarState {
    rules: Mutex<HashMap<String, (Instant, Value)>>,
}

fn valid_emote_position(p: Option<&Value>) -> bool {
    p.and_then(Value::as_i64).is_some_and(|p| (1..=EMOTE_SLOTS).contains(&p)) && p.is_some_and(|v| v.is_i64() || v.is_u64())
}

/// `normalizeAvatarEmotes`.
fn normalize_emotes(raw: &Value) -> Value {
    let mut emotes: Vec<Value> = raw
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e.get("assetId").is_some_and(Value::is_number) && valid_emote_position(e.get("position")))
        .map(|e| {
            let mut out = json!({ "assetId": e["assetId"], "position": e["position"] });
            if let Some(name) = e.get("assetName").filter(|n| n.is_string()) {
                out["assetName"] = name.clone();
            }
            out
        })
        .collect();
    emotes.sort_by_key(|e| e["position"].as_i64().unwrap_or(0));
    Value::Array(emotes)
}

/// `normalizeAvatarModel`.
fn normalize_model(mut raw: Value) -> Value {
    let Some(map) = raw.as_object_mut() else { return raw };
    if let Some(emotes) = map.remove("emotes") {
        map.insert("emotes".into(), normalize_emotes(&emotes));
    }
    match map.remove("playerAvatarType") {
        Some(t) if t == json!(R6_ID) || t == json!("R6") => {
            map.insert("playerAvatarType".into(), json!(R6_ID));
        }
        Some(t) if t == json!(R15_ID) || t == json!("R15") => {
            map.insert("playerAvatarType".into(), json!(R15_ID));
        }
        _ => {}
    }
    raw
}

fn makeup_category() -> Value {
    let sub = |name: &str, slug: &str, id: i64| {
        json!({ "assetTypeIds": [id], "bundleTypeIds": [], "name": name, "shortName": name, "subcategory": slug, "subcategoryId": -id, "taxonomy": slug })
    };
    json!({
        "assetTypeIds": [90, 89, 88, 77, 76],
        "bundleTypeIds": [],
        "category": "Makeup",
        "categoryId": -1,
        "isSearchable": false,
        "name": "Makeup",
        "orderIndex": 100,
        "subcategories": [
            sub("Eyes", "EyeMakeup", 90), sub("Lips", "LipMakeup", 89), sub("Face", "FaceMakeup", 88),
            sub("Eyelashes", "EyelashAccessories", 77), sub("Eyebrows", "EyebrowAccessories", 76),
        ],
        "taxonomy": "Makeup",
    })
}

fn unwrap_number(v: Option<&Value>) -> Option<Value> {
    match v? {
        n @ Value::Number(_) => Some(n.clone()),
        Value::Object(o) => o.get("parsedValue").filter(|p| p.is_number()).cloned(),
        _ => None,
    }
}

/// `normalizeInventoryItem` (flattens `parsedValue` wrappers in outfits).
fn normalize_inventory_item(mut item: Value) -> Value {
    let Some(detail) = item.get_mut("outfitDetail").filter(|d| d.is_object()) else { return item };
    let assets: Vec<Value> = detail
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| {
            let mut meta = Map::new();
            for key in ["order", "puffiness", "version"] {
                if let Some(v) = unwrap_number(a.get("meta").and_then(|m| m.get(key))) {
                    meta.insert(key.into(), v);
                }
            }
            if meta.is_empty() {
                json!({ "id": a.get("id") })
            } else {
                json!({ "id": a.get("id"), "meta": meta })
            }
        })
        .collect();
    let mut scales = Map::new();
    for key in ["height", "width", "head", "depth", "proportion", "bodyType"] {
        if let Some(v) = unwrap_number(detail.get("scales").and_then(|s| s.get(key))) {
            scales.insert(key.into(), v);
        }
    }
    detail["assets"] = Value::Array(assets);
    detail["scales"] = Value::Object(scales);
    item
}

fn limited_type(restrictions: Option<&Value>) -> Option<&'static str> {
    let mut limited = false;
    for v in restrictions.and_then(Value::as_array).into_iter().flatten() {
        match v {
            Value::String(s) if s == "Collectible" || s == "LimitedUnique" => return Some("limitedUnique"),
            Value::Number(n) if matches!(n.as_i64(), Some(6 | 2)) => return Some("limitedUnique"),
            Value::String(s) if s == "Limited" => limited = true,
            Value::Number(n) if n.as_i64() == Some(3) => limited = true,
            _ => {}
        }
    }
    limited.then_some("limited")
}

async fn success_of(state: &AppState, url: &str, body: &Value) -> AppResult<bool> {
    let r: Value = state.roblox.post(url, body).await?;
    Ok(r.get("success").and_then(Value::as_bool).unwrap_or(false))
}

/// `AvatarAccountClient.setWearingAssets` (also used by outfits/equip).
pub async fn set_wearing_assets(state: &AppState, assets: &[Value]) -> AppResult<Value> {
    set_wearing_assets_as(state, assets, None).await
}

async fn set_wearing_assets_as(state: &AppState, assets: &[Value], cookie: Option<&str>) -> AppResult<Value> {
    let assets: Vec<Value> = assets
        .iter()
        .map(|a| match a.get("meta").filter(|m| !m.is_null()) {
            Some(meta) => json!({ "id": a.get("id"), "meta": meta }),
            None => json!({ "id": a.get("id") }),
        })
        .collect();
    let body = json!({ "avatarDefinition": { "updateAvatarModel": { "assets": assets } }, "updateTypes": ["UpdateAssets"] });
    let url = format!("{AVATAR}/v4/avatar");
    let response = state.roblox.request_for(Method::PATCH, &url, Some(&body), cookie).await?;
    let r: Value = crate::roblox::RobloxClient::decode(&url, response).await?;
    let success = r.get("success").and_then(Value::as_bool).unwrap_or(false);
    let mut result = json!({ "success": success });
    let invalid: Vec<Value> =
        r.pointer("/validation/invalidAssets").and_then(Value::as_array).into_iter().flatten().filter(|e| e.get("id").is_some_and(Value::is_number)).cloned().collect();
    if !invalid.is_empty() {
        result["invalidAssetIds"] = Value::Array(invalid.iter().map(|a| a["id"].clone()).collect());
        result["invalidAssets"] = Value::Array(invalid);
    }
    if !success {
        tracing::warn!("Avatar: setWearingAssets returned failure");
    }
    Ok(result)
}

/// `buildCurrentAvatarOutfitUpdatePayload`.
pub async fn current_outfit_payload(state: &AppState) -> AppResult<Value> {
    let avatar: Value = state.roblox.get(&format!("{AVATAR}/v2/avatar/avatar")).await.inspect_err(|error| {
        tracing::error!("Avatar: Failed to read the current avatar for a saved avatar: {error}");
    })?;
    let avatar_type = match avatar.get("playerAvatarType").and_then(Value::as_i64) {
        Some(R6_ID) => Some("R6"),
        Some(R15_ID) => Some("R15"),
        _ => None,
    };
    let mut payload = json!({
        "bodyColor3s": avatar.get("bodyColor3s"),
        "scale": avatar.get("scales"),
        "assets": avatar.get("assets").and_then(Value::as_array).into_iter().flatten()
            .map(|a| json!({ "id": a.get("id"), "meta": a.get("meta") })).collect::<Vec<_>>(),
    });
    if let Some(t) = avatar_type {
        payload["playerAvatarType"] = json!(t);
    }
    Ok(payload)
}

/// `SavedAvatarWriteError` (or any other failure, with no status).
pub struct SavedAvatarError {
    pub status: Option<u16>,
    pub code: Option<i64>,
    pub message: String,
}

impl From<AppError> for SavedAvatarError {
    fn from(error: AppError) -> Self {
        Self { status: None, code: None, message: error.to_string() }
    }
}

/// `readSavedAvatarWriteResponse`, keeping the Roblox error code.
async fn saved_avatar_write_detailed(state: &AppState, method: Method, url: &str, body: &Value, operation: &str) -> Result<Value, SavedAvatarError> {
    let response = state.roblox.request(method, url, Some(body)).await?;
    if response.status().is_success() {
        return Ok(response.json().await.map_err(AppError::from)?);
    }
    let status = response.status().as_u16();
    let parsed: Value = serde_json::from_str(&response.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    let error = parsed.pointer("/errors/0");
    let message = error
        .and_then(|e| e.get("userFacingMessage").or(e.get("message")))
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Failed to {operation} saved avatar"));
    Err(SavedAvatarError { status: Some(status), code: error.and_then(|e| e.get("code")).and_then(Value::as_i64), message })
}

/// `createSavedAvatar`.
pub async fn create_saved_avatar(state: &AppState, payload: &Value) -> Result<Value, SavedAvatarError> {
    let created = saved_avatar_write_detailed(state, Method::POST, &format!("{AVATAR}/v3/outfits/create"), payload, "create").await.inspect_err(|e| {
        tracing::error!("Avatar: Failed to create saved avatar: {}", e.message);
    })?;
    if !created.get("id").is_some_and(Value::is_number) {
        tracing::error!("Avatar: Create saved avatar returned invalid response");
        return Err(AppError::msg("Failed to create saved avatar").into());
    }
    Ok(saved_summary(&created, "Untitled Outfit", true))
}

/// `updateSavedAvatar`: fills a sparse response from the outfit details.
pub async fn update_saved_avatar(state: &AppState, id: u64, payload: &Value) -> Result<Value, SavedAvatarError> {
    let mut updated = saved_avatar_write_detailed(state, Method::PATCH, &format!("{AVATAR}/v3/outfits/{id}"), payload, "update").await.inspect_err(|e| {
        tracing::error!("Avatar: Failed to update saved avatar {id}: {}", e.message);
    })?;
    if !updated.get("id").is_some_and(Value::is_number) {
        return Err(AppError::msg("Failed to update saved avatar").into());
    }
    let missing =
        updated.get("name").and_then(Value::as_str).is_none_or(str::is_empty) || updated.get("isEditable").is_none() || updated.get("outfitType").is_none();
    if missing {
        match state.roblox.get::<Value>(&format!("{AVATAR}/v3/outfits/{id}/details")).await {
            Ok(details) => {
                for key in ["name", "isEditable", "outfitType"] {
                    if updated.get(key).is_none_or(|v| v.is_null() || v.as_str() == Some("")) {
                        if let Some(v) = details.get(key) {
                            updated[key] = v.clone();
                        }
                    }
                }
            }
            Err(error) => tracing::warn!("Avatar: Failed to fetch outfit details after update: {error}"),
        }
    }
    Ok(saved_summary(&updated, "Untitled Outfit", true))
}

/// `getSavedAvatars` (public per-user character list).
pub async fn saved_avatars(state: &AppState, user: u64, token: Option<&str>, per_page: Option<u64>) -> AppResult<Value> {
    let mut url = format!("{AVATAR}/v2/avatar/users/{user}/outfits?");
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        url.push_str(&format!("paginationToken={}&", urlencoding::encode(t)));
    }
    if let Some(n) = per_page.filter(|n| *n > 0) {
        url.push_str(&format!("itemsPerPage={n}"));
    }
    let r: Value = state.roblox.get(url.trim_end_matches(['&', '?'])).await.inspect_err(|error| {
        tracing::error!("Avatar: Failed to get saved avatars: {error}");
    })?;
    let data: Vec<Value> = r
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("id").is_some_and(Value::is_number))
        .map(|o| saved_summary(o, "Untitled Outfit", false))
        .collect();
    let token = r.get("paginationToken").and_then(Value::as_str).filter(|t| !t.is_empty());
    Ok(json!({ "data": data, "paginationToken": token }))
}

/// Who an avatar write acts as: the active session (`createActiveWearTarget`)
/// or a stored account (`AvatarService.forAccount`).
#[derive(Clone, Copy)]
pub struct AvatarTarget {
    pub user_id: u64,
    account: bool,
}

/// `AvatarAccountClient.getCharacter`.
pub enum CharacterLookup {
    Found(Value),
    Missing,
    Failed,
}

impl AvatarTarget {
    pub fn active(state: &AppState) -> Option<Self> {
        state.auth.current_user_id().map(|user_id| Self { user_id, account: false })
    }

    pub fn for_account(state: &AppState, user_id: u64) -> Option<Self> {
        state.auth.request_cookie_for_user(user_id).map(|_| Self { user_id, account: true })
    }

    pub fn is_current(&self, state: &AppState) -> bool {
        if self.account {
            state.auth.request_cookie_for_user(self.user_id).is_some()
        } else {
            state.auth.current_user_id() == Some(self.user_id)
        }
    }

    fn cookie(&self, state: &AppState) -> AppResult<Option<String>> {
        if !self.account {
            return Ok(None);
        }
        state.auth.request_cookie_for_user(self.user_id).map(Some).ok_or_else(|| AppError::msg("The account's session is no longer available"))
    }

    async fn call(&self, state: &AppState, method: Method, url: &str, body: Option<&Value>) -> AppResult<Value> {
        let cookie = self.cookie(state)?;
        let response = state.roblox.request_for(method, url, body, cookie.as_deref()).await?;
        crate::roblox::RobloxClient::decode(url, response).await
    }

    async fn success(&self, state: &AppState, url: &str, body: &Value) -> AppResult<bool> {
        let r = self.call(state, Method::POST, url, Some(body)).await?;
        Ok(r.get("success").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn get_avatar(&self, state: &AppState) -> AppResult<Value> {
        Ok(normalize_model(self.call(state, Method::GET, &format!("{AVATAR}/v1/avatar"), None).await?))
    }

    pub async fn get_rules(&self, state: &AppState) -> AppResult<Value> {
        let key = format!("rules:{}", self.user_id);
        if let Some((at, rules)) = state.avatar.rules.lock().get(&key) {
            if at.elapsed() < RULES_TTL {
                return Ok(rules.clone());
            }
        }
        let rules = self.call(state, Method::GET, &format!("{AVATAR}/v1/avatar-rules"), None).await?;
        state.avatar.rules.lock().insert(key, (Instant::now(), rules.clone()));
        Ok(rules)
    }

    pub async fn set_player_avatar_type(&self, state: &AppState, rig: &Value) -> AppResult<bool> {
        self.success(state, &format!("{AVATAR}/v1/avatar/set-player-avatar-type"), &json!({ "playerAvatarType": rig })).await
    }

    pub async fn set_body_colors(&self, state: &AppState, colors: &Value) -> AppResult<bool> {
        self.success(state, &format!("{AVATAR}/v1/avatar/set-body-colors"), colors).await
    }

    pub async fn set_scales(&self, state: &AppState, scales: &Value) -> AppResult<bool> {
        self.success(state, &format!("{AVATAR}/v1/avatar/set-scales"), scales).await
    }

    pub async fn set_body_color3s(&self, state: &AppState, colors: &Value) -> AppResult<bool> {
        self.success(state, &format!("{AVATAR}/v2/avatar/set-body-colors"), colors).await
    }

    pub async fn get_body_color3s(&self, state: &AppState) -> AppResult<Value> {
        let avatar = self.call(state, Method::GET, &format!("{AVATAR}/v2/avatar/avatar"), None).await?;
        Ok(avatar.get("bodyColor3s").cloned().unwrap_or(Value::Null))
    }

    pub async fn set_wearing_assets(&self, state: &AppState, assets: &[Value]) -> AppResult<Value> {
        let cookie = self.cookie(state)?;
        set_wearing_assets_as(state, assets, cookie.as_deref()).await
    }

    pub async fn get_character(&self, state: &AppState, outfit: u64) -> CharacterLookup {
        let url = format!("{AVATAR}/v3/outfits/{outfit}/details");
        let result = async {
            let cookie = self.cookie(state)?;
            state.roblox.request_for(Method::GET, &url, None, cookie.as_deref()).await
        }
        .await;
        match result {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(character) => CharacterLookup::Found(character),
                Err(error) => {
                    tracing::warn!("Avatar: character {outfit} lookup failed: {error}");
                    CharacterLookup::Failed
                }
            },
            Ok(response) => {
                let status = response.status().as_u16();
                tracing::warn!("Avatar: character {outfit} lookup was refused ({status})");
                if matches!(status, 400 | 403 | 404) {
                    CharacterLookup::Missing
                } else {
                    CharacterLookup::Failed
                }
            }
            Err(error) => {
                tracing::warn!("Avatar: character {outfit} lookup failed: {error}");
                CharacterLookup::Failed
            }
        }
    }
}

fn saved_summary(raw: &Value, fallback_name: &str, editable_default: bool) -> Value {
    let mut out = json!({
        "id": raw.get("id"),
        "name": raw.get("name").and_then(Value::as_str).unwrap_or(fallback_name),
        "isEditable": raw.get("isEditable").and_then(Value::as_bool).unwrap_or(editable_default),
    });
    if let Some(t) = raw.get("outfitType") {
        out["outfitType"] = t.clone();
    }
    out
}

async fn equip_saved_avatar(state: &AppState, outfit: u64) -> AppResult<Value> {
    let details: Value = state.roblox.get(&format!("{AVATAR}/v1/outfits/{outfit}/details")).await?;
    if details.is_null() {
        tracing::warn!("Avatar: Outfit details missing during equip ({outfit})");
        return Ok(json!({ "success": false, "failedStage": "fetchDetails" }));
    }
    let avatar_type = match details.get("playerAvatarType").and_then(Value::as_str) {
        Some("R6") => Some(R6_ID),
        Some("R15") => Some(R15_ID),
        _ => None,
    };
    if let Some(t) = avatar_type {
        if !success_of(state, &format!("{AVATAR}/v1/avatar/set-player-avatar-type"), &json!({ "playerAvatarType": t })).await? {
            tracing::warn!("Avatar: Failed to set avatar type while equipping outfit {outfit}");
            return Ok(json!({ "success": false, "failedStage": "playerAvatarType" }));
        }
    }
    if let Some(colors) = details.get("bodyColors").filter(|c| !c.is_null()) {
        if !success_of(state, &format!("{AVATAR}/v1/avatar/set-body-colors"), colors).await? {
            tracing::warn!("Avatar: Failed to set body colors while equipping outfit {outfit}");
            return Ok(json!({ "success": false, "failedStage": "bodyColors" }));
        }
    }
    if let Some(scale) = details.get("scale").filter(|c| !c.is_null()) {
        if !success_of(state, &format!("{AVATAR}/v1/avatar/set-scales"), scale).await? {
            tracing::warn!("Avatar: Failed to set scales while equipping outfit {outfit}");
            return Ok(json!({ "success": false, "failedStage": "scales" }));
        }
    }
    let assets: Vec<Value> =
        details.get("assets").and_then(Value::as_array).into_iter().flatten().map(|a| json!({ "id": a.get("id"), "meta": a.get("meta") })).collect();
    let assets_result = set_wearing_assets(state, &assets).await?;
    if !assets_result["success"].as_bool().unwrap_or(false) {
        tracing::warn!("Avatar: Failed to set wearing assets while equipping outfit {outfit}");
        let mut result = json!({ "success": false, "failedStage": "assets" });
        for key in ["invalidAssetIds", "invalidAssets"] {
            if let Some(v) = assets_result.get(key) {
                result[key] = v.clone();
            }
        }
        return Ok(result);
    }
    Ok(json!({ "success": true }))
}

async fn inventory(state: &AppState, query: Map<String, Value>) -> AppResult<Value> {
    let params: Vec<String> = query
        .iter()
        .filter(|(_, v)| !v.is_null() && v.as_str() != Some(""))
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(&v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))))
        .collect();
    let raw: Value = state.roblox.get(&format!("{AVATAR}/v1/avatar-inventory?{}", params.join("&"))).await.inspect_err(|error| {
        tracing::error!("Avatar: Failed to get avatar inventory: {error}");
    })?;
    let items: Vec<Value> = raw.get("avatarInventoryItems").and_then(Value::as_array).into_iter().flatten().cloned().map(normalize_inventory_item).collect();
    let asset_ids: Vec<u64> = items
        .iter()
        .filter(|i| i.pointer("/itemCategory/itemType").and_then(Value::as_i64) == Some(1))
        .filter_map(|i| i.get("itemId").and_then(Value::as_u64))
        .collect();
    let items = if asset_ids.is_empty() {
        items
    } else {
        let refs: Vec<ItemRef> = asset_ids.iter().map(|id| ItemRef { id: *id, item_type: "Asset".into() }).collect();
        match raw_items_details(state, &refs, true).await {
            Ok(details) => {
                let by_id: HashMap<u64, &'static str> = details
                    .iter()
                    .filter_map(|d| Some((d.get("id")?.as_u64().filter(|id| *id > 0)?, limited_type(d.get("itemRestrictions"))?)))
                    .collect();
                items
                    .into_iter()
                    .map(|mut i| {
                        if let Some(t) = i.get("itemId").and_then(Value::as_u64).and_then(|id| by_id.get(&id)) {
                            i["limitedType"] = json!(t);
                        }
                        i
                    })
                    .collect()
            }
            Err(error) => {
                tracing::warn!("Avatar: Failed to hydrate limitedType for {} item(s): {error}", asset_ids.len());
                items
            }
        }
    };
    Ok(json!({ "avatarInventoryItems": items, "nextPageToken": raw.get("nextPageToken").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null) }))
}

async fn thumbs(state: &AppState, url: String, ids: &[u64], what: &str) -> Value {
    match state.roblox.get::<Value>(&url).await {
        Ok(r) => Value::Array(
            r.get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|i| {
                    let completed = i.get("state").and_then(Value::as_str) == Some("Completed");
                    let image = i.get("imageUrl").and_then(Value::as_str).filter(|u| completed && !u.is_empty()).map(to_local_url);
                    json!({ "targetId": i.get("targetId").cloned().filter(|v| !v.is_null()).unwrap_or(json!(0)), "imageUrl": image })
                })
                .collect(),
        ),
        Err(error) => {
            tracing::error!("Failed to fetch {what} thumbnails: {error}");
            Value::Array(ids.iter().map(|id| json!({ "targetId": id, "imageUrl": null })).collect())
        }
    }
}

async fn set_profile_frame(state: &AppState, asset: Option<u64>, user: u64) -> Value {
    if state.auth.current_user_id() != Some(user) {
        tracing::info!("ProfileFrames: account changed, change dropped");
        return json!({ "failureReason": "accountChanged", "success": false });
    }
    let body = json!({
        "avatarDefinition": { "updateAvatarConfig": { "profileFrameRequestModel": { "id": asset.unwrap_or(0) } } },
        "updateTypes": ["UpdateProfileFrame"],
    });
    let response = match state.roblox.request(Method::PATCH, &format!("{AVATAR}/v4/avatar"), Some(&body)).await {
        Ok(r) => r,
        Err(error) => {
            tracing::error!("ProfileFrames: setProfileFrame request failed: {error}");
            return json!({ "failureReason": "unknown", "success": false });
        }
    };
    if !response.status().is_success() {
        tracing::warn!("ProfileFrames: setProfileFrame refused ({})", response.status());
        return json!({ "failureReason": "unknown", "success": false });
    }
    let result: Value = response.json().await.unwrap_or(Value::Null);
    if result.get("success").and_then(Value::as_bool) == Some(true) {
        state.thumbnails.delete_framed_target("AvatarHeadShot", user);
        state.events.emit("auth:headshotChanged", json!({ "userId": user }));
        return json!({ "success": true });
    }
    let invalid = result.pointer("/validation/invalidProfileFrame").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    tracing::warn!("ProfileFrames: setProfileFrame returned failure");
    json!({ "failureReason": if invalid { "invalidFrame" } else { "unknown" }, "success": false })
}

async fn profile_frames(state: &AppState) -> AppResult<Value> {
    let url = format!(
        "{}/widgets?context=catalog-tab%3Aprofile-frames&requestId={}",
        endpoints::MARKETPLACE_WIDGETS_V1,
        uuid::Uuid::new_v4()
    );
    let widgets: Value = state.roblox.get(&url).await?;
    let mut ids = Vec::new();
    for widget in widgets.get("widgets").and_then(Value::as_object).into_iter().flat_map(|m| m.values()) {
        for c in widget.get("content").and_then(Value::as_array).into_iter().flatten() {
            if c.get("type").and_then(Value::as_str) == Some("Asset") {
                if let Some(id) = c.get("id").and_then(Value::as_u64) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
    }
    let mut names = HashMap::new();
    if !ids.is_empty() {
        let body = json!({ "content": ids.iter().map(|id| json!({ "id": id, "type": "Asset" })).collect::<Vec<_>>() });
        match state.roblox.post::<_, Value>(&format!("{}/widgets/hydrate", endpoints::MARKETPLACE_WIDGETS_V1), &body).await {
            Ok(r) => {
                for item in r.get("hydratedContent").and_then(Value::as_array).into_iter().flatten() {
                    if let (Some(id), Some(name)) = (item.get("id").and_then(Value::as_u64), item.get("name").and_then(Value::as_str).filter(|n| !n.is_empty())) {
                        names.insert(id, name.to_owned());
                    }
                }
            }
            Err(error) => tracing::warn!("ProfileFrames: frame names unavailable: {error}"),
        }
    }
    Ok(Value::Array(ids.iter().map(|id| json!({ "assetId": id, "name": names.get(id) })).collect()))
}

pub fn register(router: &mut Router) {
    router
        .handle("avatar:getAvatar", |ctx, _| async move { Ok(normalize_model(ctx.state.roblox.get(&format!("{AVATAR}/v1/avatar")).await?)) })
        .handle("avatar:getUserAvatar", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            Ok(normalize_model(ctx.state.roblox.get(&format!("{AVATAR}/v1/users/{user}/avatar")).await?))
        })
        .handle("avatar:getCategories", |ctx, _| async move {
            let mut categories: Vec<Value> = ctx.state.roblox.get::<Value>(&format!("{}/categories", endpoints::CATALOG_V1)).await?.as_array().cloned().unwrap_or_default();
            let listed = categories.iter().any(|c| {
                ["category", "taxonomy"].iter().any(|k| c.get(*k).and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("makeup")))
            });
            if !listed {
                categories.push(makeup_category());
            }
            Ok(categories)
        })
        .handle("avatar:getRules", |ctx, _| async move {
            let key = format!("rules:{}", ctx.state.auth.current_user_id().map_or_else(|| "anonymous".to_owned(), |id| id.to_string()));
            if let Some((at, rules)) = ctx.state.avatar.rules.lock().get(&key) {
                if at.elapsed() < RULES_TTL {
                    return Ok(rules.clone());
                }
            }
            let rules: Value = ctx.state.roblox.get(&format!("{AVATAR}/v1/avatar-rules")).await?;
            ctx.state.avatar.rules.lock().insert(key, (Instant::now(), rules.clone()));
            Ok(rules)
        })
        .handle("avatar:setBodyColors", |ctx, args| async move {
            let colors: Value = args.get(0)?;
            success_of(&ctx.state, &format!("{AVATAR}/v1/avatar/set-body-colors"), &colors).await
        })
        .handle("avatar:setScales", |ctx, args| async move {
            let scales: Value = args.get(0)?;
            success_of(&ctx.state, &format!("{AVATAR}/v1/avatar/set-scales"), &scales).await
        })
        .handle("avatar:setWearingAssets", |ctx, args| async move {
            let assets: Vec<Value> = args.get(0)?;
            set_wearing_assets(&ctx.state, &assets).await
        })
        .handle("avatar:getBackground", |ctx, _| async move {
            let url = format!("{AVATAR}/v4/avatar?selectionTypes=0&selectionTypes=1&selectionTypes=2&selectionTypes=3&selectionTypes=4&selectionTypes=5");
            let r: Value = ctx.state.roblox.get(&url).await?;
            let asset = r.pointer("/avatarConfigurations/background/backgroundAsset");
            Ok(match asset.and_then(|a| a.get("id")).filter(|id| id.is_number()) {
                Some(id) => json!({ "assetId": id, "name": asset.and_then(|a| a.get("name")).and_then(Value::as_str).unwrap_or("") }),
                None => Value::Null,
            })
        })
        .handle("avatar:setBackground", |ctx, args| async move {
            let asset: Option<u64> = args.opt(0)?;
            let model = asset.map(|id| json!({ "id": id })).unwrap_or(Value::Null);
            let body = json!({ "avatarDefinition": { "updateAvatarConfig": { "backgroundRequestModel": model } }, "updateTypes": ["UpdateBackground"] });
            let r: Value = ctx.state.roblox.patch(&format!("{AVATAR}/v4/avatar"), &body).await?;
            let success = r.get("success").and_then(Value::as_bool).unwrap_or(false);
            if !success {
                tracing::warn!("Avatar: setBackground returned failure");
            }
            Ok(json!({ "success": success }))
        })
        .handle("avatar:getEmotes", |ctx, _| async move { Ok(normalize_emotes(&ctx.state.roblox.get::<Value>(&format!("{AVATAR}/v1/emotes")).await?)) })
        .handle("avatar:setEmotes", |ctx, args| async move {
            let emotes: Vec<Value> = args.get(0)?;
            if !emotes.iter().all(|e| valid_emote_position(e.get("position"))) {
                return Ok(Value::Null);
            }
            let body = json!({
                "avatarDefinition": { "updateAvatarConfig": { "emoteRequestModels": emotes.iter().map(|e| json!({ "assetId": e.get("assetId"), "position": e.get("position") })).collect::<Vec<_>>() } },
                "updateTypes": ["UpdateEmotes"],
            });
            match ctx.state.roblox.patch::<_, Value>(&format!("{AVATAR}/v4/avatar"), &body).await {
                Ok(r) if r.get("success").and_then(Value::as_bool) == Some(true) => {
                    ctx.state.track("avatar.emotes_update", json!({ "emote_count": emotes.len() }));
                }
                Ok(_) => tracing::warn!("Avatar: setEmotes returned failure"),
                Err(error) => tracing::error!("Avatar: setEmotes request failed: {error}"),
            }
            Ok(Value::Null)
        })
        .handle("avatar:getProfileFrames", |ctx, _| async move { profile_frames(&ctx.state).await })
        .handle("avatar:getEquippedProfileFrame", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            let r: Value = ctx.state.roblox.get(&format!("{AVATAR}/v4/avatar/users/{user}?selectionTypes=ProfileFrame")).await?;
            let asset = r.pointer("/avatarConfigurations/profileFrame/frameAsset");
            Ok(match asset.and_then(|a| a.get("id")).and_then(Value::as_u64).filter(|id| *id > 0) {
                Some(id) => json!({ "assetId": id, "name": asset.and_then(|a| a.get("name")).cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null) }),
                None => Value::Null,
            })
        })
        .handle("avatar:setProfileFrame", |ctx, args| async move {
            let asset: Option<u64> = args.opt(0)?;
            let user: u64 = args.get(1)?;
            let result = set_profile_frame(&ctx.state, asset, user).await;
            if result["success"] == json!(true) {
                ctx.state.track("avatar.profile_frame_update", json!({ "frame_asset_id": asset }));
            }
            Ok(Value::Null)
        })
        .handle("avatar:setPlayerAvatarType", |ctx, args| async move {
            let t: Value = args.get(0)?;
            success_of(&ctx.state, &format!("{AVATAR}/v1/avatar/set-player-avatar-type"), &json!({ "playerAvatarType": t })).await
        })
        .handle("avatar:redrawThumbnail", |ctx, _| async move {
            ctx.state.roblox.post_empty::<Value>(&format!("{AVATAR}/v1/avatar/redraw-thumbnail")).await?;
            Ok(true)
        })
        .handle("avatar:getInventory", |ctx, args| async move {
            let query: Map<String, Value> = args.get(0)?;
            inventory(&ctx.state, query).await
        })
        .handle("avatar:getOutfitDetails", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            Ok(ctx.state.roblox.get::<Value>(&format!("{AVATAR}/v3/outfits/{id}/details")).await?)
        })
        .handle("avatar:getSavedAvatars", |ctx, args| async move {
            let user: u64 = args.get(0)?;
            let token: Option<String> = args.opt(1)?;
            let per_page: Option<u64> = args.opt(2)?;
            saved_avatars(&ctx.state, user, token.as_deref(), per_page).await
        })
        .handle("avatar:createSavedAvatarFromCurrent", |ctx, args| async move {
            let name: Option<String> = args.opt(0)?;
            let state = &ctx.state;
            let mut payload = current_outfit_payload(state).await?;
            if let Some(name) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
                payload["name"] = json!(name);
            }
            create_saved_avatar(state, &payload).await.map_err(|e| AppError::msg(e.message))
        })
        .handle("avatar:renameSavedAvatar", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let name: String = args.get(1)?;
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(AppError::msg("Name is required"));
            }
            let updated: Value = ctx.state.roblox.patch(&format!("{AVATAR}/v3/outfits/{id}"), &json!({ "name": trimmed })).await?;
            if !updated.get("id").is_some_and(Value::is_number) {
                return Err(AppError::msg("Failed to rename saved avatar"));
            }
            Ok(saved_summary(&updated, trimmed, true))
        })
        .handle("avatar:updateSavedAvatarFromCurrent", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let payload = current_outfit_payload(&ctx.state).await?;
            update_saved_avatar(&ctx.state, id, &payload).await.map_err(|e| AppError::msg(e.message))
        })
        .handle("avatar:deleteSavedAvatar", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            let r: Value = ctx.state.roblox.post_empty(&format!("{AVATAR}/v1/outfits/{id}/delete")).await?;
            let success = r.get("success").and_then(Value::as_bool).unwrap_or(false);
            if !success {
                tracing::warn!("Avatar: Delete saved avatar returned failure");
            }
            Ok(success)
        })
        .handle("avatar:equipSavedAvatar", |ctx, args| async move {
            let id: u64 = args.get(0)?;
            equip_saved_avatar(&ctx.state, id).await
        })
        .handle("avatar:getAssetThumbnails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size: Option<String> = args.opt(1)?;
            if ids.is_empty() {
                return Ok(json!([]));
            }
            let url = format!("{}/assets?assetIds={}&size={}&format=Webp", endpoints::THUMBNAILS_V1, join_ids(&ids), size.as_deref().unwrap_or("150x150"));
            Ok(thumbs(&ctx.state, url, &ids, "asset").await)
        })
        .handle("avatar:getOutfitThumbnails", |ctx, args| async move {
            let ids: Vec<u64> = args.get(0)?;
            let size: Option<String> = args.opt(1)?;
            if ids.is_empty() {
                return Ok(json!([]));
            }
            let url = format!(
                "{}/users/outfits?userOutfitIds={}&size={}&format=Webp",
                endpoints::THUMBNAILS_V1,
                join_ids(&ids),
                size.as_deref().unwrap_or("150x150")
            );
            Ok(thumbs(&ctx.state, url, &ids, "outfit").await)
        });
}
