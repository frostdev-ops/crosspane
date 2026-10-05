//! `config.toml`: user settings. Created with defaults on first run.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use crosspane_input::remap::RemapProfile;
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, write_private};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The name peers see.
    pub name: String,
    /// The UDP port to listen on.
    pub port: u16,
    /// Acceptance-only loopback override; rejected unless platform scratch admission succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) acceptance_bind_ip: Option<std::net::IpAddr>,
    /// Peers to dial, by address. Discovery (WP-1.6) adds more later.
    pub peers: Vec<PeerAddr>,
    /// Push-to-cross delay in milliseconds (0–200).
    pub push_to_cross_ms: u64,
    /// Store the device key in a 0600 file when the OS key store is unavailable.
    pub allow_file_keystore: bool,
    /// Always use the 0600 key file, never the OS key store (several agents on one machine).
    pub force_file_keystore: bool,
    /// E1 edge crossing from this node. `false` keeps this node's screen edges inert (it can
    /// still be controlled and project windows).
    pub crossing: bool,
    /// Drag-across offers (DRAG-v0), negotiated with each peer.
    pub drag: Drag,
    /// macOS: hide projected windows on a private-API virtual display (D7) instead of mirroring
    /// them in place (M1). Needs a build with the `private-vdisplay` feature; ignored otherwise.
    pub mac_virtual_display: bool,
    /// Modifier remap profile per peer name, applied while this node controls that peer:
    /// `none`, `swap-ctrl-gui` (Ctrl ↔ ⌘/Super) or `swap-alt-gui` (Alt ↔ ⌘/Super).
    pub remap: BTreeMap<String, RemapProfile>,
    /// E2 video bitrate in Mbit/s while windows show motion (WP-2.14); 0 turns video off and
    /// keeps every projection on lossless tiles. Unset: chosen per peer from the link class
    /// (03 §7.4): 150 on USB4/Thunderbolt or a direct cable, 50 on a wired LAN, 20 on Wi-Fi.
    pub video_mbps: Option<u32>,
    /// Show frame rate and capture-to-screen latency in projected windows' titles (05
    /// "Observability": the opt-in latency overlay).
    pub latency_overlay: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerAddr {
    pub addr: SocketAddr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Drag {
    pub across: bool,
    /// Window-drag edge dwell in milliseconds (0–2000).
    pub push_to_cross_ms: u64,
}

impl Default for Drag {
    fn default() -> Self {
        Self {
            across: true,
            push_to_cross_ms: 150,
        }
    }
}

impl Drag {
    pub fn push_to_cross(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.push_to_cross_ms.min(2000))
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            port: crosspane_transport::DEFAULT_PORT,
            peers: Vec::new(),
            push_to_cross_ms: 0,
            allow_file_keystore: cfg!(target_os = "macos"),
            force_file_keystore: false,
            acceptance_bind_ip: None,
            crossing: true,
            drag: Drag::default(),
            mac_virtual_display: false,
            remap: BTreeMap::new(),
            video_mbps: None,
            latency_overlay: false,
        }
    }
}

#[cfg(unix)]
fn default_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("/bin/hostname")
                .arg("-s")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "crosspane".to_owned())
}

#[cfg(windows)]
fn default_name() -> String {
    crate::windows::process::hostname().unwrap_or_else(|| "crosspane".to_owned())
}

/// The revision of a config file (`status.result.installer.config_revision`, WP-4.5): 16 lowercase
/// hex digits, `xxh3_64` of its bytes; all zeros for no file at all (the only zero).
pub fn revision_of(bytes: Option<&[u8]>) -> String {
    format!("{:016x}", bytes.map_or(0, xxhash_rust::xxh3::xxh3_64))
}

/// Dispatch policy is OS-specific; the Mac file operation stays testable on either OS.
pub fn settings_update(
    paths: &Paths,
    expected_revision: &str,
    mac_virtual_display: bool,
) -> crate::ctl::Response {
    #[cfg(any(target_os = "linux", windows))]
    {
        let _ = (paths, expected_revision, mac_virtual_display);
        crate::ctl::Response::err("not_supported")
    }
    #[cfg(target_os = "macos")]
    {
        update_mac_setting(&paths.config_file(), expected_revision, mac_virtual_display)
    }
}

