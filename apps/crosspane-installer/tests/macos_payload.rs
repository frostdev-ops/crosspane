#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Explicit scratch roots, fake GUI/signature/process observations; no real native commands.
use crosspane_installer::agent_contract;
#[path = "../src/platform/macos/launchd_observation.rs"]
#[allow(dead_code)]
mod launchd_observation;
#[path = "../src/legacy_payload.rs"]
mod legacy_payload;
#[path = "../src/platform/macos/native_io.rs"]
#[allow(dead_code, unused_imports)]
mod native_io;
#[path = "../src/platform/macos/payload.rs"]
#[allow(dead_code)]
mod payload;
#[path = "../src/platform/macos/transport.rs"]
#[allow(dead_code, unused_imports)]
mod transport;
use agent_contract::*;
use native_io::*;
use payload::*;
use rustix::{fd::OwnedFd, fs as rfs};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::net::UnixListener,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use transport::SelectedAgent;

const STATUS: &[u8] = br#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"test-1","features":["private-vdisplay","video"]},"instance":{"id":1,"pid":4242,"uid":1,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":0},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store",
"permissions":[{"name":"screen_recording","state":"granted"},{"name":"accessibility","state":"granted"},{"name":"input_monitoring","state":"granted"},{"name":"microphone","state":"granted"}],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
struct Support {
    observation: Mutex<SupportObservation>,
}
impl SupportProbe for Support {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        Ok(self.observation.lock().unwrap().clone())
    }
}
struct Signatures {
    change: Mutex<Option<(ArtifactRole, usize)>>,
    calls: Mutex<Vec<(PathBuf, ArtifactRole)>>,
}
impl SignatureProbe for Signatures {
    fn observe(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        deadline.check()?;
        self.calls
            .lock()
            .unwrap()
            .push((path.to_owned(), approved.role));
        let mut value = SignatureObservation {
            strict_verified: true,
            team_identifier: "ABCDE12345".into(),
            identifier: approved.identifier.clone(),
            designated_requirement: approved.designated_requirement.clone(),
            entitlements: approved.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        };
        if let Some((role, mutation)) = *self.change.lock().unwrap()
            && role == approved.role
        {
            match mutation {
                0 => value.strict_verified = false,
                1 => value.team_identifier = "OTHER12345".into(),
                2 => value.team_identifier.clear(),
                3 => value.identifier = "foreign.identifier".into(),
                4 => value.designated_requirement = "foreign requirement".into(),
                5 => {
                    value
                        .entitlements
                        .insert("com.apple.security.device.audio-input".into(), true);
                }
                6 => value.apple_development = false,
                7 => value.hardened_runtime = false,
                8 => value.ad_hoc = true,
                _ => panic!("unknown mutation"),
            }
        }
        Ok(value)
    }
}
struct Runner {
    uid: u32,
    exe: PathBuf,
    pid: AtomicU64,
    stopped: AtomicBool,
    fail_bundle: AtomicBool,
    calls: Mutex<Vec<PathBuf>>,
}
impl CommandRunner for Runner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        deadline.check()?;
        self.calls.lock().unwrap().push(spec.program().to_owned());
        assert_eq!(spec.environment()["LC_ALL"], "C");
        assert_eq!(spec.environment()["TZ"], "UTC");
        assert_ne!(spec.program(), Path::new("/bin/launchctl"));
        if spec.program() == Path::new("/bin/ps") {
            let pid: u64 = spec.args()[3].parse().unwrap();
            if (pid == 4242 && self.stopped.load(Ordering::Acquire))
                || pid != self.pid.load(Ordering::Acquire)
            {
                return Ok(CommandOutput {
                    code: Some(1),
                    stdout: vec![],
                    stderr: vec![],
                });
            }
            let bytes = match spec.args()[1].as_str() {
                "uid=" => format!("{}\n", self.uid).into_bytes(),
                "comm=" => format!("{}\n", self.exe.display()).into_bytes(),
                "lstart=" => {
                    if pid == 4242 {
                        b"Thu Jan  1 00:00:00 1970\n".to_vec()
                    } else {
                        b"Thu Jan  1 00:00:01 1970\n".to_vec()
                    }
                }
                _ => panic!("unexpected ps"),
            };
            return Ok(CommandOutput {
                code: Some(0),
                stdout: bytes,
                stderr: vec![],
            });
        }
        assert_eq!(spec.program(), Path::new("/usr/bin/codesign"));
        assert_eq!(&spec.args()[..2], &["--verify", "--strict"]);
        Ok(CommandOutput {
            code: Some(if self.fail_bundle.load(Ordering::Acquire) {
                1
            } else {
                0
            }),
            stdout: vec![],
            stderr: vec![],
        })
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
static ROOTS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Scratch>>>> = OnceLock::new();
const DIRECTORY_FLAGS: rfs::OFlags = rfs::OFlags::RDONLY
    .union(rfs::OFlags::DIRECTORY)
    .union(rfs::OFlags::NOFOLLOW)
    .union(rfs::OFlags::CLOEXEC);
