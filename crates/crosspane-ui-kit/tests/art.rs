//! Palette, cover scaling and the bounded, independently falling-back art decoder.
//!
//! Everything runs headless on an egui context with no renderer: no display, no file writes. PNGs
//! are encoded in memory. Text needs fonts, which the kit never bundles: tests that lay text out
//! load a system font at runtime and skip, saying so, when there is none.
// Helper functions in an integration-test crate are not `#[test]` functions for clippy.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use crosspane_ui_kit::art::{Art, BrandBytes, cover_rect};
use crosspane_ui_kit::theme;
use egui::{Color32, Context, Pos2, Rect, Shape, TextureId, Vec2};

/// A system font for tests that lay text out. `CROSSPANE_TEST_FONT` (a font file path) wins;
/// otherwise the usual Linux and macOS locations are tried.
fn system_fonts() -> Option<egui::FontDefinitions> {
    let from_env = std::env::var_os("CROSSPANE_TEST_FONT").map(PathBuf::from);
    let known = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ]
    .map(PathBuf::from);
    for path in from_env.into_iter().chain(known) {
        if let Ok(bytes) = std::fs::read(&path)
            && !bytes.is_empty()
        {
            let mut fonts = egui::FontDefinitions::empty();
            fonts
                .font_data
                .insert("system".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.insert(family, vec!["system".into()]);
            }
            return Some(fonts);
        }
    }
    None
}

// ---- PNG fixtures, built in memory ------------------------------------------------------------

fn png(width: u32, height: u32, depth: png::BitDepth, color: png::ColorType) -> Vec<u8> {
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
        .write_image_data(&vec![0x7f; row_bytes * height as usize])
        .expect("pixels");
    writer.finish().expect("finish");
    out
}

fn rgba(width: u32, height: u32) -> Vec<u8> {
    png(width, height, png::BitDepth::Eight, png::ColorType::Rgba)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&checked);
    out.extend_from_slice(&crc32(&checked).to_be_bytes());
    out
}

/// A well-formed header that claims a huge image, followed by a token of image data. A decoder
/// that trusted the header would try to allocate a pixel buffer of tens of gigabytes.
fn dimension_bomb(width: u32, height: u32) -> Vec<u8> {
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend(chunk(b"IHDR", &header));
    out.extend(chunk(b"IDAT", &[0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff]));
    out.extend(chunk(b"IEND", &[]));
    out
}

fn malformed_cases() -> Vec<(&'static str, Vec<u8>)> {
    let valid = rgba(16, 16);
    let mut corrupt = valid.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0xff;
    vec![
        ("not a png", b"malformed PNG".to_vec()),
        ("signature only", b"\x89PNG\r\n\x1a\n".to_vec()),
        ("empty", Vec::new()),
        ("truncated", valid[..valid.len() / 2].to_vec()),
        ("corrupt data", corrupt),
        ("dimension bomb", dimension_bomb(100_000, 100_000)),
        ("wide bomb", dimension_bomb(u32::MAX, 1)),
        // Real, cheap-to-encode images that are over the pixel and side limits.
        (
            "too wide",
            png(5000, 2, png::BitDepth::One, png::ColorType::Grayscale),
        ),
        (
            "too many pixels",
            png(4096, 4096, png::BitDepth::One, png::ColorType::Grayscale),
        ),
        ("over the input limit", vec![0; 9 * 1024 * 1024]),
    ]
}

// ---- Observing what an Art draws --------------------------------------------------------------

/// What one drawing call put on screen.
#[derive(Debug, Default, PartialEq, Eq)]
struct Drawn {
    /// Images drawn with an uploaded texture.
    images: usize,
    /// Meshes drawn with the default (font) texture, such as the fallback gradient.
    gradients: usize,
    /// Whether the text "CROSSPANE" was drawn.
    wordmark_text: bool,
}

fn textured(shape: &Shape) -> bool {
    match shape {
        Shape::Mesh(mesh) => mesh.texture_id != TextureId::default(),
        Shape::Rect(rect) => rect
            .brush
            .as_ref()
            .is_some_and(|brush| brush.fill_texture_id != TextureId::default()),
        _ => false,
    }
}

fn observe(shape: &Shape, drawn: &mut Drawn) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|shape| observe(shape, drawn)),
        Shape::Text(text) => drawn.wordmark_text |= text.galley.text() == "CROSSPANE",
        shape if textured(shape) => drawn.images += 1,
        Shape::Mesh(_) => drawn.gradients += 1,
        _ => {}
    }
}

