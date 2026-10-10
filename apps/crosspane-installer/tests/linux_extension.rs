#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Turning the Crosspane GNOME Shell extension on and off in the person's settings, and the
//! install adapter that does it with the desktop files. Everything runs in a scratch home under
//! /tmp with a fake `gsettings`: nothing here reads or writes the owner's settings.
use crosspane_installer::platform::linux::{
    detect::Desktop,
    extension::{
        self, Change, DISABLED_KEY, ENABLED_KEY, ExtensionError, GnomeSettings, Reading, SCHEMA,
        UUID,
    },
    integration::{NativePayloads, Payloads},
    native_io::*,
    payload::*,
};
use crosspane_installer_core::{OperationId, ResourceObservation};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
const DESKTOP_ENTRY: &str =
    include_str!("../../../packaging/linux/io.frostdev.crosspane.agent.desktop");
const EXTENSION: &str =
    include_str!("../../../packaging/gnome-shell-extension/crosspane@frostdev.io/extension.js");
const METADATA: &str =
    include_str!("../../../packaging/gnome-shell-extension/crosspane@frostdev.io/metadata.json");
const SHELL_XML: &str = include_str!(
    "../../../packaging/gnome-shell-extension/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml"
);

/// A stand-in for `gsettings` on one key, plus `ps` for the agent checks the core install makes.
#[derive(Default)]
struct Settings {
    enabled: Mutex<Vec<String>>,
    /// What `get enabled-extensions` prints instead of the list, when set.
    raw: Mutex<Option<String>>,
    user_extensions_disabled: Mutex<bool>,
    /// `set` exits 1 / exits 0 and changes nothing.
    set_fails: Mutex<bool>,
    set_ignored: Mutex<bool>,
    get_fails: Mutex<bool>,
    calls: Mutex<Vec<Vec<String>>>,
}
impl Settings {
    fn calls(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|argv| argv[0] == verb)
            .cloned()
            .collect()
    }
    fn list(&self) -> Vec<String> {
        self.enabled.lock().unwrap().clone()
    }
    fn set_list(&self, list: &[&str]) {
        *self.enabled.lock().unwrap() = list.iter().map(|s| (*s).to_owned()).collect();
    }
}
impl CommandRunner for Settings {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        let ok = |stdout: String| CommandOutput {
            code: Some(0),
            stdout: stdout.into_bytes(),
            stderr: vec![],
        };
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
        assert_eq!(c.executable(), Path::new("/usr/bin/gsettings"));
        let argv = c.argv().to_vec();
        self.calls.lock().unwrap().push(argv.clone());
        assert_eq!(argv[1], SCHEMA);
        match (argv[0].as_str(), argv[2].as_str()) {
            ("get", ENABLED_KEY) => {
                if *self.get_fails.lock().unwrap() {
                    return Ok(CommandOutput {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"no dconf".to_vec(),
                    });
                }
                if let Some(raw) = self.raw.lock().unwrap().clone() {
                    return Ok(ok(raw));
                }
                let list = self.list();
                Ok(ok(if list.is_empty() {
                    "@as []\n".to_owned()
                } else {
                    format!(
                        "[{}]\n",
                        list.iter()
                            .map(|n| format!("'{n}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }))
            }
            ("get", DISABLED_KEY) => Ok(ok(format!(
                "{}\n",
                *self.user_extensions_disabled.lock().unwrap()
            ))),
            ("set", ENABLED_KEY) => {
                if *self.set_fails.lock().unwrap() {
                    return Ok(CommandOutput {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"denied".to_vec(),
                    });
                }
                if !*self.set_ignored.lock().unwrap() {
                    *self.enabled.lock().unwrap() = extension::parse_enabled(&argv[3]).unwrap();
                }
                Ok(ok(String::new()))
            }
            other => panic!("unapproved settings command {other:?}"),
        }
    }
}

struct Probe(PathBuf);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: self.0.join(".local/bin/crosspane-agent"),
            generation: 77,
        })
    }
}
fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn facts(io: &LinuxNativeIo, desktop: Desktop) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
        desktop,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        compositor_version: [49, 0, 0],
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
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    settings: Arc<Settings>,
    proof: SupportProof,
    bus: BTreeMap<String, String>,
    _listener: UnixListener,
}
impl Fixture {
    fn on(desktop: Desktop, version: Option<[u16; 3]>) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cpext-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let settings = Arc::new(Settings::default());
        let io = Arc::new(
            LinuxNativeIo::scratch(&root, settings.clone(), Arc::new(Probe(root.clone()))).unwrap(),
        );
        let proof = io
            .scratch_support_with_version(facts(&io, desktop), version)
            .unwrap();
        io.create_private_dir(&proof, &io.target().paths().runtime_home)
            .unwrap();
        let path = io.target().paths().runtime_home.join("bus");
        let listener = UnixListener::bind(&path).unwrap();
        let bus = BTreeMap::from([(
            "DBUS_SESSION_BUS_ADDRESS".to_owned(),
            format!("unix:path={}", path.display()),
        )]);
        Self {
            root,
            io,
            settings,
            proof,
            bus,
            _listener: listener,
        }
    }
    fn gnome() -> Self {
        Self::on(Desktop::Gnome, Some([50, 4, 0]))
    }
    fn settings(&self) -> GnomeSettings {
        GnomeSettings::new(self.io.clone(), &self.bus, &deadline()).unwrap()
    }
    fn payloads(&self) -> NativePayloads {
        let env = self
            .io
            .session_bus_environment(self.bus.clone(), &deadline())
            .unwrap();
        NativePayloads::new(self.io.clone())
            .unwrap()
            .with_session(&env)
    }
    fn data(&self, relative: &str) -> PathBuf {
        self.io.target().paths().data_home.join(relative)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn enabling_appends_only_ours_in_one_write_and_reads_it_back() {
    let f = Fixture::gnome();
    f.settings.set_list(&["z@z.org", "a@a.org"]);
    assert_eq!(
        f.settings().enable(&f.proof, &deadline()),
        Ok(Change::Written)
    );
    assert_eq!(f.settings.list(), ["z@z.org", "a@a.org", UUID]);
    let sets = f.settings.calls("set");
    assert_eq!(sets.len(), 1);
    assert_eq!(
        sets[0],
        [
            "set",
            SCHEMA,
            ENABLED_KEY,
            "['z@z.org', 'a@a.org', 'crosspane@frostdev.io']"
        ]
    );
    // The check by reading is part of the write: a get follows the set.
    let verbs: Vec<String> = f
        .settings
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|argv| argv[0].clone())
        .collect();
    assert_eq!(verbs, ["get", "set", "get"]);
    // Already on: nothing is written.
    assert_eq!(
        f.settings().enable(&f.proof, &deadline()),
        Ok(Change::Unchanged)
    );
    assert_eq!(f.settings.calls("set").len(), 1);
}

