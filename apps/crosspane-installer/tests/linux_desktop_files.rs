#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The GNOME and KDE desktop files of a schema-2 payload (`payload/desktop.rs`): the archive
//! contract, the plan each session gets, apply, rows and removal on the private record, and the
//! hostile targets. Every install runs in a scratch home under /tmp, never the owner's files.
use crosspane_installer::platform::linux::{detect::Desktop, native_io::*, payload::*};
use crosspane_installer_core::{OperationId, ResourceObservation, ResourceOwnership};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
const LEAVES: usize = 4;

// The desktop members are the packaging's real files, so these tests judge the shipped bytes.
const DESKTOP_ENTRY: &str =
    include_str!("../../../packaging/linux/io.frostdev.crosspane.agent.desktop");
const EXTENSION: &str =
    include_str!("../../../packaging/gnome-shell-extension/crosspane@frostdev.io/extension.js");
const METADATA: &str =
    include_str!("../../../packaging/gnome-shell-extension/crosspane@frostdev.io/metadata.json");
const SHELL_XML: &str = include_str!(
    "../../../packaging/gnome-shell-extension/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml"
);

/// Where each leaf lands under the data folder, in `DESKTOP_FILES` order.
const LEAF_PATHS: [&str; LEAVES] = [
    "applications/io.frostdev.crosspane.agent.desktop",
    "gnome-shell/extensions/crosspane@frostdev.io/extension.js",
    "gnome-shell/extensions/crosspane@frostdev.io/metadata.json",
    "gnome-shell/extensions/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml",
];

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
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: self.root.join(".local/bin/crosspane-agent"),
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
        compositor_version: if desktop == Desktop::Hyprland {
            [0, 56, 0]
        } else {
            [49, 0, 0]
        },
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
    install: PayloadInstaller,
    proof: SupportProof,
}
impl Fixture {
    fn on(desktop: Desktop) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cpdesk-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe { root: root.clone() });
        let io = Arc::new(LinuxNativeIo::scratch(&root, Arc::new(Runner), probe).unwrap());
        let proof = io.scratch_support(facts(&io, desktop)).unwrap();
        io.create_private_dir(&proof, &io.target().paths().runtime_home)
            .unwrap();
        let install = PayloadInstaller::new(io.clone()).unwrap();
        Self {
            root,
            io,
            install,
            proof,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn encode(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
/// The hex digest of `bytes`, as the record and the manifest spell it.
fn hex(bytes: &[u8]) -> String {
    encode(&sha256(bytes))
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
fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn read(bytes: &[u8]) -> Result<Package, PayloadError> {
    Package::read(bytes, Architecture::native().unwrap(), sha256(bytes))
}

/// The four desktop members as the packaging ships them, in `DESKTOP_FILES` order.
fn desktop_contents() -> Vec<Vec<u8>> {
    [DESKTOP_ENTRY, EXTENSION, METADATA, SHELL_XML]
        .iter()
        .map(|text| text.as_bytes().to_vec())
        .collect()
}
fn bytes_plus(text: &str, extra: &[u8]) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.extend_from_slice(extra);
    bytes
}

type Parts = Vec<(&'static str, Vec<u8>)>;

/// The members of a payload at `schema`: the nine core files, then for schema 2 the four desktop
/// files.
fn members_at(schema: u32, version: u8) -> Parts {
    let mut parts: Parts = FILES.iter().copied().zip(contents(version)).collect();
    if schema == SCHEMA_DESKTOP {
        parts.extend(DESKTOP_FILES.iter().copied().zip(desktop_contents()));
    }
    parts
}
fn manifest_of(schema: u32, members: &Parts) -> Manifest {
    Manifest {
        schema_version: schema,
        product_version: "0.0.1".into(),
        architecture: Architecture::native().unwrap(),
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf(1)),
        }],
        members: members
            .iter()
            .enumerate()
            .map(|(index, (name, bytes))| Artifact {
                name: (*name).into(),
                size: bytes.len(),
                sha256: hex(bytes),
                features: if index == 0 {
                    vec!["video".into()]
                } else {
                    vec![]
                },
            })
            .collect(),
    }
}
fn archive_of(manifest: &Manifest, members: &Parts) -> Vec<u8> {
    let mut archive = member("manifest.json", &serde_json::to_vec(manifest).unwrap());
    for (name, bytes) in members {
        archive.extend(member(name, bytes));
    }
    archive.extend(vec![0; 1024]);
    archive
}
/// A complete archive at `schema`, after `change` edits its members. The manifest is computed from
/// the edited members, so what a test refuses is the member content, not the manifest.
fn archive_with(schema: u32, change: impl FnOnce(&mut Parts)) -> Vec<u8> {
    let mut members = members_at(schema, 1);
    change(&mut members);
    archive_of(&manifest_of(schema, &members), &members)
}
fn set(members: &mut Parts, name: &str, bytes: Vec<u8>) {
    members
        .iter_mut()
        .find(|(member, _)| *member == name)
        .unwrap()
        .1 = bytes;
}
fn package(schema: u32) -> Package {
    read(&archive_with(schema, |_| {})).unwrap()
}

