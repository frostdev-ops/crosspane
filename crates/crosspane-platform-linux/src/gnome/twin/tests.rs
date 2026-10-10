//! The twin manager against the fakes of `testing`: the creation and growth sequence, its failure
//! paths, the linger, the loss handling, the snapshot refresh, the fence hook and the capture
//! router. No Mutter, no portal, no Shell.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, PlatformError, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::id::{DisplayId, WindowId};

use super::capture::TWIN_STREAM_BASE;
use super::testing::{Rig, TEST_TIMING, wait_until};
use super::{GnomeTwin, Timing, TwinView};
use crate::gnome::display_config::LogicalConfig;
use crate::wayland_outputs::display_id;

fn px(width: u32, height: u32) -> PixelSize {
    PixelSize::new(width, height)
}

fn twin_id() -> DisplayId {
    display_id("Meta-0")
}

fn config(
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connector: &str,
    mode: &str,
) -> LogicalConfig {
    LogicalConfig {
        x,
        y,
        scale,
        transform,
        primary,
        monitors: vec![(connector.to_owned(), mode.to_owned())],
    }
}

/// The user's three monitors as `testing::World::new` lays them out, plus the twin.
fn expected_layout(twin: LogicalConfig) -> Vec<LogicalConfig> {
    vec![
        config(0, 0, 1.0, 0, true, "DP-3", "3440x1440@60.000"),
        config(3440, 0, 1.0, 1, false, "DP-2", "1920x1080@60.000"),
        config(4520, 0, 1.0, 0, false, "HDMI-1", "1920x1080@60.000"),
        twin,
    ]
}

fn ensure(rig: &Rig, window: u64, width: u32, height: u32) -> Result<TwinView, PlatformError> {
    rig.twin.ensure(window, px(width, height), 2.0)
}

/// Collects what the loss listeners are told.
fn collect_lost(twin: &GnomeTwin) -> Arc<Mutex<Vec<Vec<u64>>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    twin.on_lost(Arc::new(move |victims| {
        sink.lock().unwrap().push(victims.to_vec());
    }));
    seen
}

#[test]
fn a_new_twin_runs_the_whole_sequence_in_order() {
    let rig = Rig::new();
    let view = ensure(&rig, 5, 1800, 1169).unwrap();
    // 1800x1169 plus headroom, in whole steps.
    assert_eq!(
        rig.world.log(),
        [
            "save_layout 1",
            "open 2304x1472",
            "apply",
            "restore_layout 1 skip=[5]",
            "set_fence 6440,0 1152x736",
        ]
    );
    // The user's three monitors exactly as stored (rotation included), the twin at the right
    // edge of the rightmost one, top-aligned, at the destination's scale.
    let applies = rig.world.applies();
    assert_eq!(applies.len(), 1);
    assert_eq!(
        applies[0].1,
        expected_layout(config(6440, 0, 2.0, 0, false, "Meta-0", "2304x1472@60.000"))
    );
    assert!(view.changed);
    let display = &view.display;
    assert_eq!(display.id, twin_id());
    assert_eq!(display.geometry.pixel_size, px(2304, 1472));
    assert_eq!(display.geometry.scale, 2.0);
    assert_eq!(display.geometry.logical_origin.x, 6440.0);
    assert_eq!(display.geometry.logical_origin.y, 0.0);
    assert_eq!(rig.twin.parked(), [5]);
    assert_eq!(rig.twin.view().unwrap().display, view.display);
    // The layout is the user's again.
    let state = rig.world.lock().logical.clone();
    assert_eq!(state.len(), 4);
    assert_eq!(state[1].transform, 1);
}

#[test]
fn a_fractional_destination_scale_falls_to_one_that_gives_whole_pixels() {
    let rig = Rig::new();
    let view = rig.twin.ensure(5, px(1800, 1169), 1.5).unwrap();
    // 2304x1472 is not divisible by 1.5 (or 1.25): scale 1.0, logical = device pixels.
    assert_eq!(view.display.geometry.scale, 1.0);
    assert_eq!(
        rig.world.applies()[0].1.last().unwrap().scale,
        1.0,
        "the twin's logical monitor is applied at scale 1"
    );
    assert!(
        rig.world
            .log()
            .contains(&"set_fence 6440,0 2304x1472".to_owned())
    );
}

