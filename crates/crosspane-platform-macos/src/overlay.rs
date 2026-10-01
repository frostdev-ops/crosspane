//! Non-activating HUD panels. Only the command handle crosses threads; native objects stay in
//! the main thread's registry. Showing our own panels requires no TCC permission.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use crosspane_platform::{
    EventSink, Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, PlatformError,
};
use crosspane_types::id::DisplayId;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSBox, NSBoxType,
    NSColor, NSFont, NSLineBreakMode, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField,
    NSTitlePosition, NSView, NSWindowCollectionBehavior,
    NSWindowDidChangeOcclusionStateNotification, NSWindowOcclusionState, NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSNumber, NSObjectProtocol,
    NSOperationQueue, NSPoint, NSRect, NSSize, NSString, ns_string,
};

use crate::main_thread::{on_main, spawn_on_main};

const CALL_TIMEOUT: Duration = Duration::from_millis(50);
const MARGIN: f64 = 24.0;

define_class!(
    // SAFETY: NSPanel has no additional subclassing requirements. No ivars or custom Drop;
    // the overrides have the exact AppKit BOOL-returning, no-argument signatures.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrosspaneOverlayPanel"]
    #[derive(Debug)]
    struct OverlayPanel;

    impl OverlayPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool { false }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool { false }
    }
);

struct Shared {
    alive: AtomicBool,
    // Serialises notification events with failures reported by command threads.
    sink: Mutex<Option<Arc<dyn EventSink<OverlayEvent>>>>,
}

impl Shared {
    fn emit(&self, event: OverlayEvent) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if self.alive.load(Ordering::Acquire)
            && let Some(sink) = &*sink
        {
            sink.send(event);
        }
    }
}

/// A Send command handle for panels owned by the AppKit main thread.
pub struct MacOverlay {
    key: u64,
    shared: Arc<Shared>,
}

impl fmt::Debug for MacOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacOverlay")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl MacOverlay {
    pub fn new() -> Result<Self, PlatformError> {
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        Ok(Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            shared: Arc::new(Shared {
                alive: AtomicBool::new(true),
                sink: Mutex::new(None),
            }),
        })
    }

    fn command(&self, id: OverlayId, overlay: Option<Overlay>) -> Result<(), PlatformError> {
        let key = self.key;
        let shared = self.shared.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        let deadline = Instant::now() + CALL_TIMEOUT;
        let result = on_main(CALL_TIMEOUT, move |mtm| {
            if task_cancelled.load(Ordering::Acquire)
                || Instant::now() >= deadline
                || !shared.alive.load(Ordering::Acquire)
            {
                return Err(PlatformError::Timeout);
            }
            let host = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned());
            let result = if let Some(overlay) = overlay {
                if screen(overlay.display, mtm).is_none() {
                    if let Some(host) = host {
                        host.remove(id);
                    }
                    return Err(PlatformError::NotFound);
                }
                let host = match host {
                    Some(host) => host,
                    None => {
                        let host = Rc::new(Host::new(key, shared.clone()));
                        HOSTS.with(|hosts| hosts.borrow_mut().insert(key, host.clone()));
                        host
                    }
                };
                // Accept promptly; cold AppKit text/layout work can exceed the command budget.
                // Presentation is acknowledged only by Visible after the render task completes.
                drop(host);
                let render_cancelled = task_cancelled.clone();
                spawn_on_main(move |mtm| {
                    if render_cancelled.load(Ordering::Acquire)
                        || !shared.alive.load(Ordering::Acquire)
                    {
                        return;
                    }
                    if let Some(host) = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned()) {
                        let result = host.show(id, &overlay, mtm);
                        if result.is_err() || render_cancelled.load(Ordering::Acquire) {
                            host.remove(id);
                            shared.emit(OverlayEvent::Unavailable(id));
                        }
                    }
                });
                Ok(())
            } else {
                if let Some(host) = host {
                    host.remove(id);
                }
                Ok(())
            };
            if task_cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                task_cancelled.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            } else {
                result
            }
        })
        .and_then(|result| result);
        if result.is_err() {
            cancelled.store(true, Ordering::Release);
            self.shared.emit(OverlayEvent::Unavailable(id));
        }
        result
    }
}

impl OverlayHost for MacOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        let mut slot = self.shared.sink.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(PlatformError::Backend(
                "OverlayHost::subscribe called twice".into(),
            ));
        }
        *slot = Some(sink);
        drop(slot);
        let key = self.key;
        on_main(CALL_TIMEOUT, move |mtm| {
            if let Some(host) = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned()) {
                for entry in host.entries() {
                    entry.visible.set(false);
                }
                host.check(mtm);
            }
        })
    }

    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        self.command(id, Some(overlay.clone()))
    }

    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        self.command(id, None)
    }
}

