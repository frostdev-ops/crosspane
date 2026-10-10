//! The files a GNOME or KDE session needs besides the nine core files, installed, checked and
//! removed on their own record.
//!
//! - **Every GNOME and KDE session:** the agent's desktop entry in the applications folder. The
//!   desktop portals refuse to register the agent's application id without it (and GlobalShortcuts
//!   needs it).
//! - **GNOME only:** the Shell extension's three files in
//!   `<data>/gnome-shell/extensions/crosspane@frostdev.io/`. Turning it on is
//!   `extension.rs`; GNOME loads a newly installed extension at the next log-in.
//! - **Hyprland:** nothing. A Hyprland install does not read, write or record anything here.
//!
//! These leaves are not part of the nine-file journal. They have their own small record
//! (`desktop-outcome.json`, private, atomically replaced) so the journals, ledgers and cleanup
//! order of existing installs stay exactly as they were. The record lists what was *intended*,
//! and is written before any file is placed, so a crash never leaves a file the installer cannot
//! account for. A file is only ever removed if it still holds exactly the bytes the record lists;
//! a file the person changed is theirs and is kept.
//!
//! The installer owns these paths like the others (WP-4.32): whatever stands where a leaf goes
//! (another file, a link, a directory) is moved into the backup folder first, then the leaf is
//! written. Nothing outside these fixed paths is touched.
use super::*;
use crate::platform::linux::{detect::Desktop, extension};

const RECORD: &str = "desktop-outcome.json";
const MODE: u32 = 0o644;

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content checks for the four members of a schema-2 payload, at the point the archive is read.
/// The archive hash already pins the bytes; this keeps an archive from carrying something that
/// isn't what its name says.
pub(super) fn check_member(name: &str, data: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(data).map_err(|_| PayloadError::Invalid)?;
    if data.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    {
        return Err(PayloadError::Invalid);
    }
    let ok = match DESKTOP_FILES.iter().position(|n| *n == name) {
        // A plain desktop entry: no template fields, since nothing renders it.
        Some(0) => {
            text.starts_with("[Desktop Entry]")
                && text.lines().any(|line| line == "Type=Application")
                && !text.contains("{{")
        }
        Some(1) => !text.trim().is_empty(),
        Some(2) => serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|value| value.get("uuid").and_then(|v| v.as_str()) == Some(extension::UUID)),
        Some(3) => text.contains("io.frostdev.Crosspane.Shell1"),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(PayloadError::Invalid)
    }
}

/// Where leaf `index` of [`DESKTOP_FILES`] goes, relative to the data folder.
fn relative(index: usize) -> String {
    if index == 0 {
        "applications/io.frostdev.crosspane.agent.desktop".to_owned()
    } else {
        let file = DESKTOP_FILES[index]
            .rsplit('/')
            .next()
            .unwrap_or(DESKTOP_FILES[index]);
        format!("gnome-shell/extensions/{}/{file}", extension::UUID)
    }
}

