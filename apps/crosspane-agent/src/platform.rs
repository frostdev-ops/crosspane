//! Builds this OS's backends. A backend that can't start is left out and logged; the engine then
//! sees failures for the features it would have provided (e.g. no capture → this node can't
//! control others, but can still be controlled).

use std::sync::Arc;

use crosspane_platform::{
    Displays, FrameCapture, GlobalHotkeys, InputCapture, IoGate, KeyInjector, KeyStore,
    OverlayHost, Permissions, PlatformError, PointerInjector, SessionEvents, TrayHost, WindowInfo,
    WindowParking, WindowSource,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PointDevice, RectLogical};
use crosspane_types::id::DisplayId;
#[cfg(target_os = "macos")]
use crosspane_types::id::WindowId;
use crosspane_types::time::MonoTime;

/// Injectable read of this process's on-screen Quartz identities and titles.
#[cfg(target_os = "macos")]
pub type OwnWindowsRead = fn() -> Result<Vec<(WindowId, String)>, PlatformError>;

/// What "home on the twin" (WP-2.43) needs from the compositor beyond the platform traits: the
/// keybind that exists only while this node's input is home in one of its own projected windows
/// (§2.9), and the physical pointer's position, which is how a warp is confirmed (amendment A3).
/// Only Hyprland has one; every other platform runs without it and home never commits there.
///
/// Every call is short-lived and bounded by the compositor IPC's own timeout. The agent's engine
/// thread makes them, and no call retries: the agent decides when to try again.
pub trait HomeSeat: Send {
    /// The Hyprland key string of the release chord, e.g. `CTRL + SHIFT + ALT + Escape`, for
    /// notices.
    fn keys(&self) -> String;
    /// Install the bind and verify it is listed (`Err` if another bind holds the chord, or if the
    /// command it runs is missing).
    fn install(&self) -> Result<(), PlatformError>;
    /// Remove the bind and verify it is absent. Idempotent; leaves a foreign bind on the chord
    /// alone (`Ok` when only a foreign bind is there, `Err` when it can't be told apart from ours).
    fn remove(&self) -> Result<(), PlatformError>;
    /// Whether the compositor lists exactly our bind.
    fn installed(&self) -> Result<bool, PlatformError>;
    /// Where the physical pointer is now: its display and its position in that display's device
    /// pixels. A plain reader: it knows nothing about the I/O gate.
    fn cursor(&self) -> Result<(DisplayId, PointDevice), PlatformError>;
    /// Call `reload` whenever the compositor reloads its config (which drops runtime binds) and
    /// whenever its event connection is made again (events may have been missed). Called once.
    fn watch_reload(&mut self, reload: Box<dyn Fn() + Send>) -> Result<(), PlatformError>;
}

/// Move only an identified proxy; return the compositor's freshly confirmed content frame.
pub trait ProxyPlacementSeat: Send {
    // Only the Hyprland drag gesture places proxies (WP-2.58); a Mac never calls it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn place(
        &self,
        window: &WindowInfo,
        display: &DisplayInfo,
        at: PointDevice,
    ) -> Result<RectLogical, PlatformError>;
}

#[cfg(target_os = "linux")]
struct HyprProxyPlacement<F>(F);

#[cfg(target_os = "linux")]
impl<F> ProxyPlacementSeat for HyprProxyPlacement<F>
where
    F: Fn(&str, std::time::Duration) -> Result<serde_json::Value, PlatformError> + Send,
{
    fn place(
        &self,
        window: &WindowInfo,
        display: &DisplayInfo,
        at: PointDevice,
    ) -> Result<RectLogical, PlatformError> {
        use crosspane_types::geom::{PointLogical, SizeLogical};
        use serde_json::Value;
        use std::time::{Duration, Instant};
        let bad = || PlatformError::Backend("drag proxy identity or geometry not confirmed".into());
        let deadline = Instant::now() + Duration::from_secs(1);
        let request = |command: &str| {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PlatformError::Timeout);
            }
            let result = (self.0)(command, left)?;
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            Ok(result)
        };
        let client = |json: &Value| -> Result<Value, PlatformError> {
            let mut matching = json.as_array().ok_or_else(bad)?.iter().filter(|c| {
                c["stableId"]
                    .as_str()
                    .and_then(|s| u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok())
                    == Some(window.id.0)
            });
            let c = matching.next().ok_or_else(bad)?;
            if matching.next().is_some()
                || c["pid"].as_u64() != window.pid.map(u64::from)
                || c["title"].as_str() != Some(window.title.as_str())
                || c["mapped"] != true
            {
                return Err(bad());
            }
            Ok(c.clone())
        };
        let pair = |json: &Value| -> Result<(f64, f64), PlatformError> {
            let v = json.as_array().filter(|v| v.len() == 2).ok_or_else(bad)?;
            let (x, y) = (
                v[0].as_f64().ok_or_else(bad)?,
                v[1].as_f64().ok_or_else(bad)?,
            );
            if !x.is_finite() || !y.is_finite() {
                return Err(bad());
            }
            Ok((x, y))
        };
        let before = client(&request("clients")?)?;
        let address = before["address"].as_str().ok_or_else(bad)?;
        let hex = address
            .strip_prefix("0x")
            .filter(|s| !s.is_empty() && s.len() <= 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(bad)?;
        let address = format!("0x{hex}");
        let monitors = request("monitors")?;
        let mut matching = monitors
            .as_array()
            .ok_or_else(bad)?
            .iter()
            .filter(|m| m["id"].as_u64() == Some(u64::from(display.id.0)));
        let monitor = matching.next().ok_or_else(bad)?;
        if matching.next().is_some() {
            return Err(bad());
        }
        let workspace = monitor["activeWorkspace"]["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(bad)?;
        let reserved = monitor["reserved"]
            .as_array()
            .filter(|v| v.len() == 4)
            .ok_or_else(bad)?;
        let mut r = [0.0; 4];
        for (out, value) in r.iter_mut().zip(reserved) {
            *out = value
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(bad)?;
        }
        let g = &display.geometry;
        let mut dimensions = (
            monitor["width"].as_u64().ok_or_else(bad)?,
            monitor["height"].as_u64().ok_or_else(bad)?,
        );
        if monitor["transform"].as_u64().ok_or_else(bad)? % 2 == 1 {
            dimensions = (dimensions.1, dimensions.0);
        }
        if !at.x.is_finite()
            || !at.y.is_finite()
            || !g.scale.is_finite()
            || g.scale <= 0.0
            || monitor["scale"].as_f64() != Some(g.scale)
            || monitor["x"].as_f64() != Some(g.logical_origin.x)
            || monitor["y"].as_f64() != Some(g.logical_origin.y)
            || dimensions
                != (
                    u64::from(g.pixel_size.width),
                    u64::from(g.pixel_size.height),
                )
            || r[0] + r[2] >= f64::from(g.pixel_size.width) / g.scale
            || r[1] + r[3] >= f64::from(g.pixel_size.height) / g.scale
        {
            return Err(bad());
        }
        let guard = format!(
            "local w = hl.get_window(\"address:{address}\"); if not w or w.stable_id ~= {} then error(\"proxy changed\") end; hl.dispatch(",
            window.id.0
        );
        request(&format!(
            "{guard}hl.dsp.window.move({{ window = \"address:{address}\", workspace = \"{workspace}\", follow = false }}))"
        ))?;
        request(&format!(
            "{guard}hl.dsp.window.float({{ window = \"address:{address}\", action = \"enable\" }}))"
        ))?;
        // Enabling float can restore a remembered floating size; clamp that actual content.
        let floated = client(&request("clients")?)?;
        if floated["address"].as_str() != Some(address.as_str()) || floated["floating"] != true {
            return Err(bad());
        }
        let (width, height) = pair(&floated["size"])?;
        if width <= 0.0 || height <= 0.0 {
            return Err(bad());
        }
        let min_x = (g.logical_origin.x + r[0]).ceil();
        let min_y = (g.logical_origin.y + r[1]).ceil();
        let max_x = (g.logical_origin.x + f64::from(g.pixel_size.width) / g.scale - r[2] - width)
            .floor()
            .max(min_x);
        let max_y = (g.logical_origin.y + f64::from(g.pixel_size.height) / g.scale - r[3] - height)
            .floor()
            .max(min_y);
        let x = (g.logical_origin.x + at.x / g.scale)
            .round()
            .clamp(min_x, max_x);
        let y = (g.logical_origin.y + at.y / g.scale)
            .round()
            .clamp(min_y, max_y);
        request(&format!(
            "{guard}hl.dsp.window.move({{ window = \"address:{address}\", x = {x}, y = {y}, relative = false }}))"
        ))?;
        let after = client(&request("clients")?)?;
        let (seen_x, seen_y) = pair(&after["at"])?;
        let (width, height) = pair(&after["size"])?;
        if after["address"].as_str() != Some(address.as_str())
            || after["floating"] != true
            || after["monitor"].as_u64() != Some(u64::from(display.id.0))
            || after["workspace"]["id"].as_i64() != Some(workspace)
            || ((seen_x - x) * g.scale).abs() > 1.0
            || ((seen_y - y) * g.scale).abs() > 1.0
            || width <= 0.0
            || height <= 0.0
            || seen_x < g.logical_origin.x + r[0]
            || seen_y < g.logical_origin.y + r[1]
            || seen_x + width > g.logical_origin.x + f64::from(g.pixel_size.width) / g.scale - r[2]
            || seen_y + height
                > g.logical_origin.y + f64::from(g.pixel_size.height) / g.scale - r[3]
        {
            return Err(bad());
        }
        Ok(RectLogical::new(
            PointLogical::new(seen_x, seen_y),
            SizeLogical::new(width, height),
        ))
    }
}

#[cfg(target_os = "linux")]
fn proxy_placement() -> anyhow::Result<Box<dyn ProxyPlacementSeat>> {
    use anyhow::Context;
    use crosspane_platform_linux::hyprland::ipc::HyprIpc;
    let signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("proxy Hyprland instance")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("proxy Hyprland runtime")?;
    Ok(Box::new(HyprProxyPlacement(move |command: &str, left| {
        let ipc = HyprIpc::new(&signature, std::path::Path::new(&runtime), left);
        if matches!(command, "clients" | "monitors") {
            ipc.json(command)
        } else {
            ipc.eval(command).map(|()| serde_json::Value::Null)
        }
    })))
}

/// What the startup recovery of parked windows came to (WP-4.5). `create` runs every parking
/// backend's `recover()` before anything else (04 §8 invariant 4) and keeps the outcome here, for
/// `status.result.installer.startup_recovery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupRecovery {
    /// Every backend recovered, and this many windows were put back.
    Restored(usize),
    /// Every backend recovered, and nothing had been left parked.
    NothingParked,
    /// At least one backend could not recover: journal entries may remain.
    Failed,
    /// There is no parking backend, so nothing was recovered.
    None,
}

impl StartupRecovery {
    /// Combine what each parking backend's `recover()` came to: `Ok(n)` windows restored, or
    /// `Err(())` for a failure. No backend at all is `None`; any failure is `Failed`, whatever the
    /// others did.
    pub fn combine(outcomes: &[Result<usize, ()>]) -> StartupRecovery {
        if outcomes.is_empty() {
            return StartupRecovery::None;
        }
        let mut restored = 0usize;
        for outcome in outcomes {
            match outcome {
                Ok(count) => restored = restored.saturating_add(*count),
                Err(()) => return StartupRecovery::Failed,
            }
        }
        if restored > 0 {
            StartupRecovery::Restored(restored)
        } else {
            StartupRecovery::NothingParked
        }
    }

    /// The spelling `status` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            StartupRecovery::Restored(_) => "restored",
            StartupRecovery::NothingParked => "nothing_parked",
            StartupRecovery::Failed => "failed",
            StartupRecovery::None => "none",
        }
    }
}

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
    // E2 window projection
    pub windows: Option<Box<dyn WindowSource>>,
    /// Focuses a window from the parking worker's thread, for a source whose focus call can wait
    /// (Windows: it waits up to two seconds when the OS denies the foreground request). `None`:
    /// the agent calls `WindowSource::activate` itself. The worker takes it when it starts.
    pub window_activator: Option<crate::parking_worker::Activator>,
    pub parking: Option<Box<dyn WindowParking>>,
    pub frames: Option<Box<dyn FrameCapture>>,
    /// The tray / menu-bar icon (WP-1.34).
    pub tray: Option<Box<dyn TrayHost>>,
    /// Network interfaces and their link class (WP-1.7/1.8, 03 §2).
    pub links: Option<Box<dyn crosspane_platform::LinkInfo>>,
    /// The source side's GPU (GPU-v0): captured frames are hashed and converted to NV12 on it. On
    /// Linux it's the compositor's GPU, which DMA-BUF capture allocates on.
    pub gpu: Option<GpuDevice>,
    /// Home on the twin (WP-2.43): the release bind and the pointer read-back. `None` off
    /// Hyprland, and on Hyprland when the bind can't be spelled (the agent logs why).
    pub home: Option<Box<dyn HomeSeat>>,
    /// Proxy placement for the drag gesture (WP-2.58): Hyprland only, never read on a Mac.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub proxy_placement: Option<Box<dyn ProxyPlacementSeat>>,
    #[cfg(target_os = "macos")]
    pub visible_frame: fn(DisplayId) -> Result<RectLogical, PlatformError>,
    #[cfg(target_os = "macos")]
    pub own_windows: OwnWindowsRead,
    /// What the startup recovery of parked windows came to (WP-4.5).
    pub startup_recovery: StartupRecovery,
    /// Proof that the native factory admitted its exact isolated acceptance contract.
    #[cfg(windows)]
    pub(crate) acceptance_scratch: bool,
    /// Only the admitted owned-window source fixture omits clipboard and unused input.
    #[cfg(windows)]
    pub(crate) acceptance_source: bool,
    /// Fresh Windows placement rows. The refresh closure is weak; `displays` owns its worker.
    #[cfg(windows)]
    pub host_placement_mapping: Option<crosspane_render::proxy::HostPlacementMapping>,
    /// The same fresh observation as `host_placement_mapping`, with each monitor's work area, for
    /// the agent's own proxy placement (WP-W1.11 B2). Same weak refresh; never cached.
    #[cfg(windows)]
    pub windows_placement_rows: Option<WindowsPlacementRows>,
}

/// A wgpu device for the source side's GPU work.
#[derive(Clone, Debug)]
pub struct GpuDevice {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    #[cfg(windows)]
    pub windows: std::sync::Arc<crosspane_platform_windows::gpu::WindowsGpu>,
}

