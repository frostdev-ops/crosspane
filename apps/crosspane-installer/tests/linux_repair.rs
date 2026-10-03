#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*, removal::*, service::*},
};
use crosspane_installer_core::{OperationId, ResourceObservation, ResourceOwnership};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
const HEALTH: &str = r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":["video"]},"instance":{"id":9,"pid":4242,"uid":1000,
"exe":"/home/u/.local/bin/crosspane-agent","runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790942400000},
"config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"nothing_parked","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"backends":[
{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
{"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
{"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
{"name":"discovery","state":"ready","reason":null}],"keystore":"os_store","permissions":[],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
type FakeOutput = Result<(Option<i32>, Vec<u8>, Vec<u8>), NativeError>;
struct Runner {
    calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    output: Mutex<FakeOutput>,
    stall: Mutex<bool>,
}
impl CommandRunner for Runner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        self.calls
            .lock()
            .unwrap()
            .push((c.executable().into(), c.argv().to_vec()));
        if c.executable() == Path::new("/bin/ps") {
            return Ok(CommandOutput {
                code: Some(0),
                stdout: if c.argv()[1] == "lstart=" {
                    START.to_vec()
                } else {
                    b"crosspane-agent\n".to_vec()
                },
                stderr: vec![],
            });
        }
        if c.executable() == Path::new("/usr/bin/systemctl") {
            // Deliberately unknown service facts; never pretend stop-zero or missing socket proves exit.
            return Ok(CommandOutput {
                code: Some(1),
                stdout: vec![],
                stderr: vec![],
            });
        }
        assert_eq!(c.argv(), ["erase-identity"]);
        while *self.stall.lock().unwrap() {
            d.check()?;
            thread::sleep(Duration::from_millis(1));
        }
        self.output
            .lock()
            .unwrap()
            .clone()
            .map(|(code, stdout, stderr)| CommandOutput {
                code,
                stdout,
                stderr,
            })
    }
}
struct Probe(Mutex<Result<Option<ProcessFacts>, NativeError>>);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        self.0
            .lock()
            .unwrap()
            .clone()?
            .ok_or(NativeError::Unavailable)
    }
}
impl ExitReader for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<Option<ProcessFacts>, NativeError> {
        d.check()?;
        self.0.lock().unwrap().clone()
    }
}
fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn facts(io: &LinuxNativeIo) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        hyprland_version: [0, 56, 0],
        protocols_ready: true,
        runtime_libraries_ready: true,
        uwsm_managed: true,
        graphical_target_active: true,
        graphical_sessions: 1,
        session_id: "scratch".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    }
}
struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    probe: Arc<Probe>,
    runner: Arc<Runner>,
    proof: SupportProof,
    _listener: UnixListener,
}
#[allow(dead_code)]
impl Fixture {
    fn new(agent: bool) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp419-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe(Mutex::new(Ok(Some(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: root.join(".local/bin/crosspane-agent"),
            generation: 77,
        })))));
        let runner = Arc::new(Runner { calls: Mutex::default(), stall: Mutex::new(false),
            output: Mutex::new(Ok((Some(0),br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"}"#.to_vec(), vec![]))) });
        // Exclusive mkdir: collision is an error. No helper initializes a pre-existing root.
        let io = Arc::new(LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap());
        let proof = io.scratch_support(facts(&io)).unwrap();
        for path in [
            io.target().paths().prefix.join("bin"),
            io.target().runtime_dir().into(),
            io.target().paths().runtime_home.join("systemd"),
            io.target().paths().state_home.join("crosspane"),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        let listener =
            UnixListener::bind(io.target().paths().runtime_home.join("systemd/private")).unwrap();
        let fixture = Self {
            root,
            io,
            probe,
            runner,
            proof,
            _listener: listener,
        };
        if agent {
            fixture.executable(b"owned inert test agent");
            fixture.bootstrap(9);
        }
        fixture
    }
    fn executable(&self, bytes: &[u8]) {
        self.io.validate_target().unwrap();
        let parent = rustix::fs::open(
            self.io.target().agent_path().parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let fd = rustix::fs::openat(
            &parent,
            ".fixture-agent-new",
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let mut file = fs::File::from(fd);
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_bits_truncate(0o755)).unwrap();
        rustix::fs::renameat(&parent, ".fixture-agent-new", &parent, "crosspane-agent").unwrap();
        rustix::fs::fsync(parent).unwrap();
    }
    fn bootstrap(&self, id: u64) {
        self.io
            .atomic_write(
                &self.proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &serde_json::to_vec(&json!({"schema_version":1,"instance_id":id,"pid":4242,
                "started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,
                "keystore":"os_store","reason":null,"runtime_dir":self.io.target().runtime_dir()}))
                .unwrap(),
            )
            .unwrap();
    }
    fn exit(&self, change: impl FnOnce(&mut Value)) {
        let mut value = json!({"schema_version":1,"instance_id":9,"stopped_unix_ms":parse_ps_start(START).unwrap()+1000,
            "clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true});
        change(&mut value);
        self.io
            .atomic_write(
                &self.proof,
                &self
                    .io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/last_exit.json"),
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
    }
    fn tracked(&self) -> Arc<TrackedAgent> {
        Arc::new(
            TrackedAgent::scratch_capture(self.io.clone(), self.probe.clone(), &deadline())
                .unwrap(),
        )
    }
    fn planner(&self) -> RemovalPlanner {
        let mut planner = RemovalPlanner::new(self.io.clone());
        planner.scratch_exit_reader(self.probe.clone()).unwrap();
        planner
    }
    fn service(&self, package: &Package) -> LinuxService {
        LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            PayloadInstaller::new(self.io.clone())
                .unwrap()
                .rendered_resources(package)
                .unwrap(),
            &deadline(),
        )
        .unwrap()
    }
    fn inventory(&self, planner: &RemovalPlanner, package: &Package) -> Inventory {
        planner
            .inventory(
                &self.proof,
                package,
                &self.service(package),
                None,
                100,
                &deadline(),
            )
            .unwrap()
    }
    fn request<'a>(
        &'a self,
        package: &'a Package,
        service: &'a LinuxService,
        deadline: &'a Deadline,
    ) -> InventoryRequest<'a> {
        InventoryRequest {
            proof: &self.proof,
            package,
            service,
            reply: None,
            now_ms: 100,
            deadline,
        }
    }
    fn environment(&self) -> ChildEnvironment {
        ChildEnvironment::selected(self.io.target(), BTreeMap::new()).unwrap()
    }
    fn erase_count(&self) -> usize {
        self.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, a)| a == &["erase-identity"])
            .count()
    }
    fn reply(&self) -> AgentReply {
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let i = &mut v["result"]["installer"]["instance"];
        i["uid"] = json!(self.io.target().paths().uid);
        i["exe"] = json!(self.io.target().agent_path());
        i["runtime_dir"] = json!(self.io.target().runtime_dir());
        AgentReply {
            id: 19,
            observed_at_ms: 100,
            source: ObservationSource::Demo,
            result: decode_reply(
                &InstallerRequest::Status,
                &serde_json::to_vec(&v).unwrap(),
                AgentPlatform::Linux,
            ),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn package() -> Package {
    package_for(Architecture::native().unwrap())
}
fn package_for(architecture: Architecture) -> Package {
    package_with_icon(architecture, b"inert resource")
}
fn package_with_icon(architecture: Architecture, icon: &[u8]) -> Package {
    let mut elf = vec![0; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    elf[18..20].copy_from_slice(
        &(if architecture == Architecture::X86_64 {
            62u16
        } else {
            183u16
        })
        .to_le_bytes(),
    );
    elf[20] = 1;
    elf[52] = 64;
    let data: Vec<Vec<u8>> = (0..10)
        .map(|i| match i {
            0..=4 => elf.clone(),
            5 => include_bytes!("../../../packaging/linux/crosspane-agent.service").to_vec(),
            6 => include_bytes!("../../../packaging/linux/crosspane-settings.desktop").to_vec(),
            7 => include_bytes!("../../../packaging/linux/crosspane-installer.desktop").to_vec(),
            8 => icon.to_vec(),
            _ => b"inert resource".to_vec(),
        })
        .collect();
    let hex = |b: &[u8]| {
        sha256(b)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    };
    let manifest = Manifest {
        schema_version: 1,
        product_version: "0.0.0".into(),
        architecture,
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf),
        }],
        members: FILES
            .iter()
            .zip(&data)
            .enumerate()
            .map(|(i, (name, b))| Artifact {
                name: (*name).into(),
                size: b.len(),
                sha256: hex(b),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    };
    fn member(name: &str, b: &[u8]) -> Vec<u8> {
        let mut h = vec![0; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        for (at, width, n) in [
            (
                100,
                8,
                if name.starts_with("bin/") {
                    0o755
                } else {
                    0o644
                },
            ),
            (108, 8, 0),
            (116, 8, 0),
            (124, 12, b.len()),
            (136, 12, 0),
        ] {
            h[at..at + width]
                .copy_from_slice(format!("{n:0width$o}\0", width = width - 1).as_bytes());
        }
        h[156] = b'0';
        h[257..265].copy_from_slice(b"ustar\x0000");
        h[148..156].fill(b' ');
        let sum: usize = h.iter().map(|b| usize::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h.extend(b);
        h.resize(h.len().div_ceil(512) * 512, 0);
        h
    }
    let mut archive = member("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    for (name, b) in FILES.iter().zip(&data) {
        archive.extend(member(name, b));
    }
    archive.extend(vec![0; 1024]);
    Package::read(archive.as_slice(), architecture, sha256(&archive)).unwrap()
}

fn installed(f: &Fixture, p: &Package) {
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = install
        .plan(&f.proof, p, OperationId(47), MatchingFiles::Preserve)
        .unwrap();
    install.apply(&f.proof, p, plan, &deadline()).unwrap();
    f.bootstrap(9);
    install
        .verify(&f.proof, p, 19, 100, &f.reply(), &deadline())
        .unwrap();
}

#[test]
fn cleanup_admits_completed_literal_ledger_with_read_only_exact_snapshots() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let commands = f.runner.calls.lock().unwrap().len();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.receipt().resources.len(), 10);
    assert_eq!(proof.observation(0).unwrap(), ResourceObservation::Matching);
    assert!(proof.owned(0).unwrap());
    assert_eq!(proof.observation(10), Err(NativeError::Invalid));
    assert_eq!(proof.owned(10), Err(NativeError::Invalid));
    assert_eq!(format!("{proof:?}"), "CleanupProof(..)");
    let before: Vec<_> = proof
        .receipt()
        .resources
        .iter()
        .map(|r| fs::read(&r.resolved_path).unwrap())
        .collect();
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
    proof.revalidate(&deadline()).unwrap();
    let after: Vec<_> = proof
        .receipt()
        .resources
        .iter()
        .map(|r| fs::read(&r.resolved_path).unwrap())
        .collect();
    assert_eq!(before, after);
    assert!(
        f.io.metadata(
            &f.io
                .target()
                .paths()
                .state_home
                .join("crosspane/installer/removal-intent.json")
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
}

#[test]
fn cleanup_refuses_changed_ledger_modified_hardlinked_and_foreign_resources() {
    for mutation in 0..4 {
        let f = Fixture::new(false);
        installed(&f, &package());
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
        if mutation == 0 {
            let ledger =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/payload-outcome.json");
            let mut value: Value =
                serde_json::from_slice(&f.io.read(&ledger, MAX_RECORD_BYTES, true).unwrap())
                    .unwrap();
            value["receipt"]["operation_id"] = json!(999);
            f.io.atomic_write(&f.proof, &ledger, &serde_json::to_vec(&value).unwrap())
                .unwrap();
        } else if mutation == 1 {
            fs::write(&path, b"administrator modification").unwrap();
        } else if mutation == 2 {
            fs::hard_link(&path, f.root.join("outside-inventory-link")).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(f.io.target().agent_path(), &path).unwrap();
        }
        assert!(matches!(
            proof.revalidate(&deadline()),
            Err(NativeError::Foreign)
        ));
        // Captured facts are not silently refreshed by revalidation or clicks.
        assert_eq!(proof.observation(8).unwrap(), ResourceObservation::Matching);
        assert!(proof.owned(8).unwrap());
        assert!(fs::symlink_metadata(path).is_ok());
    }
}

#[test]
fn cleanup_rejects_unfinished_and_malformed_ledger_without_creating_lock_or_intent() {
    for field in ["phase", "resources", "path", "replacement", "source"] {
        let f = Fixture::new(false);
        installed(&f, &package());
        let state = f.io.target().paths().state_home.join("crosspane/installer");
        let ledger = state.join("payload-outcome.json");
        let mut value: Value =
            serde_json::from_slice(&f.io.read(&ledger, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        match field {
            "phase" => value["phase"] = json!("Applied"),
            "resources" => value["receipt"]["resources"][0] = json!([]),
            "path" => {
                value["receipt"]["resources"][0]["resolved_path"] = json!(f.root.join("unrecorded"))
            }
            "replacement" => value["items"][0]["replacement"]["mode"] = json!(0o777),
            "source" => value["source"] = json!("Live"),
            _ => unreachable!(),
        }
        f.io.atomic_write(&f.proof, &ledger, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(f.io.admit_cleanup(&deadline()).is_err(), "{field}");
        assert!(
            f.io.metadata(&state.join("removal-intent.json"))
                .unwrap()
                .is_none()
        );
    }
}

fn cleanup_ledger(f: &Fixture) -> (PathBuf, Value) {
    let path =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-outcome.json");
    let value = serde_json::from_slice(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
    (path, value)
}

#[test]
fn cleanup_requires_each_nullable_field_and_strict_nested_objects_and_enums() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (path, original) = cleanup_ledger(&f);
    for (pointer, key) in [
        ("", "previous_instance"),
        ("", "base_generation"),
        ("/items/0", "old"),
        ("/items/0", "replacement"),
    ] {
        let mut value = original.clone();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}/{key}"
        );
    }
    for pointer in [
        "",
        "/receipt",
        "/items/0",
        "/items/0/replacement",
        "/receipt/resources/0",
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = json!([]);
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}"
        );
    }
    for pointer in [
        "/source",
        "/items/0/ownership",
        "/receipt/resources/0/ownership",
        "/receipt/resources/0/before",
        "/receipt/resources/0/after",
        "/receipt/resources/0/outcome",
    ] {
        let mut value = original.clone();
        let enum_name = value.pointer(pointer).unwrap().as_str().unwrap().to_owned();
        *value.pointer_mut(pointer).unwrap() = json!({enum_name: null});
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}"
        );
    }
}

#[test]
fn cleanup_rejects_duplicate_unknown_fields_and_incomplete_completion_truth() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (path, original) = cleanup_ledger(&f);
    let text = serde_json::to_string(&original).unwrap();
    for (field, value) in [
        ("phase", &original["phase"]),
        (
            "resource_id",
            &original["receipt"]["resources"][0]["resource_id"],
        ),
        ("ownership", &original["items"][0]["ownership"]),
        ("hash", &original["items"][0]["replacement"]["hash"]),
    ] {
        let member = format!("\"{field}\":{}", serde_json::to_string(value).unwrap());
        assert!(text.contains(&member));
        let duplicate = text.replacen(&member, &format!("{member},{member}"), 1);
        // A permissive parser must accept the duplicate fixture before strict admission rejects it.
        let _: Value = serde_json::from_str(&duplicate).unwrap();
        f.io.atomic_write(&f.proof, &path, duplicate.as_bytes())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "duplicate {field}"
        );
    }
    for change in 0..8 {
        let mut value = original.clone();
        match change {
            0 => value["receipt"]["unfinished"] = json!([1]),
            1 => value["receipt"]["resources"][0]["outcome"] = json!("Unknown"),
            2 => value["receipt"]["resources"][0]["after"] = json!("Unknown"),
            3 => value["items"][0]["replacement"] = Value::Null,
            4 => value["items"][0]["unexpected"] = json!(true),
            5 => value["items"][0]["replacement"]["file"] = json!([1]),
            6 => {
                value["items"][0]["replacement"] = Value::Null;
                value["items"][0]["old"] = value["items"][0]["new"].clone();
            }
            7 => {
                value["items"][0]["replacement"] = Value::Null;
                value["receipt"]["resources"][0]["before"] = json!("Matching");
            }
            _ => unreachable!(),
        }
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "change {change}"
        );
    }
}

#[test]
fn cleanup_retains_adopted_foreign_modified_and_same_hash_replaced_resources() {
    for change in 0..4 {
        let f = Fixture::new(false);
        installed(&f, &package());
        let (path, mut value) = cleanup_ledger(&f);
        let resource = PathBuf::from(
            value["receipt"]["resources"][8]["resolved_path"]
                .as_str()
                .unwrap(),
        );
        if change < 2 {
            let ownership = if change == 0 { "Adopted" } else { "Foreign" };
            value["items"][8]["ownership"] = json!(ownership);
            value["receipt"]["resources"][8]["ownership"] = json!(ownership);
            f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
                .unwrap();
        } else if change == 2 {
            fs::write(&resource, b"retained administrator edit").unwrap();
        } else {
            let bytes = fs::read(&resource).unwrap();
            fs::rename(&resource, f.root.join("retained-previous-icon")).unwrap();
            fs::write(&resource, bytes).unwrap();
        }
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        assert!(!proof.owned(8).unwrap());
        assert_eq!(
            proof.observation(8).unwrap(),
            if change == 2 {
                ResourceObservation::Different
            } else {
                ResourceObservation::Matching
            }
        );
        proof.revalidate(&deadline()).unwrap();
        assert!(resource.is_file());
    }
}

#[test]
fn cleanup_absence_is_fresh_noent_and_parent_replacement_cannot_transfer_authority() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (_, value) = cleanup_ledger(&f);
    let path = PathBuf::from(
        value["receipt"]["resources"][8]["resolved_path"]
            .as_str()
            .unwrap(),
    );
    fs::remove_file(&path).unwrap();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.observation(8).unwrap(), ResourceObservation::Absent);
    assert!(proof.owned(8).unwrap());
    proof.revalidate(&deadline()).unwrap();
    let parent = path.parent().unwrap();
    fs::rename(parent, f.root.join("retained-parent")).unwrap();
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
        .create(parent)
        .unwrap();
    assert!(matches!(
        proof.revalidate(&deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(!path.exists());
    let refreshed = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(
        refreshed.observation(8).unwrap(),
        ResourceObservation::Absent
    );
    assert!(!refreshed.owned(8).unwrap());
}

#[test]
fn cleanup_unfinished_intent_and_deadline_cancellation_never_create_or_dispatch() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let calls = f.runner.calls.lock().unwrap().len();
    let cancel = Cancellation::default();
    let d = Deadline::new(5000, cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        f.io.admit_cleanup(&d),
        Err(NativeError::Cancelled)
    ));
    assert_eq!(proof.revalidate(&d), Err(NativeError::Cancelled));
    let intent =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-intent.json");
    f.io.atomic_write(&f.proof, &intent, b"unfinished payload mutation")
        .unwrap();
    assert!(matches!(
        f.io.admit_cleanup(&deadline()),
        Err(NativeError::Foreign)
    ));
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert_eq!(f.runner.calls.lock().unwrap().len(), calls);
}

#[test]
fn cleanup_refuses_replaced_ancestor_even_when_final_parent_and_file_are_preserved() {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let parent = path.parent().unwrap();
    let final_inode = fs::metadata(parent).unwrap().ino();
    let ancestor = parent.parent().unwrap();
    let retained = f.root.join("retained-scalable-ancestor");
    fs::rename(ancestor, &retained).unwrap();
    fs::DirBuilder::new().mode(0o700).create(ancestor).unwrap();
    fs::rename(retained.join("apps"), parent).unwrap();
    assert_eq!(fs::metadata(parent).unwrap().ino(), final_inode);
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert!(path.is_file());
}

#[test]
fn cleanup_refuses_parent_permission_drift_from_0700_to_0755() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let parent = path.parent().unwrap();
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert!(path.is_file());
}

