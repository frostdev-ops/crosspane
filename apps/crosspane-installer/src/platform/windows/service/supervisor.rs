//! Pure lifecycle decisions. Process and receipt facts are observations, not native authority.

use super::super::native_io::{NativeError, NativeResult};

pub const BACKOFF_MS: u64 = 5000;
pub const WINDOW_MS: u64 = 600_000;
pub const MAX_RESTARTS: usize = 3;

#[cfg(windows)]
pub(super) fn native_entry(_trusted: &super::TrustedImages) -> NativeResult<super::SupervisorExit> {
    // Lead750c0da2: fixed-role admission belongs to A4. Never launch from the dummy proof,
    // current bytes, argv or a journal; real suspended-child/job adapters remain Unsupported.
    Err(NativeError::Unsupported)
}

/// Portable startup ordering fixture only. No fake child/token can enter production adapters.
#[cfg(test)]
pub trait StartupPort {
    type Child;
    fn create_suspended(&mut self) -> NativeResult<Self::Child>;
    fn assign(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn resume(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn cleanup_never_resumed(&mut self, child: Self::Child) -> NativeResult<()>;
}
#[cfg(test)]
pub fn startup(port: &mut impl StartupPort) -> NativeResult<()> {
    let child = port.create_suspended()?;
    if let Err(error) = port.assign(&child) {
        // Only the exact owned never-resumed child is eligible for startup cleanup.
        return match port.cleanup_never_resumed(child) {
            Ok(()) => Err(error),
            Err(_) => Err(NativeError::OutcomeUnknown),
        };
    }
    match port.resume(&child) {
        Ok(()) => Ok(()),
        // Ambiguous ResumeThread dispatch cannot prove this child was never resumed.
        Err(NativeError::OutcomeUnknown) => Err(NativeError::OutcomeUnknown),
        Err(error) => match port.cleanup_never_resumed(child) {
            Ok(()) => Err(error),
            Err(_) => Err(NativeError::OutcomeUnknown),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub pid: u32,
    pub creation: u64,
    pub instance: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub instance: u64,
    pub clean: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Original {
    Running,
    Exited { code: u32, receipt: Option<Receipt> },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    Empty,
    Replacement { child: Generation, admitted: bool },
    Uncertain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Observe,
    SubmitStop { expected_instance: u64 },
    Follow(Generation),
    Backoff { until_ms: u64 },
    Start,
    Finished,
    RecoveryRetained,
}

/// Persistence precedes one terminal RPC. Its result is submission only, never completion.
#[allow(dead_code)] // Production selected-operation caller held until A4; fakes exercise ordering.
pub(crate) trait StopPort {
    fn persist_stop(&mut self, expected_instance: u64) -> NativeResult<()>;
    fn submit_stop(&mut self, expected_instance: u64) -> NativeResult<()>;
}
#[allow(dead_code)] // No production operation constructor is fabricated before A4 integration.
pub(crate) fn request_stop(
    model: &mut Supervisor,
    port: &mut impl StopPort,
    expected_instance: u64,
) -> NativeResult<()> {
    if model.stop(expected_instance)? != (Decision::SubmitStop { expected_instance }) {
        return Ok(());
    }
    // The in-process latch is already set even if publication or dispatch fails/panics.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        port.persist_stop(expected_instance)?;
        port.submit_stop(expected_instance)
    }));
    match result {
        Ok(result) => result,
        Err(_) => {
            model.ambiguous();
            Err(NativeError::OutcomeUnknown)
        }
    }
}

#[cfg(windows)]
// Future A4 integration supplies the admitted operation and opaque image prerequisite. Keeping
// this private adapter unavailable is intentional; it must not mint a new production constructor.
#[allow(dead_code)]
pub(crate) struct NativeStop<'a> {
    pub(crate) io: &'a super::super::native_io::WindowsNativeIo,
    pub(crate) proof: &'a super::super::native_io::SupportProof,
    pub(crate) lock: &'a super::super::native_io::InstallerLock,
    pub(crate) journal: &'a mut super::journal::Journal,
    pub(crate) port: Option<super::super::transport::WindowsAgentPort>,
    pub(crate) deadline: &'a super::super::native_io::Deadline,
    pub(crate) timeout_ms: u64,
}
#[cfg(windows)]
impl StopPort for NativeStop<'_> {
    fn persist_stop(&mut self, expected_instance: u64) -> NativeResult<()> {
        let current = super::journal::Journal::read(self.io, self.proof, self.deadline)?
            .ok_or(NativeError::Missing)?;
        if current != *self.journal
            || current
                .current
                .is_none_or(|generation| generation.instance != expected_instance)
        {
            return Err(NativeError::Foreign);
        }
        self.journal.phase = super::journal::Phase::StopIntent;
        self.journal.stop_instance = Some(expected_instance);
        self.journal
            .publish(self.io, self.proof, self.lock, self.deadline)
    }
    fn submit_stop(&mut self, expected_instance: u64) -> NativeResult<()> {
        let port = self.port.take().ok_or(NativeError::Unsupported)?;
        port.installer_stop(expected_instance, self.timeout_ms)
            .map(|_| ())
            .map_err(|error| match error {
                crate::agent_contract::CallFailure::TimeoutOutcomeUnknown => {
                    NativeError::OutcomeUnknown
                }
                crate::agent_contract::CallFailure::Refused(_) => NativeError::Foreign,
                _ => NativeError::Unavailable,
            })
    }
}

