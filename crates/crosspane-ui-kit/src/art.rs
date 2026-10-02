//! Optional brand artwork, decoded once from caller-supplied PNG bytes.
//!
//! The caller embeds the images (compact backdrop, emblem and wordmark) and passes the bytes in;
//! this module reads no files and looks up no fonts. Each image decodes, or fails, on its own, and
//! a missing image only changes how that one piece draws: a gradient instead of the backdrop, no
//! emblem, a text wordmark. Art problems never prevent a window from starting.
use std::io::Cursor;

use egui::{Color32, ColorImage, Pos2, Rect, TextureHandle, Vec2};

use crate::theme;

/// Encoded PNG larger than this is refused before it is parsed. The brand art is a few hundred KB.
const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
/// Widest or tallest accepted image. Larger images are refused before any pixel buffer exists.
const MAX_SIDE: u32 = 4096;
/// Most pixels accepted: 4 Mpx is 16 MiB of RGBA, the most this module will ever allocate.
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
/// Memory the PNG decoder may use for its own chunk buffers (text, palette, profiles).
const DECODER_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// The three encoded PNG images the art needs. Each is decoded independently.
#[derive(Clone, Copy, Debug)]
pub struct BrandBytes<'a> {
    /// The arctic aurora backdrop, cover-scaled behind the window.
    pub backdrop: &'a [u8],
    /// The sculptural crystalline emblem, transparent.
    pub emblem: &'a [u8],
    /// The CROSSPANE wordmark, transparent.
    pub wordmark: &'a [u8],
}

/// Decoded brand textures, each optional. Drawing never fails: a missing texture draws its
/// fallback.
pub struct Art {
    backdrop: Option<TextureHandle>,
    emblem: Option<TextureHandle>,
    wordmark: Option<TextureHandle>,
}

impl std::fmt::Debug for Art {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Texture handles have no Debug of their own; which pieces loaded is what matters.
        f.debug_struct("Art")
            .field("backdrop", &self.backdrop.is_some())
            .field("emblem", &self.emblem.is_some())
            .field("wordmark", &self.wordmark.is_some())
            .finish()
    }
}

impl Art {
    /// Decode and upload the three images. Each decode succeeds or falls back independently, so
    /// this never fails. A warning naming the image goes to stderr when one is replaced by its
    /// fallback.
    pub fn load(ctx: &egui::Context, bytes: BrandBytes<'_>) -> Self {
        // Never hand egui a texture wider than the renderer can hold: that is a panic in debug
        // builds and a GPU validation error in release builds.
        let max_side = u32::try_from(ctx.input(|input| input.max_texture_side))
            .unwrap_or(u32::MAX)
            .min(MAX_SIDE);
        let texture = |name: &str, bytes: &[u8]| -> Option<TextureHandle> {
            match decode(bytes, max_side) {
                Ok(image) => Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR)),
                Err(error) => {
                    eprintln!("Crosspane warning: could not load {name}; using fallback ({error})");
                    None
                }
            }
        };
        Self {
            backdrop: texture("Crosspane aurora", bytes.backdrop),
            emblem: texture("Crosspane emblem", bytes.emblem),
            wordmark: texture("Crosspane wordmark", bytes.wordmark),
        }
    }

    /// The emblem at `size`; nothing is drawn (and nothing is reserved) if it did not load.
    pub fn emblem(&self, ui: &mut egui::Ui, size: Vec2) {
        if let Some(emblem) = &self.emblem {
            ui.add(egui::Image::new(emblem).fit_to_exact_size(size));
        }
    }

    /// The wordmark at `size`, or the plain text "CROSSPANE" in its place if it did not load.
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

    /// Paint the backdrop, cover-scaled, under a Midnight gradient and a soft vignette. Without
    /// the backdrop this is an opaque Midnight gradient, so the native window's clear colour
    /// never shows through.
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

