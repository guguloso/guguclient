//! Roblox FileMesh parser (`parseFileMesh`): text v1, binary v2–v5 and the
//! chunked v7 container whose core mesh is a Draco stream.

use std::sync::LazyLock;

use regex::Regex;

use super::binary::BinaryReader;

const HEADER_SCAN_LIMIT: usize = 64;
const MESH_MAX_SUBSETS: u32 = 1024;
const FACS_MAX_MATRIX_VALUES: u64 = 4_000_000;
const V1_POSITION_SCALE: f32 = 0.5;
const VERTEX_SIZE_TRUNCATED: usize = 36;
const VERTEX_SIZE_FULL: usize = 40;
const FACE_SIZE: usize = 12;
const LOD_SIZE: usize = 4;
const ENVELOPE_SIZE: usize = 8;
const NULL_BONE_ID: u16 = 65535;
const SUBSET_BONE_SLOTS: usize = 26;
const TANGENT_SCALE: f32 = 127.0;
const V2_HEADER_SIZE: u16 = 12;
const V3_HEADER_SIZE: u16 = 16;
const V4_HEADER_SIZE: u16 = 24;
const V5_HEADER_SIZE: u16 = 32;
const BONE_SIZE: usize = 60;
const SUBSET_SIZE: usize = 72;
const V7_CHUNK_HEADER_SIZE: usize = 16;
const V7_LODS_PREFIX_SIZE: usize = 3;
const FACS_CHANNEL_COUNT: usize = 6;

static V1_VERTEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^,\]]*),([^,\]]*),([^,\]]*)\]\[([^,\]]*),([^,\]]*),([^,\]]*)\]\[([^,\]]*),([^,\]]*),([^,\]]*)\]").expect("regex"));
static V7_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z0-9_]{4,8}$").expect("regex"));

#[derive(Clone, Debug)]
pub struct Bone {
    pub name: String,
    pub parent_index: i32,
    pub rotation: Vec<f32>,
    pub position: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct Facs {
    pub bone_names: Vec<String>,
    pub channels: Vec<Vec<f32>>,
    pub control_names: Vec<String>,
    pub pose_count: u32,
    pub three_pose_correctives: Vec<Vec<u16>>,
    pub two_pose_correctives: Vec<Vec<u16>>,
}

#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub positions: Vec<f32>,
    pub normals: Vec<f32>,
    pub uvs: Vec<f32>,
    pub indices: Vec<u32>,
    pub draco: Option<Vec<u8>>,
    pub lod_offsets: Option<Vec<u32>>,
    pub facs: Option<Facs>,
    pub skin_indices: Option<Vec<u16>>,
    pub skin_weights: Option<Vec<f32>>,
    pub bones: Option<Vec<Bone>>,
}

type MeshResult<T> = Result<T, String>;

struct VertexData {
    positions: Vec<f32>,
    normals: Vec<f32>,
    uvs: Vec<f32>,
}

fn read_vertex_data(r: &mut BinaryReader, count: usize, size: usize) -> MeshResult<VertexData> {
    if size < VERTEX_SIZE_TRUNCATED {
        return Err(format!("Vertex size {size} is below the {VERTEX_SIZE_TRUNCATED} byte minimum"));
    }
    let bytes = count.checked_mul(size).ok_or("Vertex table overflow")?;
    if bytes > r.remaining() {
        return Err(format!("Vertex table needs {bytes} bytes but only {} remain", r.remaining()));
    }
    let has_color = size >= VERTEX_SIZE_FULL;
    let record = if has_color { VERTEX_SIZE_FULL } else { VERTEX_SIZE_TRUNCATED };
    let padding = size - record;
    let (mut positions, mut normals, mut uvs) = (Vec::with_capacity(count * 3), Vec::with_capacity(count * 3), Vec::with_capacity(count * 2));
    for _ in 0..count {
        for _ in 0..3 {
            positions.push(r.f32()?);
        }
        for _ in 0..3 {
            normals.push(r.f32()?);
        }
        uvs.push(r.f32()?);
        uvs.push(r.f32()?);
        for _ in 0..4 {
            let _tangent = f32::from(r.i8()?) / TANGENT_SCALE;
        }
        if has_color {
            r.skip(4)?;
        }
        if padding > 0 {
            r.skip(padding)?;
        }
    }
    if positions.iter().any(|v| !v.is_finite()) {
        return Err("Vertex positions contain non-finite values".into());
    }
    Ok(VertexData { positions, normals, uvs })
}

