#![allow(clippy::unwrap_used, clippy::expect_used)] // Headless UI and explicitly injected pipes.
use clap::Parser;
use crosspane_installer::{fixture::*, tutorial_window::*};
use crosspane_installer_core::AttemptId;
use crosspane_types::id::{NodeId, WindowId};
use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2, Rect};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const WINDOW: WindowId = WindowId(901);
fn call(id: u64, command: FixtureCommand) -> FixtureCall {
    FixtureCall {
        id,
        attempt: AttemptId(7),
        command,
    }
}
fn open() -> FixtureCall {
    call(
        1,
        FixtureCommand::Open {
            machine_label: "Test machine".into(),
        },
    )
}
fn observe() -> WindowObservation {
    Ok((WINDOW, OwnWindowFacts::Unknown))
}
fn packet(call: FixtureCall) -> Vec<u8> {
    encode_control(&FixtureControlPacket {
        schema_version: 1,
        call,
    })
    .unwrap()
}
fn until(mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !f() {
        assert!(Instant::now() < until, "owned fake-pipe watchdog");
        thread::sleep(Duration::from_millis(1));
    }
}
fn font() -> PathBuf {
    let candidates = [
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Verdana.ttf",
    ];
    if let Some(path) = std::env::var_os("CROSSPANE_TEST_FONT") {
        let path = PathBuf::from(path);
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
        assert!(
            path.starts_with("/usr/share/fonts")
                || path.starts_with("/System/Library/Fonts")
                || path
                    .canonicalize()
                    .is_ok_and(|p| fixture.canonicalize().is_ok_and(|f| p == f))
        );
        return path;
    }
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| {
            panic!("Set CROSSPANE_TEST_FONT to a read-only system font: {candidates:?}")
        })
}
struct UiHarness {
    ctx: egui::Context,
    practice: Practice,
    next: u64,
    widgets: (Rect, Rect),
    output: Option<egui::FullOutput>,
    animation_time: f32,
    editor: egui::Id,
}
impl UiHarness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        ctx.set_fonts(crosspane_installer::gui::load_review_font(&font()).unwrap());
        ctx.set_style_of(egui::Theme::Dark, crosspane_ui_kit::theme::style());
        let mut practice = Practice::default();
        let opened = practice.handle(open(), observe()).unwrap();
        assert!(matches!(
            opened.result,
            Ok(FixtureEvent::Opened { window: WINDOW, .. })
        ));
        Self {
            ctx,
            practice,
            next: 2,
            widgets: (Rect::NOTHING, Rect::NOTHING),
            output: None,
            animation_time: 0.16,
            editor: egui::Id::NULL,
        }
    }
    fn command(&mut self, command: FixtureCommand) -> FixtureMessage {
        let id = self.next;
        self.next += 1;
        self.practice.handle(call(id, command), observe()).unwrap()
    }
    fn arm(&mut self, phase: u64) {
        let armed = self.command(FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(phase),
        });
        assert_eq!(
            armed.result,
            Ok(FixtureEvent::TargetArmed {
                fixture: FixtureId(1),
                phase: PhaseId(phase)
            })
        );
    }
    fn snapshot(&mut self) -> FixtureSnapshot {
        match self
            .command(FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            })
            .result
            .unwrap()
        {
            FixtureEvent::Snapshot { snapshot } => snapshot,
            event => panic!("expected snapshot: {event:?}"),
        }
    }
    fn frame(&mut self, now: u64, events: Vec<Event>) -> Result<(), FixtureError> {
        let mut result = None;
        self.output = Some(self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
                time: Some(now as f64 / 1000.0),
                events,
                ..Default::default()
            },
            |ui| {
                result = Some(self.practice.show(ui, now).map(|(target, text)| {
                    self.widgets = (target.rect, text.rect);
                    self.editor = text.id;
                }));
                self.animation_time = ui.style().animation_time;
            },
        ));
        self.output.as_mut().unwrap().textures_delta.clear(); // CPU-only: no texture renderer.
        result.unwrap()
    }
    fn pointer(&mut self, now: u64, pos: Pos2, button: PointerButton, pressed: bool) {
        self.frame(
            now,
            vec![
                Event::PointerMoved(pos),
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers: Modifiers::NONE,
                },
            ],
        )
        .unwrap();
    }
    fn click(&mut self, now: u64, target: Pos2) {
        self.pointer(now, target, PointerButton::Primary, true);
        self.pointer(now + 1, target, PointerButton::Primary, false);
    }
    fn type_text(&mut self, text: &str) {
        let text_rect = self.widgets.1;
        self.click(20, text_rect.center());
        self.frame(22, vec![Event::Text(text.into())]).unwrap();
        self.frame(23, vec![]).unwrap(); // Paint the newly edited volatile buffer.
    }
    fn labels(&self) -> Vec<String> {
        fn gather(shape: &egui::epaint::Shape, labels: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(text) => labels.push(text.galley.job.text.clone()),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        gather(shape, labels);
                    }
                }
                _ => {}
            }
        }
        let mut labels = Vec::new();
        for shape in &self.output.as_ref().unwrap().shapes {
            gather(&shape.shape, &mut labels);
        }
        labels
    }
}

fn key(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

#[test]
fn editor_copy_and_cut_never_export_volatile_text_in_output_commands() {
    for event in [Event::Copy, Event::Cut] {
        let mut h = UiHarness::new();
        h.frame(0, vec![]).unwrap();
        h.type_text("PRIVATE_CLIPBOARD_SENTINEL");
        let mut state = egui::TextEdit::load_state(&h.ctx, h.editor).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new("PRIVATE_CLIPBOARD_SENTINEL".chars().count()),
            )));
        state.store(&h.ctx, h.editor);
        h.frame(24, vec![event]).unwrap();
        assert!(
            h.output
                .as_ref()
                .unwrap()
                .platform_output
                .commands
                .iter()
                .all(|command| {
                    !matches!(command, egui::OutputCommand::CopyText(_))
                        && !format!("{command:?}").contains("PRIVATE_CLIPBOARD_SENTINEL")
                })
        );
    }
}

#[test]
fn primary_edge_release_and_held_pointer_keyboard_activation_never_count() {
    for activation in [None, Some(egui::Key::Space), Some(egui::Key::Enter)] {
        let mut h = UiHarness::new();
        h.arm(1);
        h.frame(0, vec![]).unwrap();
        let rect = h.widgets.0;
        let inside = Pos2::new(rect.max.x - 0.1, rect.center().y);
        h.pointer(1, inside, PointerButton::Primary, true);
        if let Some(activation) = activation {
            h.frame(2, vec![key(activation, Modifiers::NONE)]).unwrap();
            assert_eq!(h.snapshot().target_clicks, 0);
        } else {
            let outside = Pos2::new(rect.max.x + 0.1, rect.center().y);
            h.pointer(2, outside, PointerButton::Primary, false);
            assert_eq!(h.snapshot().target_clicks, 0);
        }
    }
}

#[test]
fn returning_drag_and_long_hold_follow_primary_click_classification_without_counting() {
    for drag in [true, false] {
        let mut h = UiHarness::new();
        h.arm(1);
        h.frame(0, vec![]).unwrap();
        let target = h.widgets.0.center();
        h.pointer(1, target, PointerButton::Primary, true);
        let release_at = if drag {
            h.frame(2, vec![Event::PointerMoved(Pos2::new(750.0, 580.0))])
                .unwrap();
            h.frame(3, vec![Event::PointerMoved(target)]).unwrap();
            4
        } else {
            let limit = h.ctx.options(|o| o.input_options.max_click_duration);
            let after = (limit * 1000.0).ceil() as u64 + 100;
            h.frame(after, vec![]).unwrap();
            after + 1
        };
        h.pointer(release_at, target, PointerButton::Primary, false);
        assert_eq!(h.snapshot().target_clicks, 0, "drag {drag}");
        h.click(release_at + 1, target);
        assert_eq!(h.snapshot().target_clicks, 1);
    }
}

