//! Installer lifecycle facts. Only `run` owns a bootstrap writer; erase is a locked one-shot.
//! Owner-chosen config/state parent symlinks are supported; retargeting them requires owner privileges and is outside the threat model.
//! A timed-out Secret Service write may still finish in the daemon after the identity one-shot
//! exits; the frozen KeyStore cannot establish or cancel that completion.

use std::fs::File;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use anyhow::bail;
use anyhow::{Context, Result};
use crosspane_input::journal::Journal;
use crosspane_platform::{KeyStore, PlatformError};
use serde::{Deserialize, Serialize};

use crate::keys::KeySource;
use crate::paths::{Paths, write_private};

pub fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

/// One run's stamp, allocated before a key exists and shared with installer status.
#[derive(Clone, Copy, Debug)]
pub struct Instance {
    pub id: u64,
    pub pid: u32,
    pub started_unix_ms: u64,
}

impl Instance {
    fn current() -> Result<Self> {
        let pid = std::process::id();
        #[cfg(unix)]
        let started_unix_ms = {
            let output = std::process::Command::new("/bin/ps")
                .args(["-o", "lstart=", "-p", &pid.to_string()])
                .env("LC_ALL", "C")
                .env("TZ", "UTC")
                .output()
                .context("probe agent process start")?;
            if !output.status.success() {
                bail!("could not probe agent process start");
            }
            parse_start(std::str::from_utf8(&output.stdout)?)
                .context("invalid process start time from ps")?
        };
        #[cfg(windows)]
        let started_unix_ms = crate::windows::process::started_unix_ms()?;
        // A restart in place keeps the PID and process start, so include this run's fresh time.
        let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
        Ok(Self {
            id: instance_id(pid, now),
            pid,
            started_unix_ms,
        })
    }
}

/// The amended WP-4.5 contract: exactly 4 little-endian PID bytes, then 8 start-ns bytes.
pub(crate) fn instance_id(pid: u32, start_ns: u64) -> u64 {
    let mut bytes = [0; 12];
    bytes[..4].copy_from_slice(&pid.to_le_bytes());
    bytes[4..].copy_from_slice(&start_ns.to_le_bytes());
    xxhash_rust::xxh3::xxh3_64(&bytes)
}

/// Parse `LC_ALL=C TZ=UTC ps -o lstart=` without a new dependency or OS bindings.
#[cfg(any(unix, test))]
fn parse_start(text: &str) -> Option<u64> {
    let parts: Vec<_> = text.split_whitespace().collect();
    let [_, month, day, clock, year] = parts.as_slice() else {
        return None;
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == month)? as i64
        + 1;
    let year: i64 = year.parse().ok()?;
    let day: i64 = day.parse().ok()?;
    let time: Vec<u64> = clock
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let [hour, minute, second] = time.as_slice() else {
        return None;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&year)
        || !(1..=days_in_month[usize::try_from(month - 1).ok()?]).contains(&day)
        || *hour > 23
        || *minute > 59
        || *second > 59
    {
        return None;
    }
    // Gregorian civil date to days since 1970-01-01.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let seconds = u64::try_from(days).ok()? * 86_400 + hour * 3600 + minute * 60 + second;
    seconds.checked_mul(1000)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    WaitingForKeystore,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Config,
    Platform,
    Keystore,
    Socket,
    Other,
}

#[derive(Clone, Serialize)]
struct Bootstrap {
    schema_version: u32,
    instance_id: u64,
    pid: u32,
    started_unix_ms: u64,
    phase: Phase,
    phase_seq: u64,
    keystore: Option<&'static str>,
    reason: Option<Failure>,
    runtime_dir: String,
}

pub struct Lifecycle {
    pub instance: Instance,
    paths: Paths,
    bootstrap: Bootstrap,
}

impl Lifecycle {
    /// Called immediately after taking the instance lock. Invalidate a previous receipt first.
    pub fn start(paths: &Paths) -> Result<Self> {
        remove_if_present(&paths.exit_receipt())?;
        Self::with_instance(paths, Instance::current()?)
    }

