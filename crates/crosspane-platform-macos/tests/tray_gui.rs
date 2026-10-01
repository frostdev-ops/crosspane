//! Opt-in status-item test with a real AppKit loop; no input injection or TCC permissions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "macos")]
#[path = "../src/main_thread.rs"]
mod main_thread;

#[cfg(target_os = "macos")]
mod backend {
    pub(super) fn status_item(
        tray: &MacTray,
        _mtm: MainThreadMarker,
    ) -> Option<Retained<NSStatusItem>> {
        HOSTS.with(|hosts| hosts.borrow().get(&tray.key).map(|host| host.item.clone()))
    }

    pub(super) fn host_count(_mtm: MainThreadMarker) -> usize {
        HOSTS.with(|hosts| hosts.borrow().len())
    }

    // Compile the implementation unchanged here so native-state inspection stays test-only.
    include!("../src/tray.rs");
}

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;

    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: tray GUI test requires CROSSPANE_MAC_LIVE=1 in the GUI session");
        return;
    }
    let exit_code = Arc::new(AtomicI32::new(1));
    let worker_exit_code = exit_code.clone();
    let worker = std::thread::spawn(move || {
        if std::panic::catch_unwind(run).is_ok() {
            println!("tray_gui: 1 passed");
            worker_exit_code.store(0, Ordering::Release);
        }
        main_thread::on_main(Duration::from_secs(5), |mtm| {
            let app = NSApplication::sharedApplication(mtm);
            app.stop(None);
            // Wake this test application's event loop so run_app returns after stop.
            let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::ApplicationDefined,
                NSPoint::ZERO,
                NSEventModifierFlags::empty(),
                0.0,
                0,
                None,
                0,
                0,
                0,
            )
            .expect("application-local wake event");
            app.postEvent_atStart(&wake, true);
        })
        .expect("stop AppKit loop");
    });
    main_thread::run_app().expect("AppKit main loop");
    worker.join().expect("GUI worker");
    std::process::exit(exit_code.load(Ordering::Acquire));
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("skipped: tray GUI test requires macOS");
}

