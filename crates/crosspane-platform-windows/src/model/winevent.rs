//! WinEvent → frozen `WindowEvent` model; no OS calls or runtime Windows claim.
//!
//! `[E]` Out-of-context hooks queue events in order; the adapter must use SKIPOWNPROCESS:
//! <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook>.
//! `[E]` The historical Classic Alt+Tab walk is documented, explicitly non-contractual:
//! <https://devblogs.microsoft.com/oldnewthing/20071008-00/?p=24863>.
//! `[P]` P9 must establish current Alt+Tab behaviour and coherent owner/popup probe acquisition.
//! `[P]` The native adapter must serialize callback processing (callbacks can reenter), not
//! mistake the documented enqueue order for completion order.
//! `[U]` CREATE before SHOW, cursor LOCATIONCHANGE delivery, and SHELL cloak meaning another
//! virtual desktop are assumptions, not dependencies of this table's event filtering.
//! `[P]` P9a/c must establish hidden-window WGC delivery, which rectangle is content, and the
//! adapter's screen-space physical rectangles/DPI normalization. Fullscreen here means exact
//! frame/monitor equality, not a capture or presentation assertion.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{WindowEvent, WindowInfo, WindowRole, WindowState};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PixelRect, PointDevice, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::time::MonoTime;

/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_CREATE: u32 = 0x8000;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_DESTROY: u32 = 0x8001;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_SHOW: u32 = 0x8002;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_HIDE: u32 = 0x8003;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_REORDER: u32 = 0x8004;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_CLOAKED: u32 = 0x8017;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_OBJECT_UNCLOAKED: u32 = 0x8018;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>
pub const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winauto/object-identifiers>
pub const OBJID_WINDOW: i32 = 0;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nc-winuser-wineventproc>
pub const CHILDID_SELF: i32 = 0;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winmsg/window-styles>
pub const WS_CHILD: u32 = 0x4000_0000;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles>
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles>
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute>
pub const DWMWA_CLOAKED: u32 = 14;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute>
pub const DWM_CLOAKED_APP: u32 = 1;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute>
pub const DWM_CLOAKED_SHELL: u32 = 2;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute>
pub const DWM_CLOAKED_INHERITED: u32 = 4;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook>
pub const WINEVENT_OUTOFCONTEXT: u32 = 0;
/// `[E]` <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook>
pub const WINEVENT_SKIPOWNPROCESS: u32 = 2;

/// `[E]` Callback fields are HWND/event/object/child; `[P]` the adapter stamps its own monotonic clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawWinEvent {
    pub event: u32,
    pub hwnd: u64,
    pub id_object: i32,
    pub id_child: i32,
    pub at: MonoTime,
}

/// `[E]` One node of the cited historical walk. `[P]` Native acquisition must supply a coherent chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupProbe {
    pub hwnd: u64,
    pub last_active_popup: u64,
    pub visible: bool,
    pub ex_style: u32,
}

/// `[P]` Rectangles are screen-space physical pixels, including negative desktop origins.
/// `popup_chain` includes the root owner and each traversed popup; missing/duplicate/cyclic
/// observations cannot establish an Alt+Tab representative. Titles are data, never logged here.
#[derive(Clone, Debug)]
pub struct WindowProbe {
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    pub style: u32,
    pub ex_style: u32,
    pub owner: Option<u64>,
    pub root_owner: u64,
    pub popup_chain: Vec<PopupProbe>,
    pub visible: bool,
    pub iconic: bool,
    pub cloaked: u32,
    pub rect_physical: PixelRect,
    pub frame_physical: Option<PixelRect>,
    pub monitor: Option<DisplayId>,
    pub monitor_physical: Option<PixelRect>,
}

/// Model correlation token, not an OS handle lifetime claim. Never reuse across probe requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeRequest {
    pub hwnd: u64,
    pub sequence: u64,
}

/// Pure native work requests or frozen platform notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Probe(ProbeRequest),
    Emit(WindowEvent),
}

fn excluded(p: &WindowProbe, own_pid: u32) -> bool {
    p.pid == 0 || p.pid == own_pid || p.style & WS_CHILD != 0 || p.ex_style & WS_EX_TOOLWINDOW != 0
}

/// `[E]` Model the cited 2007 walk; `[P]` current Windows Alt+Tab and ownership→Dialog need P9.
/// `[P]` TOOLWINDOW exclusion wins over APPWINDOW. The 64-row cap is a model bound, not an OS limit.
pub fn classify(hwnd: u64, p: &WindowProbe, own_pid: u32) -> Option<WindowRole> {
    classification(hwnd, p, own_pid).ok()
}

enum Classification {
    Excluded,
    Unavailable,
}