    fn with_instance(paths: &Paths, instance: Instance) -> Result<Self> {
        let mut lifecycle = Self {
            instance,
            paths: paths.clone(),
            bootstrap: Bootstrap {
                schema_version: 1,
                instance_id: instance.id,
                pid: instance.pid,
                started_unix_ms: instance.started_unix_ms,
                phase: Phase::Starting,
                phase_seq: 0,
                keystore: None,
                reason: None,
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
            },
        };
        lifecycle.phase(Phase::Starting, None)?;
        Ok(lifecycle)
    }

    pub fn phase(&mut self, phase: Phase, reason: Option<Failure>) -> Result<()> {
        let mut next = self.bootstrap.clone();
        next.phase = phase;
        next.reason = reason;
        next.phase_seq = self
            .bootstrap
            .phase_seq
            .checked_add(1)
            .context("bootstrap sequence exhausted")?;
        write_private(&self.paths.bootstrap_file(), &serde_json::to_vec(&next)?)?;
        self.bootstrap = next;
        Ok(())
    }

    pub fn key_source(&mut self, source: KeySource) {
        self.bootstrap.keystore = Some(source.as_str());
    }

    pub fn ready(&self) -> bool {
        self.bootstrap.phase == Phase::Ready
    }

    /// The final persistent act after the agent has finished its shutdown and been dropped.
    pub fn stopped(&self, outcomes: Shutdown) -> Result<()> {
        if self.ready() {
            let receipt = ExitReceipt::new(self.instance.id, unix_ms()?, outcomes);
            write_private(&self.paths.exit_receipt(), &serde_json::to_vec(&receipt)?)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parking {
    Restored,
    NothingParked,
    Failed,
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct Shutdown {
    pub parking: Parking,
    pub input_journals_empty: bool,
    pub audio_stopped: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExitReceipt {
    schema_version: u32,
    instance_id: u64,
    stopped_unix_ms: u64,
    clean: bool,
    parking: Parking,
    input_journals_empty: bool,
    audio_stopped: bool,
}

impl ExitReceipt {
    fn new(instance_id: u64, stopped_unix_ms: u64, outcomes: Shutdown) -> Self {
        Self {
            schema_version: 1,
            instance_id,
            stopped_unix_ms,
            clean: outcomes.parking != Parking::Failed
                && outcomes.input_journals_empty
                && outcomes.audio_stopped,
            parking: outcomes.parking,
            input_journals_empty: outcomes.input_journals_empty,
            audio_stopped: outcomes.audio_stopped,
        }
    }

    fn clean(&self) -> bool {
        self.schema_version == 1
            && self.clean
            && self.parking != Parking::Failed
            && self.input_journals_empty
            && self.audio_stopped
    }
}

/// Re-open both actual journal files after releases settled; missing or unreadable is unknown.
pub fn journals_empty(paths: &Paths) -> bool {
    [paths.journal_file(), paths.e2_journal_file()]
        .iter()
        .all(|path| {
            let Ok(before) = crate::paths::metadata_private(path) else {
                return false;
            };
            if !before.is_file() {
                return false;
            }
            crate::paths::open_journal(path)
                .and_then(|journal| journal.held().map_err(Into::into))
                .is_ok_and(|held| {
                    // `open` repairs corrupt/torn tails. Such a read did not establish an intact empty
                    // journal, so conservatively keep the receipt unclean if the file changed length.
                    held.is_empty()
                        && crate::paths::metadata_private(path)
                            .is_ok_and(|after| after.len() == before.len())
                })
        })
}

pub fn instance_lock(paths: &Paths) -> std::io::Result<File> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(unix)]
    let file = options.open(paths.instance_lock())?;
    #[cfg(windows)]
    let file = crate::windows::security::open_private_lock(&paths.instance_lock())
        .map_err(std::io::Error::other)?;
    Ok(file)
}

fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    #[cfg(windows)]
    let _parents = crate::windows::security::pin_parent(path).map_err(std::io::Error::other)?;
    #[cfg(windows)]
    drop(crate::windows::security::private_file(path).map_err(std::io::Error::other)?);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Serialize)]
pub struct EraseReceipt {
    schema_version: u32,
    result: &'static str,
    reason: Option<&'static str>,
    key: &'static str,
    trust: &'static str,
}

impl EraseReceipt {
    fn refused(reason: &'static str) -> Self {
        Self {
            schema_version: 1,
            result: "refused",
            reason: Some(reason),
            key: "kept",
            trust: "kept",
        }
    }

