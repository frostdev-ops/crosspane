#![allow(clippy::unwrap_used)]
use super::*;
use crate::agent_contract::*;
use crate::platform::linux::payload::FILES;
use crate::platform::linux::removal::TrackedAgent;
use crate::platform::linux::{native_io::*, payload::*, service::*};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Condvar, mpsc},
    time::{Duration, Instant},
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

#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }
    fn entered(&self) {
        let state = self.state.lock().unwrap();
        let (state, wait) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |s| !s.0)
            .unwrap();
        assert!(state.0 && !wait.timed_out());
    }
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}
struct Probe {
    facts: Mutex<ProcessFacts>,
    process_gate: Mutex<Option<Arc<Gate>>>,
    exit_gate: Mutex<Option<Arc<Gate>>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> std::result::Result<ProcessFacts, NativeError> {
        d.check()?;
        if let Some(gate) = self.process_gate.lock().unwrap().clone() {
            gate.block();
        }
        Ok(self.facts.lock().unwrap().clone())
    }
}
impl ExitReader for Probe {
    fn snapshot(
        &self,
        _: u32,
        d: &Deadline,
    ) -> std::result::Result<Option<ProcessFacts>, NativeError> {
        d.check()?;
        if let Some(gate) = self.exit_gate.lock().unwrap().clone() {
            gate.block();
        }
        Ok(Some(self.facts.lock().unwrap().clone()))
    }
}
struct Runner;
impl CommandRunner for Runner {
    fn run(
        &self,
        c: &CommandSpec,
        d: &Deadline,
    ) -> std::result::Result<CommandOutput, NativeError> {
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
fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn facts(io: &LinuxNativeIo) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
        desktop: crate::platform::linux::detect::Desktop::Hyprland,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        compositor_version: [0, 56, 0],
        protocols_ready: true,
        runtime_libraries_ready: true,
        compositor_managed: true,
        graphical_target_active: true,
        graphical_sessions: 1,
        session_id: "scratch".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    }
}
struct Fixture {
    io: Arc<LinuxNativeIo>,
    proof: SupportProof,
    probe: Arc<Probe>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp419-association-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe {
            facts: Mutex::new(ProcessFacts {
                uid: rustix::process::geteuid().as_raw(),
                executable: root.join(".local/bin/crosspane-agent"),
                generation: 77,
            }),
            process_gate: Mutex::default(),
            exit_gate: Mutex::default(),
        });
        let io = Arc::new(LinuxNativeIo::scratch(&root, Arc::new(Runner), probe.clone()).unwrap());
        let proof = io
            .scratch_support(SupportObservations {
                uid: io.target().paths().uid,
                desktop: crate::platform::linux::detect::Desktop::Hyprland,
                architecture: std::env::consts::ARCH.into(),
                arch_based: true,
                compositor_version: [0, 56, 0],
                protocols_ready: true,
                runtime_libraries_ready: true,
                compositor_managed: true,
                graphical_target_active: true,
                graphical_sessions: 1,
                session_id: "scratch".into(),
                session_type: "wayland".into(),
                seat: "seat0".into(),
                active: true,
            })
            .unwrap();
        for path in [
            io.target().paths().prefix.join("bin"),
            io.target().paths().config_home.join("crosspane"),
            io.target().paths().state_home.join("crosspane"),
            io.target().paths().data_home.join("crosspane"),
            io.target().runtime_dir().to_path_buf(),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        Self { io, proof, probe }
    }
    fn bootstrap(io: &Arc<LinuxNativeIo>, proof: &SupportProof) {
        io.atomic_write(proof,&io.target().runtime_dir().join("bootstrap.json"),&serde_json::to_vec(&json!({"schema_version":1,"instance_id":9,"pid":4242,"started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":io.target().runtime_dir()})).unwrap()).unwrap();
    }
    fn package() -> Package {
        package_for(if std::env::consts::ARCH == "x86_64" {
            Architecture::X86_64
        } else {
            Architecture::Aarch64
        })
    }
    fn install(&self) -> Package {
        let p = Self::package();
        let install = PayloadInstaller::new(self.io.clone()).unwrap();
        let plan = install
            .plan(&self.proof, &p, OperationId(47), MatchingFiles::Preserve)
            .unwrap();
        install.apply(&self.proof, &p, plan, &deadline()).unwrap();
        Self::bootstrap(&self.io, &self.proof);
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let i = &mut v["result"]["installer"]["instance"];
        i["uid"] = json!(self.io.target().paths().uid);
        i["exe"] = json!(self.io.target().agent_path());
        i["runtime_dir"] = json!(self.io.target().runtime_dir());
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
        install
            .verify(&self.proof, &p, 19, 100, &reply, &deadline())
            .unwrap();
        p
    }
    fn plan(
        &self,
        watch: Option<Arc<TrackedAgent>>,
    ) -> uninstall::UninstallResult<(UninstallPlan, UninstallConsent)> {
        let cp = CleanupPlanner::default();
        let inventory =
            CleanupInventory::admit(self.io.admit_cleanup(&deadline()).unwrap(), &deadline())
                .unwrap();
        let c = cp
            .plan(inventory, 1, OperationId(100), RemovalSelection::default())
            .unwrap();
        let cc = cp.consent(&c, 1, OperationId(100), &deadline()).unwrap();
        let up = UninstallPlanner::default();
        let plan = up.plan(c, self.io.clone(), watch)?;
        let consent = up.consent(&plan, cc, 1, OperationId(100))?;
        Ok((plan, consent))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.io.validate_target().unwrap();
        std::fs::remove_dir_all(&self.io.target().paths().home).unwrap();
    }
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
    let data: Vec<Vec<u8>> = (0..FILES.len())
        .map(|i| match i {
            0..=3 => elf.clone(),
            4 => include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/linux/crosspane-agent.service"
            ))
            .to_vec(),
            5 => include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/linux/crosspane-settings.desktop"
            ))
            .to_vec(),
            6 => include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/linux/crosspane-installer.desktop"
            ))
            .to_vec(),
            7 => icon.to_vec(),
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

