//! Frameless window controls (`window:*`), driven by the renderer's own
//! title bar buttons. Port of `WindowController`.
//!
//! Pending: `window:navigateMain` (second screen → main navigation).

use crate::ipc::Router;

pub fn register(router: &mut Router) {
    router
        .handle("window:minimize", |ctx, _| async move {
            ctx.window.minimize()?;
            Ok(())
        })
        .handle("window:toggleMaximize", |ctx, _| async move {
            if ctx.window.is_maximized()? {
                ctx.window.unmaximize()?;
            } else {
                ctx.window.maximize()?;
            }
            Ok(())
        })
        .handle("window:close", |ctx, _| async move {
            // Goes through CloseRequested, where close-to-tray is applied.
            ctx.window.close()?;
            Ok(())
        })
        .handle("window:getIsMaximized", |ctx, _| async move {
            Ok(ctx.window.is_maximized().unwrap_or(false))
        });
}
