//! Native proxy windows. Create and run the host on the process's main thread.

use std::{fmt, sync::Arc};

use crosspane_types::{
    geom::{PixelRect, PixelSize, PointDevice},
    hid::{HidUsage, MouseButton},
    input::ScrollDelta,
};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

mod app;
mod cursor;
mod gpu;
mod input;

/// Where to put a new proxy's content (DRAG-v0 §4): its top-left in the desktop's global logical
/// coordinates, computed by the agent from the display's origin and scale. macOS places the
/// frame so that the content lands there; Wayland hosts ignore it (the agent places by IPC).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HostPlace {
    pub content: winit::dpi::LogicalPosition<f64>,
}

/// One monitor from the platform's coherent logical-desktop observation.
#[derive(Clone, Debug, PartialEq)]
pub struct HostMonitorMapping {
    /// The platform's retained display ID (`DisplayId.0` on Windows).
    pub id: u32,
    /// Opaque native identity: the exact GDI device name on Windows.
    pub native_id: String,
    pub geometry: crosspane_types::geom::DisplayGeometry,
    /// Global physical desktop origin, from the same observation as `geometry`.
    pub physical_origin: winit::dpi::PhysicalPosition<i32>,
}

/// A fresh coherent monitor table, called on the host event-loop thread.
///
/// The provider must be bounded and avoid lock inversion. Return `None` instead of cached
/// geometry when its native owner is gone or the observation failed. Only Windows consumes it;
/// the agent must retain the native owner for the lifetime of every geometry consumer.
pub type HostPlacementMapping = Arc<dyn Fn() -> Option<Vec<HostMonitorMapping>> + Send + Sync>;

/// Commands to the host, sent from any thread through `HostHandle`.
pub enum HostCommand {
    /// `Window::set_fullscreen(Some(Fullscreen::Borderless(None)))` or `None`. Never `Exclusive`.
    SetFullscreen {
        id: u64,
        fullscreen: bool,
    },
    /// Open a proxy with roughly `size` device pixels of content. `accent` is the source node's
    /// colour (sRGB bytes): the proxy always shows it as a band around its content (04 §5), so a
    /// remote window can't pass as a local one.
    Open {
        id: u64,
        title: String,
        size: PixelSize,
        accent: [u8; 3],
        place: Option<HostPlace>,
    },
    /// Consume the next primary press on proxy `id` before `until` as the start of a native move
    /// (`Window::drag_window`), without reporting it (DRAG-v0 D-5). One press only. `done` is answered
    /// once the arm is installed on the host thread (`true`), or can't be (`false`); the target never
    /// injects the press before `true`.
    /// The caller creates `done` with capacity ≥ 1.
    Arm {
        id: u64,
        until: std::time::Instant,
        done: std::sync::mpsc::SyncSender<bool>,
    },
    /// End an `Arm` that wasn't used.
    Disarm {
        id: u64,
    },
    Close {
        id: u64,
    },
    SetTitle {
        id: u64,
        title: String,
    },
    /// The source's actual content size: resize the proxy's content area to it (snap-back).
    SetContentSize {
        id: u64,
        size: PixelSize,
    },
    /// Whole BGRA8 canvas; upload only the changed rectangles.
    Frame {
        id: u64,
        size: PixelSize,
        pixels: Arc<[u8]>,
        dirty: Vec<PixelRect>,
    },
    /// A decoded video picture (03 §6 video layer). The picture's top-left `rect.size()` pixels
    /// (its coded padding is never shown) are drawn at `rect.min` in the proxy's content of
    /// `size` pixels, and the tiles they cover show video until a `Frame` updates them. `rect` is
    /// tile-aligned: `min` is a multiple of 64, and `max` is a multiple of 64 or the content's
    /// edge. Whole-window video is `rect` = (0, 0)..`size`. Only the newest picture not yet drawn
    /// is uploaded; superseded ones are dropped.
    Video {
        id: u64,
        size: PixelSize,
        rect: PixelRect,
        picture: Arc<crosspane_media::picture::Nv12>,
    },
    /// A decoded picture in native memory (WP-2.24): like `Video`, but the host imports its
    /// planes into the GPU with the [`PictureImporter`] given to [`ProxyHost::set_importer`] (no
    /// copy), or copies it when there's none or the import fails. Dropping the picture releases
    /// the decoder's buffer, so the host holds only the newest one plus the one on screen.
    VideoNative {
        id: u64,
        size: PixelSize,
        rect: PixelRect,
        picture: Arc<dyn crosspane_media::picture::NativePicture>,
    },
    /// The source's cursor over this proxy (03 §4.6, WP-2.16): BGRA with straight alpha, `size`
    /// pixels at the content's density, click point `hotspot`. All-transparent hides the cursor.
    SetCursor {
        id: u64,
        size: PixelSize,
        hotspot: (u32, u32),
        pixels: Arc<[u8]>,
    },
    /// Show the platform's default cursor over this proxy (the source can't see its shape).
    DefaultCursor {
        id: u64,
    },
    /// Close every proxy and leave the event loop.
    Shutdown,
    /// Run a closure on the main thread.
    Run(Box<dyn FnOnce() + Send>),
}

