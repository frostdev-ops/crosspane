//! Test-only: the virtual screen against a fake ScreenCast portal and a private PipeWire server.
//!
//! Two fixtures from the monitor capture's tests are reused. The **fake portal**
//! (`screencast::fake_portal`) speaks the portal's real protocol on a private `dbus-daemon`, so the
//! options the worker sends, the token handling, the consent run, a `Start` that never answers and
//! the session's `Closed` signal are exercised without touching the desktop's own portal. The
//! **private PipeWire server** (`screencast::private_server`) is a throwaway `pipewire` daemon on a
//! private socket; the fake portal hands its socket out as the session's PipeWire remote, and a
//! synthetic *range-size* producer stands in for Mutter's virtual stream (it accepts any size and
//! paints the size it is given). That runs the real data path: the consumer's fixed-size offer, the
//! negotiation, frames, crops, the cached first frame, the gate, a resize that renegotiates, and a
//! vanishing node. A synthetic producer is not Mutter: that the monitor appears at the negotiated
//! size, and how Mutter reacts to a renegotiation, are live checks. Tests skip with a printed
//! reason when `dbus-daemon` or `pipewire` is missing.

use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{Frame, StreamEndReason};
use crosspane_types::geom::euclid::point2;
use crosspane_types::id::{DisplayId, WindowId};
use pipewire as pw;
use pw::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{self, ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};

use super::stream::STREAM_NAME;
use super::worker::consent;
use super::*;
use crate::portal::screencast::fake_portal::{Portal, Select, StartScript};
use crate::portal::screencast::private_server::{Linker, Server};

const WAIT: Duration = Duration::from_secs(10);
const PRODUCER: &str = "crosspane.test.virtual-producer";
/// The red byte of every pixel the producer paints.
const RED: u8 = 0x77;

// ---- helpers -----------------------------------------------------------------------------------

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

/// A state directory under the system temp dir, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Dir(std::env::temp_dir().join(format!(
            "crosspane-virtual-screen-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn token_path(&self) -> PathBuf {
        self.0.join("portal-virtual.token")
    }

    /// A state directory that already holds a consent token.
    fn with_token(token: &str) -> Dir {
        let dir = Dir::new();
        token::write(&dir.token_path(), token).unwrap();
        dir
    }

    fn config(&self) -> VirtualScreenConfig {
        VirtualScreenConfig {
            token_path: self.token_path(),
        }
    }

    fn token(&self) -> Option<String> {
        token::read(&self.token_path())
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Short bounds, so the tests that wait for a timeout stay quick.
fn short() -> Timeouts {
    Timeouts {
        setup: Duration::from_secs(3),
        start: Duration::from_millis(400),
        remote: Duration::from_secs(2),
        negotiate: Duration::from_millis(500),
    }
}

fn size(width: u32, height: u32) -> PixelSize {
    PixelSize::new(width, height)
}

fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
    PixelRect::new(point2(x0, y0), point2(x1, y1))
}

/// Poll `condition` until it holds; panics with `what` after `WAIT`.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_value<T>(what: &str, mut value: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = value() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Records a capture's events.
#[derive(Default)]
struct Events(Mutex<Vec<FrameEvent>>);

impl EventSink<FrameEvent> for Events {
    fn send(&self, event: FrameEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    fn frames(&self) -> Vec<Frame> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                FrameEvent::Frame { frame, .. } => Some(frame.clone()),
                _ => None,
            })
            .collect()
    }

    fn ended(&self) -> Vec<StreamEndReason> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                FrameEvent::Ended { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    fn last_size(&self) -> Option<PixelSize> {
        self.frames().last().map(|frame| frame.size)
    }
}

fn sink(events: &Arc<Events>) -> Arc<dyn EventSink<FrameEvent>> {
    Arc::clone(events) as Arc<dyn EventSink<FrameEvent>>
}

