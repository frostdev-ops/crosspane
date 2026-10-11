//! The agent's log file (G5): a private, rotated file under `state_dir\logs`, written without
//! buffering so that a process `exit()` right after a log call keeps the line.

use std::cell::Cell;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::windows::security::{create_private_directory, create_private_file, pin_parent};

/// The live log and its two earlier runs, newest first.
pub(crate) const LOG_NAMES: [&str; 3] = ["agent.log", "agent.log.1", "agent.log.2"];
/// The live log's size limit (4 MiB). A write that would pass it rotates the file first.
pub(crate) const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The agent's log file. Lines are discarded until [`LogFile::open`] succeeds, which
/// `run_with_startup` calls only after the instance lock is held and the elevation check passed.
#[derive(Debug)]
pub(crate) struct LogFile {
    limit: u64,
    state: Mutex<Option<State>>,
}

/// The process-wide agent log file.
pub(crate) static AGENT_LOG: LogFile = LogFile::new(MAX_BYTES);

#[derive(Debug)]
struct State {
    /// The live file. `None` after a failed rotation or recreate, until a later write recreates it.
    file: Option<std::fs::File>,
    /// Bytes written to the live file since it was created.
    len: u64,
    dir: PathBuf,
}

thread_local! {
    /// Set while this thread is inside a write, so a log call made from within it is discarded.
    static WRITING: Cell<bool> = const { Cell::new(false) };
}

/// Holds the reentrancy flag for one write and clears it when the write ends, even by unwinding.
struct Entered;

impl Entered {
    fn enter() -> Option<Self> {
        WRITING
            .try_with(|flag| if flag.replace(true) { None } else { Some(Self) })
            .ok()
            .flatten()
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        let _ = WRITING.try_with(|flag| flag.set(false));
    }
}

impl LogFile {
    /// A log that discards every line until [`LogFile::open`].
    pub(crate) const fn new(limit: u64) -> Self {
        Self {
            limit,
            state: Mutex::new(None),
        }
    }

    /// Create `state_dir\logs` privately, move the earlier runs back (`agent.log` becomes `.1` and
    /// `.1` becomes `.2`), and start an empty `agent.log`. A rename that fails is tolerated. An
    /// error is returned when the directory cannot be created or pinned, or the live file cannot be
    /// created; the caller then logs to stderr only.
    pub(crate) fn open(&self, state_dir: &Path) -> Result<()> {
        let dir = state_dir.join("logs");
        create_private_directory(&dir).with_context(|| format!("create {}", dir.display()))?;
        let [current, previous, oldest] = LOG_NAMES.map(|name| dir.join(name));
        // Pinned across the renames and the create, so the directory cannot be swapped under them.
        let _parents = pin_parent(&current)?;
        let _ = shift(&previous, &oldest);
        let _ = shift(&current, &previous);
        // A rename refused while a reader holds the old live file leaves it: remove it once.
        let _ = std::fs::remove_file(&current);
        let file =
            create_live(&current).with_context(|| format!("create {}", current.display()))?;
        *self.lock() = Some(State {
            file: Some(file),
            len: 0,
            dir,
        });
        Ok(())
    }

    /// The writer for one event. The tracing layer writes each event with one `write_all`.
    pub(crate) fn writer(&self) -> LogWriter<'_> {
        LogWriter { file: self }
    }

    fn lock(&self) -> MutexGuard<'_, Option<State>> {
        // A panic while the lock was held leaves the state usable; logging must never stop.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append one event with a single `write_all`, rotating first when it would pass the limit.
    /// Rotation needs a non-empty live file, so one oversized event never rotates an empty file
    /// and pushes the older runs out. A failed rotation leaves the live file absent: this write
    /// tries to recreate it, and the line is dropped if that fails too. The next write tries again.
    fn write_line(&self, buf: &[u8]) {
        // A log call from inside a write (from a panic hook, say) would deadlock on the lock below.
        let Some(_entered) = Entered::enter() else {
            return;
        };
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let size = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if state.file.is_some() && state.len > 0 && state.len.saturating_add(size) > self.limit {
            state.rotate();
        }
        if state.file.is_none() {
            state.recreate();
        }
        if let Some(file) = state.file.as_mut() {
            // A failed write is dropped: a full disk must not stop the agent.
            if file.write_all(buf).is_ok() {
                state.len = state.len.saturating_add(size);
            }
        }
    }
}

