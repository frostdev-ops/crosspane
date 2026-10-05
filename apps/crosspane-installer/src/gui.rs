//! Explicit review I/O: one caller-selected font and, in demo mode, our own viewport PNG.
//! Font bytes are bounded to 16 MiB; preflight does not bound deferred rasterization, and
//! adversarial review/system fonts are outside the Tier 1 threat model.

use std::fs::File;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use crosspane_ui_kit::{
    art::{Art, BrandBytes},
    theme,
};
use eframe::egui;

use crate::WizardShell;
use crate::demo::{self, DisconnectedController};
use crate::view::{ScreenId, WizardAction, WizardView};

pub const MAX_FONT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "crosspane-installer",
    about = "Crosspane installer presentation shell"
)]
pub struct ReviewOptions {
    /// Use permanently disconnected in-memory review fixtures.
    #[arg(long)]
    pub demo: bool,
    /// Review a named demo screen.
    #[arg(long)]
    pub screen: Option<String>,
    /// Save only this demo viewport to an explicit PNG path, then exit.
    #[arg(long)]
    pub screenshot: Option<PathBuf>,
    /// Explicit, absolute path to a readable system font (maximum 16 MiB).
    #[arg(long)]
    pub font: Option<PathBuf>,
    /// Linux production: directory holding `payload.tar` and `payload.sha256`.
    #[arg(long, value_name = "DIR")]
    pub payload: Option<PathBuf>,
}

impl ReviewOptions {
    pub fn validate(&self) -> Result<Option<ScreenId>> {
        ensure!(
            self.demo || self.screen.is_none(),
            "--screen requires --demo"
        );
        ensure!(
            self.demo || self.screenshot.is_none(),
            "--screenshot requires --demo"
        );
        ensure!(
            !(self.demo && self.payload.is_some()),
            "--payload cannot be used with --demo"
        );
        if let Some(path) = &self.screenshot {
            ensure!(
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png")),
                "--screenshot needs an explicit .png output path"
            );
        }
        let screen = self
            .screen
            .as_deref()
            .map(|name| {
                demo::screen_named(name).with_context(|| format!("Unknown demo screen: {name}"))
            })
            .transpose()?;
        let font = self.font.as_ref().context(
            "Supply --font <absolute-readable-font-path>; this shell has no bundled font",
        )?;
        ensure!(font.is_absolute(), "--font must be an absolute path");
        Ok(if self.demo {
            Some(screen.unwrap_or(ScreenId::Welcome))
        } else {
            None
        })
    }
}

/// Layout reconciliation applied by the GUI frame after `tick`, never by the controller itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellEffect {
    #[default]
    FollowLayout,
    RevertLayout,
    CancelLayoutDrag,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ControllerTick {
    pub effects: Vec<ShellEffect>,
    pub wake_after_ms: Option<u64>,
}

/// One selected mode for the life of the process. Demo never becomes production.
pub trait InstallerController {
    fn view(&self) -> &WizardView;
    /// Returns true when the window should close.
    fn accept(&mut self, action: WizardAction) -> bool;
    fn tick(&mut self) -> ControllerTick;
    fn close(&mut self);
    /// The window manager asked to close the window. Returns false, and keeps the window open,
    /// while closing now would cut a running change short; otherwise closes and returns true.
    fn request_close(&mut self) -> bool {
        self.close();
        true
    }
}

impl InstallerController for DisconnectedController {
    fn view(&self) -> &WizardView {
        DisconnectedController::view(self)
    }
    fn accept(&mut self, action: WizardAction) -> bool {
        DisconnectedController::accept(self, action)
    }
    fn tick(&mut self) -> ControllerTick {
        ControllerTick::default()
    }
    fn close(&mut self) {}
}

fn apply_effects(shell: &mut WizardShell, view: &WizardView, effects: Vec<ShellEffect>) {
    let confirmed = view
        .layout
        .as_ref()
        .map(|layout| layout.confirmed.as_slice())
        .unwrap_or(&[]);
    for effect in effects {
        match effect {
            ShellEffect::FollowLayout => shell.follow_layout(confirmed),
            ShellEffect::RevertLayout => shell.revert_layout(confirmed),
            ShellEffect::CancelLayoutDrag => shell.cancel_layout_drag(),
        }
    }
}