struct Scratch {
    path: PathBuf,
    name: String,
    parent: OwnedFd,
    fd: OwnedFd,
    _container: Option<Arc<Scratch>>,
}
impl Scratch {
    fn create(name: String) -> rustix::io::Result<Arc<Self>> {
        let mut parent = rfs::open("/", DIRECTORY_FLAGS, rfs::Mode::empty())?;
        for component in ["private", "tmp"] {
            parent = rfs::openat(&parent, component, DIRECTORY_FLAGS, rfs::Mode::empty())?;
        }
        Self::create_at(parent, PathBuf::from("/private/tmp"), name, None)
    }
    fn create_in(container: &Arc<Self>, name: String) -> rustix::io::Result<Arc<Self>> {
        Self::create_at(
            container.fd.try_clone().unwrap(),
            container.path.clone(),
            name,
            Some(container.clone()),
        )
    }
    fn create_at(
        parent: OwnedFd,
        path: PathBuf,
        name: String,
        container: Option<Arc<Self>>,
    ) -> rustix::io::Result<Arc<Self>> {
        if !name.starts_with("cp-c1-")
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(rustix::io::Errno::INVAL);
        }
        // mkdirat is exclusive: an existing file, directory or symlink is never reused.
        rfs::mkdirat(&parent, name.as_str(), rfs::Mode::RWXU)?;
        let fd = rfs::openat(&parent, name.as_str(), DIRECTORY_FLAGS, rfs::Mode::empty())?;
        let stat = rfs::fstat(&fd)?;
        assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
        assert_eq!(stat.st_mode & 0o777, 0o700);
        let root = Arc::new(Self {
            path: path.join(&name),
            name,
            parent,
            fd,
            _container: container,
        });
        ROOTS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .insert(root.path.clone(), Arc::downgrade(&root));
        Ok(root)
    }
    fn directory(&self, path: &Path) -> OwnedFd {
        let mut fd = self.fd.try_clone().unwrap();
        for component in path.strip_prefix(&self.path).unwrap().components() {
            let Component::Normal(name) = component else {
                panic!("invalid scratch component")
            };
            match rfs::mkdirat(&fd, name, rfs::Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => panic!("scratch mkdir: {error}"),
            }
            fd = rfs::openat(&fd, name, DIRECTORY_FLAGS, rfs::Mode::empty()).unwrap();
            let stat = rfs::fstat(&fd).unwrap();
            assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
            assert_eq!(stat.st_mode & 0o777, 0o700);
        }
        fd
    }
}
fn same_inode(a: &rfs::Stat, b: &rfs::Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
fn clear_owned(fd: &OwnedFd) {
    for entry in rfs::Dir::read_from(fd).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let before = rfs::statat(fd, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let directory = before.st_mode & 0o170000 == 0o040000;
        if directory {
            let child = rfs::openat(fd, name, DIRECTORY_FLAGS, rfs::Mode::empty()).unwrap();
            assert!(same_inode(&before, &rfs::fstat(&child).unwrap()));
            clear_owned(&child);
        }
        assert!(same_inode(
            &before,
            &rfs::statat(fd, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap()
        ));
        rfs::unlinkat(
            fd,
            name,
            if directory {
                rfs::AtFlags::REMOVEDIR
            } else {
                rfs::AtFlags::empty()
            },
        )
        .unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let at_name = rfs::statat(
            &self.parent,
            self.name.as_str(),
            rfs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
        assert!(same_inode(&at_name, &rfs::fstat(&self.fd).unwrap()));
        clear_owned(&self.fd);
        rfs::unlinkat(&self.parent, self.name.as_str(), rfs::AtFlags::REMOVEDIR).unwrap();
    }
}
fn owned_root(path: &Path) -> Arc<Scratch> {
    // The registry guard is released before panicking, so the deliberate unowned-path probe
    // can't poison it for later tests in the same process.
    let found = ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(root, owned)| path.starts_with(root).then(|| owned.upgrade()).flatten())
        .max_by_key(|root| root.path.components().count());
    found.unwrap_or_else(|| panic!("unowned scratch path"))
}
fn directory(path: &Path) {
    owned_root(path).directory(path);
}
fn owned_parent(path: &Path) -> (Arc<Scratch>, OwnedFd, &std::ffi::OsStr) {
    let root = owned_root(path);
    let parent = root.directory(path.parent().unwrap());
    (root, parent, path.file_name().unwrap())
}
fn owned_stat(path: &Path) -> rfs::Stat {
    let (_root, parent, name) = owned_parent(path);
    let stat = rfs::statat(&parent, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
    assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
    stat
}
fn remove_owned(path: &Path) {
    let (_root, parent, name) = owned_parent(path);
    let before = owned_stat(path);
    assert!(same_inode(
        &before,
        &rfs::statat(&parent, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap()
    ));
    rfs::unlinkat(&parent, name, rfs::AtFlags::empty()).unwrap();
}
fn rename_owned(old: &Path, new: &Path) {
    let (_old_root, old_parent, old_name) = owned_parent(old);
    let (_new_root, new_parent, new_name) = owned_parent(new);
    let before = owned_stat(old);
    rfs::renameat_with(
        &old_parent,
        old_name,
        &new_parent,
        new_name,
        rfs::RenameFlags::NOREPLACE,
    )
    .unwrap();
    assert!(same_inode(&before, &owned_stat(new)));
}
fn symlink_owned(target: &Path, link: &Path) {
    let (_root, parent, name) = owned_parent(link);
    rfs::symlinkat(target, &parent, name).unwrap();
}
fn hardlink_owned(source: &Path, link: &Path) {
    let (_source_root, source_parent, source_name) = owned_parent(source);
    let (_link_root, link_parent, link_name) = owned_parent(link);
    let before = owned_stat(source);
    assert_eq!(before.st_mode & 0o170000, 0o100000);
    rfs::linkat(
        &source_parent,
        source_name,
        &link_parent,
        link_name,
        rfs::AtFlags::empty(),
    )
    .unwrap();
    assert!(same_inode(&before, &owned_stat(link)));
}
fn chmod_owned(path: &Path, mode: u32) {
    let (_root, parent, name) = owned_parent(path);
    let before = owned_stat(path);
    assert_ne!(before.st_mode & 0o170000, 0o120000);
    let mode = rfs::Mode::from_bits_truncate(mode.try_into().unwrap());
    if before.st_mode & 0o170000 == 0o140000 {
        rfs::chmodat(&parent, name, mode, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
    } else {
        let fd = rfs::openat(
            &parent,
            name,
            rfs::OFlags::RDONLY | rfs::OFlags::NOFOLLOW | rfs::OFlags::CLOEXEC,
            rfs::Mode::empty(),
        )
        .unwrap();
        assert!(same_inode(&before, &rfs::fstat(&fd).unwrap()));
        rfs::fchmod(&fd, mode).unwrap();
    }
    assert!(same_inode(&before, &owned_stat(path)));
}
fn bytes(path: &Path, data: &[u8], mode: u32) {
    let root = owned_root(path);
    let parent = root.directory(path.parent().unwrap());
    let fd = rfs::openat(
        &parent,
        path.file_name().unwrap(),
        rfs::OFlags::WRONLY | rfs::OFlags::CREATE | rfs::OFlags::NOFOLLOW | rfs::OFlags::CLOEXEC,
        rfs::Mode::WUSR | rfs::Mode::RUSR,
    )
    .unwrap();
    let stat = rfs::fstat(&fd).unwrap();
    assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
    assert_eq!(stat.st_mode & 0o170000, 0o100000);
    assert_eq!(stat.st_nlink, 1);
    rfs::fchmod(&fd, rfs::Mode::from_bits_truncate(mode.try_into().unwrap())).unwrap();
    let mut file = fs::File::from(fd);
    file.set_len(0).unwrap();
    file.write_all(data).unwrap();
}
fn sha(data: &[u8]) -> [u8; 32] {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data)
        .as_ref()
        .try_into()
        .unwrap()
}
fn macho(embedded: bool, marker: u8) -> Vec<u8> {
    let mut data = vec![0; 33];
    for (at, value) in [
        (0, 0xfeedfacfu32),
        (4, 0x0100000c),
        (12, if embedded { 6 } else { 2 }),
    ] {
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    data[32] = marker;
    data
}
fn inventory() -> ApprovedInventory {
    let entries = [
        (
            "Crosspane.app/Contents/MacOS/Crosspane",
            Some(PayloadRole::Agent),
        ),
        (
            "Crosspane.app/Contents/MacOS/crosspane-ui",
            Some(PayloadRole::Settings),
        ),
        (
            "Crosspane.app/Contents/Frameworks/libfixture.dylib",
            Some(PayloadRole::EmbeddedCode),
        ),
        ("Crosspane.app/Contents/Info.plist", None),
        ("crosspanectl", Some(PayloadRole::Ctl)),
        ("crosspane-installer", Some(PayloadRole::Installer)),
    ];
    ApprovedInventory { product_version: "test-1".into(), features: vec!["video".into(), "private-vdisplay".into()],
        files: entries.into_iter().map(|(path, role)| {
            let data = if role.is_some() { macho(role == Some(PayloadRole::EmbeddedCode), 1) } else { b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec() };
            PayloadFile { path: path.into(), size: data.len() as u64, sha256: sha(&data), mode: if role.is_some() { 0o755 } else { 0o644 },
                signing: role.map(|role| SigningRule { role, identifier: if role == PayloadRole::Agent { AGENT_LABEL.into() } else { format!("test.approved.{role:?}") },
                    designated_requirement: "trusted-test-development-requirement".into(), entitlements: if role == PayloadRole::Agent { BTreeMap::from([("com.apple.security.device.audio-input".into(), true)]) } else { BTreeMap::new() } }) }
        }).collect() }
}
struct Fixture {
    _scratch: Arc<Scratch>,
    root: PathBuf,
    home: PathBuf,
    source: PathBuf,
    runtime: PathBuf,
    io: Arc<MacNativeIo>,
    clock: Arc<FakeClock>,
    support: Arc<Support>,
    signatures: Arc<Signatures>,
    runner: Arc<Runner>,
    _listener: Option<UnixListener>,
}
impl Fixture {
    fn new(installed: bool) -> Self {
        // The start time keeps names unique when a reused pid meets a leftover directory.
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let scratch = Scratch::create(format!(
            "cp-c1-{}-{started}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap();
        let root = scratch.path.clone();
        let home = root.join("h");
        let tmp = root.join("t");
        let source = tmp.join("payload");
        let runtime = tmp.join("crosspane");
        for path in [&home, &tmp, &source, &runtime] {
            directory(path);
        }
        let approved = inventory();
        for file in &approved.files {
            let data = if let Some(rule) = &file.signing {
                macho(rule.role == PayloadRole::EmbeddedCode, 1)
            } else {
                b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec()
            };
            bytes(&source.join(&file.path), &data, file.mode);
            if installed && file.path != "crosspane-installer" {
                let final_path = if file.path == "crosspanectl" {
                    home.join(".local/bin/crosspanectl")
                } else {
                    home.join("Applications").join(&file.path)
                };
                let data = if let Some(rule) = &file.signing {
                    macho(rule.role == PayloadRole::EmbeddedCode, 0)
                } else {
                    data
                };
                bytes(&final_path, &data, file.mode);
            }
        }
        let uid = rustix::process::geteuid().as_raw();
        let target = MacTarget::scratch(TargetPaths {
            uid,
            home: home.clone(),
            gui_tmpdir: tmp.clone(),
            runtime_override: None,
            payload_root: source.clone(),
        })
        .unwrap();
        let support = Arc::new(Support {
            observation: Mutex::new(SupportObservation {
                macos_major: 26,
                apple_silicon: true,
                gui: GuiObservation {
                    console_uid: Some(uid),
                    interactive_uid: Some(uid),
                    console_session: "fake-selected-Aqua".into(),
                    interactive_session: "fake-selected-Aqua".into(),
                    active: true,
                },
                gui_tmpdir: tmp,
            }),
        });
        let signatures = Arc::new(Signatures {
            change: Mutex::default(),
            calls: Mutex::default(),
        });
        let runner = Arc::new(Runner {
            uid,
            exe: target.agent_path(),
            pid: AtomicU64::new(4242),
            stopped: AtomicBool::new(false),
            fail_bundle: AtomicBool::new(false),
            calls: Mutex::default(),
        });
        let clock = Arc::new(FakeClock::default());
        let listener = UnixListener::bind(target.socket_path()).unwrap();
        // Sole pathname-bind exception: fresh, exclusive 0700 root, never reused.
        let socket = owned_stat(&target.socket_path());
        assert_eq!(socket.st_mode & 0o170000, 0o140000);
        chmod_owned(&target.socket_path(), 0o600);
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support.clone(),
                signatures.clone(),
                clock.clone(),
            )
            .unwrap(),
        );
        let fixture = Self {
            _scratch: scratch,
            root,
            home,
            source,
            runtime,
            io,
            clock,
            support,
            signatures,
            runner,
            _listener: Some(listener),
        };
        fixture.bootstrap(1, 4242, 0, "ready");
        fixture
    }
    fn deadline(&self) -> Deadline {
        Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn payload(&self) -> MacPayload {
        MacPayload::admit(self.io.clone(), inventory(), &self.deadline()).unwrap()
    }
    fn bootstrap(&self, id: u64, pid: u32, started: u64, phase: &str) {
        bytes(&self.runtime.join("bootstrap.json"), &serde_json::to_vec(&json!({"schema_version":1,"instance_id":id,"pid":pid,"started_unix_ms":started,
            "phase":phase,"phase_seq":1,"keystore":"os_store","reason":null,"runtime_dir":self.runtime})).unwrap(), 0o600);
    }
    fn selected(&self) -> SelectedAgent {
        self.selected_on(self.io.clone())
    }
    fn selected_on(&self, io: Arc<MacNativeIo>) -> SelectedAgent {
        let rule = inventory().files[0].signing.as_ref().unwrap().clone();
        let approved = SigningRequirement {
            role: ArtifactRole::Agent,
            identifier: rule.identifier,
            designated_requirement: rule.designated_requirement,
            entitlements: rule.entitlements,
        };
        let main = io
            .admit_main_signature(&io.target().agent_path(), &approved, &self.deadline())
            .unwrap();
        let support = io.admit_support(&main, &self.deadline()).unwrap();
        let instance = Arc::new(
            io.admit_instance(&support, &main, &self.deadline())
                .unwrap(),
        );
        SelectedAgent {
            io,
            support,
            instance,
            link: None,
        }
    }
    fn status(&self, id: u64) -> Value {
        let mut value: Value = serde_json::from_slice(STATUS).unwrap();
        value["result"]["installer"]["instance"] = json!({"id":id,"pid":if id == 1 {4242}else{4243},"uid":self.runner.uid,
            "exe":self.runner.exe,"runtime_dir":self.runtime,"started_unix_ms":if id == 1 {0}else{1000}});
        value
    }
    fn health(&self, value: &Value) -> Box<HealthSnapshot> {
        match parse_status(&serde_json::to_vec(value).unwrap(), AgentPlatform::Macos).unwrap() {
            StatusAdmission::Supported(h) => h,
            other => panic!("{other:?}"),
        }
    }
    fn original(&self) -> Arc<OriginalAgent> {
        Arc::new(
            OriginalAgent::capture(
                self.selected(),
                &self.health(&self.status(1)),
                &self.deadline(),
            )
            .unwrap(),
        )
    }
    fn exit(&self, clean: bool, parking: &str) {
        self.runner.stopped.store(true, Ordering::Release);
        bytes(
            &self.io.target().state_dir().join("last_exit.json"),
            &serde_json::to_vec(
                &json!({"schema_version":1,"instance_id":1,"stopped_unix_ms":1000,
            "clean":clean,"parking":parking,"input_journals_empty":clean,"audio_stopped":clean}),
            )
            .unwrap(),
            0o600,
        );
    }
    fn install(
        &self,
        original: Option<Arc<OriginalAgent>>,
        gate: Option<&CleanStopGate>,
    ) -> PendingPayload {
        let payload = self.payload();
        let plan = payload.plan(1, 1, original, &self.deadline()).unwrap();
        let consent = plan.consent(1, 1, true).unwrap();
        payload
            .install(plan, consent, gate, &self.deadline())
            .unwrap()
            .unwrap()
    }
    fn start_new(&self) -> SelectedAgent {
        self.runner.pid.store(4243, Ordering::Release);
        self.bootstrap(2, 4243, 1000, "ready");
        self.clock.0.store(100, Ordering::Release);
        self.selected()
    }
    fn reply(&self, value: &Value, id: u64) -> AgentReply {
        AgentReply {
            id,
            observed_at_ms: self.clock.now_ms(),
            source: ObservationSource::Demo,
            result: Ok(DecodedReply::Status(StatusAdmission::Supported(
                self.health(value),
            ))),
        }
    }
    fn hooked(&self, hook: TestHook) -> Arc<MacNativeIo> {
        let mut target = self.io.target().clone();
        target.test_hook = Some(hook);
        Arc::new(
            MacNativeIo::new(
                target,
                self.runner.clone(),
                self.support.clone(),
                self.signatures.clone(),
                self.clock.clone(),
            )
            .unwrap(),
        )
    }
    fn operated(&self, filesystem: Arc<dyn FilesystemOps>) -> Arc<MacNativeIo> {
        Arc::new(
            MacNativeIo::new(
                self.io.target().clone(),
                self.runner.clone(),
                self.support.clone(),
                self.signatures.clone(),
                self.clock.clone(),
            )
            .unwrap()
            .with_filesystem(filesystem),
        )
    }
}

#[test]
fn exclusive_scratch_root_and_helpers_refuse_existing_aliases_before_writing() {
    let f = Fixture::new(false);
    let original = fs::read(f.source.join("crosspanectl")).unwrap();
    let name = format!("cp-c1-{}", NEXT.fetch_add(1, Ordering::AcqRel));
    let child = Scratch::create_in(&f._scratch, name.clone()).unwrap();
    assert!(matches!(
        Scratch::create_in(&f._scratch, name),
        Err(rustix::io::Errno::EXIST)
    ));
    drop(child);
    let name = format!("cp-c1-{}-collision", NEXT.fetch_add(1, Ordering::AcqRel));
    let collision = f.root.join(&name);
    symlink_owned(&f.home, &collision);
    let link = owned_stat(&collision);
    assert!(matches!(
        Scratch::create_in(&f._scratch, name),
        Err(rustix::io::Errno::EXIST)
    ));
    assert!(same_inode(&link, &owned_stat(&collision)));
    remove_owned(&collision);
    let alias = f.home.join("escape");
    symlink_owned(&f.source, &alias);
    assert!(
        std::panic::catch_unwind(|| bytes(&alias.join("crosspanectl"), b"must-not-write", 0o755))
            .is_err()
    );
    assert!(std::panic::catch_unwind(|| chmod_owned(&alias, 0o777)).is_err());
    assert!(
        std::panic::catch_unwind(|| remove_owned(Path::new("/private/tmp/not-owned"))).is_err()
    );
    assert_eq!(fs::read(f.source.join("crosspanectl")).unwrap(), original);
}

#[test]
fn fresh_publish_is_pending_until_admitted_health_then_rerun_is_matching() {
    let f = Fixture::new(false);
    let payload = f.payload();
    let mut pending = f.install(None, None);
    assert_eq!(pending.phase(), PayloadPhase::Published);
    let record = payload.recovery(&f.deadline()).unwrap().record.unwrap();
    assert_eq!(record.phase, PayloadPhase::Published);
    assert_ne!(
        record.receipt.manifest_sha256,
        record.receipt.payload_sha256
    );
    assert_eq!(
        fs::read(f.source.join("crosspane-installer")).unwrap(),
        macho(false, 1)
    );
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    let verified = payload
        .verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(verified.instance_id, 2);
    assert_eq!(verified.source, ObservationSource::Demo);
    assert_eq!(verified.receipt.resources.len(), 2);
    assert!(verified.receipt.unfinished.is_empty());
    assert_eq!(pending.phase(), PayloadPhase::Verified);
    let plan = f.payload().plan(2, 2, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), PayloadState::Matching);
    let consent = plan.consent(2, 2, false).unwrap();
    assert!(
        f.payload()
            .install(plan, consent, None, &f.deadline())
            .unwrap()
            .is_none()
    );
}
#[test]
fn replacement_requires_bound_consent_clean_exit_and_keeps_backup_until_new_instance() {
    let f = Fixture::new(true);
    let payload = f.payload();
    let original = f.original();
    let plan = payload
        .plan(1, 1, Some(original.clone()), &f.deadline())
        .unwrap();
    assert_eq!(plan.state(), PayloadState::AdoptionRequired);
    assert!(plan.consent(1, 1, false).is_err());
    assert!(plan.consent(2, 1, true).is_err());
    let consent = plan.consent(1, 1, true).unwrap();
    assert!(payload.install(plan, consent, None, &f.deadline()).is_err());
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(gate.instance_id(), 1);
    let mut pending = f.install(Some(original), Some(&gate));
    let backup = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane");
    assert_eq!(fs::read(&backup).unwrap(), macho(false, 0));
    assert_eq!(
        fs::read(f.io.target().agent_path()).unwrap(),
        macho(false, 1)
    );
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    let mut bad = f.status(2);
    bad["result"]["installer"]["startup_recovery"] = json!("failed");
    assert!(
        payload
            .verify(
                &mut pending,
                &selected,
                &f.reply(&bad, 1),
                None,
                &f.deadline()
            )
            .is_err()
    );
    assert!(backup.exists());
    pending.expect_health(2).unwrap();
    let reply = f.reply(&f.status(2), 2);
    payload
        .verify(
            &mut pending,
            &selected,
            &reply,
            Some("1111111111111111"),
            &f.deadline(),
        )
        .unwrap();
    assert!(!backup.exists());
    assert!(
        !f.home
            .join(".local/bin/.crosspanectl.crosspane-previous")
            .exists()
    );
}
#[test]
fn every_artifact_requires_strict_own_identity_requirement_entitlements_and_main_team() {
    for role in [
        ArtifactRole::Agent,
        ArtifactRole::Settings,
        ArtifactRole::Ctl,
        ArtifactRole::Installer,
        ArtifactRole::EmbeddedCode,
    ] {
        for mutation in 0..9 {
            if (role == ArtifactRole::Agent && mutation == 1)
                || (role == ArtifactRole::Agent && mutation == 5)
            {
                continue;
            }
            let f = Fixture::new(false);
            *f.signatures.change.lock().unwrap() = Some((role, mutation));
            assert!(
                MacPayload::admit(f.io.clone(), inventory(), &f.deadline()).is_err(),
                "{role:?}/{mutation}"
            );
            assert!(!f.home.join("Applications").exists());
        }
    }
}
#[test]
fn trusted_inventory_requires_exact_roles_paths_sizes_hashes_and_bounded_features() {
    assert_eq!(MAX_PAYLOAD_FILES, 128);
    for case in 0..15 {
        let f = Fixture::new(false);
        let mut input = inventory();
        match case {
            0 => input.files[0].path = "../escape".into(),
            1 => input.files[0].path = "/absolute".into(),
            2 => input.files[0].path = "Crosspane.app//Contents/MacOS/Crosspane".into(),
            3 => input.files.push(input.files[0].clone()),
            4 => input.files[0].sha256 = [0; 32],
            5 => input.files[0].size += 1,
            6 => input.files[0].size = MAX_PAYLOAD_BYTES,
            7 => input.files[0].signing = None,
            8 => input.files[1].signing.as_mut().unwrap().identifier.clear(),
            9 => input.files[2]
                .signing
                .as_mut()
                .unwrap()
                .entitlements
                .insert("audio".into(), true)
                .map(|_| ())
                .unwrap_or(()),
            10 => {
                input.files.remove(5);
            }
            11 => input.files[0].mode = 0o777,
            12 => input.features = vec!["video".into(), "video".into()],
            13 => input.features = vec!["x".repeat(65)],
            14 => {
                input.files.remove(4);
            }
            _ => unreachable!(),
        }
        assert!(
            MacPayload::admit(f.io.clone(), input, &f.deadline()).is_err(),
            "case {case}"
        );
    }
}
#[test]
fn missing_extra_symlink_hardlink_and_wrong_mode_tree_entries_fail_closed() {
    for case in 0..6 {
        let f = Fixture::new(false);
        let path = f.source.join("Crosspane.app/Contents/MacOS/crosspane-ui");
        match case {
            0 => remove_owned(&path),
            1 => bytes(&f.source.join("unlisted"), b"foreign", 0o644),
            2 => {
                remove_owned(&path);
                symlink_owned(Path::new("crosspane-tutorial"), &path);
            }
            3 => hardlink_owned(&path, &f.root.join("outside-inventory-link")),
            4 => chmod_owned(&path, 0o777),
            5 => directory(&f.source.join("Crosspane.app/empty-unlisted")),
            _ => unreachable!(),
        }
        assert!(MacPayload::admit(f.io.clone(), inventory(), &f.deadline()).is_err());
    }
}
#[test]
fn malformed_macho_and_wrong_architecture_are_not_admitted_even_with_approved_hash() {
    for case in 0..5 {
        let f = Fixture::new(false);
        let mut input = inventory();
        let mut data = macho(false, 1);
        match case {
            0 => data[4..8].copy_from_slice(&0x01000007u32.to_le_bytes()),
            1 => data.truncate(20),
            2 => data[12..16].copy_from_slice(&6u32.to_le_bytes()),
            3 => data[16..20].copy_from_slice(&1u32.to_le_bytes()),
            4 => data[20..24].copy_from_slice(&u32::MAX.to_le_bytes()),
            _ => unreachable!(),
        }
        input.files[0].sha256 = sha(&data);
        input.files[0].size = data.len() as u64;
        bytes(&f.source.join(&input.files[0].path), &data, 0o755);
        assert!(MacPayload::admit(f.io.clone(), input, &f.deadline()).is_err());
    }
}
#[test]
fn unsupported_gui_or_bundle_signature_never_creates_payload_targets() {
    for case in 0..5 {
        let f = Fixture::new(false);
        {
            let mut support = f.support.observation.lock().unwrap();
            match case {
                0 => support.apple_silicon = false,
                1 => support.macos_major = 25,
                2 => support.gui.active = false,
                3 => support.gui.interactive_session = "SSH-only".into(),
                4 => f.runner.fail_bundle.store(true, Ordering::Release),
                _ => unreachable!(),
            }
        }
        let admitted = MacPayload::admit(f.io.clone(), inventory(), &f.deadline());
        assert_eq!(admitted.is_ok(), case < 2);
        assert!(!f.home.join("Applications").exists());
    }
}
#[test]
fn legacy_cli_and_path_shadowing_are_reported_and_never_erased() {
    let f = Fixture::new(false);
    let legacy = f.home.join(".cargo/bin/crosspanectl");
    bytes(&legacy, b"manual unsigned old CLI", 0o755);
    let shadow = f.home.join("custom/bin/crosspanectl");
    bytes(&shadow, b"foreign CLI", 0o755);
    let result: CliInventory = f
        .payload()
        .cli_inventory(
            &[
                shadow.parent().unwrap().to_owned(),
                f.home.join(".local/bin"),
                f.home.join(".cargo/bin"),
            ],
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(result.legacy, Some(legacy.clone()));
    assert_eq!(result.shadowing, vec![shadow.clone()]);
    f.install(None, None);
    assert_eq!(fs::read(legacy).unwrap(), b"manual unsigned old CLI");
    assert_eq!(fs::read(shadow).unwrap(), b"foreign CLI");
    let result = f
        .payload()
        .cli_inventory(&[PathBuf::from("/usr/local/bin")], &f.deadline())
        .unwrap();
    assert_eq!(
        result.shadowing,
        vec![PathBuf::from("/usr/local/bin/crosspanectl")]
    );
}
#[test]
fn delayed_consent_cannot_authorize_another_plan_even_with_same_view_and_operation() {
    let f = Fixture::new(false);
    let payload = f.payload();
    let first = payload.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = first.consent(1, 1, false).unwrap();
    let replacement = payload.plan(1, 1, None, &f.deadline()).unwrap();
    assert!(
        payload
            .install(replacement, consent, None, &f.deadline())
            .is_err()
    );
    assert!(!f.home.join("Applications").exists());
}
#[test]
fn changed_source_or_target_after_consent_blocks_transmission_and_replacement() {
    for source_change in [true, false] {
        let f = Fixture::new(false);
        let payload = f.payload();
        let plan = payload.plan(1, 1, None, &f.deadline()).unwrap();
        let consent = plan.consent(1, 1, false).unwrap();
        if source_change {
            bytes(&f.source.join("crosspanectl"), b"substitute", 0o755);
        } else {
            bytes(
                &f.home.join(".local/bin/crosspanectl"),
                b"foreign target",
                0o755,
            );
        }
        assert!(payload.install(plan, consent, None, &f.deadline()).is_err());
        assert!(!f.home.join("Applications").exists());
    }
}
#[test]
fn clean_stop_requires_actual_absence_matching_receipt_and_all_cleanup_booleans() {
    for case in 0..6 {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        if case == 0 {
            f.runner.stopped.store(false, Ordering::Release);
        }
        let receipt = f.io.target().state_dir().join("last_exit.json");
        if case == 1 {
            remove_owned(&receipt);
        }
        if case >= 2 {
            let mut value: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
            match case {
                2 => value["instance_id"] = json!(2),
                3 => {
                    value["parking"] = json!("failed");
                }
                4 => {
                    value["input_journals_empty"] = json!(false);
                }
                5 => {
                    value["audio_stopped"] = json!(false);
                }
                _ => unreachable!(),
            }
            bytes(&receipt, &serde_json::to_vec(&value).unwrap(), 0o600);
        }
        assert!(
            !matches!(CleanStopGate::observe(original, &f.deadline()), Ok(Some(_))),
            "case {case}"
        );
    }
}
#[test]
fn well_formed_unclean_receipts_reach_clean_stop_gate_and_remain_pending() {
    for fact in ["parking", "input_journals_empty", "audio_stopped"] {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(false, "restored");
        assert!(f.runner.stopped.load(Ordering::Acquire));
        let path = f.io.target().state_dir().join("last_exit.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value[fact] = if fact == "parking" {
            json!("failed")
        } else {
            json!(false)
        };
        let data = serde_json::to_vec(&value).unwrap();
        assert!(!parse_last_exit(&data).unwrap().clean, "{fact}");
        bytes(&path, &data, 0o600);
        assert!(
            CleanStopGate::observe(original, &f.deadline())
                .unwrap()
                .is_none(),
            "{fact}"
        );
    }
}
#[test]
fn intent_precedes_staging_and_interruption_preserves_prior_usable_payload() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let home = f.home.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let seen = observed.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "mkdir" && path.ends_with(".Crosspane.app.crosspane-stage") {
            let record: PayloadRecord = serde_json::from_slice(
                &fs::read(
                    home.join("Library/Application Support/Crosspane/Installer/payload.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(record.phase, PayloadPhase::Intent);
            seen.store(true, Ordering::Release);
        }
        if stage == "write" && path.ends_with("crosspane-ui") {
            return Err(NativeError::Unavailable);
        }
        Ok(value)
    }));
    let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
    let plan = payload.plan(1, 1, Some(original), &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    assert!(matches!(
        payload.install(plan, consent, Some(&gate), &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    assert!(observed.load(Ordering::Acquire));
    assert_eq!(
        fs::read(f.io.target().agent_path()).unwrap(),
        macho(false, 0)
    );
    let recovery = payload.recovery(&f.deadline()).unwrap();
    assert!(recovery.app_stage_present);
    assert_eq!(recovery.record.unwrap().phase, PayloadPhase::Unknown);
    assert!(matches!(
        payload.plan(2, 2, None, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    let record = f.io.target().installer_dir().join("payload.json");
    assert_eq!(owned_stat(&record).st_mode & 0o777, 0o600);
}
#[test]
fn timeout_after_backup_rename_retains_backup_and_never_blindly_retries() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let clock = f.clock.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "complete" && path.ends_with(".Crosspane.app.crosspane-previous") {
            clock.0.store(5001, Ordering::Release);
        }
        Ok(value)
    }));
    let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
    let plan = payload.plan(1, 1, Some(original), &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    assert!(matches!(
        payload.install(plan, consent, Some(&gate), &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    assert_eq!(
        fs::read(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane")
        )
        .unwrap(),
        macho(false, 0)
    );
    let recovered = payload.recovery(&f.deadline()).unwrap();
    assert!(recovered.app_previous_present);
    assert_eq!(recovered.record.unwrap().phase, PayloadPhase::Staged);
}
#[test]
fn replacing_existing_receipt_is_atomic_and_receipt_alone_cannot_adopt_or_delete() {
    let f = Fixture::new(false);
    let mut pending = f.install(None, None);
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    let payload = f.payload();
    payload
        .verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        )
        .unwrap();
    let path = f.io.target().installer_dir().join("payload.json");
    let record: PayloadRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record.phase, PayloadPhase::Verified);
    bytes(
        &f.home.join(".local/bin/crosspanectl"),
        b"manually substituted",
        0o755,
    );
    let plan = payload.plan(2, 2, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), PayloadState::Conflict);
    assert!(plan.consent(2, 2, true).is_err());
    assert_eq!(
        fs::read(f.home.join(".local/bin/crosspanectl")).unwrap(),
        b"manually substituted"
    );
}
#[test]
fn each_replacement_health_fact_and_receipt_binding_is_independently_required() {
    for case in 0..11 {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let mut pending = f.install(Some(original), Some(&gate));
        pending.expect_health(3).unwrap();
        let selected = f.start_new();
        let mut value = f.status(2);
        let mut reply = f.reply(&value, 3);
        let mut expected = None;
        match case {
            0 => value["result"]["installer"]["startup_recovery"] = json!("failed"),
            1 => value["result"]["installer"]["recovery_pending"] = json!(1),
            2 => value["result"]["installer"]["keystore"] = json!("file"),
            3 => value["result"]["installer"]["build"]["features"] = json!([]),
            4 => value["result"]["installer"]["build"]["version"] = json!("old"),
            5 => {
                value["result"]["installer"]["node"] =
                    json!("2222222222222222222222222222222222222222222222222222222222222222")
            }
            6 => expected = Some("2222222222222222"),
            7 => reply.id = 2,
            8 => reply.observed_at_ms = 101,
            9 => reply.source = ObservationSource::Live,
            10 => reply.result = Ok(DecodedReply::Acknowledged),
            _ => unreachable!(),
        }
        if case <= 6 {
            reply.result = Ok(DecodedReply::Status(StatusAdmission::Supported(
                f.health(&value),
            )));
        }
        assert!(
            f.payload()
                .verify(&mut pending, &selected, &reply, expected, &f.deadline())
                .is_err(),
            "case {case}"
        );
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
        assert_eq!(pending.phase(), PayloadPhase::Published);
    }
}
#[test]
fn keychain_wait_and_same_instance_do_not_complete_or_create_fallback_identity() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let mut pending = f.install(Some(original), Some(&gate));
    pending.expect_health(1).unwrap();
    f.runner.stopped.store(false, Ordering::Release);
    let selected = f.selected();
    assert!(
        f.payload()
            .verify(
                &mut pending,
                &selected,
                &f.reply(&f.status(1), 1),
                None,
                &f.deadline()
            )
            .is_err()
    );
    f.bootstrap(1, 4242, 0, "waiting_for_keystore");
    let waiting = f.selected();
    pending.expect_health(2).unwrap();
    assert!(
        f.payload()
            .verify(
                &mut pending,
                &waiting,
                &f.reply(&f.status(1), 2),
                None,
                &f.deadline()
            )
            .is_err()
    );
    assert_eq!(
        fs::read(f.source.join("crosspane-installer")).unwrap(),
        macho(false, 1)
    );
    assert!(
        f.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|p| p == Path::new("/bin/ps") || p == Path::new("/usr/bin/codesign"))
    );
}

#[test]
fn standard_fat32_and_fat64_arm64_slices_are_admitted_and_overlap_is_rejected() {
    for wide in [false, true] {
        for overlap in [false, true] {
            let f = Fixture::new(false);
            let mut approved = inventory();
            let thin = macho(false, 1);
            let stride = if wide { 32 } else { 20 };
            let offset = 8 + 2 * stride;
            let mut data = vec![0; offset];
            data[0..4]
                .copy_from_slice(&(if wide { 0xcafebabfu32 } else { 0xcafebabeu32 }).to_be_bytes());
            data[4..8].copy_from_slice(&2u32.to_be_bytes());
            for n in 0..2 {
                let at = 8 + n * stride;
                data[at..at + 4].copy_from_slice(
                    &(if n == 0 { 0x01000007u32 } else { 0x0100000cu32 }).to_be_bytes(),
                );
                let start = offset + if n == 1 && !overlap { thin.len() } else { 0 };
                if wide {
                    data[at + 8..at + 16].copy_from_slice(&(start as u64).to_be_bytes());
                    data[at + 16..at + 24].copy_from_slice(&(thin.len() as u64).to_be_bytes());
                } else {
                    data[at + 8..at + 12].copy_from_slice(&(start as u32).to_be_bytes());
                    data[at + 12..at + 16].copy_from_slice(&(thin.len() as u32).to_be_bytes());
                }
            }
            data.extend_from_slice(&thin);
            data.extend_from_slice(&thin);
            approved.files[0].size = data.len() as u64;
            approved.files[0].sha256 = sha(&data);
            bytes(&f.source.join(&approved.files[0].path), &data, 0o755);
            assert_eq!(
                MacPayload::admit(f.io.clone(), approved, &f.deadline()).is_ok(),
                !overlap
            );
        }
    }
}
#[test]
fn original_startup_recovery_failure_or_file_identity_requires_tier_two_and_cannot_stop_gate() {
    for (key, value) in [("startup_recovery", "failed"), ("keystore", "file")] {
        let f = Fixture::new(true);
        let mut status = f.status(1);
        status["result"]["installer"][key] = json!(value);
        assert!(OriginalAgent::capture(f.selected(), &f.health(&status), &f.deadline()).is_err());
        assert_eq!(status["result"]["installer"]["recovery_pending"], 0);
    }
}
#[test]
fn gate_for_a_different_original_capture_has_no_replacement_authority() {
    let f = Fixture::new(true);
    let first = f.original();
    let second = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(first, &f.deadline())
        .unwrap()
        .unwrap();
    let payload = f.payload();
    let plan = payload.plan(1, 1, Some(second), &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    assert!(
        payload
            .install(plan, consent, Some(&gate), &f.deadline())
            .is_err()
    );
    assert_eq!(
        fs::read(f.io.target().agent_path()).unwrap(),
        macho(false, 0)
    );
}
#[test]
fn preexisting_backup_is_never_replaced_even_when_receipts_are_missing() {
    let f = Fixture::new(false);
    let backup = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous/kept");
    bytes(&backup, b"only usable previous", 0o644);
    let payload = f.payload();
    assert!(matches!(
        payload.plan(1, 1, None, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    assert_eq!(fs::read(&backup).unwrap(), b"only usable previous");
    let recovery = payload.recovery(&f.deadline()).unwrap();
    assert!(recovery.app_previous_present);
    assert!(recovery.record.is_none());
}
#[test]
fn substituted_backup_after_publication_is_retained_despite_successful_new_health() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let mut pending = f.install(Some(original), Some(&gate));
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    let backup = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane");
    bytes(&backup, b"manual replacement", 0o755);
    assert!(
        f.payload()
            .verify(
                &mut pending,
                &selected,
                &f.reply(&f.status(2), 1),
                None,
                &f.deadline()
            )
            .is_err()
    );
    assert_eq!(fs::read(&backup).unwrap(), b"manual replacement");
    assert_eq!(pending.phase(), PayloadPhase::Published);
}
const INSTALL_BOUNDARIES: [&str; 12] = [
    "mkdir",
    "lock-open",
    "lock",
    "create-temp",
    "write",
    "file-sync",
    "publish",
    "quarantine",
    "unlink",
    "parent-sync",
    "chmod",
    "complete",
];
const VERIFY_BOUNDARIES: [&str; 10] = [
    "lock-open",
    "lock",
    "create-temp",
    "write",
    "file-sync",
    "publish",
    "quarantine",
    "unlink",
    "parent-sync",
    "complete",
];
fn mutation_boundary(stage: &str) -> bool {
    INSTALL_BOUNDARIES.contains(&stage)
}
#[test]
fn every_native_mutation_boundary_failure_is_unknown_and_retains_a_usable_copy() {
    let baseline = Fixture::new(true);
    let original = baseline.original();
    baseline.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &baseline.deadline())
        .unwrap()
        .unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let recorded = trace.clone();
    let root = baseline.root.clone();
    let io = baseline.hooked(Arc::new(move |at, path, value| {
        if mutation_boundary(at) {
            recorded
                .lock()
                .unwrap()
                .push((at.to_owned(), path.strip_prefix(&root).unwrap().to_owned()));
        }
        Ok(value)
    }));
    let payload = MacPayload::admit(io, inventory(), &baseline.deadline()).unwrap();
    let plan = payload
        .plan(1, 1, Some(original), &baseline.deadline())
        .unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    payload
        .install(plan, consent, Some(&gate), &baseline.deadline())
        .unwrap()
        .unwrap();
    let events = trace.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .map(|(stage, _)| stage.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(INSTALL_BOUNDARIES)
    );
    println!("install mutation boundaries: {}", events.len());
    let intent_complete = events
        .iter()
        .position(|(stage, path)| stage == "complete" && path.ends_with("payload.json"))
        .unwrap();
    assert!(events.iter().any(|(at, path)| at == "publish" && path.ends_with(".crosspanectl.crosspane-previous")));
    assert!(
        events
            .iter()
            .any(|(at, path)| at == "publish" && path.ends_with(".local/bin/crosspanectl"))
    );
    for (index, expected) in events.into_iter().enumerate() {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let root = f.root.clone();
        let seen = Arc::new(AtomicU64::new(0));
        let count = seen.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let error_write_failed = Arc::new(AtomicBool::new(false));
        let error_flag = error_write_failed.clone();
        let io = f.hooked(Arc::new(move |at, path, value| {
            if flag.load(Ordering::Acquire) && at == "write" && path.ends_with("payload.json") {
                error_flag.store(true, Ordering::Release);
                return Err(NativeError::Unavailable);
            }
            if mutation_boundary(at) && count.fetch_add(1, Ordering::AcqRel) == index as u64 {
                assert_eq!(
                    (at.to_owned(), path.strip_prefix(&root).unwrap().to_owned()),
                    expected
                );
                flag.store(true, Ordering::Release);
                return Err(NativeError::Unavailable);
            }
            Ok(value)
        }));
        let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
        let plan = payload.plan(1, 1, Some(original), &f.deadline()).unwrap();
        let consent = plan.consent(1, 1, true).unwrap();
        assert!(
            payload
                .install(plan, consent, Some(&gate), &f.deadline())
                .is_err(),
            "event {index}"
        );
        assert!(
            fired.load(Ordering::Acquire),
            "injection {index} never reached"
        );
        // Reconstruct the read-only detector even when the best-effort error receipt failed.
        let recovery = f.payload().recovery(&f.deadline()).unwrap();
        assert_old_retained(&f, &recovery);
        if index > intent_complete {
            assert!(
                error_write_failed.load(Ordering::Acquire),
                "error receipt {index} unexpectedly succeeded"
            );
        }
    }
}
#[test]
fn config_identity_trust_layout_and_recovery_bytes_survive_installation_and_verification() {
    let f = Fixture::new(false);
    let state = f.io.target().state_dir();
    let names = [
        "config.toml",
        "identity",
        "trust.json",
        "layout.json",
        "recovery-journal",
    ];
    for name in names {
        bytes(&state.join(name), name.as_bytes(), 0o600);
    }
    let mut pending = f.install(None, None);
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    f.payload()
        .verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        )
        .unwrap();
    for name in names {
        assert_eq!(fs::read(state.join(name)).unwrap(), name.as_bytes());
    }
}
#[test]
fn stale_future_wrong_call_and_incomplete_health_do_not_refresh_proof_or_release_backup() {
    for case in 0..5 {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        f.clock.0.store(20, Ordering::Release);
        let mut pending = f.install(Some(original), Some(&gate));
        pending.expect_health(10).unwrap();
        let mut selected = f.start_new();
        let mut reply = f.reply(&f.status(2), 10);
        match case {
            0 => reply.observed_at_ms = 19,
            1 => {
                f.clock.0.store(5101, Ordering::Release);
                selected = f.selected();
            }
            2 => reply.id = 9,
            3 => reply.result = Err(CallFailure::Unavailable),
            4 => {
                let mut value = f.status(2);
                value["result"].as_object_mut().unwrap().remove("installer");
                reply.result = Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&value).unwrap(), AgentPlatform::Macos)
                        .unwrap(),
                ));
            }
            _ => unreachable!(),
        }
        assert!(
            f.payload()
                .verify(&mut pending, &selected, &reply, None, &f.deadline())
                .is_err()
        );
        assert_eq!(pending.phase(), PayloadPhase::Published);
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
    }
}
#[test]
fn wrong_native_instance_pid_uid_executable_or_runtime_is_not_replacement_health() {
    for key in ["id", "pid", "uid", "exe", "runtime_dir", "started_unix_ms"] {
        let f = Fixture::new(false);
        let mut pending = f.install(None, None);
        pending.expect_health(1).unwrap();
        let selected = f.start_new();
        let mut value = f.status(2);
        value["result"]["installer"]["instance"][key] = match key {
            "exe" | "runtime_dir" => json!("/foreign/owned-looking"),
            _ => json!(99999),
        };
        assert!(
            f.payload()
                .verify(
                    &mut pending,
                    &selected,
                    &f.reply(&value, 1),
                    None,
                    &f.deadline()
                )
                .is_err()
        );
    }
}
#[test]
fn helpers_cannot_inherit_agent_audio_entitlement_even_from_an_approved_table() {
    let f = Fixture::new(false);
    let mut approved = inventory();
    approved.files[1]
        .signing
        .as_mut()
        .unwrap()
        .entitlements
        .insert("com.apple.security.device.audio-input".into(), true);
    assert!(MacPayload::admit(f.io.clone(), approved, &f.deadline()).is_err());
}
#[test]
fn health_call_ids_and_redacted_authority_debug_remain_bounded() {
    let f = Fixture::new(false);
    let payload = f.payload();
    let plan = payload.plan(7, 9, None, &f.deadline()).unwrap();
    assert_eq!(plan.view_revision(), 7);
    assert_eq!(plan.operation_id(), 9);
    assert!(!format!("{plan:?}").contains("cp-c1"));
    let consent = plan.consent(7, 9, false).unwrap();
    let mut pending = payload
        .install(plan, consent, None, &f.deadline())
        .unwrap()
        .unwrap();
    assert!(pending.expect_health(0).is_err());
    pending.expect_health(10).unwrap();
    assert!(pending.expect_health(10).is_err());
    assert!(pending.expect_health(9).is_err());
    pending.expect_health(11).unwrap();
    assert_eq!(format!("{pending:?}"), "PendingPayload");
    let recovery = payload.recovery(&f.deadline()).unwrap();
    assert!(recovery.app_present && recovery.ctl_present);
    assert_eq!(
        payload.manifest_sha256(),
        recovery.record.unwrap().receipt.manifest_sha256
    );
}

#[test]
fn fresh_and_partial_existing_bootstrap_same_instance_cannot_verify_but_changed_instance_can() {
    for partial in [false, true] {
        let f = Fixture::new(false);
        if partial {
            bytes(
                &f.home.join(".local/bin/crosspanectl"),
                &macho(false, 0),
                0o755,
            );
        }
        let payload = f.payload();
        let mut pending = f.install(None, None);
        pending.expect_health(1).unwrap();
        f.clock.0.store(100, Ordering::Release);
        let prior = f.selected();
        assert!(matches!(
            payload.verify(
                &mut pending,
                &prior,
                &f.reply(&f.status(1), 1),
                None,
                &f.deadline()
            ),
            Err(NativeError::Refused)
        ));
        assert_eq!(pending.phase(), PayloadPhase::Published);
        pending.expect_health(2).unwrap();
        let new = f.start_new();
        payload
            .verify(
                &mut pending,
                &new,
                &f.reply(&f.status(2), 2),
                None,
                &f.deadline(),
            )
            .unwrap();
        assert_eq!(pending.phase(), PayloadPhase::Verified);
    }
}

#[test]
fn bootstrap_token_change_after_consent_refuses_before_any_mutation() {
    let f = Fixture::new(false);
    let payload = f.payload();
    let plan = payload.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    f.bootstrap(2, 4243, 1000, "ready");
    assert!(matches!(
        payload.install(plan, consent, None, &f.deadline()),
        Err(NativeError::Refused)
    ));
    assert!(!f.io.target().installer_dir().exists());
    assert!(!f.home.join("Applications").exists());
    assert!(!f.home.join(".local").exists());
}

fn complete_bundle(path: &Path, marker: u8) -> bool {
    inventory().files.iter().filter(|file| file.path.starts_with("Crosspane.app/")).all(|file| {
        let expected = file.signing.as_ref().map(|rule| macho(rule.role == PayloadRole::EmbeddedCode, marker))
            .unwrap_or_else(|| b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec());
        fs::read(path.join(file.path.strip_prefix("Crosspane.app/").unwrap())).is_ok_and(|bytes| bytes == expected)
    })
}
fn assert_old_retained(f: &Fixture, recovery: &RecoveryInventory) {
    assert!(
        [
            f.home.join("Applications/Crosspane.app"),
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
        ]
        .iter()
        .chain(&recovery.retained_temporaries)
        .any(|path| complete_bundle(path, 0)),
        "complete prior bundle lost"
    );
    assert!(
        [
            f.home.join(".local/bin/crosspanectl"),
            f.home.join(".local/bin/.crosspanectl.crosspane-previous")
        ]
        .iter()
        .chain(&recovery.retained_temporaries)
        .any(|path| fs::read(path).is_ok_and(|data| data == macho(false, 0))),
        "prior ctl lost"
    );
}

#[test]
fn existing_ctl_directory_is_a_conflict_without_panicking_or_mutating() {
    let f = Fixture::new(false);
    let ctl = f.home.join(".local/bin/crosspanectl");
    directory(&ctl);
    chmod_owned(&ctl, 0o755);
    let before = f.io.metadata(&ctl).unwrap();
    let plan = f.payload().plan(1, 1, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), PayloadState::Conflict);
    assert!(plan.consent(1, 1, true).is_err());
    assert_eq!(f.io.metadata(&ctl).unwrap(), before);
    assert!(!f.io.target().installer_dir().exists());
}

#[test]
fn exact_manual_contents_require_adoption_and_receipt_fields_cannot_be_guessed() {
    let f = Fixture::new(false);
    let mut pending = f.install(None, None);
    pending.expect_health(1).unwrap();
    let selected = f.start_new();
    let payload = f.payload();
    payload
        .verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        )
        .unwrap();
    let path = f.io.target().installer_dir().join("payload.json");
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for field in [
        "absent",
        "manifest",
        "product",
        "operation",
        "path",
        "ownership",
        "outcome",
        "unfinished",
    ] {
        let mut value = record.clone();
        match field {
            "absent" => remove_owned(&path),
            "manifest" => value["receipt"]["manifest_sha256"][0] = json!(99),
            "product" => value["receipt"]["product_version"] = json!("unapproved"),
            "operation" => value["receipt"]["operation_id"] = json!(0),
            "path" => value["receipt"]["resources"][0]["resolved_path"] = json!("/unknown"),
            "ownership" => value["receipt"]["resources"][0]["ownership"] = json!("Foreign"),
            "outcome" => value["receipt"]["resources"][0]["outcome"] = json!("Unknown"),
            "unfinished" => value["receipt"]["unfinished"] = json!([12]),
            _ => unreachable!(),
        }
        if field != "absent" {
            bytes(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        }
        let plan = payload.plan(2, 2, None, &f.deadline()).unwrap();
        assert_eq!(plan.state(), PayloadState::AdoptionRequired, "{field}");
        assert!(plan.consent(2, 2, false).is_err(), "{field}");
        assert!(complete_bundle(
            &f.home.join("Applications/Crosspane.app"),
            1
        ));
    }
}

#[test]
fn validated_stage_identity_is_required_at_each_publication() {
    for app in [true, false] {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let home = f.home.clone();
        let root = f.root.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let io = f.hooked(Arc::new(move |stage, path, value| {
            let boundary = if app { ".Crosspane.app.crosspane-previous" } else { "Applications/Crosspane.app" };
            if stage == "complete" && path.ends_with(boundary) && !flag.swap(true, Ordering::AcqRel) {
                if app {
                    let stage = home.join("Applications/.Crosspane.app.crosspane-stage");
                    rename_owned(&stage, &root.join("validated-app-stage-held"));
                    for file in inventory().files.iter().filter(|file| file.path.starts_with("Crosspane.app/")) {
                        let data = file.signing.as_ref().map(|rule| macho(rule.role == PayloadRole::EmbeddedCode, 1))
                            .unwrap_or_else(|| b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec());
                        bytes(&stage.join(file.path.strip_prefix("Crosspane.app/").unwrap()), &data, file.mode);
                    }
                } else {
                    let stage = home.join(".local/bin/.crosspanectl.crosspane-stage");
                    rename_owned(&stage, &root.join("validated-ctl-stage-held"));
                    bytes(&stage, &macho(false, 1), 0o755);
                }
            }
            Ok(value)
        }));
        let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
        let plan = payload.plan(1, 1, Some(original), &f.deadline()).unwrap();
        let consent = plan.consent(1, 1, true).unwrap();
        assert!(matches!(
            payload.install(plan, consent, Some(&gate), &f.deadline()),
            Err(NativeError::OutcomeUnknown)
        ));
        assert!(fired.load(Ordering::Acquire));
        assert_old_retained(&f, &payload.recovery(&f.deadline()).unwrap());
        assert!(
            !if app {
                f.home.join("Applications/Crosspane.app")
            } else {
                f.home.join(".local/bin/crosspanectl")
            }
            .exists()
        );
    }
}

#[test]
fn descendant_backup_drift_after_rename_is_unknown_before_any_stage_publication() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let home = f.home.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "complete"
            && path.ends_with(".Crosspane.app.crosspane-previous")
            && !flag.swap(true, Ordering::AcqRel)
        {
            directory(&home.join(
                "Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/unconsented-empty",
            ));
        }
        Ok(value)
    }));
    let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
    let plan = payload.plan(1, 1, Some(original), &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, true).unwrap();
    assert!(matches!(
        payload.install(plan, consent, Some(&gate), &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    assert!(fired.load(Ordering::Acquire));
    assert!(!f.home.join("Applications/Crosspane.app").exists());
    assert_old_retained(&f, &payload.recovery(&f.deadline()).unwrap());
    assert!(
        f.home
            .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/unconsented-empty")
            .exists()
    );
}

#[test]
fn replacement_removal_restart_or_receipt_expiry_at_lock_retains_both_backups() {
    for change in ["removed", "restart", "expired"] {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let mut pending = f.install(Some(original), Some(&gate));
        pending.expect_health(1).unwrap();
        f.start_new();
        let mut reply = f.reply(&f.status(2), 1);
        if change == "expired" {
            f.clock.0.store(5100, Ordering::Release);
            reply.observed_at_ms = 100;
        }
        let home = f.home.clone();
        let root = f.root.clone();
        let runtime = f.runtime.clone();
        let runner = f.runner.clone();
        let clock = f.clock.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let io = f.hooked(Arc::new(move |stage, _, value| {
            if stage == "lock" && !flag.swap(true, Ordering::AcqRel) {
                match change {
                    "removed" => rename_owned(&home.join("Applications/Crosspane.app"), &root.join("replacement-held")),
                    "restart" => {
                        runner.pid.store(4244, Ordering::Release);
                        bytes(&runtime.join("bootstrap.json"), &serde_json::to_vec(&json!({"schema_version":1,"instance_id":3,"pid":4244,"started_unix_ms":1000,
                            "phase":"failed","phase_seq":1,"keystore":"os_store","reason":"other","runtime_dir":runtime})).unwrap(), 0o600);
                    }
                    "expired" => clock.0.store(5101, Ordering::Release),
                    _ => unreachable!(),
                }
            }
            Ok(value)
        }));
        let selected = f.selected_on(io);
        assert!(
            f.payload()
                .verify(&mut pending, &selected, &reply, None, &f.deadline())
                .is_err(),
            "{change}"
        );
        assert!(fired.load(Ordering::Acquire), "{change}");
        assert_eq!(pending.phase(), PayloadPhase::Published);
        assert_old_retained(&f, &f.payload().recovery(&f.deadline()).unwrap());
    }
}

#[test]
fn replacement_loss_after_first_destructive_step_stops_remaining_cleanup() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let mut pending = f.install(Some(original), Some(&gate));
    pending.expect_health(1).unwrap();
    f.start_new();
    let home = f.home.clone();
    let root = f.root.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "complete"
            && path.starts_with(home.join("Applications/.Crosspane.app.crosspane-previous"))
            && !flag.swap(true, Ordering::AcqRel)
        {
            rename_owned(
                &home.join("Applications/Crosspane.app"),
                &root.join("replacement-held"),
            );
        }
        Ok(value)
    }));
    let selected = f.selected_on(io);
    assert!(matches!(
        f.payload().verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline()
        ),
        Err(NativeError::OutcomeUnknown)
    ));
    assert!(fired.load(Ordering::Acquire));
    assert_eq!(pending.phase(), PayloadPhase::Unknown);
    assert_eq!(
        fs::read(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane")
        )
        .unwrap(),
        macho(false, 0)
    );
    assert_eq!(
        fs::read(f.home.join(".local/bin/.crosspanectl.crosspane-previous")).unwrap(),
        macho(false, 0)
    );
    assert!(complete_bundle(&f.root.join("replacement-held"), 1));
}