#[test]
fn undo_after_clear_cancelled_close_and_stop_cannot_recover_typed_text() {
    for action in 0..4 {
        let mut h = UiHarness::new();
        h.frame(0, vec![]).unwrap();
        h.type_text("PRIVATE_UNDO_SENTINEL");
        h.frame(2000, vec![]).unwrap(); // Establish the renderer's undo checkpoint.
        if action == 0 {
            let clear = h
                .output
                .as_ref()
                .unwrap()
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::epaint::Shape::Text(t) if t.galley.job.text == "Clear test text" => {
                        Some(t.pos)
                    }
                    _ => None,
                })
                .unwrap();
            h.click(2001, clear + egui::vec2(10.0, 8.0));
            let text = h.widgets.1.center();
            h.click(2003, text);
        } else if action == 1 {
            assert!(matches!(
                h.practice.close_requested().unwrap().unwrap().result,
                Ok(FixtureEvent::CloseRequested { .. })
            ));
        } else {
            if action == 2 {
                h.practice.stop();
            } else {
                assert!(matches!(
                    h.command(FixtureCommand::Close {
                        fixture: FixtureId(1)
                    })
                    .result,
                    Ok(FixtureEvent::CloseRequested { .. })
                ));
            }
            assert!(egui::TextEdit::load_state(&h.ctx, h.editor).is_none());
            continue;
        }
        assert!(h.practice.text_is_empty());
        h.frame(2005, vec![key(egui::Key::Z, Modifiers::COMMAND)])
            .unwrap();
        h.frame(2006, vec![]).unwrap();
        assert!(h.practice.text_is_empty(), "clear action {action}");
        assert!(
            h.labels()
                .iter()
                .all(|label| !label.contains("PRIVATE_UNDO_SENTINEL"))
        );
    }
}

#[test]
fn received_calls_expire_without_gui_poll_or_response_and_release_the_pump() {
    for poll in [false, true] {
        let h = PipeHarness::new();
        h.input(packet(open()));
        until(|| h.wakes.load(Ordering::SeqCst) == 1);
        if poll {
            assert_eq!(h.channel().poll().len(), 1);
        }
        h.clock.store(2010, Ordering::SeqCst);
        h.failure(FixtureError::TimedOut);
        assert!(h.state.lock().unwrap().output.is_empty());
    }
}

#[test]
#[cfg(unix)]
fn malformed_font_subprocess_emits_only_fixed_codes_before_any_viewport() {
    use rustix::fs::{AtFlags, Mode, OFlags};
    struct OwnedFont {
        parent: rustix::fd::OwnedFd,
        root: rustix::fd::OwnedFd,
        name: String,
    }
    impl Drop for OwnedFont {
        fn drop(&mut self) {
            let _ = rustix::fs::unlinkat(&self.root, "PRIVATE_FONT_SENTINEL", AtFlags::empty());
            let _ = rustix::fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
        }
    }
    let temp = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let parent = rustix::fs::open(
        temp,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .unwrap();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "crosspane-tutorial-font-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    rustix::fs::mkdirat(&parent, name.as_str(), Mode::RWXU).unwrap(); // Exclusive; existence fails.
    let root = rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .unwrap();
    let fixture = OwnedFont { parent, root, name };
    let stat = rustix::fs::fstat(&fixture.root).unwrap();
    assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
    assert_eq!(stat.st_mode & 0o777, 0o700);
    let file = rustix::fs::openat(
        &fixture.root,
        "PRIVATE_FONT_SENTINEL",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .unwrap();
    let mut file = std::fs::File::from(file);
    file.write_all(&[0, 1, 0, 0]).unwrap(); // Accepted header, intentionally invalid body.
    drop(file);
    let output = Command::new(env!("CARGO_BIN_EXE_crosspane-tutorial"))
        .args(["--controlled", "--font"])
        .arg(
            PathBuf::from(temp)
                .join(&fixture.name)
                .join("PRIVATE_FONT_SENTINEL"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    drop(fixture); // Descriptor-confined cleanup precedes assertions and also runs on unwind.
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "crosspane-tutorial: panic\ncrosspane-tutorial: unavailable\n"
    );
}

#[test]
fn unarmed_click_and_ack_are_not_clicks_primary_press_release_counts_once() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    let target = h.widgets.0.center();
    h.click(10, target);
    assert_eq!(h.snapshot().target_clicks, 0);
    h.arm(11);
    h.frame(12, vec![]).unwrap();
    assert_eq!(h.snapshot().target_clicks, 0);
    h.pointer(13, target, PointerButton::Primary, true);
    assert_eq!(h.snapshot().target_clicks, 0);
    h.pointer(14, target, PointerButton::Primary, false);
    assert_eq!(h.snapshot().target_clicks, 1);
    h.frame(14, vec![]).unwrap();
    assert_eq!(h.snapshot().target_clicks, 1);
    h.pointer(15, target, PointerButton::Secondary, true);
    h.pointer(16, target, PointerButton::Secondary, false);
    assert_eq!(h.snapshot().target_clicks, 1);
}

#[test]
fn phase_change_discards_old_press_and_preserves_cumulative_baselines() {
    let mut h = UiHarness::new();
    h.arm(3);
    h.frame(0, vec![]).unwrap();
    let target = h.widgets.0.center();
    h.click(10, target);
    h.frame(500, vec![]).unwrap();
    let before = h.snapshot();
    assert_eq!((before.pattern_ticks, before.target_clicks), (2, 1));
    h.pointer(501, target, PointerButton::Primary, true);
    h.arm(4);
    h.pointer(502, target, PointerButton::Primary, false);
    let after = h.snapshot();
    assert_eq!(after.phase, Some(PhaseId(4)));
    assert_eq!((after.pattern_ticks, after.target_clicks), (2, 1));
    h.click(503, target);
    assert_eq!(h.snapshot().target_clicks, 2);
    let id = h.next;
    assert_eq!(
        h.practice.handle(
            call(
                id,
                FixtureCommand::ArmTarget {
                    fixture: FixtureId(1),
                    phase: PhaseId(4),
                }
            ),
            observe()
        ),
        Err(FixtureError::NotOwned)
    );
}

#[test]
fn release_outside_target_and_press_before_arm_do_not_count() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    let target = h.widgets.0.center();
    h.pointer(1, target, PointerButton::Primary, true);
    h.arm(1);
    h.pointer(2, target, PointerButton::Primary, false);
    assert_eq!(h.snapshot().target_clicks, 0);
    h.pointer(3, target, PointerButton::Primary, true);
    h.pointer(4, Pos2::new(750.0, 580.0), PointerButton::Primary, false);
    assert_eq!(h.snapshot().target_clicks, 0);
}

#[test]
fn primary_press_and_release_in_one_frame_is_a_complete_current_phase_click() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    let target = h.widgets.0.center();
    let events = || {
        [true, false]
            .into_iter()
            .flat_map(|pressed| {
                [
                    Event::PointerMoved(target),
                    Event::PointerButton {
                        pos: target,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: Modifiers::NONE,
                    },
                ]
            })
            .collect()
    };
    h.frame(1, events()).unwrap();
    assert_eq!(h.snapshot().target_clicks, 0);
    h.arm(1);
    h.frame(2, vec![]).unwrap(); // Publish the enabled target's hit-test state.
    h.frame(3, events()).unwrap();
    assert_eq!(h.snapshot().target_clicks, 1);
    h.frame(4, vec![]).unwrap();
    assert_eq!(h.snapshot().target_clicks, 1);
}

#[test]
fn two_hz_pattern_counts_submitted_updates_not_elapsed_or_presented_frames() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    assert!(h.labels().iter().any(|s| s == "Pattern 1"));
    assert!(
        h.labels()
            .iter()
            .any(|s| s.contains("Test machine") && s.contains("attempt 7"))
    );
    for now in [0, 1, 250, 499] {
        h.frame(now, vec![]).unwrap();
    }
    assert_eq!(h.snapshot().pattern_ticks, 1);
    h.frame(500, vec![]).unwrap();
    assert_eq!(h.snapshot().pattern_ticks, 2);
    assert!(h.labels().iter().any(|s| s == "Pattern 2"));
    h.frame(5000, vec![]).unwrap();
    assert_eq!(h.snapshot().pattern_ticks, 3); // No catch-up frames were actually submitted.
    h.frame(5100, vec![]).unwrap();
    // egui subtracts its predicted frame duration from the requested 400 ms delay.
    let delay = h.output.as_ref().unwrap().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    assert!(
        (Duration::from_millis(380)..=Duration::from_millis(400)).contains(&delay),
        "{delay:?}"
    );
}

