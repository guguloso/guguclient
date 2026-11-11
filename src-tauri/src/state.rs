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
    pub social: SocialService,
    pub metadata: MetadataService,
    pub updater: UpdaterService,
    pub agreements: UserAgreementsService,
    pub game_lists: crate::services::game_lists::GameListsState,
    pub catalog: crate::services::catalog::CatalogState,
    pub chat: crate::services::chat::ChatPoller,
    pub user_settings: crate::services::user_settings::UserSettingsService,
    pub instances: crate::services::instances::InstancesService,
    pub versions: crate::services::versions::VersionsService,
    pub launch: crate::services::launch::LaunchService,
    pub misc: crate::services::app::AppMisc,
    pub protocol: crate::services::protocol::ProtocolService,
    pub game_preferences: crate::services::preferences::GamePreferences,
    pub feeds: crate::services::feeds::FeedsState,
    pub forums: crate::services::forums::ForumsState,
    pub private_servers: crate::services::private_servers::PrivateServersState,
    pub avatar: crate::services::avatar::AvatarState,
    pub outfits: crate::services::outfits::OutfitsState,
    pub game_avatar: crate::services::game_avatar::GameAvatarState,
    pub version_maintenance: crate::services::version_maintenance::MaintenanceState,
    pub running: crate::services::running::RunningInstances,
    pub activity: crate::services::activity::ActivityState,
    pub discord: crate::services::discord_rpc::DiscordRpc,
    pub launcher_notifications: crate::services::launcher_notifications::NotificationsState,
    pub hotkey: crate::services::mission_control::HotkeyState,
    pub login: crate::services::login::LoginState,
    pub challenge: crate::services::challenge::ChallengeState,
    pub captures: crate::services::captures::CapturesState,
    pub macros: crate::services::macros::MacrosState,
    pub game_communities: Arc<crate::services::communities::GameCommunityIndex>,
}

impl AppState {
    pub fn initialize(app: AppHandle, paths: LauncherPaths) -> AppResult<Self> {
        paths.ensure_dirs()?;
        let roblox = Arc::new(RobloxClient::new()?);
        let crypto = SafeStorage::load(&paths.local_state_file);
        tracing::info!(available = crypto.is_available(), "safeStorage");

        let events = EventBus::new(app.clone());
        let cache = CacheService::open(&paths)?;
        let settings = SettingsService::open(&paths)?;
        let lang = Self::resolve_effective_language(
            settings
                .get("language")
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or("system"),
        );
        roblox.set_language(&lang);
        Ok(Self {
            settings,
            feature_flags: FeatureFlagService::initialize(&cache),
            cache,
            auth: AuthService::load(
                paths.auth_file.clone(),
                crypto,
                Arc::clone(&roblox),
                events.clone(),
            ),
            thumbnails: ThumbnailService::new(
                Arc::clone(&roblox),
                paths.thumbnail_cache_dir.clone(),
            ),
            user_search: UserSearchState::default(),
            discovery: DiscoveryState::default(),
            social: SocialService::new(&paths)?,
            metadata: MetadataService::new()?,
            updater: UpdaterService::new(),
            agreements: UserAgreementsService::default(),
            game_lists: Default::default(),
            catalog: Default::default(),
            chat: crate::services::chat::ChatPoller::new(),
            user_settings: Default::default(),
            instances: Default::default(),
            versions: Default::default(),
            launch: Default::default(),
            misc: Default::default(),
            protocol: Default::default(),
            game_preferences: crate::services::preferences::GamePreferences::open(&paths)?,
            feeds: Default::default(),
            forums: Default::default(),
            private_servers: Default::default(),
            avatar: Default::default(),
            outfits: Default::default(),
            game_avatar: Default::default(),
            version_maintenance: Default::default(),
            running: Default::default(),
            activity: crate::services::activity::ActivityState::load(&paths.base),
            discord: Default::default(),
            launcher_notifications: Default::default(),
            hotkey: Default::default(),
            login: Default::default(),
            challenge: Default::default(),
            captures: Default::default(),
            macros: Default::default(),
            game_communities: Default::default(),
            app,
            events,
            roblox,
            paths,
        })
    }

    /// The shared handle, for work that must outlive a borrowed `&AppState`.
    pub fn arc(&self) -> Option<Arc<AppState>> {
        use tauri::Manager;
        self.app.try_state::<Arc<AppState>>().map(|s| s.inner().clone())
    }

    /// Analytics collection is disabled; kept as a no-op for existing call sites.
    pub fn track(&self, _event: &str, _payload: Value) {}

    /// Updates Roblox API `Accept-Language` and clears localized caches.
    pub fn apply_language(&self, setting: &str) {
        let lang = Self::resolve_effective_language(setting);
        self.roblox.set_language(&lang);
        self.discovery.clear_cache();
        self.game_lists.clear_cache();
        self.catalog.clear_cache();
    }

    fn resolve_effective_language(setting: &str) -> String {
    match setting {
        "pt-BR" | "es" | "en" => setting.to_owned(),
        _ => {
            let sys = crate::services::app::system_locale().to_ascii_lowercase();
            if sys.starts_with("pt") {
                "pt-BR".to_owned()
            } else if sys.starts_with("es") {
                "es".to_owned()
            } else {
                "en".to_owned()
            }
        }
    }
}

/// Background services, in the Electron boot order (after auth).
    /// Agreements subscribe first so the restored session refreshes them,
    /// exactly like `userAgreementsService.initialize()` before
    /// `await authService.initialize()`.
    pub fn start_background(self: &Arc<Self>) {
        crate::services::challenge::install(self);
        crate::services::user_settings::UserSettingsService::initialize(self);
        crate::services::agreements::UserAgreementsService::initialize(self);
        let state = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            state.auth.initialize().await;
            crate::services::updater::UpdaterService::initialize(&state);
            crate::services::metadata::MetadataService::initialize(&state);
            crate::services::chat::ChatPoller::start(&state);
            crate::services::social::SocialService::start(&state);
            crate::services::auth::AuthService::start_headshot_refresh(&state);
            crate::services::mission_control::sync_hotkey(&state);
            crate::services::captures::start(&state);
            crate::services::macros::start(&state);
            crate::services::discord_rpc::start_stop(&state);
            crate::services::version_maintenance::start_stop_background_updates(&state);
            crate::services::game_avatar::start_recovery(&state);
            crate::services::protocol::sync_handlers(&state).await;
            crate::services::version_maintenance::cleanup_on_startup(&state);
            let argv: Vec<String> = std::env::args().collect();
            if let Some(path) = crate::services::macros::lrmacro_from_argv(&argv) {
                crate::services::macros::stage_import_from_file(&state, &path);
            }
            if let Some(url) = crate::services::protocol::extract_protocol_url(&argv) {
                tracing::info!("Processing queued protocol URL");
                crate::services::protocol::handle_url(&state, &url, "process_args").await;
            }
        });
    }
}
