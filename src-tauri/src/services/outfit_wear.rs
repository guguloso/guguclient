//! Wearing saved outfits (`outfits:wear`, `outfits:saveAsCharacter`) plus the
//! account-scoped writes the launch avatar uses. Port of `OutfitWearService`,
//! `OutfitPreparer`, `AvatarWriter`, `CharacterSaver`, `AccountOutfitWearer`,
//! `AccountAvatarModelWriter` and the try-on builders they share.

use std::collections::{HashMap, HashSet};

use futures::future::join_all;
use serde_json::{json, Map, Value};

use crate::error::AppResult;
use crate::ipc::Router;
use crate::services::avatar::{self, AvatarTarget, CharacterLookup, SavedAvatarError};
use crate::services::catalog::{self, ItemRef};
use crate::services::outfits;
use crate::state::AppState;

const BODY_PART_TYPES: [u64; 7] = [17, 27, 28, 29, 30, 31, 79];
const FACE_TYPE: u64 = 18;
const ANIMATION_TYPES: [u64; 12] = [24, 48, 49, 50, 51, 52, 53, 54, 55, 56, 61, 78];
const CLOTHING_TYPES: [u64; 3] = [2, 11, 12];
const RIGID_TYPES: [u64; 8] = [8, 41, 42, 43, 44, 45, 46, 47];
const LAYERED_TYPES: [u64; 9] = [64, 65, 66, 67, 68, 69, 70, 71, 72];
const FACE_ACCESSORY_TYPES: [u64; 2] = [76, 77];
const MAKEUP_TYPES: [u64; 3] = [88, 89, 90];
const MAKEUP_TOTAL_MAX: usize = 6;
const ANIMATION_BUNDLE_TYPE: u64 = 2;
const PREVIEWABLE_BUNDLE_TYPES: [u64; 4] = [1, 3, 4, 5];
const DEFAULT_ASSET_TYPE_MAX: usize = 1;
const SCALE_AXES: [&str; 6] = ["height", "width", "head", "depth", "proportion", "bodyType"];
const BODY_COLOR_PARTS: [(&str, &str); 6] = [
    ("headColorId", "headColor3"),
    ("torsoColorId", "torsoColor3"),
    ("leftArmColorId", "leftArmColor3"),
    ("rightArmColorId", "rightArmColor3"),
    ("leftLegColorId", "leftLegColor3"),
    ("rightLegColorId", "rightLegColor3"),
];
const CHARACTER_LIST_MAX_PAGES: usize = 20;

/// `WEB_EDITOR_LAYER_ORDERS`.
fn web_layer_order(type_id: u64) -> Option<u64> {
    Some(match type_id {
        76 => 0,
        77 => 1,
        70 | 71 => 5,
        66 => 6,
        69 => 7,
        72 => 8,
        64 => 9,
        65 => 10,
        68 => 11,
        67 => 12,
        41 => 13,
        _ => return None,
    })
}

fn is_previewable_type(t: u64) -> bool {
    BODY_PART_TYPES.contains(&t)
        || t == FACE_TYPE
        || CLOTHING_TYPES.contains(&t)
        || RIGID_TYPES.contains(&t)
        || LAYERED_TYPES.contains(&t)
        || FACE_ACCESSORY_TYPES.contains(&t)
        || MAKEUP_TYPES.contains(&t)
}

fn type_of(asset: &Value) -> u64 {
    asset.pointer("/assetType/id").and_then(Value::as_u64).unwrap_or(0)
}

fn id_of(v: &Value) -> Option<u64> {
    v.get("id").and_then(Value::as_u64)
}