fn pixel(frame: &Frame, x: usize, y: usize) -> [u8; 4] {
    let (pixels, stride) = frame.cpu_pixels().unwrap();
    let at = y * stride as usize + x * 4;
    [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
}

// ---- no PipeWire: the portal conversation ------------------------------------------------------

#[test]
fn without_a_token_open_is_unsupported_and_asks_the_portal_nothing() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let dir = Dir::new();
    let started = Instant::now();
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some(portal.address()),
        short(),
    );
    assert!(matches!(result, Err(PlatformError::Unsupported(_))));
    assert!(started.elapsed() < Duration::from_millis(300), "no waiting");
    assert_eq!(portal.sessions(), 0);
    assert_eq!(portal.starts(), 0);
    assert!(portal.selects().is_empty());
    assert!(!dir.token_path().exists());

    // The same with no bus to talk to at all: the answer comes before any connection attempt.
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some("unix:path=/nonexistent/crosspane-test-bus".to_owned()),
        short(),
    );
    assert!(matches!(result, Err(PlatformError::Unsupported(_))));
}

#[test]
fn a_closed_gate_and_unusable_sizes_are_refused_before_any_call() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let closed = IoGate::new();
    let open = |gate: &Arc<IoGate>, size: PixelSize| {
        VirtualScreen::open_on(
            Arc::clone(gate),
            dir.config(),
            size,
            Some(portal.address()),
            short(),
        )
    };
    assert!(matches!(
        open(&closed, size(800, 600)),
        Err(PlatformError::Locked)
    ));
    let gate = open_gate();
    for bad in [
        size(0, 600),
        size(800, 0),
        size(16385, 10),
        size(16384, 16384),
    ] {
        assert!(
            matches!(open(&gate, bad), Err(PlatformError::Backend(_))),
            "{bad:?}"
        );
    }
    assert_eq!(portal.sessions(), 0);
    assert_eq!(dir.token().as_deref(), Some("tok-0"));
}

#[test]
fn asks_for_one_hidden_cursor_virtual_source_with_the_stored_token_and_rotates_it() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::GrantVirtual(42)]);
    let dir = Dir::with_token("tok-0");
    // The fake portal's PipeWire remote is a socket nobody serves: nothing is ever negotiated.
    let started = Instant::now();
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some(portal.address()),
        short(),
    );
    assert!(matches!(result, Err(PlatformError::Timeout)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));

    // SourceType::Virtual = 4, CursorMode::Hidden = 1, PersistMode::ExplicitlyRevoked = 2, one
    // source, and the token that was stored.
    assert_eq!(
        portal.selects(),
        vec![Select {
            types: Some(4),
            multiple: Some(false),
            cursor_mode: Some(1),
            persist_mode: Some(2),
            restore_token: Some("tok-0".to_owned()),
        }]
    );
    // Rotated, private to the user.
    assert_eq!(dir.token().as_deref(), Some("tok-1"));
    assert_eq!(mode(&dir.token_path()), 0o600);
    // One PipeWire remote; and the failed open closed its session (which removes the monitor).
    assert_eq!(portal.remotes_opened(), 1);
    assert_eq!(portal.closed_by_client().len(), 1);
}

#[test]
fn a_start_that_never_answers_is_abandoned_and_the_token_forgotten() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::Silent]);
    let dir = Dir::with_token("tok-0");
    let started = Instant::now();
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some(portal.address()),
        short(),
    );
    assert!(
        matches!(result, Err(PlatformError::Unsupported(_))),
        "{result:?}"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(400) && took < Duration::from_secs(4),
        "{took:?}"
    );
    // The dialog is dismissed by closing the session, the token is gone, and PipeWire was never
    // touched.
    assert_eq!(portal.closed_by_client().len(), 1);
    assert!(!dir.token_path().exists());
    assert_eq!(portal.remotes_opened(), 0);
    assert_eq!(portal.starts(), 1);
}

