//! Per-user, nonroaming generic credentials, protected again with user-scope DPAPI.
//!
//! Credential Manager targets are case-insensitive. Names must already be lowercase and satisfy
//! the short-name policy; normalization would silently alias different caller names. DPAPI's
//! public application entropy separates this format, but does not isolate it from other programs
//! running as this user. No path permits UI or falls back to plaintext.
//!
//! Callers wait at most two seconds. An already-running native call cannot be preempted and may
//! finish a mutation after Timeout; no subsequent native step starts after the shared deadline.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::{
    fmt, ptr,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use crosspane_platform::{KeyStore, PlatformError};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree},
    Security::{
        Credentials::{
            CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
            CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
        },
        Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    },
};
use zeroize::{Zeroize, Zeroizing};

use crate::model::keystore::{
    TARGET_PREFIX, delete_error, native_error, read_error, validate_blob_size, validate_name,
};

const CALL_BOUND: Duration = Duration::from_secs(2);
const ENTROPY: &[u8] = b"io.frostdev.crosspane/windows-keystore/v1";
const _: () =
    assert!(CRED_MAX_CREDENTIAL_BLOB_SIZE as usize == crate::model::keystore::MAX_BLOB_SIZE);

#[derive(Default)]
struct Flights {
    count: Mutex<usize>,
    changed: Condvar,
}

struct Completion(Arc<Flights>);

impl Drop for Completion {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        self.0.changed.notify_all();
    }
}

/// Credential Manager/DPAPI adapter. Debug intentionally reveals no names, values, or sizes.
pub struct WindowsKeyStore {
    prefix: String,
    flights: Arc<Flights>,
}

impl fmt::Debug for WindowsKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WindowsKeyStore")
    }
}

impl Default for WindowsKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsKeyStore {
    pub fn new() -> Self {
        Self {
            prefix: TARGET_PREFIX.into(),
            flights: Arc::default(),
        }
    }

    fn target(&self, name: &str) -> Result<Vec<u16>, PlatformError> {
        validate_name(name)?;
        Ok(format!("{}{name}", self.prefix)
            .encode_utf16()
            .chain(Some(0))
            .collect())
    }

    fn bounded<T: Send + 'static>(
        &self,
        bound: Duration,
        call: impl FnOnce(Instant) -> Result<T, PlatformError> + Send + 'static,
    ) -> Result<T, PlatformError> {
        let deadline = Instant::now() + bound;
        *self.flights.count.lock().map_err(|_| state_error())? += 1;
        let completion = Completion(self.flights.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("crosspane-keystore".into())
            .spawn(move || {
                let _completion = completion;
                let result = check_deadline(deadline).and_then(|()| call(deadline));
                // Failed delivery drops (and zeroizes) a late plaintext result.
                let _ = tx.send(result);
            })
            .map_err(|_| PlatformError::Backend("key store worker could not start".into()))?;
        let result = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    PlatformError::Backend("key store worker stopped".into())
                }
            })?;
        check_deadline(deadline)?;
        result
    }

    #[cfg(test)]
    pub(crate) fn test_namespace(nonce: &str) -> Result<Self, PlatformError> {
        if nonce.len() != 32
            || !nonce
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(PlatformError::Backend(
                "invalid key store test namespace".into(),
            ));
        }
        Ok(Self {
            prefix: format!("io.frostdev.crosspane.test-{nonce}/"),
            flights: Arc::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn wait_quiescent(&self, bound: Duration) -> Result<(), PlatformError> {
        let deadline = Instant::now() + bound;
        let mut count = self.flights.count.lock().map_err(|_| state_error())?;
        while *count != 0 {
            check_deadline(deadline)?;
            count = self
                .flights
                .changed
                .wait_timeout(count, deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| state_error())?
                .0;
        }
        Ok(())
    }
}

impl KeyStore for WindowsKeyStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
        let target = self.target(name)?;
        self.bounded(CALL_BOUND, move |deadline| load_native(&target, deadline))
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), PlatformError> {
        let target = self.target(name)?;
        validate_blob_size(secret.len())?;
        let secret = Zeroizing::new(secret.to_vec());
        self.bounded(CALL_BOUND, move |deadline| {
            store_native(&target, &secret, deadline)
        })
    }

    fn delete(&self, name: &str) -> Result<(), PlatformError> {
        let target = self.target(name)?;
        self.bounded(CALL_BOUND, move |deadline| delete_native(&target, deadline))
    }
}

fn state_error() -> PlatformError {
    PlatformError::Backend("key store worker state unavailable".into())
}

fn check_deadline(deadline: Instant) -> Result<(), PlatformError> {
    if Instant::now() >= deadline {
        Err(PlatformError::Timeout)
    } else {
        Ok(())
    }
}

fn last_error() -> u32 {
    // SAFETY: Reads only the calling thread's last-error metadata, immediately after failure.
    unsafe { GetLastError() }
}

fn input_blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    // Every caller has already bounded this slice, or uses the fixed small entropy constant.
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    }
}

struct LocalBlob(CRYPT_INTEGER_BLOB);

impl LocalBlob {
    fn empty() -> Self {
        Self(CRYPT_INTEGER_BLOB::default())
    }

    fn bytes(&self) -> Result<&[u8], PlatformError> {
        validate_blob_size(self.0.cbData as usize)?;
        if self.0.cbData == 0 {
            return Ok(&[]);
        }
        if self.0.pbData.is_null() {
            return Err(PlatformError::Backend("invalid DPAPI result".into()));
        }
        // SAFETY: A successful DPAPI call owns an allocation of cbData bytes until LocalFree.
        Ok(unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) })
    }
}

