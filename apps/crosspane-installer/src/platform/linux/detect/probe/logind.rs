//! Trusted system-bus residuals: zbus's binary-message allocation ceiling is 128 MiB,
//! while SASL authentication reads have no byte-allocation cap. The admitted root-owned
//! endpoint with peer UID 0 is trusted; shared deadlines and owned-stream cancellation apply.

use super::*;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use zbus::{
    blocking::Connection,
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type},
};

pub type Properties = HashMap<String, OwnedValue>;
pub(crate) struct Bus {
    connection: Connection,
    deadline: Deadline,
    clock: CallerClock,
    pub last_receipt: u64,
}
impl Bus {
    pub fn new(
        stream: UnixStream,
        deadline: &Deadline,
        clock: CallerClock,
    ) -> Result<Self, ProbeIssue> {
        deadline.check().map_err(issue)?;
        let connection = zbus::blocking::connection::Builder::async_io_unix_stream(stream)
            .max_queued(4)
            .method_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| ProbeIssue::Unavailable)?;
        Ok(Self {
            connection,
            deadline: deadline.clone(),
            clock,
            last_receipt: 0,
        })
    }
    // Replies are limited to 64 KiB before decoding; decoded rows/keys/strings are bounded.
    pub fn call<T: DeserializeOwned + Type>(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        method: &str,
        body: &(impl Serialize + DynamicType),
    ) -> zbus::Result<T> {
        self.deadline
            .check()
            .map_err(|_| zbus::Error::Failure("read deadline".into()))?;
        let reply =
            self.connection
                .call_method(Some(destination), path, Some(interface), method, body);
        self.last_receipt = (self.clock)();
        let size = match &reply {
            Ok(message) | Err(zbus::Error::MethodError(_, _, message)) => {
                message.data().bytes().len()
            }
            _ => 0,
        };
        if size > MAX_PROBE_BYTES {
            return Err(zbus::Error::ExcessData);
        }
        reply?.body().deserialize()
    }
    pub fn properties(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
    ) -> Result<Properties, ProbeIssue> {
        let values = self
            .call(
                destination,
                path,
                "org.freedesktop.DBus.Properties",
                "GetAll",
                &(interface,),
            )
            .map_err(bus_issue)?;
        validate_properties(&values)?;
        Ok(values)
    }
}
pub(crate) fn bus_issue(error: zbus::Error) -> ProbeIssue {
    match error {
        zbus::Error::ExcessData => ProbeIssue::Oversize,
        zbus::Error::Variant(_)
        | zbus::Error::InvalidReply
        | zbus::Error::InvalidField
        | zbus::Error::MissingField => ProbeIssue::Malformed,
        _ => ProbeIssue::Unavailable,
    }
}
fn validate_properties(values: &Properties) -> Result<(), ProbeIssue> {
    if values.len() > 128 || values.keys().any(|s| s.len() > 128) {
        return Err(ProbeIssue::Oversize);
    }
    for key in values.keys() {
        bounded_text(key, 128)?;
    }
    Ok(())
}
pub(crate) fn property<T: TryFrom<OwnedValue> + Type>(
    values: &Properties,
    key: &str,
) -> Result<Option<T>, ProbeIssue> {
    values
        .get(key)
        .map(|v| {
            if v.value_signature() != T::SIGNATURE {
                return Err(ProbeIssue::Malformed);
            }
            T::try_from(v.try_clone().map_err(|_| ProbeIssue::Malformed)?)
                .map_err(|_| ProbeIssue::Malformed)
        })
        .transpose()
}
pub(crate) fn bounded_text(text: &str, limit: usize) -> Result<(), ProbeIssue> {
    if text.len() > limit {
        return Err(ProbeIssue::Oversize);
    }
    if text.chars().any(char::is_control) {
        return Err(ProbeIssue::Malformed);
    }
    Ok(())
}
/// Literal login1 signatures; absent fields stay absent, while a present wrong type is malformed.
pub fn decode_session(path: &str, values: &Properties) -> Result<SessionCandidate, ProbeIssue> {
    validate_properties(values)?;
    bounded_text(path, 512)?;
    if path == "/" || OwnedObjectPath::try_from(path).is_err() {
        return Err(ProbeIssue::Malformed);
    }
    let id = property::<String>(values, "Id")?;
    if let Some(id) = &id {
        bounded_text(id, 64)?;
        if id.is_empty() {
            return Err(ProbeIssue::Malformed);
        }
    }
    let id = id.unwrap_or_else(|| path.into());
    let kind = property::<String>(values, "Type")?;
    let user = property::<(u32, OwnedObjectPath)>(values, "User")?;
    let seating = property::<(String, OwnedObjectPath)>(values, "Seat")?;
    for path in user
        .iter()
        .map(|v| &v.1)
        .chain(seating.iter().map(|v| &v.1))
    {
        bounded_text(path.as_str(), 512)?;
    }
    let uid = user.map(|v| v.0);
    let seat = seating.map(|v| v.0);
    for text in kind.iter().chain(seat.iter()) {
        bounded_text(text, 64)?;
    }
    bounded_text(&id, 512)?;
    Ok(SessionCandidate {
        id,
        path: path.into(),
        kind,
        uid,
        seat,
        active: property(values, "Active")?,
        locked_hint: property(values, "LockedHint")?,
    })
}
type SessionRows = Vec<(String, OwnedObjectPath)>;
pub fn decode_user(values: &Properties) -> Result<(String, SessionRows), ProbeIssue> {
    validate_properties(values)?;
    let (id, display) =
        property::<(String, OwnedObjectPath)>(values, "Display")?.ok_or(ProbeIssue::Unverified)?;
    bounded_text(&id, 64)?;
    bounded_text(display.as_str(), 512)?;
    if id.is_empty() != (display.as_str() == "/") {
        return Err(ProbeIssue::Malformed);
    }
    let rows: SessionRows = property(values, "Sessions")?.ok_or(ProbeIssue::Unverified)?;
    if rows.len() > MAX_SESSIONS {
        return Err(ProbeIssue::Oversize);
    }
    let mut paths = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    for (id, path) in &rows {
        bounded_text(id, 64)?;
        bounded_text(path.as_str(), 512)?;
        if id.is_empty() || path.as_str() == "/" || !paths.insert(path.as_str()) || !ids.insert(id)
        {
            return Err(ProbeIssue::Malformed);
        }
    }
    Ok((display.to_string(), rows))
}
struct Lookup {
    bus: Bus,
    pid: u32,
    cached: Option<DisplaySessions>,
}
const LOGIN: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";
const MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
impl Lookup {
    fn candidate(&mut self, path: String) -> Result<SessionCandidate, ProbeIssue> {
        decode_session(
            &path,
            &self
                .bus
                .properties(LOGIN, &path, "org.freedesktop.login1.Session")?,
        )
    }
    fn lookup(
        &mut self,
        method: &str,
        body: &(impl Serialize + DynamicType),
        step: SessionSelection,
    ) -> Result<Option<SessionCandidate>, ProbeIssue> {
        let reply = self
            .bus
            .call(LOGIN, MANAGER, MANAGER_INTERFACE, method, body);
        match reply {
            Err(error) if !matches!(error, zbus::Error::MethodError(_, _, _)) => {
                Err(bus_issue(error))
            }
            reply => lookup_reply(reply, step)?
                .map(|path| self.candidate(path))
                .transpose(),
        }
    }
}
impl SessionLookup for Lookup {
    fn own_session(&mut self) -> Result<Option<SessionCandidate>, ProbeIssue> {
        self.lookup("GetSessionByPID", &(self.pid,), SessionSelection::Pid)
    }
    fn named_session(&mut self, id: &str) -> Result<Option<SessionCandidate>, ProbeIssue> {
        self.lookup("GetSession", &(id,), SessionSelection::Environment)
    }
    fn display_sessions(&mut self, uid: u32) -> Result<DisplaySessions, ProbeIssue> {
        if let Some(cached) = &self.cached {
            return Ok(cached.clone());
        }
        let path: OwnedObjectPath = self
            .bus
            .call(LOGIN, MANAGER, MANAGER_INTERFACE, "GetUser", &(uid,))
            .map_err(bus_issue)?;
        let values = self
            .bus
            .properties(LOGIN, path.as_str(), "org.freedesktop.login1.User")?;
        let (display, rows) = decode_user(&values)?;
        let mut sessions = Vec::new();
        for (_, path) in rows {
            sessions.push(self.candidate(path.to_string())?);
        }
        let result = DisplaySessions { display, sessions };
        self.cached = Some(result.clone());
        Ok(result)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct LogindFacts {
    /// Selection retains its last complete reply receipt, even when later counting takes time.
    pub selected_session: Fact<Option<SelectedSession>>,
    pub graphical_sessions: Fact<usize>,
}
pub(crate) fn read(
    stream: UnixStream,
    uid: u32,
    pid: u32,
    id: Option<String>,
    deadline: &Deadline,
    clock: &CallerClock,
) -> Result<LogindFacts, ProbeIssue> {
    let mut lookup = Lookup {
        bus: Bus::new(stream, deadline, clock.clone())?,
        pid,
        cached: None,
    };
    let selected = choose_session(&mut lookup, uid, id.as_deref());
    let selected_time = lookup.bus.last_receipt;
    let count = selected
        .as_ref()
        .map_err(|error| *error)
        .and_then(|_| lookup.display_sessions(uid))
        .and_then(|display| {
            if display
                .sessions
                .iter()
                .any(|s| s.kind.is_none() || s.uid.is_none() || s.seat.is_none())
            {
                return Err(ProbeIssue::Unverified);
            }
            Ok(display
                .sessions
                .iter()
                .filter(|s| {
                    s.uid == Some(uid)
                        && matches!(s.kind.as_deref(), Some("wayland" | "x11"))
                        && s.seat.as_deref().is_some_and(|v| !v.is_empty())
                })
                .count())
        });
    Ok(LogindFacts {
        selected_session: Fact {
            value: selected,
            source: ObservationSource::Demo,
            observed_at_ms: selected_time,
        },
        graphical_sessions: Fact {
            value: count,
            source: ObservationSource::Demo,
            observed_at_ms: lookup.bus.last_receipt,
        },
    })
}
/// Injected/explicit owned streams always produce Demo observations, never native proof.
/// The PID is explicit for tests; NativeSessionProbes always supplies THIS process's PID.
pub fn logind_from_stream(
    stream: UnixStream,
    uid: u32,
    pid: u32,
    id: Option<String>,
    deadline: &Deadline,
    clock: CallerClock,
) -> Fact<LogindFacts> {
    let shared = deadline.clone();
    let receipt = clock.clone();
    bounded(
        stream,
        deadline,
        clock,
        ObservationSource::Demo,
        move |stream| read(stream, uid, pid, id, &shared, &receipt),
    )
}
