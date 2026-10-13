//! `installed.json` (L1): the only persisted installer state, informational and never gating.

use super::{
    error::{SetupError, SetupResult},
    paths::Paths,
    setup_log::setup_note,
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
};

/// The schema this build reads and writes.
const SCHEMA: u32 = 1;
/// A larger file is not ours, so it reads as absent.
const MAX_BYTES: u64 = 256 * 1024;

/// Written last, after a successful converge.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Installed {
    pub schema: u32,
    pub version: String,
    pub installed_at_unix_ms: u64,
    pub files: std::collections::BTreeMap<String, String>,
}

/// A missing, unreadable, corrupt or other-schema file is `None`.
pub(crate) fn read(paths: &Paths) -> Option<Installed> {
    let mut bytes = Vec::new();
    File::open(paths.installed_json())
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    let installed: Installed = serde_json::from_slice(&bytes).ok()?;
    (installed.schema == SCHEMA).then_some(installed)
}

/// Temp file, flushed, then renamed over `installed.json`.
pub(crate) fn write(paths: &Paths, installed: &Installed) -> SetupResult<()> {
    let failed = |what: &str, error: &std::io::Error| {
        // Unit tests skip the log line, so they never write the real setup log.
        #[cfg(not(test))]
        super::error::note_io(what, error);
        #[cfg(test)]
        let _ = (what, error);
        SetupError::after("Setup couldn't save the installed version.")
    };
    let json = serde_json::to_vec_pretty(installed).map_err(|error| {
        setup_note!("installed.json: couldn't encode ({error})");
        SetupError::after("Setup couldn't save the installed version.")
    })?;
    fs::create_dir_all(&paths.installer_state)
        .map_err(|error| failed("create the installer folder", &error))?;
    let target = paths.installed_json();
    let mut temp = target.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let written = File::create(&temp).and_then(|mut file| {
        file.write_all(&json)?;
        file.sync_all()
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(failed("write installed.json", &error));
    }
    fs::rename(&temp, &target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        failed("replace installed.json", &error)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A fresh folder under the worktree's `target\`, never anywhere a real install lives.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target")
            .join("w41e-scratch")
            .join(format!("state-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths_in(root: &Path) -> Paths {
        Paths::at(
            &root.join("local"),
            &root.join("roaming"),
            &root.join("temp"),
        )
        .unwrap()
    }

    fn sample() -> Installed {
        Installed {
            schema: 1,
            version: "1.2.3".to_owned(),
            installed_at_unix_ms: 1_760_000_000_000,
            files: [
                ("crosspane-agent.exe".to_owned(), "ab".repeat(32)),
                ("crosspane-ui.exe".to_owned(), "cd".repeat(32)),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn a_written_record_reads_back_and_replaces_the_old_one() {
        let root = scratch("roundtrip");
        let paths = paths_in(&root);
        assert_eq!(read(&paths), None);
        write(&paths, &sample()).unwrap();
        assert_eq!(read(&paths), Some(sample()));
        let mut newer = sample();
        newer.version = "1.2.4".to_owned();
        newer.installed_at_unix_ms += 1;
        write(&paths, &newer).unwrap();
        assert_eq!(read(&paths), Some(newer));
        // No temp file lingers, and the file sits where the layout says.
        assert!(paths.installed_json().is_file());
        let mut temp = paths.installed_json().into_os_string();
        temp.push(".tmp");
        assert!(!PathBuf::from(temp).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_or_foreign_file_reads_as_none() {
        let root = scratch("corrupt");
        let paths = paths_in(&root);
        fs::create_dir_all(&paths.installer_state).unwrap();
        for bytes in [
            &b"not json at all"[..],
            b"",
            b"{\"schema\":1}",
            b"{\"schema\":1,\"version\":\"1\",\"installed_at_unix_ms\":-5,\"files\":{}}",
            b"{\"schema\":2,\"version\":\"1\",\"installed_at_unix_ms\":5,\"files\":{}}",
        ] {
            fs::write(paths.installed_json(), bytes).unwrap();
            assert_eq!(read(&paths), None, "{}", String::from_utf8_lossy(bytes));
        }
        // An oversized file is not ours either.
        fs::write(paths.installed_json(), vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read(&paths), None);
        // A good record over a corrupt one is fine.
        write(&paths, &sample()).unwrap();
        assert_eq!(read(&paths), Some(sample()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writing_fails_plainly_when_the_folder_cannot_be_made() {
        let root = scratch("blocked");
        let paths = paths_in(&root);
        fs::create_dir_all(paths.installer_state.parent().unwrap()).unwrap();
        // A file where the Installer folder belongs.
        fs::write(&paths.installer_state, b"in the way").unwrap();
        let error = write(&paths, &sample()).unwrap_err();
        assert!(error.changed);
        assert!(!error.text.is_empty());
        assert_eq!(read(&paths), None);
        let _ = fs::remove_dir_all(&root);
    }
}
