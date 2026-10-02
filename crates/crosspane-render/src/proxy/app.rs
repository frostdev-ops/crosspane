use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crosspane_types::geom::{PixelSize, PointDevice};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    keyboard::PhysicalKey,
    monitor::MonitorHandle,
    window::{CursorIcon, CustomCursor, Window, WindowId},
};

use super::{
    HostCommand, HostEvent, PictureImporter, cursor,
    gpu::{Presenter, surface_format},
    input::{InputState, mouse_button, scroll},
};
use crate::keymap::keycode_to_hid;

pub(super) struct App {
    proxy: EventLoopProxy<HostCommand>,
    events: Box<dyn FnMut(HostEvent)>,
    instance: Option<wgpu::Instance>,
    gpu: Option<Gpu>,
    windows: HashMap<u64, ProxyWindow>,
    ids: HashMap<WindowId, u64>,
    pending: VecDeque<HostCommand>,
    importer: Option<PictureImporter>,
}

struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    failed: Arc<AtomicBool>,
}

struct ProxyWindow {
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    presenter: Presenter,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    scale: f64,
    accent: [u8; 3],
    fullscreen: bool,
    flash_until: Option<Instant>,
    input: InputState,
    consecutive_surface_losses: u8,
    presents: PresentCounter,
    /// What `HostEvent::Placed` last said about this window, and the occlusion it is built on.
    placement: PlacementTracker,
}

