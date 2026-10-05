//! WP-4.32: setup owns its install paths.
//!
//! Whatever stands where Crosspane installs (another build's app, a hand-made copy, a link, the
//! leftovers and records of an interrupted or older run) is moved into a timestamped folder in
//! ~/Library/Application Support/Crosspane/Backups, after Crosspane's own sign-in item is
//! stopped, and the install continues as a fresh one. Nothing outside the fixed install paths,
//! their installer leftovers and setup's own install records is touched. The user's identity,
//! pairings and settings (Application Support/Crosspane outside Installer/) stay where they are.
use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// At most this many backup folders are kept; the oldest go first.
pub const KEEP_BACKUPS: usize = 3;
/// How long the stopped agent gets to leave after its sign-in item was booted out.
const STOP_WAIT_MS: u64 = 15_000;

/// UTC `YYYYMMDD-HHMMSS` for `seconds` since the epoch.
pub(crate) fn backup_stamp(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Setup's own install records in the Installer folder (never the lock, the audio driver's
/// records, or repair and removal records).
fn is_install_record(name: &str) -> bool {
    let name = name
        .strip_prefix('.')
        .and_then(|rest| rest.split_once(".crosspane-temp-"))
        .map_or(name, |(base, _)| base);
    matches!(name, "payload.json" | "launch-agent.json")
        || (name.starts_with("launch-agent-prior-") && name.ends_with(".plist"))
        || (name.starts_with("payload-inventory-") && name.ends_with(".json"))
}

/// Installer leftovers next to an install path called `base`: its stage and previous copies and
/// the temporaries of any of them.
fn is_leftover(name: &str, base: &str) -> bool {
    let stage = format!(".{base}.crosspane-stage");
    let previous = format!(".{base}.crosspane-previous");
    name == stage
        || name == previous
        || [base, stage.as_str(), previous.as_str()]
            .iter()
            .any(|n| name.starts_with(&format!(".{n}.crosspane-temp-")))
}

#[derive(Default)]
struct Saved {
    base: Option<PathBuf>,
    paths: Vec<PathBuf>,
}
impl Saved {
    fn folder(
        &mut self,
        agent: &MacLaunchAgent,
        support: &SupportProof,
        sub: Option<&str>,
        deadline: &Deadline,
    ) -> NativeResult<PathBuf> {
        let base = match &self.base {
            Some(base) => base.clone(),
            None => {
                let created = agent.new_backup_folder(support, deadline)?;
                self.base = Some(created.clone());
                created
            }
        };
        let Some(sub) = sub else {
            return Ok(base);
        };
        let path = base.join(sub);
        if agent.io.metadata(&path)?.is_none() {
            agent.io.create_directory(support, &path, 0o700, deadline)?;
        }
        Ok(path)
    }
    fn moved(&mut self, path: Option<PathBuf>) {
        self.paths.extend(path);
    }
}

/// What a fresh start moved, for the person: where it was saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reclaimed {
    pub folder: PathBuf,
    pub moved: Vec<PathBuf>,
    /// The sign-in item was booted out to stop the agent first.
    pub stopped: bool,
}

