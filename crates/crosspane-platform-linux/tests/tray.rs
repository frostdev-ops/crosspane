//! Lead-run round trip against a live StatusNotifierWatcher and D-Bus menu.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::tray::{TrayEvent, TrayHost, TrayItem, TrayItemId, TrayMenu};
use crosspane_platform_linux::tray::SniTray;
use zbus::blocking::{Proxy, connection::Builder};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the SNI service"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn live_registration_menu_choice_and_drop() {
    if std::env::var("CROSSPANE_LIVE_SNI").as_deref() != Ok("1") {
        eprintln!("skipped: live SNI test requires CROSSPANE_LIVE_SNI=1");
        return;
    }
    let connection = Builder::session()
        .unwrap()
        .method_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    // Disable caching so removal is observed from the watcher, not a stale proxy.
    let watcher = zbus::blocking::proxy::Builder::<Proxy<'_>>::new(&connection)
        .destination("org.kde.StatusNotifierWatcher")
        .unwrap()
        .path("/StatusNotifierWatcher")
        .unwrap()
        .interface("org.kde.StatusNotifierWatcher")
        .unwrap()
        .cache_properties(CacheProperties::No)
        .build()
        .unwrap();
    let registered = || {
        watcher
            .get_property::<Vec<String>>("RegisteredStatusNotifierItems")
            .unwrap()
    };
    let before = registered();
    let mut tray = SniTray::new().unwrap();
    assert_eq!(registered(), before, "new must not register an icon");
    let (send, events) = mpsc::channel();
    tray.subscribe(Arc::new(move |event| send.send(event).unwrap()))
        .unwrap();
    let action_id = TrayItemId(u32::MAX);
    tray.set(&TrayMenu {
        tooltip: "Crosspane tray test".into(),
        items: vec![TrayItem::Action {
            id: action_id,
            label: "WP-1.34a action".into(),
            enabled: true,
        }],
        ..Default::default()
    })
    .unwrap();

    let mut item = None;
    wait_until(|| {
        item = registered()
            .into_iter()
            .find(|entry| !before.contains(entry));
        item.is_some()
    });
    let item = item.unwrap();
    let (destination, path) = match item.split_once('/') {
        Some((destination, path)) => (destination.to_owned(), format!("/{path}")),
        None => (item.clone(), "/StatusNotifierItem".into()),
    };
    let sni = Proxy::new(
        &connection,
        destination.clone(),
        path,
        "org.kde.StatusNotifierItem",
    )
    .unwrap();
    assert_eq!(sni.get_property::<String>("Id").unwrap(), "crosspane");
    let menu_path = sni.get_property::<OwnedObjectPath>("Menu").unwrap();
    let menu = Proxy::new(
        &connection,
        destination,
        menu_path,
        "com.canonical.dbusmenu",
    )
    .unwrap();
    // ksni owns the D-Bus ids. Read the wire id from GetLayout; the callback
    // retains the agent's independent u32 TrayItemId (including u32::MAX).
    let (_, layout): (u32, Layout) = menu
        .call("GetLayout", &(0_i32, 1_i32, vec!["label"]))
        .unwrap();
    let action: Layout = layout
        .2
        .into_iter()
        .map(|child| Layout::try_from(child).unwrap())
        .find(|child| {
            child
                .1
                .get("label")
                .and_then(|label| <&str>::try_from(label).ok())
                == Some("WP-1.34a action")
        })
        .unwrap();
    let (): () = menu
        .call("Event", &(action.0, "clicked", Value::from(0_i32), 0_u32))
        .unwrap();
    assert_eq!(
        events.recv_timeout(Duration::from_secs(2)).unwrap(),
        TrayEvent::Chosen(action_id)
    );
    assert!(events.try_recv().is_err());
    drop(tray);
    wait_until(|| !registered().contains(&item));
}