fn read_face_indices(r: &mut BinaryReader, face_count: usize, face_size: usize, vertex_count: usize) -> MeshResult<Vec<u32>> {
    if face_size < FACE_SIZE {
        return Err(format!("Face size {face_size} is below the {FACE_SIZE} byte minimum"));
    }
    let bytes = face_count.checked_mul(face_size).ok_or("Face table overflow")?;
    if bytes > r.remaining() {
        return Err(format!("Face table needs {bytes} bytes but only {} remain", r.remaining()));
    }
    let padding = face_size - FACE_SIZE;
    let mut indices = Vec::with_capacity(face_count * 3);
    let mut max = 0u32;
    for _ in 0..face_count {
        for _ in 0..3 {
            let i = r.u32()?;
            max = max.max(i);
            indices.push(i);
        }
        if padding > 0 {
            r.skip(padding)?;
        }
    }
    if face_count > 0 && max as usize >= vertex_count {
        return Err(format!("Face index {max} is out of range for {vertex_count} vertices"));
    }
    Ok(indices)
}

fn read_lod_offsets(r: &mut BinaryReader, count: usize, size: usize) -> MeshResult<Vec<u32>> {
    if size < LOD_SIZE {
        return Err(format!("LOD entry size {size} is below the {LOD_SIZE} byte minimum"));
    }
    let mut offsets = Vec::with_capacity(count);
    for _ in 0..count {
        offsets.push(r.u32()?);
        if size > LOD_SIZE {
            r.skip(size - LOD_SIZE)?;
        }
    }
    Ok(offsets)
}

fn slice_lod0(indices: Vec<u32>, lods: &[u32]) -> MeshResult<Vec<u32>> {
    if lods.len() < 2 || lods[1] <= lods[0] {
        return Ok(indices);
    }
    let (begin, end) = (lods[0] as usize * 3, lods[1] as usize * 3);
    if end > indices.len() {
        return Err(format!("LOD0 face range [{}, {}) exceeds {} faces", lods[0], lods[1], indices.len() / 3));
    }
    Ok(indices[begin..end].to_vec())
}

struct RawBone {
    name_offset: u32,
    parent_index: i32,
    rotation: Vec<f32>,
    position: [f32; 3],
}

fn read_raw_bones(r: &mut BinaryReader, count: usize) -> MeshResult<Vec<RawBone>> {
    let mut bones = Vec::with_capacity(count);
    for _ in 0..count {
        let name_offset = r.u32()?;
        let parent = r.u16()?;
        r.skip(6)?;
        let mut rotation = Vec::with_capacity(9);
        for _ in 0..9 {
            rotation.push(r.f32()?);
        }
        let position = [r.f32()?, r.f32()?, r.f32()?];
        bones.push(RawBone { name_offset, parent_index: if parent == NULL_BONE_ID { -1 } else { i32::from(parent) }, rotation, position });
    }
    Ok(bones)
}

fn resolve_bone_names(raw: Vec<RawBone>, names: &[u8]) -> MeshResult<Vec<Bone>> {
    let mut reader = BinaryReader::new(names);
    raw.into_iter()
        .map(|b| {
            reader.seek(b.name_offset as usize)?;
            Ok(Bone { name: reader.cstring()?, parent_index: b.parent_index, rotation: b.rotation, position: b.position })
        })
        .collect()
}

struct Subset {
    verts_begin: u32,
    verts_length: u32,
    bone_indices: Vec<u16>,
}

