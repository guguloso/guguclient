//! Product analytics (`analytics:track`) — disabled.
//! Kept as a no-op IPC handler so frontend `window.api.analytics.track(...)`
//! calls resolve without error while collecting or queueing nothing.

use crate::ipc::Router;

pub fn register(router: &mut Router) {
    router.handle("analytics:track", |_, _| async move { Ok(true) });
}