impl Drop for MacOverlay {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        let key = self.key;
        spawn_on_main(move |_| {
            // Drop outside the registry borrow: ordering panels out can deliver notifications.
            let host = HOSTS.with(|hosts| hosts.borrow_mut().remove(&key));
            drop(host);
        });
    }
}

thread_local! {
    static HOSTS: RefCell<BTreeMap<u64, Rc<Host>>> = const { RefCell::new(BTreeMap::new()) };
}

struct Host {
    shared: Arc<Shared>,
    panels: RefCell<BTreeMap<OverlayId, Rc<Panel>>>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Host {
    fn new(key: u64, shared: Arc<Shared>) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let queue = NSOperationQueue::mainQueue();
        // SAFETY: immutable notification-name constants exported by AppKit.
        let names = unsafe {
            [
                NSWindowDidChangeOcclusionStateNotification,
                NSApplicationDidChangeScreenParametersNotification,
            ]
        };
        let observers = names
            .into_iter()
            .map(|name: &NSNotificationName| {
                let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                    let _ = on_main(CALL_TIMEOUT, move |mtm| {
                        if let Some(host) = HOSTS.with(|hosts| hosts.borrow().get(&key).cloned()) {
                            host.check(mtm);
                        }
                    });
                });
                // SAFETY: block captures only a Send integer, runs on the main queue, and ignores
                // the notification pointer. The observer is retained and removed in Host::drop.
                unsafe {
                    center.addObserverForName_object_queue_usingBlock(
                        Some(name),
                        None,
                        Some(&queue),
                        &block,
                    )
                }
            })
            .collect();
        Self {
            shared,
            panels: RefCell::new(BTreeMap::new()),
            observers,
        }
    }

    fn entries(&self) -> Vec<Rc<Panel>> {
        self.panels.borrow().values().cloned().collect()
    }

    fn remove(&self, id: OverlayId) {
        let panel = self.panels.borrow_mut().remove(&id);
        if let Some(panel) = panel {
            panel.window.orderOut(None);
        }
    }

    fn check(&self, mtm: MainThreadMarker) {
        for entry in self.entries() {
            if screen(entry.display.get(), mtm).is_none() {
                self.remove(entry.id);
                self.shared.emit(OverlayEvent::Unavailable(entry.id));
            } else {
                entry.check(&self.shared);
            }
        }
    }

    fn show(
        &self,
        id: OverlayId,
        overlay: &Overlay,
        mtm: MainThreadMarker,
    ) -> Result<(), PlatformError> {
        let screen = screen(overlay.display, mtm).ok_or(PlatformError::NotFound)?;
        let panel = self.panels.borrow().get(&id).cloned();
        let panel = match panel {
            Some(panel) => panel,
            None => {
                let panel = Rc::new(Panel::new(id, overlay.display, mtm));
                self.panels.borrow_mut().insert(id, panel.clone());
                panel
            }
        };
        let content = content(overlay, mtm)?;
        // setContentView resizes its view to the panel's old frame (initially zero).
        let size = content.frame().size;
        panel.display.set(overlay.display);
        // Suppress checks during reconfiguration, then request a fresh Visible acknowledgement.
        panel.updating.set(true);
        panel.window.setContentView(Some(&content));
        panel.window.setFrame_display(
            anchor_frame(screen.visibleFrame(), size, overlay.anchor),
            true,
        );
        panel.window.orderFrontRegardless();
        panel.updating.set(false);
        panel.visible.set(false);
        panel.check(&self.shared);
        Ok(())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        for observer in &self.observers {
            // SAFETY: these tokens came from this center's block-based registration.
            unsafe { center.removeObserver((**observer).as_ref()) };
        }
        for panel in self.panels.get_mut().values() {
            panel.window.orderOut(None);
        }
    }
}

struct Panel {
    id: OverlayId,
    window: Retained<NSPanel>,
    display: Cell<DisplayId>,
    visible: Cell<bool>,
    updating: Cell<bool>,
}