#[test]
fn an_empty_list_gets_just_ours_and_disabling_removes_only_ours() {
    let f = Fixture::gnome();
    f.settings().enable(&f.proof, &deadline()).unwrap();
    assert_eq!(f.settings.list(), [UUID]);
    assert_eq!(
        f.settings().disable(&f.proof, &deadline()),
        Ok(Change::Written)
    );
    assert!(f.settings.list().is_empty());
    assert_eq!(f.settings.calls("set")[1][3], "[]");
    // Disabling when absent writes nothing, and never touches another entry.
    f.settings.set_list(&["keep@me.org"]);
    let before = f.settings.calls("set").len();
    assert_eq!(
        f.settings().disable(&f.proof, &deadline()),
        Ok(Change::Unchanged)
    );
    assert_eq!(f.settings.calls("set").len(), before);
    f.settings.set_list(&["keep@me.org", UUID, "also@keep.org"]);
    f.settings().disable(&f.proof, &deadline()).unwrap();
    assert_eq!(f.settings.list(), ["keep@me.org", "also@keep.org"]);
}

#[test]
fn a_list_that_cannot_be_rewritten_safely_is_never_written() {
    let f = Fixture::gnome();
    for (raw, error) in [
        ("['has space@x']\n", ExtensionError::Unsafe),
        ("['quote\\'s@x']\n", ExtensionError::Unsafe),
        ("['a@x', 'b@y'\n", ExtensionError::Malformed),
        ("true\n", ExtensionError::Malformed),
        ("[1, 2]\n", ExtensionError::Malformed),
    ] {
        *f.settings.raw.lock().unwrap() = Some(raw.to_owned());
        assert_eq!(
            f.settings().enable(&f.proof, &deadline()),
            Err(error),
            "{raw}"
        );
        assert_eq!(
            f.settings().disable(&f.proof, &deadline()),
            Err(error),
            "{raw}"
        );
    }
    assert!(f.settings.calls("set").is_empty());
}

