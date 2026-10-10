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
    /// Whether this node's input injection is authorized *right now* (WP-G1.6). `None` for
    /// backends whose injectors are usable whenever they exist (Hyprland, macOS, Windows): the
    /// agent's grants then depend on the injectors' presence alone. The portal backends (GNOME,
    /// KDE) set it to the EIS source's liveness, so closing the RemoteDesktop session retires
    /// `InputAccept` on the next housekeeping tick. It is called on the agent's loop and must
    /// not block.
    pub input_live: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Proof that the native factory admitted its exact isolated acceptance contract.
    #[cfg(windows)]
    pub(crate) acceptance_scratch: bool,
    /// Only the admitted owned-window source fixture omits clipboard and unused input.
    #[cfg(windows)]
    pub(crate) acceptance_source: bool,
    /// Fresh Windows placement rows. The refresh closure is weak; `displays` owns its worker.
    #[cfg(windows)]
    pub host_placement_mapping: Option<crosspane_render::proxy::HostPlacementMapping>,
    /// The RemoteDesktop portal session of the GNOME/KDE backends, kept for the platform's
    /// lifetime (WP-G1.6). It is the last field on purpose: fields drop in declaration order, so
    /// the injectors above release what they hold before the session closes. The status glue
    /// thread holds it only weakly (see `portal_glue`).
    #[cfg(target_os = "linux")]
    pub(crate) portal_session:
        Option<Arc<crosspane_platform_linux::portal::session::RemoteDesktopSession>>,
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
        let mut debug = f.debug_struct("Platform");
        debug
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
            .field("input_live", &self.input_live.as_ref().map(|live| live()));
        #[cfg(target_os = "linux")]
        debug.field("portal_session", &self.portal_session.is_some());
        debug.finish_non_exhaustive()
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
    // The only Linux clipboard backend is Hyprland's (ext-data-control). GNOME and KDE get the
    // portal clipboard later (GNOME-v0 WP-G3.1); until then they have none rather than a
    // Hyprland-named backend that was never validated there.
    #[cfg(target_os = "linux")]
    let host = if is_hyprland_session() {
        std::env::var_os("WAYLAND_DISPLAY")
            .ok_or(PlatformError::NotFound)
            .and_then(|display| {
                crosspane_platform_linux::hyprland::clipboard::HyprlandClipboard::new(gate, display)
            })
            .map(|host| Box::new(host) as Box<dyn crosspane_platform::ClipboardHost>)
    } else {
        let _ = gate;
        Err(PlatformError::Unsupported(
            "clipboard on this desktop (portal clipboard not built yet)",
        ))
    };
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