impl State {
    /// Move the live file to `.1` (after shifting `.1` to `.2`) and start an empty live file. A
    /// refused rename, such as a reader without delete sharing, truncates the open handle instead.
    /// A failed step leaves the live file absent (`file` is `None`); the logging state stays, so
    /// `recreate` can start a new live file on a later write.
    fn rotate(&mut self) {
        self.len = 0;
        let Some(mut live) = self.file.take() else {
            return;
        };
        let [current, previous, oldest] = LOG_NAMES.map(|name| self.dir.join(name));
        // Pinned across the renames and the create, as in `LogFile::open`.
        let Ok(_parents) = pin_parent(&current) else {
            return;
        };
        let _ = shift(&previous, &oldest);
        let moved = match std::fs::rename(&current, &previous) {
            Ok(()) => true,
            // The live name is already free, so a new live file can take it.
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if moved {
            // The handle now names the previous run's file, so close it before the new one opens.
            drop(live);
            self.file = create_live(&current).ok();
        } else {
            let truncated = live.set_len(0).and_then(|()| live.seek(SeekFrom::Start(0)));
            if truncated.is_ok() {
                self.file = Some(live);
            }
        }
    }

    /// Start an empty live file while the live file is absent, after a failed rotation or an
    /// earlier failed recreate. A refused rename can leave the old live file in place, so it is
    /// removed first, as `LogFile::open` does. A failure leaves the live file absent.
    fn recreate(&mut self) {
        self.len = 0;
        let current = self.dir.join(LOG_NAMES[0]);
        // Pinned across the remove and the create, as in `LogFile::open`.
        let Ok(_parents) = pin_parent(&current) else {
            return;
        };
        let _ = std::fs::remove_file(&current);
        self.file = create_live(&current).ok();
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many live-file creations on this thread fail next. Thread-local, so tests
    /// running in parallel never see each other's failures.
    static FAIL_CREATES: Cell<u32> = const { Cell::new(0) };
}

/// Test-only: fail the next live-file creation on this thread when one is armed.
#[cfg(test)]
fn injected_create_failure() -> Result<()> {
    let fail = FAIL_CREATES.with(|left| {
        let fail = left.get() > 0;
        left.set(left.get().saturating_sub(1));
        fail
    });
    if fail {
        anyhow::bail!("injected live-file create failure");
    }
    Ok(())
}

/// Create the live file privately, the same way for `open`, rotation and recreate.
fn create_live(path: &Path) -> Result<std::fs::File> {
    #[cfg(test)]
    injected_create_failure()?;
    create_private_file(path)
}

/// Rename `from` to `to`. A missing `from` is not an error.
fn shift(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// The writer for one event. Errors are swallowed, so logging never fails its caller.
#[derive(Debug)]
pub(crate) struct LogWriter<'a> {
    file: &'a LogFile,
}

impl Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_line(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What the file records: INFO from dependencies and everything from the `crosspane*` crates.
/// `init_tracing` uses this same value, so the tests check the real filter.
fn file_filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("crosspane", LevelFilter::TRACE)
}

/// Install the tracing subscriber: stderr as before, plus the agent log file, which stays silent
/// until `AGENT_LOG` is opened. Also replaces the panic hook.
pub(crate) fn init_tracing() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(|| AGENT_LOG.writer())
                .with_filter(file_filter()),
        )
        .init();
    install_panic_hook();
}