fn assets_of(v: &Value) -> Vec<Value> {
    v.get("assets").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn item_key(item: &Value) -> String {
    format!("{}:{}", item.get("itemType").and_then(Value::as_str).unwrap_or(""), item.get("id").map(Value::to_string).unwrap_or_default())
}

fn to_item_ref(item: &Value) -> Value {
    json!({ "id": item.get("id"), "itemType": item.get("itemType") })
}

/// `canTryOnItem`: previewable as a wearable (not an animation).
fn can_try_on(details: &Value) -> bool {
    if details.get("itemType").and_then(Value::as_str) == Some("Bundle") {
        return match details.get("bundleType").and_then(Value::as_u64) {
            None => false,
            Some(ANIMATION_BUNDLE_TYPE) => false,
            Some(t) => PREVIEWABLE_BUNDLE_TYPES.contains(&t),
        };
    }
    match details.get("assetType").and_then(Value::as_u64) {
        None => false,
        Some(t) if ANIMATION_TYPES.contains(&t) => false,
        Some(t) => is_previewable_type(t),
    }
}

struct Wearable {
    id: u64,
    name: Value,
    asset_type: Option<u64>,
    bundle: Option<Vec<Value>>,
}

struct TryOn {
    wearable: Wearable,
    scales: Option<Value>,
    rig: Option<Value>,
}

#[derive(Default)]
struct Resolution {
    details: HashMap<String, Value>,
    contents: HashMap<u64, Value>,
}

/// `outfitWearables`.
fn outfit_wearables(items: &[Value], res: &Resolution) -> Vec<TryOn> {
    items
        .iter()
        .filter_map(|item| {
            let details = res.details.get(&item_key(item)).filter(|d| can_try_on(d))?;
            let id = id_of(details)?;
            let contents = res.contents.get(&id);
            let wearable = Wearable {
                id,
                name: details.get("name").cloned().unwrap_or(Value::Null),
                asset_type: if contents.is_some() { None } else { details.get("assetType").and_then(Value::as_u64) },
                bundle: contents.map(assets_of),
            };
            Some(TryOn {
                wearable,
                scales: contents.and_then(|c| c.get("scales")).filter(|s| !s.is_null()).cloned(),
                rig: contents.and_then(|c| c.get("playerAvatarType")).filter(|s| !s.is_null()).cloned(),
            })
        })
        .collect()
}

/// `findAssetTypeRule`.
fn asset_type_rule(rules: &Value, type_id: u64) -> (usize, String) {
    let rule = rules.get("wearableAssetTypes").and_then(Value::as_array).and_then(|r| r.iter().find(|e| e.get("id").and_then(Value::as_u64) == Some(type_id)));
    (
        rule.and_then(|r| r.get("maxNumber")).and_then(Value::as_u64).map_or(DEFAULT_ASSET_TYPE_MAX, |n| n as usize),
        rule.and_then(|r| r.get("name")).and_then(Value::as_str).unwrap_or("").to_owned(),
    )
}

/// `evictOverCapAssets`: drops the oldest asset of the first full slot.
fn evict_over_cap(assets: &mut Vec<Value>, rules: &Value, type_id: u64) {
    let same = |t: u64| t == type_id;
    let makeup = |t: u64| MAKEUP_TYPES.contains(&t);
    let mut competitions: Vec<(&dyn Fn(u64) -> bool, usize)> = vec![(&same, asset_type_rule(rules, type_id).0)];
    if MAKEUP_TYPES.contains(&type_id) {
        competitions.push((&makeup, MAKEUP_TOTAL_MAX));
    }
    let Some((competes, _)) = competitions.into_iter().find(|(c, max)| assets.iter().filter(|a| c(type_of(a))).count() >= *max) else {
        return;
    };
    if let Some(oldest) = assets.iter().position(|a| competes(type_of(a))) {
        assets.remove(oldest);
    }
}

/// `layerBundle`.
fn layer_bundle(assets: Vec<Value>, bundle: &[Value]) -> Vec<Value> {
    let ids: HashSet<u64> = bundle.iter().filter_map(id_of).collect();
    let types: HashSet<u64> = bundle.iter().map(type_of).filter(|t| *t != 0).collect();
    let mut next: Vec<Value> = assets.into_iter().filter(|a| !(id_of(a).is_some_and(|id| ids.contains(&id)) || types.contains(&type_of(a)))).collect();
    next.extend(bundle.iter().cloned());
    next
}

/// `buildTryOnAvatar` (without layer ordering).
fn build_try_on(base: &Value, rules: &Value, wearables: &[TryOn]) -> Value {
    let mut assets = assets_of(base);
    let mut scales = base.get("scales").filter(|s| !s.is_null()).cloned();
    let mut rig = base.get("playerAvatarType").filter(|s| !s.is_null()).cloned();
    for item in wearables {
        let w = &item.wearable;
        if let Some(bundle) = &w.bundle {
            assets = layer_bundle(assets, bundle);
        } else if !assets.iter().any(|a| id_of(a) == Some(w.id)) {
            let type_id = w.asset_type.unwrap_or(0);
            let (_, name) = asset_type_rule(rules, type_id);
            evict_over_cap(&mut assets, rules, type_id);
            assets.push(json!({ "id": w.id, "name": w.name, "assetType": { "id": type_id, "name": name } }));
        }
        if item.scales.is_some() {
            scales = item.scales.clone();
        }
        if item.rig.is_some() {
            rig = item.rig.clone();
        }
    }
    let mut avatar = base.clone();
    avatar["assets"] = Value::Array(assets);
    if let Some(s) = scales {
        avatar["scales"] = s;
    }
    if let Some(r) = rig {
        avatar["playerAvatarType"] = r;
    }
    avatar
}

/// `fillOutfitLayerOrders`.
fn fill_layer_orders(assets: Vec<Value>) -> Vec<Value> {
    let order_of = |a: &Value| a.pointer("/meta/order").and_then(Value::as_f64);
    let mut next_makeup = assets
        .iter()
        .filter(|a| MAKEUP_TYPES.contains(&type_of(a)))
        .filter_map(order_of)
        .fold(None, |m: Option<f64>, o| Some(m.map_or(o, |m| m.max(o))))
        .map_or(0.0, |m| m + 1.0);
    assets
        .into_iter()
        .map(|mut a| {
            if order_of(&a).is_some() || a.pointer("/meta/order").is_some_and(|o| !o.is_null()) {
                return a;
            }
            let order = if MAKEUP_TYPES.contains(&type_of(&a)) {
                let o = next_makeup;
                next_makeup += 1.0;
                json!(o as u64)
            } else {
                match web_layer_order(type_of(&a)) {
                    Some(o) => json!(o),
                    None => return a,
                }
            };
            let mut meta = a.get("meta").and_then(Value::as_object).cloned().unwrap_or_default();
            meta.insert("order".into(), order);
            a["meta"] = Value::Object(meta);
            a
        })
        .collect()
}

fn merge_objects(base: Option<&Value>, over: &Value) -> Value {
    let mut merged = base.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(o) = over.as_object() {
        for (k, v) in o {
            merged.insert(k.clone(), v.clone());
        }
    }
    Value::Object(merged)
}

/// `buildOutfitPreview`.
fn build_outfit_preview(base: &Value, body: Option<&Value>, apply_to_avatar: bool, rules: &Value, wearables: &[TryOn]) -> Value {
    let start = if apply_to_avatar {
        base.clone()
    } else {
        let mut stripped = base.clone();
        stripped["assets"] = Value::Array(assets_of(base).into_iter().filter(|a| BODY_PART_TYPES.contains(&type_of(a)) || type_of(a) == FACE_TYPE).collect());
        stripped
    };
    let mut avatar = build_try_on(&start, rules, wearables);
    avatar["assets"] = Value::Array(fill_layer_orders(assets_of(&avatar)));
    if let Some(body) = body {
        if let Some(colors) = body.get("bodyColors").filter(|c| c.is_object()) {
            avatar["bodyColors"] = merge_objects(avatar.get("bodyColors"), colors);
        }
        if let Some(scales) = body.get("scales").filter(|c| c.is_object()) {
            avatar["scales"] = merge_objects(avatar.get("scales"), scales);
        }
    }
    avatar
}

/// `mergeBodyColors`: null when any part lacks an id.
fn merge_body_colors(base: Option<&Value>, over: Option<&Value>) -> Option<Value> {
    let merged = merge_objects(base, over.unwrap_or(&Value::Null));
    let mut model = Map::new();
    for (field, _) in BODY_COLOR_PARTS {
        let v = merged.get(field).filter(|v| v.is_number())?;
        model.insert(field.into(), v.clone());
    }
    Some(Value::Object(model))
}

/// `mergeScales`: null when any axis is missing.
fn merge_scales(base: Option<&Value>, next: Option<&Value>) -> Option<Value> {
    let merged = merge_objects(base, next.unwrap_or(&Value::Null));
    let mut model = Map::new();
    for axis in SCALE_AXES {
        let v = merged.get(axis).filter(|v| v.is_number())?;
        model.insert(axis.into(), v.clone());
    }
    Some(Value::Object(model))
}

fn scales_differ(next: &Value, current: Option<&Value>) -> bool {
    SCALE_AXES.iter().any(|axis| next.get(*axis).and_then(Value::as_f64) != current.and_then(|c| c.get(*axis)).and_then(Value::as_f64))
}

enum Op {
    Rig(Value),
    Colors(Value),
    Scales(Value),
    Assets(Vec<Value>),
}

struct Stage {
    name: &'static str,
    run: Option<Op>,
    restore: Option<Op>,
}

async fn exec(state: &AppState, target: &AvatarTarget, op: &Op) -> AppResult<bool> {
    match op {
        Op::Rig(rig) => target.set_player_avatar_type(state, rig).await,
        Op::Colors(c) => target.set_body_colors(state, c).await,
        Op::Scales(s) => target.set_scales(state, s).await,
        Op::Assets(a) => {
            let r = target.set_wearing_assets(state, a).await?;
            Ok(r["success"].as_bool().unwrap_or(false) && r.get("invalidAssetIds").and_then(Value::as_array).is_none_or(Vec::is_empty))
        }
    }
}

enum Applied {
    /// A refused stage name, or `signedOut`.
    Outcome(&'static str),
    Done { assets_result: Value, sent: Vec<Stage> },
}

/// `AvatarWriter.restore`: undoes the sent stages in reverse.
async fn restore(state: &AppState, target: &AvatarTarget, sent: &[Stage]) {
    for stage in sent.iter().rev() {
        if !target.is_current(state) {
            return;
        }
        let Some(op) = &stage.restore else { continue };
        match exec(state, target, op).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("Restoring the {} stage was refused", stage.name),
            Err(error) => tracing::warn!("Restoring the {} stage failed: {error}", stage.name),
        }
    }
}

/// `AvatarWriter.bodyStages`.
fn body_stages(base: &Value, built: &Value, override_body: Option<&Value>) -> Vec<Stage> {
    let mut stages = Vec::new();
    let base_rig = base.get("playerAvatarType").filter(|v| !v.is_null());
    if let Some(rig) = built.get("playerAvatarType").filter(|v| !v.is_null()) {
        if Some(rig) != base_rig {
            stages.push(Stage { name: "playerAvatarType", run: Some(Op::Rig(rig.clone())), restore: base_rig.map(|r| Op::Rig(r.clone())) });
        }
    }
    let base_colors = base.get("bodyColors").filter(|v| !v.is_null());
    let colors = override_body.and_then(|o| o.get("bodyColors")).filter(|c| !c.is_null()).and_then(|c| merge_body_colors(base_colors, Some(c)));
    if let Some(colors) = colors {
        stages.push(Stage { name: "bodyColors", run: Some(Op::Colors(colors)), restore: merge_body_colors(base_colors, None).map(Op::Colors) });
    }
    let base_scales = base.get("scales").filter(|v| !v.is_null());
    let scale = built.get("scales").filter(|s| !s.is_null() && scales_differ(s, base_scales)).and_then(|s| merge_scales(base_scales, Some(s)));
    if let Some(scale) = scale {
        stages.push(Stage { name: "scales", run: Some(Op::Scales(scale)), restore: merge_scales(base_scales, None).map(Op::Scales) });
    }
    stages
}

/// `AvatarWriter.apply`: body stages, then the asset list; an error is
/// raised only after the sent stages were restored.
async fn apply(state: &AppState, target: &AvatarTarget, base: &Value, built: &Value, override_body: Option<&Value>) -> AppResult<Applied> {
    let mut applied: Vec<Stage> = Vec::new();
    for stage in body_stages(base, built, override_body) {
        if !target.is_current(state) {
            return Ok(Applied::Outcome("signedOut"));
        }
        let result = exec(state, target, stage.run.as_ref().expect("body stages always run")).await;
        let name = stage.name;
        applied.push(stage);
        match result {
            Ok(true) => {}
            Ok(false) => {
                restore(state, target, &applied).await;
                return Ok(Applied::Outcome(name));
            }
            Err(error) => {
                restore(state, target, &applied).await;
                return Err(error);
            }
        }
    }
    if !target.is_current(state) {
        return Ok(Applied::Outcome("signedOut"));
    }
    let base_assets = base.get("assets").and_then(Value::as_array).cloned();
    applied.push(Stage { name: "assets", run: None, restore: base_assets.map(Op::Assets) });
    match target.set_wearing_assets(state, &assets_of(built)).await {
        Ok(assets_result) => Ok(Applied::Done { assets_result, sent: applied }),
        Err(error) => {
            restore(state, target, &applied).await;
            Err(error)
        }
    }
}

struct Prepared {
    base: Value,
    outfit: Map<String, Value>,
    palette: Vec<Value>,
    resolution: Resolution,
    retryable: Vec<Value>,
    rules: Value,
    skipped: Vec<Value>,
    unresolved: Vec<Value>,
    user_id: u64,
}

impl Prepared {
    fn items(&self) -> Vec<Value> {
        self.outfit.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
    }
    fn body(&self) -> Option<&Value> {
        self.outfit.get("body").filter(|b| b.is_object())
    }
}

/// `OutfitPreparer.resolveItems`.
async fn resolve_items(state: &AppState, items: &[Value], user: u64) -> (Resolution, Vec<Value>, Vec<Value>) {
    let refs: Vec<ItemRef> = items
        .iter()
        .filter_map(|i| Some(ItemRef { id: id_of(i)?, item_type: i.get("itemType")?.as_str()?.to_owned() }))
        .collect();
    let (rich, not_found) = catalog::lookup_items_details_rich(state, refs, Some(user)).await;
    let mut details: HashMap<String, Value> = rich.iter().map(|d| (item_key(d), d.clone())).collect();
    let gone: HashSet<String> = not_found.iter().map(ItemRef::key).collect();
    let bundles: Vec<u64> = rich.iter().filter(|d| d.get("itemType").and_then(Value::as_str) == Some("Bundle")).filter_map(id_of).collect();
    let expanded = join_all(bundles.iter().map(|id| async move {
        let contents = catalog::bundle_contents(state, *id).await;
        if contents.is_none() {
            tracing::warn!("Could not expand bundle {id} for an outfit");
        }
        contents
    }))
    .await;
    let contents: HashMap<u64, Value> = expanded.into_iter().flatten().filter_map(|c| Some((c.get("bundleId")?.as_u64()?, c))).collect();
    let mut unresolved = Vec::new();
    let mut retryable = Vec::new();
    for item in items {
        let key = item_key(item);
        let missing_members = item.get("itemType").and_then(Value::as_str) == Some("Bundle") && !id_of(item).is_some_and(|id| contents.contains_key(&id));
        if details.contains_key(&key) && !missing_members {
            continue;
        }
        unresolved.push(to_item_ref(item));
        if !gone.contains(&key) {
            retryable.push(to_item_ref(item));
        }
    }
    for r in &unresolved {
        details.remove(&item_key(r));
    }
    (Resolution { details, contents }, retryable, unresolved)
}

/// `OutfitPreparer.prepare`.
async fn prepare(state: &AppState, outfit_id: &str, target: &AvatarTarget) -> Result<Prepared, &'static str> {
    let user = target.user_id;
    let outfit = outfits::find_for_user(state, user, outfit_id).ok_or("outfitMissing")?;
    let (base, rules) = futures::join!(target.get_avatar(state), target.get_rules(state));
    let (base, rules) = match (base, rules) {
        (Ok(b), Ok(r)) => (b, r),
        (Err(error), _) | (_, Err(error)) => {
            tracing::error!("Could not read the avatar of {user} before wearing outfit {outfit_id}: {error}");
            return Err("avatarUnavailable");
        }
    };
    let items = outfit.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let (resolution, retryable, unresolved) = resolve_items(state, &items, user).await;
    if !target.is_current(state) {
        return Err("signedOut");
    }
    let skipped = items
        .iter()
        .filter(|i| resolution.details.get(&item_key(i)).is_some_and(|d| !can_try_on(d)))
        .map(to_item_ref)
        .collect();
    Ok(Prepared {
        palette: rules.get("bodyColorsPalette").and_then(Value::as_array).cloned().unwrap_or_default(),
        rules: json!({ "playerAvatarTypes": rules.get("playerAvatarTypes"), "wearableAssetTypes": rules.get("wearableAssetTypes") }),
        base,
        outfit,
        resolution,
        retryable,
        skipped,
        unresolved,
        user_id: user,
    })
}