/// Each call takes one bounded observation on the retained display worker. Called on the
/// host event-loop thread (and the engine thread for explicit placement), without agent locks.
/// Never cache geometry: a dead owner or failed observation yields OS-selected placement.
#[cfg(windows)]
fn windows_placement_mapping(
    refresh: crosspane_platform_windows::inject::MonitorRefresh,
) -> crosspane_render::proxy::HostPlacementMapping {
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

#[cfg(windows)]
fn windows_monitor_rows(
    probes: &[crosspane_platform_windows::model::geometry::MonitorProbe],
    mut ids: crosspane_platform_windows::model::geometry::DisplayIds,
) -> Result<Vec<crosspane_render::proxy::HostMonitorMapping>, PlatformError> {
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
        rows.push(crosspane_render::proxy::HostMonitorMapping {
            id: id.0,
            native_id: probe.name.clone(),
            geometry: display.geometry,
            physical_origin: (probe.rc_monitor[0], probe.rc_monitor[1]).into(),
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
#[cfg(windows)]
pub fn windows_hotkeys_after_host(
    sink: Arc<dyn crosspane_platform::EventSink<crosspane_platform::HotkeyEvent>>,
) -> Result<Box<dyn GlobalHotkeys>, PlatformError> {
    windows_hotkeys_after_host_with(
        || {
            crosspane_platform_windows::hotkey::WindowsHotkeys::new()
                .map(|backend| Box::new(backend) as Box<dyn GlobalHotkeys>)
        },
        sink,
    )
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

#[cfg(windows)]
pub fn create(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
    mut recovered: WindowsRecoveredMirror,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_windows::{
        capture::WindowsCapture, displays::WindowsDisplays, frame_capture::WindowsFrameCapture,
        inject, link::WindowsLinkInfo, overlay::WindowsOverlay, session::WindowsSession,
        stubs::UnsupportedWindows, tray::WindowsTray, window::WindowsWindowSource,
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
    let host_placement_mapping = Some(windows_placement_mapping(displays.monitor_refresh()));
    let mut ids = snapshot.ids;
    let capture = if source_only {
        None
    } else {
        optional(
            "capture",
            WindowsCapture::new(gate.clone(), &snapshot.probes, &mut ids),
        )
        .map(|backend| Box::new(backend) as Box<dyn InputCapture>)
    };
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
            Some(Box::new(pointer) as Box<dyn PointerInjector>),
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
    let parking = if let Some(windows) = windows.as_ref() {
        use crosspane_platform_windows::{drag::WindowsRestorePlacer, model::drag::PlacedRestore};
        recovered
            .parking
            .bind_source(
                windows.resolver(),
                displays.ids(),
                displays.monitor_reader(),
            )
            .context("Windows mirror source binding (required)")?;
        // DRAG-v0 D-6 placed return decorates the final stack. The decorator needs the concrete
        // stack, so each arm wraps and then boxes its own; a boxed mirror cannot be wrapped.
        let placer = WindowsRestorePlacer::new(
            windows.resolver(),
            displays.ids(),
            displays.monitor_reader(),
        );
        let report: Box<dyn Fn(&PlatformError) + Send> = Box::new(|error: &PlatformError| {
            tracing::warn!(%error, "restored window placement refused; restore remains successful");
        });
        match recovered.twin.take() {
            // Scratch modes keep M1 alone.
            Some(mut twin) if !scratch_only => {
                twin.bind_source(
                    windows.resolver(),
                    displays.ids(),
                    displays.monitor_reader(),
                )
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
    let frames = windows.as_ref().and_then(|windows| {
        optional(
            "frame capture",
            if source_only {
                // The owned source permits per-window WGC only, never the VM's monitor.
                WindowsFrameCapture::new(gate.clone(), windows.resolver())
            } else {
                WindowsFrameCapture::new_with_monitor_reader(
                    gate.clone(),
                    windows.resolver(),
                    displays.monitor_snapshot_reader(),
                )
            },
        )
    });
    let gpu = if std::env::var("CROSSPANE_GPU").as_deref() != Ok("0") {
        frames.as_ref().and_then(|frames| {
            frames.enable_gpu(wgpu::Features::empty())
                .map(|windows| GpuDevice { device: windows.device.clone(), queue: windows.queue.clone(), windows })
                .map_err(|error| tracing::info!(%error, "Windows source GPU unavailable: CPU frame path"))
                .ok()
        })
    } else {
        None
    };
    Ok(Platform {
        gate,
        session: Box::new(session),
        // Reader/refresh closures hold Weak references. This Box retains the native owner
        // and authoritative allocator for the full lifetime of all geometry consumers.
        displays: Box::new(displays),
        capture,
        keys,
        pointer,
        overlay,
        // Startup constructs and subscribes this required observer after the host has
        // relinquished winit's keyboard/mouse registrations. A failure aborts startup.
        hotkeys: None,
        keystore: if scratch_only { None } else { keystore() },
        // required=[] is benign in the agent: no OS request/subscription, notice or retry.
        permissions: Box::new(UnsupportedWindows),
        windows: windows.map(|backend| Box::new(backend) as Box<dyn WindowSource>),
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
        input_live: None,
        acceptance_scratch: scratch_only,
        acceptance_source: source_only,
        host_placement_mapping,
    })
}

/// Call `lost` (once, from another thread) when the Hyprland instance this agent belongs to dies.
///
/// Hyprland restarts in place after a crash (Omarchy's `start-hyprland`) without ending the
/// graphical session, so the service manager doesn't stop the agent with it: on 2026-10-01 an agent
/// sat on a dead instance for hours, unable to capture, inject or park. The instance's
/// `hyprland.lock` names its process; nothing is sent over the compositor's IPC to check it.
///
/// A no-op off Hyprland (WP-G1.6): a GNOME or KDE agent has no such instance to watch, and a
/// Wayland compositor's death there ends the graphical session, which stops the agent's unit.
#[cfg(target_os = "linux")]
pub fn watch_compositor(lost: impl FnOnce() + Send + 'static) {
    if !is_hyprland_session() {
        tracing::debug!("not on Hyprland: not watching the compositor");
        return;
    }
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

/// This Linux desktop's backends (WP-G1.6). The backend is chosen from the desktop the process
/// runs in, by `desktop::detect`, and never falls back to another one: an unsupported or
/// ambiguous desktop is a startup error, not a Hyprland attempt.
#[cfg(target_os = "linux")]
pub fn create(
    state_dir: &std::path::Path,
    config: &crate::config::Config,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_linux::desktop::{DesktopEnv, LinuxDesktop, detect};

    let desktop = detect(&DesktopEnv::from_process())
        .context("Crosspane on Linux needs Hyprland, GNOME or KDE Plasma on Wayland")?;
    tracing::info!(?desktop, "Linux desktop");
    match desktop {
        LinuxDesktop::Hyprland => create_hyprland(state_dir, config),
        LinuxDesktop::Gnome | LinuxDesktop::Kde => create_portal(state_dir, config, desktop),
    }
}

/// Whether this process runs under Hyprland: the decision [`create`] made, from the same
/// environment. The Hyprland-only pieces outside `create` (the compositor watch, the clipboard
/// host) ask this instead of assuming it.
#[cfg(target_os = "linux")]
fn is_hyprland_session() -> bool {
    use crosspane_platform_linux::desktop::{DesktopEnv, LinuxDesktop, detect};
    matches!(
        detect(&DesktopEnv::from_process()),
        Ok(LinuxDesktop::Hyprland)
    )
}

/// The monotonic clock of [`now`], as the `Clock` the portal backends take.
#[cfg(target_os = "linux")]
struct AgentClock;

#[cfg(target_os = "linux")]
impl crosspane_types::time::Clock for AgentClock {
    fn now(&self) -> MonoTime {
        now()
    }
}

/// The displays of a portal backend, with the latest snapshot also readable from other threads.
///
/// The EIS injectors (absolute-pointer region mapping) and the Shell overlay need the display
/// geometry from their own threads, while `Platform::displays` owns the backend. This adapter
/// forwards to it and keeps the newest snapshot in a shared cell: seeded when it is built,
/// replaced by every subscription event *before* the event is passed on, so a consumer woken by
/// an event reads that snapshot or a newer one. A lost observation delivers an empty snapshot
/// (the `Displays` contract), which then makes region lookups fail closed (`NotFound`).
#[cfg(target_os = "linux")]
struct CachedDisplays<D> {
    inner: D,
    latest: Arc<std::sync::Mutex<Vec<DisplayInfo>>>,
}

#[cfg(target_os = "linux")]
impl<D: Displays> CachedDisplays<D> {
    /// Wrap `inner`, reading its first snapshot. Returns the adapter and the reader closure the
    /// injectors take.
    fn new(
        inner: D,
    ) -> Result<(Self, crosspane_platform_linux::portal::eis::DisplaysFn), PlatformError> {
        let latest = Arc::new(std::sync::Mutex::new(inner.displays()?));
        let reader = latest.clone();
        let read: crosspane_platform_linux::portal::eis::DisplaysFn = Arc::new(move || {
            reader
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        Ok((CachedDisplays { inner, latest }, read))
    }
}

#[cfg(target_os = "linux")]
impl<D: Displays> Displays for CachedDisplays<D> {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        self.inner.displays()
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn crosspane_platform::EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        let latest = self.latest.clone();
        self.inner
            .subscribe(Arc::new(move |displays: Vec<DisplayInfo>| {
                *latest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = displays.clone();
                sink.send(displays);
            }))
    }
}

/// Start the RemoteDesktop portal session and tie it to the EIS source (WP-G1.6).
///
/// **Threads and ownership.** The session's worker calls the status callback on its own thread,
/// where it must not block, while `EisSource::attach` can take 2 s. So the callback only queues
/// each status on a channel, and one "portal-eis" glue thread (`portal_glue`) works the queue in
/// order: it takes the EIS socket of each new epoch and attaches it, and detaches the source when
/// the session closes, is denied or is unavailable. Statuses queue from the moment the session
/// exists, so a fast silent start (a stored restore token) loses nothing before the glue runs.
///
/// The session is returned as an `Arc` for the `Platform` to hold; the glue thread holds it only
/// weakly. That breaks the cycle session worker → callback → channel → glue → session, so dropping
/// the platform's `Arc` closes the session (bounded join), which drops the callback, which ends the
/// glue thread's loop (it detaches the source once more on the way out). The glue thread is never
/// joined: it ends on its own, and shutdown never waits for it.
///
/// `None` when the portal session or the glue thread can't be started: the source then never goes
/// live and the agent never grants `InputAccept`.
#[cfg(target_os = "linux")]
fn start_portal_session(
    state_dir: &std::path::Path,
    source: crosspane_platform_linux::portal::eis::EisSource,
) -> Option<Arc<crosspane_platform_linux::portal::session::RemoteDesktopSession>> {
    use crosspane_platform_linux::portal::session::{
        RemoteDesktopConfig, RemoteDesktopSession, SessionStatus,
    };

    let (statuses_tx, statuses) = std::sync::mpsc::channel::<SessionStatus>();
    let config = RemoteDesktopConfig {
        token_path: state_dir.join("portal-remote-desktop.token"),
    };
    let session = optional(
        "RemoteDesktop portal session",
        RemoteDesktopSession::spawn(
            config,
            Arc::new(move |status| {
                // Never blocks; a send after the glue thread ended (shutdown) is dropped.
                let _ = statuses_tx.send(status);
            }),
        ),
    )?;
    let session = Arc::new(session);
    let weak = Arc::downgrade(&session);
    let glue = std::thread::Builder::new()
        .name("portal-eis".into())
        .spawn(move || portal_glue(&source, &weak, &statuses));
    if let Err(error) = glue {
        // The session drops here, which closes it: no consent without something to attach it to.
        tracing::warn!(%error, "could not start the portal input glue: remote input stays off");
        return None;
    }
    Some(session)
}

/// The body of the "portal-eis" thread: see [`start_portal_session`].
#[cfg(target_os = "linux")]
fn portal_glue(
    source: &crosspane_platform_linux::portal::eis::EisSource,
    session: &std::sync::Weak<crosspane_platform_linux::portal::session::RemoteDesktopSession>,
    statuses: &std::sync::mpsc::Receiver<crosspane_platform_linux::portal::session::SessionStatus>,
) {
    use crosspane_platform_linux::portal::session::SessionStatus;

    while let Ok(status) = statuses.recv() {
        match status {
            SessionStatus::Pending => {}
            SessionStatus::Active { epoch } => {
                // The platform dropped the session: nothing can authorize input any more.
                let Some(session) = session.upgrade() else {
                    break;
                };
                // Each epoch's socket is handed out once and only while that epoch is active, so a
                // status that was overtaken (the session has since closed, or moved on to a later
                // epoch whose own status is queued behind this one) finds nothing or the newest.
                match session.take_eis() {
                    Some((taken, fd)) => match source.attach(fd) {
                        Ok(()) => tracing::info!(epoch = taken, "remote input session attached"),
                        Err(error) => {
                            tracing::warn!(epoch = taken, %error, "remote input session not attached");
                            source.detach();
                        }
                    },
                    None => tracing::debug!(epoch, "no EIS socket to take (stale status)"),
                }
            }
            SessionStatus::Closed { epoch } => {
                tracing::warn!(epoch, "remote input session closed: input is refused");
                source.detach();
            }
            SessionStatus::Denied => {
                tracing::warn!("remote input was not allowed: input is refused");
                source.detach();
            }
            SessionStatus::Unavailable => {
                tracing::warn!("no RemoteDesktop portal: remote input is unavailable");
                source.detach();
            }
        }
    }
    source.detach();
}

/// One parking backend's startup `recover()` (04 §8 invariant 4): how many windows it put back, or
/// `Err(())` once the failure is logged. The loop body of `create_hyprland`, for the portal
/// backends.
#[cfg(target_os = "linux")]
fn recover_parked(backend: &mut dyn WindowParking) -> Result<usize, ()> {
    match backend.recover() {
        Ok(restored) => {
            if !restored.is_empty() {
                tracing::warn!(
                    count = restored.len(),
                    "restored windows a previous run left parked"
                );
            }
            Ok(restored.len())
        }
        Err(e) => {
            tracing::error!(error = %e, "could not restore parked windows");
            Err(())
        }
    }
}

/// GNOME and KDE (WP-G1.6): E1 target and E2 destination through portals, EIS and public Wayland
/// protocols. Input capture (this node cannot control others yet), GPU capture, home on the twin
/// and proxy placement are absent. The agent grants `InputAccept` only while the portal session is
/// live (`Platform::input_live`).
///
/// **GNOME as an E2 source** (WP-G2.3/G2.2) needs the Crosspane Shell extension: one
/// `ShellBridge` is shared (cloned) by the overlay, the window source, the mirror parking and the
/// window capture. Without the extension,
/// and on KDE, there is no window source, parking or frame capture, and the node stays a
/// destination. The order is fixed:
///
/// 1. connect the bridge;
/// 2. build the M1 mirror parking and run its `recover()`, before anything else can park (04 §8
///    invariant 4) and before the consent dialogs below. A failing recovery (or an unreadable
///    journal) is `StartupRecovery::Failed` and leaves no parking backend, so nothing is parked
///    on top of entries that could not be undone;
/// 3. the injection and RemoteDesktop session, hotkeys and overlay (as on KDE);
/// 4. the window source;
/// 5. frame capture, only with both a window source and a parking backend: the ScreenCast session
///    (its consent dialog is the caller-prepared one, shown at most once) wrapped by the window
///    capture. A failure leaves `frames` empty.
#[cfg(target_os = "linux")]
fn create_portal(
    state_dir: &std::path::Path,
    _config: &crate::config::Config,
    desktop: crosspane_platform_linux::desktop::LinuxDesktop,
) -> anyhow::Result<Platform> {
    use anyhow::Context;
    use crosspane_platform_linux::desktop::LinuxDesktop;
    use crosspane_platform_linux::gnome::{
        overlay::GnomeOverlay, parking::GnomeMirrorParking, shell::ShellBridge,
        window_capture::GnomeWindowCapture, windows::GnomeWindows,
    };
    use crosspane_platform_linux::logind::{LockerEvidence, LogindSession};
    use crosspane_platform_linux::permissions::LinuxPermissions;
    use crosspane_platform_linux::portal::screencast::{PortalScreenCast, ScreenCastConfig};
    use crosspane_platform_linux::portal::{eis::EisSource, shortcuts::PortalHotkeys};
    use crosspane_platform_linux::wayland_outputs::WaylandOutputs;

    let locker = match desktop {
        LinuxDesktop::Gnome => LockerEvidence::GnomeScreenSaver,
        LinuxDesktop::Kde => LockerEvidence::FreedesktopScreenSaver,
        LinuxDesktop::Hyprland => anyhow::bail!("the portal backend does not run on Hyprland"),
    };
    let gate = IoGate::new();
    let session = LogindSession::with_locker(gate.clone(), locker)
        .context("session lock state (required: Crosspane fails closed without it)")?;
    let outputs = WaylandOutputs::new().context("Wayland outputs")?;
    let (displays, displays_fn) =
        CachedDisplays::new(outputs).context("Wayland outputs: first snapshot")?;

    // The Crosspane Shell extension's bridge (GNOME only), shared by everything that needs the
    // Shell: the overlay, the window source, the mirror parking and the window capture.
    let bridge = match desktop {
        LinuxDesktop::Gnome => match ShellBridge::connect() {
            Ok(bridge) => Some(bridge),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the Crosspane Shell extension is unavailable: no overlay, and GNOME source \
                     projection (window list, parking, capture) needs it; this node stays a \
                     destination"
                );
                None
            }
        },
        LinuxDesktop::Kde | LinuxDesktop::Hyprland => None,
    };

    // No window is lost (04 §8 invariant 4): undo a previous run's parking before anything else,
    // including the consent dialogs below. Windows are mirrored in place (M1); a failed recovery,
    // or a journal that can't be read (it is the only record of the original frames), is reported
    // and leaves no parking backend, so nothing is parked on top of it.
    let mut recoveries: Vec<Result<usize, ()>> = Vec::new();
    let parking: Option<Box<dyn WindowParking>> = bridge.as_ref().and_then(|bridge| {
        match GnomeMirrorParking::new(
            bridge.clone(),
            displays_fn.clone(),
            state_dir.join("gnome-mirror.json"),
        ) {
            Ok(mut backend) => match recover_parked(&mut backend) {
                Ok(count) => {
                    recoveries.push(Ok(count));
                    Some(Box::new(backend) as Box<dyn WindowParking>)
                }
                Err(()) => {
                    recoveries.push(Err(()));
                    None
                }
            },
            Err(error) => {
                tracing::error!(%error, "the mirror journal can't be read: windows are not parked");
                recoveries.push(Err(()));
                None
            }
        }
    });
    let startup_recovery = StartupRecovery::combine(&recoveries);

    // Portals know this non-sandboxed process by a registered app id; the shared connection
    // (GlobalShortcuts) is registered here, the RemoteDesktop worker registers its own.
    if let Err(e) = crosspane_platform_linux::portal::register_host_app() {
        tracing::warn!(error = %e, "portal app id not registered");
    }

    // Injection: one EIS source shared by the keyboard and the pointer. It is live only while the
    // portal session below is, and the agent reads that through `input_live`.
    let (source, keys, pointer) = match optional(
        "injection",
        EisSource::new(gate.clone(), displays_fn.clone()),
    ) {
        Some((source, k, p)) => (
            Some(source),
            Some(Box::new(k) as Box<dyn KeyInjector>),
            Some(Box::new(p) as Box<dyn PointerInjector>),
        ),
        None => (None, None, None),
    };
    let input_live = source.as_ref().map(|source| {
        let source = source.clone();
        Arc::new(move || source.is_live()) as Arc<dyn Fn() -> bool + Send + Sync>
    });
    // The consent may be asked now (the first time): this is the startup, off the input path.
    let portal_session = source.and_then(|source| start_portal_session(state_dir, source));

    // The release chord (panic: holding it for a second ends everything). Without the
    // GlobalShortcuts portal there is none, and the tray and `crosspanectl` remain.
    let hotkeys =
        optional("hotkeys", PortalHotkeys::new(Arc::new(AgentClock))).and_then(|mut hotkeys| {
            let chord = crosspane_engine::EngineConfig::new(crosspane_types::id::NodeId([0; 32]))
                .release_chord;
            optional("release chord", hotkeys.set_chord(&chord)).map(|()| hotkeys)
        });
    // The indicator comes from the Crosspane Shell extension, on GNOME only; without it GNOME's
    // own remote-desktop indicator is the only visible sign. KDE's comes later.
    let overlay = bridge.as_ref().map(|bridge| {
        Box::new(GnomeOverlay::new(bridge.clone(), displays_fn.clone())) as Box<dyn OverlayHost>
    });

    // E2 source (GNOME with the extension only): the window list, then frame capture. Capture
    // needs the other two: with no window source or no parking backend a projection can't start,
    // and the ScreenCast session (and its consent dialog) would be asked for nothing.
    let windows = bridge.as_ref().and_then(|bridge| {
        optional(
            "windows",
            GnomeWindows::new(bridge.clone(), displays_fn.clone()),
        )
    });
    let frames = match (&bridge, &windows, &parking) {
        (Some(bridge), Some(_), Some(_)) => optional(
            "ScreenCast portal",
            PortalScreenCast::new(
                gate.clone(),
                ScreenCastConfig {
                    token_path: state_dir.join("portal-screencast.token"),
                },
                displays_fn.clone(),
            ),
        )
        .and_then(|inner| {
            optional(
                "frame capture",
                GnomeWindowCapture::new(inner, bridge.clone(), displays_fn.clone()),
            )
        }),
        _ => None,
    };
    Ok(Platform {
        gate,
        session: Box::new(session),
        displays: Box::new(displays),
        // Controller capture is deferred (GNOME-v0, G1.7): this node can be controlled, not control.
        capture: None,
        keys,
        pointer,
        overlay,
        hotkeys: hotkeys.map(|h| Box::new(h) as Box<dyn GlobalHotkeys>),
        keystore: keystore(),
        permissions: Box::new(LinuxPermissions),
        windows: windows.map(|w| Box::new(w) as Box<dyn WindowSource>),
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        tray: optional("tray", crosspane_platform_linux::tray::SniTray::new())
            .map(|t| Box::new(t) as Box<dyn TrayHost>),
        links: Some(Box::new(
            crosspane_platform_linux::link::SysfsLinkInfo::new(),
        )),
        // CPU frames from the ScreenCast stream: no GPU capture on the portal backends yet.
        gpu: None,
        home: None,
        proxy_placement: None,
        // What the mirror parking's startup recovery came to (`None` without a bridge).
        startup_recovery,
        input_live,
        portal_session,
    })
}

#[cfg(target_os = "linux")]
fn create_hyprland(
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
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        links: Some(Box::new(
            crosspane_platform_linux::link::SysfsLinkInfo::new(),
        )),
        gpu,
        tray: optional("tray", crosspane_platform_linux::tray::SniTray::new())
            .map(|t| Box::new(t) as Box<dyn TrayHost>),
        gate,
        // Hyprland's injectors are usable whenever they exist.
        input_live: None,
        portal_session: None,
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
        parking,
        frames: frames.map(|f| Box::new(f) as Box<dyn FrameCapture>),
        tray: Some(Box::new(crosspane_platform_macos::tray::MacTray::new())),
        links: Some(Box::new(crosspane_platform_macos::link::MacLinkInfo::new())),
        gpu: gpu_enabled().then(metal_device).flatten(),
        // Home on the twin is Hyprland's: a Mac node never commits it (the engine's install
        // request is answered with an error).
        home: None,
        proxy_placement: None,
        input_live: None,
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

/// The portal backends' shared display snapshot (WP-G1.6).
#[cfg(all(test, target_os = "linux"))]
mod cached_displays_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use crosspane_platform::EventSink;
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};

    use super::*;

    fn display(id: u32, x: f64) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("D{id}"),
            geometry: DisplayGeometry {
                pixel_size: PixelSize::new(1920, 1080),
                physical_size: SizeMm::new(500.0, 300.0),
                scale: 1.0,
                logical_origin: PointLogical::new(x, 0.0),
            },
            refresh_millihz: 60000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    type Subscriber = Arc<dyn EventSink<Vec<DisplayInfo>>>;

    /// A backend whose snapshot is set by the test and which keeps its subscriber.
    #[derive(Default)]
    struct Fake {
        now: Arc<Mutex<Vec<DisplayInfo>>>,
        sink: Arc<Mutex<Option<Subscriber>>>,
    }

    impl Displays for Fake {
        fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
            Ok(self.now.lock().unwrap().clone())
        }

        fn subscribe(
            &mut self,
            sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
        ) -> Result<(), PlatformError> {
            *self.sink.lock().unwrap() = Some(sink);
            Ok(())
        }
    }

    #[test]
    fn the_reader_is_seeded_and_every_event_updates_it_before_it_is_forwarded() {
        let fake = Fake::default();
        *fake.now.lock().unwrap() = vec![display(1, 0.0)];
        let backend_sink = fake.sink.clone();
        let (mut cached, read) = CachedDisplays::new(fake).unwrap();
        assert_eq!(
            read(),
            vec![display(1, 0.0)],
            "seeded from the first snapshot"
        );

        // The forwarded sink reads the shared snapshot, as an injector woken by the event would.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (reader, log) = (read.clone(), seen.clone());
        cached
            .subscribe(Arc::new(move |displays: Vec<DisplayInfo>| {
                log.lock().unwrap().push((displays, reader()));
            }))
            .unwrap();
        let deliver = |displays: Vec<DisplayInfo>| {
            let sink = backend_sink.lock().unwrap().clone().unwrap();
            sink.send(displays);
        };
        deliver(vec![display(1, 0.0), display(2, 1920.0)]);
        // A lost observation delivers an empty snapshot: region lookups then find nothing.
        deliver(Vec::new());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for (event, read_then) in seen.iter() {
            assert_eq!(
                event, read_then,
                "the cell already holds the event's snapshot"
            );
        }
        assert_eq!(seen[0].0.len(), 2);
        assert!(read().is_empty());
    }

    #[test]
    fn a_first_snapshot_that_fails_fails_the_adapter() {
        struct Failing;
        impl Displays for Failing {
            fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
                Err(PlatformError::Backend("gone".into()))
            }
            fn subscribe(
                &mut self,
                _sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
            ) -> Result<(), PlatformError> {
                Ok(())
            }
        }
        assert!(CachedDisplays::new(Failing).is_err());
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

    #[test]
    fn windows_destination_rows_join_retained_ids_and_mixed_dpi_seams() {
        let a = probe("a", "A", [0, 0, 1920, 1080], 96, true);
        let b = probe("b", "B", [1920, 0, 3840, 2160], 192, false);
        let mut ids = DisplayIds::default();
        let b_id = ids.assign("b").unwrap();
        let rows = windows_monitor_rows(&[b.clone(), a], ids.clone()).unwrap();
        let b_row = rows.iter().find(|row| row.id == b_id.0).unwrap();
        assert_eq!(b_row.native_id, "B");
        assert_eq!(b_row.physical_origin, (1920, 0).into());
        assert_eq!(b_row.geometry.logical_origin, (1920.0, 0.0).into());
        assert_eq!(
            b_row.geometry.device_to_logical((200.0, 200.0).into()),
            (2020.0, 100.0).into()
        );
        let rows = windows_monitor_rows(
            &[
                probe("a", "A", [0, 0, 2560, 1440], 192, true),
                probe("b", "B", [2560, 200, 4480, 1280], 96, false),
            ],
            ids.clone(),
        )
        .unwrap();
        let b_row = rows.iter().find(|row| row.id == b_id.0).unwrap();
        assert_eq!(
            b_row.geometry.device_to_logical((100.0, 100.0).into()),
            (1380.0, 200.0).into()
        );
        let rows = windows_monitor_rows(
            &[
                probe("a", "A", [0, 0, 1920, 1080], 96, true),
                probe("b", "B", [-1920, 0, 0, 1080], 96, false),
            ],
            ids,
        )
        .unwrap();
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
        let mapping = windows_placement_mapping(Arc::new(move || {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok((
                    vec![probe("a", "A", [0, 0, 1920, 1080], 96, true)],
                    DisplayIds::default(),
                ))
            } else {
                Err(PlatformError::NotFound)
            }
        }));
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