// FnOnce cannot derive Debug. Describe the closure without invoking it or inspecting captures.
impl fmt::Debug for HostCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open {
                id,
                title,
                size,
                accent,
                ..
            } => f
                .debug_struct("Open")
                .field("id", id)
                .field("title", title)
                .field("size", size)
                .field("accent", accent)
                .finish(),
            Self::Arm { id, until, .. } => f
                .debug_struct("Arm")
                .field("id", id)
                .field("until", until)
                .finish(),
            Self::Disarm { id } => f.debug_struct("Disarm").field("id", id).finish(),
            Self::Close { id } => f.debug_struct("Close").field("id", id).finish(),
            Self::SetTitle { id, title } => f
                .debug_struct("SetTitle")
                .field("id", id)
                .field("title", title)
                .finish(),
            Self::SetFullscreen { id, fullscreen } => f
                .debug_struct("SetFullscreen")
                .field("id", id)
                .field("fullscreen", fullscreen)
                .finish(),
            Self::SetContentSize { id, size } => f
                .debug_struct("SetContentSize")
                .field("id", id)
                .field("size", size)
                .finish(),
            Self::Frame {
                id,
                size,
                pixels,
                dirty,
            } => f
                .debug_struct("Frame")
                .field("id", id)
                .field("size", size)
                .field("bytes", &pixels.len())
                .field("dirty", dirty)
                .finish(),
            Self::Video { id, size, rect, .. } => f
                .debug_struct("Video")
                .field("id", id)
                .field("size", size)
                .field("rect", rect)
                .finish_non_exhaustive(),
            Self::VideoNative { id, size, rect, .. } => f
                .debug_struct("VideoNative")
                .field("id", id)
                .field("size", size)
                .field("rect", rect)
                .finish_non_exhaustive(),
            Self::SetCursor {
                id, size, hotspot, ..
            } => f
                .debug_struct("SetCursor")
                .field("id", id)
                .field("size", size)
                .field("hotspot", hotspot)
                .finish_non_exhaustive(),
            Self::DefaultCursor { id } => f.debug_struct("DefaultCursor").field("id", id).finish(),
            Self::Shutdown => f.write_str("Shutdown"),
            Self::Run(_) => f.write_str("Run(..)"),
        }
    }
}