fn classification(hwnd: u64, p: &WindowProbe, own_pid: u32) -> Result<WindowRole, Classification> {
    if hwnd == 0 || excluded(p, own_pid) || !p.visible || p.cloaked != 0 {
        return Err(Classification::Excluded);
    }
    if p.popup_chain.len() > 64 {
        return Err(Classification::Unavailable);
    }
    if p.ex_style & WS_EX_APPWINDOW == 0 {
        let rows: BTreeMap<_, _> = p.popup_chain.iter().map(|r| (r.hwnd, r)).collect();
        if rows.len() != p.popup_chain.len() {
            return Err(Classification::Unavailable);
        }
        let mut walk = p.root_owner;
        let mut seen = BTreeSet::new();
        loop {
            if walk == 0 || !seen.insert(walk) {
                return Err(Classification::Unavailable);
            }
            let next = rows
                .get(&walk)
                .ok_or(Classification::Unavailable)?
                .last_active_popup;
            if next == walk {
                break;
            }
            let popup = rows.get(&next).ok_or(Classification::Unavailable)?;
            if popup.visible && popup.ex_style & WS_EX_TOOLWINDOW == 0 {
                break;
            }
            walk = next;
        }
        if walk != hwnd {
            return Err(Classification::Excluded);
        }
    }
    Ok(
        if p.owner.is_some_and(|owner| owner != 0 && owner != hwnd) {
            WindowRole::Dialog
        } else {
            WindowRole::Toplevel
        },
    )
}

/// `[P]` Crosspane precedence is invisible/cloaked→Hidden, iconic→Minimized, exact bounds→Fullscreen.
pub fn window_state(p: &WindowProbe, monitor: Option<PixelRect>) -> WindowState {
    if !p.visible || p.cloaked != 0 {
        WindowState::Hidden
    } else if p.iconic {
        WindowState::Minimized
    } else if monitor == Some(p.frame_physical.unwrap_or(p.rect_physical)) {
        WindowState::Fullscreen
    } else {
        WindowState::Normal
    }
}

fn map_window(
    hwnd: u64,
    p: WindowProbe,
    role: WindowRole,
    displays: &[DisplayInfo],
) -> Option<WindowInfo> {
    let mut matches = displays.iter().filter(|d| Some(d.id) == p.monitor);
    let d = matches.next()?;
    if matches.next().is_some() || !d.geometry.is_valid() {
        return None;
    }
    let monitor = p.monitor_physical?;
    let rect = p.frame_physical.unwrap_or(p.rect_physical);
    if monitor.min.x >= monitor.max.x
        || monitor.min.y >= monitor.max.y
        || rect.min.x >= rect.max.x
        || rect.min.y >= rect.max.y
    {
        return None;
    }
    if i64::from(monitor.max.x) - i64::from(monitor.min.x) != i64::from(d.geometry.pixel_size.width)
        || i64::from(monitor.max.y) - i64::from(monitor.min.y)
            != i64::from(d.geometry.pixel_size.height)
    {
        return None;
    }
    let logical =
        |at: crosspane_types::geom::euclid::Point2D<i32, crosspane_types::geom::Device>| {
            d.geometry.device_to_logical(PointDevice::new(
                f64::from(at.x) - f64::from(monitor.min.x),
                f64::from(at.y) - f64::from(monitor.min.y),
            ))
        };
    let min = logical(rect.min);
    let max = logical(rect.max);
    Some(WindowInfo {
        id: WindowId(hwnd),
        frame: RectLogical::new(min, SizeLogical::new(max.x - min.x, max.y - min.y)),
        state: window_state(&p, Some(monitor)),
        role,
        parent: p.owner.filter(|id| *id != 0 && *id != hwnd).map(WindowId),
        title: p.title,
        app_id: p.app_id,
        pid: Some(p.pid),
        display: Some(d.id),
    })
}

/// Only answered probes enter the table. `[P]` Native dispatch supplies observations; this table
/// establishes neither WGC availability nor HWND ownership beyond the supplied facts.
#[derive(Debug)]
pub struct WindowTable {
    own_pid: u32,
    coalesce: Duration,
    windows: BTreeMap<u64, WindowInfo>,
    locations: BTreeMap<u64, MonoTime>,
    pending: BTreeMap<u64, (ProbeRequest, bool)>,
    sequence: u64,
}

