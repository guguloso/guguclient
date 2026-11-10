//! Application state shared by every IPC handler (`Arc<AppState>`).
//! Replaces the Inversify container from the Electron build: services are
//! built once at startup and shared immutably; interior mutability lives
//! inside each service behind fine-grained locks.

use std::sync::Arc;

use serde_json::Value;
use tauri::AppHandle;

use crate::crypto::SafeStorage;
use crate::error::AppResult;
use crate::ipc::EventBus;
use crate::paths::LauncherPaths;
use crate::roblox::RobloxClient;
use crate::services::agreements::UserAgreementsService;
use crate::services::analytics::AnalyticsQueue;
use crate::services::auth::AuthService;
use crate::services::cache::CacheService;
use crate::services::discovery::DiscoveryState;
use crate::services::feature_flags::FeatureFlagService;
use crate::services::metadata::MetadataService;
use crate::services::settings::SettingsService;
use crate::services::social::SocialService;
use crate::services::thumbnails::ThumbnailService;
use crate::services::updater::UpdaterService;
use crate::services::users::UserSearchState;

pub struct AppState {
    pub app: AppHandle,
    pub paths: LauncherPaths,
    pub events: EventBus,
    pub roblox: Arc<RobloxClient>,
    pub settings: SettingsService,
    pub cache: CacheService,
    pub feature_flags: FeatureFlagService,
    pub auth: AuthService,
    pub thumbnails: ThumbnailService,
    pub user_search: UserSearchState,
    pub discovery: DiscoveryState,
    pub analytics: AnalyticsQueue,
    pub social: SocialService,
    pub metadata: MetadataService,
    pub updater: UpdaterService,
    pub agreements: UserAgreementsService,
    pub game_lists: crate::services::game_lists::GameListsState,
    pub catalog: crate::services::catalog::CatalogState,
    pub chat: crate::services::chat::ChatPoller,
    pub user_settings: crate::services::user_settings::UserSettingsService,
}

impl AppState {
    pub fn initialize(app: AppHandle, paths: LauncherPaths) -> AppResult<Self> {
        paths.ensure_dirs()?;
        let roblox = Arc::new(RobloxClient::new()?);
        let crypto = SafeStorage::load(&paths.local_state_file);
        tracing::info!(available = crypto.is_available(), "safeStorage");

        let events = EventBus::new(app.clone());
        let cache = CacheService::open(&paths)?;
        Ok(Self {
            settings: SettingsService::open(&paths)?,
            feature_flags: FeatureFlagService::initialize(&cache),
            cache,
            auth: AuthService::load(paths.auth_file.clone(), crypto, Arc::clone(&roblox), events.clone()),
            thumbnails: ThumbnailService::new(Arc::clone(&roblox), paths.thumbnail_cache_dir.clone()),
            user_search: UserSearchState::default(),
            discovery: DiscoveryState::default(),
            analytics: AnalyticsQueue::default(),
            social: SocialService::new(&paths)?,
            metadata: MetadataService::new()?,
            updater: UpdaterService::new(),
            agreements: UserAgreementsService::default(),
            game_lists: Default::default(),
            catalog: Default::default(),
            chat: crate::services::chat::ChatPoller::new(),
            user_settings: Default::default(),
            app,
            events,
            roblox,
            paths,
        })
    }

    /// `analyticsService.track`: queued only when collection is allowed.
    pub fn track(&self, event: &str, payload: Value) {
        if self.settings.get_bool("allow-analytics-collection") {
            self.analytics.push(event.to_owned(), payload);
        }
    }

    /// Background services, in the Electron boot order (after auth).
    /// Agreements subscribe first so the restored session refreshes them,
    /// exactly like `userAgreementsService.initialize()` before
    /// `await authService.initialize()`.
    pub fn start_background(self: &Arc<Self>) {
        crate::services::user_settings::UserSettingsService::initialize(self);
        crate::services::agreements::UserAgreementsService::initialize(self);
        let state = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            state.auth.initialize().await;
            crate::services::updater::UpdaterService::initialize(&state);
            crate::services::metadata::MetadataService::initialize(&state);
            crate::services::chat::ChatPoller::start(&state);
            crate::services::social::SocialService::start(&state);
        });
    }
}
