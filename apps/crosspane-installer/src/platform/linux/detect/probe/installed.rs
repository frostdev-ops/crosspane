use super::*;
use crate::{
    agent_contract::{
        AgentCall, AgentPort, AgentReply, BootstrapV1, CallFailure, InstallerRequest,
        StatusAdmission,
    },
    platform::linux::transport::LinuxAgentPort,
};
fn call_issue(error: impl std::borrow::Borrow<CallFailure>) -> ProbeIssue {
    match error.borrow() {
        CallFailure::TimeoutOutcomeUnknown => ProbeIssue::Timeout,
        CallFailure::InvalidResponse | CallFailure::InvalidCall(_) => ProbeIssue::Malformed,
        CallFailure::Refused(_) => ProbeIssue::Unverified,
        _ => ProbeIssue::Unavailable,
    }
}
/// Wire correlation is followed by the frozen native instance admission, retaining receipt time.
/// The frozen transport collapses status admission Foreign to Unavailable; bootstrap Foreign
/// remains distinct. This second admission is defence in depth, not transport error recovery.
pub fn associate_installed(
    io: &LinuxNativeIo,
    bootstrap: BootstrapV1,
    process: &ProcessIdentity,
    reply: AgentReply,
) -> Fact<InstalledAgentFacts> {
    let source = reply.source;
    let observed_at_ms = reply.observed_at_ms;
    let value = (|| {
        reply.result.as_ref().map_err(call_issue)?;
        let facts = InstalledAgentFacts::from_reply(bootstrap, reply)?;
        if let StatusAdmission::Supported(health) = &facts.status {
            io.admit_instance(&health.installer().instance, &facts.bootstrap, process)
                .map_err(issue)?;
        }
        Ok(facts)
    })();
    Fact {
        value,
        source,
        observed_at_ms,
    }
}
pub(super) fn read(
    io: Arc<LinuxNativeIo>,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<InstalledAgentFacts> {
    let result = (|| {
        deadline.check().map_err(issue)?;
        let (bootstrap, process) = io.bootstrap(deadline).map_err(|error| {
            // Lead-approved fresh NOENT observation only after bootstrap fails; no creation/retry.
            let absent = deadline
                .check()
                .and_then(|_| io.metadata(io.target().runtime_dir()))
                .and_then(|runtime| {
                    if runtime.is_none() {
                        Ok(None)
                    } else {
                        io.metadata(&io.target().runtime_dir().join("bootstrap.json"))
                    }
                });
            let missing = deadline.check().is_ok() && matches!(absent, Ok(None));
            if missing {
                ProbeIssue::Missing
            } else {
                issue(error)
            }
        })?;
        let mut port = LinuxAgentPort::new(io.clone(), None, clock.clone()).map_err(issue)?;
        let received = (|| {
            deadline.check().map_err(issue)?;
            port.submit(AgentCall {
                id: 1,
                request: InstallerRequest::Status,
                timeout_ms: 5000,
            })
            .map_err(call_issue)?;
            loop {
                deadline.check().map_err(issue)?;
                let mut replies = port.poll();
                if !replies.is_empty() {
                    if replies.len() != 1 || replies[0].id != 1 {
                        return Err(ProbeIssue::Malformed);
                    }
                    deadline.check().map_err(issue)?;
                    return Ok(replies.remove(0));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })();
        port.shutdown();
        Ok(associate_installed(&io, bootstrap, &process, received?))
    })();
    result.unwrap_or_else(|error| Fact::issue(error, io.target().source(), clock()))
}
