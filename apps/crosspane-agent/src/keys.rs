//! The device identity (04 §2): a P-256 key kept in the OS key store, created on first run.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crosspane_platform::{KeyStore, PlatformError};
use crosspane_security::identity::DeviceIdentity;

use crate::paths::write_private;

/// The key store entry name.
pub(crate) const KEY_NAME: &str = "device-key";

/// Where the device identity came from (`status.result.installer.keystore`, WP-4.5): decided here,
/// where the load picks between the OS key store and the key file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// Loaded from, or created in, the OS key store.
    OsStore,
    /// The 0600 key file supplied it: the OS key store was forced off, unavailable, or refused it.
    File,
}

impl KeySource {
    /// The spelling `status` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::OsStore => "os_store",
            KeySource::File => "file",
        }
    }
}

/// How often `run` retries the load while the OS key store is locked.
const UNLOCK_POLL: Duration = Duration::from_secs(2);
/// How often a log line says the wait goes on.
const UNLOCK_REPORT: Duration = Duration::from_secs(60);

/// Load the device identity, creating and storing a new one on first run. With
/// `allow_file_fallback`, an unavailable OS key store falls back to a 0600 file in the state
/// directory (development use; logged as a warning). A locked key store is an error here (or, with
/// the fallback, falls back too); `run` waits for it instead ([`load_or_create_waiting`]).
pub fn load_or_create(
    store: Option<&dyn KeyStore>,
    key_file: &Path,
    allow_file_fallback: bool,
) -> Result<DeviceIdentity> {
    match attempt(store, key_file, allow_file_fallback, false)? {
        Attempt::Loaded(identity, _) => Ok(identity),
        Attempt::Locked => bail!("the OS key store is locked; unlock it and restart Crosspane"),
    }
}

/// What [`load_or_create_waiting`] ended with.
#[derive(Debug)]
pub enum Startup {
    /// The identity, loaded (or created) once the key store allowed it, and where it came from.
    Identity(DeviceIdentity, KeySource),
    /// A stop request (SIGTERM or SIGINT) arrived while the key store was still locked.
    Stopped,
}

/// How the wait for a locked key store paces itself and notices a stop request. A trait so the
/// tests run without real time.
pub trait Pacer {
    /// Wait for `duration`, or less if the agent is asked to stop. True means "stop".
    fn pause(&mut self, duration: Duration) -> bool;
    /// A monotonic clock reading (any fixed origin).
    fn now(&self) -> Duration;
}

/// [`load_or_create`] for `run`: a locked OS key store (the login keyring is often still locked
/// when the agent starts at login) is waited out instead of ending the process. The load is retried
/// every [`UNLOCK_POLL`], without limit, until it works or `pacer` reports a stop request. Only
/// the key store unlocking ends the wait with an identity: a lock never causes the file fallback,
/// since the key belongs in the OS store. Every other outcome is as in [`load_or_create`].
pub fn load_or_create_waiting(
    store: Option<&dyn KeyStore>,
    key_file: &Path,
    allow_file_fallback: bool,
    pacer: &mut dyn Pacer,
    mut on_locked: impl FnMut() -> Result<()>,
) -> Result<Startup> {
    let mut waiting: Option<Waiting> = None;
    loop {
        match attempt(store, key_file, allow_file_fallback, true)? {
            Attempt::Loaded(identity, source) => {
                if let Some(waiting) = waiting {
                    tracing::info!(
                        waited_s = waiting.waited(pacer.now()).as_secs(),
                        "the OS key store unlocked"
                    );
                }
                return Ok(Startup::Identity(identity, source));
            }
            Attempt::Locked => {}
        }
        if waiting.is_none() {
            on_locked()?;
        }
        let waiting = waiting.get_or_insert_with(|| {
            tracing::warn!("the OS key store is locked; waiting for it to unlock");
            Waiting::start(pacer.now())
        });
        if pacer.pause(UNLOCK_POLL) {
            tracing::info!("stop requested while waiting for the OS key store to unlock");
            return Ok(Startup::Stopped);
        }
        if let Some(waited) = waiting.report_due(pacer.now()) {
            tracing::info!(
                waited_s = waited.as_secs(),
                "still waiting for the OS key store to unlock"
            );
        }
    }
}

