use crate::{OperationId, StepId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceOwnership {
    Created,
    Adopted,
    Foreign,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceObservation {
    Absent,
    Matching,
    Different,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationOutcome {
    Verified,
    Refused,
    Failed,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReceipt {
    pub resource_id: String,
    pub resolved_path: String,
    pub ownership: ResourceOwnership,
    pub before: ResourceObservation,
    pub after: ResourceObservation,
    pub outcome: MutationOutcome,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReceipt {
    pub schema_version: u32,
    pub operation_id: OperationId,
    pub product_version: String,
    pub manifest_sha256: [u8; 32],
    pub payload_sha256: [u8; 32],
    pub resources: Vec<ResourceReceipt>,
    pub unfinished: Vec<StepId>,
}
