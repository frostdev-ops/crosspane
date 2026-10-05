#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*},
};
use crosspane_installer_core::{MutationOutcome, OperationId, ResourceOwnership};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
fn stopped_runtime(f: &Fixture, pid: u32) -> std::os::unix::net::UnixListener {
    f.io.create_private_dir(&f.proof, f.io.target().runtime_dir())
        .unwrap();
    let socket = f.io.target().socket_path();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let bytes = serde_json::to_vec(&json!({"schema_version":1,"instance_id":9,"pid":pid,
        "started_unix_ms":1,"phase":"ready","phase_seq":2,"keystore":"os_store",
        "reason":null,"runtime_dir":f.io.target().runtime_dir()}))
    .unwrap();
    f.io.atomic_write(
        &f.proof,
        &f.io.target().runtime_dir().join("bootstrap.json"),
        &bytes,
    )
    .unwrap();
    listener
}
#[test]
fn exact_dead_runtime_is_a_readonly_observation_until_locked_payload_apply() {
    let f = Fixture::new();
    drop(stopped_runtime(&f, i32::MAX as u32));
    let p = package(1);
    assert!(f.install.detect(&f.proof, &p).is_ok());
    assert!(f.io.target().socket_path().exists());
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
        .unwrap();
    f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert!(!f.io.target().socket_path().exists());
    assert!(!f.io.target().runtime_dir().join("bootstrap.json").exists());
    assert!(f.io.target().runtime_dir().exists());
    assert!(f.io.target().agent_path().exists());
}
#[test]
fn live_reused_pid_or_connectable_runtime_is_never_removed() {
    for live_pid in [false, true] {
        let f = Fixture::new();
        let listener = stopped_runtime(
            &f,
            if live_pid {
                std::process::id()
            } else {
                i32::MAX as u32
            },
        );
        let _listener = if live_pid {
            drop(listener);
            None
        } else {
            Some(listener)
        };
        assert!(f.install.detect(&f.proof, &package(1)).is_err());
        assert!(f.io.target().socket_path().exists());
        assert!(f.io.target().runtime_dir().join("bootstrap.json").exists());
        assert!(!f.io.target().agent_path().exists());
    }
}
#[test]
fn dead_runtime_drift_foreign_leaves_and_unsafe_permissions_refuse() {
    for change in 0..5 {
        let f = Fixture::new();
        drop(stopped_runtime(&f, i32::MAX as u32));
        let p = package(1);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
            .unwrap();
        let bootstrap = f.io.target().runtime_dir().join("bootstrap.json");
        let socket = f.io.target().socket_path();
        match change {
            0 => {
                fs::remove_file(&socket).unwrap();
                drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
                fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
            }
            1 => {
                fs::hard_link(&bootstrap, f.io.target().runtime_dir().join("foreign")).unwrap();
            }
            2 => {
                fs::set_permissions(&bootstrap, fs::Permissions::from_mode(0o644)).unwrap();
            }
            3 => {
                put(
                    &f.io.target().runtime_dir().join("unrecognised"),
                    b"keep",
                    0o600,
                );
            }
            _ => {
                let mut value: Value =
                    serde_json::from_slice(&fs::read(&bootstrap).unwrap()).unwrap();
                value["runtime_dir"] = json!("/foreign");
                put(&bootstrap, &serde_json::to_vec(&value).unwrap(), 0o600);
            }
        }
        assert!(f.install.apply(&f.proof, &p, plan, &deadline()).is_err());
        assert!(socket.exists());
        assert!(bootstrap.exists());
        assert!(!f.io.target().agent_path().exists());
    }
}
static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
// Producer literal; only explicit scratch coordinates and the staged build version vary.
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
struct Runner;
impl CommandRunner for Runner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        assert_eq!(c.executable(), Path::new("/bin/ps"));
        Ok(CommandOutput {
            code: Some(0),
            stdout: if c.argv()[1] == "lstart=" {
                START.to_vec()
            } else {
                b"crosspane-agent\n".to_vec()
            },
            stderr: vec![],
        })
    }
}
struct Probe {
    root: PathBuf,
    inode: Mutex<Option<u64>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        let target = self.root.join(".local/bin/crosspane-agent");
        let inode = *self.inode.lock().unwrap();
        let executable = if inode.is_some_and(|old| fs::metadata(&target).unwrap().ino() != old) {
            fs::read_dir(target.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| fs::metadata(p).is_ok_and(|m| Some(m.ino()) == inode))
                // Moved out of the install folder altogether (into a backup): never the agent path.
                .unwrap_or_else(|| self.root.join("moved-away"))
        } else {
            target
        };
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable,
            generation: 77,
        })
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
    install: PayloadInstaller,
    proof: SupportProof,
    probe: Arc<Probe>,
}
impl Fixture {
    fn new() -> Self {
        Self::named("")
    }
    fn named(suffix: &str) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp47b-{}-{}{suffix}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe {
            root: root.clone(),
            inode: Mutex::new(None),
        });
        let io = Arc::new(LinuxNativeIo::scratch(&root, Arc::new(Runner), probe.clone()).unwrap());
        let proof = io.scratch_support(facts(&io)).unwrap();
        io.create_private_dir(&proof, &io.target().paths().runtime_home)
            .unwrap();
        let install = PayloadInstaller::new(io.clone()).unwrap();
        Self {
            root,
            io,
            install,
            proof,
            probe,
        }
    }
    fn bootstrap(&self) {
        self.bootstrap_instance(9);
    }
    fn bootstrap_instance(&self, instance: u64) {
        *self.probe.inode.lock().unwrap() = fs::metadata(self.io.target().agent_path())
            .ok()
            .map(|m| m.ino());
        self.io
            .create_private_dir(&self.proof, self.io.target().runtime_dir())
            .unwrap();
        let bytes = serde_json::to_vec(&json!({"schema_version":1,"instance_id":instance,"pid":4242,"started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":self.io.target().runtime_dir()})).unwrap();
        self.io
            .atomic_write(
                &self.proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &bytes,
            )
            .unwrap();
    }
    fn reply(&self, version: &str) -> AgentReply {
        self.reply_with(version, |_| {})
    }
    fn reply_with(&self, version: &str, change: impl FnOnce(&mut Value)) -> AgentReply {
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let h = &mut v["result"]["installer"];
        h["build"]["version"] = json!(version);
        h["instance"]["uid"] = json!(self.io.target().paths().uid);
        h["instance"]["exe"] = json!(self.io.target().agent_path());
        h["instance"]["runtime_dir"] = json!(self.io.target().runtime_dir());
        h["instance"]["started_unix_ms"] = json!(parse_ps_start(START).unwrap());
        if let Ok(bytes) = fs::read(self.io.target().runtime_dir().join("bootstrap.json")) {
            let bootstrap: Value = serde_json::from_slice(&bytes).unwrap();
            h["instance"]["id"] = bootstrap["instance_id"].clone();
        }
        change(h);
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
    fn applied(&self, p: &Package, op: u64) {
        let plan = self
            .install
            .plan(&self.proof, p, OperationId(op), MatchingFiles::Preserve)
            .unwrap();
        self.install
            .apply(&self.proof, p, plan, &deadline())
            .unwrap();
    }
    fn verified(&self, p: &Package, op: u64) {
        self.applied(p, op);
        self.bootstrap();
        let r = self
            .install
            .verify(
                &self.proof,
                p,
                19,
                100,
                &self.reply(&p.manifest().product_version),
                &deadline(),
            )
            .unwrap();
        assert!(r.unfinished.is_empty());
        assert!(
            r.resources
                .iter()
                .all(|r| r.outcome == MutationOutcome::Verified)
        );
    }
    fn record(&self, intent: bool) -> PathBuf {
        self.io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer")
            .join(if intent {
                "payload-intent.json"
            } else {
                "payload-outcome.json"
            })
    }
    fn sibling(&self, op: u64, index: usize, backup: bool) -> PathBuf {
        self.install.targets()[index].with_file_name(format!(
            ".crosspane-{}-{op}-{index}",
            if backup { "previous" } else { "stage" }
        ))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn hex(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn elf(version: u8) -> Vec<u8> {
    let mut v = vec![0; 64];
    v[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    v[16..18].copy_from_slice(&3u16.to_le_bytes());
    let machine: u16 = if Architecture::native().unwrap() == Architecture::X86_64 {
        62
    } else {
        183
    };
    v[18..20].copy_from_slice(&machine.to_le_bytes());
    v[20] = 1;
    v[52] = 64;
    v[63] = version;
    v
}
fn contents(version: u8) -> Vec<Vec<u8>> {
    (0..FILES.len())
        .map(|i| {
            if i < 4 {
                elf(version)
            } else if i == 4 {
                format!("[Service]\nExecStart={{{{agent_executable}}}} run\nEnvironment={{{{xdg_config_environment}}}}\nEnvironment={{{{xdg_state_environment}}}}\nEnvironment={{{{xdg_runtime_environment}}}}\nEnvironment={{{{crosspane_runtime_environment}}}}\n# fixture-version-{version}\n").into_bytes()
            } else if i < 7 {
                format!("[Desktop Entry]\nType=Application\nName=Crosspane\nExec={{{{{}_executable}}}}\n# fixture-version-{version}\n", if i == 5 { "settings" } else { "installer" }).into_bytes()
            } else {
                format!("inert-resource-{i}-{version}\n").into_bytes()
            }
        })
        .collect()
}
fn installed_contents(f: &Fixture, version: u8) -> Vec<Vec<u8>> {
    let mut result = contents(version);
    for (index, resource) in f
        .install
        .rendered_resources(&package(version))
        .unwrap()
        .into_iter()
        .enumerate()
    {
        result[index + 4] = resource.bytes;
    }
    result
}
fn manifest(version: u8, bytes: &[Vec<u8>]) -> Manifest {
    Manifest {
        schema_version: 1,
        product_version: format!("0.0.{version}"),
        architecture: Architecture::native().unwrap(),
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf(version)),
        }],
        members: FILES
            .iter()
            .zip(bytes)
            .enumerate()
            .map(|(i, (name, b))| Artifact {
                name: (*name).into(),
                size: b.len(),
                sha256: hex(b),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    }
}
fn number(h: &mut [u8], start: usize, width: usize, n: usize) {
    let s = format!("{n:0width$o}\0", width = width - 1);
    h[start..start + width].copy_from_slice(s.as_bytes());
}
fn checksum(h: &mut [u8]) {
    h[148..156].fill(b' ');
    let n: usize = h.iter().map(|b| usize::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{n:06o}\0 ").as_bytes());
}
fn member(name: &str, b: &[u8]) -> Vec<u8> {
    let mut h = vec![0; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    number(
        &mut h,
        100,
        8,
        if name.starts_with("bin/") {
            0o755
        } else {
            0o644
        },
    );
    number(&mut h, 108, 8, 0);
    number(&mut h, 116, 8, 0);
    number(&mut h, 124, 12, b.len());
    number(&mut h, 136, 12, 0);
    h[156] = b'0';
    h[257..265].copy_from_slice(b"ustar\x0000");
    checksum(&mut h);
    h.extend_from_slice(b);
    h.resize(h.len().div_ceil(512) * 512, 0);
    h
}
fn archive(m: &Manifest, data: &[Vec<u8>]) -> Vec<u8> {
    let mut a = member("manifest.json", &serde_json::to_vec(m).unwrap());
    for (n, b) in FILES.iter().zip(data) {
        a.extend(member(n, b));
    }
    a.extend(vec![0; 1024]);
    a
}
#[test]
fn current_payload_accepts_no_tutorial_inventory() {
    let data = contents(1);
    let mut m = manifest(1, &data);
    let selected: Vec<_> = m
        .members
        .iter()
        .cloned()
        .zip(data)
        .filter(|(artifact, _)| artifact.name != "bin/crosspane-tutorial")
        .collect();
    m.members = selected
        .iter()
        .map(|(artifact, _)| artifact.clone())
        .collect();
    let mut a = member("manifest.json", &serde_json::to_vec(&m).unwrap());
    for (artifact, bytes) in selected {
        a.extend(member(&artifact.name, &bytes));
    }
    a.extend(vec![0; 1024]);
    let current = read(&a).unwrap();
    assert_eq!(current.manifest().members.len(), 9);
    assert!(
        current
            .manifest()
            .members
            .iter()
            .all(|artifact| artifact.name != "bin/crosspane-tutorial")
    );
}

#[test]
fn current_payload_rejects_installing_the_retired_tutorial() {
    let mut data = contents(1);
    let mut m = manifest(1, &data);
    if !m
        .members
        .iter()
        .any(|artifact| artifact.name == "bin/crosspane-tutorial")
    {
        let bytes = elf(1);
        m.members.insert(
            4,
            Artifact {
                name: "bin/crosspane-tutorial".into(),
                size: bytes.len(),
                sha256: hex(&bytes),
                features: vec![],
            },
        );
        data.insert(4, bytes);
    }
    let mut a = member("manifest.json", &serde_json::to_vec(&m).unwrap());
    for (artifact, bytes) in m.members.iter().zip(data) {
        a.extend(member(&artifact.name, &bytes));
    }
    a.extend(vec![0; 1024]);
    assert!(read(&a).is_err());
}

// V1 a3aafbfb: exact ten-member journal, Tutorial at index 4. The receipt alone is not
// authority: this fixture records the actual scratch leaf inode/parent/hash, as V1 did.
fn legacy_v1(f: &Fixture) -> (PathBuf, Vec<u8>) {
    let path = f.io.target().paths().prefix.join("bin/crosspane-tutorial");
    let bytes = elf(1);
    put(&path, &bytes, 0o755);
    let meta = fs::metadata(&path).unwrap();
    let parent = fs::metadata(path.parent().unwrap()).unwrap();
    let mut journal: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    let mut row = journal["receipt"]["resources"][0].clone();
    row["resource_id"] = json!("bin/crosspane-tutorial");
    row["resolved_path"] = json!(path);
    let mut item = journal["items"][0].clone();
    let hash = sha256(&bytes);
    item["new"] = json!(hash);
    item["template"] = json!(hash);
    item["replacement"] = json!({"file":[meta.dev(),meta.ino()],
        "parent":[parent.dev(),parent.ino()],"hash":hash,"mode":493});
    journal["receipt"]["resources"]
        .as_array_mut()
        .unwrap()
        .insert(4, row);
    journal["items"].as_array_mut().unwrap().insert(4, item);
    // Old package hashes are advisory here and never make the retired executable installable.
    journal["receipt"]["manifest_sha256"] = json!([44; 32].to_vec());
    journal["receipt"]["payload_sha256"] = json!([45; 32].to_vec());
    put(
        &f.record(false),
        &serde_json::to_vec(&journal).unwrap(),
        0o600,
    );
    (path, bytes)
}
#[test]
fn legacy_v1_journal_upgrade_backs_up_and_removes_tutorial() {
    let f = Fixture::new();
    f.verified(&package(1), 1);
    let (obsolete, bytes) = legacy_v1(&f);
    let detected = f.install.detect(&f.proof, &package(2)).unwrap();
    assert_eq!(detected.len(), 10);
    assert_eq!(
        detected.last().unwrap().resource_id,
        "bin/crosspane-tutorial"
    );
    use crosspane_installer::platform::linux::integration::{NativePayloads, Payloads};
    let next = package(2);
    let mut native = NativePayloads::new(f.io.clone()).unwrap();
    assert!(
        native
            .plan(&f.proof, &next, OperationId(2), false)
            .unwrap()
            .replacing
    );
    native
        .apply(&f.proof, &next, OperationId(2), &deadline())
        .unwrap();
    let backup = fs::read_dir(f.io.target().paths().state_home.join("crosspane/backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.join("crosspane-tutorial").exists())
        .unwrap();
    assert!(!obsolete.exists());
    assert_eq!(fs::read(backup.join("crosspane-tutorial")).unwrap(), bytes);
    f.bootstrap_instance(10);
    f.install
        .verify(
            &f.proof,
            &next,
            19,
            100,
            &f.reply(&next.manifest().product_version),
            &deadline(),
        )
        .unwrap();
    assert!(!obsolete.exists());
    assert_eq!(f.install.detect(&f.proof, &package(2)).unwrap().len(), 9);
}
#[test]
fn legacy_v1_matching_current_files_still_require_obsolete_cleanup() {
    let f = Fixture::new();
    let current = package(1);
    f.verified(&current, 1);
    legacy_v1(&f);
    let rows = f.install.detect(&f.proof, &current).unwrap();
    assert!(
        rows[..9]
            .iter()
            .all(|r| r.before == crosspane_installer_core::ResourceObservation::Matching)
    );
    assert_eq!(
        rows[9].before,
        crosspane_installer_core::ResourceObservation::Different
    );
    assert!(matches!(
        f.install
            .plan(&f.proof, &current, OperationId(2), MatchingFiles::Preserve),
        Err(PayloadError::Foreign)
    ));
}
#[test]
fn legacy_v1_absent_tutorial_is_obsolete_not_missing() {
    let f = Fixture::new();
    f.verified(&package(1), 1);
    let (obsolete, _) = legacy_v1(&f);
    fs::remove_file(obsolete).unwrap();
    let detected = f.install.detect(&f.proof, &package(1)).unwrap();
    assert_eq!(detected.len(), 9);
    assert!(
        detected
            .iter()
            .all(|r| r.before != crosspane_installer_core::ResourceObservation::Absent)
    );
    assert!(f.install.recorded(&f.proof).unwrap());
}
#[test]
fn legacy_v1_wrong_position_path_or_count_is_refused() {
    for change in 0..3 {
        let f = Fixture::new();
        f.verified(&package(1), 1);
        legacy_v1(&f);
        let mut journal: Value =
            serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
        match change {
            0 => {
                journal["receipt"]["resources"]
                    .as_array_mut()
                    .unwrap()
                    .swap(3, 4);
                journal["items"].as_array_mut().unwrap().swap(3, 4);
            }
            1 => journal["receipt"]["resources"][4]["resolved_path"] = json!(f.root.join("wrong")),
            _ => {
                journal["receipt"]["resources"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
        }
        put(
            &f.record(false),
            &serde_json::to_vec(&journal).unwrap(),
            0o600,
        );
        assert!(
            f.install.detect(&f.proof, &package(1)).is_err(),
            "case {change}"
        );
    }
}

fn package(version: u8) -> Package {
    let data = contents(version);
    let a = archive(&manifest(version, &data), &data);
    read(&a).unwrap()
}
fn read(a: &[u8]) -> Result<Package, PayloadError> {
    Package::read(a, Architecture::native().unwrap(), sha256(a))
}
fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn bounded_archive_rejects_adversarial_members_and_framing() {
    let data = contents(1);
    let a = archive(&manifest(1, &data), &data);
    assert!(read(&a).is_ok());
    for kind in [
        b'1', b'2', b'3', b'4', b'5', b'6', b'S', b'x', b'g', b'L', b'K', 0,
    ] {
        let mut b = a.clone();
        b[156] = kind;
        checksum(&mut b[..512]);
        assert!(read(&b).is_err(), "type {kind}");
    }
    for name in [
        "/manifest.json",
        "../manifest.json",
        "resources/../manifest.json",
        "manifest\n.json",
        "extra",
        "./manifest.json",
    ] {
        let mut b = a.clone();
        b[..100].fill(0);
        b[..name.len()].copy_from_slice(name.as_bytes());
        checksum(&mut b[..512]);
        assert!(read(&b).is_err());
    }
    for at in [157, 265, 329, 345, 500] {
        let mut b = a.clone();
        b[at] = 1;
        checksum(&mut b[..512]);
        assert!(read(&b).is_err());
    }
    let mut b = a.clone();
    b[148] ^= 1;
    assert!(read(&b).is_err());
    for size in [0, MAX_RECORD_BYTES + 1, usize::MAX >> 4] {
        let mut b = a.clone();
        if size < 1 << 33 {
            number(&mut b, 124, 12, size);
            checksum(&mut b[..512]);
            assert!(read(&b).is_err());
        }
    }
    let mut b = a.clone();
    number(&mut b, 100, 8, 0o4755);
    checksum(&mut b[..512]);
    assert!(read(&b).is_err());
    let mut b = a.clone();
    b.truncate(b.len() - 512);
    assert!(read(&b).is_err());
    let mut b = a.clone();
    *b.last_mut().unwrap() = 1;
    assert!(read(&b).is_err());
    let mut b = a.clone();
    b.splice(b.len() - 1024..b.len() - 1024, member(FILES[0], &data[0]));
    assert!(read(&b).is_err());
    let mut b = a.clone();
    b.splice(b.len() - 1024..b.len() - 1024, member("extra", b"x"));
    assert!(read(&b).is_err());
    let m = serde_json::to_vec(&manifest(1, &data)).unwrap();
    let mut b = a.clone();
    b[512 + m.len()] = 1;
    assert!(read(&b).is_err());
    assert!(Package::read(a.as_slice(), Architecture::native().unwrap(), [0; 32]).is_err());
}
#[test]
fn provenance_binds_every_member_architecture_features_and_libraries() {
    let data = contents(1);
    let good = manifest(1, &data);
    for mutation in 0..12 {
        let mut m = good.clone();
        match mutation {
            0 => m.schema_version = 2,
            1 => m.product_version = "bad\n".into(),
            2 => {
                m.architecture = if m.architecture == Architecture::X86_64 {
                    Architecture::Aarch64
                } else {
                    Architecture::X86_64
                }
            }
            3 => m.source_revision = "bad".into(),
            4 => m.profile = "guess".into(),
            5 => m.libraries.clear(),
            6 => m.libraries[0].sha256 = "z".repeat(64),
            7 => m.members[0].features.clear(),
            8 => m.members[1].sha256 = "0".repeat(64),
            9 => m.members[1].size += 1,
            10 => m.members[1] = m.members[0].clone(),
            _ => m.members[0].features = vec!["video".into(), "video".into()],
        };
        assert!(read(&archive(&m, &data)).is_err(), "case {mutation}");
    }
    let mut bad = data;
    bad[2][18] ^= 1;
    let m = manifest(1, &bad);
    assert!(read(&archive(&m, &bad)).is_err());
    let mut bytes = archive(&good, &contents(1));
    bytes[512 + 5] ^= 1;
    assert!(read(&bytes).is_err());
}
#[test]
fn exact_target_map_modes_and_private_receipts() {
    let f = Fixture::new();
    let p = package(1);
    let paths = f.install.targets();
    let roots = f.io.target().paths();
    assert_eq!(
        &paths[..4],
        &FILES[..4]
            .iter()
            .map(|n| roots.prefix.join(n))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        paths[4],
        roots
            .config_home
            .join("systemd/user/crosspane-agent.service")
    );
    assert_eq!(
        paths[5],
        roots
            .data_home
            .join("applications/crosspane-settings.desktop")
    );
    assert_eq!(
        paths[6],
        roots
            .data_home
            .join("applications/crosspane-installer.desktop")
    );
    assert_eq!(
        paths[7],
        roots
            .data_home
            .join("icons/hicolor/scalable/apps/crosspane.svg")
    );
    assert_eq!(paths[8], roots.data_home.join("crosspane/LICENSE"));
    let expected = installed_contents(&f, 1);
    f.applied(&p, 1);
    for (i, path) in paths.iter().enumerate() {
        assert_eq!(fs::read(path).unwrap(), expected[i]);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            if i < 4 { 0o755 } else { 0o644 }
        );
    }
    assert_eq!(
        fs::metadata(f.record(false)).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(f.record(false).parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!f.record(true).exists());
    assert!(matches!(
        f.install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve),
        Err(PayloadError::Pending)
    ));
}
#[test]
fn scratch_proofs_target_validation_and_links_refuse_before_mutation() {
    let f = Fixture::new();
    let other = Fixture::new();
    let p = package(1);
    assert!(
        f.install
            .plan(&other.proof, &p, OperationId(1), MatchingFiles::Preserve)
            .is_err()
    );
    assert!(!f.record(true).exists());
    let mut bad = facts(&f.io);
    bad.active = false;
    assert!(f.proof.revalidate(&f.io, &bad).is_err());
    assert!(
        f.install
            .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
            .is_err()
    );
    assert!(!f.record(true).exists());
    for level in [
        ".local",
        ".local/bin",
        ".config",
        ".config/systemd",
        ".local/share",
        ".local/share/icons",
    ] {
        let f = Fixture::new();
        let path = f.root.join(level);
        let destination = f.root.join("escape");
        fs::create_dir(&destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(&destination, &path).unwrap();
        assert!(
            f.install
                .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
                .is_err()
        );
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
    }
    for mode in [0o666, 0o4755, 0o755] {
        let f = Fixture::new();
        put(&f.install.targets()[1], &contents(1)[1], mode);
        if mode == 0o755 {
            fs::hard_link(&f.install.targets()[1], f.root.join("alias")).unwrap();
        }
        assert!(
            f.install
                .plan(&f.proof, &p, OperationId(1), MatchingFiles::Adopt)
                .is_err()
        );
        assert!(!f.record(true).exists());
    }
}
#[test]
fn fresh_plans_never_mutate_and_unknown_runtime_leftovers_refuse() {
    // WP-4.32: an agent file with no record, or a running agent's bootstrap, is an install
    // to do (setup owns its install paths). Planning still writes nothing. A runtime that is
    // neither a running agent nor a dead one's exact leftovers still refuses.
    let p = package(1);
    for kind in 0..3 {
        let f = Fixture::new();
        match kind {
            0 => put(&f.install.targets()[0], &contents(1)[0], 0o755),
            1 => f.bootstrap(),
            _ => {
                f.io.create_private_dir(&f.proof, f.io.target().runtime_dir())
                    .unwrap();
                put(&f.io.target().socket_path(), b"unknown", 0o600);
            }
        }
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(1), MatchingFiles::Adopt);
        assert_eq!(plan.is_ok(), kind < 2, "{kind}");
        assert!(!f.record(true).exists());
        assert!(!f.record(false).exists());
    }
}
#[test]
fn matching_adoption_and_modified_files_are_preserved() {
    let f = Fixture::new();
    let p = package(1);
    put(&f.install.targets()[7], &contents(1)[7], 0o644);
    let before = fs::metadata(&f.install.targets()[7]).unwrap();
    use std::os::unix::fs::MetadataExt;
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(1), MatchingFiles::Adopt)
        .unwrap();
    assert_eq!(
        plan.receipt().resources[7].ownership,
        ResourceOwnership::Adopted
    );
    f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert_eq!(
        before.ino(),
        fs::metadata(&f.install.targets()[7]).unwrap().ino()
    );
    f.bootstrap();
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline())
        .unwrap();
    // WP-4.32: a newer package replaces the adopted file too; setup owns the install path.
    let newer = f
        .install
        .plan(
            &f.proof,
            &package(2),
            OperationId(2),
            MatchingFiles::Preserve,
        )
        .unwrap();
    assert_eq!(
        newer.receipt().resources[7].ownership,
        ResourceOwnership::Created
    );
    assert_eq!(fs::read(&f.install.targets()[7]).unwrap(), contents(1)[7]);
    // A modified file is saved into the backup folder before it is replaced.
    let g = Fixture::new();
    g.verified(&p, 1);
    put(&g.install.targets()[2], b"user edit", 0o755);
    assert_eq!(
        g.install.detect(&g.proof, &p).unwrap()[2].ownership,
        ResourceOwnership::Foreign
    );
    let plan = g
        .install
        .plan(&g.proof, &p, OperationId(2), MatchingFiles::Adopt)
        .unwrap();
    assert_eq!(fs::read(&g.install.targets()[2]).unwrap(), b"user edit");
    g.install.apply(&g.proof, &p, plan, &deadline()).unwrap();
    assert_eq!(fs::read(&g.install.targets()[2]).unwrap(), contents(1)[2]);
    let backup = g.install.backup_used().unwrap();
    assert!(backup.starts_with(g.io.target().paths().state_home.join("crosspane/backups")));
    assert_eq!(fs::read(backup.join("crosspane-ui")).unwrap(), b"user edit");
}
#[test]
fn repeat_install_preserves_existing_inodes_and_owned_receipts() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let p = package(1);
    f.verified(&p, 1);
    let inodes: Vec<_> = f
        .install
        .targets()
        .iter()
        .map(|p| fs::metadata(p).unwrap().ino())
        .collect();
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    assert!(
        plan.receipt()
            .resources
            .iter()
            .all(|r| r.ownership == ResourceOwnership::Created)
    );
    f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline())
        .unwrap();
    assert_eq!(
        inodes,
        f.install
            .targets()
            .iter()
            .map(|p| fs::metadata(p).unwrap().ino())
            .collect::<Vec<_>>()
    );
}
#[test]
fn crash_points_redetect_then_resume_and_keep_previous_payload() {
    for point in [
        Interruption::Intent,
        Interruption::Staged(0),
        Interruption::BackedUp(0),
        Interruption::Replaced(0),
        Interruption::Replaced(9),
        Interruption::Outcome,
    ] {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let before = installed_contents(&f, 1);
        let after = installed_contents(&f, 2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        f.install.scratch_interrupt(Some(point)).unwrap();
        assert_eq!(
            f.install.apply(&f.proof, &p, plan, &deadline()),
            Err(PayloadError::OutcomeUnknown)
        );
        assert!(f.record(true).exists());
        assert!(matches!(
            f.install
                .plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve),
            Err(PayloadError::Pending)
        ));
        for (i, path) in f.install.targets().iter().enumerate() {
            let actual = fs::read(path).unwrap();
            assert!(actual == before[i] || actual == after[i]);
            if actual == after[i] {
                assert_eq!(fs::read(f.sibling(2, i, true)).unwrap(), before[i]);
            }
        }
        f.install.scratch_interrupt(None).unwrap();
        let resume = f.install.resume_plan(&f.proof, &p).unwrap();
        f.install.apply(&f.proof, &p, resume, &deadline()).unwrap();
        assert!(!f.record(true).exists());
        for (i, bytes) in before.iter().enumerate() {
            assert_eq!(fs::read(f.sibling(2, i, true)).unwrap(), *bytes);
        }
        f.bootstrap_instance(10);
        f.install
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
            .unwrap();
        for i in 0..FILES.len() {
            assert!(!f.sibling(2, i, true).exists());
            assert!(!f.sibling(2, i, false).exists());
        }
    }
}
#[test]
fn modified_recovery_material_and_receipts_never_authorize_cleanup() {
    for kind in 0..4 {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        f.install
            .scratch_interrupt(Some(Interruption::Replaced(0)))
            .unwrap();
        f.install
            .apply(&f.proof, &p, plan, &deadline())
            .unwrap_err();
        let altered = match kind {
            0 => f.sibling(2, 0, true),
            1 => f.sibling(2, 0, false),
            2 => f.install.targets()[0].clone(),
            _ => f.record(true),
        };
        if kind < 3 {
            put(&altered, b"user modification", 0o755);
        } else {
            let mut j: Value = serde_json::from_slice(&fs::read(&altered).unwrap()).unwrap();
            j["receipt"]["resources"][0]["resolved_path"] = json!(f.root.join("unrelated"));
            put(&altered, &serde_json::to_vec(&j).unwrap(), 0o600);
        }
        f.install.scratch_interrupt(None).unwrap();
        assert!(f.install.resume_plan(&f.proof, &p).is_err());
        assert!(altered.exists());
        assert!(f.sibling(2, 0, true).exists());
    }
}
#[test]
fn wrong_stale_refused_or_foreign_health_cannot_retire_backups() {
    let f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    f.applied(&p, 2);
    f.bootstrap_instance(10);
    for case in 0..7 {
        let mut reply = f.reply("0.0.2");
        match case {
            0 => reply.id = 20,
            1 => reply.source = ObservationSource::Live,
            2 => reply.observed_at_ms = 101,
            3 => reply.result = Err(CallFailure::Refused(AgentRefusal::Other)),
            4 => reply = f.reply("0.0.1"),
            5 => reply.observed_at_ms = 0,
            _ => {
                let mut r: Value = serde_json::from_slice(
                    &fs::read(f.io.target().runtime_dir().join("bootstrap.json")).unwrap(),
                )
                .unwrap();
                r["instance_id"] = json!(11);
                f.io.atomic_write(
                    &f.proof,
                    &f.io.target().runtime_dir().join("bootstrap.json"),
                    &serde_json::to_vec(&r).unwrap(),
                )
                .unwrap();
            }
        }
        let now = if case == 5 { 6000 } else { 100 };
        assert!(
            f.install
                .verify(&f.proof, &p, 19, now, &reply, &deadline())
                .is_err()
        );
        assert!(f.sibling(2, 0, true).exists());
    }
    f.bootstrap_instance(10);
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
        .unwrap();
    assert!(!f.sibling(2, 0, true).exists());
}

#[test]
fn same_version_changed_bytes_require_a_new_admitted_instance() {
    let f = Fixture::new();
    f.verified(&package(1), 1);
    let data = contents(2);
    let mut m = manifest(2, &data);
    m.product_version = "0.0.1".into();
    let p = read(&archive(&m, &data)).unwrap();
    f.applied(&p, 2);
    let j: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert_eq!(j["previous_instance"], json!(9));
    assert_eq!(
        f.install
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline()),
        Err(PayloadError::Foreign)
    );
    assert_eq!(fs::read(f.sibling(2, 0, true)).unwrap(), contents(1)[0]);
    f.bootstrap_instance(10);
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline())
        .unwrap();
    assert!(!f.sibling(2, 0, true).exists());
}

#[test]
fn pre_exchange_instance_survives_executable_movement_and_resume() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    f.install
        .scratch_interrupt(Some(Interruption::Replaced(0)))
        .unwrap();
    f.install
        .apply(&f.proof, &p, plan, &deadline())
        .unwrap_err();
    assert_eq!(
        f.probe.snapshot(4242, &deadline()).unwrap().executable,
        f.sibling(2, 0, false)
    );
    let intent: Value = serde_json::from_slice(&fs::read(f.record(true)).unwrap()).unwrap();
    assert_eq!(intent["previous_instance"], json!(9));
    f.bootstrap_instance(10);
    f.install.scratch_interrupt(None).unwrap();
    let plan = f.install.resume_plan(&f.proof, &p).unwrap();
    f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    let outcome: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert_eq!(outcome["previous_instance"], json!(9));
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
        .unwrap();
}
#[test]
fn cancellation_before_mutation_and_pending_recovery_are_explicit() {
    let f = Fixture::new();
    let p = package(1);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
        .unwrap();
    let cancellation = Cancellation::default();
    let d = Deadline::new(1000, cancellation.clone()).unwrap();
    cancellation.cancel();
    assert_eq!(
        f.install.apply(&f.proof, &p, plan, &d),
        Err(PayloadError::Native(NativeError::Cancelled))
    );
    assert!(!f.record(true).exists());
    assert!(f.install.targets().iter().all(|p| !p.exists()));
}

#[test]
fn rendered_resources_are_target_bound_and_escape_unit_and_desktop_literals() {
    let suffix = "-space %$é";
    let f = Fixture::named(suffix);
    let g = Fixture::new();
    let p = package(1);
    let rendered = f.install.rendered_resources(&p).unwrap();
    assert_eq!(rendered, f.install.rendered_resources(&p).unwrap());
    let other = g.install.rendered_resources(&p).unwrap();
    for (index, (a, b)) in rendered.iter().zip(&other).enumerate() {
        assert_eq!(a.template_sha256, b.template_sha256);
        assert_ne!(a.rendered_sha256, b.rendered_sha256);
        assert_eq!(a.target, f.install.targets()[index + 4]);
        assert_eq!(a.source, ObservationSource::Demo);
        assert_eq!(a.rendered_sha256, sha256(&a.bytes));
        assert_eq!(a.template_sha256, sha256(&contents(1)[index + 4]));
    }
    let base = f.root.to_str().unwrap().strip_suffix(suffix).unwrap();
    let unit = String::from_utf8(rendered[0].bytes.clone()).unwrap();
    assert!(unit.contains(&format!(
        r#"ExecStart="{base}-space %%$é/.local/bin/crosspane-agent" run"#
    )));
    assert!(unit.contains(&format!(
        r#"Environment="XDG_CONFIG_HOME={base}-space %%$é/.config""#
    )));
    assert!(unit.contains(&format!(
        r#"Environment="CROSSPANE_RUNTIME_DIR={base}-space %%$é/run/crosspane""#
    )));
    let desktop = String::from_utf8(rendered[1].bytes.clone()).unwrap();
    assert!(desktop.contains(&format!(
        r#"Exec="{base}-space %%\\$é/.local/bin/crosspane-ui""#
    )));
    f.applied(&p, 1);
    for resource in rendered {
        assert_eq!(fs::read(resource.target).unwrap(), resource.bytes);
    }
}

#[test]
fn malformed_template_unknown_leftovers_and_newline_targets_fail_closed() {
    for case in 0..5 {
        let f = Fixture::new();
        let mut data = contents(1);
        let mut text = String::from_utf8(data[4].clone()).unwrap();
        match case {
            0 => text = text.replace("{{agent_executable}}", "{{unknown}}"),
            1 => text.push_str("{{unknown"),
            2 => text.push_str("{{installer_executable}}"),
            3 => text.push_str("{{agent_executable}}"),
            _ => text.push('\0'),
        }
        data[4] = text.into_bytes();
        let p = read(&archive(&manifest(1, &data), &data)).unwrap();
        assert_eq!(f.install.rendered_resources(&p), Err(PayloadError::Invalid));
        assert!(matches!(
            f.install
                .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve),
            Err(PayloadError::Invalid)
        ));
        assert!(!f.record(true).exists());
        assert!(f.install.targets().iter().all(|p| !p.exists()));
    }
    let path = PathBuf::from(format!(
        "/tmp/cp47b-newline-{}-{}\n",
        std::process::id(),
        ID.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(
        LinuxNativeIo::scratch(
            &path,
            Arc::new(Runner),
            Arc::new(Probe {
                root: path.clone(),
                inode: Mutex::new(None)
            })
        )
        .is_err()
    );
    assert!(!path.exists());
}

#[test]
fn rendered_hashes_keep_owned_repeat_files_and_preserve_hand_edits() {
    let f = Fixture::new();
    let p = package(1);
    f.verified(&p, 1);
    let records = f.install.rendered_resources(&p).unwrap();
    let journal: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    for (i, r) in records.iter().enumerate() {
        assert_eq!(
            journal["items"][i + 4]["template"],
            json!(r.template_sha256)
        );
        assert_eq!(journal["items"][i + 4]["new"], json!(r.rendered_sha256));
    }
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    assert!(
        plan.receipt().resources[4..7]
            .iter()
            .all(|r| r.ownership == ResourceOwnership::Created)
    );
    let path = &f.install.targets()[4];
    put(path, b"hand-edited unit\n", 0o644);
    let rows = f.install.detect(&f.proof, &p).unwrap();
    assert_eq!(rows[4].ownership, ResourceOwnership::Foreign);
    // WP-4.32: a fresh plan installs over the hand edit, saving it first.
    let fresh = f
        .install
        .plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve)
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"hand-edited unit\n");
    // A plan made before the edit still never writes over it.
    assert_eq!(
        f.install.apply(&f.proof, &p, plan, &deadline()),
        Err(PayloadError::Foreign)
    );
    assert_eq!(fs::read(path).unwrap(), b"hand-edited unit\n");
    f.install.apply(&f.proof, &p, fresh, &deadline()).unwrap();
    assert_eq!(fs::read(path).unwrap(), records[0].bytes);
    assert_eq!(
        fs::read(
            f.install
                .backup_used()
                .unwrap()
                .join("crosspane-agent.service")
        )
        .unwrap(),
        b"hand-edited unit\n"
    );
}