#[test]
fn edited_ctl_backup_between_full_comparison_and_delete_is_retained() {
    let f = Fixture::new(true);
    let original = f.original();
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let mut pending = f.install(Some(original), Some(&gate));
    pending.expect_health(1).unwrap();
    f.start_new();
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let plain = f.io.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "metadata"
            && path.ends_with(".crosspanectl.crosspane-previous")
            && count.fetch_add(1, Ordering::AcqRel) + 1 == 5
        {
            bytes(path, b"manual-in-place-edit", 0o755);
            return plain.metadata(path);
        }
        Ok(value)
    }));
    let selected = f.selected_on(io);
    assert!(matches!(
        f.payload().verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline()
        ),
        Err(NativeError::OutcomeUnknown)
    ));
    assert!(calls.load(Ordering::Acquire) >= 5);
    assert_eq!(pending.phase(), PayloadPhase::Unknown);
    assert_eq!(
        fs::read(f.home.join(".local/bin/.crosspanectl.crosspane-previous")).unwrap(),
        b"manual-in-place-edit"
    );
}

#[test]
fn new_waiting_starting_and_failed_instances_independently_cannot_release_backups() {
    for phase in ["waiting_for_keystore", "starting", "failed"] {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let mut pending = f.install(Some(original), Some(&gate));
        pending.expect_health(1).unwrap();
        f.start_new();
        f.bootstrap(2, 4243, 1000, phase);
        let selected = f.selected();
        assert!(
            matches!(
                f.payload().verify(
                    &mut pending,
                    &selected,
                    &f.reply(&f.status(2), 1),
                    None,
                    &f.deadline()
                ),
                Err(NativeError::Refused)
            ),
            "{phase}"
        );
        assert_eq!(pending.phase(), PayloadPhase::Published);
        assert_old_retained(&f, &f.payload().recovery(&f.deadline()).unwrap());
    }
}

