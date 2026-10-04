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

mod cleanup_consent_tests {
    use super::*;
    use crosspane_installer::platform::linux::removal::executor::*;

    fn inventory(f: &Fixture) -> CleanupInventory {
        CleanupInventory::admit(f.io.admit_cleanup(&deadline()).unwrap(), &deadline()).unwrap()
    }
    fn edit_owned(f: &Fixture, path: &Path, bytes: &[u8]) {
        f.io.validate_target().unwrap();
        assert!(path.starts_with(&f.root));
        let parent = rustix::fs::open(
            path.parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut file = fs::File::from(
            rustix::fs::openat(
                &parent,
                path.file_name().unwrap(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::TRUNC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap(),
        );
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn progress() -> CleanupProgress {
        CleanupProgress {
            stage: CleanupStage::Prepared,
            resources: [CleanupResult::Pending; FILES.len()],
            autostart: CleanupResult::Pending,
            stop: CleanupResult::Pending,
            identity: CleanupResult::Kept,
            lan: CleanupResult::Kept,
            mdns: CleanupResult::Kept,
        }
    }
    fn cancelled() -> Deadline {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        Deadline::new(5000, cancellation).unwrap()
    }

    #[test]
    fn genuine_cleanup_inventory_retains_recovery_and_never_uses_receipt_ownership_alone() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let first = inventory(&f);
        assert_eq!(first.resources().len(), FILES.len());
        for (row, name) in first.resources().iter().zip(FILES) {
            assert_eq!(row.receipt.resource_id, name);
            assert_eq!(row.observation, ResourceObservation::Matching);
            assert!(row.owned);
            assert_eq!(
                row.action,
                if name.starts_with("bin/") || name.ends_with(".service") {
                    ResourceAction::RetainRecovery
                } else {
                    ResourceAction::Remove
                }
            );
        }
        let installer = PayloadInstaller::new(f.io.clone()).unwrap();
        let p = package();
        let repeat = installer
            .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
            .unwrap();
        installer.apply(&f.proof, &p, repeat, &deadline()).unwrap();
        installer
            .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
            .unwrap();
        for row in inventory(&f).resources() {
            assert_eq!(row.receipt.ownership, ResourceOwnership::Created);
            assert_eq!(row.observation, ResourceObservation::Matching);
            assert!(!row.owned);
            assert_eq!(row.action, ResourceAction::Retain);
        }
        assert!(
            f.io.metadata(
                &f.io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn cleanup_plan_is_immutable_and_absent_or_modified_rows_do_not_authorize_removal() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let targets = PayloadInstaller::new(f.io.clone()).unwrap();
        let path = &targets.targets()[8];
        edit_owned(&f, path, b"owned test edit");
        let changed = inventory(&f);
        assert_eq!(
            changed.resources()[8].observation,
            ResourceObservation::Different
        );
        assert_eq!(changed.resources()[8].action, ResourceAction::Retain);
        let parent = rustix::fs::open(
            path.parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        rustix::fs::unlinkat(
            &parent,
            path.file_name().unwrap(),
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
        let planner = CleanupPlanner::default();
        let admitted = inventory(&f);
        for (revision, operation) in [(0, 0), (0, 1), (1, 0)] {
            assert!(
                planner
                    .plan(
                        admitted.clone(),
                        revision,
                        OperationId(operation),
                        RemovalSelection::default()
                    )
                    .is_err()
            );
        }
        let plan = planner
            .plan(admitted, 1, OperationId(1), RemovalSelection::default())
            .unwrap();
        assert_eq!(plan.resources()[8].observation, ResourceObservation::Absent);
        assert_eq!(plan.resources()[8].action, ResourceAction::AlreadyAbsent);
        let mut detached = plan.resources()[0].clone();
        detached.action = ResourceAction::Remove;
        assert_eq!(plan.resources()[0].action, ResourceAction::RetainRecovery);
        assert_eq!(plan.selection(), RemovalSelection::default());
        assert_eq!(plan.form(), CleanupForm::NotCleanRetainIdentityAndRecovery);
        assert_eq!(plan.revision(), 1);
        assert_eq!(plan.operation(), OperationId(1));
    }

    #[test]
    fn superseded_coherent_consent_refuses_before_deadline_io_and_releases_cached_lease() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let old = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&old, 1, OperationId(1), &deadline())
            .unwrap();
        assert!(old.read_intent(&consent, &deadline()).unwrap().is_none());
        let written = old.write_intent(&consent, progress(), &deadline()).unwrap();
        let new = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let before = f.io.read(&path, MAX_RECORD_BYTES, true).unwrap();
        let commands = f.runner.calls.lock().unwrap().len();
        assert!(matches!(
            old.read_intent(&consent, &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            old.write_intent(&consent, progress(), &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            planner.consent(&old, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.io.read(&path, MAX_RECORD_BYTES, true).unwrap(), before);
        assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
        let renewed = planner
            .consent(&new, 2, OperationId(2), &deadline())
            .unwrap();
        // The old record remains diagnostic data; it never supplies this new consent.
        assert_eq!(
            new.read_intent(&renewed, &deadline()).unwrap(),
            Some(written)
        );
        assert_eq!(
            new.write_intent(&renewed, progress(), &deadline())
                .unwrap()
                .operation,
            OperationId(2)
        );
    }

    #[test]
    fn cross_controller_and_wrong_view_consents_never_open_native_lease() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let a = CleanupPlanner::default();
        let b = CleanupPlanner::default();
        let plan = a
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let other = b
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        assert!(matches!(
            b.consent(&plan, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        for (revision, op) in [(2, 1), (1, 2)] {
            assert!(matches!(
                a.consent(&plan, revision, OperationId(op), &cancelled()),
                Err(RemovalError::Stale)
            ));
        }
        let other_consent = b.consent(&other, 1, OperationId(1), &deadline()).unwrap();
        assert!(matches!(
            plan.write_intent(&other_consent, progress(), &cancelled()),
            Err(RemovalError::Stale)
        ));
        let parent = f.io.target().paths().state_home.join("crosspane/installer");
        assert!(
            f.io.metadata(&parent.join("cleanup-intent.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failed_consent_permanently_retires_cached_inventory_and_last_ids_survive() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let cached = inventory(&f);
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                cached.clone(),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let target = PayloadInstaller::new(f.io.clone()).unwrap().targets()[8].clone();
        let original = f.io.read(&target, MAX_MEMBER_BYTES, false).unwrap();
        edit_owned(&f, &target, b"changed test state");
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &deadline()),
            Err(RemovalError::Native(NativeError::Foreign))
        ));
        edit_owned(&f, &target, &original);
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        for (revision, op) in [(1, 1), (1, 2), (2, 1)] {
            assert!(
                planner
                    .plan(
                        cached.clone(),
                        revision,
                        OperationId(op),
                        RemovalSelection::default()
                    )
                    .is_err()
            );
        }
        let fresh = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        assert!(
            planner
                .consent(&fresh, 2, OperationId(2), &deadline())
                .is_ok()
        );
    }

    #[test]
    fn failed_native_access_retires_before_retry_and_new_consent_is_required() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&plan, 1, OperationId(1), &deadline())
            .unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &cancelled()),
            Err(RemovalError::Native(NativeError::Cancelled))
        ));
        assert!(matches!(
            plan.write_intent(&consent, progress(), &deadline()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &deadline()),
            Err(RemovalError::Stale)
        ));
        let renewed = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&renewed, 2, OperationId(2), &deadline())
            .unwrap();
        assert!(
            renewed
                .read_intent(&consent, &deadline())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn guarded_intent_binds_selection_operation_and_ledger_without_recreating_clean_exit() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let selection = RemovalSelection {
            identity: IdentityChoice::DeleteIdentityAndPairings,
            lan_rule: true,
            mdns_rule: true,
        };
        let admitted = inventory(&f);
        let plan = planner
            .plan(admitted.clone(), u64::MAX, OperationId(u64::MAX), selection)
            .unwrap();
        for (revision, operation) in [
            (u64::MAX, u64::MAX),
            (u64::MAX - 1, u64::MAX),
            (u64::MAX, u64::MAX - 1),
        ] {
            assert!(
                planner
                    .plan(
                        admitted.clone(),
                        revision,
                        OperationId(operation),
                        selection
                    )
                    .is_err()
            );
        }
        let consent = planner
            .consent(&plan, u64::MAX, OperationId(u64::MAX), &deadline())
            .unwrap();
        let record = plan
            .write_intent(&consent, progress(), &deadline())
            .unwrap();
        assert!(record.delete_identity && record.lan_rule && record.mdns_rule);
        assert_eq!(record.revision, u64::MAX);
        assert_eq!(record.operation, OperationId(u64::MAX));
        assert_eq!(
            record.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
        assert_eq!(
            plan.read_intent(&consent, &deadline()).unwrap(),
            Some(record.clone())
        );
        // A live plan retains its native installation lock and rejects file drift.
        let lease = f.io.admit_cleanup(&deadline()).unwrap().lease(&deadline());
        assert!(matches!(lease, Err(NativeError::Busy))); // current plan retains the installation lock.
        let parent = f.io.target().paths().state_home.join("crosspane/installer");
        let mut wrong = record;
        wrong.ledger_digest[0] ^= 1;
        f.io.atomic_write(
            &f.proof,
            &parent.join("cleanup-intent.json"),
            &wrong.encode().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Native(NativeError::Foreign))
        ));
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.erase_count(), 0);
    }

    #[test]
    fn observed_wrong_ledger_digest_retires_guard_even_when_native_snapshot_matches() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let wrong = CleanupIntent {
            revision: 77,
            operation: OperationId(88),
            ledger_digest: [0; 32],
            delete_identity: false,
            lan_rule: false,
            mdns_rule: false,
            progress: progress(),
        };
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        // Persist BEFORE capturing the lease: native identity validation must succeed.
        f.io.atomic_write(&f.proof, &path, &wrong.encode().unwrap())
            .unwrap();
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&plan, 1, OperationId(1), &deadline())
            .unwrap();
        let bytes = f.io.read(&path, MAX_RECORD_BYTES, true).unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            plan.write_intent(&consent, progress(), &deadline()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.io.read(&path, MAX_RECORD_BYTES, true).unwrap(), bytes);
        assert_eq!(f.erase_count(), 0);
    }

    #[test]
    fn each_cleanup_policy_debug_is_type_only_with_live_and_retired_binding() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let admitted = inventory(&f);
        let planner = CleanupPlanner::default();
        let selection = RemovalSelection {
            identity: IdentityChoice::DeleteIdentityAndPairings,
            lan_rule: true,
            mdns_rule: true,
        };
        let plan = planner
            .plan(
                admitted.clone(),
                8877665544,
                OperationId(1122334455),
                selection,
            )
            .unwrap();
        let consent = planner
            .consent(&plan, plan.revision(), plan.operation(), &deadline())
            .unwrap();
        plan.write_intent(&consent, progress(), &deadline())
            .unwrap();
        for (name, debug) in [
            ("CleanupResource", format!("{:?}", plan.resources()[0])),
            ("CleanupInventory", format!("{admitted:?}")),
            ("CleanupPlanner", format!("{planner:?}")),
            ("CleanupPlan", format!("{plan:?}")),
            ("CleanupConsent", format!("{consent:?}")),
        ] {
            assert_eq!(debug, format!("{name}(..)"));
            for forbidden in [
                f.root.to_str().unwrap(),
                "resources",
                "ledger_digest",
                "DeleteIdentityAndPairings",
                "1122334455",
                "8877665544",
                "active",
                "lease",
            ] {
                assert!(!debug.contains(forbidden), "{name} exposed {forbidden}");
            }
        }
        assert!(plan.read_intent(&consent, &cancelled()).is_err());
        assert_eq!(format!("{planner:?}"), "CleanupPlanner(..)");
        assert_eq!(format!("{plan:?}"), "CleanupPlan(..)");
        assert_eq!(format!("{consent:?}"), "CleanupConsent(..)");
    }
}

mod cleanup_intent_tests {
    use super::*;
    use crosspane_installer::platform::linux::removal::executor::*;