/// GPU frame paths are on unless `CROSSPANE_GPU=0` (for comparisons and as an escape hatch).
#[cfg(unix)]
fn gpu_enabled() -> bool {
    std::env::var("CROSSPANE_GPU").as_deref() != Ok("0")
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
            .field("tray", &self.tray.is_some())
            .field("gpu", &self.gpu.is_some())
            .field("home", &self.home.is_some())
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

/// Audio sharing is on unless `CROSSPANE_AUDIO=0` (an escape hatch, and how test harnesses keep an
/// agent off the machine's audio server).
pub(crate) fn audio_enabled() -> bool {
    std::env::var("CROSSPANE_AUDIO").as_deref() != Ok("0")
}

/// This OS's audio backend for speaker v0 (AUDIO-v0 §4): PipeWire on Linux, CoreAudio on macOS.
/// Constructing it connects to the audio server, so with `CROSSPANE_AUDIO=0` nothing is touched.
/// `None` (and a log line) when it can't start: the node then doesn't advertise `audio`, and peers
/// see it as a machine without audio sharing. The backend refuses to open a device while `gate` is
/// closed (04 §7).
pub fn audio_host(gate: Arc<IoGate>) -> Option<Box<dyn crosspane_platform::AudioHost>> {
    if !audio_enabled() {
        tracing::info!("audio sharing is off (CROSSPANE_AUDIO=0)");
        return None;
    }
    #[cfg(target_os = "linux")]
    let host = crosspane_platform_linux::audio::PipeWireAudioHost::new(gate)
        .map(|h| Box::new(h) as Box<dyn crosspane_platform::AudioHost>);
    #[cfg(target_os = "macos")]
    let host = crosspane_platform_macos::audio::CoreAudioHost::new(gate)
        .map(|h| Box::new(h) as Box<dyn crosspane_platform::AudioHost>);
    #[cfg(windows)]
    let host = crosspane_platform_windows::audio::WindowsAudioHost::new(gate)
        .map(|h| Box::new(h) as Box<dyn crosspane_platform::AudioHost>);
    match host {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::info!(error = %e, "no audio backend: audio sharing is off");
            None
        }
    }
}

/// Called on the process main thread during startup: AppKit construction must not wait on
/// a main thread blocked in the agent factory. All subsequent host calls belong to the worker.
pub fn clipboard_host(gate: Arc<IoGate>) -> Option<Box<dyn crosspane_platform::ClipboardHost>> {
    #[cfg(target_os = "linux")]
    let host = std::env::var_os("WAYLAND_DISPLAY")
        .ok_or(PlatformError::NotFound)
        .and_then(|display| {
            crosspane_platform_linux::hyprland::clipboard::HyprlandClipboard::new(gate, display)
        })
        .map(|host| Box::new(host) as Box<dyn crosspane_platform::ClipboardHost>);
    #[cfg(target_os = "macos")]
    let host = crosspane_platform_macos::clipboard::MacClipboard::new(
        gate,
        crosspane_platform_macos::clipboard::PasteboardName::General,
    )
    .map(|host| Box::new(host) as Box<dyn crosspane_platform::ClipboardHost>);
    #[cfg(windows)]
    let host: Result<Box<dyn crosspane_platform::ClipboardHost>, PlatformError> = {
        crosspane_platform_windows::clipboard::WindowsClipboard::new(gate)
            .map(|host| Box::new(host) as Box<dyn crosspane_platform::ClipboardHost>)
    };
    match host {
        Ok(host) => Some(host),
        Err(error) => {
            tracing::info!(reason = ?crate::clipboard::failure(&error), "clipboard backend unavailable");
            None
        }
    }
}

/// The video codecs for E2's motion path (WP-2.14), if this build and machine have them. With the
/// source GPU, NVENC takes NV12 straight from GPU memory (WP-2.29).
#[cfg(any(not(all(windows, feature = "video")), test))]
pub fn video_codecs(
    gpu: Option<&GpuDevice>,
) -> Option<std::sync::Arc<dyn crosspane_media::codec::VideoCodecs>> {
    let _ = gpu;
    #[cfg(all(target_os = "linux", feature = "video"))]
    {
        match crosspane_platform_linux::video::FfmpegCodecs::new() {
            Ok(c) => {
                let c = match gpu {
                    Some(gpu) => c.with_gpu(gpu.device.clone()),
                    None => c,
                };
                return Some(std::sync::Arc::new(c));
            }
            Err(e) => tracing::warn!(error = %e, "no video codecs: lossless tiles only"),
        }
    }
    #[cfg(all(target_os = "macos", feature = "video"))]
    {
        return Some(std::sync::Arc::new(
            crosspane_platform_macos::video::VtCodecs::new(),
        ));
    }
    #[cfg(all(windows, feature = "video"))]
    {
        return Some(std::sync::Arc::new(gpu.map_or_else(
            crosspane_platform_windows::video::MfCodecs::new,
            |gpu| crosspane_platform_windows::video::MfCodecs::new().with_gpu(gpu.windows.clone()),
        )));
    }
    #[allow(unreachable_code)]
    None
}

