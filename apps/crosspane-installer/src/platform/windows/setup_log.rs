//! Setup log: one plain-text line per installer event, in `%TEMP%\Crosspane\setup.log`.
//!
//! The GUI-subsystem installer has no console, so every native failure (with its Windows code and
//! source location) and every finished job leaves a line here. Callers pass paths, codes, step
//! names and their own refusal text only. Nothing in this module reads or records file contents,
//! record bytes, key material, tokens or SIDs. Writing is best effort: failures are ignored,
//! nothing is retried and nothing panics. The folder sits outside every installer-admitted folder.

use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

/// The folder under the user's temp directory, and the log file inside it.
const FOLDER: &str = "Crosspane";
const FILE: &str = "setup.log";
/// At the first open in a process, a log larger than this is renamed to `setup.log.1`.
const CAP_BYTES: u64 = 1024 * 1024;

thread_local! {
    /// Set while this thread is inside `note`, so a log call made during formatting is dropped.
    static WRITING: Cell<bool> = const { Cell::new(false) };
}

/// The process-wide sink, created by the first note and kept for the life of the process.
static SINK: Mutex<Option<SetupLog>> = Mutex::new(None);

/// One log file. Each write appends one `<unix-ms> [<thread>] <text>` line.
pub(crate) struct SetupLog {
    path: PathBuf,
    /// Set by the first write, whether or not the open succeeded, so a failed open is never retried.
    attempted: bool,
    file: Option<File>,
}

/// Appends one line to the process's setup log. Never panics, returns nothing and never retries.
pub(crate) fn note(args: std::fmt::Arguments<'_>) {
    let Some(_entry) = Entry::enter() else {
        return;
    };
    let text = args.to_string();
    let mut sink = SINK.lock().unwrap_or_else(PoisonError::into_inner);
    sink.get_or_insert_with(|| SetupLog::new(default_path()))
        .write_text(&text);
}

/// Appends a formatted line to the setup log, for example
/// `setup_note!("step={step} stage={stage:?}")`.
#[cfg_attr(test, allow(unused_macros))]
macro_rules! setup_note {
    ($($arg:tt)*) => {
        $crate::platform::windows::setup_log::note(format_args!($($arg)*))
    };
}
#[cfg_attr(test, allow(unused_imports))]
pub(crate) use setup_note;

/// The default location: `%TEMP%\Crosspane\setup.log`.
fn default_path() -> PathBuf {
    std::env::temp_dir().join(FOLDER).join(FILE)
}

impl SetupLog {
    /// A sink for `path`. Nothing is opened until the first write.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            attempted: false,
            file: None,
        }
    }

    /// Appends `text` as one line, stamped with the time and this thread's name. Line breaks in
    /// `text` become spaces, so every event stays on one line.
    pub(crate) fn write_text(&mut self, text: &str) {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let line = format!(
            "{} [{name}] {}\n",
            unix_millis(),
            text.replace(['\r', '\n'], " ")
        );
        if !self.attempted {
            self.attempted = true;
            self.file = self.open();
        }
        if let Some(file) = self.file.as_mut() {
            // Best effort: a failed write loses this line and nothing else.
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Creates the folder, rotates an oversized log, and opens the log for appending.
    fn open(&self) -> Option<File> {
        fs::create_dir_all(self.path.parent()?).ok()?;
        rotate_if_over_cap(&self.path);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok()
    }
}

/// Renames a log larger than the cap to `<name>.1`, replacing any older rotation.
fn rotate_if_over_cap(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() <= CAP_BYTES {
        return;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    // A rename that fails (for example, another process holds the file) leaves the log in place.
    let _ = fs::rename(path, rotated);
}

/// Milliseconds since the Unix epoch; zero if the clock is before it.
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

/// Holds the thread's reentrancy flag for one `note` call and releases it on drop.
struct Entry;

impl Entry {
    /// `None` when this thread is already writing, or when thread-local storage is gone.
    fn enter() -> Option<Self> {
        WRITING
            .try_with(|writing| {
                if writing.get() {
                    None
                } else {
                    writing.set(true);
                    Some(Self)
                }
            })
            .ok()
            .flatten()
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        let _ = WRITING.try_with(|writing| writing.set(false));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{CAP_BYTES, SetupLog};
    use std::{fs, fs::File, path::PathBuf};

    /// A fresh folder under the temp directory. Never `%TEMP%\Crosspane`, which the real log uses.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crosspane-setup-log-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn setup_log_rotates_over_cap_on_first_open() {
        let dir = scratch("rotate");
        let path = dir.join("setup.log");
        let oversized = File::create(&path).unwrap();
        oversized.set_len(CAP_BYTES + 1).unwrap();
        drop(oversized);

        let mut log = SetupLog::new(path.clone());
        log.write_text("after rotation");
        drop(log);

        assert_eq!(
            fs::metadata(dir.join("setup.log.1")).unwrap().len(),
            CAP_BYTES + 1
        );
        let current = fs::read_to_string(&path).unwrap();
        assert_eq!(current.lines().count(), 1);
        assert!(current.ends_with("] after rotation\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_log_appends_lines_with_thread() {
        let dir = scratch("append");
        let path = dir.join("setup.log");
        for (name, text) in [
            ("setup-log-first", "first line"),
            ("setup-log-second", "second line"),
        ] {
            let writer = path.clone();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    let mut log = SetupLog::new(writer);
                    log.write_text(text);
                })
                .unwrap()
                .join()
                .unwrap();
        }

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let expected = [
            "[setup-log-first] first line",
            "[setup-log-second] second line",
        ];
        for (line, want) in lines.iter().zip(expected) {
            let (stamp, rest) = line.split_once(' ').unwrap();
            assert!(stamp.parse::<u128>().is_ok_and(|ms| ms > 0));
            assert_eq!(rest, want);
        }
        assert!(content.ends_with('\n'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_log_never_panics_when_dir_cannot_be_created() {
        let dir = scratch("blocked");
        let blocker = dir.join("not-a-folder");
        fs::write(&blocker, b"a file where a folder is needed").unwrap();
        let path = blocker.join("Crosspane").join("setup.log");

        let mut log = SetupLog::new(path.clone());
        log.write_text("first");
        log.write_text("second");
        assert!(log.file.is_none());
        assert!(log.attempted);
        assert!(!path.exists());
        drop(log);
        let _ = fs::remove_dir_all(&dir);
    }
}
