//! Builds this OS's backends. A backend that can't start is left out and logged; the engine then
//! sees failures for the features it would have provided (e.g. no capture → this node can't
//! control others, but can still be controlled).

use std::sync::Arc;

use crosspane_platform::{
    Displays, FrameCapture, GlobalHotkeys, InputCapture, IoGate, KeyInjector, KeyStore,
    OverlayHost, Permissions, PlatformError, PointerInjector, SessionEvents, WindowParking,
    WindowSource,
};
use crosspane_types::time::MonoTime;

pub struct Platform {
    pub gate: Arc<IoGate>,
    pub session: Box<dyn SessionEvents>,
    pub displays: Box<dyn Displays>,
    pub capture: Option<Box<dyn InputCapture>>,
    pub keys: Option<Box<dyn KeyInjector>>,
    pub pointer: Option<Box<dyn PointerInjector>>,
    pub overlay: Option<Box<dyn OverlayHost>>,
    pub hotkeys: Option<Box<dyn GlobalHotkeys>>,
    pub keystore: Option<Box<dyn KeyStore>>,
    pub permissions: Box<dyn Permissions>,
    // E2 (docs/wp/E2-v0.md)
    pub windows: Option<Box<dyn WindowSource>>,
    pub parking: Option<Box<dyn WindowParking>>,
    pub frames: Option<Box<dyn FrameCapture>>,
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Platform")
            .field("capture", &self.capture.is_some())
            .field("keys", &self.keys.is_some())
            .field("pointer", &self.pointer.is_some())
            .field("overlay", &self.overlay.is_some())
            .field("hotkeys", &self.hotkeys.is_some())
            .field("keystore", &self.keystore.is_some())
            .field("windows", &self.windows.is_some())
            .field("parking", &self.parking.is_some())
            .field("frames", &self.frames.is_some())
            .finish_non_exhaustive()
    }
}

/// Run `f` on a short-lived worker thread and wait for it. Mac parking refuses the main thread (its
/// waits need the main queue); at startup nothing needs that queue, since twin displays died with
/// the previous process and restoring a frame is Accessibility only.
#[cfg(target_os = "macos")]
fn off_main<T: Send>(
    f: impl FnOnce() -> Result<T, PlatformError> + Send,
) -> Result<T, PlatformError> {
    std::thread::scope(|scope| {
        scope
            .spawn(f)
            .join()
            .unwrap_or_else(|_| Err(PlatformError::Backend("parking recovery panicked".into())))
    })
}

/// The OS key store alone, for commands that need the identity but must not start the backends
/// (starting them recovers parked windows, which would undo a running agent's projections).
#[cfg(target_os = "linux")]
pub fn keystore() -> Option<Box<dyn KeyStore>> {
    optional(
        "keystore",
        crosspane_platform_linux::secret_service::SecretServiceStore::new(),
    )
    .map(|k| Box::new(k) as Box<dyn KeyStore>)
}

#[cfg(target_os = "macos")]
pub fn keystore() -> Option<Box<dyn KeyStore>> {
    Some(Box::new(
        crosspane_platform_macos::keychain::MacKeychain::new(),
    ))
}

fn optional<T>(what: &str, result: Result<T, PlatformError>) -> Option<T> {
    match result {
        Ok(backend) => Some(backend),
        Err(e) => {
            tracing::warn!(backend = what, error = %e, "backend unavailable");
            None
        }
    }
}

