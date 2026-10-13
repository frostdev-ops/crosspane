//! The SUPPORT step's checks: own session, fixed folders, approved inventory, payload presence.

use std::path::Path;

/// The verdict plus the checklist rows shown under it.
#[derive(Debug)]
pub(crate) struct Support {
    pub supported: bool,
    pub reason: Option<String>,
    pub checks: Vec<crate::live::SupportCheck>,
}

/// `identity::native::current()` (`NOT_OWN_SESSION`), `Paths::current()`, `Inventory::embedded()`,
/// payload presence (a note only).
pub(crate) fn check(payload: Option<&Path>) -> Support {
    let _ = payload;
    Support {
        supported: false,
        reason: Some("not built".to_owned()),
        checks: Vec::new(),
    }
}
