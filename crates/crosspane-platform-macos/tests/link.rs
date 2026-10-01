#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{Interface, LinkClass, LinkInfo, PlatformError};
use crosspane_platform_macos::link::MacLinkInfo;

#[test]
fn enumerates_current_interfaces_read_only() {
    let interfaces = MacLinkInfo::new().interfaces().expect("interface snapshot");
    println!("index  name         class          up");
    for interface in &interfaces {
        println!(
            "{:<6} {:<12} {:<14} {}",
            interface.index,
            interface.name,
            format!("{:?}", interface.class),
            interface.up
        );
    }
    assert!(
        interfaces
            .windows(2)
            .all(|pair| pair[0].index <= pair[1].index)
    );
    assert_eq!(
        interfaces
            .iter()
            .map(|interface| &interface.name)
            .collect::<BTreeSet<_>>()
            .len(),
        interfaces.len()
    );
    for interface in &interfaces {
        assert_ne!(interface.name, "lo0");
        assert!(!interface.addrs.is_empty());
        assert_eq!(interface.mtu, None);
        assert_eq!(interface.speed_mbps, None);
    }

    let started = Instant::now();
    let mut child = match Command::new("/usr/sbin/networksetup")
        .arg("-listallhardwareports")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            println!("networksetup unavailable; snapshot succeeded: {error}");
            return;
        }
    };
    loop {
        if started.elapsed() >= Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            println!("networksetup timed out; snapshot succeeded");
            return;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                println!("networksetup wait failed; snapshot succeeded: {error}");
                return;
            }
        }
    }
    let output = child.wait_with_output().expect("networksetup output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !stdout.contains("Hardware Port:") {
        println!(
            "networksetup unavailable ({}); snapshot succeeded: {} {}",
            output.status,
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }
    let mut port = "";
    for line in stdout.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Hardware Port:") {
            port = name.trim();
        } else if line == "Device: en0" && matches!(port, "Wi-Fi" | "AirPort") {
            let en0 = interfaces
                .iter()
                .find(|interface| interface.name == "en0")
                .expect("addressed en0 Wi-Fi interface");
            assert_eq!(en0.class, LinkClass::Wifi);
        }
    }
}

#[test]
fn subscription_delivers_initial_snapshot_and_stops_on_drop() {
    let mut links = MacLinkInfo::new();
    let (tx, rx) = mpsc::channel::<Vec<Interface>>();
    let started = Instant::now();
    links
        .subscribe(Arc::new(move |snapshot| {
            let _ = tx.send(snapshot);
        }))
        .expect("subscribe");
    let initial = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("first snapshot within 3 seconds");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(initial.iter().all(|interface| interface.name != "lo0"));
    assert!(matches!(
        links.subscribe(Arc::new(|_| {})),
        Err(PlatformError::Backend(_))
    ));
    let started = Instant::now();
    drop(links);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "drop took {:?}",
        started.elapsed()
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
}
