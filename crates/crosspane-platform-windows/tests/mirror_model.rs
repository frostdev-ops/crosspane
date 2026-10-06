//! Frozen M1 durability/geometry/guard model checks; never invoke a desktop API.
#![allow(clippy::unwrap_used)]
use crosspane_platform::PlatformError;
use crosspane_platform_windows::model::{
    geometry::{DisplayIds, MonitorProbe},
    journal::{Original, Show},
    parking::*,
};
use crosspane_types::{geom::PixelSize, id::WindowId};
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
fn entry() -> MirrorEntry {
    let o = observed();
    MirrorEntry {
        identity: identity(),
        original: Original {
            rect_physical: o.outer,
            monitor_path: o.monitor_path,
            show: o.show,
            dpi: o.dpi,
        },
        visible_original: o.visible,
        may_have_mutated: false,
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
struct Port {
    current: Option<Observed>,
    query_error: bool,
    abandoned: bool,
    wrong_identity: bool,
    native_log: Vec<&'static str>,
    constrained: bool,
    original_monitor_present: bool,
    restore_preflight_error: bool,
}
impl NativePort for Port {
    fn check(&self) -> Result<(), PlatformError> {
        if self.abandoned {
            Err(PlatformError::Timeout)
        } else {
            Ok(())
        }
    }
    fn resolve(&mut self, _: WindowId) -> Result<NativeIdentity, PlatformError> {
        self.check()?;
        if self.wrong_identity {
            Err(PlatformError::NotFound)
        } else {
            Ok(identity())
        }
    }
    fn inspect(
        &mut self,
        _: NativeIdentity,
        _: Option<WindowId>,
    ) -> Result<Option<Observed>, PlatformError> {
        self.check()?;
        if self.query_error {
            Err(PlatformError::SecureInput)
        } else {
            Ok(self.current.clone())
        }
    }
    fn resize(
        &mut self,
        _: NativeIdentity,
        outer: [i32; 4],
        _: WindowId,
    ) -> Result<Observed, PlatformError> {
        self.check()?;
        self.native_log.push("resize");
        let mut o = self.current.clone().unwrap();
        let mut target = outer;
        if self.constrained {
            target[2] = target[0] + 516;
        }
        o.outer = target;
        o.visible = [target[0] + 8, target[1], target[2] - 8, target[3] - 8];
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
        self.check()?;
        if self.restore_preflight_error {
            return Err(PlatformError::SecureInput);
        }
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
        query_error: false,
        abandoned: false,
        wrong_identity: false,
        native_log: vec![],
        constrained: false,
        original_monitor_present: true,
        restore_preflight_error: false,
    };
    (
        Controller::new(Box::new(store.clone()), port).unwrap(),
        store,
    )
}
#[test]
fn complete_documents_and_every_truncation_refuse_without_partial_load() {
    let j = Journal::empty().insert(entry()).unwrap();
    let b = j.bytes().unwrap();
    assert_eq!(
        Journal::load(&MirrorJournalImages {
            committed: Some(b.clone()),
            pending: Some(b.clone())
        })
        .unwrap()
        .0,
        j
    );
    for n in 0..b.len() {
        assert!(
            Journal::load(&MirrorJournalImages {
                committed: Some(b[..n].to_vec()),
                pending: Some(b.clone())
            })
            .is_err(),
            "truncation {n}"
        );
    }
    let bad = String::from_utf8(b.clone())
        .unwrap()
        .replace(FORMAT, "crosspane-win-twin-v1")
        .into_bytes();
    assert!(
        Journal::load(&MirrorJournalImages {
            committed: Some(bad),
            pending: Some(b.clone())
        })
        .is_err()
    );
    assert!(
        Journal::load(&MirrorJournalImages {
            committed: Some(vec![b' '; MAX_BYTES + 1]),
            pending: None
        })
        .is_err()
    );
    let mut j = Journal::empty();
    for hwnd in 1..=MAX_ENTRIES as u64 {
        let mut e = entry();
        e.identity.hwnd = hwnd;
        j = j.insert(e).unwrap();
    }
    let mut e = entry();
    e.identity.hwnd = 999;
    assert!(j.insert(e).is_err());
    let duplicate = String::from_utf8(b)
        .unwrap()
        .replace(
            "\"entries\":[",
            &format!("\"entries\":[{},", serde_json::to_string(&entry()).unwrap()),
        )
        .into_bytes();
    assert!(
        Journal::load(&MirrorJournalImages {
            committed: Some(duplicate),
            pending: None
        })
        .is_err()
    );
}
#[test]
fn interrupted_revision_reconciliation_is_conservative_and_generation_never_wraps() {
    let a = Journal::empty().insert(entry()).unwrap();
    let mut e = entry();
    e.identity.hwnd = 11;
    let b = a.insert(e.clone()).unwrap();
    e.identity.hwnd = 12;
    let c = b.insert(e).unwrap();
    let load = |a: Option<&Journal>, b: Option<&Journal>| {
        Journal::load(&MirrorJournalImages {
            committed: a.map(|j| j.bytes().unwrap()),
            pending: b.map(|j| j.bytes().unwrap()),
        })
    };
    assert_eq!(load(Some(&a), Some(&b)).unwrap(), (b.clone(), true));
    assert_eq!(load(Some(&b), Some(&a)).unwrap(), (b.clone(), true));
    assert_eq!(load(None, Some(&a)).unwrap(), (a.clone(), true));
    assert!(load(Some(&a), Some(&c)).is_err());
    let mut e = entry();
    e.original.rect_physical[0] -= 1;
    let conflicting = Journal::empty().insert(e).unwrap();
    assert!(load(Some(&a), Some(&conflicting)).is_err());
    let bytes = String::from_utf8(a.bytes().unwrap())
        .unwrap()
        .replace("\"generation\":1", &format!("\"generation\":{}", u64::MAX))
        .into_bytes();
    let max = Journal::load(&MirrorJournalImages {
        committed: Some(bytes),
        pending: None,
    })
    .unwrap()
    .0;
    let mut e = entry();
    e.identity.hwnd = 55;
    assert!(max.insert(e).is_err());
}
#[test]
fn park_is_in_place_repeated_park_preserves_original_and_save_failure_prevents_native_work() {
    let (mut c, s) = controller();
    assert!(
        c.park(WindowId(12345), PixelSize::new(400, 300), 2.0)
            .is_err()
    );
    c.bind();
    c.park(WindowId(12345), PixelSize::new(400, 300), 2.0)
        .unwrap();
    let original = c.journal().entries()[0].clone();
    assert!(c.port.native_log.is_empty());
    c.resize(WindowId(12345), PixelSize::new(250, 200), 1.25)
        .unwrap();
    c.park(WindowId(12345), PixelSize::new(250, 200), 3.0)
        .unwrap();
    assert_eq!(c.journal().entries()[0].original, original.original);
    let log = c.port.native_log.clone();
    s.0.lock().unwrap().fail = true;
    assert!(
        c.resize(WindowId(12345), PixelSize::new(300, 200), 1.0)
            .is_err()
    );
    assert_eq!(c.port.native_log, log);
    s.0.lock().unwrap().fail = false;
    assert!(
        c.resize(WindowId(12345), PixelSize::new(300, 200), 1.0)
            .is_err()
    );
    assert_eq!(c.port.native_log, log);
}
#[test]
fn actual_device_geometry_uses_negative_origin_and_not_destination_scale() {
    let (mut c, _) = controller();
    c.bind();
    let actual = c
        .park(WindowId(12345), PixelSize::new(400, 300), 2.75)
        .unwrap();
    assert_eq!(
        (
            actual.content.min.x,
            actual.content.min.y,
            actual.content.width(),
            actual.content.height()
        ),
        (40, 40, 400, 300)
    );
    c.port.constrained = true;
    let actual = c
        .resize(WindowId(12345), PixelSize::new(200, 180), 2.75)
        .unwrap();
    assert_eq!(
        (actual.content.width(), actual.content.height()),
        (500, 180)
    );
    assert_eq!(c.port.native_log, ["resize"]);
    let p = probe();
    assert!(
        actual_geometry(
            WindowId(12345),
            observed().visible,
            "MISSING",
            p.rc_monitor,
            std::slice::from_ref(&p),
            &mut DisplayIds::default(),
            false
        )
        .is_err()
    );
    assert!(
        actual_geometry(
            WindowId(12345),
            observed().visible,
            "OWNED",
            p.rc_monitor,
            &[p.clone(), p],
            &mut DisplayIds::default(),
            false
        )
        .is_err()
    );
}
#[test]
fn missing_or_reused_retires_without_mutation_unknown_integrity_and_show_changes_keep_entries() {
    let (mut c, _) = controller();
    c.bind();
    c.park(WindowId(12345), PixelSize::new(400, 300), 1.0)
        .unwrap();
    c.port.query_error = true;
    assert!(c.recover_startup().is_err());
    assert_eq!(c.journal().entries().len(), 1);
    assert!(c.port.native_log.is_empty());
    c.port.query_error = false;
    c.port.current.as_mut().unwrap().show = Show::Maximized;
    assert!(matches!(
        c.restore(WindowId(12345)),
        Err(PlatformError::Unsupported(_))
    ));
    assert_eq!(c.journal().entries().len(), 1);
    c.port.current = None;
    let r = c.recover_startup().unwrap();
    assert_eq!((r.restored, r.retired, r.pending), (0, 1, 0));
    assert!(c.port.native_log.is_empty());
}
#[test]
fn exact_cleanup_restores_and_never_fabricates_window_ids_at_startup() {
    let (mut c, s) = controller();
    c.bind();
    c.park(WindowId(12345), PixelSize::new(400, 300), 1.0)
        .unwrap();
    c.resize(WindowId(12345), PixelSize::new(220, 180), 1.0)
        .unwrap();
    assert_eq!(c.recover().unwrap(), [WindowId(12345)]);
    assert!(c.journal().entries().is_empty());
    assert_eq!(c.port.native_log, ["resize", "restore"]);
    let journal = Journal::empty().insert(entry()).unwrap();
    s.0.lock().unwrap().images = MirrorJournalImages {
        committed: Some(journal.bytes().unwrap()),
        pending: Some(journal.bytes().unwrap()),
    };
    let (mut fresh, _) = controller();
    fresh = Controller::new(Box::new(s), fresh.port).unwrap();
    fresh.port.current.as_mut().unwrap().outer[2] += 20;
    assert!(fresh.recover().is_err());
    let report = fresh.recover_startup().unwrap();
    assert_eq!((report.restored, report.retired, report.pending), (1, 0, 0));
}
#[test]
fn hidden_iconic_cloaked_model_refusals_and_fullscreen_ensure_are_truthful() {
    let (mut c, _) = controller();
    c.bind();
    c.port.current.as_mut().unwrap().eligible = false;
    assert!(
        c.park(WindowId(12345), PixelSize::new(400, 300), 1.0)
            .is_err()
    );
    assert!(c.journal().entries().is_empty());
    c.port.current.as_mut().unwrap().eligible = true;
    c.park(WindowId(12345), PixelSize::new(400, 300), 1.0)
        .unwrap();
    assert!(matches!(
        c.set_fullscreen(WindowId(12345), true),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(c.set_fullscreen(WindowId(12345), false).is_ok());
    c.port.current.as_mut().unwrap().show = Show::Maximized;
    assert!(
        c.resize(WindowId(12345), PixelSize::new(400, 300), 2.0)
            .is_ok()
    );
    assert!(matches!(
        c.resize(WindowId(12345), PixelSize::new(200, 100), 2.0),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(c.port.native_log.is_empty());
}
#[test]
fn abandoned_stream_and_reused_resolver_never_start_more_native_work() {
    let (mut c, _) = controller();
    c.bind();
    c.park(WindowId(12345), PixelSize::new(400, 300), 1.0)
        .unwrap();
    c.port.abandoned = true;
    assert!(matches!(
        c.resize(WindowId(12345), PixelSize::new(200, 100), 1.0),
        Err(PlatformError::Timeout)
    ));
    assert_eq!(c.journal().entries().len(), 1);
    assert!(c.port.native_log.is_empty());
    c.port.abandoned = false;
    c.port.wrong_identity = true;
    assert!(
        c.resize(WindowId(12345), PixelSize::new(200, 100), 1.0)
            .is_err()
    );
    assert!(c.port.native_log.is_empty());
    c.port.wrong_identity = false;
    c.resize(WindowId(12345), PixelSize::new(210, 110), 1.0)
        .unwrap();
    c.resize(WindowId(12345), PixelSize::new(220, 120), 1.0)
        .unwrap();
    assert_eq!(c.port.native_log, ["resize", "resize"]);
}

#[test]
fn consecutive_revision_cannot_change_a_common_original_and_republish_failure_is_not_loaded() {
    let a = Journal::empty().insert(entry()).unwrap();
    let mut e = entry();
    e.identity.hwnd = 11;
    let b = a.insert(e).unwrap();
    let bytes = String::from_utf8(b.bytes().unwrap())
        .unwrap()
        .replace("-1888", "-1889")
        .into_bytes();
    assert!(
        Journal::load(&MirrorJournalImages {
            committed: Some(a.bytes().unwrap()),
            pending: Some(bytes)
        })
        .is_err()
    );
    let (c, s) = controller();
    s.0.lock().unwrap().images = MirrorJournalImages {
        committed: Some(a.bytes().unwrap()),
        pending: None,
    };
    s.0.lock().unwrap().fail = true;
    assert!(Controller::new(Box::new(s), c.port).is_err());
}

#[test]
fn repeated_restore_and_recover_then_restore_are_noops_without_resolving_absent_ids() {
    let (mut c, _) = controller();
    c.bind();
    let id = WindowId(12345);
    c.park(id, PixelSize::new(400, 300), 1.0).unwrap();
    c.resize(id, PixelSize::new(250, 180), 1.0).unwrap();
    c.restore(id).unwrap();
    let log = c.port.native_log.clone();
    c.port.wrong_identity = true;
    c.restore(id).unwrap();
    assert_eq!(c.port.native_log, log);
    c.port.wrong_identity = false;
    c.park(id, PixelSize::new(400, 300), 1.0).unwrap();
    c.resize(id, PixelSize::new(240, 170), 1.0).unwrap();
    c.recover().unwrap();
    let log = c.port.native_log.clone();
    c.port.wrong_identity = true;
    c.restore(id).unwrap();
    c.restore(WindowId(99999)).unwrap();
    assert_eq!(c.port.native_log, log);
}

#[test]
fn removed_original_monitor_retains_pending_entry_for_a_later_startup_retry() {
    let (mut c, _) = controller();
    c.bind();
    let id = WindowId(12345);
    c.park(id, PixelSize::new(400, 300), 1.0).unwrap();
    c.resize(id, PixelSize::new(250, 180), 1.0).unwrap();
    let log = c.port.native_log.clone();
    c.port.original_monitor_present = false;
    assert!(matches!(c.restore(id), Err(PlatformError::Unsupported(_))));
    let pending = c.recover_startup().unwrap();
    assert_eq!(
        (pending.restored, pending.retired, pending.pending),
        (0, 0, 1)
    );
    assert!(matches!(
        c.park(id, PixelSize::new(400, 300), 1.0),
        Err(PlatformError::Unsupported(PENDING_REPARK_REASON))
    ));
    assert_eq!(c.journal().entries().len(), 1);
    assert_eq!(c.port.native_log, log);
    c.port.original_monitor_present = true;
    let report = c.recover_startup().unwrap();
    assert_eq!((report.restored, report.retired, report.pending), (1, 0, 0));
    assert!(c.journal().entries().is_empty());
}

#[test]
fn strict_post_publication_preflight_failure_retains_entry_instead_of_pending_success() {
    let (mut c, store) = controller();
    c.bind();
    let id = WindowId(12345);
    c.park(id, PixelSize::new(400, 300), 1.0).unwrap();
    c.resize(id, PixelSize::new(250, 180), 1.0).unwrap();
    let log = c.port.native_log.clone();
    let commits = store.0.lock().unwrap().commits;
    c.port.original_monitor_present = false;
    c.port.restore_preflight_error = true;
    assert!(matches!(
        c.recover_startup(),
        Err(PlatformError::SecureInput)
    ));
    assert!(store.0.lock().unwrap().commits > commits);
    assert_eq!(c.journal().entries().len(), 1);
    assert_eq!(c.port.native_log, log);
    c.port.restore_preflight_error = false;
    assert_eq!(c.recover_startup().unwrap().pending, 1);
}

fn marker_observed() -> MarkerObservation {
    MarkerObservation {
        identity: identity(),
        frame: [-1880, -80, -1480, 220],
        dpi: 144,
        visible: true,
        minimized: false,
        cloaked: false,
        topmost: false,
        session_allowed: true,
    }
}
#[test]
fn marker_park_move_resize_follow_exact_physical_frame_at_source_dpi() {
    let mut marker = MarkerModel::default();
    let mut facts = marker_observed();
    assert_eq!(
        marker.park(facts).unwrap(),
        MarkerState::Shown(MarkerFrame {
            rect: facts.frame,
            border: 3,
            topmost: false,
        })
    );
    let generation = marker.generation();
    facts.frame = [-1900, -100, -1360, 300];
    facts.dpi = 192;
    assert_eq!(
        marker.observe(facts, true).unwrap(),
        MarkerState::Shown(MarkerFrame {
            rect: facts.frame,
            border: 4,
            topmost: false,
        })
    );
    assert_eq!(marker.generation(), generation);
}
#[test]
fn marker_minimize_hide_cloak_hide_and_fresh_restore_shows() {
    for kind in 0..3 {
        let mut marker = MarkerModel::default();
        let original = marker_observed();
        marker.park(original).unwrap();
        let mut hidden = original;
        match kind {
            0 => hidden.minimized = true,
            1 => hidden.visible = false,
            _ => hidden.cloaked = true,
        }
        assert_eq!(marker.observe(hidden, true).unwrap(), MarkerState::Hidden);
        assert!(matches!(
            marker.observe(original, true).unwrap(),
            MarkerState::Shown(_)
        ));
    }
}
#[test]
fn marker_destroy_restore_drop_secure_inactive_remove_and_repark_recreates() {
    for reason in 0..5 {
        let mut marker = MarkerModel::default();
        let original = marker_observed();
        marker.park(original).unwrap();
        let generation = marker.generation();
        match reason {
            0..=2 => marker.remove(), // Native destroy, successful restore and owner Drop.
            _ => {
                let mut blocked = original;
                blocked.session_allowed = false;
                assert_eq!(marker.observe(blocked, true).unwrap(), MarkerState::Absent);
            }
        }
        assert_eq!(marker.state(), MarkerState::Absent);
        assert_eq!(marker.observe(original, true).unwrap(), MarkerState::Absent);
        assert!(matches!(
            marker.park(original).unwrap(),
            MarkerState::Shown(_)
        ));
        assert_eq!(marker.generation(), generation + 1);
    }
    let mut marker = MarkerModel::default();
    let mut replaced = marker_observed();
    marker.park(replaced).unwrap();
    replaced.identity.process_created += 1;
    assert_eq!(marker.observe(replaced, true).unwrap(), MarkerState::Absent);
}
#[test]
fn marker_band_matches_only_source_and_failed_adjacency_waits_for_change() {
    let mut marker = MarkerModel::default();
    let mut facts = marker_observed();
    marker.park(facts).unwrap();
    facts.topmost = true;
    assert!(matches!(
        marker.observe(facts, true).unwrap(),
        MarkerState::Shown(MarkerFrame { topmost: true, .. })
    ));
    marker.adjacency_failed();
    assert_eq!(marker.observe(facts, false).unwrap(), MarkerState::Hidden);
    assert!(matches!(
        marker.observe(facts, true).unwrap(),
        MarkerState::Shown(_)
    ));
    facts.topmost = false;
    assert!(matches!(
        marker.observe(facts, true).unwrap(),
        MarkerState::Shown(MarkerFrame { topmost: false, .. })
    ));
}

#[test]
fn marker_fresh_revalidation_refuses_changed_identity_and_lost_session() {
    for identity_changed in [false, true] {
        let mut marker = MarkerModel::default();
        let original = marker_observed();
        marker.park(original).unwrap();
        let mut fresh = original;
        if identity_changed {
            fresh.identity.process_created += 1;
        } else {
            fresh.session_allowed = false;
        }
        assert_eq!(marker.observe(fresh, false).unwrap(), MarkerState::Absent);
        assert_eq!(marker.observe(original, true).unwrap(), MarkerState::Absent);
        assert!(matches!(
            marker.park(original).unwrap(),
            MarkerState::Shown(_)
        ));
    }
}

#[test]
fn marker_decoration_failure_rolls_back_real_park_in_place() {
    let (mut c, store) = controller();
    c.bind();
    let before = c.port.current.as_ref().unwrap().outer;
    let mut decorated = false;
    let error = c
        .park_decorated(WindowId(12345), PixelSize::new(900, 700), 2.0, |port| {
            // Actual M1 park never changes source geometry to requested dimensions.
            assert_eq!(port.current.as_ref().unwrap().outer, before);
            assert!(port.native_log.is_empty());
            decorated = true;
            Err(PlatformError::Unsupported("owned marker creation refused"))
        })
        .unwrap_err();
    assert!(decorated);
    assert!(matches!(
        error,
        PlatformError::Unsupported("owned marker creation refused")
    ));
    assert_eq!(c.port.current.as_ref().unwrap().outer, before);
    assert!(c.port.native_log.is_empty());
    assert!(c.journal().entries().is_empty());
    assert!(
        Journal::load(&store.0.lock().unwrap().images)
            .unwrap()
            .0
            .entries()
            .is_empty()
    );
}
#[test]
fn marker_decoration_failure_retains_journal_when_concurrent_move_restore_refuses() {
    let (mut c, store) = controller();
    c.bind();
    let before = c.port.current.as_ref().unwrap().outer;
    let error = c
        .park_decorated(WindowId(12345), PixelSize::new(400, 300), 1.0, |port| {
            // Model an external app/fixture move while decoration is attempted, never a marker move.
            let observed = port.current.as_mut().unwrap();
            observed.outer[2] += 20;
            observed.visible[2] += 20;
            port.restore_preflight_error = true;
            Err(PlatformError::Unsupported("owned marker creation refused"))
        })
        .unwrap_err();
    let PlatformError::Backend(detail) = error else {
        panic!("combined rollback failure required");
    };
    assert!(detail.contains("owned marker creation refused"));
    assert!(detail.contains(&PlatformError::SecureInput.to_string()));
    assert!(detail.contains("journal retained"));
    assert_ne!(c.port.current.as_ref().unwrap().outer, before);
    assert!(c.port.native_log.is_empty());
    assert_eq!(c.journal().entries().len(), 1);
    assert!(c.journal().entries()[0].may_have_mutated);
    assert_eq!(
        Journal::load(&store.0.lock().unwrap().images)
            .unwrap()
            .0
            .entries()
            .len(),
        1
    );
}
#[test]
fn marker_journal_fault_query_poison_prevents_later_decoration_or_source_mutation() {
    let (mut c, store) = controller();
    c.bind();
    c.park_decorated(WindowId(12345), PixelSize::new(400, 300), 1.0, |_| Ok(()))
        .unwrap();
    assert!(!c.faulted());
    store.0.lock().unwrap().fail = true;
    assert!(
        c.resize(WindowId(12345), PixelSize::new(200, 100), 1.0)
            .is_err()
    );
    assert!(c.faulted());
    let commits = store.0.lock().unwrap().commits;
    assert!(
        c.park_decorated(WindowId(12345), PixelSize::new(400, 300), 1.0, |_| panic!(
            "faulted journal must not reach decoration"
        ))
        .is_err()
    );
    assert!(c.restore(WindowId(12345)).is_err());
    assert!(
        c.resize(WindowId(12345), PixelSize::new(300, 200), 1.0)
            .is_err()
    );
    assert_eq!(store.0.lock().unwrap().commits, commits);
    assert!(c.port.native_log.is_empty());
    assert_eq!(c.journal().entries().len(), 1);
}

#[test]
fn marker_desktop_current_guid_error_zero_keeps_real_park() {
    for guid in [MarkerDesktopGuid::Error, MarkerDesktopGuid::Zero] {
        for initial in [false, true] {
            assert_eq!(
                marker_desktop_placement(
                    initial,
                    MarkerDesktopMembership::Current,
                    guid,
                    true,
                    false
                ),
                MarkerDesktopPlacement::Keep
            );
        }
        let (mut c, store) = controller();
        c.bind();
        let before = c.port.current.as_ref().unwrap().outer;
        let parked = c
            .park_decorated(WindowId(12345), PixelSize::new(400, 300), 1.0, |port| {
                assert_eq!(
                    marker_desktop_placement(
                        true,
                        MarkerDesktopMembership::Current,
                        guid,
                        true,
                        false
                    ),
                    MarkerDesktopPlacement::Keep
                );
                assert_eq!(port.current.as_ref().unwrap().outer, before);
                assert!(port.native_log.is_empty());
                Ok(())
            })
            .unwrap();
        assert_eq!(parked.kind, crosspane_platform::ParkingKind::Mirror);
        assert_eq!(c.journal().entries().len(), 1);
        assert!(c.port.native_log.is_empty());
        c.restore(WindowId(12345)).unwrap();
        assert!(c.journal().entries().is_empty());
        assert!(
            Journal::load(&store.0.lock().unwrap().images)
                .unwrap()
                .0
                .entries()
                .is_empty()
        );
    }
}
#[test]
fn marker_desktop_initial_unknown_requires_visible_uncloaked_and_later_hides() {
    use MarkerDesktopGuid::{Error, Known, Zero};
    use MarkerDesktopMembership::{Current, Other, Unknown};
    use MarkerDesktopPlacement::{Follow, Hide, Keep};
    assert_eq!(
        marker_desktop_placement(true, Current, Known, true, false),
        Follow
    );
    for guid in [Error, Zero, Known] {
        assert_eq!(
            marker_desktop_placement(true, Unknown, guid, true, false),
            Keep
        );
        assert_eq!(
            marker_desktop_placement(true, Unknown, guid, false, false),
            Hide
        );
        assert_eq!(
            marker_desktop_placement(true, Unknown, guid, true, true),
            Hide
        );
        assert_eq!(
            marker_desktop_placement(false, Unknown, guid, true, false),
            Hide
        );
        assert_eq!(
            marker_desktop_placement(true, Other, guid, true, false),
            Hide
        );
        assert_eq!(
            marker_desktop_placement(false, Other, guid, true, false),
            Hide
        );
    }
}
#[test]
fn marker_desktop_own_unknown_only_allows_initial_keep_and_other_never_shows() {
    use MarkerDesktopMembership::{Current, Other, Unknown};
    use MarkerDesktopPlacement::{Follow, Hide, Keep};
    for (initial, placement, own, expected) in [
        (true, Keep, Unknown, true),
        (false, Keep, Unknown, false),
        (true, Follow, Unknown, false),
        (false, Follow, Unknown, false),
        (true, Keep, Current, true),
        (false, Keep, Current, true),
        (true, Follow, Current, true),
        (false, Follow, Current, true),
        (true, Keep, Other, false),
        (false, Keep, Other, false),
        (true, Follow, Other, false),
        (false, Follow, Other, false),
        (true, Hide, Current, false),
        (true, Hide, Unknown, false),
    ] {
        assert_eq!(marker_desktop_ready(initial, placement, own), expected);
    }
}
#[test]
fn marker_desktop_fallback_never_masks_genuine_creation_failure_or_rollback() {
    for stage in [
        "marker class unavailable",
        "marker window unavailable",
        "marker observer unavailable",
    ] {
        let (mut c, store) = controller();
        c.bind();
        let before = c.port.current.as_ref().unwrap().outer;
        let error = c
            .park_decorated(WindowId(12345), PixelSize::new(400, 300), 1.0, |_| {
                assert_eq!(
                    marker_desktop_placement(
                        true,
                        MarkerDesktopMembership::Current,
                        MarkerDesktopGuid::Error,
                        true,
                        false
                    ),
                    MarkerDesktopPlacement::Keep
                );
                Err(PlatformError::Backend(stage.into()))
            })
            .unwrap_err();
        assert!(matches!(error, PlatformError::Backend(detail) if detail == stage));
        assert_eq!(c.port.current.as_ref().unwrap().outer, before);
        assert!(c.port.native_log.is_empty());
        assert!(c.journal().entries().is_empty());
        assert!(
            Journal::load(&store.0.lock().unwrap().images)
                .unwrap()
                .0
                .entries()
                .is_empty()
        );
    }
}