#[test]
fn a_window_that_fits_reuses_the_twin_without_a_sequence() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.clear_log();
    let view = ensure(&rig, 6, 1000, 900).unwrap();
    assert!(!view.changed);
    assert!(rig.world.log().is_empty());
    assert_eq!(rig.world.screens_made(), 1);
    assert_eq!(rig.twin.parked(), [5, 6]);
}

#[test]
fn a_bigger_window_grows_the_twin_and_keeps_the_scale() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.clear_log();
    let view = ensure(&rig, 6, 2400, 1000).unwrap();
    // Width 2400 > 2304 grows to 3000 -> 3008; the height fits and stays.
    assert_eq!(
        rig.world.log(),
        [
            "save_layout 2",
            "resize 3008x1472",
            "apply",
            "restore_layout 2 skip=[5, 6]",
            "set_fence 6440,0 1504x736",
        ]
    );
    assert!(view.changed);
    assert_eq!(view.display.geometry.pixel_size, px(3008, 1472));
    assert_eq!(view.display.geometry.scale, 2.0);
    let applies = rig.world.applies();
    assert_eq!(
        applies[1].1,
        expected_layout(config(6440, 0, 2.0, 0, false, "Meta-0", "3008x1472@60.000"))
    );
    assert_eq!(rig.world.screens_made(), 1, "the same screen was resized");
}

#[test]
fn a_stale_serial_is_read_again_and_retried_up_to_three_times() {
    let rig = Rig::new();
    rig.world.lock().serial_races = 3;
    ensure(&rig, 5, 1800, 1169).unwrap();
    assert_eq!(rig.world.applies().len(), 1, "the fourth attempt applied");
    let rig = Rig::new();
    rig.world.lock().serial_races = 4;
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    assert!(rig.world.applies().is_empty());
    assert_eq!(rig.world.screens_dropped(), 1);
}

#[test]
fn a_refused_layout_closes_the_screen_restores_the_frames_and_is_unsupported() {
    let rig = Rig::new();
    rig.world
        .lock()
        .fail_apply
        .push_back(PlatformError::Backend(
            "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
                .into(),
        ));
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    // The screen goes first (Mutter puts its stored layout back), then the saved frames.
    assert_eq!(
        rig.world.log(),
        [
            "save_layout 1",
            "open 2304x1472",
            "screen dropped",
            "restore_layout 1 skip=[5]",
        ]
    );
    assert!(rig.twin.view().is_none());
    assert!(rig.twin.parked().is_empty());
    let (logical, stored) = {
        let inner = rig.world.lock();
        (inner.logical.clone(), inner.stored.clone())
    };
    assert_eq!(logical, stored);
    // The next try starts from scratch.
    ensure(&rig, 5, 1800, 1169).unwrap();
    assert_eq!(rig.world.screens_made(), 2);
}

#[test]
fn a_twin_that_never_appears_times_out_as_unsupported() {
    let rig = Rig::new();
    rig.world.lock().hide_twin = true;
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    assert_eq!(rig.world.screens_dropped(), 1);
    assert!(rig.world.applies().is_empty());
}

#[test]
fn another_virtual_monitor_is_left_alone() {
    let rig = Rig::new();
    rig.world.lock().foreign = true;
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    assert!(rig.world.log().is_empty(), "nothing was saved or opened");
    assert_eq!(rig.world.screens_made(), 0);
}

#[test]
fn no_consent_is_unsupported_and_the_saved_frames_are_released() {
    let rig = Rig::new();
    rig.world.lock().fail_open = Some(PlatformError::Unsupported(
        "no stored consent for a virtual screen",
    ));
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(
        error,
        PlatformError::Unsupported("no stored consent for a virtual screen")
    ));
    assert_eq!(
        rig.world.log(),
        ["save_layout 1", "restore_layout 1 skip=[5]"]
    );
    assert!(rig.world.applies().is_empty());
}

#[test]
fn a_failed_save_is_unsupported_before_anything_is_created() {
    let rig = Rig::new();
    rig.world.lock().fail_save = Some(PlatformError::Timeout);
    let error = ensure(&rig, 5, 1800, 1169).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    assert_eq!(rig.world.screens_made(), 0);
    assert!(rig.world.log().is_empty());
}