/// The receive device belongs to the proxy host, independently of the source GPU. The
/// bridge is metadata-only until the host reports its real device; the codecs retain it.
#[cfg(all(windows, feature = "video"))]
pub(crate) fn windows_receive_codecs(
    host: Option<crosspane_render::proxy::HostHandle>,
    source: Option<&GpuDevice>,
) -> (
    Arc<dyn crosspane_media::codec::VideoCodecs>,
    Option<Arc<crosspane_platform_windows::video::MfDecodeGpu>>,
) {
    use crosspane_platform_windows::video::{MfCodecs, MfDecodeGpu};
    let gpu = host.map(|host| {
        Arc::new(MfDecodeGpu::new(Arc::new(move || {
            // This is the initial wake for a copy with no pending redraw. Queue callbacks
            // cannot make progress until the host polls its device.
            let _ = host.send(crosspane_render::proxy::HostCommand::Run(Box::new(|| {})));
        })))
    });
    let codecs = source.map_or_else(MfCodecs::new, |source| {
        MfCodecs::new().with_gpu(source.windows.clone())
    });
    let codecs = match &gpu {
        Some(gpu) => codecs.with_decode_gpu(gpu.clone()),
        None => codecs,
    };
    (Arc::new(codecs), gpu)
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

#[cfg(windows)]
pub fn keystore() -> Option<Box<dyn KeyStore>> {
    Some(Box::new(
        crosspane_platform_windows::keystore::WindowsKeyStore::new(),
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

/// An input-capture backend that replaces itself when it reports that it is permanently lost
/// (WP-W4.1d CAP-REBUILD).
///
/// The Windows backend turns every fault into a terminal state: its owner thread leaves its loop
/// and every later request answers "observation lost". The agent builds that backend once, so
/// without this wrapper E1 stays broken until the agent restarts. The wrapper is the capture the
/// agent holds (`Platform::capture`); it forwards every request to the current backend and, when a
/// request fails and the backend says it can never serve again, drops it, builds a new one through
/// the same factory, gives it back the agent's subscription, portals and local-activity setting,
/// and runs the request once more on it. Rebuilds are bounded (`MAX_REBUILDS` per
/// `REBUILD_WINDOW`); beyond that the backend stays down until the window slides, with one
/// `error!` per episode and no retry loop. The `InputCapture` contract is unchanged.
#[cfg(any(windows, test))]
pub(crate) mod rebuilding_capture {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crosspane_platform::{
        CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, EventSink,
        InputCapture, PlatformError, PortalId,
    };
    use crosspane_types::geom::PointDevice;
    use crosspane_types::hid::MouseButton;
    use crosspane_types::id::DisplayId;

    /// At most this many rebuilds in any `REBUILD_WINDOW`, whether they succeed or not.
    pub(crate) const MAX_REBUILDS: usize = 3;
    pub(crate) const REBUILD_WINDOW: Duration = Duration::from_secs(60);

    /// A backend and the way to ask whether it can never serve another request.
    pub(crate) struct Built {
        pub(crate) backend: Box<dyn InputCapture>,
        /// `Some(cause)` once `backend` is permanently lost. A fixed label (a fault site, never
        /// input data). Cheap, and it must not keep the backend alive.
        pub(crate) lost: Box<dyn Fn() -> Option<&'static str> + Send>,
    }

    /// Builds a replacement. It runs after the lost backend has been dropped, so it may wait for
    /// that backend's native owner to let go.
    pub(crate) type Build = Box<dyn FnMut() -> Result<Built, PlatformError> + Send>;

    /// The rebuild budget: a sliding window over the times a rebuild was taken.
    #[derive(Debug, Default)]
    pub(crate) struct RebuildLimiter {
        taken: VecDeque<Instant>,
    }

    impl RebuildLimiter {
        /// Takes one rebuild at `now`. `false` once `MAX_REBUILDS` were taken within the last
        /// `REBUILD_WINDOW`.
        pub(crate) fn try_take(&mut self, now: Instant) -> bool {
            while self
                .taken
                .front()
                .is_some_and(|&at| now.saturating_duration_since(at) >= REBUILD_WINDOW)
            {
                self.taken.pop_front();
            }
            if self.taken.len() >= MAX_REBUILDS {
                return false;
            }
            self.taken.push_back(now);
            true
        }
    }

    struct NoAbort;
    impl CaptureAbort for NoAbort {
        fn abort(&self) {}
    }

    pub(crate) struct RebuildingCapture {
        /// `None` after a rebuild failed: the next request tries again, within the budget.
        slot: Option<Built>,
        build: Build,
        limiter: RebuildLimiter,
        now: Box<dyn Fn() -> Instant + Send>,
        /// The exhaustion `error!` was written for this episode (reset by the next rebuild).
        exhausted_logged: bool,
        /// What the agent asked of the first backend, replayed on each replacement.
        sink: Option<Arc<dyn EventSink<CaptureEvent>>>,
        portals: Vec<CapturePortal>,
        monitor_local_activity: bool,
    }

    impl std::fmt::Debug for RebuildingCapture {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RebuildingCapture")
                .field("serving", &self.slot.is_some())
                .finish_non_exhaustive()
        }
    }

    impl RebuildingCapture {
        #[cfg(windows)]
        pub(crate) fn new(first: Built, build: Build) -> Self {
            Self::with_clock(first, build, Box::new(Instant::now))
        }

        pub(crate) fn with_clock(
            first: Built,
            build: Build,
            now: Box<dyn Fn() -> Instant + Send>,
        ) -> Self {
            Self {
                slot: Some(first),
                build,
                limiter: RebuildLimiter::default(),
                now,
                exhausted_logged: false,
                sink: None,
                portals: Vec::new(),
                monitor_local_activity: false,
            }
        }

        /// Why the current backend can never serve again; `None` while it serves.
        fn lost(&self) -> Option<&'static str> {
            match &self.slot {
                Some(built) => (built.lost)(),
                None => Some("the previous rebuild failed"),
            }
        }

        /// Replaces a permanently lost backend, if the budget allows. `true` when a working
        /// replacement is installed.
        fn rebuild(&mut self) -> bool {
            let Some(cause) = self.lost() else {
                return false;
            };
            if !self.limiter.try_take((self.now)()) {
                // The lost backend stays in place (and keeps answering with its own error).
                if !std::mem::replace(&mut self.exhausted_logged, true) {
                    tracing::error!(
                        cause,
                        max = MAX_REBUILDS,
                        window_s = REBUILD_WINDOW.as_secs(),
                        "capture backend lost and the rebuild limit is reached: input capture stays down"
                    );
                }
                return false;
            }
            self.exhausted_logged = false;
            tracing::info!(cause, "capture backend lost: rebuilding it");
            // The lost backend goes first: its native owner must be gone before another can exist.
            self.slot = None;
            let rebuilt = (self.build)().and_then(|built| self.restore(built));
            match rebuilt {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%error, cause, "capture backend rebuild failed");
                    false
                }
            }
        }

        /// Gives `built` what the agent had set up on the backend it replaces, then installs it.
        /// The portals and the local-activity setting are best effort: the engine re-sends its
        /// portals when its layout changes, and the first request fails visibly if they are missing.
        fn restore(&mut self, mut built: Built) -> Result<(), PlatformError> {
            if let Some(sink) = &self.sink {
                built.backend.subscribe(sink.clone())?;
            }
            if !self.portals.is_empty()
                && let Err(error) = built.backend.set_portals(&self.portals)
            {
                tracing::warn!(%error, "capture portals were not restored on the rebuilt backend");
            }
            if self.monitor_local_activity
                && let Err(error) = built.backend.set_monitor_local_activity(true)
                && !matches!(error, PlatformError::Unsupported(_))
            {
                tracing::warn!(%error, "local-activity monitoring was not restored on the rebuilt backend");
            }
            self.slot = Some(built);
            Ok(())
        }

        /// Runs `op` on the current backend. If it fails and the backend is permanently lost, the
        /// backend is replaced and `op` runs once more on the replacement.
        fn run<T>(
            &mut self,
            op: impl Fn(&mut dyn InputCapture) -> Result<T, PlatformError>,
        ) -> Result<T, PlatformError> {
            let first = match self.slot.as_mut() {
                Some(built) => match op(built.backend.as_mut()) {
                    Ok(value) => return Ok(value),
                    Err(error) => error,
                },
                None => PlatformError::Backend("capture backend unavailable".into()),
            };
            if self.rebuild()
                && let Some(built) = self.slot.as_mut()
            {
                return op(built.backend.as_mut());
            }
            Err(first)
        }
    }

    impl InputCapture for RebuildingCapture {
        fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
            self.run(|backend| backend.set_portals(portals))?;
            self.portals = portals.to_vec();
            Ok(())
        }

        fn subscribe(
            &mut self,
            sink: Arc<dyn EventSink<CaptureEvent>>,
        ) -> Result<(), PlatformError> {
            // Called once, before anything can be lost: no rebuild here (a replacement is
            // subscribed by `restore`, which a second `subscribe` on it would contradict).
            let built = self
                .slot
                .as_mut()
                .ok_or_else(|| PlatformError::Backend("capture backend unavailable".into()))?;
            built.backend.subscribe(sink.clone())?;
            self.sink = Some(sink);
            Ok(())
        }

        fn begin(
            &mut self,
            id: CaptureId,
            portal: PortalId,
        ) -> Result<CaptureStart, PlatformError> {
            self.run(|backend| backend.begin(id, portal))
        }

        fn begin_drag(
            &mut self,
            id: CaptureId,
            portal: PortalId,
            button: MouseButton,
        ) -> Result<CaptureStart, PlatformError> {
            self.run(|backend| backend.begin_drag(id, portal, button))
        }

        fn end(&mut self, warp_to: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
            self.run(|backend| backend.end(warp_to))
        }

        /// The handle of the current backend. One taken before a rebuild reaches only the backend
        /// it was taken from, and keeps that backend's native owner from being released: the
        /// agent takes none.
        fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
            match &self.slot {
                Some(built) => built.backend.abort_handle(),
                None => Arc::new(NoAbort),
            }
        }

        fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError> {
            self.run(|backend| backend.set_monitor_local_activity(on))?;
            self.monitor_local_activity = on;
            Ok(())
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::*;
        use crosspane_platform::Edge;
        use crosspane_types::input::LockKeys;
        use std::collections::BTreeSet;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};

        /// What the fake backends did and which of them are lost, shared with the test.
        #[derive(Default)]
        struct World {
            /// In order: "build 1", "0 drop", "1 begin 9", ...
            events: Vec<String>,
            /// Backends that report themselves permanently lost and refuse every request.
            lost: BTreeSet<usize>,
            builds: usize,
            refuse_builds: usize,
            refuse_subscribe: BTreeSet<usize>,
            refuse_portals: bool,
            sinks: Vec<(usize, Arc<dyn EventSink<CaptureEvent>>)>,
        }
        type Handle = Arc<Mutex<World>>;

        fn world(handle: &Handle) -> std::sync::MutexGuard<'_, World> {
            handle.lock().unwrap_or_else(|e| e.into_inner())
        }

        struct Fake {
            index: usize,
            world: Handle,
        }

        impl Fake {
            fn note(&self, what: &str) {
                world(&self.world)
                    .events
                    .push(format!("{} {what}", self.index));
            }
            fn check(&self) -> Result<(), PlatformError> {
                if world(&self.world).lost.contains(&self.index) {
                    Err(PlatformError::Backend(
                        "capture observation lost (test)".into(),
                    ))
                } else {
                    Ok(())
                }
            }
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                self.note("drop");
            }
        }

        impl InputCapture for Fake {
            fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
                self.check()?;
                if world(&self.world).refuse_portals {
                    return Err(PlatformError::NotFound);
                }
                let ids: Vec<u32> = portals.iter().map(|p| p.id.0).collect();
                self.note(&format!("portals {ids:?}"));
                Ok(())
            }
            fn subscribe(
                &mut self,
                sink: Arc<dyn EventSink<CaptureEvent>>,
            ) -> Result<(), PlatformError> {
                if world(&self.world).refuse_subscribe.contains(&self.index) {
                    return Err(PlatformError::Backend("subscribe refused".into()));
                }
                self.note("subscribe");
                world(&self.world).sinks.push((self.index, sink));
                Ok(())
            }
            fn begin(
                &mut self,
                id: CaptureId,
                _portal: PortalId,
            ) -> Result<CaptureStart, PlatformError> {
                self.check()?;
                self.note(&format!("begin {}", id.0));
                Ok(CaptureStart {
                    held_keys: Vec::new(),
                    lock_keys: LockKeys::default(),
                })
            }
            fn end(
                &mut self,
                warp_to: Option<(DisplayId, PointDevice)>,
            ) -> Result<(), PlatformError> {
                self.check()?;
                self.note(&format!("end warp={}", warp_to.is_some()));
                Ok(())
            }
            fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
                Arc::new(NoAbort)
            }
            fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError> {
                self.check()?;
                self.note(&format!("monitor {on}"));
                Ok(())
            }
        }

        fn built(index: usize, handle: &Handle) -> Built {
            let probe = handle.clone();
            Built {
                backend: Box::new(Fake {
                    index,
                    world: handle.clone(),
                }),
                lost: Box::new(move || world(&probe).lost.contains(&index).then_some("test fault")),
            }
        }

        /// A wrapper over backend 0 whose factory makes backends 1, 2, ..., and a clock the test
        /// moves (milliseconds after a fixed start).
        fn rig() -> (RebuildingCapture, Handle, Arc<AtomicU64>) {
            let handle = Handle::default();
            let factory = handle.clone();
            let build: Build = Box::new(move || {
                let index = {
                    let mut state = world(&factory);
                    if state.refuse_builds > 0 {
                        state.refuse_builds -= 1;
                        state.events.push("build refused".into());
                        return Err(PlatformError::Backend("build refused".into()));
                    }
                    state.builds += 1;
                    let index = state.builds;
                    state.events.push(format!("build {index}"));
                    index
                };
                Ok(built(index, &factory))
            });
            let start = Instant::now();
            let offset = Arc::new(AtomicU64::new(0));
            let clock = offset.clone();
            let capture = RebuildingCapture::with_clock(
                built(0, &handle),
                build,
                Box::new(move || start + Duration::from_millis(clock.load(Ordering::SeqCst))),
            );
            (capture, handle, offset)
        }

        fn events(handle: &Handle) -> Vec<String> {
            world(handle).events.clone()
        }

        fn count(handle: &Handle, what: &str) -> usize {
            world(handle)
                .events
                .iter()
                .filter(|e| e.ends_with(what))
                .count()
        }

        fn kill(handle: &Handle, index: usize) {
            world(handle).lost.insert(index);
        }

        fn portal(id: u32) -> CapturePortal {
            CapturePortal {
                id: PortalId(id),
                display: DisplayId(1),
                edge: Edge::Right,
                from: 0.0,
                to: 100.0,
            }
        }

        fn begin(capture: &mut RebuildingCapture) -> Result<CaptureStart, PlatformError> {
            capture.begin(CaptureId(9), PortalId(1))
        }

        #[test]
        fn a_serving_backend_is_used_as_it_is() {
            let (mut capture, handle, _) = rig();
            capture.set_portals(&[portal(1)]).unwrap();
            begin(&mut capture).unwrap();
            capture.end(None).unwrap();
            assert_eq!(world(&handle).builds, 0);
            assert_eq!(
                events(&handle),
                ["0 portals [1]", "0 begin 9", "0 end warp=false"]
            );
        }

        #[test]
        fn a_request_that_fails_on_a_lost_backend_rebuilds_and_runs_on_the_replacement() {
            let (mut capture, handle, _) = rig();
            let received = Arc::new(Mutex::new(Vec::new()));
            let collector = received.clone();
            capture
                .subscribe(Arc::new(move |event: CaptureEvent| {
                    collector.lock().unwrap().push(event)
                }))
                .unwrap();
            capture.set_portals(&[portal(1), portal(2)]).unwrap();
            capture.set_monitor_local_activity(true).unwrap();
            kill(&handle, 0);
            begin(&mut capture).unwrap();
            // The lost backend is gone before its replacement is built, and the replacement has
            // the subscription, the portals and the monitoring setting before the request runs.
            assert_eq!(
                events(&handle),
                [
                    "0 subscribe",
                    "0 portals [1, 2]",
                    "0 monitor true",
                    "0 drop",
                    "build 1",
                    "1 subscribe",
                    "1 portals [1, 2]",
                    "1 monitor true",
                    "1 begin 9",
                ]
            );
            // Its events reach the sink the agent gave the first backend.
            let sink = world(&handle)
                .sinks
                .iter()
                .find(|(index, _)| *index == 1)
                .map(|(_, sink)| sink.clone())
                .unwrap();
            sink.send(CaptureEvent::KeyboardBlinded(true));
            assert_eq!(
                *received.lock().unwrap(),
                [CaptureEvent::KeyboardBlinded(true)]
            );
            // A later request goes straight to the replacement.
            capture.end(None).unwrap();
            assert_eq!(world(&handle).builds, 1);
        }

        #[test]
        fn the_new_portal_set_replaces_the_replayed_one_when_the_set_itself_is_what_failed() {
            let (mut capture, handle, _) = rig();
            capture.set_portals(&[portal(1)]).unwrap();
            kill(&handle, 0);
            capture.set_portals(&[portal(2), portal(3)]).unwrap();
            assert_eq!(
                events(&handle),
                [
                    "0 portals [1]",
                    "0 drop",
                    "build 1",
                    "1 portals [1]",
                    "1 portals [2, 3]",
                ]
            );
            // The newest set is the one a later replacement gets back.
            kill(&handle, 1);
            capture.end(None).unwrap();
            assert_eq!(events(&handle).last().unwrap(), "2 end warp=false");
            assert!(events(&handle).contains(&"2 portals [2, 3]".to_string()));
        }

        #[test]
        fn a_cleared_portal_set_and_a_monitoring_switched_off_are_not_replayed() {
            let (mut capture, handle, _) = rig();
            capture.set_portals(&[portal(1)]).unwrap();
            capture.set_portals(&[]).unwrap();
            capture.set_monitor_local_activity(true).unwrap();
            capture.set_monitor_local_activity(false).unwrap();
            kill(&handle, 0);
            begin(&mut capture).unwrap();
            let after: Vec<String> = events(&handle)
                .into_iter()
                .skip_while(|e| e != "build 1")
                .collect();
            assert_eq!(after, ["build 1", "1 begin 9"]);
        }

        #[test]
        fn a_refused_request_on_a_serving_backend_neither_rebuilds_nor_is_remembered() {
            let (mut capture, handle, _) = rig();
            capture.set_portals(&[portal(1)]).unwrap();
            world(&handle).refuse_portals = true;
            assert!(capture.set_portals(&[portal(2)]).is_err());
            world(&handle).refuse_portals = false;
            assert_eq!(world(&handle).builds, 0);
            kill(&handle, 0);
            begin(&mut capture).unwrap();
            assert!(events(&handle).contains(&"1 portals [1]".to_string()));
            assert!(!events(&handle).iter().any(|e| e.contains("[2]")));
        }

        #[test]
        fn end_after_the_loss_gives_input_back_through_the_replacement() {
            let (mut capture, handle, _) = rig();
            kill(&handle, 0);
            capture
                .end(Some((DisplayId(1), PointDevice::new(5.0, 5.0))))
                .unwrap();
            assert_eq!(
                events(&handle),
                ["0 drop", "build 1", "1 end warp=true"],
                "the warp that the lost backend could not complete runs on the replacement"
            );
        }

        #[test]
        fn rebuilds_are_bounded_per_window_and_the_lost_backend_stays_once_the_budget_is_spent() {
            let (mut capture, handle, offset) = rig();
            for round in 0..MAX_REBUILDS {
                kill(&handle, round);
                begin(&mut capture).unwrap();
                assert_eq!(world(&handle).builds, round + 1);
            }
            // The budget is spent: the lost backend stays, every request fails at once, and
            // nothing is built, dropped or retried in a loop.
            kill(&handle, MAX_REBUILDS);
            let drops = count(&handle, "drop");
            for _ in 0..5 {
                assert!(begin(&mut capture).is_err());
            }
            assert_eq!(world(&handle).builds, MAX_REBUILDS);
            assert_eq!(count(&handle, "drop"), drops);
            assert!(capture.exhausted_logged, "the error is written once");
            // Once the window has slid past the first rebuilds, one more is allowed.
            offset.store(
                u64::try_from(REBUILD_WINDOW.as_millis()).unwrap() + 1,
                Ordering::SeqCst,
            );
            begin(&mut capture).unwrap();
            assert_eq!(world(&handle).builds, MAX_REBUILDS + 1);
            assert!(!capture.exhausted_logged);
        }

        #[test]
        fn the_limiter_allows_three_per_window_and_forgets_old_rebuilds() {
            let mut limiter = RebuildLimiter::default();
            let t0 = Instant::now();
            let at = |s: u64| t0 + Duration::from_secs(s);
            assert!(limiter.try_take(at(0)));
            assert!(limiter.try_take(at(10)));
            assert!(limiter.try_take(at(20)));
            assert!(!limiter.try_take(at(59)));
            assert!(
                limiter.try_take(at(60)),
                "the rebuild at 0 s has left the window"
            );
            assert!(!limiter.try_take(at(61)));
            assert!(
                limiter.try_take(at(70)),
                "the rebuild at 10 s has left the window"
            );
        }

        #[test]
        fn a_failed_build_leaves_no_backend_and_the_next_request_tries_again_within_the_budget() {
            let (mut capture, handle, _) = rig();
            world(&handle).refuse_builds = 1;
            kill(&handle, 0);
            let error = begin(&mut capture).unwrap_err();
            assert!(error.to_string().contains("observation lost"), "{error}");
            assert!(capture.slot.is_none());
            // A request with no backend asks for one again, and that attempt is counted too.
            begin(&mut capture).unwrap();
            assert_eq!(world(&handle).builds, 1);
            assert_eq!(capture.limiter.taken.len(), 2);
        }

        #[test]
        fn a_replacement_that_cannot_subscribe_is_dropped_and_the_next_request_builds_another() {
            let (mut capture, handle, _) = rig();
            capture.subscribe(Arc::new(|_: CaptureEvent| {})).unwrap();
            kill(&handle, 0);
            world(&handle).refuse_subscribe.insert(1);
            assert!(begin(&mut capture).is_err());
            assert!(capture.slot.is_none());
            assert!(events(&handle).contains(&"1 drop".to_string()));
            begin(&mut capture).unwrap();
            assert_eq!(world(&handle).builds, 2);
            assert_eq!(events(&handle).last().unwrap(), "2 begin 9");
        }

        #[test]
        fn subscribing_is_never_retried_on_a_replacement() {
            let (mut capture, handle, _) = rig();
            world(&handle).refuse_subscribe.insert(0);
            kill(&handle, 0);
            assert!(capture.subscribe(Arc::new(|_: CaptureEvent| {})).is_err());
            assert_eq!(world(&handle).builds, 0);
        }
    }
}

/// The node's monotonic clock, the one every backend stamps events with.
pub fn now() -> MonoTime {
    #[cfg(windows)]
    {
        crosspane_platform_windows::clock::now()
    }
    #[cfg(target_os = "macos")]
    {
        crosspane_platform_macos::clock::now()
    }
    #[cfg(target_os = "linux")]
    {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let nanos = u64::try_from(t.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            + u64::try_from(t.tv_nsec).unwrap_or(0);
        MonoTime::from_nanos(nanos)
    }
}

/// The acceptance switch admits only the disposable, unpaired file-keystore fixture.
#[cfg(windows)]
fn acceptance_e1(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<bool> {
    use anyhow::{Context, ensure};
    use std::path::PathBuf;
    let Some(value) = std::env::var_os("CROSSPANE_ACCEPTANCE_E1_ONLY") else {
        return Ok(false);
    };
    ensure!(value == "1", "invalid E1 acceptance switch");
    let appdata = PathBuf::from(std::env::var_os("APPDATA").context("scratch APPDATA")?);
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("scratch LOCALAPPDATA")?);
    let runtime =
        PathBuf::from(std::env::var_os("CROSSPANE_RUNTIME_DIR").context("scratch runtime")?);
    let root = appdata.parent().context("scratch root")?;
    let suffix = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("crosspane-WP-W1.5b-"))
        .context("E1 acceptance requires a uniquely named scratch root")?;
    ensure!(
        suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "E1 acceptance requires a unique scratch identity"
    );
    ensure!(
        root.parent().map(std::fs::canonicalize).transpose()?
            == Some(std::fs::canonicalize(std::env::temp_dir())?),
        "E1 acceptance root must be under the temporary directory"
    );
    ensure!(
        appdata == root.join("roaming")
            && local == root.join("local")
            && runtime == root.join("runtime")
            && state_dir == local.join("Crosspane"),
        "E1 acceptance requires isolated config, state and runtime paths"
    );
    ensure!(
        config.force_file_keystore
            && config.name == format!("wp-w1-5b-{suffix}")
            && config.peers.is_empty()
            && ["CROSSPANE_DISCOVERY", "CROSSPANE_AUDIO", "CROSSPANE_GPU"]
                .into_iter()
                .all(|name| std::env::var(name).as_deref() == Ok("0")),
        "E1 acceptance requires a scratch file identity and disabled discovery/audio/GPU"
    );
    Ok(true)
}

/// One monitor of a fresh Windows placement observation (WP-W1.11 B2). `mapping` is the render
/// host's row; `rc_monitor` and `rc_work` are the same probe's physical rectangles, which only the
/// agent's proxy placement reads (it keeps the proxy inside the work area).
#[cfg(windows)]
#[derive(Clone, Debug, PartialEq)]
pub struct WindowsPlacementRow {
    pub mapping: crosspane_render::proxy::HostMonitorMapping,
    pub rc_monitor: [i32; 4],
    pub rc_work: [i32; 4],
}

/// Each call takes one fresh observation of [`WindowsPlacementRow`]s (see `windows_placement_rows`).
#[cfg(windows)]
pub type WindowsPlacementRows = Arc<dyn Fn() -> Option<Vec<WindowsPlacementRow>> + Send + Sync>;

/// Each call takes one bounded observation on the retained display worker. Called on the
/// host event-loop thread (and the engine thread for explicit placement), without agent locks.
/// Never cache geometry: a dead owner or failed observation yields OS-selected placement.
#[cfg(windows)]
fn windows_placement_rows(
    refresh: crosspane_platform_windows::inject::MonitorRefresh,
) -> WindowsPlacementRows {
    Arc::new(move || {
        let (probes, ids) = refresh().ok()?;
        match windows_monitor_rows(&probes, ids) {
            Ok(rows) => Some(rows),
            Err(_) => {
                tracing::warn!(
                    count = probes.len(),
                    "Windows placement observation refused"
                );
                None
            }
        }
    })
}

