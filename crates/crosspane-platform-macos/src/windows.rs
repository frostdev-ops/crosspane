//! Quartz window snapshots, main-thread AppKit metadata, and public Accessibility operations.

use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
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
    CGError, CGGetDisplaysWithPoint, CGRectMakeWithDictionaryRepresentation,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowBounds,
    kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};

use crate::{main_thread::on_main, permissions};

const POLL: Duration = Duration::from_millis(250);
const QUERY_WAIT: Duration = Duration::from_millis(500);
const MAIN_WAIT: Duration = Duration::from_millis(250);

/// A Send handle; no AppKit objects cross the main-thread boundary.
#[derive(Debug)]
pub struct MacWindows {
    query: WindowQuery,
    stop: Option<Arc<AtomicBool>>,
}

impl MacWindows {
    pub fn new() -> Result<MacWindows, PlatformError> {
        Ok(Self {
            query: WindowQuery::new()?,
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
        Ok(snapshot(&self.query)?.0)
    }

    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        Ok(snapshot(&self.query)?.1)
    }

    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        require_accessibility()?;
        let raw = self
            .query
            .list(false)?
            .into_iter()
            .find(|w| w.id == window)
            .ok_or(PlatformError::NotFound)?;
        let deadline = Instant::now() + MAIN_WAIT;
        let pid = raw.pid;
        on_main(MAIN_WAIT, move |_| {
            // on_main may execute a timed-out closure later. Never activate in that case.
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            require_accessibility()?;
            autoreleasepool(|_| {
                let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                    .ok_or(PlatformError::NotFound)?;
                if app.activateWithOptions(NSApplicationActivationOptions::empty()) {
                    Ok(())
                } else {
                    Err(PlatformError::Backend(
                        "application rejected activation".into(),
                    ))
                }
            })
        })??;
        AxWindow::find(&raw, Instant::now() + Duration::from_secs(1))?.raise()
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        if self.stop.is_some() {
            return Err(PlatformError::Backend(
                "WindowSource::subscribe called twice".into(),
            ));
        }
        let initial = snapshot(&self.query)?;
        let query = self.query.clone();
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
                    let (next, next_focus) = match snapshot(&query) {
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
                    let previous: HashMap<_, _> = last.iter().map(|w| (w.id, w)).collect();
                    for window in &next {
                        match previous.get(&window.id) {
                            None => sink.send(WindowEvent::Added(window.clone())),
                            Some(old) if **old != *window => {
                                sink.send(WindowEvent::Changed(window.clone()))
                            }
                            Some(_) => {}
                        }
                    }
                    let next_ids: HashSet<_> = next.iter().map(|w| w.id).collect();
                    for window in &last {
                        if !next_ids.contains(&window.id) {
                            sink.send(WindowEvent::Removed(window.id));
                        }
                    }
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

#[derive(Clone, Debug)]
pub(crate) struct RawWindow {
    pub(crate) id: WindowId,
    pub(crate) pid: i32,
    pub(crate) title: String,
    owner: String,
    pub(crate) frame: RectLogical,
}

type QueryReply = mpsc::SyncSender<Result<Vec<RawWindow>, PlatformError>>;

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
    Some(RawWindow {
        id: WindowId(u64::from(id)),
        pid,
        title: string(name),
        owner: string(owner),
        frame,
    })
}

fn map_window(raw: RawWindow, app_id: String, display: Option<DisplayId>) -> WindowInfo {
    WindowInfo {
        id: raw.id,
        title: raw.title,
        app_id,
        pid: u32::try_from(raw.pid).ok(),
        display,
        frame: raw.frame,
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn snapshot(query: &WindowQuery) -> Result<(Vec<WindowInfo>, Option<WindowId>), PlatformError> {
    let raw = query.list(false)?;
    on_main(MAIN_WAIT, move |_| {
        autoreleasepool(|_| {
            let front = NSWorkspace::sharedWorkspace()
                .frontmostApplication()
                .map(|app| app.processIdentifier());
            let windows: Vec<_> = raw
                .into_iter()
                .filter_map(|raw| {
                    let app =
                        NSRunningApplication::runningApplicationWithProcessIdentifier(raw.pid)?;
                    if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                        return None;
                    }
                    let app_id = app
                        .bundleIdentifier()
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| raw.owner.clone());
                    if app_id == "com.apple.dock" {
                        return None;
                    }
                    let display = display_for_frame(raw.frame).ok();
                    Some(map_window(raw, app_id, display))
                })
                .collect();
            let focused = windows
                .iter()
                .find(|w| w.pid.and_then(|p| i32::try_from(p).ok()) == front)
                .map(|w| w.id);
            (windows, focused)
        })
    })
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
            .ok_or(PlatformError::Timeout)?;
        // SAFETY: valid retained AX object; positive timeout bounds its synchronous messages.
        ax_result(unsafe {
            self.element
                .set_messaging_timeout(remaining.as_secs_f32().max(f32::MIN_POSITIVE))
        })
    }

    fn attribute(&self, name: &str) -> Result<CFRetained<CFType>, PlatformError> {
        self.prepare()?;
        let mut value = std::ptr::null();
        // SAFETY: valid retained element/name and writable output pointer. Copy returns +1 ownership.
        ax_result(unsafe {
            self.element
                .copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
        })?;
        let value = NonNull::new(value.cast_mut())
            .ok_or_else(|| PlatformError::Backend("empty AX attribute".into()))?;
        // SAFETY: successful Copy supplied a non-null +1 CF object, now owned by this handle.
        Ok(unsafe { CFRetained::from_raw(value) })
    }

    fn set(&self, name: &str, value: &CFType) -> Result<(), PlatformError> {
        self.prepare()?;
        // SAFETY: element and typed CF value are alive; name is a public AX attribute.
        ax_result(unsafe {
            self.element
                .set_attribute_value(&CFString::from_str(name), value)
        })
    }

    pub(crate) fn find(raw: &RawWindow, deadline: Instant) -> Result<Self, PlatformError> {
        require_accessibility()?;
        let app = Self {
            // SAFETY: positive pid obtained from Quartz; creates a retained public AX application object.
            element: unsafe { AXUIElement::new_application(raw.pid) },
            deadline,
        };
        let values = app
            .attribute("AXWindows")?
            .downcast::<CFArray>()
            .map_err(|_| PlatformError::Backend("AXWindows is not an array".into()))?;
        // SAFETY: the public AXWindows attribute is an array of AXUIElement CF objects;
        // each element is additionally downcast before use.
        let values = unsafe { values.cast_unchecked::<CFType>() };
        let mut found = None;
        // What each AX window looked like, for the error when none matches.
        let mut seen = Vec::new();
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
            seen.push(format!(
                "({:.0},{:.0} {:.0}x{:.0})",
                frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
            ));
            let matches = (frame.origin.x - raw.frame.origin.x).abs() <= 2.0
                && (frame.origin.y - raw.frame.origin.y).abs() <= 2.0
                && (frame.size.width - raw.frame.size.width).abs() <= 2.0
                && (frame.size.height - raw.frame.size.height).abs() <= 2.0;
            if !matches {
                continue;
            }
            if !raw.title.is_empty() {
                let title = window
                    .attribute("AXTitle")?
                    .downcast::<CFString>()
                    .map_err(|_| PlatformError::Backend("AXTitle is not a string".into()))?;
                if title.to_string() != raw.title {
                    seen.push(format!("title {:?}", title.to_string()));
                    continue;
                }
            }
            if found.is_some() {
                return Err(PlatformError::Backend("ambiguous AX window match".into()));
            }
            found = Some(window);
        }
        found.ok_or_else(|| {
            PlatformError::Backend(format!(
                "Quartz window ({:.0},{:.0} {:.0}x{:.0}) has no matching AX window among {} [{}]",
                raw.frame.origin.x,
                raw.frame.origin.y,
                raw.frame.size.width,
                raw.frame.size.height,
                values.len(),
                seen.join(" "),
            ))
        })
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
        self.resize(frame.size)?;
        let mut position = CGPoint::new(frame.origin.x, frame.origin.y);
        // SAFETY: public CGPoint AXValue type and valid initialized CGPoint storage; AX copies it.
        let value =
            unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut position).cast()) }
                .ok_or_else(|| PlatformError::Backend("create AX position".into()))?;
        self.set("AXPosition", &value)
    }

    fn raise(&self) -> Result<(), PlatformError> {
        self.prepare()?;
        // SAFETY: valid retained AX window; AXRaise is a public action name.
        ax_result(unsafe { self.element.perform_action(&CFString::from_str("AXRaise")) })?;
        self.set("AXMain", CFBoolean::new(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::CGRectCreateDictionaryRepresentation;

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
            ]
        };
        let dict = CFDictionary::<CFString, CFType>::from_slices(
            &keys,
            &[&number, &pid, &layer, &bounds, &title, &owner],
        );
        let raw = parse_window(dict.as_opaque()).unwrap();
        let info = map_window(raw, "io.test.fixture".into(), Some(DisplayId(7)));
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
