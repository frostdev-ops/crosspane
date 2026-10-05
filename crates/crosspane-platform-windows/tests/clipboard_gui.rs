//! Native clipboard body is ignored by elevated cargo; fake seams perform no Win32 calls.
#![cfg(windows)]
#![allow(
    dead_code,
    unused_imports,
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used
)]
pub use crosspane_platform_windows::model;
#[path = "../src/clipboard.rs"]
mod clipboard;

use crosspane_platform::{ClipboardEvent, ClipboardHost, IoGate, PlatformError};
use crosspane_types::ClipKind;
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{
        DataExchange::*,
        LibraryLoader::GetModuleHandleW,
        Memory::*,
        Ole::{CF_DIBV5, CF_UNICODETEXT},
        Threading::{GetCurrentProcess, GetCurrentThreadId, OpenProcessToken},
    },
    UI::WindowsAndMessaging::*,
};

struct Watchdog {
    done: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new() -> Self {
        let (done, waiting) = mpsc::channel();
        let worker = thread::spawn(move || {
            if waiting.recv_timeout(Duration::from_secs(15)).is_err() {
                eprintln!(
                    "CLIP_PROBE watchdog expired; own-process exit, clipboard cleanup unverified"
                );
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
fn limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut length = 0;
    // SAFETY: reads only this process token, exact initialized buffer, always closes token.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(ok, 0);
    }
    assert_eq!(elevation.TokenIsElevated, 0, "Limited win-gui required");
}
struct Fixture {
    window: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    sequence: Option<u32>,
    cleaned: Arc<AtomicBool>,
}
impl Fixture {
    fn new() -> Self {
        // SAFETY: own process module and thread only.
        let (instance, id) = unsafe { (GetModuleHandleW(null()), GetCurrentThreadId()) };
        assert!(!instance.is_null());
        let class: Vec<u16> = format!("CrosspaneClipFixture{}-{id}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(DefWindowProcW),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: owned never-shown message-only fixture; no activation or owner-window fields.
        let window = unsafe {
            assert_ne!(RegisterClassW(&wc), 0);
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                null(),
            )
        };
        assert!(!window.is_null());
        Self {
            window,
            class,
            instance,
            sequence: None,
            cleaned: Arc::new(AtomicBool::new(false)),
        }
    }
    fn open(&self) -> Option<FixtureOpen> {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            // SAFETY: own fixture HWND, no data read; every success has RAII close on this thread.
            if unsafe { OpenClipboard(self.window) } != 0 {
                return Some(FixtureOpen);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }
    fn admitted(&self, initial: bool) -> bool {
        // SAFETY: called only while clipboard is open; exact live fixture identity is checked
        // without querying any foreign window field. Zero formats is distinguished from failure.
        unsafe {
            let mut pid = 0;
            if IsWindow(self.window) == 0
                || GetWindowThreadProcessId(self.window, &mut pid) == 0
                || pid != std::process::id()
            {
                return false;
            }
            let sequence = GetClipboardSequenceNumber();
            if sequence == 0 {
                return false;
            }
            if let Some(expected) = self.sequence {
                return GetClipboardOwner() == self.window && sequence == expected;
            }
            if let Some((owner, expected)) = clipboard::probe_fixture() {
                let mut owner_pid = 0;
                return expected == sequence
                    && owner != 0
                    && GetClipboardOwner() as usize == owner
                    && IsWindow(owner as HWND) != 0
                    && GetWindowThreadProcessId(owner as HWND, &mut owner_pid) != 0
                    && owner_pid == std::process::id();
            }
            if !initial {
                return false;
            }
            SetLastError(0);
            let count = CountClipboardFormats();
            count == 0 && GetLastError() == 0 && sequence == GetClipboardSequenceNumber()
        }
    }
    fn paste(&self, format: u32) -> Option<Option<Vec<u8>>> {
        let _opened = self.open()?;
        if !self.admitted(false) {
            return None;
        }
        // SAFETY: exact authenticated fixture/observer owner and sequence while OpenClipboard
        // is held; no foreign content call. Only our promised test bytes can be rendered.
        let handle = unsafe { GetClipboardData(format) } as HGLOBAL;
        if !self.admitted(false) {
            return None;
        }
        if handle.is_null() {
            return Some(None);
        }
        // SAFETY: supported test HGLOBAL belongs to OS, borrowed only while open. Exact bounded
        // GlobalSize is checked; no raw pointer escapes and no foreign/window content is read.
        unsafe {
            let size = GlobalSize(handle);
            assert!(size > 0 && size < 1024 * 1024);
            let pointer = GlobalLock(handle);
            assert!(!pointer.is_null());
            let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
            SetLastError(0);
            assert!(GlobalUnlock(handle) != 0 || GetLastError() == 0);
            Some(Some(bytes))
        }
    }
    fn write(&mut self, format: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        let _opened = self.open()?;
        if !self.admitted(self.sequence.is_none()) {
            return None;
        }
        let mut block = Block::new(bytes);
        // SAFETY: admission was checked with clipboard open; Empty destroys only empty contents
        // or our exact still-current fixture. Our allocated handle transfers only on success.
        unsafe {
            assert_ne!(EmptyClipboard(), 0);
            assert_eq!(GetClipboardOwner(), self.window);
            let sequence = GetClipboardSequenceNumber();
            self.sequence = Some(sequence);
            if SetClipboardData(format, block.memory).is_null() {
                return None;
            }
            block.transferred = true;
            let sequence = GetClipboardSequenceNumber();
            assert!(sequence != 0 && GetClipboardOwner() == self.window);
            self.sequence = Some(sequence);
            clipboard::set_probe_fixture(self.window as usize, sequence);
        }
        Some(std::mem::take(&mut block.expected))
    }
    fn clear(&mut self) -> bool {
        if self.sequence.is_none() {
            return true;
        }
        let Some(_opened) = self.open() else {
            return false;
        };
        if !self.admitted(false) {
            return false;
        }
        // SAFETY: exact fixture owner and sequence still admitted while open; never clears a
        // newer/foreign owner. Only this probe's known eager fixture is discarded.
        unsafe {
            if EmptyClipboard() == 0 {
                return false;
            }
            SetLastError(0);
            if CountClipboardFormats() != 0 || GetLastError() != 0 {
                return false;
            }
        }
        self.sequence = None;
        true
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let cleared = self.clear();
        // SAFETY: only the own fixture window/class, on its creating thread. Never owner HWNDs.
        let (destroyed, unregistered) = unsafe {
            (
                DestroyWindow(self.window) != 0,
                UnregisterClassW(self.class.as_ptr(), self.instance) != 0,
            )
        };
        self.cleaned
            .store(cleared && destroyed && unregistered, Ordering::Release);
        if !(cleared && destroyed && unregistered) {
            eprintln!("CLIP_PROBE fixture cleanup unverified; replacement left untouched");
        }
    }
}
struct FixtureOpen;
impl Drop for FixtureOpen {
    fn drop(&mut self) {
        // SAFETY: balances this same thread's successful fixture OpenClipboard.
        assert_ne!(unsafe { CloseClipboard() }, 0);
    }
}
struct Block {
    memory: HGLOBAL,
    transferred: bool,
    expected: Vec<u8>,
}
impl Block {
    fn new(bytes: &[u8]) -> Self {
        // SAFETY: our eager test bytes only; initialize the entire allocation including padding
        // so registered PNG equality is against exact known HGLOBAL bytes, never owner contents.
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
            assert!(!memory.is_null());
            let size = GlobalSize(memory);
            assert!(size >= bytes.len() && size < 1024 * 1024);
            let pointer = GlobalLock(memory);
            assert!(!pointer.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            let expected = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
            SetLastError(0);
            assert!(GlobalUnlock(memory) != 0 || GetLastError() == 0);
            Self {
                memory,
                transferred: false,
                expected,
            }
        }
    }
}
impl Drop for Block {
    fn drop(&mut self) {
        if !self.transferred {
            // SAFETY: only our allocation, and only before SetClipboardData transfers it to Windows.
            assert!(unsafe { GlobalFree(self.memory) }.is_null());
        }
    }
}
fn text_fixture() -> Vec<u8> {
    "fixture\r\nline\0ignored"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn dib_fixture() -> Vec<u8> {
    let mut raw = vec![0; 128];
    for (offset, value) in [
        (0, 124u32),
        (4, 1),
        (8, (-1i32) as u32),
        (16, 3),
        (40, 0xff0000),
        (44, 0xff00),
        (48, 0xff),
        (52, 0xff000000),
        (56, 0x73524742),
    ] {
        raw[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    raw[12..14].copy_from_slice(&1u16.to_le_bytes());
    raw[14..16].copy_from_slice(&32u16.to_le_bytes());
    raw[124..128].copy_from_slice(&[3, 2, 1, 128]);
    raw
}
fn wait_kind(receive: &mpsc::Receiver<ClipboardEvent>, kind: ClipKind) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let ClipboardEvent::Changed { kinds } = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        else {
            panic!("unexpected part1 event");
        };
        if match kind {
            ClipKind::Text => kinds.text,
            ClipKind::Image => kinds.image,
        } {
            break;
        }
    }
}
fn promise_probe(backend: &mut clipboard::WindowsClipboard, fixture: &mut Fixture) -> bool {
    let reply = backend.probe_fulfiller();
    let (send, receive) = mpsc::channel();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = requests.clone();
    backend
        .subscribe(Arc::new(move |event| {
            if let ClipboardEvent::PasteRequested { paste, offer, kind } = &event {
                assert_eq!(*kind, ClipKind::Text);
                count.fetch_add(1, Ordering::Relaxed);
                reply(
                    *paste,
                    if *offer == 71 {
                        Some(b"fixture\npromise".to_vec())
                    } else {
                        None
                    },
                );
            }
            let _ = send.send(event);
        }))
        .unwrap();
    if backend
        .promise(
            71,
            crosspane_platform::ClipKinds {
                text: true,
                image: false,
            },
        )
        .is_err()
    {
        println!("CLIP_PROMISE [U] admission_changed_before_promise");
        return false;
    }
    // Ownership transferred to our authenticated observer; the old fixture has no clear authority.
    fixture.sequence = None;
    let paster = Fixture::new();
    let paster_cleaned = paster.cleaned.clone();
    let Some(Some(data)) = paster.paste(u32::from(CF_UNICODETEXT)) else {
        println!("CLIP_PROMISE [U] promise_replaced_before_owned_paste");
        return false;
    };
    let expected = model::clipboard::text_native(b"fixture\npromise").unwrap();
    assert!(
        data.starts_with(&expected),
        "own promised UTF16 CRLF mismatch"
    );
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    if backend
        .promise(
            72,
            crosspane_platform::ClipKinds {
                text: true,
                image: false,
            },
        )
        .is_err()
    {
        println!("CLIP_PROMISE [U] admission_changed_before_empty_promise");
        return false;
    }
    if !matches!(paster.paste(u32::from(CF_UNICODETEXT)), Some(None)) {
        println!("CLIP_PROMISE [U] empty_promise_replaced_before_owned_paste");
        return false;
    }
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    backend.withdraw(72).unwrap();
    {
        let Some(_opened) = paster.open() else {
            println!("CLIP_PROMISE [U] contention_before_withdraw_receipt");
            return false;
        };
        if !paster.admitted(false) {
            println!("CLIP_PROMISE [U] replacement_before_withdraw_receipt");
            return false;
        }
        // SAFETY: exact authenticated observer owner/sequence while open; metadata only.
        unsafe {
            SetLastError(0);
            assert_eq!(CountClipboardFormats(), 0);
            assert_eq!(GetLastError(), 0);
        }
    }
    if backend
        .promise(
            73,
            crosspane_platform::ClipKinds {
                text: true,
                image: false,
            },
        )
        .is_err()
    {
        println!("CLIP_PROMISE [U] admission_changed_before_loss_case");
        return false;
    }
    let Some(_) = fixture.write(u32::from(CF_UNICODETEXT), &text_fixture()) else {
        println!("CLIP_PROMISE [U] replacement_before_owned_copy");
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut losses = 0;
    loop {
        let event = receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if let ClipboardEvent::PromiseLost { offer } = event {
            assert_eq!(offer, 73);
            losses += 1;
            break;
        }
    }
    backend.withdraw(73).unwrap();
    while let Ok(event) = receive.try_recv() {
        if matches!(event, ClipboardEvent::PromiseLost { .. }) {
            losses += 1;
        }
    }
    assert_eq!(losses, 1);
    drop(paster);
    assert!(paster_cleaned.load(Ordering::Acquire));
    println!(
        "CLIP_PROMISE requests=2 text_native_bytes={} crlf=true none_empty=true withdraw_empty=true lost=1 paster_destroyed=true",
        expected.len()
    );
    true
}
#[test]
#[ignore = "explicit Limited win-gui, empty-or-authenticated-own clipboard only"]
fn limited_empty_or_owned_clipboard_watch_and_read() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_CLIPBOARD_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new();
    let started = Instant::now();
    let mut fixture = Fixture::new();
    let cleaned = fixture.cleaned.clone();
    let admitted = fixture.open().is_some_and(|_opened| fixture.admitted(true));
    let gate = IoGate::new();
    let mut backend = clipboard::WindowsClipboard::new(gate.clone()).unwrap();
    if !admitted {
        let kinds = backend.kinds();
        assert_eq!(clipboard::probe_data_requests(), 0);
        println!(
            "CLIP_PROBE [U] contents_not_admitted metadata_available={} data_reads={} writes=0 clears=0",
            kinds.is_ok(),
            clipboard::probe_data_requests()
        );
        assert!(clipboard::WindowsClipboard::stop_verified(backend));
        drop(fixture);
        assert!(cleaned.load(Ordering::Acquire));
        println!(
            "CLIP_PROBE observer_joined=true listener_removed=true fixture_destroyed=true clipboard_untouched=true elapsed_ms={}",
            started.elapsed().as_millis()
        );
        return;
    }
    let Some(_) = fixture.write(u32::from(CF_UNICODETEXT), &text_fixture()) else {
        println!("CLIP_PROBE [U] fixture_admission_changed_before_write");
        assert!(clipboard::WindowsClipboard::stop_verified(backend));
        return;
    };
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let (send, receive) = mpsc::channel();
    backend
        .subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
    wait_kind(&receive, ClipKind::Text);
    assert_eq!(
        clipboard::probe_data_requests(),
        0,
        "watcher must never request data"
    );
    let text = backend.read(ClipKind::Text, 1024).unwrap();
    assert!(text == b"fixture\nline", "owned text mismatch");
    assert!(matches!(
        backend.read(ClipKind::Text, 1),
        Err(PlatformError::TooLarge)
    ));
    gate.set_engine_permits(false);
    assert!(matches!(
        backend.read(ClipKind::Text, 1024),
        Err(PlatformError::Locked)
    ));
    let dib = dib_fixture();
    let png = model::clipboard::dib_png(&dib, 1024).unwrap();
    // SAFETY: registers only our standard test PNG format, no content/foreign name query.
    let png_format = unsafe { RegisterClipboardFormatW([80, 78, 71, 0].as_ptr()) };
    assert_ne!(png_format, 0);
    let Some(expected) = fixture.write(png_format, &png) else {
        println!("CLIP_PROBE [U] fixture_replaced_before_png");
        assert!(clipboard::WindowsClipboard::stop_verified(backend));
        return;
    };
    while receive.try_recv().is_ok() {}
    thread::sleep(Duration::from_millis(50));
    assert!(receive.try_recv().is_err());
    gate.set_engine_permits(true);
    wait_kind(&receive, ClipKind::Image);
    let image = backend.read(ClipKind::Image, 1024).unwrap();
    assert!(image == expected, "owned PNG mismatch");
    assert!(matches!(
        backend.read(ClipKind::Image, image.len() - 1),
        Err(PlatformError::TooLarge)
    ));
    let Some(_) = fixture.write(u32::from(CF_DIBV5), &dib) else {
        println!("CLIP_PROBE [U] fixture_replaced_before_dib");
        assert!(clipboard::WindowsClipboard::stop_verified(backend));
        return;
    };
    wait_kind(&receive, ClipKind::Image);
    let converted = backend.read(ClipKind::Image, 1024).unwrap();
    assert!(converted == png, "owned DIB conversion mismatch");
    let promised = promise_probe(&mut backend, &mut fixture);
    assert!(clipboard::WindowsClipboard::stop_verified(backend));
    assert!(
        fixture.clear(),
        "fixture replacement/close leaves contents untouched; cleanup unverified"
    );
    drop(fixture);
    assert!(cleaned.load(Ordering::Acquire));
    println!(
        "CLIP_PROBE text_bytes={} png_bytes={} dib_png_bytes={} crlf_nul=true too_large=true gate_locked=true kinds_only=true",
        text.len(),
        image.len(),
        converted.len()
    );
    println!(
        "CLIP_PROBE native_data_requests={} watcher_data_requests=0",
        clipboard::probe_data_requests()
    );
    println!(
        "CLIP_PROBE owner_admission=empty_then_exact_fixture promise_cases_completed={} listener_removed=true owner_joined=true delivery_joined=true fixture_cleared=true fixture_destroyed=true elapsed_ms={}",
        promised,
        started.elapsed().as_millis()
    );
}
