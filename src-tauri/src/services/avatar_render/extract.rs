//! Instance extractors over a parsed model: property helpers, the XML
//! fallbacks, body parts, accessories, makeup and animations.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::rbxm::{CFrame, Document, Instance, Prop};

pub const MESH_ID_PROP_NAMES: &[&str] = &["MeshId", "MeshID", "MeshID_Serialized"];
pub const TEXTURE_ID_PROP_NAMES: &[&str] = &["TextureID", "TextureId", "Texture"];
const SIZE_PROP_NAMES: &[&str] = &["size", "Size"];
const COLOR_PROP_NAMES: &[&str] = &["Color3uint8", "Color"];
const ANIMATION_ID_PROP_NAMES: &[&str] = &["AnimationId", "AnimationID"];
const CLOTHING_TEMPLATE_PROP_NAMES: &[&str] = &["ShirtTemplate", "PantsTemplate"];
const LOOP_PROP_NAMES: &[&str] = &["Loop", "Loops"];
const R15_FOLDER_NAMES: [&str; 3] = ["R15ArtistIntent", "R15Fixed", "R15"];
pub const HEAD_PART_NAME: &str = "Head";
const IDENTITY_ROTATION: [f32; 9] = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
const DEFAULT_ANIMATION_WEIGHT: f64 = 1.0;
const FLOAT_CURVE_KEY_SIZE: usize = 14;
const CURVE_TIME_UNITS_PER_SECOND: f64 = 1200.0;
const MAX_CURVE_KEYFRAMES: usize = 640;
const MAX_INSTANCE_TREE_DEPTH: usize = 64;