/// What the host reports. Positions are device pixels of the content area, origin top-left.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    /// `Window::fullscreen().is_some()` changed; sent before the `Resized` of the same change.
    Fullscreen {
        id: u64,
        fullscreen: bool,
    },
    Opened {
        id: u64,
        size: PixelSize,
        scale: f64,
    },
    OpenFailed {
        id: u64,
        error: String,
    },
    Resized {
        id: u64,
        size: PixelSize,
        scale: f64,
    },
    Focus {
        id: u64,
        focused: bool,
    },
    CloseRequested {
        id: u64,
    },
    /// The window went away without a request, or rendering failed for good.
    Lost {
        id: u64,
    },
    /// The proxy presented `frames` frames with new content since its previous `Presented`
    /// report. A frame counts when the window acquires a surface texture, draws and presents it
    /// after at least one `Frame`, `Video` or `VideoNative` for it arrived since its previous
    /// counted present. Redraws that only resize, flash the edge or change the cursor don't
    /// count, and neither do frames whose surface texture couldn't be acquired (occluded,
    /// timeout, outdated or lost). wgpu doesn't report a presentation the platform rejects after
    /// a successful acquisition, so such a rare frame may be counted. Reports are coalesced:
    /// at most one per window per 250 ms, and pending counts go out within 250 ms even when no
    /// further present happens. `frames` is never 0. Counts still pending when the window closes
    /// or is lost may be dropped.
    Presented {
        id: u64,
        frames: u32,
    },
    Key {
        id: u64,
        usage: HidUsage,
        down: bool,
    },
    Button {
        id: u64,
        button: MouseButton,
        down: bool,
        position: PointDevice,
    },
    Scroll {
        id: u64,
        delta: ScrollDelta,
        position: PointDevice,
    },
    Motion {
        id: u64,
        position: PointDevice,
    },
    /// Where the content area is: its top-left at `origin` device pixels on monitor `monitor`
    /// (the platform's native id: the `CGDirectDisplayID` on macOS; `None` where the host can't
    /// tell, e.g. Wayland), `size` device pixels. `visible: false`: minimised or fully occluded.
    /// Sent after `Opened` and whenever any of it changes.
    Placed {
        id: u64,
        /// The own NSWindow's number on macOS; unavailable on other platforms.
        window_number: Option<u32>,
        visible: bool,
        monitor: Option<u32>,
        origin: PointDevice,
        size: PixelSize,
    },
}

/// The exact Windows host device/queue lifetime, observed on the host thread.
/// Ready is sent after lazy device creation and before Opened or native import; Retired is
/// sent once on loss or shutdown, before the host drops that generation's device.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
pub enum WindowsDecodeDevice {
    Ready {
        generation: u64,
        device: wgpu::Device,
        queue: wgpu::Queue,
    },
    Retired {
        generation: u64,
    },
}

/// Bounded metadata-only callback: no COM/MF calls, waits or locks held by a waiting decoder.
#[cfg(target_os = "windows")]
pub type WindowsDecodeDeviceObserver = Arc<dyn Fn(WindowsDecodeDevice) + Send + Sync>;

/// Memory-only progress after nonblocking device polling. Return the earliest active absolute
/// operation deadline, capped to a 5 ms tick, or None when work has settled/retired. Retire work
/// at its 2 s operation deadline before returning; an expired Some retires host decode admission.
/// Never perform native work or wait here. First work must separately wake HostHandle::Run.
#[cfg(target_os = "windows")]
pub type WindowsDecodeProgress =
    Arc<dyn Fn(std::time::Instant) -> Option<std::time::Instant> + Send + Sync>;

/// Imports a native picture's planes into the host's GPU `device` without copying them (WP-2.24):
/// `[luma, chroma]` as an `R8Unorm` texture of the picture's coded size and an `Rg8Unorm` texture
/// of half that, both with `TEXTURE_BINDING`. The textures must keep the picture's memory alive
/// until wgpu destroys them (wgpu does that only after the GPU has finished with them). Platform
/// crates provide one for their decoders' pictures; it runs on the host's thread.
pub type PictureImporter = Arc<
    dyn Fn(
            &wgpu::Device,
            &dyn crosspane_media::picture::NativePicture,
        ) -> Result<[wgpu::Texture; 2], String>
        + Send
        + Sync,
>;

/// Cloneable, Send handle for commanding the host from other threads.
#[derive(Clone, Debug)]
pub struct HostHandle {
    proxy: EventLoopProxy<HostCommand>,
}

impl HostHandle {
    /// `Err` if the host has exited.
    pub fn send(&self, command: HostCommand) -> Result<(), HostError> {
        self.proxy
            .send_event(command)
            .map_err(|_| HostError::Exited)
    }
}

