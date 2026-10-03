#![cfg(target_os = "linux")]

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle, ProxyHost};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};

struct TestHost {
    handle: HostHandle,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Drop for TestHost {
    fn drop(&mut self) {
        let _ = self.handle.send(HostCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Filesystem-only validation: the compositor lock must bind the supplied signature and display
/// to the PID in a script-owned state record. No IPC or Wayland connection may precede this.
fn find_nested_state(runtime: &Path, signature: &str, display: &str) -> Result<(String, u32)> {
    ensure!(
        !signature.is_empty()
            && signature
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "unknown compositor signature"
    );
    ensure!(
        !display.is_empty()
            && display
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "unknown Wayland socket name"
    );
    let lock = std::fs::read_to_string(runtime.join("hypr").join(signature).join("hyprland.lock"))
        .context("reading the supplied compositor's lock")?;
    // Hyprland's lock contains exactly the compositor PID and its Wayland socket, one per line.
    let mut lines = lock.lines();
    let lock_pid = lines.next().context("missing compositor PID")?.trim();
    ensure!(
        !lock_pid.is_empty() && lock_pid.bytes().all(|byte| byte.is_ascii_digit()),
        "unknown compositor PID"
    );
    let pid: u32 = lock_pid.parse().context("invalid compositor PID")?;
    ensure!(pid > 0, "invalid compositor PID");
    let socket = lines
        .next()
        .context("missing compositor Wayland socket")?
        .trim();
    ensure!(
        socket == display,
        "compositor serves {socket}, not {display}"
    );
    ensure!(lines.next().is_none(), "unknown compositor lock format");
    let matches = |env: &str| {
        env.lines()
            .any(|line| line == format!("export HYPRLAND_INSTANCE_SIGNATURE={signature}"))
            && env
                .lines()
                .any(|line| line == format!("export WAYLAND_DISPLAY={display}"))
            && env
                .lines()
                .any(|line| line == "export CROSSPANE_NESTED_HYPR=1")
    };
    for entry in std::fs::read_dir(runtime)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let filename = entry.file_name();
        let Some(name) = filename
            .to_str()
            .and_then(|name| name.strip_prefix("crosspane-hypr-"))
        else {
            continue;
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            continue;
        }
        let Ok(env) = std::fs::read_to_string(entry.path().join("env")) else {
            continue;
        };
        if !matches(&env) {
            continue;
        }
        let state_pid = std::fs::read_to_string(entry.path().join("pid"))
            .context("missing script-owned compositor PID")?;
        ensure!(
            state_pid.trim() == lock_pid,
            "script state PID does not match the compositor lock"
        );
        let start = std::fs::read_to_string(entry.path().join("start"))
            .context("missing script-owned process start time")?;
        let start = start.trim();
        ensure!(
            !start.is_empty()
                && start.bytes().all(|byte| byte.is_ascii_digit())
                && start.parse::<u64>().is_ok_and(|start| start > 0),
            "unknown script-owned process start time"
        );
        return Ok((name.to_owned(), pid));
    }
    bail!("environment does not match a script-owned nested instance")
}

/// Bind both endpoints before the script makes its first IPC query. The script then checks the
/// recorded PID's current process start time, so a dead or reused PID still cannot pass.
fn nested_signature() -> Result<String> {
    ensure!(
        std::env::var_os("WAYLAND_SOCKET").is_none(),
        "WAYLAND_SOCKET must be unset in the script's nested environment"
    );
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let display = std::env::var("WAYLAND_DISPLAY")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("nested runtime directory")?;
    let (name, pid) = find_nested_state(Path::new(&runtime), &signature, &display)?;
    let verified = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/hypr-nested.sh"
    ))
    .args(["env", "--name", &name])
    .output()?;
    ensure!(
        verified.status.success(),
        "the matching script-owned nested instance is not running"
    );
    let env = String::from_utf8(verified.stdout)?;
    ensure!(
        env.lines()
            .any(|line| line == format!("export HYPRLAND_INSTANCE_SIGNATURE={signature}"))
            && env
                .lines()
                .any(|line| line == format!("export WAYLAND_DISPLAY={display}")),
        "the script's nested environment changed during validation"
    );
    let status = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/hypr-nested.sh"
    ))
    .args(["status", "--name", &name])
    .output()?;
    ensure!(
        status.status.success()
            && String::from_utf8(status.stdout)?
                .lines()
                .any(|line| line == format!("{name}: running (pid {pid}, instance {signature})")),
        "signature does not belong to the script's running nested process"
    );
    Ok(signature)
}

