//! Where the agent keeps its files.
//!
//! - Linux: config in `$XDG_CONFIG_HOME/crosspane`, state (the input journal) in
//!   `$XDG_STATE_HOME/crosspane`, the control socket in `$XDG_RUNTIME_DIR/crosspane`.
//! - macOS: config and state in `~/Library/Application Support/Crosspane`, the control socket in
//!   `$TMPDIR/crosspane` (per-user on macOS).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Paths> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let paths = if cfg!(target_os = "macos") {
            let base = home.join("Library/Application Support/Crosspane");
            let tmp =
                std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            Paths {
                config_dir: base.clone(),
                state_dir: base,
                runtime_dir: tmp.join("crosspane"),
            }
        } else {
            let xdg = |var: &str, fallback: &str| {
                std::env::var_os(var).map_or_else(|| home.join(fallback), PathBuf::from)
            };
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .context("XDG_RUNTIME_DIR is not set")?;
            Paths {
                config_dir: xdg("XDG_CONFIG_HOME", ".config").join("crosspane"),
                state_dir: xdg("XDG_STATE_HOME", ".local/state").join("crosspane"),
                runtime_dir: runtime.join("crosspane"),
            }
        };
        // Several agents on one machine (integration tests) need their own control sockets.
        let paths = match std::env::var_os("CROSSPANE_RUNTIME_DIR") {
            Some(dir) => Paths {
                runtime_dir: PathBuf::from(dir),
                ..paths
            },
            None => paths,
        };
        for dir in [&paths.config_dir, &paths.state_dir, &paths.runtime_dir] {
            create_private_dir(dir)?;
        }
        Ok(paths)
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn trust_file(&self) -> PathBuf {
        self.config_dir.join("trust.json")
    }

    pub fn journal_file(&self) -> PathBuf {
        self.state_dir.join("input.journal")
    }

    pub fn e2_journal_file(&self) -> PathBuf {
        self.state_dir.join("projection-input.journal")
    }

    pub fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join("agent.sock")
    }

    pub fn bootstrap_file(&self) -> PathBuf {
        self.runtime_dir.join("bootstrap.json")
    }

    pub fn exit_receipt(&self) -> PathBuf {
        self.state_dir.join("last_exit.json")
    }

    pub fn instance_lock(&self) -> PathBuf {
        self.state_dir.join("agent.lock")
    }

    /// Fallback device-key file, used only when the OS key store is unavailable and the config
    /// allows it.
    pub fn key_file(&self) -> PathBuf {
        self.state_dir.join("device-key.pk8")
    }
}

/// Create `dir` (and parents) readable only by this user.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))?;
    Ok(())
}

/// Serialize identity and trust mutations independently of the running agent's instance lock.
/// The returned file holds the lock until dropped; a busy writer gets at most five seconds.
pub fn identity_mutation_lock(state_dir: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = state_dir.join("identity.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open identity mutation lock {}", path.display()))?;
    wait_for_mutation_lock(&file, std::time::Duration::from_secs(5))?;
    Ok(file)
}

fn wait_for_mutation_lock(file: &std::fs::File, timeout: std::time::Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                MUTATION_LOCK_WAITING.with(|waiting| {
                    if let Some(waiting) = waiting.borrow_mut().take() {
                        let _ = waiting.send(());
                    }
                });
                let remaining = timeout.saturating_sub(started.elapsed());
                anyhow::ensure!(
                    !remaining.is_zero(),
                    "identity mutation lock is busy; timed out waiting for another writer"
                );
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(20)));
            }
            Err(error) => return Err(error).context("lock identity mutations"),
        }
    }
}

#[cfg(test)]
std::thread_local! {
    // A one-shot observation of actual lock contention, so concurrency fixtures need no sleeps.
    static MUTATION_LOCK_WAITING: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn observe_mutation_lock_wait(waiting: std::sync::mpsc::Sender<()>) {
    MUTATION_LOCK_WAITING.with(|slot| *slot.borrow_mut() = Some(waiting));
}

/// Write `bytes` to `path` atomically, readable only by this user.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (tmp, mut file) = loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create private temporary file"),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod mutation_tests {
    #[test]
    fn identity_mutation_lock_has_a_bounded_wait_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("crosspane-mutation-lock-{}", std::process::id()));
        super::create_private_dir(&dir).unwrap();
        let held = super::identity_mutation_lock(&dir).unwrap();
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("identity.lock"))
            .unwrap();
        let error = super::wait_for_mutation_lock(&other, std::time::Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(
            other.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(held);
        super::wait_for_mutation_lock(&other, std::time::Duration::ZERO).unwrap();
        drop(other);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
