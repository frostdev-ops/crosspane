//! OS-free commit-reveal pairing with number matching (04 §2).
//!
//! The caller supplies the exporter from this pairing's TLS connection: label
//! [`EXPORTER_LABEL`], an empty context, and exactly [`EXPORTER_LEN`] bytes. The transport
//! frames the payload codec with `crosspane_protocol::wire::KIND_PAIRING`; this module
//! neither opens connections nor drives the UI. Peer grants are informational only.
//!
//! Stored nonces and exporters, and nonces owned by reveal messages, are zeroized on drop.
//! The caller owns any encoded wire buffers and copies of the exporter it retains.

use core::fmt;

use aws_lc_rs::{constant_time, digest, hmac};
use crosspane_protocol::msg::Capability;
use crosspane_types::id::NodeId;
use zeroize::{Zeroize, Zeroizing};

use crate::rng::Rng;

pub const NONCE_LEN: usize = 32;
pub const EXPORTER_LEN: usize = 32;
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-crosspane-pairing-v1";

const MAX_SPKI: usize = 128;
const MAX_NAME: usize = 64;
const MAX_GRANTS: usize = 8;
const SAS_COUNT: u32 = 1_000_000;
const COMMIT_CONTEXT: &[u8] = b"crosspane pairing commit v1\0";
const SAS_CONTEXT: &[u8] = b"crosspane pairing sas v1\0";

/// A 6-digit short authentication string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sas(pub u32);

impl fmt::Display for Sas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}", self.0)
    }
}

/// Commit to a nonce and a 1–128 byte SPKI.
///
/// # Panics
/// Panics if `spki` is outside the protocol's length bounds.
pub fn commitment(nonce: &[u8; NONCE_LEN], spki: &[u8]) -> [u8; 32] {
    assert!(valid_spki(spki), "SPKI must contain 1–128 bytes");
    let mut hash = digest::Context::new(&digest::SHA256);
    hash.update(COMMIT_CONTEXT);
    hash.update(nonce);
    hash.update(&(spki.len() as u16).to_be_bytes());
    hash.update(spki);
    digest_bytes(hash.finish())
}

