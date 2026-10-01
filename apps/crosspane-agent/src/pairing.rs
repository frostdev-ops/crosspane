//! SAS pairing (04 §3, WP-1.3 state machines over the WP-1.38 pairing endpoint).
//!
//! - **Listen** (`crosspanectl pair listen`): opens a 120 s pairing window on port + 1. The first
//!   device that connects runs the exchange as the *joiner*; this node is the *initiator*, shows
//!   the six-digit code and asks the user to confirm.
//! - **Join** (`crosspanectl pair join ADDR`): dials the other node's pairing window, shows three
//!   candidate codes, and the user picks the one the other screen shows.
//!
//! A peer is pinned only after the exchange succeeds **and** the SPKI it revealed is the one the
//! TLS handshake proved possession of (the channel's `peer_spki`). The SAS is bound to that TLS
//! session through the exporter, so a relay between two sessions shows different codes.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crosspane_protocol::msg::Capability;
use crosspane_security::identity::DeviceIdentity;
use crosspane_security::pairing::{Event, Initiator, Joiner, Local, Peer, Sas};
use crosspane_security::rng::SystemRng;
use crosspane_security::trust::default_grants;
use crosspane_transport::pairing::{PAIRING_PORT_OFFSET, PairingChannel, PairingListener, pair_connect};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::agent::Event as AgentEvent;

/// How long a pairing window stays open (04 §3).
const WINDOW: Duration = Duration::from_secs(120);

/// What `crosspanectl pair status` shows.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PairStatus {
    /// idle, listening, connecting, confirm (initiator: show `sas`, wait for yes/no), pick
    /// (joiner: show `candidates`, wait for a pick), waiting, paired, failed.
    pub phase: String,
    pub sas: Option<String>,
    pub candidates: Vec<String>,
    pub peer: Option<String>,
    pub error: Option<String>,
}

/// A user decision, delivered to the running exchange.
#[derive(Debug)]
pub enum Decision {
    Confirm(bool),
    Pick(usize),
}

/// Shared between the control socket handlers and the exchange task.
#[derive(Clone, Debug, Default)]
pub struct Pairing {
    status: Arc<Mutex<PairStatus>>,
    decisions: Arc<Mutex<Option<mpsc::UnboundedSender<Decision>>>>,
}

/// What a successful pairing hands the engine loop to pin.
#[derive(Debug)]
pub struct Paired {
    pub peer: Peer,
    pub granted: std::collections::BTreeSet<Capability>,
    /// The joiner dials the initiator's normal port afterwards.
    pub dial: Option<SocketAddr>,
}

impl Pairing {
    pub fn status(&self) -> PairStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(&self, f: impl FnOnce(&mut PairStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    fn busy(&self) -> bool {
        let phase = self.status().phase;
        !matches!(phase.as_str(), "" | "idle" | "paired" | "failed")
    }

    pub fn decide(&self, decision: Decision) -> Result<(), String> {
        let tx = self.decisions.lock().map_err(|_| "poisoned")?.clone();
        tx.ok_or_else(|| "no pairing is waiting for a decision".to_owned())?
            .send(decision)
            .map_err(|_| "the pairing exchange has ended".to_owned())
    }

    /// Open a pairing window on `port + 1`.
    pub fn listen(
        &self,
        runtime: &tokio::runtime::Handle,
        port: u16,
        identity: Arc<DeviceIdentity>,
        name: String,
        allow_input: bool,
        events: std::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("a pairing is already in progress".into());
        }
        let this = self.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        *self.decisions.lock().map_err(|_| "poisoned")? = Some(tx);
        self.set(|s| *s = PairStatus { phase: "listening".into(), ..PairStatus::default() });
        let addr = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port.saturating_add(PAIRING_PORT_OFFSET)));
        runtime.spawn(async move {
            let result = async {
                let listener = PairingListener::bind(addr, identity.clone()).map_err(|e| e.to_string())?;
                let channel = tokio::time::timeout(WINDOW, listener.accept())
                    .await
                    .map_err(|_| "the pairing window closed without a connection".to_owned())?
                    .map_err(|e| e.to_string())?;
                drop(listener); // one exchange per window
                run(&this, channel, identity, name, allow_input, true, rx, None).await
            }
            .await;
            finish(&this, result, &events);
        });
        Ok(())
    }

    /// Join the pairing window at `addr` (the other node's normal address; the pairing port is
    /// derived).
    pub fn join(
        &self,
        runtime: &tokio::runtime::Handle,
        addr: SocketAddr,
        identity: Arc<DeviceIdentity>,
        name: String,
        allow_input: bool,
        events: std::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("a pairing is already in progress".into());
        }
        let this = self.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        *self.decisions.lock().map_err(|_| "poisoned")? = Some(tx);
        self.set(|s| *s = PairStatus { phase: "connecting".into(), ..PairStatus::default() });
        let pairing_addr = SocketAddr::new(addr.ip(), addr.port().saturating_add(PAIRING_PORT_OFFSET));
        runtime.spawn(async move {
            let result = async {
                let channel = pair_connect(pairing_addr, identity.clone()).await.map_err(|e| e.to_string())?;
                run(&this, channel, identity, name, allow_input, false, rx, Some(addr)).await
            }
            .await;
            finish(&this, result, &events);
        });
        Ok(())
    }
}

