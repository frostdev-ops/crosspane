//! Revocation notices this node issued (04 §4). They are kept on disk so that a peer which was
//! offline when a lost or stolen device was revoked still gets the notice when it next connects.
//! Receivers ignore duplicates and notices older than their own pairing with the revoked node.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crosspane_protocol::msg::RevocationNotice;
use crosspane_types::id::NodeId;
use serde::{Deserialize, Serialize};

/// At most this many notices are kept (the oldest go first).
const MAX: usize = 64;

pub struct Issued {
    path: PathBuf,
    notices: Vec<RevocationNotice>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    revoked: String,
    issuer: String,
    issued_at_ms: u64,
    signature: String,
}

impl Issued {
    /// Load the notices from `path`; a missing file is empty, a corrupt one is logged and
    /// treated as empty (the trust store's revoked set is what protects this node).
    pub fn load(path: PathBuf) -> Issued {
        #[cfg(windows)]
        let _parents = match crate::windows::security::pin_parent(&path) {
            Ok(pins) => pins,
            Err(_) => {
                return Issued {
                    path,
                    notices: Vec::new(),
                };
            }
        };
        let notices = match crate::paths::read_private_to_string(&path) {
            Ok(text) => parse(&text).unwrap_or_else(|e| {
                // Keep the unreadable file for inspection rather than overwrite it later.
                let aside = path.with_extension("json.corrupt");
                tracing::warn!(path = %path.display(), error = %e, aside = %aside.display(),
                    "unreadable revocations file moved aside");
                let _ = std::fs::rename(&path, &aside);
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Issued { path, notices }
    }

    pub fn notices(&self) -> &[RevocationNotice] {
        &self.notices
    }

    /// Keep `notice` (replacing an older one for the same node) and save.
    pub fn add(&mut self, notice: RevocationNotice) -> Result<()> {
        self.notices.retain(|n| n.revoked != notice.revoked);
        self.notices.push(notice);
        if self.notices.len() > MAX {
            let excess = self.notices.len() - MAX;
            self.notices.drain(..excess);
        }
        self.save()
    }

    /// Stop sending the notice for `node` (it was paired again). Returns whether one existed.
    pub fn remove(&mut self, node: NodeId) -> Result<bool> {
        let before = self.notices.len();
        self.notices.retain(|n| n.revoked != node);
        if self.notices.len() == before {
            return Ok(false);
        }
        self.save().map(|()| true)
    }

    fn save(&self) -> Result<()> {
        let stored: Vec<Stored> = self
            .notices
            .iter()
            .map(|n| Stored {
                revoked: n.revoked.to_string(),
                issuer: n.issuer.to_string(),
                issued_at_ms: n.issued_at_ms,
                signature: hex(&n.signature),
            })
            .collect();
        let json = serde_json::to_vec_pretty(&stored)?;
        if let Some(dir) = self.path.parent() {
            crate::paths::create_private_dir(dir)?;
        }
        crate::paths::write_private(&self.path, &json)
            .with_context(|| format!("write {}", self.path.display()))
    }
}

fn parse(text: &str) -> Result<Vec<RevocationNotice>> {
    let stored: Vec<Stored> = serde_json::from_str(text)?;
    // A malformed entry is skipped, not fatal: the others stay deliverable.
    Ok(stored
        .into_iter()
        .take(MAX)
        .filter_map(|s| {
            Some(RevocationNotice {
                revoked: s.revoked.parse().ok()?,
                issuer: s.issuer.parse().ok()?,
                issued_at_ms: s.issued_at_ms,
                signature: unhex(&s.signature)?,
            })
        })
        .collect())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2)
        || text.len() > 512
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The file next to the trust store.
pub fn file_beside(trust_file: &Path) -> PathBuf {
    trust_file.with_file_name("revocations.json")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn notice(revoked: u8, at: u64) -> RevocationNotice {
        RevocationNotice {
            revoked: NodeId([revoked; 32]),
            issuer: NodeId([9; 32]),
            issued_at_ms: at,
            signature: vec![0x30, 0x45, 0x02, 0xff],
        }
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crosspane-revocations-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("revocations.json")
    }

    #[test]
    fn notices_survive_a_reload_and_replace_per_node() {
        let path = temp();
        let mut issued = Issued::load(path.clone());
        assert!(issued.notices().is_empty());
        issued.add(notice(1, 10)).unwrap();
        issued.add(notice(2, 11)).unwrap();
        issued.add(notice(1, 12)).unwrap();
        let reloaded = Issued::load(path.clone());
        assert_eq!(reloaded.notices(), &[notice(2, 11), notice(1, 12)]);
        let mut reloaded = reloaded;
        assert!(reloaded.remove(NodeId([2; 32])).unwrap());
        assert!(!reloaded.remove(NodeId([2; 32])).unwrap());
        assert_eq!(Issued::load(path).notices(), &[notice(1, 12)]);
    }

    #[test]
    fn the_list_is_capped_and_corrupt_files_are_empty() {
        let path = temp();
        let mut issued = Issued::load(path.clone());
        for i in 0..70u8 {
            issued.add(notice(i, u64::from(i))).unwrap();
        }
        assert_eq!(issued.notices().len(), MAX);
        assert_eq!(issued.notices()[0], notice(6, 6));
        crate::paths::write_fixture(&path, "not json").unwrap();
        assert!(Issued::load(path.clone()).notices().is_empty());
        assert!(path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])),
            Some(vec![0, 1, 0xab, 0xff])
        );
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("+f"), None);
    }
}
