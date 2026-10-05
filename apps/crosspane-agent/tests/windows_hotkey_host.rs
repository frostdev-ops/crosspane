#![cfg(windows)]
//! Compile only in cargo; run the owned, network-free driver through Limited win-gui.sh.

use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Command};

const DRIVER: &str = r#"
use std::{ffi::c_void, sync::{Arc, mpsc}, time::{Duration, Instant}};
use crosspane_engine::EngineConfig;
use crosspane_platform::{GlobalHotkeys, HotkeyEvent, IoGate};
use crosspane_platform_windows::{capture::WindowsCapture, displays::WindowsDisplays, hotkey::WindowsHotkeys};
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle, ProxyHost};
use crosspane_types::{geom::PixelSize, id::NodeId};

type Handle = *mut c_void;
#[repr(C)] #[derive(Clone, Copy)]
struct RawDevice { page: u16, usage: u16, flags: u32, target: Handle }
#[link(name = "user32")]
unsafe extern "system" {
    fn GetRegisteredRawInputDevices(devices: *mut RawDevice, count: *mut u32, size: u32) -> u32;
    fn GetWindowThreadProcessId(window: Handle, process: *mut u32) -> u32;
    fn EnumThreadWindows(thread: u32, callback: unsafe extern "system" fn(Handle, isize) -> i32, data: isize) -> i32;
    fn GetClassNameW(window: Handle, text: *mut u16, len: i32) -> i32;
    fn PostMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" { fn GetCurrentThreadId() -> u32; }

struct Inventory { total: u32, keyboards: Vec<(usize, bool)>, mice: Vec<(usize, bool)> }
fn inventory() -> Inventory {
    let mut count = 0;
    let size = std::mem::size_of::<RawDevice>() as u32;
    // SAFETY: documented current-process inventory query; writable count, null buffer, no input data.
    assert_ne!(unsafe { GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count, size) }, u32::MAX);
    assert!(count <= 64, "bounded own-process registration inventory");
    if count == 0 { return Inventory { total: 0, keyboards: Vec::new(), mice: Vec::new() }; }
    let mut rows = vec![RawDevice { page: 0, usage: 0, flags: 0, target: std::ptr::null_mut() }; count as usize];
    // SAFETY: initialized aligned array has the advertised capacity; concurrent growth fails without retry.
    let copied = unsafe { GetRegisteredRawInputDevices(rows.as_mut_ptr(), &mut count, size) };
    assert_ne!(copied, u32::MAX);
    assert!(copied as usize <= rows.len());
    let observed: Vec<_> = rows.into_iter().take(copied as usize)
        .filter(|row| row.page == 1 && (row.usage == 6 || row.usage == 2 || row.usage == 0))
        .map(|row| {
            let mut process = 0;
            // SAFETY: current-process registration target and writable PID; no title/content observation.
            if !row.target.is_null() { unsafe { GetWindowThreadProcessId(row.target, &mut process); } }
            (row.usage, row.target as usize, process == std::process::id())
        }).collect();
    let keyboards = observed.iter().filter(|row| row.0 == 6 || row.0 == 0).map(|row| (row.1, row.2)).collect();
    let mice = observed.iter().filter(|row| row.0 == 2 || row.0 == 0).map(|row| (row.1, row.2)).collect();
    Inventory { total: copied, keyboards, mice }
}
fn sample(stage: &str, baseline: (usize, usize), expected: bool) {
    let rows = inventory();
    let equal = rows.keyboards.len() == 1 && rows.keyboards[0].0 == baseline.0;
    let mouse_equal = rows.mice.len() == 1 && rows.mice[0].0 == baseline.1;
    let owned = rows.keyboards.iter().chain(rows.mice.iter()).all(|(_, owned)| *owned);
    eprintln!("registration stage={stage} count={} keyboards={} own_targets={owned} hotkey_target_equal={equal} mice={} capture_target_equal={mouse_equal}", rows.total, rows.keyboards.len(), rows.mice.len());
    assert!(owned, "all observed registration targets belong to this fixture PID");
    assert_eq!(equal, expected, "keyboard ownership at {stage}");
    assert_eq!(mouse_equal, expected, "mouse ownership at {stage}");
}
fn capture() -> (WindowsDisplays, WindowsCapture) {
    let displays = WindowsDisplays::new().expect("owned display geometry observer");
    let snapshot = displays.snapshot().expect("own display snapshot");
    let mut ids = snapshot.ids;
    let capture = WindowsCapture::new(IoGate::new(), &snapshot.probes, &mut ids).expect("closed-gate owned capture observer");
    (displays, capture)
}
fn hotkeys() -> WindowsHotkeys {
    let mut backend = WindowsHotkeys::new().expect("exclusive fixture observer");
    backend.set_chord(&EngineConfig::new(NodeId([0; 32])).release_chord).expect("default release observer");
    backend.subscribe(Arc::new(|_: HotkeyEvent| {})).expect("observer subscription");
    backend
}
unsafe extern "system" fn find_proxy(window: Handle, data: isize) -> i32 {
    let mut process = 0;
    // SAFETY: synchronous enumeration of own host thread and writable PID storage.
    unsafe { GetWindowThreadProcessId(window, &mut process); }
    if process != std::process::id() { return 1; }
    let mut class = [0u16; 128];
    // SAFETY: verified own-PID HWND and writable buffer; class only, never title/content.
    let len = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "CrosspaneProxy" {
        // SAFETY: data points to live callback storage for synchronous EnumThreadWindows.
        unsafe { (&mut *(data as *mut Vec<usize>)).push(window as usize); }
    }
    1
}
fn windows() -> Vec<usize> {
    let mut rows = Vec::new();
    // SAFETY: only current fixture host thread; callback storage lives through synchronous enumeration.
    unsafe { EnumThreadWindows(GetCurrentThreadId(), find_proxy, &mut rows as *mut _ as isize); }
    rows
}
fn on_host<T: Send + 'static>(host: &HostHandle, action: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = mpsc::sync_channel(1);
    host.send(HostCommand::Run(Box::new(move || { let _ = send.send(action()); }))).expect("host action");
    receive.recv_timeout(Duration::from_secs(8)).expect("bounded host result")
}
fn wait(events: &mpsc::Receiver<HostEvent>, mut accept: impl FnMut(&HostEvent) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("bounded own proxy event");
        assert!(!matches!(event, HostEvent::OpenFailed { .. } | HostEvent::Lost { .. }), "owned proxy ready");
        // Input payloads, keys, characters and foreign window data are never logged.
        if accept(&event) { return; }
    }
}
fn check(host: &HostHandle, events: &mpsc::Receiver<HostEvent>, backend: &mut WindowsHotkeys, baseline: (usize, usize), before: bool) {
    for id in 1..=2 {
        host.send(HostCommand::Open { id, title: "Crosspane owned registration fixture".into(), size: PixelSize::new(320, 240), accent: [0, 120, 200], place: None }).expect("owned open");
        wait(events, |event| matches!(event, HostEvent::Opened { id: own, .. } if *own == id));
        sample(if id == 1 { "open" } else { "reopen" }, baseline, !before);
        on_host(host, || {
            let rows = windows();
            assert_eq!(rows.len(), 1, "one owned proxy");
            let window = rows[0] as Handle;
            let mut process = 0;
            // SAFETY: retained own-thread proxy HWND; PID assertion precedes the scoped message.
            unsafe { GetWindowThreadProcessId(window, &mut process); }
            assert_eq!(process, std::process::id());
            // SAFETY: posted focus notification only to verified own fixture HWND; no global input.
            assert_ne!(unsafe { PostMessageW(window, 0x0007, 0, 0) }, 0);
        });
        wait(events, |event| matches!(event, HostEvent::Focus { id: own, focused: true } if *own == id));
        sample(if id == 1 { "focus" } else { "refocus" }, baseline, !before);
        if !before {
            backend.set_chord(&EngineConfig::new(NodeId([0; 32])).release_chord).expect("ownership remains asserted after focus");
        }
        host.send(HostCommand::Close { id }).expect("owned close");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if on_host(host, || windows().is_empty()) { break; }
            assert!(Instant::now() < deadline, "bounded native close");
            std::thread::sleep(Duration::from_millis(20));
        }
        sample(if id == 1 { "close" } else { "reclose" }, baseline, !before);
    }
    eprintln!("owned registration fixture: passed before_fix={before}");
}
fn main() {
    if std::env::var("CROSSPANE_HOTKEY_HOST_FIXTURE").as_deref() != Ok("1") {
        eprintln!("SKIP: only Limited win-gui.sh owned fixture"); return;
    }
    let mode = std::env::args().nth(1).expect("before or after mode");
    assert!(mode == "before" || mode == "after");
    let before = mode == "before";
    let early_capture = before.then(capture);
    let early = before.then(hotkeys);
    let early_target = early.as_ref().map(|_| {
        let rows = inventory();
        assert_eq!(rows.keyboards.len(), 1);
        assert_eq!(rows.mice.len(), 1);
        assert!(rows.keyboards[0].1 && rows.mice[0].1);
        (rows.keyboards[0].0, rows.mice[0].0)
    });
    if let Some(target) = early_target { sample("before_host", target, true); }
    let (host, handle) = ProxyHost::new().expect("process-main proxy host");
    let (displays, capture) = early_capture.unwrap_or_else(capture);
    let mut backend = early.unwrap_or_else(hotkeys);
    let target = early_target.unwrap_or_else(|| {
        let rows = inventory();
        assert_eq!(rows.keyboards.len(), 1);
        assert_eq!(rows.mice.len(), 1);
        assert!(rows.keyboards[0].1 && rows.mice[0].1);
        (rows.keyboards[0].0, rows.mice[0].0)
    });
    sample("after_host", target, !before);
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&handle, &events, &mut backend, target, before)));
        let _ = handle.send(HostCommand::Shutdown);
        // Drop and bounded native cleanup complete before this process exits. A fresh observer
        // is tested only in a separate process; old cleanup cannot unregister the new owner.
        drop(backend);
        drop(capture);
        drop(displays);
        result.is_ok()
    });
    host.run(Box::new(move |event| { let _ = send.send(event); })).expect("main host loop");
    if !worker.join().expect("owned driver worker") { std::process::exit(1); }
}
"#;