#[test]
fn reduced_motion_still_changes_discrete_pattern_and_stops_transition_animation() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    // The checkbox is immediately after two label rows; resolve its actual painted label.
    let text = h
        .output
        .as_ref()
        .unwrap()
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.job.text == "Reduced motion" => Some(t.pos),
            _ => None,
        })
        .unwrap();
    h.click(10, text + egui::vec2(10.0, 8.0));
    h.frame(20, vec![]).unwrap();
    assert_eq!(h.animation_time, 0.0);
    let before = h.snapshot().pattern_ticks;
    h.frame(500, vec![]).unwrap();
    assert_eq!(h.snapshot().pattern_ticks, before + 1);
    assert!(h.labels().iter().any(|s| s == "Pattern 2"));
}

#[test]
fn volatile_unicode_text_is_bounded_excluded_from_wire_debug_and_cleared_on_close_loss() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    h.type_text(&format!("PRIVATE_SENTINEL{}", "界".repeat(200)));
    assert!(!h.practice.text_is_empty());
    let painted = h
        .labels()
        .iter()
        .find(|s| s.starts_with("PRIVATE_SENTINEL"))
        .cloned()
        .unwrap_or_else(|| panic!("injected labels: {:?}", h.labels()));
    assert_eq!(painted.chars().count(), 128);
    let snapshot = h.command(FixtureCommand::ObserveWindow {
        fixture: FixtureId(1),
    });
    let bytes = encode_event(&FixtureEventPacket {
        schema_version: 1,
        message: snapshot,
    })
    .unwrap();
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains("PRIVATE_SENTINEL")
    );
    assert!(!format!("{:?}", h.practice).contains("PRIVATE_SENTINEL"));
    let requested = h.practice.close_requested().unwrap().unwrap();
    assert_eq!(requested.call_id, None);
    assert_eq!(
        requested.result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    assert!(h.practice.text_is_empty());
    h.type_text("PRIVATE_SENTINEL");
    assert!(!h.practice.text_is_empty());
    h.practice.stop();
    assert!(h.practice.text_is_empty());
    assert_eq!(h.frame(30, vec![]), Err(FixtureError::ChannelClosed));
}

#[test]
fn clear_control_and_commanded_close_clear_text_without_claiming_return_or_closed() {
    let mut h = UiHarness::new();
    h.frame(0, vec![]).unwrap();
    h.type_text("PRIVATE_SENTINEL");
    let clear = h
        .output
        .as_ref()
        .unwrap()
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.job.text == "Clear test text" => Some(t.pos),
            _ => None,
        })
        .unwrap();
    h.click(24, clear + egui::vec2(10.0, 8.0));
    assert!(h.practice.text_is_empty());
    h.type_text("PRIVATE_SENTINEL");
    let closed = h.command(FixtureCommand::Close {
        fixture: FixtureId(1),
    });
    assert_eq!(
        closed.result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    assert!(h.practice.text_is_empty());
    assert_eq!(h.frame(40, vec![]), Err(FixtureError::ChannelClosed));
    let packet = String::from_utf8(
        encode_event(&FixtureEventPacket {
            schema_version: 1,
            message: closed,
        })
        .unwrap(),
    )
    .unwrap();
    for invented in [
        "restored",
        "parking",
        "frames_presented",
        "\"event\":\"closed\"",
    ] {
        assert!(!packet.contains(invented));
    }
}

#[test]
fn unavailable_dispatch_never_fabricates_window_ids_or_tone_success_on_any_os() {
    let mut practice = Practice::default();
    let opened = practice.handle(open(), unavailable_window()).unwrap();
    assert_eq!(opened.result, Err(FixtureError::Unavailable));
    assert_eq!(opened.sequence, 1);
    let observed = practice
        .handle(
            call(
                2,
                FixtureCommand::ObserveWindow {
                    fixture: FixtureId(1),
                },
            ),
            unavailable_window(),
        )
        .unwrap();
    assert_eq!(observed.result, Err(FixtureError::Unavailable));
    assert_eq!(
        practice.handle(
            call(
                3,
                FixtureCommand::ArmTarget {
                    fixture: FixtureId(1),
                    phase: PhaseId(1)
                }
            ),
            unavailable_window()
        ),
        Err(FixtureError::NotOwned)
    );
    let peer = NodeId([3; 32]);
    for (id, command) in [
        (
            3,
            FixtureCommand::PlayTone {
                fixture: FixtureId(1),
                output: SpeakersSelection {
                    peer,
                    device_key: format!("crosspane.{peer}.speaker"),
                },
            },
        ),
        (
            4,
            FixtureCommand::StopTone {
                fixture: FixtureId(1),
                tone: ToneId(3),
            },
        ),
    ] {
        assert_eq!(
            practice
                .handle(call(id, command), unavailable_window())
                .unwrap()
                .result,
            Err(FixtureError::Unavailable)
        );
    }
}

#[test]
fn literal_unknown_visibility_and_display_facts_are_preserved_without_restore_policy() {
    let mut practice = Practice::default();
    practice.handle(open(), observe()).unwrap();
    for (id, facts) in [
        OwnWindowFacts::Unknown,
        OwnWindowFacts::Present {
            visible_on_user_workspace: None,
            on_initial_display: None,
        },
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(false),
            on_initial_display: Some(true),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let message = practice
            .handle(
                call(
                    id as u64 + 2,
                    FixtureCommand::ObserveWindow {
                        fixture: FixtureId(1),
                    },
                ),
                Ok((WINDOW, facts.clone())),
            )
            .unwrap();
        let FixtureEvent::Snapshot { snapshot } = message.result.unwrap() else {
            panic!()
        };
        assert_eq!(snapshot.window_facts, facts);
        assert_eq!(snapshot.tone, OwnToneState::Stopped);
    }
    let changed = practice
        .handle(
            call(
                5,
                FixtureCommand::ObserveWindow {
                    fixture: FixtureId(1),
                },
            ),
            Ok((WindowId(902), OwnWindowFacts::Missing)),
        )
        .unwrap();
    assert_eq!(changed.result, Err(FixtureError::UnknownWindow));
}

#[test]
fn ownership_attempt_call_phase_and_time_exhaustion_fail_without_positive_evidence() {
    let mut practice = Practice::default();
    let mut bad = open();
    bad.id = 0;
    assert!(practice.handle(bad, observe()).is_err());
    practice.handle(open(), observe()).unwrap();
    assert_eq!(
        practice.handle(open(), observe()),
        Err(FixtureError::InvalidMessage)
    );
    let mut wrong = call(
        2,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    );
    wrong.attempt = AttemptId(8);
    assert_eq!(
        practice.handle(wrong, observe()),
        Err(FixtureError::InvalidMessage)
    );
    assert_eq!(
        practice.handle(
            call(
                2,
                FixtureCommand::ObserveWindow {
                    fixture: FixtureId(2)
                }
            ),
            observe()
        ),
        Err(FixtureError::NotOwned)
    );
    let mut h = UiHarness::new();
    assert_eq!(
        h.frame(u64::MAX, vec![]),
        Err(FixtureError::CounterExhausted)
    );
    let mut exhausted = Practice::default();
    let mut last = open();
    last.id = u64::MAX;
    exhausted.handle(last.clone(), observe()).unwrap();
    assert_eq!(
        exhausted.handle(last, observe()),
        Err(FixtureError::CounterExhausted)
    );
}

