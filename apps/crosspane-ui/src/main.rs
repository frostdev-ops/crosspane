//! Settings in a separate process, communicating only through the agent's control socket.

mod app;
mod art;
mod ctl;
mod demo;
mod fonts;
mod layout;
mod model;

fn main() {
    if let Err(error) = run() {
        eprintln!("Crosspane Settings: {error}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let fonts = fonts::load()?;
    let screenshot = std::env::var_os("CROSSPANE_UI_SCREENSHOT").map(std::path::PathBuf::from);
    let review =
        screenshot.is_some() || std::env::var("CROSSPANE_UI_DEMO").is_ok_and(|value| value == "1");
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
    eframe::run_native(
        "Crosspane Settings",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.set_theme(eframe::egui::Theme::Dark);
            cc.egui_ctx
                .set_style_of(eframe::egui::Theme::Dark, crosspane_ui_kit::theme::style());
            let art = art::load(&cc.egui_ctx);
            Ok(Box::new(app::Settings::new(worker, art, screenshot)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}
