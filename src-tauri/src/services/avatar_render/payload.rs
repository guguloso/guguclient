//! Asset payload builders (`parsePayload` and friends) and the client
//! defaults read from the installed Roblox content folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use futures::future::{join_all, BoxFuture, FutureExt};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use super::delivery::{AssetBytes, AssetDelivery, AssetRequest};
use super::extract::{self as ex, PartExtract, Sequence};
use super::mesh::{parse_file_mesh, Mesh};
use super::rbxm::{parse_rbxm, CFrame, Document};
use crate::services::roblox_layout::product_paths;
use crate::state::AppState;

const IMAGE_ASSET_TYPE_ID: i64 = 1;
const MESH_ASSET_TYPE_ID: i64 = 4;
const ANIMATION_ASSET_TYPE_ID: i64 = 24;
pub const DYNAMIC_HEAD_ASSET_TYPE_ID: i64 = 79;
pub const DYNAMIC_HEAD_ASSET_FORMAT: &str = "avatar_meshpart_head";
const ANIMATION_ASSET_TYPE_IDS: [i64; 12] = [24, 48, 49, 50, 51, 52, 53, 54, 55, 56, 61, 78];
const BODY_PART_ASSET_TYPE_IDS: [i64; 7] = [17, 27, 28, 29, 30, 31, 79];
const RIGID_ACCESSORY_ASSET_TYPE_IDS: [i64; 8] = [8, 41, 42, 43, 44, 45, 46, 47];
const LAYERED_CLOTHING_ASSET_TYPE_IDS: [i64; 9] = [64, 65, 66, 67, 68, 69, 70, 71, 72];
const FACE_ACCESSORY_ASSET_TYPE_IDS: [i64; 2] = [76, 77];
const MAKEUP_ASSET_TYPE_IDS: [i64; 3] = [88, 89, 90];
const KNOWN_RIG_PART_NAMES: [&str; 20] = [
    "Torso", "Left Arm", "Right Arm", "Left Leg", "Right Leg", "Head", "UpperTorso", "LowerTorso", "LeftUpperArm", "LeftLowerArm",
    "LeftHand", "RightUpperArm", "RightLowerArm", "RightHand", "LeftUpperLeg", "LeftLowerLeg", "LeftFoot", "RightUpperLeg", "RightLowerLeg",
    "RightFoot",
];
const MAX_ANIMATION_REFERENCE_DEPTH: u32 = 2;
const XML_ANIMATION_REFERENCE_DEPTH: u32 = 1;
const DEFAULT_PART_SIZE: [f32; 3] = [1., 1., 1.];
const DEFAULT_HEAD_HEIGHT: f64 = 1.2;
const PNG_MAGIC: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const JPEG_MAGIC: [u8; 3] = [255, 216, 255];
const R6_LIMB_MESH_FILES: [(&str, &str); 5] =
    [("Torso", "torso.mesh"), ("LeftArm", "leftarm.mesh"), ("RightArm", "rightarm.mesh"), ("LeftLeg", "leftleg.mesh"), ("RightLeg", "rightleg.mesh")];
const COMPOSIT_LIMB_BASE_FILES: [(&str, &str); 5] = [
    ("Torso", "CompositTorsoBase.mesh"),
    ("LeftArm", "CompositLeftArmBase.mesh"),
    ("RightArm", "CompositRightArmBase.mesh"),
    ("LeftLeg", "CompositLeftLegBase.mesh"),
    ("RightLeg", "CompositRightLegBase.mesh"),
];
const COMPOSIT_MESH_FILES: [(&str, &str); 8] = [
    ("shirtTemplate", "CompositShirtTemplate.mesh"),
    ("pantsTemplate", "CompositPantsTemplate.mesh"),
    ("tshirt", "CompositTShirt.mesh"),
    ("fullAtlasBase", "CompositFullAtlasBaseTexture.mesh"),
    ("fullAtlasOverlay", "CompositFullAtlasOverlayTexture.mesh"),
    ("r15TorsoBase", "R15CompositTorsoBase.mesh"),
    ("r15LeftLimbBase", "R15CompositLeftArmBase.mesh"),
    ("r15RightLimbBase", "R15CompositRightArmBase.mesh"),
];

type PayloadResult = Result<Option<Value>, String>;

// ---- typed array encoding ---------------------------------------------------

fn typed(kind: &str, bytes: &[u8]) -> Value {
    json!({ "__lbxTyped": kind, "b64": base64::engine::general_purpose::STANDARD.encode(bytes) })
}