/// `itemsBackedByAssets`.
fn items_backed_by(asset_ids: &[u64], items: &[Value], res: &Resolution) -> Vec<Value> {
    if asset_ids.is_empty() {
        return Vec::new();
    }
    let ids: HashSet<u64> = asset_ids.iter().copied().collect();
    items
        .iter()
        .filter(|item| {
            let id = id_of(item);
            if item.get("itemType").and_then(Value::as_str) == Some("Asset") {
                id.is_some_and(|id| ids.contains(&id))
            } else {
                id.and_then(|id| res.contents.get(&id)).is_some_and(|c| assets_of(c).iter().any(|a| id_of(a).is_some_and(|i| ids.contains(&i))))
            }
        })
        .map(to_item_ref)
        .collect()
}

/// `partitionOwnedWearables`.
fn partition_owned(items: &[Value], res: &Resolution) -> (Vec<Value>, Vec<TryOn>) {
    let mut unowned = Vec::new();
    let owned: Vec<Value> = items
        .iter()
        .filter(|item| {
            let unowned_wearable = res.details.get(&item_key(item)).is_some_and(|d| d.get("owned") == Some(&json!(false)) && can_try_on(d));
            if unowned_wearable {
                unowned.push(to_item_ref(item));
            }
            !unowned_wearable
        })
        .cloned()
        .collect();
    let wearables = outfit_wearables(&owned, res);
    (unowned, wearables)
}

