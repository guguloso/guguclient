//! Backend → renderer push events (Electron's `webContents.send`).
//! The bridge routes `{ channel, payload }` to the same `api.*.onX(callback)`
//! subscribers the renderer already uses.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

const BRIDGE_EVENT: &str = "gugu:ipc-event";

#[derive(Clone)]
pub struct EventBus {
    app: AppHandle,
}

#[derive(Clone, Serialize)]
struct Envelope<'a, T> {
    channel: &'a str,
    payload: T,
}

impl EventBus {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }

    pub fn emit<T: Serialize + Clone>(&self, channel: &str, payload: T) {
        if let Err(error) = self.app.emit(BRIDGE_EVENT, Envelope { channel, payload }) {
            tracing::warn!(channel, %error, "failed to emit ipc event");
        }
    }
}
