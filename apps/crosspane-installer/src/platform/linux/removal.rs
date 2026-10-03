//! Immutable repair/removal inventory and consent. Execution, durable resume and cleanup-only
//! admission belong to WP-4.19b. Receipts never grant ownership or recover tutorial readiness.
mod authority;
pub mod executor;
mod inventory;
mod plan;
use super::{native_io::*, payload::PayloadError, service::ServiceError};
use crate::agent_contract::ContractError;
pub use authority::{CleanAuthority, TrackedAgent, admit_erase_output};
pub use inventory::{ActivityFacts, Inventory, InventoryFacts, InventoryRequest, RemovalPlanner};
pub use plan::{
    CleanupForm, IdentityChoice, PlanKind, RemovalConsent, RemovalPlan, RemovalSelection,
    ResourceAction,
};
type Result<T> = std::result::Result<T, RemovalError>;
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RemovalError {
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("invalid selection or exhausted operation/revision")]
    Invalid,
    #[error("retired or changed preview; detect and renew consent")]
    Stale,
    #[error("matching clean exit of the originally admitted process is unproved")]
    NotClean,
}
