use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crosspane_types::geom::{PixelSize, PointDevice};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::PhysicalKey,
    window::{Window, WindowId},
};

use super::{
    HostCommand, HostEvent,
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
    input: InputState,
    consecutive_surface_losses: u8,
}

impl App {
    pub(super) fn new(
        proxy: EventLoopProxy<HostCommand>,
        events: Box<dyn FnMut(HostEvent)>,
    ) -> Self {
        Self {
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
    ) -> Result<(), String> {
        if self.windows.contains_key(&id) {
            return Err("proxy ID is already open".into());
        }
        if size.width == 0 || size.height == 0 {
            return Err("content size must be nonzero".into());
        }
        let instance = self.instance.as_ref().ok_or("event loop has not resumed")?;
        let initial_scale = event_loop
            .primary_monitor()
            .map_or(1.0, |monitor| monitor.scale_factor());
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_decorations(true)
            .with_resizable(true)
            .with_inner_size(logical_size(size, initial_scale));
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
            let _ = window.request_inner_size(logical_size(size, scale));
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
        config.present_mode = wgpu::PresentMode::Fifo;
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
            input: InputState::default(),
            consecutive_surface_losses: 0,
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
        Ok(())
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
            HostCommand::Open { id, title, size } => {
                if let Err(error) = self.open(event_loop, id, title, size) {
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
                    let result = window
                        .window
                        .request_inner_size(logical_size(size, window.window.scale_factor()));
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

    fn resized(&mut self, id: u64, size: PhysicalSize<u32>, scale: f64) {
        let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) else {
            return;
        };
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
            }
            WindowEvent::Resized(size) => {
                let scale = window.window.scale_factor();
                self.resized(id, size, scale);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = window.window.inner_size();
                self.resized(id, size, scale_factor);
            }
            WindowEvent::CloseRequested => (self.events)(HostEvent::CloseRequested { id }),
            WindowEvent::Destroyed => self.remove(id, true),
            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(instance)) = (&self.gpu, &self.instance)
                    && let Err(error) = window.render(gpu, instance)
                {
                    tracing::warn!(id, %error, "proxy rendering failed");
                    self.remove(id, true);
                }
            }
            _ => {}
        }
        self.check_gpu();
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.check_gpu();
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

    fn render(&mut self, gpu: &Gpu, instance: &wgpu::Instance) -> Result<(), String> {
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
        self.consecutive_surface_losses = 0;
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("proxy draw"),
            });
        self.presenter.encode(&mut encoder, &view);
        gpu.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        gpu.queue.present(frame);
        if suboptimal {
            self.resize(gpu, self.window.inner_size())?;
        }
        Ok(())
    }
}

fn pixel_size(size: PhysicalSize<u32>) -> PixelSize {
    PixelSize::new(size.width, size.height)
}
fn logical_size(size: PixelSize, scale: f64) -> LogicalSize<f64> {
    LogicalSize::new(
        f64::from(size.width) / scale,
        f64::from(size.height) / scale,
    )
}
fn window_scale(windows: &HashMap<u64, ProxyWindow>, id: u64) -> f64 {
    windows
        .get(&id)
        .map_or(1.0, |window| window.window.scale_factor())
}