fn u8s(bytes: &[u8]) -> Value {
    typed("Uint8Array", bytes)
}

fn f32s(values: &[f32]) -> Value {
    typed("Float32Array", &values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>())
}

fn u32s(values: &[u32]) -> Value {
    typed("Uint32Array", &values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>())
}

fn u16s(values: &[u16]) -> Value {
    typed("Uint16Array", &values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>())
}

/// `sumTypedArrayBytes`, read back from the encoded form.
pub fn typed_bytes(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.iter().map(typed_bytes).sum(),
        Value::Object(map) => match (map.get("__lbxTyped"), map.get("b64").and_then(Value::as_str)) {
            (Some(_), Some(b64)) => b64.len() / 4 * 3 - b64.bytes().rev().take_while(|b| *b == b'=').count(),
            _ => map.values().map(typed_bytes).sum(),
        },
        _ => 0,
    }
}

fn num(v: f32) -> Value {
    json!(f64::from(v))
}

fn vec3(v: &[f32; 3]) -> Value {
    Value::Array(v.iter().map(|x| num(*x)).collect())
}

fn vec2(v: &[f32; 2]) -> Value {
    Value::Array(v.iter().map(|x| num(*x)).collect())
}

fn cframe(c: &CFrame) -> Value {
    json!({ "position": vec3(&c.position), "rotation": c.rotation.iter().map(|x| num(*x)).collect::<Vec<_>>() })
}

fn insert_some(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

/// `classifyAssetPayload`.
pub fn classify(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&PNG_MAGIC) {
        return "png";
    }
    if bytes.starts_with(&JPEG_MAGIC) {
        return "jpeg";
    }
    if bytes.starts_with(b"version ") {
        return "mesh";
    }
    let mut trimmed = bytes.strip_prefix(&[239, 187, 191]).unwrap_or(bytes);
    while let [b' ' | b'\t' | b'\n' | b'\r', rest @ ..] = trimmed {
        trimmed = rest;
    }
    if trimmed.starts_with(b"<roblox!") {
        "rbxmBinary"
    } else if trimmed.starts_with(b"<roblox") {
        "rbxmXml"
    } else {
        "unknown"
    }
}