impl App {
    pub(super) fn new(
        proxy: EventLoopProxy<HostCommand>,
        events: Box<dyn FnMut(HostEvent)>,
        importer: Option<PictureImporter>,
    ) -> Self {
        Self {
            importer,
            proxy,
            events,
            instance: None,
            gpu: None,
            windows: HashMap::new(),
            ids: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: u64,
        title: String,
        size: PixelSize,
        accent: [u8; 3],
    ) -> Result<(), String> {
        if self.windows.contains_key(&id) {
            return Err("proxy ID is already open".into());
        }
        if size.width == 0 || size.height == 0 {
            return Err("content size must be nonzero".into());
        }
        let instance = self.instance.as_ref().ok_or("event loop has not resumed")?;
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let initial_scale = monitor
            .as_ref()
            .map_or(1.0, |monitor| monitor.scale_factor());
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_decorations(true)
            .with_resizable(true)
            .with_inner_size(fit(
                logical_size(size, initial_scale),
                screen(monitor.as_ref()),
            ));
        #[cfg(target_os = "linux")]
        let attributes = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attributes.with_name("crosspane-proxy", "Crosspane")
        };
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| error.to_string())?,
        );
        window.set_ime_allowed(false);
        let scale = window.scale_factor();
        if scale != initial_scale {
            let _ = window.request_inner_size(fit(
                logical_size(size, scale),
                screen(window.current_monitor().as_ref()),
            ));
        }
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| error.to_string())?;
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::new(instance, &surface, self.proxy.clone())?);
        }
        let gpu = self.gpu.as_ref().ok_or("GPU initialization failed")?;
        if gpu.failed.load(Ordering::Acquire) {
            return Err("shared GPU has failed".into());
        }
        let caps = surface.get_capabilities(&gpu.adapter);
        let format =
            surface_format(&caps.formats).ok_or("surface has no byte-exact 8-bit format")?;
        tracing::info!(?format, formats = ?caps.formats, adapter = ?gpu.adapter.get_info(), "proxy surface format");
        let actual_size = window.inner_size();
        let mut config = surface
            .get_default_config(
                &gpu.adapter,
                actual_size.width.max(1),
                actual_size.height.max(1),
            )
            .ok_or("surface has no supported configuration")?;
        if !caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            return Err("surface does not support FIFO presentation".into());
        }
        config.format = format;
        config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        config.desired_maximum_frame_latency = 1;
        tracing::debug!(id, mode = ?config.present_mode, "proxy present mode");
        // Opaque avoids compositor alpha blending of a source window's canvas where supported.
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        }
        let presenter = Presenter::new(&gpu.device, format);
        let mut proxy = ProxyWindow {
            surface,
            window,
            presenter,
            config,
            size: actual_size,
            scale,
            accent,
            fullscreen: false,
            flash_until: None,
            input: InputState::default(),
            consecutive_surface_losses: 0,
            presents: PresentCounter::new(Instant::now()),
            placement: PlacementTracker::for_new_window(),
        };
        proxy.resize(gpu, actual_size)?;
        if gpu.failed.load(Ordering::Acquire) {
            return Err("GPU failed while opening proxy".into());
        }
        self.ids.insert(proxy.window.id(), id);
        self.windows.insert(id, proxy);
        (self.events)(HostEvent::Opened {
            id,
            size: pixel_size(actual_size),
            scale,
        });
        self.report_placement(id, Trigger::Opened);
        Ok(())
    }

    /// Say where the proxy's content is (`HostEvent::Placed`), computed from the window as it is
    /// now and only if it differs from what was last said. Which events call this on which
    /// platform is [`Trigger::applies`].
    fn report_placement(&mut self, id: u64, trigger: Trigger) {
        if !trigger.applies(TRACKS_GEOMETRY) {
            return;
        }
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        let sample = sample_window(&window.window);
        if let Some(placed) = window.placement.update(&sample) {
            tracing::debug!(id, ?placed, ?trigger, "proxy placement");
            (self.events)(placed.event(id));
        }
    }

    fn remove(&mut self, id: u64, lost: bool) {
        if let Some(mut window) = self.windows.remove(&id) {
            self.ids.remove(&window.window.id());
            window.input.release(id, self.events.as_mut());
            if lost {
                (self.events)(HostEvent::Lost { id });
            }
        }
    }

    fn check_gpu(&mut self) {
        if let Some(gpu) = &self.gpu
            && gpu.device.poll(wgpu::PollType::Poll).is_err()
        {
            gpu.failed.store(true, Ordering::Release);
        }
        if self
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.failed.load(Ordering::Acquire))
        {
            let ids: Vec<_> = self.windows.keys().copied().collect();
            for id in ids {
                self.remove(id, true);
            }
        }
    }

    fn command(&mut self, event_loop: &ActiveEventLoop, command: HostCommand) {
        self.check_gpu();
        match command {
            HostCommand::Open {
                id,
                title,
                size,
                accent,
            } => {
                if let Err(error) = self.open(event_loop, id, title, size, accent) {
                    (self.events)(HostEvent::OpenFailed { id, error });
                }
            }
            HostCommand::Close { id } => self.remove(id, false),
            HostCommand::SetTitle { id, title } => {
                if let Some(window) = self.windows.get(&id) {
                    window.window.set_title(&title);
                }
            }
            HostCommand::SetContentSize { id, size } => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                if let Some(window) = self.windows.get_mut(&id) {
                    // The source's size is exact: no opening fit and no logical rounding, or the
                    // window system's answer would differ from it and be sent back as a resize.
                    let result = window.window.request_inner_size(content_request(size));
                    if let Some(actual) = result {
                        self.resized(id, actual, window_scale(&self.windows, id));
                    }
                }
            }
            HostCommand::Frame {
                id,
                size,
                pixels,
                dirty,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    window.presents.content();
                    match window
                        .presenter
                        .upload(&gpu.device, &gpu.queue, size, &pixels, &dirty)
                    {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::Video {
                id,
                size,
                rect,
                picture,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    window.presents.content();
                    match window.presenter.set_video(&gpu.device, size, rect, picture) {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::VideoNative {
                id,
                size,
                rect,
                picture,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    window.presents.content();
                    match window
                        .presenter
                        .set_native_video(&gpu.device, size, rect, picture)
                    {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::SetCursor {
                id,
                size,
                hotspot,
                pixels,
            } => {
                if let Some(window) = self.windows.get(&id) {
                    set_cursor(event_loop, &window.window, size, hotspot, &pixels);
                }
            }
            HostCommand::DefaultCursor { id } => {
                if let Some(window) = self.windows.get(&id) {
                    window.window.set_cursor(CursorIcon::Default);
                    window.window.set_cursor_visible(true);
                }
            }
            HostCommand::Shutdown => {
                let ids: Vec<_> = self.windows.keys().copied().collect();
                for id in ids {
                    self.remove(id, false);
                }
                self.pending.clear();
                event_loop.exit();
            }
            HostCommand::Run(function) => function(),
        }
        self.check_gpu();
    }

    fn geometry_changed(&mut self, id: u64, report: Report) {
        if report.stale {
            tracing::debug!(id, size = ?report.size, scale = report.scale, "window event payload was stale");
        }
        self.resized(id, report.size, report.scale);
    }

    fn resized(&mut self, id: u64, size: PhysicalSize<u32>, scale: f64) {
        let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) else {
            return;
        };
        let fullscreen = window.window.fullscreen().is_some();
        if fullscreen && !window.fullscreen {
            window.flash_until = Some(Instant::now() + Duration::from_secs(2));
        }
        window.fullscreen = fullscreen;
        window.scale = scale;
        if let Err(error) = window.resize(gpu, size) {
            tracing::warn!(id, %error, "proxy resize failed");
            self.remove(id, true);
            return;
        }
        (self.events)(HostEvent::Resized {
            id,
            size: pixel_size(size),
            scale,
        });
        // After `Resized`, so the engine has the new size before the placement that carries it.
        self.report_placement(id, Trigger::Geometry);
    }
}

fn set_cursor(
    event_loop: &ActiveEventLoop,
    window: &Window,
    size: PixelSize,
    hotspot: (u32, u32),
    pixels: &[u8],
) {
    match cursor::shape(
        size.width,
        size.height,
        hotspot,
        pixels,
        window.scale_factor(),
    ) {
        // The pointer over a proxy is this machine's own: it never disappears. A hidden remote
        // cursor (an app hides it while typing until the mouse moves, which injected motion may
        // not count as) shows the default arrow instead.
        Some(cursor::Shape::Hidden) => {
            window.set_cursor(CursorIcon::Default);
            window.set_cursor_visible(true);
        }
        Some(cursor::Shape::Image(image)) => {
            let source = CustomCursor::from_rgba(
                image.rgba,
                image.width,
                image.height,
                image.hotspot.0,
                image.hotspot.1,
            );
            match source {
                Ok(source) => {
                    window.set_cursor(event_loop.create_custom_cursor(source));
                    window.set_cursor_visible(true);
                }
                Err(error) => tracing::debug!(%error, "cursor image refused"),
            }
        }
        None => tracing::debug!("malformed cursor image"),
    }
}

impl ApplicationHandler<HostCommand> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.instance.is_none() {
            self.instance = Some(wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
                    event_loop.owned_display_handle(),
                )),
            ));
        }
        while let Some(command) = self.pending.pop_front() {
            self.command(event_loop, command);
            if event_loop.exiting() {
                break;
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: HostCommand) {
        if self.instance.is_none() {
            self.pending.push_back(event);
        } else {
            self.command(event_loop, event);
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.check_gpu();
        let Some(&id) = self.ids.get(&window_id) else {
            return;
        };
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        match event {
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(usage) = keycode_to_hid(code)
                {
                    window.input.key(usage, event.state);
                    (self.events)(HostEvent::Key {
                        id,
                        usage,
                        down: event.state.is_pressed(),
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = mouse_button(button) {
                    window.input.button(button, state);
                    (self.events)(HostEvent::Button {
                        id,
                        button,
                        down: state.is_pressed(),
                        position: window.input.position,
                    });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                window.input.position = PointDevice::new(position.x, position.y);
                (self.events)(HostEvent::Motion {
                    id,
                    position: window.input.position,
                });
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                (self.events)(HostEvent::Scroll {
                    id,
                    delta: scroll(delta, phase, window.scale),
                    position: window.input.position,
                });
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    window.input.release(id, self.events.as_mut());
                }
                (self.events)(HostEvent::Focus { id, focused });
                // A window that was deminiaturised becomes key again after AppKit's last
                // occlusion event, which can still have seen it as minimised.
                if focused {
                    self.report_placement(id, Trigger::Focus);
                }
            }
            // What is reported is the window as it is now, never the event's own payload: winit's
            // macOS `Resized` is a frame snapshot taken when the event was queued, and delivery
            // can come after later native changes.
            WindowEvent::Resized(size) => {
                let report = reported(Change::Resized(size), sample(&window.window));
                self.geometry_changed(id, report);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let report = reported(Change::ScaleFactor(scale_factor), sample(&window.window));
                self.geometry_changed(id, report);
            }
            // Moving the window is how it changes display (macOS has no other event for it), and
            // moves report where it is now, never the event's own position.
            WindowEvent::Moved(_) => self.report_placement(id, Trigger::Geometry),
            WindowEvent::Occluded(occluded) => {
                window.placement.set_occluded(occluded);
                self.report_placement(id, Trigger::Occlusion);
            }
            WindowEvent::CloseRequested => (self.events)(HostEvent::CloseRequested { id }),
            WindowEvent::Destroyed => self.remove(id, true),
            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(instance)) = (&self.gpu, &self.instance)
                    && let Err(error) = window.render(gpu, instance, self.importer.as_deref())
                {
                    tracing::warn!(id, %error, "proxy rendering failed");
                    self.remove(id, true);
                }
            }
            _ => {}
        }
        self.check_gpu();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.check_gpu();
        let now = Instant::now();
        let mut next = None;
        for (&id, window) in &mut self.windows {
            if let Some(frames) = window.presents.report(now) {
                (self.events)(HostEvent::Presented { id, frames });
            }
            if let Some(deadline) = window.presents.deadline() {
                next = Some(next.map_or(deadline, |previous: Instant| previous.min(deadline)));
            }
            if let Some(deadline) = window.flash_until {
                if deadline <= now {
                    window.flash_until = None;
                    window.window.request_redraw();
                } else {
                    next = Some(next.map_or(deadline, |previous: Instant| previous.min(deadline)));
                }
            }
        }
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

impl Gpu {
    fn new(
        instance: &wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        proxy: EventLoopProxy<HostCommand>,
    ) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(surface),
            force_fallback_adapter: false,
            power_preference: wgpu::PowerPreference::None,
            apply_limit_buckets: false,
        }))
        .map_err(|error| error.to_string())?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("shared proxy device"),
            ..Default::default()
        }))
        .map_err(|error| error.to_string())?;
        let failed = Arc::new(AtomicBool::new(false));
        let error_flag = failed.clone();
        let error_proxy = proxy.clone();
        device.on_uncaptured_error(Arc::new(move |_error| {
            error_flag.store(true, Ordering::Release);
            let _ = error_proxy.send_event(HostCommand::Run(Box::new(|| {})));
        }));
        let lost_flag = failed.clone();
        device.set_device_lost_callback(move |_reason, _message| {
            lost_flag.store(true, Ordering::Release);
            let _ = proxy.send_event(HostCommand::Run(Box::new(|| {})));
        });
        Ok(Self {
            adapter,
            device,
            queue,
            failed,
        })
    }
}