impl MacLaunchAgent {
    /// Stop Crosspane's own sign-in item, then clear the install paths into a new backup folder.
    /// Returns `None` when nothing had to move.
    pub fn reclaim(&self, deadline: &Deadline) -> NativeResult<Option<Reclaimed>> {
        let mut support = self.io.admit_support(&self.main, deadline)?;
        let before = self.snapshot(&self.io, deadline)?;
        match (before.disabled, before.job) {
            (Disabled::Yes, _) => return Err(NativeError::Unsupported),
            (Disabled::Unknown, _) | (_, Job::Unknown) => return Err(NativeError::Unavailable),
            _ => {}
        }
        self.parents(&self.io.target().installer_dir(), &support, deadline)?;
        let lock = self.io.lock(&support, deadline)?;
        let mut saved = Saved::default();
        // 1. Setup's records first: the install that follows is a fresh one.
        let installer = self.io.target().installer_dir();
        for (name, _) in self.io.entries(&installer, 4096, deadline)? {
            if is_install_record(&name) {
                let to = saved.folder(self, &support, Some("Installer"), deadline)?;
                saved.moved(self.io.move_aside(
                    &support,
                    &installer.join(&name),
                    &to,
                    &name,
                    deadline,
                )?);
            }
        }
        // 2. Stop Crosspane's own sign-in item, so nothing runs from the files that move next.
        let stopped = matches!(before.job, Job::Running(_) | Job::LoadedStopped);
        if stopped {
            let output = Self::command(
                &self.io,
                NativeOperation::Launchctl(LaunchctlAction::Bootout),
                Some(&support),
                deadline,
            )?;
            let pid = match before.job {
                Job::Running(pid) => Some(pid),
                _ => None,
            };
            let end = self.io.clock().now_ms().saturating_add(STOP_WAIT_MS);
            let started = std::time::Instant::now();
            loop {
                let job = self.snapshot(&self.io, deadline)?.job;
                let running = match pid {
                    Some(pid) => self.io.pid_running(pid, deadline).unwrap_or(true),
                    None => false,
                };
                if job == Job::Absent && !running {
                    break;
                }
                let waited = self.io.clock().now_ms() >= end
                    || started.elapsed() >= Duration::from_millis(STOP_WAIT_MS);
                if waited || deadline.check().is_err() {
                    // The job must be gone before a new one can be bootstrapped. A process that
                    // outlives its job keeps running from the moved copy until it exits.
                    if job == Job::Absent {
                        break;
                    }
                    return Err(if output.code == Some(0) {
                        NativeError::OutcomeUnknown
                    } else {
                        NativeError::Refused
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            support = self.io.admit_support(&self.main, deadline)?;
        }
        // 3. The install paths and their installer leftovers.
        let home = self.io.target().paths().home.clone();
        let paths = [
            (home.join("Applications"), "Crosspane.app"),
            (home.join(".local/bin"), "crosspanectl"),
            (
                home.join("Library/LaunchAgents"),
                "io.frostdev.crosspane.agent.plist",
            ),
        ];
        for (directory, base) in paths {
            if self.io.metadata(&directory)?.is_none() {
                continue;
            }
            let mut names: Vec<_> = self
                .io
                .entries(&directory, 4096, deadline)?
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| name == base || is_leftover(name, base))
                .collect();
            names.sort();
            for name in names {
                let sub = (name != base).then_some("leftovers");
                let to = saved.folder(self, &support, sub, deadline)?;
                saved.moved(self.io.move_aside(
                    &support,
                    &directory.join(&name),
                    &to,
                    &name,
                    deadline,
                )?);
            }
        }
        drop(lock);
        Ok(saved.base.map(|folder| Reclaimed {
            folder,
            moved: saved.paths,
            stopped,
        }))
    }

    fn new_backup_folder(
        &self,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<PathBuf> {
        let root = self.io.target().backups_dir();
        self.parents(&root, support, deadline)?;
        let stamp = backup_stamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        );
        let mut name = stamp.clone();
        let mut attempt = 1;
        while self.io.metadata(&root.join(&name))?.is_some() {
            attempt += 1;
            if attempt > 100 {
                return Err(NativeError::Busy);
            }
            name = format!("{stamp}-{attempt}");
        }
        self.io
            .create_directory(support, &root.join(&name), 0o700, deadline)?;
        let mut older: Vec<_> = self
            .io
            .entries(&root, 4096, deadline)?
            .into_iter()
            .filter(|(entry, identity)| {
                *entry != name
                    && identity.mode & 0o170000 == 0o040000
                    && identity.uid == self.io.target().paths().uid
            })
            .map(|(entry, _)| entry)
            .collect();
        older.sort();
        let excess = (older.len() + 1).saturating_sub(KEEP_BACKUPS);
        for entry in older.into_iter().take(excess) {
            // A folder that can't be removed completely is simply kept.
            let _ = self.io.remove_backup(support, &entry, deadline);
        }
        Ok(root.join(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_leftovers_are_recognized_exactly() {
        for name in [
            "payload.json",
            "launch-agent.json",
            "launch-agent-prior-41.plist",
            "payload-inventory-f26b.json",
            ".payload.json.crosspane-temp-0123456789abcdef0123456789abcdef",
        ] {
            assert!(is_install_record(name), "{name}");
        }
        for name in [
            "lock",
            "packages",
            "repair.json",
            "removal.json",
            "audio.json",
        ] {
            assert!(!is_install_record(name), "{name}");
        }
        for name in [
            ".Crosspane.app.crosspane-stage",
            ".Crosspane.app.crosspane-previous",
            ".Crosspane.app.crosspane-temp-00",
            "..Crosspane.app.crosspane-stage.crosspane-temp-00",
        ] {
            assert!(is_leftover(name, "Crosspane.app"), "{name}");
        }
        for name in ["Crosspane.app", "Other.app", ".Other.app.crosspane-stage"] {
            assert!(!is_leftover(name, "Crosspane.app"), "{name}");
        }
        assert_eq!(backup_stamp(1_791_201_845), "20261005-120405");
    }
}
