//! Per-instance content mods (`mods:*`) and the modded runtime used at launch.
//! Port of `ModsService` and `ModRuntimeService`: packages live in
//! `<instance>/mods/<uuid>/`, `mods-manifest.json` orders them, and a launch
//! with enabled mods or a custom font runs from
//! `<instance>/runtime/<versionHash>/`, a copy of the build with the overlays
//! applied (reused while its fingerprint matches).

use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::fonts::custom_font_path;
use crate::services::instances::Instance;
use crate::services::roblox_layout::{self as layout, instance_config_dir, read_json, write_json};
use crate::state::AppState;
use crate::util::now_iso;

const MANIFEST_SCHEMA_VERSION: u64 = 1;
const MOD_METADATA_FILENAME: &str = "mod.json";
const ALLOWED_TOP_LEVEL_DIRS: [&str; 3] = ["content", "extracontent", "platformcontent"];
const IMPORT_EXTENSIONS: [&str; 18] =
    [".csv", ".dds", ".dict", ".jpg", ".json", ".mesh", ".mp3", ".ogg", ".otf", ".placeholder", ".png", ".rbxl", ".rbxm", ".rbxmx", ".tex", ".ttf", ".txt", ".md"];
const RUNTIME_EXTENSIONS: [&str; 18] =
    [".csv", ".dds", ".dict", ".jpg", ".js", ".json", ".mesh", ".mp3", ".ogg", ".otf", ".placeholder", ".png", ".rbxl", ".rbxm", ".rbxmx", ".tex", ".ttf", ".txt"];
const RUNTIME_METADATA_SCHEMA_VERSION: u64 = 1;
const RUNTIME_METADATA_FILENAME: &str = ".gugu-for-roblox-runtime.json";
const CUSTOM_FONT_ASSET_PATH: &str = "rbxasset://fonts/CustomFont.ttf";
const NO_OVERLAY_FILES: &str = "No valid overlay files found. Use content/, ExtraContent/, or PlatformContent/ paths.";

fn mods_dir(state: &AppState, id: &str) -> AppResult<PathBuf> {
    Ok(instance_config_dir(&state.paths.base, id, "player")?.join("mods"))
}

fn manifest_path(state: &AppState, id: &str) -> AppResult<PathBuf> {
    Ok(instance_config_dir(&state.paths.base, id, "player")?.join("mods-manifest.json"))
}

/// Primary-strength ordering close to ICU's root collation (`localeCompare`):
/// punctuation, then digits, then letters, ignoring case first.
fn collation_key(c: char) -> (u8, u32) {
    const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if c.is_whitespace() {
        return (0, c as u32);
    }
    if let Some(i) = PUNCTUATION.find(c) {
        return (1, i as u32);
    }
    if c.is_ascii_digit() {
        return (2, c as u32);
    }
    (3, c.to_lowercase().next().unwrap_or(c) as u32)
}

pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let primary = a.chars().map(collation_key).cmp(b.chars().map(collation_key));
    primary.then_with(|| {
        // Tertiary: lowercase sorts before uppercase.
        a.chars().map(|c| c.is_uppercase()).cmp(b.chars().map(|c| c.is_uppercase()))
    })
}

fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

struct FileRecord {
    relative: String,
    absolute: PathBuf,
    size: u64,
}

/// `collectPackageFiles`: `None` when the package contains a symlink.
fn collect_files(root: &Path) -> Option<Vec<FileRecord>> {
    let mut files = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let abs = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let Ok(entries) = std::fs::read_dir(&abs) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let next = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                return None;
            }
            if kind.is_dir() {
                stack.push(next);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(FileRecord { relative: next.replace('\\', "/"), absolute: entry.path(), size });
        }
    }
    Some(files)
}

fn is_metadata_candidate(relative: &str) -> bool {
    let n = relative.replace('\\', "/").to_lowercase();
    n == MOD_METADATA_FILENAME || n.ends_with(&format!("/{MOD_METADATA_FILENAME}"))
}