#[test]
fn a_refused_start_forgets_the_token() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    for code in [1, 2] {
        portal.script(&[StartScript::Respond(code)]);
        let dir = Dir::with_token("tok-0");
        let result = VirtualScreen::open_on(
            open_gate(),
            dir.config(),
            size(800, 600),
            Some(portal.address()),
            short(),
        );
        assert!(
            matches!(result, Err(PlatformError::Unsupported(_))),
            "response {code}: {result:?}"
        );
        assert!(!dir.token_path().exists(), "response {code}");
    }
    assert_eq!(portal.remotes_opened(), 0);
}

#[test]
fn a_portal_without_virtual_sources_is_unsupported_before_a_session_exists() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.set_source_types(1 | 2);
    let dir = Dir::with_token("tok-0");
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some(portal.address()),
        short(),
    );
    assert!(matches!(result, Err(PlatformError::Unsupported(_))));
    assert_eq!(portal.sessions(), 0, "no dialog can come of it");
    assert_eq!(dir.token().as_deref(), Some("tok-0"), "the consent is kept");
    // With a token the consent run does nothing and does not even ask...
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_millis(500),
            Some(portal.address())
        ),
        Ok(true)
    ));
    // ...and without one it finds out there is nothing to ask for, and does not open a dialog.
    let none = Dir::new();
    assert!(matches!(
        consent(
            none.token_path(),
            Duration::from_millis(500),
            Some(portal.address())
        ),
        Err(PlatformError::Unsupported(_))
    ));
    assert_eq!(portal.sessions(), 0);
}

#[test]
fn a_stream_that_is_not_virtual_is_refused() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::GrantVirtualAs {
        node: 42,
        source_type: 1,
    }]);
    let dir = Dir::with_token("tok-0");
    let result = VirtualScreen::open_on(
        open_gate(),
        dir.config(),
        size(800, 600),
        Some(portal.address()),
        short(),
    );
    assert!(matches!(result, Err(PlatformError::Unsupported(_))));
    // Never fixate the size of a real monitor's stream.
    assert_eq!(portal.remotes_opened(), 0);
    assert_eq!(portal.closed_by_client().len(), 1);
}

#[test]
fn consent_with_a_token_present_makes_no_call() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let started = Instant::now();
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_secs(5),
            Some(portal.address())
        ),
        Ok(true)
    ));
    assert!(matches!(
        prepare_consent(&dir.config(), Duration::from_secs(5)),
        Ok(true)
    ));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(portal.sessions(), 0);
    assert_eq!(portal.starts(), 0);
    assert_eq!(dir.token().as_deref(), Some("tok-0"));
}

#[test]
fn consent_asks_once_stores_the_token_and_never_opens_pipewire() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::GrantVirtual(7)]);
    let dir = Dir::new();
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_secs(5),
            Some(portal.address())
        ),
        Ok(true)
    ));
    // No token was sent (there was none), and the rest is the screen's request.
    assert_eq!(
        portal.selects(),
        vec![Select {
            types: Some(4),
            multiple: Some(false),
            cursor_mode: Some(1),
            persist_mode: Some(2),
            restore_token: None,
        }]
    );
    assert_eq!(dir.token().as_deref(), Some("tok-1"));
    assert_eq!(mode(&dir.token_path()), 0o600);
    // Closed without a PipeWire remote: no consumer, so Mutter makes no monitor.
    assert_eq!(portal.remotes_opened(), 0);
    assert_eq!(portal.closed_by_client().len(), 1);
    // A second run finds the token and does nothing.
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_secs(5),
            Some(portal.address())
        ),
        Ok(true)
    ));
    assert_eq!(portal.sessions(), 1);
}

#[test]
fn a_no_to_the_consent_dialog_is_false_and_stores_nothing() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::Respond(1)]);
    let dir = Dir::new();
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_secs(5),
            Some(portal.address())
        ),
        Ok(false)
    ));
    assert!(!dir.token_path().exists());
    assert_eq!(portal.closed_by_client().len(), 1);
    assert_eq!(portal.remotes_opened(), 0);
}

