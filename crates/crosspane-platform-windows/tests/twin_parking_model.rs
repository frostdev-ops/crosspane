//! M2 parking orchestration (W3.2 T15): the native call order, rollbacks, restore, lost-lease reap,
//! recover, and the twin geometry and probe helpers. Pure model checks with fakes that record every
//! native call in order. Never invokes a desktop API.
#![allow(clippy::unwrap_used)]
use crosspane_platform::{Parked, ParkingKind, PlatformError};
use crosspane_platform_windows::model::{
    geometry::{DisplayIds, MonitorProbe},
    journal::Show,
    parking::{
        Controller, MirrorEntry, MirrorJournalImages, MirrorJournalStore, NativeIdentity,
        NativePort, Observed, RestoreOutcome, actual_geometry, observed_geometry,
    },
    twin::{
        Refusal, TWIN_ABSENT_REASON, TWIN_UNAVAILABLE_REASON, TwinDisplay, TwinError, TwinKey,
        TwinMode, twin_mode,
    },
    twin_parking::{
        Placement, TWIN_MAXIMIZED_REASON, TwinOps, TwinParking, clamp_to_twin, find_twin_probe,
        twin_outer,
    },
};
use crosspane_types::{
    geom::{PixelRect, PixelSize, euclid::point2},
    id::{DisplayId, WindowId},
};
use std::sync::{Arc, Mutex};

const WINDOW: WindowId = WindowId(12345);
const OWNED_PATH: &str = "owned-monitor";
const OWNED_NAME: &str = "OWNED";
const TWIN_PATH: &str = "owned-twin";
const TWIN_NAME: &str = "TWIN";

/// The native calls of a clean park onto a fresh twin. The controller's own observation inside
/// `park` and `relocate` is an `inspect`, and each journal publication is a `commit`.
const PARK_EVENTS: [&str; 9] = [
    "ready", "inspect", "inspect", "commit", "add", "locate", "inspect", "commit", "move",
];

fn identity() -> NativeIdentity {
    NativeIdentity {
        hwnd: 7,
        pid: 8,
        tid: 9,
        process_created: 10,
    }
}

