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

/// Native authority lives in this test process, never in the transport wrapper.
mod proxy_controller {
    use anyhow::{Result, ensure};
    use std::{
        cell::RefCell,
        collections::VecDeque,
        fs::{File, OpenOptions},
        io::{Read, Write},
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            fs::OpenOptionsExt,
            io::{AsRawHandle, FromRawHandle},
        },
        path::{Path, PathBuf},
        ptr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, FILETIME,
            GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HWND, SetHandleInformation, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Security::Cryptography::{
            BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE, BCRYPT_OBJECT_LENGTH, BCRYPT_SHA256_ALGORITHM,
            BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
            BCryptGetProperty, BCryptHashData, BCryptOpenAlgorithmProvider,
        },
        Security::{GetTokenInformation, TOKEN_QUERY, TokenElevation, TokenUIAccess},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FileAttributeTagInfo, GetFileInformationByHandleEx,
        },
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject,
        },
        System::Pipes::{CreatePipe, PeekNamedPipe},
        System::Threading::{
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            GetExitCodeProcess, GetProcessId, GetProcessTimes, GetThreadId,
            InitializeProcThreadAttributeList, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROCESS_INFORMATION, QueryFullProcessImageNameW, ResumeThread, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        },
        UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        UI::WindowsAndMessaging::{
            DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_SHOW,
            GetClassNameW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, MSG, OBJID_WINDOW,
            PM_REMOVE, PeekMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT,
        },
    };

    const PIPE_CAP: usize = 128 * 1024;
    struct Kernel(HANDLE);
    impl Drop for Kernel {
        fn drop(&mut self) {
            // SAFETY: exactly one real owned kernel handle, never pseudo or transferred elsewhere.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Algorithm(BCRYPT_ALG_HANDLE);
    impl Drop for Algorithm {
        fn drop(&mut self) {
            // SAFETY: owns one successful CNG algorithm provider, after dependent hashes drop.
            unsafe {
                BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
    struct Hash(BCRYPT_HASH_HANDLE);
    impl Drop for Hash {
        fn drop(&mut self) {
            // SAFETY: owns one hash; its object buffer remains alive until after this guard drops.
            unsafe {
                BCryptDestroyHash(self.0);
            }
        }
    }
    struct Attributes {
        // Explicit 16-byte alignment exceeds pointer alignment for the opaque native list.
        words: Vec<u128>,
        live: bool,
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            if self.live {
                // SAFETY: initialized list lives in this aligned buffer, after synchronous CreateProcess.
                unsafe {
                    DeleteProcThreadAttributeList(self.words.as_mut_ptr().cast());
                }
            }
        }
    }
    fn wide(value: &std::ffi::OsStr) -> Result<Vec<u16>> {
        let mut bytes: Vec<_> = value.encode_wide().collect();
        ensure!(bytes.len() < 1024 && !bytes.contains(&0), "W24C_WIDE");
        bytes.push(0);
        Ok(bytes)
    }
    fn creation(process: HANDLE) -> Result<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        ensure!(
            // SAFETY: retained live process handle and exact initialized writable FILETIME outputs.
            unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0,
            "W24C_CREATION"
        );
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    fn limited(process: HANDLE) -> Result<()> {
        let mut raw = ptr::null_mut();
        ensure!(
            // SAFETY: query-only access on the original retained process or current pseudo-handle.
            unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } != 0,
            "W24C_TOKEN"
        );
        let token = Kernel(raw);
        for class in [TokenElevation, TokenUIAccess] {
            let mut value = 1u32;
            let mut returned = 0;
            ensure!(
                // SAFETY: both fixed information classes have exactly one DWORD writable layout.
                unsafe {
                    GetTokenInformation(
                        token.0,
                        class,
                        (&mut value as *mut u32).cast(),
                        4,
                        &mut returned,
                    )
                } != 0
                    && returned == 4
                    && value == 0,
                "W24C_LIMITED"
            );
        }
        Ok(())
    }
    fn image_pin(
        path: &Path,
        until: Instant,
    ) -> Result<(File, super::fixture_security::ParentPins)> {
        let expected = std::env::var("CROSSPANE_W24C_DRIVER_SHA256")
            .map_err(|_| anyhow::anyhow!("W24C_PIN"))?;
        ensure!(
            expected.len() == 64
                && expected
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && expected.bytes().any(|b| b != b'0'),
            "W24C_PIN"
        );
        let bytes: u64 = std::env::var("CROSSPANE_W24C_DRIVER_BYTES")
            .map_err(|_| anyhow::anyhow!("W24C_PIN"))?
            .parse()
            .map_err(|_| anyhow::anyhow!("W24C_PIN"))?;
        ensure!(bytes > 0 && bytes <= 128 * 1024 * 1024, "W24C_PIN");
        let parents = super::fixture_security::pin_parent(path)
            .map_err(|_| anyhow::anyhow!("W24C_PARENT"))?;
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|_| anyhow::anyhow!("W24C_IMAGE"))?;
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        ensure!(
            // SAFETY: retained actual file handle and correctly sized initialized attribute output.
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle().cast(),
                    FileAttributeTagInfo,
                    (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                    size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
                )
            } != 0
                && info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "W24C_IMAGE_REPARSE"
        );
        ensure!(
            file.metadata()
                .map_err(|_| anyhow::anyhow!("W24C_IMAGE"))?
                .len()
                == bytes,
            "W24C_IMAGE_SIZE"
        );
        let mut algorithm = ptr::null_mut();
        ensure!(
            // SAFETY: fixed documented SHA256 provider, initialized writable handle output, no secrets.
            unsafe {
                BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_SHA256_ALGORITHM, ptr::null(), 0)
            } >= 0,
            "W24C_SHA"
        );
        let algorithm = Algorithm(algorithm);
        let mut size = 0u32;
        let mut returned = 0u32;
        ensure!(
            // SAFETY: provider is live; exact DWORD object-length property storage.
            unsafe {
                BCryptGetProperty(
                    algorithm.0,
                    BCRYPT_OBJECT_LENGTH,
                    (&mut size as *mut u32).cast(),
                    4,
                    &mut returned,
                    0,
                )
            } >= 0
                && returned == 4
                && size > 0
                && size <= 65536,
            "W24C_SHA"
        );
        let mut object = vec![0u8; size as usize];
        let mut raw = ptr::null_mut();
        ensure!(
            // SAFETY: object storage stays fixed and alive through Hash destruction; unkeyed hash only.
            unsafe {
                BCryptCreateHash(
                    algorithm.0,
                    &mut raw,
                    object.as_mut_ptr(),
                    size,
                    ptr::null(),
                    0,
                    0,
                )
            } >= 0,
            "W24C_SHA"
        );
        let hash = Hash(raw);
        let mut chunk = [0u8; 65536];
        let mut total = 0u64;
        loop {
            ensure!(Instant::now() < until, "W24C_DEADLINE");
            let count = file
                .read(&mut chunk)
                .map_err(|_| anyhow::anyhow!("W24C_IMAGE"))?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(total <= bytes, "W24C_IMAGE_SIZE");
            ensure!(
                // SAFETY: bounded live image buffer, exact input length, live unkeyed hash object.
                unsafe { BCryptHashData(hash.0, chunk.as_ptr(), count as u32, 0) } >= 0,
                "W24C_SHA"
            );
        }
        let mut digest = [0u8; 32];
        ensure!(
            // SAFETY: SHA256 requires exactly the initialized 32-byte writable output.
            unsafe { BCryptFinishHash(hash.0, digest.as_mut_ptr(), 32, 0) } >= 0,
            "W24C_SHA"
        );
        let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        ensure!(total == bytes && actual == expected, "W24C_IMAGE_SHA");
        Ok((file, parents)) // pin ancestors and deny image write/delete through retirement.
    }
    fn pipe() -> Result<(Kernel, Kernel)> {
        let mut read = ptr::null_mut();
        let mut write = ptr::null_mut();
        let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        ensure!(
            // SAFETY: exact initialized security struct, writable outputs, private anonymous pipe only.
            unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } != 0,
            "W24C_PIPE"
        );
        Ok((Kernel(read), Kernel(write)))
    }
    fn no_inherit(handle: &Kernel) -> Result<()> {
        ensure!(
            // SAFETY: exact owned parent endpoint; changes only its inherit flag, not ACL/security policy.
            unsafe { SetHandleInformation(handle.0, HANDLE_FLAG_INHERIT, 0) } != 0,
            "W24C_PIPE"
        );
        Ok(())
    }
    struct PipeReader {
        file: File,
        bytes: Vec<u8>,
        eof: bool,
    }
    impl PipeReader {
        fn new(handle: Kernel) -> Self {
            let raw = handle.0;
            std::mem::forget(handle);
            // SAFETY: exactly one original anonymous parent read endpoint transferred to File.
            let file = unsafe { File::from_raw_handle(raw.cast()) };
            Self {
                file,
                bytes: Vec::new(),
                eof: false,
            }
        }
        fn poll(&mut self) -> Result<()> {
            if self.eof {
                return Ok(());
            }
            let mut available = 0u32;
            // SAFETY: retained original anonymous read endpoint; existence/available count only.
            if unsafe {
                PeekNamedPipe(
                    self.file.as_raw_handle().cast(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            } == 0
            {
                // SAFETY: sample this thread's immediately preceding API error, never arbitrary strings.
                ensure!(unsafe { GetLastError() } == ERROR_BROKEN_PIPE, "W24C_DRAIN");
                self.eof = true;
                return Ok(());
            }
            if available == 0 {
                return Ok(());
            }
            ensure!(self.bytes.len() < PIPE_CAP, "W24C_DRAIN_CAP");
            let mut chunk = [0u8; 4096];
            let capacity = (available as usize)
                .min(chunk.len())
                .min(PIPE_CAP - self.bytes.len());
            let count = self
                .file
                .read(&mut chunk[..capacity])
                .map_err(|_| anyhow::anyhow!("W24C_DRAIN"))?;
            ensure!(count != 0, "W24C_DRAIN");
            self.bytes.extend_from_slice(&chunk[..count]);
            Ok(())
        }
    }
    struct PrivateJob {
        handle: Kernel,
        clean: bool,
    }
    impl Drop for PrivateJob {
        fn drop(&mut self) {
            if !self.clean {
                // SAFETY: fallback ONLY this original private non-breakaway job; never a normal-success proof.
                unsafe {
                    TerminateJobObject(self.handle.0, 2);
                }
            }
        }
    }
    struct SuspendedProcess {
        handle: Kernel,
        assigned: bool,
    }
    impl Drop for SuspendedProcess {
        fn drop(&mut self) {
            if !self.assigned {
                // SAFETY: setup failure before assignment; original CreateProcess handle, still suspended,
                // never resumed, never PID/name adoption. Closing alone would strand a suspended child.
                unsafe {
                    TerminateProcess(self.handle.0, 2);
                }
            }
        }
    }
    struct WatchJob(Kernel);
    impl WatchJob {
        fn terminate(&self) {
            // SAFETY: this independently owned duplicate names ONLY our original private job.
            unsafe {
                TerminateJobObject(self.0.0, 2);
            }
        }
    }
    // SAFETY: this wrapper ONLY owns an independently duplicated kernel job handle. Kernel jobs
    // are thread-agnostic; no HWND/hook/pointer or original process/thread guard is transferred.
    unsafe impl Send for WatchJob {}
    struct Watchdog {
        stop: mpsc::SyncSender<()>,
        join: Option<JoinHandle<()>>,
        expired: Arc<AtomicBool>,
    }
    impl Watchdog {
        fn start(job: &Kernel) -> Result<Self> {
            let mut duplicated = ptr::null_mut();
            ensure!(
                // SAFETY: original live job, borrowed current-process pseudo-handles, separate owned duplicate.
                unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        job.0,
                        GetCurrentProcess(),
                        &mut duplicated,
                        0,
                        0,
                        DUPLICATE_SAME_ACCESS,
                    )
                } != 0,
                "W24C_WATCHDOG"
            );
            let duplicated = WatchJob(Kernel(duplicated));
            let expired = Arc::new(AtomicBool::new(false));
            let observed = expired.clone();
            let (stop, stopped) = mpsc::sync_channel(1);
            let join = std::thread::spawn(move || {
                if stopped.recv_timeout(Duration::from_secs(60)).is_err() {
                    observed.store(true, Ordering::Release);
                    duplicated.terminate();
                }
                drop(duplicated);
            });
            Ok(Self {
                stop,
                join: Some(join),
                expired,
            })
        }
        fn finish(&mut self) -> Result<()> {
            let _ = self.stop.try_send(());
            if let Some(join) = self.join.take() {
                ensure!(join.join().is_ok(), "W24C_WATCHDOG");
            }
            ensure!(
                !self.expired.load(Ordering::Acquire),
                "W24C_WATCHDOG_EXPIRED"
            );
            Ok(())
        }
    }
    impl Drop for Watchdog {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }
    #[derive(Default)]
    struct Events {
        pid: u32,
        tid: u32,
        queue: VecDeque<(u32, usize)>,
        proxies: Vec<usize>,
        retired: Vec<usize>,
        count: usize,
        refused: bool,
    }
    static CALLBACK_REFUSED: AtomicBool = AtomicBool::new(false);
    thread_local! { static EVENTS: RefCell<Option<Events>> = const { RefCell::new(None) }; }
    unsafe extern "system" fn callback(
        _: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        object: i32,
        child: i32,
        tid: u32,
        _: u32,
    ) {
        if object != OBJID_WINDOW || child != 0 || hwnd.is_null() {
            return;
        }
        EVENTS.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                CALLBACK_REFUSED.store(true, Ordering::Release);
                return;
            };
            if let Some(events) = slot.as_mut() {
                if tid != events.tid {
                    return;
                }
                if !matches!(
                    event,
                    EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW | EVENT_OBJECT_DESTROY
                ) {
                    events.refused = true;
                    return;
                }
                events.count += 1;
                if events.count > 64 {
                    events.refused = true;
                    return;
                }
                // Callback retains numeric notifications only; HWND is corroborated before any metadata.
                events.queue.push_back((event, hwnd as usize));
            }
        });
    }
    struct Hooks(Vec<HWINEVENTHOOK>);
    impl Drop for Hooks {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }
    impl Hooks {
        fn finish(&mut self) -> Result<()> {
            let mut removed = true;
            for hook in self.0.drain(..) {
                // SAFETY: exact original hook removed once on its installing thread; no foreign listener.
                removed &= unsafe { UnhookWinEvent(hook) } != 0;
            }
            EVENTS.with(|slot| *slot.borrow_mut() = None);
            ensure!(removed, "W24C_HOOK_RETIREMENT");
            Ok(())
        }
        fn install(pid: u32, tid: u32, until: Instant) -> Result<Self> {
            CALLBACK_REFUSED.store(false, Ordering::Release);
            EVENTS.with(|slot| {
                *slot.borrow_mut() = Some(Events {
                    pid,
                    tid,
                    queue: VecDeque::with_capacity(64),
                    proxies: Vec::with_capacity(2),
                    retired: Vec::with_capacity(64),
                    ..Events::default()
                })
            });
            let mut hooks = Self(Vec::new());
            for event in [EVENT_OBJECT_CREATE, EVENT_OBJECT_SHOW, EVENT_OBJECT_DESTROY] {
                ensure!(Instant::now() < until, "W24C_DEADLINE");
                // SAFETY: fixed callback ABI, only original child PID/main TID; callback TLS outlives hook.
                let hook = unsafe {
                    SetWinEventHook(
                        event,
                        event,
                        ptr::null_mut(),
                        Some(callback),
                        pid,
                        tid,
                        WINEVENT_OUTOFCONTEXT,
                    )
                };
                ensure!(!hook.is_null(), "W24C_HOOK");
                hooks.0.push(hook);
            }
            Ok(hooks)
        }
        fn pump(&self) -> Result<()> {
            ensure!(
                !CALLBACK_REFUSED.load(Ordering::Acquire),
                "W24C_CALLBACK_REENTRY"
            );
            for _ in 0..512 {
                let mut message = MSG::default();
                // SAFETY: this controller's message queue only, exact initialized writable MSG.
                if unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                // SAFETY: unmodified message obtained on this installing controller thread.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            EVENTS.with(|slot| {
                let mut slot = slot.borrow_mut();
                let events = slot.as_mut().ok_or_else(|| anyhow::anyhow!("W24C_HOOK"))?;
                ensure!(!events.refused, "W24C_EVENT_CAP");
                while let Some((event, hwnd)) = events.queue.pop_front() {
                    if event == EVENT_OBJECT_DESTROY {
                        if events.proxies.contains(&hwnd) {
                            events.retired.push(hwnd);
                        }
                        continue;
                    } // numeric notification, not teardown proof.
                    let mut pid = 0;
                    // SAFETY: identity-only query of exact PID-filtered event before any class/property access.
                    let tid = unsafe { GetWindowThreadProcessId(hwnd as HWND, &mut pid) };
                    if pid == 0 && tid == 0 {
                        continue;
                    } // disappeared before acquisition; never adopt.
                    ensure!(
                        pid == events.pid && tid == events.tid,
                        "W24C_WINDOW_IDENTITY"
                    );
                    let mut class = [0u16; 64];
                    // SAFETY: only after original exact PID/TID proof; bounded own class output.
                    let length = unsafe { GetClassNameW(hwnd as HWND, class.as_mut_ptr(), 64) };
                    ensure!(length > 0 && length < 64, "W24C_CLASS");
                    let is_proxy = &class[..length as usize]
                        == "CrosspaneProxy"
                            .encode_utf16()
                            .collect::<Vec<_>>()
                            .as_slice();
                    if is_proxy {
                        ensure!(!events.retired.contains(&hwnd), "W24C_HWND_GENERATION");
                        if !events.proxies.contains(&hwnd) {
                            ensure!(events.proxies.len() < 2, "W24C_PROXY_CAP");
                            events.proxies.push(hwnd);
                        }
                        // SAFETY: liveness/visibility only on the authenticated original child HWND.
                        ensure!(unsafe { IsWindow(hwnd as HWND) } != 0, "W24C_WINDOW_GONE");
                        // SAFETY: same fresh original PID/TID-owned HWND, visibility only.
                        let _visible = unsafe { IsWindowVisible(hwnd as HWND) } != 0;
                    }
                }
                Ok(())
            })
        }
    }
    fn environment(root: &Path) -> Result<Vec<u16>> {
        let mut vars = std::collections::BTreeMap::<String, std::ffi::OsString>::new();
        for (key, value) in std::env::vars_os() {
            let key = key.to_string_lossy().to_ascii_uppercase();
            if key.starts_with('=')
                || key.starts_with("CROSSPANE_")
                || matches!(
                    key.as_str(),
                    "DISPLAY" | "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE"
                )
            {
                continue;
            }
            vars.insert(key, value);
        }
        for (key, value) in [
            ("APPDATA", root.join("roaming")),
            ("LOCALAPPDATA", root.join("local")),
            ("USERPROFILE", root.join("home")),
            ("HOME", root.join("home")),
            ("TEMP", root.join("tmp")),
            ("TMP", root.join("tmp")),
        ] {
            vars.insert(key.into(), value.into_os_string());
        }
        for (key, value) in [
            (
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/crosspane-test-bus",
            ),
            (
                "DBUS_SYSTEM_BUS_ADDRESS",
                "unix:path=/nonexistent/crosspane-test-bus",
            ),
            ("CROSSPANE_NO_MULTICAST", "1"),
            ("CROSSPANE_WINDOWS_PROXY_GUI", "1"),
        ] {
            vars.insert(key.into(), value.into());
        }
        let mut output = Vec::new();
        for (key, value) in vars {
            output.extend(wide(std::ffi::OsStr::new(&format!(
                "{key}={}",
                value.to_string_lossy()
            )))?);
        }
        output.push(0);
        ensure!(output.len() <= 32767, "W24C_ENV_CAP");
        Ok(output)
    }
    fn root() -> Result<PathBuf> {
        ensure!(
            std::env::var("CROSSPANE_E2_SOURCE_FIXTURE").as_deref() == Ok("1"),
            "W24C_OPTIN"
        );
        // SAFETY: borrowed current-process pseudo-handle, inspected without mutation.
        limited(unsafe { GetCurrentProcess() })?;
        let root = PathBuf::from(
            std::env::var_os("CROSSPANE_E2_FIXTURE_ROOT")
                .ok_or_else(|| anyhow::anyhow!("W24C_ROOT"))?,
        );
        let nonce = root
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("crosspane-WP-W2.5a-"))
            .ok_or_else(|| anyhow::anyhow!("W24C_ROOT"))?;
        ensure!(
            nonce.len() == 32
                && nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "W24C_NONCE"
        );
        ensure!(
            root.is_absolute()
                && root
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("W24C_ROOT"))?
                    .canonicalize()
                    .map_err(|_| anyhow::anyhow!("W24C_ROOT"))?
                    == std::env::temp_dir()
                        .canonicalize()
                        .map_err(|_| anyhow::anyhow!("W24C_ROOT"))?,
            "W24C_ROOT"
        );
        ensure!(
            std::env::var_os("APPDATA").ok_or_else(|| anyhow::anyhow!("W24C_ROOT"))?
                == root.join("roaming"),
            "W24C_ROOT"
        );
        for leaf in ["fixture", "roaming", "local", "home", "tmp"] {
            super::paths::create_private_dir(&root.join(leaf))
                .map_err(|_| anyhow::anyhow!("W24C_PRIVATE"))?;
        }
        Ok(root)
    }
    fn accounting(job: &Kernel) -> Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        ensure!(
            // SAFETY: original private job handle and exact initialized writable accounting structure.
            unsafe {
                QueryInformationJobObject(
                    job.0,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    ptr::null_mut(),
                )
            } != 0,
            "W24C_JOB"
        );
        Ok(info.ActiveProcesses)
    }
    fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
        let mut file = super::fixture_security::create_private_file(path)
            .map_err(|_| anyhow::anyhow!("W24C_RECEIPT"))?;
        file.write_all(bytes)
            .map_err(|_| anyhow::anyhow!("W24C_RECEIPT"))?;
        Ok(())
    }
    fn row_output(bytes: &[u8], row: &str) -> Result<()> {
        ensure!(bytes.len() <= 512 && bytes.is_ascii(), "W24C_PROTOCOL");
        let bytes = bytes
            .strip_prefix(b"W24C_NO_RAW keyboard=0 mouse=0\n")
            .ok_or_else(|| anyhow::anyhow!("W24C_RAW_REGISTRATION"))?;
        let line = std::str::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("W24C_PROTOCOL"))?
            .strip_suffix('\n')
            .ok_or_else(|| anyhow::anyhow!("W24C_PROTOCOL"))?;
        ensure!(!line.contains(['\n', '\r']), "W24C_PROTOCOL");
        let fields: Vec<_> = line.split(' ').collect();
        ensure!(
            fields.len() == 12 && fields[0] == "W24C_ROW_PASS" && fields[1] == format!("row={row}"),
            "W24C_PROTOCOL"
        );
        for (field, name) in fields[2..].iter().zip([
            "gains_a",
            "losses_a",
            "gains_b",
            "losses_b",
            "downs_a",
            "ups_a",
            "downs_b",
            "ups_b",
            "events",
            "hwnd_retired",
        ]) {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("W24C_PROTOCOL"))?;
            ensure!(
                key == name && !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                "W24C_PROTOCOL"
            );
            let value: u32 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("W24C_PROTOCOL"))?;
            ensure!(
                value <= 512 && (name != "hwnd_retired" || value == 2),
                "W24C_PROTOCOL"
            );
        }
        Ok(())
    }
    pub(super) fn driver(row: &str) -> Result<()> {
        ensure!(matches!(row, "focus" | "no-theft"), "W24C_SELECTOR");
        let until = Instant::now() + Duration::from_secs(30);
        let root = root()?;
        let nonce = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap()
            .strip_prefix("crosspane-WP-W2.5a-")
            .unwrap();
        let marker = root.join(format!("fixture/consumed.{row}"));
        let mut consume = super::fixture_security::create_private_file(&marker)
            .map_err(|_| anyhow::anyhow!("W24C_CONSUMED"))?;
        consume
            .write_all(b"W24C_ATTEMPT_V1\n")
            .map_err(|_| anyhow::anyhow!("W24C_CONSUMED"))?;
        for leaf in [
            format!("fixture/{row}.stdout"),
            format!("fixture/{row}.stderr"),
            format!("fixture/{row}.authority.json"),
        ] {
            ensure!(
                matches!(std::fs::symlink_metadata(root.join(leaf)), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "W24C_OUTPUT_EXISTS"
            );
        }
        let image = root.join("fixture/proxy-windows-driver.exe");
        let _image_locked = image_pin(&image, until)?;
        // SAFETY: fixed unnamed job in this process; owns its returned handle, no existing job adoption.
        let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        ensure!(!raw_job.is_null(), "W24C_JOB");
        let mut job = PrivateJob {
            handle: Kernel(raw_job),
            clean: false,
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        ensure!(
            // SAFETY: original private job and exact generated initialized extended-limit struct.
            unsafe {
                SetInformationJobObject(
                    job.handle.0,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } != 0,
            "W24C_JOB"
        );
        let (stdin_read, stdin_write) = pipe()?;
        let (stdout_read, stdout_write) = pipe()?;
        let (stderr_read, stderr_write) = pipe()?;
        no_inherit(&stdin_write)?;
        no_inherit(&stdout_read)?;
        no_inherit(&stderr_read)?;
        let handles = [stdin_read.0, stdout_write.0, stderr_write.0];
        let mut length = 0usize;
        // SAFETY: documented zero-buffer size query; length is writable, no list exists yet.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut length);
        }
        ensure!(length > 0 && length <= 16384, "W24C_ATTRIBUTES");
        let mut attributes = Attributes {
            words: vec![0u128; length.div_ceil(size_of::<u128>())],
            live: false,
        };
        ensure!(
            // SAFETY: aligned initialized allocation holds requested byte count; exact one attribute.
            unsafe {
                InitializeProcThreadAttributeList(
                    attributes.words.as_mut_ptr().cast(),
                    1,
                    0,
                    &mut length,
                )
            } != 0,
            "W24C_ATTRIBUTES"
        );
        attributes.live = true;
        ensure!(
            // SAFETY: exact three inheritable anonymous CHILD endpoints live through CreateProcess; excludes all parent/job handles.
            unsafe {
                UpdateProcThreadAttribute(
                    attributes.words.as_mut_ptr().cast(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    size_of::<[HANDLE; 3]>(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            } != 0,
            "W24C_ATTRIBUTES"
        );
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin_read.0;
        startup.StartupInfo.hStdOutput = stdout_write.0;
        startup.StartupInfo.hStdError = stderr_write.0;
        startup.lpAttributeList = attributes.words.as_mut_ptr().cast();
        let application = wide(image.as_os_str())?;
        let mut command = wide(std::ffi::OsStr::new(&format!(
            "\"{}\" --row {row} --run {nonce}",
            image.display()
        )))?;
        let current = wide(root.join("fixture").as_os_str())?;
        let environment = environment(&root)?;
        let mut process = PROCESS_INFORMATION::default();
        ensure!(Instant::now() < until, "W24C_DEADLINE");
        ensure!(
            // SAFETY: fixed image/argv/environment/CWD, fully initialized STARTUPINFOEX cb/layout;
            // no parent borrowed pointer survives this synchronous call; child starts SUSPENDED.
            unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    1,
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                    environment.as_ptr().cast(),
                    current.as_ptr(),
                    &startup.StartupInfo,
                    &mut process,
                )
            } != 0,
            "W24C_CREATE"
        );
        let mut original = SuspendedProcess {
            handle: Kernel(process.hProcess),
            assigned: false,
        };
        let main_thread = Kernel(process.hThread);
        ensure!(
            // SAFETY: original returned handles exclusively owned here; assign before resume/windows.
            unsafe { AssignProcessToJobObject(job.handle.0, original.handle.0) } != 0,
            "W24C_ASSIGN"
        );
        original.assigned = true;
        let mut watchdog = Watchdog::start(&job.handle)?;
        ensure!(
            // SAFETY: exact original handle IDs/liveness, no OpenProcess or PID-based acquisition.
            unsafe { GetProcessId(original.handle.0) } == process.dwProcessId
                // SAFETY: retained original primary-thread handle returned by this CreateProcess.
                && unsafe { GetThreadId(main_thread.0) } == process.dwThreadId
                // SAFETY: zero-time liveness query of the retained original process handle.
                && unsafe { WaitForSingleObject(original.handle.0, 0) } == WAIT_TIMEOUT,
            "W24C_ORIGINAL"
        );
        let created = creation(original.handle.0)?;
        ensure!(
            created != 0 && accounting(&job.handle)? == 1,
            "W24C_ORIGINAL"
        );
        limited(original.handle.0)?;
        let mut actual = [0u16; 1024];
        let mut units = 1024;
        ensure!(
            // SAFETY: original query-capable process handle and initialized bounded image-path output.
            unsafe {
                QueryFullProcessImageNameW(original.handle.0, 0, actual.as_mut_ptr(), &mut units)
            } != 0
                && units < 1024,
            "W24C_IMAGE_IDENTITY"
        );
        let actual = PathBuf::from(
            String::from_utf16(&actual[..units as usize])
                .map_err(|_| anyhow::anyhow!("W24C_IMAGE_IDENTITY"))?,
        );
        ensure!(
            actual
                .canonicalize()
                .map_err(|_| anyhow::anyhow!("W24C_IMAGE_IDENTITY"))?
                == image
                    .canonicalize()
                    .map_err(|_| anyhow::anyhow!("W24C_IMAGE_IDENTITY"))?,
            "W24C_IMAGE_IDENTITY"
        );
        let mut hooks = Hooks::install(process.dwProcessId, process.dwThreadId, until)?;
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);
        drop(attributes);
        let mut stdout = PipeReader::new(stdout_read);
        let mut stderr = PipeReader::new(stderr_read);
        ensure!(Instant::now() < until, "W24C_DEADLINE");
        // SAFETY: original primary thread is suspended and in our non-breakaway job with hooks installed.
        ensure!(unsafe { ResumeThread(main_thread.0) } == 1, "W24C_RESUME");
        let raw = stdin_write.0;
        std::mem::forget(stdin_write);
        // SAFETY: transfer exactly the one original anonymous parent write endpoint to File.
        let mut start = unsafe { File::from_raw_handle(raw.cast()) };
        start
            .write_all(b"START\n")
            .map_err(|_| anyhow::anyhow!("W24C_START"))?;
        drop(start);
        loop {
            hooks.pump()?;
            stdout.poll()?;
            stderr.poll()?;
            ensure!(creation(original.handle.0)? == created, "W24C_ORIGINAL");
            // SAFETY: zero-time wait on retained original process, never replacement PID.
            match unsafe { WaitForSingleObject(original.handle.0, 0) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT => {}
                _ => anyhow::bail!("W24C_WAIT"),
            }
            ensure!(
                Instant::now() < until && !watchdog.expired.load(Ordering::Acquire),
                "W24C_DEADLINE"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut exit = u32::MAX;
        ensure!(
            // SAFETY: signalled original process handle remains open, writable DWORD exit output.
            unsafe { GetExitCodeProcess(original.handle.0, &mut exit) } != 0,
            "W24C_EXIT"
        );
        while accounting(&job.handle)? != 0 {
            ensure!(Instant::now() < until, "W24C_JOB_NONEMPTY");
            std::thread::sleep(Duration::from_millis(10));
        }
        while !stdout.eof || !stderr.eof {
            stdout.poll()?;
            stderr.poll()?;
            ensure!(Instant::now() < until, "W24C_DRAIN_DEADLINE");
        }
        watchdog.finish()?;
        let observed = EVENTS.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|events| events.proxies.len())
                .unwrap_or(0)
        });
        hooks.finish()?;
        job.clean = true;
        let stdout = stdout.bytes;
        let stderr = stderr.bytes;
        write_new(&root.join(format!("fixture/{row}.stdout")), &stdout)?;
        write_new(&root.join(format!("fixture/{row}.stderr")), &stderr)?;
        let authority = serde_json::json!({"schema":1,"pid":process.dwProcessId,"tid":process.dwThreadId,
            "created_filetime":created,"limited":true,"assigned_before_resume":true,"hooks_before_resume":true,
            "original_process_exit":true,"exit_code":exit,"job_empty":true,"watchdog_expired":false});
        write_new(
            &root.join(format!("fixture/{row}.authority.json")),
            &serde_json::to_vec(&authority).map_err(|_| anyhow::anyhow!("W24C_RECEIPT"))?,
        )?;
        println!(
            "W24C_CONTROLLER original_process_exit=true job_empty=true exit_code={exit} stdout_bytes={} stderr_bytes={}",
            stdout.len(),
            stderr.len()
        );
        // Persist actual owned exit/job/streams before refusing a foreground-lock U or row fault.
        // Retirement evidence is separate from the two-proxy/PASS assertions below.
        ensure!(observed == 2, "W24C_PROXY_DISCOVERY");
        ensure!(exit == 0 && stderr.is_empty(), "W24C_ROW");
        row_output(&stdout, row)?;
        Ok(())
    }
}

#[test]
#[ignore = "ROOT-released Limited original-child focus row only"]
fn owned_proxy_focus() {
    if std::env::var("CROSSPANE_E2_SOURCE_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    assert!(
        proxy_controller::driver("focus").is_ok(),
        "W24C_FOCUS_REFUSED"
    );
}
#[test]
#[ignore = "ROOT-released Limited original-child no-theft row only"]
fn owned_proxy_no_theft() {
    if std::env::var("CROSSPANE_E2_SOURCE_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    assert!(
        proxy_controller::driver("no-theft").is_ok(),
        "W24C_NO_THEFT_REFUSED"
    );
}