fn data_home(f: &Fixture) -> PathBuf {
    f.io.target().paths().data_home.clone()
}
fn extension_dir(f: &Fixture) -> PathBuf {
    data_home(f).join("gnome-shell/extensions/crosspane@frostdev.io")
}
fn target(f: &Fixture, index: usize) -> PathBuf {
    data_home(f).join(LEAF_PATHS[index])
}
fn record(f: &Fixture) -> PathBuf {
    f.io.target()
        .paths()
        .state_home
        .join("crosspane/installer/desktop-outcome.json")
}
fn backup_root(f: &Fixture) -> PathBuf {
    f.io.target().paths().state_home.join("crosspane/backups")
}
/// The one backup folder this fixture's installs made.
fn backup_folder(f: &Fixture) -> PathBuf {
    let mut folders: Vec<PathBuf> = fs::read_dir(backup_root(f))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(folders.len(), 1);
    folders.remove(0)
}
/// The name a displaced leaf takes in the backup folder.
fn backup_name(index: usize) -> String {
    format!("desktop-{}", LEAF_PATHS[index].rsplit('/').next().unwrap())
}
fn record_json(f: &Fixture) -> Value {
    serde_json::from_slice(&fs::read(record(f)).unwrap()).unwrap()
}
/// A record as the installer would write it, with the given leaves (id, digest).
fn record_bytes(schema: u64, leaves: &[(&str, String)]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": schema,
        "operation": 1,
        "desktop": "gnome",
        "manifest_sha256": "0".repeat(64),
        "leaves": leaves
            .iter()
            .map(|(id, digest)| json!({"id": id, "sha256": digest}))
            .collect::<Vec<_>>(),
    }))
    .unwrap()
}
fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o777
}
fn ids() -> Vec<String> {
    DESKTOP_FILES.iter().map(|id| (*id).to_owned()).collect()
}
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}
fn file_ids(plan: &DesktopPlan) -> Vec<&'static str> {
    plan.files().iter().map(DesktopFile::id).collect()
}
fn plan_for(f: &Fixture, p: &Package, desktop: Desktop, extension: bool) -> DesktopPlan {
    f.install.desktop_plan(p, desktop, extension).unwrap()
}
fn apply_desktop(f: &Fixture, plan: &DesktopPlan, operation: u64, p: &Package) {
    f.install
        .desktop_apply(
            &f.proof,
            plan,
            OperationId(operation),
            p.manifest_hash(),
            &deadline(),
        )
        .unwrap();
}
fn install_core(f: &Fixture, p: &Package, operation: u64) {
    let plan = f
        .install
        .plan(&f.proof, p, OperationId(operation), MatchingFiles::Preserve)
        .unwrap();
    f.install.apply(&f.proof, p, plan, &deadline()).unwrap();
}

#[test]
fn schema_two_carries_thirteen_members_and_schema_one_carries_none() {
    let two = read(&archive_with(SCHEMA_DESKTOP, |_| {})).unwrap();
    assert!(two.has_desktop_files());
    assert_eq!(two.manifest().schema_version, SCHEMA_DESKTOP);
    assert_eq!(two.manifest().members.len(), 13);
    let one = read(&archive_with(SCHEMA_CORE, |_| {})).unwrap();
    assert!(!one.has_desktop_files());
    assert_eq!(one.manifest().members.len(), 9);
}

