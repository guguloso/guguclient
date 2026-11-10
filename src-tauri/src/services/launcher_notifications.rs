//! Post-game toasts ("You played …"). Port of `LauncherNotificationsService`
//! and `GameMetadataService.getByPlace` (LRU of 200 places).

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::services::{app as app_service, games};
use crate::state::AppState;

const MAX_TOAST_IDS: usize = 100;
const TOAST_DURATION_MS: u64 = 8000;
const MAX_GAME_METADATA_ENTRIES: usize = 200;

#[derive(Default)]
pub struct NotificationsState {
    sent: Mutex<Vec<String>>,
    metadata: Mutex<Vec<(u64, Value)>>,
}

/// `formatActivitySessionDurationMs`.
fn format_duration(ms: u64) -> String {
    if ms < 60_000 {
        return "<1m".into();
    }
    let minutes = ms / 60_000;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn fallback_metadata(place: u64) -> Value {
    json!({ "creatorName": null, "thumbnailUrl": null, "title": format!("Place {place}"), "universeId": null })
}

/// `GameMetadataService.getByPlace`.
async fn metadata_for(state: &Arc<AppState>, place: u64, universe: Option<u64>) -> crate::error::AppResult<Value> {
    if let Some((_, cached)) = state.launcher_notifications.metadata.lock().iter().find(|(p, _)| *p == place) {
        return Ok(cached.clone());
    }
    let universe = match universe {
        Some(u) => Some(u),
        None => games::universe_by_place(state, place).await,
    };
    let (mut game, mut thumbnail) = (Value::Null, Value::Null);
    if let Some(u) = universe {
        let ids = [u];
        let (details, icons) = futures::join!(games::details_for(state, &ids), games::icons_for(state, &ids, "512x512"));
        game = details?.into_iter().next().unwrap_or(Value::Null);
        thumbnail = icons?.get(&u.to_string()).map(|s| json!(s)).unwrap_or(Value::Null);
    }
    let trimmed = |v: Option<&Value>| v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let metadata = json!({
        "creatorName": trimmed(game.pointer("/creator/name")),
        "thumbnailUrl": thumbnail,
        "title": trimmed(game.get("name")).unwrap_or_else(|| format!("Place {place}")),
        "universeId": universe,
    });
    let mut cache = state.launcher_notifications.metadata.lock();
    cache.retain(|(p, _)| *p != place);
    cache.push((place, metadata.clone()));
    if cache.len() > MAX_GAME_METADATA_ENTRIES {
        cache.remove(0);
    }
    Ok(metadata)
}

/// `handleActivitySessionEnded`: a toast after leaving a game.
pub fn on_session_ended(state: &Arc<AppState>, session: &Value) {
    let enabled = state.settings.get_bool("enable-activity-tracking") && state.settings.get_bool("post-game-notifications");
    if session["reason"] != "leave" || !enabled {
        return;
    }
    let place = session["placeId"].as_u64().unwrap_or(0);
    let toast_id = format!("{}:{place}:{}:{}", session["instanceId"].as_str().unwrap_or(""), session["leftAt"].as_str().unwrap_or(""), session["reason"].as_str().unwrap_or(""));
    {
        let mut sent = state.launcher_notifications.sent.lock();
        if sent.contains(&toast_id) {
            return;
        }
        if sent.len() >= MAX_TOAST_IDS {
            sent.remove(0);
        }
        sent.push(toast_id.clone());
    }
    let state = Arc::clone(state);
    let session = session.clone();
    tauri::async_runtime::spawn(async move {
        let universe = session["universeId"].as_u64();
        let (metadata, source) = match metadata_for(&state, place, universe).await {
            Ok(m) => (m, "resolved"),
            Err(error) => {
                tracing::error!("[LauncherNotifications] Failed to resolve post-game toast metadata: {error}");
                (fallback_metadata(place), "fallback")
            }
        };
        let duration = format_duration(session["durationMs"].as_u64().unwrap_or(0));
        let description = match metadata["creatorName"].as_str() {
            Some(creator) => format!("by {creator} - Lasted {duration}"),
            None => format!("Lasted {duration}"),
        };
        let analytics = json!({
            "creatorName": metadata["creatorName"], "durationMs": session["durationMs"], "instanceId": session["instanceId"], "metadataSource": source,
            "placeId": session["placeId"], "reason": session["reason"], "title": metadata["title"], "universeId": metadata["universeId"],
        });
        app_service::show_toast(
            &state,
            &json!({
                "analytics": { "notificationType": "activity_session_ended", "payload": analytics },
                "duration": TOAST_DURATION_MS,
                "id": toast_id,
                "image": metadata["thumbnailUrl"],
                "isDismissable": true,
                "title": format!("You played {}", metadata["title"].as_str().unwrap_or("")),
                "variant": "activity-session-ended",
                "variantData": { "description": description },
            }),
        );
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations() {
        assert_eq!(super::format_duration(59_000), "<1m");
        assert_eq!(super::format_duration(61 * 60_000), "1h 1m");
        assert_eq!(super::format_duration(5 * 60_000), "5m");
    }
}
