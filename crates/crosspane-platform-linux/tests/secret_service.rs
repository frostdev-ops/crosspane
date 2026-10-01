//! Round trip against the desktop's Secret Service, using only a unique test item.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::io::Read;
use std::time::Duration;

use crosspane_platform::KeyStore;
use crosspane_platform_linux::secret_service::SecretServiceStore;
use zbus::blocking::{connection::Builder, fdo::DBusProxy};

struct Cleanup<'a> {
    store: &'a SecretServiceStore,
    name: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.store.delete(&self.name) {
            eprintln!(
                "Secret Service test cleanup failed for {}: {error}",
                self.name
            );
        }
    }
}

#[test]
fn live_round_trip() {
    let builder = match Builder::session() {
        Ok(builder) => builder,
        Err(_) => {
            eprintln!("skipped: no session bus for org.freedesktop.secrets");
            return;
        }
    };
    let connection = match builder.method_timeout(Duration::from_secs(2)).build() {
        Ok(connection) => connection,
        Err(_) => {
            eprintln!("skipped: no session bus for org.freedesktop.secrets");
            return;
        }
    };
    let bus = DBusProxy::new(&connection).unwrap();
    if !bus
        .name_has_owner("org.freedesktop.secrets".try_into().unwrap())
        .unwrap()
    {
        eprintln!("skipped: org.freedesktop.secrets is not on the session bus");
        return;
    }

    let mut random = [0_u8; 16];
    File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!("wp129-test-{suffix}");
    let store = SecretServiceStore::new().unwrap();
    // Install the guard before the first write, including a write that returns an error.
    let cleanup = Cleanup {
        store: &store,
        name,
    };
    let name = &cleanup.name;

    let original: [u8; 32] = [
        0x00, 0xff, 0x80, 0x7f, 0x01, 0xfe, 0x81, 0x7e, 0x02, 0xfd, 0x82, 0x7d, 0x03, 0xfc, 0x83,
        0x7c, 0x04, 0xfb, 0x84, 0x7b, 0x05, 0xfa, 0x85, 0x7a, 0x06, 0xf9, 0x86, 0x79, 0x07, 0xf8,
        0x87, 0x78,
    ];
    store.store(name, &original).unwrap();
    assert_eq!(store.load(name).unwrap().unwrap().as_slice(), &original);

    let replacement = [0x80; 32];
    store.store(name, &replacement).unwrap();
    assert_eq!(store.load(name).unwrap().unwrap().as_slice(), &replacement);

    store.delete(name).unwrap();
    assert!(store.load(name).unwrap().is_none());
    // The guard also exercises idempotent deletion of the now-missing item.
}
