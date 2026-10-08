//! Pure twin display types and rules (WP-W3.1b). No Win32 types or calls: the native client in
//! `twin/` translates OS values into these inputs and carries out their outputs.

pub use super::cpd::CpdMode as TwinMode;
use super::cpd::{CpdError, HARDWARE_ID};
use crosspane_platform::PlatformError;
use crosspane_types::geom::PixelSize;
use std::fmt;

/// The accepted twin modes, smallest first. M2 parking always uses the largest (lead ruling R-F2).
pub const MODES: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];
pub const MM_MIN: u32 = 10;
pub const MM_MAX: u32 = 2_000;
pub const ADD_TIMEOUT_MS: u32 = 3_000;
pub const REMOVE_TIMEOUT_MS: u32 = 2_000;
pub const LIST_TIMEOUT_MS: u32 = 1_000;
pub const HEARTBEAT_TIMEOUT_MS: u32 = 2_500;
pub const HEARTBEAT_MISSES: u32 = 2;
pub const CANCEL_GRACE_MS: u32 = 1_000;
pub const NEW_PATH_TIMEOUT_MS: u32 = 2_000;
pub const PATH_GONE_TIMEOUT_MS: u32 = 1_500;
pub const PATH_POLL_MS: u32 = 50;
pub const STALE_TIMEOUT_MS: u32 = 6_000; // LEASE_MS + 1000
pub const STALE_POLL_MS: u32 = 100;
pub const TWIN_ABSENT_REASON: &str = "Crosspane's twin display driver isn't installed, so this window stays visible in place (mirror mode).";
pub const TWIN_UNAVAILABLE_REASON: &str = "Crosspane couldn't set up a twin display, so this window stays visible in place (mirror mode).";

/// Bounds for the interface list read from `CM_Get_Device_Interface_List`, as in the p9f spike.
const MULTI_SZ_MAX_WORDS: usize = 8_192;
const MULTI_SZ_MAX_ENTRIES: usize = 16;
const MULTI_SZ_MAX_UNITS: usize = 512;

/// Identifies a twin within one client. The ledger assigns it; it is never a monitor id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TwinKey(pub u32);

/// A driver refusal, classified from its Win32 code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Expired,
    Busy,
    NotFound,
    Capacity,
    Stopped,
    Invalid,
    Cancelled,
    Closed,
    Other(u32),
}

/// Errors from the twin client. `Display` never prints foreign identities.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TwinError {
    /// The control interface or its adapter is not present.
    Absent,
    /// More than one control interface or adapter matches.
    Ambiguous,
    /// The control device has the wrong hardware ID.
    Foreign,
    /// Opening the control interface failed with this Win32 error.
    Open(u32),
    /// The driver refused the request.
    Refused(Refusal),
    /// A reply or interface list broke the protocol or its bounds.
    Protocol(&'static str),
    /// A bounded wait ran out.
    Timeout(&'static str),
    /// The heartbeat lease was lost.
    LeaseLost,
    /// The new twin never appeared as an active desktop display.
    NotOnDesktop,
    /// Twins from an earlier run are still present.
    Stale(usize),
    /// No twin has this key.
    UnknownKey,
    /// The twin ledger could not be saved.
    Journal(String),
    /// A Win32 call failed.
    Native(&'static str, u32),
}

impl fmt::Display for TwinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("the twin display driver is not installed"),
            Self::Ambiguous => {
                f.write_str("more than one twin display control interface is present")
            }
            Self::Foreign => f.write_str("the twin display control device is not Crosspane's"),
            Self::Open(code) => {
                write!(
                    f,
                    "could not open the twin display control interface (Win32 {code})"
                )
            }
            Self::Refused(Refusal::Other(code)) => {
                write!(
                    f,
                    "the twin display driver refused the request (Win32 {code})"
                )
            }
            Self::Refused(refusal) => write!(
                f,
                "the twin display driver refused the request: {}",
                refusal_name(*refusal)
            ),
            Self::Protocol(what) => write!(f, "twin display protocol error: {what}"),
            Self::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Self::LeaseLost => f.write_str("the twin display lease was lost"),
            Self::NotOnDesktop => f.write_str("the twin display did not appear on the desktop"),
            Self::Stale(count) => {
                write!(
                    f,
                    "{count} stale twin display(s) remain from an earlier run"
                )
            }
            Self::UnknownKey => f.write_str("no such twin display"),
            Self::Journal(_) => f.write_str("the twin display ledger could not be saved"),
            Self::Native(what, code) => write!(f, "{what} failed (Win32 {code})"),
        }
    }
}

impl std::error::Error for TwinError {}

impl From<CpdError> for TwinError {
    fn from(error: CpdError) -> Self {
        Self::Protocol(match error {
            CpdError::Mode => "CPD1 mode is invalid",
            CpdError::ZeroId => "CPD1 id is zero",
            CpdError::Size => "CPD1 size is wrong",
            CpdError::Header => "CPD1 header is invalid",
            CpdError::Echo => "CPD1 echo does not match",
            CpdError::Field => "CPD1 field is invalid",
        })
    }
}