/// An owned fake runtime: guard tests read and write only these temporary records.
struct FakeRuntime(PathBuf);

impl FakeRuntime {
    fn new() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..100 {
            let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "crosspane-render-nest-guard-{}-{next}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not create an owned fake runtime")
    }

    fn write(&self, relative: &str, text: &str) -> Result<()> {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().context("fixture parent")?)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    fn state(&self, signature: &str, display: &str, pid: u32) -> Result<()> {
        self.write(
            "crosspane-hypr-test/env",
            &format!(
                "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE={signature}\nexport WAYLAND_DISPLAY={display}\nexport CROSSPANE_NESTED_HYPR=1\n"
            ),
        )?;
        self.write("crosspane-hypr-test/pid", &format!("{pid}\n"))?;
        self.write("crosspane-hypr-test/start", "42\n")
    }
}

impl Drop for FakeRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn nest_guard_refuses_a_compositor_display_mismatch() -> Result<()> {
    let runtime = FakeRuntime::new()?;
    runtime.write("hypr/sigNEST/hyprland.lock", "4242\nwayland-9\n")?;
    runtime.state("sigNEST", "wayland-9", 4242)?;
    assert_eq!(
        find_nested_state(&runtime.0, "sigNEST", "wayland-9")?,
        ("test".to_owned(), 4242)
    );
    // A state file can claim another display while retaining a valid signature and PID.
    // Hyprland's own lock must reject that pairing before the script or a client can connect.
    runtime.state("sigNEST", "wayland-live", 4242)?;
    let error = find_nested_state(&runtime.0, "sigNEST", "wayland-live").unwrap_err();
    assert!(error.to_string().contains("compositor serves"));
    Ok(())
}

#[test]
fn nest_guard_refuses_foreign_missing_and_unknown_records() -> Result<()> {
    let runtime = FakeRuntime::new()?;
    runtime.write("hypr/sigNEST/hyprland.lock", "4242\nwayland-9\n")?;
    runtime.state("sigNEST", "wayland-9", 4343)?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    runtime.state("sigOTHER", "wayland-9", 4242)?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    runtime.state("sigNEST", "wayland-other", 4242)?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    runtime.state("sigNEST", "wayland-9", 4242)?;
    std::fs::remove_file(runtime.0.join("crosspane-hypr-test/start"))?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    runtime.write("crosspane-hypr-test/start", "unknown\n")?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    runtime.state("sigNEST", "wayland-9", 4242)?;
    for lock in [
        "bad\nwayland-9\n",
        "0\nwayland-9\n",
        "4242\n",
        "4242\nwayland-9\nunknown\n",
    ] {
        runtime.write("hypr/sigNEST/hyprland.lock", lock)?;
        assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    }
    runtime.write("hypr/sigNEST/hyprland.lock", "4242\nwayland-9\n")?;
    for signature in ["", "../sigNEST", "sig/NEST", "sigMISSING"] {
        assert!(find_nested_state(&runtime.0, signature, "wayland-9").is_err());
    }
    for display in ["", "/tmp/wayland-9"] {
        assert!(find_nested_state(&runtime.0, "sigNEST", display).is_err());
    }
    std::fs::rename(
        runtime.0.join("crosspane-hypr-test"),
        runtime.0.join("foreign-test"),
    )?;
    assert!(find_nested_state(&runtime.0, "sigNEST", "wayland-9").is_err());
    Ok(())
}

