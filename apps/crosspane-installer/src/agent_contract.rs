//! Bounded local JSON values. Parsing confers no native transport or process authority.
pub use crosspane_installer_core::ObservationSource;
use crosspane_installer_core::{CounterId, CounterSample, Epochs, EvidenceBinding};
use crosspane_types::id::{NodeId, WindowId};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    net::SocketAddr,
};

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_ITEMS: usize = 128;
pub const MAX_STRING_BYTES: usize = 4096;
pub const MAX_QUEUE: usize = 32;
pub const MAX_TIMEOUT_MS: u64 = 5000;
/// Maximum nested JSON containers, counting the outer object or array.
pub const MAX_JSON_DEPTH: usize = 64;

// Shared derives keep literal definitions compact; Debug exposes type names only.
macro_rules! redacted_debug {
    ($($name:ident),+ $(,)?) => { $(
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), " { .. }"))
            }
        }
    )+ };
}
macro_rules! wire_enum {
    ($name:ident { $($(#[$attr:meta])* $variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(rename_all = "snake_case")]
                enum Name { $($(#[$attr])* $variant),+ }
                let value = Value::String(String::deserialize(d)?);
                let name: Name = serde_json::from_value(value).map_err(de::Error::custom)?;
                Ok(match name { $(Name::$variant => Self::$variant),+ })
            }
        }
    };
}
macro_rules! wire_struct {
    ($name:ident { $($(#[$attr:meta])* $field:ident: $ty:ty),+ $(,)? }) => {
        #[derive(Clone, PartialEq, Serialize)]
        pub struct $name { $($(#[$attr])* pub $field: $ty),+ }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct Fields { $($(#[$attr])* $field: $ty),+ }
                let value = Value::deserialize(d)?;
                if !value.is_object() { return Err(de::Error::custom("expected object")); }
                let fields: Fields = serde_json::from_value(value).map_err(de::Error::custom)?;
                Ok(Self { $($field: fields.$field),+ })
            }
        }
        redacted_debug!($name);
    };
}
macro_rules! error_enum {
    ($name:ident { $($variant:ident $(($ty:ty))?),+ $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum $name { $($variant $(($ty))?),+ }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{self:?}") }
        }
        impl std::error::Error for $name {}
    };
}
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
wire_enum!(AgentPlatform { Linux, Macos });
wire_enum!(BackendName {
    Capture,
    Keys,
    Pointer,
    Overlay,
    Hotkeys,
    Keystore,
    Windows,
    Parking,
    Frames,
    Tray,
    Links,
    Gpu,
    Home,
    Audio,
    Discovery
});
wire_enum!(BackendState {
    Ready,
    Missing,
    Blocked,
    Failed
});
wire_enum!(BackendReason {
    NotSupported,
    Permission,
    ConstructionFailed,
    WorkerExited,
    Disabled,
    Unknown
});
wire_enum!(PermissionName {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
    Microphone
});
wire_enum!(PermissionState {
    Granted,
    NotGranted,
    Unknown
});
wire_enum!(KeyStoreProvenance { OsStore, File });
wire_enum!(SessionState {
    Unlocked,
    Locked,
    Unknown
});
wire_enum!(SourceParking { Twin, Mirror });
wire_enum!(StartupRecovery {
    Restored,
    NothingParked,
    Failed,
    None
});
wire_enum!(DiscoveryReason {
    DaemonFailed,
    BrowseFailed,
    Unknown
});
wire_enum!(Capability {
    Input,
    Share,
    Browse,
    Present,
    Speaker,
    Mic
});
wire_enum!(GrantableCapability {
    Input,
    Share,
    Browse,
    Present,
    Speaker
});
wire_struct!(BuildStatus { version: String, features: Vec<String> });
wire_struct!(InstanceStatus {
    id: u64,
    pid: u32,
    uid: u32,
    exe: String,
    runtime_dir: String,
    started_unix_ms: u64
});
wire_struct!(GateStatus { open: bool, session: SessionState,
    #[serde(deserialize_with = "nullable")] active: Option<bool>, armed: bool, panic: bool });
wire_struct!(WireEpochs {
    gate: u64,
    grants: u64,
    layout: u64,
    backends: u64
});
wire_struct!(BackendFact { name: BackendName, state: BackendState,
    #[serde(deserialize_with = "nullable")] reason: Option<BackendReason> });
wire_struct!(PermissionFact {
    name: PermissionName,
    state: PermissionState
});
wire_struct!(DiscoveryStatus { enabled: bool, running: bool, candidates: u32,
    #[serde(deserialize_with = "nullable")] error: Option<DiscoveryReason> });
wire_struct!(TrayStatus { created: bool });
wire_struct!(GlobalAudioStatus { enabled: bool, active_peers: Vec<NodeId>, frames_sent: u64,
    frames_played: u64 });
wire_struct!(PeerCounters { e1_controller_started: u64, e1_controller_ended: u64,
    e1_target_started: u64, e1_target_ended: u64, e1_injections_ok: u64, e1_hud_shows: u64,
    e1_chord_releases: u64, e1_command_releases: u64, e2_source_started: u64,
    e2_source_returned: u64, e2_dest_started: u64, e2_dest_returned: u64,
    #[serde(deserialize_with = "nullable")] e2_frames_presented: Option<u64>, e2_returns_failed: u64 });
wire_struct!(PeerStatus { node: NodeId, name: String, connected: bool,
    #[serde(deserialize_with = "nullable")] link_generation: Option<u64>, features: Vec<String>,
    grants_given: Vec<Capability>,
    #[serde(deserialize_with = "nullable")] last_source_parking: Option<SourceParking>,
    counters: PeerCounters });
wire_struct!(InstallerStatusV1 { schema_version: u32, build: BuildStatus, instance: InstanceStatus,
    config_revision: String, node: NodeId, recovery_pending: u32, startup_recovery: StartupRecovery,
    gate: GateStatus, epochs: WireEpochs, backends: Vec<BackendFact>, keystore: KeyStoreProvenance,
    permissions: Vec<PermissionFact>, discovery: DiscoveryStatus, tray: TrayStatus,
    audio: GlobalAudioStatus, settings_opened: u64, peers: Vec<PeerStatus> });
wire_struct!(ProjectionRef {
    source: NodeId,
    projection: u64
});
wire_struct!(TerminalStatus {
    #[serde(deserialize_with = "nullable")] controlling: Option<NodeId>,
    #[serde(deserialize_with = "nullable")] controlled_by: Option<NodeId>,
    projections: Vec<ProjectionRef> });
wire_struct!(LegacyDisplay {
    id: u32,
    name: String,
    pixels: [u32; 2],
    scale: f64,
    mm: [f64; 2],
    origin: [f64; 2]
});
wire_struct!(PeerDisplays { node: NodeId, displays: Vec<LegacyDisplay> });
wire_struct!(CommittedPlacement {
    node: NodeId,
    display: u32,
    origin_mm: [f64; 2],
    version: u64
});
wire_struct!(DisplayLayoutStatus { local_displays: Vec<LegacyDisplay>, peer_displays: Vec<PeerDisplays>,
    placements: Vec<CommittedPlacement> });

#[derive(Clone, PartialEq)]
pub struct HealthSnapshot {
    installer: InstallerStatusV1,
    terminal: TerminalStatus,
    display_layout: DisplayLayoutStatus,
}
impl HealthSnapshot {
    pub fn installer(&self) -> &InstallerStatusV1 {
        &self.installer
    }
    pub fn terminal(&self) -> &TerminalStatus {
        &self.terminal
    }
    pub fn display_layout(&self) -> &DisplayLayoutStatus {
        &self.display_layout
    }
}
wire_enum!(PendingHealthReason {
    Absent,
    UnsupportedVersion,
    Incomplete,
    UnknownEnum,
    AmbiguousIdentity,
    MissingDisplay
});
#[derive(Clone, PartialEq)]
pub enum StatusAdmission {
    Supported(Box<HealthSnapshot>),
    PendingHealthContract(PendingHealthReason),
}
error_enum!(ContractError {
    Oversize,
    InvalidJson,
    TrailingData,
    WrongEnvelope,
    WrongType,
    InvalidValue,
    MissingCounter,
    WrongScope,
    DuplicateMetric,
    UnknownPeer,
    DisconnectedPeer,
    IdExhausted,
    InvalidDeadline,
    QueueFull
});

/// Stable application metric ids. Global audio remains global in a peer-bound sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Metric {
    ControllerStarted = 0,
    ControllerEnded = 1,
    TargetStarted = 2,
    TargetEnded = 3,
    InjectionsOk = 4,
    HudShows = 5,
    ChordReleases = 6,
    CommandReleases = 7,
    SourceStarted = 8,
    SourceReturned = 9,
    DestStarted = 10,
    DestReturned = 11,
    FramesPresented = 12,
    ReturnsFailed = 13,
    GlobalAudioSent = 14,
    GlobalAudioPlayed = 15,
    SettingsOpened = 16,
}
#[derive(Clone, PartialEq)]
pub struct NormalizedCounters {
    pub sample: CounterSample,
    pub unavailable: Vec<Metric>,
}
wire_struct!(Placement {
    node: NodeId,
    display: u32,
    origin_mm: [f64; 2]
});
#[derive(Clone, PartialEq, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum InstallerRequest {
    Status,
    Release,
    Panic,
    Restart,
    AskPermissions,
    Dial {
        addr: SocketAddr,
    },
    PairListen {
        allow_input: bool,
    },
    PairJoin {
        addr: SocketAddr,
        allow_input: bool,
    },
    PairStatus,
    PairScan,
    PairConfirm {
        accept: bool,
    },
    PairPick {
        index: usize,
    },
    Allow {
        peer: NodeId,
        capability: GrantableCapability,
        allow: bool,
    },
    Place {
        placements: Vec<Placement>,
    },
    Windows,
    WindowsFrom {
        peer: NodeId,
    },
    Project {
        window: WindowId,
        peer: NodeId,
    },
    Pull {
        peer: NodeId,
        window: WindowId,
    },
    Return {
        projection: u64,
        source: Option<NodeId>,
    },
    SettingsUpdate {
        expected_revision: String,
        mac_virtual_display: bool,
    },
}
wire_enum!(PairPhase {
    Idle,
    Listening,
    Connecting,
    Confirm,
    Pick,
    Waiting,
    Paired,
    Failed,
    #[serde(other)]
    Unknown,
});
wire_struct!(PairingStatus { phase: PairPhase,
    #[serde(deserialize_with = "nullable")] sas: Option<String>, candidates: Vec<String>,
    #[serde(deserialize_with = "nullable")] peer: Option<String>,
    #[serde(deserialize_with = "nullable")] error: Option<String> });
wire_struct!(PairCandidate {
    name: String,
    addr: SocketAddr
});
wire_struct!(LocalWindow { id: WindowId, app: String, title: String,
    #[serde(deserialize_with = "nullable")] display: Option<u32>, size: [f64; 2] });
wire_struct!(RemoteWindow {
    id: WindowId,
    app: String,
    title: String,
    size: [u32; 2]
});
wire_struct!(SettingsUpdated {
    revision: String,
    restart_required: bool
});
#[derive(Clone, PartialEq)]
pub enum DecodedReply {
    Status(StatusAdmission),
    PairScan(Vec<PairCandidate>),
    PairStatus(PairingStatus),
    Windows(Vec<LocalWindow>),
    WindowsFrom(Vec<RemoteWindow>),
    SettingsUpdated(SettingsUpdated),
    Acknowledged,
}
wire_enum!(AgentRefusal {
    RevisionConflict,
    NotSupported,
    Other
});
error_enum!(CallFailure { Unavailable, Refused(AgentRefusal), InvalidResponse, TimeoutOutcomeUnknown,
    QueueFull, InvalidCall(ContractError) });
#[derive(Clone, PartialEq)]
pub struct AgentCall {
    pub id: u64,
    pub request: InstallerRequest,
    pub timeout_ms: u64,
}
#[derive(Clone, PartialEq)]
pub struct AgentReply {
    pub id: u64,
    pub observed_at_ms: u64,
    pub source: ObservationSource,
    pub result: Result<DecodedReply, CallFailure>,
}
/// Implementations only enqueue/drain, with at most 32 outstanding calls and 32 replies.
/// Native workers enforce the total 1..=5000-ms deadline and admit selected-agent authority.
pub trait AgentPort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure>;
    fn poll(&mut self) -> Vec<AgentReply>;
}
wire_enum!(BootstrapPhase {
    Starting,
    WaitingForKeystore,
    Ready,
    Failed
});
wire_enum!(BootstrapReason {
    Config,
    LockHeld,
    Platform,
    Keystore,
    Socket,
    Other
});
wire_struct!(BootstrapV1 { schema_version: u32, instance_id: u64, pid: u32, started_unix_ms: u64,
    phase: BootstrapPhase, phase_seq: u64,
    #[serde(deserialize_with = "nullable")] keystore: Option<KeyStoreProvenance>,
    #[serde(deserialize_with = "nullable")] reason: Option<BootstrapReason>, runtime_dir: String });
wire_enum!(ParkingExit {
    Restored,
    NothingParked,
    Failed,
    None
});
wire_struct!(LastExitV1 {
    schema_version: u32,
    instance_id: u64,
    stopped_unix_ms: u64,
    clean: bool,
    parking: ParkingExit,
    input_journals_empty: bool,
    audio_stopped: bool
});
wire_enum!(EraseResult {
    Removed,
    AlreadyAbsent,
    Refused,
    Waiting,
    Failed
});
wire_enum!(EraseReason {
    AgentRunning,
    NoExitReceipt,
    UncleanExit,
    KeystoreLocked,
    KeystoreError,
    Io
});
wire_enum!(EraseItem {
    Removed,
    Absent,
    Kept,
    Failed
});
wire_struct!(EraseIdentityV1 { schema_version: u32, result: EraseResult,
    #[serde(deserialize_with = "nullable")] reason: Option<EraseReason>, key: EraseItem, trust: EraseItem });

/// Streaming recursive admission rejects duplicate keys and bounds data before retaining it.
struct Bounded(Value);
impl<'de> Deserialize<'de> for Bounded {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Bounded;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Bounded, E> {
                Ok(Bounded(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Bounded, E> {
                Ok(Bounded(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Bounded, E> {
                Ok(Bounded(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Bounded, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Bounded(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Bounded, E> {
                Ok(Bounded(Value::Null))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Bounded, E> {
                if v.len() > MAX_STRING_BYTES {
                    return Err(E::custom("contract oversize"));
                }
                Ok(Bounded(Value::String(v.into())))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Bounded, A::Error> {
                let mut values = Vec::new();
                while let Some(Bounded(v)) = a.next_element()? {
                    if values.len() == MAX_ITEMS {
                        return Err(de::Error::custom("contract oversize"));
                    }
                    values.push(v);
                }
                Ok(Bounded(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Bounded, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if key.len() > MAX_STRING_BYTES || values.len() == MAX_ITEMS {
                        return Err(de::Error::custom("contract oversize"));
                    }
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    let Bounded(value) = a.next_value()?;
                    values.insert(key, value);
                }
                Ok(Bounded(Value::Object(values)))
            }
        }
        d.deserialize_any(JsonVisitor)
    }
}
fn json(bytes: &[u8]) -> Result<Value, ContractError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ContractError::Oversize);
    }
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let Bounded(v) = Bounded::deserialize(&mut d).map_err(|e| {
        if e.to_string().contains("contract oversize") {
            ContractError::Oversize
        } else {
            ContractError::InvalidJson
        }
    })?;
    d.end().map_err(|_| ContractError::TrailingData)?;
    if json_depth(&v) > MAX_JSON_DEPTH {
        return Err(ContractError::InvalidJson);
    }
    Ok(v)
}
fn json_depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}
fn complete_objects(v: &Value, names: &[&str]) -> Result<bool, ContractError> {
    for value in v.as_array().ok_or(ContractError::WrongType)? {
        let object = value.as_object().ok_or(ContractError::WrongType)?;
        if names.iter().any(|name| !object.contains_key(*name)) {
            return Ok(false);
        }
    }
    Ok(true)
}
fn typed<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, ContractError> {
    serde_json::from_value(v.clone()).map_err(|_| ContractError::WrongType)
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value, ContractError> {
    v.get(name).ok_or(ContractError::WrongType)
}
fn revision(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn node(v: &Value) -> Result<NodeId, ContractError> {
    let s = v.as_str().ok_or(ContractError::WrongType)?;
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ContractError::InvalidValue);
    }
    s.parse().map_err(|_| ContractError::InvalidValue)
}
fn sorted_unique<T: Ord>(xs: &[T]) -> bool {
    xs.windows(2).all(|w| w[0] < w[1])
}
fn finite(xs: &[f64]) -> bool {
    xs.iter().all(|n| n.is_finite())
}
fn envelope(v: &Value) -> Result<bool, ContractError> {
    let o = v.as_object().ok_or(ContractError::WrongEnvelope)?;
    if o.keys()
        .any(|k| !matches!(k.as_str(), "ok" | "result" | "error"))
    {
        return Err(ContractError::WrongEnvelope);
    }
    let ok = o
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or(ContractError::WrongEnvelope)?;
    if (ok && o.get("error").is_some_and(|e| !e.is_null()))
        || (!ok
            && (!o.get("error").is_some_and(Value::is_string)
                || o.get("result").is_some_and(|r| !r.is_null())))
    {
        return Err(ContractError::WrongEnvelope);
    }
    Ok(ok)
}
fn refusal(v: &Value) -> CallFailure {
    CallFailure::Refused(match v.get("error").and_then(Value::as_str) {
        Some("revision_conflict") => AgentRefusal::RevisionConflict,
        Some("not_supported") => AgentRefusal::NotSupported,
        _ => AgentRefusal::Other,
    })
}

pub fn encode_request(request: &InstallerRequest) -> Result<Vec<u8>, ContractError> {
    match request {
        InstallerRequest::PairListen { allow_input: true }
        | InstallerRequest::PairJoin {
            allow_input: true, ..
        } => return Err(ContractError::InvalidValue),
        InstallerRequest::PairPick { index } if *index >= MAX_ITEMS => {
            return Err(ContractError::InvalidValue);
        }
        InstallerRequest::SettingsUpdate {
            expected_revision, ..
        } if !revision(expected_revision) => return Err(ContractError::InvalidValue),
        InstallerRequest::Place { placements } => {
            if placements.len() > MAX_ITEMS {
                return Err(ContractError::Oversize);
            }
            let mut seen = BTreeSet::new();
            for p in placements {
                if !finite(&p.origin_mm) || !seen.insert((p.node, p.display)) {
                    return Err(ContractError::InvalidValue);
                }
            }
        }
        _ => {}
    }
    let mut bytes = serde_json::to_vec(request).map_err(|_| ContractError::InvalidValue)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ContractError::Oversize);
    }
    Ok(bytes)
}
pub fn decode_reply(
    request: &InstallerRequest,
    bytes: &[u8],
    platform: AgentPlatform,
) -> Result<DecodedReply, CallFailure> {
    let v = json(bytes).map_err(|_| CallFailure::InvalidResponse)?;
    if !envelope(&v).map_err(|_| CallFailure::InvalidResponse)? {
        return Err(refusal(&v));
    }
    let decode = || -> Result<DecodedReply, ContractError> {
        if matches!(request, InstallerRequest::Status) {
            return Ok(DecodedReply::Status(status(&v, platform)?));
        }
        let result = v.get("result").unwrap_or(&Value::Null);
        Ok(match request {
            InstallerRequest::PairScan => DecodedReply::PairScan(typed(result)?),
            InstallerRequest::PairStatus => {
                let p: PairingStatus = typed(result)?;
                if p.sas.as_ref().is_some_and(|s| s.len() > 128)
                    || p.candidates.iter().any(|s| s.len() > 128)
                {
                    return Err(ContractError::Oversize);
                }
                DecodedReply::PairStatus(p)
            }
            InstallerRequest::Windows => {
                let ws: Vec<LocalWindow> = typed(result)?;
                unique_windows(ws.iter().map(|w| w.id))?;
                if ws
                    .iter()
                    .any(|w| !finite(&w.size) || w.size.iter().any(|n| *n < 0.0))
                {
                    return Err(ContractError::InvalidValue);
                }
                DecodedReply::Windows(ws)
            }
            InstallerRequest::WindowsFrom { .. } => {
                let ws: Vec<RemoteWindow> = typed(result)?;
                unique_windows(ws.iter().map(|w| w.id))?;
                DecodedReply::WindowsFrom(ws)
            }
            InstallerRequest::SettingsUpdate { .. } => {
                let s: SettingsUpdated = typed(result)?;
                if !revision(&s.revision) || !s.restart_required {
                    return Err(ContractError::InvalidValue);
                }
                DecodedReply::SettingsUpdated(s)
            }
            _ => DecodedReply::Acknowledged,
        })
    };
    // Discard all unrecognized prose immediately.
    decode().map_err(|_| CallFailure::InvalidResponse)
}
fn unique_windows(ids: impl Iterator<Item = WindowId>) -> Result<(), ContractError> {
    let mut seen = BTreeSet::new();
    if ids.into_iter().any(|id| !seen.insert(id)) {
        return Err(ContractError::InvalidValue);
    }
    Ok(())
}

pub fn parse_status(
    bytes: &[u8],
    platform: AgentPlatform,
) -> Result<StatusAdmission, ContractError> {
    let v = json(bytes)?;
    if !envelope(&v)? {
        return Err(ContractError::WrongEnvelope);
    }
    status(&v, platform)
}
fn pending(reason: PendingHealthReason) -> StatusAdmission {
    StatusAdmission::PendingHealthContract(reason)
}
fn status(v: &Value, platform: AgentPlatform) -> Result<StatusAdmission, ContractError> {
    let r = field(v, "result")?;
    if !r.is_object() {
        return Err(ContractError::WrongType);
    }
    let Some(i) = r.get("installer") else {
        return Ok(pending(PendingHealthReason::Absent));
    };
    if !i.is_object() {
        return Err(ContractError::WrongType);
    }
    let Some(version) = i.get("schema_version") else {
        return Ok(pending(PendingHealthReason::Incomplete));
    };
    if typed::<u32>(version)? != 1 {
        return Ok(pending(PendingHealthReason::UnsupportedVersion));
    }
    let installer: InstallerStatusV1 = match serde_json::from_value(i.clone()) {
        Ok(value) => value,
        Err(e) => {
            let message = e.to_string();
            if message.starts_with("missing field") {
                return Ok(pending(PendingHealthReason::Incomplete));
            }
            if message.starts_with("unknown variant") {
                return Ok(pending(PendingHealthReason::UnknownEnum));
            }
            return Err(ContractError::WrongType);
        }
    };
    node(field(i, "node")?)?;
    for p in field(i, "peers")?
        .as_array()
        .ok_or(ContractError::WrongType)?
    {
        node(field(p, "node")?)?;
    }
    for p in field(field(i, "audio")?, "active_peers")?
        .as_array()
        .ok_or(ContractError::WrongType)?
    {
        node(p)?;
    }
    if !validate_installer(&installer, platform)? {
        return Ok(pending(PendingHealthReason::Incomplete));
    }
    let identities: BTreeSet<_> = std::iter::once(installer.node)
        .chain(installer.peers.iter().map(|p| p.node))
        .collect();
    let resolve = |v: &Value| -> Result<Option<NodeId>, ContractError> {
        let short = v.as_str().ok_or(ContractError::WrongType)?;
        let matches: Vec<_> = identities
            .iter()
            .filter(|n| n.short() == short)
            .copied()
            .collect();
        Ok(if matches.len() == 1 {
            matches.first().copied()
        } else {
            None
        })
    };
    let (
        Some(controlling),
        Some(controlled_by),
        Some(projections),
        Some(displays),
        Some(peers),
        Some(layout),
    ) = (
        r.get("controlling"),
        r.get("controlled_by"),
        r.get("projections"),
        r.get("displays"),
        r.get("peers"),
        r.get("layout"),
    )
    else {
        return Ok(pending(PendingHealthReason::Incomplete));
    };
    let display_fields = ["id", "name", "pixels", "scale", "mm", "origin"];
    if !complete_objects(projections, &["source", "projection", "text", "received"])?
        || !complete_objects(displays, &display_fields)?
        || !complete_objects(peers, &["node", "displays"])?
        || !complete_objects(layout, &["node", "display", "origin_mm", "version"])?
    {
        return Ok(pending(PendingHealthReason::Incomplete));
    }
    let routing = |v: &Value| -> Result<Option<NodeId>, ContractError> {
        if v.is_null() {
            Ok(None)
        } else {
            node(v).map(Some)
        }
    };
    let controlling = routing(controlling)?;
    let controlled_by = routing(controlled_by)?;
    if controlling
        .into_iter()
        .chain(controlled_by)
        .any(|n| !identities.contains(&n))
    {
        return Ok(pending(PendingHealthReason::AmbiguousIdentity));
    }
    let mut projection_refs = Vec::new();
    let mut seen = BTreeSet::new();
    for p in projections.as_array().ok_or(ContractError::WrongType)? {
        let received = field(p, "received")?;
        if !field(p, "text")?.is_string() || !(received.is_null() || received.is_object()) {
            return Err(ContractError::WrongType);
        }
        let Some(source) = resolve(field(p, "source")?)? else {
            return Ok(pending(PendingHealthReason::AmbiguousIdentity));
        };
        let projection = typed(field(p, "projection")?)?;
        // The producer's text/decoded count are deliberately neither retained nor evidence.
        if !seen.insert((source, projection)) {
            return Err(ContractError::InvalidValue);
        }
        projection_refs.push(ProjectionRef { source, projection });
    }
    let local_displays: Vec<LegacyDisplay> = typed(displays)?;
    let mut geometry = BTreeSet::new();
    add_displays(installer.node, &local_displays, &mut geometry)?;
    let mut peer_displays = Vec::new();
    let mut peer_nodes = BTreeSet::new();
    for p in peers.as_array().ok_or(ContractError::WrongType)? {
        let peer = node(field(p, "node")?)?;
        if peer == installer.node || !peer_nodes.insert(peer) {
            return Err(ContractError::InvalidValue);
        }
        if !identities.contains(&peer) {
            return Ok(pending(PendingHealthReason::AmbiguousIdentity));
        }
        let ds = field(p, "displays")?;
        if !complete_objects(ds, &display_fields)? {
            return Ok(pending(PendingHealthReason::Incomplete));
        }
        let ds: Vec<LegacyDisplay> = typed(ds)?;
        add_displays(peer, &ds, &mut geometry)?;
        peer_displays.push(PeerDisplays {
            node: peer,
            displays: ds,
        });
    }
    let mut placements = Vec::new();
    let mut seen = BTreeSet::new();
    for p in layout.as_array().ok_or(ContractError::WrongType)? {
        let Some(node) = resolve(field(p, "node")?)? else {
            return Ok(pending(PendingHealthReason::AmbiguousIdentity));
        };
        let display = typed(field(p, "display")?)?;
        let origin_mm: [f64; 2] = typed(field(p, "origin_mm")?)?;
        let version = typed(field(p, "version")?)?;
        if !finite(&origin_mm) || !seen.insert((node, display)) {
            return Err(ContractError::InvalidValue);
        }
        if !geometry.contains(&(node, display)) {
            return Ok(pending(PendingHealthReason::MissingDisplay));
        }
        placements.push(CommittedPlacement {
            node,
            display,
            origin_mm,
            version,
        });
    }
    Ok(StatusAdmission::Supported(Box::new(HealthSnapshot {
        installer,
        terminal: TerminalStatus {
            controlling,
            controlled_by,
            projections: projection_refs,
        },
        display_layout: DisplayLayoutStatus {
            local_displays,
            peer_displays,
            placements,
        },
    })))
}
fn validate_installer(
    i: &InstallerStatusV1,
    platform: AgentPlatform,
) -> Result<bool, ContractError> {
    use BackendName::*;
    let names = [
        Capture, Keys, Pointer, Overlay, Hotkeys, Keystore, Windows, Parking, Frames, Tray, Links,
        Gpu, Home, Audio, Discovery,
    ];
    if i.backends.len() < names.len() {
        return Ok(false);
    }
    if i.backends.iter().map(|b| b.name).ne(names)
        || i.backends
            .iter()
            .any(|b| (b.state == BackendState::Ready) != b.reason.is_none())
        || !revision(&i.config_revision)
        || !sorted_unique(&i.build.features)
        || !sorted_unique(&i.audio.active_peers)
    {
        return Err(ContractError::InvalidValue);
    }
    let mut nodes = BTreeSet::from([i.node]);
    for p in &i.peers {
        let grants: Vec<_> = p
            .grants_given
            .iter()
            .map(|g| match g {
                Capability::Input => "input",
                Capability::Share => "share",
                Capability::Browse => "browse",
                Capability::Present => "present",
                Capability::Speaker => "speaker",
                Capability::Mic => "mic",
            })
            .collect();
        if !nodes.insert(p.node) || !sorted_unique(&grants) {
            return Err(ContractError::InvalidValue);
        }
    }
    let permissions: BTreeSet<_> = i.permissions.iter().map(|p| p.name).collect();
    if permissions.len() != i.permissions.len() {
        return Err(ContractError::InvalidValue);
    }
    let base = [
        PermissionName::ScreenRecording,
        PermissionName::Accessibility,
        PermissionName::InputMonitoring,
    ];
    Ok(match platform {
        AgentPlatform::Linux => i.permissions.is_empty(),
        AgentPlatform::Macos => {
            base.iter().all(|p| permissions.contains(p)) && (3..=4).contains(&permissions.len())
        }
    })
}
fn add_displays(
    node: NodeId,
    displays: &[LegacyDisplay],
    seen: &mut BTreeSet<(NodeId, u32)>,
) -> Result<(), ContractError> {
    for d in displays {
        if !seen.insert((node, d.id))
            || !d.scale.is_finite()
            || d.scale <= 0.0
            || !finite(&d.mm)
            || d.mm.iter().any(|n| *n <= 0.0)
            || !finite(&d.origin)
        {
            return Err(ContractError::InvalidValue);
        }
    }
    Ok(())
}

pub fn counter_sample(
    health: &HealthSnapshot,
    peer: Option<NodeId>,
    source: ObservationSource,
    observed_at_ms: u64,
) -> Result<NormalizedCounters, ContractError> {
    let i = health.installer();
    let mut values = BTreeMap::from([
        (
            CounterId(Metric::GlobalAudioSent as u16),
            i.audio.frames_sent,
        ),
        (
            CounterId(Metric::GlobalAudioPlayed as u16),
            i.audio.frames_played,
        ),
        (CounterId(Metric::SettingsOpened as u16), i.settings_opened),
    ]);
    let mut unavailable = Vec::new();
    let link_generation = if let Some(peer) = peer {
        let p = i
            .peers
            .iter()
            .find(|p| p.node == peer)
            .ok_or(ContractError::UnknownPeer)?;
        if !p.connected {
            return Err(ContractError::DisconnectedPeer);
        }
        let link = p.link_generation.ok_or(ContractError::DisconnectedPeer)?;
        let c = &p.counters;
        let metrics = [
            c.e1_controller_started,
            c.e1_controller_ended,
            c.e1_target_started,
            c.e1_target_ended,
            c.e1_injections_ok,
            c.e1_hud_shows,
            c.e1_chord_releases,
            c.e1_command_releases,
            c.e2_source_started,
            c.e2_source_returned,
            c.e2_dest_started,
            c.e2_dest_returned,
        ];
        for (id, value) in (0_u16..12).zip(metrics) {
            values.insert(CounterId(id), value);
        }
        if let Some(value) = c.e2_frames_presented {
            values.insert(CounterId(Metric::FramesPresented as u16), value);
        } else {
            unavailable.push(Metric::FramesPresented);
        }
        values.insert(CounterId(Metric::ReturnsFailed as u16), c.e2_returns_failed);
        Some(link)
    } else {
        None
    };
    Ok(NormalizedCounters {
        sample: CounterSample {
            source,
            observed_at_ms,
            binding: EvidenceBinding {
                local_node: i.node,
                peer,
                link_generation,
                epochs: Epochs {
                    instance_id: i.instance.id,
                    gate_epoch: i.epochs.gate,
                    grants_epoch: i.epochs.grants,
                    layout_epoch: i.epochs.layout,
                    backends_epoch: i.epochs.backends,
                },
            },
            values,
        },
        unavailable,
    })
}

pub fn parse_bootstrap(bytes: &[u8]) -> Result<BootstrapV1, ContractError> {
    let b: BootstrapV1 = typed(&json(bytes)?)?;
    if b.schema_version != 1 || (b.phase != BootstrapPhase::Failed && b.reason.is_some()) {
        return Err(ContractError::InvalidValue);
    }
    Ok(b)
}
pub fn parse_last_exit(bytes: &[u8]) -> Result<LastExitV1, ContractError> {
    let e: LastExitV1 = typed(&json(bytes)?)?;
    let clean = e.parking != ParkingExit::Failed && e.input_journals_empty && e.audio_stopped;
    if e.schema_version != 1 || e.clean != clean {
        return Err(ContractError::InvalidValue);
    }
    Ok(e)
}
pub fn parse_erase_identity(bytes: &[u8]) -> Result<EraseIdentityV1, ContractError> {
    let e: EraseIdentityV1 = typed(&json(bytes)?)?;
    let success = matches!(e.result, EraseResult::Removed | EraseResult::AlreadyAbsent);
    let items = [e.key, e.trust];
    let valid = if success {
        e.reason.is_none()
            && matches!(e.key, EraseItem::Removed | EraseItem::Absent)
            && !items.contains(&EraseItem::Failed)
            && ((e.result == EraseResult::Removed) == items.contains(&EraseItem::Removed))
    } else {
        match e.result {
            EraseResult::Refused => {
                matches!(
                    e.reason,
                    Some(
                        EraseReason::AgentRunning
                            | EraseReason::NoExitReceipt
                            | EraseReason::UncleanExit
                    )
                ) && e.key == EraseItem::Kept
                    && e.trust == EraseItem::Kept
            }
            EraseResult::Waiting => {
                e.reason == Some(EraseReason::KeystoreLocked)
                    && e.key == EraseItem::Kept
                    && e.trust == EraseItem::Kept
            }
            EraseResult::Failed => {
                matches!(e.reason, Some(EraseReason::KeystoreError | EraseReason::Io))
            }
            _ => false,
        }
    };
    if e.schema_version != 1 || !valid {
        return Err(ContractError::InvalidValue);
    }
    Ok(e)
}
impl EraseIdentityV1 {
    /// Literal receipt truth only; native process/exit/lock admission remains mandatory.
    pub fn identity_and_pairings_removed(&self) -> bool {
        matches!(
            self.result,
            EraseResult::Removed | EraseResult::AlreadyAbsent
        ) && self.schema_version == 1
            && self.reason.is_none()
            && matches!(self.key, EraseItem::Removed | EraseItem::Absent)
            && matches!(self.trust, EraseItem::Removed | EraseItem::Absent)
            && ((self.result == EraseResult::Removed)
                == (self.key == EraseItem::Removed || self.trust == EraseItem::Removed))
    }
}

/// Pure bounded queue usable by fake ports and native workers. It performs no I/O or admission.
#[derive(Default)]
pub struct AgentQueue {
    last_id: u64,
    calls: VecDeque<AgentCall>,
    pending: BTreeSet<u64>,
    replies: VecDeque<AgentReply>,
}
impl AgentQueue {
    pub fn take_calls(&mut self) -> Vec<AgentCall> {
        self.calls.drain(..).collect()
    }
    pub fn push_reply(&mut self, reply: AgentReply) -> Result<(), ContractError> {
        if self.replies.len() == MAX_QUEUE {
            return Err(ContractError::QueueFull);
        }
        if !self.pending.remove(&reply.id) {
            return Err(ContractError::InvalidValue);
        }
        self.calls.retain(|call| call.id != reply.id);
        self.replies.push_back(reply);
        Ok(())
    }
}
impl AgentPort for AgentQueue {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        if self.last_id == u64::MAX {
            return Err(CallFailure::InvalidCall(ContractError::IdExhausted));
        }
        if call.id <= self.last_id {
            return Err(CallFailure::InvalidCall(ContractError::InvalidValue));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&call.timeout_ms) {
            return Err(CallFailure::InvalidCall(ContractError::InvalidDeadline));
        }
        encode_request(&call.request).map_err(CallFailure::InvalidCall)?;
        if self.pending.len() == MAX_QUEUE {
            return Err(CallFailure::QueueFull);
        }
        self.last_id = call.id;
        self.pending.insert(call.id);
        self.calls.push_back(call);
        Ok(())
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        self.replies.drain(..).collect()
    }
}
redacted_debug!(
    HealthSnapshot,
    StatusAdmission,
    NormalizedCounters,
    InstallerRequest,
    DecodedReply,
    AgentCall,
    AgentReply,
    AgentQueue
);