#[derive(Default)]
struct PipeState {
    input: VecDeque<u8>,
    output: Vec<u8>,
    eof: bool,
    blocked: bool,
    writes: usize,
    partial: usize,
    partial_time: Option<u64>,
    receipt_time: Option<u64>,
    block_after: Option<usize>,
}
struct Reader {
    state: Arc<Mutex<PipeState>>,
    clock: Arc<AtomicU64>,
    released: Arc<AtomicUsize>,
}
impl Read for Reader {
    fn read(&mut self, byte: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        if let Some(b) = state.input.pop_front() {
            byte[0] = b;
            if b == b'\n'
                && let Some(at) = state.receipt_time
            {
                self.clock.store(at, Ordering::SeqCst);
            }
            Ok(1)
        } else if state.eof {
            Ok(0)
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}
struct Writer {
    state: Arc<Mutex<PipeState>>,
    clock: Arc<AtomicU64>,
    released: Arc<AtomicUsize>,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        state.writes += 1;
        if state.blocked || state.block_after.is_some_and(|at| state.output.len() >= at) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = if state.partial == 0 {
            bytes.len()
        } else {
            state.partial.min(bytes.len())
        };
        state.output.extend_from_slice(&bytes[..n]);
        if let Some(at) = state.partial_time {
            self.clock.store(at, Ordering::SeqCst);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}
struct PipeHarness {
    channel: Option<ChildChannel>,
    state: Arc<Mutex<PipeState>>,
    clock: Arc<AtomicU64>,
    released: Arc<AtomicUsize>,
    wakes: Arc<AtomicUsize>,
}
impl PipeHarness {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(PipeState::default()));
        let clock = Arc::new(AtomicU64::new(10));
        let released = Arc::new(AtomicUsize::new(0));
        let now = clock.clone();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake = wakes.clone();
        let channel = ChildChannel::new(
            Reader {
                state: state.clone(),
                clock: clock.clone(),
                released: released.clone(),
            },
            Writer {
                state: state.clone(),
                clock: clock.clone(),
                released: released.clone(),
            },
            Arc::new(move || now.load(Ordering::SeqCst)),
            Arc::new(move || {
                wake.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        Self {
            channel: Some(channel),
            state,
            clock,
            released,
            wakes,
        }
    }
    fn channel(&self) -> &ChildChannel {
        self.channel.as_ref().unwrap()
    }
    fn input(&self, bytes: Vec<u8>) {
        self.state.lock().unwrap().input.extend(bytes);
    }
    fn received(&self, n: usize) -> Vec<ReceivedCall> {
        let mut all = Vec::new();
        until(|| {
            all.extend(self.channel().poll());
            all.len() >= n
        });
        assert_eq!(all.len(), n);
        all
    }
    fn failure(&self, reason: FixtureError) {
        until(|| self.channel().failure().is_some());
        assert_eq!(self.channel().failure(), Some(reason));
        until(|| self.released.load(Ordering::SeqCst) == 2);
    }
    fn finish(&mut self) {
        drop(self.channel.take());
        until(|| self.released.load(Ordering::SeqCst) == 2);
    }
}
fn unavailable_message(sequence: u64) -> FixtureMessage {
    FixtureMessage {
        call_id: Some(sequence),
        attempt: AttemptId(7),
        sequence,
        result: Err(FixtureError::Unavailable),
    }
}

#[test]
fn normal_close_waits_for_complete_reply_and_partial_writes_or_deadline_failure() {
    for partial in [false, true] {
        let mut h = PipeHarness::new();
        let mut practice = Practice::default();
        practice.handle(open(), observe()).unwrap();
        {
            let mut state = h.state.lock().unwrap();
            state.blocked = !partial;
            state.partial = if partial { 3 } else { 0 };
            state.block_after = partial.then_some(3);
        }
        h.input(packet(call(
            2,
            FixtureCommand::Close {
                fixture: FixtureId(1),
            },
        )));
        until(|| h.wakes.load(Ordering::SeqCst) == 1);
        assert_eq!(practice.process(h.channel(), 10, false), Ok(false));
        until(|| h.state.lock().unwrap().writes > 0);
        assert_eq!(practice.process(h.channel(), 11, true), Ok(false));
        assert_eq!(
            h.state.lock().unwrap().output.len(),
            if partial { 3 } else { 0 }
        );
        {
            let mut state = h.state.lock().unwrap();
            state.blocked = false;
            state.block_after = None;
        }
        until(|| h.state.lock().unwrap().output.last() == Some(&b'\n'));
        until(|| practice.process(h.channel(), 12, false).unwrap());
        let message = decode_event(&h.state.lock().unwrap().output)
            .unwrap()
            .message;
        assert_eq!(message.call_id, Some(2));
        assert_eq!(
            message.result,
            Ok(FixtureEvent::CloseRequested {
                fixture: FixtureId(1)
            })
        );
        h.finish();
    }
    let h = PipeHarness::new();
    let mut practice = Practice::default();
    practice.handle(open(), observe()).unwrap();
    h.state.lock().unwrap().blocked = true;
    h.input(packet(call(
        2,
        FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    assert_eq!(practice.process(h.channel(), 10, false), Ok(false));
    h.clock.store(2010, Ordering::SeqCst);
    h.failure(FixtureError::TimedOut);
    assert_eq!(
        practice.process(h.channel(), 2010, false),
        Err(FixtureError::TimedOut)
    );
    assert!(h.state.lock().unwrap().output.is_empty());
}

#[test]
fn cross_frame_viewport_close_commands_cancel_only_pending_shutdown_and_then_exit() {
    struct ViewportCycle {
        ctx: egui::Context,
        programmatic_request: bool,
        exited: bool,
    }
    impl ViewportCycle {
        fn frame(
            &mut self,
            practice: &mut Practice,
            channel: &ChildChannel,
            now: u64,
            user_close: bool,
        ) -> Vec<egui::ViewportCommand> {
            let requested = user_close || self.programmatic_request;
            let mut input = egui::RawInput {
                time: Some(now as f64 / 1000.0),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))),
                ..Default::default()
            };
            if requested {
                input
                    .viewports
                    .entry(egui::ViewportId::ROOT)
                    .or_default()
                    .events
                    .push(egui::ViewportEvent::Close);
            }
            let mut output = self
                .ctx
                .run_ui(input, |ui| practice.viewport(ui.ctx(), channel, now));
            output.textures_delta.clear();
            let commands = output
                .viewport_output
                .remove(&egui::ViewportId::ROOT)
                .unwrap()
                .commands;
            // Pinned eframe's root close gate reads CancelClose from this actual frame output.
            self.exited = requested && !commands.contains(&egui::ViewportCommand::CancelClose);
            // Pinned egui-winit maps a programmatic Close into the NEXT frame's Close event.
            self.programmatic_request = commands.contains(&egui::ViewportCommand::Close);
            commands
        }
    }
    for failed in [false, true] {
        let mut h = PipeHarness::new();
        let mut practice = Practice::default();
        practice.handle(open(), observe()).unwrap();
        let ctx = egui::Context::default();
        ctx.set_fonts(crosspane_installer::gui::load_review_font(&font()).unwrap());
        let mut cycle = ViewportCycle {
            ctx,
            programmatic_request: false,
            exited: false,
        };
        if failed {
            h.state.lock().unwrap().eof = true;
            h.failure(FixtureError::ChannelClosed);
        } else {
            h.state.lock().unwrap().blocked = true;
            h.input(packet(call(
                2,
                FixtureCommand::Close {
                    fixture: FixtureId(1),
                },
            )));
            until(|| h.wakes.load(Ordering::SeqCst) == 1);
            let pending = cycle.frame(&mut practice, h.channel(), 10, true);
            assert!(pending.contains(&egui::ViewportCommand::CancelClose));
            assert!(!pending.contains(&egui::ViewportCommand::Close));
            assert!(!cycle.exited);
            h.state.lock().unwrap().blocked = false;
            until(|| h.channel().written(2));
        }
        let completed = cycle.frame(&mut practice, h.channel(), 11, false);
        assert!(completed.contains(&egui::ViewportCommand::Close));
        assert!(!completed.contains(&egui::ViewportCommand::CancelClose));
        assert!(!cycle.exited); // Close is a request for the next frame, not an immediate exit.
        let next = cycle.frame(&mut practice, h.channel(), 12, false);
        assert!(!next.contains(&egui::ViewportCommand::CancelClose));
        assert!(cycle.exited, "terminal failure {failed}");
        h.finish();
    }
}

#[test]
fn rejected_gui_command_aborts_blocked_pending_output_before_any_next_write() {
    for kind in 0..4 {
        let h = PipeHarness::new();
        let mut practice = Practice::default();
        practice.handle(open(), observe()).unwrap();
        practice
            .handle(
                call(
                    2,
                    FixtureCommand::ArmTarget {
                        fixture: FixtureId(1),
                        phase: PhaseId(1),
                    },
                ),
                observe(),
            )
            .unwrap();
        h.state.lock().unwrap().blocked = true;
        h.input(packet(call(
            3,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        )));
        until(|| h.wakes.load(Ordering::SeqCst) == 1);
        assert_eq!(practice.process(h.channel(), 10, false), Ok(false));
        until(|| h.state.lock().unwrap().writes > 0);
        let mut rejected = call(
            4,
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(2),
            },
        );
        match kind {
            0 => rejected.attempt = AttemptId(8),
            1 => rejected.id = 3,
            2 => {
                rejected.command = FixtureCommand::ObserveWindow {
                    fixture: FixtureId(9),
                }
            }
            3 => {
                rejected.command = FixtureCommand::ArmTarget {
                    fixture: FixtureId(1),
                    phase: PhaseId(1),
                }
            }
            _ => unreachable!(),
        }
        h.input(packet(rejected));
        until(|| h.wakes.load(Ordering::SeqCst) >= 2);
        assert!(practice.process(h.channel(), 11, false).is_err());
        h.state.lock().unwrap().blocked = false;
        until(|| {
            !h.state.lock().unwrap().output.is_empty() || h.released.load(Ordering::SeqCst) == 2
        });
        assert!(
            h.state.lock().unwrap().output.is_empty(),
            "rejection kind {kind}"
        );
        assert_eq!(h.released.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn fake_pipe_partial_packets_and_replies_keep_complete_receipt_time_and_strict_codec() {
    let mut h = PipeHarness::new();
    h.state.lock().unwrap().receipt_time = Some(13);
    let bytes = packet(open());
    h.input(bytes[..20].to_vec());
    thread::sleep(Duration::from_millis(5));
    assert!(h.channel().poll().is_empty());
    h.input(bytes[20..].to_vec());
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    h.clock.store(1000, Ordering::SeqCst);
    let received = h.received(1);
    assert_eq!(received[0].call, open());
    assert_eq!(received[0].received_at_ms, 13);
    h.state.lock().unwrap().partial = 3;
    h.channel()
        .send(unavailable_message(1), received[0].received_at_ms)
        .unwrap();
    until(|| h.state.lock().unwrap().output.last() == Some(&b'\n'));
    assert_eq!(
        decode_event(&h.state.lock().unwrap().output)
            .unwrap()
            .message,
        unavailable_message(1)
    );
    assert!(h.state.lock().unwrap().writes > 1);
    h.finish();
}

#[test]
fn fake_pipe_input_queue32_overflow_corruption_oversize_and_eof_release_owned_streams() {
    for error in [
        FixtureError::Busy,
        FixtureError::InvalidMessage,
        FixtureError::ChannelClosed,
    ] {
        let h = PipeHarness::new();
        match error {
            FixtureError::Busy => {
                for id in 1..=33 {
                    let mut call = open();
                    call.id = id;
                    h.input(packet(call));
                }
            }
            FixtureError::InvalidMessage => h.input(vec![b'x'; MAX_LINE_BYTES + 1]),
            FixtureError::ChannelClosed => h.state.lock().unwrap().eof = true,
            _ => unreachable!(),
        }
        h.failure(error);
        if error == FixtureError::Busy {
            assert_eq!(h.channel().poll().len(), MAX_QUEUE);
        }
    }
    let h = PipeHarness::new();
    h.input(b"{\"schema_version\":2}\n".to_vec());
    h.failure(FixtureError::InvalidMessage);
}

#[test]
fn fake_pipe_output_queue32_deadline_and_partial_timeout_never_resend() {
    let mut h = PipeHarness::new();
    h.state.lock().unwrap().blocked = true;
    h.channel().send(unavailable_message(1), 10).unwrap();
    until(|| h.state.lock().unwrap().writes > 0);
    for seq in 2..=33 {
        h.channel().send(unavailable_message(seq), 10).unwrap();
    }
    assert_eq!(
        h.channel().send(unavailable_message(34), 10),
        Err(FixtureError::Busy)
    );
    h.clock.store(2010, Ordering::SeqCst);
    h.failure(FixtureError::TimedOut);
    assert!(h.state.lock().unwrap().output.is_empty());
    h.finish();
    let h = PipeHarness::new();
    {
        let mut state = h.state.lock().unwrap();
        state.partial = 3;
        state.partial_time = Some(2010);
    }
    h.channel().send(unavailable_message(1), 10).unwrap();
    h.failure(FixtureError::TimedOut);
    assert_eq!(h.state.lock().unwrap().output.len(), 3);
    assert_eq!(h.state.lock().unwrap().writes, 1);
}

#[test]
fn delayed_gui_response_does_not_renew_deadline_and_timestamp_overflow_starts_no_write() {
    let h = PipeHarness::new();
    h.input(packet(open()));
    let received = h.received(1);
    h.clock.store(2010, Ordering::SeqCst);
    h.failure(FixtureError::TimedOut);
    assert_eq!(
        h.channel()
            .send(unavailable_message(1), received[0].received_at_ms),
        Err(FixtureError::ChannelClosed)
    );
    assert!(h.state.lock().unwrap().output.is_empty());
    let mut h = PipeHarness::new();
    assert_eq!(
        h.channel().send(unavailable_message(1), u64::MAX),
        Err(FixtureError::CounterExhausted)
    );
    assert!(h.state.lock().unwrap().output.is_empty());
    h.finish();
}

#[test]
fn controlled_only_cli_help_and_usage_never_open_viewport_or_echo_input() {
    let help = Command::new(env!("CARGO_BIN_EXE_crosspane-tutorial"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("--controlled"));
    assert!(text.contains("--font"));
    for args in [
        vec![],
        vec!["--controlled"],
        vec!["--font", "/SECRET_SENTINEL"],
        vec!["--controlled", "--font", "relative"],
        vec!["SECRET_SENTINEL"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crosspane-tutorial"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            error,
            "crosspane-tutorial: usage: --controlled --font <absolute-path>\n"
        );
        assert!(!error.contains("SECRET_SENTINEL"));
    }
    let valid = TutorialOptions::try_parse_from([
        "crosspane-tutorial",
        "--controlled",
        "--font",
        "/usr/share/fonts/example.ttf",
    ])
    .unwrap();
    assert_eq!(
        valid.validate().unwrap(),
        std::path::Path::new("/usr/share/fonts/example.ttf")
    );
}

#[derive(Debug)]
struct NativeFacts {
    window: Option<WindowObservation>,
    observations: Vec<(u64, String)>,
    tone: OwnToneState,
    plays: Vec<(ToneId, SpeakersSelection)>,
    stops: Vec<ToneId>,
    play_pending: bool,
    stop_pending: bool,
}
impl Default for NativeFacts {
    fn default() -> Self {
        Self {
            window: None,
            observations: Vec::new(),
            tone: OwnToneState::Stopped,
            plays: Vec::new(),
            stops: Vec::new(),
            play_pending: false,
            stop_pending: false,
        }
    }
}
struct NativeFake(Arc<Mutex<NativeFacts>>);
impl TutorialNative for NativeFake {
    fn observe_window(&mut self, call_id: u64, title: &str) -> Option<WindowObservation> {
        let mut facts = self.0.lock().unwrap();
        facts.observations.push((call_id, title.into()));
        facts.window.clone()
    }
    fn play_tone(
        &mut self,
        tone: ToneId,
        output: &SpeakersSelection,
    ) -> Option<Result<(), FixtureError>> {
        let mut facts = self.0.lock().unwrap();
        facts.plays.push((tone, output.clone()));
        if facts.stops.contains(&tone) {
            return Some(Err(FixtureError::Unavailable));
        }
        facts.tone = OwnToneState::Running { tone };
        (!facts.play_pending).then_some(Ok(()))
    }
    fn stop_tone(&mut self, tone: ToneId) -> Option<Result<(), FixtureError>> {
        let mut facts = self.0.lock().unwrap();
        facts.stops.push(tone);
        if facts.stop_pending {
            facts.tone = OwnToneState::StopUnconfirmed { tone };
            None
        } else {
            facts.tone = OwnToneState::Stopped;
            Some(Ok(()))
        }
    }
    fn tone_state(&self) -> OwnToneState {
        self.0.lock().unwrap().tone.clone()
    }
}
fn native_practice() -> (Practice, Arc<Mutex<NativeFacts>>) {
    let facts = Arc::new(Mutex::new(NativeFacts::default()));
    (
        Practice::with_native(Box::new(NativeFake(facts.clone()))),
        facts,
    )
}
fn tone_call(id: u64) -> FixtureCall {
    call(
        id,
        FixtureCommand::PlayTone {
            fixture: FixtureId(1),
            output: SpeakersSelection {
                peer: NodeId([7; 32]),
                device_key: format!("crosspane.{}.speaker", NodeId([7; 32])),
            },
        },
    )
}
fn native_messages(h: &PipeHarness, n: usize) -> Vec<FixtureMessage> {
    until(|| {
        h.state
            .lock()
            .unwrap()
            .output
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            == n
    });
    h.state
        .lock()
        .unwrap()
        .output
        .split_inclusive(|b| *b == b'\n')
        .map(|b| decode_event(b).unwrap().message)
        .collect()
}

#[test]
fn deferred_open_applies_title_before_observation_and_holds_later_calls() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    h.input(packet(open()));
    h.input(packet(call(
        2,
        FixtureCommand::ArmTarget {
            fixture: FixtureId(1),
            phase: PhaseId(1),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) == 2);
    let ctx = egui::Context::default();
    let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
        practice.viewport(ui.ctx(), h.channel(), 10)
    });
    frame.textures_delta.clear(); // CPU-only frame: no native renderer consumes textures.
    let title = practice_title("Test machine", AttemptId(7), FixtureId(1)).unwrap();
    assert!(
        frame
            .viewport_output
            .values()
            .flat_map(|v| &v.commands)
            .any(|c| matches!(c, egui::ViewportCommand::Title(t) if t.as_str() == title.as_str()))
    );
    assert!(
        frame.viewport_output[&egui::ViewportId::ROOT].repaint_delay <= Duration::from_millis(16)
    );
    assert!(facts.lock().unwrap().observations.is_empty());
    for now in [11, 500, 1000] {
        assert_eq!(practice.process(h.channel(), now, false), Ok(false));
        assert!(h.state.lock().unwrap().output.is_empty());
    }
    facts.lock().unwrap().window = Some(observe());
    assert_eq!(practice.process(h.channel(), 1500, false), Ok(false));
    let messages = native_messages(&h, 2);
    assert_eq!(messages[0].call_id, Some(1));
    assert!(matches!(
        messages[0].result,
        Ok(FixtureEvent::Opened { window: WINDOW, .. })
    ));
    assert_eq!(
        messages[1].result,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(1),
            phase: PhaseId(1)
        })
    );
    assert!(
        facts
            .lock()
            .unwrap()
            .observations
            .iter()
            .all(|(id, t)| *id == 1 && t == &title)
    );
    h.finish();
}

#[test]
fn deferred_open_rejects_missing_zero_and_errors_until_matching_observation() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    h.input(packet(open()));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    for observation in [
        Err(FixtureError::UnknownWindow),
        Ok((WindowId(0), OwnWindowFacts::Unknown)),
        Ok((WINDOW, OwnWindowFacts::Missing)),
    ] {
        facts.lock().unwrap().window = Some(observation);
        practice.process(h.channel(), 11, false).unwrap();
        assert!(h.state.lock().unwrap().output.is_empty());
    }
    facts.lock().unwrap().window = Some(observe());
    practice.process(h.channel(), 12, false).unwrap();
    assert!(matches!(
        native_messages(&h, 1)[0].result,
        Ok(FixtureEvent::Opened { .. })
    ));
    h.finish();
}

#[test]
fn deferred_open_no_observation_expires_aborts_and_never_sends_late_opened() {
    let h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    h.input(packet(open()));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    practice.process(h.channel(), 2009, false).unwrap();
    h.clock.store(2010, Ordering::SeqCst);
    h.failure(FixtureError::TimedOut);
    facts.lock().unwrap().window = Some(observe());
    assert_eq!(
        practice.process(h.channel(), 2010, false),
        Err(FixtureError::TimedOut)
    );
    assert!(h.state.lock().unwrap().output.is_empty());
    assert_eq!(facts.lock().unwrap().observations.len(), 1);
}

#[test]
fn observe_window_polls_fresh_call_and_snapshot_uses_native_tone_state() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    h.input(packet(call(
        2,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    assert!(h.state.lock().unwrap().output.is_empty());
    facts.lock().unwrap().window = Some(Ok((
        WINDOW,
        OwnWindowFacts::Present {
            visible_on_user_workspace: None,
            on_initial_display: Some(false),
        },
    )));
    facts.lock().unwrap().tone = OwnToneState::StopUnconfirmed { tone: ToneId(99) };
    practice.process(h.channel(), 11, false).unwrap();
    let messages = native_messages(&h, 1);
    let Ok(FixtureEvent::Snapshot { snapshot }) = &messages[0].result else {
        panic!("snapshot")
    };
    assert_eq!(
        snapshot.tone,
        OwnToneState::StopUnconfirmed { tone: ToneId(99) }
    );
    assert_eq!(
        snapshot.window_facts,
        OwnWindowFacts::Present {
            visible_on_user_workspace: None,
            on_initial_display: Some(false),
        }
    );
    facts.lock().unwrap().window = Some(Err(FixtureError::UnknownWindow));
    h.input(packet(call(
        3,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) >= 3); // First input, reply write, then fresh input.
    practice.process(h.channel(), 12, false).unwrap();
    assert_eq!(
        native_messages(&h, 2)[1].result,
        Err(FixtureError::UnknownWindow)
    );
    assert_eq!(
        facts
            .lock()
            .unwrap()
            .observations
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [2, 2, 3]
    );
    h.finish();
}

#[test]
fn pending_play_reuses_tone_id_and_later_native_stop_emits_once_uncorrelated() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    facts.lock().unwrap().play_pending = true;
    h.input(packet(tone_call(2)));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    for now in [10, 11] {
        practice.process(h.channel(), now, false).unwrap();
    }
    assert!(h.state.lock().unwrap().output.is_empty());
    facts.lock().unwrap().play_pending = false;
    practice.process(h.channel(), 12, false).unwrap();
    assert_eq!(
        native_messages(&h, 1)[0].result,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(1),
            tone: ToneId(2)
        })
    );
    assert!(
        facts
            .lock()
            .unwrap()
            .plays
            .iter()
            .all(|(tone, output)| *tone == ToneId(2)
                && output.device_key == format!("crosspane.{}.speaker", output.peer))
    );
    facts.lock().unwrap().tone = OwnToneState::StopUnconfirmed { tone: ToneId(2) };
    practice.process(h.channel(), 13, false).unwrap();
    assert_eq!(
        h.state
            .lock()
            .unwrap()
            .output
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        1
    );
    facts.lock().unwrap().tone = OwnToneState::Stopped;
    practice.process(h.channel(), 14, false).unwrap();
    let messages = native_messages(&h, 2);
    assert_eq!(messages[1].call_id, None);
    assert_eq!(
        messages[1].result,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2)
        })
    );
    practice.process(h.channel(), 15, false).unwrap();
    assert_eq!(
        h.state
            .lock()
            .unwrap()
            .output
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        2
    );
    h.finish();
}