/// Derive the SAS, with initiator inputs first regardless of the caller's role.
///
/// # Panics
/// Panics if either SPKI is outside the protocol's length bounds.
pub fn derive_sas(
    exporter: &[u8; EXPORTER_LEN],
    initiator_spki: &[u8],
    joiner_spki: &[u8],
    initiator_nonce: &[u8; NONCE_LEN],
    joiner_nonce: &[u8; NONCE_LEN],
) -> Sas {
    assert!(valid_spki(initiator_spki) && valid_spki(joiner_spki));
    let key = hmac::Key::new(hmac::HMAC_SHA256, exporter);
    let mut mac = hmac::Context::with_key(&key);
    mac.update(SAS_CONTEXT);
    for spki in [initiator_spki, joiner_spki] {
        mac.update(&(spki.len() as u16).to_be_bytes());
        mac.update(spki);
    }
    mac.update(initiator_nonce);
    mac.update(joiner_nonce);
    let mac = mac.sign();
    let mut prefix = Zeroizing::new([0; 8]);
    prefix.copy_from_slice(&mac.as_ref()[..8]);
    Sas((u64::from_be_bytes(*prefix) % u64::from(SAS_COUNT)) as u32)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingMsg {
    Commit([u8; 32]),
    Reveal {
        nonce: [u8; NONCE_LEN],
        spki: Vec<u8>,
    },
    Matched,
    Confirmed {
        name: String,
        grants: Vec<Capability>,
    },
    Abort(AbortReason),
}

impl Drop for PairingMsg {
    fn drop(&mut self) {
        if let Self::Reveal { nonce, .. } = self {
            nonce.zeroize();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AbortReason {
    Protocol,
    CommitmentMismatch,
    CodeMismatch,
    UserRejected,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("pairing message malformed")]
    Malformed,
    #[error("unexpected pairing message")]
    Unexpected,
    #[error("the commitment didn't match")]
    CommitmentMismatch,
    #[error("the user picked a code that didn't match")]
    CodeMismatch,
    #[error("the user rejected pairing")]
    UserRejected,
    #[error("the peer aborted: {0:?}")]
    PeerAborted(AbortReason),
    #[error("both sides used the same key")]
    SameKey,
}

impl PairingMsg {
    /// Encode the payload (the type byte first).
    ///
    /// # Panics
    /// This infallible API requires valid fields. Panics for a manually constructed
    /// message with invalid lengths, unsupported capabilities or duplicate grants.
    /// Decoded messages and messages emitted by the state machines satisfy these bounds.
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.valid_fields(), "invalid pairing message fields");
        let mut out = Vec::new();
        match self {
            Self::Commit(hash) => {
                out.push(1);
                out.extend_from_slice(hash);
            }
            Self::Reveal { nonce, spki } => {
                out.push(2);
                out.extend_from_slice(nonce);
                out.extend_from_slice(&(spki.len() as u16).to_be_bytes());
                out.extend_from_slice(spki);
            }
            Self::Matched => out.push(3),
            Self::Confirmed { name, grants } => {
                out.extend_from_slice(&[4, name.len() as u8]);
                out.extend_from_slice(name.as_bytes());
                out.push(grants.len() as u8);
                for grant in grants {
                    // Validation above excludes the non-exhaustive enum's unknown variants.
                    if let Some(code) = capability_code(*grant) {
                        out.push(code);
                    }
                }
            }
            Self::Abort(reason) => out.extend_from_slice(&[5, reason_code(*reason)]),
        }
        out
    }

    /// Decode a payload, rejecting malformed fields and trailing bytes.
    pub fn decode(payload: &[u8]) -> Result<PairingMsg, PairingError> {
        let (&kind, body) = payload.split_first().ok_or(PairingError::Malformed)?;
        match kind {
            1 if body.len() == 32 => {
                let mut hash = [0; 32];
                hash.copy_from_slice(body);
                Ok(Self::Commit(hash))
            }
            2 if body.len() >= NONCE_LEN + 2 => {
                let len = usize::from(u16::from_be_bytes([body[32], body[33]]));
                if !(1..=MAX_SPKI).contains(&len) || body.len() != NONCE_LEN + 2 + len {
                    return Err(PairingError::Malformed);
                }
                let mut nonce = Zeroizing::new([0; NONCE_LEN]);
                nonce.copy_from_slice(&body[..NONCE_LEN]);
                Ok(Self::Reveal {
                    nonce: *nonce,
                    spki: body[NONCE_LEN + 2..].to_vec(),
                })
            }
            3 if body.is_empty() => Ok(Self::Matched),
            4 => {
                let name_len = usize::from(*body.first().ok_or(PairingError::Malformed)?);
                if !(1..=MAX_NAME).contains(&name_len) {
                    return Err(PairingError::Malformed);
                }
                let count = usize::from(*body.get(1 + name_len).ok_or(PairingError::Malformed)?);
                if count > MAX_GRANTS || body.len() != 2 + name_len + count {
                    return Err(PairingError::Malformed);
                }
                let name = core::str::from_utf8(&body[1..1 + name_len])
                    .map_err(|_| PairingError::Malformed)?
                    .to_owned();
                let mut grants = Vec::with_capacity(count);
                for code in &body[2 + name_len..] {
                    let grant = decode_capability(*code).ok_or(PairingError::Malformed)?;
                    if grants.contains(&grant) {
                        return Err(PairingError::Malformed);
                    }
                    grants.push(grant);
                }
                Ok(Self::Confirmed { name, grants })
            }
            5 if body.len() == 1 => Ok(Self::Abort(
                decode_reason(body[0]).ok_or(PairingError::Malformed)?,
            )),
            _ => Err(PairingError::Malformed),
        }
    }

    fn valid_fields(&self) -> bool {
        match self {
            Self::Reveal { spki, .. } => valid_spki(spki),
            Self::Confirmed { name, grants } => valid_metadata(name, grants),
            _ => true,
        }
    }
}

/// What the pairing yields: pin `spki`; the peer is `node`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub node: NodeId,
    pub spki: Vec<u8>,
    pub name: String,
    /// What the peer says it grants us (informational; enforcement is local, 04 §2).
    pub grants_to_us: Vec<Capability>,
}

/// What the caller must do next. Events are returned in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Send(PairingMsg),
    /// Initiator: display this code.
    ShowSas(Sas),
    /// Joiner: let the user pick the code shown on the initiator; the order is random.
    ShowCandidates([Sas; 3]),
    /// Initiator: the joiner's user matched the code; ask "Confirm pairing with ⟨name⟩?".
    AskConfirm,
    /// Done: pin the peer.
    Paired(Peer),
    /// Done: failed. When the error is local, an `Abort` is sent first.
    Failed(PairingError),
}