#[test]
fn an_unanswered_consent_dialog_is_dismissed_when_the_wait_runs_out() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::Silent]);
    let dir = Dir::new();
    let started = Instant::now();
    assert!(matches!(
        consent(
            dir.token_path(),
            Duration::from_millis(300),
            Some(portal.address())
        ),
        Ok(false)
    ));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(4),
        "{took:?}"
    );
    // Closing the session is what dismisses the dialog.
    assert_eq!(portal.closed_by_client().len(), 1);
    assert!(!dir.token_path().exists());
}

#[test]
fn consent_without_a_portal_is_an_error() {
    let dir = Dir::new();
    let result = consent(
        dir.token_path(),
        Duration::from_millis(300),
        Some("unix:path=/nonexistent/crosspane-test-bus".to_owned()),
    );
    assert!(result.is_err(), "{result:?}");
    assert!(!dir.token_path().exists());
}

// ---- with a private PipeWire server ------------------------------------------------------------

struct ProducerState {
    size: Option<(usize, usize)>,
    painted: Arc<AtomicU64>,
    sizes: Arc<Mutex<Vec<(u32, u32)>>>,
    frozen: Arc<AtomicBool>,
}

/// A `Video/Source` that stands in for Mutter's virtual stream: it accepts any size from 1x1 to
/// 4096x4096 (offering 640x480 when asked to choose), allocates memory-backed buffers with padded
/// rows for whatever size it is given, and paints pixel (x, y) as `[x, y, RED, 0]`. It renegotiates
/// when the consumer's offer changes. Dropping it stops it (and so removes its node).
struct Producer {
    stop: Arc<AtomicBool>,
    painted: Arc<AtomicU64>,
    /// The sizes it was negotiated at, in order.
    sizes: Arc<Mutex<Vec<(u32, u32)>>>,
    /// While set it paints nothing, like a compositor with nothing to repaint.
    frozen: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Producer {
    fn start(fd: OwnedFd) -> Producer {
        let stop = Arc::new(AtomicBool::new(false));
        let painted = Arc::new(AtomicU64::new(0));
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let frozen = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = ProducerState {
                size: None,
                painted: Arc::clone(&painted),
                sizes: Arc::clone(&sizes),
                frozen: Arc::clone(&frozen),
            };
            let stop = Arc::clone(&stop);
            thread::spawn(move || produce(fd, &stop, state))
        };
        Producer {
            stop,
            painted,
            sizes,
            frozen,
            thread: Some(thread),
        }
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }

    fn sizes(&self) -> Vec<(u32, u32)> {
        self.sizes.lock().unwrap().clone()
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.halt();
    }
}

fn serialize(object: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .unwrap()
    .0
    .into_inner()
}

fn stride_for(width: usize) -> usize {
    width * 4 + 16
}

