//! Durable journal record of the elevated setup (WP-W4.1c2 C3). OS-free: every rule here is a pure
//! function of the record. The store that reads and publishes it lives in the Windows installer.
use super::status::{JournalEntry, JournalPhase, intent_entry};
use super::{AgentProgram, ElevatedError, InstallId, RuleScope, Verb, VerbName};
use crate::{MutationOutcome, ResourceObservation, ResourceOwnership};

pub const RECORD_SCHEMA: u32 = 1;
pub const RECORD_LEAF: &str = "elevated-setup.json";
pub const RECORD_KIND: &str = "elevated-setup";
pub const MAX_ENTRIES: usize = 32;

/// The journal of one install: its install id, the rule scope it was made for, and the Intent and
/// Outcome entries of the journaled verbs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElevatedRecord {
    pub schema: u32,
    pub install_id: InstallId,
    pub program: AgentProgram,
    pub entries: Vec<JournalEntry>,
}

impl ElevatedRecord {
    /// An empty record for `scope`.
    pub fn new(scope: &RuleScope) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            install_id: scope.id.clone(),
            program: scope.program.clone(),
            entries: Vec::new(),
        }
    }

    /// The rule scope this record was made for.
    pub fn scope(&self) -> RuleScope {
        RuleScope {
            id: self.install_id.clone(),
            program: self.program.clone(),
        }
    }

    /// Checks the schema, the bound, every entry's identity and shape, and the Intent/Outcome
    /// protocol replayed in order. Any failure is `Record`.
    pub fn validate(&self) -> Result<(), ElevatedError> {
        let mut pending = Vec::new();
        let valid = self.schema == RECORD_SCHEMA
            && self.entries.len() <= MAX_ENTRIES
            && self.entries.iter().all(|entry| {
                self.identifies(entry) && open_intent(entry) && advance(&mut pending, entry)
            });
        if valid {
            Ok(())
        } else {
            Err(ElevatedError::Record)
        }
    }

    /// Adds `entry` if the protocol accepts it and the record stays valid. Past `MAX_ENTRIES` the
    /// earliest settled pair that does not include `entry` is dropped first, as many times as
    /// needed. Any refusal is `Journal`, and the record is then unchanged.
    pub fn append(&mut self, entry: &JournalEntry) -> Result<(), ElevatedError> {
        let mut next = self.clone();
        next.entries.push(entry.clone());
        while next.entries.len() > MAX_ENTRIES {
            if !next.drop_settled_pair() {
                return Err(ElevatedError::Journal);
            }
        }
        next.validate().map_err(|_| ElevatedError::Journal)?;
        *self = next;
        Ok(())
    }

    /// The verbs with an Intent and no Outcome yet, in the order their Intents were written.
    /// Meaningful for a validated record.
    pub fn pending(&self) -> Vec<VerbName> {
        let mut pending = Vec::new();
        for entry in &self.entries {
            advance(&mut pending, entry);
        }
        pending
    }

    /// The pending verbs rebuilt with this record's scope, for observation-only settlement.
    pub fn pending_verbs(&self) -> Vec<Verb> {
        self.pending()
            .into_iter()
            .filter_map(|name| self.scoped_verb(name))
            .collect()
    }

    /// The verb `name` runs with this record's scope. `None` for every name `journaled` refuses.
    fn scoped_verb(&self, name: VerbName) -> Option<Verb> {
        match name {
            VerbName::AddFirewall => Some(Verb::AddFirewall(self.scope())),
            VerbName::RemoveFirewall => Some(Verb::RemoveFirewall(self.scope())),
            VerbName::InstallDriver => Some(Verb::InstallDriver),
            VerbName::RemoveDriver => Some(Verb::RemoveDriver),
            VerbName::Status | VerbName::Setup | VerbName::Teardown => None,
        }
    }

    /// Whether `entry` belongs to a journaled verb and carries the receipt identity that verb
    /// writes: ownership `Created`, its `resource_id`, and its `resolved_path` (this install's rule
    /// name for firewall verbs, the hardware ID for driver verbs).
    fn identifies(&self, entry: &JournalEntry) -> bool {
        let Some(verb) = self.scoped_verb(entry.verb) else {
            return false;
        };
        let expected = intent_entry(&verb, None).receipt;
        entry.receipt.ownership == ResourceOwnership::Created
            && entry.receipt.resource_id == expected.resource_id
            && entry.receipt.resolved_path == expected.resolved_path
    }

    /// Removes the earliest settled pair that does not include the last entry: an Intent and the
    /// next entry for the same verb, when that entry is its Outcome. The last entry is the one
    /// `append` is adding, so its pair is never chosen. `false` when there is no such pair.
    fn drop_settled_pair(&mut self) -> bool {
        let settled = self.entries.iter().enumerate().find_map(|(first, entry)| {
            if entry.phase != JournalPhase::Intent {
                return None;
            }
            let (second, later) = self
                .entries
                .iter()
                .enumerate()
                .skip(first + 1)
                .find(|(_, later)| later.verb == entry.verb)?;
            (later.phase == JournalPhase::Outcome && second + 1 < self.entries.len())
                .then_some((first, second))
        });
        let Some((first, second)) = settled else {
            return false;
        };
        self.entries = std::mem::take(&mut self.entries)
            .into_iter()
            .enumerate()
            .filter(|(index, _)| *index != first && *index != second)
            .map(|(_, entry)| entry)
            .collect();
        true
    }
}

/// The verbs the journal records: the firewall and driver mutations. Combined verbs run as their
/// parts, and `status` only observes.
pub fn journaled(verb: VerbName) -> bool {
    matches!(
        verb,
        VerbName::AddFirewall
            | VerbName::RemoveFirewall
            | VerbName::InstallDriver
            | VerbName::RemoveDriver
    )
}

/// Applies `entry` to the pending verbs: an Intent makes its verb pending and an Outcome settles
/// it. Returns `false`, changing nothing, when the protocol refuses the entry (an Intent for a
/// verb that is pending, or an Outcome for a verb that is not).
fn advance(pending: &mut Vec<VerbName>, entry: &JournalEntry) -> bool {
    let is_pending = pending.contains(&entry.verb);
    match entry.phase {
        JournalPhase::Intent if !is_pending => {
            pending.push(entry.verb);
            true
        }
        JournalPhase::Outcome if is_pending => {
            pending.retain(|verb| *verb != entry.verb);
            true
        }
        JournalPhase::Intent | JournalPhase::Outcome => false,
    }
}

/// An Intent is written before its verb runs, so it has no exit code and no observation after.
fn open_intent(entry: &JournalEntry) -> bool {
    entry.phase != JournalPhase::Intent
        || (entry.exit.is_none()
            && entry.receipt.after == ResourceObservation::Unknown
            && entry.receipt.outcome == MutationOutcome::Unknown)
}