#[test]
fn explicit_stop_waits_for_native_confirmation_and_has_no_duplicate_transition() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    practice.handle(tone_call(2), observe()).unwrap();
    facts.lock().unwrap().stop_pending = true;
    h.input(packet(call(
        3,
        FixtureCommand::StopTone {
            fixture: FixtureId(1),
            tone: ToneId(2),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    assert_eq!(
        facts.lock().unwrap().tone,
        OwnToneState::StopUnconfirmed { tone: ToneId(2) }
    );
    assert!(h.state.lock().unwrap().output.is_empty());
    facts.lock().unwrap().stop_pending = false;
    facts.lock().unwrap().tone = OwnToneState::Stopped; // Worker completed between frames.
    practice.process(h.channel(), 11, false).unwrap();
    let messages = native_messages(&h, 1);
    assert_eq!(messages[0].call_id, Some(3));
    assert_eq!(
        messages[0].result,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2)
        })
    );
    practice.process(h.channel(), 12, false).unwrap();
    assert_eq!(
        h.state
            .lock()
            .unwrap()
            .output
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        1
    );
    h.finish();
}

#[test]
fn native_tone_is_stopped_on_close_cancel_drop_and_pending_play_timeout() {
    for lifecycle in 0..4 {
        let (mut practice, facts) = native_practice();
        practice.handle(open(), observe()).unwrap();
        practice.handle(tone_call(2), observe()).unwrap();
        match lifecycle {
            0 => {
                practice.close_requested().unwrap();
            }
            1 => {
                practice.stop();
            }
            2 => {
                practice
                    .handle(
                        call(
                            3,
                            FixtureCommand::Close {
                                fixture: FixtureId(1),
                            },
                        ),
                        observe(),
                    )
                    .unwrap();
            }
            _ => {
                drop(practice);
            }
        }
        assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
        assert!(facts.lock().unwrap().stops.contains(&ToneId(2)));
    }
    let h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    facts.lock().unwrap().play_pending = true;
    h.input(packet(tone_call(2)));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    assert_eq!(
        facts.lock().unwrap().tone,
        OwnToneState::Running { tone: ToneId(2) }
    );
    h.clock.store(2010, Ordering::SeqCst);
    h.failure(FixtureError::TimedOut);
    assert_eq!(
        practice.process(h.channel(), 2010, false),
        Err(FixtureError::TimedOut)
    );
    assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
    assert!(h.state.lock().unwrap().output.is_empty());
}

