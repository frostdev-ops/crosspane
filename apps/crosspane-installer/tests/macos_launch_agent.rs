#![cfg(target_os = "macos")]
#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Explicit scratch roots, fake GUI/signature/process observations; no real native commands.
use crosspane_installer::agent_contract;
#[path = "../src/platform/macos/launchd_observation.rs"]
#[allow(dead_code)]
mod launchd_observation;
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
#[path = "../src/platform/macos/launch_agent.rs"]
mod launch_agent;
use launch_agent::*;
use rustix::{fd::OwnedFd, fs as rfs};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
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
    native_calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    behavior: Mutex<Behavior>,
}
impl CommandRunner for Runner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        deadline.check()?;
        self.calls.lock().unwrap().push(spec.program().to_owned());
        assert_eq!(spec.environment()["LC_ALL"], "C");
        assert_eq!(spec.environment()["TZ"], "UTC");
        self.native_calls
            .lock()
            .unwrap()
            .push((spec.program().to_owned(), spec.args().to_vec()));
        if spec.program() == Path::new("/bin/launchctl")
            || spec.program() == Path::new("/usr/bin/plutil")
        {
            return self.native(spec, deadline);
        }
        if spec.program() == Path::new("/bin/ps") {
            let pid: u64 = spec.args()[3].parse().unwrap();
            if self.stopped.load(Ordering::Acquire) || pid != self.pid.load(Ordering::Acquire) {
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
        if !name.starts_with("cp-c2-")
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
            "Crosspane.app/Contents/MacOS/crosspane-tutorial",
            Some(PayloadRole::Tutorial),
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
    _listener: UnixListener,
}
impl Fixture {
    fn new(installed: bool) -> Self {
        let scratch = Scratch::create(format!(
            "cp-c2-{}-{}",
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
            native_calls: Mutex::default(),
            behavior: Mutex::new(Behavior {
                job_pid: if installed { 4242 } else { 0 },
                ..Behavior::default()
            }),
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
            _listener: listener,
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
        value["result"]["installer"]["instance"] = json!({"id":id,"pid":4241+id,"uid":self.runner.uid,
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

#[derive(Default)]
struct Behavior {
    job_pid: u32,
    loaded_stopped: bool,
    disabled: u8,
    bootstrap: u8,
    bootout: u8,
    lint: u8,
    print_override: Option<CommandOutput>,
    disabled_override: Option<CommandOutput>,
}
fn output(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> CommandOutput {
    CommandOutput {
        code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}
impl Runner {
    fn native(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        deadline.check()?;
        let home = PathBuf::from(&spec.environment()["HOME"]);
        assert!(owned_root(&home).path != home);
        let runtime = PathBuf::from(&spec.environment()["CROSSPANE_RUNTIME_DIR"]);
        let domain = format!("gui/{}", self.uid);
        let service = format!("{domain}/{AGENT_LABEL}");
        let plist = home.join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
        let mut b = self.behavior.lock().unwrap();
        if spec.program() == Path::new("/usr/bin/plutil") {
            assert_eq!(
                spec.args(),
                &[
                    "-lint".to_owned(),
                    "--".into(),
                    plist.to_str().unwrap().into()
                ]
            );
            assert!(!spec.is_mutation());
            if b.lint == 2 {
                bytes(&plist, b"changed-during-lint", 0o600);
            } else if b.lint == 3 {
                bytes(
                    &home.join(".local/bin/crosspanectl"),
                    &macho(false, 2),
                    0o755,
                );
            } else if b.lint == 4 {
                let record =
                    home.join("Library/Application Support/Crosspane/Installer/payload.json");
                let mut value: Value = serde_json::from_slice(&read_owned(&record)).unwrap();
                value["phase"] = json!("Published");
                value["receipt"]["unfinished"] = json!([12]);
                bytes(&record, &serde_json::to_vec(&value).unwrap(), 0o600);
            }
            return Ok(output(
                if b.lint == 1 { 1 } else { 0 },
                b"".to_vec(),
                b"".to_vec(),
            ));
        }
        assert_eq!(
            spec.max_output(),
            if spec.is_mutation() {
                64 * 1024
            } else {
                1024 * 1024
            }
        );
        match spec.args()[0].as_str() {
            "print" => {
                assert_eq!(spec.args(), &["print", &service]);
                assert!(!spec.is_mutation());
                if let Some(value) = &b.print_override {
                    return Ok(value.clone());
                }
                Ok(if b.loaded_stopped {
                    output(
                        0,
                        format!(
                            "{service} = {{\n path = {}\n program = {}\n state = not running\n}}\n",
                            plist.display(),
                            self.exe.display()
                        )
                        .into_bytes(),
                        vec![],
                    )
                } else if b.job_pid == 0 {
                    output(
                        113,
                        vec![],
                        format!(
                            "Bad request.\nCould not find service \"{AGENT_LABEL}\" in domain for user gui: {}\n",
                            self.uid
                        )
                        .into_bytes(),
                    )
                } else {
                    output(
                        0,
                        format!(
                            "{service} = {{\n path = {}\n program = {}\n pid = {}\n}}\n",
                            plist.display(),
                            self.exe.display(),
                            b.job_pid
                        )
                        .into_bytes(),
                        vec![],
                    )
                })
            }
            "print-disabled" => {
                assert_eq!(spec.args(), &["print-disabled", &domain]);
                assert!(!spec.is_mutation());
                if let Some(value) = &b.disabled_override {
                    return Ok(value.clone());
                }
                Ok(output(
                    0,
                    match b.disabled {
                        0 => format!(
                            "\n\tdisabled services = {{\n\t\t\"{AGENT_LABEL}\" => enabled\n\t}}\n"
                        ),
                        1 => format!(
                            "\n\tdisabled services = {{\n\t\t\"{AGENT_LABEL}\" => disabled\n\t}}\n"
                        ),
                        _ => "unobservable format\n".into(),
                    }
                    .into_bytes(),
                    vec![],
                ))
            }
            "bootout" => {
                assert_eq!(spec.args(), &["bootout", &service]);
                assert!(spec.is_mutation());
                let intent: Value = serde_json::from_slice(&read_owned(
                    &home.join("Library/Application Support/Crosspane/Installer/launch-agent.json"),
                ))
                .unwrap();
                assert_eq!(intent["phase"], "Intent");
                if b.bootout == 4 {
                    return Err(NativeError::OutcomeUnknown);
                }
                if b.bootout != 1 {
                    b.job_pid = 0;
                    b.loaded_stopped = false;
                    self.stopped.store(true, Ordering::Release);
                    if b.bootout != 2 {
                        let original =
                            parse_bootstrap(&read_owned(&runtime.join("bootstrap.json"))).unwrap();
                        let value = json!({"schema_version":1,"instance_id":if b.bootout == 5 {9}else{original.instance_id},
                            "stopped_unix_ms":2000,"clean":b.bootout != 3,"parking":if b.bootout == 3 {"failed"}else{"restored"},
                            "input_journals_empty":true,"audio_stopped":true});
                        bytes(
                            &home.join("Library/Application Support/Crosspane/last_exit.json"),
                            &serde_json::to_vec(&value).unwrap(),
                            0o600,
                        );
                    }
                }
                Ok(output(0, vec![], vec![]))
            }
            "bootstrap" => {
                assert_eq!(
                    spec.args(),
                    &["bootstrap", &domain, plist.to_str().unwrap()]
                );
                assert!(spec.is_mutation());
                let intent: Value = serde_json::from_slice(&read_owned(
                    &home.join("Library/Application Support/Crosspane/Installer/launch-agent.json"),
                ))
                .unwrap();
                assert_eq!(intent["phase"], "BootstrapRequested");
                if b.bootstrap == 1 {
                    return Ok(output(5, vec![], b"denied-fixture-only".to_vec()));
                }
                if b.bootstrap == 3 {
                    return Err(NativeError::OutcomeUnknown);
                }
                let previous =
                    parse_bootstrap(&read_owned(&runtime.join("bootstrap.json"))).unwrap();
                let next = previous.instance_id + 1;
                b.job_pid = previous.pid + 1;
                b.loaded_stopped = false;
                self.pid.store(u64::from(b.job_pid), Ordering::Release);
                self.stopped.store(false, Ordering::Release);
                let phase = if b.bootstrap == 4 {
                    "waiting_for_keystore"
                } else {
                    "ready"
                };
                bytes(&runtime.join("bootstrap.json"), &serde_json::to_vec(&json!({
                    "schema_version":1,"instance_id":next,"pid":b.job_pid,"started_unix_ms":1000,
                    "phase":phase,"phase_seq":1,"keystore":if b.bootstrap == 4 {Value::Null}else{json!("os_store")},
                    "reason":null,"runtime_dir":runtime})).unwrap(), 0o600);
                if b.bootstrap == 2 {
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(output(0, vec![], vec![]))
            }
            _ => panic!("unapproved native action"),
        }
    }
    fn count(&self, verb: &str) -> usize {
        self.native_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(program, args)| program == Path::new("/bin/launchctl") && args[0] == verb)
            .count()
    }
}
struct ApprovalFixture(Approval);
impl ApprovalProbe for ApprovalFixture {
    fn observe(&self, target: &MacTarget, deadline: &Deadline) -> NativeResult<Approval> {
        deadline.check()?;
        owned_root(&target.paths().home);
        Ok(self.0)
    }
}
fn adapter(f: &Fixture, approval: Approval) -> MacLaunchAgent {
    MacLaunchAgent::admit(
        f.io.clone(),
        inventory(),
        Arc::new(ApprovalFixture(approval)),
        &f.deadline(),
    )
    .unwrap()
}
fn launch_plist(f: &Fixture) -> PathBuf {
    f.home
        .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist")
}
// Independent complete golden bytes: no production template or renderer is consulted.
fn expected_plist(home: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.frostdev.crosspane.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>{home}/Applications/Crosspane.app/Contents/MacOS/Crosspane</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>StandardErrorPath</key>
    <string>{home}/Library/Logs/Crosspane/agent.log</string>
    <key>StandardOutPath</key>
    <string>{home}/Library/Logs/Crosspane/agent.log</string>
</dict>
</plist>
"#).into_bytes()
}
fn execute_new(f: &Fixture, a: &mut MacLaunchAgent) -> PendingLaunch {
    let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), LaunchState::Absent);
    let consent = plan.consent(1, 1, false, false).unwrap();
    a.execute(plan, consent, &f.deadline()).unwrap()
}
fn read_owned(path: &Path) -> Vec<u8> {
    let (_root, parent, name) = owned_parent(path);
    let fd = rfs::openat(
        &parent,
        name,
        rfs::OFlags::RDONLY | rfs::OFlags::NOFOLLOW | rfs::OFlags::CLOEXEC,
        rfs::Mode::empty(),
    )
    .unwrap();
    assert!(same_inode(&owned_stat(path), &rfs::fstat(&fd).unwrap()));
    let mut data = vec![];
    fs::File::from(fd).read_to_end(&mut data).unwrap();
    data
}
fn replace_existing(f: &Fixture, a: &mut MacLaunchAgent) -> PendingLaunch {
    bytes(
        &launch_plist(f),
        &render_plist(f.io.target()).unwrap(),
        0o600,
    );
    let selected = f.selected();
    let reply = f.reply(&f.status(1), 1);
    let plan = a
        .plan(1, 1, Some((&selected, &reply)), &f.deadline())
        .unwrap();
    assert_eq!(plan.state(), LaunchState::AdoptionRequired);
    assert!(plan.consent(1, 1, false, true).is_err());
    let consent = plan.consent(1, 1, true, true).unwrap();
    a.execute(plan, consent, &f.deadline()).unwrap()
}
fn finish(
    f: &Fixture,
    a: &MacLaunchAgent,
    pending: &mut PendingLaunch,
    id: u64,
    call: u64,
) -> StartupFacts {
    pending.expect_health(call).unwrap();
    a.observe(
        pending,
        &f.selected(),
        f.reply(&f.status(id), call),
        None,
        &f.deadline(),
    )
    .unwrap()
}

#[test]
fn escaped_plist_preserves_fixed_launchagent_semantics_and_bounded_exact_argv() {
    let f = Fixture::new(false);
    let mut paths = f.io.target().paths().clone();
    paths.home = f.root.join("h&<>\"'é");
    let target = MacTarget::scratch(paths).unwrap();
    assert_eq!(
        render_plist(&target).unwrap(),
        expected_plist(&format!("{}/h&amp;&lt;&gt;&quot;&apos;é", f.root.display()))
    );
    let mut a = adapter(&f, Approval::Allowed);
    let pending = execute_new(&f, &mut a);
    assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
    assert_eq!(pending.error(), None);
    assert_eq!(
        read_owned(&launch_plist(&f)),
        expected_plist(f.home.to_str().unwrap())
    );
    let calls = f.runner.native_calls.lock().unwrap();
    let lint = calls
        .iter()
        .position(|(p, _)| p == Path::new("/usr/bin/plutil"))
        .unwrap();
    let bootstrap = calls
        .iter()
        .position(|(p, args)| p == Path::new("/bin/launchctl") && args[0] == "bootstrap")
        .unwrap();
    assert!(lint < bootstrap);
    drop(calls);
    assert_eq!(f.runner.count("bootstrap"), 1);
    let record: Value = serde_json::from_slice(&read_owned(
        &f.io.target().installer_dir().join("launch-agent.json"),
    ))
    .unwrap();
    assert_eq!(record["phase"], "BootstrapRequested");
    assert!(
        !record["receipt"]["unfinished"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn exclusive_descriptor_scratch_and_mutation_helpers_refuse_aliases_and_unowned_paths() {
    let f = Fixture::new(false);
    let name = format!("cp-c2-child-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let child = Scratch::create_in(&f._scratch, name.clone()).unwrap();
    assert!(matches!(
        Scratch::create_in(&f._scratch, name),
        Err(rustix::io::Errno::EXIST)
    ));
    drop(child);
    let alias = f.home.join("escape");
    symlink_owned(&f.source, &alias);
    let original = read_owned(&f.source.join("crosspanectl"));
    assert!(
        std::panic::catch_unwind(|| bytes(&alias.join("crosspanectl"), b"must-not-write", 0o755))
            .is_err()
    );
    assert!(
        std::panic::catch_unwind(|| remove_owned(Path::new("/private/tmp/not-owned"))).is_err()
    );
    assert_eq!(read_owned(&f.source.join("crosspanectl")), original);
}
#[test]
fn unsupported_target_creates_no_payload_plist_config_or_startup_action() {
    for case in 0..4 {
        let f = Fixture::new(false);
        {
            let mut facts = f.support.observation.lock().unwrap();
            match case {
                0 => facts.macos_major = 25,
                1 => facts.apple_silicon = false,
                2 => facts.gui.active = false,
                3 => facts.gui.interactive_uid = None,
                _ => unreachable!(),
            }
        }
        assert!(
            MacLaunchAgent::admit(
                f.io.clone(),
                inventory(),
                Arc::new(UnobservableApproval),
                &f.deadline()
            )
            .is_err()
        );
        assert!(!launch_plist(&f).exists());
        assert!(!f.home.join("Applications").exists());
        assert!(!f.io.target().state_dir().exists());
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn persistent_disabled_or_unobservable_native_state_never_enables_or_bootstraps() {
    for disabled in [1, 2] {
        let f = Fixture::new(false);
        let mut a = adapter(&f, Approval::Allowed);
        f.runner.behavior.lock().unwrap().disabled = disabled;
        let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
        assert_eq!(
            plan.state(),
            if disabled == 1 {
                LaunchState::UserDisabled
            } else {
                LaunchState::Unobservable
            }
        );
        assert!(plan.consent(1, 1, true, true).is_err());
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert!(!f.io.target().installer_dir().exists());
    }
}
#[test]
fn real_c1_clean_stop_and_new_instance_health_release_only_payload_backups() {
    let f = Fixture::new(true);
    for name in [
        "identity",
        "trust.json",
        "config.toml",
        "layout.json",
        "recovery",
    ] {
        bytes(
            &f.io.target().state_dir().join(name),
            name.as_bytes(),
            0o600,
        );
    }
    let mut a = adapter(&f, Approval::Allowed);
    let mut pending = replace_existing(&f, &mut a);
    assert_eq!(pending.error(), None);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 1);
    let previous = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous");
    assert!(previous.exists());
    let prior = pending.retained_prior().unwrap().to_owned();
    assert_eq!(read_owned(&prior), render_plist(f.io.target()).unwrap());
    let facts = finish(&f, &a, &mut pending, 2, 2);
    assert!(facts.payload_verified.is_some());
    assert_eq!(facts.login, LoginEvidence::SameSession);
    assert!(!previous.exists());
    assert!(prior.exists());
    for name in [
        "identity",
        "trust.json",
        "config.toml",
        "layout.json",
        "recovery",
    ] {
        assert_eq!(
            read_owned(&f.io.target().state_dir().join(name)),
            name.as_bytes()
        );
    }
}
#[test]
fn live_original_missing_unclean_or_wrong_receipt_never_replaces_and_resume_never_reissues_stop() {
    for mode in [1, 2, 3, 5] {
        let f = Fixture::new(true);
        f.runner.behavior.lock().unwrap().bootout = mode;
        let mut a = adapter(&f, Approval::Allowed);
        let mut pending = replace_existing(&f, &mut a);
        assert_eq!(f.runner.count("bootout"), 1);
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 0));
        assert!(
            !f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
        if mode == 5 {
            assert_eq!(pending.phase(), LaunchPhase::Unknown);
        } else {
            assert_eq!(pending.phase(), LaunchPhase::WaitingForCleanStop);
        }
        if mode == 1 {
            f.exit(true, "restored");
            f.runner.behavior.lock().unwrap().job_pid = 0;
            a.resume_clean_stop(&mut pending, &f.deadline()).unwrap();
            assert_eq!(pending.error(), None);
            assert_eq!(f.runner.count("bootstrap"), 1);
            assert_eq!(f.runner.count("bootout"), 1);
        }
    }
}
#[test]
fn unknown_bootout_and_bootstrap_require_detection_without_resending_mutations() {
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 4;
    let mut a = adapter(&f, Approval::Unknown);
    let mut pending = replace_existing(&f, &mut a);
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
    assert!(a.resume_clean_stop(&mut pending, &f.deadline()).is_err());
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 0);
    let f = Fixture::new(false);
    f.runner.behavior.lock().unwrap().bootstrap = 2;
    let mut a = adapter(&f, Approval::Unknown);
    let mut pending = execute_new(&f, &mut a);
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
    let mut reconstructed = adapter(&f, Approval::Unknown);
    let selected = f.selected();
    assert!(matches!(
        reconstructed.plan(
            2,
            2,
            Some((&selected, &f.reply(&f.status(2), 1))),
            &f.deadline()
        ),
        Err(NativeError::OutcomeUnknown)
    ));
    let facts = finish(&f, &a, &mut pending, 2, 2);
    assert!(facts.payload_verified.is_some());
    assert_eq!(facts.approval, Approval::Unknown);
    assert_eq!(f.runner.count("bootstrap"), 1);
}
#[test]
fn bootstrap_exit_zero_and_ready_are_progress_denied_unknown_approval_remains_explicit() {
    for approval in [Approval::Denied, Approval::Unknown] {
        let f = Fixture::new(false);
        let mut a = adapter(&f, approval);
        let mut pending = execute_new(&f, &mut a);
        assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
        assert!(
            !f.payload()
                .recovery(&f.deadline())
                .unwrap()
                .record
                .unwrap()
                .receipt
                .unfinished
                .is_empty()
        );
        let facts = finish(&f, &a, &mut pending, 2, 2);
        assert_eq!(facts.approval, approval);
        assert_eq!(facts.login, LoginEvidence::SameSession);
        let record: Value = serde_json::from_slice(&read_owned(
            &f.io.target().installer_dir().join("launch-agent.json"),
        ))
        .unwrap();
        assert_eq!(record["receipt"]["resources"][0]["outcome"], "Unknown");
        assert_eq!(record["receipt"]["unfinished"], json!([12]));
    }
}
#[test]
fn keychain_wait_and_failed_startup_recovery_keep_real_c1_backups_without_fallback_identity() {
    for waiting in [true, false] {
        let f = Fixture::new(true);
        f.runner.behavior.lock().unwrap().bootstrap = if waiting { 4 } else { 0 };
        let mut a = adapter(&f, Approval::Unknown);
        let mut pending = replace_existing(&f, &mut a);
        pending.expect_health(2).unwrap();
        let mut value = f.status(2);
        if !waiting {
            value["result"]["installer"]["startup_recovery"] = json!("failed");
        }
        assert!(
            a.observe(
                &mut pending,
                &f.selected(),
                f.reply(&value, 2),
                None,
                &f.deadline()
            )
            .is_err()
        );
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
        assert!(!f.io.target().state_dir().join("identity").exists());
    }
}
#[test]
fn native_process_plist_and_disabled_parsers_fail_closed_on_wrong_or_ambiguous_observations() {
    for case in 0..6 {
        let f = Fixture::new(true);
        bytes(
            &launch_plist(&f),
            &render_plist(f.io.target()).unwrap(),
            0o600,
        );
        let mut a = adapter(&f, Approval::Allowed);
        let selected = f.selected();
        let domain = format!("gui/{}/{AGENT_LABEL}", f.runner.uid);
        let mut b = f.runner.behavior.lock().unwrap();
        if case == 0 {
            b.job_pid = 99999;
        }
        if case == 1 {
            b.print_override = Some(output(
                0,
                format!(
                    "{domain} = {{\n path = {}\n program = /foreign\n pid = 4242\n}}\n",
                    launch_plist(&f).display()
                )
                .into_bytes(),
                vec![],
            ));
        }
        if case == 2 {
            b.print_override = Some(output(
                0,
                format!(
                    "{domain} = {{\n path = {}\n program = {}\n pid = 4242\n pid = 4242\n}}\n",
                    launch_plist(&f).display(),
                    f.runner.exe.display()
                )
                .into_bytes(),
                vec![],
            ));
        }
        if case == 3 {
            b.disabled_override = Some(output(0, format!("\n\tdisabled services = {{\n\t\t\"{AGENT_LABEL}\" => enabled\n\t\t\"{AGENT_LABEL}\" => enabled\n\t}}\n").into_bytes(), vec![]));
        }
        if case == 4 {
            b.print_override = Some(output(0, vec![b'x'; 64 * 1024 + 1], vec![]));
        }
        if case == 5 {
            b.disabled_override = Some(output(1, vec![], b"not-observable".to_vec()));
        }
        drop(b);
        match a.plan(
            1,
            1,
            Some((&selected, &f.reply(&f.status(1), 1))),
            &f.deadline(),
        ) {
            Ok(plan) => {
                assert_eq!(plan.state(), LaunchState::Unobservable);
                assert!(plan.consent(1, 1, true, true).is_err());
            }
            Err(error) => assert!(matches!(
                error,
                NativeError::Foreign | NativeError::Oversize
            )),
        }
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn stale_view_consent_resource_drift_and_replayed_health_cannot_change_authority() {
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Allowed);
    let old = a.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = old.consent(1, 1, false, false).unwrap();
    let plan = a.plan(2, 2, None, &f.deadline()).unwrap();
    assert!(a.execute(plan, consent, &f.deadline()).is_err());
    assert_eq!(f.runner.count("bootstrap"), 0);
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Allowed);
    let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, false, false).unwrap();
    bytes(&launch_plist(&f), b"foreign-edit", 0o600);
    assert!(a.execute(plan, consent, &f.deadline()).is_err());
    assert_eq!(read_owned(&launch_plist(&f)), b"foreign-edit");
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut a);
    let facts = finish(&f, &a, &mut pending, 2, 2);
    assert!(
        a.observe(
            &mut pending,
            &f.selected(),
            facts.reply,
            None,
            &f.deadline()
        )
        .is_err()
    );
    assert!(pending.expect_health(2).is_err());
}
#[test]
fn same_session_launchagent_only_restart_is_distinct_from_observed_new_gui_session_and_retains_prior()
 {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Unknown);
    let mut pending = execute_new(&f, &mut first);
    finish(&f, &first, &mut pending, 2, 1);
    let mut a = adapter(&f, Approval::Unknown);
    let selected = f.selected();
    let plan = a
        .plan(
            2,
            2,
            Some((&selected, &f.reply(&f.status(2), 2))),
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(plan.state(), LaunchState::Owned);
    let consent = plan.consent(2, 2, false, false).unwrap();
    let mut pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert_eq!(pending.error(), None);
    let facts = finish(&f, &a, &mut pending, 3, 3);
    assert!(facts.payload_verified.is_none());
    assert_eq!(facts.login, LoginEvidence::SameSession);
    let prior = facts.retained_prior.unwrap();
    assert!(prior.exists());
    {
        let mut facts = f.support.observation.lock().unwrap();
        facts.gui.console_session = "fake-next-login".into();
        facts.gui.interactive_session = "fake-next-login".into();
    }
    f.runner.pid.store(4245, Ordering::Release);
    f.runner.behavior.lock().unwrap().job_pid = 4245;
    f.bootstrap(4, 4245, 1000, "ready");
    let facts = finish(&f, &a, &mut pending, 4, 4);
    assert_eq!(facts.login, LoginEvidence::DifferentInteractiveSession);
    assert_eq!(facts.approval, Approval::Unknown);
    assert!(prior.exists());
    assert_eq!(f.runner.count("bootstrap"), 2);
}
#[test]
fn plist_lint_failure_or_substitution_never_bootstraps_and_retains_prior_state() {
    for lint in [1, 2] {
        let f = Fixture::new(true);
        let mut a = adapter(&f, Approval::Allowed);
        f.runner.behavior.lock().unwrap().lint = lint;
        let pending = replace_existing(&f, &mut a);
        assert_eq!(pending.phase(), LaunchPhase::Unknown);
        assert!(pending.error().is_some());
        if lint == 2 {
            assert_eq!(read_owned(&launch_plist(&f)), b"changed-during-lint");
        }
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert!(pending.retained_prior().unwrap().exists());
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
    }
}

#[test]
fn stopped_process_with_each_well_formed_unclean_fact_keeps_original_payload_and_waits() {
    for fact in ["parking", "input_journals_empty", "audio_stopped"] {
        let f = Fixture::new(true);
        f.runner.behavior.lock().unwrap().bootout = 2;
        let mut a = adapter(&f, Approval::Unknown);
        let mut pending = replace_existing(&f, &mut a);
        let mut receipt = json!({"schema_version":1,"instance_id":1,"stopped_unix_ms":2000,
            "clean":false,"parking":"restored","input_journals_empty":true,"audio_stopped":true});
        if fact == "parking" {
            receipt[fact] = json!("failed");
        } else {
            receipt[fact] = json!(false);
        }
        let encoded = serde_json::to_vec(&receipt).unwrap();
        assert!(!parse_last_exit(&encoded).unwrap().clean);
        bytes(
            &f.io.target().state_dir().join("last_exit.json"),
            &encoded,
            0o600,
        );
        a.resume_clean_stop(&mut pending, &f.deadline()).unwrap();
        assert_eq!(pending.phase(), LaunchPhase::WaitingForCleanStop, "{fact}");
        assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 0));
        assert_eq!(f.runner.count("bootout"), 1);
        assert_eq!(f.runner.count("bootstrap"), 0);
    }
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 1;
    let mut a = adapter(&f, Approval::Unknown);
    let mut pending = replace_existing(&f, &mut a);
    bytes(
        &f.io.target().state_dir().join("last_exit.json"),
        &serde_json::to_vec(&json!({
        "schema_version":1,"instance_id":1,"stopped_unix_ms":2000,"clean":true,
        "parking":"restored","input_journals_empty":true,"audio_stopped":true}))
        .unwrap(),
        0o600,
    );
    a.resume_clean_stop(&mut pending, &f.deadline()).unwrap();
    assert_eq!(pending.phase(), LaunchPhase::WaitingForCleanStop);
    assert_eq!(f.runner.count("bootstrap"), 0);
}

#[test]
fn stale_prebootstrap_wrong_source_instance_or_call_replies_cannot_release_backups() {
    for case in 0..5 {
        let f = Fixture::new(true);
        f.clock.0.store(100, Ordering::Release);
        let mut a = adapter(&f, Approval::Unknown);
        let mut pending = replace_existing(&f, &mut a);
        pending.expect_health(2).unwrap();
        if case == 0 {
            f.clock.0.store(10_000, Ordering::Release);
        }
        let selected = f.selected(); // fresh native proof even when the reply is stale
        let mut reply = f.reply(&f.status(2), 2);
        match case {
            0 => reply.observed_at_ms = 100,
            1 => reply.observed_at_ms = 99,
            2 => reply.source = ObservationSource::Live,
            3 => reply = f.reply(&f.status(3), 2),
            4 => reply.id = 3,
            _ => unreachable!(),
        }
        assert!(
            a.observe(&mut pending, &selected, reply, None, &f.deadline())
                .is_err(),
            "{case}"
        );
        assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
        pending.expect_health(4).unwrap();
        let time = f.clock.now_ms();
        let facts = a
            .observe(
                &mut pending,
                &selected,
                f.reply(&f.status(2), 4),
                None,
                &f.deadline(),
            )
            .unwrap();
        assert_eq!(facts.reply.observed_at_ms, time);
        assert_eq!(facts.reply.source, ObservationSource::Demo);
        assert!(facts.payload_verified.is_some());
    }
}

#[test]
fn timeout_before_start_keeps_pending_until_observed_new_instance_without_resending() {
    let f = Fixture::new(true);
    let original = f.selected();
    f.runner.behavior.lock().unwrap().bootstrap = 3;
    let mut a = adapter(&f, Approval::Unknown);
    let mut pending = replace_existing(&f, &mut a);
    assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
    pending.expect_health(1).unwrap();
    assert!(
        original
            .instance
            .revalidate(&original.io, &original.support, &f.deadline())
            .is_err()
    );
    assert!(
        f.home
            .join("Applications/.Crosspane.app.crosspane-previous")
            .exists()
    );
    f.runner.stopped.store(false, Ordering::Release);
    f.runner.pid.store(4243, Ordering::Release);
    f.runner.behavior.lock().unwrap().job_pid = 4243;
    f.bootstrap(2, 4243, 1000, "ready");
    let facts = a
        .observe(
            &mut pending,
            &f.selected(),
            f.reply(&f.status(2), 1),
            None,
            &f.deadline(),
        )
        .unwrap();
    assert!(facts.payload_verified.is_some());
    assert_eq!(f.runner.count("bootstrap"), 1);
}

#[test]
fn file_alias_directory_and_hardlink_conflicts_and_id_exhaustion_start_no_mutation() {
    for case in 0..3 {
        let f = Fixture::new(false);
        let path = launch_plist(&f);
        match case {
            0 => symlink_owned(&f.source.join("crosspanectl"), &path),
            1 => directory(&path),
            2 => {
                bytes(&path, b"foreign", 0o600);
                hardlink_owned(&path, &f.home.join("outside-inventory"));
            }
            _ => unreachable!(),
        }
        let mut a = adapter(&f, Approval::Unknown);
        assert!(a.plan(1, 1, None, &f.deadline()).is_err());
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert!(!f.io.target().installer_dir().exists());
    }
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Unknown);
    let plan = a.plan(u64::MAX, u64::MAX, None, &f.deadline()).unwrap();
    assert!(!plan.interrupts_agent());
    assert!(matches!(
        a.plan(u64::MAX, u64::MAX, None, &f.deadline()),
        Err(NativeError::IdExhausted)
    ));
    assert!(plan.consent(u64::MAX - 1, u64::MAX, false, false).is_err());
    assert_eq!(f.runner.count("bootstrap"), 0);
}

struct LaunchReceiptOps {
    path: PathBuf,
    versions: Mutex<Vec<Vec<u8>>>,
    active: AtomicBool,
    events: Mutex<Vec<&'static str>>,
    fail: Option<(&'static str, bool)>,
    fired: AtomicBool,
}
impl LaunchReceiptOps {
    fn observe(&self) {
        let bytes = read_owned(&self.path);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["receipt"]["resources"][0]["resource_id"],
            "mac.launch-agent"
        );
        assert!(
            self.versions.lock().unwrap().contains(&bytes),
            "receipt must be complete old or new, never absent"
        );
    }
}
impl FilesystemOps for LaunchReceiptOps {
    fn execute(&self, operation: FilesystemOperation<'_>) -> NativeResult<()> {
        let kind = match &operation {
            FilesystemOperation::Write(_, bytes) => {
                let selected = serde_json::from_slice::<Value>(bytes)
                    .ok()
                    .is_some_and(|v| {
                        v["receipt"]["resources"][0]["resource_id"] == "mac.launch-agent"
                    });
                self.active.store(selected, Ordering::Release);
                if selected {
                    self.versions.lock().unwrap().push(bytes.to_vec());
                }
                "write"
            }
            FilesystemOperation::FileSync(_) => "file-sync",
            FilesystemOperation::Rename { .. } => "rename",
            FilesystemOperation::DirectorySync(_) => "directory-sync",
        };
        let selected = self.active.load(Ordering::Acquire);
        if selected {
            self.events.lock().unwrap().push(kind);
        }
        self.observe();
        let fail = selected
            && self.fail.is_some_and(|(at, _)| at == kind)
            && !self.fired.swap(true, Ordering::AcqRel);
        if fail && !self.fail.unwrap().1 {
            return Err(NativeError::Unavailable);
        }
        SystemFilesystem.execute(operation)?;
        self.observe();
        if kind == "directory-sync" {
            self.active.store(false, Ordering::Release);
        }
        if fail {
            return Err(NativeError::Unavailable);
        }
        Ok(())
    }
}
#[test]
fn actual_receipt_write_sync_atomic_publication_and_directory_sync_keep_complete_old_or_new() {
    for failure in std::iter::once(None).chain(
        ["write", "file-sync", "rename", "directory-sync"]
            .into_iter()
            .flat_map(|kind| [Some((kind, false)), Some((kind, true))]),
    ) {
        let f = Fixture::new(true);
        let mut a = adapter(&f, Approval::Unknown);
        let mut pending = replace_existing(&f, &mut a);
        pending.expect_health(2).unwrap();
        let path = f.io.target().installer_dir().join("launch-agent.json");
        let seam = Arc::new(LaunchReceiptOps {
            versions: Mutex::new(vec![read_owned(&path)]),
            path,
            active: AtomicBool::new(false),
            events: Mutex::default(),
            fail: failure,
            fired: AtomicBool::new(false),
        });
        let selected = f.selected_on(f.operated(seam.clone()));
        let result = a.observe(
            &mut pending,
            &selected,
            f.reply(&f.status(2), 2),
            None,
            &f.deadline(),
        );
        seam.observe();
        if failure.is_some() {
            assert!(seam.fired.load(Ordering::Acquire));
            assert!(matches!(result, Err(NativeError::OutcomeUnknown)));
        } else {
            result.unwrap();
            assert_eq!(
                *seam.events.lock().unwrap(),
                ["write", "file-sync", "rename", "directory-sync"]
            );
        }
        assert!(pending.retained_prior().unwrap().exists());
        assert_eq!(f.runner.count("bootstrap"), 1);
    }
}

#[test]
fn interruption_at_each_scoped_plist_boundary_retains_prior_and_never_retries_bootstrap() {
    for boundary in [
        "create-temp",
        "write",
        "file-sync",
        "publish",
        "parent-sync",
        "unlink",
        "complete",
    ] {
        let f = Fixture::new(true);
        let path = launch_plist(&f);
        let fired = Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        let io = f.hooked(Arc::new(move |stage, p, identity| {
            if p == path && stage == boundary && !observed.swap(true, Ordering::AcqRel) {
                return Err(NativeError::Unavailable);
            }
            Ok(identity)
        }));
        let mut a = MacLaunchAgent::admit(
            io,
            inventory(),
            Arc::new(ApprovalFixture(Approval::Unknown)),
            &f.deadline(),
        )
        .unwrap();
        let pending = replace_existing(&f, &mut a);
        assert!(fired.load(Ordering::Acquire), "{boundary}");
        assert_eq!(pending.phase(), LaunchPhase::Unknown);
        assert!(pending.error().is_some());
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(
            read_owned(pending.retained_prior().unwrap()),
            render_plist(f.io.target()).unwrap()
        );
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
    }
}

#[test]
fn cancellation_and_deadline_before_plist_publication_transmit_no_bootstrap() {
    for cancelled in [false, true] {
        let f = Fixture::new(true);
        let path = launch_plist(&f);
        let cancel = Cancellation::default();
        let signal = cancel.clone();
        let clock = f.clock.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let observed = fired.clone();
        let io = f.hooked(Arc::new(move |stage, p, identity| {
            if p == path && stage == "publish" && !observed.swap(true, Ordering::AcqRel) {
                if cancelled {
                    signal.cancel();
                } else {
                    clock.0.store(5000, Ordering::Release);
                }
            }
            Ok(identity)
        }));
        let mut a = MacLaunchAgent::admit(
            io,
            inventory(),
            Arc::new(ApprovalFixture(Approval::Unknown)),
            &f.deadline(),
        )
        .unwrap();
        bytes(
            &launch_plist(&f),
            &render_plist(f.io.target()).unwrap(),
            0o600,
        );
        let selected = f.selected();
        let deadline = Deadline::new(5000, f.clock.clone(), cancel).unwrap();
        let plan = a
            .plan(
                1,
                1,
                Some((&selected, &f.reply(&f.status(1), 1))),
                &deadline,
            )
            .unwrap();
        let consent = plan.consent(1, 1, true, true).unwrap();
        let pending = a.execute(plan, consent, &deadline).unwrap();
        assert!(fired.load(Ordering::Acquire));
        assert_eq!(pending.phase(), LaunchPhase::Unknown);
        assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(f.runner.count("bootout"), 1);
        assert!(pending.retained_prior().unwrap().exists());
    }
}

#[test]
fn coherent_superseded_plan_and_own_consent_are_rejected_before_any_io() {
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Allowed);
    let old = a.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = old.consent(1, 1, false, false).unwrap();
    let _new = a.plan(2, 2, None, &f.deadline()).unwrap();
    let calls = f.runner.native_calls.lock().unwrap().len();
    let signatures = f.signatures.calls.lock().unwrap().len();
    assert!(a.execute(old, consent, &f.deadline()).is_err());
    assert_eq!(f.runner.native_calls.lock().unwrap().len(), calls);
    assert_eq!(f.signatures.calls.lock().unwrap().len(), signatures);
    assert!(!f.home.join("Applications").exists());
    assert!(!f.io.target().installer_dir().exists());
}

#[test]
fn matching_authority_is_rechecked_for_full_inventory_and_unfinished_receipts_before_bootstrap() {
    for lint in [0, 3, 4] {
        let f = Fixture::new(false);
        let mut first = adapter(&f, Approval::Unknown);
        let mut pending = execute_new(&f, &mut first);
        finish(&f, &first, &mut pending, 2, 1);
        let mut a = adapter(&f, Approval::Unknown);
        let selected = f.selected();
        let plan = a
            .plan(
                2,
                2,
                Some((&selected, &f.reply(&f.status(2), 2))),
                &f.deadline(),
            )
            .unwrap();
        assert_eq!(plan.state(), LaunchState::Owned);
        let consent = plan.consent(2, 2, false, false).unwrap();
        f.runner.behavior.lock().unwrap().lint = lint;
        let pending = a.execute(plan, consent, &f.deadline()).unwrap();
        if lint == 0 {
            assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
            assert_eq!(f.runner.count("bootstrap"), 2);
        } else {
            assert_eq!(pending.phase(), LaunchPhase::Unknown);
            assert_eq!(f.runner.count("bootstrap"), 1);
            assert!(pending.retained_prior().unwrap().exists());
            if lint == 3 {
                assert_eq!(
                    read_owned(&f.home.join(".local/bin/crosspanectl")),
                    macho(false, 2)
                );
            } else {
                assert_eq!(
                    f.payload()
                        .recovery(&f.deadline())
                        .unwrap()
                        .record
                        .unwrap()
                        .phase,
                    PayloadPhase::Published
                );
            }
        }
    }
}

#[test]
fn nested_duplicate_or_unbalanced_job_fields_never_admit_an_actionable_service() {
    for case in 0..12 {
        let f = Fixture::new(true);
        bytes(
            &launch_plist(&f),
            &render_plist(f.io.target()).unwrap(),
            0o600,
        );
        let fields = format!(
            "path = {}\nprogram = {}\npid = 4242\n",
            launch_plist(&f).display(),
            f.runner.exe.display()
        );
        let body = match case {
            0 => format!("nested = {{\n{fields}}}\n"),
            1 => format!("{fields}nested = {{\n"),
            2 => format!("{fields}}}\nignored\n"),
            3 => format!("{fields}nested {{ malformed\n"),
            4 => format!("{fields}program = {}\n", f.runner.exe.display()),
            5 => format!("nested = {{ extra = {{\n}}\n{fields}"),
            6 => format!("{fields}pid = {{\nignored = true\n}}\n"),
            7 => format!("\"nested\" = {{\n}}\n{fields}"),
            8 => format!("nested # comment = {{\n}}\n{fields}"),
            9 => format!("nested {{ junk = {{\n}}\n{fields}"),
            10 => format!("pid = {{\nignored = true\n}}\n{fields}"),
            11 => format!("metadata = {{\nignored = value\n}}\n{fields}"),
            _ => unreachable!(),
        };
        f.runner.behavior.lock().unwrap().print_override = Some(output(
            0,
            format!("gui/{}/{AGENT_LABEL} = {{\n{body}}}\n", f.runner.uid).into_bytes(),
            vec![],
        ));
        let mut a = adapter(&f, Approval::Unknown);
        let selected = f.selected();
        let plan = a
            .plan(
                1,
                1,
                Some((&selected, &f.reply(&f.status(1), 1))),
                &f.deadline(),
            )
            .unwrap();
        if matches!(case, 3 | 7 | 8 | 9 | 11) {
            assert_eq!(plan.state(), LaunchState::AdoptionRequired);
            assert!(plan.consent(1, 1, true, true).is_ok());
        } else {
            assert_eq!(plan.state(), LaunchState::Unobservable, "{case}");
            assert!(plan.consent(1, 1, true, true).is_err());
        }
        assert_eq!(f.runner.count("bootout"), 0);
    }
}

#[test]
fn whole_disabled_key_requires_supported_quoting_and_cannot_hide_selected_disable() {
    for key in [
        "\"io.frostdev.crosspane.agent\\\"",
        "\"io.frostdev.crosspane.agent\"garbage\"",
        "\"\"",
        "\"io.frostdev.crosspane.agent\\u002e\"",
        "unquoted",
        "\"label with space\"",
    ] {
        let f = Fixture::new(false);
        f.runner.behavior.lock().unwrap().disabled_override = Some(output(
            0,
            format!("\n\tdisabled services = {{\n\t\t{key} => disabled\n\t}}\n").into_bytes(),
            vec![],
        ));
        let mut a = adapter(&f, Approval::Unknown);
        let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
        if key.contains(AGENT_LABEL) {
            assert_eq!(plan.state(), LaunchState::Unobservable, "{key}");
            assert!(plan.consent(1, 1, true, true).is_err());
        } else {
            assert_eq!(plan.state(), LaunchState::Absent, "unrelated {key}");
        }
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert!(!f.home.join("Applications").exists());
    }
}

#[test]
fn existing_log_plist_and_bookkeeping_parents_are_admitted_before_any_payload_mutation() {
    for case in 0..7 {
        let f = Fixture::new(false);
        let logs = f.home.join("Library/Logs/Crosspane");
        let path = if case == 4 {
            launch_plist(&f).parent().unwrap().to_owned()
        } else if case == 5 {
            f.io.target().installer_dir()
        } else {
            logs.clone()
        };
        match case {
            0 | 4 => bytes(&path, b"not-directory", 0o600),
            1 | 5 => symlink_owned(&f.source, &path),
            _ => {
                directory(&path);
                if case == 2 {
                    chmod_owned(&path, 0o722);
                }
            }
        }
        let changed = path.clone();
        let io = if case == 3 {
            f.hooked(Arc::new(move |stage, p, identity| {
                if stage == "walk" && p == changed {
                    let mut id = identity.unwrap();
                    id.uid += 1;
                    Ok(Some(id))
                } else {
                    Ok(identity)
                }
            }))
        } else {
            f.io.clone()
        };
        let mut a = MacLaunchAgent::admit(
            io,
            inventory(),
            Arc::new(ApprovalFixture(Approval::Unknown)),
            &f.deadline(),
        )
        .unwrap();
        if case == 6 {
            let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
            let consent = plan.consent(1, 1, false, false).unwrap();
            rename_owned(&logs, &f.home.join("retained-logs"));
            symlink_owned(&f.source, &logs);
            assert!(a.execute(plan, consent, &f.deadline()).is_err());
        } else {
            assert!(a.plan(1, 1, None, &f.deadline()).is_err(), "{case}");
        }
        assert!(!f.home.join("Applications").exists());
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}

#[test]
fn superseded_waiting_resume_preserves_pending_receipt_and_performs_no_io() {
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 1;
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let io = f.hooked(Arc::new(move |kind, _, identity| {
        captured.lock().unwrap().push(kind.to_owned());
        Ok(identity)
    }));
    let mut a = MacLaunchAgent::admit(
        io.clone(),
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    bytes(
        &launch_plist(&f),
        &render_plist(f.io.target()).unwrap(),
        0o600,
    );
    let selected = f.selected_on(io);
    let plan = a
        .plan(
            1,
            1,
            Some((&selected, &f.reply(&f.status(1), 1))),
            &f.deadline(),
        )
        .unwrap();
    let consent = plan.consent(1, 1, true, true).unwrap();
    let mut pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert_eq!(pending.phase(), LaunchPhase::WaitingForCleanStop);
    let _new = a
        .plan(
            2,
            2,
            Some((&selected, &f.reply(&f.status(1), 2))),
            &f.deadline(),
        )
        .unwrap();
    f.exit(true, "restored");
    f.runner.behavior.lock().unwrap().job_pid = 0;
    let record = f.io.target().installer_dir().join("launch-agent.json");
    let receipt = read_owned(&record);
    let identity = owned_stat(&record);
    let phase = pending.phase();
    let error = pending.error();
    let calls = f.runner.native_calls.lock().unwrap().len();
    let signatures = f.signatures.calls.lock().unwrap().len();
    events.lock().unwrap().clear();
    assert!(matches!(
        a.resume_clean_stop(&mut pending, &f.deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(f.runner.native_calls.lock().unwrap().len(), calls);
    assert_eq!(f.signatures.calls.lock().unwrap().len(), signatures);
    assert_eq!(pending.phase(), phase);
    assert_eq!(pending.error(), error);
    assert!(same_inode(&identity, &owned_stat(&record)));
    assert_eq!(read_owned(&record), receipt);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 0);
    assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 0));
}

#[test]
fn six_second_consent_refreshes_admission_without_rebinding_the_plan() {
    for installed in [false, true] {
        let f = Fixture::new(installed);
        let mut a = adapter(&f, Approval::Allowed);
        let selected = installed.then(|| f.selected());
        let reply = f.reply(&f.status(1), 1);
        let plan = a
            .plan(1, 1, selected.as_ref().map(|s| (s, &reply)), &f.deadline())
            .unwrap();
        let consent = plan.consent(1, 1, true, true).unwrap();
        f.clock.0.store(6000, Ordering::Release);
        let pending = a.execute(plan, consent, &f.deadline()).unwrap();
        assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
        assert_eq!(pending.error(), None);
        assert_eq!(f.runner.count("bootout"), usize::from(installed));
        assert_eq!(f.runner.count("bootstrap"), 1);
    }
}

#[test]
fn six_second_clean_stop_refreshes_admission_and_uses_one_bootout() {
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 1;
    let mut a = adapter(&f, Approval::Allowed);
    let mut pending = replace_existing(&f, &mut a);
    f.clock.0.store(6000, Ordering::Release);
    f.exit(true, "restored");
    f.runner.behavior.lock().unwrap().job_pid = 0;
    a.resume_clean_stop(&mut pending, &f.deadline()).unwrap();
    assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
    assert_eq!(pending.error(), None);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 1);
}

#[test]
fn refreshed_signature_identity_drift_is_foreign_before_any_mutation() {
    for installed in [false, true] {
        let f = Fixture::new(installed);
        let mut a = adapter(&f, Approval::Allowed);
        let selected = installed.then(|| f.selected());
        let reply = f.reply(&f.status(1), 1);
        let plan = a
            .plan(1, 1, selected.as_ref().map(|s| (s, &reply)), &f.deadline())
            .unwrap();
        let consent = plan.consent(1, 1, true, true).unwrap();
        let path = if installed {
            f.io.target().agent_path()
        } else {
            f.source.join("Crosspane.app/Contents/MacOS/Crosspane")
        };
        bytes(&path, &macho(false, 2), 0o755);
        f.clock.0.store(6000, Ordering::Release);
        assert!(matches!(
            a.execute(plan, consent, &f.deadline()),
            Err(NativeError::Foreign)
        ));
        assert_eq!(f.runner.count("bootout"), 0);
        assert_eq!(f.runner.count("bootstrap"), 0);
        assert!(!f.io.target().installer_dir().exists());
        assert!(!launch_plist(&f).exists());
    }
}

#[test]
fn unknown_bootout_reconciles_only_original_clean_exit_without_repeating_stop() {
    for outcome in [0, 1, 2, 3] {
        let f = Fixture::new(true);
        f.runner.behavior.lock().unwrap().bootout = 4;
        let mut a = adapter(&f, Approval::Allowed);
        let mut pending = replace_existing(&f, &mut a);
        assert_eq!(pending.phase(), LaunchPhase::Unknown);
        assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
        let record = f.io.target().installer_dir().join("launch-agent.json");
        let receipt = read_owned(&record);
        let persisted: Value = serde_json::from_slice(&receipt).unwrap();
        assert_eq!(persisted["stop_attempted"], true);
        if outcome != 1 {
            f.exit(true, "restored");
            f.runner.behavior.lock().unwrap().job_pid = 0;
        }
        if outcome == 2 {
            let path = f.io.target().state_dir().join("last_exit.json");
            let mut value: Value = serde_json::from_slice(&read_owned(&path)).unwrap();
            value["instance_id"] = json!(9);
            bytes(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        }
        let deadline = f.deadline();
        if outcome == 3 {
            f.clock.0.store(5000, Ordering::Release);
        }
        let result = a.resume_clean_stop(&mut pending, &deadline);
        if outcome == 0 {
            result.unwrap();
            assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
            assert_eq!(pending.error(), None);
            assert_eq!(f.runner.count("bootstrap"), 1);
            assert!(
                f.home
                    .join("Applications/.Crosspane.app.crosspane-previous")
                    .exists()
            );
            assert_eq!(
                read_owned(pending.retained_prior().unwrap()),
                render_plist(f.io.target()).unwrap()
            );
        } else {
            assert_eq!(
                result,
                Err(match outcome {
                    1 => NativeError::Unavailable,
                    2 => NativeError::Foreign,
                    _ => NativeError::Timeout,
                })
            );
            assert_eq!(pending.phase(), LaunchPhase::Unknown);
            assert_eq!(pending.error(), Some(NativeError::OutcomeUnknown));
            assert_eq!(read_owned(&record), receipt);
            assert_eq!(f.runner.count("bootstrap"), 0);
            assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 0));
            assert_eq!(
                read_owned(&launch_plist(&f)),
                render_plist(f.io.target()).unwrap()
            );
        }
        assert_eq!(f.runner.count("bootout"), 1);
    }
}

#[test]
fn clean_stop_resume_refuses_changed_original_main_without_mutating_pending_state() {
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 1;
    let mut a = adapter(&f, Approval::Allowed);
    let mut pending = replace_existing(&f, &mut a);
    f.exit(true, "restored");
    f.runner.behavior.lock().unwrap().job_pid = 0;
    bytes(&f.io.target().agent_path(), &macho(false, 2), 0o755);
    f.clock.0.store(6000, Ordering::Release);
    let record = f.io.target().installer_dir().join("launch-agent.json");
    let receipt = read_owned(&record);
    let calls = f.runner.native_calls.lock().unwrap().len();
    assert_eq!(
        a.resume_clean_stop(&mut pending, &f.deadline()),
        Err(NativeError::Foreign)
    );
    assert_eq!(pending.phase(), LaunchPhase::WaitingForCleanStop);
    assert_eq!(pending.error(), None);
    assert_eq!(read_owned(&record), receipt);
    assert_eq!(f.runner.native_calls.lock().unwrap().len(), calls);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 0);
    assert!(
        !f.home
            .join("Applications/.Crosspane.app.crosspane-previous")
            .exists()
    );
}

#[test]
fn prebootstrap_payload_readmission_refuses_source_drift_and_retains_all_prior_state() {
    let f = Fixture::new(true);
    let fired = Arc::new(AtomicBool::new(false));
    let observed = fired.clone();
    let source_main = f.source.join("Crosspane.app/Contents/MacOS/Crosspane");
    let io = f.hooked(Arc::new(move |kind, path, identity| {
        if kind == "dispatch" && path == Path::new("/usr/bin/plutil") {
            bytes(&source_main, &macho(false, 2), 0o755);
            observed.store(true, Ordering::Release);
        }
        Ok(identity)
    }));
    let mut a = MacLaunchAgent::admit(
        io.clone(),
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    bytes(
        &launch_plist(&f),
        &render_plist(f.io.target()).unwrap(),
        0o600,
    );
    let selected = f.selected_on(io);
    let plan = a
        .plan(
            1,
            1,
            Some((&selected, &f.reply(&f.status(1), 1))),
            &f.deadline(),
        )
        .unwrap();
    let consent = plan.consent(1, 1, true, true).unwrap();
    let pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert!(fired.load(Ordering::Acquire));
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::Foreign));
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 0);
    assert_eq!(
        read_owned(pending.retained_prior().unwrap()),
        render_plist(f.io.target()).unwrap()
    );
    assert_eq!(
        read_owned(
            &f.home
                .join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane")
        ),
        macho(false, 0)
    );
    assert_eq!(
        read_owned(&f.home.join(".local/bin/.crosspanectl.crosspane-previous")),
        macho(false, 0)
    );
    assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 1));
}