fn finish(this: &Pairing, result: Result<Paired, String>, events: &std::sync::mpsc::Sender<AgentEvent>) {
    if let Ok(mut d) = this.decisions.lock() {
        *d = None;
    }
    match result {
        Ok(paired) => {
            let name = paired.peer.name.clone();
            this.set(|s| {
                s.phase = "paired".into();
                s.peer = Some(name);
            });
            let _ = events.send(AgentEvent::Paired(paired));
        }
        Err(error) => {
            tracing::info!(%error, "pairing failed");
            this.set(|s| {
                s.phase = "failed".into();
                s.error = Some(error);
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    this: &Pairing,
    mut channel: PairingChannel,
    identity: Arc<DeviceIdentity>,
    name: String,
    allow_input: bool,
    initiator: bool,
    mut decisions: mpsc::UnboundedReceiver<Decision>,
    dial: Option<SocketAddr>,
) -> Result<Paired, String> {
    let mut granted = default_grants();
    if allow_input {
        granted.insert(Capability::InputAccept);
    }
    let local = Local {
        spki: identity.spki().to_vec(),
        name,
        grants: granted.iter().copied().collect(),
    };
    let exporter = channel.exporter();
    let mut rng = SystemRng;
    enum Role {
        I(Initiator),
        J(Joiner),
    }
    let (mut role, mut pending) = if initiator {
        let (m, ev) = Initiator::start(local, exporter, &mut rng);
        (Role::I(m), ev)
    } else {
        let (m, ev) = Joiner::start(local, exporter, &mut rng);
        (Role::J(m), ev)
    };
    let mut candidates: Vec<Sas> = Vec::new();
    loop {
        let mut need_input = false;
        let mut need_message = true;
        for event in std::mem::take(&mut pending) {
            match event {
                Event::Send(msg) => channel.send(&msg).await.map_err(|e| e.to_string())?,
                Event::ShowSas(sas) => {
                    this.set(|s| {
                        s.phase = "waiting".into();
                        s.sas = Some(sas.to_string());
                    });
                }
                Event::ShowCandidates(c) => {
                    candidates = c.to_vec();
                    this.set(|s| {
                        s.phase = "pick".into();
                        s.candidates = c.iter().map(Sas::to_string).collect();
                    });
                    need_input = true;
                    need_message = false;
                }
                Event::AskConfirm => {
                    this.set(|s| s.phase = "confirm".into());
                    need_input = true;
                    need_message = false;
                }
                Event::Paired(peer) => {
                    // Pin only the key TLS proved possession of.
                    if peer.spki != channel.peer_spki() {
                        channel.close("key mismatch").await;
                        return Err("the revealed key differs from the TLS key: refusing to pair".into());
                    }
                    channel.close("paired").await;
                    return Ok(Paired { peer, granted, dial });
                }
                Event::Failed(error) => {
                    channel.close("failed").await;
                    return Err(format!("{error:?}"));
                }
                _ => {}
            }
        }
        if need_input {
            let decision = tokio::time::timeout(WINDOW, decisions.recv())
                .await
                .map_err(|_| "no decision in time".to_owned())?
                .ok_or_else(|| "cancelled".to_owned())?;
            pending = match (&mut role, decision) {
                (Role::I(m), Decision::Confirm(accept)) => m.user_confirmed(accept),
                (Role::J(m), Decision::Pick(index)) => {
                    let pick = candidates.get(index).copied().ok_or("no such candidate")?;
                    m.user_picked(pick)
                }
                _ => return Err("that decision doesn't fit this pairing".into()),
            };
        } else if need_message {
            let msg = channel.recv().await.map_err(|e| e.to_string())?;
            pending = match &mut role {
                Role::I(m) => m.on_message(msg),
                Role::J(m) => m.on_message(msg),
            };
        }
    }
}
