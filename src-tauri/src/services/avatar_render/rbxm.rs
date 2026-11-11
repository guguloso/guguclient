//! Binary Roblox model (`.rbxm`) parser (`parseRbxm`): chunks (LZ4 or zstd),
//! classes, properties and the parent tree.

use std::collections::HashMap;
use std::io::Read;

use super::binary::BinaryReader;

const RBXM_MAGIC: &[u8] = b"<roblox!";
const RBXM_SIGNATURE: [u8; 6] = [137, 255, 13, 10, 26, 10];
const ZSTD_MAGIC: [u8; 4] = [40, 181, 47, 253];
const LZ4_MAX_EXPANSION_RATIO: usize = 255;
const MAX_CHUNK_BYTES: usize = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_INSTANCES: usize = 100_000;
const CONTENT_SOURCE_TYPE_URI: i32 = 1;

#[derive(Clone, Debug)]
pub struct CFrame {
    pub position: [f32; 3],
    pub rotation: Vec<f32>,
}

/// Every property type the parser decodes; some are read only to keep the
/// stream aligned and carry no consumer yet.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Prop {
    Str { bytes: Vec<u8>, value: String },
    Bool(bool),
    Int32(i32),
    Float32(f32),
    Float64(f64),
    BrickColor(u32),
    Color3([f32; 3]),
    Vector2([f32; 2]),
    Vector3([f32; 3]),
    CFrame(CFrame),
    Enum(u32),
    Referent(i32),
    Color3Uint8([u8; 3]),
    Int64(i64),
    SharedString(String),
    OptionalCFrame(Option<CFrame>),
    Content(String),
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub class_name: String,
    pub name: String,
    pub props: HashMap<String, Prop>,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
}

/// The parsed tree; instances live in an arena indexed by `usize`.
#[derive(Debug, Default)]
pub struct Document {
    pub instances: Vec<Instance>,
    pub roots: Vec<usize>,
}

impl Document {
    pub fn get(&self, index: usize) -> &Instance {
        &self.instances[index]
    }
}

type RbxmResult<T> = Result<T, String>;

fn ext_byte(input: &[u8], pos: usize) -> RbxmResult<u8> {
    input.get(pos).copied().ok_or_else(|| "LZ4 length extension extends past the end of the compressed data".to_owned())
}

/// `decompressLz4Block`.
fn decompress_lz4(input: &[u8], out_len: usize) -> RbxmResult<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let (mut ip, mut op) = (0usize, 0usize);
    while ip < input.len() {
        let token = input[ip];
        ip += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let e = ext_byte(input, ip)?;
                ip += 1;
                lit += e as usize;
                if e != 255 {
                    break;
                }
            }
        }
        if ip + lit > input.len() {
            return Err("LZ4 literal run extends past the end of the compressed data".into());
        }
        if op + lit > out_len {
            return Err("LZ4 literal run overflows the decompressed buffer".into());
        }
        out[op..op + lit].copy_from_slice(&input[ip..ip + lit]);
        ip += lit;
        op += lit;
        if ip >= input.len() {
            break;
        }
        if ip + 2 > input.len() {
            return Err("LZ4 match offset extends past the end of the compressed data".into());
        }
        let offset = input[ip] as usize | (input[ip + 1] as usize) << 8;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(format!("LZ4 match offset {offset} points outside the decompressed data"));
        }
        let mut len = (token & 15) as usize + 4;
        if token & 15 == 15 {
            loop {
                let e = ext_byte(input, ip)?;
                ip += 1;
                len += e as usize;
                if e != 255 {
                    break;
                }
            }
        }
        if op + len > out_len {
            return Err("LZ4 match copy overflows the decompressed buffer".into());
        }
        let mut mp = op - offset;
        for _ in 0..len {
            out[op] = out[mp];
            op += 1;
            mp += 1;
        }
    }
    if op != out_len {
        return Err(format!("LZ4 block decompressed to {op} bytes, expected {out_len}"));
    }
    Ok(out)
}

fn decompress_chunk(compressed: &[u8], out_len: usize) -> RbxmResult<Vec<u8>> {
    if out_len > MAX_CHUNK_BYTES {
        return Err(format!("Chunk declares {out_len} uncompressed bytes, above the {MAX_CHUNK_BYTES} byte cap"));
    }
    if !compressed.starts_with(&ZSTD_MAGIC) {
        if out_len > compressed.len() * LZ4_MAX_EXPANSION_RATIO {
            return Err(format!(
                "LZ4 chunk declares {out_len} uncompressed bytes from {} compressed bytes, beyond the format's {LZ4_MAX_EXPANSION_RATIO}x bound",
                compressed.len()
            ));
        }
        return decompress_lz4(compressed, out_len);
    }
    let mut source = compressed;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut source).map_err(|e| format!("Failed to decompress zstd chunk: {e}"))?;
    let mut payload = Vec::with_capacity(out_len);
    decoder.by_ref().take(out_len as u64 + 1).read_to_end(&mut payload).map_err(|e| format!("Failed to decompress zstd chunk: {e}"))?;
    if payload.len() != out_len {
        return Err(format!("zstd chunk decompressed to {} bytes, expected {out_len}", payload.len()));
    }
    Ok(payload)
}

