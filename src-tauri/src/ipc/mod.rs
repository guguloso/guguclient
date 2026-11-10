//! IPC layer: one Tauri command (`ipc_invoke`) receives every
//! `window.api.<namespace>.<method>(...)` call from the renderer and dispatches
//! it to a typed async handler registered under `"<namespace>:<method>"` —
//! the exact channel names the Electron main process used.

mod args;
mod events;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use tauri::WebviewWindow;

pub use args::Args;
pub use events::EventBus;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Handler = Box<dyn Fn(Ctx, Args) -> BoxFuture<AppResult<Value>> + Send + Sync>;

/// Per-call context: shared app state plus the window that sent the call
/// (Electron's `event.sender`), needed by window-scoped channels.
#[derive(Clone)]
pub struct Ctx {
    pub state: Arc<AppState>,
    pub window: WebviewWindow,
}

#[derive(Default)]
pub struct Router {
    routes: HashMap<&'static str, Handler>,
}

impl Router {
    /// Builds the router with every ported service registered.
    pub fn build() -> Self {
        let mut router = Self {
            routes: HashMap::with_capacity(400),
        };
        crate::services::register_all(&mut router);
        router
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Registers `handler` for `channel`. Handlers are plain async closures
    /// returning any `Serialize` value; conversion to JSON happens once here.
    pub fn handle<F, Fut, T>(&mut self, channel: &'static str, handler: F) -> &mut Self
    where
        F: Fn(Ctx, Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AppResult<T>> + Send + 'static,
        T: Serialize,
    {
        let previous = self.routes.insert(
            channel,
            Box::new(move |ctx, args| {
                let fut = handler(ctx, args);
                Box::pin(async move { Ok(serde_json::to_value(fut.await?)?) })
            }),
        );
        debug_assert!(previous.is_none(), "duplicate IPC handler for {channel}");
        self
    }

    async fn dispatch(&self, ctx: Ctx, channel: String, args: Vec<Value>) -> AppResult<Value> {
        let handler = self
            .routes
            .get(channel.as_str())
            .ok_or_else(|| AppError::NotImplemented(channel.clone()))?;
        handler(ctx, Args::new(channel, args)).await
    }
}

/// Single entry point for all renderer → backend calls.
#[tauri::command]
pub async fn ipc_invoke(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
    router: tauri::State<'_, Arc<Router>>,
    channel: String,
    args: Vec<Value>,
) -> Result<Value, AppError> {
    let started = Instant::now();
    let ctx = Ctx {
        state: Arc::clone(&state),
        window,
    };
    let label = channel.clone();
    let result = router.dispatch(ctx, channel, args).await;
    match &result {
        Ok(_) => {
            tracing::trace!(channel = %label, elapsed_ms = started.elapsed().as_millis(), "ipc ok")
        }
        Err(AppError::NotImplemented(_)) => {
            tracing::debug!(channel = %label, "ipc channel not ported yet")
        }
        Err(error) => tracing::warn!(channel = %label, %error, "ipc failed"),
    }
    result
}