/// The progress of one wait, for the once-a-minute reminder.
#[derive(Clone, Copy, Debug)]
struct Waiting {
    since: Duration,
    next_report: Duration,
}

impl Waiting {
    fn start(now: Duration) -> Waiting {
        Waiting {
            since: now,
            next_report: now + UNLOCK_REPORT,
        }
    }

    fn waited(&self, now: Duration) -> Duration {
        now.saturating_sub(self.since)
    }

    /// How long the wait has lasted, when a reminder is due at `now` (and the next one is set).
    fn report_due(&mut self, now: Duration) -> Option<Duration> {
        if now < self.next_report {
            return None;
        }
        self.next_report = now + UNLOCK_REPORT;
        Some(self.waited(now))
    }
}

/// One try of the load.
enum Attempt {
    Loaded(DeviceIdentity, KeySource),
    /// The OS key store is locked, and the caller asked to wait for it.
    Locked,
}

fn attempt(
    store: Option<&dyn KeyStore>,
    key_file: &Path,
    allow_file_fallback: bool,
    wait_on_lock: bool,
) -> Result<Attempt> {
    // Both one-shot identity and startup use this attempt. Do not retain the mutation lock
    // while startup paces a locked key store: only the actual load/create must exclude erase.
    let state_dir = key_file
        .parent()
        .context("device key has no state directory")?;
    let mutation = crate::paths::identity_mutation_lock(state_dir)?;
    if let Some(store) = store {
        match store.load(KEY_NAME) {
            Ok(Some(pkcs8)) => {
                return DeviceIdentity::from_pkcs8(&pkcs8)
                    .map(|identity| Attempt::Loaded(identity, KeySource::OsStore))
                    .context("stored device key is invalid");
            }
            Ok(None) => {
                let identity = DeviceIdentity::generate().context("generate device key")?;
                let stored = store.store(KEY_NAME, identity.pkcs8());
                if matches!(stored, Err(PlatformError::Timeout)) {
                    // Both adapters may still be writing after Timeout, and the frozen KeyStore
                    // cannot establish completion. Keep the descriptor (and flock) until this
                    // writer process ends, including when the file fallback succeeds below.
                    std::mem::forget(mutation);
                }
                match stored {
                    Ok(()) => {
                        tracing::info!(node = %identity.node().short(), "created device key in the OS key store");
                        return Ok(Attempt::Loaded(identity, KeySource::OsStore));
                    }
                    Err(e) if allow_file_fallback => {
                        tracing::warn!(error = %e, "OS key store refused the device key; using the file fallback");
                    }
                    Err(e) => bail!("store device key: {e}"),
                }
            }
            Err(PlatformError::InteractionRequired) if wait_on_lock => {
                return Ok(Attempt::Locked);
            }
            Err(PlatformError::InteractionRequired) if !allow_file_fallback => {
                bail!("the OS key store is locked; unlock it and restart Crosspane")
            }
            Err(e) if allow_file_fallback => {
                tracing::warn!(error = %e, "OS key store unavailable; using the file fallback");
            }
            Err(e) => bail!("load device key: {e}"),
        }
    } else if !allow_file_fallback {
        bail!("no OS key store on this platform and the file fallback is disabled");
    }
    load_or_create_file(key_file).map(|identity| Attempt::Loaded(identity, KeySource::File))
}

/// The production [`Pacer`]: it sleeps on SIGTERM and SIGINT, which end the sleep with "stop".
///
/// The signal handlers are installed on the first pause only, so a start that finds the key store
/// unlocked never touches signal handling. The wait ends when the pacer is dropped (it stops the
/// watcher thread; the process's later signal handling is the agent's own, as always).
#[derive(Debug)]
pub struct SignalPacer {
    start: Instant,
    watcher: Option<StopWatch>,
    tried: bool,
}

impl SignalPacer {
    pub fn new() -> SignalPacer {
        SignalPacer {
            start: Instant::now(),
            watcher: None,
            tried: false,
        }
    }
}