#[test]
fn a_failed_ignored_or_unreadable_write_is_an_error_never_a_success() {
    let f = Fixture::gnome();
    f.settings.set_list(&["z@z.org"]);
    *f.settings.set_fails.lock().unwrap() = true;
    assert_eq!(
        f.settings().enable(&f.proof, &deadline()),
        Err(ExtensionError::Native(NativeError::Unavailable))
    );
    *f.settings.set_fails.lock().unwrap() = false;
    // dconf said yes and kept nothing (a memory backend): the read-back catches it.
    *f.settings.set_ignored.lock().unwrap() = true;
    assert_eq!(
        f.settings().enable(&f.proof, &deadline()),
        Err(ExtensionError::Changed)
    );
    assert_eq!(f.settings.list(), ["z@z.org"]);
    *f.settings.set_ignored.lock().unwrap() = false;
    *f.settings.get_fails.lock().unwrap() = true;
    let before = f.settings.calls("set").len();
    assert!(f.settings().enable(&f.proof, &deadline()).is_err());
    assert!(f.settings().read(&deadline()).is_err());
    assert_eq!(
        f.settings.calls("set").len(),
        before,
        "no write without a read"
    );
}

#[test]
fn reading_reports_the_switch_and_never_changes_the_users_own_switch() {
    let f = Fixture::gnome();
    f.settings.set_list(&[UUID, "x@y.org"]);
    *f.settings.user_extensions_disabled.lock().unwrap() = true;
    assert_eq!(
        f.settings().read(&deadline()),
        Ok(Reading {
            enabled: true,
            user_extensions_disabled: true
        })
    );
    f.settings().enable(&f.proof, &deadline()).unwrap();
    f.settings().disable(&f.proof, &deadline()).unwrap();
    // Not one command ever wrote or even named the user-extensions switch except to read it.
    assert!(
        f.settings
            .calls("set")
            .iter()
            .all(|argv| argv[2] == ENABLED_KEY)
    );
    assert!(*f.settings.user_extensions_disabled.lock().unwrap());
}

#[test]
fn there_is_no_settings_access_without_the_selected_sessions_bus() {
    let f = Fixture::gnome();
    assert!(GnomeSettings::new(f.io.clone(), &BTreeMap::new(), &deadline()).is_err());
    let missing = BTreeMap::from([(
        "DBUS_SESSION_BUS_ADDRESS".to_owned(),
        format!(
            "unix:path={}",
            f.io.target().paths().runtime_home.join("nope").display()
        ),
    )]);
    assert!(GnomeSettings::new(f.io.clone(), &missing, &deadline()).is_err());
    // A bus outside the selected runtime directory is refused as well.
    let foreign = BTreeMap::from([(
        "DBUS_SESSION_BUS_ADDRESS".to_owned(),
        "unix:path=/tmp/some-other-bus".to_owned(),
    )]);
    assert!(GnomeSettings::new(f.io.clone(), &foreign, &deadline()).is_err());
    assert!(f.settings.calls.lock().unwrap().is_empty());
}