/// What each side brings.
#[derive(Clone, Debug)]
pub struct Local {
    pub spki: Vec<u8>,
    pub name: String,
    pub grants: Vec<Capability>,
}

struct Exchange {
    local: Local,
    nonce: Zeroizing<[u8; NONCE_LEN]>,
    exporter: Zeroizing<[u8; EXPORTER_LEN]>,
    peer_spki: Vec<u8>,
}

impl Exchange {
    fn new(local: Local, exporter: [u8; EXPORTER_LEN], rng: &mut dyn Rng) -> Self {
        let mut nonce = Zeroizing::new([0; NONCE_LEN]);
        rng.fill(nonce.as_mut());
        Self {
            local,
            nonce,
            exporter: Zeroizing::new(exporter),
            peer_spki: Vec::new(),
        }
    }

    fn valid(&self) -> bool {
        valid_spki(&self.local.spki) && valid_metadata(&self.local.name, &self.local.grants)
    }

    fn reveal(&self) -> Event {
        Event::Send(PairingMsg::Reveal {
            nonce: *self.nonce,
            spki: self.local.spki.clone(),
        })
    }

    fn confirmed(&self) -> Event {
        Event::Send(PairingMsg::Confirmed {
            name: self.local.name.clone(),
            grants: self.local.grants.clone(),
        })
    }

    fn peer(&mut self, name: &str, grants: &[Capability]) -> Peer {
        let spki = core::mem::take(&mut self.peer_spki);
        Peer {
            node: NodeId(digest_bytes(digest::digest(&digest::SHA256, &spki))),
            spki,
            name: name.to_owned(),
            grants_to_us: grants.to_vec(),
        }
    }
}

#[derive(Debug)]
enum InitiatorState {
    Reveal,
    Matched,
    UserConfirm,
    Confirmed,
    Terminal,
}

pub struct Initiator {
    exchange: Exchange,
    state: InitiatorState,
}

impl fmt::Debug for Initiator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Initiator")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Initiator {
    /// Opens the exchange: `[Send(Commit)]`. The nonce comes from `rng`.
    /// Invalid local fields produce an abort and terminal `Malformed` failure.
    pub fn start(
        local: Local,
        exporter: [u8; EXPORTER_LEN],
        rng: &mut dyn Rng,
    ) -> (Initiator, Vec<Event>) {
        let mut this = Self {
            exchange: Exchange::new(local, exporter, rng),
            state: InitiatorState::Reveal,
        };
        let events = if this.exchange.valid() {
            vec![Event::Send(PairingMsg::Commit(commitment(
                &this.exchange.nonce,
                &this.exchange.local.spki,
            )))]
        } else {
            this.fail(PairingError::Malformed, AbortReason::Protocol)
        };
        (this, events)
    }

    pub fn on_message(&mut self, msg: PairingMsg) -> Vec<Event> {
        if matches!(self.state, InitiatorState::Terminal) {
            return Vec::new();
        }
        if let PairingMsg::Abort(reason) = &msg {
            self.state = InitiatorState::Terminal;
            return vec![Event::Failed(PairingError::PeerAborted(*reason))];
        }
        match (&self.state, &msg) {
            (InitiatorState::Reveal, PairingMsg::Reveal { nonce, spki }) => {
                if !msg.valid_fields() {
                    return self.fail(PairingError::Malformed, AbortReason::Protocol);
                }
                if *spki == self.exchange.local.spki {
                    return self.fail(PairingError::SameKey, AbortReason::Protocol);
                }
                let sas = derive_sas(
                    &self.exchange.exporter,
                    &self.exchange.local.spki,
                    spki,
                    &self.exchange.nonce,
                    nonce,
                );
                self.exchange.peer_spki = spki.clone();
                self.state = InitiatorState::Matched;
                vec![self.exchange.reveal(), Event::ShowSas(sas)]
            }
            (InitiatorState::Matched, PairingMsg::Matched) => {
                self.state = InitiatorState::UserConfirm;
                vec![Event::AskConfirm]
            }
            (InitiatorState::Confirmed, PairingMsg::Confirmed { name, grants }) => {
                if !msg.valid_fields() {
                    return self.fail(PairingError::Malformed, AbortReason::Protocol);
                }
                self.state = InitiatorState::Terminal;
                vec![Event::Paired(self.exchange.peer(name, grants))]
            }
            _ => self.fail(PairingError::Unexpected, AbortReason::Protocol),
        }
    }

    /// The user's answer to `AskConfirm`.
    pub fn user_confirmed(&mut self, accept: bool) -> Vec<Event> {
        match self.state {
            InitiatorState::Terminal => Vec::new(),
            InitiatorState::UserConfirm if accept => {
                self.state = InitiatorState::Confirmed;
                vec![self.exchange.confirmed()]
            }
            InitiatorState::UserConfirm => {
                self.fail(PairingError::UserRejected, AbortReason::UserRejected)
            }
            _ => self.fail(PairingError::Unexpected, AbortReason::Protocol),
        }
    }

    fn fail(&mut self, error: PairingError, reason: AbortReason) -> Vec<Event> {
        self.state = InitiatorState::Terminal;
        failure(error, reason)
    }
}