/// One file of a desktop plan.
#[derive(Clone, Debug)]
pub struct DesktopFile {
    id: &'static str,
    path: PathBuf,
    bytes: Vec<u8>,
    hash: [u8; 32],
}
impl DesktopFile {
    pub fn id(&self) -> &'static str {
        self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What a session installs from a staged payload. Opaque to callers except for its summary.
#[derive(Clone, Debug)]
pub struct DesktopPlan {
    desktop: Desktop,
    files: Vec<DesktopFile>,
    extension: bool,
}
impl DesktopPlan {
    pub fn desktop(&self) -> Desktop {
        self.desktop
    }
    pub fn files(&self) -> &[DesktopFile] {
        &self.files
    }
    /// Nothing to install (Hyprland).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    /// The Shell extension's files are part of this plan.
    pub fn installs_extension(&self) -> bool {
        self.extension
    }
    /// A sentence for the install preview, or `None` when the plan adds nothing.
    pub fn preview(&self) -> Option<String> {
        if self.files.is_empty() {
            None
        } else if self.extension {
            Some(
                "It also adds Crosspane's desktop entry to your applications folder (the desktop \
                 portals need it) and Crosspane's Shell extension to \
                 ~/.local/share/gnome-shell/extensions, and turns the extension on in your GNOME \
                 settings, keeping every extension you already use. GNOME loads a new extension \
                 at your next log-in."
                    .into(),
            )
        } else {
            Some(
                "It also adds Crosspane's desktop entry to your applications folder, which the \
                 desktop portals need."
                    .into(),
            )
        }
    }
}

/// What a removal did, by leaf id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopRemoval {
    pub removed: Vec<String>,
    pub absent: Vec<String>,
    /// Present but no longer the bytes the installer wrote: the person's, left alone.
    pub kept: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Leaf {
    id: String,
    sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema_version: u32,
    operation: u64,
    desktop: String,
    manifest_sha256: String,
    leaves: Vec<Leaf>,
}
struct Loaded {
    record: Record,
    hash: [u8; 32],
}
impl Loaded {
    /// The hash recorded for `id`, if the record lists it.
    fn hash_of(&self, id: &str) -> Option<[u8; 32]> {
        self.record
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)
            .and_then(|leaf| super::hash(&leaf.sha256).ok())
    }
}

impl PayloadInstaller {
    fn data_home(&self) -> PathBuf {
        self.io.target().paths().data_home.clone()
    }
    /// The fixed target of a leaf id; ids come from [`DESKTOP_FILES`], never from a record.
    fn desktop_target(&self, id: &str) -> Result<PathBuf> {
        let index = DESKTOP_FILES
            .iter()
            .position(|name| *name == id)
            .ok_or(PayloadError::Foreign)?;
        Ok(self.data_home().join(relative(index)))
    }
    fn extension_dir(&self) -> PathBuf {
        self.data_home()
            .join("gnome-shell/extensions")
            .join(extension::UUID)
    }
    fn record_path(&self) -> PathBuf {
        self.state.join(RECORD)
    }

    /// What `desktop` installs from `package`. `extension` is whether the Shell extension should
    /// be installed on GNOME (the caller leaves it out when the Shell is too old for it).
    pub fn desktop_plan(
        &self,
        package: &Package,
        desktop: Desktop,
        extension: bool,
    ) -> Result<DesktopPlan> {
        self.io.validate_target()?;
        let indexes: &[usize] = match desktop {
            Desktop::Hyprland => &[],
            Desktop::Kde => &[0],
            Desktop::Gnome if extension => &[0, 1, 2, 3],
            Desktop::Gnome => &[0],
        };
        if !indexes.is_empty() && !package.has_desktop_files() {
            return Err(PayloadError::NoDesktopFiles);
        }
        let mut files = Vec::new();
        for &index in indexes {
            let id = DESKTOP_FILES[index];
            let bytes = package.files.get(id).ok_or(PayloadError::Invalid)?.clone();
            files.push(DesktopFile {
                id,
                path: self.data_home().join(relative(index)),
                hash: sha256(&bytes),
                bytes,
            });
        }
        Ok(DesktopPlan {
            desktop,
            extension: indexes.len() > 1,
            files,
        })
    }

    /// What `proof`'s desktop installs from `package`: the Shell extension is left out only when
    /// the Shell is known to be older than the extension supports (an unreadable version leaves
    /// that to GNOME, which refuses an extension it cannot load).
    pub fn desktop_plan_for(&self, package: &Package, proof: &SupportProof) -> Result<DesktopPlan> {
        let extension = proof
            .compositor_version()
            .is_none_or(|version| version[0] >= extension::MIN_SHELL);
        self.desktop_plan(package, proof.desktop(), extension)
    }

    /// The rows of the desktop files that are not as the payload says: absent, different, or a
    /// link or odd file in the way. Empty for Hyprland and when everything is in place.
    pub fn desktop_drift(
        &self,
        package: &Package,
        proof: &SupportProof,
    ) -> Result<Vec<ResourceReceipt>> {
        let plan = self.desktop_plan_for(package, proof)?;
        Ok(self
            .desktop_rows(proof, &plan)?
            .into_iter()
            .filter(|row| row.before != ResourceObservation::Matching)
            .collect())
    }

