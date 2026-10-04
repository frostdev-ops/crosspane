#![cfg(target_os = "macos")]
#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Explicit scratch roots, fake GUI/signature/process observations; no real native commands.
use crosspane_installer::agent_contract;
use crosspane_installer::{live, view};
#[path = "../src/platform/macos/repair/test_native.rs"]
mod native_binding;
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
#[path = "../src/platform/macos/repair.rs"]
mod repair;
use repair::*;
#[path = "../src/platform/macos/audio_package.rs"]
mod audio_package;
#[path = "../src/platform/macos/removal.rs"]
mod removal;
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
impl audio_package::AudioClock for FakeClock {
    fn unix_ms(&self) -> NativeResult<u64> {
        Ok(5000)
    }
}
struct Support {
    observation: Mutex<SupportObservation>,
    calls: AtomicU64,
}
impl SupportProbe for Support {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        self.calls.fetch_add(1, Ordering::Relaxed);
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
fn fixture_data(path: &str, role: Option<PayloadRole>, marker: u8) -> Vec<u8> {
    if role.is_some() {
        return macho(role == Some(PayloadRole::EmbeddedCode), marker);
    }
    if path.ends_with("packages.json") {
        let hex = |b: &[u8]| {
            sha(b)
                .iter()
                .map(|n| format!("{n:02x}"))
                .collect::<String>()
        };
        return format!("{{\"schema_version\":1,\"version\":\"0.1.0\",\"packages\":[{{\"kind\":\"install\",\"file\":\"CrosspaneAudio-install-0.1.0.pkg\",\"sha256\":\"{}\"}},{{\"kind\":\"remove\",\"file\":\"CrosspaneAudio-remove-0.1.0.pkg\",\"sha256\":\"{}\"}}]}}\n",hex(b"inert-install"),hex(b"inert-remove")).into_bytes();
    }
    if path.ends_with(".pkg") {
        return if path.contains("-install-") {
            b"inert-install".to_vec()
        } else {
            b"inert-remove".to_vec()
        };
    }
    b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec()
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
        ("Crosspane.app/Contents/Resources/audio/packages.json", None),
        (
            "Crosspane.app/Contents/Resources/audio/CrosspaneAudio-install-0.1.0.pkg",
            None,
        ),
        (
            "Crosspane.app/Contents/Resources/audio/CrosspaneAudio-remove-0.1.0.pkg",
            None,
        ),
        ("crosspanectl", Some(PayloadRole::Ctl)),
        ("crosspane-installer", Some(PayloadRole::Installer)),
    ];
    ApprovedInventory {
        product_version: "test-1".into(),
        features: vec!["video".into(), "private-vdisplay".into()],
        files: entries
            .into_iter()
            .map(|(path, role)| {
                let data = fixture_data(path, role, 1);
                PayloadFile {
                    path: path.into(),
                    size: data.len() as u64,
                    sha256: sha(&data),
                    mode: if role.is_some() { 0o755 } else { 0o644 },
                    signing: role.map(|role| SigningRule {
                        role,
                        identifier: if role == PayloadRole::Agent {
                            AGENT_LABEL.into()
                        } else {
                            format!("test.approved.{role:?}")
                        },
                        designated_requirement: "trusted-test-development-requirement".into(),
                        entitlements: if role == PayloadRole::Agent {
                            BTreeMap::from([("com.apple.security.device.audio-input".into(), true)])
                        } else {
                            BTreeMap::new()
                        },
                    }),
                }
            })
            .collect(),
    }
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
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let scratch = (0..16)
            .find_map(|_| {
                match Scratch::create(format!(
                    "cp-c2-r-{}-{nonce:x}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                )) {
                    Ok(root) => Some(root),
                    Err(rustix::io::Errno::EXIST) => None,
                    Err(error) => panic!("exclusive scratch: {error}"),
                }
            })
            .expect("fresh scratch name within bounded retries");
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
            let data = fixture_data(&file.path, file.signing.as_ref().map(|r| r.role), 1);
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
        let mut target = MacTarget::scratch(TargetPaths {
            uid,
            home: home.clone(),
            gui_tmpdir: tmp.clone(),
            runtime_override: None,
            payload_root: source.clone(),
        })
        .unwrap();
        let library = root.join("fake-Library");
        for relative in [
            "Audio/Plug-Ins/HAL",
            "Application Support/Crosspane/Installer",
        ] {
            directory(&library.join(relative));
        }
        let mapped = library.clone();
        target.test_path = Some(Arc::new(move |p| {
            p.strip_prefix("/Library")
                .map(|s| mapped.join(s))
                .unwrap_or_else(|_| p.to_owned())
        }));
        target.test_hook = Some(Arc::new(|stage, p, mut identity| {
            if stage.starts_with("audio-") {
                assert!(p.starts_with("/Library"));
                if let Some(id) = &mut identity {
                    id.uid = 0;
                }
            }
            Ok(identity)
        }));
        let support = Arc::new(Support {
            calls: AtomicU64::new(0),
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
    require_repair_record: bool,
    job_pid: u32,
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
        assert_eq!(spec.max_output(), 64 * 1024);
        match spec.args()[0].as_str() {
            "print" => {
                assert_eq!(spec.args(), &["print", &service]);
                assert!(!spec.is_mutation());
                if let Some(value) = &b.print_override {
                    return Ok(value.clone());
                }
                Ok(if b.job_pid == 0 {
                    output(
                        113,
                        vec![],
                        format!(
                            "Could not find service \"{AGENT_LABEL}\" in domain for user gui: {}\n",
                            self.uid
                        )
                        .into_bytes(),
                    )
                } else {
                    output(
                        0,
                        format!(
                            "{service} = {{\n path = {}\n program = {}\n pid = {}\n arguments = {{\n {}\n run\n }}\n environment = {{\n RUST_LOG => info\n }}\n}}\n",
                            plist.display(),
                            self.exe.display(),
                            b.job_pid,
                            self.exe.display()
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
                        0 => "disabled services = {\n}\n".into(),
                        1 => format!("disabled services = {{\n \"{AGENT_LABEL}\" => true\n}}\n"),
                        _ => "unobservable format\n".into(),
                    }
                    .into_bytes(),
                    vec![],
                ))
            }
            "bootout" => {
                assert_eq!(spec.args(), &["bootout", &service]);
                assert!(spec.is_mutation());
                if b.require_repair_record {
                    let repair: Value = serde_json::from_slice(&read_owned(
                        &home.join("Library/Application Support/Crosspane/Installer/repair.json"),
                    ))
                    .unwrap();
                    assert_eq!(repair["step"], "before_stop");
                    assert_eq!(repair["status_watermark"], 101);
                }
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

fn producer_fixture() -> Fixture {
    let f = Fixture::new(false);
    let mut a = adapter(&f, Approval::Allowed);
    let mut pending = execute_new(&f, &mut a);
    f.clock.0.store(100, Ordering::Release);
    let facts = finish(&f, &a, &mut pending, 2, 20);
    assert!(facts.payload_verified.is_some());
    assert_eq!(pending.phase(), LaunchPhase::Observed);
    f
}
fn repair_adapter(f: &Fixture) -> MacRepair {
    repair_on(f, f.io.clone())
}
fn repair_on(f: &Fixture, io: Arc<MacNativeIo>) -> MacRepair {
    MacRepair::admit(
        io,
        inventory(),
        Arc::new(ApprovalFixture(Approval::Allowed)),
        &f.deadline(),
    )
    .unwrap()
}
fn current(f: &Fixture, call: u64) -> (SelectedAgent, AgentReply) {
    let bootstrap = parse_bootstrap(&read_owned(&f.runtime.join("bootstrap.json"))).unwrap();
    (
        f.selected(),
        f.reply(&f.status(bootstrap.instance_id), call),
    )
}
fn repair_plan(
    f: &Fixture,
    r: &mut MacRepair,
    revision: u64,
    operation: u64,
    call: u64,
) -> RepairPlan {
    let (selected, reply) = current(f, call);
    r.plan(
        revision,
        operation,
        Some((&selected, &reply)),
        &f.deadline(),
    )
    .unwrap()
}
fn apply_repair(f: &Fixture, r: &mut MacRepair, p: RepairPlan, call: u64) -> PendingRepair {
    let c = p
        .consent(p.view_revision(), p.operation_id(), true)
        .unwrap();
    let (selected, reply) = current(f, call);
    r.apply(p, c, Some((&selected, &reply)), &f.deadline())
        .unwrap()
}
fn complete_repair(
    f: &Fixture,
    r: &mut MacRepair,
    pending: &mut PendingRepair,
    call: u64,
) -> RepairCompletion {
    r.expect_health(pending, call).unwrap();
    let (selected, reply) = current(f, call);
    r.observe(pending, &selected, reply, &f.deadline()).unwrap()
}
fn record(f: &Fixture, name: &str) -> Value {
    serde_json::from_slice(&read_owned(&f.io.target().installer_dir().join(name))).unwrap()
}
fn change_record(f: &Fixture, name: &str, change: impl FnOnce(&mut Value)) {
    let mut value = record(f, name);
    change(&mut value);
    bytes(
        &f.io.target().installer_dir().join(name),
        &serde_json::to_vec(&value).unwrap(),
        0o600,
    );
}
fn counts(f: &Fixture) -> (usize, usize, u64) {
    (
        f.runner.native_calls.lock().unwrap().len(),
        f.signatures.calls.lock().unwrap().len(),
        f.support.calls.load(Ordering::Acquire),
    )
}
fn assert_owned_origins(f: &Fixture) {
    for name in ["payload.json", "launch-agent.json"] {
        let value = record(f, name);
        for resource in value["receipt"]["resources"].as_array().unwrap() {
            assert_eq!(resource["ownership"], "Created");
            assert_eq!(resource["before"], "Absent");
            assert_eq!(resource["after"], "Matching");
        }
    }
}
#[test]
fn full_reinstall_from_genuine_producer_receipts_replaces_app_ctl_and_plist() {
    let f = producer_fixture();
    let old = [
        f.io.target().app_path(),
        f.home.join(".local/bin/crosspanectl"),
        launch_plist(&f),
    ]
    .map(|p| owned_stat(&p).st_ino);
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    assert_eq!(p.preview().effects.len(), 5);
    assert!(matches!(
        p.preview().effects[0],
        RepairEffect::StopTrackedOriginal { instance: 2, .. }
    ));
    let mut pending = apply_repair(&f, &mut r, p, 101);
    assert_eq!(pending.progress(), RepairProgress::Published);
    let now = [
        f.io.target().app_path(),
        f.home.join(".local/bin/crosspanectl"),
        launch_plist(&f),
    ]
    .map(|p| owned_stat(&p).st_ino);
    for (before, after) in old.into_iter().zip(now) {
        assert_ne!(before, after);
    }
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(
        complete_repair(&f, &mut r, &mut pending, 102).progress,
        RepairProgress::Verified
    );
    assert_owned_origins(&f);
}
#[test]
fn durable_repair_record_precedes_the_first_stop_and_survives_the_window() {
    let f = producer_fixture();
    f.runner.behavior.lock().unwrap().require_repair_record = true;
    let mut repair = repair_adapter(&f);
    let plan = repair_plan(&f, &mut repair, 1, 2, 100);
    let pending = apply_repair(&f, &mut repair, plan, 101);
    drop(pending);
    drop(repair);
    let path = f.io.target().installer_dir().join("repair.json");
    assert_eq!(owned_stat(&path).st_mode & 0o777, 0o600);
    let value = record(&f, "repair.json");
    assert_eq!(value["operation"], 2);
    assert_eq!(value["step"], "applied");
    assert_eq!(value["status_watermark"], 101);
    assert!(!value["backups"].as_array().unwrap().is_empty());
}

#[test]
fn verified_repair_retires_its_durable_record_only_after_fresh_health() {
    let f = producer_fixture();
    let mut repair = repair_adapter(&f);
    let plan = repair_plan(&f, &mut repair, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut repair, plan, 101);
    let path = f.io.target().installer_dir().join("repair.json");
    assert!(f.io.metadata(&path).unwrap().is_some());
    assert_eq!(
        complete_repair(&f, &mut repair, &mut pending, 102).progress,
        RepairProgress::Verified
    );
    assert!(f.io.metadata(&path).unwrap().is_none());
}

fn completed_with_saved_hint() -> (Fixture, Vec<u8>) {
    let f = producer_fixture();
    let mut repair = repair_adapter(&f);
    let plan = repair_plan(&f, &mut repair, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut repair, plan, 101);
    let saved = read_owned(&f.io.target().installer_dir().join("repair.json"));
    assert_eq!(
        complete_repair(&f, &mut repair, &mut pending, 102).progress,
        RepairProgress::Verified
    );
    (f, saved)
}

#[test]
fn fresh_window_reassesses_every_visible_boundary_without_replaying_any_change() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let inodes = [
        f.io.target().app_path(),
        f.home.join(".local/bin/crosspanectl"),
        launch_plist(&f),
    ]
    .map(|path| owned_stat(&path).st_ino);
    for step in [
        "before_stop",
        "stopped",
        "applying",
        "applied",
        "health_wait",
    ] {
        let mut value: Value = serde_json::from_slice(&saved).unwrap();
        value["step"] = json!(step);
        bytes(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
            .unwrap()
            .unwrap();
        assert_eq!(saved.operation_id(), 2);
        assert_eq!(saved.status_watermark(), 101);
        assert_eq!(format!("{saved:?}"), "RepairRecord");
        let mut fresh = repair_adapter(&f);
        let (selected, reply) = current(&f, 103);
        assert_eq!(
            fresh
                .resume(saved, Some((&selected, &reply)), &f.deadline())
                .unwrap(),
            RepairReassessment::CurrentInstallHealthy,
            "{step}"
        );
        assert!(f.io.metadata(&path).unwrap().is_none());
        assert_eq!(
            (f.runner.count("bootout"), f.runner.count("bootstrap")),
            mutations
        );
        assert_eq!(
            [
                f.io.target().app_path(),
                f.home.join(".local/bin/crosspanectl"),
                launch_plist(&f)
            ]
            .map(|path| owned_stat(&path).st_ino),
            inodes
        );
    }
}

#[test]
fn round_trip_keeps_plan_receipt_bindings_and_only_fixed_backup_hints() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let record = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(record.boundary(), RepairBoundary::Applied);
    assert_eq!(record.operation_id(), 2);
    assert_eq!(record.status_watermark(), 101);
    assert_eq!(record.backups().len(), 3);
    let value: Value = serde_json::from_slice(&saved).unwrap();
    for field in [
        "target",
        "inventory",
        "plan_fingerprint",
        "payload_receipt",
        "launch_receipt",
    ] {
        assert_eq!(value[field].as_array().unwrap().len(), 32);
    }
}

#[test]
fn corrupt_unreadable_foreign_or_unbounded_records_refuse_and_are_never_replaced() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    let mut variants = vec![b"not-json".to_vec(), vec![b' '; 64 * 1024 + 1]];
    for field in [
        "schema_version",
        "operation",
        "revision",
        "target",
        "inventory",
        "step",
        "backups",
        "unexpected",
    ] {
        let mut value: Value = serde_json::from_slice(&saved).unwrap();
        value[field] = match field {
            "schema_version" => json!(2),
            "operation" | "revision" => json!(0),
            "target" | "inventory" => json!(vec![0; 32]),
            "step" => json!("invented"),
            "backups" => json!([f.root.join("unrelated")]),
            _ => json!(true),
        };
        variants.push(serde_json::to_vec(&value).unwrap());
    }
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    for data in variants {
        bytes(&path, &data, 0o600);
        assert!(MacRepair::saved_record(&f.io, &inventory(), &f.deadline()).is_err());
        assert!(
            MacRepair::admit(
                f.io.clone(),
                inventory(),
                Arc::new(ApprovalFixture(Approval::Allowed)),
                &f.deadline()
            )
            .is_err()
        );
        assert_eq!(read_owned(&path), data);
        assert_eq!(
            (f.runner.count("bootout"), f.runner.count("bootstrap")),
            mutations
        );
    }
    bytes(&path, &saved, 0o400);
    assert!(MacRepair::saved_record(&f.io, &inventory(), &f.deadline()).is_err());
    chmod_owned(&path, 0o600);
    let link = f.root.join("record-alias");
    hardlink_owned(&path, &link);
    assert!(MacRepair::saved_record(&f.io, &inventory(), &f.deadline()).is_err());
    remove_owned(&link);
    remove_owned(&path);
    bytes(&link, &saved, 0o600);
    symlink_owned(&link, &path);
    assert!(MacRepair::saved_record(&f.io, &inventory(), &f.deadline()).is_err());
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn stopped_original_is_reassessed_without_minting_clean_stop_or_starting_it() {
    let f = producer_fixture();
    f.runner.behavior.lock().unwrap().bootout = 2; // Actual fake exit, missing clean receipt.
    let running = f.runner.behavior.lock().unwrap().job_pid;
    let mut original = repair_adapter(&f);
    let plan = repair_plan(&f, &mut original, 1, 2, 100);
    let pending = apply_repair(&f, &mut original, plan, 101);
    assert_eq!(pending.progress(), RepairProgress::WaitingForCleanStop);
    drop(pending);
    drop(original);
    let path = f.io.target().installer_dir().join("repair.json");
    let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(saved.boundary(), RepairBoundary::Stopped);
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let mut fresh = repair_adapter(&f);
    assert_eq!(
        fresh.resume(saved, None, &f.deadline()).unwrap(),
        RepairReassessment::AgentStopped
    );
    assert!(f.io.metadata(&path).unwrap().is_some());
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
    assert!(
        repair_adapter(&f).plan(1, 3, None, &f.deadline()).is_err(),
        "stopped app still needs the unchanged live-original gate"
    );
    // Crosspane is started again (nothing was replaced). Its launch receipt is still the
    // interrupted repair's unfinished intent, so a healthy answer alone never retires the record.
    f.runner.behavior.lock().unwrap().job_pid = running;
    f.runner.stopped.store(false, Ordering::Release);
    let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let (selected, reply) = current(&f, 103);
    assert_eq!(
        repair_adapter(&f)
            .resume(saved, Some((&selected, &reply)), &f.deadline())
            .unwrap(),
        RepairReassessment::RecoveryRetained
    );
    assert!(f.io.metadata(&path).unwrap().is_some());
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

fn interrupted_after_publication(f: &Fixture) -> (Vec<u8>, Vec<PathBuf>) {
    let mut repair = repair_adapter(f);
    let plan = repair_plan(f, &mut repair, 1, 2, 100);
    let pending = apply_repair(f, &mut repair, plan, 101);
    assert_eq!(pending.progress(), RepairProgress::Published);
    // The window dies during the health wait: no genuine pending token survives.
    drop(pending);
    drop(repair);
    let path = f.io.target().installer_dir().join("repair.json");
    let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let backups = saved.backups().to_vec();
    assert!(!backups.is_empty());
    (read_owned(&path), backups)
}

#[test]
fn interrupted_health_wait_stays_unverified_while_the_payload_receipt_is_unfinished() {
    let f = producer_fixture();
    let (original, backups) = interrupted_after_publication(&f);
    let path = f.io.target().installer_dir().join("repair.json");
    let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let (selected, reply) = current(&f, 103);
    // A healthy answer alone never verifies: only the genuine pending token could have finished
    // the payload receipt, so the record and every recovery copy stay.
    assert_eq!(
        repair_adapter(&f)
            .resume(saved, Some((&selected, &reply)), &f.deadline())
            .unwrap(),
        RepairReassessment::RecoveryRetained
    );
    assert_eq!(read_owned(&path), original);
    for backup in backups {
        assert!(f.io.metadata(&backup).unwrap().is_some(), "{backup:?}");
    }
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn a_running_job_that_did_not_answer_is_unverifiable_and_never_treated_as_stopped() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let record = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(
        repair_adapter(&f)
            .resume(record, None, &f.deadline())
            .unwrap(),
        RepairReassessment::RecoveryRetained
    );
    assert_eq!(read_owned(&path), saved);
}

#[test]
fn partial_publication_and_unverified_health_keep_all_material_without_replay() {
    let f = producer_fixture();
    let (original, backups) = interrupted_after_publication(&f);
    let path = f.io.target().installer_dir().join("repair.json");
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    // A partially present payload is unverifiable even with a healthy answer.
    remove_owned(&f.home.join(".local/bin/crosspanectl"));
    let saved = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let (selected, reply) = current(&f, 103);
    assert_eq!(
        repair_adapter(&f)
            .resume(saved, Some((&selected, &reply)), &f.deadline())
            .unwrap(),
        RepairReassessment::RecoveryRetained
    );
    assert_eq!(read_owned(&path), original);
    for backup in backups {
        assert!(f.io.metadata(&backup).unwrap().is_some());
    }
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn an_earlier_record_blocks_a_new_apply_and_is_never_overwritten() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let identity = f.io.metadata(&path).unwrap();
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let mut repair = repair_adapter(&f);
    let plan = repair_plan(&f, &mut repair, 1, 3, 103);
    let consent = plan.consent(1, 3, true).unwrap();
    let (selected, reply) = current(&f, 104);
    assert_eq!(
        repair
            .apply(plan, consent, Some((&selected, &reply)), &f.deadline())
            .unwrap_err(),
        NativeError::Refused
    );
    assert_eq!(read_owned(&path), saved);
    assert_eq!(f.io.metadata(&path).unwrap(), identity);
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn resumed_status_watermark_rejects_old_ids_and_records_failed_observation_reservations() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    MacRepair::reserve_status(&f.io, &inventory(), 500, &f.deadline()).unwrap();
    assert_eq!(
        MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
            .unwrap()
            .unwrap()
            .status_watermark(),
        500
    );
    assert!(MacRepair::reserve_status(&f.io, &inventory(), 500, &f.deadline()).is_err());
    let mut fresh = repair_adapter(&f);
    let record = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let (selected, old) = current(&f, 500);
    assert_eq!(
        fresh
            .resume(record, Some((&selected, &old)), &f.deadline())
            .unwrap_err(),
        NativeError::Foreign
    );
    let mut fresh = repair_adapter(&f);
    let record = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let (_, new) = current(&f, 501);
    assert_eq!(
        fresh
            .resume(record, Some((&selected, &new)), &f.deadline())
            .unwrap(),
        RepairReassessment::CurrentInstallHealthy
    );
}

#[test]
fn record_flush_failure_before_stop_refuses_without_mutation() {
    let f = producer_fixture();
    let record = f.io.target().installer_dir().join("repair.json");
    let io = f.hooked(Arc::new(move |stage, path, identity| {
        if stage == "file-sync" && path == record {
            Err(NativeError::Unavailable)
        } else {
            Ok(identity)
        }
    }));
    let mut repair = repair_on(&f, io);
    let plan = repair_plan(&f, &mut repair, 1, 2, 100);
    let consent = plan.consent(1, 2, true).unwrap();
    let (selected, reply) = current(&f, 101);
    assert!(
        repair
            .apply(plan, consent, Some((&selected, &reply)), &f.deadline())
            .is_err()
    );
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(f.runner.count("bootstrap"), 1); // Original fixture install only.
}

#[test]
fn post_dispatch_record_failure_keeps_the_genuine_pending_repair_and_the_pre_dispatch_hint() {
    let f = producer_fixture();
    let record = f.io.target().installer_dir().join("repair.json");
    let writes = Arc::new(AtomicU64::new(0));
    let count = writes.clone();
    let io = f.hooked(Arc::new(move |stage, path, identity| {
        if stage == "file-sync" && path == record && count.fetch_add(1, Ordering::Relaxed) > 0 {
            Err(NativeError::Unavailable)
        } else {
            Ok(identity)
        }
    }));
    let mut repair = repair_on(&f, io);
    let plan = repair_plan(&f, &mut repair, 1, 2, 100);
    let consent = plan.consent(1, 2, true).unwrap();
    let (selected, reply) = current(&f, 101);
    // The boundary after dispatch is only a hint: losing it never drops the same-window repair.
    let pending = repair
        .apply(plan, consent, Some((&selected, &reply)), &f.deadline())
        .unwrap();
    assert_eq!(pending.progress(), RepairProgress::Published);
    assert_eq!(f.runner.count("bootout"), 1);
    assert!(
        writes.load(Ordering::Relaxed) > 1,
        "the post-dispatch update was attempted"
    );
    let hint = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(hint.boundary(), RepairBoundary::BeforeStop);
    assert!(
        f.io.metadata(
            &f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn changed_record_and_different_context_do_not_resume_or_delete_anything() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let captured = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let mut fresh = repair_adapter(&f);
    change_record(&f, "repair.json", |value| value["operation"] = json!(99));
    let changed = read_owned(&path);
    let (selected, reply) = current(&f, 103);
    assert_eq!(
        fresh
            .resume(captured, Some((&selected, &reply)), &f.deadline())
            .unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(read_owned(&path), changed);
    bytes(&path, &saved, 0o600);
    let captured = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let other = producer_fixture();
    assert_eq!(
        repair_adapter(&other)
            .resume(captured, None, &other.deadline())
            .unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(read_owned(&path), saved);
    assert_eq!(other.runner.count("bootout"), 0);
}

#[test]
fn fallback_identity_recovery_pending_and_incompatible_health_never_retire_saved_record() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    let (selected, _) = current(&f, 103);
    for mutation in [
        "keystore",
        "recovery_pending",
        "startup_recovery",
        "version",
        "features",
    ] {
        bytes(&path, &saved, 0o600);
        let mut value = f.status(selected.instance.bootstrap().instance_id);
        let health = &mut value["result"]["installer"];
        match mutation {
            "keystore" => health["keystore"] = json!("file"),
            "recovery_pending" => health["recovery_pending"] = json!(1),
            "startup_recovery" => health["startup_recovery"] = json!("failed"),
            "version" => health["build"]["version"] = json!("different-version"),
            "features" => health["build"]["features"] = json!([]),
            _ => unreachable!(),
        }
        let reply = f.reply(&value, 103);
        let record = MacRepair::saved_record(&f.io, &inventory(), &f.deadline())
            .unwrap()
            .unwrap();
        assert_eq!(
            repair_adapter(&f)
                .resume(record, Some((&selected, &reply)), &f.deadline())
                .unwrap(),
            RepairReassessment::RecoveryRetained,
            "{mutation}"
        );
        assert_eq!(read_owned(&path), saved);
    }
}

#[test]
fn healthy_current_install_reports_failed_record_retirement_without_claiming_a_new_repair() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let target = path.clone();
    let io = f.hooked(Arc::new(move |stage, path, identity| {
        if stage == "quarantine" && path == target {
            Err(NativeError::Unavailable)
        } else {
            Ok(identity)
        }
    }));
    let record = MacRepair::saved_record(&io, &inventory(), &f.deadline())
        .unwrap()
        .unwrap();
    let (_, reply) = current(&f, 103);
    // Retirement uses the fresh selected I/O, so that context must carry the same injected failure.
    let selected = f.selected_on(io.clone());
    let mut repair = repair_on(&f, io);
    assert_eq!(
        repair
            .resume(record, Some((&selected, &reply)), &f.deadline())
            .unwrap(),
        RepairReassessment::CurrentInstallHealthyCleanupIncomplete
    );
    assert_eq!(read_owned(&path), saved);
}

#[test]
fn removal_classifies_only_a_strict_record_and_keeps_existing_clean_and_consent_gates() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    let observe = || {
        removal::MacRemovalObserver::admit(
            f.io.clone(),
            inventory(),
            f.source.join("Crosspane.app/Contents/Resources/audio"),
            f.clock.clone(),
            &f.deadline(),
        )
        .unwrap()
    };
    bytes(&path, &saved, 0o600);
    let mut removal = removal::MacRemoval::new(observe());
    let plan = removal
        .plan(
            1,
            crosspane_installer_core::OperationId(3),
            removal::RemovalChoices {
                delete_identity: false,
                remove_driver: false,
            },
            Some(current(&f, 103)),
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(
        plan.preview()
            .deltas
            .iter()
            .find(|row| row.resource == "mac.repair-record")
            .unwrap()
            .effect,
        removal::RemovalEffect::RemoveOwnedAfterVerification
    );
    assert_eq!(
        read_owned(&path),
        saved,
        "a preview is never deletion authority"
    );
    drop(plan);
    drop(removal);
    f.runner.behavior.lock().unwrap().job_pid = 0;
    f.runner.stopped.store(true, Ordering::Release);
    let mut removal = removal::MacRemoval::new(observe());
    let plan = removal
        .plan(
            1,
            crosspane_installer_core::OperationId(3),
            removal::RemovalChoices {
                delete_identity: false,
                remove_driver: false,
            },
            None,
            &f.deadline(),
        )
        .unwrap();
    assert_eq!(
        plan.preview()
            .deltas
            .iter()
            .find(|row| row.resource == "mac.repair-record")
            .unwrap()
            .effect,
        removal::RemovalEffect::KeepRecovery
    );
    drop(plan);
    drop(removal);
    bytes(&path, b"corrupt", 0o600);
    let mut removal = removal::MacRemoval::new(observe());
    let plan = removal
        .plan(
            1,
            crosspane_installer_core::OperationId(3),
            removal::RemovalChoices {
                delete_identity: false,
                remove_driver: false,
            },
            None,
            &f.deadline(),
        )
        .unwrap();
    assert!(
        !plan
            .preview()
            .deltas
            .iter()
            .any(|row| row.resource == "mac.repair-record")
    );
    assert_eq!(
        plan.preview()
            .deltas
            .iter()
            .find(|row| row.path.as_ref() == Some(&path))
            .unwrap()
            .effect,
        removal::RemovalEffect::KeepRecovery
    );
    assert_eq!(read_owned(&path), b"corrupt");
}
#[test]
fn missing_owned_ctl_ui_and_data_are_recreated_by_full_reinstall() {
    for relative in [
        ".local/bin/crosspanectl",
        "Applications/Crosspane.app/Contents/MacOS/crosspane-ui",
        "Applications/Crosspane.app/Contents/Info.plist",
    ] {
        let f = producer_fixture();
        remove_owned(&f.home.join(relative));
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let mut pending = apply_repair(&f, &mut r, p, 101);
        assert_eq!(pending.progress(), RepairProgress::Published);
        assert!(f.home.join(relative).exists());
        assert_eq!(
            complete_repair(&f, &mut r, &mut pending, 102).progress,
            RepairProgress::Verified
        );
        assert_owned_origins(&f);
    }
}
#[test]
fn successful_repair_preserves_origins_for_a_second_repair() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut r, p, 101);
    complete_repair(&f, &mut r, &mut pending, 102);
    assert_owned_origins(&f);
    let mut second = repair_adapter(&f);
    let p = repair_plan(&f, &mut second, 2, 3, 103);
    let mut pending = apply_repair(&f, &mut second, p, 104);
    assert_eq!(
        complete_repair(&f, &mut second, &mut pending, 105).progress,
        RepairProgress::Verified
    );
    assert_owned_origins(&f);
}

#[test]
fn second_repair_receipts_are_owned_by_the_frozen_removal_observer() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut r, p, 101);
    complete_repair(&f, &mut r, &mut pending, 102);
    assert_eq!(owned_stat(&launch_plist(&f)).st_mode & 0o7777, 0o600);
    let mut second = repair_adapter(&f);
    let p = repair_plan(&f, &mut second, 2, 3, 103);
    let mut pending = apply_repair(&f, &mut second, p, 104);
    complete_repair(&f, &mut second, &mut pending, 105);
    assert_owned_origins(&f);
    assert_eq!(owned_stat(&launch_plist(&f)).st_mode & 0o7777, 0o600);
    let o = removal::MacRemovalObserver::admit(
        f.io.clone(),
        inventory(),
        f.source.join("Crosspane.app/Contents/Resources/audio"),
        f.clock.clone(),
        &f.deadline(),
    )
    .unwrap();
    let current = current(&f, 106);
    let observed = o
        .observe(
            Some(current),
            1,
            crosspane_installer_core::OperationId(4),
            &f.deadline(),
        )
        .unwrap();
    for id in [
        "Crosspane.app/Contents/MacOS/Crosspane",
        "crosspanectl",
        "mac.launch-agent",
    ] {
        assert_eq!(
            observed
                .inventory()
                .resources
                .iter()
                .find(|r| r.id == id)
                .unwrap()
                .state,
            removal::ResourceState::Owned
        );
    }
}
#[test]
fn absent_whole_owned_app_reinstalls_without_minting_a_clean_original() {
    let f = producer_fixture();
    let app = f.io.target().app_path();
    let (_root, parent, name) = owned_parent(&app);
    let fd = rfs::openat(&parent, name, DIRECTORY_FLAGS, rfs::Mode::empty()).unwrap();
    clear_owned(&fd);
    assert!(same_inode(&rfs::fstat(&fd).unwrap(), &owned_stat(&app)));
    rfs::unlinkat(&parent, name, rfs::AtFlags::REMOVEDIR).unwrap();
    f.runner.behavior.lock().unwrap().job_pid = 0;
    f.runner.stopped.store(true, Ordering::Release);
    let mut r = repair_adapter(&f);
    let p = r.plan(1, 2, None, &f.deadline()).unwrap();
    assert_eq!(p.preview().effects.len(), 4);
    let c = p.consent(1, 2, true).unwrap();
    let mut pending = r.apply(p, c, None, &f.deadline()).unwrap();
    assert_eq!(f.runner.count("bootout"), 0);
    assert_eq!(
        complete_repair(&f, &mut r, &mut pending, 100).progress,
        RepairProgress::Verified
    );
    assert_owned_origins(&f);
}
#[test]
fn missing_agent_and_stopped_app_refuse_with_uninstall_guidance() {
    for missing in [false, true] {
        let f = producer_fixture();
        if missing {
            remove_owned(&f.io.target().agent_path());
        } else {
            f.runner.behavior.lock().unwrap().job_pid = 0;
            f.runner.stopped.store(true, Ordering::Release);
        }
        let mut r = repair_adapter(&f);
        let error = r.plan(1, 2, None, &f.deadline()).unwrap_err();
        assert_eq!(error, NativeError::Refused);
        assert_eq!(plan_guidance(error), RepairGuidance::UninstallThenInstall);
        assert_eq!(f.runner.count("bootout"), 0);
        assert_eq!(f.runner.count("bootstrap"), 1);
    }
}
#[test]
fn adopted_or_malformed_payload_receipts_never_grant_repair_authority() {
    for mode in 0..8 {
        let f = producer_fixture();
        change_record(&f, "payload.json", |value| match mode {
            0 => {
                value["receipt"]["resources"][0]["ownership"] = json!("Adopted");
                value["receipt"]["resources"][0]["before"] = json!("Different");
            }
            1 => value["receipt"]["payload_sha256"][0] = json!(99),
            2 => value["receipt"]["manifest_sha256"][0] = json!(99),
            3 => value["receipt"]["resources"][0]["before"] = json!("Different"),
            4 => {
                value["receipt"]["resources"].as_array_mut().unwrap().pop();
            }
            5 => value["receipt"]["resources"][1]["resolved_path"] = json!("/foreign"),
            6 => value["receipt"]["extra"] = json!(true),
            7 => value["phase"] = json!("Unknown"),
            _ => unreachable!(),
        });
        let mut r = repair_adapter(&f);
        let (s, reply) = current(&f, 100);
        assert!(r.plan(1, 2, Some((&s, &reply)), &f.deadline()).is_err());
        assert_eq!(f.runner.count("bootout"), 0);
        assert_eq!(f.runner.count("bootstrap"), 1);
    }
}
#[test]
fn edited_unsigned_data_and_foreign_members_are_retained() {
    for foreign in [false, true] {
        let f = producer_fixture();
        let path = f.io.target().app_path().join(if foreign {
            "Contents/foreign"
        } else {
            "Contents/Info.plist"
        });
        bytes(&path, b"retained-foreign-or-edited", 0o644);
        let mut r = repair_adapter(&f);
        let (s, reply) = current(&f, 100);
        let error = r.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap_err();
        assert_eq!(error, NativeError::Foreign);
        assert_eq!(
            plan_guidance(error),
            RepairGuidance::RestoreOrRemoveFileOrUninstallThenInstall
        );
        assert_eq!(read_owned(&path), b"retained-foreign-or-edited");
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn differing_code_requires_approved_signature_and_architecture() {
    for valid in [false, true] {
        let f = producer_fixture();
        let ui = f.io.target().app_path().join("Contents/MacOS/crosspane-ui");
        bytes(
            &ui,
            &if valid {
                macho(false, 9)
            } else {
                b"not-approved-code".to_vec()
            },
            0o755,
        );
        let mut r = repair_adapter(&f);
        let (s, reply) = current(&f, 100);
        let result = r.plan(1, 2, Some((&s, &reply)), &f.deadline());
        assert_eq!(result.is_ok(), valid);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn unsigned_installed_code_refuses_before_any_dispatch() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    *f.signatures.change.lock().unwrap() = Some((ArtifactRole::Settings, 0));
    let (s, reply) = current(&f, 100);
    assert!(r.plan(1, 2, Some((&s, &reply)), &f.deadline()).is_err());
    assert_eq!(f.runner.count("bootout"), 0);
}
#[test]
fn edited_adopted_and_incomplete_launch_receipts_are_retained() {
    for mode in 0..5 {
        let f = producer_fixture();
        if mode == 0 {
            bytes(&launch_plist(&f), b"hand-edited plist", 0o644);
        } else {
            change_record(&f, "launch-agent.json", |v| match mode {
                1 => {
                    v["receipt"]["resources"][0]["ownership"] = json!("Adopted");
                    v["receipt"]["resources"][0]["before"] = json!("Different");
                }
                2 => v["phase"] = json!("BootstrapRequested"),
                3 => v["receipt"]["unfinished"] = json!([]),
                4 => v["receipt"]["payload_sha256"][0] = json!(99),
                _ => unreachable!(),
            });
        }
        let mut r = repair_adapter(&f);
        let (s, reply) = current(&f, 100);
        assert!(r.plan(1, 2, Some((&s, &reply)), &f.deadline()).is_err());
        assert_eq!(f.runner.count("bootout"), 0);
        assert_eq!(f.runner.count("bootstrap"), 1);
    }
}
#[test]
fn user_disabled_refuses_with_manual_guidance_and_never_enables() {
    let f = producer_fixture();
    f.runner.behavior.lock().unwrap().disabled = 1;
    let mut r = repair_adapter(&f);
    let (s, reply) = current(&f, 100);
    let error = r.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap_err();
    assert_eq!(error, NativeError::Unsupported);
    assert_eq!(plan_guidance(error), RepairGuidance::EnableManually);
    assert_eq!(f.runner.count("enable"), 0);
    assert_eq!(f.runner.count("bootout"), 0);
}
#[test]
fn incompatible_live_version_refuses_without_payload_mutation() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    let s = f.selected();
    let mut v = f.status(2);
    v["result"]["installer"]["build"]["version"] = json!("different-version");
    let reply = f.reply(&v, 100);
    assert!(r.plan(1, 2, Some((&s, &reply)), &f.deadline()).is_err());
    assert_eq!(f.runner.count("bootout"), 0);
}
#[test]
fn stale_and_foreign_consents_do_zero_filesystem_command_or_probe_work() {
    for foreign in [false, true] {
        let f = producer_fixture();
        let fs_calls = Arc::new(AtomicU64::new(0));
        let count = fs_calls.clone();
        let io = f.hooked(Arc::new(move |_, _, id| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(id)
        }));
        let mut r = repair_on(&f, io.clone());
        let (s, reply) = (f.selected_on(io.clone()), f.reply(&f.status(2), 100));
        let p = r.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap();
        let c = if foreign {
            let mut other = repair_on(&f, io.clone());
            let p = other.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap();
            p.consent(1, 2, true).unwrap()
        } else {
            let c = p.consent(1, 2, true).unwrap();
            let reply = f.reply(&f.status(2), 101);
            r.plan(2, 3, Some((&s, &reply)), &f.deadline()).unwrap();
            c
        };
        let fresh = f.reply(&f.status(2), 102);
        let baseline = (counts(&f), fs_calls.load(Ordering::Acquire));
        assert_eq!(
            r.apply(p, c, Some((&s, &fresh)), &f.deadline())
                .unwrap_err(),
            NativeError::Refused
        );
        assert_eq!((counts(&f), fs_calls.load(Ordering::Acquire)), baseline);
    }
}
#[test]
fn identical_or_backwards_activity_receipts_cannot_revalidate() {
    for mode in 0..3 {
        let f = producer_fixture();
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let c = p.consent(1, 2, true).unwrap();
        let (s, mut reply) = current(&f, if mode == 0 { 100 } else { 101 });
        if mode == 1 {
            reply.observed_at_ms = 99;
        }
        if mode == 2 {
            reply.source = ObservationSource::Live;
        }
        let baseline = counts(&f);
        assert!(r.apply(p, c, Some((&s, &reply)), &f.deadline()).is_err());
        assert_eq!(counts(&f), baseline);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn changed_activity_epochs_config_or_session_refuse_current_consent() {
    for mode in 0..4 {
        let f = producer_fixture();
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let c = p.consent(1, 2, true).unwrap();
        let s = f.selected();
        let mut v = f.status(2);
        match mode {
            0 => v["result"]["installer"]["epochs"]["layout"] = json!(2),
            1 => v["result"]["installer"]["config_revision"] = json!("2222222222222222"),
            2 => {
                v["result"]["controlling"] =
                    json!("1111111111111111111111111111111111111111111111111111111111111111")
            }
            3 => {
                let mut support = f.support.observation.lock().unwrap();
                support.gui.console_session = "new-session".into();
                support.gui.interactive_session = "new-session".into();
            }
            _ => unreachable!(),
        }
        let reply = f.reply(&v, 101);
        assert!(r.apply(p, c, Some((&s, &reply)), &f.deadline()).is_err());
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn original_receipt_and_resource_drift_refuse_before_stop() {
    for receipt in [false, true] {
        let f = producer_fixture();
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let c = p.consent(1, 2, true).unwrap();
        if receipt {
            let path = f.io.target().installer_dir().join("payload.json");
            let contents = read_owned(&path);
            rename_owned(&path, &path.with_file_name("retained-prior-receipt"));
            bytes(&path, &contents, 0o600);
        } else {
            bytes(
                &f.home.join(".local/bin/crosspanectl"),
                &macho(false, 8),
                0o755,
            );
        }
        let (s, reply) = current(&f, 101);
        assert!(r.apply(p, c, Some((&s, &reply)), &f.deadline()).is_err());
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn r1_launch_receipt_swap_at_lock_preserves_foreign_record_and_dispatches_nothing() {
    for adopted in [false, true] {
        let f = producer_fixture();
        let path = f.io.target().installer_dir().join("launch-agent.json");
        let mut value = record(&f, "launch-agent.json");
        if adopted {
            value["receipt"]["resources"][0]["ownership"] = json!("Adopted");
            value["receipt"]["resources"][0]["before"] = json!("Different");
        } else {
            value["phase"] = json!("Intent");
        }
        let foreign = serde_json::to_vec(&value).unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let hook_fired = fired.clone();
        let record_path = path.clone();
        let replacement = foreign.clone();
        let io = f.hooked(Arc::new(move |stage, lock_path, identity| {
            if stage == "complete"
                && lock_path.file_name() == Some(std::ffi::OsStr::new("lock"))
                && !hook_fired.swap(true, Ordering::AcqRel)
            {
                rename_owned(
                    &record_path,
                    &record_path.with_file_name("retained-admitted-launch-receipt"),
                );
                bytes(&record_path, &replacement, 0o600);
            }
            Ok(identity)
        }));
        let mut repair = repair_on(&f, io.clone());
        let selected = f.selected_on(io);
        let plan = repair
            .plan(
                1,
                2,
                Some((&selected, &f.reply(&f.status(2), 100))),
                &f.deadline(),
            )
            .unwrap();
        let consent = plan.consent(1, 2, true).unwrap();
        let original = [
            f.io.target().app_path(),
            f.home.join(".local/bin/crosspanectl"),
            launch_plist(&f),
        ]
        .map(|p| owned_stat(&p).st_ino);
        let bootout = f.runner.count("bootout");
        let bootstrap = f.runner.count("bootstrap");
        let result = repair.apply(
            plan,
            consent,
            Some((&selected, &f.reply(&f.status(2), 101))),
            &f.deadline(),
        );
        assert!(
            fired.load(Ordering::Acquire),
            "swap must occur at actual lock acquisition"
        );
        assert!(matches!(result, Err(NativeError::Foreign)), "{result:?}");
        assert_eq!(
            read_owned(&path),
            foreign,
            "replacement record must never be overwritten"
        );
        assert_eq!(f.runner.count("bootout"), bootout);
        assert_eq!(f.runner.count("bootstrap"), bootstrap);
        assert_eq!(
            [
                f.io.target().app_path(),
                f.home.join(".local/bin/crosspanectl"),
                launch_plist(&f)
            ]
            .map(|p| owned_stat(&p).st_ino),
            original,
        );
    }
}
#[test]
fn real_clean_stop_is_required_and_never_blindly_resent() {
    for mode in [1, 2, 3, 4, 5] {
        let f = producer_fixture();
        f.runner.behavior.lock().unwrap().bootout = mode;
        let old = owned_stat(&f.io.target().app_path()).st_ino;
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let mut pending = apply_repair(&f, &mut r, p, 101);
        assert_ne!(pending.progress(), RepairProgress::Published);
        let _ = r.continue_clean_stop(&mut pending, &f.deadline());
        assert_eq!(f.runner.count("bootout"), 1);
        assert_eq!(f.runner.count("bootstrap"), 1);
        assert_eq!(owned_stat(&f.io.target().app_path()).st_ino, old);
        assert!(r.plan(2, 3, None, &f.deadline()).is_err());
    }
}
#[test]
fn stop_wait_can_continue_only_after_the_actual_original_exit() {
    let f = producer_fixture();
    f.runner.behavior.lock().unwrap().bootout = 1;
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut r, p, 101);
    assert_eq!(pending.progress(), RepairProgress::WaitingForCleanStop);
    f.runner.behavior.lock().unwrap().job_pid = 0;
    f.runner.stopped.store(true, Ordering::Release);
    bytes(
        &f.io.target().state_dir().join("last_exit.json"),
        &serde_json::to_vec(&json!({
        "schema_version":1,"instance_id":2,"stopped_unix_ms":2000,"clean":true,
        "parking":"restored","input_journals_empty":true,"audio_stopped":true}))
        .unwrap(),
        0o600,
    );
    r.continue_clean_stop(&mut pending, &f.deadline()).unwrap();
    assert_eq!(pending.progress(), RepairProgress::Published);
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(
        complete_repair(&f, &mut r, &mut pending, 102).progress,
        RepairProgress::Verified
    );
}
#[test]
fn after_mutation_uncertainty_retains_prior_payload_and_refuses_replay() {
    for mode in [1, 2, 3] {
        let f = producer_fixture();
        f.runner.behavior.lock().unwrap().bootstrap = mode;
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let mut pending = apply_repair(&f, &mut r, p, 101);
        assert_eq!(pending.progress(), RepairProgress::RecoveryRetained);
        assert!(
            f.home
                .join("Applications/.Crosspane.app.crosspane-previous")
                .exists()
        );
        assert_eq!(f.runner.count("bootout"), 1);
        let baseline = counts(&f);
        assert!(r.continue_clean_stop(&mut pending, &f.deadline()).is_err());
        assert!(r.plan(2, 3, None, &f.deadline()).is_err());
        assert_eq!(counts(&f), baseline);
    }
}
#[test]
fn before_mutation_deadline_or_cancellation_keeps_original_install_unchanged() {
    for cancel in [false, true] {
        let f = producer_fixture();
        let mut r = repair_adapter(&f);
        let p = repair_plan(&f, &mut r, 1, 2, 100);
        let c = p.consent(1, 2, true).unwrap();
        let (s, reply) = current(&f, 101);
        let old = owned_stat(&f.io.target().app_path()).st_ino;
        let token = Cancellation::default();
        let d = Deadline::new(1, f.clock.clone(), token.clone()).unwrap();
        if cancel {
            token.cancel();
        } else {
            f.clock.0.store(102, Ordering::Release);
        }
        assert!(r.apply(p, c, Some((&s, &reply)), &d).is_err());
        assert_eq!(owned_stat(&f.io.target().app_path()).st_ino, old);
        assert_eq!(f.runner.count("bootout"), 0);
    }
}
#[test]
fn pre_health_refusal_leaves_published_and_fresh_health_can_verify_without_resend() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut r, p, 101);
    r.expect_health(&mut pending, 102).unwrap();
    let s = f.selected();
    let mut v = f.status(3);
    v["result"]["installer"]["build"]["version"] = json!("wrong-version");
    assert!(
        r.observe(&mut pending, &s, f.reply(&v, 102), &f.deadline())
            .is_err()
    );
    assert_eq!(pending.progress(), RepairProgress::Published);
    assert!(
        f.home
            .join("Applications/.Crosspane.app.crosspane-previous")
            .exists()
    );
    assert_eq!(
        complete_repair(&f, &mut r, &mut pending, 103).progress,
        RepairProgress::Verified
    );
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 2);
}
#[test]
fn post_health_cleanup_and_both_receipt_publication_failures_are_explicit() {
    for mode in 0..3 {
        let f = producer_fixture();
        let armed = Arc::new(AtomicBool::new(false));
        let flag = armed.clone();
        let io = f.hooked(Arc::new(move |stage, path, identity| {
            let fail = match mode {
                0 => stage == "unlink" && path.to_string_lossy().contains(".crosspane-previous"),
                1 => {
                    stage == "write"
                        && path.file_name() == Some(std::ffi::OsStr::new("payload.json"))
                }
                2 => {
                    stage == "write"
                        && path.file_name() == Some(std::ffi::OsStr::new("launch-agent.json"))
                }
                _ => false,
            };
            if flag.load(Ordering::Acquire) && fail {
                return Err(NativeError::Unavailable);
            }
            Ok(identity)
        }));
        let mut r = repair_on(&f, io.clone());
        let s = f.selected_on(io.clone());
        let reply = f.reply(&f.status(2), 100);
        let p = r.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap();
        let c = p.consent(1, 2, true).unwrap();
        let reply = f.reply(&f.status(2), 101);
        let mut pending = r.apply(p, c, Some((&s, &reply)), &f.deadline()).unwrap();
        r.expect_health(&mut pending, 102).unwrap();
        let new = f.selected_on(io.clone());
        armed.store(true, Ordering::Release);
        let complete = r
            .observe(
                &mut pending,
                &new,
                f.reply(&f.status(3), 102),
                &f.deadline(),
            )
            .unwrap();
        assert_eq!(
            complete.progress,
            RepairProgress::HealthVerifiedCleanupIncomplete
        );
        assert!(complete.startup.is_none());
        let baseline = counts(&f);
        assert!(
            r.observe(
                &mut pending,
                &new,
                f.reply(&f.status(3), 103),
                &f.deadline()
            )
            .is_err()
        );
        assert_eq!(counts(&f), baseline);
        assert_eq!(f.runner.count("bootout"), 1);
        assert_eq!(f.runner.count("bootstrap"), 2);
    }
}
#[test]
fn identity_configuration_trust_and_recovery_journals_are_byte_identical() {
    let f = producer_fixture();
    let paths: Vec<_> = [
        "device-key.pk8",
        "config.toml",
        "trust.json",
        "revocations.json",
        "input.journal",
        "projection-input.journal",
    ]
    .into_iter()
    .map(|name| f.io.target().state_dir().join(name))
    .collect();
    for (index, p) in paths.iter().enumerate() {
        bytes(p, format!("inert-preserved-{index}").as_bytes(), 0o600);
    }
    let before: Vec<_> = paths
        .iter()
        .map(|p| (owned_stat(p).st_ino, read_owned(p)))
        .collect();
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let mut pending = apply_repair(&f, &mut r, p, 101);
    complete_repair(&f, &mut r, &mut pending, 102);
    assert_eq!(
        paths
            .iter()
            .map(|p| (owned_stat(p).st_ino, read_owned(p)))
            .collect::<Vec<_>>(),
        before
    );
    assert_eq!(f.runner.count("erase-identity"), 0);
}
#[test]
fn interrupted_prior_payload_is_retained_and_never_restored_automatically() {
    let f = producer_fixture();
    let prior = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous");
    directory(&prior);
    bytes(&prior.join("inert-recovery-A"), b"manual-A", 0o644);
    let mut r = repair_adapter(&f);
    let (s, reply) = current(&f, 100);
    assert_eq!(
        r.plan(1, 2, Some((&s, &reply)), &f.deadline()).unwrap_err(),
        NativeError::OutcomeUnknown
    );
    assert_eq!(read_owned(&prior.join("inert-recovery-A")), b"manual-A");
    assert_eq!(f.runner.count("bootout"), 0);
}
#[test]
fn opaque_debug_contains_no_paths_nodes_or_activity_data() {
    let f = producer_fixture();
    let mut r = repair_adapter(&f);
    let p = repair_plan(&f, &mut r, 1, 2, 100);
    let c = p.consent(1, 2, true).unwrap();
    for value in [format!("{r:?}"), format!("{p:?}"), format!("{c:?}")] {
        assert!(!value.contains(f.home.to_str().unwrap()));
        assert!(!value.contains("1111111111111111"));
    }
}