#[test]
fn a_closed_gate_is_locked_for_a_new_twin() {
    let rig = Rig::new();
    rig.world.lock().fail_open = Some(PlatformError::Locked);
    assert!(matches!(
        ensure(&rig, 5, 1800, 1169),
        Err(PlatformError::Locked)
    ));
    assert!(rig.twin.parked().is_empty());
}

#[test]
fn a_closed_gate_leaves_a_live_twin_alone() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.lock().fail_resize = Some(PlatformError::Locked);
    assert!(matches!(
        ensure(&rig, 6, 3000, 1000),
        Err(PlatformError::Locked)
    ));
    // Nothing was torn down: the window on the twin stays hidden, and the new one is not on it.
    assert_eq!(rig.world.screens_dropped(), 0);
    assert!(rig.twin.view().is_some());
    assert_eq!(rig.twin.parked(), [5]);
    // After the gate reopens the growth works.
    ensure(&rig, 6, 3000, 1000).unwrap();
    assert_eq!(rig.world.screens_made(), 1);
}

#[test]
fn restore_and_fence_failures_never_fail_the_sequence() {
    let rig = Rig::new();
    {
        let mut inner = rig.world.lock();
        inner.fail_restore = true;
        inner.fail_fence = true;
    }
    let view = ensure(&rig, 5, 1800, 1169).unwrap();
    assert!(view.changed);
    assert!(rig.world.lock().fence.is_none());
}

#[test]
fn content_no_virtual_monitor_can_hold_is_unsupported_without_a_sequence() {
    let rig = Rig::new();
    assert!(matches!(
        ensure(&rig, 5, 9000, 100),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        ensure(&rig, 5, 0, 100),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(rig.world.log().is_empty());
    assert!(rig.twin.parked().is_empty());
}

#[test]
fn the_twin_lingers_after_the_last_window_and_then_goes() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    ensure(&rig, 6, 800, 600).unwrap();
    rig.twin.release(5);
    std::thread::sleep(TEST_TIMING.linger * 2);
    assert_eq!(rig.world.screens_dropped(), 0, "a window is still parked");
    rig.twin.release(6);
    std::thread::sleep(TEST_TIMING.linger / 4);
    assert_eq!(rig.world.screens_dropped(), 0, "still lingering");
    wait_until("the linger to end", || rig.world.screens_dropped() == 1);
    assert!(rig.twin.view().is_none());
    assert!(rig.world.log().contains(&"clear_fence".to_owned()));
    // A later park makes a new twin.
    ensure(&rig, 7, 800, 600).unwrap();
    assert_eq!(rig.world.screens_made(), 2);
}

#[test]
fn a_park_during_the_linger_keeps_the_twin() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.twin.release(5);
    std::thread::sleep(TEST_TIMING.linger / 4);
    ensure(&rig, 6, 800, 600).unwrap();
    std::thread::sleep(TEST_TIMING.linger * 3);
    assert_eq!(rig.world.screens_dropped(), 0);
    assert_eq!(rig.world.screens_made(), 1);
    rig.twin.release(6);
    wait_until("the linger to end", || rig.world.screens_dropped() == 1);
}

#[test]
fn a_refused_park_on_an_empty_twin_still_lets_it_go() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.twin.release(5);
    // 6 is refused (too big for any twin) and never registered: the twin stays empty.
    assert!(ensure(&rig, 6, 9000, 100).is_err());
    assert!(rig.twin.parked().is_empty());
    wait_until("the linger to end", || rig.world.screens_dropped() == 1);
}

#[test]
fn a_window_released_that_was_never_parked_changes_nothing() {
    let rig = Rig::new();
    rig.twin.release(42);
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.twin.release(42);
    assert_eq!(rig.twin.parked(), [5]);
}

