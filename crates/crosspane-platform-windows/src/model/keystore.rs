//! Credential-name, protected-size, and absence/error rules without native calls.

use crosspane_platform::PlatformError;

/// Credential Manager's generic credential blob capacity (5 × 512 bytes).
pub const MAX_BLOB_SIZE: usize = 2560;
/// Production credentials always use this application namespace.
pub const TARGET_PREFIX: &str = "io.frostdev.crosspane/";
pub const ERROR_NOT_FOUND: u32 = 1168;
pub const ERROR_PASSWORD_RESTRICTION: u32 = 1325;

/// Reject before any native call; targets are case-insensitive, so uppercase is not normalized.
pub fn validate_name(name: &str) -> Result<(), PlatformError> {
    if !(1..=64).contains(&name.len())
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
    {
        return Err(PlatformError::Backend("invalid key store name".into()));
    }
    Ok(())
}

/// Applied to the actual DPAPI output, not to an assumed encryption overhead.
pub fn validate_blob_size(size: usize) -> Result<(), PlatformError> {
    if size > MAX_BLOB_SIZE {
        Err(PlatformError::TooLarge)
    } else {
        Ok(())
    }
}

/// Metadata only: no credential name, plaintext, or size is included in errors.
pub fn native_error(code: u32) -> PlatformError {
    if code == ERROR_PASSWORD_RESTRICTION {
        PlatformError::InteractionRequired
    } else {
        PlatformError::Backend(format!("Windows key store error {code}"))
    }
}

/// Only this error from CredRead confirms absence; DPAPI errors never imply absence.
pub fn read_error(code: u32) -> Result<(), PlatformError> {
    if code == ERROR_NOT_FOUND {
        Ok(())
    } else {
        Err(native_error(code))
    }
}

/// Missing credentials may be deleted repeatedly.
pub fn delete_error(code: u32) -> Result<(), PlatformError> {
    if code == ERROR_NOT_FOUND {
        Ok(())
    } else {
        Err(native_error(code))
    }
}
