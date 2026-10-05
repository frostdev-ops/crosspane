#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! b1 admission + b2 adapter matrix. All /Library access maps to explicit scratch, and every
//! command uses Recorder. No SystemCommandRunner, native Installer, codesign, or agent is run.
use crosspane_installer::agent_contract;
#[path = "../src/platform/macos/audio_package.rs"]
#[allow(dead_code)]
mod audio_package;
#[path = "../src/platform/macos/launchd_observation.rs"]
#[allow(dead_code)]
mod launchd_observation;
#[path = "../src/platform/macos/native_io.rs"]
#[allow(dead_code, unused_imports)]
mod native_io;
use audio_package::*;
use native_io::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
const TIME: u64 = 1_790_950_100_000;
const INST: &str = "/Library/Application Support/Crosspane/Installer";
const DRIVER: &str = "/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver";
const METADATA: [&str; 6] = [
    "/Library/Audio/Plug-Ins/HAL",
    DRIVER,
    "/Library/Application Support",
    "/Library/Application Support/Crosspane",
    INST,
    "/Library/Application Support/Crosspane/Installer/previous",
];
static NONCE: AtomicU64 = AtomicU64::new(1);
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        0
    }
}
impl AudioClock for TestClock {
    fn unix_ms(&self) -> NativeResult<u64> {
        Ok(self.0.load(Ordering::Acquire))
    }
}
struct Support(Mutex<SupportObservation>);
impl SupportProbe for Support {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        Ok(self.0.lock().unwrap().clone())
    }
}
struct Signatures;
impl SignatureProbe for Signatures {
    fn observe(
        &self,
        _: &Path,
        rule: &SigningRequirement,
        _: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        Ok(SignatureObservation {
            strict_verified: true,
            team_identifier: "ABCDE12345".into(),
            identifier: rule.identifier.clone(),
            designated_requirement: rule.designated_requirement.clone(),
            entitlements: rule.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        })
    }
}
struct Recorder {
    home: PathBuf,
    calls: Mutex<Vec<CommandSpec>>,
    result: Mutex<NativeResult<CommandOutput>>,
    expected_substitution: Mutex<Option<Vec<u8>>>,
    observed_bytes: Mutex<Vec<Vec<u8>>>,
}
impl CommandRunner for Recorder {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        deadline.check()?;
        assert_eq!(spec.program(), Path::new("/usr/bin/open"));
        assert_eq!(&spec.args()[..3], ["-b", "com.apple.installer", "--"]);
        let path = Path::new(&spec.args()[3]);
        assert_eq!(
            path.parent().unwrap(),
            self.home
                .join("Library/Application Support/Crosspane/Installer/packages")
        );
        let bytes = fs::read(path).unwrap(); // Only the scratch selected-user staged package.
        let expected = self
            .expected_substitution
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                if path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("-install-")
                {
                    b"inert-install".to_vec()
                } else {
                    b"inert-remove".to_vec()
                }
            });
        assert_eq!(bytes, expected);
        self.observed_bytes.lock().unwrap().push(bytes);
        self.calls.lock().unwrap().push(spec.clone());
        self.result.lock().unwrap().clone()
    }
}
#[derive(Clone)]
enum Override {
    Uid(u32),
    Mode(u32),
    Device(u64),
    Error(NativeError),
}
type Overrides = Arc<Mutex<BTreeMap<(String, PathBuf), Override>>>;
struct Fixture {
    base: PathBuf,
    library: PathBuf,
    io: Arc<MacNativeIo>,
    proof: SupportProof,
    clock: Arc<TestClock>,
    support: Arc<Support>,
    runner: Arc<Recorder>,
    overrides: Overrides,
    events: Arc<Mutex<Vec<(String, PathBuf)>>>,
}
fn directory(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn bytes(path: &Path, value: &[u8], mode: u32) {
    if !path.parent().unwrap().exists() {
        directory(path.parent().unwrap(), 0o700);
    }
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
// Match the frozen writers' key order while retaining deliberately invalid/extra fixture fields.
fn canonical(value: &Value) -> Vec<u8> {
    struct Wire<'a>(&'a Value);
    impl serde::Serialize for Wire<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self.0 {
                Value::Object(fields) => {
                    use serde::ser::SerializeMap;
                    let order = [
                        "schema_version",
                        "version",
                        "packages",
                        "kind",
                        "file",
                        "sha256",
                        "result",
                        "driver_version",
                        "at_unix_ms",
                    ];
                    let mut map = serializer.serialize_map(Some(fields.len()))?;
                    for key in order {
                        if let Some(value) = fields.get(key) {
                            map.serialize_entry(key, &Wire(value))?;
                        }
                    }
                    for (key, value) in fields {
                        if !order.contains(&key.as_str()) {
                            map.serialize_entry(key, &Wire(value))?;
                        }
                    }
                    map.end()
                }
                Value::Array(values) => {
                    use serde::ser::SerializeSeq;
                    let mut seq = serializer.serialize_seq(Some(values.len()))?;
                    for value in values {
                        seq.serialize_element(&Wire(value))?;
                    }
                    seq.end()
                }
                value => serde::Serialize::serialize(value, serializer),
            }
        }
    }
    let mut raw = serde_json::to_vec(&Wire(value)).unwrap();
    raw.push(b'\n');
    raw
}
fn digest(value: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, value)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
impl Fixture {
    fn new() -> Self {
        let uid = rustix::process::geteuid().as_raw();
        assert_ne!(uid, 0);
        let base = PathBuf::from(format!(
            "/private/tmp/crosspane-audio-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let home = base.join("home");
        let temporary = base.join("temporary");
        let source = home.join("distribution");
        let library = base.join("Library");
        for path in [&home, &temporary, &source, &library] {
            directory(path, 0o700);
        }
        for path in [
            "Audio",
            "Audio/Plug-Ins",
            "Audio/Plug-Ins/HAL",
            "Application Support",
            "Application Support/Crosspane",
            "Application Support/Crosspane/Installer",
        ] {
            directory(&library.join(path), 0o755);
        }
        let mut target = MacTarget::scratch(TargetPaths {
            uid,
            home: home.clone(),
            gui_tmpdir: temporary.clone(),
            runtime_override: Some(temporary.join("runtime")),
            payload_root: source.clone(),
        })
        .unwrap();
        let physical = library.clone();
        target.test_path = Some(Arc::new(move |path| {
            path.strip_prefix("/Library")
                .map(|suffix| physical.join(suffix))
                .unwrap_or_else(|_| path.to_owned())
        }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let overrides: Overrides = Arc::new(Mutex::new(BTreeMap::new()));
        let injected = overrides.clone();
        target.test_hook = Some(Arc::new(move |stage, path, mut file| {
            captured
                .lock()
                .unwrap()
                .push((stage.into(), path.to_owned()));
            if stage.starts_with("audio-") {
                let allowed = match stage {
                    "audio-metadata" => METADATA.contains(&path.to_str().unwrap()),
                    "audio-ancestor" => [
                        "/Library",
                        "/Library/Application Support",
                        "/Library/Application Support/Crosspane",
                        INST,
                    ]
                    .contains(&path.to_str().unwrap()),
                    "audio-outcome" => {
                        path.as_os_str() == Path::new(INST).join("audio-outcome.json").as_os_str()
                            || path.as_os_str()
                                == Path::new(INST)
                                    .join("audio-removal-outcome.json")
                                    .as_os_str()
                    }
                    _ => false,
                };
                assert!(allowed, "unapproved root observation: {stage} {path:?}");
                if let Some(s) = &mut file {
                    s.uid = 0;
                }
            }
            if let Some(value) = injected
                .lock()
                .unwrap()
                .get(&(stage.into(), path.to_owned()))
            {
                match value {
                    Override::Error(e) => return Err(*e),
                    Override::Uid(uid) => file.as_mut().unwrap().uid = *uid,
                    Override::Mode(mode) => file.as_mut().unwrap().mode = *mode,
                    Override::Device(dev) => file.as_mut().unwrap().device = *dev,
                }
            }
            Ok(file)
        }));
        let clock = Arc::new(TestClock(AtomicU64::new(TIME)));
        let support = Arc::new(Support(Mutex::new(SupportObservation {
            macos_major: 26,
            apple_silicon: true,
            gui: GuiObservation {
                console_uid: Some(uid),
                interactive_uid: Some(uid),
                console_session: "fixture-Aqua".into(),
                interactive_session: "fixture-Aqua".into(),
                active: true,
            },
            gui_tmpdir: temporary,
        })));
        let runner = Arc::new(Recorder {
            home,
            calls: Mutex::new(Vec::new()),
            expected_substitution: Mutex::new(None),
            observed_bytes: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(CommandOutput {
                code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })),
        });
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support.clone(),
                Arc::new(Signatures),
                clock.clone(),
            )
            .unwrap(),
        );
        let main = source.join("Crosspane.app/Contents/MacOS/Crosspane");
        bytes(&main, b"inert-main", 0o755);
        let main = io
            .admit_main_signature(
                &main,
                &SigningRequirement {
                    role: ArtifactRole::Agent,
                    identifier: AGENT_LABEL.into(),
                    designated_requirement: "fixture-only".into(),
                    entitlements: BTreeMap::new(),
                },
                &Deadline::new(5000, clock.clone(), Cancellation::default()).unwrap(),
            )
            .unwrap();
        let proof = io
            .admit_support(
                &main,
                &Deadline::new(5000, clock.clone(), Cancellation::default()).unwrap(),
            )
            .unwrap();
        let f = Self {
            base,
            library,
            io,
            proof,
            clock,
            support,
            runner,
            overrides,
            events,
        };
        bytes(
            &f.source().join("CrosspaneAudio-install-0.1.0.pkg"),
            b"inert-install",
            0o600,
        );
        bytes(
            &f.source().join("CrosspaneAudio-remove-0.1.0.pkg"),
            b"inert-remove",
            0o600,
        );
        f.manifest(f.manifest_value());
        f
    }
    fn deadline(&self) -> Deadline {
        Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn source(&self) -> PathBuf {
        self.io.target().paths().payload_root.clone()
    }
    fn physical(&self, path: &str) -> PathBuf {
        self.library.join(path.strip_prefix("/Library/").unwrap())
    }
    fn manifest_value(&self) -> Value {
        json!({"schema_version":1,"version":"0.1.0","packages":[
        {"kind":"install","file":"CrosspaneAudio-install-0.1.0.pkg","sha256":digest(b"inert-install")},
        {"kind":"remove","file":"CrosspaneAudio-remove-0.1.0.pkg","sha256":digest(b"inert-remove")}]})
    }
    fn manifest(&self, value: Value) {
        bytes(
            &self.source().join("packages.json"),
            &canonical(&value),
            0o600,
        );
    }
    fn adapter(&self) -> MacAudioPackage {
        MacAudioPackage::admit(
            self.io.clone(),
            self.source(),
            self.clock.clone(),
            &self.deadline(),
        )
        .unwrap()
    }
    fn set(&self, stage: &str, path: &str, value: Override) {
        self.overrides
            .lock()
            .unwrap()
            .insert((stage.into(), path.into()), value);
    }
    fn outcome_path(&self, kind: AudioPackageKind) -> PathBuf {
        self.physical(&format!(
            "{INST}/audio-{}outcome.json",
            if kind == AudioPackageKind::Remove {
                "removal-"
            } else {
                ""
            }
        ))
    }
    fn outcome_value(&self, kind: AudioPackageKind, result: &str, at: u64) -> Value {
        if kind == AudioPackageKind::Remove {
            json!({"schema_version":1,"result":result,"at_unix_ms":at})
        } else {
            json!({"schema_version":1,"result":result,"driver_version":"0.1.0","at_unix_ms":at})
        }
    }
    fn raw_outcome(&self, kind: AudioPackageKind, raw: &[u8]) {
        let path = self.outcome_path(kind);
        let temp = path.with_extension("new");
        bytes(&temp, raw, 0o644);
        fs::rename(temp, path).unwrap();
    }
    fn outcome(&self, kind: AudioPackageKind, result: &str, at: u64) {
        self.raw_outcome(kind, &canonical(&self.outcome_value(kind, result, at)));
    }
    fn open(
        &self,
        a: &mut MacAudioPackage,
        kind: AudioPackageKind,
        op: u64,
    ) -> AudioPackageAttempt {
        let plan = a.plan(&self.proof, kind, op, op, &self.deadline()).unwrap();
        let consent = plan.consent(op, op, true, true).unwrap();
        a.open(plan, consent, &self.proof, &self.deadline())
            .unwrap()
    }
    fn calls(&self) -> usize {
        self.runner.calls.lock().unwrap().len()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).unwrap();
    }
}

// WP-4.14b1: native admissions; every root path above maps to Fixture::library.
#[test]
fn b1_outcome_fixed_pair_identity_and_absent_only_noent() {
    for kind in [AudioPackageKind::Install, AudioPackageKind::Remove] {
        let f = Fixture::new();
        let remove = kind == AudioPackageKind::Remove;
        assert!(
            f.io.read_audio_outcome(remove, &f.deadline())
                .unwrap()
                .is_none()
        );
        f.outcome(kind, if remove { "absent" } else { "installed" }, TIME);
        let (raw, first) =
            f.io.read_audio_outcome(remove, &f.deadline())
                .unwrap()
                .unwrap();
        assert_eq!(raw.last(), Some(&b'\n'));
        assert_eq!(first.length, raw.len() as u64);
        assert_eq!(first.uid, 0);
        assert_eq!(first.mode & 0o7777, 0o644);
        f.outcome(kind, if remove { "removed" } else { "installed" }, TIME);
        let (_, second) =
            f.io.read_audio_outcome(remove, &f.deadline())
                .unwrap()
                .unwrap();
        assert_ne!(first.inode, second.inode);
        assert_eq!(first.device, second.device);
    }
    for absent in ["/Library/Application Support/Crosspane", INST] {
        let f = Fixture::new();
        fs::remove_dir_all(f.physical(absent)).unwrap();
        assert!(
            f.io.read_audio_outcome(false, &f.deadline())
                .unwrap()
                .is_none()
        );
    }
    let f = Fixture::new();
    fs::remove_dir_all(f.physical("/Library/Application Support")).unwrap();
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(
        f.io.read(
            &Path::new(INST).join("audio-outcome.json"),
            256,
            false,
            &f.deadline()
        )
        .unwrap_err(),
        NativeError::Foreign
    );
}
#[test]
fn b1_outcome_leaf_owner_mode_size_symlink_and_special_refuse() {
    for mode in [0o600, 0o640, 0o664] {
        let f = Fixture::new();
        f.outcome(AudioPackageKind::Install, "installed", TIME);
        fs::set_permissions(
            f.outcome_path(AudioPackageKind::Install),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        assert_eq!(
            f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
            NativeError::Foreign
        );
    }
    // Unprivileged macOS chmod may clear setgid; inject the stat fact rather than rely on it.
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    f.set(
        "audio-outcome",
        &format!("{INST}/audio-outcome.json"),
        Override::Mode(0o102644),
    );
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    f.set(
        "audio-outcome",
        &format!("{INST}/audio-outcome.json"),
        Override::Uid(f.io.target().paths().uid),
    );
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    let f = Fixture::new();
    f.raw_outcome(AudioPackageKind::Install, &[b'x'; 257]);
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Oversize
    );
    let f = Fixture::new();
    let leaf = f.outcome_path(AudioPackageKind::Install);
    let target = f.base.join("inert-outside");
    bytes(&target, b"untouched", 0o644);
    symlink(&target, &leaf).unwrap();
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(fs::read(target).unwrap(), b"untouched");
    fs::remove_file(&leaf).unwrap();
    directory(&leaf, 0o644);
    assert_eq!(
        f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
}
#[test]
fn b1_outcome_protected_ancestry_and_changed_held_file_refuse() {
    for path in [
        "/Library",
        "/Library/Application Support",
        "/Library/Application Support/Crosspane",
        INST,
    ] {
        for change in [Override::Uid(12345), Override::Mode(0o040777)] {
            let f = Fixture::new();
            f.set("audio-ancestor", path, change);
            assert_eq!(
                f.io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
                NativeError::Foreign
            );
        }
    }
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    let mut target = f.io.target().clone();
    let original = target.test_hook.clone().unwrap();
    let count = Arc::new(AtomicU64::new(0));
    let increment = count.clone();
    target.test_hook = Some(Arc::new(move |stage, path, value| {
        let mut value = original(stage, path, value)?;
        if stage == "audio-outcome" && increment.fetch_add(1, Ordering::Relaxed) > 0 {
            value.as_mut().unwrap().inode += 1;
        }
        Ok(value)
    }));
    let io = MacNativeIo::new(
        target,
        f.runner.clone(),
        f.support.clone(),
        Arc::new(Signatures),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    assert!(count.load(Ordering::Relaxed) >= 2);
}
#[test]
fn b1_metadata_exact_six_paths_lstat_only_absent_unknown_and_link_kind() {
    let f = Fixture::new();
    for path in METADATA {
        let value = f.io.audio_metadata(Path::new(path), &f.deadline()).unwrap();
        if matches!(
            path,
            DRIVER | "/Library/Application Support/Crosspane/Installer/previous"
        ) {
            assert!(value.is_none());
        } else {
            let (kind, uid, mode, _) = value.unwrap();
            assert_eq!((kind, uid), (0o040000, 0));
            assert_eq!(mode & 0o022, 0);
        }
    }
    for path in [
        "/Library",
        "/Library/Audio",
        "/Library/Application Support/Crosspane/Installer/staging",
        "/Library/Audio/Plug-Ins/HAL/Other.driver",
    ] {
        assert_eq!(
            f.io.audio_metadata(Path::new(path), &f.deadline())
                .unwrap_err(),
            NativeError::Invalid
        );
    }
    f.set(
        "audio-metadata",
        DRIVER,
        Override::Error(NativeError::Unavailable),
    );
    assert_eq!(
        f.io.audio_metadata(Path::new(DRIVER), &f.deadline())
            .unwrap_err(),
        NativeError::Unavailable
    );
    f.overrides.lock().unwrap().clear();
    symlink(f.base.join("does-not-exist"), f.physical(DRIVER)).unwrap();
    assert_eq!(
        f.io.audio_metadata(Path::new(DRIVER), &f.deadline())
            .unwrap()
            .unwrap()
            .0,
        0o120000
    );
    assert_eq!(f.calls(), 0);
}
#[test]
fn b1_open_admission_exact_version_path_argv_and_cleared_environment() {
    let f = Fixture::new();
    let dir = f.io.target().installer_dir().join("packages");
    for name in [
        "CrosspaneAudio-install-0.1.0.pkg",
        "CrosspaneAudio-remove-9999.0000.2.pkg",
    ] {
        let path = dir.join(name);
        let command = CommandSpec::new(
            f.io.target(),
            NativeOperation::OpenAudioPackage { path: path.clone() },
        )
        .unwrap();
        assert_eq!(command.program(), Path::new("/usr/bin/open"));
        assert_eq!(
            command.args(),
            ["-b", "com.apple.installer", "--", path.to_str().unwrap()]
        );
        assert!(command.is_mutation());
        assert_eq!(command.max_output(), 4096);
        assert_eq!(
            command
                .environment()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            [
                "HOME",
                "TMPDIR",
                "CROSSPANE_RUNTIME_DIR",
                "PATH",
                "LC_ALL",
                "TZ"
            ]
            .map(str::to_owned)
            .into()
        );
    }
    for name in [
        "CrosspaneAudio-install-0.1.pkg",
        "CrosspaneAudio-install-00000.1.0.pkg",
        "CrosspaneAudio-install-0.a.0.pkg",
        "CrosspaneAudio-install-0.1.0.pkg.extra",
        "CrosspaneAudio-restore-0.1.0.pkg",
        "-CrosspaneAudio-install-0.1.0.pkg",
    ] {
        assert!(
            CommandSpec::new(
                f.io.target(),
                NativeOperation::OpenAudioPackage {
                    path: dir.join(name)
                }
            )
            .is_err()
        );
    }
    for path in [
        dir.join("../CrosspaneAudio-install-0.1.0.pkg"),
        f.base.join("CrosspaneAudio-install-0.1.0.pkg"),
        PathBuf::from(
            "/Library/Application Support/Crosspane/Installer/packages/CrosspaneAudio-install-0.1.0.pkg",
        ),
    ] {
        assert!(
            CommandSpec::new(f.io.target(), NativeOperation::OpenAudioPackage { path }).is_err()
        );
    }
    assert_eq!(f.calls(), 0); // Admission only: no native open invocation.
}

// WP-4.14b2: data-only adapter, production manifest, consent and current-attempt facts.
#[test]
fn b2_manifest_exact_schema_kinds_filename_version_hash_and_production_name() {
    let f = Fixture::new();
    for change in 0..9 {
        let mut m = f.manifest_value();
        match change {
            0 => m["schema_version"] = json!(2),
            1 => m["version"] = json!("0.1"),
            2 => m["version"] = json!("10000.1.0"),
            3 => m["packages"][0]["file"] = json!("../CrosspaneAudio-install-0.1.0.pkg"),
            4 => m["packages"][0]["sha256"] = json!("A".repeat(64)),
            5 => m["packages"][1]["kind"] = json!("install"),
            6 => m["packages"][0]["extra"] = json!(false),
            7 => m["packages"] = json!([]),
            _ => m["extra"] = json!(true),
        }
        f.manifest(m);
        assert!(
            MacAudioPackage::admit(f.io.clone(), f.source(), f.clock.clone(), &f.deadline())
                .is_err()
        );
    }
    f.manifest(f.manifest_value());
    let raw = fs::read(f.source().join("packages.json")).unwrap();
    for bytes_value in [
        raw[..raw.len() - 1].to_vec(),
        [raw.clone(), b"{}".to_vec()].concat(),
        vec![b' '; 1025],
        raw.iter()
            .copied()
            .take(1)
            .chain(b"\"schema_version\":1,".iter().copied())
            .chain(raw[1..].iter().copied())
            .collect(),
    ] {
        bytes(&f.source().join("packages.json"), &bytes_value, 0o600);
        assert!(
            MacAudioPackage::admit(f.io.clone(), f.source(), f.clock.clone(), &f.deadline())
                .is_err()
        );
    }
    f.manifest(f.manifest_value());
    fs::rename(
        f.source().join("packages.json"),
        f.source().join("packages.test.json"),
    )
    .unwrap();
    assert!(
        MacAudioPackage::admit(f.io.clone(), f.source(), f.clock.clone(), &f.deadline()).is_err()
    );
    assert_eq!(f.calls(), 0);
}
#[test]
fn b2_preview_absent_current_retained_previous_conflict_owner_and_volume() {
    for case in 0..7 {
        let f = Fixture::new();
        match case {
            1 => directory(&f.physical(DRIVER), 0o755),
            2 => {
                directory(&f.physical(DRIVER), 0o755);
                directory(&f.physical(&format!("{INST}/previous")), 0o700);
            }
            3 => bytes(&f.physical(DRIVER), b"foreign", 0o644),
            4 => f.set(
                "audio-metadata",
                DRIVER,
                Override::Error(NativeError::Unavailable),
            ),
            5 => f.set(
                "audio-metadata",
                "/Library/Audio/Plug-Ins/HAL",
                Override::Device(u64::MAX),
            ),
            6 => f.set("audio-metadata", INST, Override::Uid(12345)),
            _ => (),
        }
        let mut a = f.adapter();
        let plan = a
            .plan(&f.proof, AudioPackageKind::Install, 1, 1, &f.deadline())
            .unwrap();
        assert_eq!(
            plan.preview().driver,
            if case == 0 || case >= 5 {
                Presence::Absent
            } else if case == 4 {
                Presence::Unknown
            } else {
                Presence::Present
            }
        );
        assert_eq!(
            plan.preview().previous,
            if case == 2 {
                Presence::Present
            } else {
                Presence::Absent
            }
        );
        assert_eq!(plan.consent(1, 1, true, true).is_ok(), case <= 2);
        assert!(plan.preview().interrupts_system_audio);
        assert_eq!(f.calls(), 0);
        assert!(f.events.lock().unwrap().iter().all(
            |(stage, path)| stage != "audio-outcome" || path.parent() == Some(Path::new(INST))
        ));
    }
    assert_eq!(
        REMOVE_AUDIO_LABEL,
        "Remove the Crosspane audio driver (affects every user on this Mac)"
    );
}
#[test]
fn b2_consent_binds_current_revision_operation_preview_and_single_attempt() {
    let f = Fixture::new();
    let mut a = f.adapter();
    let plan = a
        .plan(&f.proof, AudioPackageKind::Install, 1, 2, &f.deadline())
        .unwrap();
    for (revision, op, shared) in [(2, 2, true), (1, 3, true), (1, 2, false)] {
        assert!(plan.consent(revision, op, shared, true).is_err());
    }
    let consent = plan.consent(1, 2, true, true).unwrap();
    let newer = a
        .plan(&f.proof, AudioPackageKind::Install, 1, 2, &f.deadline())
        .unwrap();
    assert!(a.open(plan, consent, &f.proof, &f.deadline()).is_err());
    let consent = newer.consent(1, 2, true, true).unwrap();
    let mut other = f.adapter();
    assert!(other.open(newer, consent, &f.proof, &f.deadline()).is_err());
    assert_eq!(f.calls(), 0);
    directory(&f.physical(&format!("{INST}/previous")), 0o700);
    let plan = a
        .plan(&f.proof, AudioPackageKind::Remove, 2, 3, &f.deadline())
        .unwrap();
    assert!(plan.consent(2, 3, true, false).is_err());
    assert!(plan.consent(2, 3, true, true).is_ok());
}
#[test]
fn b2_detection_source_target_and_session_drift_refuse_before_mutation() {
    for drift in 0..4 {
        let f = Fixture::new();
        let mut a = f.adapter();
        let plan = a
            .plan(&f.proof, AudioPackageKind::Install, 1, 1, &f.deadline())
            .unwrap();
        let consent = plan.consent(1, 1, true, true).unwrap();
        match drift {
            0 => directory(&f.physical(DRIVER), 0o755),
            1 => bytes(
                &f.source().join("CrosspaneAudio-install-0.1.0.pkg"),
                b"changed",
                0o600,
            ),
            2 => {
                let dir = f.io.target().installer_dir().join("packages");
                directory(&dir, 0o700);
                bytes(
                    &dir.join("CrosspaneAudio-install-0.1.0.pkg"),
                    b"inert-install",
                    0o600,
                );
            }
            _ => {
                let mut s = f.support.0.lock().unwrap();
                s.gui.console_session = "another-Aqua".into();
                s.gui.interactive_session = s.gui.console_session.clone();
            }
        }
        f.events.lock().unwrap().clear();
        assert!(a.open(plan, consent, &f.proof, &f.deadline()).is_err());
        assert_eq!(f.calls(), 0);
        assert!(
            !f.events
                .lock()
                .unwrap()
                .iter()
                .any(|(s, _)| matches!(s.as_str(), "mkdir" | "create-temp" | "publish"))
        );
    }
}
#[test]
fn b2_hash_symlink_and_staged_foreign_refuse_and_copy_precedes_open() {
    for corrupt in 0..3 {
        let f = Fixture::new();
        let mut a = f.adapter();
        let source = f.source().join("CrosspaneAudio-install-0.1.0.pkg");
        match corrupt {
            0 => bytes(&source, b"wrong hash", 0o600),
            1 => {
                fs::remove_file(&source).unwrap();
                symlink(f.source().join("CrosspaneAudio-remove-0.1.0.pkg"), &source).unwrap();
            }
            _ => bytes(
                &f.io
                    .target()
                    .installer_dir()
                    .join("packages/CrosspaneAudio-install-0.1.0.pkg"),
                b"foreign",
                0o600,
            ),
        }
        assert!(
            a.plan(&f.proof, AudioPackageKind::Install, 1, 1, &f.deadline())
                .is_err()
        );
        assert_eq!(f.calls(), 0);
    }
    let f = Fixture::new();
    let mut a = f.adapter();
    let attempt = f.open(&mut a, AudioPackageKind::Install, 1);
    assert_eq!(attempt.facts().state, PackageState::OpenRequested);
    assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
    assert_eq!(f.calls(), 1);
    let dir = f.io.target().installer_dir().join("packages");
    assert_eq!(
        fs::metadata(dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
#[test]
fn b2_install_and_removal_outcomes_exact_literals_remain_advisory() {
    for (kind, result, expected) in [
        (
            AudioPackageKind::Install,
            "installed",
            AudioOutcome::Installed,
        ),
        (
            AudioPackageKind::Install,
            "verify_failed",
            AudioOutcome::VerifyFailed,
        ),
        (
            AudioPackageKind::Install,
            "move_failed",
            AudioOutcome::MoveFailed,
        ),
        (AudioPackageKind::Remove, "removed", AudioOutcome::Removed),
        (AudioPackageKind::Remove, "absent", AudioOutcome::Absent),
        (
            AudioPackageKind::Remove,
            "remove_failed",
            AudioOutcome::RemoveFailed,
        ),
    ] {
        let f = Fixture::new();
        let mut a = f.adapter();
        let mut attempt = f.open(&mut a, kind, 1);
        f.outcome(kind, result, TIME);
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert_eq!(attempt.facts().state, PackageState::Outcome(expected));
        assert_eq!(attempt.facts().error, None);
        assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
        assert_eq!(f.calls(), 1);
    }
}
#[test]
fn b2_outcome_strict_keys_schema_version_result_and_bounded_json() {
    for kind in [AudioPackageKind::Install, AudioPackageKind::Remove] {
        let f = Fixture::new();
        let mut a = f.adapter();
        let mut attempt = f.open(&mut a, kind, 1);
        for change in 0..8 {
            let mut value = f.outcome_value(
                kind,
                if kind == AudioPackageKind::Install {
                    "installed"
                } else {
                    "removed"
                },
                TIME,
            );
            match change {
                0 => value["schema_version"] = json!(2),
                1 => value["extra"] = json!(1),
                2 => {
                    value.as_object_mut().unwrap().remove("at_unix_ms");
                }
                3 => value["at_unix_ms"] = json!(-1),
                4 => value["driver_version"] = json!("9.9.9"),
                5 => value["result"] = json!("unknown"),
                6 => {
                    value["result"] = json!(if kind == AudioPackageKind::Install {
                        "removed"
                    } else {
                        "installed"
                    })
                }
                _ => value["at_unix_ms"] = json!(1.5),
            }
            f.raw_outcome(kind, &canonical(&value));
            a.observe(&mut attempt, &f.deadline()).unwrap();
            assert_eq!(attempt.facts().state, PackageState::Unknown);
        }
        let valid = serde_json::to_vec(&f.outcome_value(
            kind,
            if kind == AudioPackageKind::Install {
                "installed"
            } else {
                "removed"
            },
            TIME,
        ))
        .unwrap();
        let duplicate = [b"{\"schema_version\":1,".as_slice(), &valid[1..], b"\n"].concat();
        for invalid in [
            valid.clone(),
            [valid, b"{}\n".to_vec()].concat(),
            duplicate,
            vec![b'x'; 257],
        ] {
            f.raw_outcome(kind, &invalid);
            a.observe(&mut attempt, &f.deadline()).unwrap();
            assert_eq!(attempt.facts().state, PackageState::Unknown);
        }
    }
}
#[test]
fn b2_current_attempt_requires_changed_identity_and_inclusive_time_window() {
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    let mut a = f.adapter();
    let mut attempt = f.open(&mut a, AudioPackageKind::Install, 1);
    a.observe(&mut attempt, &f.deadline()).unwrap();
    assert_eq!(attempt.facts().state, PackageState::Unknown);
    for (at, accepted) in [
        (TIME - 2001, false),
        (TIME - 2000, true),
        (TIME + 2000, true),
        (TIME + 2001, false),
    ] {
        f.outcome(AudioPackageKind::Install, "installed", at);
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert_eq!(
            matches!(attempt.facts().state, PackageState::Outcome(_)),
            accepted
        );
    }
    f.clock.0.store(TIME - 1, Ordering::Release);
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    a.observe(&mut attempt, &f.deadline()).unwrap();
    assert_eq!(attempt.facts().state, PackageState::Unknown);
}
#[test]
fn b2_refused_admin_restart_unknown_and_dismissal_never_prove_audio() {
    for result in [
        Ok(CommandOutput {
            code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }),
        Err(NativeError::Refused),
        Err(NativeError::OutcomeUnknown),
    ] {
        let f = Fixture::new();
        *f.runner.result.lock().unwrap() = result;
        let mut a = f.adapter();
        let mut attempt = f.open(&mut a, AudioPackageKind::Install, 1);
        assert_eq!(attempt.facts().state, PackageState::Unknown);
        assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
        // Installer/admin refusal and restart refusal have no independent native proof here.
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert_eq!(attempt.facts().state, PackageState::Unknown);
        f.outcome(AudioPackageKind::Install, "installed", TIME);
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert_eq!(
            attempt.facts().state,
            PackageState::Outcome(AudioOutcome::Installed)
        );
        assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
        attempt.dismiss();
        assert_eq!(attempt.facts().state, PackageState::Unknown);
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert_eq!(
            attempt.facts().state,
            PackageState::Outcome(AudioOutcome::Installed)
        );
        assert_eq!(f.calls(), 1);
    }
}
#[test]
fn b2_unknown_explicit_retry_redetects_and_requires_new_consent_no_blind_open() {
    let f = Fixture::new();
    let mut a = f.adapter();
    let mut attempt = f.open(&mut a, AudioPackageKind::Install, 1);
    for _ in 0..3 {
        a.observe(&mut attempt, &f.deadline()).unwrap();
    }
    attempt.dismiss();
    assert_eq!(f.calls(), 1);
    let earlier = f
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|(s, _)| s == "audio-metadata")
        .count();
    let plan = a
        .plan(&f.proof, AudioPackageKind::Install, 2, 2, &f.deadline())
        .unwrap();
    assert_eq!(
        f.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == "audio-metadata")
            .count(),
        earlier + 6
    );
    let consent = plan.consent(2, 2, true, true).unwrap();
    a.open(plan, consent, &f.proof, &f.deadline()).unwrap();
    assert_eq!(f.calls(), 2);
}
#[test]
fn b2_removal_uses_separate_package_and_never_deletes_driver_or_previous() {
    let f = Fixture::new();
    directory(&f.physical(DRIVER), 0o755);
    let previous = f.physical(&format!("{INST}/previous"));
    directory(&previous, 0o700);
    let sentinel = previous.join("CrosspaneAudio.driver/manual-A");
    bytes(&sentinel, b"working-A", 0o600);
    let mut a = f.adapter();
    let mut attempt = f.open(&mut a, AudioPackageKind::Remove, 1);
    assert!(
        f.runner.calls.lock().unwrap()[0].args()[3].ends_with("CrosspaneAudio-remove-0.1.0.pkg")
    );
    f.outcome(AudioPackageKind::Remove, "removed", TIME);
    a.observe(&mut attempt, &f.deadline()).unwrap();
    assert_eq!(fs::read(sentinel).unwrap(), b"working-A");
    assert!(f.physical(DRIVER).is_dir());
    assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
}

#[test]
fn b1_fixed_metadata_spellings_refuse_normalized_aliases_before_io() {
    let f = Fixture::new();
    f.events.lock().unwrap().clear();
    for path in METADATA {
        for alias in [
            format!("{path}/"),
            format!("{path}/."),
            path.replace("/Library/", "/Library//"),
        ] {
            assert_eq!(
                f.io.audio_metadata(Path::new(&alias), &f.deadline())
                    .unwrap_err(),
                NativeError::Invalid
            );
        }
    }
    assert!(f.events.lock().unwrap().is_empty());
    let target = f.base.join("driver-link-target");
    directory(&target, 0o700);
    bytes(&target.join("sentinel"), b"not followed", 0o600);
    symlink(&target, f.physical(DRIVER)).unwrap();
    assert_eq!(
        f.io.audio_metadata(Path::new(&format!("{DRIVER}/.")), &f.deadline())
            .unwrap_err(),
        NativeError::Invalid
    );
    assert!(f.events.lock().unwrap().is_empty());
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"not followed");
    for removal in [false, true] {
        let kind = if removal {
            AudioPackageKind::Remove
        } else {
            AudioPackageKind::Install
        };
        f.outcome(kind, if removal { "removed" } else { "installed" }, TIME);
        assert!(
            f.io.read_audio_outcome(removal, &f.deadline())
                .unwrap()
                .is_some()
        );
    }
    assert!(f.events.lock().unwrap().iter().all(|(stage, path)| {
        stage != "audio-outcome"
            || path.as_os_str() == Path::new(INST).join("audio-outcome.json").as_os_str()
            || path.as_os_str()
                == Path::new(INST)
                    .join("audio-removal-outcome.json")
                    .as_os_str()
    }));
}

#[test]
fn b1_outcome_leaf_entry_replacement_after_stable_fstat_refused() {
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    f.events.lock().unwrap().clear();
    let leaf = f.outcome_path(AudioPackageKind::Install);
    let mut target = f.io.target().clone();
    let original = target.test_hook.clone().unwrap();
    let observed = Arc::new(Mutex::new(Vec::<FileIdentity>::new()));
    let captured = observed.clone();
    target.test_hook = Some(Arc::new(move |stage, path, value| {
        let value = original(stage, path, value)?;
        if stage == "audio-outcome" {
            let mut ids = captured.lock().unwrap();
            ids.push(value.clone().unwrap());
            if ids.len() == 2 {
                assert_eq!(ids[0], ids[1]); // Held-file recheck passes; mutate only afterward.
                let replacement = leaf.with_extension("replacement");
                bytes(&replacement, b"replacement entry", 0o644);
                fs::rename(replacement, &leaf).unwrap();
            }
        }
        Ok(value)
    }));
    let io = MacNativeIo::new(
        target,
        f.runner.clone(),
        f.support.clone(),
        Arc::new(Signatures),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    let ids = observed.lock().unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], ids[1]);
    assert_ne!(ids[0].inode, ids[2].inode);
    // The leaf-entry comparison refuses before the ancestry guard is reached.
    assert_eq!(
        f.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(stage, _)| stage == "audio-ancestor")
            .count(),
        4
    );
}

#[test]
fn b1_outcome_ancestor_replacement_after_stable_entry_refused() {
    let f = Fixture::new();
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    f.events.lock().unwrap().clear();
    let installer = f.physical(INST);
    let retained = installer.with_file_name("Installer-held");
    let mut target = f.io.target().clone();
    let original = target.test_hook.clone().unwrap();
    let observed = Arc::new(Mutex::new(Vec::<FileIdentity>::new()));
    let captured = observed.clone();
    let ancestors = Arc::new(Mutex::new(Vec::<FileIdentity>::new()));
    let captured_ancestors = ancestors.clone();
    target.test_hook = Some(Arc::new(move |stage, path, value| {
        let value = original(stage, path, value)?;
        if stage == "audio-outcome" {
            let mut ids = captured.lock().unwrap();
            ids.push(value.clone().unwrap());
            if ids.len() == 3 {
                assert_eq!(ids[0], ids[1]);
                assert_eq!(ids[0], ids[2]);
                // Both earlier guards passed. Replace the ancestor after the entry stat was taken.
                fs::rename(&installer, &retained).unwrap();
                directory(&installer, 0o755);
            }
        }
        if stage == "audio-ancestor" && path.as_os_str() == Path::new(INST).as_os_str() {
            captured_ancestors
                .lock()
                .unwrap()
                .push(value.clone().unwrap());
        }
        Ok(value)
    }));
    let io = MacNativeIo::new(
        target,
        f.runner.clone(),
        f.support.clone(),
        Arc::new(Signatures),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        io.read_audio_outcome(false, &f.deadline()).unwrap_err(),
        NativeError::Foreign
    );
    let ids = observed.lock().unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], ids[1]);
    assert_eq!(ids[0], ids[2]);
    let dirs = ancestors.lock().unwrap();
    assert_eq!(dirs.len(), 2);
    assert_ne!(dirs[0].inode, dirs[1].inode);
    assert_eq!(
        f.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(stage, _)| stage == "audio-ancestor")
            .count(),
        8
    );
}

