//! Roblox client builds (`versions:*`). Port of `VersionsService`.
//!
//! Latest build lookup through clientsettings(cdn), package-manifest
//! downloads from the setup CDN mirrors, extraction into
//! `versions/<hash>` and the `installed-versions.json` registry shared with
//! the Electron build.
//!
//! Pending: `cleanup` and `getStorageBreakdown` (cache cleanup service).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::ipc::Router;
use crate::services::roblox_layout::{self as layout, platform_config, product_paths, read_json, write_json};
use crate::state::AppState;
use crate::util::now_iso;

const ROBLOX_CDN_MIRRORS: [&str; 4] = [
    "https://setup.rbxcdn.com",
    "https://setup-aws.rbxcdn.com",
    "https://setup-ak.rbxcdn.com",
    "https://roblox-setup.cachefly.net",
];
const CLIENT_SETTINGS_CDN_BASE_URL: &str = "https://clientsettingscdn.roblox.com";
const CLIENT_SETTINGS_BASE_URL: &str = "https://clientsettings.roblox.com";
const ROBLOX_USER_AGENT: &str = "RobloxLauncher/0.1.0";
const CHANNELS: [(&str, &str, &str); 4] = [
    ("LIVE", "LIVE (Stable)", "Production release - recommended for most users"),
    ("zlive", "zlive", "Live channel with additional feature flags"),
    ("zcanary", "zcanary (Beta)", "Early testing builds with new features"),
    ("zintegration", "zintegration", "Internal integration testing"),
];
const APP_SETTINGS_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Settings>\n\t<ContentFolder>content</ContentFolder>\n\t<BaseUrl>http://www.roblox.com</BaseUrl>\n</Settings>";
pub const DOWNLOAD_PROGRESS: &str = "versions:downloadProgress";

type DownloadFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

#[derive(Default)]
pub struct VersionsService {
    active: Mutex<HashMap<String, DownloadFuture>>,
    channel_token: tokio::sync::Mutex<Option<String>>,
}

/// `getLatestVersion` result (`ClientVersionInfo`).
#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    #[serde(default)]
    pub bootstrapper_version: String,
    pub channel: String,
    pub client_version_upload: String,
    pub product: String,
    pub version: String,
}

struct ChannelUnavailable;

fn plain_http(state: &AppState) -> &reqwest::Client {
    state.roblox.http()
}

/// `fetchClientVersion`: CDN first, origin on network errors or 5xx.
async fn fetch_client_version(
    state: &AppState,
    binary_type: &str,
    channel: &str,
    token: Option<&str>,
) -> AppResult<Result<Value, ChannelUnavailable>> {
    let path = if channel == "LIVE" {
        format!("/v2/client-version/{binary_type}")
    } else {
        format!("/v2/client-version/{binary_type}/channel/{}", urlencoding::encode(channel))
    };
    let send = |base: &str| {
        let mut request = plain_http(state).get(format!("{base}{path}")).header("cookie", "");
        if let Some(token) = token {
            request = request.header("Roblox-Channel-Token", token);
        }
        request.send()
    };
    let response = match send(CLIENT_SETTINGS_CDN_BASE_URL).await {
        Ok(r) if r.status().as_u16() < 500 => r,
        _ => send(CLIENT_SETTINGS_BASE_URL).await?,
    };
    let status = response.status().as_u16();
    if !response.status().is_success() {
        if channel != "LIVE" && matches!(status, 401 | 403 | 404) {
            return Ok(Err(ChannelUnavailable));
        }
        return Err(AppError::msg(format!("Version lookup failed with status {status}")));
    }
    Ok(Ok(response.json().await?))
}

async fn fetch_channel_token(state: &AppState, binary_type: &str) -> Option<String> {
    let url = format!("{CLIENT_SETTINGS_BASE_URL}/v2/user-channel?binaryType={binary_type}");
    match state.roblox.get::<Value>(&url).await {
        Ok(r) => r.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_owned),
        Err(error) => {
            tracing::warn!("Failed to fetch user channel token: {error}");
            None
        }
    }
}

