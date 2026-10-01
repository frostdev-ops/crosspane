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
        let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")?;
        let paths = if cfg!(target_os = "macos") {
            let base = home.join("Library/Application Support/Crosspane");
            let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            Paths { config_dir: base.clone(), state_dir: base, runtime_dir: tmp.join("crosspane") }
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

    pub fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join("agent.sock")
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

/// Write `bytes` to `path` atomically, readable only by this user.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("write {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