pub fn load_review_font(path: &Path) -> Result<egui::FontDefinitions> {
    ensure!(path.is_absolute(), "--font must be an absolute path");
    ensure!(
        std::fs::metadata(path)
            .with_context(|| format!("Cannot inspect review font {}", path.display()))?
            .is_file(),
        "Review font must be a regular file"
    );
    let file =
        File::open(path).with_context(|| format!("Cannot read review font {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Review font must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_FONT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    font_definitions(bytes)
}

/// Kept independent of path selection so the bounded input contract can be tested headlessly.
pub fn font_definitions(bytes: Vec<u8>) -> Result<egui::FontDefinitions> {
    ensure!(!bytes.is_empty(), "Review font is empty");
    ensure!(bytes.len() <= MAX_FONT_BYTES, "Review font exceeds 16 MiB");
    ensure!(
        bytes.starts_with(&[0, 1, 0, 0])
            || bytes.starts_with(b"OTTO")
            || bytes.starts_with(b"ttcf")
            || bytes.starts_with(b"true"),
        "Review font must be a TrueType or OpenType font"
    );
    let mut fonts = egui::FontDefinitions::empty();
    fonts.font_data.insert(
        "review-system-font".into(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .insert(family, vec!["review-system-font".into()]);
    }
    // epaint's fallible FontFace parser is private. Its public Fonts constructor
    // parses the actual font, but reports parse failure by panicking. Preflight a
    // separate CPU-only instance before opening a viewport and convert that failure
    // into the startup error. Preserve the process panic hook and use no default font.
    std::panic::catch_unwind(|| {
        egui::epaint::text::Fonts::new(egui::epaint::text::TextOptions::default(), fonts.clone())
    })
    .map_err(|_| anyhow::anyhow!("Review font could not be parsed by the renderer"))?;
    Ok(fonts)
}

/// The one mode a launch runs in, chosen from the flags before anything else is constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    /// Permanently disconnected review fixtures: no production port, probe or receipt reader.
    Demo,
    /// The live installer for this operating system, with its own system font.
    Production,
    /// Review flags without `--demo`: refused with the validation error.
    Refused,
}

impl ReviewOptions {
    pub fn mode(&self) -> LaunchMode {
        if self.demo {
            LaunchMode::Demo
        } else if self.screen.is_some() || self.screenshot.is_some() || self.font.is_some() {
            LaunchMode::Refused
        } else {
            LaunchMode::Production
        }
    }
}

pub fn run(options: ReviewOptions, brand: BrandBytes<'static>) -> Result<()> {
    match options.mode() {
        LaunchMode::Demo => {
            let screen = options
                .validate()?
                .context("--demo needs a screen to review")?;
            let font = options
                .font
                .as_deref()
                .context("Missing explicit review font")?;
            // A named screen may be one of its review variants (see `demo::VARIANTS`).
            let controller = match options.screen.as_deref() {
                Some(name) => DisconnectedController::named(name)
                    .with_context(|| format!("Unknown demo screen: {name}"))?,
                None => DisconnectedController::new(Some(screen)),
            };
            run_gui(
                options.screenshot,
                brand,
                load_review_font(font)?,
                Box::new(controller),
            )
        }
        LaunchMode::Refused => {
            options.validate()?;
            // A font without --demo: production reads the system font itself and takes no
            // override. Other operating systems keep today's disconnected normal entry.
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            bail!("--font is only valid with --demo; production uses the system font");
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let font = options.font.as_deref().context("Missing explicit font")?;
                run_gui(
                    None,
                    brand,
                    load_review_font(font)?,
                    Box::new(DisconnectedController::new(None)),
                )
            }
        }
        LaunchMode::Production => {
            let (fonts, controller) = production_controller(options.payload)?;
            run_gui(None, brand, fonts, controller)
        }
    }
}

fn production_controller(
    payload: Option<PathBuf>,
) -> Result<(egui::FontDefinitions, Box<dyn InstallerController>)> {
    // The installer never runs elevated: it refuses before reading a font or building any port.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ensure!(
        !rustix::process::geteuid().is_root(),
        "The installer refuses to run as root or with sudo. Run it from your own account."
    );
    #[cfg(target_os = "linux")]
    {
        crate::platform::linux::integration::open(payload)
    }
    #[cfg(target_os = "macos")]
    {
        // The Mac installs from a signed payload embedded in this build, never a directory.
        ensure!(payload.is_none(), "--payload is only used on Linux");
        crate::platform::macos::integration::open()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = payload;
        bail!("Installation is not available on this operating system yet.");
    }
}