fn produce(fd: OwnedFd, stop: &AtomicBool, state: ProducerState) {
    let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
    let core = context.connect_fd_rc(fd, None).unwrap();
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        PRODUCER,
        pw::properties::properties! {
            *pw::keys::NODE_NAME => PRODUCER,
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .unwrap();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(move |stream, state, id, pod| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let (width, height) = (info.size().width as usize, info.size().height as usize);
            state.size = Some((width, height));
            state
                .sizes
                .lock()
                .unwrap()
                .push((width as u32, height as u32));
            let stride = stride_for(width);
            let buffers = serialize(Object {
                type_: SpaTypes::ObjectParamBuffers.as_raw(),
                id: ParamType::Buffers.as_raw(),
                properties: vec![
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_buffers,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Range {
                                default: 4,
                                min: 2,
                                max: 8,
                            },
                        ))),
                    ),
                    Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_size,
                        Value::Int((stride * height) as i32),
                    ),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_stride,
                        Value::Int(stride as i32),
                    ),
                    Property::new(spa::sys::SPA_PARAM_BUFFERS_align, Value::Int(16)),
                    Property::new(
                        spa::sys::SPA_PARAM_BUFFERS_dataType,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: 1 << spa::sys::SPA_DATA_MemFd,
                                flags: Vec::new(),
                            },
                        ))),
                    ),
                ],
            });
            let header = serialize(Object {
                type_: SpaTypes::ObjectParamMeta.as_raw(),
                id: ParamType::Meta.as_raw(),
                properties: vec![
                    Property::new(
                        spa::sys::SPA_PARAM_META_type,
                        Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
                    ),
                    Property::new(
                        spa::sys::SPA_PARAM_META_size,
                        Value::Int(size_of::<spa::sys::spa_meta_header>() as i32),
                    ),
                ],
            });
            let mut pods = [
                Pod::from_bytes(&buffers).unwrap(),
                Pod::from_bytes(&header).unwrap(),
            ];
            stream.update_params(&mut pods).unwrap();
        })
        .process(|stream, state| {
            if state.frozen.load(Ordering::Acquire) {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some((width, height)) = state.size else {
                return;
            };
            let stride = stride_for(width);
            let [data] = buffer.datas_mut() else { return };
            let Some(bytes) = data.data() else { return };
            if bytes.len() < stride * height {
                return;
            }
            for y in 0..height {
                let row = &mut bytes[y * stride..(y + 1) * stride];
                for x in 0..width {
                    row[x * 4..x * 4 + 4].copy_from_slice(&[x as u8, y as u8, RED, 0x00]);
                }
                row[width * 4..].fill(0xEE);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = (stride * height) as u32;
            *chunk.stride_mut() = stride as i32;
            state.painted.fetch_add(1, Ordering::Relaxed);
        })
        .register()
        .unwrap();
    let format = serialize(pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 640,
                height: 480
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 30, denom: 1 }
        ),
    ));
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [Pod::from_bytes(&format).unwrap()],
        )
        .unwrap();
    while !stop.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
    }
}

/// The private server, the producer, the linker that stands in for the session manager and the
/// fake portal that grants the producer's node. Fields drop in order: the producer goes before the
/// server it lives on.
struct Rig {
    producer: Producer,
    _linker: Linker,
    portal: Portal,
    node: u32,
    _server: Server,
}

impl Rig {
    fn start() -> Option<Rig> {
        let server = Server::start()?;
        let portal = Portal::start(4)?;
        let producer = Producer::start(server.connection());
        let linker = Linker::start(server.connection(), PRODUCER, STREAM_NAME);
        let node = wait_for_value("the producer's node", || linker.node(PRODUCER));
        portal.serve_remote(server.socket());
        portal.script(&[StartScript::GrantVirtual(node)]);
        Some(Rig {
            producer,
            _linker: linker,
            portal,
            node,
            _server: server,
        })
    }

    /// Another grant of the same node, for a second screen.
    fn grant_again(&self) {
        self.portal.script(&[StartScript::GrantVirtual(self.node)]);
    }

    fn open(&self, dir: &Dir, gate: &Arc<IoGate>, wanted: PixelSize) -> VirtualScreen {
        VirtualScreen::open_on(
            Arc::clone(gate),
            dir.config(),
            wanted,
            Some(self.portal.address()),
            Timeouts::default(),
        )
        .unwrap()
    }
}

const DISPLAY: CaptureTarget = CaptureTarget::Display(DisplayId(7));