fn is_mutation_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "mkdir"
            | "lock-open"
            | "create-temp"
            | "write"
            | "file-sync"
            | "publish"
            | "parent-sync"
            | "unlink"
            | "complete"
    )
}
fn traced_native(
    f: &Fixture,
    support: Arc<dyn SupportProbe>,
    signatures: Arc<dyn SignatureProbe>,
    hook: TestHook,
) -> Arc<MacNativeIo> {
    let mut target = f.io.target().clone();
    target.test_hook = Some(hook);
    Arc::new(
        MacNativeIo::new(
            target,
            f.runner.clone(),
            support,
            signatures,
            f.clock.clone(),
        )
        .unwrap(),
    )
}
struct SwitchingSession {
    inner: Arc<Support>,
    armed: Arc<AtomicBool>,
    observations: AtomicU64,
    switch_at: u64,
}
impl SupportProbe for SwitchingSession {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        let mut facts = self.inner.observe(deadline)?;
        if self.armed.load(Ordering::Acquire)
            && self.observations.fetch_add(1, Ordering::AcqRel) + 1 >= self.switch_at
        {
            facts.gui.console_session = "different-supported-Aqua".into();
            facts.gui.interactive_session = facts.gui.console_session.clone();
        }
        Ok(facts)
    }
}
type SignatureCallback = Arc<dyn Fn(&Path, &mut SignatureObservation) + Send + Sync>;
struct ObservedSignatures {
    inner: Arc<Signatures>,
    callback: SignatureCallback,
}
impl SignatureProbe for ObservedSignatures {
    fn observe(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        let mut observation = self.inner.observe(path, approved, deadline)?;
        (self.callback)(path, &mut observation);
        Ok(observation)
    }
}