#[test]
fn invalid_desktop_members_refuse_the_whole_archive() {
    assert!(read(&archive_with(SCHEMA_DESKTOP, |_| {})).is_ok());
    let metadata = METADATA.replace(
        "\"uuid\": \"crosspane@frostdev.io\"",
        "\"uuid\": \"other@frostdev.io\"",
    );
    let template = DESKTOP_ENTRY.replace("Exec=crosspane-agent", "Exec={{agent_executable}}");
    let headerless = DESKTOP_ENTRY.replacen("[Desktop Entry]", "[Desktop Action]", 1);
    assert_ne!(metadata, METADATA);
    assert_ne!(template, DESKTOP_ENTRY);
    assert_ne!(headerless, DESKTOP_ENTRY);
    let oversized = bytes_plus(
        DESKTOP_ENTRY,
        format!("# {}\n", "x".repeat(MAX_RECORD_BYTES)).as_bytes(),
    );

    // Thirteen listed, twelve carried.
    let full = members_at(SCHEMA_DESKTOP, 1);
    let mut short = full.clone();
    short.retain(|(name, _)| *name != DESKTOP_FILES[3]);
    assert!(read(&archive_of(&manifest_of(SCHEMA_DESKTOP, &full), &short)).is_err());
    // Thirteen-minus-one listed as well: the manifest itself is short.
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            m.retain(|(name, _)| *name != DESKTOP_FILES[3]);
        }))
        .is_err()
    );
    // Schema 1 with a desktop member on top: nine listed, ten carried.
    let core = members_at(SCHEMA_CORE, 1);
    let mut carrying = core.clone();
    carrying.push((DESKTOP_FILES[0], DESKTOP_ENTRY.as_bytes().to_vec()));
    assert!(read(&archive_of(&manifest_of(SCHEMA_CORE, &core), &carrying)).is_err());
    // A schema this build does not know.
    assert!(read(&archive_of(&manifest_of(3, &full), &full)).is_err());
    // Content: each desktop member must be what its name says.
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[2], metadata.as_bytes().to_vec());
        }))
        .is_err()
    );
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[0], template.as_bytes().to_vec());
        }))
        .is_err()
    );
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[0], headerless.as_bytes().to_vec());
        }))
        .is_err()
    );
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[0], oversized);
        }))
        .is_err()
    );
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[1], bytes_plus(EXTENSION, b"\0"));
        }))
        .is_err()
    );
    assert!(
        read(&archive_with(SCHEMA_DESKTOP, |m| {
            set(m, DESKTOP_FILES[1], bytes_plus(EXTENSION, &[0xff, 0xfe]));
        }))
        .is_err()
    );
}

#[test]
fn plan_shapes_follow_the_session_and_the_schema() {
    let f = Fixture::on(Desktop::Gnome);
    let two = package(SCHEMA_DESKTOP);
    let one = package(SCHEMA_CORE);

    // Hyprland installs nothing, on either schema.
    for (p, extension) in [(&two, true), (&two, false), (&one, true)] {
        let plan = plan_for(&f, p, Desktop::Hyprland, extension);
        assert!(plan.is_empty());
        assert!(!plan.installs_extension());
        assert!(plan.preview().is_none());
        assert_eq!(plan.desktop(), Desktop::Hyprland);
    }

    // KDE gets the agent's desktop entry only, whatever the extension flag says.
    for extension in [false, true] {
        let plan = plan_for(&f, &two, Desktop::Kde, extension);
        assert_eq!(plan.desktop(), Desktop::Kde);
        assert!(!plan.installs_extension());
        assert_eq!(file_ids(&plan), [DESKTOP_FILES[0]]);
        assert_eq!(plan.files()[0].path(), target(&f, 0).as_path());
    }

    // GNOME gets all four with the extension, the desktop entry alone without it.
    let gnome = plan_for(&f, &two, Desktop::Gnome, true);
    assert_eq!(gnome.desktop(), Desktop::Gnome);
    assert!(gnome.installs_extension());
    assert_eq!(file_ids(&gnome), DESKTOP_FILES.to_vec());
    for (index, file) in gnome.files().iter().enumerate() {
        assert_eq!(file.path(), target(&f, index).as_path());
    }
    let entry_only = plan_for(&f, &two, Desktop::Gnome, false);
    assert!(!entry_only.installs_extension());
    assert_eq!(file_ids(&entry_only), [DESKTOP_FILES[0]]);

    // A schema-1 payload has no desktop files, so KDE and GNOME refuse it.
    for (desktop, extension) in [
        (Desktop::Kde, false),
        (Desktop::Gnome, true),
        (Desktop::Gnome, false),
    ] {
        assert_eq!(
            f.install
                .desktop_plan(&one, desktop, extension)
                .unwrap_err(),
            PayloadError::NoDesktopFiles
        );
    }
}