#[test]
fn losing_the_twin_tells_the_listeners_which_windows_were_on_it() {
    let rig = Rig::new();
    let seen = collect_lost(&rig.twin);
    ensure(&rig, 5, 1800, 1169).unwrap();
    ensure(&rig, 6, 800, 600).unwrap();
    rig.world.lose_screen(0);
    wait_until("the listener", || !seen.lock().unwrap().is_empty());
    assert_eq!(*seen.lock().unwrap(), [vec![5, 6]]);
    assert!(rig.twin.view().is_none());
    assert!(rig.world.log().contains(&"clear_fence".to_owned()));
    // Until a window is released it is not moved onto a new twin; others are.
    assert!(ensure(&rig, 5, 800, 600).is_err());
    ensure(&rig, 7, 800, 600).unwrap();
    assert_eq!(rig.world.screens_made(), 2);
    rig.twin.release(5);
    ensure(&rig, 5, 800, 600).unwrap();
    assert_eq!(rig.twin.parked(), [5, 7]);
}

#[test]
fn a_dead_twin_found_by_a_park_is_replaced_and_its_windows_are_reported() {
    let rig = Rig::new();
    let seen = collect_lost(&rig.twin);
    ensure(&rig, 5, 1800, 1169).unwrap();
    // The screen is dead, but its callback has not run yet.
    rig.world.lose_screen_silently(0);
    ensure(&rig, 7, 800, 600).unwrap();
    wait_until("the listener", || !seen.lock().unwrap().is_empty());
    assert_eq!(*seen.lock().unwrap(), [vec![5]]);
    assert_eq!(rig.world.screens_made(), 2);
    assert_eq!(rig.twin.parked(), [7]);
}

#[test]
fn a_failed_growth_drops_the_twin_and_the_windows_on_it_are_reported() {
    let rig = Rig::new();
    let seen = collect_lost(&rig.twin);
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world
        .lock()
        .fail_apply
        .push_back(PlatformError::Backend(
            "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
                .into(),
        ));
    rig.world.clear_log();
    let error = ensure(&rig, 6, 3000, 1000).unwrap_err();
    assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
    // Width 3000 > 2304 asks for 3750 -> 3776. The fence comes down before the screen goes.
    assert_eq!(
        rig.world.log(),
        [
            "save_layout 2",
            "resize 3776x1472",
            "clear_fence",
            "screen dropped",
            "restore_layout 2 skip=[5, 6]",
        ]
    );
    wait_until("the listener", || !seen.lock().unwrap().is_empty());
    assert_eq!(*seen.lock().unwrap(), [vec![5, 6]]);
    assert!(rig.twin.view().is_none());
    assert!(rig.twin.parked().is_empty());
}

#[test]
fn the_physical_snapshot_follows_a_layout_the_user_changed() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    // The user moves HDMI-1 down while the twin exists.
    rig.world.user_moves_hdmi(300);
    wait_until("the snapshot to follow", || {
        rig.twin
            .snapshot()
            .iter()
            .any(|l| l.connectors[0] == "HDMI-1" && l.y == 300)
    });
    ensure(&rig, 6, 3000, 1000).unwrap();
    let applies = rig.world.applies();
    let last = &applies.last().unwrap().1;
    let hdmi = last.iter().find(|c| c.monitors[0].0 == "HDMI-1").unwrap();
    assert_eq!(hdmi.y, 300, "the user's new layout is what is put back");
    // The twin follows the rightmost monitor it is top-aligned with.
    assert_eq!(last.last().unwrap().y, 300);
}

#[test]
fn a_layout_that_does_not_change_is_left_alone() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let before = rig.world.log();
    let snapshot = rig.twin.snapshot();
    // A MonitorsChanged that changes nothing (our own apply is one too): the timer thread looks
    // at the state, and finds nothing to do.
    let reads = rig.world.reads();
    rig.world.signal_change();
    wait_until("the timer thread to look", || rig.world.reads() > reads);
    std::thread::sleep(TEST_TIMING.debounce);
    assert_eq!(rig.world.log(), before);
    assert_eq!(rig.world.applies().len(), 1);
    assert_eq!(rig.twin.snapshot(), snapshot);
}

#[test]
fn a_twin_that_moves_updates_its_display_and_its_fence() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.move_twin(7000);
    wait_until("the display to follow", || {
        rig.twin.view().unwrap().display.geometry.logical_origin.x == 7000.0
    });
    wait_until("the fence to follow", || {
        rig.world.lock().fence == Some((7000, 0, 1152, 736))
    });
}

