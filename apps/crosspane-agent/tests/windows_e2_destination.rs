//! Test-only E2 destination and owned source-window process for WP-W2.5b.
//! Only explicit Limited scratch orchestration may run this harness; no shipping fake.
#![cfg(all(windows, feature = "video"))]
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

#[path = "../src/windows/security.rs"]
pub(crate) mod fixture_security;
mod windows {
    pub(crate) use crate::fixture_security as security;
}
#[path = "windows_e2_destination/owned_window.rs"]
mod owned_window;
#[path = "../src/paths.rs"]
mod paths;

use crosspane_engine::{Engine, EngineConfig, Input, Notice, Output, ProjectionKey, ProxyEvent};
use crosspane_input::journal::MemoryJournal;
use crosspane_media::{
    codec::VideoDecoder,
    tiles::TileDecoder,
    wire::{Codec, read_codec, read_cursor, read_header, read_video_region},
};
use crosspane_platform::{LockState, SessionEvent, SessionState};
use crosspane_protocol::{
    link::LinkEvent,
    msg::{Capability, ControlMessage, Hello, InputMessage},
    projection::{ParkingKind, ProjInput, ProjectionEndReason, ProjectionMessage},
};
use crosspane_security::{
    identity::DeviceIdentity,
    trust::{PeerEntry, TrustStore},
};
use crosspane_transport::{PinStore, Transport, TransportConfig};
use crosspane_types::{geom::PixelSize, id::NodeId};
use owned_window::OwnedWindow;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn now() -> crosspane_types::time::MonoTime {
    crosspane_platform_windows::clock::now()
}
struct Pins(TrustStore);
impl PinStore for Pins {
    fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        self.0.trusted(spki)
    }
}
fn pin(identity: &DeviceIdentity, grant: Capability) -> TrustStore {
    let mut store = TrustStore::new();
    store
        .pin(PeerEntry {
            node: identity.node(),
            spki: identity.spki().to_vec(),
            name: "owned WP-W2.5b peer".into(),
            granted: [grant].into(),
            paired_at_ms: 1,
        })
        .unwrap();
    store
}
fn write(root: &Path, name: &str, value: &impl serde::Serialize) {
    paths::write_private(
        &root.join("fixture").join(name),
        &serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}
fn bytes(path: &Path, maximum: usize) -> Option<Vec<u8>> {
    let _pin = fixture_security::private_file(path).unwrap()?;
    let mut value = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .take(maximum as u64 + 1)
        .read_to_end(&mut value)
        .unwrap();
    assert!(
        value.len() <= maximum,
        "owned fixture file exceeded its bound"
    );
    Some(value)
}
async fn ctl(request: Value) -> anyhow::Result<Value> {
    // The production facade creates its own runtime; keep it off this async runtime thread.
    let response =
        tokio::task::spawn_blocking(move || crosspanectl::windows_ctl::exchange(&request))
            .await??;
    anyhow::ensure!(response["ok"] == true, "owned control request refused");
    Ok(response["result"].clone())
}
fn extent(bounds: [i32; 4]) -> PixelSize {
    let w = i64::from(bounds[2]) - i64::from(bounds[0]);
    let h = i64::from(bounds[3]) - i64::from(bounds[1]);
    assert!(w > 0 && h > 0 && w <= 16384 && h <= 16384);
    PixelSize::new(w as u32, h as u32)
}
fn journal_empty(state: &Path) -> bool {
    use crosspane_platform_windows::{
        model::parking::{COMMITTED_NAME, Journal, MAX_BYTES, PENDING_NAME},
        parking::MirrorJournalImages,
    };
    let images = MirrorJournalImages {
        committed: bytes(&state.join(COMMITTED_NAME), MAX_BYTES),
        pending: bytes(&state.join(PENDING_NAME), MAX_BYTES),
    };
    Journal::load(&images).unwrap().0.entries().is_empty()
}

#[derive(Default, serde::Serialize)]
struct Counts {
    connected: bool,
    tiles: u64,
    video: u64,
    tile_patterns: u64,
    video_patterns: u64,
    video_variants: u8,
    late_media: u64,
    decode_errors: u64,
    cursors: u64,
    cursor_default: u64,
    cursor_hidden: u64,
    geometry: u64,
    empty_held_heartbeats: u64,
    resize_requests: Vec<u32>,
    resize_answers: Vec<u32>,
    returned: bool,
}
struct Destination {
    engine: Engine,
    transport: Transport,
    source: NodeId,
    root: PathBuf,
    counts: Counts,
    key: Option<ProjectionKey>,
    geometry: Option<(u32, PixelSize)>,
    tiles: TileDecoder,
    video: Box<dyn VideoDecoder>,
    canvas: Vec<u8>,
    frame_size: PixelSize,
    last_seq: u64,
    frame_geometry: Option<PixelSize>,
    closing: bool,
}
impl Destination {
    fn record(&self) {
        write(&self.root, "destination-progress.json", &self.counts);
    }
    fn feed(&mut self, input: Input) {
        let mut pending = VecDeque::from(self.engine.handle(input, now()));
        while let Some(output) = pending.pop_front() {
            let follow = match output {
                Output::SendControl { peer, msg } => {
                    assert_eq!(peer, self.source);
                    if let ControlMessage::Projection(ProjectionMessage::Resize {
                        request, ..
                    }) = &msg
                    {
                        self.counts.resize_requests.push(*request);
                    }
                    self.transport
                        .link(peer)
                        .unwrap()
                        .send_control(&msg)
                        .unwrap();
                    None
                }
                Output::OpenProxy {
                    key, size, place, ..
                } => {
                    assert_eq!(key.source, self.source);
                    assert!(self.key.is_none() && place.is_none());
                    self.key = Some(key);
                    Some(Input::ProxyOpened {
                        key,
                        result: Ok((size, 1.0)),
                    })
                }
                Output::ProxyGeometry { key, size, parking } => {
                    assert_eq!(Some(key), self.key);
                    assert_eq!(parking, ParkingKind::Mirror);
                    self.counts.geometry += 1;
                    Some(Input::Proxy {
                        key,
                        event: ProxyEvent::Resized { size, scale: 1.0 },
                    })
                }
                Output::CloseProxy { key } => {
                    assert_eq!(Some(key), self.key);
                    None
                }
                Output::Notice(Notice::ProjectionEnded { reason, .. }) => {
                    assert_eq!(reason, ProjectionEndReason::Returned);
                    self.counts.returned = true;
                    None
                }
                Output::Notice(Notice::ProjectionRefused { .. }) => {
                    panic!("owned source projection refused")
                }
                Output::SendInput { peer, msg } => {
                    // Production emits an empty lease heartbeat even without authored input.
                    // Admit only the current owned projection; every input-bearing variant refuses.
                    let empty_lease = peer == self.source
                        && matches!(&msg, InputMessage::Proj(ProjInput::Held {
                            projection, keys, buttons, ..
                        }) if self.key.is_some_and(|key| key.source == peer && key.projection == *projection)
                            && keys.is_empty() && buttons.is_empty());
                    assert!(empty_lease, "destination refused Output::SendInput");
                    self.counts.empty_held_heartbeats += 1;
                    self.transport.link(peer).unwrap().send_input(&msg).unwrap();
                    None
                }
                Output::StartCapture { .. } => panic!("destination refused Output::StartCapture"),
                Output::Park { .. } => panic!("destination refused Output::Park"),
                Output::ResizeParked { .. } => panic!("destination refused Output::ResizeParked"),
                _ => None,
            };
            if let Some(input) = follow {
                pending.extend(self.engine.handle(input, now()));
            }
        }
        self.record();
    }
    fn pattern(&self, tolerance: u8) -> Option<u8> {
        let size = self.frame_size;
        if size.width < 4 || size.height < 4 {
            return None;
        }
        let static_bgra = [[20, 20, 220], [20, 220, 20], [220, 20, 20], [220, 220, 220]];
        let moving_bgra = [[20, 220, 20], [20, 20, 220], [220, 220, 220], [220, 20, 20]];
        let matches = |palette: [[u8; 3]; 4]| {
            [(1, 1), (3, 1), (1, 3), (3, 3)]
                .iter()
                .zip(palette)
                .all(|((x, y), colour)| {
                    let offset =
                        (((size.height * y / 4) * size.width + size.width * x / 4) * 4) as usize;
                    self.canvas.get(offset..offset + 3).is_some_and(|p| {
                        p.iter()
                            .zip(colour)
                            .all(|(v, w)| v.abs_diff(w) <= tolerance)
                    })
                })
        };
        if matches(static_bgra) {
            Some(1)
        } else if matches(moving_bgra) {
            Some(2)
        } else {
            None
        }
    }
    fn media(&mut self, data: &[u8]) {
        let header = read_header(data).unwrap();
        assert_eq!(header.projection, self.key.unwrap().projection.0);
        if self.closing {
            // Retired projection: queued transport bytes are counted but never decoded/presented.
            self.counts.late_media += 1;
            return;
        }
        match read_codec(data).unwrap() {
            Codec::Cursor => {
                let cursor = read_cursor(data).unwrap();
                self.counts.cursors += 1;
                self.counts.cursor_default += u64::from(cursor.default);
                self.counts.cursor_hidden += u64::from(
                    !cursor.default && cursor.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0),
                );
            }
            codec => {
                if header.seq <= self.last_seq {
                    return;
                }
                let size = PixelSize::new(header.width, header.height);
                let mut video_covers_samples = false;
                let decoded = match codec {
                    Codec::Tiles => self
                        .tiles
                        .apply(data)
                        .map(|_| {
                            let (pixels, actual) = self.tiles.canvas();
                            assert_eq!(actual, size);
                            self.canvas = pixels.to_vec();
                            self.frame_size = size;
                            self.counts.tiles += 1;
                        })
                        .map_err(|e| e.to_string()),
                    Codec::H264 => {
                        let (_, region, access_unit) = read_video_region(data).unwrap();
                        let mut picture = Vec::new();
                        self.video
                            .decode(access_unit, &mut picture)
                            .map(|coded| {
                                let (x, y, w, h) = region
                                    .map_or((0, 0, size.width, size.height), |r| {
                                        (r.x, r.y, r.width, r.height)
                                    });
                                assert!(coded.width >= w && coded.height >= h);
                                video_covers_samples =
                                    [(1, 1), (3, 1), (1, 3), (3, 3)].iter().all(|(sx, sy)| {
                                        let (px, py) = (size.width * sx / 4, size.height * sy / 4);
                                        px >= x && px < x + w && py >= y && py < y + h
                                    });
                                if self.frame_size != size {
                                    self.canvas = vec![0; (size.width * size.height * 4) as usize];
                                    self.frame_size = size;
                                }
                                for row in 0..h {
                                    let start = ((y + row) * size.width + x) as usize * 4;
                                    let source = (row * coded.width) as usize * 4;
                                    self.canvas[start..start + w as usize * 4]
                                        .copy_from_slice(&picture[source..source + w as usize * 4]);
                                }
                                self.counts.video += 1;
                            })
                            .map_err(|e| e.to_string())
                    }
                    Codec::Cursor => unreachable!(),
                };
                if decoded.is_err() {
                    self.counts.decode_errors += 1;
                    self.feed(Input::MediaError {
                        key: self.key.unwrap(),
                    });
                } else {
                    self.last_seq = header.seq;
                    self.frame_geometry = Some(size);
                    if codec == Codec::Tiles && self.pattern(0) == Some(1) {
                        self.counts.tile_patterns += 1;
                    }
                    if codec == Codec::H264
                        && video_covers_samples
                        && let Some(variant) = self.pattern(45)
                    {
                        self.counts.video_patterns += 1;
                        self.counts.video_variants |= variant;
                    }
                }
            }
        }
        self.record();
    }
    fn event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Control {
                peer,
                msg: ControlMessage::Hello(hello),
            } => {
                assert_eq!(peer, self.source);
                assert!(!self.counts.connected);
                assert!(hello.features.iter().any(|f| f == "h264"));
                self.counts.connected = true;
                self.feed(Input::PeerUp { peer });
                self.feed(Input::PeerDisplays {
                    peer,
                    displays: hello.displays,
                });
            }
            LinkEvent::Control {
                peer,
                msg: ControlMessage::Ping { t0 },
            } => {
                assert_eq!(peer, self.source);
                self.transport
                    .link(peer)
                    .unwrap()
                    .send_control(&ControlMessage::Pong {
                        t0,
                        t1: now().as_nanos(),
                        t2: now().as_nanos(),
                    })
                    .unwrap();
            }
            LinkEvent::Control { peer, msg } => {
                assert_eq!(peer, self.source);
                if let ControlMessage::Projection(ProjectionMessage::Geometry {
                    projection,
                    size,
                    parking,
                    answers,
                    ..
                }) = &msg
                {
                    assert_eq!(*projection, self.key.unwrap().projection);
                    assert_eq!(*parking, ParkingKind::Mirror);
                    assert!(*answers == 0 || self.counts.resize_requests.contains(answers));
                    self.geometry = Some((*answers, *size));
                    self.counts.resize_answers.push(*answers);
                }
                self.feed(Input::Link(LinkEvent::Control { peer, msg }));
            }
            LinkEvent::Media { peer, data } => {
                assert_eq!(peer, self.source);
                self.media(&data);
            }
            LinkEvent::Closed { .. } => panic!("owned source link closed before installer stop"),
            LinkEvent::Input { .. } | LinkEvent::Audio { .. } | LinkEvent::ClipData { .. } => {
                panic!("unexpected input/audio/clipboard traffic")
            }
            event => self.feed(Input::Link(event)),
        }
    }
    fn resize(&mut self, size: PixelSize) -> u32 {
        let expected = self
            .counts
            .resize_requests
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .unwrap();
        self.frame_geometry = None;
        self.geometry = None;
        self.feed(Input::Proxy {
            key: self.key.unwrap(),
            event: ProxyEvent::Resized { size, scale: 1.0 },
        });
        // Tick flushes the production engine's debounced request; never forge a wire number.
        self.feed(Input::Tick);
        expected
    }
}

