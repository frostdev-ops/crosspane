//! Owner-attended only. Every case uses a unique private pasteboard; never generalPasteboard.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "macos")]
fn main() {
    if std::env::var("CROSSPANE_MAC_GUI").as_deref() != Ok("1") {
        eprintln!("clipboard_gui requires the lead's attended private-board run");
        return;
    }
    std::thread::spawn(|| {
        let result = std::panic::catch_unwind(|| {
            crosspane_platform_macos::main_thread::on_main(
                std::time::Duration::from_secs(10),
                |_| (),
            )
            .expect("AppKit main-loop startup barrier");
            cases::run();
        });
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    });
    crosspane_platform_macos::main_thread::run_app().expect("AppKit main loop");
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod cases {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use block2::RcBlock;
    use crosspane_platform::{ClipKinds, ClipboardEvent, ClipboardHost, IoGate, PlatformError};
    use crosspane_platform_macos::clipboard::{MacClipboard, PasteboardName};
    use crosspane_platform_macos::main_thread::on_main;
    use crosspane_types::ClipKind;
    use objc2_app_kit::NSPasteboard;
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use objc2_foundation::NSString;

    const TEXT: &str = "public.utf8-plain-text";
    const PNG: &str = "public.png";
    const WAIT: Duration = Duration::from_secs(5);
    type Host = Arc<Mutex<MacClipboard>>;
    type Events = mpsc::Receiver<ClipboardEvent>;

    fn private_name(case: &str) -> String {
        format!(
            "io.frostdev.crosspane.c3b.test.{case}.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn host(name: &str) -> (Host, Arc<IoGate>, Events) {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        gate.set_session_permits(true);
        let mut host =
            MacClipboard::new(Arc::clone(&gate), PasteboardName::Private(name.into())).unwrap();
        let (tx, rx) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            ClipboardEvent::Changed { .. }
        ));
        (Arc::new(Mutex::new(host)), gate, rx)
    }

    fn changed(events: &Events, expected: ClipKinds) {
        let until = Instant::now() + WAIT;
        loop {
            let event = events
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .unwrap();
            if let ClipboardEvent::Changed { kinds } = event {
                assert_eq!(kinds, expected);
                return;
            }
        }
    }

    fn clear(name: String) {
        on_main(WAIT, move |_| {
            NSPasteboard::pasteboardWithName(&NSString::from_str(&name)).clearContents();
        })
        .unwrap();
    }

    // Public default-mode work can run inside a provider's nested wait; main dispatch cannot.
    fn during_wait(f: impl FnOnce() + Send + 'static) {
        let task = Mutex::new(Some(f));
        let block = RcBlock::new(move || {
            if let Some(task) = task.lock().unwrap().take() {
                on_main(WAIT, move |_| task()).unwrap();
            }
        });
        let run_loop = CFRunLoop::main().unwrap();
        // SAFETY: Public main run loop and default mode. Core Foundation copies the block;
        // its Send Rust closure reaches every AppKit operation through on_main.
        unsafe { run_loop.perform_block(kCFRunLoopDefaultMode.map(|m| m.as_ref()), Some(&block)) };
        run_loop.wake_up();
    }

    fn promise_metadata_and_cross_thread_fulfil() {
        let name = private_name("roundtrip");
        let (a, _, events_a) = host(&name);
        let (b, _, events_b) = host(&name);
        let text = ClipKinds {
            text: true,
            image: false,
        };
        a.lock().unwrap().promise(1, text).unwrap();
        changed(&events_b, text);
        assert!(
            events_a.try_recv().is_err(),
            "A's own write must not echo Changed"
        );
        let responder = Arc::clone(&a);
        let response = std::thread::spawn(move || {
            let event = events_a.recv_timeout(WAIT).unwrap();
            let ClipboardEvent::PasteRequested {
                paste,
                offer: 1,
                kind: ClipKind::Text,
            } = event
            else {
                panic!("expected one text paste request");
            };
            responder
                .lock()
                .unwrap()
                .fulfil(paste, Some(b"C3b private test fixture\r\n".to_vec()));
            events_a
        });
        let bytes = on_main(WAIT, move |_| b.lock().unwrap().read(ClipKind::Text, 1024))
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"C3b private test fixture\n");
        let events_a = response.join().unwrap();
        assert!(events_a.try_recv().is_err());
        let count_name = name.clone();
        let before = on_main(WAIT, move |_| {
            NSPasteboard::pasteboardWithName(&NSString::from_str(&count_name)).changeCount()
        })
        .unwrap();
        // Withdrawal stops future transfer; already supplied data need not be recalled.
        a.lock().unwrap().withdraw(1).unwrap();
        let check_name = name.clone();
        on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&check_name));
            assert_eq!(board.changeCount(), before);
            assert!(
                board
                    .dataForType(&NSString::from_str(TEXT))
                    .is_none_or(|bytes| {
                        bytes.is_empty() || bytes.to_vec() == b"C3b private test fixture\r\n"
                    })
            );
        })
        .unwrap();
        clear(name);
        drop(a);
        println!("promise_metadata_and_cross_thread_fulfil: passed");
    }

    fn expiry_pumps_default_run_loop_and_reentrant_request_answers_empty() {
        let name = private_name("expiry");
        let (a, _, events) = host(&name);
        a.lock()
            .unwrap()
            .promise(
                2,
                ClipKinds {
                    text: true,
                    image: true,
                },
            )
            .unwrap();
        let progress = Arc::new(AtomicUsize::new(0));
        let progressed = Arc::clone(&progress);
        let reading = Arc::new(AtomicBool::new(false));
        let inside = Arc::clone(&reading);
        let reentrant_empty = Arc::new(AtomicBool::new(false));
        let reentered = Arc::clone(&reentrant_empty);
        let reentrant_name = name.clone();
        let scheduler = std::thread::spawn(move || {
            assert!(matches!(
                events.recv_timeout(WAIT).unwrap(),
                ClipboardEvent::PasteRequested {
                    offer: 2,
                    kind: ClipKind::Text,
                    ..
                }
            ));
            during_wait(move || {
                let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&reentrant_name));
                let bytes = board.dataForType(&NSString::from_str(PNG));
                reentered.store(bytes.is_none_or(|bytes| bytes.is_empty()), Ordering::SeqCst);
                if inside.load(Ordering::SeqCst) {
                    progressed.fetch_add(1, Ordering::SeqCst);
                }
            });
            events
        });
        let read_name = name.clone();
        let elapsed = on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&read_name));
            let started = Instant::now();
            reading.store(true, Ordering::SeqCst);
            let bytes = board.dataForType(&NSString::from_str(TEXT));
            reading.store(false, Ordering::SeqCst);
            assert!(bytes.is_none_or(|bytes| bytes.is_empty()));
            started.elapsed()
        })
        .unwrap();
        let events = scheduler.join().unwrap();
        assert!(elapsed >= Duration::from_millis(2400) && elapsed < Duration::from_millis(3000));
        assert_eq!(
            progress.load(Ordering::SeqCst),
            1,
            "default-mode work ran during the wait"
        );
        assert!(reentrant_empty.load(Ordering::SeqCst));
        assert!(
            events.try_recv().is_err(),
            "reentrancy must not allocate another paste"
        );
        clear(name);
        drop(a);
        println!(
            "expiry_pumps_default_run_loop_and_reentrant_request_answers_empty: passed ({elapsed:?})"
        );
    }

    fn external_write_loses_promise_once_and_closed_gate_is_locked() {
        let name = private_name("lost");
        let (a, gate, events) = host(&name);
        a.lock()
            .unwrap()
            .promise(
                3,
                ClipKinds {
                    text: true,
                    image: false,
                },
            )
            .unwrap();
        let write_name = name.clone();
        on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&write_name));
            board.clearContents();
            assert!(board.setString_forType(
                &NSString::from_str("C3b external private fixture"),
                &NSString::from_str(TEXT)
            ));
        })
        .unwrap();
        assert_eq!(
            events.recv_timeout(WAIT).unwrap(),
            ClipboardEvent::PromiseLost { offer: 3 }
        );
        changed(
            &events,
            ClipKinds {
                text: true,
                image: false,
            },
        );
        assert!(
            events.recv_timeout(Duration::from_millis(600)).is_err(),
            "lost once only"
        );
        gate.set_engine_permits(false);
        assert!(matches!(
            a.lock().unwrap().read(ClipKind::Text, 1024),
            Err(PlatformError::Locked)
        ));
        assert!(matches!(
            a.lock().unwrap().promise(
                4,
                ClipKinds {
                    text: true,
                    image: false
                }
            ),
            Err(PlatformError::Locked)
        ));
        clear(name);
        drop(a);
        println!("external_write_loses_promise_once_and_closed_gate_is_locked: passed");
    }

    fn withdraw_only_matching_owner_and_drop_keeps_native_types() {
        let name = private_name("withdraw");
        let (a, _, _) = host(&name);
        a.lock()
            .unwrap()
            .promise(
                5,
                ClipKinds {
                    text: true,
                    image: false,
                },
            )
            .unwrap();
        a.lock().unwrap().withdraw(999).unwrap();
        assert!(a.lock().unwrap().kinds().unwrap().text);
        a.lock().unwrap().withdraw(5).unwrap();
        let check_name = name.clone();
        on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&check_name));
            assert!(
                board
                    .types()
                    .unwrap()
                    .iter()
                    .any(|value| value.to_string() == TEXT)
            );
            assert!(
                board
                    .dataForType(&NSString::from_str(TEXT))
                    .is_none_or(|bytes| bytes.is_empty())
            );
        })
        .unwrap();
        a.lock()
            .unwrap()
            .promise(
                6,
                ClipKinds {
                    text: true,
                    image: false,
                },
            )
            .unwrap();
        drop(a);
        let check_name = name.clone();
        on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&check_name));
            assert!(
                board
                    .types()
                    .unwrap()
                    .iter()
                    .any(|value| value.to_string() == TEXT)
            );
            assert!(
                board
                    .dataForType(&NSString::from_str(TEXT))
                    .is_none_or(|bytes| bytes.is_empty())
            );
        })
        .unwrap();
        clear(name);
        println!("withdraw_only_matching_owner_and_drop_keeps_native_types: passed");
    }

    fn withdraw_cancels_pending_and_late_fulfil_is_ignored() {
        let name = private_name("pending-withdraw");
        let (a, _, events) = host(&name);
        a.lock()
            .unwrap()
            .promise(
                7,
                ClipKinds {
                    text: true,
                    image: false,
                },
            )
            .unwrap();
        let responder = Arc::clone(&a);
        let cancel = std::thread::spawn(move || {
            let ClipboardEvent::PasteRequested {
                paste, offer: 7, ..
            } = events.recv_timeout(WAIT).unwrap()
            else {
                panic!("expected paste before cancellation");
            };
            responder.lock().unwrap().withdraw(7).unwrap();
            responder
                .lock()
                .unwrap()
                .fulfil(paste, Some(b"C3b cancelled private fixture".to_vec()));
        });
        let read_name = name.clone();
        let elapsed = on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&read_name));
            let started = Instant::now();
            let bytes = board.dataForType(&NSString::from_str(TEXT));
            assert!(bytes.is_none_or(|bytes| bytes.is_empty()));
            started.elapsed()
        })
        .unwrap();
        cancel.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(1),
            "withdraw must wake a pending paste"
        );
        clear(name);
        drop(a);
        println!("withdraw_cancels_pending_and_late_fulfil_is_ignored: passed");
    }

    fn copy_after_validation_survives_withdrawal() {
        let name = private_name("copy-after-validation");
        let (a, _, _) = host(&name);
        a.lock()
            .unwrap()
            .promise(
                8,
                ClipKinds {
                    text: true,
                    image: false,
                },
            )
            .unwrap();
        let check_name = name.clone();
        let validated = on_main(WAIT, move |_| {
            NSPasteboard::pasteboardWithName(&NSString::from_str(&check_name)).changeCount()
        })
        .unwrap();
        let copy_name = name.clone();
        let newer = on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&copy_name));
            let newer = board.clearContents();
            assert!(board.setString_forType(
                &NSString::from_str("C3b newer local fixture"),
                &NSString::from_str(TEXT)
            ));
            newer
        })
        .unwrap();
        assert_ne!(validated, newer);
        a.lock().unwrap().withdraw(8).unwrap();
        let check_name = name.clone();
        on_main(WAIT, move |_| {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&check_name));
            assert_eq!(board.changeCount(), newer);
            assert_eq!(
                board
                    .stringForType(&NSString::from_str(TEXT))
                    .unwrap()
                    .to_string(),
                "C3b newer local fixture"
            );
        })
        .unwrap();
        clear(name);
        drop(a);
        println!("copy_after_validation_survives_withdrawal: passed");
    }

    pub(super) fn run() {
        promise_metadata_and_cross_thread_fulfil();
        expiry_pumps_default_run_loop_and_reentrant_request_answers_empty();
        external_write_loses_promise_once_and_closed_gate_is_locked();
        withdraw_only_matching_owner_and_drop_keeps_native_types();
        withdraw_cancels_pending_and_late_fulfil_is_ignored();
        copy_after_validation_survives_withdrawal();
        println!("clipboard_gui: 6 passed; private boards only; provider/UI timing measured");
    }
}