#[test]
fn wrong_attempt_or_fixture_never_reaches_native_tone_and_unavailable_stays_explicit() {
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    let mut foreign = tone_call(2);
    foreign.attempt = AttemptId(8);
    assert_eq!(
        practice.handle(foreign, observe()),
        Err(FixtureError::InvalidMessage)
    );
    let mut foreign = tone_call(2);
    let FixtureCommand::PlayTone { fixture, .. } = &mut foreign.command else {
        unreachable!()
    };
    *fixture = FixtureId(2);
    assert_eq!(
        practice.handle(foreign, observe()),
        Err(FixtureError::NotOwned)
    );
    assert!(facts.lock().unwrap().plays.is_empty());
    let mut unavailable = UnavailableTutorial;
    assert_eq!(
        unavailable.observe_window(1, "owned-only"),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(
        unavailable.play_tone(
            ToneId(2),
            &SpeakersSelection {
                peer: NodeId([7; 32]),
                device_key: format!("crosspane.{}.speaker", NodeId([7; 32]))
            }
        ),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(
        unavailable.stop_tone(ToneId(2)),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(unavailable.tone_state(), OwnToneState::Stopped);
}

#[test]
fn native_close_during_pending_tone_disables_output_and_same_tone_never_restarts() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    facts.lock().unwrap().play_pending = true;
    h.input(packet(tone_call(2)));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    practice.process(h.channel(), 11, true).unwrap();
    assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
    assert_eq!(
        native_messages(&h, 1)[0].result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    facts.lock().unwrap().play_pending = false;
    practice.process(h.channel(), 12, false).unwrap();
    let messages = native_messages(&h, 2);
    assert_eq!(messages[1].call_id, Some(2));
    assert_eq!(messages[1].result, Err(FixtureError::Unavailable));
    assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::ToneStarted { .. })))
    );
    h.finish();
}

