//! Explicitly unsupported platform services, without native calls or retained resources.

use std::sync::Arc;

use crosspane_platform::{
    AudioCapture, AudioEvent, AudioFormat, AudioHost, AudioPlayback, CaptureAbort, CaptureEvent,
    CaptureId, CapturePortal, CaptureStart, CaptureTarget, Chord, Displays, EventSink,
    FrameCapture, FrameEvent, GlobalHotkeys, HotkeyEvent, InputCapture, Interface, KeyInjector,
    KeyStore, LinkInfo, LockState, Overlay, OverlayEvent, OverlayHost, OverlayId, Parked,
    Permission, PermissionState, Permissions, PlatformError, PointerInjector, PortalId,
    SessionEvent, SessionEvents, SessionState, StreamId, TrayEvent, TrayHost, TrayMenu,
    VirtualPorts, WindowEvent, WindowInfo, WindowParking, WindowSource,
};
use crosspane_types::{
    display::DisplayInfo,
    geom::{PixelRect, PixelSize, PointDevice},
    hid::{HidUsage, MouseButton},
    id::{DisplayId, NodeId, WindowId},
    input::{LockKeys, ScrollDelta},
};
use zeroize::Zeroizing;

const REASON: &str = "windows: not implemented (Phase 3 Lane W)";

// Keep every frozen method signature visible while sharing the identical refusal body.
macro_rules! unsupported {
    ($(fn $name:ident($($args:tt)*) -> $result:ty;)*) => {
        $(fn $name($($args)*) -> $result {
            Err(PlatformError::Unsupported(REASON))
        })*
    };
}

/// A placeholder for every platform service. Fallible operations always refuse; infallible
/// observations remain unknown or empty. It never captures, injects, opens or stores anything.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedWindows;

#[derive(Debug)]
struct InertAbort;

impl CaptureAbort for InertAbort {
    fn abort(&self) {
        // No capture can start, so there is no suppressed input or pending work to release.
    }
}

impl Displays for UnsupportedWindows {
    unsupported! {
        fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<Vec<DisplayInfo>>>) -> Result<(), PlatformError>;
    }
}

impl GlobalHotkeys for UnsupportedWindows {
    unsupported! {
        fn set_chord(&mut self, _: &Chord) -> Result<(), PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError>;
    }
}

impl TrayHost for UnsupportedWindows {
    unsupported! {
        fn subscribe(&mut self, _: Arc<dyn EventSink<TrayEvent>>) -> Result<(), PlatformError>;
        fn set(&mut self, _: &TrayMenu) -> Result<(), PlatformError>;
    }
}

impl AudioHost for UnsupportedWindows {
    unsupported! {
        fn add_peer(&mut self, _: NodeId, _: &str) -> Result<VirtualPorts, PlatformError>;
        fn remove_peer(&mut self, _: NodeId) -> Result<(), PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError>;
        fn open_capture(&mut self, _: AudioFormat) -> Result<AudioCapture, PlatformError>;
        fn open_playback(&mut self, _: AudioFormat) -> Result<AudioPlayback, PlatformError>;
    }
}

impl KeyStore for UnsupportedWindows {
    unsupported! {
        fn load(&self, _: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError>;
        fn store(&self, _: &str, _: &[u8]) -> Result<(), PlatformError>;
        fn delete(&self, _: &str) -> Result<(), PlatformError>;
    }
}

impl LinkInfo for UnsupportedWindows {
    unsupported! {
        fn interfaces(&self) -> Result<Vec<Interface>, PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<Vec<Interface>>>) -> Result<(), PlatformError>;
    }
}

impl OverlayHost for UnsupportedWindows {
    unsupported! {
        fn subscribe(&mut self, _: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError>;
        fn show(&mut self, _: OverlayId, _: &Overlay) -> Result<(), PlatformError>;
        fn hide(&mut self, _: OverlayId) -> Result<(), PlatformError>;
    }
}

impl Permissions for UnsupportedWindows {
    fn required(&self) -> Vec<Permission> {
        Vec::new()
    }

