//! On-disk layout and platform glue shared by instances, versions and
//! launch. Port of the `paths`/`getProductPaths`/`getPlatformConfig`
//! helpers plus the process list (`ps-list`) and `MutexService`.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

pub const PRODUCTS: [&str; 2] = ["player", "studio"];

/// `getPlatformConfig(product)` on Windows.
pub struct PlatformConfig {
    pub binary_type: &'static str,
    pub executable_name: &'static str,
    pub process_name: &'static str,
    pub skip_package_names: &'static [&'static str],
    pub package_map: &'static [(&'static str, &'static str)],
}

const PLAYER_PACKAGES: &[(&str, &str)] = &[
    ("RobloxApp.zip", ""),
    ("Libraries.zip", ""),
    ("redist.zip", ""),
    ("WebView2.zip", ""),
    ("shaders.zip", "shaders"),
    ("ssl.zip", "ssl"),
    ("WebView2RuntimeInstaller.zip", "WebView2RuntimeInstaller"),
    ("content-avatar.zip", "content/avatar"),
    ("content-configs.zip", "content/configs"),
    ("content-fonts.zip", "content/fonts"),
    ("content-sky.zip", "content/sky"),
    ("content-sounds.zip", "content/sounds"),
    ("content-textures2.zip", "content/textures"),
    ("content-models.zip", "content/models"),
    ("content-textures3.zip", "PlatformContent/pc/textures"),
    ("content-terrain.zip", "PlatformContent/pc/terrain"),
    ("content-platform-fonts.zip", "PlatformContent/pc/fonts"),
    (
        "content-platform-dictionaries.zip",
        "PlatformContent/pc/shared_compression_dictionaries",
    ),
    ("extracontent-luapackages.zip", "ExtraContent/LuaPackages"),
    ("extracontent-translations.zip", "ExtraContent/translations"),
    ("extracontent-models.zip", "ExtraContent/models"),
    ("extracontent-textures.zip", "ExtraContent/textures"),
    ("extracontent-places.zip", "ExtraContent/places"),
];

const STUDIO_PACKAGES: &[(&str, &str)] = &[
    ("ApplicationConfig.zip", "ApplicationConfig"),
    ("BuiltInPlugins.zip", "BuiltInPlugins"),
    ("BuiltInStandalonePlugins.zip", "BuiltInStandalonePlugins"),
    ("Plugins.zip", "Plugins"),
    ("StudioFonts.zip", "StudioFonts"),
    ("WebView2.zip", ""),
    ("WebView2RuntimeInstaller.zip", "WebView2RuntimeInstaller"),
    ("RobloxStudio.zip", ""),
    ("Libraries.zip", ""),
    ("LibrariesQt5.zip", ""),
    ("content-avatar.zip", "content/avatar"),
    ("content-configs.zip", "content/configs"),
    ("content-fonts.zip", "content/fonts"),
    ("content-models.zip", "content/models"),
    ("content-qt_translations.zip", "content/qt_translations"),
    ("content-sky.zip", "content/sky"),
    ("content-sounds.zip", "content/sounds"),
    ("shaders.zip", "shaders"),
    ("ssl.zip", "ssl"),
    ("content-textures2.zip", "content/textures"),
    ("content-textures3.zip", "PlatformContent/pc/textures"),
    (
        "content-studio_svg_textures.zip",
        "content/studio_svg_textures",
    ),
    ("content-terrain.zip", "PlatformContent/pc/terrain"),
    ("content-platform-fonts.zip", "PlatformContent/pc/fonts"),
    (
        "content-platform-dictionaries.zip",
        "PlatformContent/pc/shared_compression_dictionaries",
    ),
    ("content-api-docs.zip", "content/api-docs"),
    ("extracontent-scripts.zip", "ExtraContent/scripts"),
    ("extracontent-luapackages.zip", "ExtraContent/LuaPackages"),
    ("extracontent-translations.zip", "ExtraContent/translations"),
    ("extracontent-models.zip", "ExtraContent/models"),
    ("extracontent-textures.zip", "ExtraContent/textures"),
    ("studiocontent-models.zip", "StudioContent/models"),
    ("studiocontent-textures.zip", "StudioContent/textures"),
    ("redist.zip", ""),
];

