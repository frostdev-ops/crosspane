//! Window admission and observed HWND lifetimes, independent of Win32.

use std::collections::{BTreeMap, BTreeSet};

use crosspane_platform::{WindowEvent, WindowInfo, WindowRole, WindowState};
use crosspane_types::{
    geom::{RectLogical, SizeLogical},
    id::{DisplayId, WindowId},
    time::MonoTime,
};

use super::winevent::{self, RawWinEvent};

/// PID/TID and process creation time corroborate an observed HWND generation.
/// Win32 has no atomic window-lifetime token: an unobserved same-process/thread reuse
/// remains possible. The adapter rechecks this tuple before and after field reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub hwnd: u64,
    pub pid: u32,
    pub tid: u32,
    pub process_created: u64,
}

/// A coherent native snapshot. A failed snapshot must not be treated as closure.
#[derive(Clone, Debug)]
pub struct Observation {
    pub identity: Identity,
    pub title: String,
    pub app_id: String,
    pub class: String,
    pub style: u32,
    pub ex_style: u32,
    pub owner: Option<u64>,
    pub root: bool,
    pub visible: bool,
    pub iconic: bool,
    pub cloaked: bool,
    pub current_desktop: Option<bool>,
    pub display: DisplayId,
    pub frame: RectLogical,
    pub fills_monitor: bool,
}

