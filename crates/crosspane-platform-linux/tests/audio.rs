//! All server/client operations in this test are restricted to the owned private null graph.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_platform::{
    AudioDeviceError, AudioEvent, AudioHost, AudioKind, IoGate, PlatformError,
};
use crosspane_platform_linux::audio::PipeWireAudioHost;
use crosspane_types::id::NodeId;
use pipewire::{self as pw, spa};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn private_server() -> bool {
    if std::env::var("CROSSPANE_PRIVATE_PIPEWIRE").as_deref() != Ok("1") {
        return false;
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").expect("private runtime required");
    let remote = std::env::var("PIPEWIRE_REMOTE").expect("explicit private remote required");
    assert!(runtime.starts_with("/tmp/crosspane-pipewire."));
    assert!(remote.starts_with("crosspane-") && !remote.contains('/'));
    assert_eq!(std::env::var("PIPEWIRE_RUNTIME_DIR").unwrap(), runtime);
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let directory = std::fs::symlink_metadata(&runtime).unwrap();
    assert!(directory.is_dir() && directory.mode() & 0o777 == 0o700);
    assert_eq!(directory.uid(), rustix::process::getuid().as_raw());
    assert!(
        std::fs::symlink_metadata(format!("{runtime}/{remote}"))
            .unwrap()
            .file_type()
            .is_socket()
    );
    true
}
fn spin(loop_: &pw::main_loop::MainLoopRc, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        loop_
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(1)));
    }
}
fn graph() -> serde_json::Value {
    // A file avoids pipe backpressure while we poll the owned child's deadline. The guard also
    // reaps the child and removes its scratch file on timeout, I/O failure or assertion panic.
    struct Dump {
        child: Option<std::process::Child>,
        path: std::path::PathBuf,
    }
    impl Drop for Dump {
        fn drop(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }
    static NEXT_DUMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "crosspane-pw-dump-{}-{}",
        std::process::id(),
        NEXT_DUMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let mut dump = Dump { child: None, path };
    let deadline = Instant::now() + Duration::from_secs(2);
    dump.child = Some(
        std::process::Command::new("pw-dump")
            .stdout(file)
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start pw-dump on the verified private server"),
    );
    let status = loop {
        if let Some(status) = dump.child.as_mut().unwrap().try_wait().unwrap() {
            // try_wait has reaped this child; do not kill a potentially reused PID on Drop.
            dump.child = None;
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "private pw-dump exceeded 2 seconds"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(status.success(), "private server failed, cannot skip");
    serde_json::from_slice(&std::fs::read(&dump.path).unwrap()).unwrap()
}
fn node(graph: &serde_json::Value, name: &str) -> u32 {
    graph
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["info"]["props"]["node.name"] == name)
        .unwrap_or_else(|| panic!("missing fixture node {name}"))
        .get("id")
        .unwrap()
        .as_u64()
        .unwrap() as u32
}
fn has_node(name: &str) -> bool {
    graph()
        .as_array()
        .unwrap()
        .iter()
        .any(|object| object["info"]["props"]["node.name"] == name)
}
fn defaults(graph: &serde_json::Value) -> serde_json::Value {
    graph
        .as_array()
        .unwrap()
        .iter()
        .find(|object| object["props"]["metadata.name"] == "default")
        .unwrap()["metadata"]
        .clone()
}
fn link(core: &pw::core::Core, output: u32, input: u32, channel: u32) -> pw::link::Link {
    core.create_object::<pw::link::Link>(
        "link-factory",
        &pw::properties::properties! {
            "link.output.node" => output.to_string(), "link.input.node" => input.to_string(),
            "link.output.port" => channel.to_string(), "link.input.port" => channel.to_string(),
        },
    )
    .unwrap()
}

struct SyntheticData {
    channels: usize,
    output: bool,
    frames: usize,
    received: Rc<RefCell<Vec<f32>>>,
}
struct Client<'a> {
    _listener: pw::stream::StreamListener<SyntheticData>,
    stream: pw::stream::StreamBox<'a>,
    received: Rc<RefCell<Vec<f32>>>,
}
fn client<'a>(core: &'a pw::core::Core, output: bool, channels: usize, index: usize) -> Client<'a> {
    let stream = pw::stream::StreamBox::new(core, "private-synthetic", pw::properties::properties! {
        "node.name" => format!("crosspane.private.synthetic.{index}"),
        "media.type" => "Audio", "media.category" => if output { "Playback" } else { "Capture" },
        "audio.channels" => channels.to_string(), "audio.rate" => "48000",
        "audio.position" => if channels == 2 { "[ FL FR ]" } else { "[ MONO ]" },
        "adapter.auto-port-config" => "{ mode = dsp position = preserve }",
    }).unwrap();
    let received = Rc::new(RefCell::new(Vec::with_capacity(48_000)));
    let listener = stream
        .add_local_listener_with_user_data(SyntheticData {
            channels,
            output,
            frames: 0,
            received: received.clone(),
        })
        .process(|stream, data| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.len() != 1 {
                    return;
                }
                let item = &mut datas[0];
                if data.output {
                    let bytes = item.data().unwrap();
                    let size = (480 * data.channels * 4).min(bytes.len());
                    for frame in bytes[..size].chunks_exact_mut(data.channels * 4) {
                        for (channel, sample) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate()
                        {
                            // Only test-owned 1 kHz left / 2 kHz right tones; never log their PCM.
                            let frequency = if channel == 0 { 1000.0 } else { 2000.0 };
                            let value = (data.frames as f32 * frequency * std::f32::consts::TAU
                                / 48_000.0)
                                .sin()
                                * 0.25;
                            *sample = value.to_le_bytes();
                        }
                        data.frames += 1;
                    }
                    *item.chunk_mut().offset_mut() = 0;
                    *item.chunk_mut().size_mut() = size as u32;
                    *item.chunk_mut().stride_mut() = (data.channels * 4) as i32;
                } else {
                    let offset = item.chunk().offset() as usize;
                    let size = item.chunk().size() as usize;
                    let bytes = item.data().unwrap();
                    assert!(offset + size <= bytes.len());
                    let mut received = data.received.borrow_mut();
                    for sample in bytes[offset..offset + size].as_chunks::<4>().0 {
                        if received.len() < received.capacity() {
                            received.push(f32::from_le_bytes(*sample));
                        }
                    }
                }
            }
        })
        .register()
        .unwrap();
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(channels as u32);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .unwrap()
    .0
    .into_inner();
    stream
        .connect(
            if output {
                spa::utils::Direction::Output
            } else {
                spa::utils::Direction::Input
            },
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [spa::pod::Pod::from_bytes(&bytes).unwrap()],
        )
        .unwrap();
    Client {
        _listener: listener,
        stream,
        received,
    }
}
fn take(events: &Arc<Mutex<Vec<AudioEvent>>>) -> Vec<AudioEvent> {
    std::mem::take(&mut *events.lock().unwrap())
}
fn active(peer: NodeId, kind: AudioKind, active: bool) -> AudioEvent {
    AudioEvent::VirtualActive { peer, kind, active }
}
fn tone(samples: &[f32], channels: usize, channel: usize, frequency: f32) {
    assert!(samples.len() >= 480 * channels, "tone did not arrive");
    assert!(samples.iter().all(|sample| sample.is_finite()));
    let values: Vec<_> = samples
        .chunks_exact(channels)
        .map(|frame| frame[channel])
        .collect();
    let first = values
        .iter()
        .position(|value| value.abs() > 0.1)
        .expect("tone never became audible");
    let last = values.iter().rposition(|value| value.abs() > 0.1).unwrap() + 1;
    let values = &values[first.max(last.saturating_sub(2400))..last];
    let crossings = values
        .windows(2)
        .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
        .count();
    let measured = crossings as f32 * 48_000.0 / values.len() as f32;
    assert!(
        (measured - frequency).abs() < 150.0,
        "tone frequency/channel mismatch: measured {measured}, expected {frequency}, frames {}",
        values.len()
    );
    assert!(values.iter().any(|value| value.abs() > 0.1));
}