#[test]
fn whole_directives_single_exec_and_undeclared_codes_fail_closed() {
    let f = Fixture::new();
    for index in 4..7 {
        let field = TEMPLATE_FIELDS[if index == 4 { 0 } else { index }];
        for case in 0..9 {
            let mut data = contents(1);
            let text = String::from_utf8(data[index].clone()).unwrap();
            data[index] = match case {
                0 => text.replace(field, &format!("\"{field}\"")),
                1 => text.replace(field, &format!("{field}suffix")),
                2 => text.replace(field, &format!("prefix{field}")),
                3 => text.replace(field, "/bin/false") + &format!("\n#{field}\n"),
                4 => {
                    text + if index == 4 {
                        "\nExecStart=/bin/false\n"
                    } else {
                        "\nExec=/bin/false\n"
                    }
                }
                5 => text + "\nName=undeclared %f\n",
                6 => text + "\nName=undeclared %u\n",
                7 => text + "\nName=undeclared %h\n",
                _ => text.replace(
                    if index == 4 {
                        "[Service]"
                    } else {
                        "[Desktop Entry]"
                    },
                    "[Wrong]",
                ),
            }
            .into_bytes();
            let p = read(&archive(&manifest(1, &data), &data)).unwrap();
            assert_eq!(f.install.rendered_resources(&p), Err(PayloadError::Invalid));
            assert!(matches!(
                f.install
                    .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve),
                Err(PayloadError::Invalid)
            ));
        }
    }
    assert!(f.install.targets().iter().all(|p| !p.exists()));
}