fn extract_metadata(files: &[FileRecord]) -> Map<String, Value> {
    for file in files.iter().filter(|f| is_metadata_candidate(&f.relative)) {
        match std::fs::read_to_string(&file.absolute).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
            Ok(parsed) => {
                let mut out = Map::new();
                for key in ["name", "author", "version", "description"] {
                    if let Some(v) = parsed.get(key).filter(|v| v.is_string()) {
                        out.insert(key.into(), v.clone());
                    }
                }
                return out;
            }
            Err(error) => tracing::warn!("Failed to parse mod metadata at {}: {error}", file.relative),
        }
    }
    Map::new()
}

fn detect_wrapper_prefix(files: &[&FileRecord]) -> Option<String> {
    let first = files.first()?.relative.split('/').next()?.to_owned();
    if first.is_empty() || ALLOWED_TOP_LEVEL_DIRS.contains(&first.to_lowercase().as_str()) {
        return None;
    }
    files.iter().all(|f| f.relative.split('/').next() == Some(first.as_str())).then_some(first)
}

/// `normalizeOverlayTargetPath` (import-time).
fn overlay_target(relative: &str, wrapper: Option<&str>) -> Option<String> {
    let normalized = relative.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    let mut segments: Vec<&str> = normalized.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if let Some(w) = wrapper {
        if segments.first().is_some_and(|s| s.eq_ignore_ascii_case(w)) {
            segments.remove(0);
        }
    }
    if segments.is_empty() || segments.join("/").to_lowercase() == MOD_METADATA_FILENAME {
        return None;
    }
    let lower: Vec<String> = segments.iter().map(|s| s.to_lowercase()).collect();
    if let Some(i) = lower.iter().enumerate().position(|(i, s)| s == "contents" && lower.get(i + 1).map(String::as_str) == Some("resources")) {
        segments = segments.split_off(i + 2);
    }
    let top = segments.iter().position(|s| ALLOWED_TOP_LEVEL_DIRS.contains(&s.to_lowercase().as_str()))?;
    let target = &segments[top..];
    if target.is_empty() || target.iter().any(|s| *s == "." || *s == ".." || s.contains('\0')) {
        return None;
    }
    Some(target.join("/"))
}

struct Summary {
    files: Vec<(PathBuf, String, u64)>,
    total_bytes: u64,
    hash: String,
    metadata: Map<String, Value>,
}

fn hash_file(hasher: &mut Sha256, path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..n]);
    }
}

/// `summarizePackage`.
fn summarize(root: &Path) -> Option<Summary> {
    if !root.exists() {
        return None;
    }
    let records = collect_files(root)?;
    let metadata = extract_metadata(&records);
    let candidates: Vec<&FileRecord> = records.iter().filter(|r| !is_metadata_candidate(&r.relative)).collect();
    if candidates.is_empty() {
        return None;
    }
    let wrapper = detect_wrapper_prefix(&candidates);
    let mut overlay = Vec::new();
    for record in &records {
        let Some(target) = overlay_target(&record.relative, wrapper.as_deref()) else { continue };
        let extension = extension_of(&target);
        if !IMPORT_EXTENSIONS.contains(&extension.as_str()) {
            tracing::warn!("Skipped unsupported file extension \"{extension}\" in mod package: {}", record.relative);
            continue;
        }
        overlay.push((record.absolute.clone(), target, record.size));
    }
    if overlay.is_empty() {
        return None;
    }
    overlay.sort_by(|a, b| locale_compare(&a.1, &b.1));
    let mut hasher = Sha256::new();
    let mut total = 0;
    for (source, target, size) in &overlay {
        total += size;
        hasher.update(target.as_bytes());
        hasher.update([0u8]);
        if let Err(error) = hash_file(&mut hasher, source) {
            tracing::warn!("Failed to hash mod file {}: {error}", source.display());
            return None;
        }
        hasher.update([0u8]);
    }
    Some(Summary { files: overlay, total_bytes: total, hash: format!("{:x}", hasher.finalize()), metadata })
}

fn ensure_dirs(state: &AppState, id: &str) -> AppResult<()> {
    std::fs::create_dir_all(mods_dir(state, id)?)?;
    Ok(())
}