#[test]
fn every_verification_mutation_boundary_is_observed_and_failure_retains_complete_replacement() {
    let baseline = Fixture::new(true);
    let original = baseline.original();
    baseline.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &baseline.deadline())
        .unwrap()
        .unwrap();
    let mut pending = baseline.install(Some(original), Some(&gate));
    pending.expect_health(1).unwrap();
    baseline.start_new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let trace = events.clone();
    let root = baseline.root.clone();
    let io = baseline.hooked(Arc::new(move |at, path, value| {
        if mutation_boundary(at) {
            trace
                .lock()
                .unwrap()
                .push((at.to_owned(), path.strip_prefix(&root).unwrap().to_owned()));
        }
        Ok(value)
    }));
    let selected = baseline.selected_on(io);
    baseline
        .payload()
        .verify(
            &mut pending,
            &selected,
            &baseline.reply(&baseline.status(2), 1),
            None,
            &baseline.deadline(),
        )
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .map(|(stage, _)| stage.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(VERIFY_BOUNDARIES)
    );
    println!("verification mutation boundaries: {}", events.len());
    assert!(events.iter().any(|(stage, _)| stage == "unlink"));
    assert!(
        events
            .iter()
            .any(|(stage, path)| stage == "publish" && path.ends_with("payload.json"))
    );
    for (index, expected) in events.into_iter().enumerate() {
        let f = Fixture::new(true);
        let original = f.original();
        f.exit(true, "restored");
        let gate = CleanStopGate::observe(original.clone(), &f.deadline())
            .unwrap()
            .unwrap();
        let mut pending = f.install(Some(original), Some(&gate));
        pending.expect_health(1).unwrap();
        f.start_new();
        let root = f.root.clone();
        let counter = Arc::new(AtomicU64::new(0));
        let count = counter.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let io = f.hooked(Arc::new(move |stage, path, value| {
            if mutation_boundary(stage) && count.fetch_add(1, Ordering::AcqRel) == index as u64 {
                assert_eq!(
                    (
                        stage.to_owned(),
                        path.strip_prefix(&root).unwrap().to_owned()
                    ),
                    expected
                );
                flag.store(true, Ordering::Release);
                return Err(NativeError::Unavailable);
            }
            Ok(value)
        }));
        let selected = f.selected_on(io);
        assert!(
            f.payload()
                .verify(
                    &mut pending,
                    &selected,
                    &f.reply(&f.status(2), 1),
                    None,
                    &f.deadline()
                )
                .is_err(),
            "verify event {index}"
        );
        assert!(
            fired.load(Ordering::Acquire),
            "verify injection {index} never fired"
        );
        assert_ne!(pending.phase(), PayloadPhase::Verified);
        assert!(complete_bundle(
            &f.home.join("Applications/Crosspane.app"),
            1
        ));
        assert_eq!(
            fs::read(f.home.join(".local/bin/crosspanectl")).unwrap(),
            macho(false, 1)
        );
        f.payload().recovery(&f.deadline()).unwrap();
    }
}