#[test]
fn opens_at_the_offered_size_and_serves_cropped_frames() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    let mut screen = rig.open(&dir, &gate, size(320, 200));
    assert_eq!(screen.size(), size(320, 200));
    assert!(screen.is_live());
    // The producer was given exactly the offered size, though it would have taken 640x480.
    wait_for("the producer to be negotiated", || {
        !rig.producer.sizes().is_empty()
    });
    assert_eq!(rig.producer.sizes(), vec![(320, 200)]);
    assert_eq!(dir.token().as_deref(), Some("tok-1"));

    let events = Arc::new(Events::default());
    let stream = screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
    wait_for("a frame", || !events.frames().is_empty());
    let frame = events.frames()[0].clone();
    assert_eq!(frame.size, size(320, 200));
    // Tight rows, every pixel where it belongs, opaque alpha.
    assert_eq!(frame.cpu_pixels().unwrap().1, 320 * 4);
    for (x, y) in [(0, 0), (1, 0), (0, 1), (319, 199), (17, 130), (255, 0)] {
        assert_eq!(
            pixel(&frame, x, y),
            [x as u8, y as u8, RED, 0xFF],
            "({x}, {y})"
        );
    }
    assert_eq!(frame.damage, None, "the first frame has no baseline");

    // A crop moves on the running stream.
    screen.set_crop(stream, Some(rect(8, 4, 24, 12))).unwrap();
    wait_for("a cropped frame", || {
        events.last_size() == Some(size(16, 8))
    });
    let frame = events.frames().last().unwrap().clone();
    assert_eq!(pixel(&frame, 0, 0), [8, 4, RED, 0xFF]);
    assert_eq!(pixel(&frame, 15, 7), [23, 11, 0x77, 0xFF]);

    screen.stop(stream).unwrap();
    assert_eq!(events.ended(), vec![StreamEndReason::Requested]);
    // Stopping again, or an unknown stream, is fine and says nothing more.
    screen.stop(stream).unwrap();
    screen.stop(StreamId(99)).unwrap();
    assert!(matches!(
        screen.set_crop(stream, None),
        Err(PlatformError::NotFound)
    ));
    assert_eq!(events.ended().len(), 1);
}

#[test]
fn start_refuses_what_it_cannot_capture() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    let mut screen = rig.open(&dir, &gate, size(64, 48));
    let events = Arc::new(Events::default());
    // Windows are the Shell bridge's, and a display id is whatever the caller routes.
    assert!(matches!(
        screen.start(CaptureTarget::Window(WindowId(1)), None, 30, sink(&events)),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        screen.start(DISPLAY, None, 0, sink(&events)),
        Err(PlatformError::Backend(_))
    ));
    assert!(matches!(
        screen.start(DISPLAY, Some(rect(5, 5, 5, 9)), 30, sink(&events)),
        Err(PlatformError::Backend(_))
    ));
    assert!(matches!(
        screen.set_crop(StreamId(1), Some(rect(5, 5, 5, 9))),
        Err(PlatformError::Backend(_))
    ));
    gate.set_engine_permits(false);
    assert!(matches!(
        screen.start(DISPLAY, None, 30, sink(&events)),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        screen.resize(size(128, 96)),
        Err(PlatformError::Locked)
    ));
    assert!(events.ended().is_empty());
    gate.set_engine_permits(true);
    for any in [DisplayId(1), DisplayId(7), DisplayId(1000)] {
        screen
            .start(CaptureTarget::Display(any), None, 30, sink(&events))
            .unwrap();
    }
}

#[test]
fn a_capture_started_on_a_static_screen_still_gets_a_frame() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    let mut screen = rig.open(&dir, &gate, size(64, 48));
    let first = Arc::new(Events::default());
    screen.start(DISPLAY, None, 30, sink(&first)).unwrap();
    wait_for("a frame", || !first.frames().is_empty());

    // Nothing is repainted any more (a compositor sends buffers on damage only)...
    rig.producer.frozen.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(300));
    let painted = rig.producer.painted.load(Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.producer.painted.load(Ordering::Relaxed), painted);

    // ...and a second capture, with its own crop and rate, still starts with the newest image.
    let second = Arc::new(Events::default());
    screen
        .start(DISPLAY, Some(rect(8, 4, 24, 12)), 5, sink(&second))
        .unwrap();
    wait_for("the second capture's first frame", || {
        !second.frames().is_empty()
    });
    let frame = second.frames()[0].clone();
    assert_eq!(frame.size, size(16, 8));
    assert_eq!(pixel(&frame, 0, 0), [8, 4, RED, 0xFF]);
    assert_eq!(frame.damage, None);
    // The first capture was not disturbed.
    assert!(first.ended().is_empty());

    // And a crop change on a static screen is served from the same image.
    let stream = StreamId(2);
    screen.set_crop(stream, Some(rect(0, 0, 4, 4))).unwrap();
    wait_for("the new crop", || second.last_size() == Some(size(4, 4)));
}