#[test]
fn b2_manifest_requires_canonical_bytes_for_arrays_enums_order_whitespace_and_newline() {
    let f = Fixture::new();
    let value = f.manifest_value();
    let good = canonical(&value);
    let text = String::from_utf8(good.clone()).unwrap();
    let mut package_array = value.clone();
    let entry = &value["packages"][0];
    package_array["packages"][0] = json!([entry["kind"], entry["file"], entry["sha256"]]);
    let mut object_enum = value.clone();
    object_enum["packages"][0]["kind"] = json!({"install":null});
    let reordered_entry = text.replace(
        "\"kind\":\"install\",\"file\":\"CrosspaneAudio-install-0.1.0.pkg\"",
        "\"file\":\"CrosspaneAudio-install-0.1.0.pkg\",\"kind\":\"install\"",
    );
    assert_ne!(reordered_entry.as_bytes(), good);
    let mut reordered = serde_json::to_vec(&value).unwrap();
    reordered.push(b'\n');
    assert_ne!(reordered, good);
    let mut pretty = serde_json::to_vec_pretty(&value).unwrap();
    pretty.push(b'\n');
    let cases = [
        (
            "positional manifest",
            canonical(&json!([1, "0.1.0", value["packages"]])),
        ),
        ("positional package", canonical(&package_array)),
        ("object enum", canonical(&object_enum)),
        (
            "duplicate",
            [b"{\"schema_version\":1,".as_slice(), &good[1..]].concat(),
        ),
        ("root order", reordered),
        ("package order", reordered_entry.into_bytes()),
        ("whitespace", [b"{ ".as_slice(), &good[1..]].concat()),
        ("pretty", pretty),
        ("extra newline", [good.clone(), b"\n".to_vec()].concat()),
        (
            "CRLF",
            [good[..good.len() - 1].to_vec(), b"\r\n".to_vec()].concat(),
        ),
        ("missing newline", good[..good.len() - 1].to_vec()),
        (
            "escaped literal",
            text.replace("0.1.0", "\\u0030.1.0").into_bytes(),
        ),
    ];
    for (name, raw) in cases {
        bytes(&f.source().join("packages.json"), &raw, 0o600);
        assert!(
            MacAudioPackage::admit(f.io.clone(), f.source(), f.clock.clone(), &f.deadline())
                .is_err(),
            "{name}"
        );
    }
    bytes(&f.source().join("packages.json"), &good, 0o600);
    assert_eq!(f.adapter().version(), "0.1.0");
    assert_eq!(f.calls(), 0);
}

