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

/// Commands to the host, sent from any thread through `HostHandle`.
pub enum HostCommand {
    /// Open a proxy with roughly `size` device pixels of content. `accent` is the source node's
    /// colour (sRGB bytes): the proxy always shows it as a band around its content (04 §5), so a
    /// remote window can't pass as a local one.
    Open {
        id: u64,
        title: String,
        size: PixelSize,
        accent: [u8; 3],
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
            } => f
                .debug_struct("Open")
                .field("id", id)
                .field("title", title)
                .field("size", size)
                .field("accent", accent)
                .finish(),
            Self::Close { id } => f.debug_struct("Close").field("id", id).finish(),
            Self::SetTitle { id, title } => f
                .debug_struct("SetTitle")
                .field("id", id)
                .field("title", title)
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
}

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
        event_loop.set_control_flow(ControlFlow::Wait);
        let handle = HostHandle {
            proxy: event_loop.create_proxy(),
        };
        Ok((
            Self {
                event_loop,
                importer: None,
            },
            handle,
        ))
    }

    /// Import `VideoNative` pictures with `importer` instead of copying them.
    pub fn set_importer(&mut self, importer: PictureImporter) {
        self.importer = Some(importer);
    }

    /// Run until `Shutdown`. The event callback runs on the main thread and must not block.
    pub fn run(self, events: Box<dyn FnMut(HostEvent)>) -> Result<(), HostError> {
        let mut app = app::App::new(self.event_loop.create_proxy(), events, self.importer);
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