fn read_subsets(r: &mut BinaryReader, count: u32) -> MeshResult<Vec<Subset>> {
    if count > MESH_MAX_SUBSETS {
        return Err(format!("Mesh declares {count} skinning subsets, above the {MESH_MAX_SUBSETS} cap"));
    }
    let mut subsets = Vec::new();
    for _ in 0..count {
        let _faces_begin = r.u32()?;
        let _faces_length = r.u32()?;
        let verts_begin = r.u32()?;
        let verts_length = r.u32()?;
        let bone_count = r.u32()?;
        let mut bone_indices = Vec::new();
        for slot in 0..SUBSET_BONE_SLOTS {
            let id = r.u16()?;
            if (slot as u32) < bone_count {
                bone_indices.push(id);
            }
        }
        subsets.push(Subset { verts_begin, verts_length, bone_indices });
    }
    Ok(subsets)
}

fn resolve_skin(envelopes: &[u8], subsets: &[Subset], vertex_count: usize) -> (Vec<u16>, Vec<f32>) {
    let mut indices = vec![0u16; vertex_count * 4];
    let mut weights = vec![0f32; vertex_count * 4];
    for v in 0..vertex_count {
        let base = v * ENVELOPE_SIZE;
        let sum: u32 = (4..8).map(|k| u32::from(envelopes[base + k])).sum();
        if sum == 0 {
            continue;
        }
        let subset = subsets.iter().find(|s| (v as u32) >= s.verts_begin && (v as u32) < s.verts_begin + s.verts_length);
        for slot in 0..4 {
            let weight = envelopes[base + 4 + slot];
            if weight == 0 {
                continue;
            }
            let local = envelopes[base + slot];
            let global = match subset {
                Some(s) => s.bone_indices.get(local as usize).copied().unwrap_or(NULL_BONE_ID),
                None => u16::from(local),
            };
            indices[v * 4 + slot] = if global == NULL_BONE_ID { 0 } else { global };
            weights[v * 4 + slot] = f32::from(weight) / sum as f32;
        }
    }
    (indices, weights)
}

fn parse_v2(r: &mut BinaryReader) -> MeshResult<Mesh> {
    let start = r.position();
    let header = r.u16()?;
    if header < V2_HEADER_SIZE {
        return Err(format!("v2 header size {header} is below {V2_HEADER_SIZE} bytes"));
    }
    let vertex_size = r.u8()? as usize;
    let face_size = r.u8()? as usize;
    let vertex_count = r.u32()? as usize;
    let face_count = r.u32()? as usize;
    r.seek(start + header as usize)?;
    let v = read_vertex_data(r, vertex_count, vertex_size)?;
    let indices = read_face_indices(r, face_count, face_size, vertex_count)?;
    Ok(Mesh { positions: v.positions, normals: v.normals, uvs: v.uvs, indices, ..Mesh::default() })
}

fn parse_v3(r: &mut BinaryReader) -> MeshResult<Mesh> {
    let start = r.position();
    let header = r.u16()?;
    if header < V3_HEADER_SIZE {
        return Err(format!("v3 header size {header} is below {V3_HEADER_SIZE} bytes"));
    }
    let vertex_size = r.u8()? as usize;
    let face_size = r.u8()? as usize;
    let lod_size = r.u16()? as usize;
    let lod_count = r.u16()? as usize;
    let vertex_count = r.u32()? as usize;
    let face_count = r.u32()? as usize;
    r.seek(start + header as usize)?;
    let v = read_vertex_data(r, vertex_count, vertex_size)?;
    let all = read_face_indices(r, face_count, face_size, vertex_count)?;
    let lods = read_lod_offsets(r, lod_count, lod_size)?;
    Ok(Mesh { positions: v.positions, normals: v.normals, uvs: v.uvs, indices: slice_lod0(all, &lods)?, ..Mesh::default() })
}

fn read_name_blob(r: &mut BinaryReader, size: usize) -> MeshResult<Vec<String>> {
    Ok(String::from_utf8_lossy(r.bytes(size)?).split('\0').filter(|n| !n.is_empty()).map(str::to_owned).collect())
}

