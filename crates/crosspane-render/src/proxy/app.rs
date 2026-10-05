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
    window::{CursorIcon, CustomCursor, Fullscreen, Window, WindowId},
};

#[cfg(any(target_os = "windows", test))]
use super::HostMonitorMapping;
#[cfg(target_os = "windows")]
use super::HostPlacementMapping;
use super::{
    HostCommand, HostEvent, HostPlace, PictureImporter, cursor,
    gpu::{Presenter, surface_format},
    input::{InputState, install_arm, mouse_button, scroll},
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
    pending_arms: HashMap<u64, (Instant, std::sync::mpsc::SyncSender<bool>, bool)>,
    importer: Option<PictureImporter>,
    #[cfg(target_os = "windows")]
    placement_mapping: Option<HostPlacementMapping>,
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
    #[cfg(target_os = "macos")]
    fullscreen_poll: Option<FullscreenPoll>,
    flash_until: Option<Instant>,
    input: InputState,
    consecutive_surface_losses: u8,
    presents: PresentCounter,
    /// What `HostEvent::Placed` last said about this window, and the occlusion it is built on.
    placement: PlacementTracker,
    #[cfg(target_os = "windows")]
    dpi_correction: Option<DpiCorrection>,
    /// Latest confirmed geometry or newer source request, independent of the presenter size.
    #[cfg(target_os = "windows")]
    dpi_baseline: (PhysicalSize<u32>, f64),
}

impl App {
    pub(super) fn new(
        proxy: EventLoopProxy<HostCommand>,
        events: Box<dyn FnMut(HostEvent)>,
        importer: Option<PictureImporter>,
        #[cfg(target_os = "windows")] placement_mapping: Option<HostPlacementMapping>,
    ) -> Self {
        Self {
            importer,
            #[cfg(target_os = "windows")]
            placement_mapping,
            proxy,
            events,
            instance: None,
            gpu: None,
            windows: HashMap::new(),
            ids: HashMap::new(),
            pending: VecDeque::new(),
            pending_arms: HashMap::new(),
        }
    }

    fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: u64,
        title: String,
        size: PixelSize,
        accent: [u8; 3],
        place: Option<HostPlace>,
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
        #[cfg(target_os = "windows")]
        let mapped = place.and_then(|place| {
            let rows = self.placement_mapping.as_ref().and_then(|read| read());
            let count = rows.as_ref().map_or(0, Vec::len);
            let mapped = rows
                .as_deref()
                .and_then(|rows| map_logical(rows, place.content))
                .and_then(|(row, content)| {
                    use winit::platform::windows::MonitorHandleExtWindows;
                    event_loop
                        .available_monitors()
                        .find(|monitor| {
                            monitor.native_id() == row.native_id
                                && native_bounds_match(row, monitor)
                        })
                        .map(|monitor| (monitor, content))
                });
            if mapped.is_none() {
                tracing::warn!(
                    monitors = count,
                    degraded = 1,
                    "proxy placement mapping unavailable; using OS placement"
                );
            }
            mapped
        });
        #[cfg(target_os = "windows")]
        let monitor = mapped
            .as_ref()
            .map(|(monitor, _)| monitor.clone())
            .or(monitor);
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
        #[cfg(target_os = "windows")]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            // The frozen Open describes an independent, decorated toplevel, with no owner
            // or popup relation. Winit supplies the normal resizable overlapped style.
            let attributes = attributes
                .with_class_name("CrosspaneProxy")
                .with_skip_taskbar(false)
                .with_drag_and_drop(false);
            if let Some((_, content)) = &mapped {
                attributes.with_position(*content)
            } else {
                attributes
            }
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
        // Wayland ignores placement: the destination agent floats/moves it by IPC (WP-2.58).
        #[cfg(target_os = "macos")]
        let scale = if let Some(place) = place {
            place_content(
                place.content,
                || {
                    let scale = window.scale_factor();
                    Ok::<_, winit::error::NotSupportedError>((
                        window.outer_position()?.to_logical(scale),
                        window.inner_position()?.to_logical(scale),
                        scale,
                    ))
                },
                |outer| window.set_outer_position(outer),
            )
            .map_err(|error| error.to_string())?;
            window.scale_factor()
        } else {
            scale
        };
        #[cfg(not(target_os = "macos"))]
        let _ = place;
        #[cfg(target_os = "windows")]
        if let Some((_, desired)) = mapped {
            let physical_content = window.inner_size();
            if !place_physical_content(&window, desired) {
                tracing::warn!(degraded = 1, "proxy placement could not be verified");
            }
            // Creation/moving can dispatch DPI events before this HWND has an ID in App.
            // Preserve its fitted physical viewport before publishing Opened/Placed.
            if window.inner_size() != physical_content {
                let _ = window.request_inner_size(physical_content);
            }
        }
        #[cfg(target_os = "windows")]
        let scale = window.scale_factor();
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
            #[cfg(target_os = "macos")]
            fullscreen_poll: None,
            flash_until: None,
            input: InputState::default(),
            consecutive_surface_losses: 0,
            presents: PresentCounter::new(Instant::now()),
            placement: PlacementTracker::for_new_window(),
            #[cfg(target_os = "windows")]
            dpi_correction: None,
            #[cfg(target_os = "windows")]
            dpi_baseline: (actual_size, scale),
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
        #[cfg(target_os = "windows")]
        if trigger == Trigger::Geometry && window.dpi_correction.is_some() {
            return;
        }
        #[cfg(target_os = "windows")]
        let rows = self.placement_mapping.as_ref().and_then(|read| read());
        let sample = sample_window(&window.window);
        #[cfg(target_os = "windows")]
        let placed = {
            use winit::platform::windows::MonitorHandleExtWindows;
            let monitor = window.window.current_monitor();
            let row = rows
                .as_deref()
                .filter(|rows| valid_mapping(rows))
                .and_then(|rows| {
                    monitor.as_ref().and_then(|monitor| {
                        rows.iter().find(|row| {
                            row.native_id == monitor.native_id()
                                && native_bounds_match(row, monitor)
                        })
                    })
                });
            let mut placed = placement(&sample, window.placement.occluded);
            if let (Some(row), Some(inner)) = (row, sample.inner_position)
                && window.window.current_monitor().map(|m| m.native_id())
                    == monitor.as_ref().map(|m| m.native_id())
            {
                placed.monitor = Some(row.id);
                placed.origin = PointDevice::new(
                    f64::from(inner.x) - f64::from(row.physical_origin.x),
                    f64::from(inner.y) - f64::from(row.physical_origin.y),
                );
            }
            if window.placement.last == Some(placed) {
                None
            } else {
                window.placement.last = Some(placed);
                Some(placed)
            }
        };
        #[cfg(not(target_os = "windows"))]
        let placed = window.placement.update(&sample);
        if let Some(placed) = placed {
            tracing::debug!(id, ?placed, ?trigger, "proxy placement");
            #[cfg(target_os = "macos")]
            let window_number = mac_window_number(&window.window);
            #[cfg(not(target_os = "macos"))]
            let window_number = None;
            (self.events)(placed.event(id, window_number));
        }
    }

    fn remove(&mut self, id: u64, lost: bool) {
        self.cancel_pending_arm(id);
        if let Some(mut window) = self.windows.remove(&id) {
            self.ids.remove(&window.window.id());
            window.input.close();
            window.input.release(id, self.events.as_mut());
            if lost {
                (self.events)(HostEvent::Lost { id });
            }
        }
    }

    fn cancel_pending_arm(&mut self, id: u64) {
        if let Some((until, done, _)) = self.pending_arms.remove(&id) {
            install_arm(None, until, done);
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
            HostCommand::Arm { id, until, done } => {
                self.cancel_pending_arm(id);
                if let Some(window) = self.windows.get_mut(&id) {
                    window.input.disarm();
                }
                // Winit delivers commands before buffered Wayland input. Install only after
                // that input drains, so an earlier click cannot consume this new arm.
                self.pending_arms
                    .insert(id, (until, done, self.windows.contains_key(&id)));
            }
            HostCommand::Disarm { id } => {
                self.cancel_pending_arm(id);
                if let Some(window) = self.windows.get_mut(&id) {
                    window.input.disarm();
                }
            }
            HostCommand::Open {
                id,
                title,
                size,
                accent,
                place,
            } => {
                if let Err(error) = self.open(event_loop, id, title, size, accent, place) {
                    (self.events)(HostEvent::OpenFailed { id, error });
                }
            }
            HostCommand::Close { id } => self.remove(id, false),
            HostCommand::SetTitle { id, title } => {
                if let Some(window) = self.windows.get(&id) {
                    window.window.set_title(&title);
                }
            }
            HostCommand::SetFullscreen { id, fullscreen } => {
                #[cfg(target_os = "windows")]
                if let Some(window) = self.windows.get_mut(&id) {
                    window.dpi_correction = None;
                }
                if let Some(window) = self.windows.get(&id) {
                    #[cfg(target_os = "windows")]
                    window
                        .window
                        .set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)));
                    #[cfg(not(target_os = "windows"))]
                    window
                        .window
                        .set_fullscreen(fullscreen.then(|| Fullscreen::Borderless(None)));
                }
            }
            HostCommand::SetContentSize { id, size } => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                if let Some(window) = self.windows.get_mut(&id) {
                    #[cfg(target_os = "windows")]
                    {
                        window.dpi_correction = None;
                    }
                    // The source's size is exact: no opening fit and no logical rounding, or the
                    // window system's answer would differ from it and be sent back as a resize.
                    let Some(request) = content_request(size, window.window.fullscreen().is_some())
                    else {
                        tracing::debug!(id, "content resize dropped while proxy is fullscreen");
                        return;
                    };
                    #[cfg(target_os = "windows")]
                    {
                        // Windows requests return None even when native resizing is synchronous.
                        // Supersede buffered DPI callbacks before their Resized event arrives.
                        window.dpi_baseline = (request, window.window.scale_factor());
                    }
                    let result = window.window.request_inner_size(request);
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
                for (_, (until, done, _)) in self.pending_arms.drain() {
                    install_arm(None, until, done);
                }
                self.pending.clear();
                event_loop.exit();
            }
            HostCommand::Run(function) => function(),
        }
        self.check_gpu();
    }

    fn geometry_changed(&mut self, id: u64, report: Report) {
        #[cfg(target_os = "windows")]
        if let Some(window) = self.windows.get_mut(&id)
            && let Some(correction) = &window.dpi_correction
        {
            if !correction.done(report.size, Instant::now()) {
                return;
            }
            window.dpi_correction = None;
        }
        if report.stale {
            tracing::debug!(id, size = ?report.size, scale = report.scale, "window event payload was stale");
        }
        self.resized(id, report.size, report.scale);
    }

    fn resized(&mut self, id: u64, size: PhysicalSize<u32>, scale: f64) {
        let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) else {
            return;
        };
        window.scale = scale;
        #[cfg(target_os = "windows")]
        {
            window.dpi_baseline = (size, scale);
        }
        if let Err(error) = window.resize(gpu, size) {
            tracing::warn!(id, %error, "proxy resize failed");
            self.remove(id, true);
            return;
        }
        window.observe_fullscreen(id, Some((size, scale)), self.events.as_mut());
        // After `Resized`, so the engine has the new size before the placement that carries it.
        self.report_placement(id, Trigger::Geometry);
    }
}