#[test]
fn fresh_gnome_apply_places_exact_bytes_modes_and_the_record() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);

    let before = f.install.desktop_rows(&f.proof, &plan).unwrap();
    assert_eq!(before.len(), LEAVES);
    for (index, row) in before.iter().enumerate() {
        assert_eq!(row.resource_id, DESKTOP_FILES[index]);
        assert_eq!(row.resolved_path, target(&f, index).to_str().unwrap());
        assert_eq!(row.before, ResourceObservation::Absent);
        assert_eq!(row.ownership, ResourceOwnership::Created);
    }

    apply_desktop(&f, &plan, 7, &p);
    for (index, bytes) in desktop_contents().iter().enumerate() {
        assert_eq!(fs::read(target(&f, index)).unwrap(), *bytes);
        assert_eq!(mode_of(&target(&f, index)), 0o644);
    }
    assert_eq!(mode_of(&record(&f)), 0o600);

    let value = record_json(&f);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["operation"], 7);
    assert_eq!(value["desktop"], "gnome");
    assert_eq!(value["manifest_sha256"], encode(&p.manifest_hash()));
    let leaves = value["leaves"].as_array().unwrap();
    let listed: Vec<&str> = leaves
        .iter()
        .map(|leaf| leaf["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, DESKTOP_FILES);
    let digests: Vec<&str> = leaves
        .iter()
        .map(|leaf| leaf["sha256"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = desktop_contents().iter().map(|b| hex(b)).collect();
    assert_eq!(digests, expected);
    assert_eq!(f.install.desktop_recorded(&f.proof).unwrap(), ids());

    for row in f.install.desktop_rows(&f.proof, &plan).unwrap() {
        assert_eq!(row.before, ResourceObservation::Matching);
        assert_eq!(row.ownership, ResourceOwnership::Created);
    }
}

#[test]
fn applying_again_keeps_every_placed_inode_and_its_bytes() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    apply_desktop(&f, &plan, 7, &p);
    let first: Vec<u64> = (0..LEAVES)
        .map(|index| fs::metadata(target(&f, index)).unwrap().ino())
        .collect();

    apply_desktop(&f, &plan, 8, &p);
    let second: Vec<u64> = (0..LEAVES)
        .map(|index| fs::metadata(target(&f, index)).unwrap().ino())
        .collect();
    assert_eq!(first, second);
    for (index, bytes) in desktop_contents().iter().enumerate() {
        assert_eq!(fs::read(target(&f, index)).unwrap(), *bytes);
        assert_eq!(mode_of(&target(&f, index)), 0o644);
    }
    for row in f.install.desktop_rows(&f.proof, &plan).unwrap() {
        assert_eq!(row.before, ResourceObservation::Matching);
        assert_eq!(row.ownership, ResourceOwnership::Created);
    }
}

#[test]
fn foreign_bytes_at_every_leaf_are_saved_in_the_backup_then_replaced() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    let foreign: Vec<Vec<u8>> = (0..LEAVES)
        .map(|index| format!("someone else's file {index}\n").into_bytes())
        .collect();
    for (index, bytes) in foreign.iter().enumerate() {
        put(&target(&f, index), bytes, 0o644);
    }

    for row in f.install.desktop_rows(&f.proof, &plan).unwrap() {
        assert_eq!(row.before, ResourceObservation::Different);
        assert_eq!(row.ownership, ResourceOwnership::Foreign);
    }

    apply_desktop(&f, &plan, 7, &p);
    let backup = backup_folder(&f);
    for (index, bytes) in foreign.iter().enumerate() {
        assert_eq!(fs::read(backup.join(backup_name(index))).unwrap(), *bytes);
    }
    for (index, bytes) in desktop_contents().iter().enumerate() {
        assert_eq!(fs::read(target(&f, index)).unwrap(), *bytes);
        assert_eq!(mode_of(&target(&f, index)), 0o644);
    }
}

#[test]
fn symlinks_at_a_leaf_and_the_extension_folder_are_moved_aside_never_followed() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    let outside_entry = f.root.join("outside/entry.desktop");
    put(&outside_entry, b"outside entry\n", 0o644);
    let outside_dir = f.root.join("outside/extension");
    put(&outside_dir.join("keep.txt"), b"keep\n", 0o644);
    fs::create_dir_all(data_home(&f).join("applications")).unwrap();
    fs::create_dir_all(data_home(&f).join("gnome-shell/extensions")).unwrap();
    symlink(&outside_entry, target(&f, 0)).unwrap();
    symlink(&outside_dir, extension_dir(&f)).unwrap();

    for row in f.install.desktop_rows(&f.proof, &plan).unwrap() {
        assert_eq!(row.before, ResourceObservation::Different);
        assert_eq!(row.ownership, ResourceOwnership::Foreign);
    }

    apply_desktop(&f, &plan, 7, &p);
    let backup = backup_folder(&f);
    assert_eq!(
        fs::read_link(backup.join(backup_name(0))).unwrap(),
        outside_entry
    );
    assert_eq!(
        fs::read_link(backup.join("desktop-extension-folder")).unwrap(),
        outside_dir
    );
    assert_eq!(fs::read(&outside_entry).unwrap(), b"outside entry\n");
    assert_eq!(fs::read(outside_dir.join("keep.txt")).unwrap(), b"keep\n");

    assert!(
        fs::symlink_metadata(target(&f, 0))
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(fs::read(target(&f, 0)).unwrap(), desktop_contents()[0]);
    assert!(
        fs::symlink_metadata(extension_dir(&f))
            .unwrap()
            .file_type()
            .is_dir()
    );
    for (index, bytes) in desktop_contents().iter().enumerate().skip(1) {
        assert_eq!(fs::read(target(&f, index)).unwrap(), *bytes);
    }
}

#[test]
fn hyprland_installs_no_desktop_file_and_the_core_nine_are_unchanged() {
    let f = Fixture::on(Desktop::Hyprland);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Hyprland, true);
    assert!(f.install.desktop_rows(&f.proof, &plan).unwrap().is_empty());
    f.install
        .desktop_apply(
            &f.proof,
            &plan,
            OperationId(1),
            p.manifest_hash(),
            &deadline(),
        )
        .unwrap();
    assert!(!record(&f).exists());
    assert!(!data_home(&f).join("gnome-shell").exists());
    assert!(!data_home(&f).join("applications").exists());
    assert!(f.install.desktop_recorded(&f.proof).unwrap().is_empty());

    install_core(&f, &p, 1);
    assert_eq!(f.install.targets().len(), FILES.len());
    for path in f.install.targets() {
        assert!(path.exists(), "{}", path.display());
    }
    assert!(!target(&f, 0).exists());
    assert!(!extension_dir(&f).exists());
    assert!(!record(&f).exists());
    assert!(f.install.desktop_recorded(&f.proof).unwrap().is_empty());
    assert_eq!(
        f.install.desktop_remove(&f.proof, &deadline()).unwrap(),
        DesktopRemoval::default()
    );
}