impl WindowTable {
    pub fn new(own_pid: u32, coalesce: Duration) -> Self {
        Self {
            own_pid,
            coalesce,
            windows: BTreeMap::new(),
            locations: BTreeMap::new(),
            pending: BTreeMap::new(),
            sequence: 0,
        }
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowInfo> {
        self.windows.values()
    }

    fn probe(&mut self, hwnd: u64) -> Vec<Action> {
        self.locations.remove(&hwnd);
        self.pending.remove(&hwnd);
        let Some(sequence) = self.sequence.checked_add(1) else {
            return Vec::new();
        };
        self.sequence = sequence;
        let request = ProbeRequest { hwnd, sequence };
        self.pending.insert(hwnd, (request, false));
        vec![Action::Probe(request)]
    }

    /// Initial supplied enumeration uses the same admission as event-driven answers.
    pub fn seed(
        &mut self,
        probes: Vec<(u64, WindowProbe)>,
        displays: &[DisplayInfo],
    ) -> Vec<WindowEvent> {
        let mut events = Vec::new();
        for (hwnd, probe) in probes {
            if hwnd != 0 {
                for action in self.probe(hwnd) {
                    if let Action::Probe(request) = action {
                        events.extend(self.on_probe(
                            request,
                            Some(probe.clone()),
                            displays,
                            MonoTime::ZERO,
                        ));
                    }
                }
            }
        }
        events
    }

    pub fn on_event(&mut self, e: RawWinEvent) -> Vec<Action> {
        if (e.hwnd == 0 && e.event != EVENT_SYSTEM_FOREGROUND)
            || e.id_object != OBJID_WINDOW
            || e.id_child != CHILDID_SELF
        {
            return Vec::new();
        }
        match e.event {
            EVENT_OBJECT_CREATE
            | EVENT_OBJECT_SHOW
            | EVENT_OBJECT_UNCLOAKED
            | EVENT_SYSTEM_MINIMIZEEND
            | EVENT_OBJECT_NAMECHANGE
            | EVENT_SYSTEM_MOVESIZEEND
            | EVENT_OBJECT_REORDER
            | EVENT_OBJECT_HIDE
            | EVENT_OBJECT_CLOAKED => self.probe(e.hwnd),
            EVENT_OBJECT_LOCATIONCHANGE => {
                self.locations
                    .entry(e.hwnd)
                    .or_insert(e.at.saturating_add(self.coalesce));
                Vec::new()
            }
            EVENT_OBJECT_DESTROY => {
                self.locations.remove(&e.hwnd);
                self.pending.remove(&e.hwnd);
                self.windows
                    .remove(&e.hwnd)
                    .map_or_else(Vec::new, |w| vec![Action::Emit(WindowEvent::Removed(w.id))])
            }
            EVENT_SYSTEM_MINIMIZESTART => {
                self.locations.remove(&e.hwnd);
                if let Some(w) = self.windows.get_mut(&e.hwnd) {
                    self.pending.remove(&e.hwnd);
                    if w.state != WindowState::Minimized {
                        w.state = WindowState::Minimized;
                        return vec![Action::Emit(WindowEvent::Changed(w.clone()))];
                    }
                } else if let Some((_, minimized)) = self.pending.get_mut(&e.hwnd) {
                    *minimized = true;
                }
                Vec::new()
            }
            EVENT_SYSTEM_FOREGROUND => self
                .on_foreground(Some(e.hwnd))
                .into_iter()
                .map(Action::Emit)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Only the latest exact token can answer. `None`/bad geometry is unavailable, not closure.
    /// `[E]` Cloaked windows still exist; already listed invisible/cloaked windows retain their
    /// role until a qualifying reclassification or DESTROY, matching the required Hidden rule.
    pub fn on_probe(
        &mut self,
        request: ProbeRequest,
        p: Option<WindowProbe>,
        displays: &[DisplayInfo],
        _now: MonoTime,
    ) -> Vec<WindowEvent> {
        if self.pending.get(&request.hwnd).map(|(token, _)| token) != Some(&request) {
            return Vec::new();
        }
        let minimized = self
            .pending
            .remove(&request.hwnd)
            .is_some_and(|(_, minimized)| minimized);
        let Some(mut p) = p else {
            return Vec::new();
        };
        p.iconic |= minimized;
        let role = if !excluded(&p, self.own_pid) && (!p.visible || p.cloaked != 0) {
            self.windows
                .get(&request.hwnd)
                .map(|w| w.role)
                .ok_or(Classification::Excluded)
        } else {
            classification(request.hwnd, &p, self.own_pid)
        };
        let role = match role {
            Ok(role) => role,
            Err(Classification::Unavailable) => return Vec::new(),
            Err(Classification::Excluded) => {
                return self
                    .windows
                    .remove(&request.hwnd)
                    .map_or_else(Vec::new, |w| vec![WindowEvent::Removed(w.id)]);
            }
        };
        let Some(window) = map_window(request.hwnd, p, role, displays) else {
            return Vec::new();
        };
        match self.windows.insert(request.hwnd, window.clone()) {
            None => vec![WindowEvent::Added(window)],
            Some(previous) if previous != window => vec![WindowEvent::Changed(window)],
            Some(_) => Vec::new(),
        }
    }

    pub fn on_foreground(&mut self, hwnd: Option<u64>) -> Vec<WindowEvent> {
        vec![WindowEvent::Focused(
            hwnd.filter(|id| self.windows.contains_key(id))
                .map(WindowId),
        )]
    }

    pub fn poll(&mut self, now: MonoTime) -> Vec<Action> {
        let due: Vec<_> = self
            .locations
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(hwnd, _)| *hwnd)
            .collect();
        due.into_iter().flat_map(|hwnd| self.probe(hwnd)).collect()
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        self.locations.values().copied().min()
    }
}