/// The host. `new` must run on the main thread (winit's rule on macOS).
pub struct ProxyHost {
    event_loop: EventLoop<HostCommand>,
    importer: Option<PictureImporter>,
    #[cfg(target_os = "windows")]
    placement_mapping: Option<HostPlacementMapping>,
    #[cfg(target_os = "windows")]
    decode_device_observer: Option<WindowsDecodeDeviceObserver>,
    #[cfg(target_os = "windows")]
    decode_progress: Option<WindowsDecodeProgress>,
}

impl ProxyHost {
    pub fn new() -> Result<(ProxyHost, HostHandle), HostError> {
        #[allow(unused_mut)]
        let mut builder = EventLoop::<HostCommand>::with_user_event();
        // Crosspane is a menu-bar app: no Dock icon, never steals activation on its own.
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        }
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            // Winit attempts PER_MONITOR_AWARE_V2 before its older-OS fallbacks. This must
            // happen before any proxy HWND is created; geometry stays in physical pixels.
            builder.with_dpi_aware(true);
        }
        Self::from_loop(builder.build()?)
    }

    /// Linux tests only: build the event loop on a non-main thread (Wayland allows it).
    #[cfg(target_os = "linux")]
    pub fn new_any_thread() -> Result<(ProxyHost, HostHandle), HostError> {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        let mut builder = EventLoop::<HostCommand>::with_user_event();
        builder.with_wayland().with_any_thread(true);
        Self::from_loop(builder.build()?)
    }

    fn from_loop(event_loop: EventLoop<HostCommand>) -> Result<(Self, HostHandle), HostError> {
        // Windows winit 0.30.13 registers both Raw Input classes during construction. Its
        // immediate Never call removes them once, before the agent creates its exclusive
        // capture/hotkey observers; this host consumes only window events. Never repeat this
        // call after those observers exist (including on resume, focus or proxy recreation).
        #[cfg(windows)]
        event_loop.listen_device_events(winit::event_loop::DeviceEvents::Never);
        event_loop.set_control_flow(ControlFlow::Wait);
        let handle = HostHandle {
            proxy: event_loop.create_proxy(),
        };
        Ok((
            Self {
                event_loop,
                importer: None,
                #[cfg(target_os = "windows")]
                placement_mapping: None,
                #[cfg(target_os = "windows")]
                decode_device_observer: None,
                #[cfg(target_os = "windows")]
                decode_progress: None,
            },
            handle,
        ))
    }

    /// Import `VideoNative` pictures with `importer` instead of copying them.
    pub fn set_importer(&mut self, importer: PictureImporter) {
        self.importer = Some(importer);
    }

    /// Install a placement provider before `run`. Only the Windows host consumes it.
    pub fn set_placement_mapping(&mut self, mapping: HostPlacementMapping) {
        #[cfg(target_os = "windows")]
        {
            self.placement_mapping = Some(mapping);
        }
        #[cfg(not(target_os = "windows"))]
        let _ = mapping;
    }

    /// Observe the real Windows receive device before run. This opts into requesting NV12
    /// when supported; optional device creation failure retries ordinary CPU-render creation.
    #[cfg(target_os = "windows")]
    pub fn set_windows_decode_device_observer(&mut self, observer: WindowsDecodeDeviceObserver) {
        self.decode_device_observer = Some(observer);
    }

    /// Install bounded receive-copy progress before run; no hook or idle polling by default.
    #[cfg(target_os = "windows")]
    pub fn set_windows_decode_progress(&mut self, progress: WindowsDecodeProgress) {
        self.decode_progress = Some(progress);
    }

    /// Run until `Shutdown`. The event callback runs on the main thread and must not block.
    pub fn run(self, events: Box<dyn FnMut(HostEvent)>) -> Result<(), HostError> {
        let mut app = app::App::new(
            self.event_loop.create_proxy(),
            events,
            self.importer,
            #[cfg(target_os = "windows")]
            self.placement_mapping,
            #[cfg(target_os = "windows")]
            self.decode_device_observer,
            #[cfg(target_os = "windows")]
            self.decode_progress,
        );
        self.event_loop.run_app(&mut app)?;
        Ok(())
    }
}