pub fn platform_config(product: &str) -> &'static PlatformConfig {
    static PLAYER: PlatformConfig = PlatformConfig {
        binary_type: "WindowsPlayer",
        executable_name: "RobloxPlayerBeta.exe",
        process_name: "RobloxPlayerBeta",
        skip_package_names: &["RobloxPlayerLauncher.exe"],
        package_map: PLAYER_PACKAGES,
    };
    static STUDIO: PlatformConfig = PlatformConfig {
        binary_type: "WindowsStudio64",
        executable_name: "RobloxStudioBeta.exe",
        process_name: "RobloxStudioBeta",
        skip_package_names: &[],
        package_map: STUDIO_PACKAGES,
    };
    if product == "studio" {
        &STUDIO
    } else {
        &PLAYER
    }
}

/// `getProductPaths(product)`.
pub struct ProductPaths {
    pub versions_dir: PathBuf,
    pub installed_versions_file: PathBuf,
    pub temp_dir: PathBuf,
    pub data_dir: PathBuf,
    pub instances_dir: PathBuf,
}

pub fn product_paths(base: &Path, product: &str) -> ProductPaths {
    let root = if product == "studio" {
        base.join("studio")
    } else {
        base.to_path_buf()
    };
    ProductPaths {
        versions_dir: root.join("versions"),
        installed_versions_file: root.join("installed-versions.json"),
        temp_dir: root.join("temp"),
        data_dir: root.join("data"),
        instances_dir: root.join("instances"),
    }
}

/// `VERSION_HASH_REGEX`: `^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$`.
pub fn is_valid_version_hash(hash: &str) -> bool {
    let bytes = hash.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// `UUID_REGEX` (case-insensitive, hyphenated 8-4-4-4-12).
pub fn is_valid_instance_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn version_install_dir(base: &Path, hash: &str, product: &str) -> AppResult<PathBuf> {
    if !is_valid_version_hash(hash) {
        return Err(AppError::msg(format!("Invalid version hash: '{hash}'")));
    }
    Ok(product_paths(base, product).versions_dir.join(hash))
}

pub fn binary_path(install_dir: &Path, product: &str) -> PathBuf {
    install_dir.join(platform_config(product).executable_name)
}

pub fn is_version_installed(base: &Path, hash: &str, product: &str) -> bool {
    version_install_dir(base, hash, product).is_ok_and(|dir| binary_path(&dir, product).exists())
}

fn checked_instance(id: &str) -> AppResult<()> {
    if is_valid_instance_id(id) {
        Ok(())
    } else {
        Err(AppError::msg(format!("Invalid instance ID: '{id}'")))
    }
}

pub fn instance_config_dir(base: &Path, id: &str, product: &str) -> AppResult<PathBuf> {
    checked_instance(id)?;
    Ok(product_paths(base, product).instances_dir.join(id))
}

pub fn instance_data_dir(base: &Path, id: &str, product: &str) -> AppResult<PathBuf> {
    checked_instance(id)?;
    Ok(product_paths(base, product).data_dir.join(id))
}

fn app_support_folder(product: &str) -> &'static str {
    if product == "studio" {
        "RobloxStudio"
    } else {
        "Roblox"
    }
}

fn cache_bundle(product: &str) -> &'static str {
    if product == "studio" {
        "com.roblox.RobloxStudio"
    } else {
        "com.roblox.Roblox"
    }
}

/// `ensureInstanceDataDir`: the fake HOME tree for isolated instances.
pub fn ensure_instance_data_dir(base: &Path, id: &str, product: &str) -> AppResult<PathBuf> {
    let data = instance_data_dir(base, id, product)?;
    let library = data.join("Library");
    for dir in [
        data.clone(),
        library.join(app_support_folder(product)),
        library.join("Logs").join(app_support_folder(product)),
        library.join("Caches").join(cache_bundle(product)),
        library.join("Preferences"),
        data.join("tmp"),
    ] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(data)
}

/// `%LOCALAPPDATA%\Roblox\LocalStorage\appStorage.json`.
pub fn system_app_storage() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| {
        d.join("Roblox")
            .join("LocalStorage")
            .join("appStorage.json")
    })
}