/// The render host's view of one `windows_placement_rows` observation: the same monitors, without
/// the physical rectangles the agent uses.
#[cfg(windows)]
fn windows_placement_mapping(
    rows: WindowsPlacementRows,
) -> crosspane_render::proxy::HostPlacementMapping {
    Arc::new(move || {
        let rows = rows()?;
        Some(rows.into_iter().map(|row| row.mapping).collect::<Vec<_>>())
    })
}

#[cfg(windows)]
fn windows_monitor_rows(
    probes: &[crosspane_platform_windows::model::geometry::MonitorProbe],
    mut ids: crosspane_platform_windows::model::geometry::DisplayIds,
) -> Result<Vec<WindowsPlacementRow>, PlatformError> {
    use crosspane_platform_windows::model::geometry;
    use std::collections::BTreeSet;
    let refused = || PlatformError::Backend("invalid Windows placement observation".into());
    let layout = geometry::displays(probes, &mut ids).map_err(|_| refused())?;
    let mut seen_ids = BTreeSet::new();
    let mut seen_native = BTreeSet::new();
    let mut rows = Vec::new();
    for probe in probes.iter().filter(|probe| !probe.twin) {
        let id = ids.assign(&probe.device_path).map_err(|_| refused())?;
        if !seen_ids.insert(id) || probe.name.is_empty() || !seen_native.insert(&probe.name) {
            return Err(refused());
        }
        let display = layout
            .displays
            .iter()
            .find(|display| display.id == id)
            .ok_or_else(refused)?;
        if !display.geometry.is_valid() {
            return Err(refused());
        }
        rows.push(WindowsPlacementRow {
            mapping: crosspane_render::proxy::HostMonitorMapping {
                id: id.0,
                native_id: probe.name.clone(),
                geometry: display.geometry,
                physical_origin: (probe.rc_monitor[0], probe.rc_monitor[1]).into(),
            },
            rc_monitor: probe.rc_monitor,
            rc_work: probe.rc_work,
        });
    }
    if rows.is_empty() || rows.len() != layout.displays.len() {
        return Err(refused());
    }
    Ok(rows)
}

/// Separate, never-default destination fixture. No owner enumeration/capture or native key store.
#[cfg(windows)]
fn acceptance_e2_destination(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<bool> {
    use anyhow::{Context, ensure};
    use std::path::PathBuf;
    let Some(value) = std::env::var_os("CROSSPANE_ACCEPTANCE_E2_DESTINATION") else {
        return Ok(false);
    };
    ensure!(
        value == "1" && std::env::var_os("CROSSPANE_ACCEPTANCE_E1_ONLY").is_none(),
        "invalid or conflicting E2 acceptance switch"
    );
    let appdata = PathBuf::from(std::env::var_os("APPDATA").context("scratch APPDATA")?);
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("scratch LOCALAPPDATA")?);
    let runtime =
        PathBuf::from(std::env::var_os("CROSSPANE_RUNTIME_DIR").context("scratch runtime")?);
    let root = appdata.parent().context("scratch root")?;
    let suffix = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("crosspane-WP-W2.5a-"))
        .context("E2 acceptance requires a uniquely named scratch root")?;
    ensure!(
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "E2 acceptance requires a unique scratch identity"
    );
    ensure!(
        root.parent().map(std::fs::canonicalize).transpose()?
            == Some(std::fs::canonicalize(std::env::temp_dir())?),
        "E2 acceptance root must be under the temporary directory"
    );
    ensure!(
        appdata == root.join("roaming")
            && local == root.join("local")
            && runtime == root.join("runtime")
            && state_dir == local.join("Crosspane"),
        "E2 acceptance requires isolated config, state and runtime paths"
    );
    ensure!(
        config.force_file_keystore
            && config.name == format!("wp-w2-5a-dst-{suffix}")
            && config.peers.len() == 1
            && config.peers[0].addr.ip().is_loopback()
            && config.peers[0].addr.port() != 0
            && !config.crossing
            && !config.drag.across
            && ["CROSSPANE_DISCOVERY", "CROSSPANE_AUDIO", "CROSSPANE_GPU"]
                .into_iter()
                .all(|name| std::env::var(name).as_deref() == Ok("0")),
        "E2 acceptance requires an explicit loopback fixture peer, file identity and confined input"
    );
    Ok(true)
}

/// Never-default source acceptance: every observed/captured window belongs to one pinned fixture.
#[cfg(windows)]
fn acceptance_e2_source(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<Option<Vec<crosspane_platform_windows::window::OwnedProcessClaim>>> {
    use anyhow::{Context, ensure};
    use std::{
        net::{IpAddr, Ipv4Addr},
        path::PathBuf,
    };
    let Some(value) = std::env::var_os("CROSSPANE_ACCEPTANCE_E2_SOURCE") else {
        return Ok(None);
    };
    ensure!(
        value == "1"
            && std::env::var_os("CROSSPANE_ACCEPTANCE_E1_ONLY").is_none()
            && std::env::var_os("CROSSPANE_ACCEPTANCE_E2_DESTINATION").is_none(),
        "invalid or conflicting source acceptance switch"
    );
    let appdata = PathBuf::from(std::env::var_os("APPDATA").context("source scratch APPDATA")?);
    let local =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("source scratch LOCALAPPDATA")?);
    let runtime =
        PathBuf::from(std::env::var_os("CROSSPANE_RUNTIME_DIR").context("source scratch runtime")?);
    let root = appdata.parent().context("source scratch root")?;
    let suffix = root
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("crosspane-WP-W2.5b-"))
        .context("source acceptance requires a unique scratch root")?;
    ensure!(
        suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "source acceptance requires a unique scratch identity"
    );
    ensure!(
        root.parent().map(std::fs::canonicalize).transpose()?
            == Some(std::fs::canonicalize(std::env::temp_dir())?),
        "source acceptance root must be under the temporary directory"
    );
    ensure!(
        appdata == root.join("roaming")
            && local == root.join("local")
            && runtime == root.join("runtime")
            && state_dir == local.join("Crosspane"),
        "source acceptance requires isolated config, state and runtime paths"
    );
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    ensure!(
        config.force_file_keystore
            && config.name == format!("wp-w2-5b-src-{suffix}")
            && config.port == 0
            && config.acceptance_bind_ip == Some(loopback)
            && config.peers.len() == 1
            && config.peers[0].addr.ip() == loopback
            && config.peers[0].addr.port() != 0
            && !config.crossing
            && !config.drag.across
            && ["CROSSPANE_DISCOVERY", "CROSSPANE_AUDIO", "CROSSPANE_GPU"]
                .into_iter()
                .all(|name| std::env::var(name).as_deref() == Ok("0")),
        "source acceptance requires a pinned IPv4 fixture, private identity and disabled unused sharing"
    );
    let document =
        std::env::var("CROSSPANE_E2_SOURCE_CLAIM").context("source fixture claim missing")?;
    Ok(Some(vec![owned_source_claim(
        &document,
        &root.join("fixture/owned-window.exe"),
    )?]))
}

#[cfg(windows)]
fn owned_source_claim(
    document: &str,
    executable: &std::path::Path,
) -> anyhow::Result<crosspane_platform_windows::window::OwnedProcessClaim> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Claim {
        pid: u32,
        process_created: u64,
        executable: std::path::PathBuf,
    }
    anyhow::ensure!(document.len() <= 8192, "owned source claim byte bound");
    let claim: Claim = serde_json::from_str(document)
        .map_err(|_| anyhow::anyhow!("invalid owned source claim"))?;
    anyhow::ensure!(
        claim.pid != 0
            && claim.process_created != 0
            && claim.executable.is_absolute()
            && claim.executable == executable,
        "source claim must name the exact scratch fixture"
    );
    Ok(crosspane_platform_windows::window::OwnedProcessClaim {
        pid: claim.pid,
        process_created: claim.process_created,
        executable: claim.executable,
    })
}

/// Required release/panic observer, only after the host has relinquished Raw Input.
/// Configuration and the initial-state subscription must both succeed before startup commits.
/// The health view is taken before the backend is boxed, so the agent can watch for loss.
#[cfg(windows)]
pub fn windows_hotkeys_after_host(
    sink: Arc<dyn crosspane_platform::EventSink<crosspane_platform::HotkeyEvent>>,
) -> Result<
    (
        Box<dyn GlobalHotkeys>,
        crosspane_platform_windows::hotkey::HotkeyHealth,
    ),
    PlatformError,
> {
    let mut health = None;
    let backend = windows_hotkeys_after_host_with(
        || {
            let backend = crosspane_platform_windows::hotkey::WindowsHotkeys::new()?;
            health = Some(backend.health());
            Ok(Box::new(backend) as Box<dyn GlobalHotkeys>)
        },
        sink,
    )?;
    let health =
        health.ok_or_else(|| PlatformError::Backend("Windows hotkey health missing".into()))?;
    Ok((backend, health))
}

#[cfg(windows)]
fn windows_hotkeys_after_host_with(
    factory: impl FnOnce() -> Result<Box<dyn GlobalHotkeys>, PlatformError>,
    sink: Arc<dyn crosspane_platform::EventSink<crosspane_platform::HotkeyEvent>>,
) -> Result<Box<dyn GlobalHotkeys>, PlatformError> {
    let mut backend = factory()?;
    // There is no configurable release chord: preserve the engine's exact default.
    let chord =
        crosspane_engine::EngineConfig::new(crosspane_types::id::NodeId([0; 32])).release_chord;
    backend.set_chord(&chord)?;
    backend.subscribe(sink)?;
    Ok(backend)
}