fn read_quantized_matrix(r: &mut BinaryReader) -> MeshResult<Option<(u32, u32, Vec<f32>)>> {
    let version = r.u16()?;
    let rows = r.u32()?;
    let cols = r.u32()?;
    let count = u64::from(rows) * u64::from(cols);
    if count > FACS_MAX_MATRIX_VALUES {
        return Ok(None);
    }
    let count = count as usize;
    if version == 1 {
        if count * 4 > r.remaining() {
            return Ok(None);
        }
        let values = (0..count).map(|_| r.f32()).collect::<MeshResult<Vec<_>>>()?;
        return Ok(Some((rows, cols, values)));
    }
    if version != 2 {
        return Ok(None);
    }
    let min = r.f32()?;
    let max = r.f32()?;
    if count * 2 > r.remaining() {
        return Ok(None);
    }
    let scale = (max - min) / 65535.0;
    let values = (0..count).map(|_| r.u16().map(|q| min + f32::from(q) * scale)).collect::<MeshResult<Vec<_>>>()?;
    Ok(Some((rows, cols, values)))
}

fn read_correctives(r: &mut BinaryReader, size: usize, slots: usize) -> MeshResult<Vec<Vec<u16>>> {
    let stride = slots * 2;
    let count = size / stride;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push((0..slots).map(|_| r.u16()).collect::<MeshResult<Vec<_>>>()?);
    }
    r.skip(size - count * stride)?;
    Ok(out)
}

/// `readFacsData`: `None` (block skipped) when malformed.
fn read_facs(r: &mut BinaryReader, size: usize) -> MeshResult<Option<Facs>> {
    let start = r.position();
    let bone_names_size = r.u32()? as usize;
    let control_names_size = r.u32()? as usize;
    let transforms_size = r.u32()? as usize;
    r.skip(4)?;
    let two_pose_size = r.u32()? as usize;
    let three_pose_size = r.u32()? as usize;
    let bone_names = read_name_blob(r, bone_names_size)?;
    let control_names = read_name_blob(r, control_names_size)?;
    let transforms_start = r.position();
    let mut channels = Vec::new();
    let (mut rows, mut cols) = (0, 0);
    for channel in 0..FACS_CHANNEL_COUNT {
        let Some((mr, mc, values)) = read_quantized_matrix(r)? else {
            r.seek(start + size)?;
            return Ok(None);
        };
        if channel == 0 {
            (rows, cols) = (mr, mc);
        } else if mr != rows || mc != cols {
            r.seek(start + size)?;
            return Ok(None);
        }
        channels.push(values);
    }
    r.seek(transforms_start + transforms_size)?;
    let two = read_correctives(r, two_pose_size, 2)?;
    let three = read_correctives(r, three_pose_size, 3)?;
    r.seek(start + size)?;
    if rows as usize != bone_names.len() || (cols as usize) < control_names.len() {
        return Ok(None);
    }
    Ok(Some(Facs { bone_names, channels, control_names, pose_count: cols, three_pose_correctives: three, two_pose_correctives: two }))
}

fn parse_v4(r: &mut BinaryReader, has_facs: bool) -> MeshResult<Mesh> {
    let start = r.position();
    let header = r.u16()?;
    let min = if has_facs { V5_HEADER_SIZE } else { V4_HEADER_SIZE };
    if header < min {
        return Err(format!("v4 header size {header} is below {min} bytes"));
    }
    r.skip(2)?;
    let vertex_count = r.u32()? as usize;
    let face_count = r.u32()? as usize;
    let lod_count = r.u16()? as usize;
    let bone_count = r.u16()? as usize;
    let bone_names_size = r.u32()? as usize;
    let subset_count = u32::from(r.u16()?);
    r.skip(2)?;
    let facs_size = if has_facs {
        r.skip(4)?;
        r.u32()? as usize
    } else {
        0
    };
    r.seek(start + header as usize)?;
    let v = read_vertex_data(r, vertex_count, VERTEX_SIZE_FULL)?;
    let envelopes = if bone_count > 0 { Some(r.bytes(vertex_count * ENVELOPE_SIZE)?.to_vec()) } else { None };
    let all = read_face_indices(r, face_count, FACE_SIZE, vertex_count)?;
    let lods = read_lod_offsets(r, lod_count, LOD_SIZE)?;
    let raw_bones = read_raw_bones(r, bone_count)?;
    let names = r.bytes(bone_names_size)?.to_vec();
    let subsets = read_subsets(r, subset_count)?;
    let facs = if facs_size > 0 { read_facs(r, facs_size)? } else { None };
    let mut mesh = Mesh { positions: v.positions, normals: v.normals, uvs: v.uvs, indices: slice_lod0(all, &lods)?, facs, ..Mesh::default() };
    if let Some(envelopes) = envelopes {
        let (si, sw) = resolve_skin(&envelopes, &subsets, vertex_count);
        mesh.skin_indices = Some(si);
        mesh.skin_weights = Some(sw);
        mesh.bones = Some(resolve_bone_names(raw_bones, &names)?);
    }
    Ok(mesh)
}