    fn state(&self, _: Permission) -> PermissionState {
        PermissionState::Unknown
    }

    unsupported! {
        fn request(&mut self, _: Permission) -> Result<(), PlatformError>;
        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<(Permission, PermissionState)>>,
        ) -> Result<(), PlatformError>;
    }
}

impl KeyInjector for UnsupportedWindows {
    unsupported! {
        fn key(&mut self, _: HidUsage, _: bool) -> Result<(), PlatformError>;
        fn lock_keys(&self) -> Result<LockKeys, PlatformError>;
        fn set_lock_keys(&mut self, _: LockKeys) -> Result<(), PlatformError>;
        fn release_all(&mut self) -> Result<(), PlatformError>;
        fn recover_keys(&mut self, _: &[HidUsage]) -> Result<(), PlatformError>;
    }
}

impl PointerInjector for UnsupportedWindows {
    unsupported! {
        fn move_to(&mut self, _: DisplayId, _: PointDevice) -> Result<(), PlatformError>;
        fn button(&mut self, _: MouseButton, _: bool) -> Result<(), PlatformError>;
        fn scroll(&mut self, _: ScrollDelta) -> Result<(), PlatformError>;
        fn release_all(&mut self) -> Result<(), PlatformError>;
        fn recover_buttons(&mut self, _: &[MouseButton]) -> Result<(), PlatformError>;
    }
}

impl InputCapture for UnsupportedWindows {
    unsupported! {
        fn set_portals(&mut self, _: &[CapturePortal]) -> Result<(), PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError>;
        fn begin(&mut self, _: CaptureId, _: PortalId) -> Result<CaptureStart, PlatformError>;
        fn begin_drag(
            &mut self,
            _: CaptureId,
            _: PortalId,
            _: MouseButton,
        ) -> Result<CaptureStart, PlatformError>;
        fn end(&mut self, _: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError>;
        fn set_monitor_local_activity(&mut self, _: bool) -> Result<(), PlatformError>;
    }

    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        Arc::new(InertAbort)
    }
}

impl WindowSource for UnsupportedWindows {
    unsupported! {
        fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError>;
        fn focused(&self) -> Result<Option<WindowId>, PlatformError>;
        fn activate(&mut self, _: WindowId) -> Result<(), PlatformError>;
        fn subscribe(&mut self, _: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError>;
    }
}

impl WindowParking for UnsupportedWindows {
    unsupported! {
        fn set_fullscreen(&mut self, _: WindowId, _: bool) -> Result<(), PlatformError>;
        fn park(&mut self, _: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError>;
        fn resize(&mut self, _: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError>;
        fn geometry(&self, _: WindowId) -> Result<Parked, PlatformError>;
        fn restore(&mut self, _: WindowId) -> Result<(), PlatformError>;
        fn restore_at(
            &mut self,
            _: WindowId,
            _: DisplayId,
            _: PointDevice,
        ) -> Result<(), PlatformError>;
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError>;
    }
}

impl FrameCapture for UnsupportedWindows {
    unsupported! {
        fn start(
            &mut self,
            _: CaptureTarget,
            _: Option<PixelRect>,
            _: u32,
            _: Arc<dyn EventSink<FrameEvent>>,
        ) -> Result<StreamId, PlatformError>;
        fn set_crop(&mut self, _: StreamId, _: Option<PixelRect>) -> Result<(), PlatformError>;
        fn stop(&mut self, _: StreamId) -> Result<(), PlatformError>;
    }
}

impl SessionEvents for UnsupportedWindows {
    fn state(&self) -> SessionState {
        SessionState {
            lock: LockState::Unknown,
            active: None,
        }
    }

    unsupported! {
        fn subscribe(&mut self, _: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError>;
    }
}