/// The native recovery owner is constructed before winit or any source/capture/hook.
#[cfg(windows)]
pub(crate) struct WindowsRecoveredMirror {
    pub(crate) parking: crosspane_platform_windows::parking::WindowsMirrorParking,
    /// M2 twin parking (WP-W3.2). `None` in E1 and destination scratch, which skip recovery.
    pub(crate) twin: Option<crosspane_platform_windows::twin_parking::WindowsTwinParking>,
    pub(crate) startup: StartupRecovery,
    pub(crate) pending: usize,
}
#[cfg(windows)]
struct WindowsMirrorStore {
    directory: std::path::PathBuf,
    empty_required: bool,
    /// The two journal file names. M1 and M2 use the same format under different names.
    committed: &'static str,
    pending: &'static str,
}
#[cfg(windows)]
impl WindowsMirrorStore {
    fn read_image(&self, name: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        use std::io::Read;
        let path = self.directory.join(name);
        // Reuse exactly the existing private admission/pinning implementation. The admitted
        // leaf's no-delete pin and ancestor pins remain live across the bounded std file read.
        let _parents = crate::windows::security::pin_parent(&path)
            .map_err(|_| PlatformError::Backend("mirror private parent admission".into()))?;
        let Some(_leaf) = crate::windows::security::private_file(&path)
            .map_err(|_| PlatformError::Backend("mirror private leaf admission".into()))?
        else {
            return Ok(None);
        };
        let file = std::fs::File::open(&path)
            .map_err(|_| PlatformError::Backend("mirror journal read".into()))?;
        let mut bytes = Vec::new();
        file.take(crosspane_platform_windows::model::parking::MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| PlatformError::Backend("mirror journal read".into()))?;
        if bytes.len() > crosspane_platform_windows::model::parking::MAX_BYTES {
            return Err(PlatformError::Backend("mirror journal byte bound".into()));
        }
        Ok(Some(bytes))
    }
}
#[cfg(windows)]
impl crosspane_platform_windows::parking::MirrorJournalStore for WindowsMirrorStore {
    fn read(
        &mut self,
    ) -> Result<crosspane_platform_windows::parking::MirrorJournalImages, PlatformError> {
        use crosspane_platform_windows::model::parking::Journal;
        let images = crosspane_platform_windows::parking::MirrorJournalImages {
            committed: self.read_image(self.committed)?,
            pending: self.read_image(self.pending)?,
        };
        // E1/destination scratch is forbidden from inspecting a persisted owner's source HWND.
        // Fully validate the same journal but refuse outstanding entries before native work.
        if self.empty_required && !Journal::load(&images)?.0.entries().is_empty() {
            return Err(PlatformError::Backend(
                "scratch startup refused: outstanding mirror recovery".into(),
            ));
        }
        Ok(images)
    }
    fn commit(&mut self, document: &[u8]) -> Result<(), PlatformError> {
        use crosspane_platform_windows::model::parking::MAX_BYTES;
        if document.len() > MAX_BYTES {
            return Err(PlatformError::Backend("mirror journal byte bound".into()));
        }
        // write_private writes/flushed its private same-directory temporary before publishing.
        // A failure in either publish faults the parking stream; no native mutation follows.
        for name in [self.pending, self.committed] {
            crate::paths::write_private(&self.directory.join(name), document).map_err(|_| {
                PlatformError::Backend("mirror journal publication failed; retained".into())
            })?;
        }
        Ok(())
    }
}
#[cfg(windows)]
pub(crate) fn recover_windows_mirror_before_host(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<WindowsRecoveredMirror> {
    use anyhow::Context;
    use crosspane_platform_windows::{
        model::{
            journal::JOURNAL_NAME,
            parking::{COMMITTED_NAME, PENDING_NAME},
        },
        twin_parking::{WINDOW_COMMITTED_NAME, WINDOW_PENDING_NAME, WindowsTwinParking},
    };
    // Scratch validation precedes even host construction and any native source field queries.
    let e1 = acceptance_e1(state_dir, config)?;
    let destination = acceptance_e2_destination(state_dir, config)?;
    let source = acceptance_e2_source(state_dir, config)?.is_some();
    let scratch = e1 || destination || source;
    let store = WindowsMirrorStore {
        directory: state_dir.to_owned(),
        empty_required: scratch,
        committed: COMMITTED_NAME,
        pending: PENDING_NAME,
    };
    let mut parking =
        crosspane_platform_windows::parking::WindowsMirrorParking::new(Box::new(store))
            .context("Windows mirror journal (required)")?;
    let (startup, pending, twin) = if e1 || destination {
        (StartupRecovery::None, 0, None)
    } else {
        let recovery = parking
            .recover_startup()
            .context("Windows pre-host mirror recovery (required)")?;
        tracing::info!(
            restored = recovery.restored,
            retired = recovery.retired,
            pending = recovery.pending,
            "Windows mirror recovery"
        );
        // M2 keeps the window originals in the same format under its own names. Scratch modes
        // never reach the driver (`twins` is off), but still refuse outstanding entries.
        let twin_store = WindowsMirrorStore {
            directory: state_dir.to_owned(),
            empty_required: scratch,
            committed: WINDOW_COMMITTED_NAME,
            pending: WINDOW_PENDING_NAME,
        };
        let mut twin =
            WindowsTwinParking::new(Box::new(twin_store), state_dir.join(JOURNAL_NAME), !scratch)
                .context("Windows twin parking journal (required)")?;
        let twin_recovery = twin
            .recover_startup()
            .context("Windows pre-host twin recovery (required)")?;
        tracing::info!(
            restored = twin_recovery.windows.restored,
            retired = twin_recovery.windows.retired,
            pending = twin_recovery.windows.pending,
            twins_journaled = twin_recovery.twins_journaled,
            driver_present = twin_recovery.driver_present,
            "Windows twin recovery"
        );
        if let Some(reason) = twin_recovery.unavailable {
            tracing::warn!(
                reason,
                "twin parking unavailable: projected windows stay in place (mirror mode, M1)"
            );
        }
        let pending = recovery.pending + twin_recovery.windows.pending;
        let startup = if pending > 0 {
            StartupRecovery::Failed
        } else {
            StartupRecovery::combine(&[Ok(recovery.restored), Ok(twin_recovery.windows.restored)])
        };
        (startup, pending, Some(twin))
    };
    Ok(WindowsRecoveredMirror {
        parking,
        twin,
        startup,
        pending,
    })
}

/// Backends the Windows agent shares between threads (WP-W4.1d AGENT-WIN).
///
/// The window source is used by the agent loop (catalog reads), by the parking worker (focus
/// requests, which can wait for the OS) and by the capture's drag observer (which needs the very
/// same source to resolve window identities). The pointer injector is used by the loop and, for
/// each capture build, to mint the drag observer's settlement capability. Each is shared behind one
/// mutex, and these adapters are what the agent holds as `Box<dyn ...>`.
#[cfg(any(windows, test))]
pub(crate) mod shared {
    use std::sync::{Arc, Mutex, PoisonError, TryLockError};
    use std::time::{Duration, Instant};

    use crosspane_platform::{
        EventSink, PlatformError, PointerInjector, WindowEvent, WindowInfo, WindowSource,
    };
    use crosspane_types::geom::PointDevice;
    use crosspane_types::hid::MouseButton;
    use crosspane_types::id::{DisplayId, WindowId};
    use crosspane_types::input::ScrollDelta;

    /// How long a catalog read waits for the source while a focus request holds it. The agent
    /// loop must not wait longer: it reports the source as busy and its caller degrades.
    const READ_WAIT: Duration = Duration::from_millis(25);

    fn poisoned(what: &'static str) -> PlatformError {
        PlatformError::Backend(what.into())
    }

    /// The agent loop's handle on a window source that other threads also hold.
    ///
    /// A panic in one caller (a focus request caught by the parking worker) poisons the mutex. Every
    /// lock recovers the guard instead: the source keeps answering, and a single caught panic does
    /// not turn every later read into a backend error.
    pub(crate) struct SharedSource<S>(pub(crate) Arc<Mutex<S>>);

    impl<S> SharedSource<S> {
        fn read<T>(
            &self,
            read: impl FnOnce(&S) -> Result<T, PlatformError>,
        ) -> Result<T, PlatformError> {
            let deadline = Instant::now() + READ_WAIT;
            loop {
                match self.0.try_lock() {
                    Ok(source) => return read(&source),
                    Err(TryLockError::Poisoned(error)) => return read(&error.into_inner()),
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(TryLockError::WouldBlock) => return Err(PlatformError::Timeout),
                }
            }
        }
    }

    impl<S: WindowSource> WindowSource for SharedSource<S> {
        fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
            self.read(|source| source.windows())
        }
        fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
            self.read(|source| source.focused())
        }
        fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .activate(window)
        }
        fn subscribe(
            &mut self,
            sink: Arc<dyn EventSink<WindowEvent>>,
        ) -> Result<(), PlatformError> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .subscribe(sink)
        }
    }

    /// A focus request for the parking worker's thread: it holds the source for as long as the
    /// source's own call takes. A poisoned source is recovered, as in `SharedSource`.
    pub(crate) fn activator<S: WindowSource + 'static>(
        source: Arc<Mutex<S>>,
    ) -> crate::parking_worker::Activator {
        Box::new(move |window| {
            source
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .activate(window)
        })
    }

    /// The pointer injector as the agent loop holds it, while the capture factory can still ask it
    /// for a new drag settlement.
    pub(crate) struct SharedPointer<P>(pub(crate) Arc<Mutex<P>>);

    impl<P: PointerInjector> SharedPointer<P> {
        fn with<T>(
            &self,
            call: impl FnOnce(&mut P) -> Result<T, PlatformError>,
        ) -> Result<T, PlatformError> {
            call(
                &mut *self
                    .0
                    .lock()
                    .map_err(|_| poisoned("pointer injector state"))?,
            )
        }
    }

    impl<P: PointerInjector> PointerInjector for SharedPointer<P> {
        fn move_to(
            &mut self,
            display: DisplayId,
            position: PointDevice,
        ) -> Result<(), PlatformError> {
            self.with(|p| p.move_to(display, position))
        }
        fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
            self.with(|p| p.button(button, down))
        }
        fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
            self.with(|p| p.scroll(delta))
        }
        fn release_all(&mut self) -> Result<(), PlatformError> {
            self.with(|p| p.release_all())
        }
        fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
            self.with(|p| p.recover_buttons(buttons))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;

        #[derive(Default)]
        struct Source {
            activated: Vec<WindowId>,
        }
        impl WindowSource for Source {
            fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
                Ok(Vec::new())
            }
            fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
                Ok(Some(WindowId(9)))
            }
            fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
                // WindowId(13) stands in for an OS call that panics while the source is held.
                assert_ne!(window, WindowId(13), "fixture: activation panics");
                self.activated.push(window);
                Ok(())
            }
            fn subscribe(
                &mut self,
                _: Arc<dyn EventSink<WindowEvent>>,
            ) -> Result<(), PlatformError> {
                Ok(())
            }
        }

        #[test]
        fn a_read_waits_a_bounded_time_for_a_focus_request_and_then_reports_busy() {
            let source = Arc::new(Mutex::new(Source::default()));
            let shared = SharedSource(source.clone());
            assert_eq!(shared.focused().unwrap(), Some(WindowId(9)));
            // A holder stands in for a focus request that waits for the OS.
            let (held, holding) = mpsc::channel();
            let (release, released) = mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                let _guard = source.lock().unwrap();
                held.send(()).unwrap();
                released.recv().unwrap();
            });
            holding.recv_timeout(Duration::from_secs(1)).unwrap();
            let started = Instant::now();
            assert!(matches!(shared.windows(), Err(PlatformError::Timeout)));
            assert!(matches!(shared.focused(), Err(PlatformError::Timeout)));
            assert!(started.elapsed() < Duration::from_secs(1));
            release.send(()).unwrap();
            holder.join().unwrap();
            assert!(shared.windows().unwrap().is_empty());
        }

        #[test]
        fn the_activator_and_the_loops_handle_reach_the_same_source() {
            let source = Arc::new(Mutex::new(Source::default()));
            let mut activate = activator(source.clone());
            let mut shared = SharedSource(source.clone());
            activate(WindowId(3)).unwrap();
            shared.activate(WindowId(4)).unwrap();
            assert_eq!(source.lock().unwrap().activated, [WindowId(3), WindowId(4)]);
            let poisoner = source.clone();
            let _ = std::thread::spawn(move || {
                let _guard = poisoner.lock().unwrap();
                panic!("fixture: poison the source");
            })
            .join();
            // The poisoned source still answers its reads and still takes focus requests.
            assert!(shared.windows().unwrap().is_empty());
            assert_eq!(shared.focused().unwrap(), Some(WindowId(9)));
            activate(WindowId(5)).unwrap();
            shared.activate(WindowId(6)).unwrap();
            assert_eq!(
                source
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .activated,
                [WindowId(3), WindowId(4), WindowId(5), WindowId(6)]
            );
        }

        #[test]
        fn a_caught_activation_panic_leaves_the_window_source_usable() {
            let source = Arc::new(Mutex::new(Source::default()));
            let mut activate = activator(source.clone());
            let shared = SharedSource(source.clone());
            // The parking worker catches the panic and keeps running.
            let caught =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| activate(WindowId(13))));
            assert!(caught.is_err());
            // The next read succeeds, with no busy or backend error, and so does the next focus.
            assert_eq!(shared.focused().unwrap(), Some(WindowId(9)));
            assert!(shared.windows().unwrap().is_empty());
            activate(WindowId(3)).unwrap();
            assert_eq!(
                source
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .activated,
                [WindowId(3)]
            );
        }

        #[derive(Default)]
        struct Pointer {
            calls: AtomicU32,
        }
        impl PointerInjector for Pointer {
            fn move_to(&mut self, _: DisplayId, _: PointDevice) -> Result<(), PlatformError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn button(&mut self, _: MouseButton, _: bool) -> Result<(), PlatformError> {
                self.calls.fetch_add(10, Ordering::SeqCst);
                Ok(())
            }
            fn scroll(&mut self, _: ScrollDelta) -> Result<(), PlatformError> {
                Ok(())
            }
            fn release_all(&mut self) -> Result<(), PlatformError> {
                self.calls.fetch_add(100, Ordering::SeqCst);
                Ok(())
            }
            fn recover_buttons(&mut self, _: &[MouseButton]) -> Result<(), PlatformError> {
                Ok(())
            }
        }

        #[test]
        fn the_shared_pointer_forwards_to_the_injector_the_factory_also_holds() {
            let inner = Arc::new(Mutex::new(Pointer::default()));
            let mut shared = SharedPointer(inner.clone());
            shared
                .move_to(DisplayId(1), PointDevice::new(1.0, 2.0))
                .unwrap();
            shared.button(MouseButton::PRIMARY, true).unwrap();
            shared.release_all().unwrap();
            assert_eq!(inner.lock().unwrap().calls.load(Ordering::SeqCst), 111);
        }
    }
}

/// The two backends the capture's drag observer needs besides the gate: the window source it
/// resolves identities through, and the pointer injector it mints a settlement capability from.
/// A capture owns its observer's configuration, so each build (the first and every rebuild)
/// makes a new one from these.
#[cfg(windows)]
#[derive(Clone)]
struct DragWiring {
    source: Arc<std::sync::Mutex<crosspane_platform_windows::window::WindowsWindowSource>>,
    pointer: Arc<std::sync::Mutex<crosspane_platform_windows::inject::WindowsPointerInjector>>,
}