struct AtomicReceiptOps {
    path: PathBuf,
    original: Vec<u8>,
    replacement: Mutex<Option<Vec<u8>>>,
    events: Mutex<Vec<&'static str>>,
    fail: Option<(&'static str, bool)>,
    fired: AtomicBool,
}
impl AtomicReceiptOps {
    fn observe(&self) {
        let bytes = fs::read(&self.path).expect("canonical receipt must never be absent");
        let record: PayloadRecord =
            serde_json::from_slice(&bytes).expect("canonical receipt must be complete JSON");
        assert!(
            bytes == self.original || self.replacement.lock().unwrap().as_ref() == Some(&bytes)
        );
        assert!(matches!(
            record.phase,
            PayloadPhase::Published | PayloadPhase::Verified
        ));
    }
}
impl FilesystemOps for AtomicReceiptOps {
    fn execute(&self, operation: FilesystemOperation<'_>) -> NativeResult<()> {
        let kind = match &operation {
            FilesystemOperation::Write(_, bytes) => {
                *self.replacement.lock().unwrap() = Some(bytes.to_vec());
                "write"
            }
            FilesystemOperation::FileSync(_) => "file-sync",
            FilesystemOperation::Rename { .. } => "rename",
            FilesystemOperation::DirectorySync(_) => "directory-sync",
        };
        self.events.lock().unwrap().push(kind);
        self.observe();
        let fail =
            self.fail.is_some_and(|(at, _)| at == kind) && !self.fired.swap(true, Ordering::AcqRel);
        if fail && !self.fail.unwrap().1 {
            return Err(NativeError::Unavailable);
        }
        SystemFilesystem.execute(operation)?;
        self.observe();
        if fail {
            return Err(NativeError::Unavailable);
        }
        Ok(())
    }
}

#[test]
fn existing_receipt_is_complete_old_or_new_at_actual_write_sync_rename_and_directory_sync() {
    for failure in std::iter::once(None).chain(
        ["write", "file-sync", "rename", "directory-sync"]
            .into_iter()
            .flat_map(|kind| [Some((kind, false)), Some((kind, true))]),
    ) {
        let f = Fixture::new(false);
        let mut pending = f.install(None, None);
        pending.expect_health(1).unwrap();
        f.start_new();
        let path = f.io.target().installer_dir().join("payload.json");
        let seam = Arc::new(AtomicReceiptOps {
            original: fs::read(&path).unwrap(),
            path,
            replacement: Mutex::default(),
            events: Mutex::default(),
            fail: failure,
            fired: AtomicBool::new(false),
        });
        let selected = f.selected_on(f.operated(seam.clone()));
        let result = f.payload().verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        );
        seam.observe();
        if failure.is_some() {
            assert!(seam.fired.load(Ordering::Acquire));
            assert!(matches!(result, Err(NativeError::OutcomeUnknown)));
            assert_eq!(pending.phase(), PayloadPhase::Unknown);
        } else {
            result.unwrap();
            assert_eq!(
                *seam.events.lock().unwrap(),
                [
                    "write",
                    "file-sync",
                    "rename",
                    "directory-sync",
                    "directory-sync"
                ]
            );
        }
    }
}