/// The node's monotonic clock, the one every backend stamps events with.
pub fn now() -> MonoTime {
    #[cfg(target_os = "macos")]
    {
        crosspane_platform_macos::clock::now()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let nanos = u64::try_from(t.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            + u64::try_from(t.tv_nsec).unwrap_or(0);
        MonoTime::from_nanos(nanos)
    }
}

#[cfg(target_os = "linux")]
pub fn create(
    state_dir: &std::path::Path,
    _config: &crate::config::Config,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_linux::hyprland::{
        capture::HyprlandCapture, displays::HyprlandDisplays, frame_capture::HyprlandFrameCapture,
        hotkeys::HyprlandHotkeys, inject, ipc::HyprIpc, overlay::HyprlandOverlay,
        parking::HyprlandParking, windows::HyprlandWindows,
    };
    use crosspane_platform_linux::{logind::LogindSession, permissions::LinuxPermissions};

    let gate = IoGate::new();
    let ipc = HyprIpc::from_env().context("Crosspane on Linux needs Hyprland")?;
    ipc.require_supported().context("unsupported Hyprland")?;
    let session = LogindSession::new(gate.clone(), Some(ipc.clone()))
        .context("logind session state (required: Crosspane fails closed without it)")?;
    let displays = HyprlandDisplays::new(ipc.clone()).context("Hyprland displays")?;
    // No window is lost (04 §8 invariant 4): undo a previous run's parking before anything else.
    let mut parking = optional(
        "parking",
        HyprlandParking::new(ipc.clone(), state_dir.join("parking.json")),
    );
    if let Some(parking) = &mut parking {
        use crosspane_platform::WindowParking as _;
        match parking.recover() {
            Ok(restored) if !restored.is_empty() => {
                tracing::warn!(
                    count = restored.len(),
                    "restored windows a previous run left parked"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "could not restore parked windows"),
        }
    }
    let windows = optional("windows", HyprlandWindows::new(ipc.clone()));
    let frames = optional(
        "frame capture",
        HyprlandFrameCapture::new(gate.clone(), ipc.clone()),
    );
    let (keys, pointer) = match optional("injection", inject::connect(gate.clone(), ipc)) {
        Some((k, p)) => (
            Some(Box::new(k) as Box<dyn KeyInjector>),
            Some(Box::new(p) as Box<dyn PointerInjector>),
        ),
        None => (None, None),
    };
    Ok(Platform {
        session: Box::new(session),
        displays: Box::new(displays),
        capture: optional("capture", HyprlandCapture::new(gate.clone()))
            .map(|c| Box::new(c) as Box<dyn InputCapture>),
        keys,
        pointer,
        overlay: optional("overlay", HyprlandOverlay::new())
            .map(|o| Box::new(o) as Box<dyn OverlayHost>),
        hotkeys: Some(Box::new(HyprlandHotkeys)),
        keystore: keystore(),
        permissions: Box::new(LinuxPermissions),
        windows: windows.map(|w| Box::new(w) as Box<dyn WindowSource>),
        parking: parking.map(|p| Box::new(p) as Box<dyn WindowParking>),
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        gate,
    })
}

#[cfg(target_os = "macos")]
pub fn create(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_macos::{
        capture::MacCapture, displays::MacDisplays, frame_capture::MacFrameCapture, inject,
        overlay::MacOverlay, parking::MacMirrorParking, permissions::MacPermissions,
        session::MacSession, windows::MacWindows,
    };

    let gate = IoGate::new();
    let session = MacSession::new(gate.clone())
        .context("macOS session state (required: Crosspane fails closed without it)")?;
    let displays = MacDisplays::new().context("macOS displays")?;
    // No window is lost (04 §8 invariant 4): each journal is recovered before anything else. The
    // twin journal is recovered whenever twin parking is compiled in, even if it has since been
    // switched off in the config.
    let mut parking: Option<Box<dyn WindowParking>> = optional(
        "parking",
        MacMirrorParking::new(state_dir.join("parking.json")),
    )
    .map(|p| Box::new(p) as Box<dyn WindowParking>);
    #[cfg(feature = "private-vdisplay")]
    {
        use crosspane_platform_macos::private_vdisplay::MacTwinParking;
        let twin = optional(
            "virtual display parking",
            MacTwinParking::new(state_dir.join("parking-twin.journal")),
        );
        match (twin, parking.take()) {
            (Some(twin), Some(mirror)) if config.mac_virtual_display => {
                tracing::info!("projected windows are hidden on virtual displays (D7)");
                parking = Some(Box::new(crate::twin::TwinOrMirror::new(
                    Box::new(twin),
                    mirror,
                )));
            }
            (Some(mut twin), mirror) => {
                if let Err(e) = off_main(|| twin.recover()) {
                    tracing::error!(error = %e, "could not restore windows parked on virtual displays");
                }
                parking = mirror;
            }
            (None, mirror) => parking = mirror,
        }
    }
    #[cfg(not(feature = "private-vdisplay"))]
    if config.mac_virtual_display {
        tracing::warn!(
            "mac_virtual_display needs a build with the private-vdisplay feature; mirroring (M1)"
        );
    }
    if let Some(Err(e)) = parking.as_mut().map(|p| off_main(|| p.recover())) {
        tracing::error!(error = %e, "could not restore parked windows");
    }
    let windows = optional("windows", MacWindows::new());
    let frames = optional("frame capture", MacFrameCapture::new(gate.clone()));
    let (keys, pointer) = match optional("injection", inject::injectors(gate.clone())) {
        Some((k, p)) => (
            Some(Box::new(k) as Box<dyn KeyInjector>),
            Some(Box::new(p) as Box<dyn PointerInjector>),
        ),
        None => (None, None),
    };
    Ok(Platform {
        session: Box::new(session),
        displays: Box::new(displays),
        capture: optional("capture", MacCapture::new(gate.clone()))
            .map(|c| Box::new(c) as Box<dyn InputCapture>),
        keys,
        pointer,
        overlay: optional("overlay", MacOverlay::new())
            .map(|o| Box::new(o) as Box<dyn OverlayHost>),
        hotkeys: None,
        keystore: keystore(),
        permissions: Box::new(MacPermissions::new()),
        windows: windows.map(|w| Box::new(w) as Box<dyn WindowSource>),
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        gate,
    })
}
