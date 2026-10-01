//! Owner-attended only. Build crosspane-testapp first (or set CROSSPANE_TESTAPP to its binary).
//! The fixture owns its AppKit loop; libtest never creates AppKit objects.
#![cfg(target_os = "macos")]
#![allow(clippy::expect_used)]

use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr::NonNull;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crosspane_platform::{IoGate, KeyInjector, PlatformError, PointerInjector};
use crosspane_platform_macos::inject::{MacKeyInjector, injectors};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGMainDisplayID};

fn live(name: &str) -> Option<File> {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: {name} requires CROSSPANE_MAC_LIVE=1 and the lead's GUI session");
        return None;
    }
    // File locks also serialize separate nextest processes, which a Rust static mutex cannot.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(std::env::temp_dir().join("crosspane-mac-live-inject.lock"))
        .expect("open live-test lock");
    lock.lock()
        .expect("serialize owner-attended injection tests");
    Some(lock)
}

fn open_gate() -> std::sync::Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

struct Fixture {
    child: Child,
    directory: PathBuf,
    events: PathBuf,
}

impl Fixture {
    fn start() -> Self {
        let executable = std::env::var_os("CROSSPANE_TESTAPP")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .expect("test executable path")
                    .parent()
                    .expect("test deps directory")
                    .parent()
                    .expect("target profile directory")
                    .join("crosspane-testapp")
            });
        assert!(
            executable.is_file(),
            "build crosspane-testapp first, or set CROSSPANE_TESTAPP"
        );
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("wall clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("crosspane-inject-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).expect("create fixture directory");
        let events = directory.join("events.jsonl");
        let child = Command::new(executable)
            .args([
                "window",
                "--title",
                "Crosspane injection fixture",
                "--events",
            ])
            .arg(&events)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("start fixture");
        let fixture = Self {
            child,
            directory,
            events,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = fs::read_to_string(&fixture.events).unwrap_or_default();
            if log.contains("\"event\":\"ready\"") && fixture.focused() {
                return fixture;
            }
            assert!(
                Instant::now() < deadline,
                "fixture must be ready and focused"
            );
            sleep(Duration::from_millis(10));
        }
    }

    fn focused(&self) -> bool {
        // SAFETY: public read-only system AX object; this never raises or moves an owner's window.
        let system = unsafe { AXUIElement::new_system_wide() };
        // SAFETY: valid AX object and positive timeout; only bounds this read-only query.
        let status = unsafe { system.set_messaging_timeout(0.05) };
        if status != AXError::Success {
            return false;
        }
        // AXAttributeConstants.h defines kAXFocusedApplicationAttribute as this public string.
        let attribute = CFString::from_str("AXFocusedApplication");
        let mut value: *const CFType = std::ptr::null();
        // SAFETY: the output slot is writable and remains alive for the call; AX returns +1 CFType.
        let status = unsafe { system.copy_attribute_value(&attribute, NonNull::from(&mut value)) };
        if status != AXError::Success {
            return false;
        }
        let Some(value) = NonNull::new(value.cast_mut()) else {
            return false;
        };
        // SAFETY: a non-null result of CopyAttributeValue owns one retain; RAII releases it.
        let value = unsafe { CFRetained::from_raw(value) };
        let Some(application) = value.downcast_ref::<AXUIElement>() else {
            return false;
        };
        let mut pid = 0;
        // SAFETY: a valid AX application and writable pid output slot; this only reads its pid.
        let status = unsafe { application.pid(NonNull::from(&mut pid)) };
        status == AXError::Success && u32::try_from(pid).ok() == Some(self.child.id())
    }

    fn press(&self, keys: &mut MacKeyInjector, usage: HidUsage) {
        assert!(self.focused(), "refuse to type after fixture loses focus");
        keys.key(usage, true).expect("submit key down");
    }

    fn text(&self) -> String {
        // Read only the ASCII text generated by these tests, from the existing fixture's JSONL.
        let log = fs::read_to_string(&self.events).expect("read fixture events");
        // Some input methods deliver committed text instead of keyboard-event text.
        let commits = log.contains("\"event\":\"ime_commit\"");
        log.lines()
            .filter(|line| {
                if commits {
                    line.contains("\"event\":\"ime_commit\"")
                } else {
                    line.contains("\"event\":\"key\"") && line.contains("\"state\":\"pressed\"")
                }
            })
            .filter_map(|line| {
                line.split_once("\"text\":\"")?
                    .1
                    .split_once('"')
                    .map(|s| s.0)
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only the process spawned by this test is terminated; no owner's application is touched.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn live_types_hello_into_fixture() {
    let Some(_live) = live("type Hello") else {
        return;
    };
    let fixture = Fixture::start();
    let (mut keys, _pointer) = injectors(open_gate()).expect("Accessibility grant for test runner");
    let caps = keys
        .lock_keys()
        .expect("lock state")
        .caps_lock
        .expect("caps lock state");
    let shift = HidUsage::keyboard(0xE1);
    for (index, id) in [0x0B, 0x08, 0x0F, 0x0F, 0x12].into_iter().enumerate() {
        let needs_shift = caps != (index == 0);
        if needs_shift {
            fixture.press(&mut keys, shift);
        }
        let usage = HidUsage::keyboard(id);
        fixture.press(&mut keys, usage);
        keys.key(usage, false).expect("release fixture key");
        if needs_shift {
            keys.key(shift, false).expect("release shift");
        }
    }
    keys.release_all().expect("release fixture keys");
    let deadline = Instant::now() + Duration::from_secs(2);
    while fixture.text().len() < 5 && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    assert_eq!(fixture.text(), "Hello");
}

#[test]
fn live_closed_gate_is_locked() {
    let Some(_live) = live("closed gate") else {
        return;
    };
    let (mut keys, mut pointer) =
        injectors(IoGate::new()).expect("Accessibility grant for test runner");
    assert!(matches!(
        keys.key(HidUsage::keyboard(0x04), true),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.button(MouseButton::PRIMARY, true),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.move_to(DisplayId(CGMainDisplayID()), PointDevice::new(100.0, 100.0)),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        pointer.scroll(ScrollDelta {
            v120_x: 0,
            v120_y: 120,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        }),
        Err(PlatformError::Locked)
    ));
    let caps = keys
        .lock_keys()
        .expect("caps state")
        .caps_lock
        .expect("macOS caps lock");
    assert!(matches!(
        keys.set_lock_keys(LockKeys {
            caps_lock: Some(!caps),
            ..LockKeys::default()
        }),
        Err(PlatformError::Locked)
    ));
}

#[test]
fn live_release_all_leaves_no_keys_held() {
    let Some(_live) = live("release_all") else {
        return;
    };
    let fixture = Fixture::start();
    let gate = open_gate();
    let (mut keys, _pointer) =
        injectors(gate.clone()).expect("Accessibility grant for test runner");
    fixture.press(&mut keys, HidUsage::keyboard(0xE1));
    fixture.press(&mut keys, HidUsage::keyboard(0x04));
    let deadline = Instant::now() + Duration::from_secs(1);
    while ![0x00, 0x38]
        .into_iter()
        .all(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code))
    {
        assert!(fixture.focused(), "fixture lost focus while keys were held");
        assert!(
            Instant::now() < deadline,
            "injected downs were not observed"
        );
        sleep(Duration::from_millis(5));
    }
    gate.set_engine_permits(false);
    keys.release_all()
        .expect("releases must work with a closed gate");
    keys.release_all().expect("idempotent release_all");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let held = [0x00, 0x38]
            .into_iter()
            .any(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code));
        if !held {
            break;
        }
        assert!(Instant::now() < deadline, "an injected key remained held");
        sleep(Duration::from_millis(5));
    }
}

#[test]
fn live_repeats_a_for_one_second() {
    let Some(_live) = live("autorepeat") else {
        return;
    };
    let fixture = Fixture::start();
    let (mut keys, _pointer) = injectors(open_gate()).expect("Accessibility grant for test runner");
    let caps = keys
        .lock_keys()
        .expect("lock state")
        .caps_lock
        .expect("caps lock state");
    if caps {
        fixture.press(&mut keys, HidUsage::keyboard(0xE1));
    }
    fixture.press(&mut keys, HidUsage::keyboard(0x04));
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        assert!(fixture.focused(), "fixture lost focus during repeat");
        sleep(Duration::from_millis(10));
    }
    keys.key(HidUsage::keyboard(0x04), false)
        .expect("stop repeat with release");
    keys.release_all().expect("release fixture keys");
    sleep(Duration::from_millis(100));
    let text = fixture.text();
    assert!(
        text.len() > 5,
        "expected more than five repeated characters"
    );
    assert!(text.chars().all(|character| character == 'a'));
    sleep(Duration::from_millis(150));
    assert_eq!(
        fixture.text(),
        text,
        "repetition must stop before release returns"
    );
}