#[test]
fn refreshed_support_correlates_planned_session_after_admission_before_any_mutation() {
    for switch_at in [3, 4] {
        let f = Fixture::new(false);
        let armed = Arc::new(AtomicBool::new(false));
        let probe = Arc::new(SwitchingSession {
            inner: f.support.clone(),
            armed: armed.clone(),
            observations: AtomicU64::new(0),
            switch_at,
        });
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let captured = mutations.clone();
        let io = traced_native(
            &f,
            probe.clone(),
            f.signatures.clone(),
            Arc::new(move |kind, _, identity| {
                if is_mutation_boundary(kind) {
                    captured.lock().unwrap().push(kind.to_owned());
                }
                Ok(identity)
            }),
        );
        let mut a = MacLaunchAgent::admit(
            io,
            inventory(),
            Arc::new(ApprovalFixture(Approval::Allowed)),
            &f.deadline(),
        )
        .unwrap();
        let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
        let consent = plan.consent(1, 1, false, false).unwrap();
        armed.store(true, Ordering::Release);
        assert!(matches!(
            a.execute(plan, consent, &f.deadline()),
            Err(NativeError::Foreign | NativeError::Unsupported)
        ));
        assert!(probe.observations.load(Ordering::Acquire) >= switch_at);
        assert!(mutations.lock().unwrap().is_empty());
        assert!(!f.io.target().installer_dir().exists());
        assert!(!launch_plist(&f).exists());
        assert_eq!(f.runner.count("bootout"), 0);
        assert_eq!(f.runner.count("bootstrap"), 0);
    }
}