impl Observation {
    /// Invisible windows remain hidden even if they also carry an iconic bit;
    /// activation must never show a deliberately hidden/parked window.
    pub fn state(&self) -> WindowState {
        if self.cloaked || self.current_desktop != Some(true) || !self.visible {
            WindowState::Hidden
        } else if self.iconic {
            WindowState::Minimized
        } else if self.fills_monitor && self.style & 0x00c0_0000 == 0 {
            WindowState::Fullscreen
        } else {
            WindowState::Normal
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    identity: Identity,
    info: WindowInfo,
}

/// IDs hash `(HWND, observed generation)`, with collision checking against all
/// previously allocated IDs. They remain stable until a confirmed close, CREATE,
/// or a changed native identity. Location bursts use a non-sliding 50ms deadline.
#[derive(Debug, Default)]
pub struct Windows {
    entries: BTreeMap<u64, Entry>,
    used: BTreeSet<u64>,
    generation: u64,
    locations: BTreeMap<u64, MonoTime>,
    focused: Option<WindowId>,
}

impl Windows {
    pub fn list(&self) -> Vec<WindowInfo> {
        self.entries.values().map(|e| e.info.clone()).collect()
    }

    pub fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    pub fn identity(&self, id: WindowId) -> Option<Identity> {
        self.entries
            .values()
            .find(|e| e.info.id == id)
            .map(|e| e.identity)
    }

    pub fn close(&mut self, hwnd: u64, expected: Option<Identity>) -> Vec<WindowEvent> {
        if expected.is_some_and(|i| self.entries.get(&hwnd).is_none_or(|e| e.identity != i)) {
            return Vec::new();
        }
        self.locations.remove(&hwnd);
        let Some(entry) = self.entries.remove(&hwnd) else {
            return Vec::new();
        };
        let mut events = vec![WindowEvent::Removed(entry.info.id)];
        if self.focused == Some(entry.info.id) {
            self.focused = None;
            events.push(WindowEvent::Focused(None));
        }
        events
    }

    pub fn observe(&mut self, observation: Observation) -> Vec<WindowEvent> {
        let o = observation;
        if o.identity.hwnd == 0
            || o.identity.pid == 0
            || o.identity.tid == 0
            || o.identity.process_created == 0
            || !o.frame.origin.x.is_finite()
            || !o.frame.origin.y.is_finite()
            || !o.frame.size.width.is_finite()
            || !o.frame.size.height.is_finite()
            || o.frame.size.width <= 0.0
            || o.frame.size.height <= 0.0
        {
            return Vec::new();
        }
        let hwnd = o.identity.hwnd;
        let mut events = Vec::new();
        if self
            .entries
            .get(&hwnd)
            .is_some_and(|e| e.identity != o.identity)
        {
            events.extend(self.close(hwnd, None));
        }
        let parent = o
            .owner
            .and_then(|h| self.entries.get(&h).map(|e| e.info.id));
        let popup = matches!(
            o.class.as_str(),
            "#32768" | "tooltips_class32" | "IME" | "MSCTFIME UI" | "CiceroUIWndFrame"
        );
        let dialog =
            !popup && (o.class == "#32770" || o.owner.is_some() && o.style & 0x00c0_0000 != 0);
        let role = if popup {
            WindowRole::Popup
        } else if dialog {
            WindowRole::Dialog
        } else if o.root && o.style & winevent::WS_CHILD == 0 {
            WindowRole::Toplevel
        } else {
            WindowRole::Other
        };
        let state = o.state();
        let hidden = state == WindowState::Hidden;
        if !self.entries.contains_key(&hwnd) {
            let eligible = o.visible
                && !hidden
                && (o.ex_style & winevent::WS_EX_TOOLWINDOW == 0 || dialog && o.owner.is_some())
                && match role {
                    WindowRole::Toplevel => !o.title.is_empty(),
                    WindowRole::Dialog => {
                        o.root && !o.title.is_empty()
                            || o.owner.is_some() && o.style & 0x8000_0000 != 0
                    }
                    WindowRole::Popup => parent.is_some(),
                    _ => false,
                };
            if !eligible {
                return events;
            }
            self.generation = self.generation.saturating_add(1);
            let mut id = 0xcbf2_9ce4_8422_2325_u64;
            for byte in hwnd
                .to_le_bytes()
                .into_iter()
                .chain(self.generation.to_le_bytes())
            {
                id = (id ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
            while id == 0 || !self.used.insert(id) {
                id = id.wrapping_add(1);
            }
            let info = WindowInfo {
                id: WindowId(id),
                title: o.title,
                app_id: o.app_id,
                pid: Some(o.identity.pid),
                display: Some(o.display),
                frame: o.frame,
                state,
                role,
                parent,
            };
            events.push(WindowEvent::Added(info.clone()));
            self.entries.insert(
                hwnd,
                Entry {
                    identity: o.identity,
                    info,
                },
            );
        } else if let Some(entry) = self.entries.get_mut(&hwnd) {
            let info = WindowInfo {
                id: entry.info.id,
                title: o.title,
                app_id: o.app_id,
                pid: Some(o.identity.pid),
                display: Some(o.display),
                frame: o.frame,
                state,
                role: entry.info.role,
                parent: if entry.info.role == WindowRole::Popup {
                    parent.or(entry.info.parent)
                } else {
                    parent
                },
            };
            if entry.info != info {
                entry.info = info.clone();
                events.push(WindowEvent::Changed(info));
            }
        }
        events
    }

    pub fn focus(&mut self, hwnd: Option<u64>) -> Vec<WindowEvent> {
        let next = hwnd.and_then(|h| self.entries.get(&h).map(|e| e.info.id));
        if self.focused == next {
            return Vec::new();
        }
        self.focused = next;
        vec![WindowEvent::Focused(next)]
    }

    /// Returns HWNDs requiring a fresh snapshot; DESTROY and CREATE mutate the
    /// observed lifetime immediately. Native acquisition stays outside callbacks.
    pub fn event(&mut self, event: RawWinEvent) -> (Vec<WindowEvent>, bool) {
        if event.hwnd == 0
            || event.id_object != winevent::OBJID_WINDOW
            || event.id_child != winevent::CHILDID_SELF
        {
            return (Vec::new(), false);
        }
        match event.event {
            winevent::EVENT_OBJECT_DESTROY => (self.close(event.hwnd, None), false),
            winevent::EVENT_OBJECT_CREATE => (self.close(event.hwnd, None), true),
            winevent::EVENT_OBJECT_LOCATIONCHANGE => {
                self.locations.entry(event.hwnd).or_insert(
                    event
                        .at
                        .saturating_add(std::time::Duration::from_millis(50)),
                );
                (Vec::new(), false)
            }
            winevent::EVENT_OBJECT_SHOW
            | winevent::EVENT_OBJECT_HIDE
            | winevent::EVENT_OBJECT_NAMECHANGE
            | winevent::EVENT_OBJECT_CLOAKED
            | winevent::EVENT_OBJECT_UNCLOAKED
            | winevent::EVENT_SYSTEM_MINIMIZESTART
            | winevent::EVENT_SYSTEM_MINIMIZEEND
            | winevent::EVENT_SYSTEM_FOREGROUND => (Vec::new(), true),
            _ => (Vec::new(), false),
        }
    }

    pub fn due(&mut self, now: MonoTime) -> Vec<u64> {
        let due: Vec<_> = self
            .locations
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(h, _)| *h)
            .collect();
        for hwnd in &due {
            self.locations.remove(hwnd);
        }
        due
    }
}

/// Convert with the canonical retained allocator; ambiguous/stale native monitor
/// facts fail rather than inventing another display identity or logical origin.
pub fn logical_frame(
    frame: [i32; 4],
    monitor: [i32; 4],
    name: &str,
    probes: &[super::geometry::MonitorProbe],
    ids: &mut super::geometry::DisplayIds,
) -> Result<(DisplayId, RectLogical), super::geometry::GeometryError> {
    use super::geometry::{self, GeometryError};
    let mut matches = probes
        .iter()
        .filter(|p| !p.twin && p.rc_monitor == monitor && p.name == name);
    let probe = matches.next().ok_or(GeometryError::InvalidMonitor)?;
    if matches.next().is_some() || frame[2] <= frame[0] || frame[3] <= frame[1] {
        return Err(GeometryError::InvalidMonitor);
    }
    let layout = geometry::displays(probes, ids)?;
    let id = ids.assign(&probe.device_path)?;
    let display = layout
        .displays
        .iter()
        .find(|d| d.id == id)
        .ok_or(GeometryError::InvalidMonitor)?;
    let origin = display
        .geometry
        .device_to_logical(geometry::physical_to_device((frame[0], frame[1]), monitor));
    Ok((
        id,
        RectLogical::new(
            origin,
            SizeLogical::new(
                (f64::from(frame[2]) - f64::from(frame[0])) / display.geometry.scale,
                (f64::from(frame[3]) - f64::from(frame[1])) / display.geometry.scale,
            ),
        ),
    ))
}

/// Admission for the documented conditional focus attempt. Hidden windows are
/// never shown/unparked, and an unknown desktop or changed identity fails closed.
pub fn may_activate(
    expected: Identity,
    actual: Identity,
    current_desktop: Option<bool>,
    default_desktop: bool,
    state: WindowState,
) -> bool {
    expected == actual
        && current_desktop == Some(true)
        && default_desktop
        && state != WindowState::Hidden
}

/// How long the source rides out failing monitor reads before it faults. Adding, re-moding or
/// removing a twin display, or plugging a monitor, fails coherent reads for a moment.
pub const MONITOR_READ_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// What the source does after one monitor read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorRead {
    /// The read succeeded: scan with its probes.
    Scan,
    /// Failed within the grace: map no frame, keep every identity, retry at the next scan.
    Skip,
    /// Reads have failed for the whole grace: the source faults, as before.
    Fault,
}

/// Failing monitor reads. A failed read never closes or forgets a window.
#[derive(Debug, Default)]
pub struct MonitorReads {
    failing_since: Option<MonoTime>,
}

impl MonitorReads {
    pub fn record(&mut self, ok: bool, now: MonoTime) -> MonitorRead {
        if ok {
            self.failing_since = None;
            return MonitorRead::Scan;
        }
        let since = *self.failing_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= MONITOR_READ_GRACE {
            MonitorRead::Fault
        } else {
            MonitorRead::Skip
        }
    }
}