static RBX_ASSET_ID_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^rbxassetid://(\d+)").expect("regex"));
static HTTP_ASSET_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^https?://[^\s"']*[?&]id=(\d+)"#).expect("regex"));
static XML_URL_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<url>([^<]+)</url>").expect("regex"));
static XML_KEYFRAME_TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<float name="Time">([^<]+)</float>"#).expect("regex"));
static XML_INSTANCE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<string name="Name">([^<]*)</string>"#).expect("regex"));
static XML_POSE_WEIGHT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<float name="Weight">([^<]+)</float>"#).expect("regex"));
static XML_LOOP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<bool name="Loop">([^<]+)</bool>"#).expect("regex"));
static JS_FLOAT_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[+-]?(Infinity|(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?)").expect("regex"));
const CFRAME_COMPONENTS: [&str; 12] = ["X", "Y", "Z", "R00", "R01", "R02", "R10", "R11", "R12", "R20", "R21", "R22"];

/// `Number.parseFloat`: the longest numeric prefix after leading whitespace.
pub fn js_parse_float(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let Some(m) = JS_FLOAT_PREFIX.find(trimmed) else { return f64::NAN };
    let s = m.as_str();
    if s.ends_with("Infinity") {
        return if s.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    s.parse().unwrap_or(f64::NAN)
}

fn decode_xml_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let replaced = [("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&apos;", "'")]
            .iter()
            .find(|(entity, _)| rest.starts_with(entity));
        match replaced {
            Some((entity, value)) => {
                out.push_str(value);
                rest = &rest[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `parseContentUrl`.
pub fn parse_content_url(url: &str) -> Option<i64> {
    RBX_ASSET_ID_URL
        .captures(url)
        .or_else(|| HTTP_ASSET_URL.captures(url))
        .and_then(|c| c[1].parse::<u64>().ok())
        .map(|v| v as i64)
}

pub fn extract_first_xml_content_asset_id(xml: &str) -> Option<i64> {
    XML_URL_TAG.captures_iter(xml).find_map(|c| parse_content_url(&decode_xml_entities(c[1].trim())))
}

fn read_xml_cframe(block: &str, name: &str) -> Option<CFrame> {
    let opening = block.find(&format!("<CoordinateFrame name=\"{name}\">"))?;
    let body = match block[opening..].find("</CoordinateFrame>") {
        Some(end) => &block[opening..opening + end],
        None => &block[opening..],
    };
    let mut values = Vec::with_capacity(12);
    for tag in CFRAME_COMPONENTS {
        let pattern = Regex::new(&format!("<{tag}>([^<]+)</{tag}>")).ok()?;
        let value = js_parse_float(&pattern.captures(body)?[1]);
        if !value.is_finite() {
            return None;
        }
        values.push(value as f32);
    }
    Some(CFrame { position: [values[0], values[1], values[2]], rotation: values[3..].to_vec() })
}

#[derive(Clone, Debug)]
pub struct Pose {
    pub part: String,
    pub cframe: CFrame,
    pub weight: f64,
}

#[derive(Clone, Debug)]
pub struct Keyframe {
    pub time: f64,
    pub poses: Vec<Pose>,
    pub facs_controls: Option<Vec<(String, f64)>>,
}

#[derive(Clone, Debug)]
pub struct Sequence {
    pub keyframes: Vec<Keyframe>,
    pub looped: bool,
}

fn split_after<'a>(text: &'a str, tag: &'static str) ->impl Iterator<Item = &'a str> {
    text.split(tag).skip(1)
}

pub fn extract_xml_keyframe_sequence(xml: &str) -> Option<Sequence> {
    if !xml.contains("class=\"KeyframeSequence\"") {
        return None;
    }
    let mut keyframes = Vec::new();
    for block in split_after(xml, "<Item class=\"Keyframe\"") {
        let Some(time) = XML_KEYFRAME_TIME.captures(block).map(|c| js_parse_float(&c[1])) else { continue };
        if !time.is_finite() {
            continue;
        }
        let mut poses = Vec::new();
        for pose in split_after(block, "<Item class=\"Pose\"") {
            let (Some(name), Some(cframe)) = (XML_INSTANCE_NAME.captures(pose), read_xml_cframe(pose, "CFrame")) else { continue };
            let weight = XML_POSE_WEIGHT.captures(pose).map_or(DEFAULT_ANIMATION_WEIGHT, |c| js_parse_float(&c[1]));
            if weight.is_finite() && weight > 0.0 {
                poses.push(Pose { part: name[1].to_owned(), cframe, weight });
            }
        }
        keyframes.push(Keyframe { time, poses, facs_controls: None });
    }
    if keyframes.is_empty() {
        return None;
    }
    keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
    let looped = XML_LOOP.captures(xml).is_none_or(|c| &c[1] == "true");
    Some(Sequence { keyframes, looped })
}

fn read_xml_content_asset_id(xml: &str, name: &str) -> Option<i64> {
    let pattern = Regex::new(&format!(r#"(?i)<Content name="{}">\s*<url>([^<]+)</url>"#, regex::escape(name))).ok()?;
    parse_content_url(&decode_xml_entities(pattern.captures(xml)?[1].trim()))
}

fn read_xml_vector3(xml: &str, name: &str) -> Option<[f32; 3]> {
    let pattern = Regex::new(&format!(r#"(?i)<Vector3 name="{}">\s*<X>([^<]+)</X>\s*<Y>([^<]+)</Y>\s*<Z>([^<]+)</Z>"#, regex::escape(name))).ok()?;
    let c = pattern.captures(xml)?;
    let v = [js_parse_float(&c[1]), js_parse_float(&c[2]), js_parse_float(&c[3])];
    v.iter().all(|x| x.is_finite()).then(|| v.map(|x| x as f32))
}

fn read_xml_coordinate_frame(xml: &str, name: &str) -> Option<CFrame> {
    let components: String = CFRAME_COMPONENTS.iter().map(|c| format!(r"<{c}>([^<]+)</{c}>\s*")).collect();
    let pattern = Regex::new(&format!(r#"(?i)<CoordinateFrame name="{}">\s*{components}"#, regex::escape(name))).ok()?;
    let c = pattern.captures(xml)?;
    let values: Vec<f64> = (1..=12).map(|i| js_parse_float(&c[i])).collect();
    if !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let values: Vec<f32> = values.into_iter().map(|v| v as f32).collect();
    Some(CFrame { position: [values[0], values[1], values[2]], rotation: values[3..].to_vec() })
}

pub struct XmlAccessory {
    pub mesh_content_id: Option<i64>,
    pub texture_content_id: Option<i64>,
    pub mesh_scale: Option<[f32; 3]>,
    pub mesh_offset: Option<[f32; 3]>,
    pub attachment_point: Option<CFrame>,
    pub attachments: Vec<(String, CFrame)>,
}

pub fn extract_xml_accessory(xml: &str) -> XmlAccessory {
    let attachments = split_after(xml, "<Item class=\"Attachment\"")
        .filter_map(|block| Some((XML_INSTANCE_NAME.captures(block)?[1].to_owned(), read_xml_cframe(block, "CFrame")?)))
        .collect();
    XmlAccessory {
        mesh_content_id: read_xml_content_asset_id(xml, "MeshId"),
        texture_content_id: read_xml_content_asset_id(xml, "TextureId"),
        mesh_scale: read_xml_vector3(xml, "Scale"),
        mesh_offset: read_xml_vector3(xml, "Offset"),
        attachment_point: read_xml_coordinate_frame(xml, "AttachmentPoint"),
        attachments,
    }
}

// ---- property helpers ----------------------------------------------------

fn props<'a>(instance: &'a Instance, names: &'a [&str]) -> impl Iterator<Item = &'a Prop> {
    names.iter().filter_map(|n| instance.props.get(*n))
}

pub fn string_prop(instance: &Instance, names: &[&str]) -> Option<String> {
    props(instance, names).find_map(|p| match p {
        Prop::Str { value, .. } | Prop::Content(value) if !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

fn string_bytes_prop<'a>(instance: &'a Instance, names: &[&str]) -> Option<&'a [u8]> {
    names.iter().filter_map(|n| instance.props.get(*n)).find_map(|p| match p {
        Prop::Str { bytes, .. } if !bytes.is_empty() => Some(bytes.as_slice()),
        _ => None,
    })
}

pub fn content_asset_id(instance: &Instance, names: &[&str]) -> Option<i64> {
    string_prop(instance, names).and_then(|url| parse_content_url(&url))
}

fn vector2_prop(instance: &Instance, names: &[&str]) -> Option<[f32; 2]> {
    props(instance, names).find_map(|p| if let Prop::Vector2(v) = p { Some(*v) } else { None })
}

fn vector3_prop(instance: &Instance, names: &[&str]) -> Option<[f32; 3]> {
    props(instance, names).find_map(|p| if let Prop::Vector3(v) = p { Some(*v) } else { None })
}

fn cframe_prop(instance: &Instance, names: &[&str]) -> Option<CFrame> {
    props(instance, names).find_map(|p| match p {
        Prop::CFrame(c) | Prop::OptionalCFrame(Some(c)) => Some(c.clone()),
        _ => None,
    })
}

fn color_prop(instance: &Instance, names: &[&str]) -> Option<[f64; 3]> {
    props(instance, names).find_map(|p| match p {
        Prop::Color3(c) => Some(c.map(f64::from)),
        Prop::Color3Uint8(c) => Some(c.map(|v| f64::from(v) / 255.0)),
        _ => None,
    })
}

fn float_prop(instance: &Instance, names: &[&str]) -> Option<f64> {
    props(instance, names).find_map(|p| match p {
        Prop::Float32(v) => Some(f64::from(*v)),
        Prop::Float64(v) => Some(*v),
        Prop::Int32(v) => Some(f64::from(*v)),
        _ => None,
    })
}

fn bool_prop(instance: &Instance, names: &[&str]) -> Option<bool> {
    props(instance, names).find_map(|p| if let Prop::Bool(v) = p { Some(*v) } else { None })
}

fn int_prop(instance: &Instance, names: &[&str]) -> Option<i64> {
    props(instance, names).find_map(|p| match p {
        Prop::Int32(v) => Some(i64::from(*v)),
        Prop::Int64(v) => Some(*v),
        Prop::Enum(v) => Some(i64::from(*v)),
        _ => None,
    })
}

/// `findInstancesByClass`: pre-order walk.
pub fn find_by_class(doc: &Document, roots: &[usize], classes: &[&str]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(i) = stack.pop() {
        let instance = doc.get(i);
        if classes.contains(&instance.class_name.as_str()) {
            found.push(i);
        }
        stack.extend(instance.children.iter().rev());
    }
    found
}

fn find_child<'a>(doc: &'a Document, parent: &Instance, predicate: impl Fn(&Instance) -> bool) -> Option<&'a Instance> {
    parent.children.iter().map(|c| doc.get(*c)).find(|c| predicate(c))
}

fn find_avatar_part_scale_type(doc: &Document, roots: &[usize]) -> Option<String> {
    find_by_class(doc, roots, &["StringValue"])
        .into_iter()
        .map(|i| doc.get(i))
        .filter(|i| i.name == "AvatarPartScaleType")
        .find_map(|i| string_prop(i, &["Value"]))
}

// ---- extracted shapes ------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SurfaceExtract {
    pub color_map_id: Option<i64>,
    pub normal_map_id: Option<i64>,
    pub roughness_map_id: Option<i64>,
    pub metalness_map_id: Option<i64>,
    pub alpha_mode: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct WrapTargetExtract {
    pub cage_mesh_id: i64,
    pub cage_origin: CFrame,
}

#[derive(Clone, Debug)]
pub struct WrapLayerExtract {
    pub reference_mesh_id: i64,
    pub reference_origin: CFrame,
    pub cage_mesh_id: Option<i64>,
    pub cage_origin: Option<CFrame>,
    pub order: i64,
    pub puffiness: f64,
}

#[derive(Clone, Debug)]
pub struct PartExtract {
    pub name: String,
    pub mesh_content_id: Option<i64>,
    pub texture_content_id: Option<i64>,
    pub size: Option<[f32; 3]>,
    pub initial_size: Option<[f32; 3]>,
    pub color_rgb: Option<[f64; 3]>,
    pub transparency: f64,
    pub double_sided: bool,
    pub attachments: Vec<(String, CFrame)>,
    pub mesh_scale: Option<[f32; 3]>,
    pub mesh_offset: Option<[f32; 3]>,
    pub scale_type: Option<String>,
    pub surface_appearance: Option<SurfaceExtract>,
    pub wrap_target: Option<WrapTargetExtract>,
}

fn extract_attachments(doc: &Document, part: &Instance) -> Vec<(String, CFrame)> {
    part.children
        .iter()
        .map(|c| doc.get(*c))
        .filter(|c| c.class_name == "Attachment")
        .filter_map(|c| Some((c.name.clone(), cframe_prop(c, &["CFrame"])?)))
        .collect()
}

fn extract_surface_appearance(doc: &Document, part: &Instance) -> Option<SurfaceExtract> {
    let surface = find_child(doc, part, |c| c.class_name == "SurfaceAppearance")?;
    Some(SurfaceExtract {
        color_map_id: content_asset_id(surface, &["ColorMap"]),
        normal_map_id: content_asset_id(surface, &["NormalMap"]),
        roughness_map_id: content_asset_id(surface, &["RoughnessMap"]),
        metalness_map_id: content_asset_id(surface, &["MetalnessMap"]),
        alpha_mode: match surface.props.get("AlphaMode") {
            Some(Prop::Enum(v)) => Some(*v),
            _ => None,
        },
    })
}

fn extract_wrap_target(doc: &Document, part: &Instance) -> Option<WrapTargetExtract> {
    let wrap = find_child(doc, part, |c| c.class_name == "WrapTarget")?;
    Some(WrapTargetExtract { cage_mesh_id: content_asset_id(wrap, &["CageMeshId"])?, cage_origin: cframe_prop(wrap, &["CageOrigin"])? })
}

fn extract_wrap_layer(doc: &Document, accessory_root: usize, handle: usize) -> Option<WrapLayerExtract> {
    let wrap = doc.get(*find_by_class(doc, &[handle, accessory_root], &["WrapLayer"]).first()?);
    Some(WrapLayerExtract {
        reference_mesh_id: content_asset_id(wrap, &["ReferenceMeshId"])?,
        reference_origin: cframe_prop(wrap, &["ReferenceOrigin"])?,
        cage_mesh_id: content_asset_id(wrap, &["CageMeshId"]),
        cage_origin: cframe_prop(wrap, &["CageOrigin"]),
        order: int_prop(wrap, &["Order"]).unwrap_or(1),
        puffiness: float_prop(wrap, &["Puffiness"]).unwrap_or(1.0),
    })
}

/// `extractMeshPart`.
pub fn extract_mesh_part(doc: &Document, index: usize) -> PartExtract {
    let part = doc.get(index);
    let special = find_child(doc, part, |c| c.class_name == "SpecialMesh" || c.class_name == "FileMesh");
    let original_size = find_child(doc, part, |c| c.class_name == "Vector3Value" && c.name == "OriginalSize");
    let source = special.unwrap_or(part);
    PartExtract {
        surface_appearance: extract_surface_appearance(doc, part),
        wrap_target: extract_wrap_target(doc, part),
        name: part.name.clone(),
        mesh_content_id: content_asset_id(source, MESH_ID_PROP_NAMES),
        texture_content_id: content_asset_id(source, TEXTURE_ID_PROP_NAMES),
        size: vector3_prop(part, SIZE_PROP_NAMES),
        initial_size: vector3_prop(part, &["InitialSize"]).or_else(|| original_size.and_then(|o| vector3_prop(o, &["Value"]))),
        color_rgb: color_prop(part, COLOR_PROP_NAMES),
        transparency: float_prop(part, &["Transparency"]).unwrap_or(0.0),
        double_sided: bool_prop(part, &["DoubleSided"]).unwrap_or(false),
        attachments: extract_attachments(doc, part),
        mesh_scale: special.and_then(|s| vector3_prop(s, &["Scale"])),
        mesh_offset: special.and_then(|s| vector3_prop(s, &["Offset"])),
        scale_type: None,
    }
}

fn extract_special_mesh_head(doc: &Document) -> Option<PartExtract> {
    let mesh = doc.get(*find_by_class(doc, &doc.roots, &["SpecialMesh", "FileMesh"]).first()?);
    let mesh_content_id = content_asset_id(mesh, MESH_ID_PROP_NAMES)?;
    let attachments = mesh
        .children
        .iter()
        .map(|c| doc.get(*c))
        .filter(|c| c.class_name == "Vector3Value" && c.name.ends_with("Attachment"))
        .filter_map(|c| Some((c.name.clone(), CFrame { position: vector3_prop(c, &["Value"])?, rotation: IDENTITY_ROTATION.to_vec() })))
        .collect();
    Some(PartExtract {
        name: HEAD_PART_NAME.to_owned(),
        mesh_content_id: Some(mesh_content_id),
        texture_content_id: content_asset_id(mesh, TEXTURE_ID_PROP_NAMES),
        size: None,
        initial_size: None,
        color_rgb: None,
        transparency: 0.0,
        double_sided: false,
        attachments,
        mesh_scale: vector3_prop(mesh, &["Scale"]),
        mesh_offset: vector3_prop(mesh, &["Offset"]),
        scale_type: None,
        surface_appearance: None,
        wrap_target: None,
    })
}

/// `extractBodyParts`.
pub fn extract_body_parts(doc: &Document) -> Vec<PartExtract> {
    let scale_type = find_avatar_part_scale_type(doc, &doc.roots);
    let with_scale = |parts: Vec<PartExtract>| -> Vec<PartExtract> {
        parts.into_iter().map(|p| PartExtract { scale_type: scale_type.clone(), ..p }).collect()
    };
    let folders = find_by_class(doc, &doc.roots, &["Folder"]);
    for folder_name in R15_FOLDER_NAMES {
        if let Some(folder) = folders.iter().find(|i| doc.get(**i).name == folder_name) {
            let children = doc.get(*folder).children.clone();
            return with_scale(find_by_class(doc, &children, &["MeshPart", "Part"]).into_iter().map(|i| extract_mesh_part(doc, i)).collect());
        }
    }
    let parts: Vec<PartExtract> = find_by_class(doc, &doc.roots, &["MeshPart", "Part"]).into_iter().map(|i| extract_mesh_part(doc, i)).collect();
    if !parts.is_empty() {
        return with_scale(parts);
    }
    extract_special_mesh_head(doc).map(|h| with_scale(vec![h])).unwrap_or_default()
}

pub struct CharacterMeshExtract {
    pub body_part: i64,
    pub mesh_asset_id: i64,
    pub base_texture_asset_id: Option<i64>,
    pub overlay_texture_asset_id: Option<i64>,
}

pub fn extract_character_meshes(doc: &Document) -> Vec<CharacterMeshExtract> {
    find_by_class(doc, &doc.roots, &["CharacterMesh"])
        .into_iter()
        .map(|i| doc.get(i))
        .filter_map(|instance| {
            let body_part = int_prop(instance, &["BodyPart"])?;
            let mesh_asset_id = int_prop(instance, &["MeshId"]).filter(|v| *v > 0)?;
            Some(CharacterMeshExtract {
                body_part,
                mesh_asset_id,
                base_texture_asset_id: int_prop(instance, &["BaseTextureId"]).filter(|v| *v > 0),
                overlay_texture_asset_id: int_prop(instance, &["OverlayTextureId"]).filter(|v| *v > 0),
            })
        })
        .collect()
}

pub struct AccessoryExtract {
    pub handle: PartExtract,
    pub attachment_point: Option<CFrame>,
    pub scale_type: Option<String>,
    pub wrap_layer: Option<WrapLayerExtract>,
}

pub fn extract_accessory(doc: &Document) -> Option<AccessoryExtract> {
    let root = find_by_class(doc, &doc.roots, &["Accessory", "Hat"]).first().copied().or_else(|| doc.roots.first().copied())?;
    let root_instance = doc.get(root);
    let handle = root_instance
        .children
        .iter()
        .copied()
        .find(|c| doc.get(*c).name == "Handle")
        .or_else(|| find_by_class(doc, &[root], &["MeshPart", "Part"]).first().copied())?;
    Some(AccessoryExtract {
        handle: extract_mesh_part(doc, handle),
        attachment_point: cframe_prop(root_instance, &["AttachmentPoint"]),
        scale_type: find_avatar_part_scale_type(doc, &[root]),
        wrap_layer: extract_wrap_layer(doc, root, handle),
    })
}

pub struct MakeupDecal {
    pub reference_cage_mesh_id: Option<i64>,
    pub texture_content_id: i64,
    pub uv_max: [f32; 2],
    pub uv_min: [f32; 2],
    pub z_index: i64,
}

pub fn extract_makeup(doc: &Document) -> Vec<MakeupDecal> {
    let mut decals: Vec<MakeupDecal> = find_by_class(doc, &doc.roots, &["Decal"])
        .into_iter()
        .map(|i| doc.get(i))
        .filter_map(|decal| {
            let transfer = find_child(doc, decal, |c| c.class_name == "WrapTextureTransfer")?;
            let texture_content_id = content_asset_id(decal, TEXTURE_ID_PROP_NAMES)?;
            Some(MakeupDecal {
                uv_min: vector2_prop(transfer, &["UVMinBound"])?,
                uv_max: vector2_prop(transfer, &["UVMaxBound"])?,
                reference_cage_mesh_id: content_asset_id(transfer, &["ReferenceCageMeshContent"]),
                texture_content_id,
                z_index: int_prop(decal, &["ZIndex"]).unwrap_or(0),
            })
        })
        .collect();
    decals.sort_by_key(|d| d.z_index);
    decals
}

fn collect_poses(doc: &Document, instance: &Instance, out: &mut Vec<Pose>, depth: usize) {
    if depth >= MAX_INSTANCE_TREE_DEPTH {
        return;
    }
    for child in instance.children.iter().map(|c| doc.get(*c)) {
        if child.class_name == "Pose" {
            let weight = float_prop(child, &["Weight"]).unwrap_or(1.0);
            if let Some(cframe) = cframe_prop(child, &["CFrame"]).filter(|_| weight > 0.0) {
                out.push(Pose { part: child.name.clone(), cframe, weight });
            }
        }
        collect_poses(doc, child, out, depth + 1);
    }
}

fn collect_facs_controls(doc: &Document, instance: &Instance, out: &mut Vec<(String, f64)>, depth: usize) -> bool {
    if depth >= MAX_INSTANCE_TREE_DEPTH {
        return false;
    }
    let mut found = false;
    for child in instance.children.iter().map(|c| doc.get(*c)) {
        if child.class_name == "NumberPose" {
            found = true;
            if let Some(value) = float_prop(child, &["Value"]).filter(|v| *v != 0.0) {
                match out.iter_mut().find(|(n, _)| *n == child.name) {
                    Some(entry) => entry.1 = value,
                    None => out.push((child.name.clone(), value)),
                }
            }
        }
        if collect_facs_controls(doc, child, out, depth + 1) {
            found = true;
        }
    }
    found
}

pub fn extract_keyframe_sequence(doc: &Document) -> Option<Sequence> {
    let sequence = doc.get(*find_by_class(doc, &doc.roots, &["KeyframeSequence"]).first()?);
    let mut keyframes = Vec::new();
    for child in sequence.children.iter().map(|c| doc.get(*c)).filter(|c| c.class_name == "Keyframe") {
        let Some(time) = float_prop(child, &["Time"]) else { continue };
        let mut poses = Vec::new();
        collect_poses(doc, child, &mut poses, 0);
        let mut facs = Vec::new();
        let is_facial = collect_facs_controls(doc, child, &mut facs, 0);
        keyframes.push(Keyframe { time, poses, facs_controls: is_facial.then_some(facs) });
    }
    keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
    Some(Sequence { keyframes, looped: bool_prop(sequence, LOOP_PROP_NAMES).unwrap_or(true) })
}

fn animation_weight(doc: &Document, animation: &Instance) -> f64 {
    animation
        .children
        .iter()
        .map(|c| doc.get(*c))
        .filter(|c| c.name == "Weight")
        .find_map(|c| float_prop(c, &["Value"]))
        .unwrap_or(DEFAULT_ANIMATION_WEIGHT)
}

pub fn extract_animation_reference(doc: &Document) -> Option<i64> {
    let mut best: Option<&Instance> = None;
    let mut best_weight = f64::NEG_INFINITY;
    for animation in find_by_class(doc, &doc.roots, &["Animation"]).into_iter().map(|i| doc.get(i)) {
        let weight = animation_weight(doc, animation);
        if weight > best_weight {
            best_weight = weight;
            best = Some(animation);
        }
    }
    if best_weight <= 0.0 {
        return None;
    }
    content_asset_id(best?, ANIMATION_ID_PROP_NAMES)
}

pub fn extract_clothing_template(doc: &Document) -> Option<i64> {
    let clothing = doc.get(*find_by_class(doc, &doc.roots, &["Shirt", "Pants"]).first()?);
    content_asset_id(clothing, CLOTHING_TEMPLATE_PROP_NAMES)
}

// ---- curve animations ------------------------------------------------------

#[derive(Clone, Debug)]
struct Channel {
    times: Vec<f64>,
    values: Vec<f64>,
}

type Triple = [Option<Channel>; 3];

fn parse_values_and_times(bytes: &[u8]) -> Option<Channel> {
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()));
    if bytes.len() < 8 {
        return None;
    }
    let count = u32_at(4)? as usize;
    if count == 0 {
        return None;
    }
    let times_at = 8 + count * FLOAT_CURVE_KEY_SIZE;
    if times_at + 8 + count * 4 > bytes.len() || u32_at(times_at + 4)? as usize != count {
        return None;
    }
    let mut channel = Channel { times: Vec::with_capacity(count), values: Vec::with_capacity(count) };
    for key in 0..count {
        channel.values.push(f64::from(f32::from_bits(u32_at(8 + key * FLOAT_CURVE_KEY_SIZE + 2)?)));
        channel.times.push(f64::from(u32_at(times_at + 8 + key * 4)?) / CURVE_TIME_UNITS_PER_SECOND);
    }
    Some(channel)
}

fn read_channel_triple(doc: &Document, curve: &Instance) -> Option<Triple> {
    let channels: Triple = ["X", "Y", "Z"].map(|axis| {
        let child = find_child(doc, curve, |c| c.class_name == "FloatCurve" && c.name == axis)?;
        parse_values_and_times(string_bytes_prop(child, &["ValuesAndTimes"])?)
    });
    channels.iter().any(Option::is_some).then_some(channels)
}

struct PartCurves {
    part: String,
    position: Option<Triple>,
    rotation: Option<Triple>,
}

fn collect_part_curves(doc: &Document, instance: &Instance, out: &mut Vec<PartCurves>, depth: usize) {
    if depth >= MAX_INSTANCE_TREE_DEPTH {
        return;
    }
    for child in instance.children.iter().map(|c| doc.get(*c)) {
        match child.class_name.as_str() {
            "Vector3Curve" => {
                if let Some(position) = read_channel_triple(doc, child) {
                    out.push(PartCurves { part: instance.name.clone(), position: Some(position), rotation: None });
                }
            }
            "EulerRotationCurve" => {
                if let Some(rotation) = read_channel_triple(doc, child) {
                    out.push(PartCurves { part: instance.name.clone(), position: None, rotation: Some(rotation) });
                }
            }
            _ => collect_part_curves(doc, child, out, depth + 1),
        }
    }
}

fn sample_channel(channel: Option<&Channel>, time: f64) -> f64 {
    let Some(Channel { times, values }) = channel.filter(|c| !c.times.is_empty()) else { return 0.0 };
    if time <= times[0] {
        return values[0];
    }
    let last = times.len() - 1;
    if time >= times[last] {
        return values[last];
    }
    let mut index = 0;
    while index < last && times[index + 1] <= time {
        index += 1;
    }
    let span = times[index + 1] - times[index];
    let alpha = if span > 0.0 { (time - times[index]) / span } else { 0.0 };
    values[index] + (values[index + 1] - values[index]) * alpha
}

fn axis_rotation(axis: usize, angle: f64) -> [f64; 9] {
    let (s, c) = angle.sin_cos();
    match axis {
        0 => [1., 0., 0., 0., c, -s, 0., s, c],
        1 => [c, 0., s, 0., 1., 0., -s, 0., c],
        _ => [c, -s, 0., s, c, 0., 0., 0., 1.],
    }
}

fn multiply(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut out = [0.0; 9];
    for row in 0..3 {
        for col in 0..3 {
            out[row * 3 + col] = a[row * 3] * b[col] + a[row * 3 + 1] * b[3 + col] + a[row * 3 + 2] * b[6 + col];
        }
    }
    out
}

fn collect_sample_times(parts: &[PartCurves]) -> Vec<f64> {
    let mut times: Vec<f64> = Vec::new();
    let mut seen = HashSet::new();
    for entry in parts {
        for channel in entry.position.iter().chain(entry.rotation.iter()).flatten().flatten() {
            for time in &channel.times {
                if seen.insert(time.to_bits()) {
                    times.push(*time);
                }
            }
        }
    }
    times.sort_by(f64::total_cmp);
    if times.len() <= MAX_CURVE_KEYFRAMES {
        return times;
    }
    let stride = times.len() as f64 / MAX_CURVE_KEYFRAMES as f64;
    let mut thinned: Vec<f64> = (0..MAX_CURVE_KEYFRAMES).map(|i| times[(i as f64 * stride).floor() as usize]).collect();
    if let Some(last) = times.last().copied() {
        if thinned.last() != Some(&last) {
            thinned.push(last);
        }
    }
    thinned
}

/// Curve poses keep a full-precision rotation (`eulerToRotation` in doubles).
pub struct CurvePose {
    pub part: String,
    pub position: [f64; 3],
    pub rotation: [f64; 9],
}

pub struct CurveKeyframe {
    pub time: f64,
    pub poses: Vec<CurvePose>,
}

pub fn extract_curve_animation(doc: &Document) -> Option<Vec<CurveKeyframe>> {
    let animation = doc.roots.iter().map(|r| doc.get(*r)).find(|r| r.class_name == "CurveAnimation")?;
    let mut collected = Vec::new();
    collect_part_curves(doc, animation, &mut collected, 0);
    let mut parts: Vec<PartCurves> = Vec::new();
    let mut by_part: HashMap<String, usize> = HashMap::new();
    for entry in collected {
        match by_part.get(&entry.part) {
            Some(&i) => {
                let existing = &mut parts[i];
                if existing.position.is_none() {
                    existing.position = entry.position;
                }
                if existing.rotation.is_none() {
                    existing.rotation = entry.rotation;
                }
            }
            None => {
                by_part.insert(entry.part.clone(), parts.len());
                parts.push(entry);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let times = collect_sample_times(&parts);
    if times.len() < 2 {
        return None;
    }
    Some(
        times
            .into_iter()
            .map(|time| CurveKeyframe {
                time,
                poses: parts
                    .iter()
                    .map(|entry| {
                        let position = entry.position.as_ref().map_or([0.0; 3], |p| [0, 1, 2].map(|i| sample_channel(p[i].as_ref(), time)));
                        let rotation = entry.rotation.as_ref().map_or([1., 0., 0., 0., 1., 0., 0., 0., 1.], |r| {
                            let angles = [0, 1, 2].map(|i| sample_channel(r[i].as_ref(), time));
                            multiply(&multiply(&axis_rotation(0, angles[0]), &axis_rotation(1, angles[1])), &axis_rotation(2, angles[2]))
                        });
                        CurvePose { part: entry.part.clone(), position, rotation }
                    })
                    .collect(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_urls_and_floats() {
        assert_eq!(super::parse_content_url("rbxassetid://123"), Some(123));
        assert_eq!(super::parse_content_url("http://www.roblox.com/asset/?id=45"), Some(45));
        assert_eq!(super::parse_content_url("rbxasset://textures/x.png"), None);
        assert_eq!(super::js_parse_float(" 1.5abc"), 1.5);
        assert!(super::js_parse_float("abc").is_nan());
        assert_eq!(super::decode_xml_entities("a&amp;b&lt;"), "a&b<");
    }
}