#[test]
fn equal_paths_independent_target_nonce_refreshes_original_instance_through_selected_io() {
    let f = Fixture::new(true);
    let target = MacTarget::scratch(f.io.target().paths().clone()).unwrap();
    let selected_io = Arc::new(
        MacNativeIo::new(
            target,
            f.runner.clone(),
            f.support.clone(),
            f.signatures.clone(),
            f.clock.clone(),
        )
        .unwrap(),
    );
    let selected = f.selected_on(selected_io);
    assert_eq!(selected.io.target().paths(), f.io.target().paths());
    let mut a = adapter(&f, Approval::Allowed);
    bytes(
        &launch_plist(&f),
        &render_plist(f.io.target()).unwrap(),
        0o600,
    );
    let plan = a
        .plan(
            1,
            1,
            Some((&selected, &f.reply(&f.status(1), 1))),
            &f.deadline(),
        )
        .unwrap();
    let consent = plan.consent(1, 1, true, true).unwrap();
    f.clock.0.store(6000, Ordering::Release);
    let mut pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert_eq!(pending.phase(), LaunchPhase::BootstrapRequested);
    assert_eq!(pending.error(), None);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 1);
    let facts = finish(&f, &a, &mut pending, 2, 1);
    assert!(facts.payload_verified.is_some());
}