fn read_lods_chunk(payload: &[u8]) -> Option<Vec<u32>> {
    if payload.len() < V7_LODS_PREFIX_SIZE + 4 {
        return None;
    }
    let at = |e: usize| V7_LODS_PREFIX_SIZE + 4 * (e + 1);
    let u32_at = |o: usize| u32::from_le_bytes(payload[o..o + 4].try_into().unwrap_or_default());
    let count = u32_at(V7_LODS_PREFIX_SIZE) as usize;
    if count < 2 || at(count) > payload.len() {
        return None;
    }
    let offsets: Vec<u32> = (0..count).map(|e| u32_at(at(e))).collect();
    (offsets[1] > offsets[0]).then_some(offsets)
}

struct Skinning {
    bones: Vec<Bone>,
    skin_indices: Vec<u16>,
    skin_weights: Vec<f32>,
}

fn read_skinning_chunk(payload: &[u8]) -> MeshResult<Option<Skinning>> {
    let mut r = BinaryReader::new(payload);
    let vertex_count = r.u32()? as usize;
    if vertex_count == 0 || vertex_count * ENVELOPE_SIZE > r.remaining() {
        return Ok(None);
    }
    let envelopes = r.bytes(vertex_count * ENVELOPE_SIZE)?.to_vec();
    let bone_count = r.u32()? as usize;
    if bone_count == 0 || bone_count * BONE_SIZE > r.remaining() {
        return Ok(None);
    }
    let raw = read_raw_bones(&mut r, bone_count)?;
    let names_size = r.u32()? as usize;
    if names_size > r.remaining() {
        return Ok(None);
    }
    let names = r.bytes(names_size)?.to_vec();
    let subset_count = r.u32()?;
    if subset_count as usize * SUBSET_SIZE > r.remaining() {
        return Ok(None);
    }
    let subsets = read_subsets(&mut r, subset_count)?;
    let (skin_indices, skin_weights) = resolve_skin(&envelopes, &subsets, vertex_count);
    Ok(Some(Skinning { bones: resolve_bone_names(raw, &names)?, skin_indices, skin_weights }))
}

fn parse_v7(r: &mut BinaryReader) -> MeshResult<Mesh> {
    let tag = r.string(8)?;
    if tag != "COREMESH" {
        return Err(format!("Expected \"COREMESH\" chunk, found \"{tag}\""));
    }
    r.skip(4)?;
    let core_size = r.u32()? as i64;
    let draco_len = r.u32()? as usize;
    let draco = r.bytes(draco_len)?.to_vec();
    if !draco.starts_with(b"DRACO") {
        return Err("COREMESH chunk does not hold a Draco stream".into());
    }
    let trailing = core_size - 4 - draco_len as i64;
    if trailing < 0 {
        return Err(format!("COREMESH size {core_size} is smaller than its Draco stream"));
    }
    r.skip(trailing as usize)?;
    let mut mesh = Mesh { draco: Some(draco), ..Mesh::default() };
    while r.remaining() >= V7_CHUNK_HEADER_SIZE {
        let tag = r.string(8)?.trim_end_matches('\0').to_owned();
        if !V7_TAG.is_match(&tag) {
            break;
        }
        r.skip(4)?;
        let size = r.u32()? as usize;
        if size > r.remaining() {
            break;
        }
        let payload = r.bytes(size)?;
        match tag.as_str() {
            "LODS" => mesh.lod_offsets = read_lods_chunk(payload),
            "SKINNING" => {
                if let Some(s) = read_skinning_chunk(payload)? {
                    mesh.bones = Some(s.bones);
                    mesh.skin_indices = Some(s.skin_indices);
                    mesh.skin_weights = Some(s.skin_weights);
                } else {
                    mesh.bones = None;
                    mesh.skin_indices = None;
                    mesh.skin_weights = None;
                }
            }
            "FACS" => {
                let mut fr = BinaryReader::new(payload);
                let size = fr.u32()? as usize;
                mesh.facs = if size > fr.remaining() { None } else { read_facs(&mut fr, size)? };
            }
            _ => {}
        }
    }
    Ok(mesh)
}