fn refusal_name(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Expired => "lease expired",
        Refusal::Busy => "busy",
        Refusal::NotFound => "not found",
        Refusal::Capacity => "no capacity",
        Refusal::Stopped => "stopped",
        Refusal::Invalid => "invalid request",
        Refusal::Cancelled => "cancelled",
        Refusal::Closed => "handle closed",
        Refusal::Other(_) => "unrecognised status",
    }
}

/// A twin's Windows identity, read from the QDC paths of our own adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnPath {
    pub luid: u64,
    pub target: u32,
    pub source: u32,
    pub monitor_path: String,
    pub gdi_name: String,
    /// Left, top, right, bottom, in desktop pixels.
    pub rect: [i32; 4],
}

/// A twin as the client reports it: its monitor id, mode and Windows display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwinDisplay {
    pub key: TwinKey,
    pub monitor_id: u32,
    pub mode: TwinMode,
    pub gdi_name: String,
    pub monitor_path: String,
    pub rect: [i32; 4],
    pub dpi: u32,
}

/// The result of waiting for a twin's new path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathWait {
    /// The index into `after` of the new path.
    Found(usize),
    Pending,
    Ambiguous,
}

/// Strictly increasing heartbeat sequence numbers. The first value is 1, because the CPD1 codec
/// refuses 0. Once closed, or once the sequence is exhausted, every call fails.
#[derive(Clone, Debug, Default)]
pub struct HeartbeatSeq {
    last: u64,
    closed: bool,
}

impl HeartbeatSeq {
    // The name is frozen (WP-W3.1b A1). This is not an `Iterator`: it can fail, and it never ends.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<u64, TwinError> {
        if self.closed {
            return Err(TwinError::Protocol("heartbeat sequence is closed"));
        }
        let Some(next) = self.last.checked_add(1) else {
            self.closed = true;
            return Err(TwinError::Protocol("heartbeat sequence is exhausted"));
        };
        self.last = next;
        Ok(next)
    }

    pub fn close(&mut self) {
        self.closed = true;
    }
}

/// Picks the twin mode for a window. The twin is always the largest mode, 1920x1080, whatever the
/// window's size: a window bigger than that is clamped when it is placed. The mm size comes from
/// the mode and `scale` (96 dpi at scale 1), clamped to `MM_MIN`..=`MM_MAX`, so only a scale change
/// changes the mode. A zero size, or a scale that is not finite and positive, is refused.
pub fn twin_mode(size: PixelSize, scale: f64) -> Result<TwinMode, PlatformError> {
    if size.width == 0 || size.height == 0 || !scale.is_finite() || scale <= 0.0 {
        return Err(PlatformError::Backend(
            "twin mode needs a nonzero size and a positive finite scale".into(),
        ));
    }
    let (width, height) = MODES[MODES.len() - 1];
    let mm = |px: u32| -> u32 {
        (f64::from(px) * 25.4 / (96.0 * scale))
            .round()
            .clamp(f64::from(MM_MIN), f64::from(MM_MAX)) as u32
    };
    Ok(TwinMode {
        width,
        height,
        width_mm: mm(width),
        height_mm: mm(height),
    })
}

/// Classifies a driver refusal from its Win32 code (`drivers/windows-idd/src/Control.cpp:50-60`).
pub fn classify(code: u32) -> Refusal {
    match code {
        121 => Refusal::Expired,
        170 => Refusal::Busy,
        1168 => Refusal::NotFound,
        1450 => Refusal::Capacity,
        21 => Refusal::Stopped,
        87 => Refusal::Invalid,
        995 => Refusal::Cancelled,
        6 => Refusal::Closed,
        other => Refusal::Other(other),
    }
}

/// The window-facing error for a twin failure. A missing driver gives the absent reason. A
/// ledger failure passes its message through. Every other failure gives the unavailable reason.
pub fn fallback(error: &TwinError) -> PlatformError {
    match error {
        TwinError::Absent => PlatformError::Unsupported(TWIN_ABSENT_REASON),
        TwinError::Journal(message) => PlatformError::Backend(message.clone()),
        _ => PlatformError::Unsupported(TWIN_UNAVAILABLE_REASON),
    }
}

/// Interface paths match when both are nonempty and equal under ASCII case folding.
pub fn interface_path_eq(left: &str, right: &str) -> bool {
    !left.is_empty() && left.eq_ignore_ascii_case(right)
}

