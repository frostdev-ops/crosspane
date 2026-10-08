//! Owned VM probe for the elevated setup kit (WP-W4.1c2 T26, lead row V6). It works only under one
//! fresh directory that it creates in `%TEMP%`: it places four fixture files, verifies them,
//! refuses a tampered byte, and repairs it. It holds the placed files and checks that they can't
//! be replaced or deleted while held (L17), then removes the kit (B2). It never elevates, touches
//! the registry, or installs a driver. The probe is `#[ignore]`; the VM run uses
//! `-- --ignored --nocapture`.
#![cfg(windows)]
#![allow(dead_code, unused_imports)] // Source-included seams; the probe uses only the kit path.

#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/elevated_kit.rs"]
mod elevated_kit;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer::agent_contract;
use crosspane_installer_core::elevated::{
    DRIVER_DIRECTORY, HELPER_IMAGE,
    kit::{KitDocument, KitFile, KitManifest},
};
use native_io::NativeError;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// The staging sibling suffix that `elevated_kit` writes before each rename.
const STAGING_SUFFIX: &str = ".crosspane-new";

/// Distinct owned fixture bytes for each kit file. They are text, not binaries.
fn fixture_bytes(file: KitFile) -> Vec<u8> {
    format!(
        "crosspane-w41c2 owned probe fixture: {}\r\n",
        file_tag(file)
    )
    .into_bytes()
}

/// The inventory name of one kit file: the kebab-case serde value of `KitFile` (plan C4).
fn file_tag(file: KitFile) -> &'static str {
    match file {
        KitFile::Helper => "helper",
        KitFile::DriverInf => "driver-inf",
        KitFile::DriverCatalog => "driver-catalog",
        KitFile::DriverBinary => "driver-binary",
    }
}

/// Lowercase hex SHA-256 of `bytes`, the text of an inventory `sha256` field.
fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Admits the inventory for `fixtures` from its JSON text: `schema_version` 1 and one `files`
/// entry per kit file, each with `file`, `size` and `sha256`.
fn admit(fixtures: &[(KitFile, Vec<u8>)]) -> anyhow::Result<KitManifest> {
    let entries: Vec<String> = fixtures
        .iter()
        .map(|(file, bytes)| {
            format!(
                r#"{{"file":"{}","size":{},"sha256":"{}"}}"#,
                file_tag(*file),
                bytes.len(),
                sha256_hex(bytes)
            )
        })
        .collect();
    let text = format!(r#"{{"schema_version":1,"files":[{}]}}"#, entries.join(","));
    let document: KitDocument = serde_json::from_str(&text)?;
    KitManifest::admit(document).map_err(|error| anyhow::anyhow!("inventory refused: {error:?}"))
}

/// The on-disk path of one kit file under `root`, joined as `elevated_kit` joins it.
fn kit_path(root: &Path, file: KitFile) -> PathBuf {
    file.components()
        .iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The one directory this probe created. Dropping it removes exactly that tree, on success or on
/// failure.
#[derive(Debug)]
struct ProbeDir(PathBuf);

impl Drop for ProbeDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(self.0.as_path()) {
            Ok(()) => println!("cleanup: removed {}", self.0.display()),
            Err(error) => println!("cleanup: {} not removed: {error}", self.0.display()),
        }
    }
}

/// Creates a fresh directory under `std::env::temp_dir()`. An existing name is refused.
fn probe_dir() -> anyhow::Result<ProbeDir> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!(
        "crosspane-w41c2-kit-{}-{nanos}",
        std::process::id()
    ));
    if fs::symlink_metadata(&path).is_ok() {
        anyhow::bail!("refusing to reuse existing {}", path.display());
    }
    fs::create_dir(&path)?;
    Ok(ProbeDir(path))
}

