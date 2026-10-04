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
    // E2 (docs/wp/E2-v0.md)
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
    pub proxy_placement: Option<Box<dyn ProxyPlacementSeat>>,
    #[cfg(target_os = "macos")]
    pub visible_frame: fn(DisplayId) -> Result<RectLogical, PlatformError>,
    #[cfg(target_os = "macos")]
    pub own_windows: OwnWindowsRead,
    /// What the startup recovery of parked windows came to (WP-4.5).
    pub startup_recovery: StartupRecovery,
}

/// A wgpu device for the source side's GPU work (docs/wp/GPU-v0.md, decision 3).
#[derive(Clone, Debug)]
pub struct GpuDevice {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// GPU frame paths are on unless `CROSSPANE_GPU=0` (for comparisons and as an escape hatch).
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
    #[allow(unreachable_code)]
    None
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
            #[cfg(not(target_os = "linux"))]
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
    Ok(Platform {
        startup_recovery,
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