/// `normalizeModOrder`: by order, then creation time; renumbered.
fn normalize_order(mut mods: Vec<Value>) -> Vec<Value> {
    mods.sort_by(|a, b| {
        let (oa, ob) = (a["order"].as_f64().unwrap_or(0.0), b["order"].as_f64().unwrap_or(0.0));
        oa.partial_cmp(&ob)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(a["createdAt"].as_str().unwrap_or(""), b["createdAt"].as_str().unwrap_or("")))
    });
    mods.into_iter()
        .enumerate()
        .map(|(i, mut m)| {
            m["order"] = json!(i);
            m
        })
        .collect()
}

fn read_manifest(state: &AppState, id: &str) -> AppResult<Vec<Value>> {
    ensure_dirs(state, id)?;
    let path = manifest_path(state, id)?;
    match read_json(&path) {
        Some(raw) if raw["schemaVersion"] == MANIFEST_SCHEMA_VERSION && raw["mods"].is_array() => {
            Ok(normalize_order(raw["mods"].as_array().cloned().unwrap_or_default()))
        }
        _ => {
            write_json(&path, &json!({ "schemaVersion": MANIFEST_SCHEMA_VERSION, "mods": [] }))?;
            Ok(Vec::new())
        }
    }
}

fn write_manifest(state: &AppState, id: &str, mods: Vec<Value>) -> AppResult<Vec<Value>> {
    let mods = normalize_order(mods);
    write_json(&manifest_path(state, id)?, &json!({ "schemaVersion": MANIFEST_SCHEMA_VERSION, "mods": mods }))?;
    Ok(mods)
}

fn put_metadata(m: &mut Value, metadata: &Map<String, Value>) {
    for key in ["author", "version", "description"] {
        if let Some(v) = metadata.get(key) {
            m[key] = v.clone();
        }
    }
}

/// `refresh`: reconciles the manifest with the package folders on disk.
pub fn refresh(state: &AppState, id: &str) -> AppResult<Vec<Value>> {
    ensure_dirs(state, id)?;
    let manifest = read_manifest(state, id)?;
    let packages = mods_dir(state, id)?;
    let package_dirs: Vec<String> =
        std::fs::read_dir(&packages).into_iter().flatten().flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let now = now_iso();
    let mut changed = false;
    let mut refreshed = Vec::new();
    for m in &manifest {
        let dir = m["packageDir"].as_str().unwrap_or("");
        if !package_dirs.iter().any(|p| p == dir) {
            changed = true;
            continue;
        }
        let Some(summary) = summarize(&packages.join(dir)) else {
            changed = true;
            continue;
        };
        let name = summary.metadata.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).map(|n| json!(n)).unwrap_or_else(|| m["name"].clone());
        let pick = |key: &str| summary.metadata.get(key).cloned().or_else(|| m.get(key).cloned());
        let (author, version, description) = (pick("author"), pick("version"), pick("description"));
        let material = m["fileCount"] != json!(summary.files.len())
            || m["totalBytes"] != json!(summary.total_bytes)
            || m["contentHash"] != json!(summary.hash)
            || m["name"] != name
            || m.get("author") != author.as_ref()
            || m.get("version") != version.as_ref()
            || m.get("description") != description.as_ref();
        let mut next = m.clone();
        next["name"] = name;
        for (key, v) in [("author", author), ("version", version), ("description", description)] {
            match v {
                Some(v) => next[key] = v,
                None => {
                    if let Some(o) = next.as_object_mut() {
                        o.remove(key);
                    }
                }
            }
        }
        next["fileCount"] = json!(summary.files.len());
        next["totalBytes"] = json!(summary.total_bytes);
        next["contentHash"] = json!(summary.hash);
        if material {
            next["updatedAt"] = json!(now);
            changed = true;
        }
        refreshed.push(next);
    }
    for dir in &package_dirs {
        if manifest.iter().any(|m| m["packageDir"] == dir.as_str()) {
            continue;
        }
        let Some(summary) = summarize(&packages.join(dir)) else { continue };
        changed = true;
        let name = summary.metadata.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(dir).to_owned();
        let mut m = json!({
            "id": uuid::Uuid::new_v4().to_string(), "name": name, "enabled": true, "order": refreshed.len(), "sourceType": "folder",
            "packageDir": dir, "fileCount": summary.files.len(), "totalBytes": summary.total_bytes, "contentHash": summary.hash,
        });
        put_metadata(&mut m, &summary.metadata);
        m["createdAt"] = json!(now);
        m["updatedAt"] = json!(now);
        refreshed.push(m);
    }
    let normalized = normalize_order(refreshed);
    if changed || normalized.len() != manifest.len() {
        write_manifest(state, id, normalized.clone())?;
    }
    Ok(normalized)
}

