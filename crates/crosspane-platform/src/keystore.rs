//! Secret storage in the OS key store: Keychain on macOS, Secret Service on Linux, DPAPI on
//! Windows (04 §2, 09 §2).

use zeroize::Zeroizing;

use crate::PlatformError;

/// Stores small secrets, such as the device private key, under short names in Crosspane's own
/// namespace (`io.frostdev.crosspane`). Backends restrict access to this user and this app where
/// the OS allows. Calls never prompt: if the store needs unlocking or approval, they return
/// [`PlatformError::InteractionRequired`].
pub trait KeyStore: Send + Sync {
    /// The secret stored under `name`. `None` only when the store confirms there is none; any doubt
    /// is an error.
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError>;

    /// Store `secret` under `name`, replacing any previous value.
    fn store(&self, name: &str, secret: &[u8]) -> Result<(), PlatformError>;

    /// Delete the secret under `name`. Deleting a missing secret succeeds.
    fn delete(&self, name: &str) -> Result<(), PlatformError>;
}
