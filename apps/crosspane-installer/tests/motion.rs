#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use crosspane_installer::demo;
use crosspane_installer::gui::load_review_font;
use crosspane_installer::*;
use crosspane_ui_kit::{
    art::{Art, BrandBytes},
    theme,
};
use eframe::egui;

fn test_font_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CROSSPANE_TEST_FONT") {
        return PathBuf::from(path);
    }
    const CANDIDATES: [&str; 6] = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
    ];
    for candidate in CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    panic!(
        "Set CROSSPANE_TEST_FONT to a readable system font; none of the candidates exists: {}",
        CANDIDATES.join(", ")
    );
}

#[test]
fn auto_unknown_and_reduced_are_static_full_is_explicit() {
    assert!(reduced_motion(MotionPreference::Auto, None));
    assert!(reduced_motion(MotionPreference::Auto, Some(true)));
    assert!(!reduced_motion(MotionPreference::Auto, Some(false)));
    assert!(reduced_motion(MotionPreference::Reduced, Some(false)));
    assert!(!reduced_motion(MotionPreference::Full, Some(true)));
}

#[test]
fn finite_transition_endpoints_are_bounded() {
    assert_eq!(transition_fraction(0, 200, false), 0.0);
    assert_eq!(transition_fraction(100, 200, false), 0.5);
    assert_eq!(transition_fraction(200, 200, false), 1.0);
    assert_eq!(transition_fraction(u64::MAX, 200, false), 1.0);
    assert_eq!(transition_fraction(0, 0, false), 1.0);
    assert_eq!(transition_fraction(0, 200, true), 1.0);
}

#[test]
fn idle_and_completed_success_do_not_schedule_a_perpetual_animation() {
    let ctx = egui::Context::default();
    ctx.set_fonts(load_review_font(&test_font_path()).unwrap());
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, theme::style());
    let art = Art::load(
        &ctx,
        BrandBytes {
            backdrop: &[],
            emblem: &[],
            wordmark: &[],
        },
    );
    let mut shell = WizardShell::default();
    let mut view = demo::fixture(ScreenId::Summary);
    view.motion = MotionPreference::Full;
    let original = view.clone();
    for now in [0, 100, 300, 1000, 2000, 3000] {
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                time: Some(now as f64 / 1000.0),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    assert!(shell.show(ui, &view, &art, now).is_empty());
                    if now >= 300 {
                        assert_eq!(ui.style().animation_time, 0.14);
                    }
                });
            },
        );
        if now == 3000 {
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                    >= Duration::from_millis(500)
            );
        }
        assert_eq!(view, original);
        output.drop_without_applying_deltas();
    }
}

#[test]
fn reduced_waiting_screen_updates_without_widget_motion() {
    let ctx = egui::Context::default();
    ctx.set_fonts(load_review_font(&test_font_path()).unwrap());
    let art = Art::load(
        &ctx,
        BrandBytes {
            backdrop: &[],
            emblem: &[],
            wordmark: &[],
        },
    );
    let mut shell = WizardShell::default();
    let view = demo::fixture(ScreenId::Installing);
    let output = ctx.run_ui(egui::RawInput::default(), |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            shell.show(ui, &view, &art, 0);
            assert_eq!(ui.style().animation_time, 0.0);
            assert_eq!(ui.style().scroll_animation.duration.max, 0.0);
        });
    });
    output.drop_without_applying_deltas();
    assert!(view.rows.iter().any(|row| row.state == RowState::Waiting));
}

fn illustration_context() -> (egui::Context, Art) {
    let ctx = egui::Context::default();
    ctx.set_fonts(load_review_font(&test_font_path()).unwrap());
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, theme::style());
    let art = Art::load(
        &ctx,
        BrandBytes {
            backdrop: &[],
            emblem: &[],
            wordmark: &[],
        },
    );
    (ctx, art)
}

fn paint(
    ctx: &egui::Context,
    shell: &mut WizardShell,
    art: &Art,
    view: &WizardView,
    now_ms: u64,
) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(now_ms as f64 / 1000.0),
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert!(shell.show(ui, view, art, now_ms).is_empty());
            });
        },
    );
    output.textures_delta.clear();
    output
}

fn visible_shapes(output: &egui::FullOutput) -> Vec<&egui::epaint::ClippedShape> {
    output
        .shapes
        .iter()
        .filter(|clipped| {
            // egui may retain fully transparent scrollbar shapes with sub-pixel
            // bookkeeping differences; they are not visible illustrative motion.
            !matches!(&clipped.shape, egui::epaint::Shape::Rect(rect)
            if rect.fill.a() == 0 && rect.stroke.color.a() == 0)
        })
        .collect()
}