    /// Put `proof`'s desktop files in place and, on GNOME, turn the extension on in the person's
    /// settings. A file that can't be written fails the call. The switch never does: what it
    /// couldn't do, and what the person must do next (log out and in), come back as lines to
    /// show. `session` carries the selected session's bus address, nothing else is used.
    pub fn desktop_install(
        &self,
        package: &Package,
        proof: &SupportProof,
        session: &std::collections::BTreeMap<String, String>,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<Vec<String>> {
        let plan = self.desktop_plan_for(package, proof)?;
        let mut lines = Vec::new();
        if plan.is_empty() {
            return Ok(lines);
        }
        self.desktop_apply(proof, &plan, operation, package.manifest_hash(), deadline)?;
        if plan.installs_extension() {
            // First, so a length bound on the shown text can never cut it off.
            lines.push(extension::NEXT_LOGIN_NOTE.into());
            let switched = extension::GnomeSettings::new(self.io.clone(), session, deadline)
                .and_then(|settings| {
                    settings.enable(proof, deadline)?;
                    settings.read(deadline)
                });
            match switched {
                Ok(reading) if reading.user_extensions_disabled => lines.push(
                    "GNOME has all user extensions switched off, so Crosspane's Shell extension \
                     won't load until you switch them on (in the Extensions app). Crosspane works \
                     without it."
                        .into(),
                ),
                Ok(_) => {}
                Err(_) => lines.push(
                    "Crosspane's Shell extension is installed but couldn't be turned on in your \
                     GNOME settings. Crosspane works without it; turn it on in the Extensions \
                     app after your next log-in."
                        .into(),
                ),
            }
        }
        Ok(lines)
    }

    fn load_record(&self, proof: &SupportProof) -> Result<Option<Loaded>> {
        let Some(snapshot) = self.snapshot(proof, &self.record_path(), 0o600, MAX_RECORD_BYTES)?
        else {
            return Ok(None);
        };
        let record: Record =
            serde_json::from_slice(&snapshot.bytes).map_err(|_| PayloadError::Foreign)?;
        let mut seen = Vec::new();
        if record.schema_version != 1
            || record.operation == 0
            || !matches!(record.desktop.as_str(), "gnome" | "kde")
            || super::hash(&record.manifest_sha256).is_err()
            || record.leaves.is_empty()
            || record.leaves.iter().any(|leaf| {
                let known = DESKTOP_FILES.contains(&leaf.id.as_str());
                let fresh = !seen.contains(&leaf.id);
                seen.push(leaf.id.clone());
                !known || !fresh || super::hash(&leaf.sha256).is_err()
            })
        {
            return Err(PayloadError::Foreign);
        }
        Ok(Some(Loaded {
            record,
            hash: snapshot.hash,
        }))
    }

    /// How each file of `plan` stands, judged the way the core files are: a row per file, with
    /// what is there (`before`) and whether the installer's own record accounts for it.
    pub fn desktop_rows(
        &self,
        proof: &SupportProof,
        plan: &DesktopPlan,
    ) -> Result<Vec<ResourceReceipt>> {
        proof.check(&self.io)?;
        let loaded = self.load_record(proof)?;
        let mut rows = Vec::new();
        for file in &plan.files {
            let (before, current) = match self.snapshot(proof, &file.path, MODE, MAX_RECORD_BYTES) {
                Ok(None) => (ResourceObservation::Absent, None),
                Ok(Some(found)) if found.hash == file.hash => {
                    (ResourceObservation::Matching, Some(found.hash))
                }
                Ok(Some(found)) => (ResourceObservation::Different, Some(found.hash)),
                // A link, a directory or an odd mode: not ours, and replaced (after a backup).
                Err(PayloadError::Foreign) => (ResourceObservation::Different, None),
                Err(error) => return Err(error),
            };
            let accounted = loaded
                .as_ref()
                .and_then(|loaded| loaded.hash_of(file.id))
                .is_some_and(|recorded| Some(recorded) == current);
            rows.push(ResourceReceipt {
                resource_id: file.id.into(),
                resolved_path: file.path.to_string_lossy().into_owned(),
                ownership: if before == ResourceObservation::Absent || accounted {
                    ResourceOwnership::Created
                } else {
                    ResourceOwnership::Foreign
                },
                before,
                after: ResourceObservation::Unknown,
                outcome: MutationOutcome::Unknown,
            });
        }
        Ok(rows)
    }

    /// Move aside whatever stands where the extension's folder goes if it isn't a plain folder.
    fn clear_extension_dir(&self, proof: &SupportProof) -> Result<()> {
        let dir = self.extension_dir();
        let Some((parent, name)) = self.parent(proof, &dir, false)? else {
            return Ok(());
        };
        match rfs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if stat.st_uid == self.io.target().paths().uid
                    && stat.st_mode & 0o170022 == 0o040000 =>
            {
                Ok(())
            }
            Ok(_) => {
                let folder = self.backup_folder(proof)?;
                self.move_aside(proof, &dir, &folder, "desktop-extension-folder")?;
                Ok(())
            }
            Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(_) => Err(PayloadError::Native(NativeError::Unavailable)),
        }
    }

