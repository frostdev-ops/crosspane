//! The device identity (04 §2): a P-256 key kept in the OS key store, created on first run.

use std::path::Path;

use anyhow::{Context, Result, bail};
use crosspane_platform::{KeyStore, PlatformError};
use crosspane_security::identity::DeviceIdentity;

use crate::paths::write_private;

/// The key store entry name.
const KEY_NAME: &str = "device-key";

/// Load the device identity, creating and storing a new one on first run. With
/// `allow_file_fallback`, an unavailable OS key store falls back to a 0600 file in the state
/// directory (development use; logged as a warning).
pub fn load_or_create(
    store: Option<&dyn KeyStore>,
    key_file: &Path,
    allow_file_fallback: bool,
) -> Result<DeviceIdentity> {
    if let Some(store) = store {
        match store.load(KEY_NAME) {
            Ok(Some(pkcs8)) => {
                return DeviceIdentity::from_pkcs8(&pkcs8).context("stored device key is invalid");
            }
            Ok(None) => {
                let identity = DeviceIdentity::generate().context("generate device key")?;
                match store.store(KEY_NAME, identity.pkcs8()) {
                    Ok(()) => {
                        tracing::info!(node = %identity.node().short(), "created device key in the OS key store");
                        return Ok(identity);
                    }
                    Err(e) if allow_file_fallback => {
                        tracing::warn!(error = %e, "OS key store refused the device key; using the file fallback");
                    }
                    Err(e) => bail!("store device key: {e}"),
                }
            }
            Err(PlatformError::InteractionRequired) if !allow_file_fallback => {
                bail!("the OS key store is locked; unlock it and restart Crosspane")
            }
            Err(e) if allow_file_fallback => {
                tracing::warn!(error = %e, "OS key store unavailable; using the file fallback");
            }
            Err(e) => bail!("load device key: {e}"),
        }
    } else if !allow_file_fallback {
        bail!("no OS key store on this platform and the file fallback is disabled");
    }
    load_or_create_file(key_file)
}

fn load_or_create_file(path: &Path) -> Result<DeviceIdentity> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = zeroize::Zeroizing::new(bytes);
            DeviceIdentity::from_pkcs8(&bytes)
                .with_context(|| format!("invalid key in {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity = DeviceIdentity::generate().context("generate device key")?;
            write_private(path, identity.pkcs8())?;
            tracing::info!(node = %identity.node().short(), path = %path.display(), "created device key file");
            Ok(identity)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        bail!("odd-length hex");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .context("invalid hex")
        })
        .collect()
}
