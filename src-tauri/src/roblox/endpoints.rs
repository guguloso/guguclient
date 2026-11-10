//! Base URLs, identical to the `RBLX_*` constants in the Electron main process.
#![allow(dead_code)] // the full table is kept; later phases use the rest

pub const AVATAR_V1: &str = "https://avatar.roblox.com/v1";
pub const AUTH_V1: &str = "https://auth.roblox.com/v1";
pub const USERS_V1: &str = "https://users.roblox.com/v1";
pub const THUMBNAILS_V1: &str = "https://thumbnails.roblox.com/v1";
pub const FRIENDS_V1: &str = "https://friends.roblox.com/v1";
pub const GAMES_V1: &str = "https://games.roblox.com/v1";
pub const GAMES_V2: &str = "https://games.roblox.com/v2";
pub const GROUPS_V1: &str = "https://groups.roblox.com/v1";
pub const CATALOG_V1: &str = "https://catalog.roblox.com/v1";
pub const CATALOG_V2: &str = "https://catalog.roblox.com/v2";
pub const ECONOMY_V1: &str = "https://economy.roblox.com/v1";
pub const BADGES_V1: &str = "https://badges.roblox.com/v1";
pub const INVENTORY_V1: &str = "https://inventory.roblox.com/v1";
pub const INVENTORY_V2: &str = "https://inventory.roblox.com/v2";
pub const PRESENCE_V1: &str = "https://presence.roblox.com/v1";
pub const NOTIFICATIONS_V2: &str = "https://notifications.roblox.com/v2";
pub const PRIVATE_MESSAGES_V1: &str = "https://privatemessages.roblox.com/v1";
pub const ASSET_DELIVERY_V2: &str = "https://assetdelivery.roblox.com/v2";
pub const PREMIUM_FEATURES_V1: &str = "https://premiumfeatures.roblox.com/v1";

pub const MARKETPLACE_SALES_V1: &str = "https://apis.roblox.com/marketplace-sales/v1";
pub const MARKETPLACE_ITEMS_V1: &str = "https://apis.roblox.com/marketplace-items/v1";
pub const MARKETPLACE_WIDGETS_V1: &str = "https://apis.roblox.com/marketplace-widgets/v1";
pub const CLOUD_V2: &str = "https://apis.roblox.com/cloud/v2";
pub const GAME_PASSES_V1: &str = "https://apis.roblox.com/game-passes/v1";
pub const PLATFORM_CHAT_API: &str = "https://apis.roblox.com/platform-chat-api/v1";
pub const EXPLORE_API: &str = "https://apis.roblox.com/explore-api/v1";
pub const USER_PROFILE_API: &str = "https://apis.roblox.com/user-profile-api/v1";
pub const PROFILE_PLATFORM_API: &str = "https://apis.roblox.com/profile-platform-api/v1";
pub const UNIVERSES_V1: &str = "https://apis.roblox.com/universes/v1";
pub const COMMUNITY_LINKS_V1: &str = "https://apis.roblox.com/community-links/v1";
pub const VIRTUAL_EVENTS_V1: &str = "https://apis.roblox.com/virtual-events/v1";
pub const VOTING_API: &str = "https://apis.roblox.com/voting-api";
pub const DISCOVERY_API: &str = "https://apis.roblox.com/discovery-api";
pub const SEARCH_LANDING_PAGE: &str = "https://apis.roblox.com/search-landing-page-api/v1";
pub const GAMES_AUTOCOMPLETE: &str = "https://apis.roblox.com/games-autocomplete/v1";
pub const MARKETPLACE_AUTOCOMPLETE: &str = "https://apis.roblox.com/autocomplete-avatar/v2";
pub const SEARCH_API: &str = "https://apis.roblox.com/search-api";
pub const USER_SETTINGS_API: &str = "https://apis.roblox.com/user-settings-api";
pub const USER_AGREEMENTS_V1: &str = "https://apis.roblox.com/user-agreements/v1";
pub const SHARE_LINKS_V1: &str = "https://apis.roblox.com/sharelinks/v1";
pub const WEB: &str = "https://www.roblox.com";

/// Joins numeric ids the way rozod serializes array query params.
pub fn join_ids(ids: &[u64]) -> String {
    let mut out = String::with_capacity(ids.len() * 11);
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out
}

/// Repeated query param form (`userIds=1&userIds=2`), used by some APIs.
pub fn repeat_param(name: &str, ids: &[u64]) -> String {
    let mut out = String::with_capacity(ids.len() * (name.len() + 12));
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(name);
        out.push('=');
        out.push_str(&id.to_string());
    }
    out
}

/// Deduplicates positive ids while keeping first-seen order (`new Set(...)`).
pub fn unique_positive(ids: &[u64]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|id| *id > 0 && seen.insert(*id)).collect()
}