#[test]
fn the_twin_is_a_display_for_the_injector_only_while_it_exists() {
    let rig = Rig::new();
    let physical: crate::portal::eis::DisplaysFn = Arc::new(Vec::new);
    let displays = rig.twin.displays(physical);
    assert!(displays().is_empty());
    ensure(&rig, 5, 1800, 1169).unwrap();
    let all = displays();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, twin_id());
    rig.twin.release(5);
    wait_until("the linger to end", || displays().is_empty());
}

#[test]
fn moves_onto_the_twin_lower_the_fence_and_it_comes_back_after_the_quiet_time() {
    // A re-arm time long enough that the five moves below are certainly inside one quiet window.
    let rig = Rig::with_timing(Timing {
        fence_rearm: Duration::from_millis(300),
        ..TEST_TIMING
    });
    let hook = rig.twin.before_move();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.clear_log();
    // A move elsewhere changes nothing.
    hook(DisplayId(12345));
    assert!(rig.world.log().is_empty());
    // Moves onto the twin: one clear, however many moves.
    for _ in 0..5 {
        hook(twin_id());
    }
    assert_eq!(rig.world.log(), ["clear_fence"]);
    assert!(rig.world.lock().fence.is_none());
    // Quiet for the re-arm time: the fence is up again, once.
    wait_until("the fence to come back", || {
        rig.world.lock().fence.is_some()
    });
    assert_eq!(
        rig.world.log(),
        ["clear_fence", "set_fence 6440,0 1152x736"]
    );
    // And the next move lowers it again.
    hook(twin_id());
    assert!(rig.world.lock().fence.is_none());
}

#[test]
fn the_hook_does_nothing_without_a_twin() {
    let rig = Rig::new();
    let hook = rig.twin.before_move();
    hook(twin_id());
    assert!(rig.world.log().is_empty());
}

#[test]
fn dropping_the_twin_manager_closes_the_screen_and_takes_the_fence_down() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let world = Arc::clone(&rig.world);
    drop(rig);
    // The timer thread may hold the last reference for a moment.
    wait_until("the screen to close", || world.screens_dropped() == 1);
    assert!(world.lock().fence.is_none());
    assert!(world.lock().twin.is_none());
}

#[test]
fn outward_handles_do_not_keep_the_twin_alive() {
    let rig = Rig::new();
    let hook = rig.twin.before_move();
    let displays = rig.twin.displays(Arc::new(Vec::new));
    ensure(&rig, 5, 1800, 1169).unwrap();
    let world = Arc::clone(&rig.world);
    drop(rig);
    wait_until("the screen to close", || world.screens_dropped() == 1);
    // And they keep working, doing nothing.
    hook(twin_id());
    assert!(displays().is_empty());
}

// ---- the capture router ----

/// The capture the router wraps: records what it is asked.
#[derive(Clone, Default)]
struct FakeInner {
    log: Arc<Mutex<Vec<String>>>,
}

impl FrameCapture for FakeInner {
    fn start(
        &mut self,
        target: CaptureTarget,
        _crop: Option<PixelRect>,
        _max_fps: u32,
        _sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        self.log.lock().unwrap().push(format!("start {target:?}"));
        Ok(StreamId(1))
    }

    fn set_crop(
        &mut self,
        stream: StreamId,
        _crop: Option<PixelRect>,
    ) -> Result<(), PlatformError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("set_crop {}", stream.0));
        Ok(())
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        self.log.lock().unwrap().push(format!("stop {}", stream.0));
        Ok(())
    }
}

fn describe(event: &FrameEvent) -> String {
    match event {
        FrameEvent::Frame { stream, .. } => format!("frame {}", stream.0),
        FrameEvent::Ended { stream, reason } => format!("ended {} {reason:?}", stream.0),
        FrameEvent::Cursor { stream, .. } => format!("cursor {}", stream.0),
        FrameEvent::CursorDefault { stream } => format!("cursor-default {}", stream.0),
        _ => "other".to_owned(),
    }
}

type Events = Arc<Mutex<Vec<String>>>;

fn event_sink() -> (Arc<dyn EventSink<FrameEvent>>, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&events);
    (
        Arc::new(move |event: FrameEvent| log.lock().unwrap().push(describe(&event))),
        events,
    )
}

fn router(rig: &Rig) -> (super::TwinCapture, FakeInner) {
    let inner = FakeInner::default();
    (rig.twin.capture(Box::new(inner.clone())), inner)
}

