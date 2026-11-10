//! Filesystem layout. Mirrors the Electron build exactly so an existing install
//! (accounts, settings, instances, versions) is picked up with no migration.

use std::path::PathBuf;

use crate::error::{AppError, AppResult};

/// Same folder name as `LUNAR_DIRECTORY` in the Electron main process.
const LAUNCHER_DIRECTORY: &str = ".lunar-for-roblox";
/// Electron `app.getPath("userData")` — holds Chromium's `Local State`.
const USER_DATA_DIRECTORY: &str = "Gugu Client for Roblox";

#[derive(Debug, Clone)]
pub struct LauncherPaths {
    pub base: PathBuf,
    pub settings_dir: PathBuf,
    pub auth_file: PathBuf,
    pub logs_dir: PathBuf,
    pub thumbnail_cache_dir: PathBuf,
    pub local_state_file: PathBuf,
}

impl LauncherPaths {
    pub fn resolve() -> AppResult<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| AppError::msg("Could not resolve home directory"))?;
        let roaming =
            dirs::config_dir().ok_or_else(|| AppError::msg("Could not resolve %APPDATA%"))?;
        let base = home.join(LAUNCHER_DIRECTORY);
        Ok(Self {
            settings_dir: base.join("settings"),
            auth_file: base.join("auth.json"),
            logs_dir: base.join("logs"),
            thumbnail_cache_dir: base.join("cache").join("thumbnails"),
            local_state_file: roaming.join(USER_DATA_DIRECTORY).join("Local State"),
            base,
        })
    }

    pub fn ensure_dirs(&self) -> AppResult<()> {
        for dir in [
            &self.base,
            &self.settings_dir,
            &self.logs_dir,
            &self.thumbnail_cache_dir,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}
