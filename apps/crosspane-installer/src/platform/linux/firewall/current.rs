//! Current evidence only; 4.21 owns acquisition and the single receipt/dispatch clock.
use super::*;
use crate::agent_contract::{
    AgentCall, AgentReply, DecodedReply, InstallerRequest, InstallerStatusV1, InstanceStatus,
    PeerStatus, StatusAdmission,
};
use crosspane_types::id::NodeId;
use std::net::SocketAddr;

#[derive(Clone)]
pub struct CurrentObservations {
    pub peer: NodeId,
    pub connection: AgentReply,
    pub discovery: AgentReply,
}
impl std::fmt::Debug for CurrentObservations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurrentObservations")
            .finish_non_exhaustive()
    }
}
/// Fresh, correlated replies from the same admitted selected agent, never cached status.
/// Acquisition honors the deadline. Receipts and dispatch stamps MUST
/// use one caller clock, retained across reader replacement. This module owns no clock/port.
pub trait CurrentReader {
    fn read(
        &mut self,
        target: &LinuxTarget,
        link: &LanLink,
        peer: NodeId,
        deadline: &Deadline,
    ) -> NativeResult<CurrentObservations>;
    fn dispatch_stamp_ms(&self) -> NativeResult<u64>;
}
#[derive(Clone)]
pub struct DialSequence {
    pub peer: NodeId,
    pub address: SocketAddr,
    pub before: AgentReply,
    pub call: AgentCall,
    pub acknowledgement: AgentReply,
    pub connection: AgentReply,
    pub discovery: AgentReply,
}
impl std::fmt::Debug for DialSequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialSequence").finish_non_exhaustive()
    }
}
#[derive(Clone)]
pub struct TrafficEvidence {
    peer: NodeId,
    link: LanLink,
    instance: InstanceStatus,
    generation: u64,
    call_id: u64,
    receipts: [u64; 4],
    target: TargetPaths,
    source: ObservationSource,
}
impl std::fmt::Debug for TrafficEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrafficEvidence").finish_non_exhaustive()
    }
}
fn status<'a>(
    target: &LinuxTarget,
    peer: NodeId,
    reply: &'a AgentReply,
) -> NativeResult<(&'a InstallerStatusV1, &'a PeerStatus)> {
    let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &reply.result else {
        return Err(NativeError::Unavailable);
    };
    let s = health.installer();
    if reply.id == 0
        || reply.source != target.source()
        || s.instance.uid != target.paths().uid
        || std::path::Path::new(&s.instance.exe) != target.agent_path()
        || std::path::Path::new(&s.instance.runtime_dir) != target.runtime_dir()
    {
        return Err(NativeError::Foreign);
    }
    let p = s
        .peers
        .iter()
        .find(|p| p.node == peer)
        .ok_or(NativeError::Unavailable)?;
    Ok((s, p))
}
impl TrafficEvidence {
    /// Proves the peer was reachable over the selected link after the dial; does not prove the
    /// dial itself completed. Ack is submission only; concurrent inbound reachability is valid.
    pub fn after_dial(
        target: &LinuxTarget,
        link: LanLink,
        sequence: DialSequence,
    ) -> NativeResult<Self> {
        let (before, old) = status(target, sequence.peer, &sequence.before)?;
        let (connected, peer) = status(target, sequence.peer, &sequence.connection)?;
        let (discovery, found) = status(target, sequence.peer, &sequence.discovery)?;
        let generation = peer.link_generation.ok_or(NativeError::Unavailable)?;
        let ack = &sequence.acknowledgement;
        if sequence.call.id == 0
            || !(1..=crate::agent_contract::MAX_TIMEOUT_MS).contains(&sequence.call.timeout_ms)
            || sequence.call.request
                != (InstallerRequest::Dial {
                    addr: sequence.address,
                })
            || [
                sequence.before.id,
                ack.id,
                sequence.connection.id,
                sequence.discovery.id,
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
                != 4
            || ack.id != sequence.call.id
            || ack.source != target.source()
            || ack.result != Ok(DecodedReply::Acknowledged)
            || sequence.before.observed_at_ms >= ack.observed_at_ms
            || ack.observed_at_ms >= sequence.connection.observed_at_ms
            || sequence.connection.observed_at_ms >= sequence.discovery.observed_at_ms
            || before.instance != connected.instance
            || connected.instance != discovery.instance
            || !peer.connected
            || !found.connected
            || found.link_generation != Some(generation)
            || old.link_generation.is_some_and(|g| g >= generation)
            || (old.connected && old.link_generation.is_none())
            || !discovery.discovery.enabled
            || discovery.discovery.error.is_none()
            || crate::agent_contract::encode_request(&sequence.call.request).is_err()
        {
            return Err(NativeError::Unavailable);
        }
        Ok(Self {
            peer: sequence.peer,
            link,
            instance: connected.instance.clone(),
            generation,
            call_id: sequence.call.id,
            receipts: [
                sequence.before.observed_at_ms,
                ack.observed_at_ms,
                sequence.connection.observed_at_ms,
                sequence.discovery.observed_at_ms,
            ],
            target: target.paths().clone(),
            source: target.source(),
        })
    }
    fn matches(
        &self,
        target: &LinuxTarget,
        link: &LanLink,
        observations: &CurrentObservations,
    ) -> bool {
        let (Ok((connection, peer)), Ok((discovery, found))) = (
            status(target, self.peer, &observations.connection),
            status(target, self.peer, &observations.discovery),
        ) else {
            return false;
        };
        self.peer == observations.peer
            && &self.link == link
            && self.target == *target.paths()
            && self.source == target.source()
            && connection.instance == self.instance
            && discovery.instance == self.instance
            && peer.connected
            && found.connected
            && peer.link_generation == Some(self.generation)
            && found.link_generation == Some(self.generation)
            && observations.connection.observed_at_ms >= self.receipts[3]
            && observations.discovery.observed_at_ms >= observations.connection.observed_at_ms
            && discovery.discovery.enabled
            && discovery.discovery.error.is_some()
    }
}
#[derive(Default)]
pub(super) struct CurrentState {
    reader: Option<(NodeId, Box<dyn CurrentReader>)>,
    traffic: Option<TrafficEvidence>,
    watermarks: [Option<u64>; 2],
    latest_receipt: Option<u64>,
    // Call-ID watermark and all four receipts survive retirement and reader replacement.
    admitted: Option<(u64, [u64; 4])>,
}
impl CurrentState {
    fn read(
        &mut self,
        target: &LinuxTarget,
        link: &LanLink,
        deadline: &Deadline,
    ) -> NativeResult<CurrentObservations> {
        let result = (|| {
            deadline.check()?;
            let (peer, reader) = self.reader.as_mut().ok_or(NativeError::Unavailable)?;
            let o = reader.read(target, link, *peer, deadline)?;
            let prior = self.latest_receipt;
            let a = status(target, *peer, &o.connection);
            let b = status(target, *peer, &o.discovery);
            // Retain individually admitted receipts even when the combined batch is refused.
            for (reply, valid) in [(&o.connection, a.is_ok()), (&o.discovery, b.is_ok())] {
                if valid {
                    self.latest_receipt = Some(
                        self.latest_receipt
                            .map_or(reply.observed_at_ms, |t| t.max(reply.observed_at_ms)),
                    );
                }
            }
            deadline.check()?;
            let (a, p) = a?;
            let (b, q) = b?;
            if o.peer != *peer
                || a.instance != b.instance
                || p.connected != q.connected
                || p.link_generation != q.link_generation
                || o.discovery.observed_at_ms < o.connection.observed_at_ms
                || prior.is_some_and(|t| o.connection.observed_at_ms < t)
            {
                return Err(NativeError::Unavailable);
            }
            Ok(o)
        })();
        if let Ok(o) = &result {
            if self
                .traffic
                .as_ref()
                .is_some_and(|t| !t.matches(target, link, o))
            {
                self.traffic = None;
            }
        } else {
            self.traffic = None;
        }
        result
    }
    pub(super) fn mdns(&self, link: Option<&LanLink>) -> Result<()> {
        if self.reader.is_none()
            || self
                .traffic
                .as_ref()
                .is_none_or(|t| link.is_some_and(|l| &t.link != l))
        {
            return Err(FirewallError::MdnsPending);
        }
        Ok(())
    }
    pub(super) fn before_dispatch(
        &mut self,
        target: &LinuxTarget,
        intent: &FirewallIntent,
        deadline: &Deadline,
    ) -> Result<()> {
        // First LAN remains b1-compatible. An unknown watermark never readmits an unresolved kind.
        if self.reader.is_none() && intent.kind == RuleKind::Lan {
            return Ok(());
        }
        let o = self
            .read(target, &intent.link, deadline)
            .map_err(|_| FirewallError::CurrentRequired)?;
        if intent.kind == RuleKind::Mdns && self.traffic.is_none() {
            return Err(FirewallError::CurrentRequired);
        }
        let (_, reader) = self.reader.as_ref().ok_or(FirewallError::CurrentRequired)?;
        let stamp = reader
            .dispatch_stamp_ms()
            .map_err(|_| FirewallError::CurrentRequired)?;
        if stamp < o.discovery.observed_at_ms
            || self.watermarks.iter().flatten().any(|t| stamp < *t)
        {
            return Err(FirewallError::CurrentRequired);
        }
        deadline.check()?;
        self.watermarks[intent.kind.index()] = Some(stamp);
        Ok(())
    }
}
impl LinuxFirewall {
    /// Retains replay records and the clock domain; 4.21 keeps call IDs increasing across replacement.
    pub fn install_current_reader(&mut self, peer: NodeId, reader: Box<dyn CurrentReader>) {
        self.current = None;
        self.current_checks.reader = Some((peer, reader));
        self.current_checks.traffic = None;
    }
    fn selected_current_link(&self, snapshot: &FirewallSnapshot, link: &LanLink) -> Result<()> {
        if !self.observed
            || !Arc::ptr_eq(&snapshot.owner, &self.owner)
            || snapshot.generation != self.generation
            || !snapshot
                .facts
                .links
                .as_ref()
                .is_ok_and(|links| links.contains(link))
        {
            return Err(FirewallError::Stale);
        }
        Ok(())
    }
    pub fn admit_dial(
        &mut self,
        snapshot: &FirewallSnapshot,
        evidence: TrafficEvidence,
    ) -> Result<()> {
        self.current = None;
        self.current_checks.traffic = None;
        self.selected_current_link(snapshot, &evidence.link)?;
        if self
            .current_checks
            .reader
            .as_ref()
            .is_none_or(|(peer, _)| *peer != evidence.peer)
            || evidence.target != *self.io.target().paths()
            || evidence.source != self.io.target().source()
            || self
                .current_checks
                .admitted
                .is_some_and(|(id, _)| evidence.call_id <= id)
            || self
                .current_checks
                .latest_receipt
                .is_some_and(|t| evidence.receipts.iter().any(|receipt| *receipt <= t))
        {
            return Err(FirewallError::MdnsPending);
        }
        self.current_checks.latest_receipt = Some(evidence.receipts[3]);
        self.current_checks.admitted = Some((evidence.call_id, evidence.receipts));
        self.current_checks.traffic = Some(evidence);
        Ok(())
    }
    /// Fresh firewall detect plus two later current receipts are both required. No known stamp
    /// leaves an unresolved kind blocked; neither installing a reader nor re-detecting resets it.
    pub fn refresh_current(
        &mut self,
        snapshot: &FirewallSnapshot,
        link: &LanLink,
        deadline: &Deadline,
    ) -> Result<()> {
        self.current = None;
        self.selected_current_link(snapshot, link)?;
        let o = self
            .current_checks
            .read(self.io.target(), link, deadline)
            .map_err(|_| FirewallError::CurrentRequired)?;
        for kind in [RuleKind::Lan, RuleKind::Mdns] {
            if self.current_checks.watermarks[kind.index()].is_some_and(|stamp| {
                o.connection.observed_at_ms > stamp && o.discovery.observed_at_ms > stamp
            }) {
                self.unresolved[kind.index()] = false;
            }
        }
        Ok(())
    }
}