#[test]
fn the_native_command_allowlist_admits_only_the_one_key_and_only_canonical_lists() {
    let f = Fixture::gnome();
    let env =
        f.io.session_bus_environment(f.bus.clone(), &deadline())
            .unwrap();
    let spec = |argv: &[&str]| {
        CommandSpec::new(
            "/usr/bin/gsettings".into(),
            argv.iter().map(|s| (*s).to_owned()).collect(),
            env.clone(),
            4096,
        )
    };
    assert!(spec(&["get", SCHEMA, ENABLED_KEY]).is_ok());
    assert!(spec(&["get", SCHEMA, DISABLED_KEY]).is_ok());
    assert!(
        spec(&[
            "set",
            SCHEMA,
            ENABLED_KEY,
            "['a@b', 'crosspane@frostdev.io']"
        ])
        .is_ok()
    );
    for bad in [
        // another key, schema or verb
        &["get", SCHEMA, "favorite-apps"][..],
        &["get", "org.gnome.desktop.interface", ENABLED_KEY],
        &["set", SCHEMA, DISABLED_KEY, "true"],
        &["reset", SCHEMA, ENABLED_KEY],
        &["list-recursively"],
        // a value that is not exactly what the installer renders
        &["set", SCHEMA, ENABLED_KEY, "['a b']"],
        &["set", SCHEMA, ENABLED_KEY, "@as []"],
        &["set", SCHEMA, ENABLED_KEY, "['a@b','c@d']"],
        &["set", SCHEMA, ENABLED_KEY, "[\"a@b\"]"],
        &["set", SCHEMA, ENABLED_KEY, "['a@b']; reboot"],
        &["set", SCHEMA, ENABLED_KEY],
        &["set", "--", SCHEMA, ENABLED_KEY, "[]"],
    ] {
        assert!(spec(bad).is_err(), "{bad:?}");
    }
    // Settings go over an admitted session bus, never an environment without one.
    let bare = ChildEnvironment::selected(f.io.target(), BTreeMap::new()).unwrap();
    assert!(
        CommandSpec::new(
            "/usr/bin/gsettings".into(),
            vec!["get".into(), SCHEMA.into(), ENABLED_KEY.into()],
            bare,
            4096
        )
        .is_err()
    );
    // A write is only ever sent as a mutation with a current proof of the same target.
    let write = spec(&["set", SCHEMA, ENABLED_KEY, "[]"]).unwrap();
    assert_eq!(
        f.io.run(&write, &deadline()).unwrap_err(),
        NativeError::Unsupported
    );
    let read = spec(&["get", SCHEMA, ENABLED_KEY]).unwrap();
    assert_eq!(
        f.io.settings_mutation(&f.proof, &read, &deadline())
            .unwrap_err(),
        NativeError::Invalid
    );
    let other = Fixture::gnome();
    assert_eq!(
        f.io.settings_mutation(&other.proof, &write, &deadline())
            .unwrap_err(),
        NativeError::Unsupported
    );
    assert!(f.settings.calls("set").is_empty());
    f.io.settings_mutation(&f.proof, &write, &deadline())
        .unwrap();
    assert_eq!(f.settings.calls("set").len(), 1);
}

// ---- the install adapter ------------------------------------------------------------------