#[test]
fn native_parser_continuations_bom_non_ascii_and_cr_fail_closed() {
    let f = Fixture::new();
    for index in 4..7 {
        for case in 0..4 {
            let mut data = contents(1);
            let text = String::from_utf8(data[index].clone()).unwrap();
            // Native parsers normalize these physical lines; our templates forbid that ambiguity.
            let command = if index == 4 { "ExecStart=" } else { "Exec=" };
            data[index] = match case {
                0 => text.replace(command, &format!("Type=exec\\\n{command}")),
                1 => text + &format!("\u{feff}{command}/bin/false\n"),
                2 => text + "# non-ASCII \u{e9}\n",
                _ => text.replace('\n', "\r\n"),
            }
            .into_bytes();
            // Manifest hashes/sizes and ustar framing remain otherwise valid.
            let p = read(&archive(&manifest(1, &data), &data)).unwrap();
            assert_eq!(f.install.rendered_resources(&p), Err(PayloadError::Invalid));
            assert!(matches!(
                f.install
                    .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve),
                Err(PayloadError::Invalid)
            ));
        }
    }
    assert!(!f.record(true).exists());
    assert!(f.install.targets().iter().all(|p| !p.exists()));
}

#[test]
fn unsupported_decoded_executable_characters_refuse_before_mutation() {
    for suffix in [
        "-quote\"",
        "-backslash\\",
        "-glob*",
        "-query?",
        "-class[",
        "-tick`",
        "-colon:",
    ] {
        let f = Fixture::named(suffix);
        let p = package(1);
        assert_eq!(f.install.rendered_resources(&p), Err(PayloadError::Invalid));
        assert!(matches!(
            f.install
                .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve),
            Err(PayloadError::Invalid)
        ));
        assert!(f.install.targets().iter().all(|p| !p.exists()));
    }
}

