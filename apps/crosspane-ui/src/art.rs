//! Optional campaign artwork, decoded once; missing art never prevents settings actions.
use std::io::{BufWriter, Cursor};
use std::path::Path;

use anyhow::{Context, Result, bail};
use eframe::egui::{self, Color32, ColorImage, Pos2, Rect, TextureHandle, Vec2};

use crate::theme;

pub struct Art {
    backdrop: Option<TextureHandle>,
    emblem: Option<TextureHandle>,
    wordmark: Option<TextureHandle>,
}

impl Art {
    pub fn load(ctx: &egui::Context) -> Self {
        Self::from_bytes(
            ctx,
            include_bytes!("../assets/backdrop.png"),
            include_bytes!("../assets/emblem-256.png"),
            include_bytes!("../assets/wordmark.png"),
        )
    }

    /// Each decode succeeds or falls back independently; this constructor is infallible.
    fn from_bytes(ctx: &egui::Context, backdrop: &[u8], emblem: &[u8], wordmark: &[u8]) -> Self {
        let texture = |name: &str, bytes: &[u8]| -> Option<TextureHandle> {
            match decode(bytes) {
                Ok(image) => Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR)),
                Err(error) => {
                    eprintln!(
                        "Crosspane Settings warning: could not load {name}; using fallback ({error})"
                    );
                    None
                }
            }
        };
        Self {
            backdrop: texture("Crosspane aurora", backdrop),
            emblem: texture("Crosspane emblem", emblem),
            wordmark: texture("Crosspane wordmark", wordmark),
        }
    }

    pub fn emblem(&self, ui: &mut egui::Ui, size: Vec2) {
        if let Some(emblem) = &self.emblem {
            ui.add(egui::Image::new(emblem).fit_to_exact_size(size));
        }
    }

    pub fn wordmark(&self, ui: &mut egui::Ui, size: Vec2) {
        if let Some(wordmark) = &self.wordmark {
            ui.add(egui::Image::new(wordmark).fit_to_exact_size(size));
        } else {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                "CROSSPANE",
                egui::FontId::proportional(28.0),
                theme::ICE,
            );
        }
    }

    pub fn background(&self, painter: &egui::Painter, rect: Rect) {
        let painter = painter.with_clip_rect(rect);
        if let Some(backdrop) = &self.backdrop {
            painter.image(
                backdrop.id(),
                cover_rect(backdrop.size_vec2(), rect),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            theme::gradient(
                &painter,
                rect,
                0.0,
                theme::alpha(theme::MIDNIGHT, 55),
                theme::alpha(theme::MIDNIGHT, 235),
            );
        } else {
            // Opaque fallback: the native window's clear color must not show through.
            theme::gradient(
                &painter,
                rect,
                0.0,
                theme::MIDNIGHT,
                Color32::from_rgb(4, 12, 22),
            );
            return;
        }
        // A soft vignette keeps the header and panel edges legible at every aspect ratio.
        for step in (0..32).rev() {
            let inset = step as f32 * 4.0;
            painter.rect_stroke(
                rect.shrink(inset),
                0.0,
                egui::Stroke::new(4.0, theme::alpha(theme::MIDNIGHT, (32 - step) as u8 * 2)),
                egui::StrokeKind::Inside,
            );
        }
    }
}

fn decode(bytes: &[u8]) -> Result<ColorImage> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .context("PNG image is too large")?;
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer)?;
    let bytes = &buffer[..info.buffer_size()];
    let mut rgba = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(bytes),
        png::ColorType::Rgb => {
            for pixel in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &gray in bytes {
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for pixel in bytes.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
        }
        png::ColorType::Indexed => bail!("PNG palette was not expanded"),
    }
    Ok(ColorImage::from_rgba_unmultiplied(
        [info.width as usize, info.height as usize],
        &rgba,
    ))
}

/// Centered cover scaling: preserve proportions while completely covering the destination.
pub fn cover_rect(image: Vec2, destination: Rect) -> Rect {
    let scale = (destination.width() / image.x).max(destination.height() / image.y);
    Rect::from_center_size(destination.center(), image * scale)
}

pub fn save_screenshot(path: &Path, image: &ColorImage) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
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
    fn malformed_png_falls_back_and_other_assets_load_independently() {
        let ctx = egui::Context::default();
        let valid = [
            include_bytes!("../assets/backdrop.png").as_slice(),
            include_bytes!("../assets/emblem-256.png").as_slice(),
            include_bytes!("../assets/wordmark.png").as_slice(),
        ];
        for malformed in [b"malformed PNG".as_slice(), b"\x89PNG\r\n\x1a\n".as_slice()] {
            assert!(decode(malformed).is_err());
            let art = Art::from_bytes(&ctx, malformed, malformed, malformed);
            assert!(art.backdrop.is_none() && art.emblem.is_none() && art.wordmark.is_none());
            for failed in 0..3 {
                let mut bytes = valid;
                bytes[failed] = malformed;
                let art = Art::from_bytes(&ctx, bytes[0], bytes[1], bytes[2]);
                assert_eq!(
                    [
                        art.backdrop.is_none(),
                        art.emblem.is_none(),
                        art.wordmark.is_none()
                    ],
                    [failed == 0, failed == 1, failed == 2]
                );
                drop(art);
                // No renderer in this unit test; discard upload/free notifications explicitly.
                ctx.tex_manager().write().take_delta().clear();
            }
        }
    }

    #[test]
    fn cover_scaling_preserves_aspect_and_centers_landscape_portrait_and_hidpi() {
        for (image, viewport) in [
            (Vec2::new(960.0, 540.0), Vec2::new(1000.0, 700.0)),
            (Vec2::new(960.0, 540.0), Vec2::new(400.0, 900.0)),
            (Vec2::new(540.0, 960.0), Vec2::new(1200.0, 500.0)),
            (Vec2::new(1920.0, 1080.0), Vec2::new(960.0, 540.0)),
        ] {
            let target = Rect::from_min_size(Pos2::new(35.0, 72.0), viewport);
            let cover = cover_rect(image, target);
            assert_eq!(cover.center(), target.center());
            assert!(cover.width() + 0.001 >= viewport.x && cover.height() + 0.001 >= viewport.y);
            assert!((cover.width() / cover.height() - image.x / image.y).abs() < 0.001);
            assert!(
                (cover.width() - viewport.x).abs() < 0.001
                    || (cover.height() - viewport.y).abs() < 0.001
            );
        }
    }
}
