//! Ignored native probe: initial observation only, never synthetic input or owner-window access.
#![cfg(windows)]
#![allow(unsafe_code)]

use crosspane_platform::{Chord, GlobalHotkeys, HotkeyEvent};
use crosspane_platform_windows::hotkey::WindowsHotkeys;
use crosspane_types::hid::HidUsage;
use std::{
    ptr,
    sync::{Arc, mpsc},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, TokenElevationType,
        TokenElevationTypeLimited,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    UI::Input::{GetRegisteredRawInputDevices, RAWINPUTDEVICE},
};

struct Cancel(mpsc::Sender<()>);
impl Drop for Cancel {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

fn limited() -> bool {
    let mut token = ptr::null_mut();
    // SAFETY: read-only query of this probe's own token, exact valid output handle slot.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut kind = 0i32;
    let mut bytes = 0;
    // SAFETY: initialized fixed-size buffers match the two own-token information classes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        ) != 0
            && GetTokenInformation(
                token,
                TokenElevationType,
                (&mut kind as *mut i32).cast(),
                size_of::<i32>() as u32,
                &mut bytes,
            ) != 0
    };
    // SAFETY: the probe owns the token handle and closes it once on every opened path.
    unsafe {
        CloseHandle(token);
    }
    ok && elevation.TokenIsElevated == 0 && kind == TokenElevationTypeLimited
}

fn keyboard_registrations() -> usize {
    let mut entries = [RAWINPUTDEVICE::default(); 64];
    let mut count = entries.len() as u32;
    // SAFETY: only our process's registration inventory; aligned fixed local buffer and size.
    let read = unsafe {
        GetRegisteredRawInputDevices(
            entries.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    assert_ne!(read, u32::MAX);
    assert!(read as usize <= entries.len());
    entries
        .iter()
        .take(read as usize)
        .filter(|entry| entry.usUsagePage == 1 && (entry.usUsage == 6 || entry.usUsage == 0))
        .count()
}

#[test]
#[ignore = "Limited initial-state probe; invoke only through win-gui, --ignored --exact"]
fn limited_hotkey_initial_subscription_and_owned_cleanup() {
    let (cancel, cancelled) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if matches!(
            cancelled.recv_timeout(Duration::from_secs(8)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("Hotkey probe exceeded own deadline");
            std::process::exit(124);
        }
    });
    let cancellation = Cancel(cancel);
    assert!(
        limited(),
        "Limited token required BEFORE any registration or async key observation"
    );
    assert_eq!(keyboard_registrations(), 0);
    let mut backend = WindowsHotkeys::new().unwrap();
    backend
        .set_chord(&Chord {
            modifiers: vec![HidUsage::keyboard(0xe0), HidUsage::keyboard(0xe1)],
            key: HidUsage::keyboard(0x45),
        })
        .unwrap();
    let (tx, rx) = mpsc::channel();
    backend
        .subscribe(Arc::new(move |event| {
            tx.send(event).unwrap();
        }))
        .unwrap();
    let initial = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    println!(
        "Limited initial state delivered: {}",
        if matches!(initial, HotkeyEvent::Pressed { .. }) {
            "Pressed"
        } else {
            "Released"
        }
    );
    assert_eq!(keyboard_registrations(), 1);
    drop(backend);
    assert_eq!(keyboard_registrations(), 0);
    println!(
        "Owned cleanup verified: keyboard registrations 1->0; no input posted or session transition invoked"
    );
    drop(cancellation);
    watchdog.join().unwrap();
}