impl ProxyWindow {
    fn resize(&mut self, gpu: &Gpu, size: PhysicalSize<u32>) -> Result<(), String> {
        self.size = size;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let limit = gpu.device.limits().max_texture_dimension_2d;
        if size.width > limit || size.height > limit {
            return Err("window exceeds GPU limits".into());
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&gpu.device, &self.config);
        self.window.request_redraw();
        Ok(())
    }

    fn render(
        &mut self,
        gpu: &Gpu,
        instance: &wgpu::Instance,
        import: Option<super::gpu::Import<'_>>,
    ) -> Result<(), String> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(());
        }
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.window.request_redraw();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(gpu, self.window.inner_size())?;
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.consecutive_surface_losses += 1;
                if self.consecutive_surface_losses > 3 {
                    return Err("surface remains lost after three recreations".into());
                }
                self.surface = instance
                    .create_surface(self.window.clone())
                    .map_err(|error| error.to_string())?;
                self.resize(gpu, self.window.inner_size())?;
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("surface validation failed".into());
            }
        };
        self.presenter
            .prepare_video(&gpu.device, &gpu.queue, import);
        self.consecutive_surface_losses = 0;
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("proxy draw"),
            });
        let normal = (2.0 * self.scale).round().max(1.0) as u32;
        let flashing = self
            .flash_until
            .is_some_and(|deadline| deadline > Instant::now());
        self.presenter.set_edge(
            self.accent,
            if flashing {
                normal.saturating_mul(4)
            } else {
                normal
            },
        );
        self.presenter
            .encode(&gpu.queue, pixel_size(self.size), &mut encoder, &view);
        gpu.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        gpu.queue.present(frame);
        self.presents.present(true);
        if suboptimal {
            self.resize(gpu, self.window.inner_size())?;
        }
        Ok(())
    }
}

const PRESENT_REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// Counts new content only after acquisition, drawing and handoff to present. The opening time
/// starts the first coalescing interval; subsequent intervals start when a report is emitted.
struct PresentCounter {
    fresh: bool,
    unreported: u32,
    last_report: Instant,
}

impl PresentCounter {
    fn new(now: Instant) -> Self {
        Self {
            fresh: false,
            unreported: 0,
            last_report: now,
        }
    }

    fn content(&mut self) {
        self.fresh = true;
    }

    fn present(&mut self, successful: bool) {
        if successful && self.fresh {
            self.unreported = self.unreported.saturating_add(1);
            self.fresh = false;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        (self.unreported > 0).then(|| self.last_report + PRESENT_REPORT_INTERVAL)
    }

    fn report(&mut self, now: Instant) -> Option<u32> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            let frames = self.unreported;
            self.unreported = 0;
            self.last_report = now;
            Some(frames)
        } else {
            None
        }
    }
}

fn pixel_size(size: PhysicalSize<u32>) -> PixelSize {
    PixelSize::new(size.width, size.height)
}

/// Whether this platform's window events track the content's geometry. macOS can say which
/// display the content is on (the native display ID) and where on it, so every geometry event
/// is reported. Elsewhere (Wayland: no window position, no display ID) a report says only
/// whether the proxy is visible, and the agent's own placement producer supplies the rest.
const TRACKS_GEOMETRY: bool = cfg!(target_os = "macos");

/// Why `HostEvent::Placed` is being considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// The proxy was just opened.
    Opened,
    /// `WindowEvent::Occluded`.
    Occlusion,
    /// `Moved`, `Resized`, `ScaleFactorChanged`, or a `SetContentSize` that resized the window.
    Geometry,
    /// The window gained focus: a late look at its minimised state.
    Focus,
}

impl Trigger {
    /// Whether the trigger produces a report on a platform that does (`tracks_geometry`) or
    /// does not track the content's geometry. Opening and occlusion always report; the rest
    /// would only repeat what a geometry-blind report already says.
    fn applies(self, tracks_geometry: bool) -> bool {
        matches!(self, Self::Opened | Self::Occlusion) || tracks_geometry
    }
}