fn owned_probe() -> MonitorProbe {
    MonitorProbe {
        device_path: OWNED_PATH.into(),
        name: OWNED_NAME.into(),
        rc_monitor: [-1920, -120, 0, 960],
        rc_work: [-1920, -120, 0, 960],
        primary: true,
        dpi: 144,
        refresh_millihz: 60000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

fn twin_probe(rect: [i32; 4]) -> MonitorProbe {
    MonitorProbe {
        device_path: TWIN_PATH.into(),
        name: TWIN_NAME.into(),
        rc_monitor: rect,
        rc_work: rect,
        primary: false,
        dpi: 96,
        refresh_millihz: 60000,
        edid: None,
        twin: true,
        quarter_turns: 0,
    }
}

/// The window before any parking. Its frame borders are 8 px left, 0 top, 8 right and 8 bottom.
fn original() -> Observed {
    let visible = [-1880, -80, -1480, 220];
    let (geometry, path) = actual_geometry(
        WINDOW,
        visible,
        OWNED_NAME,
        owned_probe().rc_monitor,
        &[owned_probe()],
        &mut DisplayIds::default(),
        false,
    )
    .unwrap();
    Observed {
        outer: [-1888, -80, -1472, 228],
        visible,
        monitor_path: path,
        show: Show::Normal,
        dpi: 144,
        eligible: true,
        fullscreen: false,
        geometry: Some(geometry),
    }
}

#[derive(Default)]
struct World {
    events: Vec<&'static str>,
    images: MirrorJournalImages,
    commits: usize,
    fail_commit: bool,
    ids: DisplayIds,
    /// The desktop rect of the current twin. Empty until a twin exists.
    twin_rect: [i32; 4],
    discarded: Vec<TwinKey>,
}
type Shared = Arc<Mutex<World>>;

fn log(world: &Shared, event: &'static str) {
    world.lock().unwrap().events.push(event);
}

fn contains(outer: [i32; 4], inner: [i32; 4]) -> bool {
    inner[0] >= outer[0] && inner[1] >= outer[1] && inner[2] <= outer[2] && inner[3] <= outer[3]
}

/// The geometry the native side would report for a window whose visible frame is `visible`. It is
/// on the twin when the frame lies inside the twin rect, and on the owned monitor otherwise.
fn geometry_at(world: &mut World, visible: [i32; 4], fullscreen: bool) -> Parked {
    let twin_rect = world.twin_rect;
    let on_twin = twin_rect != [0; 4] && contains(twin_rect, visible);
    let (name, rect) = if on_twin {
        (TWIN_NAME, twin_rect)
    } else {
        (OWNED_NAME, owned_probe().rc_monitor)
    };
    let mut probes = vec![owned_probe()];
    if twin_rect != [0; 4] {
        probes.push(twin_probe(twin_rect));
    }
    observed_geometry(
        WINDOW,
        visible,
        name,
        rect,
        &probes,
        &mut world.ids,
        fullscreen,
    )
    .unwrap()
    .0
}

struct Port {
    world: Shared,
    current: Option<Observed>,
    fail_move: bool,
    fail_restore: bool,
    /// Reports an M1 (mirror) geometry after each move: a result of the wrong kind.
    mirror_after_move: bool,
}

impl NativePort for Port {
    fn check(&self) -> Result<(), PlatformError> {
        Ok(())
    }
    fn resolve(&mut self, _: WindowId) -> Result<NativeIdentity, PlatformError> {
        Ok(identity())
    }
    fn inspect(
        &mut self,
        _: NativeIdentity,
        _: Option<WindowId>,
    ) -> Result<Option<Observed>, PlatformError> {
        log(&self.world, "inspect");
        Ok(self.current.clone())
    }
    fn resize(
        &mut self,
        _: NativeIdentity,
        outer: [i32; 4],
        _: WindowId,
    ) -> Result<Observed, PlatformError> {
        log(&self.world, "move");
        let mut moved = self.current.clone().unwrap();
        moved.outer = outer;
        moved.visible = [outer[0] + 8, outer[1], outer[2] - 8, outer[3] - 8];
        moved.geometry = Some(if self.mirror_after_move {
            let mut world = self.world.lock().unwrap();
            Parked {
                window: WINDOW,
                kind: ParkingKind::Mirror,
                display: world.ids.assign(OWNED_PATH).unwrap(),
                content: PixelRect::new(point2(0, 0), point2(400, 300)),
                fullscreen: false,
            }
        } else {
            geometry_at(
                &mut self.world.lock().unwrap(),
                moved.visible,
                moved.fullscreen,
            )
        });
        // The window has moved even when the native call reports an error.
        self.current = Some(moved.clone());
        if self.fail_move {
            return Err(PlatformError::Backend("owned move failed".into()));
        }
        Ok(moved)
    }
    fn restore(
        &mut self,
        entry: &MirrorEntry,
        _: Option<WindowId>,
    ) -> Result<RestoreOutcome, PlatformError> {
        log(&self.world, "restore");
        if self.fail_restore {
            return Err(PlatformError::Backend("owned restore failed".into()));
        }
        let mut restored = self.current.clone().unwrap();
        restored.outer = entry.original.rect_physical;
        restored.visible = entry.visible_original;
        restored.show = entry.original.show;
        restored.geometry = Some(geometry_at(
            &mut self.world.lock().unwrap(),
            restored.visible,
            restored.fullscreen,
        ));
        self.current = Some(restored.clone());
        Ok(RestoreOutcome::Restored(restored))
    }
}

#[derive(Clone)]
struct Store(Shared);

impl MirrorJournalStore for Store {
    fn read(&mut self) -> Result<MirrorJournalImages, PlatformError> {
        Ok(self.0.lock().unwrap().images.clone())
    }
    fn commit(&mut self, bytes: &[u8]) -> Result<(), PlatformError> {
        let mut world = self.0.lock().unwrap();
        world.events.push("commit");
        world.commits += 1;
        world.images.pending = Some(bytes.to_vec());
        if world.fail_commit {
            return Err(PlatformError::Backend("owned injected save failure".into()));
        }
        world.images.committed = Some(bytes.to_vec());
        Ok(())
    }
}

/// A fake twin driver. Its one-shot failures are taken on the first matching call.
struct Ops {
    world: Shared,
    ready_result: Result<(), TwinError>,
    add_error: Option<TwinError>,
    resize_error: Option<TwinError>,
    locate_error: Option<PlatformError>,
    locate_path: &'static str,
    remove_error: bool,
    lost: Vec<TwinKey>,
    next_key: u32,
}

impl Ops {
    fn display(&self, key: TwinKey, mode: TwinMode) -> TwinDisplay {
        let rect = [0, 0, mode.width as i32, mode.height as i32];
        self.world.lock().unwrap().twin_rect = rect;
        TwinDisplay {
            key,
            monitor_id: 7,
            mode,
            gdi_name: TWIN_NAME.into(),
            monitor_path: TWIN_PATH.into(),
            rect,
            dpi: 96,
        }
    }
}

impl TwinOps for Ops {
    fn ready(&mut self) -> Result<(), TwinError> {
        log(&self.world, "ready");
        self.ready_result.clone()
    }
    fn add(&mut self, mode: TwinMode) -> Result<TwinDisplay, TwinError> {
        log(&self.world, "add");
        if let Some(error) = self.add_error.take() {
            return Err(error);
        }
        let key = TwinKey(self.next_key);
        self.next_key += 1;
        Ok(self.display(key, mode))
    }
    fn resize(&mut self, key: TwinKey, mode: TwinMode) -> Result<TwinDisplay, TwinError> {
        log(&self.world, "twin-resize");
        if let Some(error) = self.resize_error.take() {
            return Err(error);
        }
        Ok(self.display(key, mode))
    }
    fn remove(&mut self, _: TwinKey) -> Result<(), TwinError> {
        log(&self.world, "remove");
        if self.remove_error {
            Err(TwinError::Timeout("remove"))
        } else {
            Ok(())
        }
    }
    fn discard(&mut self, key: TwinKey) {
        log(&self.world, "discard");
        self.world.lock().unwrap().discarded.push(key);
    }
    fn locate(&mut self, display: &TwinDisplay) -> Result<(DisplayId, [i32; 4]), PlatformError> {
        log(&self.world, "locate");
        if let Some(error) = self.locate_error.take() {
            return Err(error);
        }
        let id = self
            .world
            .lock()
            .unwrap()
            .ids
            .assign(self.locate_path)
            .unwrap();
        Ok((id, display.rect))
    }
    fn lost(&mut self) -> Vec<TwinKey> {
        std::mem::take(&mut self.lost)
    }
}

struct Rig {
    world: Shared,
    parking: TwinParking<Port>,
    ops: Ops,
}

fn new_rig() -> Rig {
    let world: Shared = Arc::default();
    let port = Port {
        world: world.clone(),
        current: Some(original()),
        fail_move: false,
        fail_restore: false,
        mirror_after_move: false,
    };
    let mut controller = Controller::new(Box::new(Store(world.clone())), port).unwrap();
    controller.bind();
    Rig {
        ops: Ops {
            world: world.clone(),
            ready_result: Ok(()),
            add_error: None,
            resize_error: None,
            locate_error: None,
            locate_path: TWIN_PATH,
            remove_error: false,
            lost: Vec::new(),
            next_key: 1,
        },
        parking: TwinParking::new(controller),
        world,
    }
}

impl Rig {
    fn events(&self) -> Vec<&'static str> {
        self.world.lock().unwrap().events.clone()
    }
    fn clear(&self) {
        self.world.lock().unwrap().events.clear();
    }
    fn count(&self, event: &str) -> usize {
        self.events().iter().filter(|seen| **seen == event).count()
    }
    fn commits(&self) -> usize {
        self.world.lock().unwrap().commits
    }
    fn discarded(&self) -> Vec<TwinKey> {
        self.world.lock().unwrap().discarded.clone()
    }
    fn journal_len(&self) -> usize {
        self.parking.windows.journal().entries().len()
    }
    fn window_outer(&self) -> [i32; 4] {
        self.parking.windows.port.current.as_ref().unwrap().outer
    }
    fn park_at(&mut self, width: u32, height: u32) -> Result<Parked, PlatformError> {
        self.parking
            .park(&mut self.ops, WINDOW, PixelSize::new(width, height), 1.0)
    }
    fn park_400x300(&mut self) -> Parked {
        self.park_at(400, 300).unwrap()
    }
    fn park_400x300_error(&mut self) -> PlatformError {
        self.park_at(400, 300).unwrap_err()
    }
}

fn reason(error: &PlatformError) -> Option<&'static str> {
    match error {
        PlatformError::Unsupported(reason) => Some(*reason),
        _ => None,
    }
}