#[test]
fn a_closed_gate_ends_the_captures_blocked_and_the_screen_lives_on() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    let mut screen = rig.open(&dir, &gate, size(64, 48));
    let events = Arc::new(Events::default());
    screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
    screen
        .start(DISPLAY, Some(rect(0, 0, 8, 8)), 30, sink(&events))
        .unwrap();
    wait_for("a frame", || !events.frames().is_empty());

    gate.set_engine_permits(false);
    wait_for("both captures to end", || events.ended().len() == 2);
    assert_eq!(
        events.ended(),
        vec![StreamEndReason::Blocked, StreamEndReason::Blocked]
    );
    let frames = events.frames().len();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(events.frames().len(), frames, "no frame after the end");
    // The monitor itself stays: this is the gate, not the screen.
    assert!(screen.is_live());

    gate.set_engine_permits(true);
    let again = Arc::new(Events::default());
    screen.start(DISPLAY, None, 30, sink(&again)).unwrap();
    wait_for("frames again", || !again.frames().is_empty());
}

#[test]
fn resize_renegotiates_the_stream_and_keeps_the_captures_running() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    let mut screen = rig.open(&dir, &gate, size(320, 200));
    let events = Arc::new(Events::default());
    let stream = screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
    wait_for("a frame", || !events.frames().is_empty());
    assert_eq!(events.last_size(), Some(size(320, 200)));

    // The same size is no renegotiation at all.
    screen.resize(size(320, 200)).unwrap();
    assert_eq!(rig.producer.sizes(), vec![(320, 200)]);

    let started = Instant::now();
    screen.resize(size(480, 270)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(screen.size(), size(480, 270));
    assert!(screen.is_live());
    wait_for("the producer to follow", || {
        rig.producer.sizes().last() == Some(&(480, 270))
    });
    // The running capture goes on, with whole frames at the new size.
    wait_for("a frame at the new size", || {
        events.last_size() == Some(size(480, 270))
    });
    let frame = events.frames().last().unwrap().clone();
    assert_eq!(pixel(&frame, 479, 269), [223, 13, RED, 0xFF]);
    assert_eq!(frame.damage, None, "a new size starts from a whole frame");
    assert!(events.ended().is_empty());

    // Shrinking works as well, and the crop is still the caller's to move.
    screen.resize(size(160, 100)).unwrap();
    screen.set_crop(stream, Some(rect(0, 0, 10, 10))).unwrap();
    wait_for("a cropped frame at the small size", || {
        events.last_size() == Some(size(10, 10))
    });
    assert_eq!(screen.size(), size(160, 100));

    // Unusable sizes are refused without touching the stream.
    assert!(matches!(
        screen.resize(size(0, 100)),
        Err(PlatformError::Backend(_))
    ));
    assert_eq!(screen.size(), size(160, 100));
}

#[test]
fn dropping_the_screen_closes_the_session_and_ends_the_captures_requested() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let lost = Arc::new(AtomicU32::new(0));
    let events = Arc::new(Events::default());
    {
        let mut screen = rig.open(&dir, &open_gate(), size(64, 48));
        let lost = Arc::clone(&lost);
        screen.on_lost(Arc::new(move || {
            lost.fetch_add(1, Ordering::SeqCst);
        }));
        screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
        wait_for("a frame", || !events.frames().is_empty());
        assert!(rig.portal.closed_by_client().is_empty());
    }
    // The session is closed (that is what removes the monitor), the capture ended as asked, and
    // a drop is not a loss.
    assert_eq!(rig.portal.closed_by_client().len(), 1);
    assert_eq!(events.ended(), vec![StreamEndReason::Requested]);
    assert_eq!(lost.load(Ordering::SeqCst), 0);
}

