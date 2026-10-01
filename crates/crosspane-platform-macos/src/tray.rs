// Menu-bar status items. Native objects stay in the main thread's registry; only the command
// handle crosses threads.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crosspane_platform::tray::{TrayEvent, TrayHost, TrayItem, TrayItemId, TrayMenu, TrayState};
use crosspane_platform::{EventSink, PlatformError};
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, ns_string};

use crate::main_thread::spawn_on_main;

struct Shared {
    alive: AtomicBool,
    sink: Mutex<Option<Arc<dyn EventSink<TrayEvent>>>>,
}

/// A Send command handle for a menu-bar item owned by the AppKit main thread.
pub struct MacTray {
    key: u64,
    shared: Arc<Shared>,
}

impl fmt::Debug for MacTray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacTray")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl MacTray {
    /// Never touches AppKit itself; the status item is created on the main thread at the first
    /// `set`. Works only once the AppKit run loop is running on the main thread.
    pub fn new() -> MacTray {
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            shared: Arc::new(Shared {
                alive: AtomicBool::new(true),
                sink: Mutex::new(None),
            }),
        }
    }
}

impl Default for MacTray {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayHost for MacTray {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<TrayEvent>>) -> Result<(), PlatformError> {
        let mut slot = self.shared.sink.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(PlatformError::Backend(
                "TrayHost::subscribe called twice".into(),
            ));
        }
        *slot = Some(sink);
        Ok(())
    }

    fn set(&mut self, menu: &TrayMenu) -> Result<(), PlatformError> {
        let menu = menu.clone();
        let key = self.key;
        let shared = self.shared.clone();
        spawn_on_main(move |mtm| {
            if !shared.alive.load(Ordering::Acquire) {
                return;
            }
            HOSTS.with(|hosts| {
                let mut hosts = hosts.borrow_mut();
                let host = hosts.entry(key).or_insert_with(|| Host {
                    item: NSStatusBar::systemStatusBar()
                        .statusItemWithLength(NSVariableStatusItemLength),
                    target: Target::new(shared, mtm),
                });
                host.item
                    .setMenu(Some(&build_menu(&menu.items, &host.target, mtm)));
                if let Some(button) = host.item.button(mtm) {
                    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &NSString::from_str(symbol(menu.state)),
                        None,
                    );
                    if let Some(image) = &image {
                        image.setTemplate(true);
                    }
                    button.setImage(image.as_deref());
                    button.setTitle(if image.is_some() {
                        ns_string!("")
                    } else {
                        ns_string!("⧉")
                    });
                    button.setToolTip(Some(&NSString::from_str(&menu.tooltip)));
                }
            });
        });
        Ok(())
    }
}

impl Drop for MacTray {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        let key = self.key;
        spawn_on_main(move |_| {
            let host = HOSTS.with(|hosts| hosts.borrow_mut().remove(&key));
            if let Some(host) = host {
                NSStatusBar::systemStatusBar().removeStatusItem(&host.item);
            }
        });
    }
}

thread_local! {
    static HOSTS: RefCell<BTreeMap<u64, Host>> = const { RefCell::new(BTreeMap::new()) };
}

struct Host {
    item: Retained<NSStatusItem>,
    // NSMenuItem targets are weak. Keep one target alive across menu replacements.
    target: Retained<Target>,
}

define_class!(
    // SAFETY: NSObject has no additional subclassing requirements; no custom Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "CrosspaneTrayTarget"]
    #[ivars = Arc<Shared>]
    struct Target;

    // SAFETY: NSObjectProtocol adds no requirements.
    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        // SAFETY: Matches AppKit's void action signature with an NSMenuItem sender.
        #[unsafe(method(chosen:))]
        fn chosen(&self, sender: &NSMenuItem) {
            if !sender.isEnabled() || !self.ivars().alive.load(Ordering::Acquire) {
                return;
            }
            if let Ok(id) = u32::try_from(sender.tag()) {
                let sink = self.ivars().sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(sink) = sink {
                    sink.send(TrayEvent::Chosen(TrayItemId(id)));
                }
            }
        }
    }
);

impl Target {
    fn new(shared: Arc<Shared>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(shared);
        // SAFETY: NSObject's init takes no arguments; the Rust ivars are initialized.
        unsafe { msg_send![super(this), init] }
    }
}

fn build_menu(items: &[TrayItem], target: &Target, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for item in items {
        let native = match item {
            TrayItem::Separator => NSMenuItem::separatorItem(mtm),
            _ => {
                let (label, enabled, id) = match item {
                    TrayItem::Label(label) => (label, false, None),
                    TrayItem::Action { id, label, enabled }
                    | TrayItem::Toggle {
                        id, label, enabled, ..
                    } => (label, *enabled, Some(*id)),
                    TrayItem::Submenu { label, items } => (label, !items.is_empty(), None),
                    TrayItem::Separator => unreachable!(),
                };
                // SAFETY: chosen: is implemented by Target with the NSMenuItem action signature.
                // Non-choosable items have no action. All construction runs on the main thread.
                let native = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(label),
                        id.map(|_| sel!(chosen:)),
                        ns_string!(""),
                    )
                };
                native.setEnabled(enabled);
                if let Some(id) = id {
                    native.setTag(id.0 as isize);
                }
                match item {
                    TrayItem::Toggle { checked, .. } => native.setState(if *checked {
                        NSControlStateValueOn
                    } else {
                        NSControlStateValueOff
                    }),
                    TrayItem::Submenu { items, .. } => {
                        native.setSubmenu(Some(&build_menu(items, target, mtm)));
                    }
                    _ => {}
                }
                native
            }
        };
        // SAFETY: Target implements the only action used here and is retained by Host until
        // removal. Every item, including non-choosable ones, uses this target on main.
        unsafe { native.setTarget(Some(target)) };
        menu.addItem(&native);
    }
    menu
}

fn symbol(state: TrayState) -> &'static str {
    match state {
        TrayState::Idle => "rectangle.on.rectangle",
        TrayState::Active => "rectangle.fill.on.rectangle.fill",
        TrayState::Attention => "exclamationmark.triangle",
        TrayState::Offline => "rectangle.on.rectangle.slash",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn symbols_match_all_states() {
        use super::*;

        for (state, expected) in [
            (TrayState::Idle, "rectangle.on.rectangle"),
            (TrayState::Active, "rectangle.fill.on.rectangle.fill"),
            (TrayState::Attention, "exclamationmark.triangle"),
            (TrayState::Offline, "rectangle.on.rectangle.slash"),
        ] {
            assert_eq!(symbol(state), expected);
        }
    }
}
