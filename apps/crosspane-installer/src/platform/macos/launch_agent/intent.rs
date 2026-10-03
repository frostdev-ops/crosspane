//! Same-filesystem atomic intent/plist publication and retained private prior state.
use super::*;
pub fn render_plist(target: &MacTarget) -> NativeResult<Vec<u8>> {
    let home = target.paths().home.to_str().ok_or(NativeError::Invalid)?;
    let mut escaped = String::new();
    for c in home.chars() {
        escaped.push_str(match c {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&apos;",
            _ => {
                escaped.push(c);
                continue;
            }
        });
    }
    let bytes = TEMPLATE.replace("__HOME__", &escaped).into_bytes();
    if bytes.len() > LIMIT {
        return Err(NativeError::Oversize);
    }
    Ok(bytes)
}
impl MacLaunchAgent {
    pub(super) fn plist(io: &MacNativeIo) -> PathBuf {
        io.target()
            .paths()
            .home
            .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist")
    }
    pub(super) fn record(io: &MacNativeIo) -> PathBuf {
        io.target().installer_dir().join("launch-agent.json")
    }
    pub(super) fn preflight(&self, deadline: &Deadline) -> NativeResult<()> {
        for path in [
            self.io.target().installer_dir(),
            Self::plist(&self.io)
                .parent()
                .ok_or(NativeError::Invalid)?
                .to_owned(),
            self.io.target().paths().home.join("Library/Logs/Crosspane"),
        ] {
            self.parent_directories(&path, &self.support, deadline, false)?;
        }
        Ok(())
    }
    pub(super) fn parents(
        &self,
        path: &Path,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        self.parent_directories(path, proof, deadline, true)
    }
    fn parent_directories(
        &self,
        path: &Path,
        proof: &SupportProof,
        deadline: &Deadline,
        create: bool,
    ) -> NativeResult<()> {
        let mut missing = Vec::new();
        let mut cursor = path;
        while self.io.metadata(cursor)?.is_none() {
            if !cursor.starts_with(&self.io.target().paths().home) {
                return Err(NativeError::Foreign);
            }
            missing.push(cursor.to_owned());
            cursor = cursor.parent().ok_or(NativeError::Invalid)?;
        }
        self.io.entries(cursor, 4096, deadline)?;
        if !create {
            return Ok(());
        }
        for directory in missing.into_iter().rev() {
            self.io
                .create_directory(proof, &directory, 0o700, deadline)?;
        }
        Ok(())
    }
    pub(super) fn persist(
        &self,
        pending: &PendingLaunch,
        io: &MacNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let applied = pending.requested || pending.phase == LaunchPhase::Published;
        let record = Record {
            phase: pending.phase,
            stop_attempted: pending.stop_attempted,
            session: pending.plan.session.clone(),
            baseline: pending.plan.baseline,
            prior: pending
                .prior
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            receipt: InstallReceipt {
                schema_version: 1,
                operation_id: OperationId(pending.plan.operation),
                product_version: self.version.clone(),
                manifest_sha256: self.payload.manifest_sha256(),
                payload_sha256: digest(&self.xml),
                resources: vec![ResourceReceipt {
                    resource_id: "mac.launch-agent".into(),
                    resolved_path: Self::plist(io).to_string_lossy().into_owned(),
                    ownership: if pending.plan.snapshot.identity.is_some() {
                        ResourceOwnership::Adopted
                    } else {
                        ResourceOwnership::Created
                    },
                    before: if pending.plan.snapshot.identity.is_some() {
                        ResourceObservation::Different
                    } else {
                        ResourceObservation::Absent
                    },
                    after: if applied {
                        ResourceObservation::Matching
                    } else {
                        ResourceObservation::Unknown
                    },
                    outcome: MutationOutcome::Unknown,
                }],
                unfinished: vec![StepId(12)],
            },
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| NativeError::Invalid)?;
        let path = Self::record(io);
        io.atomic_write(proof, &path, &bytes, io.metadata(&path)?.as_ref(), deadline)
            .map(|_| ())
    }
}
pub(super) fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0; 32];
    result.copy_from_slice(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref());
    result
}
