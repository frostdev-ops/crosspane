//! The macOS main thread. AppKit objects (overlay panels, proxy windows, the menu-bar item) must
//! live on it, so the agent hands its main thread to [`run_app`] and every backend reaches it
//! through [`on_main`] (the "handle marshals calls there" rule in `crosspane-platform`).

use std::sync::mpsc;
use std::time::Duration;

use crosspane_platform::PlatformError;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply,
};
use objc2_foundation::{NSObject, NSObjectProtocol};

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

/// Run the non-hosted AppKit application with a deferred termination callback.
///
/// `terminate` does not return from AppKit's run loop. The callback must arrange for the process
/// to exit after bounded cleanup; this adapter defers AppKit's automatic successful exit while
/// keeping the main queue available. An existing delegate is never replaced.
#[cfg_attr(test, allow(dead_code))] // Existing source-included GUI harnesses use run_app instead.
pub fn run_app_with_termination(callback: impl Fn() + 'static) -> Result<(), PlatformError> {
    let mtm = MainThreadMarker::new().ok_or(PlatformError::Backend(
        "run_app_with_termination must be called on the main thread".into(),
    ))?;
    let app = NSApplication::sharedApplication(mtm);
    if app.delegate().is_some() {
        tracing::error!(
            "cannot install the agent termination callback: AppKit delegate already set"
        );
        return Err(PlatformError::Backend("AppKit delegate already set".into()));
    }
    let delegate = TerminationDelegate::new(Box::new(callback), mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    app.run();
    app.setDelegate(None);
    Ok(())
}

define_class!(
    // SAFETY: NSObject has no additional subclassing requirements; no custom Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "CrosspaneAgentTerminationDelegate"]
    #[ivars = Box<dyn Fn()>]
    struct TerminationDelegate;

    // SAFETY: NSObjectProtocol adds no requirements.
    unsafe impl NSObjectProtocol for TerminationDelegate {}

    // SAFETY: The method has NSApplicationDelegate's exact main-thread signature.
    unsafe impl NSApplicationDelegate for TerminationDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            deferred_termination(self.ivars())
        }
    }
);

#[cfg_attr(test, allow(dead_code))]
impl TerminationDelegate {
    fn new(callback: Box<dyn Fn()>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(callback);
        // SAFETY: NSObject's init takes no arguments; the Rust callback ivar is initialized.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg_attr(test, allow(dead_code))]
fn deferred_termination(callback: &dyn Fn()) -> NSApplicationTerminateReply {
    callback();
    NSApplicationTerminateReply::TerminateLater
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

#[cfg(test)]
mod tests {
    #[test]
    fn termination_callback_runs_before_appkit_is_deferred() {
        let called = std::cell::Cell::new(false);
        let reply = super::deferred_termination(&|| called.set(true));
        assert!(called.get());
        assert_eq!(
            reply,
            objc2_app_kit::NSApplicationTerminateReply::TerminateLater
        );
    }
}