fn elf() -> Vec<u8> {
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
    v
}
fn number(h: &mut [u8], start: usize, width: usize, n: usize) {
    let s = format!("{n:0width$o}\0", width = width - 1);
    h[start..start + width].copy_from_slice(s.as_bytes());
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
    h[148..156].fill(b' ');
    let n: usize = h.iter().map(|b| usize::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{n:06o}\0 ").as_bytes());
    h.extend_from_slice(b);
    h.resize(h.len().div_ceil(512) * 512, 0);
    h
}
fn hex(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn package(schema: u32) -> Package {
    let mut members: Vec<(&str, Vec<u8>)> = FILES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                *name,
                match i {
                    0..=3 => elf(),
                    4 => b"[Service]\nExecStart={{agent_executable}} run\nEnvironment={{xdg_config_environment}}\nEnvironment={{xdg_state_environment}}\nEnvironment={{xdg_runtime_environment}}\nEnvironment={{crosspane_runtime_environment}}\n".to_vec(),
                    5 => b"[Desktop Entry]\nType=Application\nName=Crosspane\nExec={{settings_executable}}\n".to_vec(),
                    6 => b"[Desktop Entry]\nType=Application\nName=Crosspane\nExec={{installer_executable}}\n".to_vec(),
                    _ => format!("inert-{i}\n").into_bytes(),
                },
            )
        })
        .collect();
    if schema == SCHEMA_DESKTOP {
        for (name, text) in
            DESKTOP_FILES
                .iter()
                .zip([DESKTOP_ENTRY, EXTENSION, METADATA, SHELL_XML])
        {
            members.push((*name, text.as_bytes().to_vec()));
        }
    }
    let manifest = Manifest {
        schema_version: schema,
        product_version: "0.0.1".into(),
        architecture: Architecture::native().unwrap(),
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf()),
        }],
        members: members
            .iter()
            .enumerate()
            .map(|(i, (name, bytes))| Artifact {
                name: (*name).into(),
                size: bytes.len(),
                sha256: hex(bytes),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    };
    let mut archive = member("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    for (name, bytes) in &members {
        archive.extend(member(name, bytes));
    }
    archive.extend(vec![0; 1024]);
    Package::read(
        &archive[..],
        Architecture::native().unwrap(),
        sha256(&archive),
    )
    .unwrap()
}

fn leaf_paths(f: &Fixture) -> [PathBuf; 4] {
    [
        f.data("applications/io.frostdev.crosspane.agent.desktop"),
        f.data("gnome-shell/extensions/crosspane@frostdev.io/extension.js"),
        f.data("gnome-shell/extensions/crosspane@frostdev.io/metadata.json"),
        f.data("gnome-shell/extensions/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml"),
    ]
}
fn install(f: &Fixture, payloads: &mut NativePayloads, p: &Package) -> Result<(), PayloadError> {
    payloads.plan(&f.proof, p, OperationId(1), false).unwrap();
    payloads.apply(&f.proof, p, OperationId(1), &deadline())
}