#[test]
fn every_operational_health_clause_retains_every_backup_until_satisfied() {
    let f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    f.applied(&p, 2);
    f.bootstrap_instance(10);
    let before: Vec<_> = (0..FILES.len())
        .map(|i| fs::read(f.sibling(2, i, true)).unwrap())
        .collect();
    let mandatory = [
        "keystore", "links", "parking", "windows", "frames", "capture", "keys", "pointer",
    ];
    let optional = [
        "overlay",
        "hotkeys",
        "tray",
        "gpu",
        "audio",
        "home",
        "discovery",
    ];
    for recovery in ["failed", "none"] {
        let reply = f.reply_with("0.0.2", |h| h["startup_recovery"] = json!(recovery));
        assert!(matches!(
            reply.result,
            Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
        ));
        assert_eq!(
            f.install.verify(&f.proof, &p, 19, 100, &reply, &deadline()),
            Err(PayloadError::Pending)
        );
    }
    let reply = f.reply_with("0.0.2", |h| h["recovery_pending"] = json!(1));
    assert_eq!(
        f.install.verify(&f.proof, &p, 19, 100, &reply, &deadline()),
        Err(PayloadError::Pending)
    );
    for name in mandatory.iter().chain(&optional) {
        for state in ["missing", "blocked", "failed"] {
            if optional.contains(name) && state != "failed" {
                continue;
            }
            let reply = f.reply_with("0.0.2", |h| {
                let b = h["backends"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|b| b["name"] == *name)
                    .unwrap();
                b["state"] = json!(state);
                b["reason"] = json!("unknown");
            });
            assert!(matches!(
                reply.result,
                Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
            ));
            assert_eq!(
                f.install.verify(&f.proof, &p, 19, 100, &reply, &deadline()),
                Err(PayloadError::Pending),
                "{name}/{state}"
            );
            for (i, bytes) in before.iter().enumerate() {
                assert_eq!(fs::read(f.sibling(2, i, true)).unwrap(), *bytes);
            }
            let journal: Value =
                serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
            assert_eq!(journal["phase"], json!("Applied"));
            assert!(
                journal["receipt"]["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["outcome"] != "Verified")
            );
        }
    }
    let reply = f.reply_with("0.0.2", |h| {
        h["startup_recovery"] = json!("restored");
        for (i, b) in h["backends"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|b| optional.contains(&b["name"].as_str().unwrap()))
            .enumerate()
        {
            b["state"] = json!(if i % 2 == 0 { "missing" } else { "blocked" });
            b["reason"] = json!("unknown");
        }
    });
    f.install
        .verify(&f.proof, &p, 19, 100, &reply, &deadline())
        .unwrap();
    assert!((0..FILES.len()).all(|i| !f.sibling(2, i, true).exists()));
}

#[test]
fn fresh_observations_cannot_claim_matching_files_created_after_plan() {
    let f = Fixture::new();
    let p = package(1);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
        .unwrap();
    let bytes = installed_contents(&f, 1)[7].clone();
    put(&f.install.targets()[7], &bytes, 0o644);
    assert_eq!(
        f.install.apply(&f.proof, &p, plan, &deadline()),
        Err(PayloadError::Foreign)
    );
    assert!(!f.record(true).exists());
    assert_eq!(fs::read(&f.install.targets()[7]).unwrap(), bytes);
    f.applied(&p, 2);
    f.bootstrap();
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline())
        .unwrap();
    // WP-4.32: the matching file was there when op 2 planned; setup owns the install path, so
    // the install it recorded covers it.
    assert_eq!(
        f.install.detect(&f.proof, &package(2)).unwrap()[7].ownership,
        ResourceOwnership::Created
    );
    assert!(
        f.install
            .plan(
                &f.proof,
                &package(2),
                OperationId(3),
                MatchingFiles::Preserve
            )
            .is_ok()
    );
}

#[test]
fn matching_files_created_after_locked_recheck_are_never_owned_on_retry() {
    for point in [Interruption::Intent, Interruption::Replaced(0)] {
        let mut f = Fixture::new();
        let p = package(1);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
            .unwrap();
        let selected = f.install.targets()[7].clone();
        let bytes = installed_contents(&f, 1)[7].clone();
        let owner_path = selected.clone();
        let owner_bytes = bytes.clone();
        f.install
            .scratch_hook(Some(Arc::new(move |observed| {
                if observed == point {
                    put(&owner_path, &owner_bytes, 0o644);
                }
                Ok(())
            })))
            .unwrap();
        assert_eq!(
            f.install.apply(&f.proof, &p, plan, &deadline()),
            Err(PayloadError::OutcomeUnknown)
        );
        let identity = fs::metadata(&selected).unwrap().ino();
        let intent: Value = serde_json::from_slice(&fs::read(f.record(true)).unwrap()).unwrap();
        assert_eq!(intent["items"][7]["replacement"], Value::Null);
        let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
        assert!(matches!(
            reconstructed.resume_plan(&f.proof, &p),
            Err(PayloadError::Foreign)
        ));
        assert_eq!(fs::read(&selected).unwrap(), bytes);
        assert_eq!(fs::metadata(&selected).unwrap().ino(), identity);
        assert!(!f.record(false).exists());
        assert!(matches!(
            reconstructed.plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve),
            Err(PayloadError::Pending)
        ));
    }
}

#[test]
fn exchange_matching_owner_file_is_not_the_admitted_staged_inode_on_resume() {
    let mut f = Fixture::new();
    let p = package(1);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
        .unwrap();
    let owner_path = f.install.targets()[7].clone();
    let bytes = installed_contents(&f, 1)[7].clone();
    let stage = f.sibling(1, 8, false);
    let selected = owner_path.clone();
    let matching = bytes.clone();
    let staged = stage.clone();
    f.install
        .scratch_hook(Some(Arc::new(move |point| {
            if point == Interruption::Exchange(8) {
                assert!(staged.exists());
                put(&selected, &matching, 0o644);
            }
            Ok(())
        })))
        .unwrap();
    assert_eq!(
        f.install.apply(&f.proof, &p, plan, &deadline()),
        Err(PayloadError::OutcomeUnknown)
    );
    let owner = fs::metadata(&owner_path).unwrap();
    let recovery = fs::metadata(&stage).unwrap();
    assert_ne!(owner.ino(), recovery.ino());
    let record = fs::read(f.record(true)).unwrap();
    let journal: Value = serde_json::from_slice(&record).unwrap();
    let admission = &journal["items"][7]["replacement"];
    let parent = fs::metadata(stage.parent().unwrap()).unwrap();
    assert_eq!(admission["file"], json!([recovery.dev(), recovery.ino()]));
    assert_eq!(admission["parent"], json!([parent.dev(), parent.ino()]));
    assert_eq!(admission["hash"], json!(sha256(&bytes)));
    assert_eq!(admission["mode"], json!(0o644));
    let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(matches!(
        reconstructed.resume_plan(&f.proof, &p),
        Err(PayloadError::Foreign)
    ));
    f.bootstrap();
    assert_eq!(
        reconstructed.verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline()),
        Err(PayloadError::Pending)
    );
    assert!(matches!(
        reconstructed.plan(
            &f.proof,
            &package(2),
            OperationId(2),
            MatchingFiles::Preserve
        ),
        Err(PayloadError::Pending)
    ));
    assert!(!f.record(false).exists());
    assert_eq!(fs::read(f.record(true)).unwrap(), record);
    assert_eq!(fs::read(&owner_path).unwrap(), bytes);
    assert_eq!(fs::metadata(&owner_path).unwrap().ino(), owner.ino());
    assert_eq!(fs::read(&stage).unwrap(), bytes);
    assert_eq!(fs::metadata(&stage).unwrap().ino(), recovery.ino());
}

