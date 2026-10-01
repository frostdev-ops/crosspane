//! The macOS main thread. AppKit objects (overlay panels, proxy windows, the menu-bar item) must
//! live on it, so the agent hands its main thread to [`run_app`] and every backend reaches it
//! through [`on_main`] (the "handle marshals calls there" rule in `crosspane-platform`).

use std::sync::mpsc;
use std::time::Duration;

use crosspane_platform::PlatformError;
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

/// Run the AppKit application on the current thread, which must be the process's main thread.
/// Crosspane is a menu-bar (accessory) app: no Dock icon, never activates on its own. Never
/// returns.
pub fn run_app() -> Result<(), PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::Backend(
        "run_app must be called on the main thread".into(),
    ))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    app.run();
    Ok(())
}

/// Run `f` on the main thread and return its result, waiting at most `timeout`. Runs `f` directly
/// when already on the main thread. If the wait times out, `f` may still run later; callers pass
/// closures that are safe to run late (e.g. they re-check their own state).
pub fn on_main<R, F>(timeout: Duration, f: F) -> Result<R, PlatformError>
where
    R: Send + 'static,
    F: FnOnce(MainThreadMarker) -> R + Send + 'static,
{
    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(f(mtm));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    DispatchQueue::main().exec_async(move || {
        // The main queue only ever runs on the main thread.
        if let Some(mtm) = MainThreadMarker::new() {
            let _ = tx.send(f(mtm));
        }
    });
    rx.recv_timeout(timeout).map_err(|e| match e {
        mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
        mpsc::RecvTimeoutError::Disconnected => {
            PlatformError::Backend("main-thread task did not run".into())
        }
    })
}

/// Run `f` on the main thread without waiting (fire and forget).
pub fn spawn_on_main<F>(f: F)
where
    F: FnOnce(MainThreadMarker) + Send + 'static,
{
    DispatchQueue::main().exec_async(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            f(mtm);
        }
    });
}
