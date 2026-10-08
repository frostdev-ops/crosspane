//! Read-only Crosspane-rule query, in process through COM (WP-W1.8b). No elevation, mutation,
//! profile query or denial inference.
use crate::bounded_call::{Lane, Settled};
use crate::reachability::{
    MAX_RECORD_READ, RecordedId, RuleEvidence, com_rule_evidence, recorded_install_id,
};
use crate::windows::firewall_com::{ReadError, read_family};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::{
    fs::OpenOptions,
    io::{self, Read, Result as IoResult},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};

// Background diagnostic, never on a callback path. The in-process COM read takes about 140 ms on
// the dev VM, so 10 s leaves room for contention at logon. A timeout is Unavailable, and the next
// status demand retries.
const QUERY_WAIT: Duration = Duration::from_secs(10);
// One startup query, then at most one per 60 s on status demand (the W1.8 contract). Never periodic.
const QUERY_SPACING: Duration = Duration::from_secs(60);
#[cfg_attr(test, allow(dead_code))]
const POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub rule: RuleEvidence,
    pub checked: Option<Instant>,
}

pub(crate) struct Watch {
    snapshot: Arc<Mutex<Snapshot>>,
    demand: SyncSender<()>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Watch {
    pub(crate) fn new() -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (demand, requests) = mpsc::sync_channel(1);
        // Agent unit fixtures never read the owner's rule store.
        #[cfg(test)]
        let thread = {
            drop(requests);
            None
        };
        #[cfg(not(test))]
        let thread = {
            let state = Arc::clone(&snapshot);
            let stopping = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("crosspane-firewall".into())
                .spawn(move || {
                    let lane = Lane::default();
                    let mut last_started = None;
                    // Exactly one startup query. Subsequent work requires a status request and spacing.
                    loop {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let started = Instant::now();
                        if admit_query(&mut last_started, started) {
                            let rule = query(&stopping, &lane);
                            // A stop during the read is shutdown, so the snapshot stays as it was.
                            if stopping.load(Ordering::Acquire) {
                                break;
                            }
                            if let Ok(mut snapshot) = state.lock() {
                                *snapshot = Snapshot {
                                    rule,
                                    checked: Some(Instant::now()),
                                };
                            }
                        }
                        // No periodic query: the timeout checks shutdown only.
                        loop {
                            if stopping.load(Ordering::Acquire) {
                                return;
                            }
                            match requests.recv_timeout(POLL) {
                                Ok(()) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                    }
                })
                .ok()
        };
        Self {
            snapshot,
            demand,
            stop,
            thread,
        }
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }
    /// Nonblocking/coalesced; the worker owns both query admission and the read.
    pub(crate) fn demand(&self) {
        let _ = self.demand.try_send(());
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.demand.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn admit_query(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < QUERY_SPACING) {
        return false;
    }
    *last = Some(now);
    true
}
/// The canonical path of the running executable, as UTF-8. Any failure, or a non-UTF-8 path, is
/// `None`.
fn current_program() -> Option<String> {
    let program = std::env::current_exe().ok()?.canonicalize().ok()?;
    program.to_str().map(str::to_owned)
}
/// Presence of the exact rule for `install_id` and `program`, from one bounded read on `lane`.
/// Only `stop` gives `None`, and the caller then keeps its snapshot. Every other failure is
/// `Unavailable`.
fn query_rule(
    program: &str,
    install_id: &str,
    stop: &Arc<AtomicBool>,
    lane: &Lane,
) -> Option<RuleEvidence> {
    let deadline = Instant::now() + QUERY_WAIT;
    let worker_stop = Arc::clone(stop);
    let settled = lane.run("crosspane-firewall-com", stop, QUERY_WAIT, move || {
        read_family(&worker_stop, deadline)
    });
    match settled {
        Settled::Done(Ok(family)) => Some(com_rule_evidence(program, install_id, &family)),
        Settled::Done(Err(error)) => {
            // The HRESULT is the only detail logged. Never a rule name or a path.
            if let ReadError::Com(hresult) = error {
                tracing::debug!(hresult, "firewall rule read failed");
            }
            Some(RuleEvidence::Unavailable)
        }
        Settled::TimedOut | Settled::Busy | Settled::Failed => Some(RuleEvidence::Unavailable),
        Settled::Stopped => None,
    }
}
/// Reads `Installer\elevated-setup.json` under `state` (`%LOCALAPPDATA%\Crosspane`, the agent's
/// state directory). A missing record is `Absent`. An unset `LOCALAPPDATA`, a link, a non-file or
/// any other read failure is `Unreadable`. At most `MAX_RECORD_READ + 1` bytes are read, so an
/// oversized record is refused by `recorded_install_id`.
fn recorded_install_id_on_disk() -> RecordedId {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return RecordedId::Unreadable;
    };
    match read_record(&PathBuf::from(local).join("Crosspane")) {
        Ok(bytes) => recorded_install_id(Some(&bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => RecordedId::Absent,
        Err(_) => RecordedId::Unreadable,
    }
}
fn read_record(state: &Path) -> IoResult<Vec<u8>> {
    let installer = state.join("Installer");
    // The two directories are checked without following links, so a junction cannot redirect
    // the record read.
    for directory in [state, installer.as_path()] {
        let metadata = std::fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other(
                "record directory is not a plain directory",
            ));
        }
    }
    let file = OpenOptions::new()
        .read(true)
        // Open a final link itself, then refuse it below instead of following it.
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(installer.join("elevated-setup.json"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("record is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_READ as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}
/// The recorded rule's presence. No record is `Missing`, with no read. An unreadable record is
/// `Unavailable`.
fn query(stop: &Arc<AtomicBool>, lane: &Lane) -> RuleEvidence {
    match recorded_install_id_on_disk() {
        // No record means no exact name was ever recorded, so nothing is read.
        RecordedId::Absent => RuleEvidence::Missing,
        RecordedId::Unreadable => RuleEvidence::Unavailable,
        RecordedId::Id(id) => current_program()
            .and_then(|program| query_rule(&program, &id, stop, lane))
            .unwrap_or(RuleEvidence::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inert_watch_has_unverified_unavailable_facts_and_no_spawn() {
        let watch = Watch::new();
        watch.demand();
        assert_eq!(watch.snapshot().rule, RuleEvidence::Unavailable);
        assert!(watch.snapshot().checked.is_none());
        assert!(watch.thread.is_none());
        // The pinned timing contract (WP-W1.8b D4).
        assert_eq!(QUERY_WAIT.as_secs(), 10);
        assert_eq!(QUERY_SPACING.as_secs(), 60);
        // Referencing the production function verifies its compilation; it is never called.
        let _query: fn(&Arc<AtomicBool>, &Lane) -> RuleEvidence = query;
    }
    #[test]
    fn status_demand_is_coalesced_and_rate_limited() {
        let now = Instant::now();
        let mut last = None;
        assert!(admit_query(&mut last, now));
        assert!(!admit_query(&mut last, now + Duration::from_secs(59)));
        assert!(!admit_query(&mut last, now));
        assert!(admit_query(&mut last, now + Duration::from_secs(60)));
        assert!(!admit_query(&mut last, now + Duration::from_secs(60)));
    }
    fn limited_opt_in() {
        assert_eq!(
            std::env::var("CROSSPANE_W18_REACHABILITY").as_deref(),
            Ok("1")
        );
        assert!(
            !crate::windows::security::is_elevated().unwrap(),
            "Limited token required"
        );
    }
    #[test]
    #[ignore = "Limited read-only Crosspane rule query only; W1.8 explicit opt-in after held review"]
    fn owned_crosspane_rule_read_only_probe() {
        limited_opt_in();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let rule = query(&stop, &Lane::default());
        eprintln!(
            "owned_crosspane_rule_query evidence={} elapsed_ms={}",
            rule.token(),
            started.elapsed().as_millis()
        );
    }
    #[test]
    #[ignore = "Limited owned exact-rule query; W4.1c2 V5 opt-in with test-only variables"]
    fn owned_exact_rule_probe() {
        let (Ok(install_id), Ok(program)) = (
            std::env::var("CROSSPANE_W41C2_INSTALL_ID"),
            std::env::var("CROSSPANE_W41C2_PROGRAM"),
        ) else {
            eprintln!("owned_exact_rule_probe SKIP: CROSSPANE_W41C2_INSTALL_ID or _PROGRAM unset");
            return;
        };
        limited_opt_in();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let rule = query_rule(&program, &install_id, &stop, &Lane::default()).unwrap_or_default();
        eprintln!(
            "owned_exact_rule_probe evidence={} elapsed_ms={}",
            rule.token(),
            started.elapsed().as_millis()
        );
    }
    #[test]
    #[ignore = "Limited read-only COM family timing; W1.8b V3/V5 opt-in"]
    fn owned_com_family_read_timing() {
        limited_opt_in();
        let stop = AtomicBool::new(false);
        // One timed read on this thread. Only counts and durations are printed, never names.
        let timed_read = || {
            let started = Instant::now();
            let family = read_family(&stop, started + QUERY_WAIT);
            (started.elapsed(), family)
        };
        let (cold, cold_read) = timed_read();
        let Ok(family) = cold_read else {
            eprintln!(
                "owned_com_family_read_timing unavailable cold_ms={}",
                cold.as_millis()
            );
            return;
        };
        let mut warm_max = Duration::ZERO;
        for _ in 0..5 {
            let (elapsed, warm_read) = timed_read();
            if let Err(error) = warm_read {
                eprintln!("owned_com_family_read_timing unavailable warm error={error:?}");
                return;
            }
            warm_max = warm_max.max(elapsed);
        }
        eprintln!(
            "owned_com_family_read_timing family_count={} cold_ms={} warm_max_ms={}",
            family.len(),
            cold.as_millis(),
            warm_max.as_millis()
        );
    }
}