#[test]
fn installed_main_appearing_after_absent_plan_is_foreign_without_bookkeeping_mutation() {
    let f = Fixture::new(false);
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let captured = mutations.clone();
    let io = traced_native(
        &f,
        f.support.clone(),
        f.signatures.clone(),
        Arc::new(move |kind, _, identity| {
            if is_mutation_boundary(kind) {
                captured.lock().unwrap().push(kind.to_owned());
            }
            Ok(identity)
        }),
    );
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let plan = a.plan(1, 1, None, &f.deadline()).unwrap();
    let consent = plan.consent(1, 1, false, false).unwrap();
    bytes(&f.io.target().agent_path(), &macho(false, 0), 0o755);
    let identity = f.io.metadata(&f.io.target().agent_path()).unwrap();
    f.clock.0.store(6000, Ordering::Release);
    assert!(matches!(
        a.execute(plan, consent, &f.deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        f.io.metadata(&f.io.target().agent_path()).unwrap(),
        identity
    );
    assert!(!f.io.target().installer_dir().exists());
    assert!(!launch_plist(&f).exists());
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 0);
}

#[test]
fn prebootstrap_helper_admission_refusal_keeps_latest_durable_receipt_and_writes_nothing_afterward()
{
    let f = Fixture::new(false);
    let source_main = f.source.join("Crosspane.app/Contents/MacOS/Crosspane");
    let original_main = f.io.metadata(&source_main).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(None));
    let callback_path = source_main.clone();
    let record = f.io.target().installer_dir().join("launch-agent.json");
    let record_snapshot = record.clone();
    let triggered = armed.clone();
    let observed = captured.clone();
    let callback_events = events.clone();
    let signatures = Arc::new(ObservedSignatures {
        inner: f.signatures.clone(),
        callback: Arc::new(move |path, _| {
            if path == callback_path && triggered.load(Ordering::Acquire) {
                let bytes = read_owned(&record_snapshot);
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["phase"], "Published");
                *observed.lock().unwrap() = Some((bytes, callback_events.lock().unwrap().len()));
            }
        }),
    });
    let source_helper = f.source.join("Crosspane.app/Contents/MacOS/crosspane-ui");
    let hook_events = events.clone();
    let triggered = armed.clone();
    let io = traced_native(
        &f,
        f.support.clone(),
        signatures,
        Arc::new(move |kind, path, identity| {
            if is_mutation_boundary(kind) {
                hook_events.lock().unwrap().push(kind.to_owned());
            }
            if kind == "dispatch" && path == Path::new("/usr/bin/plutil") {
                bytes(&source_helper, &macho(false, 2), 0o755);
                triggered.store(true, Ordering::Release);
            }
            Ok(identity)
        }),
    );
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let pending = execute_new(&f, &mut a);
    assert!(armed.load(Ordering::Acquire));
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::Foreign));
    let (latest, count) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(read_owned(&record), latest);
    assert_eq!(events.lock().unwrap().len(), count);
    assert_eq!(f.io.metadata(&source_main).unwrap(), original_main);
    assert_eq!(f.runner.count("bootstrap"), 0);
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 1));
    let payload: Value = serde_json::from_slice(&read_owned(
        &f.io.target().installer_dir().join("payload.json"),
    ))
    .unwrap();
    assert_eq!(payload["phase"], "Published");
}