/// Writes JSON like `FileStore.writeJson` (2-space indent, atomic).
pub fn write_json(path: &Path, value: &serde_json::Value) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.gugu-for-roblox.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Opens a folder in Explorer (`shell.openPath`). Returns "" on success.
pub fn open_path(path: &Path) -> String {
    match std::process::Command::new("explorer.exe").arg(path).spawn() {
        Ok(_) => String::new(),
        Err(error) => error.to_string(),
    }
}

// ------------------------------------------------------------------ processes

pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
}

/// `ps-list` on Windows (Toolhelp snapshot).
#[cfg(windows)]
pub fn process_list() -> Vec<ProcessEntry> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    // SAFETY: plain Win32 snapshot iteration; the handle is closed below and
    // the entry struct is sized as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push(ProcessEntry {
                pid: entry.th32ProcessID,
                name: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            ok = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    out
}

#[cfg(not(windows))]
pub fn process_list() -> Vec<ProcessEntry> {
    Vec::new()
}

/// `matchesProcessName`: case-insensitive, `.exe` optional.
pub fn matches_process_name(name: &str, process: &str) -> bool {
    let trimmed = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    trimmed.eq_ignore_ascii_case(process)
}

pub fn is_pid_alive(pid: u32) -> bool {
    process_list().iter().any(|p| p.pid == pid)
}

/// Force-kills one process (`process.kill(pid, SIGKILL)`); `Ok(false)` when gone.
pub fn kill_pid(pid: u32) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        // SAFETY: handle is checked and closed; TerminateProcess on our own request.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Ok(false);
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(true)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        Ok(false)
    }
}

// ------------------------------------------------------------------ mutex

/// `MutexService`: holding `ROBLOX_singletonMutex` lets several Roblox
/// players run side by side.
#[derive(Default)]
pub struct SingletonMutex {
    handle: parking_lot::Mutex<Option<usize>>,
}

const ROBLOX_SINGLETON_MUTEX: &str = "ROBLOX_singletonMutex";

impl SingletonMutex {
    #[cfg(windows)]
    pub fn acquire(&self) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let mut slot = self.handle.lock();
        if slot.is_some() {
            return true;
        }
        let name: Vec<u16> = ROBLOX_SINGLETON_MUTEX
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: null attributes, owned initial state, NUL-terminated name.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
            let last_error = GetLastError();
            if handle.is_null() {
                tracing::warn!(
                    "Failed to create {ROBLOX_SINGLETON_MUTEX} (GetLastError={last_error})"
                );
                return false;
            }
            if last_error == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                tracing::warn!(
                    "{ROBLOX_SINGLETON_MUTEX} already exists (GetLastError={last_error})"
                );
                return false;
            }
            *slot = Some(handle as usize);
        }
        tracing::debug!("Acquired and holding {ROBLOX_SINGLETON_MUTEX}");
        true
    }

    #[cfg(not(windows))]
    pub fn acquire(&self) -> bool {
        true
    }

    pub fn release(&self) {
        let Some(handle) = self.handle.lock().take() else {
            return;
        };
        #[cfg(windows)]
        // SAFETY: the handle came from CreateMutexW above and is released once.
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::ReleaseMutex;
            let handle = handle as windows_sys::Win32::Foundation::HANDLE;
            ReleaseMutex(handle);
            CloseHandle(handle);
        }
        #[cfg(not(windows))]
        let _ = handle;
        tracing::debug!("Released {ROBLOX_SINGLETON_MUTEX}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validators_match_electron_regexes() {
        assert!(is_valid_version_hash("version-f0ea5da9412f4e35"));
        assert!(!is_valid_version_hash("../evil"));
        assert!(!is_valid_version_hash("-abc"));
        assert!(is_valid_instance_id("6de99854-ec51-4ee4-969a-05b20b7eae12"));
        assert!(!is_valid_instance_id("6de99854-ec51-4ee4-969a-05b20b7eae1"));
        assert!(matches_process_name(
            "RobloxPlayerBeta.exe",
            "robloxplayerbeta"
        ));
    }
}