#[cfg(windows)]
impl DragWiring {
    fn config(&self) -> Result<crosspane_platform_windows::drag::DragConfig, PlatformError> {
        // The configuration reads the source without waiting. A focus request holding it makes
        // that fail for a moment, so a few short retries, not an immediate loss of drag. Each try
        // mints its own settlement capability: `DragConfig::new` consumes it either way.
        let mut last = PlatformError::Timeout;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let settlement = self
                .pointer
                .lock()
                .map_err(|_| PlatformError::Backend("pointer injector state".into()))?
                .drag_settlement();
            match crosspane_platform_windows::drag::DragConfig::new(self.source.clone(), settlement)
            {
                Ok(config) => return Ok(config),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

/// How long a rebuild waits for a lost capture backend's native owner to let go of the
/// process-wide owner slot. A dead owner removes its hooks and ends its helper threads within tens
/// of milliseconds; one still containing the tail of a physical drag does not, and that rebuild
/// then fails and counts against the budget.
#[cfg(windows)]
const CAPTURE_OWNER_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// One Windows capture backend with its health handle. Both the first build (the startup
/// observation) and each rebuild (a fresh one) go through here, with the same gate and the same
/// monitor refresh, which `set_portals` re-reads the monitor table through (WP-W1.11 G2).
#[cfg(windows)]
fn build_windows_capture(
    gate: &Arc<IoGate>,
    probes: &[crosspane_platform_windows::model::geometry::MonitorProbe],
    ids: &mut crosspane_platform_windows::model::geometry::DisplayIds,
    refresh: &crosspane_platform_windows::inject::MonitorRefresh,
    drag: Option<&DragWiring>,
) -> Result<rebuilding_capture::Built, PlatformError> {
    use crosspane_platform_windows::capture::WindowsCapture;
    // With `drag`, the capture also detects a window being dragged by its title bar against a
    // portal (`InputCapture::begin_drag`). Detection is an addition: if the capture cannot be
    // built with it, it is built without, and E1 keeps working.
    let capture = match drag.map(DragWiring::config) {
        Some(Ok(config)) => {
            match WindowsCapture::new_with_drag(gate.clone(), probes, ids, config) {
                Ok(capture) => capture,
                Err(error) => {
                    tracing::warn!(%error, "drag detection unavailable: capture without it");
                    // A capture that failed to start is gone once its owner lets go.
                    if !crosspane_platform_windows::capture::wait_owner_released(
                        CAPTURE_OWNER_RELEASE_WAIT,
                    ) {
                        return Err(error);
                    }
                    WindowsCapture::new(gate.clone(), probes, ids)?
                }
            }
        }
        Some(Err(error)) => {
            tracing::warn!(%error, "drag detection unavailable: capture without it");
            WindowsCapture::new(gate.clone(), probes, ids)?
        }
        None => WindowsCapture::new(gate.clone(), probes, ids)?,
    }
    .with_monitor_refresh(refresh.clone());
    let health = capture.health();
    Ok(rebuilding_capture::Built {
        backend: Box::new(capture),
        lost: Box::new(move || health.lost()),
    })
}

#[cfg(windows)]
pub fn create(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
    mut recovered: WindowsRecoveredMirror,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_windows::{
        displays::WindowsDisplays, frame_capture::WindowsFrameCapture, inject,
        link::WindowsLinkInfo, overlay::WindowsOverlay, permissions::WindowsPermissions,
        session::WindowsSession, tray::WindowsTray, window::WindowsWindowSource,
    };
    let e1_only = acceptance_e1(state_dir, config)?;
    let e2_destination = acceptance_e2_destination(state_dir, config)?;
    let source_claims = acceptance_e2_source(state_dir, config)?;
    let source_only = source_claims.is_some();
    let scratch_only = e1_only || e2_destination || source_only;
    let gate = IoGate::new();
    let session = WindowsSession::new(gate.clone())
        .context("Windows session state (required: Crosspane fails closed without it)")?;
    let displays = WindowsDisplays::new().context("Windows displays")?;
    let snapshot = displays.snapshot().context("Windows display snapshot")?;
    // One refresh source feeds both the host's mapping and the agent's rows (WP-W1.11 B2).
    let placement_rows = windows_placement_rows(displays.monitor_refresh());
    let host_placement_mapping = Some(windows_placement_mapping(placement_rows.clone()));
    let mut ids = snapshot.ids;
    // The injectors and the window source come before the capture: the capture's drag observer is
    // built from both (`DragWiring`), and so is every rebuilt capture.
    let injectors = if source_only {
        None
    } else {
        optional(
            "injection",
            inject::injectors(
                gate.clone(),
                &snapshot.probes,
                &mut ids,
                displays.monitor_refresh(),
            ),
        )
    };
    let (keys, pointer) = match injectors {
        Some((keys, pointer)) => (
            Some(Box::new(keys) as Box<dyn KeyInjector>),
            Some(Arc::new(std::sync::Mutex::new(pointer))),
        ),
        None => (None, None),
    };
    let overlay = optional("overlay", WindowsOverlay::new(&snapshot.probes, &mut ids))
        .map(|backend| Box::new(backend) as Box<dyn OverlayHost>);
    // Source acceptance admits and retains native process identity BEFORE the first fields query.
    // E1/destination omit source observation; production retains unrestricted normal behavior.
    let windows = if let Some(claims) = source_claims {
        let allowlist = crosspane_platform_windows::window::OwnedProcessAllowlist::admit(claims)
            .context("owned Windows source process admission")?;
        Some(
            WindowsWindowSource::new_restricted(
                displays.ids(),
                displays.monitor_reader(),
                allowlist,
            )
            .context("restricted Windows source (required)")?,
        )
    } else if scratch_only {
        None
    } else {
        Some(
            WindowsWindowSource::new(displays.ids(), displays.monitor_reader())
                .context("Windows window source (required for mirror recovery binding)")?,
        )
    };
    // The resolvers read the source's identity table without the source; take them before the
    // source is shared behind its mutex.
    let resolver = windows.as_ref().map(WindowsWindowSource::resolver);
    let windows = windows.map(|source| Arc::new(std::sync::Mutex::new(source)));
    // Detecting a window dragged across a portal needs the real source and the real injector, and
    // is for the normal agent only (the isolated acceptance modes have neither).
    let drag = windows
        .as_ref()
        .zip(pointer.as_ref())
        .filter(|_| !scratch_only)
        .map(|(source, pointer)| DragWiring {
            source: source.clone(),
            pointer: pointer.clone(),
        });
    let capture = if source_only {
        None
    } else {
        let refresh = displays.monitor_refresh();
        optional(
            "capture",
            build_windows_capture(&gate, &snapshot.probes, &mut ids, &refresh, drag.as_ref()),
        )
        .map(|first| {
            // A backend that reports itself permanently lost is dropped and built again through
            // the same factory, from a fresh observation of the monitors and once its native
            // owner has let go (WP-W4.1d CAP-REBUILD; bounded in `rebuilding_capture`). The
            // replacement gets the same drag wiring as the first backend.
            let (gate, refresh) = (gate.clone(), refresh.clone());
            let build: rebuilding_capture::Build = Box::new(move || {
                if !crosspane_platform_windows::capture::wait_owner_released(
                    CAPTURE_OWNER_RELEASE_WAIT,
                ) {
                    return Err(PlatformError::Backend(
                        "the lost capture owner has not released yet".into(),
                    ));
                }
                let (probes, mut ids) = refresh()?;
                build_windows_capture(&gate, &probes, &mut ids, &refresh, drag.as_ref())
            });
            Box::new(rebuilding_capture::RebuildingCapture::new(first, build))
                as Box<dyn InputCapture>
        })
    };
    let parking = if let Some(resolver) = resolver.as_ref() {
        use crosspane_platform_windows::{drag::WindowsRestorePlacer, model::drag::PlacedRestore};
        recovered
            .parking
            .bind_source(resolver.clone(), displays.ids(), displays.monitor_reader())
            .context("Windows mirror source binding (required)")?;
        // DRAG-v0 D-6 placed return decorates the final stack. The decorator needs the concrete
        // stack, so each arm wraps and then boxes its own; a boxed mirror cannot be wrapped.
        let placer =
            WindowsRestorePlacer::new(resolver.clone(), displays.ids(), displays.monitor_reader());
        let report: Box<dyn Fn(&PlatformError) + Send> = Box::new(|error: &PlatformError| {
            tracing::warn!(%error, "restored window placement refused; restore remains successful");
        });
        match recovered.twin.take() {
            // Scratch modes keep M1 alone.
            Some(mut twin) if !scratch_only => {
                twin.bind_source(resolver.clone(), displays.ids(), displays.monitor_reader())
                    .context("Windows twin parking source binding (required)")?;
                let stack =
                    crate::twin::TwinOrMirror::new(Box::new(twin), Box::new(recovered.parking));
                let placed = PlacedRestore::new(stack, placer, report);
                Some(Box::new(placed) as Box<dyn WindowParking>)
            }
            _ => {
                let placed = PlacedRestore::new(recovered.parking, placer, report);
                Some(Box::new(placed) as Box<dyn WindowParking>)
            }
        }
    } else {
        None
    };
    let frames = resolver.as_ref().and_then(|resolver| {
        optional(
            "frame capture",
            if source_only {
                // The owned source permits per-window WGC only, never the VM's monitor.
                WindowsFrameCapture::new(gate.clone(), resolver.clone())
            } else {
                WindowsFrameCapture::new_with_monitor_reader(
                    gate.clone(),
                    resolver.clone(),
                    displays.monitor_snapshot_reader(),
                )
            },
        )
    });
    let gpu = if std::env::var("CROSSPANE_GPU").as_deref() != Ok("0") {
        frames.as_ref().and_then(|frames| {
            // The GPU is created once per process. A busy machine at logon can exceed its bounded
            // start, so a timeout is retried twice before the CPU frame path is chosen for good.
            let mut attempt = 1;
            loop {
                match frames.enable_gpu(wgpu::Features::empty()) {
                    Ok(windows) => {
                        break Some(GpuDevice {
                            device: windows.device.clone(),
                            queue: windows.queue.clone(),
                            windows,
                        });
                    }
                    Err(PlatformError::Timeout) if attempt < 3 => {
                        tracing::info!(attempt, "Windows source GPU start timed out: retrying");
                        attempt += 1;
                    }
                    Err(error) => {
                        tracing::info!(%error, "Windows source GPU unavailable: CPU frame path");
                        break None;
                    }
                }
            }
        })
    } else {
        None
    };
    // The OS can deny a foreground request and the source then waits for it for up to two
    // seconds: that wait belongs on the parking worker, not on the agent loop.
    let window_activator = windows
        .as_ref()
        .map(|source| shared::activator(source.clone()));
    Ok(Platform {
        gate,
        session: Box::new(session),
        // Reader/refresh closures hold Weak references. This Box retains the native owner
        // and authoritative allocator for the full lifetime of all geometry consumers.
        displays: Box::new(displays),
        capture,
        keys,
        pointer: pointer
            .map(|backend| Box::new(shared::SharedPointer(backend)) as Box<dyn PointerInjector>),
        overlay,
        // Startup constructs and subscribes this required observer after the host has
        // relinquished winit's keyboard/mouse registrations. A failure aborts startup.
        hotkeys: None,
        keystore: if scratch_only { None } else { keystore() },
        // Nothing to grant on Windows (v0).
        // required=[] is benign in the agent: no OS request/subscription, notice or retry.
        permissions: Box::new(WindowsPermissions),
        windows: windows
            .map(|source| Box::new(shared::SharedSource(source)) as Box<dyn WindowSource>),
        window_activator,
        parking,
        frames: frames.map(|backend| Box::new(backend) as Box<dyn FrameCapture>),
        tray: optional("tray", WindowsTray::new())
            .map(|backend| Box::new(backend) as Box<dyn TrayHost>),
        links: optional("links", WindowsLinkInfo::new())
            .map(|backend| Box::new(backend) as Box<dyn crosspane_platform::LinkInfo>),
        gpu,
        home: None,
        proxy_placement: None,
        startup_recovery: recovered.startup,
        acceptance_scratch: scratch_only,
        acceptance_source: source_only,
        host_placement_mapping,
        windows_placement_rows: Some(placement_rows),
    })
}

/// Call `lost` (once, from another thread) when the Hyprland instance this agent belongs to dies.
///
/// Hyprland restarts in place after a crash (Omarchy's `start-hyprland`) without ending the
/// graphical session, so the service manager doesn't stop the agent with it: on 2026-10-01 an agent
/// sat on a dead instance for hours, unable to capture, inject or park. The instance's
/// `hyprland.lock` names its process; nothing is sent over the compositor's IPC to check it.
#[cfg(target_os = "linux")]
pub fn watch_compositor(lost: impl FnOnce() + Send + 'static) {
    let (Some(runtime), Ok(signature)) = (
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE"),
    ) else {
        return;
    };
    let lock = std::path::Path::new(&runtime)
        .join("hypr")
        .join(signature)
        .join("hyprland.lock");
    let Some(pid) = std::fs::read_to_string(&lock)
        .ok()
        .and_then(|text| text.lines().next()?.trim().parse::<u32>().ok())
    else {
        tracing::warn!(path = %lock.display(), "no Hyprland instance lock: not watching the compositor");
        return;
    };
    let alive = move || {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "Hyprland")
    };
    let spawned = std::thread::Builder::new()
        .name("compositor-watch".into())
        .spawn(move || {
            while alive() {
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            lost();
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not watch the compositor");
    }
}

/// End this process if it is still running after `after`. A clean exit can hang once the display
/// server is gone (Xlib's I/O-error handler calls `exit` too, and two racing `exit`s deadlock).
pub fn exit_deadline(after: std::time::Duration) {
    let _ = std::thread::Builder::new()
        .name("exit-deadline".into())
        .spawn(move || {
            std::thread::sleep(after);
            #[cfg(windows)]
            crate::windows::process::exit_without_handlers(1);
            #[cfg(target_os = "macos")]
            tracing::error!("the agent did not stop in time; killing it");
            #[cfg(target_os = "linux")]
            {
                exit_diagnostic(
                    "exit-deadline",
                    "the agent did not stop in time; killing it",
                );
                let _ = rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::KILL,
                );
            }
            #[cfg(unix)]
            std::process::abort();
        });
}

#[cfg(target_os = "linux")]
static EXIT_STDERR_SAFE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Classify once before GPU startup: filesystem metadata revalidation can block even in fstat.
#[cfg(target_os = "linux")]
pub fn prepare_exit_diagnostics() {
    use rustix::fs::{FileType, fstat};
    EXIT_STDERR_SAFE.get_or_init(|| {
        fstat(std::io::stderr()).is_ok_and(|stat| {
            matches!(
                FileType::from_raw_mode(stat.st_mode),
                FileType::Fifo | FileType::Socket
            )
        })
    });
}

/// Best-effort pipe/socket writes bypass stdio locks; unprepared/unsupported sinks are omitted.
#[cfg(target_os = "linux")]
pub fn exit_diagnostic(owner: &str, reason: &str) {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    if EXIT_STDERR_SAFE.get() != Some(&true) {
        return;
    }
    let stderr = std::io::stderr();
    let Ok(flags) = fcntl_getfl(&stderr) else {
        return;
    };
    if fcntl_setfl(&stderr, flags | OFlags::NONBLOCK).is_err() {
        return;
    }
    // Called only immediately before termination: no buffering, allocation, retry or flag restore.
    let _ = rustix::io::writev(
        &stderr,
        &[
            std::io::IoSlice::new(owner.as_bytes()),
            std::io::IoSlice::new(b": "),
            std::io::IoSlice::new(reason.as_bytes()),
            std::io::IoSlice::new(b"\n"),
        ],
    );
}

/// A GPU destructor can still be running on timeout, panic or second signal. Skip libc handlers.
#[cfg(target_os = "linux")]
pub fn exit_without_handlers(status: i32) -> ! {
    // SAFETY: the declaration matches Linux libc's non-returning C ABI for _exit.
    unsafe extern "C" {
        fn _exit(status: std::ffi::c_int) -> !;
    }
    // SAFETY: _exit takes only the exit status, cannot return, and runs no driver/TLS handlers.
    // These are lead-approved forced exits; ordinary completed joins use normal exit/exec.
    unsafe { _exit(status) }
}

/// The border a Hyprland window wears while it is mirrored to another machine (04 §5).
#[cfg(target_os = "linux")]
const MIRROR_BORDER: &str = "rgb(ff8800)";

/// Hyprland's side of home on the twin: the home bind (WP-2.43f) and the pointer read-back
/// (WP-2.43d), both over the agent's own IPC endpoint.
#[cfg(target_os = "linux")]
struct HyprHomeSeat {
    ipc: crosspane_platform_linux::hyprland::ipc::HyprIpc,
    bind: crosspane_platform_linux::hyprland::home_bind::HomeBind,
    /// `crosspanectl`, which the bind runs. Checked at every install: a bind that runs nothing
    /// would leave the user without the release it promises.
    ctl: std::path::PathBuf,
    /// Keep cleanup and cursor read-back available even when the command cannot be spelled.
    command_error: bool,
    /// Dropping it stops the event thread.
    reload: Option<crosspane_platform_linux::hyprland::ipc::EventStream>,
}

/// The command the home bind runs: `crosspanectl release` against this agent's control socket,
/// with the runtime directory pinned (several agents can run on one machine, 02 §3.3). `None` for
/// a path that isn't UTF-8; `HomeBind::new` refuses one that can't be quoted.
#[cfg(target_os = "linux")]
fn home_command(runtime_dir: &std::path::Path, ctl: &std::path::Path) -> Option<String> {
    let runtime_dir = runtime_dir.to_str()?;
    let ctl = ctl.to_str()?;
    if [runtime_dir, ctl].iter().any(|path| {
        path.contains('\'') || path.contains("]==]") || path.chars().any(char::is_control)
    }) {
        return None;
    }
    Some(format!(
        "env CROSSPANE_RUNTIME_DIR='{runtime_dir}' '{ctl}' release"
    ))
}

/// Whether `path` is a file this agent's effective user may execute.
#[cfg(target_os = "linux")]
fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
        && rustix::fs::accessat(
            rustix::fs::CWD,
            path,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
}

/// The home seat for the Hyprland instance `ipc` names, with the engine's release chord and the
/// `crosspanectl` next to `exe`. Construction must succeed before input is admitted, so startup
/// can always attempt cleanup of an owned bind.
#[cfg(target_os = "linux")]
fn home_seat(
    ipc: crosspane_platform_linux::hyprland::ipc::HyprIpc,
    runtime_dir: &std::path::Path,
    exe: &std::path::Path,
) -> anyhow::Result<Box<dyn HomeSeat>> {
    use anyhow::Context;
    use crosspane_platform_linux::hyprland::home_bind::HomeBind;
    // The agent never changes the engine's chord (no config key), so its default is the chord.
    let chord =
        crosspane_engine::EngineConfig::new(crosspane_types::id::NodeId([0; 32])).release_chord;
    let ctl = exe
        .parent()
        .context("agent executable has no parent directory")?
        .join("crosspanectl");
    let command = home_command(runtime_dir, &ctl);
    let command_error = command.is_none();
    if command_error {
        tracing::warn!("home bind install disabled: release command paths cannot be quoted safely");
    }
    // A cleanup-only adapter can still remove a leftover owned bind before input is admitted.
    // Its harmless command is never installed: install() refuses while command_error is set.
    let command = command.as_deref().unwrap_or("true");
    let bind = HomeBind::new(ipc.clone(), &chord, command).context("home bind construction")?;
    Ok(Box::new(HyprHomeSeat {
        ipc,
        bind,
        ctl,
        command_error,
        reload: None,
    }))
}

#[cfg(target_os = "linux")]
impl HomeSeat for HyprHomeSeat {
    fn keys(&self) -> String {
        self.bind.keys().to_owned()
    }

    fn install(&self) -> Result<(), PlatformError> {
        if self.command_error {
            return Err(PlatformError::Unsupported(
                "home release command paths cannot be quoted safely",
            ));
        }
        if !is_executable(&self.ctl) {
            return Err(PlatformError::Backend(format!(
                "{} is not an executable file, so the release shortcut would do nothing",
                self.ctl.display()
            )));
        }
        self.bind.install()
    }

    fn remove(&self) -> Result<(), PlatformError> {
        self.bind.remove()
    }

    fn installed(&self) -> Result<bool, PlatformError> {
        self.bind.installed()
    }

    fn cursor(&self) -> Result<(DisplayId, PointDevice), PlatformError> {
        crosspane_platform_linux::hyprland::cursor_position(&self.ipc)
    }

    fn watch_reload(&mut self, reload: Box<dyn Fn() + Send>) -> Result<(), PlatformError> {
        use crosspane_platform_linux::hyprland::ipc::IpcEvent;
        let stream = self.ipc.events(Box::new(move |event| match event {
            // A reload clears every keybind; a reconnect may have missed one.
            IpcEvent::Connected => reload(),
            IpcEvent::Event {
                name: "configreloaded",
                ..
            } => reload(),
            _ => {}
        }))?;
        self.reload = Some(stream);
        Ok(())
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
        hotkeys::HyprlandHotkeys, inject, ipc::HyprIpc, mirror::HyprlandMirrorParking,
        overlay::HyprlandOverlay, parking::HyprlandParking, windows::HyprlandWindows,
    };
    use crosspane_platform_linux::{logind::LogindSession, permissions::LinuxPermissions};

    let gate = IoGate::new();
    let ipc = HyprIpc::from_env().context("Crosspane on Linux needs Hyprland")?;
    ipc.require_supported().context("unsupported Hyprland")?;
    let session = LogindSession::new(gate.clone(), Some(ipc.clone()))
        .context("logind session state (required: Crosspane fails closed without it)")?;
    let displays = HyprlandDisplays::new(ipc.clone()).context("Hyprland displays")?;
    // No window is lost (04 §8 invariant 4): undo a previous run's parking before anything else.
    // Windows go on twin outputs (M2); where those can't be made, they are mirrored in place with
    // a marked border (M1, the reported fallback).
    let mut twin = optional(
        "parking",
        HyprlandParking::new(ipc.clone(), state_dir.join("parking.json")),
    );
    let mut mirror = optional(
        "mirror parking",
        HyprlandMirrorParking::new(ipc.clone(), state_dir.join("mirror.json"), MIRROR_BORDER),
    );
    let mut recoveries = Vec::new();
    for backend in [
        twin.as_mut().map(|p| p as &mut dyn WindowParking),
        mirror.as_mut().map(|p| p as &mut dyn WindowParking),
    ]
    .into_iter()
    .flatten()
    {
        match backend.recover() {
            Ok(restored) => {
                if !restored.is_empty() {
                    tracing::warn!(
                        count = restored.len(),
                        "restored windows a previous run left parked"
                    );
                }
                recoveries.push(Ok(restored.len()));
            }
            Err(e) => {
                tracing::error!(error = %e, "could not restore parked windows");
                recoveries.push(Err(()));
            }
        }
    }
    let startup_recovery = StartupRecovery::combine(&recoveries);
    let parking: Option<Box<dyn WindowParking>> = match (twin, mirror) {
        (Some(twin), Some(mirror)) => Some(Box::new(crate::twin::TwinOrMirror::new(
            Box::new(twin),
            Box::new(mirror),
        ))),
        (Some(twin), None) => Some(Box::new(twin)),
        (None, Some(mirror)) => Some(Box::new(mirror)),
        (None, None) => None,
    };
    let windows = optional("windows", HyprlandWindows::new(ipc.clone()));
    let frames = optional(
        "frame capture",
        HyprlandFrameCapture::new(gate.clone(), ipc.clone()),
    );
    // Hyprland 0.56.2 can crash while a cursor session is open (frame_capture module docs), so
    // remote cursor shapes from Hyprland sources are opt-in.
    if let Some(frames) = &frames
        && std::env::var("CROSSPANE_HYPR_CURSORS").as_deref() == Ok("1")
    {
        tracing::warn!("Hyprland cursor capture is on (CROSSPANE_HYPR_CURSORS=1)");
        frames.set_cursor_capture(true);
    }
    // DMA-BUF capture on the compositor's GPU (WP-2.28): no readback inside Hyprland's commit.
    let gpu = frames
        .as_ref()
        .filter(|_| gpu_enabled())
        .and_then(|frames| match frames.enable_gpu(wgpu::Features::empty()) {
            Ok((device, queue)) => {
                tracing::info!("GPU frame capture on (DMA-BUF)");
                Some(GpuDevice { device, queue })
            }
            Err(e) => {
                tracing::info!(error = %e, "GPU frame capture off: shared-memory capture");
                None
            }
        });
    // Home on the twin (WP-2.43): the bind runs this agent's own `crosspanectl release`, so it is
    // spelled with this agent's control-socket directory and executable.
    let paths = crate::paths::Paths::new().context("home release control-socket paths")?;
    let exe = std::env::current_exe().context("home release agent executable")?;
    let home = Some(home_seat(ipc.clone(), &paths.runtime_dir, &exe)?);
    let (keys, pointer) = match optional("injection", inject::connect(gate.clone(), ipc)) {
        Some((k, p)) => (
            Some(Box::new(k) as Box<dyn KeyInjector>),
            Some(Box::new(p) as Box<dyn PointerInjector>),
        ),
        None => (None, None),
    };
    Ok(Platform {
        home,
        proxy_placement: Some(proxy_placement()?),
        startup_recovery,
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
        window_activator: None,
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        links: Some(Box::new(
            crosspane_platform_linux::link::SysfsLinkInfo::new(),
        )),
        gpu,
        tray: optional("tray", crosspane_platform_linux::tray::SniTray::new())
            .map(|t| Box::new(t) as Box<dyn TrayHost>),
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
    // What each journal's recovery came to, for `StartupRecovery` (WP-4.5).
    let mut recoveries: Vec<Result<usize, ()>> = Vec::new();
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
                match off_main(|| twin.recover()) {
                    Ok(restored) => recoveries.push(Ok(restored.len())),
                    Err(e) => {
                        tracing::error!(error = %e, "could not restore windows parked on virtual displays");
                        recoveries.push(Err(()));
                    }
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
    if let Some(p) = parking.as_mut() {
        match off_main(|| p.recover()) {
            Ok(restored) => recoveries.push(Ok(restored.len())),
            Err(e) => {
                tracing::error!(error = %e, "could not restore parked windows");
                recoveries.push(Err(()));
            }
        }
    }
    let startup_recovery = StartupRecovery::combine(&recoveries);
    let windows = optional("windows", MacWindows::new());
    let frames = optional("frame capture", MacFrameCapture::new(gate.clone()));
    let (keys, pointer) = match optional("injection", inject::injectors(gate.clone())) {
        Some((k, p)) => (
            Some(Box::new(k) as Box<dyn KeyInjector>),
            Some(Box::new(p) as Box<dyn PointerInjector>),
        ),
        None => (None, None),
    };
    let capture = optional("capture", MacCapture::new(gate.clone()));
    let hotkeys = capture.as_ref().and_then(|capture| {
        let chord =
            crosspane_engine::EngineConfig::new(crosspane_types::id::NodeId([0; 32])).release_chord;
        optional(
            "hotkeys",
            crosspane_platform_macos::hotkeys::MacHotkeys::new(capture, &chord),
        )
    });
    Ok(Platform {
        startup_recovery,
        session: Box::new(session),
        displays: Box::new(displays),
        capture: capture.map(|c| Box::new(c) as Box<dyn InputCapture>),
        keys,
        pointer,
        overlay: optional("overlay", MacOverlay::new())
            .map(|o| Box::new(o) as Box<dyn OverlayHost>),
        hotkeys: hotkeys.map(|h| Box::new(h) as Box<dyn GlobalHotkeys>),
        keystore: keystore(),
        // Microphone is required while audio is on: the speaker loopback input is gated by it.
        permissions: Box::new(MacPermissions::new(audio_enabled())),
        windows: windows.map(|w| Box::new(w) as Box<dyn WindowSource>),
        window_activator: None,
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        tray: Some(Box::new(crosspane_platform_macos::tray::MacTray::new())),
        links: Some(Box::new(crosspane_platform_macos::link::MacLinkInfo::new())),
        gpu: gpu_enabled().then(metal_device).flatten(),
        // Home on the twin is Hyprland's: a Mac node never commits it (the engine's install
        // request is answered with an error).
        home: None,
        proxy_placement: None,
        visible_frame: crosspane_platform_macos::displays::visible_frame,
        own_windows: crosspane_platform_macos::windows::own_windows,
        gate,
    })
}

/// A headless Metal device for hashing captured frames (WP-2.27b). The Mac has one GPU, which
/// the renderer and VideoToolbox share.
#[cfg(target_os = "macos")]
fn metal_device() -> Option<GpuDevice> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::METAL;
    let adapter =
        pollster::block_on(wgpu::Instance::new(desc).request_adapter(&Default::default()))
            .map_err(|e| tracing::info!(error = %e, "no Metal adapter: CPU frame hashing"))
            .ok()?;
    let required_features = crosspane_render::source::SourceGpu::optional_features(&adapter);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("crosspane source"),
        required_features,
        ..Default::default()
    }))
    .map_err(|e| tracing::info!(error = %e, "no Metal device: CPU frame hashing"))
    .ok()?;
    Some(GpuDevice { device, queue })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    #[test]
    fn the_home_command_pins_the_runtime_directory_and_runs_release() {
        let command = home_command(
            Path::new("/run/user/1000/crosspane"),
            Path::new("/home/u/.local/bin/crosspanectl"),
        );
        assert_eq!(
            command.as_deref(),
            Some(
                "env CROSSPANE_RUNTIME_DIR='/run/user/1000/crosspane' \
                 '/home/u/.local/bin/crosspanectl' release"
            )
        );
    }

    #[test]
    fn a_path_that_is_not_text_has_no_home_command() {
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"/run/\xff"));
        assert_eq!(home_command(raw, Path::new("/bin/crosspanectl")), None);
        assert_eq!(home_command(Path::new("/run"), raw), None);
    }

