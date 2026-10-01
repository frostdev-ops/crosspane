//! Small secrets in the file-based login Keychain, without authentication UI.

use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{KeyStore, PlatformError};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::os::macos::passwords::find_generic_password;
use security_framework::passwords::{
    PasswordOptions, delete_generic_password_options, generic_password,
    set_generic_password_options,
};
use zeroize::Zeroizing;

const SERVICE: &str = "io.frostdev.crosspane";
const TIMEOUT: Duration = Duration::from_secs(2);
const ITEM_NOT_FOUND: i32 = -25300;

// ponytail: one process-wide lock because the interaction guard is process-global; only
// per-call OS controls could allow concurrent calls safely.
static KEYCHAIN: Mutex<()> = Mutex::new(());

/// The current user's default file-based Keychain.
#[derive(Debug, Default)]
pub struct MacKeychain;

impl MacKeychain {
    pub fn new() -> MacKeychain {
        MacKeychain
    }
}

// File-based login items do not sync and stay on this device. No kSecAttrAccessible*
// attribute is used: those accessibility classes require the data-protection Keychain.
fn options(name: &str) -> PasswordOptions {
    PasswordOptions::new_generic_password(SERVICE, name)
}

impl KeyStore for MacKeychain {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
        let name = name.to_owned();
        bounded(move |deadline| {
            guarded(deadline, || {
                absent(generic_password(options(&name))).map(|secret| secret.map(Zeroizing::new))
            })
        })
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), PlatformError> {
        let name = name.to_owned();
        let secret = Zeroizing::new(secret.to_vec());
        bounded(move |deadline| {
            match guarded(deadline, || {
                absent(find_generic_password(None, SERVICE, &name).map(|(_, item)| item))
            })? {
                Some(mut item) => guarded(deadline, || {
                    item.set_password(&secret).map_err(keychain_error)
                }),
                None => {
                    let mut query = options(&name);
                    query.set_label(&format!("Crosspane: {name}"));
                    guarded(deadline, || {
                        set_generic_password_options(&secret, query).map_err(keychain_error)
                    })
                }
            }
        })
    }

    fn delete(&self, name: &str) -> Result<(), PlatformError> {
        let name = name.to_owned();
        bounded(move |deadline| {
            guarded(deadline, || {
                absent(delete_generic_password_options(options(&name)))
            })
            .map(|_| ())
        })
    }
}

fn keychain_error(error: security_framework::base::Error) -> PlatformError {
    match error.code() {
        -25308 | -25293 => PlatformError::InteractionRequired,
        status => PlatformError::Backend(status.to_string()),
    }
}

fn absent<T>(result: security_framework::base::Result<T>) -> Result<Option<T>, PlatformError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error(error)),
    }
}

fn guarded<T>(
    deadline: Instant,
    operation: impl FnOnce() -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    let _interaction = SecKeychain::disable_user_interaction().map_err(keychain_error)?;
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    operation()
}

fn bounded<T: Send + 'static>(
    operation: impl FnOnce(Instant) -> Result<T, PlatformError> + Send + 'static,
) -> Result<T, PlatformError> {
    let deadline = Instant::now() + TIMEOUT;
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("crosspane-keychain".into())
        .spawn(move || {
            let result = match KEYCHAIN.lock() {
                Ok(_lock) => operation(deadline),
                Err(_) => Err(PlatformError::Backend("Keychain mutex poisoned".into())),
            };
            let _ = tx.send(result);
        })
        .map_err(|error| PlatformError::Backend(format!("Keychain worker: {error}")))?;
    // Native calls cannot be cancelled: an in-flight mutation may finish after Timeout.
    // The worker retains its interaction guard and checks the deadline before another call.
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(PlatformError::Backend("Keychain worker stopped".into()))
        }
    }
}