#[test]
fn valid_refreshed_installed_signature_observation_changes_without_file_drift_are_foreign() {
    let f = Fixture::new(true);
    let armed = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let triggered = armed.clone();
    let reached = observed.clone();
    let path = f.io.target().agent_path();
    let signatures = Arc::new(ObservedSignatures {
        inner: f.signatures.clone(),
        callback: Arc::new(move |candidate, value| {
            if candidate == path && triggered.load(Ordering::Acquire) {
                value.team_identifier = "OTHER12345".into();
                assert!(
                    value.strict_verified
                        && value.apple_development
                        && value.hardened_runtime
                        && !value.ad_hoc
                );
                reached.store(true, Ordering::Release);
            }
        }),
    });
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let traced = mutations.clone();
    let io = traced_native(
        &f,
        f.support.clone(),
        signatures,
        Arc::new(move |kind, _, identity| {
            if is_mutation_boundary(kind) {
                traced.lock().unwrap().push(kind.to_owned());
            }
            Ok(identity)
        }),
    );
    let selected = f.selected_on(io.clone());
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let plan = a
        .plan(
            1,
            1,
            Some((&selected, &f.reply(&f.status(1), 1))),
            &f.deadline(),
        )
        .unwrap();
    let consent = plan.consent(1, 1, true, true).unwrap();
    let identity = f.io.metadata(&f.io.target().agent_path()).unwrap();
    armed.store(true, Ordering::Release);
    assert!(matches!(
        a.execute(plan, consent, &f.deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(observed.load(Ordering::Acquire));
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        f.io.metadata(&f.io.target().agent_path()).unwrap(),
        identity
    );
    assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 0));
    assert!(!f.io.target().installer_dir().exists());
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 0);
}