#[derive(Debug)]
enum JoinerState {
    Commit,
    Reveal([u8; 32]),
    UserPick(Sas),
    Confirmed,
    Terminal,
}

pub struct Joiner {
    exchange: Exchange,
    state: JoinerState,
    decoy_indices: [u32; 2],
    sas_position: usize,
}

impl fmt::Debug for Joiner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Reveal holds the public commitment; omit even that from debug output.
        let state = match self.state {
            JoinerState::Commit => "Commit",
            JoinerState::Reveal(_) => "Reveal",
            JoinerState::UserPick(_) => "UserPick",
            JoinerState::Confirmed => "Confirmed",
            JoinerState::Terminal => "Terminal",
        };
        f.debug_struct("Joiner")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

impl Joiner {
    /// Waits for `Commit`; returns no events. The nonce and decoys come from `rng`.
    /// Invalid local fields produce an abort and terminal `Malformed` failure.
    pub fn start(
        local: Local,
        exporter: [u8; EXPORTER_LEN],
        rng: &mut dyn Rng,
    ) -> (Joiner, Vec<Event>) {
        let mut this = Self {
            exchange: Exchange::new(local, exporter, rng),
            state: JoinerState::Commit,
            // Sample uniform ranks now; map around the excluded SAS once it is known.
            decoy_indices: [uniform(rng, SAS_COUNT - 1), uniform(rng, SAS_COUNT - 2)],
            sas_position: uniform(rng, 3) as usize,
        };
        let events = if this.exchange.valid() {
            Vec::new()
        } else {
            this.fail(PairingError::Malformed, AbortReason::Protocol)
        };
        (this, events)
    }

    pub fn on_message(&mut self, msg: PairingMsg) -> Vec<Event> {
        if matches!(self.state, JoinerState::Terminal) {
            return Vec::new();
        }
        if let PairingMsg::Abort(reason) = &msg {
            self.state = JoinerState::Terminal;
            return vec![Event::Failed(PairingError::PeerAborted(*reason))];
        }
        match (&self.state, &msg) {
            (JoinerState::Commit, PairingMsg::Commit(hash)) => {
                self.state = JoinerState::Reveal(*hash);
                vec![self.exchange.reveal()]
            }
            (JoinerState::Reveal(expected), PairingMsg::Reveal { nonce, spki }) => {
                if !msg.valid_fields() {
                    return self.fail(PairingError::Malformed, AbortReason::Protocol);
                }
                if *spki == self.exchange.local.spki {
                    return self.fail(PairingError::SameKey, AbortReason::Protocol);
                }
                let actual = commitment(nonce, spki);
                if constant_time::verify_slices_are_equal(expected, &actual).is_err() {
                    return self.fail(
                        PairingError::CommitmentMismatch,
                        AbortReason::CommitmentMismatch,
                    );
                }
                let sas = derive_sas(
                    &self.exchange.exporter,
                    spki,
                    &self.exchange.local.spki,
                    nonce,
                    &self.exchange.nonce,
                );
                self.exchange.peer_spki = spki.clone();
                self.state = JoinerState::UserPick(sas);
                vec![Event::ShowCandidates(self.candidates(sas))]
            }
            (JoinerState::Confirmed, PairingMsg::Confirmed { name, grants }) => {
                if !msg.valid_fields() {
                    return self.fail(PairingError::Malformed, AbortReason::Protocol);
                }
                self.state = JoinerState::Terminal;
                vec![
                    self.exchange.confirmed(),
                    Event::Paired(self.exchange.peer(name, grants)),
                ]
            }
            _ => self.fail(PairingError::Unexpected, AbortReason::Protocol),
        }
    }