async fn client_version(state: &AppState, binary_type: &str, channel: &str) -> AppResult<Result<Value, ChannelUnavailable>> {
    if channel == "LIVE" {
        return fetch_client_version(state, binary_type, channel, None).await;
    }
    let mut token = state.versions.channel_token.lock().await;
    let had_cached = token.is_some();
    if !had_cached {
        *token = fetch_channel_token(state, binary_type).await;
    }
    let first = fetch_client_version(state, binary_type, channel, token.as_deref()).await?;
    if first.is_ok() || !had_cached {
        return Ok(first);
    }
    *token = fetch_channel_token(state, binary_type).await;
    fetch_client_version(state, binary_type, channel, token.as_deref()).await
}

pub async fn latest_version(state: &AppState, channel: &str, product: &str) -> AppResult<VersionInfo> {
    latest_or_unavailable(state, channel, product)
        .await?
        .map_err(|_| AppError::msg(format!("Channel '{channel}' is unavailable")))
}

async fn latest_or_unavailable(
    state: &AppState,
    channel: &str,
    product: &str,
) -> AppResult<Result<VersionInfo, ChannelUnavailable>> {
    let config = platform_config(product);
    let raw = match client_version(state, config.binary_type, channel).await? {
        Ok(raw) => raw,
        Err(unavailable) => return Ok(Err(unavailable)),
    };
    let (Some(version), Some(upload)) = (
        raw.get("version").and_then(Value::as_str).filter(|v| !v.is_empty()),
        raw.get("clientVersionUpload").and_then(Value::as_str).filter(|v| !v.is_empty()),
    ) else {
        return Err(AppError::msg("Invalid response from version API"));
    };
    Ok(Ok(VersionInfo {
        bootstrapper_version: raw.get("bootstrapperVersion").and_then(Value::as_str).unwrap_or("").to_owned(),
        channel: channel.to_owned(),
        client_version_upload: upload.to_owned(),
        product: product.to_owned(),
        version: version.to_owned(),
    }))
}

// ------------------------------------------------------------------ registry

fn registry_path(state: &AppState, product: &str) -> PathBuf {
    product_paths(&state.paths.base, product).installed_versions_file
}

/// `readInstalledVersionsMetadata`, with the legacy-field migration.
fn read_registry(state: &AppState, product: &str) -> Vec<Value> {
    let Some(Value::Array(list)) = read_json(&registry_path(state, product)) else { return Vec::new() };
    let mut migrated_any = false;
    let out: Vec<Value> = list
        .iter()
        .map(|v| {
            let display = v
                .get("displayVersion")
                .and_then(Value::as_str)
                .or_else(|| v.get("version").and_then(Value::as_str))
                .unwrap_or("Unknown");
            let item_product = v.get("product").and_then(Value::as_str).unwrap_or(product);
            if v.get("product").and_then(Value::as_str) != Some(item_product) || v.get("displayVersion").and_then(Value::as_str) != Some(display) {
                migrated_any = true;
            }
            let mut entry = serde_json::Map::new();
            entry.insert("product".into(), json!(item_product));
            entry.insert("versionHash".into(), v.get("versionHash").cloned().unwrap_or(Value::Null));
            entry.insert("displayVersion".into(), json!(display));
            for key in ["channel", "installedAt", "lastUsedAt", "sizeBytes", "source"] {
                if let Some(value) = v.get(key) {
                    entry.insert(key.into(), value.clone());
                }
            }
            Value::Object(entry)
        })
        .collect();
    if migrated_any {
        save_registry(state, product, &out);
    }
    out
}

fn save_registry(state: &AppState, product: &str, versions: &[Value]) {
    if let Err(error) = write_json(&registry_path(state, product), &Value::Array(versions.to_vec())) {
        tracing::warn!(%error, "Failed to save installed versions");
    }
}

fn hash_of(v: &Value) -> &str {
    v.get("versionHash").and_then(Value::as_str).unwrap_or_default()
}