#[test]
fn postinstall_main_admission_session_change_refuses_all_subsequent_mutations() {
    let f = Fixture::new(false);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(None));
    let record = f.io.target().installer_dir().join("launch-agent.json");
    let installed_main = f.io.target().agent_path();
    let support = f.support.clone();
    let callback_record = record.clone();
    let payload_record = f.io.target().installer_dir().join("payload.json");
    let callback_events = events.clone();
    let callback_capture = captured.clone();
    let signatures = Arc::new(ObservedSignatures {
        inner: f.signatures.clone(),
        callback: Arc::new(move |path, _| {
            if path == installed_main && callback_capture.lock().unwrap().is_none() {
                let payload: Value = serde_json::from_slice(&read_owned(&payload_record)).unwrap();
                if payload["phase"] != "Published" {
                    return;
                }
                let receipt = read_owned(&callback_record);
                let value: Value = serde_json::from_slice(&receipt).unwrap();
                assert_eq!(value["phase"], "Intent");
                *callback_capture.lock().unwrap() =
                    Some((receipt, callback_events.lock().unwrap().len()));
                let mut facts = support.observation.lock().unwrap();
                facts.gui.console_session = "different-supported-Aqua".into();
                facts.gui.interactive_session = facts.gui.console_session.clone();
            }
        }),
    });
    let hook_events = events.clone();
    let io = traced_native(
        &f,
        f.support.clone(),
        signatures,
        Arc::new(move |kind, _, identity| {
            if is_mutation_boundary(kind) {
                hook_events.lock().unwrap().push(kind.to_owned());
            }
            Ok(identity)
        }),
    );
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let pending = execute_new(&f, &mut a);
    let (latest, count) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::Foreign));
    assert_eq!(events.lock().unwrap().len(), count);
    assert_eq!(read_owned(&record), latest);
    assert!(!launch_plist(&f).exists());
    assert!(!f.home.join("Library/Logs/Crosspane").exists());
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 0);
    assert_eq!(read_owned(&f.io.target().agent_path()), macho(false, 1));
    let payload: Value = serde_json::from_slice(&read_owned(
        &f.io.target().installer_dir().join("payload.json"),
    ))
    .unwrap();
    assert_eq!(payload["phase"], "Published");
}