impl Panel {
    fn new(id: OverlayId, display: DisplayId, mtm: MainThreadMarker) -> Self {
        // SAFETY: standard NSPanel initializer on the main thread, using a subclass with no
        // ivars. Its inherited initializer returns a retained NSPanel; auto-release on close
        // is disabled immediately below, as required for Rust-owned windows.
        let window: Retained<OverlayPanel> = unsafe {
            msg_send![
                OverlayPanel::alloc(mtm),
                initWithContentRect: NSRect::ZERO,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        let window = window.into_super();
        // SAFETY: Rust's Retained owns the panel, so closing must not release it independently.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setIgnoresMouseEvents(true);
        window.setLevel(NSStatusWindowLevel);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(true);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHidesOnDeactivate(false);
        Self {
            id,
            window,
            display: Cell::new(display),
            visible: Cell::new(false),
            updating: Cell::new(false),
        }
    }

    fn check(&self, shared: &Shared) {
        if self.updating.get() {
            return;
        }
        let visible = self.window.isVisible()
            && self
                .window
                .occlusionState()
                .contains(NSWindowOcclusionState::Visible);
        let previous = self.visible.replace(visible);
        if visible && !previous {
            shared.emit(OverlayEvent::Visible(self.id));
        } else if !visible && previous {
            shared.emit(OverlayEvent::Unavailable(self.id));
        }
    }
}

fn screen(display: DisplayId, mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    NSScreen::screens(mtm).into_iter().find(|screen| {
        screen
            .deviceDescription()
            .objectForKey(ns_string!("NSScreenNumber"))
            .and_then(|number| number.downcast::<NSNumber>().ok())
            .is_some_and(|number| number.unsignedIntValue() == display.0)
    })
}

fn anchor_frame(visible: NSRect, size: NSSize, anchor: OverlayAnchor) -> NSRect {
    let x = match anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::Center => {
            visible.origin.x + (visible.size.width - size.width) / 2.0
        }
        OverlayAnchor::TopRight | OverlayAnchor::BottomRight => {
            visible.origin.x + visible.size.width - size.width - MARGIN
        }
    };
    let y = match anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::TopRight => {
            visible.origin.y + visible.size.height - size.height - MARGIN
        }
        OverlayAnchor::BottomRight => visible.origin.y + MARGIN,
        OverlayAnchor::Center => visible.origin.y + (visible.size.height - size.height) / 2.0,
    };
    NSRect::new(NSPoint::new(x, y), size)
}

fn content(overlay: &Overlay, mtm: MainThreadMarker) -> Result<Retained<NSView>, PlatformError> {
    let label = NSTextField::labelWithString(&NSString::from_str(&overlay.text), mtm);
    label.setEditable(false);
    label.setSelectable(false);
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::systemFontOfSize(14.0)));
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.setMaximumNumberOfLines(1);
    label.sizeToFit();
    let label_size = label.frame().size;
    let size = NSSize::new(label_size.width + 40.0, label_size.height + 20.0);
    let view = NSBox::initWithFrame(NSBox::alloc(mtm), NSRect::new(NSPoint::ZERO, size));
    view.setBoxType(NSBoxType::Custom);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setBorderWidth(0.0);
    view.setCornerRadius(10.0);
    view.setContentViewMargins(NSSize::ZERO);
    view.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        31.0 / 255.0,
        41.0 / 255.0,
        55.0 / 255.0,
        0.9,
    ));
    view.setWantsLayer(true);
    if view.layer().is_none() {
        return Err(PlatformError::Backend(
            "overlay backing layer unavailable".into(),
        ));
    }
    let bar = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(8.0, 8.0), NSSize::new(4.0, size.height - 16.0)),
    );
    bar.setBoxType(NSBoxType::Custom);
    bar.setTitlePosition(NSTitlePosition::NoTitle);
    bar.setBorderWidth(0.0);
    bar.setCornerRadius(0.0);
    bar.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(overlay.accent.r) / 255.0,
        f64::from(overlay.accent.g) / 255.0,
        f64::from(overlay.accent.b) / 255.0,
        1.0,
    ));
    bar.setWantsLayer(true);
    label.setFrame(NSRect::new(NSPoint::new(24.0, 10.0), label_size));
    view.addSubview(&bar);
    view.addSubview(&label);
    Ok(view.into_super())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn anchors_use_visible_frame_and_24_point_margin() {
        for origin in [NSPoint::ZERO, NSPoint::new(-1200.0, 200.0)] {
            let visible = NSRect::new(origin, NSSize::new(1200.0, 800.0));
            let size = NSSize::new(200.0, 40.0);
            for (anchor, x, y) in [
                (OverlayAnchor::TopCenter, 500.0, 736.0),
                (OverlayAnchor::TopRight, 976.0, 736.0),
                (OverlayAnchor::BottomRight, 976.0, 24.0),
                (OverlayAnchor::Center, 500.0, 380.0),
            ] {
                assert_eq!(
                    anchor_frame(visible, size, anchor),
                    NSRect::new(NSPoint::new(origin.x + x, origin.y + y), size)
                );
            }
        }
    }
}
