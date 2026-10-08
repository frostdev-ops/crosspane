//! M2 controller `relocate`, read-only `inspect` and twin-aware `observed_geometry` (W3.2 T12).
//! Pure model checks; never invoke a desktop API. Fakes are copied from `mirror_model.rs`, with
//! the journal store shared into the port so a native move can read the durable image.
#![allow(clippy::unwrap_used)]
use crosspane_platform::{ParkingKind, PlatformError};
use crosspane_platform_windows::model::{
    geometry::{DisplayIds, MonitorProbe},
    journal::Show,
    parking::*,
};
use crosspane_types::{
    geom::{PixelRect, PixelSize, euclid::point2},
    id::WindowId,
};
use std::sync::{Arc, Mutex};

fn identity() -> NativeIdentity {
    NativeIdentity {
        hwnd: 7,
        pid: 8,
        tid: 9,
        process_created: 10,
    }
}
fn probe() -> MonitorProbe {
    MonitorProbe {
        device_path: "owned-monitor".into(),
        name: "OWNED".into(),
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
fn twin_probe() -> MonitorProbe {
    MonitorProbe {
        device_path: "owned-twin".into(),
        name: "TWIN".into(),
        rc_monitor: [0, 0, 1280, 720],
        rc_work: [0, 0, 1280, 720],
        primary: false,
        dpi: 96,
        refresh_millihz: 60000,
        edid: None,
        twin: true,
        quarter_turns: 0,
    }
}
fn observed() -> Observed {
    let visible = [-1880, -80, -1480, 220];
    let (geometry, path) = actual_geometry(
        WindowId(12345),
        visible,
        "OWNED",
        probe().rc_monitor,
        &[probe()],
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
struct Stored {
    images: MirrorJournalImages,
    fail: bool,
    commits: usize,
}
#[derive(Clone)]
struct Store(Arc<Mutex<Stored>>);
impl MirrorJournalStore for Store {
    fn read(&mut self) -> Result<MirrorJournalImages, PlatformError> {
        Ok(self.0.lock().unwrap().images.clone())
    }
    fn commit(&mut self, bytes: &[u8]) -> Result<(), PlatformError> {
        let mut s = self.0.lock().unwrap();
        s.commits += 1;
        s.images.pending = Some(bytes.to_vec());
        if s.fail {
            return Err(PlatformError::Backend("owned injected save failure".into()));
        }
        s.images.committed = Some(bytes.to_vec());
        Ok(())
    }
}
fn commits(store: &Store) -> usize {
    store.0.lock().unwrap().commits
}
struct Port {
    current: Option<Observed>,
    native_log: Vec<&'static str>,
    original_monitor_present: bool,
    /// For each native move, whether the durable journal already marked this identity.
    mutation_durable_at_move: Vec<bool>,
    store: Store,
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
        Ok(self.current.clone())
    }
    fn resize(
        &mut self,
        expected: NativeIdentity,
        outer: [i32; 4],
        _: WindowId,
    ) -> Result<Observed, PlatformError> {
        // The durable image must already say `may_have_mutated` before the native move runs.
        let images = self.store.0.lock().unwrap().images.clone();
        let (journal, _) = Journal::load(&images).unwrap();
        let durable = journal
            .entries()
            .iter()
            .any(|e| e.identity == expected && e.may_have_mutated);
        assert!(
            durable,
            "may_have_mutated must be committed before the native move"
        );
        self.mutation_durable_at_move.push(durable);
        self.native_log.push("resize");
        let mut o = self.current.clone().unwrap();
        o.outer = outer;
        o.visible = [outer[0] + 8, outer[1], outer[2] - 8, outer[3] - 8];
        let (g, _) = actual_geometry(
            WindowId(12345),
            o.visible,
            "OWNED",
            probe().rc_monitor,
            &[probe()],
            &mut DisplayIds::default(),
            o.fullscreen,
        )
        .unwrap();
        o.geometry = Some(g);
        self.current = Some(o.clone());
        Ok(o)
    }
    fn restore(
        &mut self,
        e: &MirrorEntry,
        _: Option<WindowId>,
    ) -> Result<RestoreOutcome, PlatformError> {
        if !self.original_monitor_present {
            return Ok(RestoreOutcome::MonitorGone);
        }
        self.native_log.push("restore");
        let mut o = self.current.clone().unwrap();
        o.outer = e.original.rect_physical;
        o.visible = e.visible_original;
        o.show = e.original.show;
        self.current = Some(o.clone());
        Ok(RestoreOutcome::Restored(o))
    }
}
fn controller() -> (Controller<Port>, Store) {
    let store = Store(Arc::default());
    let port = Port {
        current: Some(observed()),
        native_log: vec![],
        original_monitor_present: true,
        mutation_durable_at_move: vec![],
        store: store.clone(),
    };
    (
        Controller::new(Box::new(store.clone()), port).unwrap(),
        store,
    )
}

#[test]
fn relocate_makes_may_have_mutated_durable_before_the_native_move() {
    let (mut c, store) = controller();
    c.bind();
    c.park(WindowId(12345), PixelSize::new(484, 292), 1.0)
        .unwrap();
    let before = commits(&store);
    let target = [-1000, 0, -516, 300];
    let parked = c.relocate(WindowId(12345), target).unwrap();
    assert_eq!(c.port.native_log, ["resize"]);
    assert_eq!(c.port.mutation_durable_at_move, [true]);
    assert_eq!(commits(&store), before + 1);
    assert!(c.journal().entries().iter().all(|e| e.may_have_mutated));
    assert_eq!(c.port.current.as_ref().map(|o| o.outer), Some(target));
    assert_eq!(parked.kind, ParkingKind::Mirror);
    assert_eq!(
        parked.content,
        PixelRect::new(point2(928, 120), point2(1396, 412))
    );
}

#[test]
fn relocate_unknown_id_makes_no_native_call_and_no_commit() {
    let (mut c, store) = controller();
    c.bind();
    assert!(matches!(
        c.relocate(WindowId(999), [-1000, 0, -516, 300]),
        Err(PlatformError::NotFound)
    ));
    assert!(c.port.native_log.is_empty());
    assert_eq!(commits(&store), 0);
}

#[test]
fn relocate_publication_failure_makes_no_native_call() {
    let (mut c, store) = controller();
    c.bind();
    c.park(WindowId(12345), PixelSize::new(484, 292), 1.0)
        .unwrap();
    let journal = c.journal().clone();
    store.0.lock().unwrap().fail = true;
    assert!(matches!(
        c.relocate(WindowId(12345), [-1000, 0, -516, 300]),
        Err(PlatformError::Backend(_))
    ));
    assert!(c.port.native_log.is_empty());
    assert!(c.port.mutation_durable_at_move.is_empty());
    assert_eq!(c.journal(), &journal);
    assert!(c.faulted());
}

#[test]
fn inspect_commits_nothing_and_refuses_a_pending_identity() {
    let (mut c, store) = controller();
    c.bind();
    let seen = c.inspect(WindowId(12345)).unwrap();
    assert!(seen.eligible);
    assert_eq!(seen.outer, observed().outer);
    assert_eq!(commits(&store), 0);
    assert!(c.journal().entries().is_empty());
    assert!(c.port.native_log.is_empty());

    // Park, then strand the window: its original monitor is gone, so restore leaves it pending.
    c.park(WindowId(12345), PixelSize::new(484, 292), 1.0)
        .unwrap();
    let mut moved = observed();
    moved.outer = [-1800, -80, -1384, 228];
    c.port.current = Some(moved);
    c.port.original_monitor_present = false;
    assert_eq!(c.recover_startup().unwrap().pending, 1);
    let journal = c.journal().clone();
    let before = commits(&store);
    match c.inspect(WindowId(12345)) {
        Err(PlatformError::Unsupported(reason)) => assert_eq!(reason, PENDING_REPARK_REASON),
        other => panic!("a pending identity must refuse inspection, got {other:?}"),
    }
    assert_eq!(commits(&store), before);
    assert_eq!(c.journal(), &journal);
    assert!(c.port.native_log.is_empty());
}

#[test]
fn observed_geometry_maps_a_single_twin_probe_to_local_content() {
    let probes = [probe(), twin_probe()];
    let mut ids = DisplayIds::default();
    let (parked, path) = observed_geometry(
        WindowId(12345),
        [100, 100, 500, 400],
        "TWIN",
        [0, 0, 1280, 720],
        &probes,
        &mut ids,
        true,
    )
    .unwrap();
    assert_eq!(parked.window, WindowId(12345));
    assert_eq!(parked.kind, ParkingKind::Twin);
    assert_eq!(parked.display, ids.assign("owned-twin").unwrap());
    assert_eq!(
        parked.content,
        PixelRect::new(point2(100, 100), point2(500, 400))
    );
    assert!(parked.fullscreen);
    assert_eq!(path, "owned-twin");
}

#[test]
fn observed_geometry_without_a_twin_match_is_actual_geometry() {
    // The twin probe's name differs from the window's monitor, so it never matches.
    let probes = [probe(), twin_probe()];
    let visible = [-1880, -80, -1480, 220];
    let rect = probe().rc_monitor;
    let via_observed = observed_geometry(
        WindowId(12345),
        visible,
        "OWNED",
        rect,
        &probes,
        &mut DisplayIds::default(),
        false,
    )
    .unwrap();
    let via_actual = actual_geometry(
        WindowId(12345),
        visible,
        "OWNED",
        rect,
        &probes,
        &mut DisplayIds::default(),
        false,
    )
    .unwrap();
    assert_eq!(via_observed, via_actual);
    assert_eq!(via_observed.0.kind, ParkingKind::Mirror);
}

#[test]
fn observed_geometry_refuses_two_matching_twins() {
    let mut second = twin_probe();
    second.device_path = "owned-twin-2".into();
    let probes = [probe(), twin_probe(), second];
    let result = observed_geometry(
        WindowId(12345),
        [100, 100, 500, 400],
        "TWIN",
        [0, 0, 1280, 720],
        &probes,
        &mut DisplayIds::default(),
        false,
    );
    assert!(matches!(result, Err(PlatformError::Backend(_))));
}