/// One monitor, as the window system describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct MonitorSample {
    /// The platform's native display ID: the `CGDirectDisplayID` on macOS.
    id: u32,
    /// `MonitorHandle::position`: the monitor's top-left in the window system's global space,
    /// in points times the monitor's scale.
    position: PhysicalPosition<i32>,
    /// The monitor's scale factor.
    scale: f64,
}

/// What a window says about where it is, read together.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    /// `Window::inner_position`: the content's top-left in the global space, in points times the
    /// window's scale. `None` where the window system hides it (Wayland).
    inner_position: Option<PhysicalPosition<i32>>,
    inner_size: PhysicalSize<u32>,
    /// The window's scale factor.
    scale: f64,
    /// `Window::is_minimized`; `None` where the window system can't say.
    minimized: Option<bool>,
    /// The monitor the window is on, if there is one the platform can name.
    monitor: Option<MonitorSample>,
}

/// What `HostEvent::Placed` carries, apart from the window's ID.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    visible: bool,
    monitor: Option<u32>,
    origin: PointDevice,
    size: PixelSize,
}

impl Placement {
    fn event(self, id: u64) -> HostEvent {
        HostEvent::Placed {
            id,
            visible: self.visible,
            monitor: self.monitor,
            origin: self.origin,
            size: self.size,
        }
    }
}

/// The window as it is now.
fn sample_window(window: &Window) -> Sample {
    Sample {
        inner_position: window.inner_position().ok(),
        inner_size: window.inner_size(),
        scale: window.scale_factor(),
        minimized: window.is_minimized(),
        monitor: monitor_sample(window),
    }
}

#[cfg(target_os = "macos")]
fn monitor_sample(window: &Window) -> Option<MonitorSample> {
    use winit::platform::macos::MonitorHandleExtMacOS;
    let monitor = window.current_monitor()?;
    Some(MonitorSample {
        id: monitor.native_id(),
        position: monitor.position(),
        scale: monitor.scale_factor(),
    })
}

/// Wayland can't say which output a window is on in a form the agent can use.
#[cfg(not(target_os = "macos"))]
fn monitor_sample(_window: &Window) -> Option<MonitorSample> {
    None
}

/// A scale factor to compute with: the reported one, or 1 if it is not a positive finite number
/// (no report may carry a non-finite coordinate: the wire refuses it).
fn usable_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// The content's top-left on its monitor, in the monitor's device pixels. `inner` and `monitor`
/// are positions in the global point space times their own scale factors (`inner_scale`,
/// `monitor_scale`), so with equal scales (the window is on that monitor) the answer is their
/// plain difference. With different scales (an event delivered while a window crosses displays)
/// the positions are brought back to points first and the offset is converted to the monitor's
/// device pixels, instead of subtracting numbers in two different units. Negative offsets (a
/// window partly beyond its monitor's top or left edge) are reported as they are.
fn content_origin(
    inner: PhysicalPosition<i32>,
    inner_scale: f64,
    monitor: PhysicalPosition<i32>,
    monitor_scale: f64,
) -> PointDevice {
    let (inner_scale, monitor_scale) = (usable_scale(inner_scale), usable_scale(monitor_scale));
    if inner_scale == monitor_scale {
        // Exact integer arithmetic: no rounding noise to defeat deduplication.
        return PointDevice::new(
            f64::from(inner.x) - f64::from(monitor.x),
            f64::from(inner.y) - f64::from(monitor.y),
        );
    }
    let offset = |inner: i32, monitor: i32| {
        ((f64::from(inner) / inner_scale - f64::from(monitor) / monitor_scale) * monitor_scale)
            .round()
    };
    PointDevice::new(offset(inner.x, monitor.x), offset(inner.y, monitor.y))
}

/// The content's size in the monitor's device pixels, from its size in the window's. A size is
/// only ever scaled, never offset: it is the same wherever the window is. Equal scales (the
/// normal case) leave it untouched.
fn content_size(size: PhysicalSize<u32>, window_scale: f64, monitor_scale: f64) -> PixelSize {
    let (window_scale, monitor_scale) = (usable_scale(window_scale), usable_scale(monitor_scale));
    if window_scale == monitor_scale {
        return pixel_size(size);
    }
    let convert = |length: u32| (f64::from(length) / window_scale * monitor_scale).round() as u32;
    PixelSize::new(convert(size.width), convert(size.height))
}

/// Whether the proxy can be seen: not covered (`occluded`, from `WindowEvent::Occluded`) and
/// not minimised. A window system that can't say whether it is minimised counts as not.
fn visible(occluded: bool, minimized: Option<bool>) -> bool {
    !occluded && minimized != Some(true)
}

/// The report for a sample taken while the occlusion state is `occluded`. Where the platform
/// can't name the monitor or the position (Wayland), the report is `monitor: None` with a zero
/// origin and the content's own size, which the engine then treats as "can't tell".
fn placement(sample: &Sample, occluded: bool) -> Placement {
    let visible = visible(occluded, sample.minimized);
    match (sample.monitor, sample.inner_position) {
        (Some(monitor), Some(inner)) => Placement {
            visible,
            monitor: Some(monitor.id),
            origin: content_origin(inner, sample.scale, monitor.position, monitor.scale),
            size: content_size(sample.inner_size, sample.scale, monitor.scale),
        },
        _ => Placement {
            visible,
            monitor: None,
            origin: PointDevice::zero(),
            size: pixel_size(sample.inner_size),
        },
    }
}

/// Whether a new proxy starts out covered, so that it reports `visible: true` only after
/// `Occluded(false)`. On macOS a window can be opened behind other windows or on another Space,
/// and nothing but AppKit's occlusion notification says it is on screen; a proxy that wrongly
/// reported itself visible could be entered as the home window while hidden. AppKit posts
/// `windowDidChangeOcclusionState` when a window first becomes visible, which winit turns into
/// `Occluded(false)`. That is unverified [U: "AppKit posts Occluded(false) on first show"], and
/// the macOS live check settles it; if it fails, the proxy stays reported as hidden (the safe
/// side) until a later change samples `NSWindow.occlusionState` directly. Wayland keeps the
/// optimistic start: its compositor says when a surface is suspended, and the agent's own
/// producer combines the host's visibility with the compositor's geometry.
const STARTS_OCCLUDED: bool = cfg!(target_os = "macos");

