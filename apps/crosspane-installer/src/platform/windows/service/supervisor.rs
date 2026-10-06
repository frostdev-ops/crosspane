//! Pure lifecycle decisions. Process and receipt facts are observations, not native authority.

use super::super::native_io::{NativeError, NativeResult};

pub const BACKOFF_MS: u64 = 5000;
pub const WINDOW_MS: u64 = 600_000;
pub const MAX_RESTARTS: usize = 3;

#[cfg(windows)]
pub(super) fn native_entry(trusted: &super::TrustedImages) -> NativeResult<super::SupervisorExit> {
    #[cfg(not(test))]
    {
        match native::run(trusted) {
            Ok(exit) => Ok(exit),
            Err(error) => Err(native::retain_error(error)),
        }
    }
    #[cfg(test)]
    {
        let _ = trusted;
        Err(NativeError::Unsupported)
    }
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

/// Bootstrap PID/phase only decides whether to TRY actual readiness admission. A true result
/// never supplies creation/image/job authority; the retained CreatedChild still supplies those.
#[cfg(any(windows, test))]
fn selected_created_ready(
    expected_pid: u32,
    observed_pid: u32,
    phase: crate::agent_contract::BootstrapPhase,
) -> bool {
    expected_pid != 0
        && expected_pid == observed_pid
        && phase == crate::agent_contract::BootstrapPhase::Ready
}

/// The persisted StopIntent inhibits decisions while the genuine owner bridge arms its native
/// gate. This is scheduling only: no journal value mints a terminal or completion capability.
#[cfg(any(windows, test))]
fn terminal_decision_pending(model: &Supervisor, terminal_operation: Option<[u8; 16]>) -> bool {
    model.stop_latched() && terminal_operation.is_none()
}

/// Retry only a pre-dispatch Busy from a read-only observation on the same absolute deadline.
/// Callers never pass a mutation, native arm, transport submission, or launch operation here.
#[cfg(any(windows, test))]
fn retry_busy_observation<T>(
    deadline: &super::super::native_io::Deadline,
    mut observe: impl FnMut() -> NativeResult<T>,
    mut wait: impl FnMut(),
) -> NativeResult<T> {
    loop {
        deadline.check()?;
        match observe() {
            Err(NativeError::Busy) => {
                deadline.check()?;
                wait();
            }
            Ok(value) => {
                deadline.check()?;
                return Ok(value);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum ErrorRetentionExit {
    Original(NativeError),
    OwnershipUnknown,
}

/// Same bounded error-exit driver in production and fakes. Failed observations cannot prove
/// settlement; the absolute deadline is never renewed or extended by late/busy callbacks.
#[cfg(any(windows, test))]
fn retain_error_with(
    original: NativeError,
    deadline: &super::super::native_io::Deadline,
    mut observe: impl FnMut() -> NativeResult<super::super::native_io::jobs::ErrorRetentionObservation>,
    mut wait: impl FnMut(),
) -> ErrorRetentionExit {
    loop {
        if deadline.check().is_err() {
            return ErrorRetentionExit::OwnershipUnknown;
        }
        match observe() {
            Ok(super::super::native_io::jobs::ErrorRetentionObservation::Settled) => {
                return if deadline.check().is_ok() {
                    ErrorRetentionExit::Original(original)
                } else {
                    ErrorRetentionExit::OwnershipUnknown
                };
            }
            Ok(super::super::native_io::jobs::ErrorRetentionObservation::Pending) | Err(_) => {
                wait()
            }
        }
    }
}

#[cfg(all(windows, not(test)))]
mod native {
    use super::super::super::{
        native_io::{
            AgentObservation, Cancellation, Clock, Deadline, ExitObservation, MonotonicClock,
            SupportProof, WindowsNativeIo,
            jobs::{ChildStartPermit, CreatedChild, SupervisorOwner},
            supervisor_owner::OwnerServer,
        },
        transport::WindowsAgentPort,
    };
    use super::super::{
        SupervisorExit, TrustedImages,
        journal::{Journal, Phase},
        task,
    };
    use super::*;
    use crate::agent_contract::{
        AgentCall, AgentPort, BootstrapPhase, DecodedReply, InstallerRequest, ObservationSource,
        StatusAdmission,
    };
    use std::sync::Arc;
    use std::time::Duration;

    fn budget(clock: Arc<dyn Clock>, milliseconds: u64) -> NativeResult<Deadline> {
        Deadline::new(milliseconds, clock, Cancellation::default())
    }
    fn proof(io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<SupportProof> {
        io.admit_support(deadline)
    }
    fn pause() {
        std::thread::sleep(Duration::from_millis(50));
    }

    /// Status is read-only and bound by A2 to the selected live original endpoint. A scheduler's
    /// EnginePID or a JSON instance alone is never readiness/creation lineage.
    fn ready_status(
        io: &Arc<WindowsNativeIo>,
        original: &AgentObservation,
        clock: Arc<dyn Clock>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if original.bootstrap().phase != BootstrapPhase::Ready {
            return Err(NativeError::Unavailable);
        }
        let mut port = WindowsAgentPort::new(io.clone(), proof(io, deadline)?, clock, deadline)?;
        let timeout = deadline
            .remaining_ms()?
            .min(crate::agent_contract::MAX_TIMEOUT_MS);
        port.submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: timeout,
        })
        .map_err(|_| NativeError::Unavailable)?;
        loop {
            deadline.check()?;
            let mut replies = port.poll().into_iter();
            let Some(reply) = replies.next() else {
                pause();
                continue;
            };
            if replies.next().is_some() || reply.id != 1 || reply.source != ObservationSource::Live
            {
                return Err(NativeError::Foreign);
            }
            match reply.result {
                Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => {
                    let instance = &health.installer().instance;
                    let bootstrap = original.bootstrap();
                    if instance.id != bootstrap.instance_id
                        || instance.pid != bootstrap.pid
                        || instance.uid.is_some()
                    {
                        return Err(NativeError::Foreign);
                    }
                    original.revalidate(io, &proof(io, deadline)?, deadline)?;
                    return Ok(());
                }
                _ => return Err(NativeError::Unavailable),
            }
        }
    }
    fn await_ready(
        owner: &Arc<SupervisorOwner>,
        child: &CreatedChild,
        deadline: &Deadline,
    ) -> NativeResult<(AgentObservation, Generation)> {
        loop {
            deadline.check()?;
            let exited = retry_busy_observation(
                deadline,
                || owner.created_exit(child, &proof(owner.io(), deadline)?, deadline),
                pause,
            )?;
            if exited.is_some() {
                return Err(NativeError::Unavailable);
            }
            match retry_busy_observation(
                deadline,
                || {
                    let support = proof(owner.io(), deadline)?;
                    owner.io().observe_agent(&support, deadline)
                },
                pause,
            ) {
                Ok(original) => {
                    // A resumed child may not have rewritten its predecessor's bootstrap yet.
                    // Never send Status to or adopt that other PID; keep only this bounded wait.
                    if selected_created_ready(
                        child.pid(),
                        original.bootstrap().pid,
                        original.bootstrap().phase,
                    ) {
                        ready_status(owner.io(), &original, owner.clock(), deadline)?;
                        let generation = owner.admit_ready(
                            child,
                            &original,
                            &proof(owner.io(), deadline)?,
                            deadline,
                        )?;
                        return Ok((original, generation));
                    }
                }
                Err(
                    NativeError::Missing
                    | NativeError::Unavailable
                    | NativeError::Busy
                    | NativeError::Foreign,
                ) => {}
                Err(error) => return Err(error),
            }
            pause();
        }
    }
    fn fresh_server_agent(
        owner: &Arc<SupervisorOwner>,
        expected: Generation,
        deadline: &Deadline,
    ) -> NativeResult<AgentObservation> {
        let original = retry_busy_observation(
            deadline,
            || {
                let support = proof(owner.io(), deadline)?;
                owner.io().observe_agent(&support, deadline)
            },
            pause,
        )?;
        retry_busy_observation(
            deadline,
            || {
                let support = proof(owner.io(), deadline)?;
                if original.bootstrap().pid != expected.pid
                    || original.bootstrap().instance_id != expected.instance
                    || original.creation_time(owner.io(), &support, deadline)? != expected.creation
                {
                    return Err(NativeError::Foreign);
                }
                owner
                    .child_for_peer(&original, &support, deadline)
                    .map(|_| ())
            },
            pause,
        )?;
        Ok(original)
    }
    fn transition(
        owner: &Arc<SupervisorOwner>,
        record: &mut Journal,
        phase: Phase,
        model: &Supervisor,
        generation: Generation,
    ) -> NativeResult<()> {
        let deadline = budget(owner.clock(), 30_000)?;
        let support = proof(owner.io(), &deadline)?;
        let lock = owner.io().acquire_installer_lock(&support, &deadline)?;
        let support = proof(owner.io(), &deadline)?;
        let mut next = record.clone();
        next.phase = phase;
        next.current = Some(generation);
        next.restart_times = model.restart_times().to_vec();
        next.last_tick_ms = owner.clock().now_ms();
        next.stop_instance = None;
        next.publish_owned_transition(owner.io(), &support, &lock, Some(record), &deadline)?;
        *record = next;
        Ok(())
    }
    fn load_live(
        owner: &Arc<SupervisorOwner>,
        record: &Journal,
        deadline: &Deadline,
    ) -> NativeResult<Journal> {
        let current = Journal::read(owner.io(), &proof(owner.io(), deadline)?, deadline)?
            .ok_or(NativeError::Missing)?;
        // The actual owner/lease exists here; this scalar binding is correlation only.
        current.bind(
            record.registration,
            record.operation,
            &record.user,
            owner.clock().epoch(),
        )?;
        if current.current != record.current {
            return Err(NativeError::Foreign);
        }
        Ok(current)
    }
    pub(super) fn retain_error(original: NativeError) -> NativeError {
        // This is a NEW hard error-retention budget, never an extension of the caller's operation
        // deadline. At cap main receives a distinct ownership-unknown result, as lead d776409a.
        let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::default());
        let hard = match budget(clock.clone(), 120_000) {
            Ok(deadline) => deadline,
            Err(_) => return NativeError::OutcomeUnknown,
        };
        match retain_error_with(
            original,
            &hard,
            || {
                let remaining = hard.remaining_ms()?.min(1000);
                let observation = budget(clock.clone(), remaining)?;
                super::super::super::native_io::jobs::observe_failed_entry(&observation)
            },
            pause,
        ) {
            ErrorRetentionExit::Original(error) => error,
            ErrorRetentionExit::OwnershipUnknown => {
                // Metadata-only fixed reason. No PID, path, handle, owner data or record contents.
                eprintln!(
                    "Crosspane Installer supervisor: ownership-unknown after bounded retention"
                );
                NativeError::OutcomeUnknown
            }
        }
    }
    pub(super) fn run(trusted: &TrustedImages) -> NativeResult<SupervisorExit> {
        let initial = budget(Arc::new(MonotonicClock::default()), 30_000)?;
        let permit = Arc::new(task::claim_supervisor(trusted, &initial)?);
        let images = &trusted._native;
        let support = proof(&images.io, &initial)?;
        let owner =
            images
                .io
                .prepare_supervisor(&support, &images.agent, &images._root, &initial)?;
        let server = OwnerServer::reserve(
            images.io.clone(),
            owner.clone(),
            &proof(&images.io, &initial)?,
            &initial,
        )?;
        // Archive is real atomic admission, not a body boolean or a cold owner reconstruction.
        let archive = {
            let support = proof(owner.io(), &initial)?;
            let lock = owner.io().acquire_installer_lock(&support, &initial)?;
            owner.io().prepare_supervisor_epoch(
                &proof(owner.io(), &initial)?,
                &lock,
                &permit,
                &owner,
                &initial,
            )?
        };
        let start = ChildStartPermit::initial(
            owner.clone(),
            permit.clone(),
            &proof(owner.io(), &initial)?,
            &initial,
        )?;
        let child = owner.launch(start, &proof(owner.io(), &initial)?, &initial)?;
        let (mut original, mut generation) = await_ready(&owner, &child, &initial)?;
        let mut model = Supervisor::new(generation, owner.clock().now_ms(), Vec::new())?;
        model.running();
        let mut record = Journal {
            schema_version: 1,
            registration: permit.registration(),
            operation: permit.operation(),
            user: permit.user().to_owned(),
            phase: Phase::Running,
            current: Some(generation),
            stop_instance: None,
            original_xml: None,
            restart_times: Vec::new(),
            last_tick_ms: owner.clock().now_ms(),
            clock_epoch: owner.clock().epoch(),
        };
        {
            let deadline = budget(owner.clock(), 30_000)?;
            let support = proof(owner.io(), &deadline)?;
            let lock = owner.io().acquire_installer_lock(&support, &deadline)?;
            let support = proof(owner.io(), &deadline)?;
            archive.reverify(owner.io(), &support, &lock, &permit, &owner, &deadline)?;
            record.publish_owned_transition(owner.io(), &support, &lock, None, &deadline)?;
        }
        let bound = budget(owner.clock(), 30_000)?;
        let server_agent = fresh_server_agent(&owner, generation, &bound)?;
        server.update_generation(server_agent, &proof(owner.io(), &bound)?, &bound)?;
        let mut successor_wait: Option<Deadline> = None;
        loop {
            let deadline = budget(owner.clock(), 30_000)?;
            let current =
                retry_busy_observation(&deadline, || load_live(&owner, &record, &deadline), pause)?;
            if current.phase == Phase::StopIntent {
                // Intent inhibits this owner's decision loop, but NEVER claims that the native
                // spawn gate has been armed by the authenticated bridge or that Stop completed.
                if current.stop_instance != Some(generation.instance) {
                    return Err(NativeError::Foreign);
                }
                model.stop(generation.instance)?;
                record = current;
            } else if current != record {
                return Err(NativeError::Foreign);
            }
            if owner.terminal_operation().is_some() {
                model.stop(generation.instance)?;
            }
            let exit = retry_busy_observation(
                &deadline,
                || {
                    let support = proof(owner.io(), &deadline)?;
                    original.observe_exit(owner.io(), &support, &deadline)
                },
                pause,
            )?;
            let native_original = match exit {
                ExitObservation::Running => {
                    pause();
                    continue;
                }
                ExitObservation::Exited {
                    creation,
                    code,
                    receipt,
                } => {
                    if creation != generation.creation {
                        return Err(NativeError::Foreign);
                    }
                    Original::Exited {
                        code,
                        receipt: receipt.map(|r| Receipt {
                            instance: r.instance_id,
                            clean: r.clean,
                        }),
                    }
                }
            };
            let active = retry_busy_observation(
                &deadline,
                || owner.job_active(&proof(owner.io(), &deadline)?, &deadline),
                pause,
            )?;
            if active != 0 {
                // Clean same-job replacement is admitted from the actual retained successor.
                // Lead64c81ac6: LastExitV1 has no restart flag; exact clean receipt + real different
                // approved PID/creation/instance in this job supplies the unchanged A3 Follow facts.
                if model.stop_latched() {
                    pause();
                    continue;
                }
                let wait = match &successor_wait {
                    Some(wait) => wait.clone(),
                    None => {
                        let wait = budget(owner.clock(), 30_000)?;
                        successor_wait = Some(wait.clone());
                        wait
                    }
                };
                wait.check()?;
                let candidate = match retry_busy_observation(
                    &wait,
                    || owner.io().observe_agent(&proof(owner.io(), &wait)?, &wait),
                    pause,
                ) {
                    Ok(candidate) => candidate,
                    Err(
                        NativeError::Missing
                        | NativeError::Unavailable
                        | NativeError::Busy
                        | NativeError::Foreign,
                    ) => {
                        pause();
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if candidate.bootstrap().phase != BootstrapPhase::Ready
                    || candidate.bootstrap().instance_id == generation.instance
                {
                    pause();
                    continue;
                }
                ready_status(owner.io(), &candidate, owner.clock(), &wait)?;
                let next = owner.adopt_successor(
                    &original,
                    &candidate,
                    &proof(owner.io(), &wait)?,
                    &wait,
                )?;
                if model.observe(
                    owner.clock().now_ms(),
                    native_original,
                    Tree::Replacement {
                        child: next,
                        admitted: true,
                    },
                ) != Decision::Follow(next)
                {
                    return Err(NativeError::Foreign);
                }
                generation = next;
                original = candidate;
                successor_wait = None;
                transition(&owner, &mut record, Phase::Running, &model, generation)?;
                let candidate = fresh_server_agent(&owner, generation, &wait)?;
                server.update_generation(candidate, &proof(owner.io(), &wait)?, &wait)?;
                continue;
            }
            successor_wait = None;
            if terminal_decision_pending(&model, owner.terminal_operation()) {
                // Do not retire the pure model on clean exit before the authenticated bridge has
                // armed the actual spawn mutex. A subsequent arm must still be able to finish.
                pause();
                continue;
            }
            match model.observe(owner.clock().now_ms(), native_original, Tree::Empty) {
                Decision::Finished => {
                    if model.stop_latched() {
                        // The outer Stop holds the installer lease. No source lock/Finished write!
                        // Only actual terminal gate + original exit/job empty can export and exit.
                        owner
                            .settled_for_stop(&proof(owner.io(), &deadline)?, &original, &deadline)?
                            .ok_or(NativeError::Busy)?;
                        server.finish(&proof(owner.io(), &deadline)?, &deadline)?;
                        return Ok(SupervisorExit::InstallerStopped);
                    }
                    transition(&owner, &mut record, Phase::Finished, &model, generation)?;
                    server.finish(&proof(owner.io(), &deadline)?, &deadline)?;
                    return Ok(SupervisorExit::NormalQuit);
                }
                Decision::Backoff { .. } => {
                    if record.phase != Phase::Backoff {
                        transition(&owner, &mut record, Phase::Backoff, &model, generation)?;
                    }
                    pause();
                }
                Decision::Start => {
                    transition(
                        &owner,
                        &mut record,
                        Phase::StartRequested,
                        &model,
                        generation,
                    )?;
                    let start =
                        owner.restart_permit(&record, &proof(owner.io(), &deadline)?, &deadline)?;
                    let child = owner.launch(start, &proof(owner.io(), &deadline)?, &deadline)?;
                    let (next, next_generation) = await_ready(&owner, &child, &deadline)?;
                    model = Supervisor::new(
                        next_generation,
                        owner.clock().now_ms(),
                        record.restart_times.clone(),
                    )?;
                    model.running();
                    original = next;
                    generation = next_generation;
                    transition(&owner, &mut record, Phase::Running, &model, generation)?;
                    let candidate = fresh_server_agent(&owner, generation, &deadline)?;
                    server.update_generation(
                        candidate,
                        &proof(owner.io(), &deadline)?,
                        &deadline,
                    )?;
                }
                Decision::Observe => pause(),
                Decision::RecoveryRetained => return Err(NativeError::OutcomeUnknown),
                Decision::Follow(_) | Decision::SubmitStop { .. } => {
                    return Err(NativeError::Foreign);
                }
            }
        }
    }
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    fn old() -> Generation {
        Generation {
            pid: 101,
            creation: 102,
            instance: 103,
        }
    }
    fn next() -> Generation {
        Generation {
            pid: 201,
            creation: 202,
            instance: 203,
        }
    }
    fn crash() -> Original {
        Original::Exited {
            code: 1,
            receipt: None,
        }
    }
    #[test]
    fn stale_predecessor_is_pending_until_exact_created_child_ready() {
        use crate::agent_contract::BootstrapPhase::{Ready, Starting};
        let samples = [
            (old().pid, Ready),
            (next().pid, Starting),
            (next().pid, Ready),
        ];
        let mut submitted = Vec::new();
        for (pid, phase) in samples {
            if selected_created_ready(next().pid, pid, phase) {
                submitted.push(pid);
            }
        }
        assert_eq!(submitted, [next().pid]);
    }
    #[test]
    fn mismatched_bootstrap_never_receives_status_or_readiness_adoption() {
        use crate::agent_contract::BootstrapPhase::{Failed, Ready, Starting};
        for phase in [Starting, Ready, Failed] {
            assert!(!selected_created_ready(next().pid, old().pid, phase));
            assert!(!selected_created_ready(0, next().pid, phase));
        }
    }
    #[test]
    fn genuine_ready_required_before_constructing_model_no_fictitious_instance() {
        assert!(matches!(
            Supervisor::new(
                Generation {
                    instance: 0,
                    ..old()
                },
                0,
                vec![]
            ),
            Err(NativeError::Invalid)
        ));
    }
    #[test]
    fn native_restart_reinitialization_preserves_actual_three_in_window_ledger() {
        let mut model = Supervisor::new(old(), 100, vec![1, 2]).unwrap();
        model.running();
        assert_eq!(
            model.observe(100, crash(), Tree::Empty),
            Decision::Backoff { until_ms: 5100 }
        );
        assert_eq!(
            model.observe(5099, crash(), Tree::Empty),
            Decision::Backoff { until_ms: 5100 }
        );
        assert_eq!(model.observe(5100, crash(), Tree::Empty), Decision::Start);
        let mut resumed = Supervisor::new(next(), 5101, model.restart_times().to_vec()).unwrap();
        resumed.running();
        assert_eq!(
            resumed.observe(5102, crash(), Tree::Empty),
            Decision::RecoveryRetained
        );
    }
    #[test]
    fn matching_clean_receipt_needs_real_same_job_successor_or_empty_tree() {
        let clean = Original::Exited {
            code: 0,
            receipt: Some(Receipt {
                instance: old().instance,
                clean: true,
            }),
        };
        let mut model = Supervisor::new(old(), 0, vec![]).unwrap();
        model.running();
        assert_eq!(
            model.observe(1, clean, Tree::Uncertain),
            Decision::RecoveryRetained
        );
        let mut model = Supervisor::new(old(), 0, vec![]).unwrap();
        model.running();
        assert_eq!(
            model.observe(
                1,
                clean,
                Tree::Replacement {
                    child: next(),
                    admitted: true
                }
            ),
            Decision::Follow(next())
        );
    }
    #[test]
    fn failed_entry_retains_live_ownership_until_positive_settlement() {
        use super::super::super::native_io::{
            Cancellation, Clock, Deadline, jobs::ErrorRetentionObservation::*,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        #[derive(Default)]
        struct FakeClock(AtomicU64);
        impl Clock for FakeClock {
            fn now_ms(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
        }
        let clock = Arc::new(FakeClock::default());
        let deadline = Deadline::new(120_000, clock.clone(), Cancellation::default()).unwrap();
        let mut observations = [
            Ok(Pending),
            Err(NativeError::Busy),
            Err(NativeError::OutcomeUnknown),
            Ok(Settled),
        ]
        .into_iter();
        let result = retain_error_with(
            NativeError::Unavailable,
            &deadline,
            || observations.next().unwrap(),
            || {
                clock.0.fetch_add(1000, Ordering::Relaxed);
            },
        );
        assert_eq!(
            result,
            ErrorRetentionExit::Original(NativeError::Unavailable)
        );
        assert_eq!(clock.now_ms(), 3000);
    }
    #[test]
    fn failed_entry_hard_cap_never_treats_late_settlement_as_known_completion() {
        use super::super::super::native_io::{
            Cancellation, Clock, Deadline, jobs::ErrorRetentionObservation::*,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        #[derive(Default)]
        struct FakeClock(AtomicU64);
        impl Clock for FakeClock {
            fn now_ms(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
        }
        let clock = Arc::new(FakeClock::default());
        let deadline = Deadline::new(120_000, clock.clone(), Cancellation::default()).unwrap();
        let mut observed = 0;
        assert_eq!(
            retain_error_with(
                NativeError::Unavailable,
                &deadline,
                || {
                    observed += 1;
                    Ok(Pending)
                },
                || {
                    clock.0.fetch_add(60_000, Ordering::Relaxed);
                }
            ),
            ErrorRetentionExit::OwnershipUnknown
        );
        assert_eq!(observed, 2);
        assert_eq!(clock.now_ms(), 120_000);
        let clock = Arc::new(FakeClock::default());
        let deadline = Deadline::new(120_000, clock.clone(), Cancellation::default()).unwrap();
        assert_eq!(
            retain_error_with(
                NativeError::Unavailable,
                &deadline,
                || {
                    clock.0.store(120_000, Ordering::Relaxed);
                    Ok(Settled)
                },
                || panic!("late settlement must not retry")
            ),
            ErrorRetentionExit::OwnershipUnknown
        );
    }
    #[test]
    fn only_pre_dispatch_busy_observation_retries_with_same_absolute_budget() {
        use super::super::super::native_io::{Cancellation, Clock, Deadline};
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        #[derive(Default)]
        struct FakeClock(AtomicU64);
        impl Clock for FakeClock {
            fn now_ms(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
        }
        let clock = Arc::new(FakeClock::default());
        let deadline = Deadline::new(10, clock.clone(), Cancellation::default()).unwrap();
        let mut attempts = 0;
        let result = retry_busy_observation(
            &deadline,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(NativeError::Busy)
                } else {
                    Ok(7)
                }
            },
            || {
                clock.0.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert_eq!(result, Ok(7));
        assert_eq!(attempts, 2);
        for error in [
            NativeError::OutcomeUnknown,
            NativeError::Foreign,
            NativeError::Unavailable,
        ] {
            let mut attempts = 0;
            assert_eq!(
                retry_busy_observation(
                    &deadline,
                    || {
                        attempts += 1;
                        Err::<(), _>(error)
                    },
                    || panic!("not Busy")
                ),
                Err(error)
            );
            assert_eq!(attempts, 1);
        }
        let mut attempts = 0;
        assert_eq!(
            retry_busy_observation(
                &deadline,
                || {
                    attempts += 1;
                    Err::<(), _>(NativeError::Busy)
                },
                || {
                    clock.0.fetch_add(5, Ordering::Relaxed);
                }
            ),
            Err(NativeError::Timeout)
        );
        assert_eq!(attempts, 2);
    }
    #[test]
    fn durable_stop_waits_native_arm_without_retiring_clean_empty_decision() {
        let mut model = Supervisor::new(old(), 0, vec![]).unwrap();
        model.running();
        model.stop(old().instance).unwrap();
        assert!(terminal_decision_pending(&model, None));
        // The native loop leaves observe untouched during this gap; arrival of the actual gate
        // permits exactly one Finished decision while retaining the stop latch.
        assert!(!terminal_decision_pending(&model, Some([1; 16])));
        let clean = Original::Exited {
            code: 0,
            receipt: Some(Receipt {
                instance: old().instance,
                clean: true,
            }),
        };
        assert_eq!(model.observe(1, clean, Tree::Empty), Decision::Finished);
        assert!(model.stop_latched());
        assert_eq!(model.start_once(), Decision::RecoveryRetained);
    }
    #[test]
    fn durable_stop_inhibition_never_rearms_crash_restart_after_unknown_ack() {
        let mut model = Supervisor::new(old(), 0, vec![]).unwrap();
        model.running();
        assert_eq!(
            model.observe(1, crash(), Tree::Empty),
            Decision::Backoff { until_ms: 5001 }
        );
        assert_eq!(
            model.stop(old().instance).unwrap(),
            Decision::SubmitStop {
                expected_instance: old().instance
            }
        );
        assert_eq!(
            model.observe(6000, crash(), Tree::Empty),
            Decision::RecoveryRetained
        );
        assert_eq!(model.start_once(), Decision::RecoveryRetained);
    }
}
