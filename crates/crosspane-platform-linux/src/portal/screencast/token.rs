//! The ScreenCast restore-token file.
//!
//! The token is a portal secret: it makes later starts silent. This module only reads and writes
//! the file, under the same rules as the RemoteDesktop session's (`portal::session`): 0600, a
//! temporary file in the same directory renamed over the target, a malformed file is no token.
//! The token's value is never logged.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Restore tokens are short opaque strings; anything larger is not one.
const MAX_TOKEN_BYTES: u64 = 4096;

/// Read the stored token, if there is a usable one. A missing file is the normal first-run case.
/// An unreadable, oversized, non-UTF-8 or otherwise malformed file is logged (never its content)
/// and treated as no token, so consent is simply asked again.
pub(in crate::portal) fn read(path: &Path) -> Option<String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot read the ScreenCast restore token");
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_TOKEN_BYTES + 1).read_to_end(&mut bytes) {
        tracing::warn!(path = %path.display(), %error, "cannot read the ScreenCast restore token");
        return None;
    }
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        tracing::warn!(path = %path.display(), "ignoring an oversized ScreenCast restore token file");
        return None;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        tracing::warn!(path = %path.display(), "ignoring a ScreenCast restore token file that is not UTF-8");
        return None;
    };
    let token = text.trim();
    if token.is_empty() {
        return None;
    }
    if !is_valid(token) {
        tracing::warn!(path = %path.display(), "ignoring a malformed ScreenCast restore token file");
        return None;
    }
    Some(token.to_owned())
}

/// Store `token`: the parent directory is created with mode 0700 if missing, and the file is
/// written (mode 0600) to a temporary name in the same directory, synced, and renamed over `path`,
/// so a reader never sees a partial token.
pub(in crate::portal) fn write(path: &Path, token: &str) -> io::Result<()> {
    if !is_valid(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "malformed restore token",
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "token path has no file name")
    })?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = dir.join(temp_name);

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return result;
    }
    // Make the rename durable. Best effort: the token is replaceable by asking for consent again.
    if let Ok(dir_file) = File::open(&dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Delete the stored token (the grant it stood for is gone). A missing file is fine; any other
/// failure is logged and leaves the file, so the next start tries the same token again.
pub(in crate::portal) fn remove(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot remove the ScreenCast restore token");
        }
    }
}

/// A token is non-empty, bounded and free of control characters (which also rules out NUL).
fn is_valid(token: &str) -> bool {
    !token.is_empty()
        && (token.len() as u64) <= MAX_TOKEN_BYTES
        && !token.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "crosspane-screencast-token-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn missing_file_is_no_token() {
        let dir = TempDir::new();
        assert_eq!(read(&dir.0.join("absent.token")), None);
    }

    #[test]
    fn write_then_read_round_trips_with_private_modes() {
        let dir = TempDir::new();
        let path = dir.0.join("state").join("portal-screencast.token");
        write(&path, "tok-0123456789").unwrap();
        assert_eq!(read(&path).as_deref(), Some("tok-0123456789"));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // A rotation replaces the file and leaves no temporary behind.
        write(&path, "tok-rotated").unwrap();
        assert_eq!(read(&path).as_deref(), Some("tok-rotated"));
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "portal-screencast.token")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn unusable_files_are_ignored_and_bad_tokens_are_not_written() {
        let dir = TempDir::new();
        fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("t.token");
        fs::write(&path, b"\xff\xfe not utf-8").unwrap();
        assert_eq!(read(&path), None, "invalid UTF-8");
        fs::write(&path, "   \n").unwrap();
        assert_eq!(read(&path), None, "blank");
        fs::write(&path, "two\nlines").unwrap();
        assert_eq!(read(&path), None, "control characters");
        fs::write(&path, vec![b'a'; MAX_TOKEN_BYTES as usize + 1]).unwrap();
        assert_eq!(read(&path), None, "oversized");
        fs::write(&path, "  abc-def \n").unwrap();
        assert_eq!(read(&path).as_deref(), Some("abc-def"), "trimmed");

        let fresh = dir.0.join("fresh.token");
        assert!(write(&fresh, "").is_err());
        assert!(write(&fresh, "a\0b").is_err());
        assert!(write(&fresh, "a\nb").is_err());
        assert!(!fresh.exists());
        assert!(write(Path::new("/"), "token").is_err());
    }

    #[test]
    fn remove_deletes_the_file_and_tolerates_a_missing_one() {
        let dir = TempDir::new();
        let path = dir.0.join("state").join("portal-virtual.token");
        remove(&path);
        write(&path, "tok-1").unwrap();
        remove(&path);
        assert!(!path.exists());
        assert_eq!(read(&path), None);
        remove(&path);
    }
}