fn completed_cleanup_repair(minimal: bool) {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    if minimal {
        fs::remove_file(&installer.targets()[8]).unwrap();
    }
    let plan = installer
        .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
        .unwrap();
    installer.apply(&f.proof, &p, plan, &deadline()).unwrap();
    installer
        .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
        .unwrap();
    let (_, ledger) = cleanup_ledger(&f);
    assert_eq!(ledger["phase"], "Verified");
    assert_eq!(ledger["items"][0]["ownership"], "Created");
    assert!(ledger["items"][0]["replacement"].is_null());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    for index in 0..10 {
        assert_eq!(
            proof.observation(index).unwrap(),
            ResourceObservation::Matching
        );
        assert_eq!(proof.owned(index).unwrap(), minimal && index == 8);
    }
    proof.revalidate(&deadline()).unwrap();
}

#[test]
fn cleanup_accepts_genuine_completed_repeat_without_replacement_provenance() {
    completed_cleanup_repair(false);
}

#[test]
fn cleanup_accepts_genuine_completed_minimal_repair_with_only_one_owned_replacement() {
    completed_cleanup_repair(true);
}

#[test]
fn cleanup_member_mode_boundary_is_pinned_to_frozen_payload_inventory() {
    assert_eq!(
        FILES,
        [
            "bin/crosspane-agent",
            "bin/crosspanectl",
            "bin/crosspane-ui",
            "bin/crosspane-installer",
            "bin/crosspane-tutorial",
            "resources/crosspane-agent.service",
            "resources/crosspane-settings.desktop",
            "resources/crosspane-installer.desktop",
            "resources/crosspane-icon.svg",
            "resources/LICENSE",
        ]
    );
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert!(FILES.iter().enumerate().all(|(index, name)| if index < 5 {
        name.starts_with("bin/")
    } else {
        name.starts_with("resources/")
    }));
    proof.revalidate(&deadline()).unwrap();
}
#[test]
fn matching_repeat_is_noop_and_missing_file_is_the_exact_minimal_delta() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let matching = f.inventory(&planner, &p);
    assert!(
        matching
            .facts()
            .resources
            .as_ref()
            .unwrap()
            .iter()
            .all(|r| r.ownership == ResourceOwnership::Created
                && r.before == ResourceObservation::Matching)
    );
    let plan = planner
        .plan(
            matching,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    // Use the producer's exact target, never a guessed deletion path.
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let icon = install
        .targets()
        .iter()
        .find(|p| p.extension().is_some_and(|x| x == "svg"))
        .unwrap()
        .clone();
    fs::remove_file(&icon).unwrap();
    let next = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(next.repair_delta().len(), 1);
    assert_eq!(next.repair_delta()[0].resolved_path, icon.to_string_lossy());
    assert_eq!(next.repair_delta()[0].before, ResourceObservation::Absent);
}
#[test]
fn foreign_modified_and_unverified_resources_never_become_repair_or_delete_authority() {
    let f = Fixture::new(true);
    let p = package();
    let mut planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert_eq!(facts.facts().resources, Err(PayloadError::Foreign));
    let plan = planner
        .plan(
            facts,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    assert!(plan.preview().contains("retain files"));
    let f = Fixture::new(false);
    installed(&f, &p);
    f.executable(b"user changed the agent");
    let mut planner = f.planner();
    let inventory = f.inventory(&planner, &p);
    let rows = inventory.facts().resources.as_ref().unwrap();
    assert_eq!(rows[0].ownership, ResourceOwnership::Foreign);
    assert_eq!(rows[0].before, ResourceObservation::Different);
    let plan = planner
        .plan(
            inventory,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
}
#[test]
fn missing_agent_has_not_clean_form_and_interruption_or_identity_reset_is_never_implicit() {
    let f = Fixture::new(false);
    let p = package();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(
        plan.cleanup_form(),
        CleanupForm::NotCleanRetainIdentityAndRecovery
    );
    assert_eq!(plan.repair_delta().len(), 10);
    assert!(plan.consent(1, OperationId(1), false).is_err());
    assert!(
        planner
            .plan(
                f.inventory(&planner, &p),
                2,
                OperationId(2),
                PlanKind::Repair,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(f.erase_count(), 0);
}
#[test]
fn changed_owned_resource_retires_consent_before_any_future_executor_io() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    f.executable(b"user edit");
    let current = f.inventory(&planner, &p);
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &current,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
}
#[test]
fn receipt_never_restores_activity_and_failed_startup_remains_reported_by_frozen_verifier() {
    let f = Fixture::new(false);
    let p = package();
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = install
        .plan(&f.proof, &p, OperationId(47), MatchingFiles::Preserve)
        .unwrap();
    let receipt = install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert!(!receipt.unfinished.is_empty());
    f.bootstrap(9);
    let mut v: Value = serde_json::from_str(HEALTH).unwrap();
    let i = &mut v["result"]["installer"]["instance"];
    i["uid"] = json!(f.io.target().paths().uid);
    i["exe"] = json!(f.io.target().agent_path());
    i["runtime_dir"] = json!(f.io.target().runtime_dir());
    v["result"]["installer"]["startup_recovery"] = json!("failed");
    v["result"]["installer"]["recovery_pending"] = json!(0);
    let reply = AgentReply {
        id: 19,
        observed_at_ms: 100,
        source: ObservationSource::Demo,
        result: decode_reply(
            &InstallerRequest::Status,
            &serde_json::to_vec(&v).unwrap(),
            AgentPlatform::Linux,
        ),
    };
    assert_eq!(
        install
            .verify(&f.proof, &p, 19, 100, &reply, &deadline())
            .unwrap_err(),
        PayloadError::Pending
    );
    let planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert!(facts.facts().activity.is_none());
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn adopted_matching_resource_is_retained_and_wrong_architecture_never_proposes_delta() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let path =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-outcome.json");
    let mut receipt: Value =
        serde_json::from_slice(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
    receipt["items"][8]["ownership"] = json!("Adopted");
    receipt["receipt"]["resources"][8]["ownership"] = json!("Adopted");
    f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&receipt).unwrap())
        .unwrap();
    let mut planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert_eq!(
        facts.facts().resources.as_ref().unwrap()[8].ownership,
        ResourceOwnership::Adopted
    );
    let plan = planner
        .plan(
            facts,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    let wrong = package_for(if Architecture::native().unwrap() == Architecture::X86_64 {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    });
    let facts = f.inventory(&planner, &wrong);
    assert_eq!(facts.facts().resources, Err(PayloadError::Invalid));
    let plan = planner
        .plan(
            facts,
            2,
            OperationId(2),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn changed_detection_retires_cached_matching_inventory_and_consent() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
    fs::write(icon, b"own injected administrator edit").unwrap();
    let _changed = f.inventory(&planner, &p);
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
}

#[test]
fn approved_package_and_exact_repair_delta_cannot_be_substituted() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
    fs::remove_file(icon).unwrap();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(plan.repair_delta().len(), 1);
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let substitute = package_with_icon(
        Architecture::native().unwrap(),
        b"different approved icon replacement",
    );
    let current = f.inventory(&planner, &substitute);
    assert_eq!(plan.facts().resources, current.facts().resources);
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &current,
                f.request(&substitute, &f.service(&substitute), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
}

#[test]
fn repair_and_uninstall_previews_state_different_typed_resource_actions() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let repair = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let removal = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_ne!(repair.preview(), removal.preview());
    assert!(repair.preview().contains("Repair"));
    assert!(removal.preview().contains("Uninstall"));
    assert!(repair.preview().contains("Retain"));
    assert!(removal.preview().contains("clean exit"));
    // This fixture deliberately has unknown service ownership, so it cannot bind clean tracking.
    assert!(removal.tracked().is_none());
    assert!(
        repair
            .actions()
            .iter()
            .all(|(_, a)| *a == ResourceAction::Retain)
    );
    assert!(removal.actions().iter().any(
        |(r, a)| r.resource_id == "bin/crosspane-agent" && *a == ResourceAction::RetainRecovery
    ));
    assert!(
        removal
            .actions()
            .iter()
            .any(|(r, a)| r.resource_id.ends_with(".svg") && *a == ResourceAction::Remove)
    );
}

#[test]
fn validation_itself_redetects_resource_drift_without_a_caller_refresh() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
    fs::write(&icon, b"own injected edit after cached detection").unwrap();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(
        fs::read(icon).unwrap(),
        b"own injected edit after cached detection"
    );
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn validation_binds_package_even_when_the_supplied_snapshot_is_unchanged() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
    fs::remove_file(&icon).unwrap();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    assert_eq!(cached.facts(), plan.facts());
    let replacement = package_with_icon(
        Architecture::native().unwrap(),
        b"different icon admitted at validation only",
    );
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&replacement, &f.service(&replacement), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert!(!icon.exists());
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn restored_detection_cannot_revive_permanently_retired_consent() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
    let original = fs::read(&icon).unwrap();
    fs::write(&icon, b"own injected temporary administrator edit").unwrap();
    let changed = f.inventory(&planner, &p);
    assert_ne!(changed.facts(), plan.facts());
    fs::write(&icon, &original).unwrap();
    let restored = f.inventory(&planner, &p);
    assert_eq!(
        restored.facts(),
        plan.facts(),
        "test must reconstruct identical detection meaning"
    );
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner.validate(
            &plan,
            &consent,
            &restored,
            f.request(&p, &f.service(&p), &deadline())
        ),
        Err(RemovalError::Stale)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    assert_eq!(fs::read(icon).unwrap(), original);
}
