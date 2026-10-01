//! Harmless panel test with a real AppKit main loop; never injects or captures input.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "macos")]
fn main() {
    if std::env::var("CROSSPANE_MAC_GUI").as_deref() != Ok("1") {
        eprintln!("skipped: overlay GUI test requires CROSSPANE_MAC_GUI=1 in the GUI session");
        return;
    }
    std::thread::spawn(|| {
        let result = std::panic::catch_unwind(run);
        match result {
            Ok(()) => {
                println!("overlay_gui: 1 passed");
                std::process::exit(0);
            }
            Err(_) => std::process::exit(1),
        }
    });
    crosspane_platform_macos::main_thread::run_app().expect("AppKit main loop");
    panic!("AppKit main loop returned unexpectedly");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("skipped: overlay GUI test requires macOS");
}

#[cfg(target_os = "macos")]
fn run() {
    use std::ptr::NonNull;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use crosspane_platform::{Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, Rgb8};
    use crosspane_platform_macos::main_thread::on_main;
    use crosspane_platform_macos::overlay::MacOverlay;
    use crosspane_types::id::DisplayId;
    use objc2_app_kit::{
        NSApplication, NSScreen, NSWindowDidChangeOcclusionStateNotification,
        NSWindowOcclusionState,
    };
    use objc2_foundation::{
        NSNotification, NSNotificationCenter, NSNumber, NSOperationQueue, ns_string,
    };

    // Allow startup of run_app before timing the bounded OverlayHost calls.
    let display = on_main(Duration::from_secs(2), |mtm| {
        let screen = NSScreen::screens(mtm)
            .firstObject()
            .expect("connected display");
        let number = screen
            .deviceDescription()
            .objectForKey(ns_string!("NSScreenNumber"))
            .expect("NSScreenNumber")
            .downcast::<NSNumber>()
            .expect("display ID is NSNumber");
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        let block = RcBlock::new(|_: NonNull<NSNotification>| {
            on_main(Duration::from_millis(50), |mtm| {
                for window in NSApplication::sharedApplication(mtm).windows() {
                    println!(
                        "occlusion notification: window={}, isVisible={}, occlusionState={:#x}",
                        window.windowNumber(),
                        window.isVisible(),
                        window.occlusionState().bits()
                    );
                }
            })
            .expect("observe occlusion on main thread");
        });
        // SAFETY: immutable AppKit notification name; Send block ignores its pointer argument,
        // and is delivered on the main queue. The center owns it until this test process exits.
        unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidChangeOcclusionStateNotification),
                None,
                Some(&queue),
                &block,
            )
        };
        DisplayId(number.unsignedIntValue())
    })
    .expect("read display on main thread");

    let mut host = MacOverlay::new().expect("overlay host");
    let (tx, rx) = mpsc::channel();
    host.subscribe(Arc::new(move |event| {
        tx.send(event).expect("overlay event receiver");
    }))
    .expect("subscribe");
    let id = OverlayId(24);
    let mut overlay = Overlay {
        display,
        anchor: OverlayAnchor::TopCenter,
        text: "Crosspane overlay test".into(),
        accent: Rgb8 {
            r: 59,
            g: 130,
            b: 246,
        },
    };
    let start = Instant::now();
    host.show(id, &overlay).expect("show");
    println!("show returned in {:?}", start.elapsed());
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(500))
            .expect("Visible within 500 ms"),
        OverlayEvent::Visible(id)
    );
    println!("show Visible received in {:?}", start.elapsed());
    let window_number = on_main(Duration::from_millis(50), |mtm| {
        let panels: Vec<_> = NSApplication::sharedApplication(mtm)
            .windows()
            .into_iter()
            .filter(|w| w.isVisible())
            .collect();
        assert_eq!(panels.len(), 1);
        let panel = &panels[0];
        println!(
            "presented: window={}, isVisible={}, occlusionState={:#x}, key={}, main={}",
            panel.windowNumber(),
            panel.isVisible(),
            panel.occlusionState().bits(),
            panel.isKeyWindow(),
            panel.isMainWindow()
        );
        assert!(
            panel
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible)
        );
        assert!(!panel.canBecomeKeyWindow() && !panel.canBecomeMainWindow());
        assert!(!panel.isKeyWindow() && !panel.isMainWindow());
        assert!(panel.ignoresMouseEvents());
        panel.windowNumber()
    })
    .expect("inspect presented panel");

    overlay.text = "Crosspane overlay updated".into();
    let start = Instant::now();
    host.show(id, &overlay).expect("update text");
    println!("update returned in {:?}", start.elapsed());
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(500))
            .expect("updated Visible within 500 ms"),
        OverlayEvent::Visible(id)
    );
    println!("update Visible received in {:?}", start.elapsed());
    on_main(Duration::from_millis(50), move |mtm| {
        let panels: Vec<_> = NSApplication::sharedApplication(mtm)
            .windows()
            .into_iter()
            .filter(|w| w.isVisible())
            .collect();
        assert_eq!(panels.len(), 1);
        let panel = &panels[0];
        assert_eq!(
            panel.windowNumber(),
            window_number,
            "update keeps the panel"
        );
        println!(
            "updated: window={}, isVisible={}, occlusionState={:#x}",
            panel.windowNumber(),
            panel.isVisible(),
            panel.occlusionState().bits()
        );
        assert!(
            panel
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible)
        );
        let content = panel
            .contentView()
            .expect("updated content")
            .downcast::<objc2_app_kit::NSBox>()
            .expect("rounded background");
        assert_eq!(content.cornerRadius(), 10.0);
        assert!(content.wantsLayer() && content.layer().is_some());
        assert!(panel.frame().size.width > 0.0 && panel.frame().size.height > 0.0);
        let label = content
            .contentView()
            .expect("box content")
            .subviews()
            .into_iter()
            .find_map(|view| view.downcast::<objc2_app_kit::NSTextField>().ok())
            .expect("text label");
        assert_eq!(label.stringValue().to_string(), "Crosspane overlay updated");
        assert!(!label.isEditable() && !label.isSelectable());
        assert_eq!(label.font().expect("system font").pointSize(), 14.0);
    })
    .expect("inspect updated panel");

    let start = Instant::now();
    host.hide(id).expect("hide");
    println!("hide returned in {:?}", start.elapsed());
    on_main(Duration::from_millis(50), |mtm| {
        assert!(
            NSApplication::sharedApplication(mtm)
                .windows()
                .into_iter()
                .all(|w| {
                    println!(
                        "hidden: window={}, isVisible={}, occlusionState={:#x}",
                        w.windowNumber(),
                        w.isVisible(),
                        w.occlusionState().bits()
                    );
                    !w.isVisible()
                })
        );
    })
    .expect("verify hidden");
    drop(host);
    on_main(Duration::from_millis(50), |_| {}).expect("drain cleanup");
}