#[test]
fn dead_private_runtime_is_cleaned_under_payload_consent_without_claiming_verified_publication() {
    let mut f = Fixture::new(false);
    drop(f._listener.take());
    f.bootstrap(1, i32::MAX as u32, 0, "ready");
    assert!(f.io.dead_runtime(&f.deadline()).unwrap().is_some());
    let payload = f.payload();
    let plan = payload.plan(1, 1, None, &f.deadline()).unwrap();
    assert!(f.runtime.join("agent.sock").exists());
    assert!(f.runtime.join("bootstrap.json").exists());
    let consent = plan.consent(1, 1, false).unwrap();
    let pending = payload
        .install(plan, consent, None, &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(pending.phase(), PayloadPhase::Published);
    assert!(f.runtime.exists());
    assert!(!f.runtime.join("agent.sock").exists());
    assert!(!f.runtime.join("bootstrap.json").exists());
    assert!(
        !payload
            .recovery(&f.deadline())
            .unwrap()
            .record
            .unwrap()
            .receipt
            .unfinished
            .is_empty()
    );
}
#[test]
fn dead_runtime_identity_change_after_consent_refuses_and_retains_the_changed_socket() {
    let mut f = Fixture::new(false);
    drop(f._listener.take());
    f.bootstrap(1, i32::MAX as u32, 0, "ready");
    let payload = f.payload();
    let plan = payload.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, false).unwrap();
    remove_owned(&f.runtime.join("agent.sock"));
    let replacement = UnixListener::bind(f.runtime.join("agent.sock")).unwrap();
    chmod_owned(&f.runtime.join("agent.sock"), 0o600);
    drop(replacement);
    let socket = owned_stat(&f.runtime.join("agent.sock"));
    assert!(payload.install(plan, consent, None, &f.deadline()).is_err());
    assert!(same_inode(
        &socket,
        &owned_stat(&f.runtime.join("agent.sock"))
    ));
    assert!(f.runtime.join("bootstrap.json").exists());
    assert!(!f.io.target().app_path().exists());
}
#[test]
fn runtime_recovery_refuses_live_reused_pid_connectable_socket_foreign_neighbor_and_unsafe_modes() {
    for variant in 0..5 {
        let mut f = Fixture::new(false);
        f.bootstrap(
            1,
            if variant == 0 {
                std::process::id()
            } else {
                i32::MAX as u32
            },
            0,
            "ready",
        );
        if variant != 1 {
            drop(f._listener.take());
        }
        match variant {
            2 => bytes(&f.runtime.join("unknown"), b"foreign", 0o600),
            3 => chmod_owned(&f.runtime.join("bootstrap.json"), 0o644),
            4 => f.bootstrap(1, i32::MAX as u32, 0, "ready"),
            _ => {}
        }
        if variant == 4 {
            let mut value: Value =
                serde_json::from_slice(&fs::read(f.runtime.join("bootstrap.json")).unwrap())
                    .unwrap();
            value["runtime_dir"] = json!(f.home);
            bytes(
                &f.runtime.join("bootstrap.json"),
                &serde_json::to_vec(&value).unwrap(),
                0o600,
            );
        }
        assert!(
            f.io.dead_runtime(&f.deadline()).is_err(),
            "variant {variant}"
        );
        assert!(f.runtime.join("agent.sock").exists());
        assert!(f.runtime.join("bootstrap.json").exists());
    }
}
#[test]
fn long_cli_search_path_is_an_advisory_inventory_and_stays_finite() {
    let f = Fixture::new(false);
    let dirs = vec![PathBuf::from("/usr/local/bin"); 100];
    assert_eq!(
        f.payload()
            .cli_inventory(&dirs, &f.deadline())
            .unwrap()
            .shadowing
            .len(),
        100
    );
    assert!(
        f.payload()
            .cli_inventory(&vec![PathBuf::from("/usr/local/bin"); 4097], &f.deadline())
            .is_err()
    );
}

