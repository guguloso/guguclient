//! Filesystem layout. Mirrors the Electron build exactly so an existing install
//! (accounts, settings, instances, versions) is picked up with no migration.

use std::path::PathBuf;

use crate::error::{AppError, AppResult};

/// Launcher data folder under the user's home.
const LAUNCHER_DIRECTORY: &str = ".gugu-for-roblox";
/// Folder used by earlier builds (`LUNAR_DIRECTORY`); moved on first launch.
const LEGACY_LAUNCHER_DIRECTORY: &str = ".lunar-for-roblox";
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
        let base = migrate_legacy_directory(home.join(LEGACY_LAUNCHER_DIRECTORY), home.join(LAUNCHER_DIRECTORY));
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

/// Moves an existing legacy data folder to the new name, so accounts,
/// settings and instances carry over. Keeps using the legacy folder when
/// the move fails rather than starting empty.
fn migrate_legacy_directory(legacy: PathBuf, current: PathBuf) -> PathBuf {
    if current.exists() || !legacy.exists() {
        return current;
    }
    match std::fs::rename(&legacy, &current) {
        Ok(()) => current,
        Err(_) => legacy,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrates_legacy_folder() {
        let root = std::env::temp_dir().join(format!("gugu-paths-{}", uuid::Uuid::new_v4()));
        let (legacy, current) = (root.join(".lunar-for-roblox"), root.join(".gugu-for-roblox"));
        std::fs::create_dir_all(legacy.join("settings")).unwrap();
        std::fs::write(legacy.join("auth.json"), "{}").unwrap();
        assert_eq!(super::migrate_legacy_directory(legacy.clone(), current.clone()), current);
        assert!(current.join("auth.json").exists() && current.join("settings").is_dir() && !legacy.exists());
        // Already migrated: nothing moves.
        assert_eq!(super::migrate_legacy_directory(legacy.clone(), current.clone()), current);
        let _ = std::fs::remove_dir_all(&root);
    }
}