impl Drop for LocalBlob {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            // SAFETY: DPAPI owns this writable allocation and reports its complete byte count.
            let owned =
                unsafe { std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize) };
            // SecureZeroMemory is SDK-inline, not a DLL export. The approved equivalent uses
            // zeroize's volatile writes/fence on the OS-owned slice BEFORE LocalFree.
            owned.zeroize();
            // SAFETY: Only the DPAPI-returned LocalAlloc allocation is freed, exactly once.
            unsafe {
                LocalFree(self.0.pbData.cast());
            }
        }
    }
}

struct Credential(*mut CREDENTIALW);

impl Drop for Credential {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: CredReadW returned this single allocation; only CredFree releases it.
            unsafe {
                CredFree(self.0.cast());
            }
        }
    }
}

fn store_native(target: &[u16], secret: &[u8], deadline: Instant) -> Result<(), PlatformError> {
    check_deadline(deadline)?;
    let input = input_blob(secret);
    let entropy = input_blob(ENTROPY);
    let mut protected = LocalBlob::empty();
    // SAFETY: Input/entropy remain borrowed and valid; output is owned by protected. Null
    // description/reserved/prompt and UI_FORBIDDEN prevent interaction; no machine-scope flag.
    if unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut protected.0,
        )
    } == 0
    {
        return Err(native_error(last_error()));
    }
    // The encrypted output is authoritative. Never guess overhead, truncate, or use plaintext.
    protected.bytes()?;
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_ptr().cast_mut(),
        CredentialBlobSize: protected.0.cbData,
        CredentialBlob: protected.0.pbData,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    check_deadline(deadline)?;
    // SAFETY: All pointers reference live buffers for this synchronous call; flags=0 replaces
    // the exact generic target in the current user's credential set, with no enumeration.
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(native_error(last_error()));
    }
    Ok(())
}

fn load_native(
    target: &[u16],
    deadline: Instant,
) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
    check_deadline(deadline)?;
    let mut credential = Credential(ptr::null_mut());
    // SAFETY: target is validated, NUL-terminated UTF-16; output remains worker-local and owned.
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential.0) } == 0 {
        read_error(last_error())?;
        return Ok(None);
    }
    if credential.0.is_null() {
        return Err(PlatformError::Backend("invalid credential result".into()));
    }
    // SAFETY: Successful CredReadW returned a valid CREDENTIALW until this guard's CredFree.
    let credential_ref = unsafe { &*credential.0 };
    validate_blob_size(credential_ref.CredentialBlobSize as usize)?;
    if credential_ref.CredentialBlobSize != 0 && credential_ref.CredentialBlob.is_null() {
        return Err(PlatformError::Backend("invalid credential blob".into()));
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: credential_ref.CredentialBlobSize,
        pbData: credential_ref.CredentialBlob,
    };
    let entropy = input_blob(ENTROPY);
    let mut plain = LocalBlob::empty();
    check_deadline(deadline)?;
    // SAFETY: Ciphertext stays in the CredRead allocation; entropy is fixed. The plaintext
    // allocation is released by plain after zeroization. No prompt or description is requested.
    if unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut plain.0,
        )
    } == 0
    {
        return Err(native_error(last_error()));
    }
    Ok(Some(Zeroizing::new(plain.bytes()?.to_vec())))
}

fn delete_native(target: &[u16], deadline: Instant) -> Result<(), PlatformError> {
    check_deadline(deadline)?;
    // SAFETY: Only the validated exact target is deleted for the current user; no enumeration.
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        delete_error(last_error())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_native_capacity_matches_the_pure_model() {
        assert_eq!(
            CRED_MAX_CREDENTIAL_BLOB_SIZE as usize,
            crate::model::keystore::MAX_BLOB_SIZE
        );
        assert_eq!(CALL_BOUND, Duration::from_secs(2));
    }

    #[test]
    fn invalid_name_and_plaintext_oversize_start_no_worker() {
        let store = WindowsKeyStore::new();
        assert!(store.load("Upper").is_err());
        assert!(store.delete("../other").is_err());
        assert!(store.store("valid", &vec![0; 2561]).is_err());
        assert_eq!(*store.flights.count.lock().unwrap(), 0);
    }

    #[test]
    fn test_namespace_cannot_be_an_arbitrary_target_prefix() {
        assert!(WindowsKeyStore::test_namespace("../production").is_err());
        let store = WindowsKeyStore::test_namespace(&"a".repeat(32)).unwrap();
        assert!(store.prefix.starts_with("io.frostdev.crosspane.test-"));
        assert_eq!(format!("{store:?}"), "WindowsKeyStore");
    }

    #[test]
    fn timeout_keeps_inflight_tracked_until_worker_quiescence() {
        let store = WindowsKeyStore::new();
        let (release, wait) = mpsc::channel();
        let result = store.bounded(Duration::from_millis(10), move |_deadline| {
            wait.recv().unwrap(); // Models a nonpreemptible native call, without OS access.
            Ok(())
        });
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(matches!(
            store.wait_quiescent(Duration::ZERO),
            Err(PlatformError::Timeout)
        ));
        release.send(()).unwrap();
        store.wait_quiescent(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn expired_worker_does_not_begin_another_native_step() {
        let store = WindowsKeyStore::new();
        let (release, wait) = mpsc::channel();
        let next_call = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = next_call.clone();
        assert!(matches!(
            store.bounded(Duration::from_millis(10), move |deadline| {
                wait.recv().unwrap();
                check_deadline(deadline)?;
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
            Err(PlatformError::Timeout)
        ));
        release.send(()).unwrap();
        store.wait_quiescent(Duration::from_secs(1)).unwrap();
        assert!(!next_call.load(std::sync::atomic::Ordering::SeqCst));
    }
}