// One server test runs all phases sequentially: the final disconnect must never race another
// test sharing this fixture. This marker is printed only after every server phase actually ran.
#[test]
fn private_null_graph_activity_tone_gate_drop_disconnect() {
    if !private_server() {
        eprintln!("private audio tests skipped: no private marker");
        return;
    }
    let start = Instant::now();
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(&loop_, None).unwrap();
    let core = context.connect_rc(None).unwrap();
    let gate = IoGate::new();
    let mut host = PipeWireAudioHost::new(gate.clone()).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let received_events = events.clone();
    host.subscribe(Arc::new(move |event| {
        received_events.lock().unwrap().push(event)
    }))
    .unwrap();
    assert!(matches!(
        host.open_capture(AudioKind::Microphone.format()),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        host.open_playback(AudioKind::Speaker.format()),
        Err(PlatformError::Locked)
    ));
    let original_defaults = defaults(&graph());
    let peer = NodeId([0x31; 32]);
    let other = NodeId([0x32; 32]);
    let mut ports = host.add_peer(peer, "Peer\0\n").unwrap();
    let other_ports = host.add_peer(other, "Other").unwrap();
    assert!(host.add_peer(peer, "duplicate").is_err());
    spin(&loop_, Duration::from_millis(50));
    let snapshot = graph();
    for (id, description) in [(peer, "Peer"), (other, "Other")] {
        for (suffix, label, channels, class) in [
            ("speaker", "speakers", 2, "Audio/Sink"),
            ("mic", "microphone", 1, "Audio/Source"),
        ] {
            let name = format!("crosspane.{id}.{suffix}");
            let id = node(&snapshot, &name);
            let object = snapshot
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap();
            assert_eq!(
                object["info"]["props"]["node.description"],
                format!("{description} {label}")
            );
            assert_eq!(object["info"]["props"]["media.class"], class);
            assert_eq!(object["info"]["props"]["audio.channels"], channels);
            assert_eq!(object["info"]["props"]["audio.rate"], 48000);
        }
    }
    assert_eq!(defaults(&snapshot), original_defaults);
    assert!(
        take(&events).is_empty(),
        "own virtual setup is not application activity"
    );
    host.remove_peer(other).unwrap();
    drop(other_ports);
    spin(&loop_, Duration::from_millis(20));
    assert!(!has_node(&format!("crosspane.{other}.speaker")));
    assert!(has_node(&format!("crosspane.{peer}.mic")));
    let speaker = node(&graph(), &format!("crosspane.{peer}.speaker"));
    let mic = node(&graph(), &format!("crosspane.{peer}.mic"));
    for kind in [AudioKind::Speaker, AudioKind::Microphone] {
        let output = kind == AudioKind::Speaker;
        let channels = kind.format().channels as usize;
        let first = client(&core, output, channels, 0);
        let second = client(&core, output, channels, 1);
        spin(&loop_, Duration::from_millis(20));
        let mut links = Vec::new();
        for channel in 0..channels as u32 {
            let (from, to) = if output {
                (first.stream.node_id(), speaker)
            } else {
                (mic, first.stream.node_id())
            };
            links.push(link(&core, from, to, channel));
        }
        spin(&loop_, Duration::from_millis(50));
        assert_eq!(first.stream.state(), pw::stream::StreamState::Streaming);
        assert_eq!(take(&events), [active(peer, kind, true)]);
        if output {
            let samples: Vec<_> = std::iter::from_fn(|| ports.speaker_out.pop().ok()).collect();
            tone(&samples, 2, 0, 1000.0);
            tone(&samples, 2, 1, 2000.0);
        } else {
            assert!(!first.received.borrow().is_empty());
            assert!(
                first.received.borrow().iter().all(|sample| *sample == 0.0),
                "virtual microphone underrun must be silence"
            );
            // Feed only synthetic mono PCM, in bounded 10 ms packets.
            let producer_deadline = Instant::now() + Duration::from_secs(2);
            for frame in 0..4800 {
                let sample =
                    (frame as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
                while ports.mic_in.push(sample).is_err() {
                    assert!(
                        Instant::now() < producer_deadline,
                        "synthetic microphone producer stalled for 2 seconds"
                    );
                    spin(&loop_, Duration::from_millis(1));
                }
            }
            spin(&loop_, Duration::from_millis(80));
            let samples = first.received.borrow();
            // The initial and final silence is expected; inspect the tone-containing interval.
            let first_tone = samples
                .iter()
                .position(|sample| sample.abs() > 0.1)
                .unwrap();
            tone(&samples[first_tone..first_tone + 2400], 1, 0, 1000.0);
        }
        for channel in 0..channels as u32 {
            let (from, to) = if output {
                (second.stream.node_id(), speaker)
            } else {
                (mic, second.stream.node_id())
            };
            links.push(link(&core, from, to, channel));
        }
        spin(&loop_, Duration::from_millis(40));
        assert_eq!(second.stream.state(), pw::stream::StreamState::Streaming);
        assert!(take(&events).is_empty());
        first.stream.set_active(false).unwrap();
        spin(&loop_, Duration::from_millis(40));
        assert!(take(&events).is_empty());
        second.stream.set_active(false).unwrap();
        spin(&loop_, Duration::from_millis(40));
        assert_eq!(take(&events), [active(peer, kind, false)]);
        drop(links);
        drop(first);
        drop(second);
    }
    assert_eq!(defaults(&graph()), original_defaults);
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let mut capture = host.open_capture(AudioKind::Microphone.format()).unwrap();
    let mut playback = host.open_playback(AudioKind::Speaker.format()).unwrap();
    spin(&loop_, Duration::from_millis(30));
    let snapshot = graph();
    let capture_node = node(&snapshot, "crosspane.capture");
    let playback_node = node(&snapshot, "crosspane.playback");
    for name in ["crosspane.capture", "crosspane.playback"] {
        let id = node(&snapshot, name);
        let object = snapshot
            .as_array()
            .unwrap()
            .iter()
            .find(|object| object["id"] == id)
            .unwrap();
        assert!(object["info"]["props"].get("target.object").is_none());
    }
    // This harness has no policy daemon. Synthetic fixture links ONLY to its declared null defaults.
    let physical_links = vec![
        link(
            &core,
            node(&snapshot, "crosspane.private.source"),
            capture_node,
            0,
        ),
        link(
            &core,
            playback_node,
            node(&snapshot, "crosspane.private.sink"),
            0,
        ),
        link(
            &core,
            playback_node,
            node(&snapshot, "crosspane.private.sink"),
            1,
        ),
    ];
    for _ in 0..960 {
        playback.pcm.push(0.125).unwrap();
    }
    spin(&loop_, Duration::from_millis(40));
    let capture_samples: Vec<_> = std::iter::from_fn(|| capture.pcm.pop().ok()).collect();
    assert!(
        capture_samples.len() >= 480 && capture_samples.iter().all(|sample| sample.is_finite())
    );
    assert!(
        playback.pcm.slots() >= 960,
        "null playback must consume queued PCM"
    );
    let closure = Instant::now();
    gate.set_engine_permits(false);
    while !capture.pcm.is_abandoned() || !playback.pcm.is_abandoned() {
        assert!(
            closure.elapsed() < Duration::from_millis(50),
            "gate closure exceeded 50 ms"
        );
        spin(&loop_, Duration::from_millis(1));
    }
    let gate_time = closure.elapsed();
    gate.set_engine_permits(true);
    let old_len = capture.pcm.slots();
    spin(&loop_, Duration::from_millis(30));
    assert_eq!(capture.pcm.slots(), old_len);
    assert!(capture.pcm.is_abandoned() && playback.pcm.is_abandoned());
    drop(capture);
    drop(playback);
    drop(physical_links);
    let capture = host.open_capture(AudioKind::Microphone.format()).unwrap();
    let playback = host.open_playback(AudioKind::Speaker.format()).unwrap();
    assert_eq!(
        capture.pcm.slots(),
        0,
        "reopen never receives old mic samples"
    );
    // Simulate the engine dropping physical handles for a transient closure between callbacks.
    gate.set_engine_permits(false);
    gate.set_engine_permits(true);
    let capture_drop_start = Instant::now();
    drop(capture);
    let capture_drop_time = capture_drop_start.elapsed();
    assert!(capture_drop_time < Duration::from_millis(50));
    let playback_drop_start = Instant::now();
    drop(playback);
    let playback_drop_time = playback_drop_start.elapsed();
    assert!(playback_drop_time < Duration::from_millis(50));
    spin(&loop_, Duration::from_millis(20));
    assert!(!has_node("crosspane.capture") && !has_node("crosspane.playback"));
    // Remove only the private null source while capture is actively linked.
    let mut removal_capture = host.open_capture(AudioKind::Microphone.format()).unwrap();
    spin(&loop_, Duration::from_millis(20));
    let snapshot = graph();
    let private_source = node(&snapshot, "crosspane.private.source");
    let removal_link = link(
        &core,
        private_source,
        node(&snapshot, "crosspane.capture"),
        0,
    );
    spin(&loop_, Duration::from_millis(30));
    while removal_capture.pcm.pop().is_ok() {}
    take(&events);
    let removal_start = Instant::now();
    core.get_registry()
        .unwrap()
        .destroy_global(private_source)
        .into_result()
        .unwrap();
    spin(&loop_, Duration::from_millis(30));
    assert!(!has_node("crosspane.private.source"));
    assert!(take(&events).iter().any(|event| matches!(
        event,
        AudioEvent::DeviceError {
            peer: None,
            kind: AudioKind::Microphone,
            error: AudioDeviceError::Unavailable
        }
    )));
    assert!(removal_start.elapsed() < Duration::from_secs(2));
    while removal_capture.pcm.pop().is_ok() {}
    spin(&loop_, Duration::from_millis(20));
    assert_eq!(
        removal_capture.pcm.slots(),
        0,
        "removed device cannot keep producing"
    );
    drop(removal_link);
    drop(removal_capture);
    let host_capture = host.open_capture(AudioKind::Microphone.format()).unwrap();
    let host_playback = host.open_playback(AudioKind::Speaker.format()).unwrap();
    let drop_start = Instant::now();
    drop(host);
    let host_drop_time = drop_start.elapsed();
    assert!(host_capture.pcm.is_abandoned() && host_playback.pcm.is_abandoned());
    drop(host_capture);
    drop(host_playback);
    assert!(drop_start.elapsed() < Duration::from_secs(2));
    assert!(ports.speaker_out.is_abandoned() && ports.mic_in.is_abandoned());
    spin(&loop_, Duration::from_millis(20));
    assert!(!has_node(&format!("crosspane.{peer}.speaker")));
    // Disconnect only the PID supplied by this owned harness, after all other phases complete.
    let mut host = PipeWireAudioHost::new(gate).unwrap();
    let disconnect_events = Arc::new(Mutex::new(Vec::new()));
    let sink = disconnect_events.clone();
    host.subscribe(Arc::new(move |event| sink.lock().unwrap().push(event)))
        .unwrap();
    let disconnected_ports = host.add_peer(peer, "Disconnect").unwrap();
    spin(&loop_, Duration::from_millis(20));
    let capture = host.open_capture(AudioKind::Microphone.format()).unwrap();
    let pid: u32 = std::env::var("CROSSPANE_PRIVATE_PIPEWIRE_PID")
        .unwrap()
        .parse()
        .unwrap();
    assert!(pid > 1 && pid != std::process::id());
    let snapshot = graph();
    assert!(
        snapshot
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["type"] == "PipeWire:Interface:Core"
                && object["info"]["props"]["application.process.id"] == pid)
    );
    let disconnect_start = Instant::now();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    while !capture.pcm.is_abandoned() {
        assert!(disconnect_start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(disconnected_ports.mic_in.is_abandoned());
    assert!(take(&disconnect_events).iter().any(|event| matches!(
        event,
        AudioEvent::DeviceError {
            error: AudioDeviceError::Unavailable,
            ..
        }
    )));
    drop(capture);
    drop(host);
    let marker = std::env::var("CROSSPANE_PRIVATE_AUDIO_TEST_DONE").unwrap();
    assert_eq!(
        std::path::Path::new(&marker).parent().unwrap(),
        std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap())
    );
    std::fs::write(marker, "PRIVATE_AUDIO_SERVER_TESTS_RAN\n").unwrap();
    eprintln!(
        "PRIVATE_AUDIO_SERVER_TESTS_RAN gate={gate_time:?} capture_drop={capture_drop_time:?} playback_drop={playback_drop_time:?} host_drop={host_drop_time:?} disconnect={:?} total={:?}",
        disconnect_start.elapsed(),
        start.elapsed()
    );
}