/// `importFromDirectory`.
fn import_from_directory(state: &AppState, id: &str, source: &Path, source_type: &str, fallback: &str) -> AppResult<Value> {
    ensure_dirs(state, id)?;
    let Some(summary) = summarize(source).filter(|s| !s.files.is_empty()) else {
        return Ok(json!({ "ok": false, "message": NO_OVERLAY_FILES }));
    };
    let manifest = read_manifest(state, id)?;
    let mod_id = uuid::Uuid::new_v4().to_string();
    let destination = mods_dir(state, id)?.join(&mod_id);
    std::fs::create_dir_all(&destination)?;
    let copied = summary.files.iter().try_for_each(|(src, target, _)| {
        let dest = destination.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dest).map(|_| ())
    });
    if let Err(error) = copied {
        let _ = std::fs::remove_dir_all(&destination);
        tracing::error!("Failed to materialize imported mod package: {error}");
        return Ok(json!({ "ok": false, "message": "Failed to copy imported mod files" }));
    }
    let now = now_iso();
    let name = summary.metadata.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(fallback).to_owned();
    let mut m = json!({
        "id": mod_id, "name": name, "enabled": true, "order": manifest.len(), "sourceType": source_type, "packageDir": mod_id,
        "fileCount": summary.files.len(), "totalBytes": summary.total_bytes, "contentHash": summary.hash,
    });
    put_metadata(&mut m, &summary.metadata);
    m["createdAt"] = json!(now);
    m["updatedAt"] = json!(now);
    let mut mods = manifest;
    mods.push(m.clone());
    write_manifest(state, id, mods)?;
    Ok(json!({ "ok": true, "mod": m }))
}

/// `normalizeZipEntryPath`.
fn normalize_zip_entry(name: &str) -> Option<String> {
    let n = name.replace('\\', "/");
    let b = n.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
        return None;
    }
    let segments: Vec<&str> = n.split('/').map(str::trim).filter(|s| !s.is_empty() && *s != ".").collect();
    if segments.iter().any(|s| *s == ".." || s.contains('\0')) {
        return None;
    }
    Some(segments.join("/"))
}