#[test]
fn park_runs_inspect_commit_add_locate_commit_move_in_order() {
    let mut rig = new_rig();
    let parked = rig.park_400x300();
    assert_eq!(rig.events(), PARK_EVENTS);
    assert_eq!(parked.kind, ParkingKind::Twin);
    assert_eq!(
        parked.content,
        PixelRect::new(point2(0, 0), point2(400, 300))
    );
    let display = rig.world.lock().unwrap().ids.assign(TWIN_PATH).unwrap();
    assert_eq!(parked.display, display);
    assert_eq!(
        rig.parking.placement(WINDOW),
        Some(&Placement {
            key: TwinKey(1),
            mode: twin_mode(PixelSize::new(400, 300), 1.0).unwrap(),
            display,
            rect: [0, 0, 1920, 1080],
        })
    );
    // The window sits at the twin's top-left with its borders kept.
    assert_eq!(rig.window_outer(), [-8, 0, 408, 308]);
    assert!(rig.parking.windows.journal().entries()[0].may_have_mutated);
}

#[test]
fn absent_or_failed_readiness_stops_before_any_commit_or_move() {
    let mut rig = new_rig();
    rig.ops.ready_result = Err(TwinError::Absent);
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_ABSENT_REASON));
    assert_eq!(rig.events(), vec!["ready"]);
    assert_eq!(rig.commits(), 0);

    let mut rig = new_rig();
    rig.ops.ready_result = Err(TwinError::Timeout("ready"));
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    assert_eq!(rig.count("move"), 0);
    assert_eq!(rig.commits(), 0);
    assert_eq!(rig.count("add"), 0);
}