#[test]
fn positively_observed_live_pid_vetoes_recovery_before_any_socket_connection() {
    let f = Fixture::new(false);
    let pid = i32::MAX as u32;
    f.runner.pid.store(u64::from(pid), Ordering::Release);
    f.bootstrap(1, pid, 0, "ready");
    assert_eq!(f.io.dead_runtime(&f.deadline()), Err(NativeError::Foreign));
    let listener = f._listener.as_ref().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(f.runtime.join("bootstrap.json").exists());
}

#[test]
fn unknown_process_observation_never_becomes_death_or_opens_the_socket() {
    struct UnknownProcess(Arc<Runner>);
    impl CommandRunner for UnknownProcess {
        fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
            if spec.program() == Path::new("/bin/ps") {
                return Ok(CommandOutput {
                    code: Some(1),
                    stdout: vec![],
                    stderr: b"unavailable".to_vec(),
                });
            }
            self.0.run(spec, deadline)
        }
    }
    let f = Fixture::new(false);
    f.bootstrap(1, i32::MAX as u32, 0, "ready");
    let io = MacNativeIo::new(
        f.io.target().clone(),
        Arc::new(UnknownProcess(f.runner.clone())),
        f.support.clone(),
        f.signatures.clone(),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        io.dead_runtime(&f.deadline()),
        Err(NativeError::Unavailable)
    );
    let listener = f._listener.as_ref().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(f.runtime.join("bootstrap.json").exists());
}