#[test]
fn removal_takes_the_recorded_leaves_then_the_empty_folder_and_the_record() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    apply_desktop(&f, &plan, 7, &p);

    let removal = f.install.desktop_remove(&f.proof, &deadline()).unwrap();
    assert_eq!(
        removal,
        DesktopRemoval {
            removed: ids(),
            absent: Vec::new(),
            kept: Vec::new(),
        }
    );
    for index in 0..LEAVES {
        assert!(!target(&f, index).exists());
    }
    assert!(!extension_dir(&f).exists());
    assert!(!record(&f).exists());
    assert_eq!(
        f.install.desktop_remove(&f.proof, &deadline()).unwrap(),
        DesktopRemoval::default()
    );
    assert!(f.install.desktop_recorded(&f.proof).unwrap().is_empty());
}

#[test]
fn removal_keeps_an_edited_leaf_and_reports_a_missing_one() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    apply_desktop(&f, &plan, 7, &p);
    fs::write(target(&f, 1), b"edited by the person\n").unwrap();
    fs::remove_file(target(&f, 2)).unwrap();

    let all = ids();
    let removal = f.install.desktop_remove(&f.proof, &deadline()).unwrap();
    assert_eq!(
        removal,
        DesktopRemoval {
            removed: vec![all[0].clone(), all[3].clone()],
            absent: vec![all[2].clone()],
            kept: vec![all[1].clone()],
        }
    );
    assert_eq!(fs::read(target(&f, 1)).unwrap(), b"edited by the person\n");
    assert!(extension_dir(&f).exists());
    assert!(!target(&f, 0).exists());
    assert!(!target(&f, 3).exists());
    assert!(!record(&f).exists());
}