pub struct Supervisor {
    selected: Generation,
    stop_latched: bool,
    initial_start_done: bool,
    start_requested: bool,
    retired: bool,
    restart_times: Vec<u64>,
    backoff: Option<u64>,
    last_ms: u64,
}
impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Supervisor")
    }
}
impl Supervisor {
    pub fn new(selected: Generation, now_ms: u64, restart_times: Vec<u64>) -> NativeResult<Self> {
        if selected.pid == 0
            || selected.creation == 0
            || selected.instance == 0
            || restart_times.len() > MAX_RESTARTS
            || restart_times.iter().any(|time| *time > now_ms)
            || restart_times.windows(2).any(|pair| pair[0] > pair[1])
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            selected,
            stop_latched: false,
            initial_start_done: false,
            start_requested: false,
            retired: false,
            restart_times,
            backoff: None,
            last_ms: now_ms,
        })
    }
    pub fn stop_latched(&self) -> bool {
        self.stop_latched
    }
    pub(super) fn running(&mut self) {
        // A fresh matching running observation discharges the initial operation's start.
        // It does not represent a pending crash-replacement dispatch.
        self.initial_start_done = true;
    }
    pub fn stop(&mut self, expected_instance: u64) -> NativeResult<Decision> {
        if expected_instance != self.selected.instance {
            return Err(NativeError::Foreign);
        }
        if self.retired {
            return Err(NativeError::Unsupported);
        }
        if self.stop_latched {
            return Ok(Decision::Observe);
        }
        // This precedes returning SubmitStop: an ACK, timeout or panic cannot unlatch it.
        self.stop_latched = true;
        self.backoff = None;
        Ok(Decision::SubmitStop { expected_instance })
    }
    pub fn observe(&mut self, now_ms: u64, original: Original, tree: Tree) -> Decision {
        if self.retired || now_ms < self.last_ms {
            return self.retain();
        }
        self.last_ms = now_ms;
        let Original::Exited { code, receipt } = original else {
            return Decision::Observe;
        };
        let clean = receipt
            .is_some_and(|receipt| receipt.clean && receipt.instance == self.selected.instance);
        if receipt.is_some_and(|receipt| receipt.instance != self.selected.instance) {
            return self.retain();
        }
        match tree {
            Tree::Uncertain => return self.retain(),
            Tree::Replacement { child, admitted } => {
                if self.stop_latched
                    || !admitted
                    || code != 0
                    || !clean
                    || child.pid == 0
                    || child.creation == 0
                    || child.instance == 0
                    || child.creation == self.selected.creation
                    || child.instance == self.selected.instance
                {
                    return self.retain();
                }
                self.selected = child;
                self.initial_start_done = true;
                self.start_requested = false;
                self.backoff = None;
                return Decision::Follow(child);
            }
            Tree::Empty => {}
        }
        if code == 0 {
            if !clean {
                return self.retain();
            }
            self.retired = true; // Final completion can never become a later Start operation.
            return Decision::Finished;
        }
        if self.stop_latched {
            return self.retain();
        }
        if self.start_requested {
            return Decision::Observe;
        }
        // Native nonzero exit is a known crash, not a cleanup proof. Old recovery material stays.
        self.restart_times
            .retain(|time| now_ms.saturating_sub(*time) < WINDOW_MS);
        if self.restart_times.len() >= MAX_RESTARTS {
            return self.retain();
        }
        let until = match self.backoff {
            Some(until) => until,
            None => {
                let Some(until) = now_ms.checked_add(BACKOFF_MS) else {
                    return self.retain();
                };
                self.backoff = Some(until);
                until
            }
        };
        if now_ms < until {
            return Decision::Backoff { until_ms: until };
        }
        self.restart_times.push(now_ms);
        self.initial_start_done = true;
        self.start_requested = true;
        self.backoff = None;
        Decision::Start
    }
    pub fn start_once(&mut self) -> Decision {
        if self.retired || self.stop_latched {
            return Decision::RecoveryRetained;
        }
        if self.initial_start_done || self.start_requested {
            return Decision::Observe;
        }
        self.initial_start_done = true;
        self.start_requested = true;
        Decision::Start
    }
    pub fn ambiguous(&mut self) {
        self.retired = true;
    }
    pub fn restart_times(&self) -> &[u64] {
        &self.restart_times
    }
    fn retain(&mut self) -> Decision {
        self.retired = true;
        Decision::RecoveryRetained
    }
}