#[test]
fn b2_outcomes_require_canonical_bytes_for_arrays_enums_order_whitespace_and_newline() {
    for kind in [AudioPackageKind::Install, AudioPackageKind::Remove] {
        let f = Fixture::new();
        let mut a = f.adapter();
        let mut attempt = f.open(&mut a, kind, 1);
        let result = if kind == AudioPackageKind::Install {
            "installed"
        } else {
            "removed"
        };
        let value = f.outcome_value(kind, result, TIME);
        let good = canonical(&value);
        let text = String::from_utf8(good.clone()).unwrap();
        let array = if kind == AudioPackageKind::Install {
            json!([1, result, "0.1.0", TIME])
        } else {
            json!([1, result, TIME])
        };
        let mut object_enum = value.clone();
        let mut enum_map = serde_json::Map::new();
        enum_map.insert(result.into(), Value::Null);
        object_enum["result"] = Value::Object(enum_map);
        let mut reordered = serde_json::to_vec(&value).unwrap();
        reordered.push(b'\n');
        assert_ne!(reordered, good);
        let mut pretty = serde_json::to_vec_pretty(&value).unwrap();
        pretty.push(b'\n');
        let escaped = if kind == AudioPackageKind::Install {
            "\\u0069nstalled"
        } else {
            "\\u0072emoved"
        };
        for (name, raw) in [
            ("positional outcome", canonical(&array)),
            ("object enum", canonical(&object_enum)),
            (
                "duplicate",
                [b"{\"schema_version\":1,".as_slice(), &good[1..]].concat(),
            ),
            ("key order", reordered),
            ("whitespace", [b"{ ".as_slice(), &good[1..]].concat()),
            ("pretty", pretty),
            ("extra newline", [good.clone(), b"\n".to_vec()].concat()),
            (
                "CRLF",
                [good[..good.len() - 1].to_vec(), b"\r\n".to_vec()].concat(),
            ),
            ("missing newline", good[..good.len() - 1].to_vec()),
            (
                "escaped literal",
                text.replace(result, escaped).into_bytes(),
            ),
        ] {
            f.raw_outcome(kind, &raw);
            a.observe(&mut attempt, &f.deadline()).unwrap();
            assert_eq!(
                attempt.facts().state,
                PackageState::Unknown,
                "{kind:?}: {name}"
            );
            assert_eq!(
                attempt.facts().error,
                Some(NativeError::Invalid),
                "{kind:?}: {name}"
            );
        }
        f.raw_outcome(kind, &good);
        a.observe(&mut attempt, &f.deadline()).unwrap();
        assert!(matches!(attempt.facts().state, PackageState::Outcome(_)));
        assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
    }
}