fn companion(f: &Fixture) -> PathBuf {
    let record = f.payload().recovery(&f.deadline()).unwrap().record.unwrap();
    let digest: String = record
        .receipt
        .manifest_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    f.io.target()
        .installer_dir()
        .join(format!("payload-inventory-{digest}.json"))
}
fn newer_payload(f: &Fixture) -> MacPayload {
    let mut next = inventory();
    next.product_version = "test-2".into();
    for file in &mut next.files {
        if let Some(rule) = &file.signing {
            let data = macho(rule.role == PayloadRole::EmbeddedCode, 2);
            file.sha256 = sha(&data);
            file.size = data.len() as u64;
            bytes(&f.source.join(&file.path), &data, file.mode);
        }
    }
    MacPayload::admit(f.io.clone(), next, &f.deadline()).unwrap()
}
#[test]
fn legacy_published_unknown_record_resumes_exact_signed_bytes_without_fake_health() {
    let f = Fixture::new(false);
    f.install(None, None);
    let receipt_path = f.io.target().installer_dir().join("payload.json");
    let record: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(record["phase"], "Published");
    assert_eq!(record["receipt"]["unfinished"], json!([12]));
    for row in record["receipt"]["resources"].as_array().unwrap() {
        assert_eq!(row["ownership"], "Created");
        assert_eq!(row["before"], "Absent");
        assert_eq!(row["after"], "Unknown");
        assert_eq!(row["outcome"], "Unknown");
    }
    remove_owned(&companion(&f)); // The real earlier record predates the private companion.
    let payload = f.payload();
    let old = owned_stat(&f.io.target().agent_path());
    let plan = payload.plan(2, 2, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), PayloadState::Matching);
    assert!(plan.resuming_publication());
    let consent = plan.consent(2, 2, false).unwrap();
    let mut pending = payload
        .install(plan, consent, None, &f.deadline())
        .unwrap()
        .unwrap();
    assert!(same_inode(&old, &owned_stat(&f.io.target().agent_path())));
    assert_eq!(pending.phase(), PayloadPhase::Published);
    let current = payload.recovery(&f.deadline()).unwrap().record.unwrap();
    assert_eq!(current.receipt.operation_id.0, 2);
    assert!(
        current
            .receipt
            .resources
            .iter()
            .all(|r| r.outcome == crosspane_installer_core::MutationOutcome::Unknown)
    );
    let selected = f.start_new();
    pending.expect_health(2).unwrap();
    let verified = payload
        .verify(
            &mut pending,
            &selected,
            &f.reply(&f.status(2), 2),
            None,
            &f.deadline(),
        )
        .unwrap();
    assert!(verified.receipt.resources.iter().all(|r| r.ownership
        == crosspane_installer_core::ResourceOwnership::Created
        && r.before == crosspane_installer_core::ResourceObservation::Absent));
}
#[test]
fn prior_inventory_allows_consented_newer_payload_but_replacement_still_needs_clean_stop() {
    let f = Fixture::new(false);
    f.install(None, None);
    let original = f.original();
    let payload = newer_payload(&f);
    let plan = payload
        .plan(2, 2, Some(original.clone()), &f.deadline())
        .unwrap();
    assert!(plan.owned_files());
    assert_eq!(plan.state(), PayloadState::AdoptionRequired);
    let consent = plan.consent(2, 2, false).unwrap();
    assert_eq!(
        payload
            .install(plan, consent, None, &f.deadline())
            .unwrap_err(),
        NativeError::Refused
    );
    assert_eq!(
        fs::read(f.io.target().agent_path()).unwrap(),
        macho(false, 1)
    );
    f.exit(true, "restored");
    let gate = CleanStopGate::observe(original.clone(), &f.deadline())
        .unwrap()
        .unwrap();
    let plan = payload.plan(3, 3, Some(original), &f.deadline()).unwrap();
    let consent = plan.consent(3, 3, false).unwrap();
    let mut pending = payload
        .install(plan, consent, Some(&gate), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(f.io.target().agent_path()).unwrap(),
        macho(false, 2)
    );
    pending.expect_health(3).unwrap();
    let selected = f.start_new();
    let mut status = f.status(2);
    status["result"]["installer"]["build"]["version"] = json!("test-2");
    let verified = payload
        .verify(
            &mut pending,
            &selected,
            &f.reply(&status, 3),
            None,
            &f.deadline(),
        )
        .unwrap();
    assert!(
        verified
            .receipt
            .resources
            .iter()
            .all(|r| r.ownership == crosspane_installer_core::ResourceOwnership::Created)
    );
}
#[test]
fn unknown_legacy_different_inventory_and_changed_owned_bytes_stay_blocking() {
    for kind in 0..4 {
        let f = Fixture::new(false);
        f.install(None, None);
        if kind == 0 {
            remove_owned(&companion(&f));
        }
        if kind == 1 {
            bytes(&f.io.target().agent_path(), &macho(false, 9), 0o755);
        }
        if kind == 2 {
            let path = companion(&f);
            let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            record["inventory"]["files"][0]["signing"]["designated_requirement"] =
                json!("foreign rule");
            bytes(&path, &serde_json::to_vec(&record).unwrap(), 0o600);
        }
        if kind == 3 {
            bytes(
                &f.home.join(".local/bin/.crosspanectl.crosspane-stage"),
                b"retained",
                0o755,
            );
        }
        let payload = newer_payload(&f);
        assert!(payload.plan(2, 2, None, &f.deadline()).is_err());
        assert!(f.io.target().agent_path().exists());
    }
}
#[test]
fn published_receipt_change_after_consent_refuses_before_any_publication() {
    let f = Fixture::new(false);
    f.install(None, None);
    let payload = f.payload();
    let plan = payload.plan(2, 2, None, &f.deadline()).unwrap();
    let consent = plan.consent(2, 2, false).unwrap();
    let path = f.io.target().installer_dir().join("payload.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["receipt"]["operation_id"] = json!(3);
    bytes(&path, &serde_json::to_vec(&record).unwrap(), 0o600);
    assert_eq!(
        payload
            .install(plan, consent, None, &f.deadline())
            .unwrap_err(),
        NativeError::Foreign
    );
    let after: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(after["receipt"]["operation_id"], 3);
}

#[test]
fn new_recovery_remnant_after_preview_is_retained_without_cleanup_authority() {
    let f = Fixture::new(false);
    f.install(None, None);
    let payload = f.payload();
    let plan = payload.plan(2, 2, None, &f.deadline()).unwrap();
    let consent = plan.consent(2, 2, false).unwrap();
    let foreign = f.home.join(".local/bin/.crosspanectl.crosspane-previous");
    bytes(&foreign, b"owner retained bytes", 0o755);
    assert_eq!(
        payload
            .install(plan, consent, None, &f.deadline())
            .unwrap_err(),
        NativeError::OutcomeUnknown
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"owner retained bytes");
}

#[test]
fn failed_prior_inventory_publication_is_retained_and_blocks_blind_retry() {
    let f = Fixture::new(false);
    let io = f.hooked(Arc::new(|stage, path, identity| {
        if stage == "write"
            && path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("payload-inventory-")
        {
            return Err(NativeError::Unavailable);
        }
        Ok(identity)
    }));
    let payload = MacPayload::admit(io, inventory(), &f.deadline()).unwrap();
    let plan = payload.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, false).unwrap();
    assert!(matches!(
        payload.install(plan, consent, None, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    let recovery = f.payload().recovery(&f.deadline()).unwrap();
    assert_eq!(recovery.retained_temporaries.len(), 1);
    assert!(recovery.record.is_none());
    assert!(!recovery.app_present && !recovery.ctl_present);
    assert!(matches!(
        f.payload().plan(2, 2, None, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    ));
    assert!(recovery.retained_temporaries[0].exists());
}
