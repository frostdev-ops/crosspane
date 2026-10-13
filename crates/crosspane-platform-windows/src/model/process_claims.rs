//! Window-process claims: which processes own a currently admitted window, and when each was
//! created. Projected-app audio arrives as bare PIDs; the capture binds every PID to the process
//! identity the window source verified while one of that process's windows existed, so a reused
//! PID never redirects capture to an unrelated process.
//!
//! A claim is keyed by PID and holds the process creation time (a FILETIME as `u64`) and the set
//! of live windows of that process. Windows are keyed by the window source's opaque per-lifetime
//! window id (`WindowId.0`), one per observed HWND lifetime. The pure bookkeeping is testable on
//! every OS; only the process-wide registry below is `cfg(windows)`.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Claim {
    created: u64,
    windows: BTreeSet<u64>,
}

/// The claims of all currently admitted windows. A PID with no live window has no claim.
#[derive(Debug, Default)]
pub struct ProcessClaims {
    pids: BTreeMap<u32, Claim>,
    windows: BTreeMap<u64, u32>,
}

impl ProcessClaims {
    pub const fn new() -> Self {
        Self {
            pids: BTreeMap::new(),
            windows: BTreeMap::new(),
        }
    }

    /// Records that `window` belongs to process `pid` created at `created`, as verified by the
    /// window source while the window existed. A different creation time for a claimed PID means
    /// the PID was reused: the old claim, with all its windows, is replaced. Zero values are
    /// never a valid identity and only drop any claim `window` had.
    pub fn record(&mut self, window: u64, pid: u32, created: u64) {
        if window == 0 || pid == 0 || created == 0 {
            self.remove(window);
            return;
        }
        if self.windows.get(&window) != Some(&pid) {
            self.remove(window);
        }
        match self.pids.get_mut(&pid) {
            Some(claim) if claim.created == created => {
                claim.windows.insert(window);
            }
            _ => {
                if let Some(old) = self.pids.remove(&pid) {
                    for stale in old.windows {
                        self.windows.remove(&stale);
                    }
                }
                self.pids.insert(
                    pid,
                    Claim {
                        created,
                        windows: BTreeSet::from([window]),
                    },
                );
            }
        }
        self.windows.insert(window, pid);
    }

    /// The window went away. Its process loses its claim once it has no live window left.
    pub fn remove(&mut self, window: u64) {
        if let Some(pid) = self.windows.remove(&window)
            && let Some(claim) = self.pids.get_mut(&pid)
        {
            claim.windows.remove(&window);
            if claim.windows.is_empty() {
                self.pids.remove(&pid);
            }
        }
    }

    /// Whether capture may bind `pid`: a claim with at least one live window exists and the
    /// process now behind the PID was created exactly when the claimed one was.
    pub fn admits(&self, pid: u32, created: u64) -> bool {
        pid != 0
            && created != 0
            && self
                .pids
                .get(&pid)
                .is_some_and(|claim| claim.created == created && !claim.windows.is_empty())
    }
}

#[cfg(windows)]
static CLAIMS: std::sync::Mutex<ProcessClaims> = std::sync::Mutex::new(ProcessClaims::new());

/// Records a verified window identity in the process-wide registry (the window source).
#[cfg(windows)]
pub fn record_claim(window: u64, pid: u32, created: u64) {
    if let Ok(mut claims) = CLAIMS.lock() {
        claims.record(window, pid, created);
    }
}

/// Drops a window from the process-wide registry (the window source).
#[cfg(windows)]
pub fn remove_claim(window: u64) {
    if let Ok(mut claims) = CLAIMS.lock() {
        claims.remove(window);
    }
}

/// The admission decision against the process-wide registry; a poisoned registry refuses.
#[cfg(windows)]
pub fn claim_admits(pid: u32, created: u64) -> bool {
    CLAIMS
        .lock()
        .is_ok_and(|claims| claims.admits(pid, created))
}
