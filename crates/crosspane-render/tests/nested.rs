#![cfg(target_os = "linux")]

use std::{
    io::Write,
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

#[test]
fn nested_proxy_lifecycle() -> Result<()> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("SKIP nested_proxy_lifecycle: CROSSPANE_NESTED_HYPR is not 1");
        return Ok(());
    }
    // Verify the opt-in points at the instance created by the nested-session script.
    // This test never selects an arbitrary signature supplied by the caller.
    let env = Command::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/hypr-nested.sh"
    ))
    .arg("env")
    .output()?;
    ensure!(
        env.status.success(),
        "the script's nested instance is not running"
    );
    let env = String::from_utf8(env.stdout)?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let display = std::env::var("WAYLAND_DISPLAY")?;
    ensure!(
        env.lines()
            .any(|line| line == format!("export HYPRLAND_INSTANCE_SIGNATURE={signature}")),
        "signature is not the script's nested instance"
    );
    ensure!(
        env.lines()
            .any(|line| line == format!("export WAYLAND_DISPLAY={display}")),
        "Wayland display is not the script's nested instance"
    );

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