#[test]
fn pending_staged_identity_survives_reconstruction_and_retry_interleavings() {
    for during_retry in [false, true] {
        let mut f = Fixture::new();
        let p = package(1);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(1), MatchingFiles::Preserve)
            .unwrap();
        f.install
            .scratch_interrupt(Some(Interruption::Exchange(8)))
            .unwrap();
        assert_eq!(
            f.install.apply(&f.proof, &p, plan, &deadline()),
            Err(PayloadError::OutcomeUnknown)
        );
        let stage = f.sibling(1, 8, false);
        let target = f.install.targets()[7].clone();
        let preserved = f.root.join("preserved-stage");
        let original = fs::metadata(&stage).unwrap();
        let bytes = fs::read(&stage).unwrap();
        let record = fs::read(f.record(true)).unwrap();
        assert!(!target.exists());
        let mut reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
        let resume = reconstructed.resume_plan(&f.proof, &p).unwrap();
        if during_retry {
            let selected = stage.clone();
            let keep = preserved.clone();
            let identical = bytes.clone();
            reconstructed
                .scratch_hook(Some(Arc::new(move |point| {
                    if point == Interruption::BackedUp(8) {
                        fs::rename(&selected, &keep).unwrap();
                        put(&selected, &identical, 0o644);
                    }
                    Ok(())
                })))
                .unwrap();
            assert_eq!(
                reconstructed.apply(&f.proof, &p, resume, &deadline()),
                Err(PayloadError::OutcomeUnknown)
            );
        } else {
            fs::rename(&stage, &preserved).unwrap();
            put(&stage, &bytes, 0o644);
            assert_eq!(
                reconstructed.apply(&f.proof, &p, resume, &deadline()),
                Err(PayloadError::Foreign)
            );
        }
        let substituted = fs::metadata(&stage).unwrap();
        assert_ne!(substituted.ino(), original.ino());
        assert_eq!(fs::read(f.record(true)).unwrap(), record);
        assert_eq!(fs::read(&preserved).unwrap(), bytes);
        assert_eq!(fs::metadata(&preserved).unwrap().ino(), original.ino());
        assert!(!target.exists());
        let rebuilt_again = PayloadInstaller::new(f.io.clone()).unwrap();
        assert!(matches!(
            rebuilt_again.resume_plan(&f.proof, &p),
            Err(PayloadError::Foreign)
        ));
        f.bootstrap();
        assert_eq!(
            rebuilt_again.verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline()),
            Err(PayloadError::Pending)
        );
        assert!(matches!(
            rebuilt_again.plan(
                &f.proof,
                &package(2),
                OperationId(2),
                MatchingFiles::Preserve
            ),
            Err(PayloadError::Pending)
        ));
        assert_eq!(fs::read(&stage).unwrap(), bytes);
        assert_eq!(fs::metadata(&stage).unwrap().ino(), substituted.ino());
        assert_eq!(fs::read(f.record(true)).unwrap(), record);
        assert!(!target.exists());
        assert!(!f.record(false).exists());
    }
}

#[test]
fn exchange_retry_final_check_preserves_late_stage_substitution_and_every_backup() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let old = installed_contents(&f, 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    f.install
        .scratch_interrupt(Some(Interruption::Exchange(8)))
        .unwrap();
    assert_eq!(
        f.install.apply(&f.proof, &p, plan, &deadline()),
        Err(PayloadError::OutcomeUnknown)
    );
    let stage = f.sibling(2, 8, false);
    let target = f.install.targets()[7].clone();
    let target_inode = fs::metadata(&target).unwrap().ino();
    let staged_inode = fs::metadata(&stage).unwrap().ino();
    let bytes = fs::read(&stage).unwrap();
    let record = fs::read(f.record(true)).unwrap();
    let outcome = fs::read(f.record(false)).unwrap();
    let preserved = f.root.join("preserved-late-stage");
    let mut reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    let resume = reconstructed.resume_plan(&f.proof, &p).unwrap();
    let selected = stage.clone();
    let keep = preserved.clone();
    let identical = bytes.clone();
    reconstructed
        .scratch_hook(Some(Arc::new(move |point| {
            if point == Interruption::Exchange(8) {
                fs::rename(&selected, &keep).unwrap();
                put(&selected, &identical, 0o644);
            }
            Ok(())
        })))
        .unwrap();
    assert_eq!(
        reconstructed.apply(&f.proof, &p, resume, &deadline()),
        Err(PayloadError::OutcomeUnknown)
    );
    let substitute_inode = fs::metadata(&stage).unwrap().ino();
    assert_ne!(substitute_inode, staged_inode);
    assert_eq!(fs::read(&target).unwrap(), old[7]);
    assert_eq!(fs::metadata(&target).unwrap().ino(), target_inode);
    assert_eq!(fs::read(&preserved).unwrap(), bytes);
    assert_eq!(fs::metadata(&preserved).unwrap().ino(), staged_inode);
    assert_eq!(fs::read(&stage).unwrap(), bytes);
    assert_eq!(fs::read(f.record(true)).unwrap(), record);
    assert_eq!(fs::read(f.record(false)).unwrap(), outcome);
    let previous: Value = serde_json::from_slice(&outcome).unwrap();
    assert_eq!(previous["receipt"]["operation_id"], json!(1));
    assert!(!f.sibling(2, 9, true).exists());
    for (i, expected) in old.iter().enumerate().take(9) {
        assert_eq!(&fs::read(f.sibling(2, i, true)).unwrap(), expected);
    }
    let rebuilt_again = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(matches!(
        rebuilt_again.resume_plan(&f.proof, &p),
        Err(PayloadError::Foreign)
    ));
    assert_eq!(
        rebuilt_again.verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline()),
        Err(PayloadError::Pending)
    );
    assert!(matches!(
        rebuilt_again.plan(
            &f.proof,
            &package(3),
            OperationId(3),
            MatchingFiles::Preserve
        ),
        Err(PayloadError::Pending)
    ));
    assert_eq!(fs::read(f.record(false)).unwrap(), outcome);
    assert_eq!(fs::read(&target).unwrap(), old[7]);
    assert_eq!(fs::metadata(&target).unwrap().ino(), target_inode);
    assert_eq!(fs::metadata(&stage).unwrap().ino(), substitute_inode);
    for (i, expected) in old.iter().enumerate().take(9) {
        assert_eq!(&fs::read(f.sibling(2, i, true)).unwrap(), expected);
    }
}

#[test]
fn every_persisted_replacement_hash_and_mode_is_validated_even_when_originally_matching() {
    let f = Fixture::new();
    let p = package(1);
    f.verified(&p, 1);
    f.applied(&p, 2);
    // This repeat operation originally observed every matching file and performed no rename.
    let base: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert!(
        base["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["replacement"].is_null())
    );
    let installed: Vec<_> = f
        .install
        .targets()
        .iter()
        .map(|path| (fs::read(path).unwrap(), fs::metadata(path).unwrap().ino()))
        .collect();
    for index in 0..FILES.len() {
        for wrong_mode in [false, true] {
            let mut journal = base.clone();
            let path = &f.install.targets()[index];
            let file = fs::metadata(path).unwrap();
            let parent = fs::metadata(path.parent().unwrap()).unwrap();
            let mut hash = journal["items"][index]["new"].clone();
            let mode = if index < 4 { 0o755 } else { 0o644 };
            if !wrong_mode {
                hash[0] = json!(hash[0].as_u64().unwrap() ^ 1);
            }
            journal["items"][index]["replacement"] = json!({
                "file":[file.dev(),file.ino()], "parent":[parent.dev(),parent.ino()],
                "hash":hash, "mode":if wrong_mode { mode ^ 1 } else { mode }
            });
            let record = serde_json::to_vec(&journal).unwrap();
            f.io.atomic_write(&f.proof, &f.record(false), &record)
                .unwrap();
            let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
            assert!(matches!(
                reconstructed.resume_plan(&f.proof, &p),
                Err(PayloadError::Foreign)
            ));
            assert!(matches!(
                reconstructed.plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve),
                Err(PayloadError::Foreign)
            ));
            assert_eq!(
                reconstructed.verify(&f.proof, &p, 19, 100, &f.reply("0.0.1"), &deadline()),
                Err(PayloadError::Foreign)
            );
            assert_eq!(fs::read(f.record(false)).unwrap(), record);
            assert!(!f.record(true).exists());
            for (i, (bytes, inode)) in installed.iter().enumerate() {
                assert_eq!(&fs::read(&f.install.targets()[i]).unwrap(), bytes);
                assert_eq!(fs::metadata(&f.install.targets()[i]).unwrap().ino(), *inode);
            }
        }
    }
}

#[test]
fn planning_observations_cannot_cross_an_unfinished_ledger_generation() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = Arc::new(package(2));
    let competitor = Arc::new(PayloadInstaller::new(f.io.clone()).unwrap());
    let plan = competitor
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    let pending = Arc::new(Mutex::new(Some(plan)));
    let proof = f.proof.clone();
    let competing_package = p.clone();
    f.install
        .scratch_hook(Some(Arc::new(move |point| {
            if point == Interruption::Planning {
                let plan = pending.lock().unwrap().take().unwrap();
                competitor
                    .apply(&proof, &competing_package, plan, &deadline())
                    .unwrap();
            }
            Ok(())
        })))
        .unwrap();
    assert!(matches!(
        f.install
            .plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve),
        Err(PayloadError::Pending)
    ));
    let outcome: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert_eq!(outcome["phase"], json!("Applied"));
    assert_eq!(outcome["receipt"]["operation_id"], json!(2));
    for (i, bytes) in installed_contents(&f, 1).iter().enumerate() {
        assert_eq!(fs::read(f.sibling(2, i, true)).unwrap(), *bytes);
    }
    let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(matches!(
        reconstructed.plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve),
        Err(PayloadError::Pending)
    ));
}

#[test]
fn locked_phase_and_operation_revalidation_precedes_intent_publication() {
    for collision in [false, true] {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let path = f.record(false);
        let mut outcome: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if collision {
            outcome["receipt"]["operation_id"] = json!(2);
        } else {
            outcome["phase"] = json!("Applied");
        }
        let changed = serde_json::to_vec(&outcome).unwrap();
        let expected = changed.clone();
        let io = f.io.clone();
        let proof = f.proof.clone();
        f.install
            .scratch_hook(Some(Arc::new(move |point| {
                if point == Interruption::Locked {
                    io.atomic_write(&proof, &path, &changed).unwrap();
                }
                Ok(())
            })))
            .unwrap();
        assert_eq!(
            f.install.apply(&f.proof, &p, plan, &deadline()),
            Err(PayloadError::Pending)
        );
        assert_eq!(fs::read(f.record(false)).unwrap(), expected);
        assert!(!f.record(true).exists());
        assert!(!f.sibling(2, 0, true).exists());
        assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
    }
}

#[test]
fn competing_plan_generations_and_lock_contention_do_not_lose_recovery() {
    for changed in [false, true] {
        let f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(if changed { 2 } else { 1 });
        let a = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let b = f
            .install
            .plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve)
            .unwrap();
        f.install.apply(&f.proof, &p, a, &deadline()).unwrap();
        let outcome = fs::read(f.record(false)).unwrap();
        assert!(f.install.apply(&f.proof, &p, b, &deadline()).is_err());
        assert_eq!(fs::read(f.record(false)).unwrap(), outcome);
        if changed {
            assert_eq!(fs::read(f.sibling(2, 0, true)).unwrap(), contents(1)[0]);
        }
        let lock =
            f.io.lock(&f.proof, &f.record(false).with_file_name("install.lock"))
                .unwrap();
        let plan = f.install.resume_plan(&f.proof, &p).unwrap();
        assert!(f.install.apply(&f.proof, &p, plan, &deadline()).is_err());
        drop(lock);
        let plan = f.install.resume_plan(&f.proof, &p).unwrap();
        f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
        f.bootstrap_instance(10);
        f.install
            .verify(
                &f.proof,
                &p,
                19,
                100,
                &f.reply(&p.manifest().product_version),
                &deadline(),
            )
            .unwrap();
    }
}