fn draw(ctx: &Context, mut ui_fn: impl FnMut(&mut egui::Ui)) -> Drawn {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0))),
            ..Default::default()
        },
        |ui| ui_fn(ui),
    );
    // No renderer here: discard texture upload and free notifications explicitly.
    output.textures_delta.clear();
    let mut drawn = Drawn::default();
    for clipped in &output.shapes {
        observe(&clipped.shape, &mut drawn);
    }
    drawn
}

fn load(ctx: &Context, bytes: [&[u8]; 3]) -> Art {
    Art::load(
        ctx,
        BrandBytes {
            backdrop: bytes[0],
            emblem: bytes[1],
            wordmark: bytes[2],
        },
    )
}

fn discard_texture_traffic(ctx: &Context) {
    ctx.tex_manager().write().take_delta().clear();
}

/// The three pieces, each drawn on its own so their output can be told apart.
fn pieces(ctx: &Context, art: &Art) -> [Drawn; 3] {
    let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0));
    [
        draw(ctx, |ui| art.background(ui.painter(), rect)),
        draw(ctx, |ui| art.emblem(ui, Vec2::new(56.0, 56.0))),
        draw(ctx, |ui| art.wordmark(ui, Vec2::new(260.0, 36.0))),
    ]
}

// ---- Tests ------------------------------------------------------------------------------------