#[test]
fn review_native_close_cancels_plays_queued_behind_pending_observation() {
    let mut h = PipeHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    h.input(packet(call(
        2,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    )));
    h.input(packet(tone_call(3)));
    h.input(packet(tone_call(4)));
    until(|| h.wakes.load(Ordering::SeqCst) == 3);
    practice.process(h.channel(), 10, false).unwrap();
    practice.process(h.channel(), 11, true).unwrap();
    assert_eq!(
        native_messages(&h, 1)[0].result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    facts.lock().unwrap().window = Some(observe());
    practice.process(h.channel(), 12, false).unwrap();
    assert!(
        facts.lock().unwrap().plays.is_empty(),
        "native Close permanently cancels queued starts"
    );
    let messages = native_messages(&h, 4);
    assert!(matches!(
        messages[1].result,
        Ok(FixtureEvent::Snapshot { .. })
    ));
    assert_eq!(messages[2].result, Err(FixtureError::Unavailable));
    assert_eq!(messages[3].result, Err(FixtureError::Unavailable));
    h.finish();
}

#[test]
fn review_invalid_pending_observe_aborts_before_seam_and_before_blocked_reply_escapes() {
    for kind in 0..3 {
        let h = PipeHarness::new();
        let (mut practice, facts) = native_practice();
        practice.handle(open(), observe()).unwrap();
        practice.handle(tone_call(2), observe()).unwrap();
        h.state.lock().unwrap().blocked = true;
        h.input(packet(call(
            3,
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(1),
            },
        )));
        until(|| h.wakes.load(Ordering::SeqCst) == 1);
        practice.process(h.channel(), 10, false).unwrap();
        until(|| h.state.lock().unwrap().writes > 0);
        let mut rejected = call(
            4,
            FixtureCommand::ObserveWindow {
                fixture: FixtureId(1),
            },
        );
        let expected = match kind {
            0 => {
                rejected.attempt = AttemptId(8);
                FixtureError::InvalidMessage
            }
            1 => {
                rejected.command = FixtureCommand::ObserveWindow {
                    fixture: FixtureId(9),
                };
                FixtureError::NotOwned
            }
            _ => {
                rejected.id = 2;
                FixtureError::InvalidMessage
            }
        };
        h.input(packet(rejected));
        until(|| h.wakes.load(Ordering::SeqCst) >= 2);
        assert_eq!(practice.process(h.channel(), 11, false), Err(expected));
        assert!(facts.lock().unwrap().observations.is_empty());
        assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
        h.state.lock().unwrap().blocked = false;
        until(|| h.released.load(Ordering::SeqCst) == 2);
        assert!(h.state.lock().unwrap().output.is_empty());
    }
}

