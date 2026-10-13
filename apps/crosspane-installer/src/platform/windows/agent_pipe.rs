//! The agent's named-pipe client: `call`, `installer_stop`, and the GUI's `AgentPort`.

use super::paths::Paths;
use crate::agent_contract::{
    AgentCall, AgentPort, AgentReply, CallFailure, DecodedReply, InstallerRequest,
};
use std::{path::PathBuf, time::Duration};

/// One decoded reply with its raw bytes and the pid of the server that sent it.
#[derive(Debug)]
pub(crate) struct Answer {
    pub reply: DecodedReply,
    pub raw: Vec<u8>,
    pub server_pid: u32,
}

impl Answer {
    /// `raw["result"]["installer"]["instance"]["id"]` (works for Pending replies too).
    pub(crate) fn instance(&self) -> Option<u64> {
        None
    }
}

/// The located pipe of this user's installed agent.
#[derive(Debug)]
pub(crate) struct AgentPipe {
    name: Vec<u16>,
    agent: PathBuf,
}

impl AgentPipe {
    /// `None` if the runtime dir is missing or the identity is unreadable. Name = xxh3_128(user
    /// SDDL, logon SDDL, `fs::canonicalize(runtime)`) exactly as the agent's `security.rs:763-785`.
    pub(crate) fn locate(paths: &Paths) -> Option<Self> {
        let _ = paths;
        None
    }

    /// Connect (SQOS identification), verify the server PID's image == `paths.file(Agent)` and its
    /// token user + logon == ours, send one request line, read one response line; timeout ≤
    /// `MAX_TIMEOUT_MS`.
    pub(crate) fn call(
        &self,
        request: &InstallerRequest,
        timeout: Duration,
    ) -> Result<Answer, CallFailure> {
        let _ = (request, timeout);
        Err(CallFailure::Unavailable)
    }

    /// Status → instance → `encode_installer_stop` → `decode_installer_stop`. Returns the server PID.
    pub(crate) fn installer_stop(&self, timeout: Duration) -> Result<u32, CallFailure> {
        let _ = timeout;
        Err(CallFailure::Unavailable)
    }
}

/// Poll Status every 500 ms until `Ok(Status(_))` or `within`; never errors.
pub(crate) fn wait_until_answering(paths: &Paths, within: Duration) -> Option<Answer> {
    let _ = (paths, within);
    None
}

/// The GUI's `AgentPort`: its own worker thread, with the expiry semantics of
/// `integration/ports.rs:45-93`.
pub(crate) struct PipePort {
    paths: Paths,
    clock: crate::live::Clock,
    replies: Vec<AgentReply>,
}

impl std::fmt::Debug for PipePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipePort").finish_non_exhaustive()
    }
}

impl PipePort {
    pub(crate) fn start(paths: Paths, clock: crate::live::Clock) -> std::io::Result<Self> {
        let _ = (paths, clock);
        Err(std::io::Error::other("not built"))
    }
}

impl AgentPort for PipePort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        let _ = call;
        Err(CallFailure::Unavailable)
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        std::mem::take(&mut self.replies)
    }
}