/// `ownershipFromDetails`.
fn ownership_from_details(items: &[Value], res: &Resolution) -> HashMap<String, bool> {
    items.iter().filter_map(|i| Some((item_key(i), res.details.get(&item_key(i))?.get("owned")?.as_bool()?))).collect()
}

struct WearSummary {
    applied: Vec<Value>,
    displaced: Vec<Value>,
    invalid: Vec<Value>,
    rejected: Vec<Value>,
    worn_count: i64,
}

/// `summarizeAppliedWear`.
fn summarize(assets_result: &Value, built: &Value, items: &[Value], res: &Resolution, wearables: &[TryOn]) -> WearSummary {
    let invalid: Vec<Value> = assets_result.get("invalidAssetIds").and_then(Value::as_array).cloned().unwrap_or_default();
    let invalid_ids: Vec<u64> = invalid.iter().filter_map(Value::as_u64).collect();
    let built_ids: Vec<u64> = assets_of(built).iter().filter_map(id_of).collect();
    let applied = items_backed_by(&built_ids, items, res);
    let rejected = items_backed_by(&invalid_ids, items, res);
    let applied_keys: HashSet<String> = applied.iter().map(item_key).collect();
    let worn_ids: Vec<u64> = wearables
        .iter()
        .flat_map(|w| match &w.wearable.bundle {
            Some(b) => b.iter().filter_map(id_of).collect::<Vec<_>>(),
            None => vec![w.wearable.id],
        })
        .collect();
    let displaced = items_backed_by(&worn_ids, items, res).into_iter().filter(|i| !applied_keys.contains(&item_key(i))).collect();
    WearSummary { worn_count: applied.len() as i64 - rejected.len() as i64, applied, displaced, invalid, rejected }
}

