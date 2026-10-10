//! Compositor-neutral XDG desktop portal adapters (WP-G0.1), shared by the GNOME and KDE backends:
//! the RemoteDesktop session and its EIS socket, EIS key and pointer injection, the
//! GlobalShortcuts release chord, and (WP-G2.4) a Mutter virtual monitor through the ScreenCast
//! portal (`virtual_screen`).
//!
//! Consent is caller-prepared: the agent starts a portal session at startup, off the input path.
//! The first start may show the desktop's consent dialog; the persisted restore token makes later
//! starts silent. Trait methods on the input path never wait for a dialog. Until a session is
//! active, the injectors are not live and the agent doesn't grant `InputAccept`.

use crosspane_platform::PlatformError;

pub mod eis;
pub mod screencast;
pub mod session;
pub mod shortcuts;
pub mod virtual_screen;

/// The app id portals know this process by. A non-sandboxed process has none of its own, so it is
/// registered through `org.freedesktop.host.portal.Registry` on every bus connection that talks to
/// a portal (the registration belongs to the connection).
pub const APP_ID: &str = "io.frostdev.crosspane.agent";

/// Register [`APP_ID`] on ashpd's shared session connection (used by the GlobalShortcuts worker and
/// any other proxy built without an explicit connection). Call once at startup, before any portal
/// call. Bounded: 2 s.
pub fn register_host_app() -> Result<(), PlatformError> {
    let app_id = ashpd::AppID::try_from(APP_ID)
        .map_err(|e| PlatformError::Backend(format!("portal app id: {e}")))?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("portal-register".into())
        .spawn(move || {
            let _ = tx.send(zbus::block_on(ashpd::register_host_app(app_id)));
        })
        .map_err(|e| PlatformError::Backend(format!("spawn portal registration: {e}")))?;
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(PlatformError::Backend(format!(
            "portal app registration: {e}"
        ))),
        Err(_) => Err(PlatformError::Timeout),
    }
}

/// Register [`APP_ID`] on a connection a backend owns. Failure is logged, not fatal: the portal
/// then treats the process as an anonymous host app.
pub(crate) async fn register_on(connection: &zbus::Connection) {
    let Ok(app_id) = ashpd::AppID::try_from(APP_ID) else {
        return;
    };
    if let Err(e) = ashpd::register_host_app_with_connection(connection.clone(), app_id).await {
        tracing::warn!(error = %e, "portal app registration failed");
    }
}
