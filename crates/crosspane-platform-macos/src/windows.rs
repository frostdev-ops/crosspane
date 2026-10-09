//! Quartz window snapshots, main-thread AppKit metadata, and public Accessibility operations.

use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{
    EventSink, Permission, PermissionState, PlatformError, WindowEvent, WindowInfo, WindowRole,
    WindowSource, WindowState,
};
use crosspane_types::geom::{PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, WindowId};
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace,
};
use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDisplayBounds, CGError, CGGetDisplaysWithPoint, CGRectMakeWithDictionaryRepresentation,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowBounds,
    kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

use crate::{main_thread::on_main, permissions};

const POLL: Duration = Duration::from_millis(250);
const QUERY_WAIT: Duration = Duration::from_millis(500);
const MAIN_WAIT: Duration = Duration::from_millis(250);

// Small helper windows cannot be projected reliably through Accessibility.
const MIN_LISTED_WINDOW_SIZE_PT: f64 = 48.0;

#[derive(Debug, Default)]
struct WindowAdmission {
    ids: HashSet<WindowId>,
}

impl WindowAdmission {
    fn snapshot(
        &mut self,
        mut windows: Vec<WindowInfo>,
        front: Option<i32>,
    ) -> (Vec<WindowInfo>, Option<WindowId>) {
        let present: HashSet<_> = windows.iter().map(|window| window.id).collect();
        self.ids.retain(|id| present.contains(id));
        // Admission lasts until disappearance: shrinking a projected window is not Removed.
        windows.retain(|window| {
            self.ids.contains(&window.id)
                || (window.frame.size.width >= MIN_LISTED_WINDOW_SIZE_PT
                    && window.frame.size.height >= MIN_LISTED_WINDOW_SIZE_PT
                    && self.ids.insert(window.id))
        });
        // Keyboard focus is on an admitted showing window, not a helper or hidden window.
        let focused = windows
            .iter()
            .find(|window| {
                window.state != WindowState::Hidden
                    && window.pid.and_then(|pid| i32::try_from(pid).ok()) == front
            })
            .map(|window| window.id);
        (windows, focused)
    }
}

static OWN_QUERY_BUSY: AtomicBool = AtomicBool::new(false);
struct OwnQueryLease;
impl Drop for OwnQueryLease {
    fn drop(&mut self) {
        OWN_QUERY_BUSY.store(false, Ordering::Release);
    }
}

/// Own on-screen Quartz window numbers and titles, including accessory-app windows. Waits at
/// most 500 ms; the single admission remains occupied until a timed-out read actually finishes.
/// This does not change the normal WindowSource's exclusion of our process.
pub fn own_windows() -> Result<Vec<(WindowId, String)>, PlatformError> {
    if OWN_QUERY_BUSY.swap(true, Ordering::AcqRel) {
        return Err(PlatformError::Timeout);
    }
    let lease = OwnQueryLease;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("mac-own-window-query".into())
        .spawn(move || {
            let _lease = lease;
            let _ = tx.send(own_window_snapshot());
        })
        .map_err(|e| PlatformError::Backend(format!("spawn own-window query: {e}")))?;
    rx.recv_timeout(QUERY_WAIT)
        .map_err(|_| PlatformError::Timeout)?
}

fn own_window_snapshot() -> Result<Vec<(WindowId, String)>, PlatformError> {
    let list = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .ok_or_else(|| PlatformError::Backend("Quartz own-window list unavailable".into()))?;
    // SAFETY: CGWindowListCopyWindowInfo returns a CFArray of CFDictionary CF objects.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    Ok(list
        .iter()
        .filter_map(|value| {
            let dictionary = value.downcast::<CFDictionary>().ok()?;
            parse_own_window(&dictionary, std::process::id())
        })
        .collect())
}

fn parse_own_window(dictionary: &CFDictionary, own_pid: u32) -> Option<(WindowId, String)> {
    // SAFETY: Quartz/fixture dictionaries have CFString keys and checked CF object values.
    let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
    // SAFETY: immutable public CoreGraphics dictionary keys.
    let (number, pid, name, on_screen) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowName,
            kCGWindowIsOnscreen,
        )
    };
    if dictionary.get(pid)?.downcast::<CFNumber>().ok()?.as_i64()? != i64::from(own_pid)
        || !dictionary
            .get(on_screen)?
            .downcast::<CFBoolean>()
            .ok()?
            .as_bool()
    {
        return None;
    }
    let number = u32::try_from(
        dictionary
            .get(number)?
            .downcast::<CFNumber>()
            .ok()?
            .as_i64()?,
    )
    .ok()?;
    Some((
        WindowId(u64::from(number)),
        dictionary
            .get(name)?
            .downcast::<CFString>()
            .ok()?
            .to_string(),
    ))
}

/// A Send handle; no AppKit objects cross the main-thread boundary.
#[derive(Debug)]
pub struct MacWindows {
    query: WindowQuery,
    admission: Arc<Mutex<WindowAdmission>>,
    stop: Option<Arc<AtomicBool>>,
}

impl MacWindows {
    pub fn new() -> Result<MacWindows, PlatformError> {
        Ok(Self {
            query: WindowQuery::new()?,
            admission: Arc::default(),
            stop: None,
        })
    }
}

impl Drop for MacWindows {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
    }
}

impl WindowSource for MacWindows {
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
        Ok(snapshot(&self.query, &self.admission)?.0)
    }

    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        Ok(snapshot(&self.query, &self.admission)?.1)
    }

    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        require_accessibility()?;
        // Off-screen too: a parked window sits on a twin display.
        let raw = self
            .query
            .list(true)?
            .into_iter()
            .find(|w| w.id == window)
            .ok_or(PlatformError::NotFound)?;
        let deadline = Instant::now() + MAIN_WAIT;
        let pid = raw.pid;
        let requested = on_main(MAIN_WAIT, move |_| {
            // on_main may execute a timed-out closure later. Never activate in that case.
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            require_accessibility()?;
            autoreleasepool(|_| {
                let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                    .ok_or(PlatformError::NotFound)?;
                Ok(app.activateWithOptions(NSApplicationActivationOptions::empty()))
            })
        })
        .and_then(|result| result);
        // Since macOS 14, activation is cooperative: a background agent's request can be ignored
        // (or refused). Accessibility's AXFrontmost, the route window managers use, isn't.
        let deadline = Instant::now() + Duration::from_secs(1);
        let frontmost =
            AxWindow::application(pid, deadline).set("AXFrontmost", CFBoolean::new(true));
        if let (Ok(false) | Err(_), Err(error)) = (&requested, &frontmost) {
            return Err(PlatformError::Backend(format!(
                "activation refused ({requested:?}); AXFrontmost: {error}"
            )));
        }
        // Raising is best effort: activation routes the keys, and some apps (Safari) refuse part
        // of it (AXMain: attribute unsupported).
        if let Err(error) = AxWindow::find(&raw, deadline).and_then(|window| window.raise()) {
            tracing::debug!(%error, "raising an activated window failed");
        }
        Ok(())
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        if self.stop.is_some() {
            return Err(PlatformError::Backend(
                "WindowSource::subscribe called twice".into(),
            ));
        }
        let initial = snapshot(&self.query, &self.admission)?;
        let query = self.query.clone();
        let admission = self.admission.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("mac-windows".into())
            .spawn(move || {
                let (mut last, mut focus) = initial;
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                for window in &last {
                    sink.send(WindowEvent::Added(window.clone()));
                }
                sink.send(WindowEvent::Focused(focus));
                while !worker_stop.load(Ordering::Acquire) {
                    std::thread::sleep(POLL);
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let (next, next_focus) = match snapshot(&query, &admission) {
                        Ok(snapshot) => snapshot,
                        // A failed observation is not evidence that every window disappeared.
                        Err(error) => {
                            tracing::warn!(%error, "window snapshot failed");
                            continue;
                        }
                    };
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    emit_window_changes(&last, &next, |event| sink.send(event));
                    if next_focus != focus {
                        sink.send(WindowEvent::Focused(next_focus));
                    }
                    last = next;
                    focus = next_focus;
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn window poll thread: {e}")))?;
        self.stop = Some(stop);
        Ok(())
    }
}

fn emit_window_changes(
    last: &[WindowInfo],
    next: &[WindowInfo],
    mut send: impl FnMut(WindowEvent),
) {
    let previous: HashMap<_, _> = last.iter().map(|window| (window.id, window)).collect();
    for window in next {
        match previous.get(&window.id) {
            None => send(WindowEvent::Added(window.clone())),
            Some(old) if **old != *window => send(WindowEvent::Changed(window.clone())),
            Some(_) => {}
        }
    }
    let next_ids: HashSet<_> = next.iter().map(|window| window.id).collect();
    for window in last {
        if !next_ids.contains(&window.id) {
            send(WindowEvent::Removed(window.id));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RawWindow {
    pub(crate) id: WindowId,
    pub(crate) pid: i32,
    pub(crate) title: String,
    owner: String,
    pub(crate) frame: RectLogical,
    /// `kCGWindowIsOnscreen`: the window is ordered in on a Space a display is showing. The key is
    /// absent for windows on other Spaces, minimized and hidden windows.
    pub(crate) on_screen: bool,
}

// Used by the private_vdisplay module's tests only.
#[cfg(all(test, feature = "private-vdisplay"))]
impl RawWindow {
    /// A fixture without going through a Quartz dictionary.
    pub(crate) fn fixture(id: u64, pid: i32, frame: RectLogical, on_screen: bool) -> Self {
        Self {
            id: WindowId(id),
            pid,
            title: String::new(),
            owner: String::new(),
            frame,
            on_screen,
        }
    }
}

type QueryReply = mpsc::SyncSender<Result<Vec<RawWindow>, PlatformError>>;

#[cfg(all(test, feature = "private-vdisplay"))]
impl WindowQuery {
    /// A query that answers from a script instead of WindowServer: one reply per request, then
    /// `Timeout`. Lets tests make a Quartz read fail or change between operations.
    pub(crate) fn scripted(replies: Vec<Result<Vec<RawWindow>, PlatformError>>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(bool, QueryReply)>(1);
        std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            while let Ok((_, reply)) = rx.recv() {
                let _ = reply.send(replies.next().unwrap_or(Err(PlatformError::Timeout)));
            }
        });
        Self(tx)
    }
}

/// A single worker bounds WindowServer waits without accumulating blocked query threads.
#[derive(Clone, Debug)]
pub(crate) struct WindowQuery(mpsc::SyncSender<(bool, QueryReply)>);

impl WindowQuery {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        let (tx, rx) = mpsc::sync_channel::<(bool, QueryReply)>(1);
        std::thread::Builder::new()
            .name("mac-window-query".into())
            .spawn(move || {
                while let Ok((all, reply)) = rx.recv() {
                    let _ = reply.send(raw_windows(all));
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn window query thread: {e}")))?;
        Ok(Self(tx))
    }

    pub(crate) fn list(&self, all: bool) -> Result<Vec<RawWindow>, PlatformError> {
        self.list_until(all, Instant::now() + QUERY_WAIT)
    }

    pub(crate) fn list_until(
        &self,
        all: bool,
        deadline: Instant,
    ) -> Result<Vec<RawWindow>, PlatformError> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(PlatformError::Timeout)?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.0.try_send((all, tx)).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => PlatformError::Timeout,
            mpsc::TrySendError::Disconnected(_) => {
                PlatformError::Backend("window query stopped".into())
            }
        })?;
        rx.recv_timeout(remaining.min(QUERY_WAIT))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    PlatformError::Backend("window query stopped".into())
                }
            })?
    }
}