/// `toGeometryPayload`.
fn geometry(mesh: &Mesh) -> Value {
    let mut out = Map::new();
    out.insert("positions".into(), f32s(&mesh.positions));
    out.insert("normals".into(), f32s(&mesh.normals));
    out.insert("uvs".into(), f32s(&mesh.uvs));
    out.insert("indices".into(), u32s(&mesh.indices));
    if let Some(draco) = &mesh.draco {
        out.insert("draco".into(), u8s(draco));
    }
    if let Some(facs) = &mesh.facs {
        out.insert(
            "facs".into(),
            json!({
                "boneNames": facs.bone_names,
                "channels": facs.channels.iter().map(|c| f32s(c)).collect::<Vec<_>>(),
                "controlNames": facs.control_names,
                "poseCount": facs.pose_count,
                "threePoseCorrectives": facs.three_pose_correctives,
                "twoPoseCorrectives": facs.two_pose_correctives,
            }),
        );
    }
    if mesh.draco.is_some() {
        insert_some(&mut out, "lodOffsets", mesh.lod_offsets.as_ref().map(|l| json!(l)));
    }
    if let (Some(bones), Some(indices), Some(weights)) = (mesh.bones.as_ref().filter(|b| !b.is_empty()), &mesh.skin_indices, &mesh.skin_weights) {
        out.insert("skinIndices".into(), u16s(indices));
        out.insert("skinWeights".into(), f32s(weights));
        out.insert(
            "bones".into(),
            Value::Array(
                bones
                    .iter()
                    .map(|b| {
                        json!({
                            "name": b.name,
                            "parentIndex": b.parent_index,
                            "position": vec3(&b.position),
                            "rotation": b.rotation.iter().map(|x| num(*x)).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

fn image(bytes: &[u8]) -> Value {
    json!({ "kind": "image", "bytes": u8s(bytes) })
}

fn is_image(bytes: &[u8]) -> bool {
    matches!(classify(bytes), "png" | "jpeg")
}

// ---- parse context ------------------------------------------------------------

/// `createTrackingParseContext`: every sub-asset fetch that fails is recorded.
pub struct ParseContext<'a> {
    pub state: &'a Arc<AppState>,
    pub delivery: &'a Arc<AssetDelivery>,
    pub misses: Mutex<Vec<Value>>,
}

impl<'a> ParseContext<'a> {
    pub fn new(state: &'a Arc<AppState>, delivery: &'a Arc<AssetDelivery>) -> Self {
        Self { state, delivery, misses: Mutex::new(Vec::new()) }
    }

    async fn fetch(&self, asset_id: i64, asset_type_id: i64) -> Option<Arc<Vec<u8>>> {
        match self.delivery.get_one(self.state, AssetRequest::new(asset_id, asset_type_id)).await {
            AssetBytes::Ok { bytes, .. } => Some(bytes),
            AssetBytes::Failed { asset_id, failure } => {
                let mut miss = json!({ "assetId": asset_id, "reason": failure.reason });
                if let Some(message) = failure.message {
                    miss["message"] = json!(message);
                }
                self.misses.lock().push(miss);
                None
            }
        }
    }

    /// `fetchImagePayload`, returning just the image bytes.
    async fn fetch_image(&self, asset_id: i64) -> Option<Arc<Vec<u8>>> {
        self.fetch(asset_id, IMAGE_ASSET_TYPE_ID).await.filter(|b| is_image(b))
    }

    async fn fetch_image_payload(&self, asset_id: i64) -> Option<Value> {
        self.fetch_image(asset_id).await.map(|b| image(&b))
    }

    /// `fetchGeometry`.
    async fn fetch_geometry(&self, asset_id: i64) -> Option<Value> {
        let bytes = self.fetch(asset_id, MESH_ASSET_TYPE_ID).await?;
        match parse_file_mesh(&bytes) {
            Ok(mesh) => Some(geometry(&mesh)),
            Err(error) => {
                tracing::debug!("Mesh asset {asset_id} parse failed: {error}");
                None
            }
        }
    }
}

// ---- builders -------------------------------------------------------------------

async fn build_surface_appearance(ctx: &ParseContext<'_>, extracted: &Option<ex::SurfaceExtract>) -> Option<Value> {
    let s = extracted.as_ref()?;
    let fetch = |id: Option<i64>| async move {
        match id {
            Some(id) => ctx.fetch_image(id).await,
            None => None,
        }
    };
    let (color, normal, roughness, metalness) =
        futures::join!(fetch(s.color_map_id), fetch(s.normal_map_id), fetch(s.roughness_map_id), fetch(s.metalness_map_id));
    if color.is_none() && normal.is_none() && roughness.is_none() && metalness.is_none() {
        return None;
    }
    let mut out = Map::new();
    insert_some(&mut out, "colorMapBytes", color.map(|b| u8s(&b)));
    insert_some(&mut out, "normalMapBytes", normal.map(|b| u8s(&b)));
    insert_some(&mut out, "roughnessMapBytes", roughness.map(|b| u8s(&b)));
    insert_some(&mut out, "metalnessMapBytes", metalness.map(|b| u8s(&b)));
    insert_some(&mut out, "alphaMode", s.alpha_mode.map(|a| json!(a)));
    Some(Value::Object(out))
}

async fn build_wrap_target(ctx: &ParseContext<'_>, extracted: &Option<ex::WrapTargetExtract>) -> Option<Value> {
    let w = extracted.as_ref()?;
    let cage = ctx.fetch_geometry(w.cage_mesh_id).await?;
    Some(json!({ "cageGeometry": cage, "cageOrigin": cframe(&w.cage_origin) }))
}

async fn build_wrap_layer(ctx: &ParseContext<'_>, extracted: &Option<ex::WrapLayerExtract>) -> Option<Value> {
    let w = extracted.as_ref()?;
    let reference = ctx.fetch_geometry(w.reference_mesh_id).await?;
    let cage = match w.cage_mesh_id {
        Some(id) => ctx.fetch_geometry(id).await,
        None => None,
    };
    Some(json!({
        "referenceGeometry": reference,
        "referenceOrigin": cframe(&w.reference_origin),
        "cageGeometry": cage,
        "cageOrigin": w.cage_origin.as_ref().map(cframe),
        "order": w.order,
        "puffiness": w.puffiness,
    }))
}

/// `buildMeshPartPayload`.
async fn build_mesh_part(ctx: &ParseContext<'_>, part: &PartExtract) -> Option<Value> {
    let mesh_id = part.mesh_content_id?;
    let Some(bytes) = ctx.fetch(mesh_id, MESH_ASSET_TYPE_ID).await else {
        tracing::debug!("Mesh content {mesh_id} fetch failed for part {}", part.name);
        return None;
    };
    let geometry = match parse_file_mesh(&bytes) {
        Ok(mesh) => geometry(&mesh),
        Err(error) => {
            tracing::debug!("Mesh content {mesh_id} parse failed for part {}: {error}", part.name);
            return None;
        }
    };
    drop(bytes);
    let texture = match part.texture_content_id {
        Some(id) => ctx.fetch_image(id).await,
        None => None,
    };
    let surface = build_surface_appearance(ctx, &part.surface_appearance).await;
    let wrap_target = build_wrap_target(ctx, &part.wrap_target).await;
    let mut out = Map::new();
    out.insert("name".into(), json!(part.name));
    out.insert("geometry".into(), geometry);
    out.insert("size".into(), vec3(&part.size.or(part.initial_size).unwrap_or(DEFAULT_PART_SIZE)));
    insert_some(&mut out, "originalSize", part.initial_size.as_ref().map(vec3));
    insert_some(&mut out, "colorRgb", part.color_rgb.map(|c| json!(c)));
    if part.double_sided {
        out.insert("doubleSided".into(), json!(true));
    }
    out.insert("transparency".into(), json!(part.transparency));
    out.insert(
        "attachments".into(),
        Value::Array(part.attachments.iter().map(|(name, c)| json!({ "name": name, "cframe": cframe(c) })).collect()),
    );
    insert_some(&mut out, "meshScale", part.mesh_scale.as_ref().map(vec3));
    insert_some(&mut out, "meshOffset", part.mesh_offset.as_ref().map(vec3));
    insert_some(&mut out, "scaleType", part.scale_type.as_ref().map(|s| json!(s)));
    insert_some(&mut out, "textureBytes", texture.map(|b| u8s(&b)));
    insert_some(&mut out, "surfaceAppearance", surface);
    insert_some(&mut out, "wrapTarget", wrap_target);
    Some(Value::Object(out))
}

async fn build_character_mesh(ctx: &ParseContext<'_>, mesh: &ex::CharacterMeshExtract) -> Option<Value> {
    let Some(bytes) = ctx.fetch(mesh.mesh_asset_id, MESH_ASSET_TYPE_ID).await else {
        tracing::debug!("CharacterMesh content {} fetch failed", mesh.mesh_asset_id);
        return None;
    };
    let geometry = match parse_file_mesh(&bytes) {
        Ok(m) => geometry(&m),
        Err(error) => {
            tracing::debug!("CharacterMesh content {} parse failed: {error}", mesh.mesh_asset_id);
            return None;
        }
    };
    let image_of = |id: Option<i64>| async move {
        match id {
            Some(id) => ctx.fetch_image(id).await,
            None => None,
        }
    };
    let (base, overlay) = futures::join!(image_of(mesh.base_texture_asset_id), image_of(mesh.overlay_texture_asset_id));
    let mut out = Map::new();
    out.insert("bodyPart".into(), json!(mesh.body_part));
    out.insert("geometry".into(), geometry);
    insert_some(&mut out, "baseTextureBytes", base.map(|b| u8s(&b)));
    insert_some(&mut out, "overlayTextureBytes", overlay.map(|b| u8s(&b)));
    Some(Value::Object(out))
}

async fn build_makeup(ctx: &ParseContext<'_>, decals: Vec<ex::MakeupDecal>) -> Option<Value> {
    let mut cage_ids: Vec<i64> = Vec::new();
    for id in decals.iter().filter_map(|d| d.reference_cage_mesh_id) {
        if !cage_ids.contains(&id) {
            cage_ids.push(id);
        }
    }
    let (cages, images) = futures::join!(
        join_all(cage_ids.iter().map(|id| ctx.fetch_geometry(*id))),
        join_all(decals.iter().map(|d| ctx.fetch_image(d.texture_content_id)))
    );
    let layers: Vec<Value> = decals
        .iter()
        .zip(images)
        .filter_map(|(decal, image)| {
            let image = image?;
            let mut layer = Map::new();
            layer.insert("colorBytes".into(), u8s(&image));
            let cage = decal.reference_cage_mesh_id.and_then(|id| cage_ids.iter().position(|c| *c == id)).and_then(|i| cages[i].clone());
            insert_some(&mut layer, "referenceCage", cage);
            layer.insert("uvMax".into(), vec2(&decal.uv_max));
            layer.insert("uvMin".into(), vec2(&decal.uv_min));
            layer.insert("zIndex".into(), json!(decal.z_index));
            Some(Value::Object(layer))
        })
        .collect();
    (!layers.is_empty()).then(|| json!({ "kind": "makeup", "layers": layers }))
}

fn pose_json(part: &str, cframe_value: Value, weight: f64) -> Value {
    json!({ "part": part, "cframe": cframe_value, "weight": weight })
}

/// `toAnimationPayload`.
fn animation_payload(sequence: &Sequence) -> Value {
    let keyframes: Vec<Value> = sequence
        .keyframes
        .iter()
        .map(|k| {
            let poses: Vec<Value> = k
                .poses
                .iter()
                .filter(|p| KNOWN_RIG_PART_NAMES.contains(&p.part.as_str()))
                .map(|p| pose_json(&p.part, cframe(&p.cframe), p.weight))
                .collect();
            let mut out = json!({ "time": k.time, "poses": poses });
            if let Some(facs) = &k.facs_controls {
                out["facsControls"] = Value::Object(facs.iter().map(|(n, v)| (n.clone(), json!(v))).collect());
            }
            out
        })
        .collect();
    json!({ "kind": "animation", "loop": sequence.looped, "keyframes": keyframes })
}

fn curve_animation_payload(keyframes: &[ex::CurveKeyframe]) -> Value {
    let keyframes: Vec<Value> = keyframes
        .iter()
        .map(|k| {
            let poses: Vec<Value> = k
                .poses
                .iter()
                .filter(|p| KNOWN_RIG_PART_NAMES.contains(&p.part.as_str()))
                .map(|p| pose_json(&p.part, json!({ "position": p.position, "rotation": p.rotation }), 1.0))
                .collect();
            json!({ "time": k.time, "poses": poses })
        })
        .collect();
    json!({ "kind": "animation", "loop": true, "keyframes": keyframes })
}

fn build_animation<'a>(ctx: &'a ParseContext<'a>, asset_id: i64, depth: u32, doc: Document) -> BoxFuture<'a, PayloadResult> {
    async move {
        if let Some(sequence) = ex::extract_keyframe_sequence(&doc).filter(|s| !s.keyframes.is_empty()) {
            return Ok(Some(animation_payload(&sequence)));
        }
        if let Some(curves) = ex::extract_curve_animation(&doc) {
            return Ok(Some(curve_animation_payload(&curves)));
        }
        match ex::extract_animation_reference(&doc) {
            Some(referenced) if referenced != asset_id => fetch_animation(ctx, referenced, depth + 1).await,
            _ => Ok(None),
        }
    }
    .boxed()
}

fn fetch_animation<'a>(ctx: &'a ParseContext<'a>, asset_id: i64, depth: u32) -> BoxFuture<'a, PayloadResult> {
    async move {
        if depth > MAX_ANIMATION_REFERENCE_DEPTH {
            return Ok(None);
        }
        let Some(bytes) = ctx.fetch(asset_id, ANIMATION_ASSET_TYPE_ID).await else { return Ok(None) };
        if classify(&bytes) != "rbxmBinary" {
            return Ok(None);
        }
        let doc = parse_rbxm(&bytes)?;
        build_animation(ctx, asset_id, depth, doc).await
    }
    .boxed()
}

fn is_accessory_type(type_id: i64) -> bool {
    RIGID_ACCESSORY_ASSET_TYPE_IDS.contains(&type_id) || LAYERED_CLOTHING_ASSET_TYPE_IDS.contains(&type_id) || FACE_ACCESSORY_ASSET_TYPE_IDS.contains(&type_id)
}

async fn parse_xml(ctx: &ParseContext<'_>, bytes: &[u8], request: &AssetRequest) -> PayloadResult {
    let xml = String::from_utf8_lossy(bytes);
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(&xml);
    if ANIMATION_ASSET_TYPE_IDS.contains(&request.asset_type_id) {
        if let Some(sequence) = ex::extract_xml_keyframe_sequence(xml) {
            return Ok(Some(animation_payload(&sequence)));
        }
        return match ex::extract_first_xml_content_asset_id(xml) {
            Some(referenced) => fetch_animation(ctx, referenced, XML_ANIMATION_REFERENCE_DEPTH).await,
            None => Ok(None),
        };
    }
    if is_accessory_type(request.asset_type_id) {
        let extraction = ex::extract_xml_accessory(xml);
        if extraction.mesh_content_id.is_some() {
            let part = PartExtract {
                name: "Handle".into(),
                mesh_content_id: extraction.mesh_content_id,
                texture_content_id: extraction.texture_content_id,
                size: None,
                initial_size: None,
                color_rgb: None,
                transparency: 0.0,
                double_sided: false,
                attachments: extraction.attachments,
                mesh_scale: extraction.mesh_scale,
                mesh_offset: extraction.mesh_offset,
                scale_type: None,
                surface_appearance: None,
                wrap_target: None,
            };
            if let Some(handle) = build_mesh_part(ctx, &part).await {
                let mut out = json!({ "kind": "accessory", "handle": handle });
                if let Some(point) = &extraction.attachment_point {
                    out["attachmentPoint"] = cframe(point);
                }
                return Ok(Some(out));
            }
        }
        return Ok(None);
    }
    Ok(match ex::extract_first_xml_content_asset_id(xml) {
        Some(id) => ctx.fetch_image_payload(id).await,
        None => None,
    })
}

async fn parse_model(ctx: &ParseContext<'_>, bytes: &[u8], request: &AssetRequest) -> PayloadResult {
    let doc = parse_rbxm(bytes)?;
    let type_id = request.asset_type_id;
    if ANIMATION_ASSET_TYPE_IDS.contains(&type_id) {
        return build_animation(ctx, request.asset_id, 0, doc).await;
    }
    if BODY_PART_ASSET_TYPE_IDS.contains(&type_id) {
        let parts = ex::extract_body_parts(&doc);
        let meshes = ex::extract_character_meshes(&doc);
        let (parts, meshes) = futures::join!(
            join_all(parts.iter().map(|p| build_mesh_part(ctx, p))),
            join_all(meshes.iter().map(|m| build_character_mesh(ctx, m)))
        );
        let parts: Vec<Value> = parts.into_iter().flatten().collect();
        let meshes: Vec<Value> = meshes.into_iter().flatten().collect();
        if parts.is_empty() && meshes.is_empty() {
            return Ok(None);
        }
        let mut out = json!({ "kind": "bodyPart", "parts": parts });
        if !meshes.is_empty() {
            out["characterMeshes"] = Value::Array(meshes);
        }
        return Ok(Some(out));
    }
    if is_accessory_type(type_id) {
        let Some(accessory) = ex::extract_accessory(&doc) else { return Ok(None) };
        let Some(handle) = build_mesh_part(ctx, &accessory.handle).await else { return Ok(None) };
        let mut out = Map::new();
        out.insert("kind".into(), json!("accessory"));
        out.insert("handle".into(), handle);
        insert_some(&mut out, "attachmentPoint", accessory.attachment_point.as_ref().map(cframe));
        insert_some(&mut out, "scaleType", accessory.scale_type.as_ref().map(|s| json!(s)));
        insert_some(&mut out, "wrapLayer", build_wrap_layer(ctx, &accessory.wrap_layer).await);
        return Ok(Some(Value::Object(out)));
    }
    if MAKEUP_ASSET_TYPE_IDS.contains(&type_id) {
        return Ok(build_makeup(ctx, ex::extract_makeup(&doc)).await);
    }
    let clothing: Option<(&str, &[&str])> = match type_id {
        2 => Some(("ShirtGraphic", &["Graphic"])),
        11 => Some(("Shirt", &["ShirtTemplate"])),
        12 => Some(("Pants", &["PantsTemplate"])),
        _ => None,
    };
    let content_id = match clothing {
        Some((class, props)) => ex::find_by_class(&doc, &doc.roots, &[class]).first().and_then(|i| ex::content_asset_id(doc.get(*i), props)),
        None => ex::find_by_class(&doc, &doc.roots, &["Decal"]).first().and_then(|i| ex::content_asset_id(doc.get(*i), ex::TEXTURE_ID_PROP_NAMES)),
    };
    Ok(match content_id {
        Some(id) => ctx.fetch_image_payload(id).await,
        None => None,
    })
}

/// `parsePayload`.
pub async fn parse_payload(ctx: &ParseContext<'_>, bytes: &[u8], request: &AssetRequest) -> PayloadResult {
    match classify(bytes) {
        "png" | "jpeg" => Ok(Some(image(bytes))),
        "rbxmXml" => parse_xml(ctx, bytes, request).await,
        "rbxmBinary" => parse_model(ctx, bytes, request).await,
        _ => Ok(None),
    }
}

// ---- client defaults ----------------------------------------------------------------

/// `findInstalledContentRoot`: newest install that ships the default face.
fn find_installed_content_root(state: &AppState) -> Option<PathBuf> {
    let paths = product_paths(&state.paths.base, "player");
    let mut entries: Vec<String> = std::fs::read_dir(&paths.versions_dir).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    entries.sort();
    if let Some(records) = std::fs::read_to_string(&paths.installed_versions_file).ok().and_then(|raw| serde_json::from_str::<Vec<Value>>(&raw).ok()) {
        let installed_at = |hash: &str| -> u64 {
            records
                .iter()
                .rev()
                .find(|r| r.get("versionHash").and_then(Value::as_str) == Some(hash))
                .and_then(|r| r.get("installedAt").and_then(Value::as_str))
                .and_then(crate::services::instances::parse_iso_millis)
                .unwrap_or(0)
        };
        entries.sort_by_key(|e| std::cmp::Reverse(installed_at(e)));
    }
    entries
        .into_iter()
        .map(|e| paths.versions_dir.join(e).join("content"))
        .find(|dir| dir.join("textures").join("face.png").exists())
}

fn read_mesh(path: &Path) -> Option<Mesh> {
    parse_file_mesh(&std::fs::read(path).ok()?).ok()
}

fn default_head_part(mesh: &Mesh) -> Value {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for vertex in mesh.positions.chunks(3) {
        for (axis, value) in vertex.iter().enumerate() {
            min[axis] = min[axis].min(f64::from(*value));
            max[axis] = max[axis].max(f64::from(*value));
        }
    }
    let center = [0, 1, 2].map(|a| (min[a] + max[a]) / 2.0);
    let positions: Vec<f32> = mesh.positions.iter().enumerate().map(|(i, v)| (f64::from(*v) - center[i % 3]) as f32).collect();
    let original = [0, 1, 2].map(|a| (max[a] - min[a]).max(f64::EPSILON));
    let factor = DEFAULT_HEAD_HEIGHT / original[1];
    json!({
        "name": ex::HEAD_PART_NAME,
        "geometry": { "positions": f32s(&positions), "normals": f32s(&mesh.normals), "uvs": f32s(&mesh.uvs), "indices": u32s(&mesh.indices) },
        "size": original.map(|v| v * factor),
        "originalSize": original,
        "attachments": [],
    })
}

fn load_r6_limbs(content: &Path) -> Option<Value> {
    let dir = content.join("avatar").join("meshes");
    if !dir.exists() {
        return None;
    }
    let mut limbs = Map::new();
    for (limb, file) in R6_LIMB_MESH_FILES {
        let mesh = read_mesh(&dir.join(file))?;
        limbs.insert(
            limb.into(),
            json!({ "positions": f32s(&mesh.positions), "normals": f32s(&mesh.normals), "uvs": f32s(&mesh.uvs), "indices": u32s(&mesh.indices) }),
        );
    }
    Some(Value::Object(limbs))
}

fn load_compositing(content: &Path) -> Option<Value> {
    let dir = content.join("avatar").join("compositing");
    if !dir.exists() {
        return None;
    }
    let read = |file: &str| -> Value {
        read_mesh(&dir.join(file))
            .map(|m| json!({ "positions": f32s(&m.positions), "uvs": f32s(&m.uvs), "indices": u32s(&m.indices) }))
            .unwrap_or(Value::Null)
    };
    let mut out = Map::new();
    for (key, file) in COMPOSIT_MESH_FILES {
        out.insert(key.into(), read(file));
    }
    let limb_bases: Map<String, Value> =
        COMPOSIT_LIMB_BASE_FILES.iter().map(|(limb, file)| ((*limb).to_owned(), read(file))).filter(|(_, v)| !v.is_null()).collect();
    out.insert("limbBases".into(), Value::Object(limb_bases));
    Some(Value::Object(out))
}

async fn load_content_body_part(ctx: &ParseContext<'_>, content: &Path, label: &str, file: &[&str]) -> Option<Value> {
    let path = file.iter().fold(content.to_path_buf(), |p, s| p.join(s));
    let doc = match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| parse_rbxm(&b)) {
        Ok(doc) => doc,
        Err(error) => {
            tracing::debug!("{label} unavailable: {error}");
            return None;
        }
    };
    let parts: Vec<Value> = join_all(ex::extract_body_parts(&doc).iter().map(|p| build_mesh_part(ctx, p))).await.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| json!({ "kind": "bodyPart", "parts": parts }))
}

async fn load_default_clothing(ctx: &ParseContext<'_>, content: &Path) -> Option<Value> {
    let load = |file: &'static str| async move {
        let doc = match std::fs::read(content.join("avatar").join(file)).map_err(|e| e.to_string()).and_then(|b| parse_rbxm(&b)) {
            Ok(doc) => doc,
            Err(error) => {
                tracing::debug!("Default clothing unavailable ({file}): {error}");
                return None;
            }
        };
        ctx.fetch_image(ex::extract_clothing_template(&doc)?).await
    };
    let (shirt, pants) = futures::join!(load("defaultShirt.rbxm"), load("defaultPants.rbxm"));
    if shirt.is_none() && pants.is_none() {
        return None;
    }
    let mut out = Map::new();
    insert_some(&mut out, "shirtBytes", shirt.map(|b| u8s(&b)));
    insert_some(&mut out, "pantsBytes", pants.map(|b| u8s(&b)));
    Some(Value::Object(out))
}

async fn load_canonical_cages(ctx: &ParseContext<'_>, content: &Path) -> Option<Value> {
    let path = content.join("avatar").join("characterCagedHSRV18.rbxm");
    let doc = match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| parse_rbxm(&b)) {
        Ok(doc) => doc,
        Err(error) => {
            tracing::debug!("Canonical caged rig unavailable: {error}");
            return None;
        }
    };
    let caged: Vec<PartExtract> = ex::extract_body_parts(&doc).into_iter().filter(|p| p.wrap_target.is_some()).collect();
    let built = join_all(caged.iter().map(|p| build_wrap_target(ctx, &p.wrap_target))).await;
    let cages: Map<String, Value> = caged.iter().zip(built).filter_map(|(p, cage)| Some((p.name.clone(), cage?))).collect();
    (!cages.is_empty()).then_some(Value::Object(cages))
}

/// `loadDefaults`. Returns the payload and whether it is complete
/// (`isDefaultsLoadComplete`).
pub async fn load_defaults(state: &Arc<AppState>, delivery: &Arc<AssetDelivery>) -> (Value, bool) {
    let Some(content) = find_installed_content_root(state) else {
        let empty = json!({
            "face": null, "hasInstalledContent": false, "head": null, "compositing": null, "defaultClothing": null,
            "degraded": false, "dynamicHead": null, "canonicalCages": null, "r6Limbs": null, "r15Body": null,
        });
        return (empty, false);
    };
    let ctx = ParseContext::new(state, delivery);
    let face = match std::fs::read(content.join("textures").join("face.png")) {
        Ok(bytes) => Some(image(&bytes)),
        Err(error) => {
            tracing::debug!("Default face texture unavailable: {error}");
            None
        }
    };
    let head = match std::fs::read(content.join("avatar").join("heads").join("head.mesh")).map_err(|e| e.to_string()).and_then(|b| parse_file_mesh(&b)) {
        Ok(mesh) => Some(json!({ "kind": "bodyPart", "parts": [default_head_part(&mesh)] })),
        Err(error) => {
            tracing::debug!("Default head mesh unavailable: {error}");
            None
        }
    };
    let (canonical_cages, r15_body, default_clothing, dynamic_head) = futures::join!(
        load_canonical_cages(&ctx, &content),
        load_content_body_part(&ctx, &content, "Default R15 body", &["avatar", "characterR15.rbxm"]),
        load_default_clothing(&ctx, &content),
        load_content_body_part(&ctx, &content, "Default dynamic head", &["avatar", "defaultDynamicHead.rbxm"])
    );
    let degraded = !ctx.misses.lock().is_empty() || default_clothing.is_none() || r15_body.is_none();
    let value = json!({
        "face": face,
        "hasInstalledContent": true,
        "head": head,
        "compositing": load_compositing(&content),
        "defaultClothing": default_clothing,
        "degraded": degraded,
        "dynamicHead": dynamic_head,
        "canonicalCages": canonical_cages,
        "r6Limbs": load_r6_limbs(&content),
        "r15Body": r15_body,
    });
    (value, !degraded)
}

#[cfg(test)]
mod tests {
    #[test]
    fn classifies_payloads() {
        assert_eq!(super::classify(b"\x89PNG\r\n\x1a\nxx"), "png");
        assert_eq!(super::classify(b"version 2.00\n"), "mesh");
        assert_eq!(super::classify(b"\xef\xbb\xbf  <roblox!abc"), "rbxmBinary");
        assert_eq!(super::classify(b"\n<roblox xmlns"), "rbxmXml");
        assert_eq!(super::classify(b"abc"), "unknown");
        let v = super::f32s(&[1.0, 2.0]);
        assert_eq!(super::typed_bytes(&v), 8);
    }
}