#[test]
fn tampered_and_symlinked_records_refuse_rows_and_removal_and_are_never_followed() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    let entry = DESKTOP_FILES[0];
    let good = hex(&desktop_contents()[0]);

    // The shape used below is a sound record: a valid one reads back.
    put(
        &record(&f),
        &record_bytes(1, &[(entry, good.clone())]),
        0o600,
    );
    assert_eq!(
        f.install.desktop_recorded(&f.proof).unwrap(),
        vec![entry.to_owned()]
    );
    fs::remove_file(record(&f)).unwrap();

    let tampered = [
        b"{not json".to_vec(),
        record_bytes(1, &[]),
        record_bytes(1, &[("resources/not-a-desktop-file", good.clone())]),
        record_bytes(1, &[(entry, good.clone()), (entry, good.clone())]),
        record_bytes(1, &[(entry, "not-a-digest".to_owned())]),
        record_bytes(2, &[(entry, good.clone())]),
    ];
    for bytes in tampered {
        put(&record(&f), &bytes, 0o600);
        assert_eq!(
            f.install.desktop_rows(&f.proof, &plan).unwrap_err(),
            PayloadError::Foreign
        );
        assert_eq!(
            f.install.desktop_remove(&f.proof, &deadline()).unwrap_err(),
            PayloadError::Foreign
        );
        // A refused record is left exactly as it was.
        assert_eq!(fs::read(record(&f)).unwrap(), bytes);
        fs::remove_file(record(&f)).unwrap();
    }

    // A record that is a link is refused, and the file it names is never read or removed.
    let outside = f.root.join("outside/record.json");
    put(&outside, &record_bytes(1, &[(entry, good)]), 0o600);
    fs::create_dir_all(record(&f).parent().unwrap()).unwrap();
    symlink(&outside, record(&f)).unwrap();
    assert_eq!(
        f.install.desktop_rows(&f.proof, &plan).unwrap_err(),
        PayloadError::Foreign
    );
    assert_eq!(
        f.install.desktop_remove(&f.proof, &deadline()).unwrap_err(),
        PayloadError::Foreign
    );
    assert!(
        fs::symlink_metadata(record(&f))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(outside.exists());
}

#[test]
fn a_kde_apply_keeps_the_gnome_extension_leaves_on_the_record() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    apply_desktop(&f, &plan_for(&f, &p, Desktop::Gnome, true), 1, &p);
    // Later the same computer is used from a KDE session: its own proof, its own plan.
    let kde = f.io.scratch_support(facts(&f.io, Desktop::Kde)).unwrap();
    f.install
        .desktop_apply(
            &kde,
            &plan_for(&f, &p, Desktop::Kde, false),
            OperationId(2),
            p.manifest_hash(),
            &deadline(),
        )
        .unwrap();

    assert_eq!(
        sorted(f.install.desktop_recorded(&f.proof).unwrap()),
        sorted(ids())
    );
    // The record still lists the extension leaves, so it is a GNOME record.
    let text = fs::read_to_string(record(&f)).unwrap();
    assert!(text.contains("\"desktop\":\"gnome\""), "{text}");
    for (index, bytes) in desktop_contents().iter().enumerate() {
        assert_eq!(fs::read(target(&f, index)).unwrap(), *bytes);
    }

    let removal = f.install.desktop_remove(&f.proof, &deadline()).unwrap();
    assert_eq!(sorted(removal.removed.clone()), sorted(ids()));
    assert!(removal.absent.is_empty());
    assert!(removal.kept.is_empty());
    assert!(!extension_dir(&f).exists());
    assert!(!record(&f).exists());
}