fn raw_windows(all: bool) -> Result<Vec<RawWindow>, PlatformError> {
    let options = if all {
        CGWindowListOption::OptionAll
    } else {
        CGWindowListOption::OptionOnScreenOnly
    } | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID)
        .ok_or_else(|| PlatformError::Backend("Quartz window list unavailable".into()))?;
    // SAFETY: CGWindowListCopyWindowInfo returns a CFArray of CFDictionary CF objects.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    Ok(list
        .iter()
        .filter_map(|value| {
            let dictionary = value.downcast::<CFDictionary>().ok()?;
            parse_window(&dictionary)
        })
        .collect())
}

fn parse_window(dictionary: &CFDictionary) -> Option<RawWindow> {
    // SAFETY: Quartz window dictionaries (and our canned fixtures) have CFString keys and CF
    // object values. Each value's concrete type is checked before use below.
    let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
    // SAFETY: immutable CFString constants exported by CoreGraphics.
    let (number, pid, layer, bounds, name, owner) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowBounds,
            kCGWindowName,
            kCGWindowOwnerName,
        )
    };
    if dictionary
        .get(layer)?
        .downcast::<CFNumber>()
        .ok()?
        .as_i32()?
        != 0
    {
        return None;
    }
    let pid = dictionary.get(pid)?.downcast::<CFNumber>().ok()?.as_i32()?;
    if pid <= 0 || u32::try_from(pid).ok()? == std::process::id() {
        return None;
    }
    let id = u32::try_from(
        dictionary
            .get(number)?
            .downcast::<CFNumber>()
            .ok()?
            .as_i64()?,
    )
    .ok()?;
    let bounds = dictionary.get(bounds)?.downcast::<CFDictionary>().ok()?;
    let mut frame = CGRect::default();
    // SAFETY: bounds is a runtime-checked CFDictionary; frame is valid writable CGRect storage.
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut frame) } {
        return None;
    }
    let frame = RectLogical::new(
        PointLogical::new(frame.origin.x, frame.origin.y),
        SizeLogical::new(frame.size.width, frame.size.height),
    );
    if !valid_frame(frame) {
        return None;
    }
    let string = |key| {
        dictionary
            .get(key)
            .and_then(|v| v.downcast::<CFString>().ok())
            .map(|v| v.to_string())
            .unwrap_or_default()
    };
    // SAFETY: immutable CFString constant exported by CoreGraphics. The key is absent (not false)
    // for a window that isn't ordered on screen.
    let on_screen = dictionary
        .get(unsafe { kCGWindowIsOnscreen })
        .and_then(|v| v.downcast::<CFBoolean>().ok())
        .is_some_and(|v| v.as_bool());
    Some(RawWindow {
        id: WindowId(u64::from(id)),
        pid,
        title: string(name),
        owner: string(owner),
        frame,
        on_screen,
    })
}

/// A window and its display's bounds are equal to this many points (Quartz reports whole points;
/// the slack only absorbs rounding).
const BOUNDS_SLACK: f64 = 1.0;

/// Two Quartz or AX frames are the same window frame, within [`BOUNDS_SLACK`].
pub(crate) fn bounds_equal(a: RectLogical, b: RectLogical) -> bool {
    (a.origin.x - b.origin.x).abs() <= BOUNDS_SLACK
        && (a.origin.y - b.origin.y).abs() <= BOUNDS_SLACK
        && (a.size.width - b.size.width).abs() <= BOUNDS_SLACK
        && (a.size.height - b.size.height).abs() <= BOUNDS_SLACK
}

/// The state the engine sees: a window that isn't on screen is
/// `Hidden` (another Space, minimized, hidden app) and stays in the list; an on-screen window that
/// fills its display is `Fullscreen`; anything else is `Normal`. A window leaves the list only
/// when it closes.
fn window_state(on_screen: bool, frame: RectLogical, display: Option<RectLogical>) -> WindowState {
    if !on_screen {
        WindowState::Hidden
    } else if display.is_some_and(|display| bounds_equal(frame, display)) {
        WindowState::Fullscreen
    } else {
        WindowState::Normal
    }
}

fn display_bounds(display: DisplayId) -> Option<RectLogical> {
    let bounds = CGDisplayBounds(display.0);
    let frame = RectLogical::new(
        PointLogical::new(bounds.origin.x, bounds.origin.y),
        SizeLogical::new(bounds.size.width, bounds.size.height),
    );
    valid_frame(frame).then_some(frame)
}

/// Regular (Dock-visible) apps other than the Dock itself own projectable windows.
fn projectable_app(regular: bool, app_id: &str) -> bool {
    regular && app_id != "com.apple.dock"
}

fn map_window(
    raw: RawWindow,
    app_id: String,
    display: Option<DisplayId>,
    display_bounds: Option<RectLogical>,
) -> WindowInfo {
    WindowInfo {
        id: raw.id,
        title: raw.title,
        app_id,
        pid: u32::try_from(raw.pid).ok(),
        display,
        frame: raw.frame,
        state: window_state(raw.on_screen, raw.frame, display_bounds),
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn snapshot(
    query: &WindowQuery,
    admission: &Arc<Mutex<WindowAdmission>>,
) -> Result<(Vec<WindowInfo>, Option<WindowId>), PlatformError> {
    // Every window, on screen or not: a window on another Space (an app's fullscreen Space, say) is
    // `Hidden`, not gone, so a projection of it never ends by `Removed` until it closes.
    let raw = query.list(true)?;
    let (windows, front) = on_main(MAIN_WAIT, move |_| {
        autoreleasepool(|_| {
            let front = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier());
            // The full list has every hidden window of every app: ask AppKit once per process.
            let mut apps: HashMap<i32, Option<String>> = HashMap::new();
            let windows: Vec<_> = raw
                .into_iter()
                .filter_map(|raw| {
                    let app_id = apps
                        .entry(raw.pid)
                        .or_insert_with(|| {
                            let app =
                                NSRunningApplication::runningApplicationWithProcessIdentifier(
                                    raw.pid,
                                )?;
                            let regular =
                                app.activationPolicy() == NSApplicationActivationPolicy::Regular;
                            let app_id = app
                                .bundleIdentifier()
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| raw.owner.clone());
                            projectable_app(regular, &app_id).then_some(app_id)
                        })
                        .clone()?;
                    let display = display_for_frame(raw.frame).ok();
                    let bounds = display.and_then(display_bounds);
                    Some(map_window(raw, app_id, display, bounds))
                })
                .collect();
            (windows, front)
        })
    })?;
    // Never hold admission across AppKit/main-thread work, or commit a timed-out observation.
    let mut admission = admission
        .lock()
        .map_err(|_| PlatformError::Backend("Mac window admission lock poisoned".into()))?;
    Ok(admission.snapshot(windows, front))
}

