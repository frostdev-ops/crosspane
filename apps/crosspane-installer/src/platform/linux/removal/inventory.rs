use super::super::{
    payload::{Package, PayloadInstaller, sha256},
    service::{LinuxService, ServiceFacts},
};
use super::*;
use crate::agent_contract::{
    AgentReply, DecodedReply, ObservationSource, StatusAdmission, WireEpochs,
};
use crosspane_installer_core::ResourceReceipt;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityFacts {
    pub input: Option<bool>,
    pub projections: Option<bool>,
    pub audio: Option<bool>,
    pub instance_id: u64,
    pub epochs: WireEpochs,
    /// Original complete-reply time/source, never refreshed by inventory, planning or a click.
    pub observed_at_ms: u64,
    pub source: ObservationSource,
}
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryFacts {
    pub package_manifest_sha256: [u8; 32],
    pub member_hashes: Vec<(String, String)>,
    pub rendered_hashes: Vec<(std::path::PathBuf, [u8; 32])>,
    pub resources: std::result::Result<Vec<ResourceReceipt>, PayloadError>,
    pub service: std::result::Result<ServiceFacts, ServiceError>,
    pub original: Option<ProcessIdentity>,
    pub instance_id: Option<u64>,
    pub correlation: std::result::Result<(), RemovalError>,
    pub activity: Option<ActivityFacts>,
}
/// Facts report uncertainty. Only this controller's private capture binds a later plan.
#[derive(Debug)]
pub struct Inventory {
    pub(super) owner: Arc<()>,
    pub(super) facts: InventoryFacts,
    pub(super) tracked: Option<Arc<TrackedAgent>>,
    pub(super) generation: u64,
    pub(super) observed_ms: u64,
    expires_at: Instant,
}
#[derive(Debug)]
pub struct InventoryRequest<'a> {
    pub proof: &'a SupportProof,
    pub package: &'a Package,
    pub service: &'a LinuxService,
    pub reply: Option<&'a AgentReply>,
    pub now_ms: u64,
    pub deadline: &'a Deadline,
}
impl Inventory {
    pub(super) fn fresh(&self, now_ms: u64) -> bool {
        Instant::now() <= self.expires_at
            && now_ms
                .checked_sub(self.observed_ms)
                .is_some_and(|age| age <= 5000)
    }
    pub fn facts(&self) -> &InventoryFacts {
        &self.facts
    }
    pub fn tracked(&self) -> Option<&Arc<TrackedAgent>> {
        self.tracked.as_ref()
    }
}
#[derive(Default)]
struct DetectionHistory {
    facts: Option<InventoryFacts>,
    activity: Option<ActivityFacts>,
}
pub struct RemovalPlanner {
    pub(super) io: Arc<LinuxNativeIo>,
    pub(super) owner: Arc<()>,
    pub(super) last: (u64, u64),
    reader: Option<Arc<dyn ExitReader>>,
    pub(super) generation: AtomicU64,
    pub(super) retired: AtomicU64,
    history: Mutex<DetectionHistory>,
}
impl std::fmt::Debug for RemovalPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemovalPlanner { .. }")
    }
}
impl RemovalPlanner {
    pub fn new(io: Arc<LinuxNativeIo>) -> Self {
        Self {
            io,
            owner: Arc::new(()),
            last: (0, 0),
            reader: None,
            generation: AtomicU64::new(0),
            retired: AtomicU64::new(0),
            history: Mutex::default(),
        }
    }
    pub fn scratch_exit_reader(&mut self, reader: Arc<dyn ExitReader>) -> Result<()> {
        if self.io.target().source() != ObservationSource::Demo {
            return Err(RemovalError::Invalid);
        }
        self.reader = Some(reader);
        Ok(())
    }
    pub fn inventory(
        &self,
        proof: &SupportProof,
        package: &Package,
        service: &LinuxService,
        reply: Option<&AgentReply>,
        now_ms: u64,
        deadline: &Deadline,
    ) -> Result<Inventory> {
        // A supplied reply conservatively caps the entire snapshot's remaining lifetime.
        let lifetime = reply
            .and_then(|r| now_ms.checked_sub(r.observed_at_ms))
            .filter(|age| *age <= 5000)
            .map_or(5000, |age| 5000 - age);
        let expires_at = Instant::now() + Duration::from_millis(lifetime);
        let generation = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| g.checked_add(1))
            .map_err(|_| RemovalError::Invalid)?
            + 1;
        deadline.check()?;
        proof.check(&self.io)?;
        let installer = PayloadInstaller::new(self.io.clone())?;
        let rendered = installer.rendered_resources(package)?;
        let resources = installer.detect(proof, package);
        let service = service.observe(deadline);
        let captured = match &self.reader {
            Some(reader) => {
                TrackedAgent::scratch_capture(self.io.clone(), reader.clone(), deadline)
            }
            None => TrackedAgent::capture(self.io.clone(), deadline),
        };
        let original = captured.as_ref().ok().map(|t| t.original().clone());
        let instance_id = captured.as_ref().ok().map(|t| t.instance_id());
        let service = service.and_then(|f| {
            if rendered.first().is_none_or(|r| r.target != f.fragment)
                || f.source != self.io.target().source()
                || f.main_pid != original.as_ref().map_or(0, |p| p.pid)
            {
                Err(ServiceError::Foreign)
            } else {
                Ok(f)
            }
        });
        let mut activity = reply.and_then(|r| {
            if r.source != self.io.target().source()
                || r.observed_at_ms > now_ms
                || now_ms - r.observed_at_ms > 5000
            {
                return None;
            }
            let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &r.result else {
                return None;
            };
            let agent = captured.as_ref().ok()?;
            let instance = &health.installer().instance;
            self.io
                .admit_instance(instance, &agent.bootstrap, agent.original())
                .ok()?;
            let terminal = health.terminal();
            let installer = health.installer();
            Some(ActivityFacts {
                input: (terminal.controlling.is_some() || terminal.controlled_by.is_some())
                    .then_some(true),
                projections: (!terminal.projections.is_empty() || installer.recovery_pending != 0)
                    .then_some(true),
                audio: (!installer.audio.active_peers.is_empty()).then_some(true),
                instance_id: installer.instance.id,
                epochs: installer.epochs.clone(),
                observed_at_ms: r.observed_at_ms,
                source: r.source,
            })
        });
        deadline.check()?;
        proof.check(&self.io)?;
        let mut history = self.history.lock().map_err(|_| RemovalError::Invalid)?;
        if Instant::now() > expires_at
            || activity
                .as_ref()
                .zip(history.activity.as_ref())
                .is_some_and(|(a, b)| {
                    a.observed_at_ms < b.observed_at_ms
                        || (a.observed_at_ms == b.observed_at_ms && a != b)
                        || (a.instance_id == b.instance_id
                            && (a.epochs.gate < b.epochs.gate
                                || a.epochs.grants < b.epochs.grants
                                || a.epochs.layout < b.epochs.layout
                                || a.epochs.backends < b.epochs.backends))
                })
        {
            activity = None;
        }
        let captured = service
            .as_ref()
            .map_err(|e| RemovalError::Service(*e))
            .and(captured);
        let correlation = captured.as_ref().map(|_| ()).map_err(Clone::clone);
        let tracked = captured.ok().map(Arc::new);
        let inventory = Inventory {
            owner: self.owner.clone(),
            generation,
            observed_ms: now_ms,
            expires_at,
            facts: InventoryFacts {
                package_manifest_sha256: sha256(
                    &serde_json::to_vec(package.manifest()).map_err(|_| RemovalError::Invalid)?,
                ),
                member_hashes: package
                    .manifest()
                    .members
                    .iter()
                    .map(|m| (m.name.clone(), m.sha256.clone()))
                    .collect(),
                rendered_hashes: rendered
                    .into_iter()
                    .map(|r| (r.target, r.rendered_sha256))
                    .collect(),
                resources,
                service,
                original,
                instance_id,
                correlation,
                activity,
            },
            tracked,
        };
        if generation != self.generation.load(Ordering::Acquire) {
            return Err(RemovalError::Stale);
        }
        if history
            .facts
            .as_ref()
            .is_some_and(|f| f != &inventory.facts)
        {
            self.retired.store(generation - 1, Ordering::Release);
        }
        if inventory.facts.activity.is_some() {
            history.activity = inventory.facts.activity.clone();
        }
        history.facts = Some(inventory.facts.clone());
        Ok(inventory)
    }
}