#[test]
fn palette_and_dark_theme_construction() {
    assert_eq!(theme::MIDNIGHT.to_array(), [7, 21, 37, 255]);
    assert_eq!(theme::NAVY.to_array(), [22, 74, 116, 255]);
    assert_eq!(theme::FROST.to_array(), [23, 200, 244, 255]);
    assert_eq!(theme::GLACIER.to_array(), [111, 220, 255, 255]);
    assert_eq!(theme::ICE.to_array(), [233, 248, 255, 255]);
    assert_eq!(theme::PEER_ICE.to_array(), [183, 239, 255, 255]);
    assert_eq!(theme::QUIET.to_array(), [137, 203, 213, 255]);
    assert_eq!(theme::WARNING.to_array(), [217, 136, 145, 255]);
    let style = theme::style();
    assert!(style.visuals.dark_mode);
    assert_eq!(style.visuals.override_text_color, Some(theme::ICE));
    assert_eq!(style.visuals.panel_fill, theme::MIDNIGHT);
    assert_eq!(style.visuals.selection.stroke.color, theme::GLACIER);
    assert_eq!(style.text_styles[&egui::TextStyle::Heading].size, 24.0);
    assert_eq!(style.text_styles[&egui::TextStyle::Body].size, 14.0);
    assert_eq!(theme::glass().corner_radius, egui::CornerRadius::same(12));
    assert!(theme::glass().fill.a() < 255);
    // `alpha` keeps the colour and replaces only the opacity.
    let tinted = theme::alpha(theme::FROST, 65);
    assert_eq!(Color32::from_rgba_unmultiplied(23, 200, 244, 65), tinted);
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

#[test]
fn cover_of_an_image_without_size_is_the_destination_not_nan() {
    let target = Rect::from_min_size(Pos2::new(1.0, 2.0), Vec2::new(30.0, 20.0));
    for image in [Vec2::ZERO, Vec2::new(0.0, 10.0), Vec2::new(10.0, -1.0)] {
        assert_eq!(cover_rect(image, target), target);
    }
}

#[test]
fn valid_art_draws_every_piece() {
    let ctx = Context::default();
    let png = rgba(32, 18);
    let art = load(&ctx, [&png, &png, &png]);
    let [background, emblem, wordmark] = pieces(&ctx, &art);
    // The backdrop image, then its Midnight gradient and the vignette (no textures).
    assert_eq!(background.images, 1);
    assert!(background.gradients >= 1);
    assert_eq!(emblem.images, 1);
    assert_eq!(wordmark.images, 1);
    assert!(!wordmark.wordmark_text);
    drop(art);
    discard_texture_traffic(&ctx);
}

#[test]
fn malformed_and_hostile_pngs_are_refused_and_every_piece_falls_back() {
    let Some(fonts) = system_fonts() else {
        eprintln!("skipping: no system font for the text wordmark fallback");
        return;
    };
    let ctx = Context::default();
    ctx.set_fonts(fonts);
    for (name, bytes) in malformed_cases() {
        let art = load(&ctx, [&bytes, &bytes, &bytes]);
        let [background, emblem, wordmark] = pieces(&ctx, &art);
        // An opaque gradient and no image; nothing for the emblem; the text wordmark.
        assert_eq!(background.images, 0, "{name}");
        assert!(
            background.gradients >= 1,
            "{name}: the fallback must be opaque"
        );
        assert_eq!(emblem, Drawn::default(), "{name}");
        assert_eq!(wordmark.images, 0, "{name}");
        assert!(wordmark.wordmark_text, "{name}");
        drop(art);
        discard_texture_traffic(&ctx);
    }
}

#[test]
fn each_image_fails_independently_of_the_other_two() {
    let Some(fonts) = system_fonts() else {
        eprintln!("skipping: no system font for the text wordmark fallback");
        return;
    };
    let ctx = Context::default();
    ctx.set_fonts(fonts);
    let valid = rgba(32, 18);
    for (name, bad) in malformed_cases() {
        for failed in 0..3 {
            let mut bytes = [valid.as_slice(); 3];
            bytes[failed] = &bad;
            let art = load(&ctx, bytes);
            let [background, emblem, wordmark] = pieces(&ctx, &art);
            assert_eq!(
                background.images,
                usize::from(failed != 0),
                "{name} #{failed}"
            );
            assert!(background.gradients >= 1, "{name} #{failed}");
            assert_eq!(emblem.images, usize::from(failed != 1), "{name} #{failed}");
            assert_eq!(
                wordmark.images,
                usize::from(failed != 2),
                "{name} #{failed}"
            );
            assert_eq!(wordmark.wordmark_text, failed == 2, "{name} #{failed}");
            drop(art);
            discard_texture_traffic(&ctx);
        }
    }
}

#[test]
fn images_within_the_limits_decode_in_every_png_flavour() {
    let ctx = Context::default();
    let flavours = [
        png(7, 5, png::BitDepth::Eight, png::ColorType::Grayscale),
        png(7, 5, png::BitDepth::Eight, png::ColorType::GrayscaleAlpha),
        png(7, 5, png::BitDepth::Eight, png::ColorType::Rgb),
        png(7, 5, png::BitDepth::Eight, png::ColorType::Rgba),
        png(7, 5, png::BitDepth::Sixteen, png::ColorType::Rgba),
        png(7, 5, png::BitDepth::One, png::ColorType::Grayscale),
        // egui's default texture limit is 2048 points on a side: this is the widest it holds.
        png(2048, 1, png::BitDepth::One, png::ColorType::Grayscale),
    ];
    for bytes in &flavours {
        let art = load(&ctx, [bytes, bytes, bytes]);
        let [background, emblem, wordmark] = pieces(&ctx, &art);
        assert_eq!(
            (background.images, emblem.images, wordmark.images),
            (1, 1, 1)
        );
        drop(art);
        discard_texture_traffic(&ctx);
    }
}

#[test]
fn images_wider_than_the_renderers_texture_limit_fall_back_instead_of_panicking() {
    // Uploading an over-wide texture is a panic in egui's debug builds and a GPU validation
    // error in release builds, so the decoder must refuse it.
    let ctx = Context::default();
    let wide = png(2049, 1, png::BitDepth::One, png::ColorType::Grayscale);
    let art = load(&ctx, [&wide, &wide, &wide]);
    for piece in pieces(&ctx, &art) {
        assert_eq!(piece.images, 0);
    }
    drop(art);
    discard_texture_traffic(&ctx);
    // A renderer that reports a larger limit lets the same image through, up to the kit's own
    // ceiling of 4096.
    let mut output = ctx.run_ui(
        egui::RawInput {
            max_texture_side: Some(8192),
            ..Default::default()
        },
        |_| {},
    );
    output.textures_delta.clear();
    let art = load(&ctx, [&wide, &wide, &wide]);
    for piece in pieces(&ctx, &art) {
        assert_eq!(piece.images, 1);
    }
    drop(art);
    discard_texture_traffic(&ctx);
    let too_wide = png(4097, 1, png::BitDepth::One, png::ColorType::Grayscale);
    let art = load(&ctx, [&too_wide, &too_wide, &too_wide]);
    for piece in pieces(&ctx, &art) {
        assert_eq!(piece.images, 0);
    }
    drop(art);
    discard_texture_traffic(&ctx);
}