#[test]
fn shared_executable_different_state_runtime_watch_cannot_authorize_cleanup() {
    let f = Fixture::new();
    let _p = f.install();
    let other = f.io.second_scratch_context(&f.proof);
    let proof = other
        .scratch_support(SupportObservations {
            uid: other.target().paths().uid,
            desktop: crate::platform::linux::detect::Desktop::Hyprland,
            architecture: std::env::consts::ARCH.into(),
            arch_based: true,
            compositor_version: [0, 56, 0],
            protocols_ready: true,
            runtime_libraries_ready: true,
            compositor_managed: true,
            graphical_target_active: true,
            graphical_sessions: 1,
            session_id: "scratch".into(),
            session_type: "wayland".into(),
            seat: "seat0".into(),
            active: true,
        })
        .unwrap();
    Fixture::bootstrap(&other, &proof);
    assert_eq!(other.target().agent_path(), f.io.target().agent_path());
    assert_eq!(format!("{:?}", f.io.target_binding()), "TargetBinding(..)");
    assert_ne!(other.target_binding(), f.io.target_binding());
    assert_ne!(other.target().runtime_dir(), f.io.target().runtime_dir());
    assert_ne!(
        other.target().paths().state_home,
        f.io.target().paths().state_home
    );
    let watch =
        Arc::new(TrackedAgent::scratch_capture(other, f.probe.clone(), &deadline()).unwrap());
    assert!(matches!(
        f.plan(Some(watch)),
        Err(UninstallError::Removal(RemovalError::NotClean))
    ));
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
}
#[test]
fn shared_executable_different_service_context_refuses_before_intent_io() {
    let f = Fixture::new();
    let p = f.install();
    let other = f.io.second_scratch_context(&f.proof);
    let proof = other.scratch_support(facts(&other)).unwrap();
    other
        .create_private_dir(&proof, &other.target().paths().runtime_home.join("systemd"))
        .unwrap();
    let _listener =
        UnixListener::bind(other.target().paths().runtime_home.join("systemd/private")).unwrap();
    let records = PayloadInstaller::new(other.clone())
        .unwrap()
        .rendered_resources(&p)
        .unwrap();
    let service =
        Arc::new(LinuxService::new(other, BTreeMap::new(), records, &deadline()).unwrap());
    let (plan, consent) = f.plan(None).unwrap();
    assert!(plan.begin(consent, service, &deadline()).is_err());
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
fn original_running_revalidation_caps_noncooperative_process_and_exit_readers() {
    for process in [false, true] {
        for cancel in [false, true] {
            let f = Fixture::new();
            let _p = f.install();
            let watch = Arc::new(
                TrackedAgent::scratch_capture(f.io.clone(), f.probe.clone(), &deadline()).unwrap(),
            );
            let running = watch.running_check();
            assert_eq!(format!("{running:?}"), "OriginalRunning(..)");
            assert_eq!(running.pid(), 4242);
            drop(watch);
            let baseline = Arc::strong_count(&f.io);
            let gate = Arc::new(Gate::default());
            *if process {
                f.probe.process_gate.lock().unwrap()
            } else {
                f.probe.exit_gate.lock().unwrap()
            } = Some(gate.clone());
            let cancellation = Cancellation::default();
            let d = Deadline::new(if cancel { 5000 } else { 100 }, cancellation.clone()).unwrap();
            let (send, receive) = mpsc::sync_channel(1);
            let thread = std::thread::spawn(move || {
                send.send(running.revalidate(&d)).unwrap();
            });
            gate.entered();
            if cancel {
                cancellation.cancel();
            }
            let result = receive.recv_timeout(Duration::from_secs(1));
            // The calling TrackedAgent has been dropped; an equal count now includes its worker.
            let retained = Arc::strong_count(&f.io) >= baseline;
            gate.release();
            thread.join().unwrap();
            let error = result.unwrap().unwrap_err();
            assert_eq!(
                error,
                RemovalError::Native(if cancel {
                    NativeError::Cancelled
                } else {
                    NativeError::Timeout
                })
            );
            assert!(
                retained,
                "worker must retain the original target while reader is blocked"
            );
            let limit = Instant::now() + Duration::from_secs(1);
            while Arc::strong_count(&f.io) >= baseline && Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                Arc::strong_count(&f.io) < baseline,
                "worker released only after the reader finished"
            );
        }
    }
}