    fn key_error(locked: bool) -> Self {
        Self {
            schema_version: 1,
            result: if locked { "waiting" } else { "failed" },
            reason: Some(if locked {
                "keystore_locked"
            } else {
                "keystore_error"
            }),
            key: if locked { "kept" } else { "failed" },
            trust: "kept",
        }
    }

    fn delete_error(locked: bool) -> Self {
        let mut receipt = Self::key_error(locked);
        // Once delete starts, neither its error nor a locked recheck proves retention.
        receipt.key = "failed";
        receipt
    }

    fn io() -> Self {
        Self {
            schema_version: 1,
            result: "failed",
            reason: Some("io"),
            key: "kept",
            trust: "kept",
        }
    }
}

/// Open the standalone key store only after both safety guards pass. No identity load/create.
pub fn erase_identity(
    paths: &Paths,
    keep_trust: bool,
    open_store: impl FnOnce() -> Result<Box<dyn KeyStore>>,
) -> EraseReceipt {
    let lock = match instance_lock(paths) {
        Ok(lock) => lock,
        Err(_) => return EraseReceipt::io(),
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return EraseReceipt::refused("agent_running"),
        Err(_) => return EraseReceipt::io(),
    }
    let _mutation = match crate::paths::identity_mutation_lock(&paths.state_dir) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(%error, "erase could not acquire the identity mutation lock");
            return EraseReceipt::io();
        }
    };
    let receipt = match crate::paths::read_private(&paths.exit_receipt()) {
        Ok(bytes) => serde_json::from_slice::<ExitReceipt>(&bytes).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EraseReceipt::refused("no_exit_receipt");
        }
        Err(_) => None,
    };
    if !receipt.is_some_and(|receipt| receipt.clean()) {
        return EraseReceipt::refused("unclean_exit");
    }
    let store = match open_store() {
        Ok(store) => store,
        Err(_) => return EraseReceipt::key_error(false),
    };
    let had_key = match store.load(crate::keys::KEY_NAME) {
        Ok(key) => key.is_some(),
        Err(PlatformError::InteractionRequired) => return EraseReceipt::key_error(true),
        Err(_) => {
            // A transient error does not prove a key is present. Re-check before failing.
            match store.load(crate::keys::KEY_NAME) {
                Ok(None) => false,
                Err(PlatformError::InteractionRequired) => return EraseReceipt::key_error(true),
                _ => return EraseReceipt::key_error(false),
            }
        }
    };
    if let Err(error) = store.delete(crate::keys::KEY_NAME) {
        if matches!(error, PlatformError::InteractionRequired) {
            return EraseReceipt::delete_error(true);
        }
        match store.load(crate::keys::KEY_NAME) {
            Ok(None) => {}
            Err(PlatformError::InteractionRequired) => return EraseReceipt::delete_error(true),
            _ => return EraseReceipt::delete_error(false),
        }
    }
    let file_removed = match remove_if_present(&paths.key_file()) {
        Ok(removed) => removed,
        Err(_) => {
            let mut receipt = EraseReceipt::io();
            receipt.key = "failed";
            return receipt;
        }
    };
    let key = if had_key || file_removed {
        "removed"
    } else {
        "absent"
    };
    let trust = if keep_trust {
        "kept"
    } else {
        let trust = remove_if_present(&paths.trust_file());
        let revocations = remove_if_present(&crate::revocations::file_beside(&paths.trust_file()));
        match (trust, revocations) {
            (Ok(a), Ok(b)) => {
                if a || b {
                    "removed"
                } else {
                    "absent"
                }
            }
            _ => "failed",
        }
    };
    EraseReceipt {
        schema_version: 1,
        result: if trust == "failed" {
            "failed"
        } else if key == "removed" || trust == "removed" {
            "removed"
        } else {
            "already_absent"
        },
        reason: (trust == "failed").then_some("io"),
        key,
        trust,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crosspane_input::Held;
    use crosspane_types::hid::HidUsage;
    use serde_json::{Value, json};
    use zeroize::Zeroizing;

    use super::*;

    struct Scratch(Paths);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crosspane-lifecycle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::paths::create_private_dir(&dir).unwrap();
            Self(Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir,
            })
        }

        fn lifecycle(&self) -> Lifecycle {
            Lifecycle::with_instance(
                &self.0,
                Instance {
                    id: 123,
                    pid: 4242,
                    started_unix_ms: 1_790_950_000_000,
                },
            )
            .unwrap()
        }

        fn clean_receipt(&self) {
            self.receipt(Shutdown {
                parking: Parking::None,
                input_journals_empty: true,
                audio_stopped: true,
            });
        }

        fn receipt(&self, outcomes: Shutdown) {
            write_private(
                &self.0.exit_receipt(),
                &serde_json::to_vec(&ExitReceipt::new(123, 456, outcomes)).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.state_dir);
        }
    }

    fn read(path: PathBuf) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn bootstrap_phases_advance_and_failed_carries_a_bounded_reason() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let mut lifecycle = scratch.lifecycle();
        assert_eq!(
            read(scratch.0.bootstrap_file()),
            json!({
                "schema_version": 1, "instance_id": 123, "pid": 4242,
                "started_unix_ms": 1_790_950_000_000u64, "phase": "starting", "phase_seq": 1,
                "keystore": null, "reason": null,
                "runtime_dir": scratch.0.runtime_dir.to_string_lossy(),
            })
        );
        lifecycle.phase(Phase::WaitingForKeystore, None).unwrap();
        let waiting = read(scratch.0.bootstrap_file());
        assert_eq!(
            (waiting["phase"].clone(), waiting["phase_seq"].clone()),
            (json!("waiting_for_keystore"), json!(2))
        );
        lifecycle.key_source(KeySource::OsStore);
        lifecycle.phase(Phase::Ready, None).unwrap();
        let ready = read(scratch.0.bootstrap_file());
        assert_eq!(ready["phase_seq"], json!(3));
        assert_eq!(ready["keystore"], json!("os_store"));
        assert!(lifecycle.ready());
        lifecycle
            .phase(Phase::Failed, Some(Failure::Socket))
            .unwrap();
        let failed = read(scratch.0.bootstrap_file());
        assert_eq!(failed["phase"], json!("failed"));
        assert_eq!(failed["phase_seq"], json!(4));
        assert_eq!(failed["reason"], json!("socket"));
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(scratch.0.bootstrap_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&scratch.0.runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn bootstrap_writes_never_expose_partial_json() {
        let scratch = Scratch::new();
        let mut lifecycle = scratch.lifecycle();
        let path = scratch.0.bootstrap_file();
        let done = Arc::new(AtomicBool::new(false));
        let reading_done = done.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            while !reading_done.load(Ordering::Acquire) || reads == 0 {
                let value = read(path.clone());
                assert_eq!(value.as_object().unwrap().len(), 9);
                assert_eq!(value["instance_id"], json!(123));
                assert!(value["phase_seq"].as_u64().unwrap() >= 1);
                reads += 1;
            }
            reads
        });
        for _ in 0..100 {
            lifecycle.phase(Phase::WaitingForKeystore, None).unwrap();
        }
        done.store(true, Ordering::Release);
        assert!(reader.join().unwrap() > 0);
        assert_eq!(
            std::fs::read_dir(&scratch.0.runtime_dir).unwrap().count(),
            1
        );
    }

    #[test]
    fn process_start_parser_matches_utc_ps_and_rejects_invalid_dates() {
        assert_eq!(parse_start("Thu Jan  1 00:00:00 1970\n"), Some(0));
        assert_eq!(
            parse_start("Fri Oct  2 00:00:00 2026\n"),
            Some(1_790_899_200_000)
        );
        assert_eq!(
            parse_start("Tue Feb 29 00:00:00 2000"),
            Some(951_782_400_000)
        );
        for bad in [
            "",
            "Fri Foo 2 00:00:00 2026",
            "Fri Oct 32 00:00:00 2026",
            "Fri Oct 2 25:00:00 2026",
            "Mon Feb 29 00:00:00 2100",
        ] {
            assert_eq!(parse_start(bad), None, "{bad}");
        }
    }

    #[test]
    fn every_exit_field_combination_derives_clean_and_round_trips() {
        for parking in [
            Parking::Restored,
            Parking::NothingParked,
            Parking::Failed,
            Parking::None,
        ] {
            for input_journals_empty in [false, true] {
                for audio_stopped in [false, true] {
                    let receipt = ExitReceipt::new(
                        123,
                        456,
                        Shutdown {
                            parking,
                            input_journals_empty,
                            audio_stopped,
                        },
                    );
                    let expected =
                        parking != Parking::Failed && input_journals_empty && audio_stopped;
                    assert_eq!(receipt.clean, expected);
                    assert_eq!(receipt.clean(), expected);
                    let parsed: ExitReceipt =
                        serde_json::from_slice(&serde_json::to_vec(&receipt).unwrap()).unwrap();
                    assert_eq!(parsed.clean(), expected);
                }
            }
        }
    }

    #[test]
    fn starting_removes_the_previous_receipt_and_exit_keeps_bootstrap() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        scratch.clean_receipt();
        let mut lifecycle = Lifecycle::start(&scratch.0).unwrap();
        assert!(!scratch.0.exit_receipt().exists());
        let outcomes = Shutdown {
            parking: Parking::NothingParked,
            input_journals_empty: true,
            audio_stopped: true,
        };
        lifecycle.stopped(outcomes).unwrap();
        assert!(
            !scratch.0.exit_receipt().exists(),
            "a startup that stopped before ready has no receipt"
        );
        lifecycle.phase(Phase::Ready, None).unwrap();
        lifecycle.stopped(outcomes).unwrap();
        let receipt = read(scratch.0.exit_receipt());
        assert_eq!(receipt["instance_id"], json!(lifecycle.instance.id));
        assert_eq!(receipt["clean"], json!(true));
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(scratch.0.exit_receipt())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(read(scratch.0.bootstrap_file())["phase"], json!("ready"));
    }

    #[test]
    fn journal_emptiness_reads_both_files_and_unknown_is_false() {
        let scratch = Scratch::new();
        assert!(!journals_empty(&scratch.0));
        let mut journal = crate::paths::open_journal(&scratch.0.journal_file()).unwrap();
        crate::paths::open_journal(&scratch.0.e2_journal_file()).unwrap();
        assert!(journals_empty(&scratch.0));
        let key = Held::Key(HidUsage::keyboard(4));
        journal.record_down(key).unwrap();
        assert!(!journals_empty(&scratch.0));
        journal.record_up(key).unwrap();
        assert!(journals_empty(&scratch.0));
        std::fs::remove_file(scratch.0.e2_journal_file()).unwrap();
        std::fs::create_dir(scratch.0.e2_journal_file()).unwrap();
        assert!(!journals_empty(&scratch.0));
    }

    #[derive(Clone, Default)]
    struct FakeStore(Arc<Mutex<StoreState>>);

    #[derive(Default)]
    struct StoreState {
        present: bool,
        locked: bool,
        delete_error: bool,
        deletes_then_errors: bool,
        locks_after_delete: bool,
        delete_locked: bool,
        deleted: usize,
    }

    impl KeyStore for FakeStore {
        fn load(
            &self,
            name: &str,
        ) -> std::result::Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
            assert_eq!(name, crate::keys::KEY_NAME);
            let state = self.0.lock().unwrap();
            if state.locked {
                Err(PlatformError::InteractionRequired)
            } else {
                Ok(state.present.then(|| Zeroizing::new(vec![1])))
            }
        }

        fn store(&self, _: &str, _: &[u8]) -> std::result::Result<(), PlatformError> {
            panic!("erase must never store or create a key")
        }

        fn delete(&self, name: &str) -> std::result::Result<(), PlatformError> {
            assert_eq!(name, crate::keys::KEY_NAME);
            let mut state = self.0.lock().unwrap();
            state.deleted += 1;
            if state.deletes_then_errors {
                state.present = false;
            }
            if state.locks_after_delete {
                state.locked = true;
            }
            if state.delete_locked {
                return Err(PlatformError::InteractionRequired);
            }
            if state.delete_error {
                return Err(PlatformError::Backend("fixture: delete failed".into()));
            }
            state.present = false;
            Ok(())
        }
    }

    fn erase(scratch: &Scratch, store: &FakeStore, keep_trust: bool) -> EraseReceipt {
        erase_identity(&scratch.0, keep_trust, || Ok(Box::new(store.clone())))
    }

    #[test]
    fn erase_refuses_held_lock_missing_unclean_and_unreadable_receipts_before_opening_store() {
        let scratch = Scratch::new();
        let guarded = || {
            erase_identity(&scratch.0, false, || {
                panic!("guard failed to prevent store access")
            })
        };
        let lock = instance_lock(&scratch.0).unwrap();
        lock.try_lock().unwrap();
        assert_eq!(guarded().reason, Some("agent_running"));
        drop(lock);
        assert_eq!(guarded().reason, Some("no_exit_receipt"));
        scratch.receipt(Shutdown {
            parking: Parking::Failed,
            input_journals_empty: true,
            audio_stopped: true,
        });
        assert_eq!(guarded().reason, Some("unclean_exit"));
        crate::paths::write_fixture(scratch.0.exit_receipt(), "invalid").unwrap();
        assert_eq!(guarded().reason, Some("unclean_exit"));
        let mut dishonest = serde_json::to_value(ExitReceipt::new(
            123,
            456,
            Shutdown {
                parking: Parking::None,
                input_journals_empty: false,
                audio_stopped: true,
            },
        ))
        .unwrap();
        dishonest["clean"] = json!(true);
        crate::paths::write_fixture(scratch.0.exit_receipt(), dishonest.to_string()).unwrap();
        assert_eq!(guarded().reason, Some("unclean_exit"));
        assert!(!scratch.0.bootstrap_file().exists());
    }

    #[test]
    fn erase_removes_store_key_file_pairings_and_revocations_but_keeps_recovery_files() {
        let scratch = Scratch::new();
        scratch.clean_receipt();
        let store = FakeStore::default();
        store.0.lock().unwrap().present = true;
        for path in [
            scratch.0.key_file(),
            scratch.0.trust_file(),
            crate::revocations::file_beside(&scratch.0.trust_file()),
            scratch.0.journal_file(),
            scratch.0.e2_journal_file(),
            scratch.0.state_dir.join("parking.journal"),
        ] {
            crate::paths::write_fixture(path, "fixture").unwrap();
        }
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            serde_json::to_value(receipt).unwrap(),
            json!({ "schema_version": 1, "result": "removed", "reason": null, "key": "removed", "trust": "removed" })
        );
        assert!(!scratch.0.key_file().exists());
        assert!(!scratch.0.trust_file().exists());
        assert!(!crate::revocations::file_beside(&scratch.0.trust_file()).exists());
        assert!(!store.0.lock().unwrap().present);
        for path in [
            scratch.0.journal_file(),
            scratch.0.e2_journal_file(),
            scratch.0.state_dir.join("parking.journal"),
        ] {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "fixture");
        }
        assert!(scratch.0.exit_receipt().exists());
        assert!(!scratch.0.bootstrap_file().exists());
    }

    #[test]
    fn erase_keep_trust_keeps_pairings_and_revocations_and_both_absent_is_already_absent() {
        let scratch = Scratch::new();
        scratch.clean_receipt();
        let store = FakeStore::default();
        assert_eq!(erase(&scratch, &store, false).result, "already_absent");
        crate::paths::write_fixture(scratch.0.trust_file(), "pairings").unwrap();
        let revocations = crate::revocations::file_beside(&scratch.0.trust_file());
        crate::paths::write_fixture(&revocations, "revocations").unwrap();
        // The lead amendment counts only selected items: kept trust does not prevent an
        // already-absent key from being an already_absent result.
        let receipt = erase(&scratch, &store, true);
        assert_eq!(
            (receipt.result, receipt.key, receipt.trust),
            ("already_absent", "absent", "kept")
        );
        store.0.lock().unwrap().present = true;
        let receipt = erase(&scratch, &store, true);
        assert_eq!(
            (receipt.result, receipt.key, receipt.trust),
            ("removed", "removed", "kept")
        );
        assert_eq!(
            std::fs::read_to_string(scratch.0.trust_file()).unwrap(),
            "pairings"
        );
        assert_eq!(std::fs::read_to_string(revocations).unwrap(), "revocations");
    }

    #[test]
    fn erase_locked_store_waits_and_delete_errors_recheck_presence() {
        let scratch = Scratch::new();
        scratch.clean_receipt();
        let store = FakeStore::default();
        store.0.lock().unwrap().locked = true;
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.reason, receipt.key),
            ("waiting", Some("keystore_locked"), "kept")
        );
        assert_eq!(store.0.lock().unwrap().deleted, 0);
        {
            let mut state = store.0.lock().unwrap();
            state.locked = false;
            state.present = true;
            state.delete_error = true;
        }
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.reason, receipt.key),
            ("failed", Some("keystore_error"), "failed")
        );
        store.0.lock().unwrap().deletes_then_errors = true;
        assert_eq!(erase(&scratch, &store, false).result, "removed");
        {
            let mut state = store.0.lock().unwrap();
            state.present = true;
            state.locks_after_delete = true;
        }
        // Delete actually removed the key, but returned an error and locked the recheck. Neither
        // that error nor the lock proves retention: waiting must now say key=failed.
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.reason, receipt.key, receipt.trust),
            ("waiting", Some("keystore_locked"), "failed", "kept")
        );
        assert!(!store.0.lock().unwrap().present);
        {
            let mut state = store.0.lock().unwrap();
            state.locked = false;
            state.present = true;
            state.delete_locked = true;
        }
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.reason, receipt.key),
            ("waiting", Some("keystore_locked"), "failed")
        );
    }

    #[test]
    fn erase_counts_revocations_alone_as_trust_and_reports_partial_io_failure() {
        let scratch = Scratch::new();
        scratch.clean_receipt();
        let store = FakeStore::default();
        crate::paths::write_fixture(
            crate::revocations::file_beside(&scratch.0.trust_file()),
            "fixture",
        )
        .unwrap();
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.key, receipt.trust),
            ("removed", "absent", "removed")
        );
        std::fs::create_dir(scratch.0.trust_file()).unwrap();
        let receipt = erase(&scratch, &store, false);
        assert_eq!(
            (receipt.result, receipt.reason, receipt.trust),
            ("failed", Some("io"), "failed")
        );
    }

    /// A fake OS store whose write remains pending after it returns Timeout. Only the child
    /// fixture's explicit completion command writes it; erase can only load and delete it.
    struct PendingStore {
        path: PathBuf,
        pending: Mutex<Option<Zeroizing<Vec<u8>>>>,
        writer: bool,
    }

    impl PendingStore {
        fn new(paths: &Paths, writer: bool) -> Self {
            Self {
                path: paths.state_dir.join("fixture-os-key.pk8"),
                pending: Mutex::new(None),
                writer,
            }
        }
    }

    impl KeyStore for PendingStore {
        fn load(
            &self,
            name: &str,
        ) -> std::result::Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
            assert_eq!(name, crate::keys::KEY_NAME);
            match std::fs::read(&self.path) {
                Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(PlatformError::Backend(error.to_string())),
            }
        }

        fn store(&self, name: &str, bytes: &[u8]) -> std::result::Result<(), PlatformError> {
            assert_eq!(name, crate::keys::KEY_NAME);
            assert!(self.writer, "erase must never create a key");
            *self.pending.lock().unwrap() = Some(Zeroizing::new(bytes.to_vec()));
            Err(PlatformError::Timeout)
        }

        fn delete(&self, name: &str) -> std::result::Result<(), PlatformError> {
            assert_eq!(name, crate::keys::KEY_NAME);
            remove_if_present(&self.path)
                .map(|_| ())
                .map_err(|error| PlatformError::Backend(error.to_string()))
        }
    }

    fn timeout_writer_fixture(paths: &Paths, allow_file: bool) {
        use std::io::{BufRead, Write};
        let store = PendingStore::new(paths, true);
        let result = crate::keys::load_or_create(Some(&store), &paths.key_file(), allow_file);
        if allow_file {
            assert!(result.is_ok());
            assert!(paths.key_file().exists());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(&PlatformError::Timeout.to_string())
            );
        }
        assert!(store.pending.lock().unwrap().is_some());
        assert!(!store.path.exists());
        println!("WP46_TIMEOUT_RETURNED");
        std::io::stdout().flush().unwrap();
        let stdin = std::io::stdin();
        let mut commands = stdin.lock().lines();
        // Closing the owned child pipe during a failed parent assertion also ends the fixture.
        let Some(command) = commands.next() else {
            return;
        };
        assert_eq!(command.unwrap(), "complete");
        let pending = store.pending.lock().unwrap().take().unwrap();
        write_private(&store.path, &pending).unwrap();
        println!("WP46_WRITE_COMPLETED");
        std::io::stdout().flush().unwrap();
        if let Some(command) = commands.next() {
            assert_eq!(command.unwrap(), "exit");
        }
    }

    /// The owned test process never calls agent main or any OS store/backend. Closing its input
    /// on an assertion failure makes it exit normally, so it cannot leave a paused child behind.
    struct WriterProcess(std::process::Child);

    impl Drop for WriterProcess {
        fn drop(&mut self) {
            drop(self.0.stdin.take());
            let _ = self.0.wait();
        }
    }

    #[test]
    fn timed_out_identity_write_retains_the_lock_until_the_writer_process_exits() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::time::Duration;
        const FIXTURE: &str = "CROSSPANE_WP46_TIMEOUT_FIXTURE";
        const FALLBACK: &str = "CROSSPANE_WP46_TIMEOUT_FALLBACK";
        if let Some(dir) = std::env::var_os(FIXTURE) {
            let dir = PathBuf::from(dir);
            timeout_writer_fixture(
                &Paths {
                    config_dir: dir.clone(),
                    state_dir: dir.clone(),
                    runtime_dir: dir,
                },
                std::env::var(FALLBACK).as_deref() == Ok("1"),
            );
            return;
        }
        for allow_file in [false, true] {
            let scratch = Scratch::new();
            scratch.clean_receipt();
            // Run this exact unit fixture through the required session-isolation wrapper. The
            // child simulates the writer's process lifetime, including descriptor teardown.
            #[cfg(unix)]
            let wrapper =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/test-env.sh");
            #[cfg(unix)]
            let mut command = {
                let mut command = Command::new(wrapper);
                command.arg(std::env::current_exe().unwrap());
                command
            };
            #[cfg(windows)]
            let mut command = Command::new(std::env::current_exe().unwrap());
            let mut writer = WriterProcess(
                command
                    .args([
                        "--exact",
                        "lifecycle::tests::timed_out_identity_write_retains_the_lock_until_the_writer_process_exits",
                        "--nocapture",
                    ])
                    .env(FIXTURE, &scratch.0.state_dir)
                    .env(FALLBACK, if allow_file { "1" } else { "0" })
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut output = BufReader::new(writer.0.stdout.take().unwrap());
            let marker = |output: &mut BufReader<std::process::ChildStdout>, expected: &str| {
                let mut line = String::new();
                loop {
                    line.clear();
                    assert_ne!(
                        output.read_line(&mut line).unwrap(),
                        0,
                        "child exited before {expected}"
                    );
                    if line.contains(expected) {
                        break;
                    }
                }
            };
            marker(&mut output, "WP46_TIMEOUT_RETURNED");
            assert!(writer.0.try_wait().unwrap().is_none());
            std::thread::scope(|scope| {
                let (waiting, wait_started) = mpsc::channel();
                let erased = scope.spawn(|| {
                    crate::paths::observe_mutation_lock_wait(waiting);
                    erase_identity(&scratch.0, false, || {
                        Ok(Box::new(PendingStore::new(&scratch.0, false)))
                    })
                });
                // The writer already returned Timeout (or its file fallback), but erase's real
                // flock must still report WouldBlock before it can inspect or mutate the store.
                wait_started.recv_timeout(Duration::from_secs(2)).unwrap();
                writer
                    .0
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"complete\n")
                    .unwrap();
                marker(&mut output, "WP46_WRITE_COMPLETED");
                assert!(scratch.0.state_dir.join("fixture-os-key.pk8").exists());
                assert!(writer.0.try_wait().unwrap().is_none());
                assert!(!erased.is_finished());
                writer
                    .0
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"exit\n")
                    .unwrap();
                assert!(writer.0.wait().unwrap().success());
                let receipt = erased.join().unwrap();
                assert_eq!(
                    (receipt.result, receipt.key, receipt.trust),
                    ("removed", "removed", "absent")
                );
            });
            assert!(!scratch.0.state_dir.join("fixture-os-key.pk8").exists());
            assert!(!scratch.0.key_file().exists());
        }
    }
}