#[test]
fn visible_illustrations_are_finite_reduced_static_and_failure_halts_motion() {
    let (ctx, art) = illustration_context();
    let mut shell = WizardShell::default();
    let mut view = demo::fixture(ScreenId::Welcome);
    view.motion = MotionPreference::Full;
    let original = view.clone();
    let start = paint(&ctx, &mut shell, &art, &view, 0);
    let middle = paint(&ctx, &mut shell, &art, &view, 300);
    let settled = paint(&ctx, &mut shell, &art, &view, 800);
    let idle = paint(&ctx, &mut shell, &art, &view, 2000);
    assert_ne!(start.shapes, middle.shapes);
    assert_eq!(settled.shapes, idle.shapes);
    assert!(
        idle.viewport_output[&egui::ViewportId::ROOT].repaint_delay >= Duration::from_millis(500)
    );
    assert_eq!(view, original);
    let (ctx, art) = illustration_context();
    let mut shell = WizardShell::default();
    view.motion = MotionPreference::Reduced;
    paint(&ctx, &mut shell, &art, &view, 0);
    let early = paint(&ctx, &mut shell, &art, &view, 100);
    let later = paint(&ctx, &mut shell, &art, &view, 2000);
    assert!(
        visible_shapes(&early) == visible_shapes(&later),
        "Reduced visible drawings changed"
    );
    let (ctx, art) = illustration_context();
    let mut shell = WizardShell::default();
    view.motion = MotionPreference::Full;
    paint(&ctx, &mut shell, &art, &view, 0);
    paint(&ctx, &mut shell, &art, &view, 100);
    view.rows.push(RowView {
        id: 90,
        label: "Paused after failure".into(),
        detail: "Retry is required".into(),
        state: RowState::Failed,
        human_confirmed: false,
    });
    paint(&ctx, &mut shell, &art, &view, 200);
    // The new failure card can expose a solid scrollbar and change content width.
    // Allow its finite scrollbar/layout settling before comparing paused drawing.
    paint(&ctx, &mut shell, &art, &view, 400);
    let stopped = paint(&ctx, &mut shell, &art, &view, 500);
    let later = paint(&ctx, &mut shell, &art, &view, 2500);
    assert!(stopped.shapes == later.shapes, "Paused drawings changed");
    assert!(
        later.viewport_output[&egui::ViewportId::ROOT].repaint_delay >= Duration::from_millis(500)
    );
}

#[test]
fn ready_settles_once_only_when_supplied_verification_changes_on_summary() {
    let (ctx, art) = illustration_context();
    let mut shell = WizardShell::default();
    let mut view = demo::fixture(ScreenId::Summary);
    view.motion = MotionPreference::Full;
    view.summary = SummaryView::InstalledWaiting;
    view.progress.completed.clear();
    view.rows[0].state = RowState::Waiting;
    let before = view.clone();
    paint(&ctx, &mut shell, &art, &view, 0);
    paint(&ctx, &mut shell, &art, &view, 5000);
    assert_eq!(view, before);
    view.summary = SummaryView::WorkspaceReady;
    view.rows[0].state = RowState::Verified;
    view.revision += 1;
    let verified = view.clone();
    let start = paint(&ctx, &mut shell, &art, &view, 5100);
    let settled = paint(&ctx, &mut shell, &art, &view, 5300);
    let idle = paint(&ctx, &mut shell, &art, &view, 8000);
    assert_ne!(start.shapes, settled.shapes);
    assert_eq!(settled.shapes, idle.shapes);
    assert_eq!(view, verified);
}

#[test]
fn choosing_full_runs_welcome_once_without_replaying_on_text_revisions() {
    let (ctx, art) = illustration_context();
    let mut shell = WizardShell::default();
    let mut view = demo::fixture(ScreenId::Welcome);
    view.motion = MotionPreference::Reduced;
    paint(&ctx, &mut shell, &art, &view, 0);
    paint(&ctx, &mut shell, &art, &view, 100);
    view.motion = MotionPreference::Full;
    view.revision += 1;
    let start = paint(&ctx, &mut shell, &art, &view, 500);
    let middle = paint(&ctx, &mut shell, &art, &view, 800);
    let settled = paint(&ctx, &mut shell, &art, &view, 1100);
    let idle = paint(&ctx, &mut shell, &art, &view, 3000);
    assert_ne!(start.shapes, middle.shapes);
    assert_eq!(settled.shapes, idle.shapes);
    view.message = "Review your keyboard, windows and sound across your computers. Review each change before it happens.".into();
    view.revision += 1;
    paint(&ctx, &mut shell, &art, &view, 3100);
    // Different system-font metrics can change wrapping, centering and scrollbar width.
    // Allow four layout frames, but less than the 600 ms welcome animation: a replay
    // must still be repainting when this bounded settling window ends.
    let mut idle = None;
    for now in [3200, 3300, 3400, 3500] {
        let output = paint(&ctx, &mut shell, &art, &view, now);
        if output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
            >= Duration::from_millis(500)
        {
            idle = Some((now, output));
            break;
        }
    }
    let (now, idle) = idle.expect("Text revision must settle without replaying welcome motion");
    let later = paint(&ctx, &mut shell, &art, &view, now + 100);
    assert_eq!(visible_shapes(&idle), visible_shapes(&later));
    assert!(
        later.viewport_output[&egui::ViewportId::ROOT].repaint_delay >= Duration::from_millis(500)
    );
}