/// `describeOwnership`.
fn describe_ownership(applied: &[Value], items: &[Value], rejected: &[Value], res: &Resolution) -> HashMap<String, bool> {
    let applied: HashSet<String> = applied.iter().map(item_key).collect();
    let rejected: HashSet<String> = rejected.iter().map(item_key).collect();
    let mut ownership = HashMap::new();
    for item in items {
        let key = item_key(item);
        if rejected.contains(&key) {
            ownership.insert(key, false);
        } else if applied.contains(&key) {
            ownership.insert(key, true);
        } else if let Some(owned) = res.details.get(&key).and_then(|d| d.get("owned")).and_then(Value::as_bool) {
            ownership.insert(key, owned);
        }
    }
    ownership
}

fn refuse_wear(state: &AppState, error: &str, extra: Value) -> Value {
    let mut out = json!({ "ok": false, "document": outfits::list(state), "error": error });
    if let (Some(o), Some(extra)) = (out.as_object_mut(), extra.as_object()) {
        o.extend(extra.clone());
    }
    out
}

/// `OutfitWearService.wear`.
async fn wear(state: &AppState, outfit_id: &str, mode: &Value, source: &Value) -> Value {
    let Some(target) = AvatarTarget::active(state) else { return refuse_wear(state, "signedOut", Value::Null) };
    let prepared = match prepare(state, outfit_id, &target).await {
        Ok(p) => p,
        Err(error) => return refuse_wear(state, error, Value::Null),
    };
    let items = prepared.items();
    let res = &prepared.resolution;
    let track_failure = |failed: Option<&str>| {
        let mut payload = json!({
            "item_count": items.len(), "mode": mode, "outcome": "failed", "outfit_id": outfit_id, "rejected_count": 0,
            "skipped_count": prepared.skipped.len(), "source": source, "unresolved_count": prepared.unresolved.len(),
        });
        if let Some(stage) = failed {
            payload["failed_stage"] = json!(stage);
        }
        state.track("marketplace.outfit.wear", payload);
    };
    if !prepared.retryable.is_empty() {
        track_failure(None);
        return refuse_wear(state, "itemsUnavailable", Value::Null);
    }
    if outfit_wearables(&items, res).is_empty() {
        track_failure(None);
        return refuse_wear(state, "nothingToWear", Value::Null);
    }
    let (unowned, _) = partition_owned(&items, res);
    if !unowned.is_empty() {
        outfits::record_ownership(state, outfit_id, &ownership_from_details(&items, res));
        track_failure(None);
        return refuse_wear(state, "unownedItems", json!({ "unownedItems": unowned }));
    }
    let wearables = outfit_wearables(&items, res);
    let built = build_outfit_preview(&prepared.base, prepared.body(), mode.as_str() == Some("layer"), &prepared.rules, &wearables);
    let (assets_result, sent) = match apply(state, &target, &prepared.base, &built, prepared.body()).await {
        Ok(Applied::Done { assets_result, sent }) => (assets_result, sent),
        Ok(Applied::Outcome("signedOut")) => {
            track_failure(None);
            return refuse_wear(state, "signedOut", Value::Null);
        }
        Ok(Applied::Outcome(stage)) => {
            track_failure(Some(stage));
            return refuse_wear(state, "stageFailed", json!({ "failedStage": stage }));
        }
        Err(error) => {
            tracing::error!("Wearing outfit {outfit_id} failed: {error}");
            track_failure(None);
            return refuse_wear(state, "requestFailed", Value::Null);
        }
    };
    let summary = summarize(&assets_result, &built, &items, res, &wearables);
    if !assets_result["success"].as_bool().unwrap_or(false) && summary.invalid.is_empty() {
        track_failure(Some("assets"));
        return refuse_wear(state, "stageFailed", json!({ "failedStage": "assets" }));
    }
    let ownership = describe_ownership(&summary.applied, &items, &summary.rejected, res);
    let same_user = target.is_current(state);
    if summary.worn_count <= 0 {
        if same_user {
            outfits::record_ownership(state, outfit_id, &ownership);
        }
        restore(state, &target, &sent).await;
        track_failure(Some("assets"));
        return refuse_wear(state, "stageFailed", json!({ "failedStage": "assets", "invalidAssetIds": summary.invalid }));
    }
    let document = if same_user { outfits::record_wear(state, outfit_id, &ownership) } else { outfits::list(state) };
    state.track(
        "marketplace.outfit.wear",
        json!({
            "item_count": items.len(), "mode": mode, "outcome": if summary.rejected.is_empty() { "success" } else { "partial" },
            "outfit_id": outfit_id, "rejected_count": summary.rejected.len(), "skipped_count": prepared.skipped.len(),
            "source": source, "unresolved_count": prepared.unresolved.len(),
        }),
    );
    json!({
        "ok": true,
        "document": document,
        "summary": {
            "displacedItems": summary.displaced, "invalidAssetIds": summary.invalid, "rejectedItems": summary.rejected,
            "skippedItems": prepared.skipped, "unresolvedItems": prepared.unresolved, "wornCount": summary.worn_count,
        },
    })
}