/// `extractZipArchive` with `rejectSymlinks: true`.
fn extract_zip(zip_path: &Path, destination: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_owned();
        let Some(normalized) = normalize_zip_entry(&name) else { return Err(format!("ZIP archive contains invalid path: {name}")) };
        if normalized.is_empty() {
            continue;
        }
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(format!("ZIP archive contains unsupported symlink entry: {name}"));
        }
        let target = destination.join(normalized.replace('/', std::path::MAIN_SEPARATOR_STR));
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        files.push((i, name, target));
    }
    for (i, name, target) in files {
        if target.is_dir() {
            return Err(format!("ZIP archive contains file entry colliding with a directory: {name}"));
        }
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(&target).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn import_folder(state: &AppState, id: &str) -> AppResult<Value> {
    ensure_dirs(state, id)?;
    let Some(folder) = rfd::AsyncFileDialog::new().set_title("Import Mod Folder").pick_folder().await else { return Ok(Value::Null) };
    let source = folder.path().to_path_buf();
    let fallback = source.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
    import_from_directory(state, id, &source, "folder", &fallback)
}

async fn import_zip(state: &AppState, id: &str) -> AppResult<Value> {
    ensure_dirs(state, id)?;
    let picked = rfd::AsyncFileDialog::new().set_title("Import Mod Zip").add_filter("Zip archives", &["zip"]).add_filter("All files", &["*"]).pick_file().await;
    let Some(picked) = picked else { return Ok(Value::Null) };
    let zip_path = picked.path().to_path_buf();
    let temp = layout::product_paths(&state.paths.base, "player").temp_dir.join(format!("mod-import-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        std::fs::create_dir_all(&temp).map_err(|e| e.to_string())?;
        extract_zip(&zip_path, &temp)?;
        let fallback = zip_path.file_stem().and_then(|n| n.to_str()).unwrap_or("").to_owned();
        import_from_directory(state, id, &temp, "zip", &fallback).map_err(|e| e.to_string())
    })();
    if temp.exists() {
        let _ = std::fs::remove_dir_all(&temp);
    }
    Ok(result.unwrap_or_else(|error| {
        tracing::error!("Failed to import zip mod: {error}");
        json!({ "ok": false, "message": "Failed to import zip archive" })
    }))
}

// ----------------------------------------------------------------- runtime

/// `ModRuntimeService.prepareRuntime`: `Some(install_dir)` of the modded
/// runtime, or `None` when the instance launches the plain build.
pub fn prepare_runtime(state: &AppState, instance: &Instance) -> Result<Option<PathBuf>, String> {
    if crate::services::instances::product_of(instance) != "player" {
        return Ok(None);
    }
    let id = instance.get("id").and_then(Value::as_str).unwrap_or("");
    let mut enabled: Vec<Value> = refresh(state, id).map_err(|e| e.to_string())?.into_iter().filter(|m| m["enabled"] == true).collect();
    enabled.sort_by_key(|m| m["order"].as_u64().unwrap_or(0));
    let font = instance.get("customFont").filter(|f| !f.is_null()).cloned();
    let hash = crate::services::instances::version_hash_of(instance).to_owned();
    let root = instance_config_dir(&state.paths.base, id, "player").map_err(|e| e.to_string())?.join("runtime");
    if !layout::is_valid_version_hash(&hash) {
        return Err(format!("Invalid version hash: '{hash}'"));
    }
    let runtime = root.join(&hash);
    if enabled.is_empty() && font.is_none() {
        cleanup_other_runtimes(&root, &hash);
        return Ok(None);
    }
    let source = layout::version_install_dir(&state.paths.base, &hash, "player").map_err(|e| e.to_string())?;
    if !source.exists() {
        return Err(format!("Cannot prepare modded runtime: version install missing at {}", source.display()));
    }
    let fingerprint = fingerprint(&hash, &enabled, font.as_ref());
    let metadata_path = runtime.join(RUNTIME_METADATA_FILENAME);
    let reusable = runtime.exists()
        && metadata_path.exists()
        && layout::binary_path(&runtime, "player").exists()
        && read_json(&metadata_path).is_some_and(|m| m["schemaVersion"] == RUNTIME_METADATA_SCHEMA_VERSION && m["fingerprint"] == fingerprint.as_str());
    if reusable {
        cleanup_other_runtimes(&root, &hash);
        return Ok(Some(runtime));
    }
    let built = (|| -> Result<(), String> {
        if runtime.exists() {
            std::fs::remove_dir_all(&runtime).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        copy_dir(&source, &runtime).map_err(|e| e.to_string())?;
        apply_overlays(state, id, &runtime, &enabled)?;
        if font.is_some() {
            apply_custom_font(state, id, &runtime);
        }
        let metadata = json!({
            "schemaVersion": RUNTIME_METADATA_SCHEMA_VERSION, "fingerprint": fingerprint,
            "fontHash": font.as_ref().and_then(|f| f.get("assetHash")).cloned().unwrap_or(Value::Null),
            "versionHash": hash, "generatedAt": now_iso(), "modIds": enabled.iter().map(|m| m["id"].clone()).collect::<Vec<_>>(),
        });
        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata).unwrap_or_default()).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(error) = built {
        if runtime.exists() {
            let _ = std::fs::remove_dir_all(&runtime);
        }
        return Err(error);
    }
    cleanup_other_runtimes(&root, &hash);
    Ok(Some(runtime))
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let meta = std::fs::metadata(entry.path())?; // dereference: true
        if meta.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// `computeFingerprint`.
fn fingerprint(hash: &str, mods: &[Value], font: Option<&Value>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.as_bytes());
    hasher.update([0u8]);
    for m in mods {
        let entry = json!({ "id": m["id"], "order": m["order"], "enabled": m["enabled"], "hash": m["contentHash"] });
        hasher.update(entry.to_string().as_bytes());
        hasher.update([0u8]);
    }
    hasher.update(font.and_then(|f| f.get("assetHash")).and_then(Value::as_str).unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// `normalizeRuntimeOverlayPath`.
fn runtime_overlay_path(relative: &str) -> Result<Option<String>, String> {
    let normalized = relative.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(format!("Absolute paths are not allowed in mods: {relative}"));
    }
    let segments: Vec<&str> = normalized.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Ok(None);
    }
    if segments.iter().any(|s| *s == "." || *s == ".." || s.contains('\0')) {
        return Err(format!("Path traversal is not allowed in mods: {relative}"));
    }
    let first = segments[0].as_bytes();
    if first.len() == 2 && first[0].is_ascii_alphabetic() && first[1] == b':' {
        return Err(format!("Absolute paths are not allowed in mods: {relative}"));
    }
    if segments.join("/").to_lowercase() == MOD_METADATA_FILENAME || !ALLOWED_TOP_LEVEL_DIRS.contains(&segments[0].to_lowercase().as_str()) {
        return Ok(None);
    }
    Ok(Some(segments.join("/")))
}

/// `applyOverlays`: lowest-priority mod first so the top mod wins.
fn apply_overlays(state: &AppState, id: &str, runtime: &Path, mods: &[Value]) -> Result<(), String> {
    let packages = mods_dir(state, id).map_err(|e| e.to_string())?;
    for m in mods.iter().rev() {
        let name = m["name"].as_str().unwrap_or("");
        let root = packages.join(m["packageDir"].as_str().unwrap_or(""));
        if !root.exists() {
            return Err(format!("Mod package \"{name}\" is missing: {}", root.display()));
        }
        let records = collect_files(&root).ok_or_else(|| format!("Symlinks are not allowed in mod packages: {}", root.display()))?;
        let mut files = Vec::new();
        for record in records {
            let Some(path) = runtime_overlay_path(&record.relative)? else { continue };
            let extension = extension_of(&path);
            if !RUNTIME_EXTENSIONS.contains(&extension.as_str()) {
                return Err(format!("Unsupported file extension in mod package: {}", record.relative));
            }
            files.push((record.absolute, path));
        }
        if files.is_empty() {
            return Err(format!("Mod package \"{name}\" has no valid overlay files"));
        }
        files.sort_by(|a, b| locale_compare(&a.1, &b.1));
        for (source, path) in files {
            let dest = runtime.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&source, &dest).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// `applyCustomFont`: replaces every font family face with the custom font.
fn apply_custom_font(state: &AppState, id: &str, runtime: &Path) {
    let Some(source) = custom_font_path(state, id, "player").filter(|p| p.exists()) else {
        tracing::warn!("Custom font file missing for instance {id}; skipping font override");
        return;
    };
    let fonts = runtime.join("content").join("fonts");
    let _ = std::fs::create_dir_all(&fonts);
    if let Err(error) = std::fs::copy(&source, fonts.join("CustomFont.ttf")) {
        tracing::warn!("Failed to copy the custom font: {error}");
        return;
    }
    for entry in std::fs::read_dir(fonts.join("families")).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(mut parsed) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(faces) = parsed.get_mut("faces").and_then(Value::as_array_mut) else { continue };
        let mut changed = false;
        for face in faces.iter_mut().filter(|f| f.is_object()) {
            if face["assetId"] != CUSTOM_FONT_ASSET_PATH {
                face["assetId"] = json!(CUSTOM_FONT_ASSET_PATH);
                changed = true;
            }
        }
        if changed {
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&parsed).unwrap_or_default());
        }
    }
}

fn cleanup_other_runtimes(root: &Path, keep: &str) {
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if entry.path().is_dir() && entry.file_name().to_string_lossy() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

pub fn register(router: &mut Router) {
    router
        .handle("mods:getForInstance", |ctx, args| async move {
            let id: String = args.get(0)?;
            read_manifest(&ctx.state, &id)
        })
        .handle("mods:importFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            import_folder(&ctx.state, &id).await
        })
        .handle("mods:importZip", |ctx, args| async move {
            let id: String = args.get(0)?;
            import_zip(&ctx.state, &id).await
        })
        .handle("mods:refresh", |ctx, args| async move {
            let id: String = args.get(0)?;
            refresh(&ctx.state, &id)
        })
        .handle("mods:setEnabled", |ctx, args| async move {
            let request: Value = args.get(0)?;
            let state = &ctx.state;
            let id = request["instanceId"].as_str().unwrap_or("").to_owned();
            let mut mods = read_manifest(state, &id)?;
            let Some(index) = mods.iter().position(|m| m["id"] == request["modId"]) else { return Ok(mods) };
            mods[index]["enabled"] = request["enabled"].clone();
            mods[index]["updatedAt"] = json!(now_iso());
            write_manifest(state, &id, mods)?;
            read_manifest(state, &id)
        })
        .handle("mods:reorder", |ctx, args| async move {
            let id: String = args.get(0)?;
            let ordered: Vec<String> = args.get(1)?;
            let state = &ctx.state;
            let mods = read_manifest(state, &id)?;
            if ordered.len() != mods.len() {
                return Err(AppError::msg("Reorder payload length mismatch"));
            }
            let now = now_iso();
            let mut seen = std::collections::HashSet::new();
            let mut reordered = Vec::new();
            for mod_id in &ordered {
                if !seen.insert(mod_id.clone()) {
                    return Err(AppError::msg("Reorder payload contains duplicate ids"));
                }
                let mut m = mods.iter().find(|m| m["id"] == mod_id.as_str()).cloned().ok_or_else(|| AppError::msg("Reorder payload contains unknown id"))?;
                m["order"] = json!(reordered.len());
                m["updatedAt"] = json!(now);
                reordered.push(m);
            }
            write_manifest(state, &id, reordered)
        })
        .handle("mods:remove", |ctx, args| async move {
            let id: String = args.get(0)?;
            let mod_id: String = args.get(1)?;
            let state = &ctx.state;
            let mods = read_manifest(state, &id)?;
            let Some(m) = mods.iter().find(|m| m["id"] == mod_id.as_str()) else { return Ok(mods) };
            let package = mods_dir(state, &id)?.join(m["packageDir"].as_str().unwrap_or(""));
            if package.exists() && package != mods_dir(state, &id)? {
                let _ = std::fs::remove_dir_all(&package);
            }
            let remaining = mods.into_iter().filter(|m| m["id"] != mod_id.as_str()).collect();
            write_manifest(state, &id, remaining)
        })
        .handle("mods:openFolder", |ctx, args| async move {
            let id: String = args.get(0)?;
            ensure_dirs(&ctx.state, &id)?;
            let error = layout::open_path(&mods_dir(&ctx.state, &id)?);
            if !error.is_empty() {
                return Err(AppError::msg(error));
            }
            Ok(Value::Null)
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_paths() {
        assert_eq!(overlay_target("MyMod/content/textures/a.png", Some("MyMod")).as_deref(), Some("content/textures/a.png"));
        assert_eq!(overlay_target("x/Contents/Resources/content/a.png", None).as_deref(), Some("content/a.png"));
        assert!(overlay_target("readme.md", None).is_none());
        assert_eq!(normalize_zip_entry("./a/../b"), None);
        assert_eq!(normalize_zip_entry("a\\b/").as_deref(), Some("a/b"));
        assert_eq!(locale_compare("content/a_b.png", "content/a.png"), Ordering::Less);
        assert_eq!(locale_compare("Content/b", "content/a"), Ordering::Greater);
    }
}