pub(crate) fn display_for_frame(frame: RectLogical) -> Result<DisplayId, PlatformError> {
    let center = frame.center();
    let mut display = 0;
    let mut count = 0;
    // SAFETY: one display slot and count are valid writable storage; capacity is one.
    let status = unsafe {
        CGGetDisplaysWithPoint(
            CGPoint::new(center.x, center.y),
            1,
            &mut display,
            &mut count,
        )
    };
    if status != CGError::Success {
        return Err(PlatformError::Backend(format!(
            "display at window centre: CGError {}",
            status.0
        )));
    }
    if count == 0 {
        return Err(PlatformError::NotFound);
    }
    Ok(DisplayId(display))
}

pub(crate) fn valid_frame(frame: RectLogical) -> bool {
    [
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    ]
    .into_iter()
    .all(f64::is_finite)
        && frame.size.width > 0.0
        && frame.size.height > 0.0
}

pub(crate) fn require_accessibility() -> Result<(), PlatformError> {
    if permissions::state(Permission::Accessibility) != PermissionState::Granted {
        return Err(PlatformError::PermissionDenied(Permission::Accessibility));
    }
    Ok(())
}

fn ax_result(status: AXError) -> Result<(), PlatformError> {
    match status {
        AXError::Success => Ok(()),
        AXError::APIDisabled => Err(PlatformError::PermissionDenied(Permission::Accessibility)),
        AXError::InvalidUIElement => Err(PlatformError::NotFound),
        AXError::CannotComplete => Err(PlatformError::Timeout),
        _ => Err(PlatformError::Backend(format!(
            "Accessibility error {}",
            status.0
        ))),
    }
}

/// What the matcher knows of one AX window of the app.
#[derive(Clone, Debug, PartialEq)]
struct AxCandidate {
    /// `AXTitle`; `None` when it wasn't read (the Quartz window is untitled).
    title: Option<String>,
    frame: RectLogical,
}

#[derive(Debug, PartialEq, Eq)]
enum AxMatch {
    One(usize),
    /// Several windows are equally good matches; moving any of them could be wrong.
    Ambiguous,
    /// No AX window is this Quartz window.
    Missing,
}

/// The outcome of looking up a Quartz window in its app's AX windows.
#[derive(Debug)]
pub(crate) enum AxLookup {
    Found(AxWindow),
    /// AX has no window for it (the description names frames only).
    Missing(String),
}

/// Which callers a lookup serves, and so how strictly it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxPolicy {
    /// M1 mirror parking (`parking.rs`): the policy from before WP-2.45a, unchanged. The title
    /// is a filter, and a lone candidate matches whatever its frame.
    Mirror,
    /// M2 twin parking: the title is a preference, and there is no lone-candidate fallback. A
    /// window is another window unless its title or its frame says it is this one.
    #[cfg_attr(not(feature = "private-vdisplay"), allow(dead_code))]
    Twin,
    /// Strict geometry verification for the drag's observed native tile.
    VerifiedFrame,
}

/// The slack between the Quartz frame and an AX frame of the same window, in points.
const AX_SLACK: f64 = 2.0;

fn ax_frame_matches(a: RectLogical, b: RectLogical) -> bool {
    (a.origin.x - b.origin.x).abs() <= AX_SLACK
        && (a.origin.y - b.origin.y).abs() <= AX_SLACK
        && (a.size.width - b.size.width).abs() <= AX_SLACK
        && (a.size.height - b.size.height).abs() <= AX_SLACK
}

/// Pick the AX window that is the Quartz window `title`/`frame`, by `policy`.
fn match_ax_window(
    policy: AxPolicy,
    title: &str,
    frame: RectLogical,
    candidates: &[AxCandidate],
) -> AxMatch {
    match policy {
        AxPolicy::Mirror => match_mirror(title, frame, candidates),
        AxPolicy::Twin => match_twin(title, frame, candidates),
        AxPolicy::VerifiedFrame => {
            let mut hits = candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| ax_frame_matches(candidate.frame, frame));
            match (hits.next(), hits.next()) {
                (Some((index, _)), None) => AxMatch::One(index),
                (Some(_), Some(_)) => AxMatch::Ambiguous,
                _ => AxMatch::Missing,
            }
        }
    }
}

fn match_fullscreen_ax(
    policy: AxPolicy,
    title: &str,
    frame: RectLogical,
    candidates: &[AxCandidate],
    fullscreen: bool,
) -> AxMatch {
    if !fullscreen || policy == AxPolicy::VerifiedFrame {
        return match_ax_window(policy, title, frame, candidates);
    }
    let framed = |c: &AxCandidate| ax_frame_matches(c.frame, frame);
    let titled = |c: &AxCandidate| !title.is_empty() && c.title.as_deref() == Some(title);
    let tiers: [&dyn Fn(&AxCandidate) -> bool; 3] = [&|c| framed(c) && titled(c), &framed, &titled];
    for tier in tiers {
        let mut hits = candidates.iter().enumerate().filter(|(_, c)| tier(c));
        match (hits.next(), hits.next()) {
            (Some((i, _)), None) => return AxMatch::One(i),
            (Some(_), Some(_)) => return AxMatch::Ambiguous,
            _ => {}
        }
    }
    AxMatch::Missing
}

pub(crate) const FULLSCREEN_WAIT: Duration = Duration::from_secs(2);

/// Only an on-screen Quartz window filling its display admits the relaxed AX ranking.
pub(crate) fn quartz_fullscreen(raw: &RawWindow) -> bool {
    let bounds = display_for_frame(raw.frame).ok().and_then(display_bounds);
    window_state(raw.on_screen, raw.frame, bounds) == WindowState::Fullscreen
}

/// Both parking adapters use this ensure sequence; the injected operations also test it without
/// AX or a window server. An unsupported public button never falls back to an undocumented API.
pub(crate) fn ensure_fullscreen_with(
    desired: bool,
    mut observe: impl FnMut() -> Result<(RawWindow, bool), PlatformError>,
    press: impl FnOnce(&RawWindow) -> Result<bool, PlatformError>,
    mut pause: impl FnMut() -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    let (raw, actual) = observe()?;
    if actual == desired && (desired || raw.on_screen) {
        return Ok(());
    }
    if actual != desired && (!raw.on_screen || !press(&raw)?) {
        return Err(PlatformError::Unsupported(
            "window has no accessible full-screen button",
        ));
    }
    let mut previous = None;
    loop {
        pause()?;
        let (raw, actual) = observe()?;
        // Leaving a Space briefly hides and animates the window. Keep the restore guard:
        // don't write AX geometry until two visible normal frames agree.
        if actual == desired
            && (desired
                || (raw.on_screen && previous.is_some_and(|frame| bounds_equal(frame, raw.frame))))
        {
            return Ok(());
        }
        previous = (raw.on_screen && !actual).then_some(raw.frame);
    }
}

pub(crate) fn fullscreen_pause(deadline: Instant) -> Result<(), PlatformError> {
    let sleep = next_sleep(deadline, Instant::now(), Duration::from_millis(50))
        .ok_or(PlatformError::Timeout)?;
    std::thread::sleep(sleep);
    Ok(())
}

pub(crate) fn fullscreen_press_needed(
    raw: &RawWindow,
    actual: bool,
    desired: bool,
    deadline: Instant,
) -> Result<bool, PlatformError> {
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    if actual == desired && (desired || raw.on_screen) {
        return Ok(false);
    }
    if !raw.on_screen && !actual {
        return Err(PlatformError::Unsupported("window is not showing"));
    }
    Ok(true)
}

fn prepare_fullscreen_press_with(
    still_needed: impl FnOnce() -> Result<bool, PlatformError>,
    prepare: impl FnOnce() -> Result<(), PlatformError>,
) -> Result<bool, PlatformError> {
    let needed = still_needed()?;
    if needed {
        prepare()?;
    }
    Ok(needed)
}

pub(crate) fn next_sleep(deadline: Instant, now: Instant, poll: Duration) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|left| !left.is_zero())
        .map(|left| left.min(poll))
}