/// Record where a thread panicked (file and line) and which thread it was. The payload is never
/// logged; the previous hook still runs, so the default message still reaches stderr.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &std::panic::PanicHookInfo<'_>| {
        let location = info.location().map_or_else(
            || "unknown".to_owned(),
            |location| format!("{}:{}", location.file(), location.line()),
        );
        let current = std::thread::current();
        let thread = current.name().unwrap_or("unnamed");
        tracing::error!(
            location = %location,
            thread = thread,
            "a thread panicked (message kept out of the log file)"
        );
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use tracing::Level;

    use super::*;

    /// A private scratch directory under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crosspane-agent-log-{}-{stamp}",
                std::process::id()
            ));
            create_private_directory(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join("logs").join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn agent_log_rotates_at_limit_and_keeps_three() {
        let root = Scratch::new();
        let log = LogFile::new(1024);
        log.open(&root.0).unwrap();
        for line in 0..200 {
            let text = format!("line {line:04} {}\n", "x".repeat(60));
            log.writer().write_all(text.as_bytes()).unwrap();
        }
        for name in LOG_NAMES {
            let len = std::fs::metadata(root.file(name)).unwrap().len();
            assert!(len <= 1024, "{name} holds {len} bytes");
        }
        assert!(!root.file("agent.log.3").exists());
        let last = format!("line 0199 {}\n", "x".repeat(60));
        assert!(
            std::fs::read_to_string(root.file("agent.log"))
                .unwrap()
                .ends_with(&last)
        );
    }

    #[test]
    fn agent_log_open_shifts_previous_runs() {
        let root = Scratch::new();
        for run in ["first", "second", "third"] {
            let log = LogFile::new(MAX_BYTES);
            log.open(&root.0).unwrap();
            log.writer()
                .write_all(format!("{run}\n").as_bytes())
                .unwrap();
        }
        let read = |name: &str| std::fs::read_to_string(root.file(name)).unwrap();
        assert_eq!(read("agent.log"), "third\n");
        assert_eq!(read("agent.log.1"), "second\n");
        assert_eq!(read("agent.log.2"), "first\n");
    }

    #[test]
    fn agent_log_discards_before_open() {
        let root = Scratch::new();
        let log = LogFile::new(MAX_BYTES);
        let early = b"before open\n";
        assert_eq!(log.writer().write(early).unwrap(), early.len());
        log.open(&root.0).unwrap();
        log.writer().write_all(b"after open\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.file("agent.log")).unwrap(),
            "after open\n"
        );
    }

    #[test]
    fn agent_log_file_filter_caps_dependencies() {
        let filter = file_filter();
        assert!(filter.would_enable("crosspane_agent::agent", &Level::DEBUG));
        assert!(!filter.would_enable("winit::platform", &Level::DEBUG));
    }

    #[test]
    fn agent_log_recovers_after_failed_rotation() {
        let root = Scratch::new();
        let log = LogFile::new(1024);
        log.open(&root.0).unwrap();
        let line = |tag: &str| format!("{tag} {}\n", "x".repeat(600));
        let read = |name: &str| std::fs::read_to_string(root.file(name)).unwrap();

        log.writer().write_all(line("first").as_bytes()).unwrap();
        // The rotation's create fails once. The retry in the same write succeeds, so the line
        // that triggered the rotation lands in the new live file.
        FAIL_CREATES.with(|left| left.set(1));
        log.writer().write_all(line("second").as_bytes()).unwrap();
        assert_eq!(
            FAIL_CREATES.with(Cell::get),
            0,
            "the rotation create was attempted"
        );
        assert_eq!(read("agent.log"), line("second"));
        assert_eq!(read("agent.log.1"), line("first"));

        // The rotation's create and the retry both fail. That line is dropped, and the next
        // write recreates the live file.
        FAIL_CREATES.with(|left| left.set(2));
        log.writer().write_all(line("third").as_bytes()).unwrap();
        assert_eq!(FAIL_CREATES.with(Cell::get), 0);
        log.writer().write_all(line("fourth").as_bytes()).unwrap();
        assert_eq!(read("agent.log"), line("fourth"));
        assert_eq!(read("agent.log.1"), line("second"));
        assert_eq!(read("agent.log.2"), line("first"));
    }
}