    /// Put one file in place: kept when it is already exactly this, otherwise whatever stands
    /// there is saved in the backup folder and the file is written.
    fn place(&self, proof: &SupportProof, file: &DesktopFile) -> Result<()> {
        match self.snapshot(proof, &file.path, MODE, MAX_RECORD_BYTES) {
            Ok(Some(found)) if found.hash == file.hash => return Ok(()),
            Ok(None) => {}
            Ok(Some(_)) | Err(PayloadError::Foreign) => {
                let folder = self.backup_folder(proof)?;
                let name = file
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(PayloadError::Invalid)?;
                self.move_aside(proof, &file.path, &folder, &format!("desktop-{name}"))?;
            }
            Err(error) => return Err(error),
        }
        self.write_new(proof, &file.path, &file.bytes, MODE)
    }

    /// Install `plan`: the record first, then each file, then a read-back of every file. Safe to
    /// repeat. `operation` and `manifest` only label the record.
    pub fn desktop_apply(
        &self,
        proof: &SupportProof,
        plan: &DesktopPlan,
        operation: OperationId,
        manifest: [u8; 32],
        deadline: &Deadline,
    ) -> Result<()> {
        if plan.files.is_empty() {
            return Ok(());
        }
        if operation.0 == 0 {
            return Err(PayloadError::Invalid);
        }
        deadline.check()?;
        proof.check(&self.io)?;
        // A proof admitted for one desktop never serves a plan made for another.
        if plan.desktop != proof.desktop() {
            return Err(PayloadError::Foreign);
        }
        self.io.validate_target()?;
        self.parent(proof, &self.state.join("install.lock"), true)?
            .ok_or(PayloadError::Foreign)?;
        let _lease = self.io.install_lease(proof)?;
        // What an earlier install listed stays listed, so removal can still account for it.
        let mut leaves: Vec<Leaf> = plan
            .files
            .iter()
            .map(|file| Leaf {
                id: file.id.into(),
                sha256: hex(&file.hash),
            })
            .collect();
        if let Some(earlier) = self.load_record(proof)? {
            leaves.extend(
                earlier
                    .record
                    .leaves
                    .into_iter()
                    .filter(|leaf| !plan.files.iter().any(|file| file.id == leaf.id)),
            );
        }
        // The record names the widest thing it lists: any extension leaf makes it a GNOME record.
        let gnome = plan.desktop == Desktop::Gnome
            || leaves
                .iter()
                .any(|leaf| leaf.id.starts_with("resources/gnome-shell-extension/"));
        let record = Record {
            schema_version: 1,
            operation: operation.0,
            desktop: if gnome { "gnome" } else { "kde" }.into(),
            manifest_sha256: hex(&manifest),
            leaves,
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| PayloadError::Invalid)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(PayloadError::Invalid);
        }
        self.parent(proof, &self.record_path(), true)?
            .ok_or(PayloadError::Foreign)?;
        self.io.atomic_write(proof, &self.record_path(), &bytes)?;
        if plan.extension {
            self.clear_extension_dir(proof)?;
        }
        for file in &plan.files {
            deadline.check()?;
            proof.check(&self.io)?;
            self.place(proof, file)?;
        }
        for file in &plan.files {
            let found = self
                .snapshot(proof, &file.path, MODE, MAX_RECORD_BYTES)?
                .ok_or(PayloadError::Desktop)?;
            if found.hash != file.hash {
                return Err(PayloadError::Desktop);
            }
        }
        Ok(())
    }

    /// Remove one file only if it is still exactly `expected`: the same bytes, and the same file
    /// that was just read.
    fn delete_exact(
        &self,
        proof: &SupportProof,
        path: &Path,
        expected: [u8; 32],
        mode: u32,
    ) -> Result<bool> {
        let Some(found) = self.snapshot(proof, path, mode, MAX_RECORD_BYTES)? else {
            return Ok(false);
        };
        if found.hash != expected {
            return Err(PayloadError::Foreign);
        }
        proof.check(&self.io)?;
        let stat = system(rfs::statat(
            &found.parent,
            &found.name,
            AtFlags::SYMLINK_NOFOLLOW,
        ))?;
        if (stat.st_dev, stat.st_ino) != found.identity {
            return Err(PayloadError::Foreign);
        }
        system(rfs::unlinkat(&found.parent, &found.name, AtFlags::empty()))?;
        self.sync_parent(&found.parent)?;
        Ok(true)
    }

    /// Remove what the record lists and the person hasn't changed; then the extension's folder
    /// when it is empty; then the record. A file that differs is kept and reported, not an error.
    pub fn desktop_remove(
        &self,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> Result<DesktopRemoval> {
        deadline.check()?;
        proof.check(&self.io)?;
        self.io.validate_target()?;
        let mut report = DesktopRemoval::default();
        let Some(loaded) = self.load_record(proof)? else {
            return Ok(report);
        };
        let _lease = self.io.install_lease(proof)?;
        for leaf in &loaded.record.leaves {
            deadline.check()?;
            let path = self.desktop_target(&leaf.id)?;
            let expected = super::hash(&leaf.sha256)?;
            match self.snapshot(proof, &path, MODE, MAX_RECORD_BYTES) {
                Ok(None) => report.absent.push(leaf.id.clone()),
                Ok(Some(found)) if found.hash == expected => {
                    self.delete_exact(proof, &path, expected, MODE)?;
                    report.removed.push(leaf.id.clone());
                }
                Ok(Some(_)) | Err(PayloadError::Foreign) => report.kept.push(leaf.id.clone()),
                Err(error) => return Err(error),
            }
        }
        // The extension's folder goes only when nothing at all is left in it. A folder that
        // still holds anything (the person's files) simply stays.
        if let Some((parent, name)) = self.parent(proof, &self.extension_dir(), false)? {
            let _ = rfs::unlinkat(&parent, &name, AtFlags::REMOVEDIR);
            let _ = rfs::fsync(&parent);
        }
        self.delete_exact(proof, &self.record_path(), loaded.hash, 0o600)?;
        Ok(report)
    }

    /// The leaf ids the installer's record lists, if there is a record. Read only.
    pub fn desktop_recorded(&self, proof: &SupportProof) -> Result<Vec<String>> {
        proof.check(&self.io)?;
        Ok(self
            .load_record(proof)?
            .map(|loaded| {
                loaded
                    .record
                    .leaves
                    .into_iter()
                    .map(|leaf| leaf.id)
                    .collect()
            })
            .unwrap_or_default())
    }
}
