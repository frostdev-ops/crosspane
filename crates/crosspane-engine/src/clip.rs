//! Lazy clipboard admission and metadata only; content is moved straight between I/O values.
//! Peer promises clear native kinds without advancing the epoch and are never re-offered
//! onward: B holding A's promise cannot relay it to C; C needs its own offer path with A.
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{ClipKinds, ClipboardEvent, LocalPasteId, SessionEvent};
use crosspane_protocol::clip::{MAX_CLIP_IMAGE, MAX_CLIP_TEXT};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetch, ClipFetchFailed, ClipFetchId, ClipOffer, ClipOfferId,
    ClipWithdraw, ControlMessage,
};
use crosspane_types::{
    ClipKind,
    id::{NodeId, ProjectionId},
    time::MonoTime,
};

use crate::{Input, Output, ProjectionKey};

#[derive(Debug, Default)]
struct Peer {
    up: bool,
    available: bool,
    read: bool,
    write: bool,
    last_epoch: Option<u64>,
    high_offer: Option<u64>,
    // Retained through cancellation/reconnect: a late read must never acquire a new serve.
    high_fetch: Option<u64>,
    offer: Option<ClipOffer>,
    serves: BTreeMap<u64, Serve>,
}
#[derive(Debug)]
struct Serve {
    offer: ClipOfferId,
    kind: ClipKind,
}
#[derive(Debug)]
struct Paste {
    paste: LocalPasteId,
    kind: ClipKind,
    deadline: MonoTime,
}
#[derive(Debug)]
struct Promise {
    local: u64,
    peer: NodeId,
    remote: ClipOffer,
    pending: BTreeMap<u64, Paste>,
}
#[derive(Debug, Default)]
pub(crate) struct Clip {
    peers: BTreeMap<NodeId, Peer>,
    epoch: u64,
    kinds: ClipKinds,
    offer_id: u64,
    promise_id: u64,
    fetch_id: u64,
    promise: Option<Promise>,
    session_permits: bool,
    asleep: bool,
    engine_permits: bool,
    controller: Option<NodeId>,
    target: Option<NodeId>,
    source_focus: BTreeMap<ProjectionId, (NodeId, u64)>,
    destination_focus: BTreeSet<ProjectionKey>,
}
impl Clip {
    pub(crate) fn new() -> Self {
        Self {
            engine_permits: true,
            ..Self::default()
        }
    }
    pub(crate) fn next_deadline(&self) -> Option<MonoTime> {
        self.promise
            .as_ref()?
            .pending
            .values()
            .map(|p| p.deadline)
            .min()
    }
    fn gate(&self) -> bool {
        self.session_permits && !self.asleep && self.engine_permits
    }
    fn admitted(&self, peer: NodeId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.up && p.available)
    }
    pub(crate) fn engine_gate(&mut self, permits: bool, out: &mut Vec<Output>) {
        self.engine_permits = permits;
        if !permits {
            self.clear(out);
        }
    }
    pub(crate) fn handle(
        &mut self,
        input: Input,
        now: MonoTime,
        controller: Option<NodeId>,
        target: Option<NodeId>,
        e2: &crate::e2::E2,
        out: &mut Vec<Output>,
    ) {
        if let Some(p) = &mut self.promise {
            p.pending.retain(|_, pending| {
                if now < pending.deadline {
                    return true;
                }
                out.push(Output::ClipFulfil {
                    paste: pending.paste,
                    data: None,
                });
                false
            });
        }
        let changed = matches!(&input, Input::Clipboard(ClipboardEvent::Changed { .. }));
        match input {
            Input::PeerUp { peer } => self.peers.entry(peer).or_default().up = true,
            Input::ClipPeer { peer, available } => {
                if let Some(p) = self.peers.get_mut(&peer).filter(|p| p.up) {
                    p.available = available;
                    if !available {
                        self.clear_peer(peer, out);
                    }
                }
            }
            Input::Link(LinkEvent::Closed { peer, .. }) => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.up = false;
                    p.available = false;
                }
                self.clear_peer(peer, out);
            }
            Input::Grants(grants) => {
                for peer in grants.keys() {
                    self.peers.entry(*peer).or_default();
                }
                let peers: Vec<_> = self.peers.keys().copied().collect();
                for peer in peers {
                    let g = grants.get(&peer);
                    let read = g.is_some_and(|g| g.contains(&Capability::ClipboardRead));
                    let write = g.is_some_and(|g| g.contains(&Capability::ClipboardWrite));
                    if let Some(p) = self.peers.get_mut(&peer) {
                        p.read = read;
                        p.write = write;
                    }
                    if !read {
                        self.withdraw_offer(peer, out);
                        self.cancel_serves(peer);
                    }
                    if !write && self.promise.as_ref().is_some_and(|p| p.peer == peer) {
                        self.withdraw_promise(out);
                    }
                }
            }
            Input::Session(event) => {
                match event {
                    SessionEvent::State(s) => self.session_permits = s.permits_io(),
                    SessionEvent::WillSleep => {
                        self.asleep = true;
                        self.session_permits = false;
                    }
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.session_permits = false;
                    }
                    _ => self.session_permits = false,
                }
                if !self.gate() {
                    self.clear(out);
                }
            }
            Input::Clipboard(ClipboardEvent::Changed { kinds }) => {
                self.withdraw_promise(out);
                self.withdraw_offers(out);
                if let Some(epoch) = self.epoch.checked_add(1) {
                    self.epoch = epoch;
                    self.kinds = kinds;
                } else {
                    self.kinds = ClipKinds::default();
                }
            }
            Input::Clipboard(ClipboardEvent::PromiseLost { offer }) => {
                if self.promise.as_ref().is_some_and(|p| p.local == offer) {
                    self.withdraw_promise(out);
                }
            }
            Input::Clipboard(ClipboardEvent::PasteRequested { paste, offer, kind }) => {
                self.paste(paste, offer, kind, now, out);
            }
            Input::Link(LinkEvent::Control { peer, msg }) if self.admitted(peer) => match msg {
                ControlMessage::ClipOffer(offer) => self.receive_offer(peer, offer, out),
                ControlMessage::ClipWithdraw(w) => {
                    if self
                        .promise
                        .as_ref()
                        .is_some_and(|p| p.peer == peer && p.remote.offer == w.offer)
                    {
                        self.withdraw_promise(out);
                    }
                }
                ControlMessage::ClipFetch(fetch) => self.fetch(peer, fetch, out),
                ControlMessage::ClipFetchFailed(f) => self.answer(peer, f.fetch, None, out),
                _ => {}
            },
            Input::ClipReadDone {
                peer,
                fetch,
                result,
            } => {
                let serve = self
                    .peers
                    .get_mut(&peer)
                    .and_then(|p| p.serves.remove(&fetch.0));
                if let Some(s) = serve.filter(|_| self.admitted(peer)) {
                    let result = self.check(peer, s.offer, s.kind).map_or(result, Err);
                    match result {
                        Ok(data) if data.0.len() <= cap(s.kind) => out.push(Output::SendClipData {
                            peer,
                            fetch,
                            kind: s.kind,
                            data,
                        }),
                        other => Self::failed(
                            peer,
                            fetch,
                            other.err().unwrap_or(ClipFailure::TooLarge),
                            out,
                        ),
                    }
                }
            }
            Input::ClipData {
                peer,
                fetch,
                kind,
                data,
            } => {
                let matches = self.admitted(peer)
                    && self.gate()
                    && self.promise.as_ref().is_some_and(|p| {
                        p.peer == peer && p.pending.get(&fetch.0).is_some_and(|p| p.kind == kind)
                    });
                if matches && data.0.len() <= cap(kind) {
                    self.answer(peer, fetch, Some(data), out);
                }
            }
            _ => {}
        }
        if controller != self.controller
            && let Some(peer) = controller
        {
            self.offer(peer, out);
        }
        if target.is_none()
            && let Some(peer) = self.target
        {
            self.offer(peer, out);
        }
        if changed {
            for peer in [controller, target].into_iter().flatten() {
                self.offer(peer, out);
            }
        }
        self.controller = controller;
        self.target = target;
        self.e2_triggers(e2, out);
    }
    fn e2_triggers(&mut self, e2: &crate::e2::E2, out: &mut Vec<Output>) {
        let sources: BTreeMap<_, _> = e2.clipboard_source_focus().collect();
        for (projection, (peer, epoch)) in std::mem::take(&mut self.source_focus) {
            if sources.get(&projection) == Some(&peer) {
                self.source_focus.insert(projection, (peer, epoch));
            } else if self.epoch != epoch {
                self.offer(peer, out);
            }
        }
        for (projection, peer) in sources {
            self.source_focus
                .entry(projection)
                .or_insert((peer, self.epoch));
        }
        let destinations: BTreeSet<_> = e2.clipboard_destination_focus().collect();
        let gained: Vec<_> = destinations
            .difference(&self.destination_focus)
            .copied()
            .collect();
        for key in gained {
            self.offer(key.source, out);
        }
        self.destination_focus = destinations;
    }
    fn offer(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        if !self.admitted(peer) || !self.gate() || (!self.kinds.text && !self.kinds.image) {
            return;
        }
        let Some(p) = self
            .peers
            .get_mut(&peer)
            .filter(|p| p.read && p.last_epoch != Some(self.epoch))
        else {
            return;
        };
        let Some(id) = next(&mut self.offer_id) else {
            return;
        };
        let kinds = [
            (ClipKind::Text, self.kinds.text),
            (ClipKind::Image, self.kinds.image),
        ]
        .into_iter()
        .filter_map(|(k, present)| present.then_some(k))
        .collect();
        let offer = ClipOffer {
            offer: ClipOfferId(id),
            kinds,
        };
        p.last_epoch = Some(self.epoch);
        p.offer = Some(offer.clone());
        send(peer, ControlMessage::ClipOffer(offer), out);
    }
    fn check(&self, peer: NodeId, offer: ClipOfferId, kind: ClipKind) -> Option<ClipFailure> {
        if !self.peers.get(&peer).is_some_and(|p| p.read) {
            Some(ClipFailure::NotGranted)
        } else if !self.gate() {
            Some(ClipFailure::Locked)
        } else if !self
            .peers
            .get(&peer)
            .and_then(|p| p.offer.as_ref())
            .is_some_and(|o| o.offer == offer && o.kinds.contains(&kind))
        {
            Some(ClipFailure::Expired)
        } else {
            None
        }
    }
    fn fetch(&mut self, peer: NodeId, fetch: ClipFetch, out: &mut Vec<Output>) {
        let mut failure = self.check(peer, fetch.offer, fetch.kind);
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        if failure.is_none() {
            if p.high_fetch.is_some_and(|h| fetch.fetch.0 <= h) {
                failure = Some(ClipFailure::Unavailable);
            } else {
                p.high_fetch = Some(fetch.fetch.0);
                if p.serves.len() >= 2 {
                    failure = Some(ClipFailure::Unavailable);
                }
            }
        }
        if let Some(reason) = failure {
            Self::failed(peer, fetch.fetch, reason, out);
            return;
        }
        p.serves.insert(
            fetch.fetch.0,
            Serve {
                offer: fetch.offer,
                kind: fetch.kind,
            },
        );
        out.push(Output::ClipRead {
            peer,
            fetch: fetch.fetch,
            kind: fetch.kind,
            max_bytes: cap(fetch.kind),
        });
    }
    fn receive_offer(&mut self, peer: NodeId, offer: ClipOffer, out: &mut Vec<Output>) {
        if offer.kinds.is_empty()
            || offer.kinds.len() > 2
            || (offer.kinds.len() == 2 && offer.kinds[0] == offer.kinds[1])
        {
            return;
        }
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        if p.high_offer.is_some_and(|h| offer.offer.0 <= h) {
            return;
        }
        // A valid ID seen while locked or ungranted cannot be replayed after admission changes.
        p.high_offer = Some(offer.offer.0);
        if !p.write || !self.gate() {
            return;
        }
        let Some(local) = next(&mut self.promise_id) else {
            return;
        };
        self.withdraw_promise(out);
        self.withdraw_offers(out);
        self.kinds = ClipKinds::default();
        let kinds = ClipKinds {
            text: offer.kinds.contains(&ClipKind::Text),
            image: offer.kinds.contains(&ClipKind::Image),
        };
        self.promise = Some(Promise {
            local,
            peer,
            remote: offer,
            pending: BTreeMap::new(),
        });
        out.push(Output::ClipPromise {
            offer: local,
            kinds,
        });
    }
    fn paste(
        &mut self,
        paste: LocalPasteId,
        offer: u64,
        kind: ClipKind,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let valid = self.gate()
            && self.promise.as_ref().is_some_and(|p| {
                p.local == offer
                    && p.remote.kinds.contains(&kind)
                    && self.admitted(p.peer)
                    && self.peers.get(&p.peer).is_some_and(|p| p.write)
            });
        if valid
            && let Some(fetch) = next(&mut self.fetch_id)
            && let Some(p) = &mut self.promise
        {
            p.pending.insert(
                fetch,
                Paste {
                    paste,
                    kind,
                    deadline: now.saturating_add(Duration::from_secs(2)),
                },
            );
            send(
                p.peer,
                ControlMessage::ClipFetch(ClipFetch {
                    fetch: ClipFetchId(fetch),
                    offer: p.remote.offer,
                    kind,
                }),
                out,
            );
        } else {
            out.push(Output::ClipFulfil { paste, data: None });
        }
    }
    fn answer(
        &mut self,
        peer: NodeId,
        fetch: ClipFetchId,
        data: Option<crate::io::ClipBytes>,
        out: &mut Vec<Output>,
    ) {
        if let Some(p) = self.promise.as_mut().filter(|p| p.peer == peer)
            && let Some(pending) = p.pending.remove(&fetch.0)
        {
            out.push(Output::ClipFulfil {
                paste: pending.paste,
                data,
            });
        }
    }
    fn failed(peer: NodeId, fetch: ClipFetchId, reason: ClipFailure, out: &mut Vec<Output>) {
        send(
            peer,
            ControlMessage::ClipFetchFailed(ClipFetchFailed { fetch, reason }),
            out,
        );
    }
    fn withdraw_offer(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        let offer = self.peers.get_mut(&peer).and_then(|p| p.offer.take());
        if self.admitted(peer)
            && let Some(o) = offer
        {
            send(
                peer,
                ControlMessage::ClipWithdraw(ClipWithdraw { offer: o.offer }),
                out,
            );
        }
    }
    fn cancel_serves(&mut self, peer: NodeId) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.serves.clear();
        }
    }
    fn withdraw_offers(&mut self, out: &mut Vec<Output>) {
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.withdraw_offer(peer, out);
        }
    }
    fn withdraw_promise(&mut self, out: &mut Vec<Output>) {
        if let Some(p) = self.promise.take() {
            out.push(Output::ClipWithdraw { offer: p.local });
            for paste in p.pending.into_values() {
                out.push(Output::ClipFulfil {
                    paste: paste.paste,
                    data: None,
                });
            }
        }
    }
    fn clear_peer(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        self.withdraw_offer(peer, out);
        self.cancel_serves(peer);
        if self.promise.as_ref().is_some_and(|p| p.peer == peer) {
            self.withdraw_promise(out);
        }
    }
    fn clear(&mut self, out: &mut Vec<Output>) {
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.clear_peer(peer, out);
        }
    }
}
fn next(counter: &mut u64) -> Option<u64> {
    *counter = counter.checked_add(1)?;
    Some(*counter)
}
fn send(peer: NodeId, msg: ControlMessage, out: &mut Vec<Output>) {
    out.push(Output::SendControl { peer, msg });
}
fn cap(kind: ClipKind) -> usize {
    match kind {
        ClipKind::Text => MAX_CLIP_TEXT as usize,
        ClipKind::Image => MAX_CLIP_IMAGE as usize,
    }
}