    fn literal() -> Vec<u8> {
        br#"{"schema_version":1,"revision":3,"operation":4,"ledger_digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"delete_identity":false,"lan_rule":true,"mdns_rule":false,"stage":0,"resources":[0,1,2,3,4,5,6,0,0,0],"autostart":1,"stop":6,"identity":3,"lan":4,"mdns":2}"#.to_vec()
    }
    fn record() -> CleanupIntent {
        CleanupIntent::decode(&literal()).unwrap()
    }
    fn lease(f: &Fixture) -> CleanupLease {
        installed(f, &package());
        f.io.admit_cleanup(&deadline())
            .unwrap()
            .lease(&deadline())
            .unwrap()
    }

    #[test]
    fn literal_results_roundtrip_and_resume_never_recreate_clean_authority() {
        let intent = record();
        assert_eq!(intent.revision, 3);
        assert_eq!(intent.operation, OperationId(4));
        assert!(!intent.delete_identity);
        assert!(intent.lan_rule);
        assert!(!intent.mdns_rule);
        assert_eq!(intent.progress.stage, CleanupStage::Prepared);
        assert_eq!(
            intent.progress.resources,
            [
                CleanupResult::Pending,
                CleanupResult::Removed,
                CleanupResult::AlreadyAbsent,
                CleanupResult::Kept,
                CleanupResult::Refused,
                CleanupResult::Failed,
                CleanupResult::Unknown,
                CleanupResult::Pending,
                CleanupResult::Pending,
                CleanupResult::Pending
            ]
        );
        assert_eq!(intent.progress.autostart, CleanupResult::Removed);
        assert_eq!(intent.progress.stop, CleanupResult::Unknown);
        assert_eq!(intent.progress.identity, CleanupResult::Kept);
        assert_eq!(intent.progress.lan, CleanupResult::Refused);
        assert_eq!(intent.progress.mdns, CleanupResult::AlreadyAbsent);
        assert_eq!(
            intent.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
        assert_eq!(
            CleanupIntent::decode(&intent.encode().unwrap()).unwrap(),
            intent
        );
        let mut explicit = intent.clone();
        explicit.delete_identity = true;
        assert_eq!(
            CleanupIntent::decode(&explicit.encode().unwrap())
                .unwrap()
                .form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
    }

    #[test]
    fn intent_requires_every_field_unique_object_and_exact_fixed_array_types() {
        let original: Value = serde_json::from_slice(&literal()).unwrap();
        for field in original.as_object().unwrap().keys() {
            let mut value = original.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                matches!(
                    CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                    Err(RemovalError::Invalid)
                ),
                "missing {field}"
            );
            let member = format!("\"{field}\":{}", original[field]);
            let text = serde_json::to_string(&original).unwrap();
            let duplicate = text.replacen(&member, &format!("{member},{member}"), 1);
            let _: Value = serde_json::from_str(&duplicate).unwrap();
            assert!(
                matches!(
                    CleanupIntent::decode(duplicate.as_bytes()),
                    Err(RemovalError::Invalid)
                ),
                "duplicate {field}"
            );
        }
        let mut variants = vec![json!([]), Value::Null];
        let mut unknown = original.clone();
        unknown["unknown"] = json!(true);
        variants.push(unknown);
        for field in ["ledger_digest", "resources"] {
            for invalid in [json!([]), json!({}), Value::Null, json!([256]), json!([-1])] {
                let mut value = original.clone();
                value[field] = invalid;
                variants.push(value);
            }
            let mut extra = original.clone();
            extra[field].as_array_mut().unwrap().push(json!(0));
            variants.push(extra);
            for invalid in [
                json!(-1),
                json!(256),
                json!(0.5),
                json!("0"),
                json!(true),
                Value::Null,
                json!({}),
            ] {
                let mut element = original.clone();
                element[field][0] = invalid;
                variants.push(element);
            }
        }
        for value in variants {
            assert!(matches!(
                CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                Err(RemovalError::Invalid)
            ));
        }
        let mut two = literal();
        two.extend(literal());
        assert!(matches!(
            CleanupIntent::decode(&two),
            Err(RemovalError::Invalid)
        ));
        assert!(matches!(
            CleanupIntent::decode(&vec![b' '; MAX_RECORD_BYTES + 1]),
            Err(RemovalError::Invalid)
        ));
    }

    #[test]
    fn intent_scalar_boundaries_and_each_closed_result_and_stage_are_exact() {
        let original: Value = serde_json::from_slice(&literal()).unwrap();
        for field in [
            "schema_version",
            "revision",
            "operation",
            "stage",
            "autostart",
            "stop",
            "identity",
            "lan",
            "mdns",
        ] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!(true),
                json!("1"),
                Value::Null,
                json!({"Prepared":null}),
            ] {
                let mut value = original.clone();
                value[field] = invalid;
                assert!(
                    matches!(
                        CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                        Err(RemovalError::Invalid)
                    ),
                    "{field}"
                );
            }
        }
        for field in ["delete_identity", "lan_rule", "mdns_rule"] {
            for invalid in [json!(0), json!("false"), Value::Null] {
                let mut value = original.clone();
                value[field] = invalid;
                assert!(
                    matches!(
                        CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                        Err(RemovalError::Invalid)
                    ),
                    "{field}"
                );
            }
        }
        for (field, invalid) in [
            ("schema_version", 2),
            ("revision", 0),
            ("operation", 0),
            ("stage", 7),
            ("autostart", 7),
            ("stop", 7),
            ("identity", 7),
            ("lan", 7),
            ("mdns", 7),
        ] {
            let mut value = original.clone();
            value[field] = json!(invalid);
            assert!(
                matches!(
                    CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                    Err(RemovalError::Invalid)
                ),
                "{field}"
            );
        }
        let mut value = original.clone();
        value["resources"][9] = json!(7);
        assert!(matches!(
            CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
            Err(RemovalError::Invalid)
        ));
        value = original.clone();
        value["revision"] = json!(u64::MAX);
        value["operation"] = json!(u64::MAX);
        let decoded = CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.revision, u64::MAX);
        assert_eq!(decoded.operation, OperationId(u64::MAX));
        for (code, stage) in [
            CleanupStage::Prepared,
            CleanupStage::Disabled,
            CleanupStage::StopObserved,
            CleanupStage::IdentityObserved,
            CleanupStage::FirewallObserved,
            CleanupStage::FilesObserved,
            CleanupStage::Finished,
        ]
        .into_iter()
        .enumerate()
        {
            value["stage"] = json!(code);
            assert_eq!(
                CleanupIntent::decode(&serde_json::to_vec(&value).unwrap())
                    .unwrap()
                    .progress
                    .stage,
                stage
            );
        }
        let overflow = String::from_utf8(literal())
            .unwrap()
            .replace("\"operation\":4", "\"operation\":18446744073709551616");
        assert!(matches!(
            CleanupIntent::decode(overflow.as_bytes()),
            Err(RemovalError::Invalid)
        ));
        let mut invalid = record();
        invalid.revision = 0;
        assert!(matches!(invalid.encode(), Err(RemovalError::Invalid)));
    }

    #[test]
    fn lease_store_reads_fresh_absence_and_atomic_replacements_without_commands() {
        let f = Fixture::new(false);
        let lease = lease(&f);
        let commands = f.runner.calls.lock().unwrap().len();
        assert_eq!(lease.read_intent(&deadline()).unwrap(), None);
        let store = CleanupStore::new(lease.clone());
        assert_eq!(store.read(&deadline()).unwrap(), None);
        let mut intent = record();
        store.write(&intent, &deadline()).unwrap();
        assert_eq!(store.read(&deadline()).unwrap(), Some(intent.clone()));
        intent.progress.stage = CleanupStage::Disabled;
        intent.progress.autostart = CleanupResult::Unknown;
        store.write(&intent, &deadline()).unwrap();
        assert_eq!(store.read(&deadline()).unwrap(), Some(intent.clone()));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(intent.encode().unwrap())
        );
        assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
        for target in PayloadInstaller::new(f.io.clone()).unwrap().targets() {
            assert!(target.is_file());
        }
        assert_eq!(
            intent.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
    }

    #[test]
    fn lease_read_rejects_intent_swap_symlink_mode_hardlink_and_oversize() {
        for change in 0..5 {
            let f = Fixture::new(false);
            let lease = lease(&f);
            lease.write_intent(&literal(), &deadline()).unwrap();
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            let parent = rustix::fs::open(
                path.parent().unwrap(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            let file = rustix::fs::openat(
                &parent,
                "cleanup-intent.json",
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            match change {
                0 => {
                    rustix::fs::renameat(
                        &parent,
                        "cleanup-intent.json",
                        &parent,
                        "kept-old-intent",
                    )
                    .unwrap();
                    let fd = rustix::fs::openat(
                        &parent,
                        "cleanup-intent.json",
                        rustix::fs::OFlags::WRONLY
                            | rustix::fs::OFlags::CREATE
                            | rustix::fs::OFlags::EXCL
                            | rustix::fs::OFlags::NOFOLLOW,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                    )
                    .unwrap();
                    fs::File::from(fd).write_all(&literal()).unwrap();
                }
                1 => {
                    rustix::fs::renameat(
                        &parent,
                        "cleanup-intent.json",
                        &parent,
                        "kept-old-intent",
                    )
                    .unwrap();
                    rustix::fs::symlinkat("kept-old-intent", &parent, "cleanup-intent.json")
                        .unwrap();
                }
                2 => {
                    rustix::fs::fchmod(&file, rustix::fs::Mode::from_bits_truncate(0o644)).unwrap()
                }
                3 => rustix::fs::linkat(
                    &parent,
                    "cleanup-intent.json",
                    &parent,
                    "other-link",
                    rustix::fs::AtFlags::empty(),
                )
                .unwrap(),
                4 => fs::File::from(file)
                    .write_all(&vec![b' '; MAX_RECORD_BYTES + 1])
                    .unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(
                lease.read_intent(&deadline()).unwrap_err(),
                NativeError::Foreign,
                "change {change}"
            );
        }
    }

    #[test]
    fn store_invalid_record_and_cancelled_read_preserve_last_durable_bytes() {
        let f = Fixture::new(false);
        let lease = lease(&f);
        let store = CleanupStore::new(lease.clone());
        store.write(&record(), &deadline()).unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(matches!(
            store.read(&Deadline::new(5000, cancellation).unwrap()),
            Err(RemovalError::Native(NativeError::Cancelled))
        ));
        let mut invalid = record();
        invalid.operation = OperationId(0);
        assert!(matches!(
            store.write(&invalid, &deadline()),
            Err(RemovalError::Invalid)
        ));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(record().encode().unwrap())
        );
        lease
            .write_intent(b"malformed observations", &deadline())
            .unwrap();
        assert!(matches!(
            store.read(&deadline()),
            Err(RemovalError::Invalid)
        ));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(b"malformed observations".to_vec())
        );
    }

    #[test]
    fn lease_read_preserves_captured_ancestry_and_refuses_new_uncaptured_intent() {
        for replace_parent in [false, true] {
            let f = Fixture::new(false);
            let lease = lease(&f);
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            if replace_parent {
                lease.write_intent(&literal(), &deadline()).unwrap();
                let ancestor = path.parent().unwrap().parent().unwrap();
                let root = rustix::fs::open(
                    ancestor.parent().unwrap(),
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .unwrap();
                rustix::fs::renameat(&root, "crosspane", &root, "retained-crosspane").unwrap();
                rustix::fs::mkdirat(&root, "crosspane", rustix::fs::Mode::RWXU).unwrap();
            } else {
                f.io.atomic_write(&f.proof, &path, &literal()).unwrap();
            }
            assert_eq!(
                lease.read_intent(&deadline()).unwrap_err(),
                NativeError::Foreign
            );
        }
    }
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
fn cleanup_mutations_never_delete_genuine_repeat_resources_without_replacement_provenance() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = installer
        .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
        .unwrap();
    installer.apply(&f.proof, &p, plan, &deadline()).unwrap();
    installer
        .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
        .unwrap();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    let previous_calls = f.runner.calls.lock().unwrap().len();
    for (index, path) in installer.targets().iter().enumerate() {
        let bytes = fs::read(path).unwrap();
        assert!(!proof.owned(index).unwrap());
        assert_eq!(lease.delete(index, &deadline()), Ok(false));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(f.runner.calls.lock().unwrap().len(), previous_calls);
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
fn cleanup_mutation_lease_locks_writes_only_intent_and_tracks_its_exact_removals() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    assert!(matches!(proof.lease(&deadline()), Err(NativeError::Busy)));
    let intent =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/cleanup-intent.json");
    lease
        .write_intent(b"private first intent", &deadline())
        .unwrap();
    lease
        .write_intent(b"private replacement intent", &deadline())
        .unwrap();
    assert_eq!(fs::read(&intent).unwrap(), b"private replacement intent");
    let first = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let second = PathBuf::from(&proof.receipt().resources[9].resolved_path);
    assert!(lease.delete(8, &deadline()).unwrap());
    assert!(!lease.delete(8, &deadline()).unwrap());
    assert!(lease.delete(9, &deadline()).unwrap());
    assert!(!first.exists() && !second.exists());
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    lease
        .write_intent(b"private completed mutations", &deadline())
        .unwrap();
    assert!(f.io.target().agent_path().is_file());
    drop(lease);
    let renewed =
        f.io.admit_cleanup(&deadline())
            .unwrap()
            .lease(&deadline())
            .unwrap();
    assert!(!renewed.delete(8, &deadline()).unwrap());
}

#[test]
fn cleanup_delete_revalidates_admitted_identity_and_retains_foreign_replacements() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let bytes = fs::read(&path).unwrap();
    use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};
    let parent = rfs::open(
        path.parent().unwrap(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .unwrap();
    let mut file = fs::File::from(
        rfs::openat(
            &parent,
            ".owned-test-replacement",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap(),
    );
    file.write_all(&bytes).unwrap();
    rfs::fchmod(&file, Mode::from_bits_truncate(0o644)).unwrap();
    rfs::renameat(
        &parent,
        ".owned-test-replacement",
        &parent,
        path.file_name().unwrap(),
    )
    .unwrap();
    assert!(
        rfs::statat(
            &parent,
            path.file_name().unwrap(),
            AtFlags::SYMLINK_NOFOLLOW
        )
        .is_ok()
    );
    assert_eq!(lease.delete(8, &deadline()), Err(NativeError::Foreign));
    assert_eq!(fs::read(&path).unwrap(), bytes);
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