fn parse_component(raw: &str) -> MeshResult<f32> {
    let t = raw.trim();
    t.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v as f32).ok_or_else(|| format!("Invalid vertex component \"{raw}\""))
}

fn parse_text_v1(text: &str, version: &str) -> MeshResult<Mesh> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < 2 {
        return Err("Text mesh is missing its face count".into());
    }
    let face_count: usize = lines[1].trim().parse().map_err(|_| format!("Invalid text mesh face count \"{}\"", lines[1].trim()))?;
    let vertex_count = face_count * 3;
    let data = lines[2..].join("\n");
    let matches: Vec<_> = V1_VERTEX.captures_iter(&data).collect();
    if matches.len() != vertex_count {
        return Err(format!("Text mesh declares {vertex_count} vertices but has {}", matches.len()));
    }
    let scale = if version == "1.00" { V1_POSITION_SCALE } else { 1.0 };
    let mut m = Mesh::default();
    for c in &matches {
        for k in 1..=3 {
            m.positions.push(parse_component(&c[k])? * scale);
        }
        for k in 4..=6 {
            m.normals.push(parse_component(&c[k])?);
        }
        m.uvs.push(parse_component(&c[7])?);
        m.uvs.push(1.0 - parse_component(&c[8])?);
    }
    m.indices = (0..vertex_count as u32).collect();
    Ok(m)
}

/// `parseFileMesh`.
pub fn parse_file_mesh(bytes: &[u8]) -> MeshResult<Mesh> {
    let scan = &bytes[..bytes.len().min(HEADER_SCAN_LIMIT)];
    let newline = scan.iter().position(|b| *b == b'\n').ok_or("Missing FileMesh version header line")?;
    let line = String::from_utf8_lossy(&bytes[..newline]).trim_end().to_owned();
    let version = line
        .strip_prefix("version ")
        .filter(|v| {
            let mut parts = v.split('.');
            matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()))
        })
        .ok_or_else(|| format!("Invalid FileMesh version header line \"{line}\""))?
        .to_owned();
    let reader = || {
        let mut r = BinaryReader::new(bytes);
        r.seek(newline + 1).map(|_| r)
    };
    match version.as_str() {
        "1.00" | "1.01" => parse_text_v1(&String::from_utf8_lossy(bytes), &version),
        "2.00" => parse_v2(&mut reader()?),
        "3.00" | "3.01" => parse_v3(&mut reader()?),
        "4.00" | "4.01" => parse_v4(&mut reader()?, false),
        "5.00" => parse_v4(&mut reader()?, true),
        "7.00" => parse_v7(&mut reader()?),
        other => Err(format!("Unsupported FileMesh version {other}")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_mesh() {
        let text = "version 1.00\n1\n[1,2,3][0,1,0][0.5,0.25,0][4,5,6][0,1,0][0,0,0][7,8,9][0,1,0][1,1,0]";
        let m = super::parse_file_mesh(text.as_bytes()).unwrap();
        assert_eq!(m.positions[..3], [0.5, 1.0, 1.5]);
        assert_eq!(m.uvs[..2], [0.5, 0.75]);
        assert_eq!(m.indices, vec![0, 1, 2]);
        assert!(super::parse_file_mesh(b"version 9.00\n").is_err());
    }
}
