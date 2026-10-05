//! Settings in a separate process, communicating only through the agent's control socket.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod art;
mod ctl;
mod demo;
mod fonts;
mod layout;
mod model;
#[cfg(windows)]
mod windows;

fn main() {
    if let Err(error) = run() {
        eprintln!("Crosspane Settings: {error}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let fonts = fonts::load()?;
    let screenshot = std::env::var_os("CROSSPANE_UI_SCREENSHOT").map(std::path::PathBuf::from);
    #[cfg(windows)]
    let acceptance = windows::Acceptance::from_env(screenshot.as_deref())?;
    let review =
        screenshot.is_some() || std::env::var("CROSSPANE_UI_DEMO").is_ok_and(|value| value == "1");
    #[cfg(windows)]
    let review = review && acceptance.is_none();
    // Review modes never even start the real socket worker.
    let worker = if review {
        None
    } else {
        Some(ctl::Worker::start()?)
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    let options = windows::native_options(options, acceptance.is_some())?;
    eframe::run_native(
        "Crosspane Settings",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(eframe::egui::Theme::Dark);
            cc.egui_ctx
                .set_style_of(eframe::egui::Theme::Dark, crosspane_ui_kit::theme::style());
            let art = art::load(&cc.egui_ctx);
            let settings = app::Settings::new(worker, art, screenshot);
            #[cfg(windows)]
            let settings = match acceptance {
                Some(mode) => {
                    let info = cc
                        .wgpu_render_state
                        .as_ref()
                        .ok_or_else(|| std::io::Error::other("live UI renderer is absent"))?
                        .adapter
                        .get_info();
                    if info.backend != eframe::wgpu::Backend::Dx12
                        || info.device_type != eframe::wgpu::DeviceType::Cpu
                    {
                        return Err(std::io::Error::other(
                            "live UI requires DX12 software rendering",
                        )
                        .into());
                    }
                    eprintln!("scratch_ui_backend=Dx12; scratch_ui_device=Cpu");
                    settings.for_acceptance(mode)
                }
                None => settings,
            };
            Ok(Box::new(settings))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}
