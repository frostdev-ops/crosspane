//! Native body is ignored by elevated cargo; only Limited win-gui may opt in.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
pub use crosspane_platform_windows::model;
#[path = "../src/link.rs"]
mod link;

use crosspane_platform::LinkInfo;
use std::{
    mem::size_of,
    ptr::null_mut,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Security::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct Watchdog {
    done: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new() -> Self {
        let (done, waiting) = mpsc::channel();
        let worker = thread::spawn(move || {
            if waiting.recv_timeout(Duration::from_secs(10)).is_err() {
                eprintln!("LINK_PROBE own-process watchdog expired; cleanup unverified");
                std::process::exit(124);
            }
        });
        Self {
            done,
            worker: Some(worker),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(worker) = self.worker.take() {
            assert!(worker.join().is_ok());
        }
    }
}
fn require_limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut length = 0;
    // SAFETY: read-only own token query, initialized exact output size, balanced handle close.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(result, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "Limited win-gui route required"
    );
}
#[test]
#[ignore = "read-only native inventory; explicit Limited win-gui opt-in only"]
fn limited_read_only_link_snapshot_and_subscription() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_LINK_PROBE").as_deref(),
        Ok("1")
    );
    require_limited();
    let _watchdog = Watchdog::new();
    let mut link = link::WindowsLinkInfo::new().unwrap();
    let interfaces = link.interfaces().unwrap();
    println!("LINK_PROBE count={}", interfaces.len());
    for interface in &interfaces {
        println!(
            "LINK index={} class={:?} up={} mtu={:?} speed_mbps={:?} v4_count={} v6_count={}",
            interface.index,
            interface.class,
            interface.up,
            interface.mtu,
            interface.speed_mbps,
            interface.addrs.iter().filter(|ip| ip.is_ipv4()).count(),
            interface.addrs.iter().filter(|ip| ip.is_ipv6()).count()
        );
    }
    let (send, receive) = mpsc::channel();
    let started = Instant::now();
    link.subscribe(Arc::new(move |snapshot| {
        let _ = send.send(snapshot);
    }))
    .unwrap();
    let first = receive
        .recv_timeout(Duration::from_secs(1).saturating_sub(started.elapsed()))
        .unwrap();
    assert!(started.elapsed() <= Duration::from_secs(1));
    println!(
        "LINK_PROBE initial_count={} first_ms={}",
        first.len(),
        started.elapsed().as_millis()
    );
    assert!(link::WindowsLinkInfo::stop_verified(link));
    println!(
        "LINK_PROBE cleanup=verified notifications_cancelled=2 observer_joined=true delivery_joined=true"
    );
}