/// Stage `members` (with `manifest`) through `scripts/installer/stage-linux.sh` and return the
/// stager's `payload.tar`, or its refusal.
fn staged(f: &Fixture, manifest: &Manifest, members: &Parts) -> Result<Vec<u8>, String> {
    let input = f.root.join("stage-input");
    let output = f.root.join("stage-output");
    fs::create_dir(&input).unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in members {
        put(&input.join(name), bytes, 0o600);
    }
    let library = elf(1);
    put(
        &input.join("libraries").join(&manifest.libraries[0].name),
        &library,
        0o600,
    );
    put(
        &input.join("provenance.json"),
        &serde_json::to_vec(manifest).unwrap(),
        0o600,
    );
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let result = std::process::Command::new(project.join("scripts/test-env.sh"))
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
    if result.status.success() {
        Ok(fs::read(output.join("payload.tar")).unwrap())
    } else {
        Err(String::from_utf8_lossy(&result.stderr).into_owned())
    }
}

#[test]
fn the_stager_and_the_reader_agree_on_both_schemas() {
    // Schema 2: the stager accepts the real packaging files and the reader accepts its archive.
    let f = Fixture::on(Desktop::Hyprland);
    let members = members_at(SCHEMA_DESKTOP, 1);
    let manifest = manifest_of(SCHEMA_DESKTOP, &members);
    let bytes = staged(&f, &manifest, &members).unwrap();
    let p = read(&bytes).unwrap();
    assert!(p.has_desktop_files());
    assert_eq!(p.manifest().members.len(), 13);
    assert!(f.install.targets().iter().all(|path| !path.exists()));
    // Schema 1 still stages and reads, with the nine.
    let g = Fixture::on(Desktop::Hyprland);
    let members = members_at(SCHEMA_CORE, 1);
    let manifest = manifest_of(SCHEMA_CORE, &members);
    let p = read(&staged(&g, &manifest, &members).unwrap()).unwrap();
    assert!(!p.has_desktop_files());
    assert_eq!(p.manifest().members.len(), 9);
    // What the reader refuses, the stager refuses first: a half-carried schema 2, a schema 1
    // that lists desktop members, and a metadata.json for another extension.
    let h = Fixture::on(Desktop::Hyprland);
    let mut members = members_at(SCHEMA_DESKTOP, 1);
    members.pop();
    let manifest = manifest_of(SCHEMA_DESKTOP, &members);
    assert!(staged(&h, &manifest, &members).is_err());
    let i = Fixture::on(Desktop::Hyprland);
    let members = members_at(SCHEMA_DESKTOP, 1);
    let manifest = manifest_of(SCHEMA_CORE, &members);
    assert!(staged(&i, &manifest, &members).is_err());
    let j = Fixture::on(Desktop::Hyprland);
    let mut members = members_at(SCHEMA_DESKTOP, 1);
    set(
        &mut members,
        DESKTOP_FILES[2],
        METADATA
            .replace("crosspane@frostdev.io", "other@example.org")
            .into_bytes(),
    );
    let manifest = manifest_of(SCHEMA_DESKTOP, &members);
    assert!(staged(&j, &manifest, &members).is_err());
}

#[test]
fn a_proof_for_one_desktop_never_serves_a_plan_made_for_another() {
    let f = Fixture::on(Desktop::Kde);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    assert_eq!(
        f.install
            .desktop_apply(
                &f.proof,
                &plan,
                OperationId(1),
                p.manifest_hash(),
                &deadline()
            )
            .unwrap_err(),
        PayloadError::Foreign
    );
    assert!(!record(&f).exists());
    for index in 0..LEAVES {
        assert!(!target(&f, index).exists(), "{index}");
    }
}

#[test]
fn operation_zero_refuses_before_any_write() {
    let f = Fixture::on(Desktop::Gnome);
    let p = package(SCHEMA_DESKTOP);
    let plan = plan_for(&f, &p, Desktop::Gnome, true);
    assert_eq!(
        f.install
            .desktop_apply(
                &f.proof,
                &plan,
                OperationId(0),
                p.manifest_hash(),
                &deadline()
            )
            .unwrap_err(),
        PayloadError::Invalid
    );
    assert!(!record(&f).exists());
    for index in 0..LEAVES {
        assert!(!target(&f, index).exists());
    }
}
