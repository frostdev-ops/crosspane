//! Opt-in, Limited-token round trip. Never enumerates or touches production credentials.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::{
    fmt::Write,
    ptr,
    time::{Duration, Instant},
};

use crosspane_platform::{KeyStore, PlatformError};
use crosspane_platform_windows::model;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{
        Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, TokenElevationType,
        TokenElevationTypeLimited,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
use zeroize::Zeroizing;

// Compiling the adapter as part of this test exposes ONLY its cfg(test) nonce constructor.
// The production library offers no configurable namespace constructor.
#[path = "../src/keystore.rs"]
mod adapter;
use adapter::WindowsKeyStore;

const NAMES: [&str; 2] = ["roundtrip", "boundary"];

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: This test owns the token opened below and closes it exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn require_limited() -> Result<(), &'static str> {
    let mut token = ptr::null_mut();
    // SAFETY: Only this process's query-only token is opened; output is a valid local slot.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err("own-token query failed");
    }
    let token = Token(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut kind = 0i32;
    let mut written = 0;
    // SAFETY: Both output buffers match the requested fixed token-information classes.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        ) != 0
            && GetTokenInformation(
                token.0,
                TokenElevationType,
                (&mut kind as *mut i32).cast(),
                size_of::<i32>() as u32,
                &mut written,
            ) != 0
    };
    if !ok || elevation.TokenIsElevated != 0 || kind != TokenElevationTypeLimited {
        return Err("native round trip requires the Limited GUI helper");
    }
    Ok(())
}

fn nonce() -> Result<String, &'static str> {
    let mut random = [0u8; 16];
    // SAFETY: The system RNG writes only this test's fixed local buffer; no provider handle.
    if unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err("nonce generation failed");
    }
    let mut text = String::with_capacity(32);
    for byte in random {
        write!(&mut text, "{byte:02x}").map_err(|_| "nonce formatting failed")?;
    }
    Ok(text)
}

fn room(deadline: Instant) -> Result<(), PlatformError> {
    if deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(2) {
        Err(PlatformError::Timeout)
    } else {
        Ok(())
    }
}

struct Cleanup {
    store: WindowsKeyStore,
    namespace: String,
    verified: bool,
    attempted: bool,
}

impl Cleanup {
    fn run(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        self.attempted = true;
        // A timed-out store may still own an admitted native write. Never delete ahead of it.
        self.store
            .wait_quiescent(deadline.saturating_duration_since(Instant::now()))?;
        for name in NAMES {
            room(deadline)?;
            self.store.delete(name)?;
            self.store
                .wait_quiescent(deadline.saturating_duration_since(Instant::now()))?;
            room(deadline)?;
            if self.store.load(name)?.is_some() {
                return Err(PlatformError::Backend(
                    "owned test credential remains".into(),
                ));
            }
            self.store
                .wait_quiescent(deadline.saturating_duration_since(Instant::now()))?;
        }
        self.verified = true;
        Ok(())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.attempted {
            let _ = self.run(Instant::now() + Duration::from_secs(12));
        }
        if !self.verified {
            // Namespace/name metadata only; report exactly the two possible owned residues.
            eprintln!(
                "CLEANUP BLOCKER: {}roundtrip or {}boundary",
                self.namespace, self.namespace
            );
        }
    }
}

fn check_value(store: &WindowsKeyStore, name: &str, expected: &[u8]) -> Result<(), PlatformError> {
    if store
        .load(name)?
        .as_deref()
        .is_some_and(|actual| actual.as_slice() == expected)
    {
        Ok(())
    } else {
        Err(PlatformError::Backend("opaque test value mismatch".into()))
    }
}

fn roundtrip(store: &WindowsKeyStore, deadline: Instant) -> Result<(), PlatformError> {
    let first = Zeroizing::new(vec![0x37; 32]);
    let second = Zeroizing::new(vec![0x91; 48]);
    room(deadline)?;
    store.store("roundtrip", &first)?;
    room(deadline)?;
    check_value(store, "roundtrip", &first)?;
    room(deadline)?;
    store.store("roundtrip", &second)?;
    room(deadline)?;
    check_value(store, "roundtrip", &second)?;
    room(deadline)?;
    store.delete("roundtrip")?;
    room(deadline)?;
    if store.load("roundtrip")?.is_some() {
        return Err(PlatformError::Backend("delete not confirmed".into()));
    }
    room(deadline)?;
    store.delete("roundtrip")?;

    // Plaintext at the raw cap expands under DPAPI. Failure must preserve the prior value.
    room(deadline)?;
    store.store("boundary", &first)?;
    room(deadline)?;
    let at_raw_cap = Zeroizing::new(vec![0x53; model::keystore::MAX_BLOB_SIZE]);
    if !matches!(
        store.store("boundary", &at_raw_cap),
        Err(PlatformError::TooLarge)
    ) {
        return Err(PlatformError::Backend(
            "encrypted expansion boundary not enforced".into(),
        ));
    }
    room(deadline)?;
    check_value(store, "boundary", &first)?;
    Ok(())
}

#[test]
#[ignore = "Limited win-gui opt-in; nonce credentials only"]
fn limited_nonce_store_load_overwrite_delete_and_encrypted_capacity() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_KEYSTORE_GUI").as_deref(),
        Ok("1"),
        "native test opt-in missing"
    );
    require_limited().expect("Limited token is required before credential access");
    let nonce = nonce().expect("nonce is required before credential access");
    // Guard exists before the first possible mutation, including any failed assertion.
    let mut cleanup = Cleanup {
        store: WindowsKeyStore::test_namespace(&nonce).unwrap(),
        namespace: format!("io.frostdev.crosspane.test-{nonce}/"),
        verified: false,
        attempted: false,
    };
    let result = roundtrip(&cleanup.store, Instant::now() + Duration::from_secs(8));
    let cleaned = cleanup.run(Instant::now() + Duration::from_secs(12));
    assert!(cleaned.is_ok(), "nonce cleanup could not be verified");
    assert!(
        result.is_ok(),
        "opaque native round trip failed: {result:?}"
    );
    println!("Limited nonce round trip PASS; expansion refusal PASS; cleanup 2/2 verified");
}