fn admit(
    status: &Value,
    claim: &Value,
    root: &Path,
    name: &str,
    source: NodeId,
    destination: NodeId,
    instance: Option<u64>,
) -> u64 {
    assert_eq!(status["name"].as_str(), Some(name));
    assert_eq!(status["node"].as_str(), Some(source.to_string().as_str()));
    let installer = &status["installer"];
    let identity = &installer["instance"];
    let pid = claim["pid"].as_u64().unwrap();
    assert!(pid > 0 && pid <= u64::from(u32::MAX));
    assert_eq!(identity["pid"].as_u64(), Some(pid));
    assert!(identity["uid"].is_null());
    assert_eq!(
        PathBuf::from(identity["exe"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        root.join("agent/crosspane-agent.exe")
            .canonicalize()
            .unwrap()
    );
    assert_eq!(
        PathBuf::from(claim["executable"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        root.join("agent/crosspane-agent.exe")
            .canonicalize()
            .unwrap()
    );
    assert_eq!(
        PathBuf::from(identity["runtime_dir"].as_str().unwrap()),
        root.join("runtime")
    );
    assert_eq!(installer["keystore"], "file");
    assert_eq!(installer["discovery"]["enabled"], false);
    assert_eq!(installer["audio"]["enabled"], false);
    let addr: SocketAddr = status["listening"].as_str().unwrap().parse().unwrap();
    assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(addr.port(), 0);
    let peers = installer["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(
        peers[0]["node"].as_str(),
        Some(destination.to_string().as_str())
    );
    assert_eq!(peers[0]["grants_given"], json!(["share"]));
    let current = identity["id"].as_u64().unwrap();
    assert_ne!(current, 0);
    if let Some(expected) = instance {
        assert_eq!(current, expected);
    }
    current
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_e2_destination() {
    if std::env::var("CROSSPANE_E2_DESTINATION_FIXTURE").as_deref() != Ok("1") {
        eprintln!("SKIP owned E2 destination: explicit Limited harness required");
        return;
    }
    assert!(!fixture_security::is_elevated().unwrap());
    assert_eq!(
        std::env::var("CROSSPANE_ACCEPTANCE_E2_SOURCE").as_deref(),
        Ok("1")
    );
    for flag in [
        "CROSSPANE_ACCEPTANCE_E1_ONLY",
        "CROSSPANE_ACCEPTANCE_E2_DESTINATION",
    ] {
        assert!(std::env::var_os(flag).is_none());
    }
    let root = PathBuf::from(std::env::var_os("CROSSPANE_E2_FIXTURE_ROOT").unwrap());
    let suffix = root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("crosspane-WP-W2.5b-")
        .unwrap();
    assert!(
        root.is_absolute() && suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
    );
    assert_eq!(
        root.parent().unwrap().canonicalize().unwrap(),
        std::env::temp_dir().canonicalize().unwrap()
    );
    for (env, dir) in [
        ("APPDATA", "roaming"),
        ("LOCALAPPDATA", "local"),
        ("CROSSPANE_RUNTIME_DIR", "runtime"),
    ] {
        assert_eq!(
            PathBuf::from(std::env::var_os(env).unwrap()),
            root.join(dir)
        );
    }
    for flag in ["CROSSPANE_DISCOVERY", "CROSSPANE_AUDIO", "CROSSPANE_GPU"] {
        assert_eq!(std::env::var(flag).as_deref(), Ok("0"));
    }
    let config = root.join("roaming/Crosspane");
    let state = root.join("local/Crosspane");
    for dir in [&config, &state, &root.join("fixture")] {
        paths::create_private_dir(dir).unwrap();
    }
    let source = Arc::new(DeviceIdentity::generate().unwrap());
    let destination = Arc::new(DeviceIdentity::generate().unwrap());
    paths::write_private(&state.join("device-key.pk8"), source.pkcs8()).unwrap();
    paths::write_private(
        &config.join("trust.json"),
        pin(&destination, Capability::WindowShare)
            .to_json()
            .as_bytes(),
    )
    .unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let transport = Transport::bind(
        TransportConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            identity: destination.clone(),
            pins: Arc::new(Pins(pin(&source, Capability::WindowPresent))),
            hello: Hello {
                minor: 0,
                name: "owned WP-W2.5b destination".into(),
                features: vec!["e1".into(), "h264".into(), "cursor".into()],
                displays: Vec::new(),
            },
        },
        Arc::new(move |event| {
            let _ = send.send(event);
        }),
    )
    .unwrap();
    assert_eq!(transport.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(transport.local_addr().port(), 0);
    let name = format!("wp-w2-5b-src-{suffix}");
    paths::write_private(&config.join("config.toml"), format!(
        "name = \"{name}\"\nport = 0\nacceptance_bind_ip = \"127.0.0.1\"\nforce_file_keystore = true\ncrossing = false\nlatency_overlay = false\npeers = [{{ addr = \"{}\" }}]\n[drag]\nacross = false\n", transport.local_addr()).as_bytes()).unwrap();
    let window = OwnedWindow::new();
    let initial = window.facts();
    assert!(initial.visible_now);
    let (engine, _) = Engine::new(
        EngineConfig::new(destination.node()),
        Box::<MemoryJournal>::default(),
        Box::<MemoryJournal>::default(),
        now(),
    )
    .unwrap();
    let mut fixture = Destination {
        engine,
        transport,
        source: source.node(),
        root: root.clone(),
        counts: Counts::default(),
        key: None,
        geometry: None,
        tiles: TileDecoder::new(),
        video: Box::new(
            crosspane_platform_windows::video::MfCodecs::new()
                .decoder_cpu()
                .unwrap(),
        ),
        canvas: Vec::new(),
        frame_size: PixelSize::new(0, 0),
        last_seq: 0,
        frame_geometry: None,
        closing: false,
    };
    fixture.feed(Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Unlocked,
        active: Some(true),
    })));
    fixture.feed(Input::Grants(
        [(source.node(), [Capability::WindowPresent].into())].into(),
    ));
    write(
        &root,
        "destination-ready.json",
        &json!({"claim":window.claim(),"initial":initial,"source_node":source.node().to_string(),"destination_node":destination.node().to_string()}),
    );
    let deadline = Instant::now() + Duration::from_secs(70);
    let claim = loop {
        assert!(Instant::now() < deadline, "agent claim timeout");
        if let Some(value) = bytes(&root.join("fixture/agent-claim.json"), 65536) {
            break serde_json::from_slice::<Value>(&value).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        claim["pid"].as_u64().is_some_and(|v| v > 0)
            && claim["process_created"].as_u64().is_some_and(|v| v > 0)
    );
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let mut instance = None;
    let mut projected = false;
    let mut stage = 0;
    let mut requested = 0;
    let mut initial_geometry = None;
    let mut normal = None;
    let mut minimum = None;
    loop {
        assert!(Instant::now() < deadline, "owned E2 destination timed out");
        tokio::select! {
            Some(event) = receive.recv() => fixture.event(event),
            _ = interval.tick() => {
                fixture.feed(Input::Tick);
                let latest_status = match ctl(json!({"cmd": "status"})).await {
                    Ok(status) => {
                        instance = Some(admit(
                            &status, &claim, &root, &name, source.node(), destination.node(), instance,
                        ));
                        status
                    }
                    Err(_) => continue,
                };
                if !projected && fixture.counts.connected
                    && latest_status["peers"][0]["connected"] == true
                {
                    let windows = ctl(json!({"cmd": "windows"})).await.unwrap();
                    let windows = windows.as_array().unwrap();
                    // The production restricted inventory admits the owned PID before any labels.
                    // Its frozen ctl response has no PID field; do not inspect or log labels here.
                    assert_eq!(windows.len(), 1, "restricted source must expose exactly one owned fixture");
                    let id = windows[0]["id"].as_u64().unwrap();
                    assert_ne!(id, 0);
                    ctl(json!({"cmd": "project", "window": id, "peer": destination.node().to_string()}))
                        .await.unwrap();
                    projected = true;
                }
                if stage == 0 && fixture.counts.tile_patterns > 0
                    && let Some((answer, size)) = fixture.geometry
                    && answer == fixture.counts.resize_requests.last().copied().unwrap_or(0)
                    && fixture.frame_geometry == Some(size)
                {
                    let facts = window.facts();
                    assert_eq!(facts.outer, initial.outer);
                    assert!(facts.visible_now);
                    assert_eq!(extent(facts.visible), size);
                    initial_geometry = Some(json!({
                        "answers": answer, "size": size, "frame_size": fixture.frame_geometry,
                        "facts": facts,
                    }));
                    window.animate(true);
                    stage = 1;
                }
                // Both received H.264 pictures must decode to the two known changing palettes.
                if stage == 1 && fixture.counts.video_variants == 3 {
                    requested = fixture.resize(PixelSize::new(540, 400));
                    stage = 2;
                }
                if stage == 2 || stage == 3 {
                    // Wait for this new request, never accept a still-in-flight earlier answer.
                    if let Some((answer, size)) = fixture.geometry.filter(|(answer, size)| {
                        *answer == requested && *answer > 0 && fixture.frame_geometry == Some(*size)
                    }) {
                        let facts = window.facts();
                        assert!(facts.visible_now);
                        assert_eq!(extent(facts.visible), size);
                        assert!(fixture.counts.resize_requests.contains(&answer));
                        if stage == 2 {
                            assert_eq!(size, PixelSize::new(540, 400));
                            normal = Some(json!({
                                "request": answer, "answers": answer, "asked": PixelSize::new(540, 400),
                                "size": size, "frame_size": fixture.frame_geometry, "facts": facts,
                            }));
                            requested = fixture.resize(PixelSize::new(160, 120));
                            stage = 3;
                        } else {
                            assert!(size.width > 160 && size.height > 120, "minimum size must be actual refusal");
                            minimum = Some(json!({
                                "request": answer, "answers": answer, "asked": PixelSize::new(160, 120),
                                "size": size, "frame_size": fixture.frame_geometry, "facts": facts,
                            }));
                            window.animate(false);
                            fixture.closing = true;
                            fixture.feed(Input::Proxy {
                                key: fixture.key.unwrap(), event: ProxyEvent::CloseRequested,
                            });
                            stage = 4;
                        }
                    }
                }
                if stage == 4 && fixture.counts.returned {
                    let facts = window.facts();
                    let peer = &latest_status["installer"]["peers"][0];
                    if facts.outer == initial.outer && facts.visible == initial.visible && facts.visible_now
                        && peer["counters"]["e2_source_returned"].as_u64().unwrap_or(0) > 0
                        && journal_empty(&state)
                    {
                        assert_eq!(peer["last_source_parking"], "mirror");
                        assert_eq!(peer["counters"]["e2_returns_failed"].as_u64(), Some(0));
                        assert!(latest_status["notices"].as_array().unwrap().iter().any(|v| {
                            v.as_str().is_some_and(|s| s.contains("mirror") && s.contains("visible"))
                        }), "plain source mirror notice missing");
                        break;
                    }
                }
            }
        }
    }
    let instance = instance.unwrap();
    let ack = match ctl(json!({"cmd": "installer_stop", "expected_instance": instance})).await {
        Ok(result) => {
            assert_eq!(result, "stopping");
            true
        }
        Err(_) => false, // The launcher must independently prove exact exit and clean receipt.
    };
    fixture
        .transport
        .shutdown("owned destination complete")
        .await;
    drop(window);
    write(
        &root,
        "destination-result.json",
        &json!({
            "protocol_clean": true, "instance": instance, "source_pid": claim["pid"],
            "source_node": source.node().to_string(), "source_name": name, "ack_received": ack,
            "restored": true, "journal_empty": true, "source_returned": true,
            "initial_geometry": initial_geometry, "normal": normal, "minimum": minimum,
            "tiles": fixture.counts.tiles, "video": fixture.counts.video,
            "cursors": fixture.counts.cursors, "cursor_default": fixture.counts.cursor_default,
            "cursor_hidden": fixture.counts.cursor_hidden,
            "empty_held_heartbeats": fixture.counts.empty_held_heartbeats, "counts": fixture.counts,
            "native_exit_verified": false, "receipt_verified": false,
        }),
    );
    eprintln!(
        "owned destination tiles={} video={} geometry={} restored=true journal_empty=true installer_ack={ack}",
        fixture.counts.tiles, fixture.counts.video, fixture.counts.geometry
    );
}

/// Codec proof only: these authored bytes are never submitted as a live source cursor event.
#[test]
fn owned_cursor_wire_decode() {
    use crosspane_media::wire::{FrameHeader, write_cursor, write_default_cursor};

    let header = FrameHeader {
        projection: 7,
        seq: 1,
        key: false,
        captured_ns: 0,
        width: 1,
        height: 1,
    };
    let mut payload = Vec::new();
    write_default_cursor(header, &mut payload).unwrap();
    let default = read_cursor(&payload).unwrap();
    assert_eq!(default.header, header);
    assert!(default.default);
    assert_eq!(default.hotspot, (0, 0));

    let hidden_header = FrameHeader { seq: 2, ..header };
    write_cursor(hidden_header, (0, 0), &[0; 4], &mut payload).unwrap();
    let hidden = read_cursor(&payload).unwrap();
    assert_eq!(hidden.header, hidden_header);
    assert!(!hidden.default);
    assert!(
        hidden
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 0)
    );

    let image_header = FrameHeader {
        seq: 3,
        width: 2,
        height: 2,
        ..header
    };
    let pixels = [
        20, 20, 220, 255, 20, 220, 20, 255, 220, 20, 20, 255, 220, 220, 220, 255,
    ];
    write_cursor(image_header, (1, 0), &pixels, &mut payload).unwrap();
    let image = read_cursor(&payload).unwrap();
    assert_eq!(image.header, image_header);
    assert!(!image.default);
    assert_eq!(image.hotspot, (1, 0));
    assert_eq!(image.pixels, pixels);
}
