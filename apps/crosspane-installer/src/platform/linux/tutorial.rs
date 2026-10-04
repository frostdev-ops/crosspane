//! Owned tutorial window facts and inherited pipes; tone remains unavailable until b3.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired to TutorialNative by WP-4.15b3b")
)]
mod routing;

use super::native_io::tutorial::{TutorialChild, TutorialIpc};
use super::native_io::{
    Cancellation, ChildEnvironment, Deadline, LinuxNativeIo, NativeError, SupportProof,
};
use crate::fixture::*;
use crate::tutorial_window::{TutorialNative, WindowObservation};
use crosspane_types::id::WindowId;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, FixtureError>;
fn failure(e: NativeError) -> FixtureError {
    match e {
        NativeError::Timeout => FixtureError::TimedOut,
        NativeError::Busy => FixtureError::Busy,
        NativeError::Foreign | NativeError::Invalid => FixtureError::NotOwned,
        _ => FixtureError::Unavailable,
    }
}
#[derive(Debug, Default)]
pub struct WindowObserver {
    window: Option<WindowId>,
    initial: Option<i64>,
    title: Option<String>,
}
impl WindowObserver {
    /// Pure bounded parser. Never reports unrelated titles or treats address as stableId.
    pub fn observe(
        &mut self,
        clients: &Value,
        monitors: &Value,
        pid: u32,
        title: &str,
    ) -> WindowObservation {
        let clients = clients
            .as_array()
            .filter(|v| v.len() <= 4096)
            .ok_or(FixtureError::UnknownWindow)?;
        if pid == 0
            || title.len() > 200
            || !title.starts_with("Crosspane practice | ")
            || title.chars().any(char::is_control)
            || self.title.as_deref().is_some_and(|t| t != title)
        {
            return Err(FixtureError::NotOwned);
        }
        let mut own = clients.iter().filter(|c| {
            c["pid"].as_u64() == Some(u64::from(pid)) && c["title"].as_str() == Some(title)
        });
        let first = own.next();
        if own.next().is_some() {
            return Err(FixtureError::AmbiguousWindow);
        }
        let Some(client) = first else {
            let window = self.window.ok_or(FixtureError::UnknownWindow)?;
            if clients.iter().any(|c| stable(c) == Some(window)) {
                return Err(FixtureError::UnknownWindow);
            }
            return Ok((window, OwnWindowFacts::Missing));
        };
        let window = stable(client).ok_or(FixtureError::UnknownWindow)?;
        if clients.iter().filter(|c| stable(c) == Some(window)).count() != 1 {
            return Err(FixtureError::AmbiguousWindow);
        }
        if self.window.is_some_and(|w| w != window) {
            return Err(FixtureError::UnknownWindow);
        }
        let monitor = client["monitor"].as_i64().filter(|n| *n >= 0);
        let monitors = monitors.as_array().filter(|v| v.len() <= 64);
        let display =
            monitor.and_then(|id| monitors?.iter().find(|m| m["id"].as_i64() == Some(id)));
        if self.window.is_none() {
            self.initial = display.and(monitor);
            self.title = Some(title.into());
        }
        self.window = Some(window);
        let visible = match (client["mapped"].as_bool(), client["hidden"].as_bool()) {
            (Some(false), _) | (_, Some(true)) => Some(false),
            (Some(true), Some(false)) => display.and_then(|m| {
                let workspace = client["workspace"]["id"].as_i64()?;
                let active = m["activeWorkspace"]["id"].as_i64()?;
                let special = m["specialWorkspace"]["id"].as_i64()?;
                Some(workspace == active || (special != 0 && workspace == special))
            }),
            _ => None,
        };
        Ok((
            window,
            OwnWindowFacts::Present {
                visible_on_user_workspace: visible,
                on_initial_display: self.initial.zip(display.and(monitor)).map(|(a, b)| a == b),
            },
        ))
    }
}
fn stable(client: &Value) -> Option<WindowId> {
    let text = client["stableId"].as_str()?;
    let text = text.strip_prefix("0x").unwrap_or(text);
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(text, 16)
        .ok()
        .filter(|id| *id != 0)
        .map(WindowId)
}
struct Backend {
    endpoint: Option<(String, PathBuf)>,
    ipc: Option<TutorialIpc>,
    observer: WindowObserver,
}
impl Backend {
    fn read(&mut self, pid: u32, title: &str) -> WindowObservation {
        if self.ipc.is_none() {
            let (signature, runtime) = self.endpoint.as_ref().ok_or(FixtureError::Unavailable)?;
            self.ipc = Some(TutorialIpc::new(signature, runtime).map_err(failure)?);
        }
        let ipc = self.ipc.as_ref().ok_or(FixtureError::Unavailable)?;
        let clients = ipc.json("clients").map_err(failure)?;
        // Missing monitor facts never manufacture display/visibility evidence.
        let monitors = ipc.json("monitors").unwrap_or(Value::Null);
        self.observer.observe(&clients, &monitors, pid, title)
    }
}
static OBSERVERS: AtomicUsize = AtomicUsize::new(0);
struct ObserverSlot;
impl Drop for ObserverSlot {
    fn drop(&mut self) {
        OBSERVERS.fetch_sub(1, Ordering::AcqRel);
    }
}
struct LinuxTutorial {
    backend: Arc<Mutex<Backend>>,
    stopped: Arc<AtomicBool>,
    pending: Option<(u64, mpsc::Receiver<WindowObservation>)>,
}
/// Infallible child factory. Only the two explicitly inherited selected-session values are read.
pub fn native(_: &Path) -> Box<dyn TutorialNative> {
    let endpoint = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .zip(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from));
    Box::new(LinuxTutorial {
        backend: Arc::new(Mutex::new(Backend {
            endpoint,
            ipc: None,
            observer: WindowObserver::default(),
        })),
        stopped: Arc::new(AtomicBool::new(false)),
        pending: None,
    })
}
impl TutorialNative for LinuxTutorial {
    fn observe_window(&mut self, call: u64, title: &str) -> Option<WindowObservation> {
        if let Some((id, receive)) = &self.pending {
            if *id != call {
                return Some(Err(FixtureError::Busy));
            }
            return match receive.try_recv() {
                Ok(value) => {
                    self.pending = None;
                    Some(value)
                }
                Err(mpsc::TryRecvError::Empty) => None,
                Err(_) => {
                    self.pending = None;
                    Some(Err(FixtureError::Unavailable))
                }
            };
        }
        if OBSERVERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            return Some(Err(FixtureError::Busy));
        }
        let slot = ObserverSlot;
        let backend = self.backend.clone();
        let stopped = self.stopped.clone();
        let title = title.to_owned();
        let (send, receive) = mpsc::sync_channel(1);
        let task = thread::Builder::new()
            .name("tutorial-observe".into())
            .spawn(move || {
                let _slot = slot;
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                        return;
                    }
                    let value = backend
                        .lock()
                        .map_err(|_| FixtureError::Unavailable)
                        .and_then(|mut b| {
                            let value = b.read(std::process::id(), &title);
                            if b.observer.window.is_none()
                                && value == Err(FixtureError::UnknownWindow)
                            {
                                return Err(FixtureError::Busy);
                            }
                            value
                        });
                    if value == Err(FixtureError::Busy) {
                        thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                    let _ = send.try_send(value);
                    return;
                }
            });
        if task.is_err() {
            return Some(Err(FixtureError::Unavailable));
        }
        self.pending = Some((call, receive));
        None
    }
    fn play_tone(&mut self, _: ToneId, _: &SpeakersSelection) -> Option<Result<()>> {
        Some(Err(FixtureError::Unavailable))
    }
    fn stop_tone(&mut self, _: ToneId) -> Option<Result<()>> {
        Some(Err(FixtureError::Unavailable))
    }
    fn tone_state(&self) -> OwnToneState {
        OwnToneState::Stopped
    }
}
impl Drop for LinuxTutorial {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

struct OwnedChild {
    child: TutorialChild,
    ipc: TutorialIpc,
}
impl InheritedFixtureChild for OwnedChild {
    fn pid(&self) -> u32 {
        self.child.identity().pid
    }
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.child.read(bytes)
    }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.child.write(bytes)
    }
    fn cleanup_confirmed(&mut self) -> Result<bool> {
        if !self
            .child
            .reaped()
            .map_err(|_| FixtureError::CleanupFailed)?
        {
            return Ok(false);
        }
        let clients = self
            .ipc
            .json("clients")
            .map_err(|_| FixtureError::CleanupFailed)?;
        let clients = clients
            .as_array()
            .filter(|v| v.len() <= 4096)
            .ok_or(FixtureError::CleanupFailed)?;
        let mut absent = true;
        for client in clients {
            let pid = client["pid"].as_u64().ok_or(FixtureError::CleanupFailed)?;
            absent &= pid != u64::from(self.pid());
        }
        Ok(absent)
    }
    fn retire(&mut self) {
        self.child.retire();
    }
}
/// The existing bounded port owns the admitted native child and its cleanup evidence.
pub type LinuxFixturePort = PipeFixturePort;
#[derive(Debug)]
pub struct LinuxFixtureLaunch {
    receive: Option<mpsc::Receiver<Result<LinuxFixturePort>>>,
    cancel: Cancellation,
    deadline: Deadline,
}
impl LinuxFixtureLaunch {
    pub fn poll(&mut self) -> Option<Result<LinuxFixturePort>> {
        let receiver = self.receive.as_ref()?;
        if let Err(error) = self.deadline.check() {
            self.cancel.cancel();
            self.receive.take();
            return Some(Err(failure(error)));
        }
        match receiver.try_recv() {
            Ok(value) => {
                self.receive.take();
                Some(value)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => {
                self.receive.take();
                Some(Err(FixtureError::Unavailable))
            }
        }
    }
}
impl Drop for LinuxFixtureLaunch {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
/// Starts bounded preparation off the GUI thread; calls begin only after poll yields a port.
pub fn launch(
    io: Arc<LinuxNativeIo>,
    proof: SupportProof,
    environment: ChildEnvironment,
    expected_sha256: [u8; 32],
    font: PathBuf,
    clock: FixtureClock,
) -> Result<LinuxFixtureLaunch> {
    OBSERVERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| FixtureError::Busy)?;
    let slot = ObserverSlot;
    let cancel = Cancellation::default();
    let deadline = Deadline::new(2000, cancel.clone()).map_err(failure)?;
    let poll_deadline = deadline.clone();
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("tutorial-start".into())
        .spawn(move || {
            let _slot = slot;
            let result = (|| {
                let command = io
                    .admit_tutorial(&proof, &environment, expected_sha256, &font, &deadline)
                    .map_err(failure)?;
                let values = environment.values();
                let runtime = values
                    .get("XDG_RUNTIME_DIR")
                    .ok_or(FixtureError::NotOwned)?;
                let signature = values
                    .get("HYPRLAND_INSTANCE_SIGNATURE")
                    .ok_or(FixtureError::NotOwned)?;
                let ipc = TutorialIpc::new(signature, Path::new(runtime)).map_err(failure)?;
                let child = io.spawn_tutorial(command, &deadline).map_err(failure)?;
                let pipe = PipeFixturePort::new(Box::new(OwnedChild { child, ipc }), clock)?;
                deadline.check().map_err(failure)?;
                Ok(pipe)
            })();
            let _ = send.send(result);
        })
        .map_err(|_| FixtureError::Unavailable)?;
    Ok(LinuxFixtureLaunch {
        receive: Some(receive),
        cancel,
        deadline: poll_deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct InertChild(Arc<AtomicBool>);
    impl InheritedFixtureChild for InertChild {
        fn pid(&self) -> u32 {
            77
        }
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn cleanup_confirmed(&mut self) -> Result<bool> {
            Ok(false)
        }
        fn retire(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[test]
    fn b2_stalled_preparation_times_out_and_disposes_late_owned_result() {
        let cancel = Cancellation::default();
        let (send, receive) = mpsc::sync_channel(1);
        let (release, stalled) = mpsc::sync_channel(1);
        let retired = Arc::new(AtomicBool::new(false));
        let cleanup = retired.clone();
        OBSERVERS.fetch_add(1, Ordering::AcqRel);
        let worker = thread::spawn(move || {
            let _slot = ObserverSlot;
            let _ = stalled.recv();
            let port = PipeFixturePort::new(Box::new(InertChild(cleanup)), Arc::new(|| 1)).unwrap();
            let _ = send.send(Ok(port));
        });
        let mut preparing = LinuxFixtureLaunch {
            receive: Some(receive),
            deadline: Deadline::new(2000, cancel.clone()).unwrap(),
            cancel: cancel.clone(),
        };
        thread::sleep(Duration::from_millis(2050));
        assert!(matches!(
            preparing.poll(),
            Some(Err(FixtureError::TimedOut))
        ));
        assert!(cancel.is_cancelled());
        assert_eq!(
            OBSERVERS.load(Ordering::Acquire),
            1,
            "stalled worker keeps its slot"
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            preparing.poll().is_none(),
            "late result cannot publish success"
        );
        let end = std::time::Instant::now() + Duration::from_secs(1);
        while !retired.load(Ordering::Acquire) && std::time::Instant::now() < end {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            retired.load(Ordering::Acquire),
            "late owned port is retired"
        );
        assert_eq!(OBSERVERS.load(Ordering::Acquire), 0);
    }
}
