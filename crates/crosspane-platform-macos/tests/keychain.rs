//! GUI-session round trip using a unique, disposable Keychain item.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::File;
use std::io::Read;

use crosspane_platform::KeyStore;
use crosspane_platform_macos::{clock, keychain::MacKeychain};
use objc2_core_graphics::CGEvent;
use security_framework::item::{ItemClass, ItemSearchOptions};
use security_framework::os::macos::keychain::SecKeychain;

// Apple's public <mach/mach_time.h>.
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

struct Cleanup<'a> {
    store: &'a MacKeychain,
    name: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.store.delete(&self.name) {
            eprintln!("Keychain test cleanup failed for {}: {error}", self.name);
            assert!(std::thread::panicking(), "Keychain test cleanup failed");
        }
    }
}

#[test]
fn live_round_trip() {
    // This only enables harmless Keychain testing, never capture, injection, or locking.
    if std::env::var("CROSSPANE_KEYCHAIN_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: run in the GUI session with CROSSPANE_KEYCHAIN_GUI=1");
        return;
    }

    let interaction_before = SecKeychain::user_interaction_allowed().unwrap();
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let store = MacKeychain::new();
    // Install cleanup before the first write, including a write that returns an error.
    let cleanup = Cleanup {
        store: &store,
        name: format!("wp130-test-{suffix}"),
    };
    let name = &cleanup.name;
    let original: [u8; 32] = [
        0x00, 0xff, 0x80, 0x7f, 0x01, 0xfe, 0x81, 0x7e, 0x02, 0xfd, 0x82, 0x7d, 0x03, 0xfc, 0x83,
        0x7c, 0x04, 0xfb, 0x84, 0x7b, 0x05, 0xfa, 0x85, 0x7a, 0x06, 0xf9, 0x86, 0x79, 0x07, 0xf8,
        0x87, 0x78,
    ];
    let replacement = [0x80; 32];
    store.store(name, &original).unwrap();
    assert_eq!(store.load(name).unwrap().unwrap().as_slice(), &original);
    {
        let _interaction = SecKeychain::disable_user_interaction().unwrap();
        let attributes = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service("io.frostdev.crosspane")
            .account(name)
            .load_attributes(true)
            .search()
            .unwrap();
        let attributes = attributes[0].simplify_dict().unwrap();
        assert_eq!(attributes.get("labl"), Some(&format!("Crosspane: {name}")));
    }
    store.store(name, &replacement).unwrap();
    assert_eq!(store.load(name).unwrap().unwrap().as_slice(), &replacement);
    store.delete(name).unwrap();
    assert!(store.load(name).unwrap().is_none());
    // Drop exercises missing-item deletion too, and must succeed before declaring completion.
    drop(cleanup);
    assert_eq!(
        SecKeychain::user_interaction_allowed().unwrap(),
        interaction_before
    );

    // Read-only timestamp observation required by the WP's shared clock contract.
    let event = CGEvent::new(None).expect("create an unposted observation event");
    let timestamp = CGEvent::timestamp(Some(&event));
    // SAFETY: no arguments; reads the system's monotonic tick counter without affecting input.
    let ticks = unsafe { mach_absolute_time() };
    let nanos = clock::now().as_nanos();
    let converted = clock::from_ticks(timestamp).as_nanos();
    eprintln!(
        "CGEvent timestamp={timestamp}, mach ticks={ticks}, clock nanos={nanos}, converted={converted}"
    );
    // A newly created event can have timestamp zero, which cannot identify its units.
    // Keychain operations have no event timestamps to convert.
    eprintln!("Keychain store/load/replace/delete and cleanup completed");
}