impl fmt::Debug for ProxyHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyHost")
            .field("event_loop", &self.event_loop)
            .field("importer", &self.importer.is_some())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("window event loop: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("proxy host has exited")]
    Exited,
}

/// The macOS smoke test of `HostEvent::Placed` (WP-2.43c). winit's macOS event loop must be
/// created on the process's main thread, and libtest runs every test on a thread of its own, so
/// the test compiles a small driver program with `rustc` against the test build's own rlibs (the
/// way the platform crate's GUI tests do) and runs it. The driver uses the public `ProxyHost`
/// API only, in the lead's GUI session.
///
/// **What it proves:**
/// - the first report follows `Opened` at once, names an active display (the main display when it
///   is the only one), repeats the opened size, and starts conservatively as `visible: false`;
/// - AppKit then posts `Occluded(false)` on first show, so the proxy becomes `visible: true`
///   (open item U: "AppKit posts Occluded(false) on first show");
/// - the content lies inside its display and below the display's top edge (a titled window's
///   content is never at the top, so a zero or defaulted origin fails);
/// - moving the window by a known 50 points moves the reported origin by 50 points in device
///   pixels (within one pixel), so a move re-samples the window;
/// - `SetContentSize` is reported with the new size, and minimising reports `visible: false`.
///
/// **What it does not prove:** that the absolute origin is where the window really is (`Open`
/// has no position, so there is no known point to compare with), that `monitor` is the primary
/// monitor on a multi-display setup, behaviour across displays of different scale, or display
/// reconfiguration. The exact-origin and `CGMainDisplayID`/`CGWindowListCopyWindowInfo` checks
/// (open item U2) are the lead's WP-2.43e live check. It does not restore a minimised window
/// (`deminiaturize:` is an unsafe binding), so the move is a relative check instead.
///
/// Run it from the worktree's own `target` (the driver takes the newest rlibs in the test
/// binary's `deps` directory) with `OPUS_LIB_DIR` set, as for any Mac build:
/// `CROSSPANE_MAC_LIVE=1 cargo nextest run -p crosspane-render --run-ignored only live_placement_smoke`
/// (`placement_driver_compiles` builds the driver without running it).
#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod mac_live {
    use std::{
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    /// The driver's source. It never runs unless `CROSSPANE_MAC_LIVE=1`.
    const DRIVER: &str = r##"
use std::{
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use core_graphics::display::CGDisplay;
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle, ProxyHost};
use crosspane_types::geom::PixelSize;
use objc2_app_kit::{NSApplication, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint};

const ID: u64 = 1;
/// How far the window is moved, in points.
const SHIFT: f64 = 50.0;

/// Run `action` on every window of this process, on the main thread.
fn on_windows(handle: &HostHandle, action: fn(&NSWindow)) {
    handle
        .send(HostCommand::Run(Box::new(move || {
            let mtm = MainThreadMarker::new().expect("Run executes on the main thread");
            for window in NSApplication::sharedApplication(mtm).windows().iter() {
                action(window);
            }
        })))
        .expect("run on the main thread");
}

fn wait_for(
    events: &Receiver<HostEvent>,
    what: &str,
    accept: impl Fn(&HostEvent) -> bool,
) -> HostEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = events
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        eprintln!("event: {event:?}");
        match event {
            HostEvent::Lost { .. } | HostEvent::OpenFailed { .. } => {
                panic!("the host failed: {event:?}")
            }
            _ if accept(&event) => return event,
            _ => {}
        }
    }
}

