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