/// One window's placement reporting: the occlusion state its reports are built on, and the last
/// report, so that an identical one is not sent twice. Reports are built from a sample taken when
/// they are made and the occlusion state as it is then, so an event processed before the
/// occlusion event it raced with can report a stale `visible`; the next event corrects it, and
/// the engine re-checks the placement before it commits to anything.
#[derive(Debug)]
struct PlacementTracker {
    occluded: bool,
    last: Option<Placement>,
}

impl PlacementTracker {
    fn new(occluded: bool) -> Self {
        Self {
            occluded,
            last: None,
        }
    }

    /// A tracker for a window that has just been opened.
    fn for_new_window() -> Self {
        Self::new(STARTS_OCCLUDED)
    }

    fn set_occluded(&mut self, occluded: bool) {
        self.occluded = occluded;
    }

    /// The report for `sample`, unless it equals the last one.
    fn update(&mut self, sample: &Sample) -> Option<Placement> {
        let now = placement(sample, self.occluded);
        if self.last == Some(now) {
            return None;
        }
        self.last = Some(now);
        Some(now)
    }
}

/// A window event saying the window's geometry changed, as winit delivers it.
#[derive(Clone, Copy, Debug)]
enum Change {
    /// `WindowEvent::Resized`: the size when the event was queued.
    Resized(PhysicalSize<u32>),
    /// `WindowEvent::ScaleFactorChanged`: the scale when the event was queued.
    ScaleFactor(f64),
}

/// What a geometry event reports to the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Report {
    size: PhysicalSize<u32>,
    scale: f64,
    /// The event's own payload disagreed with the window as it is now.
    stale: bool,
}

/// The window's actual content size and scale, sampled together.
fn sample(window: &Window) -> (PhysicalSize<u32>, f64) {
    (window.inner_size(), window.scale_factor())
}

/// What a geometry event reports: the window's `actual` size and scale, sampled together when
/// the event is handled. The event's own payload only says that something changed; it may be
/// older than later native changes, and reporting it would tell the engine the user resized the
/// proxy to a size it no longer has.
fn reported(change: Change, actual: (PhysicalSize<u32>, f64)) -> Report {
    let stale = match change {
        Change::Resized(size) => size != actual.0,
        Change::ScaleFactor(scale) => scale != actual.1,
    };
    Report {
        size: actual.0,
        scale: actual.1,
        stale,
    }
}

/// The size to request for a `SetContentSize`: the exact physical content size. Only the opening
/// size is fitted to the screen (`fit`); the size the source reports back is applied as it is.
fn content_request(size: PixelSize) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width, size.height)
}
fn logical_size(size: PixelSize, scale: f64) -> LogicalSize<f64> {
    LogicalSize::new(
        f64::from(size.width) / scale,
        f64::from(size.height) / scale,
    )
}
/// A monitor's size in logical units.
fn screen(monitor: Option<&MonitorHandle>) -> Option<LogicalSize<f64>> {
    monitor.map(|monitor| monitor.size().to_logical(monitor.scale_factor()))
}

/// `size`, scaled down (keeping its shape) to fit comfortably on `screen`. Proxies start at the
/// source window's size, and a window from an ultrawide monitor would otherwise open wider than a
/// laptop's display.
fn fit(size: LogicalSize<f64>, screen: Option<LogicalSize<f64>>) -> LogicalSize<f64> {
    let Some(screen) = screen.filter(|s| s.width > 0.0 && s.height > 0.0) else {
        return size;
    };
    let shrink = (screen.width * 0.9 / size.width)
        .min(screen.height * 0.8 / size.height)
        .min(1.0);
    LogicalSize::new(
        (size.width * shrink).floor().max(1.0),
        (size.height * shrink).floor().max(1.0),
    )
}