/// `toBodyColor3s`.
fn to_body_color3s(ids: &Value, palette: &[Value], fallback: Option<&Value>) -> Option<Value> {
    let hex_by_id: HashMap<u64, String> = palette
        .iter()
        .filter_map(|e| Some((e.get("brickColorId")?.as_u64()?, e.get("hexColor")?.as_str()?.trim_start_matches('#').to_owned())))
        .collect();
    let mut model = Map::new();
    for (id_field, hex_field) in BODY_COLOR_PARTS {
        let hex = ids
            .get(id_field)
            .and_then(Value::as_u64)
            .and_then(|id| hex_by_id.get(&id).cloned().map(Value::String))
            .or_else(|| fallback.and_then(|f| f.get(hex_field)).filter(|v| !v.is_null()).cloned())?;
        model.insert(hex_field.into(), hex);
    }
    Some(Value::Object(model))
}

/// `toCharacterAssets`.
fn to_character_assets(assets: &[Value]) -> Vec<Value> {
    assets
        .iter()
        .map(|a| {
            let t = type_of(a);
            let layered = LAYERED_TYPES.contains(&t);
            let meta = a.get("meta").filter(|m| m.is_object());
            if meta.is_none() && !layered && !FACE_ACCESSORY_TYPES.contains(&t) && !MAKEUP_TYPES.contains(&t) {
                return json!({ "id": a.get("id") });
            }
            let required = if layered { json!({ "puffiness": 1, "version": 1 }) } else { json!({ "version": 1 }) };
            json!({ "id": a.get("id"), "meta": merge_objects(Some(&required), meta.unwrap_or(&Value::Null)) })
        })
        .collect()
}

fn rig_name(rig: Option<&Value>) -> Option<&'static str> {
    match rig.and_then(Value::as_u64) {
        Some(1) => Some("R6"),
        Some(3) => Some("R15"),
        _ => None,
    }
}

/// `classifySaveError`.
fn classify_save_error(error: &SavedAvatarError, operation: &str) -> &'static str {
    let Some(status) = error.status else { return "requestFailed" };
    if operation == "create" && status == 403 && error.code == Some(1) {
        return "characterCapReached";
    }
    if operation == "update" && (status == 404 || (status == 403 && error.code == Some(2))) {
        return "updateTargetMissing";
    }
    "requestFailed"
}

/// `CharacterSaver.save`.
async fn save_as_character(state: &AppState, outfit_id: &str, mode: &Value, source: &Value) -> Value {
    let refuse = |error: &str| json!({ "ok": false, "document": outfits::list(state), "error": error });
    let Some(target) = AvatarTarget::active(state) else { return refuse("signedOut") };
    let prepared = match prepare(state, outfit_id, &target).await {
        Ok(p) => p,
        Err(error) => return refuse(error),
    };
    let items = prepared.items();
    let res = &prepared.resolution;
    let track_failure = |error: &str, skipped: usize| {
        state.track(
            "marketplace.outfit.save_as_character",
            json!({ "error": error, "item_count": items.len(), "mode": mode, "outcome": "failed", "outfit_id": outfit_id, "skipped_unowned_count": skipped, "source": source }),
        );
    };
    let target_id = prepared.outfit.get("robloxOutfitId").and_then(Value::as_u64);
    let update = mode.as_str() == Some("update");
    if update && target_id.is_none() {
        track_failure("updateTargetMissing", 0);
        return refuse("updateTargetMissing");
    }
    if !prepared.retryable.is_empty() {
        track_failure("itemsUnavailable", 0);
        return refuse("itemsUnavailable");
    }
    let (unowned, wearables) = partition_owned(&items, res);
    if wearables.is_empty() {
        track_failure("nothingToSave", unowned.len());
        return refuse("nothingToSave");
    }
    let built = build_outfit_preview(&prepared.base, prepared.body(), false, &prepared.rules, &wearables);
    let base_scales = prepared.base.get("scales").filter(|v| !v.is_null());
    let scale = merge_scales(base_scales, built.get("scales").filter(|v| !v.is_null()));
    let ids = merge_objects(prepared.base.get("bodyColors"), prepared.body().and_then(|b| b.get("bodyColors")).unwrap_or(&Value::Null));
    let body_color3s = match to_body_color3s(&ids, &prepared.palette, None) {
        Some(c) => Some(c),
        None => {
            let fallback = match target.get_body_color3s(state).await {
                Ok(f) => Some(f),
                Err(error) => {
                    tracing::warn!("Could not read the current body colors; the palette alone names them: {error}");
                    None
                }
            };
            to_body_color3s(&ids, &prepared.palette, fallback.as_ref())
        }
    };
    let Some(body_color3s) = body_color3s else {
        track_failure("avatarUnavailable", unowned.len());
        return refuse("avatarUnavailable");
    };
    let mut payload = json!({ "assets": to_character_assets(&assets_of(&built)), "bodyColor3s": body_color3s, "name": prepared.outfit.get("name") });
    let rig = built.get("playerAvatarType").filter(|v| !v.is_null()).or(prepared.base.get("playerAvatarType"));
    if let Some(name) = rig_name(rig) {
        payload["playerAvatarType"] = json!(name);
    }
    if let Some(scale) = scale {
        payload["scale"] = scale;
    }
    let is_current = || state.auth.current_user_id() == Some(prepared.user_id);
    if !is_current() {
        track_failure("signedOut", unowned.len());
        return refuse("signedOut");
    }
    let (operation, result) = match target_id.filter(|_| update) {
        Some(id) => ("update", avatar::update_saved_avatar(state, id, &payload).await),
        None => ("create", avatar::create_saved_avatar(state, &payload).await),
    };
    match result {
        Ok(character) => {
            let character_id = id_of(&character);
            if !is_current() {
                return json!({ "ok": true, "character": character, "document": outfits::list(state), "mode": mode, "skippedUnownedItems": unowned, "unresolvedItems": prepared.unresolved });
            }
            let document = outfits::link_roblox_outfit(state, outfit_id, character_id);
            state.track(
                "marketplace.outfit.save_as_character",
                json!({ "item_count": items.len(), "mode": mode, "outcome": "success", "outfit_id": outfit_id, "roblox_outfit_id": character_id, "skipped_unowned_count": unowned.len(), "source": source }),
            );
            json!({ "ok": true, "character": character, "document": document, "mode": mode, "skippedUnownedItems": unowned, "unresolvedItems": prepared.unresolved })
        }
        Err(error) => {
            let kind = classify_save_error(&error, operation);
            tracing::warn!("Saving outfit {outfit_id} as a character was refused: {}", error.message);
            let document = if kind == "updateTargetMissing" && is_current() { outfits::link_roblox_outfit(state, outfit_id, None) } else { outfits::list(state) };
            track_failure(kind, unowned.len());
            json!({ "ok": false, "document": document, "error": kind })
        }
    }
}