#[test]
fn everything_but_the_twin_goes_to_the_wrapped_capture() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, inner) = router(&rig);
    let (sink, _) = event_sink();
    let id = capture
        .start(
            CaptureTarget::Display(DisplayId(77)),
            None,
            30,
            sink.clone(),
        )
        .unwrap();
    assert_eq!(id, StreamId(1));
    let window = capture
        .start(CaptureTarget::Window(WindowId(9)), None, 30, sink)
        .unwrap();
    assert_eq!(window, StreamId(1));
    capture.set_crop(StreamId(1), None).unwrap();
    capture.stop(StreamId(1)).unwrap();
    // Ids of the window capture's range go to it as well.
    capture.stop(StreamId(1 << 62)).unwrap();
    let window_stop = format!("stop {}", 1u64 << 62);
    assert_eq!(
        *inner.log.lock().unwrap(),
        [
            "start Display(DisplayId(77))",
            "start Window(WindowId(9))",
            "set_crop 1",
            "stop 1",
            window_stop.as_str(),
        ]
    );
    assert_eq!(rig.world.screen(0).stream_count(), 0);
}

#[test]
fn the_twin_is_served_by_its_own_stream_with_renumbered_ids() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, inner) = router(&rig);
    let (sink, events) = event_sink();
    let a = capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink.clone())
        .unwrap();
    let b = capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    assert_eq!(a, StreamId(TWIN_STREAM_BASE));
    assert_eq!(b, StreamId(TWIN_STREAM_BASE + 1));
    assert!(inner.log.lock().unwrap().is_empty());
    assert_eq!(rig.world.screen(0).stream_count(), 2);
    // Events come back addressed with the outer id, whatever the screen called the stream.
    rig.world.screen(0).emit_frame();
    assert_eq!(
        *events.lock().unwrap(),
        [format!("frame {}", TWIN_STREAM_BASE)]
    );
    capture.set_crop(a, None).unwrap();
    capture.stop(a).unwrap();
    // Stopped by the caller: the ending stays `Requested`.
    assert_eq!(
        events.lock().unwrap().last().unwrap(),
        &format!("ended {} Requested", TWIN_STREAM_BASE)
    );
    assert_eq!(rig.world.screen(0).stream_count(), 1);
    assert!(matches!(
        capture.set_crop(a, None),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(capture.stop(a), Err(PlatformError::NotFound)));
}

#[test]
fn a_twin_dropped_under_a_running_stream_ends_it_as_a_vanished_target() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, events) = event_sink();
    let stream = capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    rig.world.screen(0).emit_frame();
    rig.twin.release(5);
    wait_until("the twin to go", || rig.world.screens_dropped() == 1);
    // The screen's drop says `Requested`, but nobody asked: the display went away.
    assert_eq!(
        events.lock().unwrap().last().unwrap(),
        &format!("ended {} TargetGone", stream.0)
    );
    // Stopping it later is fine, setting its crop is not.
    assert!(matches!(
        capture.set_crop(stream, None),
        Err(PlatformError::NotFound)
    ));
    capture.stop(stream).unwrap();
    // And the twin is not a capture target any more: the wrapped capture gets the request.
    let (sink, _) = event_sink();
    assert_eq!(
        capture
            .start(CaptureTarget::Display(twin_id()), None, 30, sink)
            .unwrap(),
        StreamId(1)
    );
}

#[test]
fn a_lost_twin_ends_its_streams_as_the_screen_says() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, events) = event_sink();
    let stream = capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    rig.world.lose_screen(0);
    assert_eq!(
        *events.lock().unwrap(),
        [format!("ended {} TargetGone", stream.0)]
    );
}

#[test]
fn a_stream_that_gets_no_frame_makes_the_twin_grow_one_step() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, _) = event_sink();
    capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    rig.world.clear_log();
    wait_until("the nudge", || {
        rig.world.log().contains(&"resize 2368x1472".to_owned())
    });
    // The whole sequence ran, with the parked window on the skip list.
    wait_until("the sequence", || {
        rig.world
            .log()
            .contains(&"set_fence 6440,0 1184x736".to_owned())
    });
    assert!(
        rig.world
            .log()
            .contains(&"restore_layout 2 skip=[5]".to_owned())
    );
    assert_eq!(rig.world.screens_made(), 1);
}