/// Physical frame/content readings become global logical coordinates at the current scale.
#[cfg(any(target_os = "macos", test))]
type ContentGeometry = (
    winit::dpi::LogicalPosition<f64>,
    winit::dpi::LogicalPosition<f64>,
    f64,
);

#[cfg(any(target_os = "macos", test))]
fn place_content<E>(
    desired: winit::dpi::LogicalPosition<f64>,
    mut geometry: impl FnMut() -> Result<ContentGeometry, E>,
    mut set_outer: impl FnMut(winit::dpi::LogicalPosition<f64>),
) -> Result<(), E> {
    let (outer, inner, _) = geometry()?;
    set_outer(winit::dpi::LogicalPosition::new(
        desired.x + outer.x - inner.x,
        desired.y + outer.y - inner.y,
    ));
    let (outer, inner, scale) = geometry()?;
    if (desired.x - inner.x).abs() * scale > 1.0 || (desired.y - inner.y).abs() * scale > 1.0 {
        set_outer(winit::dpi::LogicalPosition::new(
            outer.x + desired.x - inner.x,
            outer.y + desired.y - inner.y,
        ));
    }
    Ok(())
}

fn set_cursor(
    event_loop: &ActiveEventLoop,
    window: &Window,
    size: PixelSize,
    hotspot: (u32, u32),
    pixels: &[u8],
) {
    #[cfg(target_os = "windows")]
    let scale = 1.0;
    #[cfg(not(target_os = "windows"))]
    let scale = window.scale_factor();
    // Windows CreateIconIndirect uses the supplied bitmap's physical pixels directly.
    // Downsampling it by the window's DPI would make the source cursor too small.
    match cursor::shape(size.width, size.height, hotspot, pixels, scale) {
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
            self.instance = Some(wgpu::Instance::new(proxy_instance_descriptor(
                wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
                    event_loop.owned_display_handle(),
                )),
            )));
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
        if matches!(
            &event,
            WindowEvent::Moved(_) | WindowEvent::Focused(_) | WindowEvent::Occluded(_)
        ) {
            window.observe_fullscreen(id, None, self.events.as_mut());
        }
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
                if window.input.drag_button(button, state, Instant::now(), || {
                    window.window.drag_window()
                }) {
                    return;
                }
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
                #[cfg(target_os = "macos")]
                {
                    FullscreenPoll::after_resize(&mut window.fullscreen_poll, Instant::now());
                }
                let report = reported(Change::Resized(size), sample(&window.window));
                self.geometry_changed(id, report);
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                mut inner_size_writer,
            } => {
                #[cfg(target_os = "windows")]
                {
                    let size = window.window.inner_size();
                    let (previous, old_scale) = window
                        .dpi_correction
                        .as_ref()
                        .map_or(window.dpi_baseline, |correction| {
                            (correction.size, correction.scale)
                        });
                    let current_scale = window.window.scale_factor();
                    // Buffered older generations must not overwrite the latest native DPI.
                    // Fence their intervening Resized events until the current callback arrives,
                    // preserving the old physical baseline and its scale without any request.
                    if scale_factor != current_scale {
                        let target = dpi_physical_size(previous, old_scale, size, current_scale);
                        if target != size {
                            let mut fence = DpiCorrection::new(target, old_scale, Instant::now());
                            fence.requested = true;
                            window.dpi_correction = Some(fence);
                        }
                        return;
                    }
                    window.dpi_correction = None;
                    if size.width != 0
                        && size.height != 0
                        && window.window.fullscreen().is_none()
                        && !window.window.is_maximized()
                        && window.window.is_minimized() != Some(true)
                    {
                        // winit 0.30.13 windows/event_loop/runner.rs:361–402 regenerates a
                        // buffered writer after auto-resizing, and applies its answer only
                        // after this callback. Recognize that resize within one physical pixel;
                        // a concurrent user resize at the same ratio is indistinguishable.
                        let target = dpi_physical_size(previous, old_scale, size, scale_factor);
                        let accepted = inner_size_writer.request_inner_size(target).is_ok();
                        if !accepted || target != size {
                            let mut correction =
                                DpiCorrection::new(target, scale_factor, Instant::now());
                            // A successful writer needs observation only, never another request.
                            correction.requested = accepted;
                            window.dpi_correction = Some(correction);
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                let _ = &mut inner_size_writer;
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
            WindowEvent::CloseRequested => {
                window.input.close();
                self.cancel_pending_arm(id);
                (self.events)(HostEvent::CloseRequested { id });
            }
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
        for (id, (until, done, known)) in std::mem::take(&mut self.pending_arms) {
            let input = self
                .windows
                .get_mut(&id)
                .filter(|_| known)
                .map(|w| &mut w.input);
            install_arm(input, until, done);
        }
        let now = Instant::now();
        let mut next = None;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let mut corrections = Vec::new();
        for (&id, window) in &mut self.windows {
            #[cfg(target_os = "windows")]
            if let Some(correction) = &mut window.dpi_correction {
                // about_to_wait runs after WM_DPICHANGED returns; one live request only.
                let mut actual = sample(&window.window);
                if correction.begin_request(actual.0, now) {
                    let _ = window.window.request_inner_size(correction.size);
                    actual = sample(&window.window);
                }
                if correction.done(actual.0, now) {
                    window.dpi_correction = None;
                    corrections.push((id, actual));
                } else {
                    let deadline = correction.deadline;
                    next = Some(next.map_or(deadline, |previous: Instant| previous.min(deadline)));
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(poll) = &mut window.fullscreen_poll {
                if poll.due(now) && window.window.fullscreen().is_some() != window.fullscreen {
                    corrections.push((id, sample(&window.window)));
                }
                if let Some(deadline) = poll.deadline(now) {
                    next = Some(next.map_or(deadline, |previous: Instant| previous.min(deadline)));
                } else {
                    window.fullscreen_poll = None;
                }
            }
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
        // AppKit can update the fullscreen ivar after its queued Resized. A late change gets
        // Fullscreen followed by fresh current geometry; unchanged ticks never resize or rearm.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        for (id, (size, scale)) in corrections {
            self.resized(id, size, scale);
        }
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

#[cfg(any(target_os = "windows", test))]
fn valid_mapping(rows: &[HostMonitorMapping]) -> bool {
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    rows.iter().all(|row| {
        let bounds = row.geometry.logical_bounds();
        row.geometry.is_valid()
            && bounds.max_x().is_finite()
            && bounds.max_y().is_finite()
            && bounds.max_x() > bounds.min_x()
            && bounds.max_y() > bounds.min_y()
            && !row.native_id.is_empty()
            && ids.insert(row.id)
            && names.insert(&row.native_id)
            && i64::from(row.physical_origin.x) + i64::from(row.geometry.pixel_size.width)
                <= i64::from(i32::MAX)
            && i64::from(row.physical_origin.y) + i64::from(row.geometry.pixel_size.height)
                <= i64::from(i32::MAX)
    })
}

#[cfg(any(target_os = "windows", test))]
fn map_logical(
    rows: &[HostMonitorMapping],
    point: winit::dpi::LogicalPosition<f64>,
) -> Option<(&HostMonitorMapping, PhysicalPosition<i32>)> {
    if !valid_mapping(rows) || !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    let point = crosspane_types::geom::PointLogical::new(point.x, point.y);
    let mut containing = rows
        .iter()
        .filter(|row| row.geometry.logical_bounds().contains(point));
    let row = containing.next()?;
    if containing.next().is_some() {
        return None;
    }
    let local = row.geometry.logical_to_device(point);
    if local.x.round() >= f64::from(row.geometry.pixel_size.width)
        || local.y.round() >= f64::from(row.geometry.pixel_size.height)
    {
        return None;
    }
    let x = f64::from(row.physical_origin.x) + local.x.round();
    let y = f64::from(row.physical_origin.y) + local.y.round();
    if x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return None;
    }
    Some((row, PhysicalPosition::new(x as i32, y as i32)))
}

#[cfg(target_os = "windows")]
fn native_bounds_match(row: &HostMonitorMapping, monitor: &MonitorHandle) -> bool {
    monitor.position() == row.physical_origin
        && monitor.size()
            == PhysicalSize::new(
                row.geometry.pixel_size.width,
                row.geometry.pixel_size.height,
            )
}

#[cfg(target_os = "windows")]
fn place_physical_content(window: &Window, desired: PhysicalPosition<i32>) -> bool {
    for correction in 0..=1 {
        let (Ok(outer), Ok(inner)) = (window.outer_position(), window.inner_position()) else {
            return false;
        };
        if correction != 0
            && (i64::from(inner.x) - i64::from(desired.x)).abs() <= 1
            && (i64::from(inner.y) - i64::from(desired.y)).abs() <= 1
        {
            return true;
        }
        let x = i32::try_from(i64::from(desired.x) + i64::from(outer.x) - i64::from(inner.x));
        let y = i32::try_from(i64::from(desired.y) + i64::from(outer.y) - i64::from(inner.y));
        let (Ok(x), Ok(y)) = (x, y) else {
            return false;
        };
        window.set_outer_position(PhysicalPosition::new(x, y));
    }
    window.inner_position().is_ok_and(|inner| {
        (i64::from(inner.x) - i64::from(desired.x)).abs() <= 1
            && (i64::from(inner.y) - i64::from(desired.y)).abs() <= 1
    })
}

#[cfg(any(target_os = "windows", test))]
fn dpi_physical_size(
    previous: PhysicalSize<u32>,
    old_scale: f64,
    actual: PhysicalSize<u32>,
    new_scale: f64,
) -> PhysicalSize<u32> {
    let scaled = |length: u32| (f64::from(length) * new_scale / old_scale).round();
    if previous.width != 0
        && previous.height != 0
        && old_scale.is_finite()
        && old_scale > 0.0
        && new_scale.is_finite()
        && new_scale > 0.0
        && old_scale != new_scale
        && (f64::from(actual.width) - scaled(previous.width)).abs() <= 1.0
        && (f64::from(actual.height) - scaled(previous.height)).abs() <= 1.0
    {
        previous
    } else {
        actual
    }
}

#[cfg(any(target_os = "windows", test))]
struct DpiCorrection {
    size: PhysicalSize<u32>,
    scale: f64,
    deadline: Instant,
    requested: bool,
}
#[cfg(any(target_os = "windows", test))]
impl DpiCorrection {
    fn new(size: PhysicalSize<u32>, scale: f64, now: Instant) -> Self {
        Self {
            size,
            scale,
            deadline: now + Duration::from_millis(250),
            requested: false,
        }
    }
    fn done(&self, actual: PhysicalSize<u32>, now: Instant) -> bool {
        actual == self.size || now >= self.deadline
    }
    fn begin_request(&mut self, actual: PhysicalSize<u32>, now: Instant) -> bool {
        if self.requested || self.done(actual, now) {
            return false;
        }
        self.requested = true;
        true
    }
}

fn proxy_instance_descriptor(mut descriptor: wgpu::InstanceDescriptor) -> wgpu::InstanceDescriptor {
    // The proxy uses Vulkan/Metal. Unintended EGL initialization registers a driver atexit
    // cleanup whose GLES debug callback logs after tracing's TLS destruction, aborting the agent.
    descriptor.backends = wgpu::Backends::PRIMARY;
    #[cfg(target_os = "windows")]
    {
        // DX12 includes WARP when no hardware adapter is available in the VM.
        descriptor.backends = wgpu::Backends::DX12;
        // The proxy needs only SM5 shaders. Use the OS-provided compiler instead of loading
        // an ambient dxcompiler.dll whose version may be incompatible with wgpu.
        descriptor.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    }
    descriptor
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
    fn observe_fullscreen(
        &mut self,
        id: u64,
        geometry: Option<(PhysicalSize<u32>, f64)>,
        emit: &mut dyn FnMut(HostEvent),
    ) {
        let fullscreen = self.window.fullscreen().is_some();
        if fullscreen && !self.fullscreen {
            self.flash_until = Some(Instant::now() + Duration::from_secs(2));
            self.window.request_redraw();
        }
        report_fullscreen(id, &mut self.fullscreen, fullscreen, geometry, emit);
    }

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
const TRACKS_GEOMETRY: bool = cfg!(any(target_os = "macos", target_os = "windows"));

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
    fn event(self, id: u64, window_number: Option<u32>) -> HostEvent {
        HostEvent::Placed {
            id,
            window_number,
            visible: self.visible,
            monitor: self.monitor,
            origin: self.origin,
            size: self.size,
        }
    }
}

/// Read only the host's own AppKit object, on the event loop's main thread.
#[cfg(target_os = "macos")]
fn mac_window_number(window: &Window) -> Option<u32> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let _main = objc2::MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit's AppKit handle supplies its live NSView. The borrowed Window keeps it
    // alive throughout this main-thread query; no pointer or native object leaves this call.
    let view = unsafe { handle.ns_view.cast::<objc2_app_kit::NSView>().as_ref() };
    u32::try_from(view.window()?.windowNumber())
        .ok()
        .filter(|number| *number != 0)
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
    #[cfg(any(not(target_os = "windows"), test))]
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
fn content_request(size: PixelSize, fullscreen: bool) -> Option<PhysicalSize<u32>> {
    (!fullscreen).then_some(PhysicalSize::new(size.width, size.height))
}

/// Reports one sampled state and, when present, the geometry from the same observation.
fn report_fullscreen(
    id: u64,
    previous: &mut bool,
    fullscreen: bool,
    geometry: Option<(PhysicalSize<u32>, f64)>,
    emit: &mut dyn FnMut(HostEvent),
) {
    if *previous != fullscreen {
        *previous = fullscreen;
        emit(HostEvent::Fullscreen { id, fullscreen });
    }
    if let Some((size, scale)) = geometry {
        emit(HostEvent::Resized {
            id,
            size: pixel_size(size),
            scale,
        });
    }
}

#[cfg(any(target_os = "macos", test))]
struct FullscreenPoll {
    next: Instant,
    until: Instant,
}

#[cfg(any(target_os = "macos", test))]
impl FullscreenPoll {
    fn after_resize(poll: &mut Option<Self>, now: Instant) {
        // Native resize storms must not postpone the next sample or extend this poll's budget.
        if poll.is_none() {
            *poll = Some(Self::new(now));
        }
    }

    fn new(now: Instant) -> Self {
        Self {
            next: now + Duration::from_millis(50),
            until: now + Duration::from_secs(2),
        }
    }

    fn due(&mut self, now: Instant) -> bool {
        if now < self.next || now > self.until {
            return false;
        }
        self.next = now + Duration::from_millis(50);
        true
    }

    fn deadline(&self, now: Instant) -> Option<Instant> {
        (now <= self.until && self.next <= self.until).then_some(self.next)
    }
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
    fn content_placement_uses_frame_offsets_and_corrects_at_most_once() {
        use std::cell::RefCell;
        use winit::dpi::LogicalPosition as P;
        for (offset, scale, drift) in [
            ((0.0, 28.0), 1.0, (0.0, 0.0)),
            ((5.0, 32.0), 2.0, (1.0, -3.0)),
            ((-2.5, 22.5), 1.5, (-4.0, 2.0)),
        ] {
            let desired = P::new(-300.5, 60.25);
            let frame = RefCell::new(P::new(100.0, 100.0));
            let sets = RefCell::new(Vec::new());
            place_content(
                desired,
                || {
                    let outer = *frame.borrow();
                    let drift = if sets.borrow().is_empty() {
                        (0.0, 0.0)
                    } else {
                        drift
                    };
                    Ok::<_, ()>((
                        outer,
                        P::new(outer.x + offset.0 + drift.0, outer.y + offset.1 + drift.1),
                        scale,
                    ))
                },
                |outer| {
                    *frame.borrow_mut() = outer;
                    sets.borrow_mut().push(outer);
                },
            )
            .unwrap();
            let sets = sets.borrow();
            assert_eq!(sets[0], P::new(desired.x - offset.0, desired.y - offset.1));
            assert_eq!(sets.len(), if drift == (0.0, 0.0) { 1 } else { 2 });
            let final_outer = *frame.borrow();
            assert_eq!(
                P::new(
                    final_outer.x + offset.0 + drift.0,
                    final_outer.y + offset.1 + drift.1
                ),
                desired
            );
        }
    }

    #[test]
    fn content_placement_tolerance_is_one_physical_pixel_and_read_failures_propagate() {
        use std::cell::Cell;
        use winit::dpi::LogicalPosition as P;
        for error in [0.5, 0.5001] {
            let calls = Cell::new(0);
            let mut reads = 0;
            place_content(
                P::new(0.0, 0.0),
                || {
                    reads += 1;
                    Ok::<_, ()>((
                        P::new(0.0, 0.0),
                        P::new(if reads == 1 { 0.0 } else { error }, 0.0),
                        2.0,
                    ))
                },
                |_| calls.set(calls.get() + 1),
            )
            .unwrap();
            assert_eq!(calls.get(), if error == 0.5 { 1 } else { 2 });
            assert_eq!(reads, 2);
        }
        let mut sets = 0;
        assert_eq!(
            place_content(
                P::new(0.0, 0.0),
                || Err::<(P<f64>, P<f64>, f64), _>("no geometry"),
                |_| sets += 1
            ),
            Err("no geometry")
        );
        assert_eq!(sets, 0);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn proxy_backends_exclude_egl_and_keep_vulkan_and_metal() {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.flags = wgpu::InstanceFlags::empty();
        descriptor.memory_budget_thresholds.for_resource_creation = Some(75);
        let descriptor = proxy_instance_descriptor(descriptor);
        assert!(!descriptor.backends.intersects(wgpu::Backends::GL));
        assert!(descriptor.backends.contains(wgpu::Backends::VULKAN));
        assert!(descriptor.backends.contains(wgpu::Backends::METAL));
        assert_eq!(descriptor.flags, wgpu::InstanceFlags::empty());
        assert_eq!(
            descriptor.memory_budget_thresholds.for_resource_creation,
            Some(75)
        );
        assert!(descriptor.display.is_none());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn proxy_windows_defaults_to_dx12_and_preserves_descriptor_options() {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.flags = wgpu::InstanceFlags::empty();
        descriptor.memory_budget_thresholds.for_resource_creation = Some(75);
        let descriptor = proxy_instance_descriptor(descriptor);
        assert_eq!(descriptor.backends, wgpu::Backends::DX12);
        assert!(matches!(
            descriptor.backend_options.dx12.shader_compiler,
            wgpu::Dx12Compiler::Fxc
        ));
        assert_eq!(descriptor.flags, wgpu::InstanceFlags::empty());
        assert_eq!(
            descriptor.memory_budget_thresholds.for_resource_creation,
            Some(75)
        );
        assert!(descriptor.display.is_none());
    }

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
            content_request(PixelSize::new(777, 433), false),
            Some(PhysicalSize::new(777, 433))
        );
        // Beyond the opening fit: on a 1512x982 logical screen at scale 2 it would cap 2800x1600
        // physical to 2720x1554. The request is neither capped nor reshaped.
        let size = PixelSize::new(2800, 1600);
        assert_eq!(
            fit(logical_size(size, 2.0), laptop),
            LogicalSize::new(1360.0, 777.0)
        );
        assert_eq!(
            content_request(size, false),
            Some(PhysicalSize::new(2800, 1600))
        );
    }

    #[test]
    fn fullscreen_samples_emit_change_before_resize_and_deduplicate() {
        let mut previous = false;
        let mut events = Vec::new();
        let monitor = PhysicalSize::new(1280, 720);
        let old = PhysicalSize::new(320, 240);
        for (fullscreen, size) in [(true, monitor), (true, monitor), (false, old), (false, old)] {
            report_fullscreen(3, &mut previous, fullscreen, Some((size, 1.0)), &mut |e| {
                events.push(e)
            });
        }
        assert_eq!(
            events,
            [
                HostEvent::Fullscreen {
                    id: 3,
                    fullscreen: true
                },
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(monitor),
                    scale: 1.0
                },
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(monitor),
                    scale: 1.0
                },
                HostEvent::Fullscreen {
                    id: 3,
                    fullscreen: false
                },
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(old),
                    scale: 1.0
                },
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(old),
                    scale: 1.0
                },
            ]
        );
        assert!(!previous);
    }

    #[test]
    fn fullscreen_samples_without_geometry_emit_only_changes() {
        let mut previous = false;
        let mut events = Vec::new();
        for actual in [false, true, true, false, false] {
            report_fullscreen(3, &mut previous, actual, None, &mut |e| events.push(e));
        }
        assert_eq!(
            events,
            [
                HostEvent::Fullscreen {
                    id: 3,
                    fullscreen: true
                },
                HostEvent::Fullscreen {
                    id: 3,
                    fullscreen: false
                },
            ]
        );
    }

    #[test]
    fn fullscreen_poll_is_50ms_and_stops_at_2s() {
        let now = Instant::now();
        let mut poll = FullscreenPoll::new(now);
        assert_eq!(poll.deadline(now), Some(now + Duration::from_millis(50)));
        assert!(!poll.due(now + Duration::from_millis(49)));
        for elapsed in (50..=2000).step_by(50) {
            let sample = now + Duration::from_millis(elapsed);
            assert!(poll.due(sample), "missing sample at {elapsed}ms");
            assert!(!poll.due(sample), "same tick sampled twice");
        }
        assert_eq!(poll.deadline(now + Duration::from_secs(2)), None);
        assert!(!poll.due(now + Duration::from_millis(2001)));
        assert_eq!(poll.deadline(now + Duration::from_millis(2001)), None);
    }

    #[test]
    fn fullscreen_resizes_preserve_active_poll_cadence_and_original_expiry() {
        let now = Instant::now();
        let mut poll = None;
        FullscreenPoll::after_resize(&mut poll, now);
        let mut samples = Vec::new();
        for elapsed in (10..=2000).step_by(10) {
            let instant = now + Duration::from_millis(elapsed);
            if elapsed % 30 == 0 {
                FullscreenPoll::after_resize(&mut poll, instant);
            }
            let active = poll.as_mut().unwrap();
            assert_eq!(active.until, now + Duration::from_secs(2));
            if active.due(instant) {
                samples.push(elapsed);
            }
            if elapsed < 2000 {
                let next = (elapsed / 50 + 1) * 50;
                assert_eq!(
                    active.deadline(instant),
                    Some(now + Duration::from_millis(next))
                );
            } else {
                assert_eq!(active.deadline(instant), None);
                poll = None;
            }
        }
        assert_eq!(samples, (50..=2000).step_by(50).collect::<Vec<_>>());
        let later = now + Duration::from_millis(2010);
        FullscreenPoll::after_resize(&mut poll, later);
        assert_eq!(
            poll.unwrap().deadline(later),
            Some(now + Duration::from_millis(2060))
        );
    }

    #[test]
    fn fullscreen_poll_delays_do_not_extend_its_budget_and_new_polls_get_their_own_budget() {
        let now = Instant::now();
        let mut delayed = FullscreenPoll::new(now);
        assert!(delayed.due(now + Duration::from_millis(1975)));
        assert_eq!(delayed.deadline(now + Duration::from_millis(1975)), None);
        assert!(!delayed.due(now + Duration::from_millis(2025)));
        let resized = now + Duration::from_secs(1);
        let mut restarted = FullscreenPoll::new(resized);
        assert_eq!(
            restarted.deadline(resized),
            Some(resized + Duration::from_millis(50))
        );
        assert!(restarted.due(now + Duration::from_millis(2050)));
        assert!(!restarted.due(now + Duration::from_millis(3001)));
    }

    #[test]
    fn fullscreen_late_exit_reports_current_corrective_geometry_once() {
        let now = Instant::now();
        let mut poll = FullscreenPoll::new(now);
        let mut previous = true;
        let current = PhysicalSize::new(320, 240);
        let mut events = Vec::new();
        // AppKit's queued resize arrived before winit cleared its fullscreen ivar.
        report_fullscreen(3, &mut previous, true, Some((current, 2.0)), &mut |e| {
            events.push(e)
        });
        assert!(poll.due(now + Duration::from_millis(50)));
        report_fullscreen(3, &mut previous, false, Some((current, 2.0)), &mut |e| {
            events.push(e)
        });
        assert!(poll.due(now + Duration::from_millis(100)));
        report_fullscreen(3, &mut previous, false, None, &mut |e| events.push(e));
        assert_eq!(
            events,
            [
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(current),
                    scale: 2.0
                },
                HostEvent::Fullscreen {
                    id: 3,
                    fullscreen: false
                },
                HostEvent::Resized {
                    id: 3,
                    size: pixel_size(current),
                    scale: 2.0
                },
            ]
        );
    }

    #[test]
    fn fullscreen_content_size_request_is_dropped() {
        for size in [PixelSize::new(777, 433), PixelSize::new(2800, 1600)] {
            assert_eq!(content_request(size, true), None);
            assert_eq!(
                content_request(size, false),
                Some(PhysicalSize::new(size.width, size.height))
            );
        }
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
            placed.event(26, None),
            HostEvent::Placed {
                id: 26,
                window_number: None,
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

#[cfg(test)]
mod windows_mapping_tests {
    use super::*;
    use crosspane_types::geom::{DisplayGeometry, PointLogical, SizeMm};
    fn row(
        id: u32,
        physical: (i32, i32),
        pixels: (u32, u32),
        logical: (f64, f64),
        scale: f64,
    ) -> HostMonitorMapping {
        HostMonitorMapping {
            id,
            native_id: format!("monitor{id}"),
            physical_origin: PhysicalPosition::new(physical.0, physical.1),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(500.0, 300.0),
                pixel_size: PixelSize::new(pixels.0, pixels.1),
                scale,
                logical_origin: PointLogical::new(logical.0, logical.1),
            },
        }
    }
    #[test]
    fn mixed_dpi_mapping_uses_monitor_relative_offsets_and_offset_seams() {
        let a = row(1, (0, 0), (1920, 1080), (0.0, 0.0), 1.0);
        let b = row(2, (1920, 0), (1920, 2160), (1920.0, 0.0), 2.0);
        let rows = [a, b];
        let (monitor, point) =
            map_logical(&rows, winit::dpi::LogicalPosition::new(2020.0, 100.0)).unwrap();
        assert_eq!(monitor.id, 2);
        assert_eq!(point, PhysicalPosition::new(2120, 200));
        let rows = [
            row(1, (0, 0), (2560, 1440), (0.0, 0.0), 2.0),
            row(2, (2560, 200), (1920, 1080), (1280.0, 100.0), 1.0),
        ];
        assert_eq!(
            map_logical(&rows, winit::dpi::LogicalPosition::new(1380.0, 200.0))
                .unwrap()
                .1,
            PhysicalPosition::new(2660, 300)
        );
    }
    #[test]
    fn missing_invalid_and_ambiguous_mapping_degrades_instead_of_guessing() {
        let point = winit::dpi::LogicalPosition::new(10.0, 10.0);
        assert!(map_logical(&[], point).is_none());
        let a = row(1, (-1920, 0), (1920, 1080), (-1920.0, 0.0), 1.0);
        assert!(map_logical(std::slice::from_ref(&a), point).is_none());
        assert!(map_logical(&[a.clone(), a.clone()], point).is_none());
        let b = row(2, (0, 0), (1920, 1080), (-1920.0, 0.0), 1.0);
        assert!(map_logical(&[a, b], winit::dpi::LogicalPosition::new(-10.0, 10.0)).is_none());
        let mut a = row(1, (0, 0), (1920, 1080), (0.0, 0.0), 1.0);
        a.geometry.scale = f64::NAN;
        assert!(map_logical(&[a], point).is_none());
    }
    #[test]
    fn expired_writer_correction_observes_physical_size_or_absolute_expiry() {
        let now = Instant::now();
        let mut correction = DpiCorrection::new(PhysicalSize::new(640, 480), 2.0, now);
        assert!(!correction.requested);
        assert!(correction.begin_request(PhysicalSize::new(1280, 960), now));
        assert!(!correction.begin_request(PhysicalSize::new(1280, 960), now));
        assert!(correction.requested);
        assert!(!correction.done(
            PhysicalSize::new(1280, 960),
            now + Duration::from_millis(249)
        ));
        assert!(correction.done(PhysicalSize::new(640, 480), now));
        assert!(correction.done(
            PhysicalSize::new(1280, 960),
            now + Duration::from_millis(250)
        ));
        let mut delayed = DpiCorrection::new(PhysicalSize::new(640, 480), 2.0, now);
        assert!(!delayed.begin_request(
            PhysicalSize::new(1280, 960),
            now + Duration::from_millis(250)
        ));
        assert!(!delayed.requested);
        let mut already_correct = DpiCorrection::new(PhysicalSize::new(640, 480), 2.0, now);
        assert!(!already_correct.begin_request(PhysicalSize::new(640, 480), now));
        let mut accepted = DpiCorrection::new(PhysicalSize::new(320, 240), 2.0, now);
        accepted.requested = true;
        assert_eq!(accepted.scale, 2.0);
        assert!(!accepted.begin_request(PhysicalSize::new(640, 480), now));
        assert!(!accepted.done(PhysicalSize::new(640, 480), now));
        assert!(accepted.done(PhysicalSize::new(320, 240), now));
    }
    #[test]
    fn buffered_dpi_writer_retains_only_the_rounded_automatic_size() {
        let previous = PhysicalSize::new(320, 240);
        assert_eq!(
            dpi_physical_size(previous, 1.0, PhysicalSize::new(640, 480), 2.0),
            previous
        );
        assert_eq!(
            dpi_physical_size(previous, 1.0, PhysicalSize::new(641, 479), 2.0),
            previous
        );
        for actual in [PhysicalSize::new(642, 480), PhysicalSize::new(640, 482)] {
            assert_eq!(dpi_physical_size(previous, 1.0, actual, 2.0), actual);
        }
        assert_eq!(
            dpi_physical_size(previous, 2.0, PhysicalSize::new(160, 120), 1.0),
            previous
        );
        assert_eq!(dpi_physical_size(previous, 1.0, previous, 2.0), previous);
        let newer_source = PhysicalSize::new(360, 280);
        assert_eq!(
            dpi_physical_size(newer_source, 2.0, newer_source, 2.0),
            newer_source
        );
        // Older buffered DPI callbacks are ignored, so the latest generation still sees
        // the original physical baseline, even after two native automatic resizes.
        assert_eq!(
            dpi_physical_size(previous, 1.0, PhysicalSize::new(960, 720), 3.0),
            previous
        );
        assert_eq!(
            dpi_physical_size(newer_source, 1.0, PhysicalSize::new(720, 560), 2.0),
            newer_source
        );
    }
}