/// Parses a bounded REG_MULTI_SZ list of interface paths. The list must end with an extra empty
/// terminator, and only zero padding may follow it.
pub fn parse_multi_sz(words: &[u16]) -> Result<Vec<String>, TwinError> {
    if words.is_empty() || words.len() > MULTI_SZ_MAX_WORDS {
        return Err(TwinError::Protocol("interface list size is out of bounds"));
    }
    let mut entries: Vec<String> = Vec::new();
    let mut start = 0;
    loop {
        let end = words[start..]
            .iter()
            .position(|&word| word == 0)
            .map(|index| start + index)
            .ok_or(TwinError::Protocol("interface list is not terminated"))?;
        if end == start {
            // The empty string is the list's extra terminator. An empty list is a double NUL, so
            // a lone NUL is refused.
            if start == 0 && words.len() < 2 {
                return Err(TwinError::Protocol(
                    "interface list is missing its terminator",
                ));
            }
            if words[end + 1..].iter().any(|&word| word != 0) {
                return Err(TwinError::Protocol(
                    "interface list has data after its terminator",
                ));
            }
            return Ok(entries);
        }
        let entry = &words[start..end];
        if entry.len() > MULTI_SZ_MAX_UNITS || entries.len() >= MULTI_SZ_MAX_ENTRIES {
            return Err(TwinError::Protocol(
                "interface list entry or count is out of bounds",
            ));
        }
        let text = String::from_utf16(entry)
            .map_err(|_| TwinError::Protocol("interface list entry is not UTF-16"))?;
        if text.chars().any(char::is_control) {
            return Err(TwinError::Protocol(
                "interface list entry has a control character",
            ));
        }
        if entries.iter().any(|old| interface_path_eq(old, &text)) {
            return Err(TwinError::Protocol("interface list has a duplicate entry"));
        }
        entries.push(text);
        start = end + 1;
        if start >= words.len() {
            return Err(TwinError::Protocol(
                "interface list is missing its extra terminator",
            ));
        }
    }
}

/// Picks the one control interface. None is `Absent`; more than one is `Ambiguous`.
pub fn select_control_interface(interfaces: &[String]) -> Result<&str, TwinError> {
    match interfaces {
        [] => Err(TwinError::Absent),
        [only] => Ok(only.as_str()),
        _ => Err(TwinError::Ambiguous),
    }
}

/// True when the device's hardware IDs include Crosspane's (case-insensitive).
pub fn hardware_id_matches(ids: &[String]) -> bool {
    ids.iter().any(|id| id.eq_ignore_ascii_case(HARDWARE_ID))
}

/// Two paths are the same twin when LUID, target, monitor path and GDI name all match. Text
/// compares case-insensitively. The source index is not part of identity.
pub fn same_path(a: &OwnPath, b: &OwnPath) -> bool {
    a.luid == b.luid
        && a.target == b.target
        && a.monitor_path.eq_ignore_ascii_case(&b.monitor_path)
        && a.gdi_name.eq_ignore_ascii_case(&b.gdi_name)
}

/// Finds the twin that a new ADD produced. Paths in `after` that match nothing in `before` are
/// new. Exactly one new path with a nonempty rect of the mode's size is `Found`. Zero new paths,
/// or a wrong size, is `Pending`. More than one is `Ambiguous`.
pub fn select_new_path(before: &[OwnPath], after: &[OwnPath], mode: TwinMode) -> PathWait {
    let fresh: Vec<(usize, &OwnPath)> = after
        .iter()
        .enumerate()
        .filter(|(_, path)| !before.iter().any(|old| same_path(old, path)))
        .collect();
    match fresh.as_slice() {
        [] => PathWait::Pending,
        [(index, path)] if rect_has_size(path.rect, mode) => PathWait::Found(*index),
        [_] => PathWait::Pending,
        _ => PathWait::Ambiguous,
    }
}

fn rect_has_size(rect: [i32; 4], mode: TwinMode) -> bool {
    let width = i64::from(rect[2]) - i64::from(rect[0]);
    let height = i64::from(rect[3]) - i64::from(rect[1]);
    width > 0 && height > 0 && width == i64::from(mode.width) && height == i64::from(mode.height)
}

/// Applies one heartbeat result. `Ok` resets the miss count. A `Timeout` adds a miss, and the
/// lane is lost at `HEARTBEAT_MISSES`. Any other error loses it at once. The returned bool is
/// true only when this outcome marks the lane lost, so the caller ORs it into its state.
pub fn beat_outcome(result: &Result<(), TwinError>, misses: u32) -> (u32, bool) {
    match result {
        Ok(()) => (0, false),
        Err(TwinError::Timeout(_)) => {
            let misses = misses.saturating_add(1);
            (misses, misses >= HEARTBEAT_MISSES)
        }
        Err(_) => (misses, true),
    }
}

#[cfg(test)]
mod tests {
    use super::HeartbeatSeq;

    // The public API cannot reach u64::MAX, so this test sets the private state directly.
    #[test]
    fn heartbeat_seq_closes_on_exhaustion() {
        let mut seq = HeartbeatSeq {
            last: u64::MAX - 1,
            closed: false,
        };
        assert_eq!(seq.next().ok(), Some(u64::MAX));
        assert!(seq.next().is_err());
        assert!(seq.next().is_err());
    }
}