#[cfg(any(target_os = "macos", test))]
fn update_mac_setting(
    path: &std::path::Path,
    expected_revision: &str,
    mac_virtual_display: bool,
) -> crate::ctl::Response {
    let update = || -> Result<crate::ctl::Response> {
        let bytes = match crate::paths::read_private(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read config"),
        };
        if revision_of(bytes.as_deref()) != expected_revision {
            return Ok(crate::ctl::Response::err("revision_conflict"));
        }
        let text = std::str::from_utf8(bytes.as_deref().unwrap_or_default())?;
        let mut table = text.parse::<toml::Table>()?;
        table.insert(
            "mac_virtual_display".into(),
            toml::Value::Boolean(mac_virtual_display),
        );
        let text = toml::to_string(&table)?;
        write_private(path, text.as_bytes())?;
        Ok(crate::ctl::Response::ok(serde_json::json!({
            "revision": revision_of(Some(text.as_bytes())),
            "restart_required": true,
        })))
    };
    update().unwrap_or_else(|_| crate::ctl::Response::err("config_update_failed"))
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        Config::load_revision(paths).map(|(config, _)| config)
    }

    /// [`Config::load`], and the revision of the exact bytes the config was parsed from: what this
    /// run is configured with, whatever happens to the file afterwards. No file is loaded as the
    /// defaults (which are then saved) and has the zero revision; a file that can't be read is an
    /// error, never the zero revision.
    pub fn load_revision(paths: &Paths) -> Result<(Config, String)> {
        let file = paths.config_file();
        match crate::paths::read_private(&file) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .with_context(|| format!("read {}: not valid UTF-8", file.display()))?;
                let config =
                    toml::from_str(text).with_context(|| format!("parse {}", file.display()))?;
                Ok((config, revision_of(Some(&bytes))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.save(paths)?;
                Ok((config, revision_of(None)))
            }
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serialise config")?;
        write_private(&paths.config_file(), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use super::*;

    /// Paths in a scratch directory of their own, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("crosspane-config-{}-{test}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            #[cfg(unix)]
            std::fs::create_dir_all(&dir).unwrap();
            #[cfg(windows)]
            crate::paths::create_private_dir(&dir).unwrap();
            Scratch(dir)
        }

        fn paths(&self) -> Paths {
            Paths {
                config_dir: self.0.clone(),
                state_dir: self.0.clone(),
                runtime_dir: self.0.clone(),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_revision_is_sixteen_hex_digits_of_the_bytes_and_zeros_only_for_no_file() {
        assert_eq!(revision_of(None), "0000000000000000");
        // The known `xxh3_64` of no bytes at all: an empty file is a file, not "no file".
        assert_eq!(revision_of(Some(b"")), "2d06800538d394c2");
        let one = revision_of(Some(b"name = \"desk\"\n"));
        assert_eq!(one.len(), 16);
        assert!(
            one.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert_eq!(revision_of(Some(b"name = \"desk\"\n")), one);
        assert_ne!(revision_of(Some(b"name = \"desktop\"\n")), one);
    }

    #[test]
    fn drag_push_delay_default_parse_and_clamp() {
        for text in ["", "name = 'legacy'", "[drag]"] {
            let config: Config = toml::from_str(text).unwrap();
            assert_eq!(config.drag.push_to_cross_ms, 150);
            assert_eq!(
                config.drag.push_to_cross(),
                std::time::Duration::from_millis(150)
            );
        }
        for (configured, effective) in [
            (0, 0),
            (45, 45),
            (2000, 2000),
            (2001, 2000),
            (i64::MAX as u64, 2000),
        ] {
            let text = format!("[drag]\nacross = false\npush_to_cross_ms = {configured}");
            let config: Config = toml::from_str(&text).unwrap();
            assert!(!config.drag.across);
            assert_eq!(config.drag.push_to_cross_ms, configured);
            assert_eq!(
                config.drag.push_to_cross(),
                std::time::Duration::from_millis(effective)
            );
            let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(roundtrip.drag.push_to_cross(), config.drag.push_to_cross());
        }
        assert!(toml::from_str::<Config>("[drag]\npush_to_cross_ms = -1").is_err());
    }

    #[test]
    fn drag_across_defaults_on_and_an_explicit_drag_table_can_disable_it() {
        for text in ["", "name = 'legacy'", "[drag]"] {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.drag.across);
        }
        let config: Config = toml::from_str("[drag]\nacross = false").unwrap();
        assert!(!config.drag.across);
        assert!(
            !toml::from_str::<Config>(&toml::to_string(&config).unwrap())
                .unwrap()
                .drag
                .across
        );
        assert!(toml::from_str::<Config>("[drag]\nunknown = true").is_err());
    }

    #[test]
    fn the_revision_is_that_of_the_bytes_loaded_even_if_the_file_is_replaced_afterwards() {
        let scratch = Scratch::new("replaced");
        let paths = scratch.paths();
        let loaded = b"name = \"loaded\"\nport = 47811\n";
        crate::paths::write_fixture(paths.config_file(), loaded).unwrap();
        let (config, revision) = Config::load_revision(&paths).unwrap();
        assert_eq!(config.name, "loaded");
        assert_eq!(revision, revision_of(Some(loaded)));
        // Someone edits the file while the agent waits for its key store (or just runs): the
        // revision it keeps still describes what it loaded.
        let edited = b"name = \"edited\"\nport = 47811\n";
        crate::paths::write_fixture(paths.config_file(), edited).unwrap();
        assert_eq!(revision, revision_of(Some(loaded)));
        assert_ne!(revision, revision_of(Some(edited)));
        // The next start loads, and reports, the edited file.
        let (config, next) = Config::load_revision(&paths).unwrap();
        assert_eq!(config.name, "edited");
        assert_eq!(next, revision_of(Some(edited)));
    }

    #[test]
    fn no_file_loads_the_defaults_with_the_zero_revision_and_saves_them() {
        let scratch = Scratch::new("missing");
        let paths = scratch.paths();
        assert!(!paths.config_file().exists());
        let (config, revision) = Config::load_revision(&paths).unwrap();
        assert_eq!(config.port, Config::default().port);
        assert_eq!(revision, "0000000000000000");
        // The defaults are saved for the next start, and `load` still works as before.
        assert!(paths.config_file().exists());
        assert_eq!(Config::load(&paths).unwrap().port, config.port);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error_never_the_zero_revision() {
        let scratch = Scratch::new("unreadable");
        let paths = scratch.paths();
        // A directory where the file should be: reading it fails, which is not "no file".
        std::fs::create_dir(paths.config_file()).unwrap();
        let error = Config::load_revision(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("read"), "{error:#}");
        assert!(Config::load(&paths).is_err());
        // Bytes that aren't text, and text that isn't a config, are errors too.
        std::fs::remove_dir(paths.config_file()).unwrap();
        crate::paths::write_fixture(paths.config_file(), [0xff, 0xfe, 0x00]).unwrap();
        assert!(Config::load_revision(&paths).is_err());
        crate::paths::write_fixture(paths.config_file(), b"not = [valid").unwrap();
        assert!(Config::load_revision(&paths).is_err());
    }

    #[test]
    fn settings_revision_conflict_changes_nothing_including_missing_files() {
        let scratch = Scratch::new("settings-conflict");
        let path = scratch.paths().config_file();
        let response = update_mac_setting(&path, "0123456789abcdef", true);
        assert_eq!(response.error.as_deref(), Some("revision_conflict"));
        assert!(!path.exists());
        let original =
            b"# keep until an update succeeds\nname = \"desk\"\nmac_virtual_display = false\n";
        crate::paths::write_fixture(&path, original).unwrap();
        let response = update_mac_setting(&path, &revision_of(None), true);
        assert_eq!(response.error.as_deref(), Some("revision_conflict"));
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn settings_update_preserves_all_other_keys_and_returns_the_saved_revision() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("settings-preserve");
        let path = scratch.paths().config_file();
        let original = b"# comments are not preserved\nname = \"desk\"\nport = 47812\nmac_virtual_display = false\nfuture = [1, 2, 3]\n[remap]\nlaptop = \"swap-ctrl-gui\"\n[[peers]]\naddr = \"127.0.0.1:47811\"\n";
        crate::paths::write_fixture(&path, original).unwrap();
        let expected_revision = revision_of(Some(original));
        let response = update_mac_setting(&path, &expected_revision, true);
        assert!(response.ok);
        let saved = std::fs::read(&path).unwrap();
        assert_eq!(
            response.result["revision"],
            serde_json::json!(revision_of(Some(&saved)))
        );
        assert_eq!(response.result["restart_required"], serde_json::json!(true));
        let mut before = std::str::from_utf8(original)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        before.insert("mac_virtual_display".into(), toml::Value::Boolean(true));
        assert_eq!(
            std::str::from_utf8(&saved)
                .unwrap()
                .parse::<toml::Table>()
                .unwrap(),
            before
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let conflict = update_mac_setting(&path, &expected_revision, false);
        assert_eq!(conflict.error.as_deref(), Some("revision_conflict"));
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[test]
    fn settings_update_accepts_the_zero_revision_only_for_a_missing_config() {
        let scratch = Scratch::new("settings-missing");
        let path = scratch.paths().config_file();
        let response = update_mac_setting(&path, "0000000000000000", false);
        assert!(response.ok);
        let saved = std::fs::read(&path).unwrap();
        assert_eq!(
            response.result["revision"],
            serde_json::json!(revision_of(Some(&saved)))
        );
        let table = std::str::from_utf8(&saved)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table["mac_virtual_display"], toml::Value::Boolean(false));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn settings_update_is_not_supported_on_linux_and_writes_nothing() {
        let scratch = Scratch::new("settings-linux");
        let paths = scratch.paths();
        assert_eq!(
            settings_update(&paths, "0000000000000000", true)
                .error
                .as_deref(),
            Some("not_supported")
        );
        assert!(!paths.config_file().exists());
    }
}