pub fn installed_versions(state: &AppState, product: &str) -> Vec<Value> {
    let base = &state.paths.base;
    let installed: Vec<Value> =
        read_registry(state, product).into_iter().filter(|v| layout::is_version_installed(base, hash_of(v), product)).collect();
    backfill_sizes(state, product, &installed);
    installed
}

/// `scheduleSizeBackfill` (synchronous: directory sizes are cheap here).
fn backfill_sizes(state: &AppState, product: &str, versions: &[Value]) {
    let missing: Vec<String> =
        versions.iter().filter(|v| v.get("sizeBytes").is_none()).map(|v| hash_of(v).to_owned()).collect();
    if missing.is_empty() {
        return;
    }
    let base = state.paths.base.clone();
    let sizes: HashMap<String, u64> = missing
        .iter()
        .filter_map(|hash| Some((hash.clone(), dir_size(&layout::version_install_dir(&base, hash, product).ok()?))))
        .collect();
    let mut changed = false;
    let updated: Vec<Value> = read_registry(state, product)
        .into_iter()
        .map(|mut entry| {
            if entry.get("sizeBytes").is_none() {
                if let Some(size) = sizes.get(hash_of(&entry)) {
                    entry["sizeBytes"] = json!(size);
                    changed = true;
                }
            }
            entry
        })
        .collect();
    if changed {
        save_registry(state, product, &updated);
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|entries| entries.flatten().map(|e| dir_size(&e.path())).sum()).unwrap_or(0)
}

/// `markVersionUsed`.
pub fn mark_version_used(state: &AppState, product: &str, hash: &str) {
    let mut versions = read_registry(state, product);
    let Some(entry) = versions.iter_mut().find(|v| hash_of(v) == hash) else { return };
    entry["lastUsedAt"] = json!(now_iso());
    save_registry(state, product, &versions);
}

// ------------------------------------------------------------------ download

struct Package {
    name: String,
    signature: String,
}

/// `parsePackageManifest`: `v0` then 4 lines per package.
fn parse_manifest(text: &str, product: &str) -> Vec<Package> {
    let lines: Vec<&str> = text.trim().split('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("v0") {
        tracing::warn!("Unexpected manifest version: {:?}", lines.first());
    }
    let skipped = platform_config(product).skip_package_names;
    let mut out = Vec::new();
    let mut i = 1;
    while i < lines.len() {
        let name = lines[i].trim();
        let signature = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
        if name.is_empty() || signature.is_empty() {
            break;
        }
        if !skipped.contains(&name) {
            out.push(Package { name: name.to_owned(), signature: signature.to_owned() });
        }
        i += 4;
    }
    out
}

fn package_url(base: &str, channel: &str, hash: &str, file: &str) -> String {
    let channel_path = if channel == "LIVE" { "" } else { "/channel/common" };
    format!("{base}{channel_path}/{hash}-{file}")
}