fn window_scale(windows: &HashMap<u64, ProxyWindow>, id: u64) -> f64 {
    windows
        .get(&id)
        .map_or(1.0, |window| window.window.scale_factor())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_then_present_counts_one() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        counter.content();
        counter.present(true);
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), Some(1));
    }

    #[test]
    fn present_without_new_content_counts_nothing() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        counter.content();
        counter.present(true);
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), Some(1));
        // Resize, edge flash and cursor changes don't mark content as fresh.
        for _ in 0..10 {
            counter.present(true);
        }
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL * 2), None);
        assert_eq!(counter.deadline(), None);
    }

    #[test]
    fn several_contents_before_one_present_count_one() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        // Frame, Video and VideoNative all use the same content marker.
        for _ in 0..3 {
            counter.content();
        }
        counter.present(true);
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), Some(1));
    }

    #[test]
    fn refused_acquisitions_count_nothing_and_keep_content_fresh() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        counter.content();
        // Occluded, timeout, outdated and lost acquisitions leave the content pending.
        for _ in 0..4 {
            counter.present(false);
        }
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), None);
        assert_eq!(counter.deadline(), None);
        counter.present(true);
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), Some(1));
    }

    #[test]
    fn presents_are_coalesced_at_most_once_per_interval() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        for elapsed in 0..250 {
            counter.content();
            counter.present(true);
            assert_eq!(counter.report(now + Duration::from_millis(elapsed)), None);
        }
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL), Some(250));
        counter.content();
        counter.present(true);
        assert_eq!(counter.report(now + Duration::from_millis(499)), None);
        assert_eq!(counter.report(now + PRESENT_REPORT_INTERVAL * 2), Some(1));
    }

    #[test]
    fn pending_counts_flush_at_the_deadline_without_more_presents() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        assert_eq!(counter.deadline(), None);
        counter.content();
        counter.present(true);
        let deadline = counter.deadline().expect("pending report deadline");
        assert_eq!(deadline, now + PRESENT_REPORT_INTERVAL);
        assert_eq!(counter.report(deadline - Duration::from_nanos(1)), None);
        assert_eq!(counter.report(deadline), Some(1));
        assert_eq!(counter.deadline(), None);
    }

    #[test]
    fn reports_never_contain_zero_frames() {
        let now = Instant::now();
        let mut counter = PresentCounter::new(now);
        for elapsed in 0..=1000 {
            counter.present(true);
            assert_eq!(counter.report(now + Duration::from_millis(elapsed)), None);
        }
        counter.content();
        counter.present(true);
        assert_eq!(counter.report(now + Duration::from_secs(1)), Some(1));
        for elapsed in 1000..=2000 {
            assert_eq!(counter.report(now + Duration::from_millis(elapsed)), None);
        }
    }

    #[test]
    fn geometry_events_report_the_sampled_size_and_scale_not_their_payload() {
        let report = |size: (u32, u32), scale| Report {
            size: PhysicalSize::new(size.0, size.1),
            scale,
            stale: false,
        };
        let actual = (PhysicalSize::new(800, 600), 2.0);
        // A stale snapshot A (the window has since become B): B is reported, never A.
        assert_eq!(
            reported(Change::Resized(PhysicalSize::new(700, 500)), actual),
            Report {
                stale: true,
                ..report((800, 600), 2.0)
            }
        );
        // A current one is reported as it is.
        assert_eq!(
            reported(Change::Resized(PhysicalSize::new(800, 600)), actual),
            report((800, 600), 2.0)
        );
        // A deferred scale change: the size and scale come from the same sample, so the size
        // that went with the new scale is reported with it (not the queued scale alone).
        let moved = (PhysicalSize::new(1600, 1200), 2.0);
        assert_eq!(
            reported(Change::ScaleFactor(1.0), moved),
            Report {
                stale: true,
                ..report((1600, 1200), 2.0)
            }
        );
        assert_eq!(
            reported(Change::ScaleFactor(2.0), moved),
            report((1600, 1200), 2.0)
        );
    }

    #[test]
    fn content_size_requests_are_exact_physical_sizes() {
        // Odd sizes: 777x433 at scale 2 is 388.5x216.5 logical, which `fit` floors to 388x216
        // (776x432 physical), a size that is not the one the source reported.
        let laptop = Some(LogicalSize::new(1512.0, 982.0));
        assert_eq!(
            fit(logical_size(PixelSize::new(777, 433), 2.0), laptop),
            LogicalSize::new(388.0, 216.0)
        );
        assert_eq!(
            content_request(PixelSize::new(777, 433)),
            PhysicalSize::new(777, 433)
        );
        // Beyond the opening fit: on a 1512x982 logical screen at scale 2 it would cap 2800x1600
        // physical to 2720x1554. The request is neither capped nor reshaped.
        let size = PixelSize::new(2800, 1600);
        assert_eq!(
            fit(logical_size(size, 2.0), laptop),
            LogicalSize::new(1360.0, 777.0)
        );
        assert_eq!(content_request(size), PhysicalSize::new(2800, 1600));
    }

    fn pos(x: i32, y: i32) -> PhysicalPosition<i32> {
        PhysicalPosition::new(x, y)
    }

    /// A macOS-like sample: content at `inner` on monitor 7, whose top-left is `monitor`, all
    /// at `scale`.
    fn mac_sample(inner: (i32, i32), monitor: (i32, i32), scale: f64) -> Sample {
        Sample {
            inner_position: Some(pos(inner.0, inner.1)),
            inner_size: PhysicalSize::new(800, 600),
            scale,
            minimized: Some(false),
            monitor: Some(MonitorSample {
                id: 7,
                position: pos(monitor.0, monitor.1),
                scale,
            }),
        }
    }

    /// A Wayland-like sample: no window position, no nameable monitor, can't tell if minimised.
    fn wayland_sample(size: (u32, u32)) -> Sample {
        Sample {
            inner_position: None,
            inner_size: PhysicalSize::new(size.0, size.1),
            scale: 1.0,
            minimized: None,
            monitor: None,
        }
    }

    #[test]
    fn content_origin_is_the_offset_from_the_monitor_in_device_pixels() {
        let origin = |inner, monitor, scale| content_origin(inner, scale, monitor, scale);
        // The main display sits at the origin: the content's position is its origin.
        assert_eq!(
            origin(pos(400, 300), pos(0, 0), 2.0),
            PointDevice::new(400.0, 300.0)
        );
        // A display to the right of a 1512-point main display (both at scale 2): its position is
        // 3024 device pixels, and content 100 points into it is 200 device pixels in.
        assert_eq!(
            origin(pos(3224, 200), pos(3024, 0), 2.0),
            PointDevice::new(200.0, 200.0)
        );
        // A display above and to the left (negative global coordinates), scale 1.
        assert_eq!(
            origin(pos(-1800, -100), pos(-1920, -200), 1.0),
            PointDevice::new(120.0, 100.0)
        );
        // Content hanging off its monitor's top-left corner: reported as it is, not clamped.
        assert_eq!(
            origin(pos(-20, 10), pos(0, 0), 2.0),
            PointDevice::new(-20.0, 10.0)
        );
        // A fractional scale gives the same exact integer difference (no round trip through points).
        assert_eq!(
            origin(pos(1601, 333), pos(1000, 111), 1.5),
            PointDevice::new(601.0, 222.0)
        );
    }

    #[test]
    fn content_origin_goes_through_points_when_the_scales_differ() {
        // The window still says scale 2 and the monitor scale 1 (an event delivered while the
        // window crosses displays): content at 1700 x 150 points, monitor at 1512 x 0 points.
        assert_eq!(
            content_origin(pos(3400, 300), 2.0, pos(1512, 0), 1.0),
            PointDevice::new(188.0, 150.0)
        );
        // The other way: the monitor's device pixels are twice the window's.
        assert_eq!(
            content_origin(pos(1700, 150), 1.0, pos(3024, 0), 2.0),
            PointDevice::new(376.0, 300.0)
        );
        // Whatever the scales, the answer is whole device pixels.
        let origin = content_origin(pos(1601, 333), 1.5, pos(3000, 111), 2.0);
        assert_eq!((origin.x.fract(), origin.y.fract()), (0.0, 0.0));
    }

    #[test]
    fn unusable_scales_never_make_a_non_finite_origin() {
        for scale in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -2.0] {
            assert_eq!(usable_scale(scale), 1.0);
            let origin = content_origin(pos(100, 50), scale, pos(0, 0), scale);
            assert_eq!(origin, PointDevice::new(100.0, 50.0));
            let mixed = content_origin(pos(100, 50), scale, pos(0, 0), 2.0);
            assert!(mixed.x.is_finite() && mixed.y.is_finite());
            assert_eq!(
                content_size(PhysicalSize::new(10, 20), scale, scale),
                PixelSize::new(10, 20)
            );
        }
        assert_eq!(usable_scale(1.5), 1.5);
    }

    #[test]
    fn content_size_is_only_scaled_never_offset() {
        let size = PhysicalSize::new(777, 433);
        // Equal scales (the normal case): the window's own size, untouched.
        assert_eq!(content_size(size, 2.0, 2.0), PixelSize::new(777, 433));
        assert_eq!(content_size(size, 1.5, 1.5), PixelSize::new(777, 433));
        // Different scales: converted by the ratio, rounding half up.
        assert_eq!(content_size(size, 2.0, 1.0), PixelSize::new(389, 217));
        assert_eq!(
            content_size(PhysicalSize::new(1600, 1200), 2.0, 1.0),
            PixelSize::new(800, 600)
        );
        assert_eq!(
            content_size(PhysicalSize::new(801, 601), 1.0, 2.0),
            PixelSize::new(1602, 1202)
        );
        // Nothing to convert.
        assert_eq!(
            content_size(PhysicalSize::new(0, 0), 2.0, 1.0),
            PixelSize::new(0, 0)
        );
        // Where the window is has no say in how big it is.
        let near = placement(&mac_sample((10, 10), (0, 0), 2.0), false);
        let far = placement(&mac_sample((9000, 4000), (3024, 0), 2.0), false);
        assert_eq!(near.size, far.size);
        assert_eq!(near.size, PixelSize::new(800, 600));
    }

    #[test]
    fn visible_means_neither_occluded_nor_minimised() {
        assert!(visible(false, None));
        assert!(visible(false, Some(false)));
        assert!(!visible(false, Some(true)));
        assert!(!visible(true, None));
        assert!(!visible(true, Some(false)));
        assert!(!visible(true, Some(true)));
    }

    #[test]
    fn a_placement_names_the_monitor_and_the_offset_into_it() {
        let placed = placement(&mac_sample((3224, 200), (3024, 0), 2.0), false);
        assert_eq!(
            placed,
            Placement {
                visible: true,
                monitor: Some(7),
                origin: PointDevice::new(200.0, 200.0),
                size: PixelSize::new(800, 600),
            }
        );
        // The same window, minimised: still its last place, but not visible.
        let mut minimised = mac_sample((3224, 200), (3024, 0), 2.0);
        minimised.minimized = Some(true);
        assert_eq!(
            placement(&minimised, false),
            Placement {
                visible: false,
                ..placed
            }
        );
        // The same window, occluded.
        assert_eq!(
            placement(&mac_sample((3224, 200), (3024, 0), 2.0), true),
            Placement {
                visible: false,
                ..placed
            }
        );
    }

    #[test]
    fn a_host_that_cannot_name_the_monitor_says_it_cannot_tell() {
        let none = Placement {
            visible: true,
            monitor: None,
            origin: PointDevice::zero(),
            size: PixelSize::new(640, 480),
        };
        // Wayland: no window position, no monitor ID.
        assert_eq!(placement(&wayland_sample((640, 480)), false), none);
        // Occluded there (xdg_toplevel suspended): not visible, still nothing to say about where.
        assert_eq!(
            placement(&wayland_sample((640, 480)), true),
            Placement {
                visible: false,
                ..none
            }
        );
        // A monitor without a window position, or a position without a monitor, names no display.
        let mut sample = mac_sample((3224, 200), (3024, 0), 2.0);
        sample.inner_position = None;
        assert_eq!(
            placement(&sample, false),
            Placement {
                size: PixelSize::new(800, 600),
                ..none
            }
        );
        let mut sample = mac_sample((3224, 200), (3024, 0), 2.0);
        sample.monitor = None;
        assert_eq!(
            placement(&sample, false),
            Placement {
                size: PixelSize::new(800, 600),
                ..none
            }
        );
    }

    #[test]
    fn identical_reports_are_not_repeated() {
        let mut tracker = PlacementTracker::new(false);
        let base = mac_sample((3224, 200), (3024, 0), 2.0);
        assert!(tracker.update(&base).is_some(), "the first report is sent");
        assert_eq!(tracker.update(&base), None);
        assert_eq!(tracker.update(&base), None);
        // Each field of the report, changed alone, is a new report; sent once.
        let mut moved = base;
        moved.inner_position = Some(pos(3225, 200));
        let mut resized = base;
        resized.inner_size = PhysicalSize::new(801, 600);
        let mut on_another_display = base;
        if let Some(monitor) = &mut on_another_display.monitor {
            monitor.id = 8;
        }
        let mut minimised = base;
        minimised.minimized = Some(true);
        for changed in [moved, resized, on_another_display, minimised, base] {
            assert!(tracker.update(&changed).is_some(), "{changed:?}");
            assert_eq!(tracker.update(&changed), None, "{changed:?}");
        }
        // A sample that differs but makes the same report is not a new one: the monitor moving
        // along with the window leaves the content's place on it as it was.
        let shifted = mac_sample((6224, 200), (6024, 0), 2.0);
        assert_eq!(tracker.update(&shifted), None);
    }

    #[test]
    fn visibility_follows_occlusion_and_minimisation() {
        let visible_now = |tracker: &mut PlacementTracker, sample: &Sample| {
            tracker.update(sample).map(|placed| placed.visible)
        };
        // A tracker that starts uncovered (Wayland's start): visible until told otherwise.
        let mut tracker = PlacementTracker::new(false);
        let mut window = mac_sample((3224, 200), (3024, 0), 2.0);
        assert_eq!(visible_now(&mut tracker, &window), Some(true));

        // Minimised: AppKit says occluded, and the window says it is minimised.
        tracker.set_occluded(true);
        window.minimized = Some(true);
        assert_eq!(visible_now(&mut tracker, &window), Some(false));
        // Another occlusion event with nothing changed: nothing to say.
        tracker.set_occluded(true);
        assert_eq!(visible_now(&mut tracker, &window), None);

        // Restored: AppKit's occlusion event can arrive while the window still says it is
        // minimised. That stays "not visible"; the focus that follows it settles the state.
        tracker.set_occluded(false);
        assert_eq!(visible_now(&mut tracker, &window), None);
        window.minimized = Some(false);
        assert_eq!(visible_now(&mut tracker, &window), Some(true));

        // Covered by another window, then uncovered.
        tracker.set_occluded(true);
        assert_eq!(visible_now(&mut tracker, &window), Some(false));
        // Moved while covered: the new place is reported, and the proxy is still not visible.
        let moved = mac_sample((3300, 260), (3024, 0), 2.0);
        assert_eq!(
            tracker.update(&moved),
            Some(Placement {
                visible: false,
                monitor: Some(7),
                origin: PointDevice::new(276.0, 260.0),
                size: PixelSize::new(800, 600),
            })
        );
        tracker.set_occluded(false);
        assert_eq!(visible_now(&mut tracker, &moved), Some(true));
    }

    #[test]
    fn a_proxy_opened_while_covered_is_hidden_until_it_is_uncovered() {
        // Opened fully covered (behind other windows, or on another Space): AppKit has said
        // nothing yet. Nothing the window does before `Occluded(false)` may report it visible.
        let mut tracker = PlacementTracker::new(true);
        let window = mac_sample((3224, 200), (3024, 0), 2.0);
        let mut before_uncovered = vec![tracker.update(&window)];
        let mut moved = window;
        moved.inner_position = Some(pos(3300, 260));
        let mut resized = moved;
        resized.inner_size = PhysicalSize::new(900, 700);
        for sample in [moved, resized, resized, window] {
            before_uncovered.push(tracker.update(&sample));
        }
        // The late look at the minimised state that a focus gain makes changes nothing.
        before_uncovered.push(tracker.update(&window));
        let reports: Vec<_> = before_uncovered.into_iter().flatten().collect();
        assert_eq!(reports.len(), 4, "open, move, resize, back: {reports:?}");
        assert!(
            reports.iter().all(|placed| !placed.visible),
            "visible before Occluded(false): {reports:?}"
        );
        // The first report still says where it is: it is only its visibility that waits.
        assert_eq!(reports[0].monitor, Some(7));
        assert_eq!(reports[0].origin, PointDevice::new(200.0, 200.0));
        // `Occluded(false)`: now it is visible, at the place it is now.
        tracker.set_occluded(false);
        assert_eq!(
            tracker.update(&window).map(|placed| placed.visible),
            Some(true)
        );
    }

    #[test]
    fn a_new_proxy_starts_covered_on_macos_and_uncovered_elsewhere() {
        let first = PlacementTracker::for_new_window()
            .update(&mac_sample((3224, 200), (3024, 0), 2.0))
            .map(|placed| placed.visible);
        assert_eq!(first, Some(!cfg!(target_os = "macos")));
    }

    #[test]
    fn geometry_after_an_occlusion_event_keeps_the_proxy_hidden() {
        let mut tracker = PlacementTracker::new(false);
        let window = mac_sample((3224, 200), (3024, 0), 2.0);
        assert_eq!(
            tracker.update(&window).map(|placed| placed.visible),
            Some(true)
        );
        // Covered, then a move and a resize are processed (their events were queued after the
        // occlusion event): each reports its new geometry, and none says visible.
        tracker.set_occluded(true);
        assert_eq!(
            tracker.update(&window).map(|placed| placed.visible),
            Some(false)
        );
        let mut moved = window;
        moved.inner_position = Some(pos(3400, 300));
        let mut resized = moved;
        resized.inner_size = PhysicalSize::new(1000, 800);
        assert_eq!(
            tracker.update(&moved),
            Some(Placement {
                visible: false,
                monitor: Some(7),
                origin: PointDevice::new(376.0, 300.0),
                size: PixelSize::new(800, 600),
            })
        );
        assert_eq!(
            tracker.update(&resized),
            Some(Placement {
                visible: false,
                monitor: Some(7),
                origin: PointDevice::new(376.0, 300.0),
                size: PixelSize::new(1000, 800),
            })
        );
    }

    #[test]
    fn a_resize_processed_before_the_occlusion_event_is_corrected_by_it() {
        // The accepted ordering race: the resize is handled while the window still counts as
        // uncovered, so that report says visible; the occlusion event that follows corrects it.
        let mut tracker = PlacementTracker::new(false);
        let window = mac_sample((3224, 200), (3024, 0), 2.0);
        assert!(tracker.update(&window).is_some());
        let mut resized = window;
        resized.inner_size = PhysicalSize::new(900, 700);
        assert_eq!(
            tracker.update(&resized).map(|placed| placed.visible),
            Some(true)
        );
        tracker.set_occluded(true);
        assert_eq!(
            tracker.update(&resized),
            Some(Placement {
                visible: false,
                monitor: Some(7),
                origin: PointDevice::new(200.0, 200.0),
                size: PixelSize::new(900, 700),
            })
        );
    }

    #[test]
    fn a_host_without_geometry_reports_only_at_open_and_on_occlusion() {
        let all = [
            Trigger::Opened,
            Trigger::Occlusion,
            Trigger::Geometry,
            Trigger::Focus,
        ];
        // macOS: everything that can change the report is a reason to look.
        assert!(all.iter().all(|trigger| trigger.applies(true)));
        // Wayland: once after `Opened` and on `Occluded`; moves, resizes, scale changes and
        // focus are the agent's own producer's business.
        let wayland: Vec<_> = all
            .iter()
            .map(|trigger| (trigger, trigger.applies(false)))
            .collect();
        assert_eq!(
            wayland,
            [
                (&Trigger::Opened, true),
                (&Trigger::Occlusion, true),
                (&Trigger::Geometry, false),
                (&Trigger::Focus, false),
            ]
        );
    }

    #[test]
    fn a_placement_becomes_the_host_event_for_its_window() {
        let placed = Placement {
            visible: false,
            monitor: Some(69_733_248),
            origin: PointDevice::new(12.0, -34.0),
            size: PixelSize::new(1280, 720),
        };
        assert_eq!(
            placed.event(26),
            HostEvent::Placed {
                id: 26,
                visible: false,
                monitor: Some(69_733_248),
                origin: PointDevice::new(12.0, -34.0),
                size: PixelSize::new(1280, 720),
            }
        );
    }

    #[test]
    fn proxies_fit_the_screen() {
        let laptop = Some(LogicalSize::new(1512.0, 982.0));
        // An ultrawide source window: scaled down, same shape.
        let fitted = fit(LogicalSize::new(1716.0, 349.0), laptop);
        assert_eq!(fitted, LogicalSize::new(1360.0, 276.0));
        // Small enough already, or no screen known: unchanged.
        assert_eq!(
            fit(LogicalSize::new(320.0, 240.0), laptop),
            LogicalSize::new(320.0, 240.0)
        );
        assert_eq!(
            fit(LogicalSize::new(4000.0, 3000.0), None),
            LogicalSize::new(4000.0, 3000.0)
        );
        // Too tall: limited by height.
        assert_eq!(
            fit(LogicalSize::new(500.0, 2000.0), laptop),
            LogicalSize::new(196.0, 785.0)
        );
    }
}
