//! Private journal and exact receipt-bound removal; neither receipts nor exit codes prove traffic.
use super::*;
use std::{fs::File, path::PathBuf};

pub const MAX_JOURNAL_BYTES: usize = 64 * 1024;
pub const MAX_RECEIPT_BYTES: usize = 512;
pub const MAX_JOURNAL_RECORDS: usize = 32;
pub const REMOVAL_LIMIT: &str = "Inventory cannot be inspected; presence remains Unknown. Consent permits only the receipt's original exact spec/comment.";
pub const REMOVAL_TOCTOU: &str = "This removes a global rule affecting every Crosspane user. Inspection and execution are not atomic against administrator edits during the polkit prompt; other users/tools may change rules. Changed or ambiguous inspected rules are kept; no prior ruleset is restored.";
type Binding = (u32, [PathBuf; 6], Option<PathBuf>, bool);
fn binding(io: &LinuxNativeIo) -> Binding {
    let p = io.target().paths();
    (
        p.uid,
        [
            p.home.clone(),
            p.prefix.clone(),
            p.config_home.clone(),
            p.state_home.clone(),
            p.data_home.clone(),
            p.runtime_home.clone(),
        ],
        p.runtime_override.clone(),
        io.target().source() == ObservationSource::Demo,
    )
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptData {
    version: u8,
    cidr: String,
    kind: RuleKind,
    result: u8,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    operation: u64,
    revision: u64,
    cidr: String,
    kind: RuleKind,
    interface: String,
    delete: bool,
    outcome: Option<u8>,
    retired: bool,
}
impl Record {
    fn intent(i: &FirewallIntent, delete: bool) -> Self {
        Self {
            operation: i.operation.0,
            revision: i.revision,
            cidr: i.link.cidr.as_str().into(),
            kind: i.kind,
            interface: i.link.interface.clone(),
            delete,
            outcome: None,
            retired: false,
        }
    }
    fn receipt(&self) -> ReceiptData {
        ReceiptData {
            version: 1,
            cidr: self.cidr.clone(),
            kind: self.kind,
            result: self.outcome.unwrap_or(3),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    target: Binding,
    records: Vec<Record>,
}
fn result_code(result: RuleResult) -> u8 {
    match result {
        RuleResult::PendingVerification => 0,
        RuleResult::NotDispatched => 1,
        RuleResult::PromptUnavailable => 2,
        RuleResult::OutcomeUnknown => 3,
        RuleResult::Absent => 4,
        RuleResult::Kept => 5,
    }
}
/// Opaque admission binds the minimal receipt to its private original add record and target.
pub struct AdmittedReceipt {
    record: Record,
    target: Binding,
}
impl AdmittedReceipt {
    /// Literal admitted rule kind, for a selected removal stage's pre-I/O association check.
    pub fn kind(&self) -> RuleKind {
        self.record.kind
    }
}
#[cfg(test)]
mod receipt_kind_tests {
    use super::*;
    #[test]
    fn getter_preserves_each_literal_admitted_kind() {
        for kind in [RuleKind::Lan, RuleKind::Mdns] {
            let receipt = AdmittedReceipt {
                record: Record {
                    operation: 1,
                    revision: 1,
                    cidr: "192.168.4.0/24".into(),
                    kind,
                    interface: "enp1s0".into(),
                    delete: false,
                    outcome: Some(0),
                    retired: false,
                },
                target: (
                    1000,
                    std::array::from_fn(|_| PathBuf::from("/scratch")),
                    None,
                    true,
                ),
            };
            assert_eq!(receipt.kind(), kind);
        }
    }
}
impl std::fmt::Debug for AdmittedReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdmittedReceipt { .. }")
    }
}
pub struct DurableIntentStore {
    io: Arc<LinuxNativeIo>,
    path: PathBuf,
    lock: PathBuf,
    lease: Option<File>,
    active: Option<Record>,
    records: Vec<Record>,
}
impl std::fmt::Debug for DurableIntentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DurableIntentStore { .. }")
    }
}
impl DurableIntentStore {
    /// Open before planning on this controller. Recovery never invents a dispatch clock stamp:
    /// A new controller has no known watermark: recovered unresolved kinds remain fail-closed.
    pub fn open(firewall: &mut LinuxFirewall, proof: &SupportProof) -> NativeResult<Self> {
        let io = firewall.io.clone();
        let dir = io.target().paths().state_home.join("crosspane/ufw");
        io.create_private_dir(proof, &dir)?;
        let mut store = Self {
            io,
            path: dir.join("journal.json"),
            lock: dir.join("journal.lock"),
            lease: None,
            active: None,
            records: Vec::new(),
        };
        let _guard = store.io.lock(proof, &store.lock)?;
        let journal = store.load()?;
        for r in &journal.records {
            if r.outcome.is_none_or(|o| matches!(o, 1..=3)) {
                firewall.current_checks.forget_dispatch(r.kind);
                firewall.unresolved[r.kind.index()] = true;
            }
        }
        store.records = journal.records;
        firewall.current = None;
        firewall.observed = false;
        Ok(store)
    }
    fn load(&self) -> NativeResult<Journal> {
        if self.io.metadata(&self.path)?.is_none() {
            return Ok(Journal {
                version: 1,
                target: binding(&self.io),
                records: Vec::new(),
            });
        }
        let journal: Journal =
            serde_json::from_slice(&self.io.read(&self.path, MAX_JOURNAL_BYTES, true)?)
                .map_err(|_| NativeError::Invalid)?;
        if journal.version != 1 || journal.target != binding(&self.io) {
            return Err(NativeError::Foreign);
        }
        if journal.records.len() > MAX_JOURNAL_RECORDS {
            return Err(NativeError::Oversize);
        }
        for (n, r) in journal.records.iter().enumerate() {
            if r.operation == 0
                || !name(&r.interface)
                || r.outcome.is_some_and(|o| o > 5)
                || LanCidr::parse(&r.cidr)?.as_str() != r.cidr
                || journal.records[..n]
                    .iter()
                    .any(|p| p.operation == r.operation)
                || (r.delete && r.retired)
            {
                return Err(NativeError::Invalid);
            }
        }
        Ok(journal)
    }
    fn save(&self, proof: &SupportProof, journal: &Journal) -> NativeResult<()> {
        let bytes = serde_json::to_vec(journal).map_err(|_| NativeError::Invalid)?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(NativeError::Oversize);
        }
        self.io.atomic_write(proof, &self.path, &bytes)
    }
    fn begin(
        &mut self,
        proof: &SupportProof,
        intent: &FirewallIntent,
        delete: bool,
    ) -> NativeResult<()> {
        proof.check(&self.io)?;
        if self.active.is_some() || intent.target != *self.io.target().paths() {
            return Err(NativeError::Foreign);
        }
        let guard = self.io.lock(proof, &self.lock)?;
        let mut journal = self.load()?;
        if journal.records != self.records {
            return Err(NativeError::OutcomeUnknown);
        }
        let record = Record::intent(intent, delete);
        if journal.records.len() == MAX_JOURNAL_RECORDS {
            return Err(NativeError::Oversize);
        }
        if intent.operation.0 == 0
            || !name(&record.interface)
            || journal
                .records
                .iter()
                .any(|r| r.operation == record.operation)
        {
            return Err(NativeError::Invalid);
        }
        journal.records.push(record.clone());
        self.save(proof, &journal)?;
        self.records = journal.records;
        self.active = Some(record);
        self.lease = Some(guard);
        Ok(())
    }
    /// Minimal data-only receipt; an unfinished durable intent reports OutcomeUnknown.
    pub fn receipt(&self, proof: &SupportProof, operation: OperationId) -> NativeResult<Vec<u8>> {
        proof.check(&self.io)?;
        let journal = self.load()?;
        let record = journal
            .records
            .iter()
            .find(|r| {
                r.operation == operation.0
                    && !r.delete
                    && !r.retired
                    && matches!(r.receipt().result, 0 | 3)
            })
            .ok_or(NativeError::Invalid)?;
        serde_json::to_vec(&record.receipt()).map_err(|_| NativeError::Invalid)
    }
    pub fn admit_receipt(
        &self,
        proof: &SupportProof,
        bytes: &[u8],
    ) -> NativeResult<AdmittedReceipt> {
        proof.check(&self.io)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(NativeError::Oversize);
        }
        let data: ReceiptData = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
        if data.version != 1
            || !matches!(data.result, 0 | 3)
            || LanCidr::parse(&data.cidr)?.as_str() != data.cidr
        {
            return Err(NativeError::Invalid);
        }
        let journal = self.load()?;
        let mut matches = journal
            .records
            .iter()
            .filter(|r| !r.delete && !r.retired && r.receipt() == data);
        let record = matches.next().ok_or(NativeError::Foreign)?.clone();
        if matches.next().is_some() {
            return Err(NativeError::Foreign);
        }
        Ok(AdmittedReceipt {
            record,
            target: journal.target,
        })
    }
    fn admitted(&self, receipt: &AdmittedReceipt) -> NativeResult<()> {
        let journal = self.load()?;
        if receipt.target != journal.target || !journal.records.contains(&receipt.record) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
impl IntentStore for DurableIntentStore {
    fn record_intent(&mut self, proof: &SupportProof, intent: &FirewallIntent) -> NativeResult<()> {
        self.begin(proof, intent, false)
    }
    fn record_outcome(
        &mut self,
        proof: &SupportProof,
        intent: &FirewallIntent,
        outcome: RuleResult,
    ) -> NativeResult<()> {
        if intent.target != *self.io.target().paths() {
            return Err(NativeError::Foreign);
        }
        let active = self.active.as_ref().ok_or(NativeError::Invalid)?;
        if self.lease.is_none() || *active != Record::intent(intent, active.delete) {
            return Err(NativeError::Foreign);
        }
        let mut journal = self.load()?;
        if journal.records != self.records {
            return Err(NativeError::Foreign);
        }
        let record = journal
            .records
            .last_mut()
            .filter(|r| *r == active)
            .ok_or(NativeError::Foreign)?;
        record.outcome = Some(result_code(outcome));
        if active.delete && outcome == RuleResult::Absent {
            for r in &mut journal.records {
                if !r.delete && r.cidr == active.cidr && r.kind == active.kind {
                    r.retired = true;
                }
            }
        }
        self.save(proof, &journal)?;
        self.active = None;
        self.records = journal.records;
        self.lease = None;
        Ok(())
    }
}
#[derive(Debug)]
pub struct RemovalPlan {
    plan: FirewallPlan,
    receipt: AdmittedReceipt,
}
impl RemovalPlan {
    pub fn consent(&self, operation: OperationId, revision: u64) -> Result<FirewallConsent> {
        self.plan.consent(operation, revision)
    }
    pub fn preview(&self) -> String {
        format!(
            "{}\n{} {}",
            removal_command(&self.plan.intent, "pkexec"),
            if matches!(
                self.plan
                    .before
                    .presence(&self.plan.intent.link.cidr, self.plan.intent.kind),
                Presence::Unknown(_)
            ) {
                REMOVAL_LIMIT
            } else {
                ""
            },
            REMOVAL_TOCTOU
        )
    }
}
fn removal_command(intent: &FirewallIntent, launcher: &str) -> String {
    command_preview(intent, launcher).replacen("/usr/bin/ufw allow", "/usr/bin/ufw delete allow", 1)
}
fn removable(facts: &FirewallFacts, cidr: &LanCidr, kind: RuleKind) -> bool {
    let inventory = if cidr.as_str().contains(':') {
        &facts.ipv6
    } else {
        &facts.ipv4
    };
    // Positive evidence protects additions, but affected ambiguity must refuse deletion.
    !inventory.as_ref().is_ok_and(|i| i.uncertain)
        && matches!(
            facts.presence(cidr, kind),
            Presence::Owned
                | Presence::Absent
                | Presence::Unknown(InspectionIssue::Unreadable(NativeError::PermissionDenied))
        )
}
impl LinuxFirewall {
    pub fn plan_removal(
        &mut self,
        snapshot: &FirewallSnapshot,
        receipt: AdmittedReceipt,
        operation: OperationId,
        revision: u64,
    ) -> Result<RemovalPlan> {
        self.current = None;
        if receipt.target != binding(&self.io) {
            return Err(NativeError::Foreign.into());
        }
        let kind = receipt.record.kind;
        if self.unresolved[kind.index()] {
            return Err(FirewallError::CurrentRequired);
        }
        if !self.observed
            || !Arc::ptr_eq(&snapshot.owner, &self.owner)
            || snapshot.generation != self.generation
            || operation.0 == 0
        {
            return Err(FirewallError::Stale);
        }
        if snapshot.facts.manager != ManagerSelection::Ufw
            || snapshot.facts.activity != Activity::Active
        {
            return Err(FirewallError::Manual);
        }
        let cidr = LanCidr::parse(&receipt.record.cidr)?;
        if !removable(&snapshot.facts, &cidr, kind) {
            return Err(FirewallError::Kept);
        }
        let link = snapshot
            .facts
            .links
            .as_ref()
            .map_err(|_| FirewallError::Changed)?
            .iter()
            .find(|l| l.cidr == cidr && l.interface == receipt.record.interface)
            .ok_or(FirewallError::Changed)?
            .clone();
        self.serial = self.serial.checked_add(1).ok_or(FirewallError::Invalid)?;
        self.current = Some(self.serial);
        Ok(RemovalPlan {
            plan: FirewallPlan {
                intent: FirewallIntent {
                    operation,
                    revision,
                    target: self.io.target().paths().clone(),
                    link,
                    kind,
                },
                before: snapshot.facts.clone(),
                generation: snapshot.generation,
                serial: self.serial,
                owner: self.owner.clone(),
            },
            receipt,
        })
    }
    pub fn apply_removal(
        &mut self,
        proof: &SupportProof,
        manager: ManagerSelection,
        removal: RemovalPlan,
        consent: FirewallConsent,
        store: &mut DurableIntentStore,
        deadline: &Deadline,
    ) -> Result<FirewallResult> {
        let plan = removal.plan;
        let current = self.current.take();
        self.observed = false;
        if current != Some(plan.serial)
            || !Arc::ptr_eq(&plan.owner, &self.owner)
            || !Arc::ptr_eq(&consent.owner, &self.owner)
            || plan.generation != self.generation
            || consent.serial != plan.serial
            || consent.operation != plan.intent.operation
            || consent.revision != plan.intent.revision
        {
            return Err(FirewallError::Stale);
        }
        proof.check(&self.io)?;
        store.admitted(&removal.receipt)?;
        if manager != plan.before.manager
            || observe(self.reader.as_ref(), manager, deadline)? != plan.before
        {
            return Err(FirewallError::Changed);
        }
        store.begin(proof, &plan.intent, true)?;
        self.unresolved[plan.intent.kind.index()] = true;
        let preflight = (|| {
            let receipt = if plan
                .before
                .presence(&plan.intent.link.cidr, plan.intent.kind)
                == Presence::Absent
            {
                None
            } else {
                self.current_checks
                    .before_removal(self.io.target(), &plan.intent, deadline)?
            };
            store.admitted(&removal.receipt)?;
            if observe(self.reader.as_ref(), manager, deadline)? != plan.before {
                return Err(FirewallError::Changed);
            }
            proof.check(&self.io)?;
            deadline.check()?;
            self.current_checks
                .stamp_dispatch(&plan.intent, receipt, deadline)?;
            Ok(())
        })();
        if let Err(error) = preflight {
            store.record_outcome(proof, &plan.intent, RuleResult::NotDispatched)?;
            return Err(error);
        }
        let mut result = if plan
            .before
            .presence(&plan.intent.link.cidr, plan.intent.kind)
            == Presence::Absent
        {
            RuleResult::Absent
        } else {
            classify_result(
                self.io.pkexec_ufw(
                    proof,
                    UfwMutation {
                        delete: true,
                        cidr: plan.intent.link.cidr.clone(),
                        rule: plan.intent.kind.fields().2,
                    },
                    deadline,
                ),
                true,
            )
        };
        let after = observe(self.reader.as_ref(), manager, deadline);
        if after
            .as_ref()
            .is_ok_and(|f| f.links != plan.before.links || f.activity != plan.before.activity)
        {
            result = RuleResult::OutcomeUnknown;
        }
        let inventory = after
            .ok()
            .map(|f| f.presence(&plan.intent.link.cidr, plan.intent.kind))
            .unwrap_or(Presence::Unknown(InspectionIssue::Unreadable(
                NativeError::Unavailable,
            )));
        if result == RuleResult::PendingVerification && inventory == Presence::Absent {
            result = RuleResult::Absent;
        }
        if store.record_outcome(proof, &plan.intent, result).is_err() {
            result = RuleResult::OutcomeUnknown;
        }
        self.unresolved[plan.intent.kind.index()] = matches!(
            result,
            RuleResult::OutcomeUnknown | RuleResult::PromptUnavailable
        );
        Ok(FirewallResult {
            result,
            inventory,
            manual: (result == RuleResult::PromptUnavailable)
                .then(|| removal_command(&plan.intent, "sudo")),
        })
    }
}