fn check(handle: &HostHandle, events: &Receiver<HostEvent>) {
    handle
        .send(HostCommand::Open {
            id: ID,
            title: "WP-2.43c placement".into(),
            size: PixelSize::new(640, 480),
            accent: [10, 20, 30],
            place: None,
        })
        .expect("open");
    let HostEvent::Opened { size: opened, scale, .. } =
        wait_for(events, "Opened", |e| matches!(e, HostEvent::Opened { id: ID, .. }))
    else {
        unreachable!()
    };

    // The first report comes straight after `Opened`: nothing can come between them.
    let HostEvent::Placed { id: ID, visible, monitor: Some(monitor), origin, size, .. } =
        events.recv_timeout(Duration::from_secs(15)).expect("a report after Opened")
    else {
        panic!("Opened was not followed by a Placed that names a monitor");
    };
    eprintln!("placed: visible {visible}, monitor {monitor}, origin {origin:?}, size {size:?}");
    assert!(
        !visible,
        "a proxy must not report visible before AppKit's Occluded(false)"
    );
    assert_eq!(size, opened, "Placed repeats the opened content size");
    // U2: the native ID is a CGDirectDisplayID, in the same device pixels as the origin.
    let active = CGDisplay::active_displays().expect("active displays");
    assert!(active.contains(&monitor), "{monitor} is not an active display: {active:?}");
    if active.len() == 1 {
        assert_eq!(monitor, CGDisplay::main().id, "the only display is the main one");
    }
    let display = CGDisplay::new(monitor);
    let wide = display.pixels_wide() as f64 * scale;
    let high = display.pixels_high() as f64 * scale;
    // A titled window's content is below the display's top edge: a zero origin is a default.
    assert!(
        origin.x >= 0.0
            && origin.y > 0.0
            && origin.x + f64::from(size.width) <= wide + 1.0
            && origin.y + f64::from(size.height) <= high + 1.0,
        "the content at {origin:?} + {size:?} is not inside the {wide} x {high} pixel display"
    );

    // Open item: AppKit posts Occluded(false) when the window first shows. Until it does, the
    // proxy stays reported as hidden (the safe side), and this times out.
    let HostEvent::Placed { origin: shown_at, .. } = wait_for(
        events,
        "a Placed with visible: true (AppKit posts Occluded(false) on first show)",
        |e| matches!(e, HostEvent::Placed { id: ID, visible: true, .. }),
    ) else {
        unreachable!()
    };

    // A move re-samples the window: 50 points to the right is 50 * scale device pixels.
    on_windows(handle, |window| {
        let frame = window.frame();
        window.setFrameTopLeftPoint(NSPoint::new(
            frame.origin.x + SHIFT,
            frame.origin.y + frame.size.height,
        ));
    });
    let shift = SHIFT * scale;
    let HostEvent::Placed { monitor: moved_on, origin: moved_to, .. } = wait_for(
        events,
        "a Placed shifted by 50 points",
        |e| {
            matches!(e, HostEvent::Placed { id: ID, origin, .. }
                if (origin.x - shown_at.x - shift).abs() <= 1.0
                    && (origin.y - shown_at.y).abs() <= 1.0)
        },
    ) else {
        unreachable!()
    };
    assert_eq!(moved_on, Some(monitor), "the move stayed on the display");
    eprintln!("moved: {shown_at:?} -> {moved_to:?}");

    // A resize reports the new size (an even size: exact at any scale).
    let want = PixelSize::new(700, 500);
    handle
        .send(HostCommand::SetContentSize { id: ID, size: want })
        .expect("resize");
    let HostEvent::Placed { monitor: again, visible, .. } = wait_for(
        events,
        "a Placed with the new size",
        |e| matches!(e, HostEvent::Placed { id: ID, size, .. } if *size == want),
    ) else {
        unreachable!()
    };
    assert_eq!(again, Some(monitor), "resizing does not change the display");
    assert!(visible, "resizing does not hide the proxy");

    // Minimising covers the proxy: AppKit's occlusion change must reach the report.
    on_windows(handle, |window| window.miniaturize(None));
    wait_for(events, "a Placed with visible: false after minimising", |e| {
        matches!(e, HostEvent::Placed { id: ID, visible: false, .. })
    });
    eprintln!("placement driver: passed");
}