#[derive(Default)]
struct PauseState {
    armed: bool,
    entered: bool,
    released: bool,
}
struct PausedReader {
    reader: Reader,
    pause: Arc<Mutex<PauseState>>,
}
impl Read for PausedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let paused = {
            let mut pause = self.pause.lock().unwrap();
            if pause.armed {
                pause.armed = false;
                pause.entered = true;
                true
            } else {
                false
            }
        };
        if paused {
            until(|| self.pause.lock().unwrap().released); // Bounded fake-only pump pause.
        }
        self.reader.read(bytes)
    }
}
struct ReleasePump(Arc<Mutex<PauseState>>);
impl Drop for ReleasePump {
    fn drop(&mut self) {
        self.0.lock().unwrap().released = true;
    }
}
fn pausable_harness() -> (PipeHarness, ReleasePump) {
    let state = Arc::new(Mutex::new(PipeState::default()));
    let clock = Arc::new(AtomicU64::new(10));
    let released = Arc::new(AtomicUsize::new(0));
    let wakes = Arc::new(AtomicUsize::new(0));
    let pause = Arc::new(Mutex::new(PauseState::default()));
    let now = clock.clone();
    let wake = wakes.clone();
    let channel = ChildChannel::new(
        PausedReader {
            reader: Reader {
                state: state.clone(),
                clock: clock.clone(),
                released: released.clone(),
            },
            pause: pause.clone(),
        },
        Writer {
            state: state.clone(),
            clock: clock.clone(),
            released: released.clone(),
        },
        Arc::new(move || now.load(Ordering::SeqCst)),
        Arc::new(move || {
            wake.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .unwrap();
    (
        PipeHarness {
            channel: Some(channel),
            state,
            clock,
            released,
            wakes,
        },
        ReleasePump(pause),
    )
}

#[test]
fn review_saturated_ordinary_replies_retain_auto_stop_sequence_and_original_deadline() {
    for expires in [false, true] {
        let (mut h, release) = pausable_harness();
        let (mut practice, facts) = native_practice();
        practice.handle(open(), observe()).unwrap();
        practice.handle(tone_call(2), observe()).unwrap();
        for id in 3..35 {
            h.input(packet(call(
                id,
                FixtureCommand::ArmTarget {
                    fixture: FixtureId(1),
                    phase: PhaseId(id),
                },
            )));
        }
        until(|| h.wakes.load(Ordering::SeqCst) == 32);
        release.0.lock().unwrap().armed = true;
        until(|| release.0.lock().unwrap().entered);
        facts.lock().unwrap().tone = OwnToneState::Stopped;
        assert_eq!(practice.process(h.channel(), 10, false), Ok(false));
        assert!(h.state.lock().unwrap().output.is_empty());
        assert_eq!(h.channel().failure(), None);
        // A retry while the pump is still paused must neither increment nor reorder sequence.
        assert_eq!(practice.process(h.channel(), 2009, false), Ok(false));
        drop(release);
        native_messages(&h, 32);
        until(|| h.channel().written(34)); // Ordinary deadlines cannot mask the retained event.
        if expires {
            h.clock.store(2010, Ordering::SeqCst);
            assert_eq!(h.channel().failure(), None);
            assert_eq!(
                practice.process(h.channel(), 2010, false),
                Err(FixtureError::TimedOut)
            );
            until(|| h.released.load(Ordering::SeqCst) == 2);
            assert_eq!(native_messages(&h, 32).last().unwrap().sequence, 34);
            h.finish();
        } else {
            practice.process(h.channel(), 2009, false).unwrap();
            let messages = native_messages(&h, 33);
            assert_eq!(
                messages.iter().map(|m| m.sequence).collect::<Vec<_>>(),
                (3..36).collect::<Vec<_>>()
            );
            assert_eq!(messages[32].call_id, None);
            assert_eq!(
                messages[32].result,
                Ok(FixtureEvent::ToneStopped {
                    fixture: FixtureId(1),
                    tone: ToneId(2)
                })
            );
            assert_eq!(h.channel().failure(), None);
            h.finish();
        }
    }
}

struct RacingStopNative(NativeFake);
impl TutorialNative for RacingStopNative {
    fn observe_window(&mut self, id: u64, title: &str) -> Option<WindowObservation> {
        self.0.observe_window(id, title)
    }
    fn play_tone(
        &mut self,
        tone: ToneId,
        output: &SpeakersSelection,
    ) -> Option<Result<(), FixtureError>> {
        self.0.play_tone(tone, output)
    }
    fn stop_tone(&mut self, tone: ToneId) -> Option<Result<(), FixtureError>> {
        let result = self.0.stop_tone(tone);
        if result.is_none() {
            let mut facts = self.0.0.lock().unwrap();
            facts.stop_pending = false;
            facts.tone = OwnToneState::Stopped; // Worker completes immediately after returning None.
        }
        result
    }
    fn tone_state(&self) -> OwnToneState {
        self.0.tone_state()
    }
}

#[test]
fn review_stop_completion_between_none_and_state_read_emits_one_correlated_stop() {
    let mut h = PipeHarness::new();
    let facts = Arc::new(Mutex::new(NativeFacts::default()));
    let mut practice = Practice::with_native(Box::new(RacingStopNative(NativeFake(facts.clone()))));
    practice.handle(open(), observe()).unwrap();
    practice.handle(tone_call(2), observe()).unwrap();
    facts.lock().unwrap().stop_pending = true;
    h.input(packet(call(
        3,
        FixtureCommand::StopTone {
            fixture: FixtureId(1),
            tone: ToneId(2),
        },
    )));
    until(|| h.wakes.load(Ordering::SeqCst) == 1);
    practice.process(h.channel(), 10, false).unwrap();
    assert_eq!(facts.lock().unwrap().tone, OwnToneState::Stopped);
    assert!(
        h.state.lock().unwrap().output.is_empty(),
        "pending Stop owns the notification"
    );
    practice.process(h.channel(), 11, false).unwrap();
    let messages = native_messages(&h, 1);
    assert_eq!(messages[0].call_id, Some(3));
    assert_eq!(
        messages[0].result,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2)
        })
    );
    practice.process(h.channel(), 12, false).unwrap();
    assert_eq!(
        h.state
            .lock()
            .unwrap()
            .output
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        1
    );
    h.finish();
}

#[test]
fn verify_native_close_clears_text_undo_and_press_before_saturated_lifecycle_retry() {
    let (mut pipes, release) = pausable_harness();
    let mut ui = UiHarness::new();
    let (mut practice, facts) = native_practice();
    practice.handle(open(), observe()).unwrap();
    ui.practice = practice;
    ui.frame(0, vec![]).unwrap();
    ui.type_text("PRIVATE_SATURATED_CLOSE_SENTINEL");
    ui.frame(2000, vec![]).unwrap(); // Establish an undo checkpoint before Close.
    ui.practice.handle(tone_call(2), observe()).unwrap();
    pipes.clock.store(3000, Ordering::SeqCst);
    for id in 3..35 {
        pipes.input(packet(call(
            id,
            FixtureCommand::ArmTarget {
                fixture: FixtureId(1),
                phase: PhaseId(id),
            },
        )));
    }
    until(|| pipes.wakes.load(Ordering::SeqCst) == 32);
    release.0.lock().unwrap().armed = true;
    until(|| release.0.lock().unwrap().entered);
    facts.lock().unwrap().tone = OwnToneState::Stopped;
    assert_eq!(ui.practice.process(pipes.channel(), 3000, false), Ok(false));
    ui.frame(3000, vec![]).unwrap();
    ui.pointer(3001, ui.widgets.0.center(), PointerButton::Primary, true);
    assert!(!ui.practice.text_is_empty());
    assert!(egui::TextEdit::load_state(&ui.ctx, ui.editor).is_some());
    assert_eq!(ui.practice.process(pipes.channel(), 3002, true), Ok(false));
    assert!(
        ui.practice.text_is_empty(),
        "native Close clears before Busy retry"
    );
    assert!(egui::TextEdit::load_state(&ui.ctx, ui.editor).is_none());
    ui.pointer(3003, ui.widgets.0.center(), PointerButton::Primary, false);
    ui.click(3004, ui.widgets.1.center());
    ui.frame(3006, vec![key(egui::Key::Z, Modifiers::COMMAND)])
        .unwrap();
    ui.frame(3007, vec![]).unwrap();
    assert!(ui.practice.text_is_empty());
    assert!(
        ui.labels()
            .iter()
            .all(|label| !label.contains("PRIVATE_SATURATED_CLOSE_SENTINEL"))
    );
    assert_eq!(pipes.channel().failure(), None);
    drop(release);
    native_messages(&pipes, 32);
    until(|| pipes.channel().written(34));
    pipes.clock.store(3008, Ordering::SeqCst);
    ui.practice.process(pipes.channel(), 3008, false).unwrap();
    let messages = native_messages(&pipes, 34);
    assert_eq!(
        messages[32].result,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(1),
            tone: ToneId(2)
        })
    );
    assert_eq!(
        messages[33].result,
        Ok(FixtureEvent::CloseRequested {
            fixture: FixtureId(1)
        })
    );
    ui.next = 35;
    assert_eq!(
        ui.snapshot().target_clicks,
        0,
        "the pre-close primary press cannot count"
    );
    pipes.finish();
}