#[test]
fn interrupted_io_publishes_only_verified_durable_recovery_files() {
    for point in [
        Interruption::Write,
        Interruption::Chmod,
        Interruption::FileSync,
        Interruption::Publish,
        Interruption::DirectorySync,
    ] {
        for backup in [false, true] {
            let mut f = Fixture::new();
            f.verified(&package(1), 1);
            let p = package(2);
            let plan = f
                .install
                .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
                .unwrap();
            // Stage and backup each use the same actual I/O executor; fail the selected one.
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let count = Arc::new(AtomicU64::new(0));
            f.install
                .scratch_hook(Some(Arc::new(move |event| {
                    seen.lock().unwrap().push(event);
                    if event == point && count.fetch_add(1, Ordering::Relaxed) == u64::from(backup)
                    {
                        return Err(PayloadError::OutcomeUnknown);
                    }
                    Ok(())
                })))
                .unwrap();
            assert_eq!(
                f.install.apply(&f.proof, &p, plan, &deadline()),
                Err(PayloadError::OutcomeUnknown)
            );
            assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
            let selected = f.sibling(2, 0, backup);
            if point == Interruption::DirectorySync {
                assert_eq!(
                    fs::read(&selected).unwrap(),
                    if backup {
                        contents(1)[0].clone()
                    } else {
                        contents(2)[0].clone()
                    }
                );
            } else {
                assert!(!selected.exists());
            }
            let events = calls.lock().unwrap();
            assert!(events.contains(&point));
            if backup {
                let expected = [
                    Interruption::Write,
                    Interruption::Chmod,
                    Interruption::FileSync,
                    Interruption::Publish,
                    Interruption::DirectorySync,
                ];
                let observed: Vec<_> = events
                    .iter()
                    .copied()
                    .filter(|e| expected.contains(e))
                    .take(5)
                    .collect();
                assert_eq!(observed, expected);
            }
            drop(events);
            // Reconstruct from disk rather than using the failed in-memory installer.
            let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
            let plan = reconstructed.resume_plan(&f.proof, &p).unwrap();
            reconstructed
                .apply(&f.proof, &p, plan, &deadline())
                .unwrap();
            f.bootstrap_instance(10);
            reconstructed
                .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
                .unwrap();
        }
    }
}

#[test]
fn quarantine_checks_candidate_inode_and_ancestry_before_deletion() {
    for case in 0..3 {
        let parent_race = case == 2;
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        f.applied(&p, 2);
        f.bootstrap_instance(10);
        let backup = f.sibling(2, 0, true);
        let original = fs::read(&backup).unwrap();
        let replacement = if case == 1 {
            original.clone()
        } else {
            b"foreign replacement".to_vec()
        };
        let preserved = f.root.join("preserved");
        let foreign = f.root.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
        let first = Arc::new(AtomicU64::new(0));
        let guard = first.clone();
        let selected = backup.clone();
        let kept = preserved.clone();
        let unrelated = foreign.clone();
        let foreign_bytes = replacement.clone();
        f.install
            .scratch_hook(Some(Arc::new(move |point| {
                if point == Interruption::Quarantine && guard.fetch_add(1, Ordering::Relaxed) == 0 {
                    if parent_race {
                        let parent = selected.parent().unwrap();
                        fs::rename(parent, &kept).unwrap();
                        symlink(&unrelated, parent).unwrap();
                    } else {
                        fs::rename(&selected, &kept).unwrap();
                        put(&selected, &foreign_bytes, 0o755);
                    }
                }
                Ok(())
            })))
            .unwrap();
        assert!(
            f.install
                .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
                .is_err()
        );
        if parent_race {
            assert_eq!(
                fs::read(
                    preserved
                        .join(backup.file_name().unwrap())
                        .with_extension("retired")
                )
                .unwrap(),
                original
            );
            assert_eq!(fs::read_dir(foreign).unwrap().count(), 0);
        } else {
            assert_eq!(fs::read(&preserved).unwrap(), original);
            assert_eq!(
                fs::read(backup.with_file_name(format!(
                    "{}.retired",
                    backup.file_name().unwrap().to_str().unwrap()
                )))
                .unwrap(),
                replacement
            );
        }
        assert!(f.sibling(2, 1, true).exists() || parent_race);
        // The durable admission must survive reconstruction, including identical foreign bytes.
        let reconstructed = PayloadInstaller::new(f.io.clone());
        if let Ok(reconstructed) = reconstructed {
            assert!(
                reconstructed
                    .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
                    .is_err()
            );
        } else {
            assert!(parent_race);
        }
        if !parent_race {
            let retired = backup.with_file_name(format!(
                "{}.retired",
                backup.file_name().unwrap().to_str().unwrap()
            ));
            assert_eq!(fs::read(retired).unwrap(), replacement);
            assert_eq!(fs::read(&preserved).unwrap(), original);
            assert!(f.sibling(2, 1, true).exists());
        }
    }
}

#[test]
fn changed_quarantine_contents_survive_unlink_and_reconstructed_retries() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    f.applied(&p, 2);
    f.bootstrap_instance(10);
    let backup = f.sibling(2, 0, true);
    let retired = backup.with_file_name(format!(
        "{}.retired",
        backup.file_name().unwrap().to_str().unwrap()
    ));
    let admitted_inode = fs::metadata(&backup).unwrap().ino();
    let selected = retired.clone();
    f.install
        .scratch_hook(Some(Arc::new(move |point| {
            if point == Interruption::Unlink {
                assert_eq!(fs::metadata(&selected).unwrap().ino(), admitted_inode);
                fs::write(&selected, b"changed in place before unlink").unwrap();
            }
            Ok(())
        })))
        .unwrap();
    assert_eq!(
        f.install
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline()),
        Err(PayloadError::Foreign)
    );
    assert_eq!(fs::metadata(&retired).unwrap().ino(), admitted_inode);
    assert_eq!(
        fs::read(&retired).unwrap(),
        b"changed in place before unlink"
    );
    let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    assert_eq!(
        reconstructed.verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline()),
        Err(PayloadError::Foreign)
    );
    assert_eq!(
        fs::read(&retired).unwrap(),
        b"changed in place before unlink"
    );
    assert!((1..10).all(|i| f.sibling(2, i, true).exists()));
}

/// Actual journal write/file-fsync/rename/parent-fsync failures delegate to WP-4.7a's
/// frozen atomic_write contract. Here every publish is interrupted immediately before
/// or after that call, and a new installer reconstructs only from durable scratch records.
#[test]
fn journal_publish_checkpoints_reconstruct_every_transition_and_retained_bytes() {
    let mut cases = Vec::new();
    for after in [false, true] {
        for ordinal in 0..=10 {
            cases.push((true, 0, after, ordinal));
        }
        cases.push((false, 1, after, 0));
        cases.push((false, 2, after, 0));
    }
    for (intent, phase, after, ordinal) in cases {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let old = installed_contents(&f, 1);
        let new = installed_contents(&f, 2);
        let p = package(2);
        if phase == 2 {
            let plan = f
                .install
                .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
                .unwrap();
            f.install.apply(&f.proof, &p, plan, &deadline()).unwrap();
            f.bootstrap_instance(10);
        }
        let point = if after {
            Interruption::AfterJournal(intent, phase)
        } else {
            Interruption::BeforeJournal(intent, phase)
        };
        let calls = Arc::new(AtomicU64::new(0));
        let count = calls.clone();
        f.install
            .scratch_hook(Some(Arc::new(move |observed| {
                if observed == point && count.fetch_add(1, Ordering::Relaxed) == ordinal {
                    return Err(PayloadError::OutcomeUnknown);
                }
                Ok(())
            })))
            .unwrap();
        if phase == 2 {
            assert_eq!(
                f.install
                    .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline()),
                Err(PayloadError::OutcomeUnknown)
            );
        } else {
            let plan = f
                .install
                .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
                .unwrap();
            assert_eq!(
                f.install.apply(&f.proof, &p, plan, &deadline()),
                Err(PayloadError::OutcomeUnknown)
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), ordinal + 1);
        let outcome: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
        let outcome_op = if phase == 0 || (phase == 1 && !after) {
            1
        } else {
            2
        };
        assert_eq!(outcome["receipt"]["operation_id"], json!(outcome_op));
        assert_eq!(
            outcome["phase"],
            json!(if outcome_op == 1 || (phase == 2 && after) {
                "Verified"
            } else {
                "Applied"
            })
        );
        let completed = if phase == 0 {
            ordinal.saturating_sub(1) as usize
        } else {
            10
        };
        for i in 0..FILES.len() {
            assert_eq!(
                &fs::read(&f.install.targets()[i]).unwrap(),
                if i < completed { &new[i] } else { &old[i] }
            );
            if phase != 0 || (ordinal > 0 && i <= completed) {
                assert_eq!(fs::read(f.sibling(2, i, true)).unwrap(), old[i]);
                let stage = fs::read(f.sibling(2, i, false)).unwrap();
                assert_eq!(&stage, if i < completed { &old[i] } else { &new[i] });
            } else {
                assert!(!f.sibling(2, i, true).exists());
                assert!(!f.sibling(2, i, false).exists());
            }
        }
        if intent && (ordinal > 0 || after) {
            let journal: Value =
                serde_json::from_slice(&fs::read(f.record(true)).unwrap()).unwrap();
            assert_eq!(journal["receipt"]["operation_id"], json!(2));
            if ordinal > 0 {
                let admission = &journal["items"][ordinal as usize - 1]["replacement"];
                if after {
                    let stage = f.sibling(2, ordinal as usize - 1, false);
                    let file = fs::metadata(&stage).unwrap();
                    let parent = fs::metadata(stage.parent().unwrap()).unwrap();
                    assert_eq!(admission["file"], json!([file.dev(), file.ino()]));
                    assert_eq!(admission["parent"], json!([parent.dev(), parent.ino()]));
                } else {
                    assert_eq!(*admission, Value::Null);
                }
                assert_eq!(
                    journal["previous_instance"],
                    if ordinal > 1 || after {
                        json!(9)
                    } else {
                        Value::Null
                    }
                );
            }
        }
        let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
        if phase != 2 {
            let plan = if intent && ordinal == 0 && !after {
                reconstructed
                    .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
                    .unwrap()
            } else {
                reconstructed.resume_plan(&f.proof, &p).unwrap()
            };
            reconstructed
                .apply(&f.proof, &p, plan, &deadline())
                .unwrap();
            assert!((0..FILES.len()).all(|i| fs::read(f.sibling(2, i, true)).unwrap() == old[i]));
            f.bootstrap_instance(10);
        }
        reconstructed
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
            .unwrap();
        assert!(
            (0..FILES.len())
                .all(|i| !f.sibling(2, i, true).exists() && !f.sibling(2, i, false).exists())
        );
    }
}

#[test]
fn quarantine_identity_publish_checkpoints_retain_original_and_resume_cleanup() {
    for point in [
        Interruption::BeforeQuarantineRecord,
        Interruption::AfterQuarantineRecord,
    ] {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        f.applied(&p, 2);
        f.bootstrap_instance(10);
        f.install.scratch_interrupt(Some(point)).unwrap();
        assert_eq!(
            f.install
                .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline()),
            Err(PayloadError::OutcomeUnknown)
        );
        for i in 0..FILES.len() {
            assert_eq!(
                fs::read(f.sibling(2, i, true)).unwrap(),
                installed_contents(&f, 1)[i]
            );
        }
        let outcome: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
        assert_eq!(outcome["phase"], json!("Verified"));
        let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
        reconstructed
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
            .unwrap();
        assert!((0..FILES.len()).all(|i| !f.sibling(2, i, true).exists()));
    }
}

#[test]
fn retirement_interruption_reconstructs_and_never_requires_old_executable_path() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    f.applied(&p, 2);
    f.bootstrap_instance(10);
    f.install
        .scratch_interrupt(Some(Interruption::Retired(0)))
        .unwrap();
    assert!(
        f.install
            .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
            .is_err()
    );
    assert!(!f.sibling(2, 0, true).exists());
    assert!(f.sibling(2, 1, true).exists());
    let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    reconstructed
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
        .unwrap();
    assert!((0..FILES.len()).all(|i| !f.sibling(2, i, true).exists()));
}

#[test]
fn quarantined_intent_and_backup_interruptions_resume_from_durable_records() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    f.install
        .scratch_interrupt(Some(Interruption::Unlink))
        .unwrap();
    f.install
        .apply(&f.proof, &p, plan, &deadline())
        .unwrap_err();
    assert!(!f.record(true).exists());
    assert!(
        f.record(true)
            .with_file_name("payload-intent.json.retired")
            .exists()
    );
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = rebuilt.resume_plan(&f.proof, &p).unwrap();
    rebuilt.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert!(
        !f.record(true)
            .with_file_name("payload-intent.json.retired")
            .exists()
    );
    f.bootstrap_instance(10);
    f.install = rebuilt;
    f.install
        .scratch_interrupt(Some(Interruption::DirectorySync))
        .unwrap();
    f.install
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
        .unwrap_err();
    let backup = f.sibling(2, 0, true);
    let retired = backup.with_file_name(format!(
        "{}.retired",
        backup.file_name().unwrap().to_str().unwrap()
    ));
    assert!(!backup.exists());
    assert_eq!(fs::read(&retired).unwrap(), contents(1)[0]);
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(matches!(
        rebuilt.plan(&f.proof, &p, OperationId(3), MatchingFiles::Preserve),
        Err(PayloadError::Pending)
    ));
    rebuilt
        .verify(&f.proof, &p, 19, 100, &f.reply("0.0.2"), &deadline())
        .unwrap();
    assert!(!retired.exists());
}