#[test]
fn a_stream_that_gets_its_frame_is_not_nudged() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, _) = event_sink();
    capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    rig.world.clear_log();
    rig.world.screen(0).emit_frame();
    std::thread::sleep(TEST_TIMING.first_frame * 3);
    assert!(rig.world.log().is_empty(), "{:?}", rig.world.log());
}

#[test]
fn a_stream_stopped_before_its_first_frame_is_not_nudged() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, _) = event_sink();
    let stream = capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    capture.stop(stream).unwrap();
    rig.world.clear_log();
    std::thread::sleep(TEST_TIMING.first_frame * 3);
    assert!(rig.world.log().is_empty(), "{:?}", rig.world.log());
}

#[test]
fn a_stream_that_ended_before_its_first_frame_is_not_nudged() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let (sink, events) = event_sink();
    capture
        .start(CaptureTarget::Display(twin_id()), None, 30, sink)
        .unwrap();
    // The screen ends the stream by itself (a closed gate, say), and the screen stays.
    rig.world.screen(0).end_streams_blocked();
    assert_eq!(events.lock().unwrap().len(), 1);
    rig.world.clear_log();
    std::thread::sleep(TEST_TIMING.first_frame * 3);
    assert!(rig.world.log().is_empty(), "{:?}", rig.world.log());
}

#[test]
fn one_twin_is_nudged_a_limited_number_of_times() {
    let rig = Rig::with_timing(Timing {
        first_frame: Duration::from_millis(20),
        ..TEST_TIMING
    });
    ensure(&rig, 5, 1800, 1169).unwrap();
    let (mut capture, _) = router(&rig);
    let resizes = || {
        rig.world
            .log()
            .iter()
            .filter(|line| line.starts_with("resize "))
            .count()
    };
    for round in 1..=10 {
        let (sink, _) = event_sink();
        capture
            .start(CaptureTarget::Display(twin_id()), None, 30, sink)
            .unwrap();
        // Each stream gets its nudge, until the twin has had its share.
        if round <= 8 {
            wait_until("the nudge", || resizes() == round);
        } else {
            std::thread::sleep(Duration::from_millis(120));
        }
    }
    assert_eq!(resizes(), 8);
}

#[test]
fn the_linger_drop_saves_and_puts_back_the_window_layout() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.twin.release(5);
    wait_until("the drop", || rig.world.log().len() == 9);
    // After the 5 lines of the creation: the guard around the drop.
    assert_eq!(
        rig.world.log()[5..],
        [
            "save_layout 2",
            "clear_fence",
            "screen dropped",
            "restore_layout 2 skip=[]",
        ]
    );
}

#[test]
fn a_layout_that_cannot_be_saved_does_not_stop_the_drop() {
    let rig = Rig::new();
    ensure(&rig, 5, 1800, 1169).unwrap();
    rig.world.lock().fail_save = Some(PlatformError::Timeout);
    rig.twin.release(5);
    wait_until("the drop", || rig.world.screens_dropped() == 1);
    assert!(
        !rig.world
            .log()
            .iter()
            .any(|line| line.starts_with("restore_layout 2")),
        "{:?}",
        rig.world.log()
    );
}

#[test]
fn timing_defaults_are_the_specified_ones() {
    assert_eq!(super::TWIN_LINGER, Duration::from_secs(30));
    assert_eq!(super::FENCE_REARM, Duration::from_millis(1500));
    assert_eq!(Timing::DEFAULT.poll_interval, Duration::from_millis(10));
    assert_eq!(Timing::DEFAULT.poll_budget, Duration::from_secs(2));
}

#[test]
fn concurrent_parks_are_serialised_by_the_twin() {
    let rig = Rig::new();
    let successes = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for window in 0..6u64 {
            let twin = rig.twin.clone();
            let successes = Arc::clone(&successes);
            scope.spawn(move || {
                if twin
                    .ensure(window, px(800 + 100 * window as u32, 600), 2.0)
                    .is_ok()
                {
                    successes.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(successes.load(Ordering::SeqCst), 6);
    assert_eq!(rig.world.screens_made(), 1, "one twin serves them all");
    assert_eq!(rig.twin.parked().len(), 6);
}
