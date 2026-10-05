//! Owned synthetic source for WP-W2.5a. Never linked into the shipping agent.
//! Runs only as a Limited win-gui child with an explicitly isolated fixture root.
#![cfg(all(windows, feature = "video"))]
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

// Reuse the production private-file adapter for the two disposable identities/trust files.
#[path = "../src/windows/security.rs"]
pub(crate) mod fixture_security;
mod windows {
    pub(crate) use crate::fixture_security as security;
}
#[path = "../src/paths.rs"]
mod paths;

use crosspane_engine::{Command, Engine, EngineConfig, InjectCmd, Input, Notice, Output};
use crosspane_input::journal::MemoryJournal;
use crosspane_media::{
    codec::VideoEncoder,
    wire::{FrameHeader, write_video},
};
use crosspane_platform::{
    LockState, Parked, ParkingKind, SessionEvent, SessionState, StreamId, WindowEvent, WindowInfo,
    WindowRole, WindowState,
};
use crosspane_protocol::{
    link::LinkEvent,
    msg::{Capability, ControlMessage, Hello},
    projection::ProjectionEndReason,
};
use crosspane_security::{
    identity::DeviceIdentity,
    trust::{PeerEntry, TrustStore, default_grants},
};
use crosspane_transport::{PinStore, Transport, TransportConfig};
use crosspane_types::{
    color::ColorSpace,
    display::DisplayInfo,
    geom::{DisplayGeometry, PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, SizeMm},
    id::{DisplayId, NodeId, ProjectionId, WindowId},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const DISPLAY: DisplayId = DisplayId(42);
const WINDOW: WindowId = WindowId(1);
fn now() -> crosspane_types::time::MonoTime {
    crosspane_platform_windows::clock::now()
}
struct Pins(TrustStore);
impl PinStore for Pins {
    fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        self.0.trusted(spki)
    }
}
fn pin(identity: &DeviceIdentity) -> TrustStore {
    let mut store = TrustStore::new();
    store
        .pin(PeerEntry {
            node: identity.node(),
            spki: identity.spki().to_vec(),
            name: "owned WP-W2.5a fixture".into(),
            granted: default_grants(),
            paired_at_ms: 1,
        })
        .unwrap();
    store
}
#[derive(Default, serde::Serialize)]
struct Counters {
    connected: bool,
    started: u32,
    encoded: u64,
    key_down: u32,
    key_up: u32,
    restores: u32,
    returned: bool,
    clean: bool,
}
struct Source {
    engine: Engine,
    transport: Transport,
    destination: NodeId,
    root: PathBuf,
    counts: Counters,
    size: PixelSize,
    projection: Option<ProjectionId>,
    force_key: bool,
}
impl Source {
    fn record(&self) {
        paths::write_private(
            &self.root.join("fixture/source-progress.json"),
            &serde_json::to_vec(&self.counts).unwrap(),
        )
        .unwrap();
    }
    fn feed(&mut self, input: Input) {
        let mut pending = VecDeque::from(self.engine.handle(input, now()));
        while let Some(output) = pending.pop_front() {
            let follow = match output {
                Output::SendControl { peer, msg } => {
                    assert_eq!(peer, self.destination);
                    self.transport
                        .link(peer)
                        .unwrap()
                        .send_control(&msg)
                        .unwrap();
                    None
                }
                Output::SendInput { peer, msg } => {
                    assert_eq!(peer, self.destination);
                    self.transport.link(peer).unwrap().send_input(&msg).unwrap();
                    None
                }
                Output::Inject { id, cmd } => {
                    // Fake injector records only its own test's counts. It never calls SendInput.
                    match cmd {
                        InjectCmd::Key { down: true, .. } => self.counts.key_down += 1,
                        InjectCmd::Key { down: false, .. } => self.counts.key_up += 1,
                        _ => {}
                    }
                    Some(Input::InjectDone { id, ok: true })
                }
                Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                    assert_eq!(window, WINDOW);
                    self.size = size;
                    self.force_key = true;
                    Some(Input::Parked {
                        window,
                        result: Ok(Parked {
                            window,
                            kind: ParkingKind::Mirror,
                            display: DISPLAY,
                            fullscreen: false,
                            content: PixelRect::new(
                                (0, 0).into(),
                                (
                                    i32::try_from(size.width).unwrap(),
                                    i32::try_from(size.height).unwrap(),
                                )
                                    .into(),
                            ),
                        }),
                    })
                }
                Output::ActivateWindow { window } => {
                    assert_eq!(window, WINDOW);
                    // Observation belongs to the synthetic WindowSource, preserving the focus guard.
                    Some(Input::Windows(WindowEvent::Focused(Some(window))))
                }
                Output::StartCapture {
                    projection, peer, ..
                } => {
                    assert_eq!(peer, self.destination);
                    self.projection = Some(projection);
                    Some(Input::CaptureStarted {
                        projection,
                        result: Ok(StreamId(1)),
                    })
                }
                Output::RequestKeyFrame { .. } => {
                    self.force_key = true;
                    None
                }
                Output::StopCapture { .. } => {
                    self.projection = None;
                    None
                }
                Output::Restore { window, .. } => {
                    assert_eq!(window, WINDOW);
                    self.counts.restores += 1;
                    None
                }
                Output::Notice(Notice::ProjectionStarted { .. }) => {
                    self.counts.started += 1;
                    None
                }
                Output::Notice(Notice::ProjectionEnded { reason, .. }) => {
                    assert_eq!(reason, ProjectionEndReason::Returned);
                    self.counts.returned = true;
                    None
                }
                Output::Notice(Notice::ProjectionRefused { .. }) => {
                    panic!("owned projection refused")
                }
                Output::OpenProxy { .. } => panic!("synthetic source must never open a proxy"),
                _ => None,
            };
            if let Some(input) = follow {
                pending.extend(self.engine.handle(input, now()));
            }
        }
        self.record();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_e2_source() {
    if std::env::var("CROSSPANE_E2_SOURCE_FIXTURE").as_deref() != Ok("1") {
        eprintln!("SKIP owned E2 GUI fixture: explicit Limited harness required");
        return;
    }
    assert!(
        !fixture_security::is_elevated().unwrap(),
        "fixture must be Limited"
    );
    let root = PathBuf::from(std::env::var_os("CROSSPANE_E2_FIXTURE_ROOT").unwrap());
    let suffix = root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("crosspane-WP-W2.5a-")
        .unwrap();
    assert!(suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        root.parent().unwrap().canonicalize().unwrap(),
        std::env::temp_dir().canonicalize().unwrap()
    );
    assert_eq!(
        PathBuf::from(std::env::var_os("APPDATA").unwrap()),
        root.join("roaming")
    );
    let config = root.join("roaming/Crosspane");
    let state = root.join("local/Crosspane");
    paths::create_private_dir(&config).unwrap();
    paths::create_private_dir(&state).unwrap();
    paths::create_private_dir(&root.join("fixture")).unwrap();
    let source_identity = Arc::new(DeviceIdentity::generate().unwrap());
    let destination_identity = DeviceIdentity::generate().unwrap();
    paths::write_private(&state.join("device-key.pk8"), destination_identity.pkcs8()).unwrap();
    paths::write_private(
        &config.join("trust.json"),
        pin(&source_identity).to_json().as_bytes(),
    )
    .unwrap();
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let transport = Transport::bind(
        TransportConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            identity: source_identity.clone(),
            pins: Arc::new(Pins(pin(&destination_identity))),
            hello: Hello {
                minor: 0,
                name: "owned WP-W2.5a source".into(),
                features: vec!["e1".into(), "h264".into()],
                displays: Vec::new(),
            },
        },
        Arc::new(move |event| {
            let _ = events.send(event);
        }),
    )
    .unwrap();
    let port = transport.local_addr().port();
    paths::write_private(&config.join("config.toml"), format!(
        "name = \"wp-w2-5a-dst-{suffix}\"\nport = 0\nacceptance_bind_ip = \"127.0.0.1\"\nforce_file_keystore = true\ncrossing = false\nlatency_overlay = false\npeers = [{{ addr = \"127.0.0.1:{port}\" }}]\n[drag]\nacross = false\n"
    ).as_bytes()).unwrap();
    let (engine, _) = Engine::new(
        EngineConfig::new(source_identity.node()),
        Box::<MemoryJournal>::default(),
        Box::<MemoryJournal>::default(),
        now(),
    )
    .unwrap();
    let mut source = Source {
        engine,
        transport,
        destination: destination_identity.node(),
        root: root.clone(),
        counts: Counters::default(),
        size: PixelSize::new(320, 240),
        projection: None,
        force_key: true,
    };
    source.feed(Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Unlocked,
        active: Some(true),
    })));
    source.feed(Input::LocalDisplays(vec![DisplayInfo {
        id: DISPLAY,
        name: "synthetic source".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(300.0, 200.0),
            pixel_size: PixelSize::new(1920, 1080),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }]));
    source.feed(Input::Grants(
        [(
            destination_identity.node(),
            [Capability::WindowShare].into(),
        )]
        .into(),
    ));
    source.feed(Input::Windows(WindowEvent::Added(WindowInfo {
        id: WINDOW,
        title: "WP-W2.5a owned synthetic content".into(),
        app_id: "crosspane-test-fixture".into(),
        pid: None,
        display: Some(DISPLAY),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.0, 240.0)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    })));
    source.feed(Input::Windows(WindowEvent::Focused(Some(WINDOW))));
    let native_displays = crosspane_platform_windows::displays::WindowsDisplays::new().unwrap();
    let snapshot = native_displays.snapshot().unwrap();
    let mut ids = snapshot.ids;
    let monitors: Vec<_> = snapshot.probes.iter().filter(|probe| !probe.twin).map(|probe| {
        serde_json::json!({"id": ids.assign(&probe.device_path).unwrap().0, "native_id": probe.name})
    }).collect();
    paths::write_private(
        &root.join("fixture/source-ready.json"),
        &serde_json::to_vec(&serde_json::json!({"ready":true, "monitors":monitors})).unwrap(),
    )
    .unwrap();
    drop(native_displays);

    let codecs = crosspane_platform_windows::video::MfCodecs::new();
    let mut encoder = codecs.encoder_cpu(source.size, 6_000_000, 10).unwrap();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seq = 0;
    while !source.counts.returned {
        assert!(Instant::now() < deadline, "owned E2 source timed out");
        tokio::select! {
            Some(event) = receiver.recv() => {
                match event {
                    LinkEvent::Control { peer, msg: ControlMessage::Hello(hello) } => {
                        assert_eq!(peer, source.destination);
                        assert!(hello.features.iter().any(|f| f == "h264"), "real destination must advertise H264");
                        source.counts.connected = true;
                        source.feed(Input::PeerUp { peer });
                        source.feed(Input::PeerDisplays { peer, displays: hello.displays });
                        source.feed(Input::Command(Command::Project { window: WINDOW, to: peer, place: None }));
                    }
                    LinkEvent::Control { peer, msg: ControlMessage::Ping { t0 } } => {
                        source.transport.link(peer).unwrap().send_control(&ControlMessage::Pong { t0, t1: now().as_nanos(), t2: now().as_nanos() }).unwrap();
                    }
                    LinkEvent::Closed { .. } => panic!("owned destination connection closed before return"),
                    event => source.feed(Input::Link(event)),
                }
            }
            _ = tick.tick() => {
                source.feed(Input::Tick);
                if let Some(projection) = source.projection {
                    let size = source.size;
                    let mut pixels = Vec::with_capacity((size.width * size.height * 4) as usize);
                    for y in 0..size.height { for x in 0..size.width {
                        let colour = match (x < size.width / 2, y < size.height / 2) {
                            (true, true) => [20, 20, 220, 255], (false, true) => [20, 220, 20, 255],
                            (true, false) => [220, 20, 20, 255], (false, false) => [220, 220, 220, 255],
                        }; pixels.extend_from_slice(&colour);
                    }}
                    let mut access_unit = Vec::new();
                    let encoded = encoder.encode(&pixels, size.width * 4, size, source.force_key, &mut access_unit).unwrap();
                    source.force_key = false; seq += 1;
                    let mut frame = Vec::new();
                    write_video(FrameHeader { projection: projection.0, seq, key: encoded.key,
                        captured_ns: now().as_nanos(), width: size.width, height: size.height }, &access_unit, &mut frame).unwrap();
                    source.transport.send_media(source.destination, frame.into()).unwrap();
                    source.counts.encoded += 1; source.record();
                }
            }
        }
    }
    assert!(source.counts.key_down > 0 && source.counts.key_up > 0);
    assert_eq!(source.counts.started, 1);
    assert_eq!(source.counts.restores, 1);
    source.transport.shutdown("owned fixture complete").await;
    eprintln!(
        "owned source encoded={}; input_down={}; input_up={}; close_returned=true; restores=1; encoder={}",
        source.counts.encoded,
        source.counts.key_down,
        source.counts.key_up,
        encoder.name()
    );
    encoder.close().unwrap();
    source.counts.clean = true;
    source.record();
}