#[test]
fn interrupted_exchange_keeps_previous_executable_and_durable_prior_identity() {
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    // The exchange failure occurs before any rename; both durable copies are retained.
    f.install
        .scratch_interrupt(Some(Interruption::Exchange(0)))
        .unwrap();
    f.install
        .apply(&f.proof, &p, plan, &deadline())
        .unwrap_err();
    assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
    assert_eq!(fs::read(f.sibling(2, 0, false)).unwrap(), contents(2)[0]);
    assert_eq!(fs::read(f.sibling(2, 0, true)).unwrap(), contents(1)[0]);
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = rebuilt.resume_plan(&f.proof, &p).unwrap();
    rebuilt.apply(&f.proof, &p, plan, &deadline()).unwrap();
    let outcome: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert_eq!(outcome["previous_instance"], json!(9));
}

#[test]
fn torn_journals_and_hostile_mutable_leaves_preserve_payload() {
    for leaf in [
        "install.lock",
        "payload-intent.json",
        "payload-outcome.json",
    ] {
        let f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let path = f.record(false).with_file_name(leaf);
        let protected = f.root.join("protected");
        put(&protected, b"owner bytes", 0o600);
        if path.exists() {
            fs::remove_file(&path).unwrap();
        }
        symlink(&protected, &path).unwrap();
        assert!(f.install.apply(&f.proof, &p, plan, &deadline()).is_err());
        assert_eq!(fs::read(&protected).unwrap(), b"owner bytes");
        assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
    }
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    f.install
        .scratch_interrupt(Some(Interruption::Intent))
        .unwrap();
    f.install
        .apply(&f.proof, &p, plan, &deadline())
        .unwrap_err();
    put(&f.record(true), b"{\"torn", 0o600);
    let reconstructed = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(reconstructed.resume_plan(&f.proof, &p).is_err());
    assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
}

#[test]
fn mutable_ancestry_replacements_after_plan_and_during_apply_never_escape() {
    for index in [0, 5, 6, 8, 9] {
        for during in [false, true] {
            let mut f = Fixture::new();
            f.verified(&package(1), 1);
            let p = package(2);
            let plan = f
                .install
                .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
                .unwrap();
            let parent = f.install.targets()[index].parent().unwrap().to_owned();
            let held = f.root.join("held-parent");
            let foreign = f.root.join("foreign-parent");
            fs::create_dir(&foreign).unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
            put(&foreign.join("untouched"), b"owner foreign", 0o600);
            if during {
                let moved = parent.clone();
                let saved = held.clone();
                let other = foreign.clone();
                f.install
                    .scratch_hook(Some(Arc::new(move |event| {
                        if event == Interruption::Staged(index) {
                            fs::rename(&moved, &saved).unwrap();
                            symlink(&other, &moved).unwrap();
                        }
                        Ok(())
                    })))
                    .unwrap();
            } else {
                fs::rename(&parent, &held).unwrap();
                symlink(&foreign, &parent).unwrap();
            }
            assert!(f.install.apply(&f.proof, &p, plan, &deadline()).is_err());
            assert_eq!(
                fs::read(foreign.join("untouched")).unwrap(),
                b"owner foreign"
            );
            assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1);
            assert_eq!(
                fs::read(held.join(f.install.targets()[index].file_name().unwrap())).unwrap(),
                installed_contents(&f, 1)[index]
            );
            if during {
                assert_eq!(
                    fs::read(held.join(f.sibling(2, index, false).file_name().unwrap())).unwrap(),
                    installed_contents(&f, 2)[index]
                );
            }
        }
    }
}

#[test]
fn hostile_stage_and_backup_leaves_never_publish_or_modify_foreign_bytes() {
    for backup in [false, true] {
        let f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let protected = f.root.join("owner");
        put(&protected, b"owner bytes", 0o755);
        symlink(&protected, f.sibling(2, 0, backup)).unwrap();
        assert!(f.install.apply(&f.proof, &p, plan, &deadline()).is_err());
        assert_eq!(fs::read(&protected).unwrap(), b"owner bytes");
        assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
        assert!(
            fs::symlink_metadata(f.sibling(2, 0, backup))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn partial_writes_are_unpublished_and_unknown_directory_sync_redetects() {
    for backup in [false, true] {
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let parent = f.install.targets()[0].parent().unwrap().to_owned();
        let count = Arc::new(AtomicU64::new(0));
        f.install
            .scratch_hook(Some(Arc::new(move |event| {
                if event == Interruption::Write
                    && count.fetch_add(1, Ordering::Relaxed) == u64::from(backup)
                {
                    let partial = fs::read_dir(&parent)
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| {
                            p.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .contains(".partial-")
                        })
                        .unwrap();
                    fs::write(partial, b"partial write").unwrap();
                    return Err(PayloadError::OutcomeUnknown);
                }
                Ok(())
            })))
            .unwrap();
        f.install
            .apply(&f.proof, &p, plan, &deadline())
            .unwrap_err();
        assert!(!f.sibling(2, 0, backup).exists());
        assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
        let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
        let plan = rebuilt.resume_plan(&f.proof, &p).unwrap();
        rebuilt.apply(&f.proof, &p, plan, &deadline()).unwrap();
    }
    let mut f = Fixture::new();
    f.verified(&package(1), 1);
    let p = package(2);
    let plan = f
        .install
        .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    let count = Arc::new(AtomicU64::new(0));
    f.install
        .scratch_hook(Some(Arc::new(move |event| {
            if event == Interruption::DirectorySync && count.fetch_add(1, Ordering::Relaxed) == 2 {
                return Err(PayloadError::OutcomeUnknown);
            }
            Ok(())
        })))
        .unwrap();
    f.install
        .apply(&f.proof, &p, plan, &deadline())
        .unwrap_err();
    assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(2)[0]);
    assert_eq!(fs::read(f.sibling(2, 0, true)).unwrap(), contents(1)[0]);
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = rebuilt.resume_plan(&f.proof, &p).unwrap();
    rebuilt.apply(&f.proof, &p, plan, &deadline()).unwrap();
}

#[test]
fn publication_and_staged_leaf_replacement_preserve_unadmitted_objects() {
    for case in 0..3 {
        let at_publication = case != 0;
        let same_inode = case == 2;
        let mut f = Fixture::new();
        f.verified(&package(1), 1);
        let p = package(2);
        let plan = f
            .install
            .plan(&f.proof, &p, OperationId(2), MatchingFiles::Preserve)
            .unwrap();
        let stage = f.sibling(2, 0, false);
        let held = f.root.join("held-stage");
        let saved = held.clone();
        let selected = stage.clone();
        let once = Arc::new(AtomicU64::new(0));
        f.install
            .scratch_hook(Some(Arc::new(move |event| {
                let wanted = if at_publication {
                    Interruption::Publish
                } else {
                    Interruption::Staged(0)
                };
                if event == wanted && once.fetch_add(1, Ordering::Relaxed) == 0 {
                    let candidate = if at_publication {
                        fs::read_dir(selected.parent().unwrap())
                            .unwrap()
                            .map(|e| e.unwrap().path())
                            .find(|p| {
                                p.file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .contains(".partial-")
                            })
                            .unwrap()
                    } else {
                        selected.clone()
                    };
                    if !same_inode {
                        fs::rename(&candidate, &saved).unwrap();
                    }
                    put(&candidate, b"unadmitted replacement", 0o755);
                }
                Ok(())
            })))
            .unwrap();
        assert_eq!(
            f.install.apply(&f.proof, &p, plan, &deadline()),
            Err(PayloadError::OutcomeUnknown)
        );
        assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
        assert_eq!(held.exists(), !same_inode);
        if at_publication {
            assert!(!stage.exists());
            if !same_inode {
                assert!(
                    fs::read_dir(stage.parent().unwrap())
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .any(|p| p
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains(".partial-")
                            && fs::read(p).unwrap() == b"unadmitted replacement")
                );
            }
        } else {
            assert_eq!(fs::read(stage).unwrap(), b"unadmitted replacement");
            assert_eq!(fs::read(f.sibling(2, 0, true)).unwrap(), contents(1)[0]);
        }
    }
}

#[test]
fn actual_stage_output_is_accepted_by_rust_without_executing_artifacts() {
    let f = Fixture::new();
    let input = f.root.join("stage-input");
    let output = f.root.join("stage-output");
    fs::create_dir(&input).unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).unwrap();
    let data = contents(1);
    let mut m = manifest(1, &data);
    for (name, bytes) in FILES.iter().zip(&data) {
        put(&input.join(name), bytes, 0o600);
    }
    let library = elf(1);
    m.libraries[0].sha256 = hex(&library);
    put(
        &input.join("libraries").join(&m.libraries[0].name),
        &library,
        0o600,
    );
    put(
        &input.join("provenance.json"),
        &serde_json::to_vec(&m).unwrap(),
        0o600,
    );
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new(project.join("scripts/lead/impl-env.sh"))
        .args([
            "env",
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent/crosspane-test-bus",
            "bash",
        ])
        .arg(project.join("scripts/installer/stage-linux.sh"))
        .arg(&input)
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let bytes = fs::read(output.join("payload.tar")).unwrap();
    let p = Package::read(
        bytes.as_slice(),
        Architecture::native().unwrap(),
        sha256(&bytes),
    )
    .unwrap();
    assert_eq!(p.manifest().source_revision, m.source_revision);
    assert_eq!(
        fs::read_to_string(output.join("payload.sha256")).unwrap(),
        format!("{}\n", hex(&bytes))
    );
    for (name, expected) in FILES.iter().zip(&data) {
        assert_eq!(fs::read(input.join(name)).unwrap(), *expected);
    }
    assert!(f.install.targets().iter().all(|p| !p.exists()));
}

#[test]
fn declared_binary_member_and_aggregate_archive_limits_are_enforced() {
    {
        let mut data = contents(1);
        data[0].resize(MAX_MEMBER_BYTES + 1, 0);
        // Complete ELF member/body, matching manifest/hash and valid framing: only size is invalid.
        let bytes = archive(&manifest(1, &data), &data);
        assert!(bytes.len() < MAX_ARCHIVE_BYTES);
        assert!(read(&bytes).is_err());
    }
    struct Zeros(usize);
    impl std::io::Read for Zeros {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = out.len().min(self.0);
            out[..n].fill(0);
            self.0 -= n;
            Ok(n)
        }
    }
    let mut reader = Zeros(MAX_ARCHIVE_BYTES + 2);
    assert!(Package::read(&mut reader, Architecture::native().unwrap(), [0; 32]).is_err());
    assert_eq!(reader.0, 1); // the reader stops after the single bounded overflow byte.
}

