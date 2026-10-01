#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crosspane_platform::{ParkingKind, PlatformError, WindowParking, WindowSource};
use crosspane_platform_macos::{parking::MacMirrorParking, windows::MacWindows};
use crosspane_types::geom::{PixelSize, PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::WindowId;
use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CGRect};
use objc2_core_graphics::{
    CGEvent, CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber,
    kCGWindowOwnerName, kCGWindowOwnerPID,
};

// Public declaration from Apple's <mach/mach_time.h>, for the WP's read-only clock probe.
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

fn quartz_windows() -> Vec<(WindowId, String, RectLogical)> {
    let list = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .expect("read-only Quartz window list");
    // SAFETY: Quartz returns an array of CFDictionary objects.
    let list = unsafe { list.cast_unchecked::<objc2_core_foundation::CFType>() };
    // SAFETY: immutable public CoreGraphics window dictionary keys.
    let (number, pid, layer, owner, bounds) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowOwnerName,
            kCGWindowBounds,
        )
    };
    list.iter()
        .filter_map(|value| {
            let dict = value.downcast::<CFDictionary>().ok()?;
            // SAFETY: Quartz dictionaries have CFString keys and CF object values; all values
            // are downcast before use below.
            let dict = unsafe { dict.cast_unchecked::<CFString, objc2_core_foundation::CFType>() };
            if dict.get(layer)?.downcast::<CFNumber>().ok()?.as_i32()? != 0 {
                return None;
            }
            if dict.get(pid)?.downcast::<CFNumber>().ok()?.as_i64()?
                == i64::from(std::process::id())
            {
                return None;
            }
            let id =
                u64::try_from(dict.get(number)?.downcast::<CFNumber>().ok()?.as_i64()?).ok()?;
            let owner = dict.get(owner)?.downcast::<CFString>().ok()?.to_string();
            let bounds = dict.get(bounds)?.downcast::<CFDictionary>().ok()?;
            let mut frame = CGRect::default();
            // SAFETY: type-checked bounds dictionary and valid writable CGRect storage.
            if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut frame) } {
                return None;
            }
            Some((
                WindowId(id),
                owner,
                RectLogical::new(
                    PointLogical::new(frame.origin.x, frame.origin.y),
                    SizeLogical::new(frame.size.width, frame.size.height),
                ),
            ))
        })
        .collect()
}

#[test]
fn windows_read_only() {
    let event = CGEvent::new(None).expect("unposted event for timestamp probe");
    let timestamp = CGEvent::timestamp(Some(&event));
    // SAFETY: no arguments; reads the public monotonic tick counter without posting input.
    let ticks = unsafe { mach_absolute_time() };
    let now = crosspane_platform_macos::clock::now().as_nanos();
    println!("timestamp probe: CGEvent={timestamp} mach_ticks={ticks} clock_now_ns={now}");
    if timestamp == 0 {
        eprintln!(
            "timestamp probe inconclusive: CGEventCreate(NULL) has no timestamp; these window traits have no at fields"
        );
    }
    // Quartz works without libtest's missing AppKit main loop; never print window titles.
    let quartz = quartz_windows();
    assert!(!quartz.is_empty(), "expected at least one on-screen window");
    for (id, owner, frame) in quartz {
        println!("Quartz id={id:?} owner={owner} frame={frame:?}");
    }
    let windows = MacWindows::new().unwrap();
    let windows = match windows.windows() {
        Ok(windows) => windows,
        Err(PlatformError::Timeout) => {
            eprintln!(
                "skipped: WindowSource AppKit metadata requires the agent's main-thread run loop; Quartz observation passed"
            );
            return;
        }
        Err(error) => panic!("window source: {error}"),
    };
    assert!(
        !windows.is_empty(),
        "expected at least one regular-app window"
    );
    for window in windows {
        println!(
            "id={:?} app_id={} frame={:?}",
            window.id, window.app_id, window.frame
        );
    }
}

struct RestoreOnDrop {
    parking: MacMirrorParking,
    window: WindowId,
    journal: PathBuf,
}

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        match self.parking.restore(self.window) {
            Ok(()) => {
                let _ = std::fs::remove_file(&self.journal);
            }
            Err(error) => eprintln!(
                "restore failed: {error}; journal retained at {}",
                self.journal.display()
            ),
        }
    }
}

#[test]
fn live_textedit_park_and_restore() {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: TextEdit parking requires CROSSPANE_MAC_LIVE=1 in the lead's GUI session"
        );
        return;
    }
    // The lead opens a disposable TextEdit window first. No launching, activation or input here.
    let (window, _, original) = quartz_windows()
        .into_iter()
        .find(|(_, owner, _)| owner == "TextEdit")
        .expect("lead must open a disposable TextEdit window");
    let journal = std::env::temp_dir().join(format!(
        "crosspane-textedit-{}-{}.journal",
        std::process::id(),
        window.0
    ));
    let parking = MacMirrorParking::new(journal.clone()).unwrap();
    let mut cleanup = RestoreOnDrop {
        parking,
        window,
        journal,
    };
    let result = cleanup
        .parking
        .park(window, PixelSize::new(800, 600))
        .unwrap();
    assert_eq!(result.kind, ParkingKind::Mirror);
    assert_eq!(result.content.width(), 800);
    assert_eq!(result.content.height(), 600);
    assert_eq!(cleanup.parking.geometry(window).unwrap(), result);
    cleanup.parking.restore(window).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let (_, _, actual) = quartz_windows()
            .into_iter()
            .find(|(id, _, _)| *id == window)
            .expect("TextEdit window still exists");
        if (actual.origin.x - original.origin.x).abs() <= 2.0
            && (actual.origin.y - original.origin.y).abs() <= 2.0
            && (actual.size.width - original.size.width).abs() <= 2.0
            && (actual.size.height - original.size.height).abs() <= 2.0
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TextEdit did not regain its original frame"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    cleanup.parking.restore(window).unwrap();
}