impl Pacer for SignalPacer {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn pause(&mut self, duration: Duration) -> bool {
        if !self.tried {
            self.tried = true;
            self.watcher = StopWatch::install();
        }
        let Some(watcher) = &self.watcher else {
            // No signal handling here: SIGTERM then ends the process by its default action, which
            // still stops the agent (the service managers accept that).
            std::thread::sleep(duration);
            return false;
        };
        match watcher.stop.recv_timeout(duration) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The watcher thread died: carry on without it.
                self.watcher = None;
                std::thread::sleep(duration);
                false
            }
        }
    }
}

/// A thread that listens for SIGTERM and SIGINT on its own small runtime, so that waiting for the
/// key store needs none of the agent's runtime (which doesn't exist yet at this point of startup).
#[derive(Debug)]
struct StopWatch {
    /// Receives one message when a stop signal arrives.
    stop: mpsc::Receiver<()>,
    /// Dropping this ends the thread.
    end: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StopWatch {
    /// Start listening. `None` (with a warning) when signal handlers can't be set up. Returns once
    /// the handlers are in place, so a signal sent after it is never missed.
    fn install() -> Option<StopWatch> {
        let (stop_tx, stop) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (end, end_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("keystore-wait-signals".into())
            .spawn(move || watch_signals(&stop_tx, &ready_tx, end_rx))
            .inspect_err(|error| tracing::warn!(%error, "no signal handling while waiting for the key store"))
            .ok()?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => Some(StopWatch {
                stop,
                end: Some(end),
                thread: Some(thread),
            }),
            // The thread logged why; it ends by itself once `end` is dropped here.
            Ok(false) | Err(_) => None,
        }
    }
}

impl Drop for StopWatch {
    fn drop(&mut self) {
        drop(self.end.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The watcher thread's body: tell `ready` whether the handlers are installed, then send one `()` on
/// `stop` for the first SIGTERM or SIGINT, or end when `end` is dropped.
#[cfg(unix)]
fn watch_signals(
    stop: &mpsc::Sender<()>,
    ready: &mpsc::Sender<bool>,
    end: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::signal::unix::{SignalKind, signal};
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(%error, "no signal handling while waiting for the key store");
            let _ = ready.send(false);
            return;
        }
    };
    runtime.block_on(async move {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("no signal handlers while waiting for the key store");
            let _ = ready.send(false);
            return;
        };
        let _ = ready.send(true);
        tokio::select! {
            _ = term.recv() => { let _ = stop.send(()); }
            _ = int.recv() => { let _ = stop.send(()); }
            _ = end => {}
        }
    });
}

#[cfg(windows)]
fn watch_signals(
    stop: &mpsc::Sender<()>,
    ready: &mpsc::Sender<bool>,
    end: tokio::sync::oneshot::Receiver<()>,
) {
    let watch = match crate::windows::shutdown::Watch::install(stop.clone()) {
        Ok(watch) => watch,
        Err(error) => {
            tracing::warn!(%error, "no shutdown handling while waiting for the key store");
            let _ = ready.send(false);
            return;
        }
    };
    let _ = ready.send(true);
    let _ = end.blocking_recv();
    drop(watch);
}

fn load_or_create_file(path: &Path) -> Result<DeviceIdentity> {
    match crate::paths::read_private(path) {
        Ok(bytes) => {
            let bytes = zeroize::Zeroizing::new(bytes);
            DeviceIdentity::from_pkcs8(&bytes)
                .with_context(|| format!("invalid key in {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity = DeviceIdentity::generate().context("generate device key")?;
            write_private(path, identity.pkcs8())?;
            tracing::info!(node = %identity.node().short(), path = %path.display(), "created device key file");
            Ok(identity)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        bail!("odd-length hex");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .context("invalid hex")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use zeroize::Zeroizing;

    use super::*;

    /// What a fake `load` answers.
    #[derive(Clone, Debug)]
    enum Reply {
        Locked,
        Missing,
        Key(Vec<u8>),
        Broken,
    }

    /// A key store that plays back a script of `load` answers, then repeats `then`, and records
    /// what is stored. It never touches a real key store.
    struct FakeStore {
        script: Mutex<VecDeque<Reply>>,
        then: Reply,
        loads: Mutex<usize>,
        stored: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl FakeStore {
        fn new(script: &[Reply], then: Reply) -> FakeStore {
            FakeStore {
                script: Mutex::new(script.iter().cloned().collect()),
                then,
                loads: Mutex::new(0),
                stored: Mutex::new(Vec::new()),
            }
        }

        fn loads(&self) -> usize {
            *self.loads.lock().unwrap()
        }

        fn stored(&self) -> Vec<(String, Vec<u8>)> {
            self.stored.lock().unwrap().clone()
        }
    }

    impl KeyStore for FakeStore {
        fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
            assert_eq!(name, KEY_NAME);
            *self.loads.lock().unwrap() += 1;
            let reply = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.then.clone());
            match reply {
                Reply::Locked => Err(PlatformError::InteractionRequired),
                Reply::Missing => Ok(None),
                Reply::Key(bytes) => Ok(Some(Zeroizing::new(bytes))),
                Reply::Broken => Err(PlatformError::Backend("no such service".into())),
            }
        }

        fn store(&self, name: &str, secret: &[u8]) -> Result<(), PlatformError> {
            self.stored
                .lock()
                .unwrap()
                .push((name.to_owned(), secret.to_vec()));
            Ok(())
        }

        fn delete(&self, _name: &str) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// A pacer with a virtual clock: pauses take no real time. It reports "stop" on its
    /// `stop_on_pause`-th pause (1-based), or never.
    struct FakePacer {
        now: Duration,
        pauses: Vec<Duration>,
        stop_on_pause: Option<usize>,
    }

    impl FakePacer {
        fn new(stop_on_pause: Option<usize>) -> FakePacer {
            FakePacer {
                now: Duration::ZERO,
                pauses: Vec::new(),
                stop_on_pause,
            }
        }
    }

    impl Pacer for FakePacer {
        fn pause(&mut self, duration: Duration) -> bool {
            self.pauses.push(duration);
            assert!(self.pauses.len() < 10_000, "the wait never ended");
            if self.stop_on_pause == Some(self.pauses.len()) {
                return true;
            }
            self.now += duration;
            false
        }

        fn now(&self) -> Duration {
            self.now
        }
    }

    /// A fresh key file path in a clean directory of its own (the file is never created here).
    fn key_file(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crosspane-keys-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        #[cfg(unix)]
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        crate::paths::create_private_dir(&dir).unwrap();
        dir.join("device-key.pk8")
    }

    fn cleanup(key_file: &Path) {
        if let Some(dir) = key_file.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn pkcs8_of(identity: &DeviceIdentity) -> Vec<u8> {
        identity.pkcs8().to_vec()
    }

    #[test]
    fn locked_three_times_then_the_key_loads() {
        let existing = DeviceIdentity::generate().unwrap();
        let store = FakeStore::new(
            &[Reply::Locked, Reply::Locked, Reply::Locked],
            Reply::Key(pkcs8_of(&existing)),
        );
        let file = key_file("three-locks");
        let mut pacer = FakePacer::new(None);
        let mut notices = 0;
        let Startup::Identity(identity, source) =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || {
                assert_eq!(store.loads(), 1);
                // The load attempt has ended before waiting begins. Other short one-shot
                // mutations may proceed while the OS key store stays locked.
                let _mutation =
                    crate::paths::identity_mutation_lock(file.parent().unwrap()).unwrap();
                notices += 1;
                Ok(())
            })
            .unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(notices, 1);
        assert_eq!(identity.node(), existing.node());
        assert_eq!(source, KeySource::OsStore);
        // Three locked loads, three pauses of 2 s each, then the fourth load worked.
        assert_eq!(store.loads(), 4);
        assert_eq!(pacer.pauses, vec![UNLOCK_POLL; 3]);
        assert_eq!(pacer.now(), Duration::from_secs(6));
        // Nothing was created: the key was only read.
        assert!(store.stored().is_empty());
        assert!(!file.exists());
        cleanup(&file);
    }

    #[test]
    fn locked_forever_returns_a_stop_result_promptly() {
        let store = FakeStore::new(&[], Reply::Locked);
        let file = key_file("locked-forever");
        let mut pacer = FakePacer::new(Some(5));
        let startup =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || Ok(())).unwrap();
        assert!(matches!(startup, Startup::Stopped), "{startup:?}");
        // It stopped at the stop request: no further load, no further pause.
        assert_eq!(pacer.pauses.len(), 5);
        assert_eq!(store.loads(), 5);
        assert!(store.stored().is_empty());
        cleanup(&file);
    }

    #[test]
    fn a_stop_request_during_the_first_pause_ends_the_wait_at_once() {
        let store = FakeStore::new(&[], Reply::Locked);
        let file = key_file("stop-at-once");
        let mut pacer = FakePacer::new(Some(1));
        let startup =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || Ok(())).unwrap();
        assert!(matches!(startup, Startup::Stopped), "{startup:?}");
        assert_eq!(store.loads(), 1);
        assert_eq!(pacer.pauses, vec![UNLOCK_POLL]);
        cleanup(&file);
    }

    #[test]
    fn a_lock_never_causes_the_file_fallback() {
        // Even where the file fallback is allowed (the Mac default), waiting is the only way out.
        let store = FakeStore::new(&[], Reply::Locked);
        let file = key_file("no-fallback");
        let mut pacer = FakePacer::new(Some(40));
        let startup =
            load_or_create_waiting(Some(&store), &file, true, &mut pacer, || Ok(())).unwrap();
        assert!(matches!(startup, Startup::Stopped), "{startup:?}");
        assert_eq!(store.loads(), 40);
        assert!(!file.exists(), "a lock must not create or use the key file");
        cleanup(&file);
    }

    #[test]
    fn unlocking_after_a_long_wait_still_loads_the_os_key() {
        let existing = DeviceIdentity::generate().unwrap();
        let locked = vec![Reply::Locked; 200];
        let store = FakeStore::new(&locked, Reply::Key(pkcs8_of(&existing)));
        let file = key_file("long-wait");
        let mut pacer = FakePacer::new(None);
        let Startup::Identity(identity, source) =
            load_or_create_waiting(Some(&store), &file, true, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(identity.node(), existing.node());
        // A lock was waited out, never a reason to fall back: the key store supplied it.
        assert_eq!(source, KeySource::OsStore);
        assert_eq!(pacer.now(), Duration::from_secs(400));
        assert!(!file.exists());
        cleanup(&file);
    }

    #[test]
    fn another_error_is_returned_as_before() {
        let store = FakeStore::new(&[], Reply::Broken);
        let file = key_file("other-error");
        let mut pacer = FakePacer::new(None);
        let error =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "load device key: no such service");
        // The same text as `load_or_create` gives.
        let direct = load_or_create(Some(&store), &file, false).unwrap_err();
        assert_eq!(direct.to_string(), error.to_string());
        // No waiting happened.
        assert!(pacer.pauses.is_empty());
        assert_eq!(store.loads(), 2);
        cleanup(&file);
    }

    #[test]
    fn another_error_still_takes_the_file_fallback_where_allowed() {
        let store = FakeStore::new(&[], Reply::Broken);
        let file = key_file("other-error-fallback");
        let mut pacer = FakePacer::new(None);
        let Startup::Identity(identity, source) =
            load_or_create_waiting(Some(&store), &file, true, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(source, KeySource::File);
        assert!(pacer.pauses.is_empty());
        assert!(file.exists());
        assert_eq!(std::fs::read(&file).unwrap(), pkcs8_of(&identity));
        cleanup(&file);
    }

    #[test]
    fn a_missing_key_is_created_in_the_store() {
        let store = FakeStore::new(&[], Reply::Missing);
        let file = key_file("first-run");
        let mut pacer = FakePacer::new(None);
        let Startup::Identity(identity, source) =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(source, KeySource::OsStore);
        assert!(pacer.pauses.is_empty());
        assert_eq!(store.loads(), 1);
        assert_eq!(
            store.stored(),
            vec![(KEY_NAME.to_owned(), pkcs8_of(&identity))]
        );
        assert!(!file.exists());
        cleanup(&file);
    }

    #[test]
    fn a_first_run_after_a_lock_creates_the_key_once_unlocked() {
        let store = FakeStore::new(&[Reply::Locked, Reply::Locked], Reply::Missing);
        let file = key_file("first-run-after-lock");
        let mut pacer = FakePacer::new(None);
        let Startup::Identity(identity, source) =
            load_or_create_waiting(Some(&store), &file, false, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(source, KeySource::OsStore);
        assert_eq!(pacer.pauses.len(), 2);
        assert_eq!(
            store.stored(),
            vec![(KEY_NAME.to_owned(), pkcs8_of(&identity))]
        );
        cleanup(&file);
    }

    #[test]
    fn without_an_os_store_the_file_supplies_the_identity_and_says_so() {
        let file = key_file("no-store-file");
        let mut pacer = FakePacer::new(None);
        let Startup::Identity(first, source) =
            load_or_create_waiting(None, &file, true, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(source, KeySource::File);
        // The same file on the next start.
        let Startup::Identity(second, source) =
            load_or_create_waiting(None, &file, true, &mut pacer, || Ok(())).unwrap()
        else {
            panic!("expected the identity");
        };
        assert_eq!(source, KeySource::File);
        assert_eq!(first.node(), second.node());
        cleanup(&file);
    }

    #[test]
    fn the_source_has_the_spellings_status_uses() {
        assert_eq!(KeySource::OsStore.as_str(), "os_store");
        assert_eq!(KeySource::File.as_str(), "file");
    }

    #[test]
    fn no_store_without_the_fallback_is_an_error_not_a_wait() {
        let file = key_file("no-store");
        let mut pacer = FakePacer::new(None);
        assert!(load_or_create_waiting(None, &file, false, &mut pacer, || Ok(())).is_err());
        assert!(pacer.pauses.is_empty());
        cleanup(&file);
    }

    #[test]
    fn the_one_shot_load_still_fails_on_a_lock() {
        // `crosspane-agent identity` keeps its behaviour: no waiting.
        let store = FakeStore::new(&[], Reply::Locked);
        let file = key_file("one-shot");
        let error = load_or_create(Some(&store), &file, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the OS key store is locked; unlock it and restart Crosspane"
        );
        assert_eq!(store.loads(), 1);
        // With the fallback allowed it falls back to the file, as before.
        let identity = load_or_create(Some(&store), &file, true).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), pkcs8_of(&identity));
        cleanup(&file);
    }

    #[test]
    fn the_reminder_comes_once_a_minute() {
        let mut waiting = Waiting::start(Duration::from_secs(10));
        let mut reports = Vec::new();
        let mut now = Duration::from_secs(10);
        while now < Duration::from_secs(10 + 190) {
            now += UNLOCK_POLL;
            if let Some(waited) = waiting.report_due(now) {
                reports.push(waited.as_secs());
            }
        }
        assert_eq!(reports, vec![60, 120, 180]);
    }

    /// One test for both halves, because a signal is process-wide: under plain `cargo test` (one
    /// process, many threads) a second test using the signal pacer would see this one's SIGTERM.
    #[cfg(unix)]
    #[test]
    fn the_signal_pacer_sleeps_until_a_stop_signal_arrives() {
        let mut pacer = SignalPacer::new();
        let before = Instant::now();
        assert!(!pacer.pause(Duration::from_millis(50)));
        assert!(before.elapsed() >= Duration::from_millis(50));
        assert!(pacer.now() >= Duration::from_millis(50));

        // This process handles SIGTERM once the pacer's first pause has installed the watcher
        // (`install` returns only after the handlers are in place), so signalling it is safe.
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let before = Instant::now();
        assert!(pacer.pause(Duration::from_secs(30)));
        assert!(before.elapsed() < Duration::from_secs(20));
    }
}