    /// The user's pick from `ShowCandidates`.
    pub fn user_picked(&mut self, pick: Sas) -> Vec<Event> {
        match self.state {
            JoinerState::Terminal => Vec::new(),
            JoinerState::UserPick(sas) if pick == sas => {
                self.state = JoinerState::Confirmed;
                vec![Event::Send(PairingMsg::Matched)]
            }
            JoinerState::UserPick(_) => {
                self.fail(PairingError::CodeMismatch, AbortReason::CodeMismatch)
            }
            _ => self.fail(PairingError::Unexpected, AbortReason::Protocol),
        }
    }

    fn candidates(&self, sas: Sas) -> [Sas; 3] {
        let first = self.decoy_indices[0] + u32::from(self.decoy_indices[0] >= sas.0);
        let mut second = self.decoy_indices[1];
        for excluded in [sas.0.min(first), sas.0.max(first)] {
            if second >= excluded {
                second += 1;
            }
        }
        let mut candidates = [Sas(first), Sas(second), sas];
        candidates.swap(2, self.sas_position);
        candidates
    }

    fn fail(&mut self, error: PairingError, reason: AbortReason) -> Vec<Event> {
        self.state = JoinerState::Terminal;
        failure(error, reason)
    }
}

fn failure(error: PairingError, reason: AbortReason) -> Vec<Event> {
    vec![Event::Send(PairingMsg::Abort(reason)), Event::Failed(error)]
}

fn digest_bytes(hash: digest::Digest) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes.copy_from_slice(hash.as_ref());
    bytes
}

fn valid_spki(spki: &[u8]) -> bool {
    (1..=MAX_SPKI).contains(&spki.len())
}

fn valid_metadata(name: &str, grants: &[Capability]) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && grants.len() <= MAX_GRANTS
        && grants
            .iter()
            .enumerate()
            .all(|(i, grant)| capability_code(*grant).is_some() && !grants[..i].contains(grant))
}

fn capability_code(capability: Capability) -> Option<u8> {
    match capability {
        Capability::InputAccept => Some(1),
        Capability::WindowShare => Some(2),
        Capability::WindowBrowse => Some(3),
        Capability::WindowPresent => Some(4),
        Capability::AudioSpeaker => Some(5),
        Capability::AudioMic => Some(6),
        Capability::ClipboardRead => Some(7),
        Capability::ClipboardWrite => Some(8),
        _ => None,
    }
}

fn decode_capability(code: u8) -> Option<Capability> {
    match code {
        1 => Some(Capability::InputAccept),
        2 => Some(Capability::WindowShare),
        3 => Some(Capability::WindowBrowse),
        4 => Some(Capability::WindowPresent),
        5 => Some(Capability::AudioSpeaker),
        6 => Some(Capability::AudioMic),
        7 => Some(Capability::ClipboardRead),
        8 => Some(Capability::ClipboardWrite),
        _ => None,
    }
}

fn reason_code(reason: AbortReason) -> u8 {
    match reason {
        AbortReason::Protocol => 1,
        AbortReason::CommitmentMismatch => 2,
        AbortReason::CodeMismatch => 3,
        AbortReason::UserRejected => 4,
        AbortReason::Expired => 5,
    }
}

fn decode_reason(code: u8) -> Option<AbortReason> {
    match code {
        1 => Some(AbortReason::Protocol),
        2 => Some(AbortReason::CommitmentMismatch),
        3 => Some(AbortReason::CodeMismatch),
        4 => Some(AbortReason::UserRejected),
        5 => Some(AbortReason::Expired),
        _ => None,
    }
}

/// Rejection sampling avoids modulo bias, including for the three display positions.
fn uniform(rng: &mut dyn Rng, upper: u32) -> u32 {
    let threshold = upper.wrapping_neg() % upper;
    let mut bytes = Zeroizing::new([0; 4]);
    loop {
        rng.fill(bytes.as_mut());
        let value = u32::from_be_bytes(*bytes);
        if value >= threshold {
            return value % upper;
        }
    }
}