async fn download_file(state: &AppState, url: &str, dest: &Path) -> AppResult<()> {
    let mut response = plain_http(state).get(url).header("User-Agent", ROBLOX_USER_AGENT).send().await?;
    if !response.status().is_success() {
        return Err(AppError::msg(format!("Download failed: {}", response.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
    }
    file.flush()?;
    Ok(())
}

/// `extractZip` with `skipInvalidEntries`: entries escaping `dest` are skipped.
fn extract_zip(zip_path: &Path, dest: &Path) -> AppResult<()> {
    std::fs::create_dir_all(dest)?;
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| AppError::msg(format!("Invalid archive: {e}")))?;
    let mut skipped = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| AppError::msg(e.to_string()))?;
        let Some(relative) = entry.enclosed_name() else {
            skipped.push(entry.name().to_owned());
            continue;
        };
        let target = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    if !skipped.is_empty() {
        tracing::warn!(
            "Skipped {} invalid entries while extracting {}",
            skipped.len(),
            zip_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        );
    }
    Ok(())
}

fn progress(state: &AppState, info: &VersionInfo, phase: &str, percent: u64, package: Option<(&str, usize, usize)>) {
    let mut payload = json!({ "product": info.product, "phase": phase, "percent": percent, "version": info.version });
    if let Some((name, index, count)) = package {
        payload["package"] = json!({ "name": name, "index": index, "count": count });
    }
    state.events.emit(DOWNLOAD_PROGRESS, payload);
}

async fn download_with_manifest(state: &AppState, info: &VersionInfo, install_dir: &Path) -> AppResult<()> {
    let product = info.product.as_str();
    let hash = info.client_version_upload.as_str();
    let mut manifest = None;
    let mut last_error = String::new();
    for base in ROBLOX_CDN_MIRRORS {
        tracing::debug!("Trying to fetch manifest from {base}...");
        let url = package_url(base, &info.channel, hash, "rbxPkgManifest.txt");
        match plain_http(state).get(&url).header("User-Agent", ROBLOX_USER_AGENT).send().await {
            Ok(r) if r.status().is_success() => {
                manifest = Some((base, r.text().await?));
                break;
            }
            Ok(r) => last_error = format!("Manifest fetch failed: {}", r.status()),
            Err(error) => last_error = error.to_string(),
        }
        tracing::warn!("Failed to fetch manifest from {base}: {last_error}");
    }
    let (base, text) = manifest
        .ok_or_else(|| AppError::msg(format!("Failed to fetch package manifest after trying all mirrors: {last_error}")))?;
    let packages = parse_manifest(&text, product);
    tracing::info!("Found {} packages to download from {base}", packages.len());
    let temp = product_paths(&state.paths.base, product).temp_dir.join(format!("{hash}-packages"));
    std::fs::create_dir_all(&temp)?;
    let map = platform_config(product).package_map;
    let result: AppResult<()> = async {
        let count = packages.len();
        for (i, pkg) in packages.iter().enumerate() {
            let percent = (i as f64 / count as f64 * 100.0).round() as u64;
            let Some((_, subdir)) = map.iter().find(|(name, _)| *name == pkg.name) else {
                tracing::warn!("Unknown package {}, skipping", pkg.name);
                continue;
            };
            if pkg.name == "WebView2RuntimeInstaller.zip" {
                tracing::debug!("Skipping {} (WebView2 runtime installer)", pkg.name);
                continue;
            }
            progress(state, info, "downloading", percent, Some((&pkg.name, i + 1, count)));
            let download_path = temp.join(&pkg.signature);
            download_file(state, &package_url(base, &info.channel, hash, &pkg.name), &download_path).await?;
            let target = install_dir.join(subdir);
            progress(state, info, "extracting", percent, Some((&pkg.name, i + 1, count)));
            let zip_path = download_path.clone();
            tauri::async_runtime::spawn_blocking(move || extract_zip(&zip_path, &target))
                .await
                .map_err(|e| AppError::msg(e.to_string()))??;
            let _ = std::fs::remove_file(&download_path);
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&temp).await;
    if let Err(error) = result {
        tracing::error!("Package manifest download flow failed for {product}/{hash}: {error}");
        return Err(error);
    }
    progress(state, info, "finalizing", 90, None);
    std::fs::write(install_dir.join("AppSettings.xml"), APP_SETTINGS_XML)?;
    Ok(())
}

async fn perform_download(state: Arc<AppState>, info: VersionInfo, source: String) -> AppResult<Value> {
    let product = info.product.clone();
    let hash = info.client_version_upload.clone();
    let base = state.paths.base.clone();
    let install_dir = layout::version_install_dir(&base, &hash, &product)?;
    if layout::is_version_installed(&base, &hash, &product) {
        let mut registry = installed_versions(&state, &product);
        if let Some(existing) = registry.iter_mut().find(|v| hash_of(v) == hash) {
            if source == "manual" && existing.get("source").and_then(Value::as_str) != Some("manual") {
                existing["source"] = json!("manual");
                let existing = existing.clone();
                let all: Vec<Value> = read_registry(&state, &product)
                    .into_iter()
                    .map(|v| if hash_of(&v) == hash { existing.clone() } else { v })
                    .collect();
                save_registry(&state, &product, &all);
                return Ok(existing);
            }
            return Ok(existing.clone());
        }
    }
    let result: AppResult<Value> = async {
        std::fs::create_dir_all(product_paths(&base, &product).temp_dir)?;
        if install_dir.exists() {
            tokio::fs::remove_dir_all(&install_dir).await?;
        }
        std::fs::create_dir_all(&install_dir)?;
        download_with_manifest(&state, &info, &install_dir).await?;
        let size = dir_size(&install_dir);
        let installed = json!({
            "product": product,
            "versionHash": hash,
            "displayVersion": info.version,
            "channel": info.channel,
            "installedAt": now_iso(),
            "sizeBytes": size,
            "source": source,
        });
        let mut registry: Vec<Value> = installed_versions(&state, &product).into_iter().filter(|v| hash_of(v) != hash).collect();
        registry.push(installed.clone());
        save_registry(&state, &product, &registry);
        progress(&state, &info, "complete", 100, None);
        let label = if product == "studio" { "Roblox Studio" } else { "Roblox" };
        tracing::info!("Successfully installed {label} {} ({hash})", info.version);
        Ok(installed)
    }
    .await;
    if let Err(error) = &result {
        tracing::error!("Version download failed for {product}/{hash}: {error}");
    }
    result
}

/// `downloadVersion`: concurrent requests for one build share a download.
pub async fn download_version(state: &Arc<AppState>, info: VersionInfo, source: &str) -> AppResult<Value> {
    let key = format!("{}:{}", info.product, info.client_version_upload);
    let shared = {
        let mut active = state.versions.active.lock();
        active
            .entry(key.clone())
            .or_insert_with(|| {
                let state = Arc::clone(state);
                let source = source.to_owned();
                async move { perform_download(state, info, source).await.map_err(|e| e.to_string()) }.boxed().shared()
            })
            .clone()
    };
    let result = shared.await;
    state.versions.active.lock().remove(&key);
    result.map_err(AppError::msg)
}

pub fn is_downloading(state: &AppState, reference: Option<(&str, &str)>) -> bool {
    let active = state.versions.active.lock();
    match reference {
        Some((product, hash)) => active.contains_key(&format!("{product}:{hash}")),
        None => !active.is_empty(),
    }
}

async fn remove_version(state: &AppState, hash: &str, product: &str) -> AppResult<()> {
    let dir = layout::version_install_dir(&state.paths.base, hash, product)?;
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await?;
    }
    let remaining: Vec<Value> = installed_versions(state, product).into_iter().filter(|v| hash_of(v) != hash).collect();
    save_registry(state, product, &remaining);
    tracing::info!("Removed {product} version {hash}");
    Ok(())
}

async fn check_for_update(state: &AppState, channel: &str, product: &str) -> AppResult<Value> {
    let latest = match latest_or_unavailable(state, channel, product).await? {
        Ok(latest) => latest,
        Err(ChannelUnavailable) => return Ok(json!({ "status": "channelUnavailable", "channel": channel })),
    };
    let mut installed: Vec<Value> = installed_versions(state, product)
        .into_iter()
        .filter(|v| v.get("channel").and_then(Value::as_str) == Some(channel))
        .collect();
    if installed.is_empty() {
        return Ok(json!({ "status": "ok", "hasUpdate": true, "currentVersion": null, "latestVersion": latest }));
    }
    installed.sort_by(|a, b| {
        b.get("installedAt").and_then(Value::as_str).cmp(&a.get("installedAt").and_then(Value::as_str))
    });
    let current = installed.swap_remove(0);
    let has_update = hash_of(&current) != latest.client_version_upload;
    Ok(json!({ "status": "ok", "hasUpdate": has_update, "currentVersion": current, "latestVersion": latest }))
}

fn product_arg(product: Option<String>) -> String {
    product.unwrap_or_else(|| "player".to_owned())
}

pub fn register(router: &mut Router) {
    router
        .handle("versions:getChannels", |_, _| async move {
            Ok(CHANNELS.iter().map(|(id, name, description)| json!({ "id": id, "name": name, "description": description })).collect::<Vec<_>>())
        })
        .handle("versions:getLatest", |ctx, args| async move {
            let channel: Option<String> = args.opt(0)?;
            let product: Option<String> = args.opt(1)?;
            latest_version(&ctx.state, channel.as_deref().unwrap_or("LIVE"), &product_arg(product)).await
        })
        .handle("versions:getInstalled", |ctx, args| async move {
            let product: Option<String> = args.opt(0)?;
            Ok(installed_versions(&ctx.state, &product_arg(product)))
        })
        .handle("versions:getInstalledForChannel", |ctx, args| async move {
            let channel: String = args.get(0)?;
            let product: Option<String> = args.opt(1)?;
            Ok(installed_versions(&ctx.state, &product_arg(product))
                .into_iter()
                .filter(|v| v.get("channel").and_then(Value::as_str) == Some(channel.as_str()))
                .collect::<Vec<_>>())
        })
        .handle("versions:isInstalled", |ctx, args| async move {
            let hash: String = args.get(0)?;
            let product: Option<String> = args.opt(1)?;
            Ok(layout::is_version_installed(&ctx.state.paths.base, &hash, &product_arg(product)))
        })
        .handle("versions:download", |ctx, args| async move {
            let info: VersionInfo = args.get(0)?;
            let state = &ctx.state;
            let result = download_version(state, info.clone(), "manual").await;
            if !is_downloading(state, None) {
                state.events.emit(DOWNLOAD_PROGRESS, Value::Null);
            }
            let installed = result?;
            state.track(
                "version.download",
                json!({ "version_hash": info.client_version_upload, "channel": info.channel, "product": info.product }),
            );
            Ok(installed)
        })
        .handle("versions:isDownloading", |ctx, args| async move {
            let reference: Option<Value> = args.opt(0)?;
            let reference = reference.as_ref().map(|r| {
                (
                    r.get("product").and_then(Value::as_str).unwrap_or("player"),
                    r.get("versionHash").and_then(Value::as_str).unwrap_or(""),
                )
            });
            Ok(is_downloading(&ctx.state, reference))
        })
        .handle("versions:remove", |ctx, args| async move {
            let hash: String = args.get(0)?;
            let product: Option<String> = args.opt(1)?;
            let product = product_arg(product);
            remove_version(&ctx.state, &hash, &product).await?;
            ctx.state.track("version.remove", json!({ "version_hash": hash, "product": product }));
            tracing::info!("Removed version {hash}");
            Ok(true)
        })
        .handle("versions:checkForUpdate", |ctx, args| async move {
            let channel: String = args.get(0)?;
            let product: Option<String> = args.opt(1)?;
            check_for_update(&ctx.state, &channel, &product_arg(product)).await
        })
        .handle("versions:openFolder", |ctx, args| async move {
            let hash: Option<String> = args.opt(0)?;
            let product: Option<String> = args.opt(1)?;
            let product = product_arg(product);
            let base = &ctx.state.paths.base;
            let path = match hash.filter(|h| !h.is_empty()) {
                Some(hash) => layout::version_install_dir(base, &hash, &product)?,
                None => product_paths(base, &product).versions_dir,
            };
            Ok(layout::open_path(&path))
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing_skips_launcher() {
        let text = "v0\nRobloxApp.zip\nabc\n10\n20\nRobloxPlayerLauncher.exe\ndef\n1\n2\nssl.zip\nghi\n3\n4\n";
        let names: Vec<String> = parse_manifest(text, "player").into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["RobloxApp.zip", "ssl.zip"]);
        assert_eq!(package_url("https://cdn", "zcanary", "version-x", "ssl.zip"), "https://cdn/channel/common/version-x-ssl.zip");
    }
}