#[test]
fn stopping_the_share_loses_the_screen_once() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let mut screen = rig.open(&dir, &open_gate(), size(64, 48));
    let lost = Arc::new(AtomicU32::new(0));
    {
        let lost = Arc::clone(&lost);
        screen.on_lost(Arc::new(move || {
            lost.fetch_add(1, Ordering::SeqCst);
        }));
    }
    let events = Arc::new(Events::default());
    screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
    wait_for("a frame", || !events.frames().is_empty());

    // The user presses Stop in the top bar.
    rig.portal.emit_closed();
    wait_for("the capture to end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![StreamEndReason::TargetGone]);
    wait_for("the callback", || lost.load(Ordering::SeqCst) == 1);
    assert!(!screen.is_live());
    // Everything that needs the screen says it is gone; the portal closed the session itself, so
    // there is nothing for us to close.
    assert!(matches!(
        screen.start(DISPLAY, None, 30, sink(&events)),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        screen.resize(size(128, 96)),
        Err(PlatformError::NotFound)
    ));
    // A callback registered after the loss runs at once.
    let late = Arc::new(AtomicU32::new(0));
    {
        let late = Arc::clone(&late);
        screen.on_lost(Arc::new(move || {
            late.fetch_add(1, Ordering::SeqCst);
        }));
    }
    assert_eq!(late.load(Ordering::SeqCst), 1);
    drop(screen);
    assert_eq!(lost.load(Ordering::SeqCst), 1, "once");
    assert!(rig.portal.closed_by_client().is_empty());
    assert_eq!(events.ended().len(), 1);
}

#[test]
fn a_node_that_goes_away_loses_the_screen() {
    let Some(mut rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let mut screen = rig.open(&dir, &open_gate(), size(64, 48));
    let lost = Arc::new(AtomicU32::new(0));
    {
        let lost = Arc::clone(&lost);
        screen.on_lost(Arc::new(move || {
            lost.fetch_add(1, Ordering::SeqCst);
        }));
    }
    let events = Arc::new(Events::default());
    screen.start(DISPLAY, None, 30, sink(&events)).unwrap();
    wait_for("a frame", || !events.frames().is_empty());

    rig.producer.halt();
    wait_for("the capture to end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![StreamEndReason::TargetGone]);
    wait_for("the callback", || lost.load(Ordering::SeqCst) == 1);
    assert!(!screen.is_live());
    // Here the portal session is still ours to close.
    wait_for("the session to be closed", || {
        rig.portal.closed_by_client().len() == 1
    });
    drop(screen);
    assert_eq!(lost.load(Ordering::SeqCst), 1);
}

#[test]
fn a_second_screen_opens_with_the_rotated_token() {
    let Some(rig) = Rig::start() else {
        return;
    };
    let dir = Dir::with_token("tok-0");
    let gate = open_gate();
    {
        let _screen = rig.open(&dir, &gate, size(64, 48));
        assert_eq!(dir.token().as_deref(), Some("tok-1"));
    }
    wait_for("the monitor's session to be closed", || {
        rig.portal.closed_by_client().len() == 1
    });
    rig.grant_again();
    // The same size: the synthetic producer keeps the format it was first linked with (a real
    // portal session has a node of its own each time).
    let _again = rig.open(&dir, &gate, size(64, 48));
    // The second start asked with the token the first one left, and left a new one.
    let selects = rig.portal.selects();
    assert_eq!(selects.len(), 2);
    assert_eq!(selects[0].restore_token.as_deref(), Some("tok-0"));
    assert_eq!(selects[1].restore_token.as_deref(), Some("tok-1"));
    assert_eq!(dir.token().as_deref(), Some("tok-2"));
}