/// The `*.crosspane-new` staging leftovers directly under each of `dirs`.
fn staging_leftovers(dirs: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for dir in dirs {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .ends_with(STAGING_SUFFIX)
            {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

/// Whether `path` is positively absent. Any other metadata error counts as present, so an
/// assertion never passes on an unreadable path.
fn is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// The owned probe. Each step prints one line, so the VM log reads top to bottom.
#[test]
#[ignore = "owned VM probe for WP-W4.1c2 V6; run with `-- --ignored --nocapture`"]
fn owned_kit_probe() -> anyhow::Result<()> {
    let root = probe_dir()?;
    let src = root.0.join("src");
    let install = root.0.join("install");
    fs::create_dir(&src)?;
    fs::create_dir(src.join(DRIVER_DIRECTORY))?;
    fs::create_dir(&install)?;
    println!("step 1: created {} with src and install", root.0.display());

    let fixtures: Vec<(KitFile, Vec<u8>)> = KitFile::ALL
        .iter()
        .map(|file| (*file, fixture_bytes(*file)))
        .collect();
    for (file, bytes) in &fixtures {
        fs::write(kit_path(&src, *file), bytes)?;
    }
    println!(
        "step 2: wrote {} owned fixture files under src",
        fixtures.len()
    );

    let manifest = admit(&fixtures)?;
    println!(
        "step 3: admitted the inventory from JSON with {} pins",
        KitFile::ALL.len()
    );

    let sources = elevated_kit::read_sources(&src, &manifest)?;
    println!("step 4a: read_sources ok for the four fixture files");
    assert_eq!(
        elevated_kit::verify_placed(&install, &manifest).err(),
        Some(NativeError::Missing),
        "verify_placed before placement must be Missing"
    );
    println!("step 4b: verify_placed before placement is Missing");
    let helper = install.join(HELPER_IMAGE);
    let image = elevated_kit::ensure(&install, &manifest, Some(&sources))?;
    assert_eq!(image, helper, "ensure must return the helper image path");
    println!(
        "step 4c: ensure placed the kit and returned {}",
        image.display()
    );
    assert_eq!(elevated_kit::verify_placed(&install, &manifest)?, helper);
    println!("step 4d: verify_placed ok after placement");
    assert_eq!(elevated_kit::ensure(&install, &manifest, None)?, helper);
    println!("step 4e: ensure without sources accepts the placed kit");

    let driver_binary = kit_path(&install, KitFile::DriverBinary);
    let mut tampered = fs::read(&driver_binary)?;
    let index = tampered.len() / 2;
    tampered[index] ^= 0x01;
    fs::write(&driver_binary, tampered)?;
    println!(
        "step 5a: flipped one byte of {} (length kept)",
        driver_binary.display()
    );
    assert_eq!(
        elevated_kit::verify_placed(&install, &manifest).err(),
        Some(NativeError::Foreign),
        "verify_placed must refuse the tampered byte"
    );
    println!("step 5b: verify_placed refuses the tampered byte as Foreign");
    assert_eq!(
        elevated_kit::ensure(&install, &manifest, None).err(),
        Some(NativeError::Foreign),
        "ensure without sources must refuse the tampered byte"
    );
    println!("step 5c: ensure without sources refuses the tampered byte as Foreign");
    assert_eq!(
        elevated_kit::ensure(&install, &manifest, Some(&sources))?,
        helper
    );
    println!("step 5d: ensure with sources repaired the tampered file");
    assert_eq!(
        fs::read(driver_binary)?,
        fixture_bytes(KitFile::DriverBinary),
        "the repaired file must match its pinned bytes"
    );
    assert_eq!(elevated_kit::verify_placed(&install, &manifest)?, helper);
    println!("step 5e: the repaired kit matches its pins again; verify_placed ok");

    let source_binary = kit_path(&src, KitFile::DriverBinary);
    let mut flipped = fs::read(&source_binary)?;
    let source_index = flipped.len() / 2;
    flipped[source_index] ^= 0x01;
    fs::write(&source_binary, flipped)?;
    assert_eq!(
        elevated_kit::read_sources(&src, &manifest).err(),
        Some(NativeError::Foreign),
        "read_sources must refuse a source with one flipped byte"
    );
    println!("step 6a: read_sources refuses a source with one flipped byte as Foreign");
    fs::write(source_binary, fixture_bytes(KitFile::DriverBinary))?;
    elevated_kit::read_sources(&src, &manifest)?;
    println!("step 6b: the restored source reads again");

    let relative = Path::new(r"crosspane-w41c2-relative\src");
    assert_eq!(
        elevated_kit::read_sources(relative, &manifest).err(),
        Some(NativeError::Invalid),
        "read_sources must refuse a folder that is not a drive path"
    );
    println!("step 7: read_sources refuses a relative folder as Invalid");

    let leftovers = staging_leftovers(&[install.clone(), install.join(DRIVER_DIRECTORY)])?;
    assert!(
        leftovers.is_empty(),
        "staging leftovers remain: {leftovers:?}"
    );
    println!("step 8: no *.crosspane-new files remain");

    let (held_image, hold) = elevated_kit::hold_placed(&install, &manifest)?;
    assert_eq!(
        held_image, helper,
        "hold_placed must return the helper image path"
    );
    println!("step 9a: hold_placed holds the four placed files and returns the helper image");
    let held_driver_binary = kit_path(&install, KitFile::DriverBinary);
    let swap = root.0.join("swap.bin");
    fs::write(&swap, b"crosspane-w41c2 owned probe swap bytes\r\n")?;
    assert!(
        fs::rename(swap, &helper).is_err(),
        "rename over the held helper image must be refused"
    );
    assert!(
        fs::OpenOptions::new().write(true).open(&helper).is_err(),
        "a write open of the held helper image must be refused"
    );
    assert!(
        fs::remove_file(&held_driver_binary).is_err(),
        "deleting the held driver binary must be refused"
    );
    println!("step 9b: while held, rename over the helper, write open and delete are refused");
    drop(hold);
    let moved_helper = root.0.join("helper-moved.bin");
    fs::rename(&helper, &moved_helper)?;
    fs::rename(moved_helper, &helper)?;
    let moved_binary = root.0.join("driver-binary-moved.bin");
    fs::rename(&held_driver_binary, &moved_binary)?;
    fs::rename(moved_binary, &held_driver_binary)?;
    assert_eq!(elevated_kit::verify_placed(&install, &manifest)?, helper);
    println!("step 9c: after the hold is dropped both files move out and back; the kit verifies");

    let driver_dir = install.join(DRIVER_DIRECTORY);
    let extra = driver_dir.join("extra-owned.txt");
    fs::write(&extra, b"crosspane-w41c2 owned probe extra file\r\n")?;
    assert_eq!(
        elevated_kit::remove_placed(&install).err(),
        Some(NativeError::Foreign),
        "remove_placed must refuse a driver directory with an extra file"
    );
    assert_eq!(
        elevated_kit::verify_placed(&install, &manifest)?,
        helper,
        "a refused removal must leave the kit in place"
    );
    fs::remove_file(extra)?;
    println!(
        "step 10a: remove_placed refuses an extra file in driver as Foreign and removes nothing"
    );

    let staged = helper.with_file_name(format!("{HELPER_IMAGE}{STAGING_SUFFIX}"));
    fs::write(&staged, b"crosspane-w41c2 owned probe staging leftover\r\n")?;
    elevated_kit::remove_placed(&install)?;
    assert!(is_absent(&helper), "the helper image must be removed");
    assert!(is_absent(&staged), "the staging sibling must be removed");
    assert!(is_absent(&kit_path(&install, KitFile::DriverInf)));
    assert!(is_absent(&kit_path(&install, KitFile::DriverCatalog)));
    assert!(
        is_absent(&held_driver_binary),
        "the driver binary must be removed"
    );
    assert!(
        is_absent(&driver_dir),
        "the driver directory must be removed"
    );
    assert!(install.is_dir(), "install itself must stay");
    println!(
        "step 10b: remove_placed removed the kit files, a staging sibling and driver; install stays"
    );
    elevated_kit::remove_placed(&install)?;
    println!("step 10c: remove_placed on the removed kit succeeds (absent entries are fine)");

    println!("step 11: removing the probe directory");
    Ok(())
}