    #[test]
    fn home_command_refuses_paths_that_break_shell_or_lua_quoting() {
        for unsafe_path in ["/run/a'b'c", "/run/a\nb", "/run/]==]", "/run/\0bad"] {
            let unsafe_path = Path::new(unsafe_path);
            assert_eq!(
                home_command(unsafe_path, Path::new("/bin/crosspanectl")),
                None
            );
            assert_eq!(home_command(Path::new("/run"), unsafe_path), None);
        }
        assert!(home_command(Path::new("/run/a $b `c`"), Path::new("/bin/a b")).is_some());
    }

    #[test]
    fn unquotable_paths_still_construct_a_cleanup_seat_but_cannot_install() {
        use crosspane_platform_linux::hyprland::ipc::HyprIpc;
        let ipc = HyprIpc::new(
            "test-only-no-socket",
            Path::new("/tmp"),
            std::time::Duration::from_millis(10),
        );
        let seat = home_seat(
            ipc,
            Path::new("/run/unquotable'path"),
            Path::new("/bin/crosspane-agent"),
        )
        .unwrap();
        assert!(matches!(seat.install(), Err(PlatformError::Unsupported(_))));
        // No socket exists: cleanup is attempted and fails, so startup keeps its fence.
        assert!(seat.remove().is_err());
    }

    #[test]
    fn only_an_executable_file_counts_as_the_ctl() {
        let dir = std::env::temp_dir().join(format!("crosspane-agent-ctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("crosspanectl");
        assert!(!is_executable(&file), "missing");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&file), "not executable");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o001)).unwrap();
        assert!(
            !is_executable(&file),
            "executable by others but not this owner"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&file));
        assert!(!is_executable(&dir), "a directory is not a program");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod drag_placement_tests {
    use super::*;
    use crosspane_platform::{WindowRole, WindowState};
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeLogical, SizeMm};
    use crosspane_types::id::WindowId;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Duration;

    fn fixture() -> (WindowInfo, DisplayInfo, Value, Value, Value) {
        let window = WindowInfo {
            id: WindowId(0xabc),
            title: "proxy".into(),
            app_id: "app".into(),
            pid: Some(77),
            display: Some(DisplayId(1)),
            frame: RectLogical::new(
                PointLogical::new(10.0, 20.0),
                SizeLogical::new(200.0, 100.0),
            ),
            state: WindowState::Normal,
            role: WindowRole::Toplevel,
            parent: None,
        };
        let display = DisplayInfo {
            id: DisplayId(7),
            name: "fake".into(),
            geometry: DisplayGeometry {
                pixel_size: PixelSize::new(1600, 1000),
                physical_size: SizeMm::new(300.0, 200.0),
                scale: 2.0,
                logical_origin: PointLogical::new(100.0, 200.0),
            },
            refresh_millihz: 60000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        };
        let before = json!([{"stableId":"abc", "address":"0x123", "pid":77, "title":"proxy", "mapped":true, "floating":false, "monitor":1, "workspace":{"id":3}, "at":[10,20], "size":[100,50]}]);
        let mut after = before.clone();
        after[0]["floating"] = json!(true);
        after[0]["monitor"] = json!(7);
        after[0]["workspace"]["id"] = json!(17);
        after[0]["at"] = json!([660, 230]);
        after[0]["size"] = json!([200, 100]);
        let monitors = json!([{"id":7, "activeWorkspace":{"id":17}, "x":100, "y":200, "scale":2.0, "width":1600, "height":1000, "transform":0, "reserved":[20,30,40,50]}]);
        (window, display, before, after, monitors)
    }

    #[test]
    fn float_enable_move_clamp_and_fresh_confirmation_share_one_deadline() {
        let (window, display, before, after, monitors) = fixture();
        let current = Mutex::new(before);
        let calls = Mutex::new(Vec::<(String, Duration)>::new());
        let seat = HyprProxyPlacement(|command: &str, left| {
            let mut calls = calls.lock().unwrap();
            calls.push((command.to_owned(), left));
            let mut current = current.lock().unwrap();
            if command.contains("workspace = \"17\", follow = false") {
                current[0]["monitor"] = json!(7);
                current[0]["workspace"]["id"] = json!(17);
            } else if command.contains("hl.dsp.window.float(") {
                current[0]["floating"] = json!(true);
                current[0]["size"] = json!([200, 100]);
            } else if command.contains("x = 660, y = 230, relative = false") {
                // Coordinates never move a window between workspaces or monitors.
                current[0]["at"] = json!([660, 230]);
            }
            Ok(if command == "clients" {
                current.clone()
            } else if command == "monitors" {
                monitors.clone()
            } else {
                Value::Null
            })
        });
        let frame = seat
            .place(&window, &display, PointDevice::new(9999.0, -9999.0))
            .unwrap();
        assert_eq!(frame.origin, PointLogical::new(660.0, 230.0));
        assert_eq!(
            display.geometry.logical_to_device(frame.origin),
            PointDevice::new(1120.0, 60.0)
        );
        let calls = calls.lock().unwrap();
        assert_eq!(*current.lock().unwrap(), after);
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[0].0, "clients");
        assert_eq!(calls[1].0, "monitors");
        assert!(calls[2].0.contains("workspace = \"17\", follow = false"));
        assert!(calls[3].0.contains("hl.dsp.window.float("));
        assert!(calls[3].0.contains("action = \"enable\""));
        assert_eq!(calls[4].0, "clients");
        assert!(calls[5].0.contains("hl.dsp.window.move("));
        assert!(calls[5].0.contains("x = 660, y = 230, relative = false"));
        for (command, _) in [&calls[2], &calls[3], &calls[5]] {
            assert!(command.contains("address:0x123"));
            assert!(command.contains("w.stable_id ~= 2748"));
            assert!(!command.contains("focus"));
        }
        assert_eq!(calls[6].0, "clients");
        assert!(
            calls
                .iter()
                .all(|(_, left)| !left.is_zero() && *left <= Duration::from_secs(1))
        );
        assert!(calls.windows(2).all(|c| c[1].1 <= c[0].1));
    }

    #[test]
    fn any_ipc_failure_aborts_without_confirmation_or_retry() {
        for fail_at in 0..7 {
            let (window, display, before, after, monitors) = fixture();
            let calls = Mutex::new(Vec::new());
            let seat = HyprProxyPlacement(|command: &str, _| {
                let mut calls = calls.lock().unwrap();
                let index = calls.len();
                calls.push(command.to_owned());
                if index == fail_at {
                    return Err(PlatformError::Timeout);
                }
                let mut floated = before.clone();
                floated[0]["floating"] = json!(true);
                floated[0]["size"] = json!([200, 100]);
                Ok(match command {
                    "clients" if index == 6 => after.clone(),
                    "clients" if index == 4 => floated,
                    "clients" => before.clone(),
                    "monitors" => monitors.clone(),
                    _ => Value::Null,
                })
            });
            assert!(matches!(
                seat.place(&window, &display, PointDevice::new(9999.0, -9999.0)),
                Err(PlatformError::Timeout)
            ));
            assert_eq!(calls.lock().unwrap().len(), fail_at + 1);
        }
    }

    #[test]
    fn stale_wrong_or_ambiguous_client_confirmation_never_succeeds() {
        for (field, value) in [
            ("stableId", json!("def")),
            ("address", json!("0x456")),
            ("pid", json!(78)),
            ("title", json!("foreign")),
            ("mapped", json!(false)),
            ("floating", json!(false)),
            ("monitor", json!(1)),
            ("workspace", json!({"id":3})),
            ("at", json!([10, 20])),
        ] {
            let (window, display, before, mut after, monitors) = fixture();
            after[0][field] = value;
            let calls = Mutex::new(0);
            let seat = HyprProxyPlacement(|command: &str, _| {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let mut floated = before.clone();
                floated[0]["floating"] = json!(true);
                floated[0]["size"] = json!([200, 100]);
                Ok(match command {
                    "clients" if *calls == 7 => after.clone(),
                    "clients" if *calls == 5 => floated,
                    "clients" => before.clone(),
                    "monitors" => monitors.clone(),
                    _ => Value::Null,
                })
            });
            assert!(
                seat.place(&window, &display, PointDevice::new(9999.0, -9999.0))
                    .is_err(),
                "{field}"
            );
        }
        let (window, display, before, _, _) = fixture();
        let duplicate = json!([before[0], before[0]]);
        let seat = HyprProxyPlacement(|_: &str, _| Ok(duplicate.clone()));
        assert!(seat.place(&window, &display, PointDevice::zero()).is_err());
    }

    #[test]
    fn final_size_change_that_crosses_the_reserved_work_area_is_refused() {
        let (window, display, before, mut after, monitors) = fixture();
        after[0]["size"] = json!([300, 100]);
        let calls = Mutex::new(0);
        let seat = HyprProxyPlacement(|command: &str, _| {
            let mut calls = calls.lock().unwrap();
            *calls += 1;
            let mut floated = before.clone();
            floated[0]["floating"] = json!(true);
            floated[0]["size"] = json!([200, 100]);
            Ok(match command {
                "clients" if *calls == 7 => after.clone(),
                "clients" if *calls == 5 => floated,
                "clients" => before.clone(),
                "monitors" => monitors.clone(),
                _ => Value::Null,
            })
        });
        assert!(
            seat.place(&window, &display, PointDevice::new(9999.0, -9999.0))
                .is_err()
        );
        assert_eq!(
            *calls.lock().unwrap(),
            7,
            "no retry outside the shared deadline"
        );
    }
}