/// `AccountOutfitWearer.wear` (`wearForAccount`).
pub async fn wear_for_account(state: &AppState, outfit_id: &str, user: u64) -> Value {
    let fail = |error: &str| json!({ "ok": false, "error": error });
    let Some(target) = AvatarTarget::for_account(state, user) else { return fail("signedOut") };
    let prepared = match prepare(state, outfit_id, &target).await {
        Ok(p) => p,
        Err(error) => return fail(error),
    };
    if !prepared.retryable.is_empty() {
        return fail("itemsUnavailable");
    }
    let items = prepared.items();
    let res = &prepared.resolution;
    let (unowned, wearables) = partition_owned(&items, res);
    if wearables.is_empty() {
        return fail("nothingToWear");
    }
    let built = build_outfit_preview(&prepared.base, prepared.body(), false, &prepared.rules, &wearables);
    let (assets_result, sent) = match apply(state, &target, &prepared.base, &built, prepared.body()).await {
        Ok(Applied::Done { assets_result, sent }) => (assets_result, sent),
        Ok(Applied::Outcome("signedOut")) => return fail("signedOut"),
        Ok(Applied::Outcome(stage)) => return json!({ "ok": false, "error": "stageFailed", "failedStage": stage }),
        Err(error) => {
            tracing::error!("Wearing outfit {outfit_id} on account {user} failed: {error}");
            return fail("requestFailed");
        }
    };
    let summary = summarize(&assets_result, &built, &items, res, &wearables);
    if !assets_result["success"].as_bool().unwrap_or(false) && summary.invalid.is_empty() {
        return json!({ "ok": false, "error": "stageFailed", "failedStage": "assets" });
    }
    if summary.worn_count <= 0 {
        restore(state, &target, &sent).await;
        return json!({ "ok": false, "error": "stageFailed", "failedStage": "assets", "invalidAssetIds": summary.invalid });
    }
    json!({
        "ok": true,
        "summary": {
            "displacedItems": summary.displaced, "invalidAssetIds": summary.invalid, "rejectedItems": summary.rejected,
            "skippedItems": prepared.skipped, "unresolvedItems": prepared.unresolved, "wornCount": summary.worn_count,
        },
        "unownedItems": unowned,
    })
}

/// `AccountAvatarModelWriter.write`.
async fn write_model(state: &AppState, target: &AvatarTarget, model: &Value, body_color3s: Option<&Value>, label: &str) -> Value {
    let user = target.user_id;
    let base = match target.get_avatar(state).await {
        Ok(b) => b,
        Err(error) => {
            tracing::error!("Could not read the avatar of {user} before writing {label}: {error}");
            return json!({ "ok": false, "error": "avatarUnavailable" });
        }
    };
    let override_body = match (body_color3s, model.get("bodyColors").filter(|c| !c.is_null())) {
        (None, Some(colors)) => Some(json!({ "bodyColors": colors })),
        _ => None,
    };
    let assets_result = match apply(state, target, &base, model, override_body.as_ref()).await {
        Ok(Applied::Done { assets_result, .. }) => assets_result,
        Ok(Applied::Outcome("signedOut")) => return json!({ "ok": false, "error": "signedOut" }),
        Ok(Applied::Outcome(stage)) => return json!({ "ok": false, "error": "stageFailed", "failedStage": stage }),
        Err(error) => {
            tracing::error!("Writing {label} to account {user} failed: {error}");
            return json!({ "ok": false, "error": "requestFailed" });
        }
    };
    let invalid = assets_result.get("invalidAssetIds").and_then(Value::as_array).map_or(0, Vec::len);
    if !assets_result["success"].as_bool().unwrap_or(false) && invalid == 0 {
        return json!({ "ok": false, "error": "stageFailed", "failedStage": "assets" });
    }
    if invalid > 0 {
        tracing::warn!("Roblox refused {invalid} asset(s) of {label} on account {user}; kept the rest");
    }
    if let Some(colors) = body_color3s {
        let colored = match target.set_body_color3s(state, colors).await {
            Ok(ok) => ok,
            Err(error) => {
                tracing::warn!("Setting the body colors of {label} on account {user} failed: {error}");
                false
            }
        };
        if !colored {
            return json!({ "ok": false, "error": "stageFailed", "failedStage": "bodyColors" });
        }
    }
    json!({ "ok": true })
}