fn run_gui(
    screenshot: Option<PathBuf>,
    brand: BrandBytes<'static>,
    fonts: egui::FontDefinitions,
    controller: Box<dyn InstallerController>,
) -> Result<()> {
    let capture_requested = screenshot.is_some();
    // The deadline starts before native viewport setup, not when capture is requested.
    let capture_timing = screenshot
        .as_ref()
        .map(|_| CaptureTiming::new(Instant::now()));
    let completion = Arc::new(Mutex::new(CaptureCompletion::Pending));
    let app_completion = completion.clone();
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Crosspane Installer",
        native,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            cc.egui_ctx.set_style_of(egui::Theme::Dark, theme::style());
            let art = Art::load(&cc.egui_ctx, brand);
            Ok(Box::new(InstallerGui {
                controller,
                shell: WizardShell::default(),
                art,
                started: Instant::now(),
                frames: 0,
                screenshot,
                capture_timing,
                screenshot_token: egui::UserData::new("crosspane-installer-demo-viewport"),
                completion: app_completion,
                pending_actions: Vec::new(),
                pending_frame: None,
            }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    if capture_requested {
        let completion = completion
            .lock()
            .map_err(|_| anyhow::anyhow!("Screenshot completion state unavailable"))?;
        completion.result()?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum CaptureCompletion {
    Pending,
    Succeeded,
    Failed(String),
}

impl CaptureCompletion {
    fn result(&self) -> Result<()> {
        match self {
            Self::Pending => bail!("Demo screenshot: window exited before a successful PNG encode"),
            Self::Succeeded => Ok(()),
            Self::Failed(message) => bail!("{message}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureNext {
    Wait,
    Request,
    Timeout,
}

struct CaptureTiming {
    started: Instant,
    requested: bool,
}

impl CaptureTiming {
    fn new(started: Instant) -> Self {
        Self {
            started,
            requested: false,
        }
    }

    fn poll(&mut self, now: Instant, rendered_frames: u32) -> CaptureNext {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= Duration::from_secs(15) {
            CaptureNext::Timeout
        } else if !self.requested && rendered_frames >= 3 && elapsed >= Duration::from_millis(250) {
            self.requested = true;
            CaptureNext::Request
        } else {
            CaptureNext::Wait
        }
    }
}

struct InstallerGui {
    controller: Box<dyn InstallerController>,
    shell: WizardShell,
    art: Art,
    started: Instant,
    frames: u32,
    screenshot: Option<PathBuf>,
    capture_timing: Option<CaptureTiming>,
    screenshot_token: egui::UserData,
    completion: Arc<Mutex<CaptureCompletion>>,
    pending_actions: Vec<WizardAction>,
    pending_frame: Option<u64>,
}

impl Drop for InstallerGui {
    fn drop(&mut self) {
        self.controller.close();
    }
}

impl InstallerGui {
    fn finish_capture(&mut self, ctx: &egui::Context, result: Result<()>) {
        let state = match result {
            Ok(()) => CaptureCompletion::Succeeded,
            Err(error) => CaptureCompletion::Failed(format!("Demo screenshot: {error:#}")),
        };
        if let Ok(mut completion) = self.completion.lock() {
            *completion = state;
        } else {
            eprintln!("Demo screenshot: completion state unavailable");
        }
        self.capture_timing = None;
        self.screenshot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for InstallerGui {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A window-manager close during a running change is refused, so the change is never cut
        // short by the process exiting; the view explains why.
        if ctx.input(|input| input.viewport().close_requested()) && !self.controller.request_close()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.request_repaint();
        }
        // A multi-pass render keeps one immutable view. Apply its intents next frame.
        if self
            .pending_frame
            .is_some_and(|frame| frame < ctx.cumulative_frame_nr())
        {
            self.pending_frame = None;
            for action in std::mem::take(&mut self.pending_actions) {
                if self.controller.accept(action) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        let Some(path) = self.screenshot.as_ref() else {
            return;
        };
        let Some(timing) = self.capture_timing.as_mut() else {
            self.finish_capture(
                ctx,
                Err(anyhow::anyhow!("Capture timing state unavailable")),
            );
            return;
        };
        match timing.poll(Instant::now(), self.frames) {
            CaptureNext::Timeout => {
                self.finish_capture(
                    ctx,
                    Err(anyhow::anyhow!(
                        "Viewport setup and capture did not finish within 15 seconds"
                    )),
                );
                return;
            }
            CaptureNext::Request => ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                self.screenshot_token.clone(),
            )),
            CaptureNext::Wait => {}
        }
        let requested = timing.requested;
        let captured = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot {
                    viewport_id,
                    user_data,
                    image,
                } if requested
                    && *viewport_id == egui::ViewportId::ROOT
                    && *user_data == self.screenshot_token =>
                {
                    Some(image.clone())
                }
                _ => None,
            })
        });
        if let Some(image) = captured {
            let result = save_screenshot(path, &image);
            self.finish_capture(ctx, result);
        } else {
            // Finite capture setup/timeout scheduling only; ordinary idle screens do not poll.
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let tick = self.controller.tick();
        apply_effects(&mut self.shell, self.controller.view(), tick.effects);
        if let Some(ms) = tick.wake_after_ms {
            ui.ctx().request_repaint_after(Duration::from_millis(ms));
        }
        let actions = egui::Frame::new()
            .inner_margin(24)
            .show(ui, |ui| {
                self.shell
                    .show(ui, self.controller.view(), &self.art, now_ms)
            })
            .inner;
        if !actions.is_empty() {
            self.pending_frame = Some(ui.ctx().cumulative_frame_nr());
            for action in actions {
                if !self.pending_actions.contains(&action) {
                    self.pending_actions.push(action);
                }
            }
            ui.ctx().request_repaint();
        }
        self.frames = self.frames.saturating_add(1);
    }
}

fn save_screenshot(path: &Path, image: &egui::ColorImage) -> Result<()> {
    let width = u32::try_from(image.size[0])?;
    let height = u32::try_from(image.size[1])?;
    ensure!(
        width > 0 && height > 0 && image.pixels.len() <= 16 * 1024 * 1024,
        "Invalid or oversized viewport capture"
    );
    let file = File::create(path).with_context(|| format!("Cannot write {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(egui::Color32::to_array)
        .collect();
    writer.write_image_data(&bytes)?;
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logic_only_capture_setup_has_a_finite_deadline() {
        let start = Instant::now();
        let mut timing = CaptureTiming::new(start);
        // No UI renders while minimized: logic still polls through setup and times out.
        for ms in [0, 250, 5_000, 14_999] {
            assert_eq!(
                timing.poll(start + Duration::from_millis(ms), 0),
                CaptureNext::Wait
            );
        }
        assert_eq!(
            timing.poll(start + Duration::from_secs(15), 0),
            CaptureNext::Timeout
        );
        assert!(!timing.requested);
    }

    #[test]
    fn capture_delivery_uses_the_original_setup_deadline() {
        let start = Instant::now();
        let mut timing = CaptureTiming::new(start);
        assert_eq!(
            timing.poll(start + Duration::from_millis(249), 3),
            CaptureNext::Wait
        );
        assert_eq!(
            timing.poll(start + Duration::from_millis(250), 2),
            CaptureNext::Wait
        );
        assert_eq!(
            timing.poll(start + Duration::from_millis(250), 3),
            CaptureNext::Request
        );
        assert_eq!(
            timing.poll(start + Duration::from_secs(1), 4),
            CaptureNext::Wait
        );
        assert_eq!(
            timing.poll(start + Duration::from_secs(15), 4),
            CaptureNext::Timeout
        );
    }

    fn options(args: &[&str]) -> ReviewOptions {
        ReviewOptions::parse_from(
            std::iter::once("crosspane-installer").chain(args.iter().copied()),
        )
    }

    #[test]
    fn the_launch_mode_comes_from_the_flags_alone_and_demo_never_becomes_production() {
        assert_eq!(options(&[]).mode(), LaunchMode::Production);
        assert_eq!(
            options(&["--payload", "/stage"]).mode(),
            LaunchMode::Production
        );
        assert_eq!(
            options(&["--demo", "--font", "/f.ttf"]).mode(),
            LaunchMode::Demo
        );
        assert_eq!(
            options(&["--demo", "--screen", "welcome", "--font", "/f.ttf"]).mode(),
            LaunchMode::Demo
        );
        // Review flags without --demo are refused rather than quietly running production.
        for args in [
            &["--font", "/f.ttf"][..],
            &["--screen", "welcome"],
            &["--screenshot", "/x.png"],
        ] {
            assert_eq!(options(args).mode(), LaunchMode::Refused, "{args:?}");
        }
        // The staged payload belongs to production only.
        assert!(
            options(&["--demo", "--font", "/f.ttf", "--payload", "/stage"])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn incomplete_or_failed_capture_cannot_report_success() {
        assert!(CaptureCompletion::Pending.result().is_err());
        assert!(
            CaptureCompletion::Failed("Encoder failed".into())
                .result()
                .is_err()
        );
        assert!(CaptureCompletion::Succeeded.result().is_ok());
    }
}
