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
    /// macOS: hide projected windows on a private-API virtual display (D7) instead of mirroring
    /// them in place (M1). Needs a build with the `private-vdisplay` feature; ignored otherwise.
    pub mac_virtual_display: bool,
    /// Modifier remap profile per peer name, applied while this node controls that peer:
    /// `none`, `swap-ctrl-gui` (Ctrl ↔ ⌘/Super) or `swap-alt-gui` (Alt ↔ ⌘/Super).
    pub remap: BTreeMap<String, RemapProfile>,
    /// E2 video bitrate in Mbit/s while windows show motion (WP-2.14); 0 turns video off and
    /// keeps every projection on lossless tiles.
    pub video_mbps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerAddr {
    pub addr: SocketAddr,
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
            crossing: true,
            mac_virtual_display: false,
            remap: BTreeMap::new(),
            video_mbps: 20,
        }
    }
}

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

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.save(paths)?;
                Ok(config)
            }
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serialise config")?;
        write_private(&paths.config_file(), text.as_bytes())
    }
}