/// `ownsCharacter`: `None` when the list could not be read.
async fn owns_character(state: &AppState, user: u64, outfit: u64) -> Option<bool> {
    let mut token: Option<String> = None;
    for _ in 0..CHARACTER_LIST_MAX_PAGES {
        let page = match avatar::saved_avatars(state, user, token.as_deref(), None).await {
            Ok(p) => p,
            Err(error) => {
                tracing::warn!("Could not list the characters of account {user}: {error}");
                return None;
            }
        };
        if page["data"].as_array().is_some_and(|d| d.iter().any(|c| id_of(c) == Some(outfit))) {
            return Some(true);
        }
        token = page.get("paginationToken").and_then(Value::as_str).map(str::to_owned);
        if token.is_none() {
            return Some(false);
        }
    }
    None
}

/// `wearCharacterForAccount`.
pub async fn wear_character_for_account(state: &AppState, roblox_outfit: u64, user: u64) -> Value {
    let Some(target) = AvatarTarget::for_account(state, user) else { return json!({ "ok": false, "error": "signedOut" }) };
    match owns_character(state, user, roblox_outfit).await {
        Some(false) => return json!({ "ok": false, "error": "characterMissing" }),
        None => return json!({ "ok": false, "error": "requestFailed" }),
        Some(true) => {}
    }
    let character = match target.get_character(state, roblox_outfit).await {
        CharacterLookup::Found(c) => c,
        CharacterLookup::Missing => return json!({ "ok": false, "error": "characterMissing" }),
        CharacterLookup::Failed => return json!({ "ok": false, "error": "requestFailed" }),
    };
    let assets: Vec<Value> = assets_of(&character)
        .iter()
        .map(|a| {
            let mut out = json!({ "assetType": a.get("assetType"), "id": a.get("id"), "name": a.get("name") });
            if let Some(meta) = a.get("meta").filter(|m| !m.is_null()) {
                out["meta"] = meta.clone();
            }
            out
        })
        .collect();
    let mut model = json!({ "assets": assets });
    match character.get("playerAvatarType").and_then(Value::as_str) {
        Some("R6") => model["playerAvatarType"] = json!(1),
        Some("R15") => model["playerAvatarType"] = json!(3),
        _ => {}
    }
    if let Some(scale) = character.get("scale").filter(|s| !s.is_null()) {
        model["scales"] = scale.clone();
    }
    let colors = character.get("bodyColor3s").filter(|c| !c.is_null());
    write_model(state, &target, &model, colors, &format!("character {roblox_outfit}")).await
}

/// `restoreForAccount`: puts a `{ avatar, bodyColor3s }` snapshot back.
pub async fn restore_for_account(state: &AppState, snapshot: &Value, user: u64) -> Value {
    let Some(target) = AvatarTarget::for_account(state, user) else { return json!({ "ok": false, "error": "signedOut" }) };
    let model = snapshot.get("avatar").cloned().unwrap_or(Value::Null);
    let colors = snapshot.get("bodyColor3s").filter(|c| !c.is_null());
    write_model(state, &target, &model, colors, "a snapshot").await
}

pub fn register(router: &mut Router) {
    router
        .handle("outfits:wear", |ctx, args| async move {
            let id: String = args.get(0)?;
            let options: Value = args.opt(1)?.unwrap_or(Value::Null);
            Ok(wear(&ctx.state, &id, options.get("mode").unwrap_or(&Value::Null), options.get("source").unwrap_or(&Value::Null)).await)
        })
        .handle("outfits:saveAsCharacter", |ctx, args| async move {
            let id: String = args.get(0)?;
            let options: Value = args.opt(1)?.unwrap_or(Value::Null);
            Ok(save_as_character(&ctx.state, &id, options.get("mode").unwrap_or(&Value::Null), options.get("source").unwrap_or(&Value::Null)).await)
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_orders_follow_web_editor_defaults() {
        let assets = vec![
            json!({ "id": 1, "assetType": { "id": 67 } }),
            json!({ "id": 2, "assetType": { "id": 88 } }),
            json!({ "id": 3, "assetType": { "id": 89 }, "meta": { "order": 4 } }),
            json!({ "id": 4, "assetType": { "id": 8 } }),
        ];
        let out = fill_layer_orders(assets);
        assert_eq!(out[0]["meta"]["order"], 12);
        assert_eq!(out[1]["meta"]["order"], 5);
        assert_eq!(out[2]["meta"]["order"], 4);
        assert!(out[3].get("meta").is_none());
    }

    #[test]
    fn evicts_oldest_over_cap() {
        let mut assets = vec![json!({ "id": 1, "assetType": { "id": 8 } }), json!({ "id": 2, "assetType": { "id": 41 } })];
        evict_over_cap(&mut assets, &json!({ "wearableAssetTypes": [{ "id": 8, "maxNumber": 1 }] }), 8);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["id"], 2);
    }

    #[test]
    fn body_color3s_from_palette() {
        let ids = json!({ "headColorId": 1, "torsoColorId": 1, "leftArmColorId": 1, "rightArmColorId": 1, "leftLegColorId": 1, "rightLegColorId": 2 });
        let palette = vec![json!({ "brickColorId": 1, "hexColor": "#FFFFFF" })];
        assert!(to_body_color3s(&ids, &palette, None).is_none());
        let fallback = json!({ "rightLegColor3": "000000" });
        let model = to_body_color3s(&ids, &palette, Some(&fallback)).unwrap();
        assert_eq!(model["headColor3"], "FFFFFF");
        assert_eq!(model["rightLegColor3"], "000000");
    }
}
