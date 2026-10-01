//! Settings in a separate process, communicating only through the agent's control socket.

mod app;
mod ctl;
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
    let worker = ctl::Worker::start()?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Crosspane Settings",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(app::Settings::new(worker)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}