#[test]
fn invalid_size_and_failed_commit_never_reach_the_twin() {
    let mut rig = new_rig();
    assert!(rig.park_at(0, 300).is_err());
    assert!(
        rig.parking
            .park(&mut rig.ops, WINDOW, PixelSize::new(400, 300), f64::NAN)
            .is_err()
    );
    assert!(rig.events().is_empty());

    rig.world.lock().unwrap().fail_commit = true;
    let error = rig.park_400x300_error();
    assert!(matches!(error, PlatformError::Backend(_)));
    assert_eq!(rig.events(), vec!["ready", "inspect", "inspect", "commit"]);
    assert_eq!(rig.count("add"), 0);
}

#[test]
fn maximized_fullscreen_and_locked_windows_are_refused_before_the_twin() {
    let mut rig = new_rig();
    rig.parking.windows.port.current.as_mut().unwrap().show = Show::Maximized;
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_MAXIMIZED_REASON));
    assert_eq!(rig.events(), vec!["ready", "inspect"]);
    assert_eq!(rig.commits(), 0);

    let mut rig = new_rig();
    rig.parking
        .windows
        .port
        .current
        .as_mut()
        .unwrap()
        .fullscreen = true;
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_MAXIMIZED_REASON));
    assert_eq!(rig.count("add"), 0);

    let mut rig = new_rig();
    rig.parking.windows.port.current.as_mut().unwrap().eligible = false;
    let error = rig.park_400x300_error();
    assert!(matches!(error, PlatformError::Locked));
    assert_eq!(rig.events(), vec!["ready", "inspect"]);
    assert_eq!(rig.commits(), 0);
}

#[test]
fn add_failures_roll_back_and_name_the_reason() {
    let mut rig = new_rig();
    rig.ops.add_error = Some(TwinError::Refused(Refusal::Busy));
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    // The window goes back through its journal entry, and no twin was made, so nothing is discarded.
    assert_eq!(
        rig.events(),
        vec![
            "ready", "inspect", "inspect", "commit", "add", "inspect", "commit"
        ]
    );
    assert!(rig.discarded().is_empty());
    assert_eq!(rig.journal_len(), 0);
    assert!(rig.parking.placement(WINDOW).is_none());

    let mut rig = new_rig();
    rig.ops.add_error = Some(TwinError::Absent);
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_ABSENT_REASON));

    let mut rig = new_rig();
    rig.ops.add_error = Some(TwinError::Journal("ledger save failed".into()));
    let error = rig.park_400x300_error();
    assert!(
        matches!(&error, PlatformError::Backend(message) if message.as_str() == "ledger save failed")
    );
    assert_eq!(rig.journal_len(), 0);
}