fn main() {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: the placement driver needs CROSSPANE_MAC_LIVE=1 in the GUI session");
        return;
    }
    let (host, handle) = ProxyHost::new().expect("proxy host");
    let (sender, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let passed =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&handle, &events)))
                .is_ok();
        let _ = handle.send(HostCommand::Shutdown);
        passed
    });
    host.run(Box::new(move |event| {
        let _ = sender.send(event);
    }))
    .expect("event loop");
    let passed = worker.join().unwrap_or(false);
    std::process::exit(if passed { 0 } else { 1 });
}
"##;

    /// The newest rlib of `name` in `deps`. With a `version` (`objc2-app-kit-0.2.2/`), only one
    /// whose dep-info names that source directory: the build also has the objc2 0.6 family.
    fn library(deps: &Path, name: &str, version: Option<&str>) -> PathBuf {
        let prefix = format!("lib{name}-");
        std::fs::read_dir(deps)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
                    && path.extension().is_some_and(|ext| ext == "rlib")
            })
            .filter(|path| {
                version.is_none_or(|version| {
                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let info = deps.join(format!("{}.d", stem.trim_start_matches("lib")));
                    std::fs::read_to_string(info).is_ok_and(|info| info.contains(version))
                })
            })
            .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
            .unwrap_or_else(|| panic!("missing compiled dependency {name}"))
    }

    /// Compile [`DRIVER`] next to the test binary and return the executable's path. The files
    /// are named for the process, the test and a counter, so tests running at the same time (or
    /// one running twice in a process) never share a source or an executable.
    fn build_driver(test: &str) -> PathBuf {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let deps = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let stem = format!(
            "placement-driver-{}-{test}-{}",
            std::process::id(),
            BUILDS.fetch_add(1, Ordering::Relaxed)
        );
        let source = deps.join(format!("{stem}.rs"));
        let executable = deps.join(&stem);
        std::fs::write(&source, DRIVER).unwrap();
        let mut compiler = Command::new("rustc");
        compiler
            .args(["--edition=2024", "--crate-name", "placement_driver", "-L"])
            .arg(format!("dependency={}", deps.display()));
        // Cargo's build script for libopus (a dependency of the media crate) hands this search
        // path to the linker; a hand-run `rustc` must pass it itself.
        if let Some(opus) = std::env::var_os("OPUS_LIB_DIR") {
            compiler
                .arg("-L")
                .arg(format!("native={}", Path::new(&opus).join("lib").display()));
        }
        for (name, version) in [
            ("crosspane_render", None),
            ("crosspane_types", None),
            ("core_graphics", Some("core-graphics-0.23.2/")),
            ("objc2_app_kit", Some("objc2-app-kit-0.2.2/")),
            ("objc2_foundation", Some("objc2-foundation-0.2.2/")),
        ] {
            compiler.arg("--extern").arg(format!(
                "{name}={}",
                library(&deps, name, version).display()
            ));
        }
        let status = compiler.arg(&source).arg("-o").arg(&executable).status();
        std::fs::remove_file(&source).unwrap();
        assert!(
            status.unwrap().success(),
            "the placement driver does not compile"
        );
        executable
    }

    /// Compiles the driver and runs nothing: checks that it still builds against this tree.
    #[test]
    #[ignore = "compiles a driver with rustc against the test build's rlibs; opens no window"]
    fn placement_driver_compiles() {
        std::fs::remove_file(build_driver("compiles")).unwrap();
    }

    /// `HostEvent::Placed` on a real window: a smoke test, not the exact-origin check (see the
    /// module documentation for what it does and does not prove). Lead only.
    #[test]
    #[ignore = "opens a window: needs CROSSPANE_MAC_LIVE=1 in the lead's GUI session"]
    fn live_placement_smoke() {
        if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
            eprintln!(
                "skipped: the placement smoke test needs CROSSPANE_MAC_LIVE=1 in the GUI session"
            );
            return;
        }
        let executable = build_driver("smoke");
        let status = Command::new(&executable)
            .env("CROSSPANE_MAC_LIVE", "1")
            .status()
            .unwrap();
        std::fs::remove_file(executable).unwrap();
        assert!(status.success(), "placement driver failed: {status}");
    }
}
