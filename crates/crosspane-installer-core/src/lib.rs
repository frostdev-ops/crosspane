//! OS-free installer flow, live evidence, and data-only installation receipts.
pub mod elevated;
mod evidence;
mod flow;
mod receipt;

pub use evidence::*;
pub use flow::*;
pub use receipt::*;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepId(pub u16);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptId(pub u64);