#[test]
fn a_gnome_install_adds_the_desktop_files_turns_the_extension_on_and_says_to_log_in_again() {
    let f = Fixture::gnome();
    f.settings.set_list(&["keep@me.org"]);
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    payloads.plan(&f.proof, &p, OperationId(1), false).unwrap();
    let note = payloads.desktop_note().unwrap();
    assert!(
        note.contains("desktop entry")
            && note.contains("Shell extension")
            && note.contains("next log-in"),
        "{note}"
    );
    payloads
        .apply(&f.proof, &p, OperationId(1), &deadline())
        .unwrap();
    for (path, text) in leaf_paths(&f)
        .iter()
        .zip([DESKTOP_ENTRY, EXTENSION, METADATA, SHELL_XML])
    {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            text,
            "{}",
            path.display()
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
    assert_eq!(f.settings.list(), ["keep@me.org", UUID]);
    let warnings = payloads.warnings();
    assert_eq!(warnings, [extension::NEXT_LOGIN_NOTE]);
    // Core and desktop files are judged together: 9 + 4 rows, all in place.
    let rows = payloads.observe(&f.proof, &p).unwrap();
    assert_eq!(rows.len(), 13);
    assert!(
        rows.iter()
            .all(|r| r.before == ResourceObservation::Matching)
    );
}

#[test]
fn an_extension_that_cannot_be_turned_on_is_a_warning_and_the_install_still_succeeds() {
    let f = Fixture::gnome();
    f.settings.set_list(&["keep@me.org"]);
    *f.settings.set_fails.lock().unwrap() = true;
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    assert_eq!(f.settings.list(), ["keep@me.org"]);
    for path in leaf_paths(&f) {
        assert!(path.exists(), "{}", path.display());
    }
    let warnings = payloads.warnings().join(" ");
    assert!(warnings.contains("couldn't be turned on"), "{warnings}");
    assert!(warnings.contains("next log-in"), "{warnings}");
}

#[test]
fn switched_off_user_extensions_and_a_missing_session_bus_are_warnings_too() {
    let f = Fixture::gnome();
    *f.settings.user_extensions_disabled.lock().unwrap() = true;
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    assert_eq!(f.settings.list(), [UUID]);
    let warnings = payloads.warnings().join(" ");
    assert!(warnings.contains("switched off"), "{warnings}");
    // The person's switch is theirs: it is still off.
    assert!(*f.settings.user_extensions_disabled.lock().unwrap());

    let g = Fixture::gnome();
    let mut without_bus = NativePayloads::new(g.io.clone()).unwrap();
    install(&g, &mut without_bus, &p).unwrap();
    assert!(g.settings.calls.lock().unwrap().is_empty());
    assert!(
        without_bus
            .warnings()
            .join(" ")
            .contains("couldn't be turned on")
    );
    for path in leaf_paths(&g) {
        assert!(path.exists());
    }
}

#[test]
fn hyprland_installs_no_desktop_files_and_never_touches_settings() {
    let f = Fixture::on(Desktop::Hyprland, Some([0, 56, 2]));
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    payloads.plan(&f.proof, &p, OperationId(1), false).unwrap();
    assert_eq!(payloads.desktop_note(), None);
    payloads
        .apply(&f.proof, &p, OperationId(1), &deadline())
        .unwrap();
    assert!(payloads.warnings().is_empty());
    assert!(f.settings.calls.lock().unwrap().is_empty());
    assert!(!f.data("gnome-shell").exists());
    assert!(
        !f.data("applications/io.frostdev.crosspane.agent.desktop")
            .exists()
    );
    assert!(
        !f.io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer/desktop-outcome.json")
            .exists()
    );
    let rows = payloads.observe(&f.proof, &p).unwrap();
    assert_eq!(rows.len(), 9);
    assert!(
        rows.iter()
            .all(|r| r.before == ResourceObservation::Matching)
    );
}

#[test]
fn an_old_shell_gets_the_desktop_entry_but_not_the_extension() {
    let f = Fixture::on(Desktop::Gnome, Some([47, 2, 0]));
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    payloads.plan(&f.proof, &p, OperationId(1), false).unwrap();
    let note = payloads.desktop_note().unwrap();
    assert!(!note.contains("Shell extension"), "{note}");
    payloads
        .apply(&f.proof, &p, OperationId(1), &deadline())
        .unwrap();
    let [entry, extension, ..] = leaf_paths(&f);
    assert!(entry.exists());
    assert!(!extension.exists());
    assert!(!f.data("gnome-shell").exists());
    assert!(f.settings.calls.lock().unwrap().is_empty());
    assert!(payloads.warnings().is_empty());
    assert_eq!(payloads.observe(&f.proof, &p).unwrap().len(), 10);
}

#[test]
fn an_unreadable_shell_version_still_installs_the_extension_for_gnome_to_judge() {
    let f = Fixture::on(Desktop::Gnome, None);
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    assert!(leaf_paths(&f)[1].exists());
    assert_eq!(f.settings.list(), [UUID]);
}

#[test]
fn kde_gets_only_the_desktop_entry() {
    let f = Fixture::on(Desktop::Kde, None);
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    let [entry, extension, ..] = leaf_paths(&f);
    assert!(entry.exists());
    assert!(!extension.exists());
    assert!(f.settings.calls.lock().unwrap().is_empty());
    assert_eq!(payloads.observe(&f.proof, &p).unwrap().len(), 10);
}

#[test]
fn a_payload_prepared_before_gnome_and_kde_support_changes_nothing_there() {
    for desktop in [Desktop::Gnome, Desktop::Kde] {
        let f = Fixture::on(desktop, Some([50, 4, 0]));
        let p = package(SCHEMA_CORE);
        let mut payloads = f.payloads();
        assert!(matches!(
            payloads.plan(&f.proof, &p, OperationId(1), false),
            Err(PayloadError::NoDesktopFiles)
        ));
        assert!(matches!(
            payloads.detect(&f.proof, &p),
            Err(PayloadError::NoDesktopFiles)
        ));
        assert!(f.settings.calls.lock().unwrap().is_empty());
        assert!(!f.io.target().agent_path().exists(), "no core file either");
    }
    // The same schema-1 payload is a perfectly good Hyprland payload.
    let f = Fixture::on(Desktop::Hyprland, Some([0, 56, 2]));
    let p = package(SCHEMA_CORE);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    assert!(f.io.target().agent_path().exists());
}

#[test]
fn drift_is_exactly_what_a_repair_of_the_desktop_files_would_put_back() {
    let f = Fixture::gnome();
    let p = package(SCHEMA_DESKTOP);
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    // Before anything is installed, all four files are missing.
    assert_eq!(installer.desktop_drift(&p, &f.proof).unwrap().len(), 4);
    let lines = installer
        .desktop_install(&p, &f.proof, &f.bus, OperationId(1), &deadline())
        .unwrap();
    assert_eq!(lines, [extension::NEXT_LOGIN_NOTE]);
    assert_eq!(f.settings.list(), [UUID]);
    assert!(installer.desktop_drift(&p, &f.proof).unwrap().is_empty());
    // An edited extension and a deleted desktop entry are the only drift.
    let [entry, extension_js, ..] = leaf_paths(&f);
    fs::write(&extension_js, b"// edited\n").unwrap();
    fs::remove_file(&entry).unwrap();
    let drift = installer.desktop_drift(&p, &f.proof).unwrap();
    assert_eq!(
        drift
            .iter()
            .map(|r| r.resource_id.as_str())
            .collect::<Vec<_>>(),
        [DESKTOP_FILES[0], DESKTOP_FILES[1]]
    );
    // The repair step is the install step: it puts both back and keeps the edited copy.
    installer
        .desktop_install(&p, &f.proof, &f.bus, OperationId(2), &deadline())
        .unwrap();
    assert!(installer.desktop_drift(&p, &f.proof).unwrap().is_empty());
    assert_eq!(fs::read_to_string(&extension_js).unwrap(), EXTENSION);
    assert_eq!(fs::read_to_string(&entry).unwrap(), DESKTOP_ENTRY);
    // The extension was already on: no second write.
    assert_eq!(f.settings.calls("set").len(), 1);
}

#[test]
fn hyprland_has_no_desktop_drift_and_an_old_payload_is_named_on_gnome() {
    let f = Fixture::on(Desktop::Hyprland, Some([0, 56, 2]));
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    assert!(
        installer
            .desktop_drift(&package(SCHEMA_DESKTOP), &f.proof)
            .unwrap()
            .is_empty()
    );
    assert!(
        installer
            .desktop_drift(&package(SCHEMA_CORE), &f.proof)
            .unwrap()
            .is_empty()
    );
    let g = Fixture::gnome();
    let installer = PayloadInstaller::new(g.io.clone()).unwrap();
    assert_eq!(
        installer
            .desktop_drift(&package(SCHEMA_CORE), &g.proof)
            .unwrap_err(),
        PayloadError::NoDesktopFiles
    );
}

#[test]
fn a_reinstall_over_edited_desktop_files_repairs_them_from_the_backup_route() {
    let f = Fixture::gnome();
    let p = package(SCHEMA_DESKTOP);
    let mut payloads = f.payloads();
    install(&f, &mut payloads, &p).unwrap();
    let [_, extension, ..] = leaf_paths(&f);
    fs::write(&extension, b"// edited by hand\n").unwrap();
    // The edit shows as a mismatch the install flow acts on.
    let rows = payloads.observe(&f.proof, &p).unwrap();
    assert_eq!(
        rows.iter()
            .filter(|r| r.before != ResourceObservation::Matching)
            .count(),
        1
    );
    let mut again = f.payloads();
    again.plan(&f.proof, &p, OperationId(2), true).unwrap();
    again
        .apply(&f.proof, &p, OperationId(2), &deadline())
        .unwrap();
    assert_eq!(fs::read_to_string(&extension).unwrap(), EXTENSION);
    let backups: Vec<_> = fs::read_dir(f.io.target().paths().state_home.join("crosspane/backups"))
        .unwrap()
        .collect();
    assert_eq!(backups.len(), 1);
}
