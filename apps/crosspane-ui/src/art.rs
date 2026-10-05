//! The settings app's brand art and screenshot encoding. Decoding, fallbacks and drawing live in
//! `crosspane-ui-kit`; this module only embeds the compact images and writes review screenshots.
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use crosspane_ui_kit::art::{Art, BrandBytes};
use eframe::egui::{self, ColorImage};

/// Decode the embedded art. Each image falls back on its own, so this is infallible.
pub fn load(ctx: &egui::Context) -> Art {
    Art::load(
        ctx,
        BrandBytes {
            backdrop: include_bytes!("../assets/backdrop.png"),
            emblem: include_bytes!("../assets/emblem-256.png"),
            wordmark: include_bytes!("../assets/wordmark.png"),
        },
    )
}

/// Encode a captured frame as an RGBA PNG at `path`. A review aid, not part of the kit.
pub fn save_screenshot(path: &Path, image: &ColorImage) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    encode_screenshot(file, image)
}

#[cfg(windows)]
pub fn save_acceptance_screenshot(path: &Path, image: &ColorImage) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("create new own-renderer screenshot")?;
    encode_screenshot(file, image)
}

fn encode_screenshot(file: std::fs::File, image: &ColorImage) -> Result<()> {
    let mut encoder = png::Encoder::new(
        BufWriter::new(file),
        u32::try_from(image.size[0])?,
        u32::try_from(image.size[1])?,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let rgba: Vec<_> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    writer.write_image_data(&rgba)?;
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_brand_art_loads_within_the_kits_bounds() {
        let ctx = egui::Context::default();
        let art = load(&ctx);
        assert_eq!(
            format!("{art:?}"),
            "Art { backdrop: true, emblem: true, wordmark: true }"
        );
        drop(art);
        // No renderer in this unit test; discard upload/free notifications explicitly.
        ctx.tex_manager().write().take_delta().clear();
    }
}
