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
        let notices = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable revocations file");
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
    stored
        .into_iter()
        .take(MAX)
        .map(|s| {
            Ok(RevocationNotice {
                revoked: s.revoked.parse().context("revoked node id")?,
                issuer: s.issuer.parse().context("issuer node id")?,
                issued_at_ms: s.issued_at_ms,
                signature: unhex(&s.signature).context("signature")?,
            })
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.len() > 512 {
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
        std::fs::write(&path, "not json").unwrap();
        assert!(Issued::load(path).notices().is_empty());
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])),
            Some(vec![0, 1, 0xab, 0xff])
        );
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