/// The previous matching, exactly: candidates are the AX windows with the Quartz window's title
/// (all of them when it has none). The frame only picks between several candidates: right after an
/// AX move or resize Quartz still reports the old frame for a while, so a lone candidate matches
/// whatever its frame.
fn match_mirror(title: &str, frame: RectLogical, candidates: &[AxCandidate]) -> AxMatch {
    let titled = !title.is_empty();
    let pool: Vec<usize> = (0..candidates.len())
        .filter(|&i| !titled || candidates[i].title.as_deref() == Some(title))
        .collect();
    let framed: Vec<usize> = pool
        .iter()
        .copied()
        .filter(|&i| ax_frame_matches(candidates[i].frame, frame))
        .collect();
    match (framed.as_slice(), pool.len()) {
        ([one], _) => AxMatch::One(*one),
        ([], 1) => AxMatch::One(pool[0]),
        ([_, _, ..], _) => AxMatch::Ambiguous,
        _ => AxMatch::Missing,
    }
}

/// The twin's matching: in order, an AX window with the title and the frame, with the title, with
/// the frame. Equal candidates are `Ambiguous`. Right after an AX move Quartz still reports the old
/// frame for a while, so a window with the title matches whatever its frame. A window whose title
/// differs or is empty is the one only if its frame matches the Quartz frame. There is no lone-
/// window fallback: WebKit's fullscreen window is title-less and often the only AX window while the
/// page's own window sits on another Space, and writing through it moves the wrong window.
fn match_twin(title: &str, frame: RectLogical, candidates: &[AxCandidate]) -> AxMatch {
    let titled = !title.is_empty();
    let title_is = |c: &AxCandidate| titled && c.title.as_deref() == Some(title);
    let frame_is = |c: &AxCandidate| ax_frame_matches(c.frame, frame);
    let tiers: [&dyn Fn(&AxCandidate) -> bool; 3] =
        [&|c| title_is(c) && frame_is(c), &|c| title_is(c), &|c| {
            frame_is(c)
        }];
    for tier in tiers {
        let mut hits = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| tier(c))
            .map(|(index, _)| index);
        match (hits.next(), hits.next()) {
            (None, _) => {}
            (Some(one), None) => return AxMatch::One(one),
            (Some(_), Some(_)) => return AxMatch::Ambiguous,
        }
    }
    AxMatch::Missing
}

/// AX attribute names are public string macros in HIServices/AXAttributeConstants.h; the
/// generated crate omits those macros. AXRaise is from HIServices/AXActionConstants.h.
#[derive(Debug)]
pub(crate) struct AxWindow {
    element: CFRetained<AXUIElement>,
    deadline: Instant,
}

impl AxWindow {
    fn prepare(&self) -> Result<(), PlatformError> {
        require_accessibility()?;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(PlatformError::Timeout)?;
        // SAFETY: valid retained AX object; positive timeout bounds its synchronous messages.
        ax_result(unsafe {
            self.element
                .set_messaging_timeout(remaining.as_secs_f32().max(f32::MIN_POSITIVE))
        })
    }

    fn attribute(&self, name: &str) -> Result<CFRetained<CFType>, PlatformError> {
        self.attribute_opt(name)?
            .ok_or_else(|| PlatformError::Backend("empty AX attribute".into()))
    }

    /// `Ok(None)`: the element has no such attribute, or it has no value (a window without a title
    /// or a full-screen button).
    fn attribute_opt(&self, name: &str) -> Result<Option<CFRetained<CFType>>, PlatformError> {
        self.prepare()?;
        let mut value = std::ptr::null();
        // SAFETY: valid retained element/name and writable output pointer. Copy returns +1 ownership.
        let status = unsafe {
            self.element
                .copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
        };
        if matches!(status, AXError::AttributeUnsupported | AXError::NoValue) {
            return Ok(None);
        }
        ax_result(status)?;
        // SAFETY: successful Copy supplied a non-null +1 CF object, now owned by this handle.
        Ok(NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) }))
    }

    /// The window's `AXTitle`; empty when it has none.
    fn title(&self) -> Result<String, PlatformError> {
        match self.attribute_opt("AXTitle")? {
            None => Ok(String::new()),
            Some(title) => title
                .downcast::<CFString>()
                .map(|title| title.to_string())
                .map_err(|_| PlatformError::Backend("AXTitle is not a string".into())),
        }
    }

    fn set(&self, name: &str, value: &CFType) -> Result<(), PlatformError> {
        self.prepare()?;
        // SAFETY: element and typed CF value are alive; name is a public AX attribute.
        ax_result(unsafe {
            self.element
                .set_attribute_value(&CFString::from_str(name), value)
        })
    }

    /// The application element of process `pid` (for app-level attributes like AXFrontmost).
    fn application(pid: i32, deadline: Instant) -> Self {
        Self {
            // SAFETY: positive pid obtained from Quartz; creates a retained public AX application object.
            element: unsafe { AXUIElement::new_application(pid) },
            deadline,
        }
    }

    /// The AX window of `raw` for mirror parking. Ordinary windows keep `AxPolicy::Mirror`;
    /// confirmed fullscreen windows may match display bounds regardless of title.
    pub(crate) fn find(raw: &RawWindow, deadline: Instant) -> Result<Self, PlatformError> {
        match Self::lookup_with(raw, deadline, AxPolicy::Mirror)? {
            AxLookup::Found(window) => Ok(window),
            AxLookup::Missing(why) => {
                tracing::debug!(reason = %why, "AX window not found");
                Err(PlatformError::NotFound)
            }
        }
    }

    /// Match the Quartz window to one of its app's AX windows by the twin's policy
    /// ([`AxPolicy::Twin`]). AX lists only the windows on Spaces a display is showing, so a window
    /// on another Space (or one covered by a title-less fullscreen stand-in) has no AX
    /// counterpart: that is [`AxLookup::Missing`], not an error, and callers decide whether it
    /// matters.
    pub(crate) fn lookup(raw: &RawWindow, deadline: Instant) -> Result<AxLookup, PlatformError> {
        Self::lookup_with(raw, deadline, AxPolicy::Twin)
    }

    /// Drag tiling may write only a unique AX window at the observed Quartz frame. No title,
    /// lone-window or fullscreen fallback admits a different frame.
    pub(crate) fn lookup_verified_frame(
        raw: &RawWindow,
        deadline: Instant,
    ) -> Result<AxLookup, PlatformError> {
        Self::lookup_with(raw, deadline, AxPolicy::VerifiedFrame)
    }

    fn lookup_with(
        raw: &RawWindow,
        deadline: Instant,
        policy: AxPolicy,
    ) -> Result<AxLookup, PlatformError> {
        require_accessibility()?;
        let fullscreen = quartz_fullscreen(raw);
        let app = Self::application(raw.pid, deadline);
        let values = app
            .attribute_opt("AXWindows")?
            .ok_or(PlatformError::NotFound)?
            .downcast::<CFArray>()
            .map_err(|_| PlatformError::Backend("AXWindows is not an array".into()))?;
        // SAFETY: the public AXWindows attribute is an array of AXUIElement CF objects;
        // each element is additionally downcast before use.
        let values = unsafe { values.cast_unchecked::<CFType>() };
        let mut windows = Vec::new();
        let mut candidates = Vec::new();
        for value in values.iter() {
            let element = value
                .downcast::<AXUIElement>()
                .map_err(|_| PlatformError::Backend("invalid AX window".into()))?;
            let window = Self { element, deadline };
            let frame = match window.frame() {
                Ok(frame) => frame,
                Err(PlatformError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            // Only worth a round trip when Quartz has a title. Mirror reads it as it always did,
            // failing on a window whose title can't be read; the twin treats that as untitled.
            let title = if raw.title.is_empty() || policy == AxPolicy::VerifiedFrame {
                None
            } else if policy == AxPolicy::Mirror && !fullscreen {
                Some(
                    window
                        .attribute("AXTitle")?
                        .downcast::<CFString>()
                        .map_err(|_| PlatformError::Backend("AXTitle is not a string".into()))?
                        .to_string(),
                )
            } else {
                Some(window.title()?)
            };
            candidates.push(AxCandidate { title, frame });
            windows.push(window);
        }
        match match_fullscreen_ax(policy, &raw.title, raw.frame, &candidates, fullscreen) {
            AxMatch::One(index) => Ok(AxLookup::Found(windows.swap_remove(index))),
            AxMatch::Ambiguous => Err(PlatformError::Backend("ambiguous AX window match".into())),
            // Frames only: titles never go into logs.
            AxMatch::Missing => Ok(AxLookup::Missing(format!(
                "Quartz window ({:.0},{:.0} {:.0}x{:.0}) has no matching AX window among {} [{}]",
                raw.frame.origin.x,
                raw.frame.origin.y,
                raw.frame.size.width,
                raw.frame.size.height,
                values.len(),
                candidates
                    .iter()
                    .map(|c| format!(
                        "({:.0},{:.0} {:.0}x{:.0})",
                        c.frame.origin.x, c.frame.origin.y, c.frame.size.width, c.frame.size.height
                    ))
                    .collect::<Vec<_>>()
                    .join(" "),
            ))),
        }
    }

    /// Press the window's full-screen button (`kAXFullScreenButtonAttribute`, `kAXPressAction`),
    /// which toggles native fullscreen. `Ok(false)`: the window has no such button (a title-less
    /// window, an app that doesn't do native fullscreen) and nothing was pressed.
    /// `still_needed` rechecks after button lookup; false is a successful no-op.
    pub(crate) fn press_fullscreen_button(
        &self,
        still_needed: impl FnOnce() -> Result<bool, PlatformError>,
    ) -> Result<bool, PlatformError> {
        let Some(button) = self.attribute_opt("AXFullScreenButton")? else {
            return Ok(false);
        };
        let element = button
            .downcast::<AXUIElement>()
            .map_err(|_| PlatformError::Backend("AXFullScreenButton is not an element".into()))?;
        let button = Self {
            element,
            deadline: self.deadline,
        };
        // Lookup may wait: the adapter's final Quartz check runs after it, before AXPress.
        if !prepare_fullscreen_press_with(still_needed, || button.prepare())? {
            return Ok(true);
        }
        // SAFETY: valid retained AX button element; AXPress is a public action name
        // (HIServices/AXActionConstants.h kAXPressAction).
        let status = unsafe {
            button
                .element
                .perform_action(&CFString::from_str("AXPress"))
        };
        if status == AXError::ActionUnsupported {
            return Ok(false);
        }
        ax_result(status)?;
        Ok(true)
    }

    pub(crate) fn frame(&self) -> Result<RectLogical, PlatformError> {
        let position = self
            .attribute("AXPosition")?
            .downcast::<AXValue>()
            .map_err(|_| PlatformError::Backend("AXPosition is not AXValue".into()))?;
        let size = self
            .attribute("AXSize")?
            .downcast::<AXValue>()
            .map_err(|_| PlatformError::Backend("AXSize is not AXValue".into()))?;
        let mut point = CGPoint::default();
        let mut dimensions = CGSize::default();
        // SAFETY: type-checked AXValues and correctly sized writable CGPoint/CGSize storage;
        // AXValueGetValue returns false when the encoded structure type does not match.
        let valid = unsafe {
            position.value(AXValueType::CGPoint, NonNull::from(&mut point).cast())
                && size.value(AXValueType::CGSize, NonNull::from(&mut dimensions).cast())
        };
        let frame = RectLogical::new(
            PointLogical::new(point.x, point.y),
            SizeLogical::new(dimensions.width, dimensions.height),
        );
        if !valid || !valid_frame(frame) {
            return Err(PlatformError::Backend("invalid AX frame".into()));
        }
        Ok(frame)
    }

    pub(crate) fn resize(&self, size: SizeLogical) -> Result<(), PlatformError> {
        let mut dimensions = CGSize::new(size.width, size.height);
        // SAFETY: public CGSize AXValue type and valid initialized CGSize storage; AX copies it.
        let value =
            unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&mut dimensions).cast()) }
                .ok_or_else(|| PlatformError::Backend("create AX size".into()))?;
        self.set("AXSize", &value)
    }

    pub(crate) fn restore(&self, frame: RectLogical) -> Result<(), PlatformError> {
        self.restore_guarded(frame, &mut || true).map(|_| ())
    }

    /// Position only: a successful parking restore owns size, which may be read-only or newer
    /// than Quartz. Recheck caller authority immediately before attempting this public AX write.
    pub(crate) fn position_guarded(
        &self,
        origin: PointLogical,
        allowed: &mut dyn FnMut() -> bool,
    ) -> Result<bool, PlatformError> {
        position_with(origin, allowed, |attribute, origin| {
            let mut position = CGPoint::new(origin.x, origin.y);
            // SAFETY: public CGPoint AXValue type and initialized storage; AX copies the value.
            let value =
                unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut position).cast()) }
                    .ok_or_else(|| PlatformError::Backend("create AX position".into()))?;
            self.set(attribute, &value)
        })
    }

    /// [`AxWindow::restore`], but `allowed` is asked immediately before each of the two writes
    /// (size, then position). `Ok(false)`: it said no, and nothing further was written (a no
    /// after the size write leaves the new size). The twin uses it to re-read Quartz after the AX
    /// lookup, which can take up to two seconds, and before every frame write.
    pub(crate) fn restore_guarded(
        &self,
        frame: RectLogical,
        allowed: &mut dyn FnMut() -> bool,
    ) -> Result<bool, PlatformError> {
        if !allowed() {
            return Ok(false);
        }
        self.resize(frame.size)?;
        if !allowed() {
            return Ok(false);
        }
        let mut position = CGPoint::new(frame.origin.x, frame.origin.y);
        // SAFETY: public CGPoint AXValue type and valid initialized CGPoint storage; AX copies it.
        let value =
            unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut position).cast()) }
                .ok_or_else(|| PlatformError::Backend("create AX position".into()))?;
        self.set("AXPosition", &value)?;
        Ok(true)
    }

    fn raise(&self) -> Result<(), PlatformError> {
        self.prepare()?;
        // SAFETY: valid retained AX window; AXRaise is a public action name.
        ax_result(unsafe { self.element.perform_action(&CFString::from_str("AXRaise")) })?;
        self.set("AXMain", CFBoolean::new(true))
    }
}