#[test]
fn applied_unknown_receipt_is_owned_by_prior_bytes_and_newer_payload_is_consented() {
    let f = Fixture::new();
    let prior = package(1);
    f.applied(&prior, 5);
    assert!(
        !f.io.target().runtime_dir().exists(),
        "publication did not start an agent"
    );
    let record: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    assert_eq!(record["phase"], "Applied");
    assert_eq!(record["receipt"]["operation_id"], 5);
    for row in record["receipt"]["resources"].as_array().unwrap() {
        assert_eq!(row["ownership"], "Created");
        assert_eq!(row["before"], "Absent");
        assert_eq!(row["after"], "Matching");
        assert_eq!(row["outcome"], "Unknown");
    }
    let next = package(2);
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    let rows = rebuilt.detect(&f.proof, &next).unwrap();
    assert!(
        rows.iter()
            .all(|r| r.ownership == ResourceOwnership::Created
                && r.before == crosspane_installer_core::ResourceObservation::Different)
    );
    let original = installed_contents(&f, 1);
    let plan = rebuilt
        .plan(&f.proof, &next, OperationId(6), MatchingFiles::Preserve)
        .unwrap();
    for (i, path) in rebuilt.targets().iter().enumerate() {
        assert_eq!(fs::read(path).unwrap(), original[i]);
    }
    let receipt = rebuilt.apply(&f.proof, &next, plan, &deadline()).unwrap();
    assert!(!receipt.unfinished.is_empty());
    assert!(
        receipt
            .resources
            .iter()
            .all(|r| r.outcome == MutationOutcome::Unknown)
    );
    for (i, path) in rebuilt.targets().iter().enumerate() {
        assert_eq!(fs::read(path).unwrap(), installed_contents(&f, 2)[i]);
    }
}
#[test]
fn applied_unknown_same_payload_resumes_without_faking_health() {
    let f = Fixture::new();
    let p = package(1);
    f.applied(&p, 5);
    let rebuilt = PayloadInstaller::new(f.io.clone()).unwrap();
    assert_eq!(
        rebuilt
            .plan(&f.proof, &p, OperationId(6), MatchingFiles::Preserve)
            .unwrap_err(),
        PayloadError::Pending
    );
    let plan = rebuilt.resume_plan(&f.proof, &p).unwrap();
    let result = rebuilt.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert_eq!(result.operation_id, OperationId(5));
    assert!(
        result
            .resources
            .iter()
            .all(|r| r.outcome == MutationOutcome::Unknown)
    );
}
#[test]
fn applied_previous_hashes_do_not_adopt_modified_files_or_displace_recovery() {
    for kind in 0..3 {
        let f = Fixture::new();
        f.applied(&package(1), 5);
        let path = match kind {
            0 => f.install.targets()[0].clone(),
            1 => f.sibling(5, 0, false),
            _ => f.sibling(5, 0, true),
        };
        put(&path, b"user modification", 0o755);
        assert!(
            f.install
                .plan(
                    &f.proof,
                    &package(2),
                    OperationId(6),
                    MatchingFiles::Preserve
                )
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"user modification");
        let record: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
        assert_eq!(record["receipt"]["operation_id"], 5);
    }
}
#[test]
fn applied_receipt_changed_after_preview_refuses_before_replacement() {
    let f = Fixture::new();
    f.applied(&package(1), 5);
    let next = package(2);
    let plan = f
        .install
        .plan(&f.proof, &next, OperationId(6), MatchingFiles::Preserve)
        .unwrap();
    let mut record: Value = serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
    record["receipt"]["operation_id"] = json!(7);
    put(
        &f.record(false),
        &serde_json::to_vec(&record).unwrap(),
        0o600,
    );
    assert_eq!(
        f.install.apply(&f.proof, &next, plan, &deadline()),
        Err(PayloadError::Pending)
    );
    assert_eq!(fs::read(&f.install.targets()[0]).unwrap(), contents(1)[0]);
}

/// WP-4.32: setup owns its install paths. Every stuck shape seen live ends installed, with what
/// was there saved in the backup folder and nothing outside the install paths touched.
mod owned_install_paths {
    use super::*;
    use crosspane_installer::platform::linux::integration::{NativePayloads, Payloads};
    use crosspane_installer_core::ResourceObservation;

    /// One installer run against the scratch home: detect, plan (resume first when asked, as
    /// the worker does), apply. Returns the backup folder it reported.
    fn run_install(f: &Fixture, p: &Package, op: u64) -> Option<PathBuf> {
        let mut payloads = NativePayloads::new(f.io.clone()).unwrap();
        match payloads.detect(&f.proof, p) {
            Ok(rows) => assert!(
                !rows
                    .iter()
                    .all(|r| r.before == ResourceObservation::Matching
                        && r.ownership == ResourceOwnership::Created),
                "detect said current before installing"
            ),
            Err(error) => assert!(
                matches!(
                    error,
                    PayloadError::Pending
                        | PayloadError::Foreign
                        | PayloadError::OutcomeUnknown
                        | PayloadError::Native(_)
                ),
                "{error:?}"
            ),
        }
        let operation = OperationId(op);
        let preview = match payloads.plan(&f.proof, p, operation, false) {
            Err(PayloadError::Pending) => payloads.plan(&f.proof, p, operation, true),
            other => other,
        }
        .unwrap();
        assert_eq!(preview.version, p.manifest().product_version);
        payloads.apply(&f.proof, p, operation, &deadline()).unwrap();
        payloads.backup()
    }

    /// The new agent answers, the install is recorded, and a later run has nothing to do.
    fn finish(f: &Fixture, p: &Package, instance: u64) {
        for (index, bytes) in installed_contents(f, version_of(p)).iter().enumerate() {
            assert_eq!(
                &fs::read(&f.install.targets()[index]).unwrap(),
                bytes,
                "{index}"
            );
        }
        f.bootstrap_instance(instance);
        let mut payloads = NativePayloads::new(f.io.clone()).unwrap();
        payloads
            .verify(
                &f.proof,
                p,
                &f.reply(&p.manifest().product_version),
                100,
                &deadline(),
            )
            .unwrap();
        let rows = NativePayloads::new(f.io.clone())
            .unwrap()
            .detect(&f.proof, p)
            .unwrap();
        assert!(
            rows.iter()
                .all(|r| r.before == ResourceObservation::Matching
                    && r.ownership == ResourceOwnership::Created)
        );
    }

    fn version_of(p: &Package) -> u8 {
        p.manifest()
            .product_version
            .rsplit('.')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn live_runtime(f: &Fixture, instance: u64) -> std::os::unix::net::UnixListener {
        f.bootstrap_instance(instance);
        let listener = std::os::unix::net::UnixListener::bind(f.io.target().socket_path()).unwrap();
        fs::set_permissions(
            f.io.target().socket_path(),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        listener
    }

    fn state(f: &Fixture) -> PathBuf {
        f.io.target().paths().state_home.join("crosspane/installer")
    }

    /// The live Linux case (owner, 2026-10-05): an earlier build's install is running and
    /// healthy from its unit. Its Applied payload-outcome.json, a quarantine record, the install
    /// lock and a "payload planned" resume hint sit in the installer folder, and the runtime
    /// holds a live agent.sock and bootstrap.json. The newer build upgrades it by itself.
    #[test]
    fn a_running_older_build_with_its_unfinished_records_is_upgraded() {
        let f = Fixture::new();
        f.applied(&package(1), 1);
        let quarantine: Vec<_> = fs::read_dir(state(&f))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("quarantine-"))
            .collect();
        assert_eq!(quarantine.len(), 1);
        put(
            &state(&f).join("resume-hint.json"),
            br#"{"schema":1,"step":"payload","phase":"planned"}"#,
            0o600,
        );
        let _live = live_runtime(&f, 9);
        // Upgraded by the next build, or reinstalled by the same build that was cut off.
        for (p, op) in [(package(2), 2), (package(2), 3), (package(3), 4)] {
            run_install(&f, &p, op);
            // The running agent is replaced only by a new instance; the old one can't confirm it.
            let outcome: Value =
                serde_json::from_slice(&fs::read(f.record(false)).unwrap()).unwrap();
            assert_eq!(outcome["previous_instance"], json!(9));
            assert!(state(&f).join("resume-hint.json").exists());
            assert!(f.io.target().socket_path().exists());
        }
        // An install record that can't be continued is kept in the backup, not deleted.
        let backups = f.io.target().paths().state_home.join("crosspane/backups");
        assert!(fs::read_dir(&backups).unwrap().any(|folder| {
            folder
                .unwrap()
                .path()
                .join("installer/payload-outcome.json")
                .exists()
        }));
        finish(&f, &package(3), 10);
    }

    /// The same, with the earlier build's install finished (Verified) and a stale previous copy
    /// next to the agent from an older run.
    #[test]
    fn a_finished_older_install_with_stray_leftovers_is_upgraded() {
        let f = Fixture::new();
        f.verified(&package(1), 1);
        // Left by run 1 and not accounted for any more: it blocks continuing run 1's records.
        let stray = f.sibling(1, 0, true);
        put(&stray, b"older leftover", 0o755);
        let _live = live_runtime(&f, 9);
        let backup = run_install(&f, &package(2), 2).unwrap();
        assert_eq!(
            fs::read(backup.join("leftovers/.crosspane-previous-1-0")).unwrap(),
            b"older leftover"
        );
        assert!(!stray.exists());
        finish(&f, &package(2), 10);
    }

    /// Killed at every publication point of an earlier run (same build and a newer build),
    /// the next run ends installed.
    #[test]
    fn a_run_killed_anywhere_in_publication_converges() {
        let points = [
            Interruption::Intent,
            Interruption::Staged(0),
            Interruption::BackedUp(3),
            Interruption::Exchange(0),
            Interruption::Replaced(2),
            Interruption::Write,
            Interruption::Publish,
            Interruption::Outcome,
            Interruption::BeforeJournal(true, 0),
            Interruption::AfterJournal(false, 1),
        ];
        for newer in [false, true] {
            for point in points {
                let mut f = Fixture::new();
                f.verified(&package(1), 1);
                let first = if newer { package(2) } else { package(3) };
                let plan = f
                    .install
                    .plan(&f.proof, &first, OperationId(2), MatchingFiles::Preserve)
                    .unwrap();
                f.install.scratch_interrupt(Some(point)).unwrap();
                let _ = f.install.apply(&f.proof, &first, plan, &deadline());
                f.install.scratch_interrupt(None).unwrap();
                let p = if newer { package(4) } else { first };
                run_install(&f, &p, 3);
                finish(&f, &p, 10);
            }
        }
    }

    /// Bytes no install recorded, at every install path, with a running agent: saved, then
    /// replaced.
    #[test]
    fn unrecorded_files_at_every_install_path_are_saved_and_replaced() {
        let f = Fixture::new();
        for (index, path) in f.install.targets().iter().enumerate() {
            put(
                path,
                format!("hand-made {index}").as_bytes(),
                if index < 4 { 0o755 } else { 0o644 },
            );
        }
        let _live = live_runtime(&f, 9);
        let backup = run_install(&f, &package(1), 1).unwrap();
        for (index, path) in f.install.targets().iter().enumerate() {
            let name = path.file_name().unwrap();
            assert_eq!(
                fs::read(backup.join(name)).unwrap(),
                format!("hand-made {index}").as_bytes()
            );
        }
        finish(&f, &package(1), 10);
    }

    /// A link at an install path is moved aside, never followed; whatever it points to stays.
    /// So are directories, wrong modes and extra hard links.
    #[test]
    fn links_directories_modes_and_hard_links_are_moved_aside_never_followed() {
        let f = Fixture::new();
        let outside = f.root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let targets = f.install.targets().to_vec();
        for (index, path) in targets.iter().enumerate() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            match index % 4 {
                0 => {
                    let aim = outside.join(format!("aim-{index}"));
                    fs::write(&aim, b"outside").unwrap();
                    symlink(&aim, path).unwrap();
                }
                1 => fs::create_dir_all(path.join("inner")).unwrap(),
                2 => put(path, b"group writable", 0o775),
                _ => {
                    let other = outside.join(format!("link-{index}"));
                    fs::write(&other, b"hard").unwrap();
                    fs::hard_link(&other, path).unwrap();
                }
            }
        }
        let backup = run_install(&f, &package(1), 1).unwrap();
        for (index, path) in targets.iter().enumerate() {
            let saved = backup.join(path.file_name().unwrap());
            let meta = fs::symlink_metadata(&saved).unwrap();
            match index % 4 {
                0 => assert!(meta.file_type().is_symlink()),
                1 => assert!(meta.is_dir()),
                _ => assert!(meta.is_file()),
            }
        }
        for entry in fs::read_dir(&outside).unwrap() {
            assert!(fs::read(entry.unwrap().path()).is_ok_and(|b| b == b"outside" || b == b"hard"));
        }
        finish(&f, &package(1), 10);
    }

    /// An unreadable or torn record is kept in the backup; the install starts fresh.
    #[test]
    fn torn_records_are_set_aside() {
        for intent in [false, true] {
            let f = Fixture::new();
            f.verified(&package(1), 1);
            put(&f.record(intent), b"{\"torn", 0o600);
            let backup = run_install(&f, &package(2), 2).unwrap();
            assert_eq!(
                fs::read(backup.join(format!(
                    "installer/{}",
                    f.record(intent).file_name().unwrap().to_str().unwrap()
                )))
                .unwrap(),
                b"{\"torn"
            );
            finish(&f, &package(2), 10);
        }
    }

    /// Twice in a row: the second run has nothing to do. At most three backup folders stay.
    #[test]
    fn repeated_runs_converge_and_keep_three_backups() {
        let f = Fixture::new();
        let root = f.io.target().paths().state_home.join("crosspane/backups");
        for (n, old) in ["20200101-000000", "20210101-000000", "20220101-000000"]
            .iter()
            .enumerate()
        {
            put(
                &root.join(old).join("note"),
                format!("{n}").as_bytes(),
                0o600,
            );
            fs::set_permissions(root.join(old), fs::Permissions::from_mode(0o700)).unwrap();
        }
        for dir in [root.clone(), root.parent().unwrap().to_owned()] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        put(&f.install.targets()[7], b"hand-made icon", 0o644);
        let backup = run_install(&f, &package(1), 1).unwrap();
        finish(&f, &package(1), 10);
        let mut kept: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        kept.sort();
        assert_eq!(kept.len(), KEEP_BACKUPS);
        assert!(kept.contains(&backup));
        assert!(!root.join("20200101-000000").exists());
        assert!(root.join("20220101-000000/note").exists());
    }
}