#[test]
fn nested_proxy_lifecycle() -> Result<()> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("SKIP nested_proxy_lifecycle: CROSSPANE_NESTED_HYPR is not 1");
        return Ok(());
    }
    let signature = nested_signature()?;

    let (handle_sender, handle_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    let thread = thread::spawn(move || -> Result<()> {
        let _subscriber = tracing::subscriber::set_default(SurfaceLog);
        let (host, handle) = ProxyHost::new_any_thread()?;
        handle_sender.send(handle)?;
        host.run(Box::new(move |event| {
            let _ = event_sender.send(event);
        }))?;
        Ok(())
    });
    let handle = handle_receiver
        .recv_timeout(Duration::from_secs(10))
        .context("creating host")?;
    let mut host = TestHost {
        handle,
        thread: Some(thread),
    };
    let title = format!("WP-2.6 proxy {}", std::process::id());
    let changed_title = format!("WP-2.6 renamed {}", std::process::id());
    let size = PixelSize::new(640, 480);
    host.handle.send(HostCommand::Open {
        id: 26,
        title: title.clone(),
        size,
        accent: [211, 45, 137],
        place: None,
    })?;
    let opened = wait_event(&event_receiver, |event| {
        matches!(event, HostEvent::Opened { id: 26, .. })
    })?;
    if let HostEvent::Opened { size, scale, .. } = opened {
        ensure!(
            size.width > 0 && size.height > 0 && size.width < 16384 && size.height < 16384,
            "insane opened size"
        );
        ensure!(scale.is_finite() && scale > 0.0, "insane scale");
        eprintln!(
            "nested opened: {}x{}, scale {scale}",
            size.width, size.height
        );
    }
    poll_client(&signature, &title, true)?;
    host.handle.send(HostCommand::Frame {
        id: 26,
        size,
        pixels: vec![0x42; size.width as usize * size.height as usize * 4].into(),
        dirty: vec![PixelRect::new(point2(0, 0), point2(640, 480))],
    })?;
    // Run is commanded from this thread, executed on the event-loop thread.
    let (run_sender, run_receiver) = mpsc::channel();
    let host_thread_id = host.thread.as_ref().context("host thread")?.thread().id();
    host.handle.send(HostCommand::Run(Box::new(move || {
        let _ = run_sender.send(thread::current().id());
    })))?;
    ensure!(
        run_receiver.recv_timeout(Duration::from_secs(10))? == host_thread_id,
        "Run used the wrong thread"
    );

    let address = client_address(&signature, &title)?.context("mapped proxy")?;
    ipc(
        &signature,
        &[
            "dispatch",
            &format!("hl.dsp.window.float({{action=\"enable\",window=\"address:{address}\"}})"),
        ],
    )?;
    // Drain initial and float configure events before requesting a different size.
    thread::sleep(Duration::from_millis(200));
    drain(&event_receiver)?;
    ipc(
        &signature,
        &[
            "dispatch",
            &format!(
                "hl.dsp.window.resize({{x=777,y=433,relative=false,window=\"address:{address}\"}})"
            ),
        ],
    )?;
    let resized = wait_event(
        &event_receiver,
        |event| matches!(event, HostEvent::Resized { id: 26, size, .. } if size.width == 777 && size.height == 433),
    )?;
    eprintln!("nested compositor resize: {resized:?}");
    host.handle.send(HostCommand::SetTitle {
        id: 26,
        title: changed_title.clone(),
    })?;
    poll_client(&signature, &changed_title, true)?;
    ipc(
        &signature,
        &[
            "dispatch",
            &format!("hl.dsp.window.close({{window=\"address:{address}\"}})"),
        ],
    )?;
    wait_event(&event_receiver, |event| {
        matches!(event, HostEvent::CloseRequested { id: 26 })
    })?;
    ensure!(
        client_address(&signature, &changed_title)?.is_some(),
        "CloseRequested closed the proxy without Close"
    );
    host.handle.send(HostCommand::Close { id: 26 })?;
    poll_client(&signature, &changed_title, false)?;
    drain(&event_receiver)?;
    host.handle.send(HostCommand::Shutdown)?;
    host.thread
        .take()
        .context("host thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("host panicked"))??;
    ensure!(
        host.handle.send(HostCommand::Run(Box::new(|| {}))).is_err(),
        "send accepted a command after exit"
    );
    eprintln!("nested: open/frame/Run/resize/title/CloseRequested/Close/Shutdown passed");
    Ok(())
}

#[test]
fn nested_proxy_reports_only_new_content() -> Result<()> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("SKIP nested_proxy_reports_only_new_content: CROSSPANE_NESTED_HYPR is not 1");
        return Ok(());
    }
    nested_signature()?;

    let (handle_sender, handle_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    let thread = thread::spawn(move || -> Result<()> {
        let (host, handle) = ProxyHost::new_any_thread()?;
        handle_sender.send(handle)?;
        host.run(Box::new(move |event| {
            let _ = event_sender.send(event);
        }))?;
        Ok(())
    });
    let host = TestHost {
        handle: handle_receiver.recv_timeout(Duration::from_secs(10))?,
        thread: Some(thread),
    };
    let size = PixelSize::new(320, 240);
    host.handle.send(HostCommand::Open {
        id: 45,
        title: format!("WP-4.5a proxy {}", std::process::id()),
        size,
        accent: [211, 45, 137],
        place: None,
    })?;
    wait_event(&event_receiver, |event| {
        matches!(event, HostEvent::Opened { id: 45, .. })
    })?;
    host.handle.send(HostCommand::Frame {
        id: 45,
        size,
        pixels: vec![0x42; size.width as usize * size.height as usize * 4].into(),
        dirty: vec![PixelRect::new(point2(0, 0), point2(320, 240))],
    })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let event = event_receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("waiting up to 2 s for Presented")?;
        check(&event)?;
        if let HostEvent::Presented { id: 45, frames } = event {
            ensure!(frames >= 1, "Presented reported zero frames");
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match event_receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                check(&event)?;
                ensure!(
                    !matches!(event, HostEvent::Presented { id: 45, .. }),
                    "proxy reported Presented without further content"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn check(event: &HostEvent) -> Result<()> {
    match event {
        HostEvent::Lost { id } => bail!("proxy {id} was lost"),
        HostEvent::OpenFailed { error, .. } => bail!("proxy open failed: {error}"),
        _ => Ok(()),
    }
}

fn drain(receiver: &Receiver<HostEvent>) -> Result<()> {
    while let Ok(event) = receiver.try_recv() {
        check(&event)?;
    }
    Ok(())
}

fn wait_event(
    receiver: &Receiver<HostEvent>,
    predicate: impl Fn(&HostEvent) -> bool,
) -> Result<HostEvent> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("waiting for host event")?;
        check(&event)?;
        if predicate(&event) {
            return Ok(event);
        }
    }
}

fn ipc(signature: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("timeout")
        .args(["5", "hyprctl", "-i", signature])
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "nested hyprctl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn client_address(signature: &str, title: &str) -> Result<Option<String>> {
    let clients = ipc(signature, &["-j", "clients"])?;
    let mut jq = Command::new("jq")
        .args([
            "-r",
            "--arg",
            "title",
            title,
            ".[] | select(.class == \"crosspane-proxy\" and .title == $title) | .address",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    jq.stdin
        .take()
        .context("jq stdin")?
        .write_all(clients.as_bytes())?;
    let output = jq.wait_with_output()?;
    ensure!(output.status.success(), "parsing nested clients failed");
    let address = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok(if address.is_empty() {
        None
    } else {
        Some(address)
    })
}

fn poll_client(signature: &str, title: &str, present: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client_address(signature, title)?.is_some() == present {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "proxy presence/title did not update"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

// A minimal test-only subscriber records surface format evidence without another dependency.
struct SurfaceLog;
impl tracing::Subscriber for SurfaceLog {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target().starts_with("crosspane_render")
    }
    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut SurfaceFields);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}
struct SurfaceFields;
impl tracing::field::Visit for SurfaceFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        eprintln!("proxy {}: {value:?}", field.name());
    }
}