#[test]
fn b2_post_hash_same_uid_substitution_is_unobservable_and_never_ready() {
    let f = Fixture::new();
    let staged =
        f.io.target()
            .installer_dir()
            .join("packages/CrosspaneAudio-install-0.1.0.pkg");
    let mut target = f.io.target().clone();
    let original = target.test_hook.clone().unwrap();
    let substituted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired = substituted.clone();
    let late_reads = Arc::new(AtomicU64::new(0));
    let observed_reads = late_reads.clone();
    let substitution = b"inert-same-uid-substitution".to_vec();
    *f.runner.expected_substitution.lock().unwrap() = Some(substitution.clone());
    target.test_hook = Some(Arc::new(move |stage, path, value| {
        let value = original(stage, path, value)?;
        if stage == "open-before"
            && path.as_os_str() == staged.as_os_str()
            && fired.load(Ordering::Acquire)
        {
            observed_reads.fetch_add(1, Ordering::Relaxed);
        }
        if stage == "dispatch" && path.as_os_str() == Path::new("/usr/bin/open").as_os_str() {
            assert_eq!(fs::read(&staged).unwrap(), b"inert-install"); // Copy and hash already passed.
            bytes(&staged, &substitution, 0o600);
            fired.store(true, Ordering::Release);
        }
        Ok(value)
    }));
    let io = Arc::new(
        MacNativeIo::new(
            target,
            f.runner.clone(),
            f.support.clone(),
            Arc::new(Signatures),
            f.clock.clone(),
        )
        .unwrap(),
    );
    let mut a = MacAudioPackage::admit(io, f.source(), f.clock.clone(), &f.deadline()).unwrap();
    let mut attempt = f.open(&mut a, AudioPackageKind::Install, 1);
    assert!(substituted.load(Ordering::Acquire));
    assert_eq!(late_reads.load(Ordering::Relaxed), 0);
    assert_eq!(
        f.runner.observed_bytes.lock().unwrap().as_slice(),
        [b"inert-same-uid-substitution".to_vec()]
    );
    assert_eq!(attempt.facts().state, PackageState::OpenRequested);
    assert_eq!(attempt.facts().error, None);
    assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
    f.outcome(AudioPackageKind::Install, "installed", TIME);
    a.observe(&mut attempt, &f.deadline()).unwrap();
    assert_eq!(
        attempt.facts().state,
        PackageState::Outcome(AudioOutcome::Installed)
    );
    assert_eq!(attempt.facts().agent_evidence, AgentEvidence::Required);
    assert_eq!(f.calls(), 1);
}