#[test]
fn matching_prebootstrap_helper_refusal_preserves_published_receipt_without_any_later_write() {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Allowed);
    let mut first_pending = execute_new(&f, &mut first);
    assert!(
        finish(&f, &first, &mut first_pending, 2, 1)
            .payload_verified
            .is_some()
    );
    let source_main = f.source.join("Crosspane.app/Contents/MacOS/Crosspane");
    let original_source = f.io.metadata(&source_main).unwrap();
    let original_installed = f.io.metadata(&f.io.target().agent_path()).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::new(Mutex::new(None));
    let record = f.io.target().installer_dir().join("launch-agent.json");
    let callback_record = record.clone();
    let callback_main = source_main.clone();
    let callback_armed = armed.clone();
    let callback_events = events.clone();
    let callback_capture = captured.clone();
    let signatures = Arc::new(ObservedSignatures {
        inner: f.signatures.clone(),
        callback: Arc::new(move |path, _| {
            if path == callback_main && callback_armed.load(Ordering::Acquire) {
                let receipt = read_owned(&callback_record);
                let value: Value = serde_json::from_slice(&receipt).unwrap();
                assert_eq!(value["phase"], "Published");
                *callback_capture.lock().unwrap() =
                    Some((receipt, callback_events.lock().unwrap().len()));
            }
        }),
    });
    let helper = f.source.join("Crosspane.app/Contents/MacOS/crosspane-ui");
    let hook_events = events.clone();
    let hook_armed = armed.clone();
    let io = traced_native(
        &f,
        f.support.clone(),
        signatures,
        Arc::new(move |kind, path, identity| {
            if is_mutation_boundary(kind) {
                hook_events.lock().unwrap().push(kind.to_owned());
            }
            if kind == "dispatch" && path == Path::new("/usr/bin/plutil") {
                bytes(&helper, &macho(false, 2), 0o755);
                hook_armed.store(true, Ordering::Release);
            }
            Ok(identity)
        }),
    );
    let selected = f.selected_on(io.clone());
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let plan = a
        .plan(
            2,
            2,
            Some((&selected, &f.reply(&f.status(2), 2))),
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(plan.state(), LaunchState::Owned);
    let consent = plan.consent(2, 2, false, false).unwrap();
    let pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert!(armed.load(Ordering::Acquire));
    assert_eq!(pending.phase(), LaunchPhase::Unknown);
    assert_eq!(pending.error(), Some(NativeError::Foreign));
    let (latest, count) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(read_owned(&record), latest);
    assert_eq!(events.lock().unwrap().len(), count);
    assert_eq!(f.io.metadata(&source_main).unwrap(), original_source);
    assert_eq!(
        f.io.metadata(&f.io.target().agent_path()).unwrap(),
        original_installed
    );
    assert_eq!(
        read_owned(pending.retained_prior().unwrap()),
        expected_plist(f.home.to_str().unwrap())
    );
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 1);
    let payload: Value = serde_json::from_slice(&read_owned(
        &f.io.target().installer_dir().join("payload.json"),
    ))
    .unwrap();
    assert_eq!(payload["phase"], "Verified");
}

#[test]
fn owned_loaded_stopped_job_recovers_only_a_matching_payload() {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut first);
    assert!(
        finish(&f, &first, &mut pending, 2, 1)
            .payload_verified
            .is_some()
    );
    f.runner.behavior.lock().unwrap().loaded_stopped = true;
    f.runner.stopped.store(true, Ordering::Release);
    let original = f.io.metadata(&f.io.target().agent_path()).unwrap();
    let mut a = adapter(&f, Approval::Allowed);
    let plan = a.plan(2, 2, None, &f.deadline()).unwrap();
    assert_eq!(plan.state(), LaunchState::LoadedStopped);
    assert!(!plan.interrupts_agent());
    let consent = plan.consent(2, 2, false, false).unwrap();
    let pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert_eq!(pending.error(), None);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 2);
    assert_eq!(
        f.io.metadata(&f.io.target().agent_path()).unwrap(),
        original
    );
}
#[test]
fn loaded_stopped_job_cannot_authorize_payload_replacement() {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut first);
    finish(&f, &first, &mut pending, 2, 1);
    f.runner.behavior.lock().unwrap().loaded_stopped = true;
    f.runner.stopped.store(true, Ordering::Release);
    bytes(
        &f.home.join(".local/bin/crosspanectl"),
        &macho(false, 2),
        0o755,
    );
    let original = read_owned(&f.home.join(".local/bin/crosspanectl"));
    let mut a = adapter(&f, Approval::Allowed);
    assert!(matches!(
        a.plan(2, 2, None, &f.deadline()),
        Err(NativeError::Refused)
    ));
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 1);
    assert_eq!(
        read_owned(&f.home.join(".local/bin/crosspanectl")),
        original
    );
}
#[test]
fn loaded_stopped_job_change_before_bootout_refuses() {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut first);
    finish(&f, &first, &mut pending, 2, 1);
    f.runner.behavior.lock().unwrap().loaded_stopped = true;
    f.runner.stopped.store(true, Ordering::Release);
    let mut a = adapter(&f, Approval::Allowed);
    let plan = a.plan(2, 2, None, &f.deadline()).unwrap();
    let consent = plan.consent(2, 2, false, false).unwrap();
    f.runner.behavior.lock().unwrap().loaded_stopped = false;
    assert!(matches!(
        a.execute(plan, consent, &f.deadline()),
        Err(NativeError::Foreign)
    ));
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 1);
}

#[test]
fn loaded_stopped_job_change_under_install_lock_refuses_before_bootout() {
    let f = Fixture::new(false);
    let mut first = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut first);
    finish(&f, &first, &mut pending, 2, 1);
    f.runner.behavior.lock().unwrap().loaded_stopped = true;
    f.runner.stopped.store(true, Ordering::Release);
    let locks = Arc::new(AtomicU64::new(0));
    let observed_locks = locks.clone();
    let runner = f.runner.clone();
    let io = f.hooked(Arc::new(move |stage, _, identity| {
        // Execute first persists the consent intent. Change the selected job when
        // recovery takes its second lease, after the unlocked observation.
        if stage == "lock" && observed_locks.fetch_add(1, Ordering::AcqRel) == 1 {
            runner.behavior.lock().unwrap().loaded_stopped = false;
        }
        Ok(identity)
    }));
    let mut a = MacLaunchAgent::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap();
    let plan = a.plan(2, 2, None, &f.deadline()).unwrap();
    let consent = plan.consent(2, 2, false, false).unwrap();
    let pending = a.execute(plan, consent, &f.deadline()).unwrap();
    assert_eq!(pending.error(), Some(NativeError::Foreign));
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 1);
}