#[test]
fn locate_failure_rolls_back_and_discards_the_twin() {
    let mut rig = new_rig();
    rig.ops.locate_error = Some(PlatformError::Timeout);
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    assert_eq!(
        rig.events(),
        vec![
            "ready", "inspect", "inspect", "commit", "add", "locate", "inspect", "commit",
            "discard"
        ]
    );
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
    assert_eq!(rig.journal_len(), 0);
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.window_outer(), original().outer);
}

#[test]
fn failed_move_is_undone_before_the_twin_is_discarded() {
    let mut rig = new_rig();
    rig.parking.windows.port.fail_move = true;
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    // The window had already moved, so the rollback restores it before the discard.
    assert_eq!(
        rig.events(),
        vec![
            "ready", "inspect", "inspect", "commit", "add", "locate", "inspect", "commit", "move",
            "inspect", "commit", "restore", "commit", "discard"
        ]
    );
    assert_eq!(rig.window_outer(), original().outer);
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn failed_rollback_restore_keeps_the_journal_and_says_so() {
    let mut rig = new_rig();
    rig.parking.windows.port.fail_move = true;
    rig.parking.windows.port.fail_restore = true;
    let error = rig.park_400x300_error();
    assert!(matches!(
        &error,
        PlatformError::Backend(message)
            if message.contains("rollback failed") && message.contains("journal retained")
    ));
    assert_eq!(rig.journal_len(), 1);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
    assert!(rig.parking.placement(WINDOW).is_none());
}

#[test]
fn a_wrong_kind_or_another_display_rolls_back() {
    let mut rig = new_rig();
    rig.parking.windows.port.mirror_after_move = true;
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
    assert_eq!(rig.window_outer(), original().outer);

    let mut rig = new_rig();
    rig.ops.locate_path = "other-twin";
    let error = rig.park_400x300_error();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
    assert!(rig.parking.placement(WINDOW).is_none());
}

#[test]
fn a_placed_window_is_resized_in_place_without_twin_calls() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    // The twin is always 1920x1080 at scale 1, so a 402 px resize keeps the twin mode.
    let parked = rig
        .parking
        .park(&mut rig.ops, WINDOW, PixelSize::new(402, 300), 1.0)
        .unwrap();
    assert_eq!(rig.events(), vec!["inspect", "commit", "move"]);
    assert_eq!(parked.kind, ParkingKind::Twin);
    assert_eq!(rig.window_outer(), [-8, 0, 410, 308]);
}

#[test]
fn same_mode_resize_makes_no_twin_calls() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    let parked = rig
        .parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(402, 300), 1.0)
        .unwrap();
    assert_eq!(rig.events(), vec!["inspect", "commit", "move"]);
    assert_eq!(parked.kind, ParkingKind::Twin);
    let placement = rig.parking.placement(WINDOW).unwrap();
    assert_eq!(placement.key, TwinKey(1));
    assert_eq!(placement.rect, [0, 0, 1920, 1080]);
}

#[test]
fn a_bigger_window_keeps_the_twin_and_is_clamped_to_it() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    // The twin stays 1920x1080 at scale 1, so a bigger window is resized in place, with no twin
    // calls.
    let parked = rig
        .parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(1600, 900), 1.0)
        .unwrap();
    assert_eq!(rig.events(), vec!["inspect", "commit", "move"]);
    assert_eq!(parked.kind, ParkingKind::Twin);
    assert_eq!(rig.window_outer(), [-8, 0, 1608, 908]);
    let placement = rig.parking.placement(WINDOW).unwrap().clone();
    assert_eq!(placement.key, TwinKey(1));
    assert_eq!(
        placement.mode,
        twin_mode(PixelSize::new(400, 300), 1.0).unwrap()
    );
    assert_eq!(placement.rect, [0, 0, 1920, 1080]);

    // Bigger than the twin: each axis is clamped to 1920x1080, still with no twin calls.
    rig.clear();
    rig.parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(2000, 1500), 1.0)
        .unwrap();
    assert_eq!(rig.events(), vec!["inspect", "commit", "move"]);
    assert_eq!(rig.window_outer(), [-8, 0, 1928, 1088]);
    assert!(rig.discarded().is_empty());
}