fn position_with(
    origin: PointLogical,
    allowed: &mut dyn FnMut() -> bool,
    position: impl FnOnce(&str, PointLogical) -> Result<(), PlatformError>,
) -> Result<bool, PlatformError> {
    if !origin.x.is_finite() || !origin.y.is_finite() {
        return Err(PlatformError::Backend("invalid AX position".into()));
    }
    if !allowed() {
        return Ok(false);
    }
    position("AXPosition", origin)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::CGRectCreateDictionaryRepresentation;

    fn listed_window(width: f64, height: f64) -> WindowInfo {
        map_window(
            RawWindow {
                id: WindowId(42),
                pid: 123,
                title: "fixture".into(),
                owner: "fixture".into(),
                frame: rect(-100.0, -200.0, width, height),
                on_screen: true,
            },
            "io.test.fixture".into(),
            None,
            None,
        )
    }

    fn changes(last: &[WindowInfo], next: &[WindowInfo]) -> Vec<WindowEvent> {
        let mut events = Vec::new();
        emit_window_changes(last, next, |event| events.push(event));
        events
    }

    #[test]
    fn listing_filter_requires_48_points_in_both_dimensions() {
        for (width, height, expected) in [
            (1.0, 1.0, false),
            (1800.0, 39.0, false),
            (39.0, 1800.0, false),
            (47.99, 48.0, false),
            (48.0, 47.99, false),
            (48.0, 48.0, true),
            (1800.0, 48.0, true),
        ] {
            let (windows, focus) =
                WindowAdmission::default().snapshot(vec![listed_window(width, height)], Some(123));
            assert_eq!(!windows.is_empty(), expected, "{width} x {height}");
            assert_eq!(focus.is_some(), expected);
        }
    }

    #[test]
    fn listing_filter_tiny_from_creation_has_no_added_or_focus() {
        let mut admission = WindowAdmission::default();
        let (initial, focus) = admission.snapshot(vec![listed_window(1.0, 1.0)], Some(123));
        assert!(initial.is_empty());
        assert!(focus.is_none());
        assert!(changes(&[], &initial).is_empty());
        let (next, _) = admission.snapshot(vec![listed_window(1800.0, 39.0)], Some(123));
        assert!(changes(&initial, &next).is_empty());
    }

    #[test]
    fn listing_filter_tiny_growth_to_threshold_emits_added() {
        let mut admission = WindowAdmission::default();
        let (initial, _) = admission.snapshot(vec![listed_window(1.0, 1.0)], Some(123));
        let (next, focus) = admission.snapshot(vec![listed_window(48.0, 48.0)], Some(123));
        assert_eq!(focus, Some(WindowId(42)));
        assert!(matches!(changes(&initial, &next).as_slice(),
            [WindowEvent::Added(window)] if window.id == WindowId(42)));
    }

    #[test]
    fn listing_filter_admitted_shrink_keeps_identity_without_removed() {
        let mut admission = WindowAdmission::default();
        let (initial, _) = admission.snapshot(vec![listed_window(300.0, 200.0)], Some(123));
        let (next, focus) = admission.snapshot(vec![listed_window(1.0, 1.0)], Some(123));
        assert_eq!(focus, Some(WindowId(42)));
        assert_eq!(next[0].id, initial[0].id);
        assert_eq!(next[0].frame.size, SizeLogical::new(1.0, 1.0));
        assert!(matches!(changes(&initial, &next).as_slice(),
            [WindowEvent::Changed(window)] if window.id == WindowId(42)));
    }

    #[test]
    fn listing_filter_disappearance_retires_admission_before_id_reuse() {
        let mut admission = WindowAdmission::default();
        let (initial, _) = admission.snapshot(vec![listed_window(48.0, 48.0)], Some(123));
        let (closed, _) = admission.snapshot(Vec::new(), Some(123));
        assert!(matches!(
            changes(&initial, &closed).as_slice(),
            [WindowEvent::Removed(WindowId(42))]
        ));
        let (reused, focus) = admission.snapshot(vec![listed_window(1.0, 1.0)], Some(123));
        assert!(reused.is_empty());
        assert!(focus.is_none());
        assert!(changes(&closed, &reused).is_empty());
    }

    #[test]
    fn drag_verified_frame_lookup_refuses_lone_unrelated_and_equal_candidates() {
        let tile = rect(900.0, 0.0, 900.0, 1100.0);
        let unrelated = rect(100.0, 100.0, 400.0, 300.0);
        assert_eq!(
            match_ax_window(
                AxPolicy::VerifiedFrame,
                "",
                tile,
                &[candidate(None, unrelated)]
            ),
            AxMatch::Missing
        );
        assert_eq!(
            match_fullscreen_ax(
                AxPolicy::VerifiedFrame,
                "Tile",
                tile,
                &[candidate(Some("Tile"), unrelated)],
                true
            ),
            AxMatch::Missing
        );
        assert_eq!(
            match_ax_window(
                AxPolicy::VerifiedFrame,
                "Tile",
                tile,
                &[candidate(Some("Other"), tile)]
            ),
            AxMatch::One(0)
        );
        assert_eq!(
            match_ax_window(
                AxPolicy::VerifiedFrame,
                "Tile",
                tile,
                &[
                    candidate(Some("Tile"), tile),
                    candidate(Some("Other"), tile)
                ]
            ),
            AxMatch::Ambiguous
        );
    }

    #[test]
    fn drag_position_only_accepts_read_only_size_and_rechecks_before_write() {
        use std::cell::Cell;
        let writes = Cell::new(0);
        assert!(
            position_with(
                PointLogical::new(40.0, 50.0),
                &mut || true,
                |attribute, _| {
                    if attribute == "AXSize" {
                        return Err(PlatformError::Unsupported("read-only size"));
                    }
                    assert_eq!(attribute, "AXPosition");
                    writes.set(writes.get() + 1);
                    Ok(())
                }
            )
            .unwrap()
        );
        assert_eq!(writes.get(), 1);
        assert!(
            !position_with(PointLogical::new(40.0, 50.0), &mut || false, |_, _| panic!(
                "position must not be written"
            ))
            .unwrap()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_press_refreshes_timeout_after_the_fresh_quartz_check() {
        use std::cell::{Cell, RefCell};
        let start = Instant::now();
        let deadline = start + FULLSCREEN_WAIT;
        let clock = Cell::new(start + Duration::from_millis(200));
        let timeout = Cell::new(None);
        let sequence = RefCell::new(Vec::new());
        assert!(
            prepare_fullscreen_press_with(
                || {
                    sequence.borrow_mut().push("fresh Quartz");
                    clock.set(clock.get() + Duration::from_millis(500));
                    Ok(true)
                },
                || {
                    sequence.borrow_mut().push("prepare AXPress");
                    timeout.set(deadline.checked_duration_since(clock.get()));
                    Ok(())
                },
            )
            .unwrap()
        );
        assert_eq!(*sequence.borrow(), ["fresh Quartz", "prepare AXPress"]);
        assert_eq!(timeout.get(), Some(Duration::from_millis(1300)));
        assert!(
            !prepare_fullscreen_press_with(
                || Ok(false),
                || panic!("a satisfied request must not prepare an AX press"),
            )
            .unwrap()
        );
    }

    #[test]
    fn fullscreen_press_does_not_start_after_its_fresh_check_exhausts_the_deadline() {
        use std::cell::Cell;
        let start = Instant::now();
        let deadline = start + FULLSCREEN_WAIT;
        for elapsed in [FULLSCREEN_WAIT, FULLSCREEN_WAIT + Duration::from_millis(1)] {
            let clock = Cell::new(start + Duration::from_millis(200));
            let result = prepare_fullscreen_press_with(
                || {
                    clock.set(start + elapsed);
                    Ok(true)
                },
                || {
                    deadline
                        .checked_duration_since(clock.get())
                        .filter(|remaining| !remaining.is_zero())
                        .map(|_| ())
                        .ok_or(PlatformError::Timeout)
                },
            );
            assert!(matches!(result, Err(PlatformError::Timeout)));
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectLogical {
        RectLogical::new(PointLogical::new(x, y), SizeLogical::new(w, h))
    }

    /// A Quartz window dictionary as `CGWindowListCopyWindowInfo` returns it: `on_screen` is
    /// `None` for a window that isn't ordered on screen (the key is absent, not false).
    fn window_dictionary(
        layer: i32,
        pid: i64,
        frame: RectLogical,
        on_screen: Option<bool>,
    ) -> CFRetained<CFDictionary<CFString, CFType>> {
        let number = CFNumber::new_i64(42);
        let pid = CFNumber::new_i64(pid);
        let layer = CFNumber::new_i32(layer);
        let bounds = CGRectCreateDictionaryRepresentation(CGRect::new(
            CGPoint::new(frame.origin.x, frame.origin.y),
            CGSize::new(frame.size.width, frame.size.height),
        ));
        // SAFETY: immutable public CoreGraphics dictionary keys.
        let mut keys = unsafe {
            vec![
                kCGWindowNumber,
                kCGWindowOwnerPID,
                kCGWindowLayer,
                kCGWindowBounds,
            ]
        };
        let base: [&CFType; 4] = [&number, &pid, &layer, &bounds];
        let mut values = base.to_vec();
        if let Some(on_screen) = on_screen {
            // SAFETY: immutable public CoreGraphics dictionary key.
            keys.push(unsafe { kCGWindowIsOnscreen });
            values.push(CFBoolean::new(on_screen));
        }
        CFDictionary::<CFString, CFType>::from_slices(&keys, &values)
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn on_screen_is_read_from_the_quartz_dictionary() {
        let frame = rect(10.0, 20.0, 300.0, 200.0);
        let shown = window_dictionary(0, 123, frame, Some(true));
        assert!(parse_window(shown.as_opaque()).unwrap().on_screen);
        // Other Spaces, minimized and hidden windows have no kCGWindowIsOnscreen key.
        let absent = window_dictionary(0, 123, frame, None);
        assert!(!parse_window(absent.as_opaque()).unwrap().on_screen);
        let explicit_false = window_dictionary(0, 123, frame, Some(false));
        assert!(!parse_window(explicit_false.as_opaque()).unwrap().on_screen);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn quartz_facts_map_to_window_state() {
        // The built-in display; a menu bar takes 38 points of a normal window's height.
        let display = rect(0.0, 0.0, 1800.0, 1169.0);
        let state = |layer, pid, frame, on_screen, bounds| {
            let dict = window_dictionary(layer, pid, frame, on_screen);
            let raw = parse_window(dict.as_opaque())?;
            Some(map_window(raw, "io.test.fixture".into(), None, bounds).state)
        };
        // An on-screen window that fills its display is fullscreen.
        assert_eq!(
            state(0, 123, display, Some(true), Some(display)),
            Some(WindowState::Fullscreen)
        );
        // A window that isn't on screen is hidden, whatever its bounds say (Safari's page window
        // while WebKit's fullscreen window is up).
        assert_eq!(
            state(0, 123, display, None, Some(display)),
            Some(WindowState::Hidden)
        );
        assert_eq!(
            state(0, 123, rect(10.0, 20.0, 300.0, 200.0), None, Some(display)),
            Some(WindowState::Hidden)
        );
        // A normal window, and the maximized-under-the-menu-bar case.
        assert_eq!(
            state(
                0,
                123,
                rect(10.0, 20.0, 300.0, 200.0),
                Some(true),
                Some(display)
            ),
            Some(WindowState::Normal)
        );
        assert_eq!(
            state(
                0,
                123,
                rect(0.0, 38.0, 1800.0, 1131.0),
                Some(true),
                Some(display)
            ),
            Some(WindowState::Normal)
        );
        // No display to compare with: not evidence of fullscreen.
        assert_eq!(
            state(0, 123, display, Some(true), None),
            Some(WindowState::Normal)
        );
        // The same size on another display is not fullscreen there.
        assert_eq!(
            state(
                0,
                123,
                rect(1800.0, -1169.0, 1800.0, 1169.0),
                Some(true),
                Some(display)
            ),
            Some(WindowState::Normal)
        );
        // Rounding slack, but no more.
        assert_eq!(
            state(
                0,
                123,
                rect(0.0, 0.0, 1799.0, 1169.0),
                Some(true),
                Some(display)
            ),
            Some(WindowState::Fullscreen)
        );
        assert_eq!(
            state(
                0,
                123,
                rect(0.0, 0.0, 1797.0, 1169.0),
                Some(true),
                Some(display)
            ),
            Some(WindowState::Normal)
        );
        // The existing filters still apply: only layer 0 and not this process.
        assert_eq!(state(25, 123, display, Some(true), Some(display)), None);
        assert_eq!(
            state(
                0,
                i64::from(std::process::id()),
                display,
                Some(true),
                Some(display)
            ),
            None
        );
        assert_eq!(state(0, 0, display, Some(true), Some(display)), None);
        assert!(projectable_app(true, "com.apple.Safari"));
        assert!(!projectable_app(false, "com.apple.Safari"));
        assert!(!projectable_app(true, "com.apple.dock"));
    }

    fn candidate(title: Option<&str>, frame: RectLogical) -> AxCandidate {
        AxCandidate {
            title: title.map(str::to_owned),
            frame,
        }
    }

    fn twin(title: &str, frame: RectLogical, candidates: &[AxCandidate]) -> AxMatch {
        match_ax_window(AxPolicy::Twin, title, frame, candidates)
    }

    fn mirror(title: &str, frame: RectLogical, candidates: &[AxCandidate]) -> AxMatch {
        match_ax_window(AxPolicy::Mirror, title, frame, candidates)
    }

    #[test]
    fn twin_title_is_a_preference_not_a_filter() {
        let (a, b) = (rect(0.0, 0.0, 800.0, 600.0), rect(900.0, 0.0, 800.0, 600.0));
        // The title outranks the frame: Quartz's frame lags right after an AX move.
        assert_eq!(
            twin(
                "Mine",
                a,
                &[candidate(Some("Other"), a), candidate(Some("Mine"), b)]
            ),
            AxMatch::One(1)
        );
        // Title and frame together pick between two windows with the same title.
        assert_eq!(
            twin(
                "Mine",
                b,
                &[candidate(Some("Mine"), a), candidate(Some("Mine"), b)]
            ),
            AxMatch::One(1)
        );
        // No AX window has the title (it changed, or AX reports another): a frame match decides.
        assert_eq!(
            twin(
                "Mine",
                a,
                &[candidate(Some("Other"), b), candidate(Some("Else"), a)]
            ),
            AxMatch::One(1)
        );
        // Equally good matches are ambiguous, never guessed.
        assert_eq!(
            twin(
                "Mine",
                rect(5.0, 5.0, 10.0, 10.0),
                &[candidate(Some("Mine"), a), candidate(Some("Mine"), b)]
            ),
            AxMatch::Ambiguous
        );
        assert_eq!(
            twin("", a, &[candidate(None, a), candidate(None, a)]),
            AxMatch::Ambiguous
        );
        // The frame matches within 2 points and not beyond.
        let near = |dx| rect(dx, 0.0, 800.0, 600.0);
        assert_eq!(
            twin("", a, &[candidate(None, near(2.0)), candidate(None, b)]),
            AxMatch::One(0)
        );
        assert_eq!(
            twin("", a, &[candidate(None, near(3.0)), candidate(None, b)]),
            AxMatch::Missing
        );
    }

    #[test]
    fn twin_accepts_an_untitled_window_only_by_its_frame() {
        let (mine, far) = (
            rect(0.0, 0.0, 800.0, 600.0),
            rect(1800.0, -1406.0, 1710.0, 1406.0),
        );
        // An untitled or differently titled window whose frame matches is the window.
        assert_eq!(
            twin("YouTube", mine, &[candidate(Some(""), mine)]),
            AxMatch::One(0)
        );
        assert_eq!(
            twin("YouTube", mine, &[candidate(None, mine)]),
            AxMatch::One(0)
        );
        assert_eq!(
            twin("YouTube", mine, &[candidate(Some("Other"), mine)]),
            AxMatch::One(0)
        );
        // The failure that ended a projection, and the stand-in write hazard: Quartz has a
        // title, the only AX window is WebKit's title-less fullscreen window at another frame.
        // A lone untitled window with a different frame is another window, not this one.
        assert_eq!(
            twin("YouTube", mine, &[candidate(Some(""), far)]),
            AxMatch::Missing
        );
        assert_eq!(
            twin("YouTube", mine, &[candidate(None, far)]),
            AxMatch::Missing
        );
        assert_eq!(
            twin("", mine, &[candidate(Some("Whatever"), far)]),
            AxMatch::Missing
        );
        assert_eq!(
            twin("YouTube", mine, &[candidate(Some("Other"), far)]),
            AxMatch::Missing
        );
        // No AX window at all: the app's window is on another Space.
        assert_eq!(twin("YouTube", mine, &[]), AxMatch::Missing);
    }

    #[test]
    fn mirror_matching_is_the_previous_policy() {
        let (a, b, far) = (
            rect(0.0, 0.0, 800.0, 600.0),
            rect(900.0, 0.0, 800.0, 600.0),
            rect(1800.0, -1406.0, 1710.0, 1406.0),
        );
        // Regression (review S1): mirror parking must reject an untitled fullscreen stand-in with
        // a different frame, as it did before the twin's matching changed.
        assert_eq!(
            mirror("YouTube", a, &[candidate(Some(""), far)]),
            AxMatch::Missing
        );
        assert_eq!(
            mirror("YouTube", a, &[candidate(None, far)]),
            AxMatch::Missing
        );
        // The title is a filter: a window at the frame with another title is not a candidate...
        assert_eq!(
            mirror("Mine", a, &[candidate(Some("Other"), a)]),
            AxMatch::Missing
        );
        // ...and the frame only picks between candidates with the title.
        assert_eq!(
            mirror(
                "Mine",
                a,
                &[
                    candidate(Some("Other"), a),
                    candidate(Some("Mine"), b),
                    candidate(Some("Mine"), a)
                ]
            ),
            AxMatch::One(2)
        );
        // A lone candidate with the title matches whatever its frame (Quartz lags an AX move).
        assert_eq!(
            mirror("Mine", a, &[candidate(Some("Mine"), far)]),
            AxMatch::One(0)
        );
        // Without a Quartz title every window is a candidate; a lone one matches any frame.
        assert_eq!(
            mirror("", a, &[candidate(Some("Whatever"), far)]),
            AxMatch::One(0)
        );
        assert_eq!(
            mirror("", a, &[candidate(None, b), candidate(None, a)]),
            AxMatch::One(1)
        );
        // Several candidates and no frame match, or several frame matches: nothing is guessed.
        assert_eq!(
            mirror("", a, &[candidate(None, b), candidate(None, far)]),
            AxMatch::Missing
        );
        assert_eq!(
            mirror("", a, &[candidate(None, a), candidate(None, a)]),
            AxMatch::Ambiguous
        );
        assert_eq!(mirror("Mine", a, &[]), AxMatch::Missing);
    }

    #[test]
    fn fullscreen_ax_ranking_prefers_display_bounds_over_a_stale_title() {
        let display = rect(1800.0, -900.0, 1200.0, 900.0);
        let ordinary = rect(20.0, 30.0, 600.0, 400.0);
        for policy in [AxPolicy::Mirror, AxPolicy::Twin] {
            assert_eq!(
                match_fullscreen_ax(
                    policy,
                    "Page",
                    display,
                    &[candidate(Some("Page"), ordinary), candidate(None, display)],
                    true,
                ),
                AxMatch::One(1)
            );
            assert_eq!(
                match_fullscreen_ax(
                    policy,
                    "Page",
                    display,
                    &[candidate(Some("Other"), display)],
                    true,
                ),
                AxMatch::One(0)
            );
        }
    }

    #[test]
    fn titleless_same_pid_at_a_non_display_frame_is_never_a_lone_fallback() {
        let display = rect(1800.0, -900.0, 1200.0, 900.0);
        let ordinary = rect(20.0, 30.0, 600.0, 400.0);
        for policy in [AxPolicy::Mirror, AxPolicy::Twin] {
            for fullscreen in [false, true] {
                assert_eq!(
                    match_fullscreen_ax(
                        policy,
                        "Page",
                        display,
                        &[candidate(None, ordinary)],
                        fullscreen,
                    ),
                    AxMatch::Missing
                );
            }
        }
    }

    #[test]
    fn fullscreen_ax_ranking_keeps_equal_candidates_ambiguous() {
        let display = rect(1800.0, -900.0, 1200.0, 900.0);
        for policy in [AxPolicy::Mirror, AxPolicy::Twin] {
            assert_eq!(
                match_fullscreen_ax(
                    policy,
                    "Page",
                    display,
                    &[candidate(None, display), candidate(Some("Other"), display)],
                    true,
                ),
                AxMatch::Ambiguous
            );
            assert_eq!(
                match_fullscreen_ax(policy, "Page", display, &[], true),
                AxMatch::Missing
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_ensure_is_idempotent_and_presses_once_for_either_transition() {
        use std::cell::Cell;
        use std::collections::VecDeque;
        let dictionary = window_dictionary(0, 123, rect(0.0, 0.0, 800.0, 600.0), Some(true));
        let raw = parse_window(dictionary.as_opaque()).unwrap();
        for desired in [false, true] {
            assert!(
                ensure_fullscreen_with(
                    desired,
                    || Ok((raw.clone(), desired)),
                    |_| panic!("already in the requested state"),
                    || panic!("no wait needed"),
                )
                .is_ok()
            );
            let mut observations = VecDeque::from([!desired, !desired, desired, desired]);
            let presses = Cell::new(0);
            let waits = Cell::new(0);
            assert!(
                ensure_fullscreen_with(
                    desired,
                    || Ok((raw.clone(), observations.pop_front().unwrap())),
                    |_| {
                        presses.set(presses.get() + 1);
                        Ok(true)
                    },
                    || {
                        waits.set(waits.get() + 1);
                        Ok(())
                    },
                )
                .is_ok()
            );
            assert_eq!(presses.get(), 1);
            assert_eq!(waits.get(), if desired { 2 } else { 3 });
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_ensure_refusal_timeout_and_read_errors_are_bounded() {
        use std::cell::Cell;
        let dictionary = window_dictionary(0, 123, rect(0.0, 0.0, 800.0, 600.0), Some(true));
        let raw = parse_window(dictionary.as_opaque()).unwrap();
        let presses = Cell::new(0);
        assert!(matches!(
            ensure_fullscreen_with(
                true,
                || Ok((raw.clone(), false)),
                |_| Ok(false),
                || panic!("no button"),
            ),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            ensure_fullscreen_with(
                true,
                || Ok((raw.clone(), false)),
                |_| {
                    presses.set(presses.get() + 1);
                    Ok(true)
                },
                || Err(PlatformError::Timeout),
            ),
            Err(PlatformError::Timeout)
        ));
        assert_eq!(presses.get(), 1);
        assert!(matches!(
            ensure_fullscreen_with(
                true,
                || Err(PlatformError::NotFound),
                |_| panic!("no window"),
                || panic!("no window"),
            ),
            Err(PlatformError::NotFound)
        ));
        assert!(matches!(
            fullscreen_pause(Instant::now() - Duration::from_secs(1)),
            Err(PlatformError::Timeout)
        ));
        assert_eq!(FULLSCREEN_WAIT, Duration::from_secs(2));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_exit_waits_for_visible_stable_geometry_before_restore() {
        use std::cell::Cell;
        use std::collections::VecDeque;
        let dictionary = window_dictionary(0, 123, rect(0.0, 0.0, 800.0, 600.0), Some(true));
        let fullscreen = parse_window(dictionary.as_opaque()).unwrap();
        let mut hidden = fullscreen.clone();
        hidden.on_screen = false;
        let mut normal = fullscreen.clone();
        normal.frame = rect(10.0, 20.0, 400.0, 300.0);
        let mut settled = normal.clone();
        settled.frame.origin.x += 100.0;
        let mut readings = VecDeque::from([
            (fullscreen, true),
            (hidden, false),
            (normal, false),
            (settled.clone(), false),
            (settled, false),
        ]);
        let waits = Cell::new(0);
        let presses = Cell::new(0);
        assert!(
            ensure_fullscreen_with(
                false,
                || Ok(readings.pop_front().unwrap()),
                |_| {
                    presses.set(presses.get() + 1);
                    Ok(true)
                },
                || {
                    waits.set(waits.get() + 1);
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(presses.get(), 1);
        assert_eq!(waits.get(), 4);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fullscreen_ensure_never_presses_a_hidden_window() {
        let dictionary = window_dictionary(0, 123, rect(0.0, 0.0, 800.0, 600.0), None);
        let hidden = parse_window(dictionary.as_opaque()).unwrap();
        assert!(matches!(
            ensure_fullscreen_with(
                true,
                || Ok((hidden.clone(), false)),
                |_| panic!("an off-Space AX window could be another window"),
                || panic!("unsupported transition"),
            ),
            Err(PlatformError::Unsupported(_))
        ));
    }

    /// Injected dictionaries only: never call own_windows or inspect WindowServer in this test.
    #[test]
    fn own_window_dictionary_filters_pid_visibility_and_preserves_quartz_identity() {
        let number = CFNumber::new_i64(0x123);
        let title = CFString::from_str("accessory proxy");
        // SAFETY: immutable public CoreGraphics dictionary keys.
        let keys = unsafe {
            [
                kCGWindowNumber,
                kCGWindowOwnerPID,
                kCGWindowName,
                kCGWindowIsOnscreen,
            ]
        };
        let pid = 987;
        for (owner, visible, expected) in [
            (pid, true, true),
            (pid + 1, true, false),
            (pid, false, false),
        ] {
            let owner = CFNumber::new_i64(i64::from(owner));
            let dict = CFDictionary::<CFString, CFType>::from_slices(
                &keys,
                &[&number, &owner, &title, CFBoolean::new(visible)],
            );
            assert_eq!(
                parse_own_window(dict.as_opaque(), pid),
                expected.then(|| (WindowId(0x123), "accessory proxy".into()))
            );
        }
        // No app activation policy or bounds metadata is required for own accessory windows.
        let owner = CFNumber::new_i64(i64::from(pid));
        let wrong_number = CFString::from_str("not a Quartz ID");
        let dict = CFDictionary::<CFString, CFType>::from_slices(
            &keys,
            &[&wrong_number, &owner, &title, CFBoolean::new(true)],
        );
        assert!(parse_own_window(dict.as_opaque(), pid).is_none());
        let missing_title =
            CFDictionary::<CFString, CFType>::from_slices(&keys[..2], &[&number, &owner]);
        assert!(parse_own_window(missing_title.as_opaque(), pid).is_none());
        let overflow = CFNumber::new_i64(i64::from(u32::MAX) + 1);
        let dict = CFDictionary::<CFString, CFType>::from_slices(
            &keys,
            &[&overflow, &owner, &title, CFBoolean::new(true)],
        );
        assert!(parse_own_window(dict.as_opaque(), pid).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn cg_dictionary_maps_to_window_info() {
        let number = CFNumber::new_i64(42);
        let pid = CFNumber::new_i32(123);
        let layer = CFNumber::new_i32(0);
        let bounds = CGRectCreateDictionaryRepresentation(CGRect::new(
            CGPoint::new(-100.0, 40.0),
            CGSize::new(400.0, 300.0),
        ));
        let title = CFString::from_str("fixture title");
        let owner = CFString::from_str("Fixture");
        // SAFETY: immutable public CoreGraphics dictionary keys.
        let keys = unsafe {
            [
                kCGWindowNumber,
                kCGWindowOwnerPID,
                kCGWindowLayer,
                kCGWindowBounds,
                kCGWindowName,
                kCGWindowOwnerName,
                kCGWindowIsOnscreen,
            ]
        };
        let dict = CFDictionary::<CFString, CFType>::from_slices(
            &keys,
            &[
                &number,
                &pid,
                &layer,
                &bounds,
                &title,
                &owner,
                CFBoolean::new(true),
            ],
        );
        let raw = parse_window(dict.as_opaque()).unwrap();
        let info = map_window(
            raw,
            "io.test.fixture".into(),
            Some(DisplayId(7)),
            Some(rect(0.0, 0.0, 1800.0, 1169.0)),
        );
        assert_eq!(info.id, WindowId(42));
        assert_eq!(info.pid, Some(123));
        assert_eq!(info.title, "fixture title");
        assert_eq!(info.app_id, "io.test.fixture");
        assert_eq!(info.display, Some(DisplayId(7)));
        assert_eq!(
            info.frame,
            RectLogical::new(
                PointLogical::new(-100.0, 40.0),
                SizeLogical::new(400.0, 300.0)
            )
        );
        assert_eq!(info.state, WindowState::Normal);
        assert_eq!(info.role, WindowRole::Toplevel);
        assert_eq!(info.parent, None);
        let no_title = CFDictionary::<CFString, CFType>::from_slices(
            &keys[..4],
            &[&number, &pid, &layer, &bounds],
        );
        assert!(parse_window(no_title.as_opaque()).unwrap().title.is_empty());
        let popup_layer = CFNumber::new_i32(1);
        let popup = CFDictionary::<CFString, CFType>::from_slices(
            &keys[..4],
            &[&number, &pid, &popup_layer, &bounds],
        );
        assert!(parse_window(popup.as_opaque()).is_none());
        let own_pid = CFNumber::new_i64(i64::from(std::process::id()));
        let own = CFDictionary::<CFString, CFType>::from_slices(
            &keys[..4],
            &[&number, &own_pid, &layer, &bounds],
        );
        assert!(parse_window(own.as_opaque()).is_none());
    }
}
