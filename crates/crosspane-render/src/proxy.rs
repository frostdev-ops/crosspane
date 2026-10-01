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
    /// Open a proxy with roughly `size` device pixels of content.
    Open {
        id: u64,
        title: String,
        size: PixelSize,
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
    /// The source's cursor over this proxy (03 §4.6, WP-2.16): BGRA with straight alpha, `size`
    /// pixels at the content's density, click point `hotspot`. All-transparent hides the cursor.
    SetCursor {
        id: u64,
        size: PixelSize,
        hotspot: (u32, u32),
        pixels: Arc<[u8]>,
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
            Self::Open { id, title, size } => f
                .debug_struct("Open")
                .field("id", id)
                .field("title", title)
                .field("size", size)
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
            Self::SetCursor {
                id, size, hotspot, ..
            } => f
                .debug_struct("SetCursor")
                .field("id", id)
                .field("size", size)
                .field("hotspot", hotspot)
                .finish_non_exhaustive(),
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
#[derive(Debug)]
pub struct ProxyHost {
    event_loop: EventLoop<HostCommand>,
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
        Ok((Self { event_loop }, handle))
    }

    /// Run until `Shutdown`. The event callback runs on the main thread and must not block.
    pub fn run(self, events: Box<dyn FnMut(HostEvent)>) -> Result<(), HostError> {
        let mut app = app::App::new(self.event_loop.create_proxy(), events);
        self.event_loop.run_app(&mut app)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("window event loop: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("proxy host has exited")]
    Exited,
}
