//! Detached-worker payload operations; inert until explicitly called. Inventory and signing
//! requirements are trusted caller inputs, never learned from a payload's own manifest.
//! The frozen native foundation evaluates BSD modes, not macOS ACLs (trusted administrators).
use super::{native_io::*, transport::SelectedAgent};
use crate::agent_contract::{
    AgentReply, BootstrapPhase, DecodedReply, HealthSnapshot, KeyStoreProvenance,
    ObservationSource, StartupRecovery, StatusAdmission, parse_bootstrap,
};
use crosspane_installer_core::{
    InstallReceipt, MutationOutcome, OperationId, ResourceObservation, ResourceOwnership,
    ResourceReceipt, StepId,
};
use crosspane_types::id::NodeId;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const AGENT: &str = "Crosspane.app/Contents/MacOS/Crosspane";
const APP: &str = "Crosspane.app";
const CTL: &str = "crosspanectl";
const INSTALLER: &str = "crosspane-installer";

#[path = "payload/clean_stop.rs"]
mod clean_stop;
#[path = "payload/health.rs"]
mod health;
#[path = "payload/inventory.rs"]
mod inventory;
#[path = "payload/recovery.rs"]
mod recovery;
#[path = "payload/staging.rs"]
mod staging;

pub use clean_stop::{CleanStopGate, OriginalAgent};
use health::Replacement;
pub use health::{PendingPayload, VerifiedPayload};
pub use inventory::{
    ApprovedInventory, MAX_PAYLOAD_BYTES, MAX_PAYLOAD_FILES, PayloadFile, PayloadRole, SigningRule,
};
use inventory::{Tree, directories, hash, renamed_tree, same_object, tree};
pub use recovery::{PayloadPhase, PayloadRecord, RecoveryInventory};
pub use staging::{CliInventory, PayloadConsent, PayloadPlan, PayloadState};

pub struct MacPayload {
    io: Arc<MacNativeIo>,
    approved: ApprovedInventory,
    main: SignatureProof,
    support: SupportProof,
    digest: [u8; 32],
}
macro_rules! opaque {
    ($($name:ident),+ $(,)?) => { $(impl std::fmt::Debug for $name {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($name)) }
    })+ };
}
opaque!(
    OriginalAgent,
    CleanStopGate,
    PayloadPlan,
    PayloadConsent,
    RecoveryInventory,
    PendingPayload,
    VerifiedPayload,
    MacPayload
);

impl MacPayload {
    fn app_stage(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .home
            .join("Applications/.Crosspane.app.crosspane-stage")
    }
    fn app_previous(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .home
            .join("Applications/.Crosspane.app.crosspane-previous")
    }
    fn ctl(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .home
            .join(".local/bin/crosspanectl")
    }
    fn ctl_stage(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .home
            .join(".local/bin/.crosspanectl.crosspane-stage")
    }
    fn ctl_previous(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .home
            .join(".local/bin/.crosspanectl.crosspane-previous")
    }
    fn record_path(&self) -> PathBuf {
        self.io.target().installer_dir().join("payload.json")
    }
    fn parents(&self, path: &Path, deadline: &Deadline) -> NativeResult<()> {
        let home = &self.io.target().paths().home;
        let mut missing = Vec::new();
        let mut cursor = path;
        while self.io.metadata(cursor)?.is_none() {
            if cursor == home || !cursor.starts_with(home) {
                return Err(NativeError::Foreign);
            }
            missing.push(cursor.to_owned());
            cursor = cursor.parent().ok_or(NativeError::Invalid)?;
        }
        for directory in missing.into_iter().rev() {
            self.io
                .create_directory(&self.support, &directory, 0o700, deadline)?;
        }
        Ok(())
    }
}