fn deinterleave(bytes: &[u8], size: usize) -> Vec<u8> {
    let count = bytes.len() / size;
    let mut out = vec![0u8; bytes.len()];
    for v in 0..count {
        for b in 0..size {
            out[v * size + b] = bytes[b * count + v];
        }
    }
    out
}

fn interleaved_u32(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<u32>> {
    let bytes = deinterleave(r.bytes(count * 4)?, 4);
    Ok((0..count).map(|i| u32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap_or_default())).collect())
}

fn zigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn interleaved_i32(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<i32>> {
    Ok(interleaved_u32(r, count)?.into_iter().map(zigzag).collect())
}

fn rotated_f32(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<f32>> {
    Ok(interleaved_u32(r, count)?.into_iter().map(|v| f32::from_bits(v.rotate_right(1))).collect())
}

fn referents(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<i32>> {
    let mut values = interleaved_i32(r, count)?;
    for i in 1..values.len() {
        values[i] = values[i].wrapping_add(values[i - 1]);
    }
    Ok(values)
}

fn read_string(r: &mut BinaryReader) -> RbxmResult<String> {
    let len = r.u32()? as usize;
    r.string(len)
}

fn triples(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<[f32; 3]>> {
    let (x, y, z) = (rotated_f32(r, count)?, rotated_f32(r, count)?, rotated_f32(r, count)?);
    Ok((0..count).map(|i| [x[i], y[i], z[i]]).collect())
}

/// `CFRAME_ORIENTATION_ROTATIONS`.
fn orientation(id: u8) -> Option<[f32; 9]> {
    Some(match id {
        2 => [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        3 => [1., 0., 0., 0., 0., -1., 0., 1., 0.],
        5 => [1., 0., 0., 0., -1., 0., 0., 0., -1.],
        6 => [1., 0., 0., 0., 0., 1., 0., -1., 0.],
        7 => [0., 1., 0., 1., 0., 0., 0., 0., -1.],
        9 => [0., 0., 1., 1., 0., 0., 0., 1., 0.],
        10 => [0., -1., 0., 1., 0., 0., 0., 0., 1.],
        12 => [0., 0., -1., 1., 0., 0., 0., -1., 0.],
        13 => [0., 1., 0., 0., 0., 1., 1., 0., 0.],
        14 => [0., 0., -1., 0., 1., 0., 1., 0., 0.],
        16 => [0., -1., 0., 0., 0., -1., 1., 0., 0.],
        17 => [0., 0., 1., 0., -1., 0., 1., 0., 0.],
        20 => [-1., 0., 0., 0., 1., 0., 0., 0., -1.],
        21 => [-1., 0., 0., 0., 0., 1., 0., 1., 0.],
        23 => [-1., 0., 0., 0., -1., 0., 0., 0., 1.],
        24 => [-1., 0., 0., 0., 0., -1., 0., -1., 0.],
        25 => [0., 1., 0., -1., 0., 0., 0., 0., 1.],
        27 => [0., 0., -1., -1., 0., 0., 0., 1., 0.],
        28 => [0., -1., 0., -1., 0., 0., 0., 0., -1.],
        30 => [0., 0., 1., -1., 0., 0., 0., -1., 0.],
        31 => [0., 1., 0., 0., 0., -1., -1., 0., 0.],
        32 => [0., 0., 1., 0., 1., 0., -1., 0., 0.],
        34 => [0., -1., 0., 0., 0., 1., -1., 0., 0.],
        35 => [0., 0., -1., 0., -1., 0., -1., 0., 0.],
        _ => return None,
    })
}

fn cframes(r: &mut BinaryReader, count: usize) -> RbxmResult<Vec<CFrame>> {
    let mut rotations = Vec::with_capacity(count);
    for _ in 0..count {
        let id = r.u8()?;
        if id == 0 {
            rotations.push((0..9).map(|_| r.f32()).collect::<RbxmResult<Vec<_>>>()?);
        } else {
            rotations.push(orientation(id).ok_or_else(|| format!("Unknown CFrame orientation id 0x{id:x}"))?.to_vec());
        }
    }
    let positions = triples(r, count)?;
    Ok(rotations.into_iter().zip(positions).map(|(rotation, position)| CFrame { position, rotation }).collect())
}

fn read_values(r: &mut BinaryReader, type_id: u8, count: usize, shared: &[String]) -> RbxmResult<Option<Vec<Prop>>> {
    Ok(Some(match type_id {
        1 => (0..count)
            .map(|_| {
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                Ok(Prop::Str { value: String::from_utf8_lossy(&bytes).into_owned(), bytes })
            })
            .collect::<RbxmResult<_>>()?,
        2 => r.bytes(count)?.iter().map(|b| Prop::Bool(*b != 0)).collect(),
        3 => interleaved_i32(r, count)?.into_iter().map(Prop::Int32).collect(),
        4 => rotated_f32(r, count)?.into_iter().map(Prop::Float32).collect(),
        5 => (0..count).map(|_| r.f64().map(Prop::Float64)).collect::<RbxmResult<_>>()?,
        11 => interleaved_u32(r, count)?.into_iter().map(Prop::BrickColor).collect(),
        12 => triples(r, count)?.into_iter().map(Prop::Color3).collect(),
        13 => {
            let (x, y) = (rotated_f32(r, count)?, rotated_f32(r, count)?);
            (0..count).map(|i| Prop::Vector2([x[i], y[i]])).collect()
        }
        14 => triples(r, count)?.into_iter().map(Prop::Vector3).collect(),
        16 => cframes(r, count)?.into_iter().map(Prop::CFrame).collect(),
        18 => interleaved_u32(r, count)?.into_iter().map(Prop::Enum).collect(),
        19 => referents(r, count)?.into_iter().map(Prop::Referent).collect(),
        26 => {
            let (red, green, blue) = (r.bytes(count)?.to_vec(), r.bytes(count)?.to_vec(), r.bytes(count)?.to_vec());
            (0..count).map(|i| Prop::Color3Uint8([red[i], green[i], blue[i]])).collect()
        }
        27 => {
            let bytes = deinterleave(r.bytes(count * 8)?, 8);
            (0..count)
                .map(|i| {
                    let encoded = u64::from_be_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap_or_default());
                    Prop::Int64(((encoded >> 1) as i64) ^ -((encoded & 1) as i64))
                })
                .collect()
        }
        28 => interleaved_u32(r, count)?
            .into_iter()
            .map(|i| {
                shared
                    .get(i as usize)
                    .cloned()
                    .map(Prop::SharedString)
                    .ok_or_else(|| format!("SharedString index {i} is outside the SSTR table of {} entries", shared.len()))
            })
            .collect::<RbxmResult<_>>()?,
        30 => {
            let marker = r.u8()?;
            if marker != 16 {
                return Err(format!("OptionalCFrame block starts with type id 0x{marker:x} instead of the CFrame type id"));
            }
            let values = cframes(r, count)?;
            let marker = r.u8()?;
            if marker != 2 {
                return Err(format!("OptionalCFrame presence block starts with type id 0x{marker:x} instead of the Bool type id"));
            }
            let present = r.bytes(count)?.to_vec();
            values.into_iter().zip(present).map(|(c, p)| Prop::OptionalCFrame((p != 0).then_some(c))).collect()
        }
        34 => {
            let sources = interleaved_i32(r, count)?;
            let uri_count = r.u32()? as usize;
            let uris = (0..uri_count).map(|_| read_string(r)).collect::<RbxmResult<Vec<_>>>()?;
            let objects = r.u32()? as usize;
            referents(r, objects)?;
            let external = r.u32()? as usize;
            referents(r, external)?;
            let mut uri = 0;
            sources
                .into_iter()
                .map(|s| {
                    if s == CONTENT_SOURCE_TYPE_URI && uri < uris.len() {
                        uri += 1;
                        Prop::Content(uris[uri - 1].clone())
                    } else {
                        Prop::Content(String::new())
                    }
                })
                .collect()
        }
        _ => return Ok(None),
    }))
}

/// `parseRbxm`.
pub fn parse_rbxm(bytes: &[u8]) -> RbxmResult<Document> {
    parse_document(bytes).map_err(|e| if e.contains("exceeds buffer") || e.contains("outside buffer") { format!("Unexpected end of rbxm data: {e}") } else { e })
}

fn parse_document(bytes: &[u8]) -> RbxmResult<Document> {
    let mut r = BinaryReader::new(bytes);
    if r.bytes(RBXM_MAGIC.len())? != RBXM_MAGIC {
        return Err("Missing rbxm binary magic <roblox!".into());
    }
    if r.bytes(6)? != RBXM_SIGNATURE {
        return Err("Invalid rbxm binary signature".into());
    }
    let version = r.u16()?;
    if version != 0 {
        return Err(format!("Unsupported rbxm container version {version}"));
    }
    r.skip(16)?;
    let mut doc = Document::default();
    let mut by_ref: HashMap<i32, usize> = HashMap::new();
    let mut classes: HashMap<u32, Vec<i32>> = HashMap::new();
    let mut shared: Vec<String> = Vec::new();
    let mut links: Option<(Vec<i32>, Vec<i32>)> = None;
    let (mut reached_end, mut total) = (false, 0usize);
    while !reached_end && r.remaining() > 0 {
        let raw_name = r.bytes(4)?.to_vec();
        let compressed_len = r.u32()? as usize;
        let uncompressed_len = r.u32()? as usize;
        r.skip(4)?;
        let name_end = raw_name.iter().position(|b| *b == 0).unwrap_or(4);
        let name = String::from_utf8_lossy(&raw_name[..name_end]).into_owned();
        let payload = if compressed_len == 0 { r.bytes(uncompressed_len)?.to_vec() } else { decompress_chunk(r.bytes(compressed_len)?, uncompressed_len)? };
        total += payload.len();
        if total > MAX_TOTAL_BYTES {
            return Err(format!("File decompresses past the {MAX_TOTAL_BYTES} byte budget"));
        }
        let mut p = BinaryReader::new(&payload);
        match name.as_str() {
            "END" => reached_end = true,
            "SSTR" => {
                let v = p.u32()?;
                if v != 0 {
                    return Err(format!("Unsupported SSTR chunk version {v}"));
                }
                for _ in 0..p.u32()? {
                    p.skip(16)?;
                    shared.push(read_string(&mut p)?);
                }
            }
            "INST" => {
                let class_id = p.u32()?;
                let class_name = read_string(&mut p)?;
                let format = p.u8()?;
                if format > 1 {
                    return Err(format!("Unknown INST object format {format} for class {class_name}"));
                }
                let count = p.u32()? as usize;
                let refs = referents(&mut p, count)?;
                if format == 1 {
                    p.skip(count)?;
                }
                if classes.contains_key(&class_id) {
                    return Err(format!("Duplicate INST chunk for class id {class_id}"));
                }
                for referent in &refs {
                    let instance = Instance { class_name: class_name.clone(), name: String::new(), props: HashMap::new(), children: Vec::new(), parent: None };
                    match by_ref.get(referent) {
                        Some(&i) => doc.instances[i] = instance,
                        None => {
                            by_ref.insert(*referent, doc.instances.len());
                            doc.instances.push(instance);
                        }
                    }
                }
                classes.insert(class_id, refs);
                if by_ref.len() > MAX_INSTANCES {
                    return Err(format!("File declares more than {MAX_INSTANCES} instances"));
                }
            }
            "PROP" => {
                let class_id = p.u32()?;
                let refs = classes.get(&class_id).cloned().ok_or_else(|| format!("PROP chunk references undeclared class id {class_id}"))?;
                let prop_name = read_string(&mut p)?;
                let type_id = p.u8()?;
                let Some(values) = read_values(&mut p, type_id, refs.len(), &shared)? else { continue };
                for (referent, value) in refs.iter().zip(values) {
                    let Some(&i) = by_ref.get(referent) else { continue };
                    if prop_name == "Name" {
                        if let Prop::Str { value: name, .. } = &value {
                            doc.instances[i].name = name.clone();
                        }
                    }
                    doc.instances[i].props.insert(prop_name.clone(), value);
                }
            }
            "PRNT" => {
                if links.is_some() {
                    return Err("File contains more than one PRNT chunk".into());
                }
                let v = p.u8()?;
                if v != 0 {
                    return Err(format!("Unsupported PRNT chunk version {v}"));
                }
                let count = p.u32()? as usize;
                links = Some((referents(&mut p, count)?, referents(&mut p, count)?));
            }
            _ => {}
        }
    }
    if !reached_end {
        return Err("File is missing the END chunk".into());
    }
    if let Some((children, parents)) = links {
        for (child_ref, parent_ref) in children.iter().zip(parents) {
            let &child = by_ref.get(child_ref).ok_or_else(|| format!("PRNT chunk references unknown instance {child_ref}"))?;
            if doc.instances[child].parent.is_some() {
                return Err(format!("PRNT chunk assigns instance {child_ref} more than one parent"));
            }
            if let Some(&parent) = by_ref.get(&parent_ref) {
                if parent != child {
                    doc.instances[child].parent = Some(parent);
                    doc.instances[parent].children.push(child);
                }
            }
        }
    }
    doc.roots = (0..doc.instances.len()).filter(|i| doc.instances[*i].parent.is_none()).collect();
    Ok(doc)
}

#[cfg(test)]
mod tests {
    #[test]
    fn lz4_and_zigzag() {
        // literal "abc" then match offset 3 len 4 → "abcabca"
        let block = [0x30, b'a', b'b', b'c', 3, 0];
        assert_eq!(super::decompress_lz4(&block, 7).unwrap(), b"abcabca");
        assert_eq!(super::zigzag(1), -1);
        assert_eq!(super::zigzag(4), 2);
    }
}
