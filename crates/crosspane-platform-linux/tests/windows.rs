//! WindowSource acceptance in an explicit nested Hyprland. A dedicated nest prevents other
//! platform tests from changing keyboard focus during this lifecycle test.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crosspane_platform::{PlatformError, WindowEvent, WindowInfo, WindowSource};
use crosspane_platform_linux::hyprland::ipc::{DEFAULT_TIMEOUT, HyprIpc};
use crosspane_platform_linux::hyprland::windows::HyprlandWindows;
use crosspane_types::id::{DisplayId, WindowId};

fn nested_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh")
}

struct Nest {
    name: String,
}

impl Nest {
    fn start() -> (Self, HyprIpc, PathBuf) {
        let nest = Self {
            name: format!("wp-2-7-windows-{}", std::process::id()),
        };
        let parent = std::env::var("CROSSPANE_PARENT_WAYLAND_DISPLAY")
            .unwrap_or_else(|_| std::env::var("WAYLAND_DISPLAY").unwrap());
        assert!(
            Command::new(nested_script())
                .args(["start", "--name", &nest.name])
                .env("WAYLAND_DISPLAY", parent)
                .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
                .env_remove("WAYLAND_SOCKET")
                .status()
                .unwrap()
                .success()
        );
        let result = Command::new(nested_script())
            .args(["env", "--name", &nest.name])
            .output()
            .unwrap();
        assert!(result.status.success());
        let exports = String::from_utf8(result.stdout).unwrap();
        assert!(
            exports
                .lines()
                .any(|line| line == "export CROSSPANE_NESTED_HYPR=1")
        );
        let signature = exports
            .lines()
            .find_map(|line| line.strip_prefix("export HYPRLAND_INSTANCE_SIGNATURE="))
            .unwrap();
        assert!(
            signature
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        let ipc = HyprIpc::new(signature, &runtime, DEFAULT_TIMEOUT);
        let fifo = runtime
            .join(format!("crosspane-hypr-{}", nest.name))
            .join("title-fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        (nest, ipc, fifo)
    }
}

impl Drop for Nest {
    fn drop(&mut self) {
        let status = Command::new(nested_script())
            .args(["stop", "--name", &self.name])
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("failed to stop window test nest {}", self.name);
        }
    }
}

fn exec(ipc: &HyprIpc, command: &str) {
    ipc.dispatch(&format!(
        "hl.dsp.exec_cmd({})",
        serde_json::to_string(command).unwrap()
    ))
    .unwrap();
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for a window");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn receive(
    rx: &mpsc::Receiver<(WindowEvent, ThreadId)>,
    thread: &mut Option<ThreadId>,
    mut matches: impl FnMut(&WindowEvent) -> bool,
) -> WindowEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (event, current) = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        assert_ne!(current, std::thread::current().id());
        assert_eq!(*thread.get_or_insert(current), current);
        if matches(&event) {
            return event;
        }
    }
}

fn added(
    rx: &mpsc::Receiver<(WindowEvent, ThreadId)>,
    thread: &mut Option<ThreadId>,
    class: &str,
    monitor: DisplayId,
) -> WindowInfo {
    let WindowEvent::Added(info) = receive(
        rx,
        thread,
        |event| matches!(event, WindowEvent::Added(info) if info.app_id == class),
    ) else {
        unreachable!()
    };
    assert_eq!(info.display, Some(monitor));
    info
}

#[test]
fn window_lifecycle() {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs an explicit nested Hyprland");
        return;
    }
    let (_nest, ipc, fifo) = Nest::start();
    let monitor = DisplayId(ipc.monitor_ids().unwrap()[0].1);
    let mut source = HyprlandWindows::new(ipc.clone()).unwrap();
    let class1 = "crosspane-wp-2-7-first";
    let class2 = "crosspane-wp-2-7-second";
    // The test shell waits on its own FIFO. A later exec_cmd releases it to print the test OSC
    // title sequence, so Changed is checked without timing assumptions or keyboard injection.
    let script = format!(
        "read -r trigger < {}; printf '\\033]2;crosspane-test\\007'; exec sleep 60",
        shell_quote(fifo.to_str().unwrap())
    );
    exec(
        &ipc,
        &format!(
            "foot --config=/dev/null --app-id {class1} --title crosspane-before sh -c {}",
            shell_quote(&script)
        ),
    );
    wait_until(|| source.windows().unwrap().iter().any(|w| w.app_id == class1));

    let (tx, rx) = mpsc::channel();
    source
        .subscribe(Arc::new(move |event| {
            let _ = tx.send((event, std::thread::current().id()));
        }))
        .unwrap();
    let mut thread = None;
    let first = added(&rx, &mut thread, class1, monitor);
    assert_eq!(first.title, "crosspane-before");
    assert!(source.address(first.id).unwrap().starts_with("0x"));

    exec(
        &ipc,
        &format!("foot --config=/dev/null --app-id {class2} --title crosspane-second sleep 60"),
    );
    let second = added(&rx, &mut thread, class2, monitor);
    assert_ne!(first.id, second.id);
    assert_eq!(source.windows().unwrap().len(), 2);

    exec(
        &ipc,
        &format!(
            "printf 'change\\n' > {}",
            shell_quote(fifo.to_str().unwrap())
        ),
    );
    receive(&rx, &mut thread, |event| {
        matches!(event, WindowEvent::Changed(info)
            if info.id == first.id && info.title == "crosspane-test")
    });

    // Focus the second first, ensuring activation of the first causes a focus transition.
    source.activate(second.id).unwrap();
    wait_until(|| source.focused().unwrap() == Some(second.id));
    source.activate(first.id).unwrap();
    receive(
        &rx,
        &mut thread,
        |event| matches!(event, WindowEvent::Focused(Some(id)) if *id == first.id),
    );
    assert_eq!(source.focused().unwrap(), Some(first.id));
    assert!(matches!(
        source.activate(WindowId(u64::MAX)),
        Err(PlatformError::NotFound)
    ));

    ipc.dispatch(&format!(
        r#"hl.dsp.window.close({{ window = "address:{}" }})"#,
        source.address(first.id).unwrap()
    ))
    .unwrap();
    receive(
        &rx,
        &mut thread,
        |event| matches!(event, WindowEvent::Removed(id) if *id == first.id),
    );
    assert_eq!(source.address(first.id), None);
    assert!(matches!(
        source.activate(first.id),
        Err(PlatformError::NotFound)
    ));
    ipc.dispatch(&format!(
        r#"hl.dsp.window.close({{ window = "address:{}" }})"#,
        source.address(second.id).unwrap()
    ))
    .unwrap();
    receive(
        &rx,
        &mut thread,
        |event| matches!(event, WindowEvent::Removed(id) if *id == second.id),
    );
    assert_eq!(source.focused().unwrap(), None);
}