#[cfg(target_os = "macos")]
fn run() {
    use std::cell::RefCell;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use backend::MacTray;
    use crosspane_platform::tray::{
        TrayEvent, TrayHost, TrayItem, TrayItemId, TrayMenu, TrayState,
    };
    use main_thread::on_main;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSStatusItem};

    const TIMEOUT: Duration = Duration::from_secs(5);
    thread_local! {
        static ORIGINAL: RefCell<Option<(Retained<NSStatusItem>, Retained<NSMenu>)>> = const { RefCell::new(None) };
    }

    let mut tray = MacTray::new();
    tray = on_main(TIMEOUT, move |mtm| {
        assert!(
            backend::status_item(&tray, mtm).is_none(),
            "new creates no status item"
        );
        assert_eq!(backend::host_count(mtm), 0);
        tray
    })
    .expect("startup and lazy creation");
    let mut menu = TrayMenu {
        state: TrayState::Idle,
        tooltip: "Crosspane tray test".into(),
        items: vec![
            TrayItem::Label("Status label".into()),
            TrayItem::Action {
                id: TrayItemId(34),
                label: "Action".into(),
                enabled: true,
            },
            TrayItem::Action {
                id: TrayItemId(35),
                label: "Disabled".into(),
                enabled: false,
            },
            TrayItem::Toggle {
                id: TrayItemId(36),
                label: "Checked".into(),
                checked: true,
                enabled: true,
            },
            TrayItem::Toggle {
                id: TrayItemId(37),
                label: "Unchecked".into(),
                checked: false,
                enabled: false,
            },
            TrayItem::Separator,
            TrayItem::Submenu {
                label: "Submenu".into(),
                items: vec![TrayItem::Action {
                    id: TrayItemId(u32::MAX),
                    label: "Nested".into(),
                    enabled: true,
                }],
            },
            TrayItem::Submenu {
                label: "Empty submenu".into(),
                items: vec![],
            },
        ],
    };
    let start = Instant::now();
    tray.set(&menu).expect("first menu");
    println!("set returned in {:?}", start.elapsed());
    let (next_tray, idle_image) = on_main(TIMEOUT, move |mtm| {
        let item = backend::status_item(&tray, mtm).expect("status item exists");
        assert!(item.isVisible() && item.statusBar().is_some());
        assert_eq!(backend::host_count(mtm), 1);
        let button = item.button(mtm).expect("status button");
        assert_eq!(
            button.toolTip().expect("tooltip").to_string(),
            "Crosspane tray test"
        );
        let native = item.menu(mtm).expect("menu");
        assert!(!native.autoenablesItems());
        let items = native.itemArray();
        assert_eq!(items.len(), 8);
        for (index, title, enabled) in [
            (0, "Status label", false),
            (1, "Action", true),
            (2, "Disabled", false),
            (3, "Checked", true),
            (4, "Unchecked", false),
            (6, "Submenu", true),
            (7, "Empty submenu", false),
        ] {
            let item = items.objectAtIndex(index);
            assert_eq!(item.title().to_string(), title);
            assert_eq!(item.isEnabled(), enabled);
            assert!(item.target().is_some());
        }
        assert_eq!(items.objectAtIndex(1).tag(), 34);
        assert_eq!(items.objectAtIndex(2).tag(), 35);
        assert_eq!(items.objectAtIndex(3).tag(), 36);
        assert_eq!(items.objectAtIndex(3).state(), NSControlStateValueOn);
        assert_eq!(items.objectAtIndex(4).state(), NSControlStateValueOff);
        assert!(items.objectAtIndex(5).isSeparatorItem());
        let submenu = items.objectAtIndex(6).submenu().expect("submenu");
        assert!(!submenu.autoenablesItems());
        assert_eq!(submenu.numberOfItems(), 1);
        let nested = submenu.itemAtIndex(0).expect("nested action");
        assert_eq!(nested.title().to_string(), "Nested");
        assert!(nested.isEnabled());
        assert_eq!(nested.tag(), u32::MAX as isize);
        let empty = items
            .objectAtIndex(7)
            .submenu()
            .expect("empty submenu attached");
        assert!(!empty.autoenablesItems());
        assert_eq!(empty.numberOfItems(), 0);
        // A choice before subscribe is dropped and must not be replayed.
        native.performActionForItemAtIndex(1);
        let image = check_icon(&button, "rectangle.on.rectangle");
        ORIGINAL.with(|original| *original.borrow_mut() = Some((item, native)));
        println!("initial menu: titles, enabled flags, toggles, separator and submenus verified");
        (tray, image)
    })
    .expect("inspect initial menu");
    tray = next_tray;

    let (tx, rx) = mpsc::channel();
    tray.subscribe(Arc::new(move |event| {
        tx.send(event).expect("tray event receiver")
    }))
    .expect("subscribe");
    assert!(
        tray.subscribe(Arc::new(|_| {})).is_err(),
        "subscribe is called once"
    );
    assert!(rx.try_recv().is_err(), "no replay before subscribe");
    tray = on_main(TIMEOUT, move |mtm| {
        let item = backend::status_item(&tray, mtm).expect("status item");
        let native = item.menu(mtm).expect("menu");
        native.performActionForItemAtIndex(1);
        native.performActionForItemAtIndex(2);
        native.performActionForItemAtIndex(3);
        native.performActionForItemAtIndex(4);
        native.performActionForItemAtIndex(0);
        native
            .itemAtIndex(6)
            .expect("submenu item")
            .submenu()
            .expect("submenu")
            .performActionForItemAtIndex(0);
        tray
    })
    .expect("perform menu actions");
    for id in [34, 36, u32::MAX] {
        assert_eq!(
            rx.recv_timeout(TIMEOUT).expect("chosen event"),
            TrayEvent::Chosen(TrayItemId(id))
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "disabled items and labels never report"
    );
    println!("choices: action=34, toggle=36, nested=4294967295; disabled items silent");

    menu.state = TrayState::Active;
    menu.tooltip = "Crosspane replacement".into();
    menu.items = vec![TrayItem::Action {
        id: TrayItemId(38),
        label: "Replacement".into(),
        enabled: true,
    }];
    tray.set(&menu).expect("replace menu");
    let (next_tray, active_image) = on_main(TIMEOUT, move |mtm| {
        let item = backend::status_item(&tray, mtm).expect("same status item");
        let native = item.menu(mtm).expect("replacement menu");
        ORIGINAL.with(|original| {
            let original = original.borrow();
            let (old_item, old_menu) = original.as_ref().expect("original item and menu");
            assert_eq!(&item, old_item, "set keeps the status item");
            assert_ne!(&native, old_menu, "set replaces the menu");
            old_menu.performActionForItemAtIndex(1);
        });
        assert_eq!(native.numberOfItems(), 1);
        let action = native.itemAtIndex(0).expect("replacement action");
        assert_eq!(action.title().to_string(), "Replacement");
        assert_eq!(action.tag(), 38);
        assert!(action.isEnabled() && !native.autoenablesItems());
        native.performActionForItemAtIndex(0);
        let button = item.button(mtm).expect("button");
        assert_eq!(
            button.toolTip().expect("updated tooltip").to_string(),
            "Crosspane replacement"
        );
        (
            tray,
            check_icon(&button, "rectangle.fill.on.rectangle.fill"),
        )
    })
    .expect("inspect replacement");
    tray = next_tray;
    if let (Some(idle), Some(active)) = (&idle_image, &active_image) {
        assert_ne!(idle, active, "state changes the rendered image");
    }
    for id in [34, 38] {
        assert_eq!(
            rx.recv_timeout(TIMEOUT).expect("replacement choice"),
            TrayEvent::Chosen(TrayItemId(id))
        );
    }
    assert!(rx.try_recv().is_err());
    println!(
        "replacement: same status item, new menu and changed state image; old id delivered once"
    );

    for (state, symbol) in [
        (TrayState::Attention, "exclamationmark.triangle"),
        (TrayState::Offline, "rectangle.on.rectangle.slash"),
    ] {
        menu.state = state;
        tray.set(&menu).expect("change state");
        tray = on_main(TIMEOUT, move |mtm| {
            let item = backend::status_item(&tray, mtm).expect("status item");
            check_icon(&item.button(mtm).expect("button"), symbol);
            tray
        })
        .expect("inspect state icon");
    }

    drop(tray);
    on_main(TIMEOUT, |mtm| {
        assert_eq!(backend::host_count(mtm), 0, "drop removes registry entry");
        ORIGINAL.with(|original| {
            let (item, old_menu) = original.borrow_mut().take().expect("retained original");
            // AppKit keeps statusBar set on a retained item after removeStatusItem. Inspect
            // its actual presentation instead: the button is detached or its window hidden.
            let visible = item
                .button(mtm)
                .and_then(|button| button.window())
                .is_some_and(|window| window.isVisible());
            assert!(!visible, "removed status button is no longer presented");
            old_menu.performActionForItemAtIndex(1);
        });
    })
    .expect("verify removal");
    assert!(rx.try_recv().is_err(), "no events after drop");
    println!("drop: status item removed from NSStatusBar; old menu silent");
}

#[cfg(target_os = "macos")]
fn check_icon(button: &objc2_app_kit::NSStatusBarButton, symbol: &str) -> Option<Vec<u8>> {
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSString;

    let expected = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        None,
    );
    if let Some(expected) = expected {
        let image = button.image().expect("SF Symbol image");
        assert!(image.isTemplate());
        assert!(button.title().is_empty());
        let bytes = image.TIFFRepresentation().expect("rendered image").to_vec();
        assert_eq!(
            bytes,
            expected
                .TIFFRepresentation()
                .expect("expected symbol image")
                .to_vec()
        );
        println!("symbol {symbol}: template image, no fallback");
        Some(bytes)
    } else {
        assert!(button.image().is_none());
        assert_eq!(button.title().to_string(), "⧉");
        println!("symbol {symbol}: missing, fell back to ⧉");
        None
    }
}