#[test]
fn a_scale_change_re_modes_the_twin() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    // Scale 2 halves the twin's mm (254x143), so the mode changes and the twin is re-moded.
    let parked = rig
        .parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(400, 300), 2.0)
        .unwrap();
    assert_eq!(
        rig.events(),
        vec![
            "twin-resize",
            "locate",
            "inspect",
            "inspect",
            "commit",
            "move"
        ]
    );
    assert_eq!(parked.kind, ParkingKind::Twin);
    let placement = rig.parking.placement(WINDOW).unwrap().clone();
    assert_eq!(placement.key, TwinKey(1));
    assert_eq!(
        placement.mode,
        twin_mode(PixelSize::new(400, 300), 2.0).unwrap()
    );
    assert_eq!(
        (placement.mode.width_mm, placement.mode.height_mm),
        (254, 143)
    );
    assert_eq!(placement.rect, [0, 0, 1920, 1080]);
    assert_eq!(placement.display, parked.display);
    assert_eq!(rig.window_outer(), [-8, 0, 408, 308]);
    assert!(rig.discarded().is_empty());
}

#[test]
fn a_millimetre_only_change_keeps_the_twin() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    // The mm come from the mode and the scale, not the window size, so a size change never changes
    // the mode. The twin (and its DPI) is kept.
    rig.parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(400, 301), 1.0)
        .unwrap();
    assert_eq!(rig.count("twin-resize"), 0);
}

#[test]
fn a_failed_re_mode_restores_the_window_and_discards_the_twin() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    rig.ops.resize_error = Some(TwinError::Refused(Refusal::Busy));
    // A scale change is the only re-mode, so the failing twin call is a scale change.
    let error = rig
        .parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(400, 300), 2.0)
        .unwrap_err();
    assert_eq!(reason(&error), Some(TWIN_UNAVAILABLE_REASON));
    assert_eq!(
        rig.events(),
        vec![
            "twin-resize",
            "inspect",
            "commit",
            "restore",
            "commit",
            "discard"
        ]
    );
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.window_outer(), original().outer);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn restore_moves_the_window_before_the_twin_is_removed() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    rig.parking.restore(&mut rig.ops, WINDOW).unwrap();
    assert_eq!(
        rig.events(),
        vec!["inspect", "commit", "restore", "commit", "remove"]
    );
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.window_outer(), original().outer);
    assert!(rig.discarded().is_empty());
}

#[test]
fn a_failed_twin_remove_on_restore_is_not_fatal_and_discards_the_twin() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    rig.ops.remove_error = true;
    rig.parking.restore(&mut rig.ops, WINDOW).unwrap();
    assert_eq!(
        rig.events(),
        vec![
            "inspect", "commit", "restore", "commit", "remove", "discard"
        ]
    );
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn a_failed_window_restore_discards_the_twin_and_keeps_the_entry() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    rig.parking.windows.port.fail_restore = true;
    let error = rig.parking.restore(&mut rig.ops, WINDOW).unwrap_err();
    assert!(matches!(error, PlatformError::Backend(_)));
    assert_eq!(
        rig.events(),
        vec!["inspect", "commit", "restore", "discard"]
    );
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 1);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn a_lost_lease_returns_the_window_and_later_geometry_is_not_found() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.ops.lost = vec![TwinKey(1)];
    let reaped = rig.parking.reap_lost(&mut rig.ops);
    assert_eq!(reaped, vec![WINDOW]);
    assert!(rig.parking.placement(WINDOW).is_none());
    assert!(matches!(
        rig.parking.geometry(WINDOW),
        Err(PlatformError::NotFound)
    ));
    assert_eq!(rig.window_outer(), original().outer);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn every_call_that_takes_ops_reaps_lost_twins_first() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.clear();
    rig.ops.lost = vec![TwinKey(1)];
    let error = rig
        .parking
        .resize(&mut rig.ops, WINDOW, PixelSize::new(402, 300), 1.0)
        .unwrap_err();
    assert!(matches!(error, PlatformError::NotFound));
    assert_eq!(rig.count("move"), 0);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn recover_restores_every_placed_window_and_removes_its_twin() {
    let mut rig = new_rig();
    assert_eq!(
        rig.parking.recover(&mut rig.ops).unwrap(),
        Vec::<WindowId>::new()
    );
    rig.park_400x300();
    rig.clear();
    let restored = rig.parking.recover(&mut rig.ops).unwrap();
    assert_eq!(restored, vec![WINDOW]);
    assert_eq!(
        rig.events(),
        vec!["inspect", "commit", "restore", "commit", "remove"]
    );
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 0);
}