fn library(deps: &Path, name: &str) -> Result<std::path::PathBuf> {
    std::fs::read_dir(deps)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_stem().is_some_and(|stem| {
                let stem = stem.to_string_lossy();
                stem.starts_with(&format!("{name}-")) || stem.starts_with(&format!("lib{name}-"))
            }) && path.extension().is_some_and(|ext| ext == "rlib")
        })
        .max_by_key(|path| {
            path.metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
        })
        .with_context(|| format!("missing {name} test dependency"))
}

#[test]
fn owned_hotkey_host_fixture_compiles_without_running_gui() -> Result<()> {
    let executable = std::env::current_exe()?;
    let deps = executable.parent().context("test dependency directory")?;
    let source = deps.join("windows-hotkey-host-driver.rs");
    let driver = deps.join("windows-hotkey-host-driver.exe");
    std::fs::write(&source, DRIVER)?;
    let mut compiler = Command::new("rustc");
    compiler
        .args([
            "--edition=2024",
            "-Dwarnings",
            "--crate-name",
            "windows_hotkey_host_driver",
            "-L",
        ])
        .arg(format!("dependency={}", deps.display()));
    for build in std::fs::read_dir(deps.parent().context("debug directory")?.join("build"))? {
        if let Ok(output) = std::fs::read_to_string(build?.path().join("output")) {
            for line in output.lines() {
                if let Some(search) = line.strip_prefix("cargo:rustc-link-search=native=") {
                    compiler.arg("-L").arg(format!("native={search}"));
                }
            }
        }
    }
    for name in [
        "crosspane_engine",
        "crosspane_platform",
        "crosspane_platform_windows",
        "crosspane_render",
        "crosspane_types",
    ] {
        compiler
            .arg("--extern")
            .arg(format!("{name}={}", library(deps, name)?.display()));
    }
    let output = compiler.arg(&source).arg("-o").arg(&driver).output()?;
    std::fs::remove_file(source)?;
    ensure!(
        output.status.success(),
        "owned driver compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("owned registration fixture compiled: {}", driver.display());
    Ok(())
}
