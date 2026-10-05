//! The trust store on disk (`trust.json`) and the transport's view of it.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use crosspane_security::trust::TrustStore;
use crosspane_transport::PinStore;
use crosspane_types::id::NodeId;

use crate::paths::write_private;

/// The trust store shared by the transport's verifiers and the agent, reloaded when the file
/// changes (so `crosspane-agent trust …` edits apply to a running agent).
#[derive(Clone, Debug)]
pub struct SharedTrust {
    inner: Arc<RwLock<Loaded>>,
    path: PathBuf,
}

#[derive(Debug)]
struct Loaded {
    store: TrustStore,
    modified: Option<SystemTime>,
}

impl SharedTrust {
    pub fn load(path: PathBuf) -> Result<SharedTrust> {
        let (store, modified) = read(&path)?;
        Ok(SharedTrust {
            inner: Arc::new(RwLock::new(Loaded { store, modified })),
            path,
        })
    }

    /// Re-read the file if it changed. Returns true if it did.
    pub fn refresh(&self) -> Result<bool> {
        let modified = crate::paths::metadata_private(&self.path)
            .and_then(|m| m.modified())
            .ok();
        {
            let loaded = self
                .inner
                .read()
                .map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
            if loaded.modified == modified {
                return Ok(false);
            }
        }
        let (store, modified) = read(&self.path)?;
        let mut loaded = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
        *loaded = Loaded { store, modified };
        Ok(true)
    }

    pub fn with<R>(&self, f: impl FnOnce(&TrustStore) -> R) -> R {
        match self.inner.read() {
            Ok(loaded) => f(&loaded.store),
            // A poisoned lock means a panic mid-update: trust nothing.
            Err(_) => f(&TrustStore::new()),
        }
    }

    /// Change the store and save it, all or nothing: the change applies in memory only once it
    /// is on disk, so a failed save never leaves the two disagreeing (a restart would otherwise
    /// undo a revocation the running agent already acted on).
    pub fn update<R>(&self, f: impl FnOnce(&mut TrustStore) -> Result<R>) -> Result<R> {
        let mut loaded = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
        let mut next = loaded.store.clone();
        let result = f(&mut next)?;
        if next != loaded.store {
            save(&self.path, &next)?;
            loaded.store = next;
            loaded.modified = crate::paths::metadata_private(&self.path)
                .and_then(|m| m.modified())
                .ok();
        }
        Ok(result)
    }
}

impl PinStore for SharedTrust {
    fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        self.with(|store| store.trusted(spki))
    }
}

fn read(path: &std::path::Path) -> Result<(TrustStore, Option<SystemTime>)> {
    match crate::paths::read_private_to_string(path) {
        Ok(text) => {
            let modified = crate::paths::metadata_private(path)
                .and_then(|m| m.modified())
                .ok();
            let store = TrustStore::from_json(&text)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok((store, modified))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((TrustStore::new(), None)),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save(path: &std::path::Path, store: &TrustStore) -> Result<()> {
    write_private(path, store.to_json().as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crosspane_security::identity::DeviceIdentity;
    use crosspane_security::trust::{PeerEntry, default_grants};

    #[test]
    fn a_failed_save_changes_nothing_in_memory() {
        // Admit an empty, owned parent, then remove it to make every save fail portably.
        let scratch = std::env::temp_dir().join(format!(
            "crosspane-trust-save-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::paths::create_private_dir(&scratch).unwrap();
        let trust = SharedTrust::load(scratch.join("trust.json")).unwrap();
        std::fs::remove_dir(&scratch).unwrap();
        let peer = DeviceIdentity::generate().unwrap();
        let result = trust.update(|t| {
            t.pin(PeerEntry {
                node: peer.node(),
                spki: peer.spki().to_vec(),
                name: "x".into(),
                granted: default_grants(),
                paired_at_ms: 1,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
        });
        assert!(result.is_err());
        assert!(trust.with(|t| t.get(peer.node()).is_none()));
        // An update that changes nothing doesn't write (and so succeeds here).
        assert!(
            trust
                .update(|t| Ok(t.forget(peer.node())))
                .unwrap()
                .is_none()
        );
    }
}