/// Decode one PNG to RGBA. The input size, the image dimensions (at most `max_side` each) and
/// the pixel count are all checked before the pixel buffer is allocated, so hostile or corrupt
/// data stays bounded.
fn decode(bytes: &[u8], max_side: u32) -> Result<ColorImage, String> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err(format!("PNG data is larger than {MAX_PNG_BYTES} bytes"));
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: DECODER_LIMIT_BYTES,
    });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width > max_side || height > max_side || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "PNG image is too large ({width} × {height} pixels)"
        ));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "PNG image is too large".to_owned())?;
    let mut buffer = vec![0; size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    let bytes = buffer
        .get(..info.buffer_size())
        .ok_or_else(|| "PNG frame is larger than its buffer".to_owned())?;
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
        png::ColorType::Indexed => return Err("PNG palette was not expanded".to_owned()),
    }
    let size = [info.width as usize, info.height as usize];
    if size[0] * size[1] * 4 != rgba.len() {
        return Err("PNG pixel data does not match its dimensions".to_owned());
    }
    Ok(ColorImage::from_rgba_unmultiplied(size, &rgba))
}

/// Centered cover scaling: preserve proportions while completely covering the destination.
pub fn cover_rect(image: Vec2, destination: Rect) -> Rect {
    if !(image.x > 0.0 && image.y > 0.0) {
        return destination;
    }
    let scale = (destination.width() / image.x).max(destination.height() / image.y);
    Rect::from_center_size(destination.center(), image * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32, depth: png::BitDepth, color: png::ColorType) -> Vec<u8> {
        let channels = match color {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            _ => 1,
        };
        let row_bytes = (width as usize * channels * depth as usize).div_ceil(8);
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_image_data(&vec![0x55; row_bytes * height as usize])
            .expect("pixels");
        writer.finish().expect("finish");
        out
    }

    #[test]
    fn dimension_and_pixel_bounds_are_checked_before_allocation() {
        let ok = png_bytes(8, 8, png::BitDepth::Eight, png::ColorType::Rgba);
        let image = decode(&ok, MAX_SIDE).expect("small image");
        assert_eq!(image.size, [8, 8]);
        // Too wide or too tall, for the kit's own ceiling and for a lower texture limit, all as
        // cheap 1-bit images.
        for (w, h, max_side) in [
            (MAX_SIDE + 1, 1, MAX_SIDE),
            (1, MAX_SIDE + 1, MAX_SIDE),
            (65, 1, 64),
            (1, 65, 64),
        ] {
            let big = png_bytes(w, h, png::BitDepth::One, png::ColorType::Grayscale);
            let error = decode(&big, max_side).expect_err("must be refused");
            assert!(error.contains("too large"), "{error}");
        }
        // The pixel budget is exact: 4 Mpx is accepted, one row more is refused.
        let at_limit = png_bytes(2048, 2048, png::BitDepth::One, png::ColorType::Grayscale);
        assert_eq!(
            decode(&at_limit, MAX_SIDE).expect("at the limit").size,
            [2048, 2048]
        );
        let over = png_bytes(2048, 2049, png::BitDepth::One, png::ColorType::Grayscale);
        assert!(decode(&over, MAX_SIDE).is_err());
        assert!(decode(&vec![0; MAX_PNG_BYTES + 1], MAX_SIDE).is_err());
    }

    #[test]
    fn every_color_type_decodes_to_rgba() {
        for color in [
            png::ColorType::Grayscale,
            png::ColorType::GrayscaleAlpha,
            png::ColorType::Rgb,
            png::ColorType::Rgba,
        ] {
            let bytes = png_bytes(5, 3, png::BitDepth::Eight, color);
            let image = decode(&bytes, MAX_SIDE).expect("decodes");
            assert_eq!(image.size, [5, 3]);
        }
    }

    #[test]
    fn malformed_png_falls_back_and_other_assets_load_independently() {
        let ctx = egui::Context::default();
        let valid = png_bytes(4, 4, png::BitDepth::Eight, png::ColorType::Rgba);
        for malformed in [b"malformed PNG".as_slice(), b"\x89PNG\r\n\x1a\n".as_slice()] {
            assert!(decode(malformed, MAX_SIDE).is_err());
            let art = Art::load(
                &ctx,
                BrandBytes {
                    backdrop: malformed,
                    emblem: malformed,
                    wordmark: malformed,
                },
            );
            assert!(art.backdrop.is_none() && art.emblem.is_none() && art.wordmark.is_none());
            for failed in 0..3 {
                let mut bytes = [valid.as_slice(); 3];
                bytes[failed] = malformed;
                let art = Art::load(
                    &ctx,
                    BrandBytes {
                        backdrop: bytes[0],
                        emblem: bytes[1],
                        wordmark: bytes[2],
                    },
                );
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
}
