//! Application state shared by every IPC handler (`Arc<AppState>`).
//! Replaces the Inversify container from the Electron build: services are
//! built once at startup and shared immutably; interior mutability lives
//! inside each service behind fine-grained locks.

use std::sync::Arc;

use tauri::AppHandle;

use crate::crypto::SafeStorage;
use crate::error::AppResult;
use crate::ipc::EventBus;
use crate::paths::LauncherPaths;
use crate::roblox::RobloxClient;
use crate::services::analytics::AnalyticsQueue;
use crate::services::auth::AuthService;
use crate::services::discovery::DiscoveryState;
use crate::services::settings::SettingsService;
use crate::services::thumbnails::ThumbnailService;
use crate::services::users::UserSearchState;

pub struct AppState {
    pub paths: LauncherPaths,
    pub events: EventBus,
    pub roblox: Arc<RobloxClient>,
    pub settings: SettingsService,
    pub auth: AuthService,
    pub thumbnails: ThumbnailService,
    pub user_search: UserSearchState,
    pub discovery: DiscoveryState,
    pub analytics: AnalyticsQueue,
}

impl AppState {
    pub fn initialize(app: AppHandle, paths: LauncherPaths) -> AppResult<Self> {
        paths.ensure_dirs()?;
        let roblox = Arc::new(RobloxClient::new()?);
        let crypto = SafeStorage::load(&paths.local_state_file);
        tracing::info!(available = crypto.is_available(), "safeStorage");

        let events = EventBus::new(app);
        Ok(Self {
            settings: SettingsService::open(&paths)?,
            auth: AuthService::load(paths.auth_file.clone(), crypto, Arc::clone(&roblox), events.clone()),
            thumbnails: ThumbnailService::new(Arc::clone(&roblox), paths.thumbnail_cache_dir.clone()),
            user_search: UserSearchState::default(),
            discovery: DiscoveryState::default(),
            analytics: AnalyticsQueue::default(),
            events,
            roblox,
            paths,
        })
    }
}