#[test]
fn recover_also_restores_a_journaled_window_that_is_no_longer_placed() {
    let mut rig = new_rig();
    rig.park_400x300();
    // The restore fails, so the placement and the twin go, but the journal entry stays.
    rig.parking.windows.port.fail_restore = true;
    assert!(rig.parking.restore(&mut rig.ops, WINDOW).is_err());
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 1);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);

    // Nothing is placed now, so the placed loop does nothing. recover() still restores the window.
    rig.parking.windows.port.fail_restore = false;
    rig.clear();
    let restored = rig.parking.recover(&mut rig.ops).unwrap();
    assert_eq!(restored, vec![WINDOW]);
    assert_eq!(rig.events(), vec!["inspect", "commit", "restore", "commit"]);
    assert_eq!(rig.journal_len(), 0);
    assert_eq!(rig.window_outer(), original().outer);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn recover_reports_a_failed_restore_and_keeps_the_entry() {
    let mut rig = new_rig();
    rig.park_400x300();
    rig.parking.windows.port.fail_restore = true;
    assert!(rig.parking.recover(&mut rig.ops).is_err());
    assert!(rig.parking.placement(WINDOW).is_none());
    assert_eq!(rig.journal_len(), 1);
    assert_eq!(rig.discarded(), vec![TwinKey(1)]);
}

#[test]
fn twin_outer_keeps_the_borders_with_negative_origins_and_refuses_oversize() {
    let observed = original();
    // A twin left of and above the primary monitor.
    assert_eq!(
        twin_outer(&observed, [-1280, -720, 0, 0], PixelSize::new(400, 300)).unwrap(),
        [-1288, -720, -872, -412]
    );
    assert_eq!(
        twin_outer(&observed, [0, 0, 1280, 720], PixelSize::new(400, 300)).unwrap(),
        [-8, 0, 408, 308]
    );
    // The visible frame must fit on the twin, and the twin must have an area.
    assert!(twin_outer(&observed, [0, 0, 1280, 720], PixelSize::new(1300, 700)).is_err());
    assert!(twin_outer(&observed, [0, 0, 0, 0], PixelSize::new(400, 300)).is_err());
}

#[test]
fn clamp_to_twin_caps_each_axis_at_the_mode() {
    let observed = original();
    let big = twin_mode(PixelSize::new(1920, 1080), 1.0).unwrap();
    assert_eq!(
        clamp_to_twin(PixelSize::new(2000, 1500), big),
        PixelSize::new(1920, 1080)
    );
    assert_eq!(
        clamp_to_twin(PixelSize::new(2000, 300), big),
        PixelSize::new(1920, 300)
    );
    assert_eq!(
        twin_outer(
            &observed,
            [0, 0, 1920, 1080],
            clamp_to_twin(PixelSize::new(2000, 1500), big)
        )
        .unwrap(),
        [-8, 0, 1928, 1088]
    );
    // The twin is always 1920x1080, but the clamp is per mode: a 1280x720 mode caps both axes too.
    let small = TwinMode {
        width: 1280,
        height: 720,
        width_mm: 339,
        height_mm: 191,
    };
    assert_eq!(
        clamp_to_twin(PixelSize::new(400, 300), small),
        PixelSize::new(400, 300)
    );
    assert_eq!(
        clamp_to_twin(PixelSize::new(1300, 800), small),
        PixelSize::new(1280, 720)
    );
}

#[test]
fn find_twin_probe_takes_zero_or_one_and_refuses_two() {
    let rect = [0, 0, 1280, 720];
    let twin = twin_probe(rect);
    let mut not_a_twin = twin_probe(rect);
    not_a_twin.twin = false;
    let mut second = twin_probe(rect);
    second.device_path = "second-twin".into();

    assert_eq!(find_twin_probe(&[], TWIN_NAME, rect).unwrap(), None);
    assert_eq!(
        find_twin_probe(&[owned_probe(), not_a_twin.clone()], TWIN_NAME, rect).unwrap(),
        None
    );
    let one = [owned_probe(), not_a_twin, twin.clone()];
    assert_eq!(find_twin_probe(&one, TWIN_NAME, rect).unwrap(), Some(&twin));
    // The name and the rect must both match.
    assert_eq!(find_twin_probe(&one, "OTHER", rect).unwrap(), None);
    assert_eq!(
        find_twin_probe(&one, TWIN_NAME, [0, 0, 1920, 1080]).unwrap(),
        None
    );
    assert!(find_twin_probe(&[twin, second], TWIN_NAME, rect).is_err());
}
