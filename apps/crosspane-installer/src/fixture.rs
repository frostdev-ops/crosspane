//! Private inherited-pipe fixture protocol. Native adapters admit and own the child before
//! constructing this port; no PID, title, acknowledgement or pipe alone proves native ownership.
use crosspane_installer_core::AttemptId;
use crosspane_types::id::{NodeId, WindowId};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const MAX_QUEUE: usize = 32;
pub const RESPONSE_MS: u64 = 2000;
macro_rules! ids {
    ($($name:ident),+) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u64);
    )+};
}
ids!(FixtureId, PhaseId, ToneId);
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakersSelection {
    pub peer: NodeId,
    pub device_key: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCall {
    pub id: u64,
    pub attempt: AttemptId,
    pub command: FixtureCommand,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureCommand {
    Open {
        machine_label: String,
    },
    ArmTarget {
        fixture: FixtureId,
        phase: PhaseId,
    },
    ObserveWindow {
        fixture: FixtureId,
    },
    PlayTone {
        fixture: FixtureId,
        output: SpeakersSelection,
    },
    StopTone {
        fixture: FixtureId,
        tone: ToneId,
    },
    Close {
        fixture: FixtureId,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnWindowFacts {
    Unknown,
    Present {
        visible_on_user_workspace: Option<bool>,
        on_initial_display: Option<bool>,
    },
    Missing,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnToneState {
    Stopped,
    Running { tone: ToneId },
    StopUnconfirmed { tone: ToneId },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSnapshot {
    pub fixture: FixtureId,
    pub window: WindowId,
    pub phase: Option<PhaseId>,
    pub pattern_ticks: u64,
    pub target_clicks: u64,
    pub window_facts: OwnWindowFacts,
    pub tone: OwnToneState,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureEvent {
    Opened {
        fixture: FixtureId,
        pid: u32,
        window: WindowId,
        label: String,
    },
    TargetArmed {
        fixture: FixtureId,
        phase: PhaseId,
    },
    Snapshot {
        snapshot: FixtureSnapshot,
    },
    ToneStarted {
        fixture: FixtureId,
        tone: ToneId,
    },
    ToneStopped {
        fixture: FixtureId,
        tone: ToneId,
    },
    CloseRequested {
        fixture: FixtureId,
    },
    Closed {
        fixture: FixtureId,
    },
    Lost {
        fixture: FixtureId,
        reason: FixtureError,
    },
}
impl FixtureEvent {
    fn fixture(&self) -> FixtureId {
        match self {
            Self::Snapshot { snapshot } => snapshot.fixture,
            Self::Opened { fixture, .. }
            | Self::TargetArmed { fixture, .. }
            | Self::ToneStarted { fixture, .. }
            | Self::ToneStopped { fixture, .. }
            | Self::CloseRequested { fixture }
            | Self::Closed { fixture }
            | Self::Lost { fixture, .. } => *fixture,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
#[error("fixture: {self:?}")]
pub enum FixtureError {
    BadCall,
    Busy,
    NotOwned,
    Unavailable,
    UnknownWindow,
    AmbiguousWindow,
    OutputUnavailable,
    OutputChanged,
    UnsupportedFormat,
    TimedOut,
    ChildExited,
    CounterExhausted,
    ChannelClosed,
    InvalidMessage,
    Refused,
    CleanupFailed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureMessage {
    pub call_id: Option<u64>,
    pub attempt: AttemptId,
    pub sequence: u64,
    pub result: Result<FixtureEvent, FixtureError>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureControlPacket {
    pub schema_version: u32,
    pub call: FixtureCall,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureEventPacket {
    pub schema_version: u32,
    pub message: FixtureMessage,
}
pub trait FixturePort {
    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError>;
    fn poll(&mut self) -> Vec<FixtureMessage>;
}

// This schema has no arrays, signed numbers or floating-point values. Reject them recursively,
// including ignored/unknown fields, and reject duplicate keys before serde can discard them.
struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Json;
        impl<'de> Visitor<'de> for Json {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fixture JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Strict, E> {
                if v.len() > 160 || v.chars().any(char::is_control) {
                    return Err(E::custom("string"));
                }
                Ok(Strict(v.into()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if key.len() > 64 || values.len() == 16 || values.contains_key(&key) {
                        return Err(de::Error::custom("object"));
                    }
                    let Strict(v) = a.next_value()?;
                    values.insert(key, v);
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        d.deserialize_any(Json)
    }
}
fn depth(v: &Value) -> usize {
    match v {
        Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}
fn decode<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, FixtureError> {
    let bad = |_| FixtureError::InvalidMessage;
    if bytes.len() > MAX_LINE_BYTES {
        return Err(FixtureError::InvalidMessage);
    }
    if bytes.strip_suffix(b"\n").unwrap_or(bytes).contains(&b'\n') {
        return Err(FixtureError::InvalidMessage);
    }
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let Strict(v) = Strict::deserialize(&mut d).map_err(bad)?;
    d.end().map_err(bad)?;
    if depth(&v) > 16 {
        return Err(FixtureError::InvalidMessage);
    }
    let typed: T = serde_json::from_value(v.clone()).map_err(bad)?;
    // Also requires nullable fields and canonical string-only unit enums/NodeIds.
    if serde_json::to_value(&typed).map_err(bad)? != v {
        return Err(FixtureError::InvalidMessage);
    }
    Ok(typed)
}
fn label(s: &str) -> bool {
    s.len() <= 64 && !s.chars().any(char::is_control)
}
pub fn practice_title(
    machine: &str,
    attempt: AttemptId,
    fixture: FixtureId,
) -> Result<String, FixtureError> {
    if !label(machine) || attempt.0 == 0 || fixture.0 == 0 {
        return Err(FixtureError::BadCall);
    }
    Ok(format!(
        "Crosspane practice | {machine} | attempt {} | fixture {}",
        attempt.0, fixture.0
    ))
}
fn valid_call(call: &FixtureCall) -> bool {
    if call.id == 0 || call.attempt.0 == 0 {
        return false;
    }
    match &call.command {
        FixtureCommand::Open { machine_label } => label(machine_label),
        FixtureCommand::ArmTarget { fixture, phase } => fixture.0 != 0 && phase.0 != 0,
        FixtureCommand::PlayTone { fixture, output } => {
            fixture.0 != 0 && output.device_key == format!("crosspane.{}.speaker", output.peer)
        }
        FixtureCommand::StopTone { fixture, tone } => fixture.0 != 0 && tone.0 != 0,
        FixtureCommand::ObserveWindow { fixture } | FixtureCommand::Close { fixture } => {
            fixture.0 != 0
        }
    }
}
pub fn decode_control(bytes: &[u8]) -> Result<FixtureControlPacket, FixtureError> {
    let packet: FixtureControlPacket = decode(bytes)?;
    if packet.schema_version != 1 || !valid_call(&packet.call) {
        return Err(FixtureError::InvalidMessage);
    }
    Ok(packet)
}
pub fn decode_event(bytes: &[u8]) -> Result<FixtureEventPacket, FixtureError> {
    let packet: FixtureEventPacket = decode(bytes)?;
    let m = &packet.message;
    let valid = m.result.as_ref().map_or(true, |e| e.fixture().0 != 0)
        && match &m.result {
            Err(_) => true,
            Ok(FixtureEvent::Opened {
                pid,
                window,
                label: name,
                ..
            }) => *pid != 0 && window.0 != 0 && label(name),
            Ok(FixtureEvent::TargetArmed { phase, .. }) => phase.0 != 0,
            Ok(FixtureEvent::Snapshot { snapshot: s }) => {
                s.window.0 != 0
                    && s.phase.is_none_or(|p| p.0 != 0)
                    && match s.tone {
                        OwnToneState::Stopped => true,
                        OwnToneState::Running { tone } | OwnToneState::StopUnconfirmed { tone } => {
                            tone.0 != 0
                        }
                    }
            }
            Ok(FixtureEvent::ToneStarted { tone, .. } | FixtureEvent::ToneStopped { tone, .. }) => {
                tone.0 != 0
            }
            Ok(_) => true,
        };
    if packet.schema_version != 1
        || m.attempt.0 == 0
        || m.sequence == 0
        || m.call_id == Some(0)
        || !valid
    {
        return Err(FixtureError::InvalidMessage);
    }
    Ok(packet)
}
fn line<T: Serialize>(packet: &T) -> Result<Vec<u8>, FixtureError> {
    let mut bytes = serde_json::to_vec(packet).map_err(|_| FixtureError::InvalidMessage)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE_BYTES {
        return Err(FixtureError::InvalidMessage);
    }
    Ok(bytes)
}
pub fn encode_control(packet: &FixtureControlPacket) -> Result<Vec<u8>, FixtureError> {
    let bytes = line(packet)?;
    decode_control(&bytes)?;
    Ok(bytes)
}
pub fn encode_event(packet: &FixtureEventPacket) -> Result<Vec<u8>, FixtureError> {
    let bytes = line(packet)?;
    decode_event(&bytes)?;
    Ok(bytes)
}

/// Adapter-owned, already admitted child with only its inherited pipes. All methods are
/// nonblocking (WouldBlock for a pipe); Drop/retire stop output and reap only this owned child.
/// cleanup_confirmed requires actual process exit AND absence of its native owned window.
pub trait InheritedFixtureChild: Send {
    fn pid(&self) -> u32;
    fn read(&mut self, byte: &mut [u8]) -> io::Result<usize>;
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
    fn cleanup_confirmed(&mut self) -> Result<bool, FixtureError>;
    fn retire(&mut self);
}
pub type FixtureClock = Arc<dyn Fn() -> u64 + Send + Sync>;
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureReceipt {
    pub received_at_ms: u64,
    pub message: FixtureMessage,
}
fn lifecycle(m: &FixtureMessage) -> bool {
    matches!(
        m.result,
        Ok(FixtureEvent::CloseRequested { .. }
            | FixtureEvent::ToneStopped { .. }
            | FixtureEvent::Lost { .. }
            | FixtureEvent::Closed { .. })
    )
}
#[derive(Default)]
struct Exhaustion {
    reached: AtomicBool,
    retire_requested: AtomicBool,
}
#[derive(Default)]
struct State {
    exhaustion: Arc<Exhaustion>,
    last_call: u64,
    sequence: u64,
    attempt: Option<AttemptId>,
    fixture: Option<FixtureId>,
    phase: Option<PhaseId>,
    requested_phase: Option<PhaseId>,
    tone: Option<ToneId>,
    running: bool,
    window: Option<WindowId>,
    counts: (u64, u64),
    pending: BTreeMap<u64, (FixtureCall, u64, Option<u64>)>,
    output: VecDeque<FixtureReceipt>,
    failure: Option<(FixtureError, u64)>,
    retired: bool,
    close_requested: bool,
    close_confirmation: bool,
}
impl State {
    fn enqueue(&mut self, call: FixtureCall, at: u64) -> Result<Vec<u8>, FixtureError> {
        if self.retired {
            return Err(FixtureError::ChannelClosed);
        }
        if self.last_call == u64::MAX {
            return Err(FixtureError::CounterExhausted);
        }
        if call.id <= self.last_call || !valid_call(&call) {
            return Err(FixtureError::BadCall);
        }
        if self.pending.len() + self.output.len() >= MAX_QUEUE {
            return Err(FixtureError::Busy);
        }
        let owned = match &call.command {
            FixtureCommand::Open { .. } => self.attempt.is_none(),
            FixtureCommand::ArmTarget { fixture, phase } => {
                self.fixture == Some(*fixture)
                    && self.requested_phase.is_none_or(|old| phase > &old)
            }
            FixtureCommand::PlayTone { fixture, .. } => {
                self.fixture == Some(*fixture)
                    && !self.running
                    && !self
                        .pending
                        .values()
                        .any(|p| matches!(p.0.command, FixtureCommand::PlayTone { .. }))
            }
            FixtureCommand::ObserveWindow { fixture } | FixtureCommand::Close { fixture } => {
                self.fixture == Some(*fixture)
            }
            FixtureCommand::StopTone { fixture, tone } => {
                self.fixture == Some(*fixture) && self.tone == Some(*tone)
            }
        };
        if !owned || self.attempt.is_some_and(|a| a != call.attempt) {
            return Err(FixtureError::NotOwned);
        }
        let until = at
            .checked_add(RESPONSE_MS)
            .ok_or(FixtureError::CounterExhausted)?;
        let bytes = encode_control(&FixtureControlPacket {
            schema_version: 1,
            call: call.clone(),
        })?;
        self.last_call = call.id;
        if call.id == u64::MAX {
            self.exhaustion.reached.store(true, Ordering::Release);
        }
        self.attempt = Some(call.attempt);
        if let FixtureCommand::ArmTarget { phase, .. } = call.command {
            self.requested_phase = Some(phase);
        }
        self.pending.insert(call.id, (call, until, None));
        Ok(bytes)
    }
    fn accept(&mut self, m: FixtureMessage, at: u64, pid: u32) -> Result<(), FixtureError> {
        if self.retired {
            return Ok(());
        }
        if m.sequence == u64::MAX {
            self.exhaustion.reached.store(true, Ordering::Release);
            return Err(FixtureError::CounterExhausted);
        }
        if self.attempt != Some(m.attempt) || m.sequence <= self.sequence {
            return Err(FixtureError::InvalidMessage);
        }
        let action = m
            .call_id
            .map(|id| {
                self.pending
                    .get(&id)
                    .map(|p| &p.0.command)
                    .ok_or(FixtureError::InvalidMessage)
            })
            .transpose()?;
        if let Some(id) = m.call_id {
            let pending = &self.pending[&id];
            if pending.1 <= at {
                return Err(FixtureError::TimedOut);
            }
            if pending.2.is_none_or(|sent| at < sent) {
                return Err(FixtureError::InvalidMessage);
            }
        }
        let mut new_fixture = self.fixture;
        let mut new_tone = self.tone;
        let mut new_window = self.window;
        let mut new_phase = self.phase;
        let mut counts = self.counts;
        let mut terminal = matches!(action, Some(FixtureCommand::Open { .. })) && m.result.is_err();
        terminal |= matches!(
            m.result,
            Err(FixtureError::CounterExhausted
                | FixtureError::TimedOut
                | FixtureError::ChildExited
                | FixtureError::ChannelClosed
                | FixtureError::InvalidMessage)
        );
        if let Ok(event) = &m.result {
            use FixtureCommand as C;
            use FixtureEvent as E;
            if !matches!(event, E::Opened { .. }) && self.fixture != Some(event.fixture()) {
                return Err(FixtureError::InvalidMessage);
            }
            let matched = match (event, action) {
                (
                    E::Opened {
                        fixture,
                        pid: child,
                        window,
                        label,
                    },
                    Some(C::Open { machine_label }),
                ) => {
                    new_fixture = Some(*fixture);
                    new_window = Some(*window);
                    *child == pid && Some(fixture.0) == m.call_id && label == machine_label
                }
                (E::TargetArmed { phase, .. }, Some(C::ArmTarget { phase: p, .. })) => {
                    new_phase = Some(*phase);
                    phase == p && self.phase.is_none_or(|old| *phase > old)
                }
                (E::Snapshot { snapshot: s }, Some(C::ObserveWindow { .. }) | None) => {
                    counts = (s.pattern_ticks, s.target_clicks);
                    self.window == Some(s.window)
                        && s.phase == self.phase
                        && counts.0 >= self.counts.0
                        && counts.1 >= self.counts.1
                        && match s.tone {
                            OwnToneState::Stopped => true,
                            OwnToneState::Running { tone }
                            | OwnToneState::StopUnconfirmed { tone } => self.tone == Some(tone),
                        }
                }
                (E::ToneStarted { tone, .. }, Some(C::PlayTone { .. })) => {
                    new_tone = Some(*tone);
                    Some(tone.0) == m.call_id
                }
                (E::ToneStopped { tone, .. }, Some(C::StopTone { tone: t, .. })) => tone == t,
                (E::ToneStopped { tone, .. }, None) => self.tone == Some(*tone),
                (E::CloseRequested { .. }, Some(C::Close { .. }) | None) => true,
                (E::Closed { .. }, Some(C::Close { .. }) | None) => {
                    terminal = true;
                    true
                }
                (E::Lost { .. }, None) => {
                    terminal = true;
                    true
                }
                _ => false,
            };
            if !matched {
                return Err(FixtureError::InvalidMessage);
            }
        } else if action.is_none() {
            return Err(FixtureError::InvalidMessage);
        }
        let coalesce = m.call_id.is_none() && matches!(m.result, Ok(FixtureEvent::Snapshot { .. }));
        if coalesce
            && let Some(i) = self.output.iter().position(|r| {
                r.message.call_id.is_none()
                    && matches!(r.message.result, Ok(FixtureEvent::Snapshot { .. }))
            })
        {
            self.output.remove(i);
        }
        let retained_close = matches!(m.result, Ok(FixtureEvent::CloseRequested { .. }));
        // 32 ordinary ready replies plus one complete line; reserve three lifecycle progress/
        // stop entries and a terminal entry. Overflow retires explicitly instead of hiding loss.
        let reserved = lifecycle(&m);
        let queued = self
            .output
            .iter()
            .filter(|r| lifecycle(&r.message) == reserved)
            .count();
        let limit = if reserved {
            3 + usize::from(terminal)
        } else {
            MAX_QUEUE + 1
        };
        if queued >= limit {
            return Err(FixtureError::Busy);
        }
        self.sequence = m.sequence;
        self.close_requested |= retained_close;
        self.fixture = new_fixture;
        self.phase = new_phase;
        self.tone = new_tone;
        if matches!(m.result, Ok(FixtureEvent::ToneStarted { .. })) {
            self.running = true;
        }
        if matches!(m.result, Ok(FixtureEvent::ToneStopped { tone, .. }) if self.tone == Some(tone))
        {
            self.running = false;
        }
        self.window = new_window;
        self.counts = counts;
        if let Some(id) = m.call_id
            && !retained_close
        {
            self.pending.remove(&id);
        }
        // CloseRequested preserves the pending Close and cannot settle cleanup.
        let receipt = FixtureReceipt {
            received_at_ms: at,
            message: m,
        };
        self.output.push_back(receipt);
        if terminal && !self.pending.is_empty() {
            self.fail(FixtureError::ChildExited, at);
        }
        self.retired |= terminal;
        Ok(())
    }
    fn fail(&mut self, reason: FixtureError, at: u64) {
        if self.retired {
            return;
        }
        self.retired = true;
        self.failure = Some((reason, at));
    }
    fn drain(&mut self) -> Vec<FixtureReceipt> {
        let mut receipts: Vec<_> = self
            .output
            .drain(..self.output.len().min(MAX_QUEUE))
            .collect();
        let Some((reason, at)) = self.failure else {
            return receipts;
        };
        let Some(attempt) = self.attempt else {
            return receipts;
        };
        let calls = if self.pending.is_empty() {
            vec![None]
        } else {
            self.pending.keys().copied().map(Some).collect()
        };
        for call_id in calls.into_iter().take(MAX_QUEUE - receipts.len()) {
            let Some(sequence) = self.sequence.checked_add(1) else {
                break;
            };
            self.sequence = sequence;
            if let Some(id) = call_id {
                self.pending.remove(&id);
            }
            if self.pending.is_empty() {
                self.failure = None;
            }
            receipts.push(FixtureReceipt {
                received_at_ms: at,
                message: FixtureMessage {
                    call_id,
                    attempt,
                    sequence,
                    result: Err(reason),
                },
            });
        }
        receipts
    }
}

/// One admitted child per port. The adapter must construct a new child/port for a new attempt.
/// FixtureId = the Open call id; ToneId = the PlayTone call id. Neither can be reused.
/// poll_receipts retains the complete-line receipt clock for the frozen sequencer translator.
/// 32 ordinary ready messages plus one complete line and four reserved lifecycle entries are
/// bounded separately. Retirement preserves accepted receipts; each poll returns at most 32.
pub struct PipeFixturePort {
    exhaustion: Arc<Exhaustion>,
    state: Arc<Mutex<State>>,
    writes: std::sync::mpsc::SyncSender<(u64, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    clock: FixtureClock,
}
impl fmt::Debug for PipeFixturePort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PipeFixturePort(private inherited pipes)")
    }
}
impl PipeFixturePort {
    pub fn new(
        child: Box<dyn InheritedFixtureChild>,
        clock: FixtureClock,
    ) -> Result<Self, FixtureError> {
        let child = ChildGuard(child);
        if child.0.pid() == 0 {
            return Err(FixtureError::NotOwned);
        }
        let initial = State::default();
        let exhaustion = initial.exhaustion.clone();
        let state = Arc::new(Mutex::new(initial));
        let stop = Arc::new(AtomicBool::new(false));
        let (writes, receive) = std::sync::mpsc::sync_channel(MAX_QUEUE);
        let s = state.clone();
        let cancelled = stop.clone();
        let now = clock.clone();
        thread::Builder::new()
            .name("fixture-pipes".into())
            .spawn(move || {
                worker(child, s, receive, cancelled, now);
            })
            .map_err(|_| FixtureError::Unavailable)?;
        Ok(Self {
            exhaustion,
            state,
            writes,
            stop,
            clock,
        })
    }
    pub fn cancel(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
    /// Queue native cleanup confirmation for this attempt/fixture after its Close call or
    /// CloseRequested. Ok means queued ONLY, not that cleanup is proved. The owned-child worker
    /// calls cleanup_confirmed (reaped child AND a fresh absent-window observation) before emitting
    /// at most one Closed through the existing receipt/correlation/deadline rules. No timer alone
    /// completes cleanup; cancellation and retirement still refuse further confirmation.
    pub fn complete_closed(
        &mut self,
        attempt: AttemptId,
        fixture: FixtureId,
    ) -> Result<(), FixtureError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(FixtureError::ChannelClosed);
        }
        let mut state = self.state.lock().map_err(|_| FixtureError::ChannelClosed)?;
        if state.retired {
            return Err(FixtureError::ChannelClosed);
        }
        if state.attempt != Some(attempt) || state.fixture != Some(fixture) {
            return Err(FixtureError::NotOwned);
        }
        if state.close_confirmation
            || !(state.close_requested
                || state.pending.values().any(
                    |p| matches!(p.0.command, FixtureCommand::Close { fixture: f } if f == fixture),
                ))
        {
            return Err(FixtureError::BadCall);
        }
        state.close_confirmation = true;
        Ok(())
    }
    pub fn poll_receipts(&mut self) -> Vec<FixtureReceipt> {
        self.state
            .try_lock()
            .map(|mut s| s.drain())
            .unwrap_or_default()
    }
}
impl FixturePort for PipeFixturePort {
    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(FixtureError::ChannelClosed);
        }
        if self.exhaustion.reached.load(Ordering::Acquire) {
            self.exhaustion
                .retire_requested
                .store(true, Ordering::Release);
            return Err(FixtureError::CounterExhausted);
        }
        let mut state = self.state.lock().map_err(|_| FixtureError::ChannelClosed)?;
        let at = (self.clock)();
        let id = call.id;
        let bytes = match state.enqueue(call, at) {
            Err(FixtureError::CounterExhausted) => {
                state.fail(FixtureError::CounterExhausted, at);
                return Err(FixtureError::CounterExhausted);
            }
            result => result?,
        };
        if self.writes.try_send((id, bytes)).is_err() {
            state.fail(FixtureError::ChannelClosed, at);
            return Err(FixtureError::ChannelClosed);
        }
        Ok(())
    }
    fn poll(&mut self) -> Vec<FixtureMessage> {
        self.poll_receipts()
            .into_iter()
            .map(|r| r.message)
            .collect()
    }
}
impl Drop for PipeFixturePort {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
struct ChildGuard(Box<dyn InheritedFixtureChild>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.retire();
    }
}
fn pipe_progress(result: io::Result<usize>) -> Result<Option<usize>, FixtureError> {
    match result {
        Ok(n) => Ok(Some(n)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(_) => Err(FixtureError::ChannelClosed),
    }
}
fn worker(
    mut child: ChildGuard,
    state: Arc<Mutex<State>>,
    receive: std::sync::mpsc::Receiver<(u64, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    clock: FixtureClock,
) {
    let mut write: Option<(u64, Vec<u8>, usize)> = None;
    let mut bytes = Vec::new();
    let mut held: Option<(FixtureMessage, u64)> = None;
    loop {
        let result = (|| -> Result<bool, FixtureError> {
            let check = || -> Result<(), FixtureError> {
                let mut s = state.lock().map_err(|_| FixtureError::ChannelClosed)?;
                let at = clock();
                if s.exhaustion.retire_requested.load(Ordering::Acquire) {
                    s.fail(FixtureError::CounterExhausted, at);
                }
                if stop.load(Ordering::Acquire) {
                    s.fail(FixtureError::ChannelClosed, at);
                }
                let received = held.as_ref().and_then(|(m, _)| {
                    if matches!(
                        m.result,
                        Ok(FixtureEvent::Closed { .. } | FixtureEvent::CloseRequested { .. })
                    ) {
                        None
                    } else {
                        m.call_id
                    }
                });
                if s.pending
                    .iter()
                    .any(|(id, p)| Some(*id) != received && at >= p.1)
                {
                    s.fail(FixtureError::TimedOut, at);
                }
                if s.retired {
                    Err(FixtureError::ChannelClosed)
                } else {
                    Ok(())
                }
            };
            check()?;
            let complete = |child: &mut ChildGuard| -> Result<bool, FixtureError> {
                check()?;
                let queued = state
                    .lock()
                    .map_err(|_| FixtureError::ChannelClosed)?
                    .close_confirmation;
                if !queued || !child.0.cleanup_confirmed()? {
                    return Ok(false);
                }
                check()?;
                let mut s = state.lock().map_err(|_| FixtureError::ChannelClosed)?;
                let fixture = s.fixture.ok_or(FixtureError::NotOwned)?;
                let call_id = s.pending.iter().rev().find_map(|(id, p)| {
                    (p.2.is_some() && matches!(p.0.command, FixtureCommand::Close { .. }))
                        .then_some(*id)
                });
                let message = FixtureMessage {
                    call_id,
                    attempt: s.attempt.ok_or(FixtureError::NotOwned)?,
                    sequence: s
                        .sequence
                        .checked_add(1)
                        .filter(|n| *n < u64::MAX)
                        .ok_or(FixtureError::CounterExhausted)?,
                    result: Ok(FixtureEvent::Closed { fixture }),
                };
                s.accept(message, clock(), child.0.pid())?;
                Ok(true)
            };
            if held.is_none() && bytes.is_empty() && write.is_none() && complete(&mut child)? {
                return Ok(true);
            }
            let mut progress = false;
            if write.is_none() {
                write = receive.try_recv().ok().map(|(id, b)| (id, b, 0));
            }
            if let Some((id, data, offset)) = &mut write {
                check()?; // Cancellation/deadline precede every partial write, with no resend.
                if let Some(n) = pipe_progress(child.0.write(&data[*offset..]))? {
                    if n == 0 || n > data.len() - *offset {
                        return Err(FixtureError::ChannelClosed);
                    }
                    *offset += n;
                    progress = true;
                    if *offset == data.len() {
                        let mut s = state.lock().map_err(|_| FixtureError::ChannelClosed)?;
                        if let Some(p) = s.pending.get_mut(id) {
                            p.2 = Some(clock());
                        }
                        write = None;
                    }
                }
            }
            if held.is_none() {
                let mut byte = [0];
                if let Some(n) = pipe_progress(child.0.read(&mut byte))? {
                    if n == 0 {
                        if bytes.is_empty() && write.is_none() && complete(&mut child)? {
                            return Ok(true);
                        }
                        return Err(FixtureError::ChildExited);
                    }
                    if n != 1 {
                        return Err(FixtureError::InvalidMessage);
                    }
                    progress = true;
                    bytes.push(byte[0]);
                    if bytes.len() > MAX_LINE_BYTES {
                        return Err(FixtureError::InvalidMessage);
                    }
                    if byte[0] == b'\n' {
                        let received = clock();
                        held = Some((decode_event(&bytes)?.message, received));
                        bytes.clear();
                    }
                }
            }
            if let Some((message, received)) = &held {
                let closed = matches!(message.result, Ok(FixtureEvent::Closed { .. }));
                if !closed || child.0.cleanup_confirmed()? {
                    let mut s = state.lock().map_err(|_| FixtureError::ChannelClosed)?;
                    match s.accept(message.clone(), *received, child.0.pid()) {
                        Ok(()) => {
                            held = None;
                            progress = true;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(progress)
        })();
        match result {
            Err(reason) => {
                if let Ok(mut s) = state.lock() {
                    s.fail(reason, clock());
                }
                return;
            }
            Ok(false) => thread::sleep(Duration::from_millis(1)),
            Ok(true) => {}
        }
    }
}