#[cfg(test)]
mod startup_recovery_tests {
    use super::StartupRecovery as R;

    #[test]
    fn no_parking_backend_is_none() {
        assert_eq!(R::combine(&[]), R::None);
    }

    #[test]
    fn backends_that_recovered_with_nothing_parked_say_so() {
        assert_eq!(R::combine(&[Ok(0)]), R::NothingParked);
        assert_eq!(R::combine(&[Ok(0), Ok(0)]), R::NothingParked);
    }

    #[test]
    fn windows_put_back_are_counted_across_backends() {
        assert_eq!(R::combine(&[Ok(2)]), R::Restored(2));
        assert_eq!(R::combine(&[Ok(0), Ok(3)]), R::Restored(3));
        assert_eq!(R::combine(&[Ok(1), Ok(2)]), R::Restored(3));
    }

    #[test]
    fn one_failed_recovery_is_a_failure_whatever_the_others_did() {
        assert_eq!(R::combine(&[Err(())]), R::Failed);
        assert_eq!(R::combine(&[Ok(5), Err(())]), R::Failed);
        assert_eq!(R::combine(&[Err(()), Ok(0)]), R::Failed);
    }

    #[test]
    fn each_variant_has_its_frozen_spelling() {
        assert_eq!(R::Restored(1).as_str(), "restored");
        assert_eq!(R::NothingParked.as_str(), "nothing_parked");
        assert_eq!(R::Failed.as_str(), "failed");
        assert_eq!(R::None.as_str(), "none");
    }
}

/// A validated scratch admission requires explicit IPv4 loopback (127.0.0.1). Production keeps
/// its existing wildcard bind, and cannot opt into this hidden acceptance override.
pub(crate) fn acceptance_bind_ip(
    config: &crate::config::Config,
    admitted: bool,
) -> anyhow::Result<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match (admitted, config.acceptance_bind_ip) {
        (false, None) => Ok(Ipv6Addr::UNSPECIFIED.into()),
        (true, Some(ip)) if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) => Ok(ip),
        _ => anyhow::bail!(
            "acceptance bind requires admitted scratch and an explicit 127.0.0.1 address"
        ),
    }
}

#[cfg(test)]
mod acceptance_bind_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn production_wildcard_and_scratch_bind_admission_are_separate() {
        let mut config = crate::config::Config::default();
        assert_eq!(
            acceptance_bind_ip(&config, false).unwrap(),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        );
        assert!(acceptance_bind_ip(&config, true).is_err());
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        config.acceptance_bind_ip = Some(ip);
        assert!(acceptance_bind_ip(&config, false).is_err());
        assert_eq!(acceptance_bind_ip(&config, true).unwrap(), ip);
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            "127.0.0.2".parse().unwrap(),
            "192.0.2.1".parse().unwrap(),
            "fe80::1".parse().unwrap(),
            "::ffff:127.0.0.1".parse().unwrap(),
            "::ffff:127.0.0.2".parse().unwrap(),
        ] {
            config.acceptance_bind_ip = Some(ip);
            assert!(acceptance_bind_ip(&config, false).is_err());
            assert!(acceptance_bind_ip(&config, true).is_err());
        }
    }
}

#[cfg(all(test, windows))]
mod windows_destination_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crosspane_platform_windows::model::geometry::{DisplayIds, MonitorProbe};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn probe(path: &str, name: &str, bounds: [i32; 4], dpi: u32, primary: bool) -> MonitorProbe {
        MonitorProbe {
            device_path: path.into(),
            name: name.into(),
            rc_monitor: bounds,
            rc_work: bounds,
            primary,
            dpi,
            refresh_millihz: 60_000,
            edid: None,
            twin: false,
            quarter_turns: 0,
        }
    }

    /// The render host's rows only; the work areas are checked by the agent's placement test.
    fn mappings(
        rows: Vec<WindowsPlacementRow>,
    ) -> Vec<crosspane_render::proxy::HostMonitorMapping> {
        rows.into_iter().map(|row| row.mapping).collect()
    }

    #[test]
    fn windows_destination_rows_join_retained_ids_and_mixed_dpi_seams() {
        let a = probe("a", "A", [0, 0, 1920, 1080], 96, true);
        let b = probe("b", "B", [1920, 0, 3840, 2160], 192, false);
        let mut ids = DisplayIds::default();
        let b_id = ids.assign("b").unwrap();
        let rows = mappings(windows_monitor_rows(&[b.clone(), a], ids.clone()).unwrap());
        let b_row = rows.iter().find(|row| row.id == b_id.0).unwrap();
        assert_eq!(b_row.native_id, "B");
        assert_eq!(b_row.physical_origin, (1920, 0).into());
        assert_eq!(b_row.geometry.logical_origin, (1920.0, 0.0).into());
        assert_eq!(
            b_row.geometry.device_to_logical((200.0, 200.0).into()),
            (2020.0, 100.0).into()
        );
        let rows = mappings(
            windows_monitor_rows(
                &[
                    probe("a", "A", [0, 0, 2560, 1440], 192, true),
                    probe("b", "B", [2560, 200, 4480, 1280], 96, false),
                ],
                ids.clone(),
            )
            .unwrap(),
        );
        let b_row = rows.iter().find(|row| row.id == b_id.0).unwrap();
        assert_eq!(
            b_row.geometry.device_to_logical((100.0, 100.0).into()),
            (1380.0, 200.0).into()
        );
        let rows = mappings(
            windows_monitor_rows(
                &[
                    probe("a", "A", [0, 0, 1920, 1080], 96, true),
                    probe("b", "B", [-1920, 0, 0, 1080], 96, false),
                ],
                ids,
            )
            .unwrap(),
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == b_id.0)
                .unwrap()
                .physical_origin,
            (-1920, 0).into()
        );
    }

    #[test]
    fn windows_destination_rows_refuse_invalid_or_ambiguous_observations() {
        let a = probe("a", "A", [0, 0, 1920, 1080], 96, true);
        assert!(windows_monitor_rows(&[], DisplayIds::default()).is_err());
        assert!(windows_monitor_rows(&[a.clone(), a.clone()], DisplayIds::default()).is_err());
        let b = probe("b", "A", [1920, 0, 3840, 1080], 96, false);
        assert!(windows_monitor_rows(&[a.clone(), b], DisplayIds::default()).is_err());
        let mut invalid = a;
        invalid.dpi = 0;
        assert!(windows_monitor_rows(&[invalid], DisplayIds::default()).is_err());
    }

    #[test]
    fn windows_destination_provider_refreshes_once_and_never_keeps_last_good() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mapping = windows_placement_mapping(windows_placement_rows(Arc::new(move || {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok((
                    vec![probe("a", "A", [0, 0, 1920, 1080], 96, true)],
                    DisplayIds::default(),
                ))
            } else {
                Err(PlatformError::NotFound)
            }
        })));
        assert_eq!(mapping().unwrap().len(), 1);
        assert!(mapping().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn windows_destination_video_selection_matches_feature() {
        assert_eq!(video_codecs(None).is_some(), cfg!(feature = "video"));
    }
}

#[cfg(all(test, windows))]
mod windows_destination_hotkey_tests {
    use super::*;
    use crosspane_platform::{Chord, EventSink, HotkeyEvent};
    use std::sync::Mutex;

    struct Fake {
        fail: u8,
        calls: Arc<Mutex<Vec<u8>>>,
        drops: Arc<Mutex<usize>>,
    }
    impl GlobalHotkeys for Fake {
        fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError> {
            let expected =
                crosspane_engine::EngineConfig::new(crosspane_types::id::NodeId([0; 32]))
                    .release_chord;
            assert!(chord == &expected);
            self.calls
                .lock()
                .unwrap_or_else(|_| panic!("fixture calls lock poisoned"))
                .push(1);
            if self.fail == 1 {
                Err(PlatformError::Backend("fixture configure refusal".into()))
            } else {
                Ok(())
            }
        }
        fn subscribe(&mut self, _: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap_or_else(|_| panic!("fixture calls lock poisoned"))
                .push(2);
            if self.fail == 2 {
                Err(PlatformError::Backend("fixture subscribe refusal".into()))
            } else {
                Ok(())
            }
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            *self
                .drops
                .lock()
                .unwrap_or_else(|_| panic!("fixture drop lock poisoned")) += 1;
        }
    }

    #[test]
    fn windows_destination_hotkeys_require_creation_configuration_subscription() {
        // No live hook/observer factory is called: each failure boundary is injected.
        for fail in 0..=3 {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let drops = Arc::new(Mutex::new(0));
            let observed = calls.clone();
            let released = drops.clone();
            let result = windows_hotkeys_after_host_with(
                move || {
                    if fail == 0 {
                        Err(PlatformError::Backend("fixture creation refusal".into()))
                    } else {
                        Ok(Box::new(Fake {
                            fail,
                            calls: observed,
                            drops: released,
                        }))
                    }
                },
                Arc::new(|_: HotkeyEvent| {}),
            );
            assert_eq!(result.is_ok(), fail == 3);
            assert_eq!(
                *calls.lock().unwrap(),
                match fail {
                    0 => vec![],
                    1 => vec![1],
                    _ => vec![1, 2],
                }
            );
            assert_eq!(*drops.lock().unwrap(), usize::from(fail == 1 || fail == 2));
            drop(result);
            assert_eq!(*drops.lock().unwrap(), usize::from(fail != 0));
        }
    }
}

#[cfg(all(test, windows))]
#[allow(clippy::unwrap_used)]
mod windows_mirror_store_tests {
    use super::*;
    use crosspane_platform_windows::{
        model::parking::{
            COMMITTED_NAME, Journal, MAX_BYTES, MirrorEntry, NativeIdentity, PENDING_NAME,
        },
        parking::MirrorJournalStore,
    };
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "crosspane-mirror-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            crate::windows::security::create_private_directory(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn mirror_store_publishes_both_known_names_and_caps_private_reads() {
        let directory = Directory::new();
        let mut store = WindowsMirrorStore {
            directory: directory.0.clone(),
            empty_required: false,
            committed: COMMITTED_NAME,
            pending: PENDING_NAME,
        };
        let bytes = Journal::empty().bytes().unwrap();
        store.commit(&bytes).unwrap();
        let images = store.read().unwrap();
        assert_eq!(images.committed.as_deref(), Some(bytes.as_slice()));
        assert_eq!(images.pending.as_deref(), Some(bytes.as_slice()));
        crate::paths::write_private(&directory.0.join(COMMITTED_NAME), &vec![0; MAX_BYTES + 1])
            .unwrap();
        assert!(store.read().is_err());
        assert_eq!(
            std::fs::read(directory.0.join(PENDING_NAME)).unwrap(),
            bytes
        );
    }
    #[test]
    fn admitted_destination_scratch_refuses_outstanding_native_tuples_without_native_query() {
        let directory = Directory::new();
        let mut store = WindowsMirrorStore {
            directory: directory.0.clone(),
            empty_required: true,
            committed: COMMITTED_NAME,
            pending: PENDING_NAME,
        };
        let empty = Journal::empty().bytes().unwrap();
        store.commit(&empty).unwrap();
        assert!(store.read().is_ok());
        let entry = MirrorEntry {
            identity: NativeIdentity {
                hwnd: 7,
                pid: 8,
                tid: 9,
                process_created: 10,
            },
            original: crosspane_platform_windows::model::journal::Original {
                rect_physical: [0, 0, 400, 300],
                monitor_path: "owned-model".into(),
                show: crosspane_platform_windows::model::journal::Show::Normal,
                dpi: 96,
            },
            visible_original: [0, 0, 400, 300],
            may_have_mutated: true,
        };
        let bytes = Journal::empty().insert(entry).unwrap().bytes().unwrap();
        store.commit(&bytes).unwrap();
        assert!(store.read().is_err());
        assert_eq!(
            std::fs::read(directory.0.join(COMMITTED_NAME)).unwrap(),
            bytes
        );
    }
}

#[cfg(all(test, windows))]
mod windows_source_claim_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn owned_source_claim_refuses_expansion_and_preserves_exact_creation() {
        let executable = std::path::Path::new(r"C:\scratch\fixture\owned-window.exe");
        let valid =
            serde_json::json!({ "pid": 11, "process_created": u64::MAX, "executable": executable });
        let claim = owned_source_claim(&valid.to_string(), executable).unwrap();
        assert_eq!(claim.pid, 11);
        assert_eq!(claim.process_created, u64::MAX);
        for field in ["pid", "process_created"] {
            let mut invalid = valid.clone();
            invalid[field] = serde_json::json!(0);
            assert!(owned_source_claim(&invalid.to_string(), executable).is_err());
        }
        let mut invalid = valid.clone();
        invalid["executable"] = serde_json::json!(r"C:\other\owner.exe");
        assert!(owned_source_claim(&invalid.to_string(), executable).is_err());
        let mut invalid = valid.clone();
        invalid["other"] = serde_json::json!(1);
        assert!(owned_source_claim(&invalid.to_string(), executable).is_err());
        assert!(owned_source_claim(&format!("[{}]", valid), executable).is_err());
        assert!(owned_source_claim(&" ".repeat(8193), executable).is_err());
    }
}
