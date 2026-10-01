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
        Ok(SharedTrust { inner: Arc::new(RwLock::new(Loaded { store, modified })), path })
    }

    /// Re-read the file if it changed. Returns true if it did.
    pub fn refresh(&self) -> Result<bool> {
        let modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        {
            let loaded = self.inner.read().map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
            if loaded.modified == modified {
                return Ok(false);
            }
        }
        let (store, modified) = read(&self.path)?;
        let mut loaded = self.inner.write().map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
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

    /// Change the store and save it.
    pub fn update<R>(&self, f: impl FnOnce(&mut TrustStore) -> Result<R>) -> Result<R> {
        let mut loaded = self.inner.write().map_err(|_| anyhow::anyhow!("trust lock poisoned"))?;
        let result = f(&mut loaded.store)?;
        save(&self.path, &loaded.store)?;
        loaded.modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(result)
    }
}

impl PinStore for SharedTrust {
    fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        self.with(|store| store.trusted(spki))
    }
}

fn read(path: &std::path::Path) -> Result<(TrustStore, Option<SystemTime>)> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            let store = TrustStore::from_json(&text).with_context(|| format!("parse {}", path.display()))?;
            Ok((store, modified))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((TrustStore::new(), None)),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn save(path: &std::path::Path, store: &TrustStore) -> Result<()> {
    write_private(path, store.to_json().as_bytes())
}
