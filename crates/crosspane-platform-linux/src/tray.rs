//! StatusNotifierItem tray, with the menu and choices owned by the agent.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use crosspane_platform::tray::{TrayEvent, TrayHost, TrayItem, TrayItemId, TrayMenu, TrayState};
use crosspane_platform::{EventSink, PlatformError};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, Status, ToolTip};
use zbus::blocking::{Connection, connection::Builder};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const BUS_TIMEOUT: Duration = Duration::from_millis(100);

/// A session-bus tray whose registration and updates run on a backend thread.
///
/// No item is exported until the first successful `set`. Once registered, ksni
/// keeps the service alive and re-registers it when the watcher restarts.
pub struct SniTray {
    connection: Connection,
    menus: mpsc::Sender<TrayMenu>,
    service: Arc<Mutex<ServiceState>>,
}

impl fmt::Debug for SniTray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SniTray")
            .field("bus_name", &self.connection.unique_name())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct ServiceState {
    sink: Option<Arc<dyn EventSink<TrayEvent>>>,
    handle: Option<Handle<TrayModel>>,
    failure: Option<PlatformError>,
    stopped: bool,
}

fn lock(service: &Mutex<ServiceState>) -> MutexGuard<'_, ServiceState> {
    match service.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl SniTray {
    /// Connect to the session bus. Returns `Unsupported` when it is unavailable.
    /// The item itself is registered only on the first `set`.
    pub fn new() -> Result<SniTray, PlatformError> {
        let connection = Builder::session()
            .map_err(|_| PlatformError::Unsupported("no session bus"))?
            .method_timeout(BUS_TIMEOUT)
            .build()
            .map_err(|_| PlatformError::Unsupported("no session bus"))?;
        let service = Arc::new(Mutex::new(ServiceState::default()));
        let worker_service = service.clone();
        let (menus, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("crosspane-tray".into())
            .spawn(move || run(receiver, worker_service))
            .map_err(|error| PlatformError::Backend(format!("tray worker: {error}")))?;
        Ok(Self {
            connection,
            menus,
            service,
        })
    }
}

impl TrayHost for SniTray {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<TrayEvent>>) -> Result<(), PlatformError> {
        let mut service = lock(&self.service);
        if service.sink.is_some() {
            return Err(PlatformError::Backend("tray already subscribed".into()));
        }
        service.sink = Some(sink);
        Ok(())
    }

    fn set(&mut self, menu: &TrayMenu) -> Result<(), PlatformError> {
        // Query only the bus daemon, never the bar. This also avoids D-Bus
        // activation of an absent watcher. Each call has a short timeout.
        let reply = self
            .connection
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(WATCHER,),
            )
            .map_err(|error| PlatformError::Backend(format!("tray bus: {error}")))?;
        let present: bool = reply
            .body()
            .deserialize()
            .map_err(|error| PlatformError::Backend(format!("tray bus reply: {error}")))?;
        if !present {
            return Err(PlatformError::Unsupported("no tray host"));
        }
        self.menus
            .send(menu.clone())
            .map_err(|_| PlatformError::Backend("tray worker unavailable".into()))?;
        // Registration itself is asynchronous. A failure observed by the
        // worker is reported on the next set; that set also queues a retry.
        match lock(&self.service).failure.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for SniTray {
    fn drop(&mut self) {
        let handle = {
            let mut service = lock(&self.service);
            service.stopped = true;
            service.sink = None;
            service.handle.take()
        };
        if let Some(handle) = handle {
            // Enqueue shutdown without waiting for a bar or an in-flight update.
            let _ = handle.shutdown();
        }
        // Dropping the sender wakes the idle worker. If registration is still
        // in flight, the worker shuts its new handle down as soon as it returns.
    }
}

fn run(menus: mpsc::Receiver<TrayMenu>, service: Arc<Mutex<ServiceState>>) {
    while let Ok(menu) = menus.recv() {
        let handle = {
            let state = lock(&service);
            if state.stopped {
                break;
            }
            state.handle.clone()
        };
        if let Some(handle) = handle
            && !handle.is_closed()
            && handle.update(|tray| tray.menu = menu.clone()).is_some()
        {
            continue;
        }
        let result = TrayModel {
            menu,
            service: service.clone(),
        }
        .spawn();
        let mut state = lock(&service);
        match result {
            Ok(handle) => {
                if state.stopped {
                    let _ = handle.shutdown();
                    break;
                }
                state.handle = Some(handle);
                state.failure = None;
            }
            Err(error) => {
                state.handle = None;
                state.failure = Some(match error {
                    ksni::Error::Watcher(zbus::fdo::Error::ServiceUnknown(_))
                    | ksni::Error::WontShow => PlatformError::Unsupported("no tray host"),
                    _ => PlatformError::Backend(format!("tray registration: {error}")),
                });
            }
        }
    }
}

struct TrayModel {
    menu: TrayMenu,
    service: Arc<Mutex<ServiceState>>,
}

impl TrayModel {
    fn chosen(&self, id: TrayItemId) {
        let sink = lock(&self.service).sink.clone();
        if let Some(sink) = sink {
            // ksni serializes all menu callbacks on its service lock. Do not
            // hold our state lock while calling the nonblocking event sink.
            sink.send(TrayEvent::Chosen(id));
        }
    }
}

impl ksni::Tray for TrayModel {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "crosspane".into()
    }

    fn title(&self) -> String {
        "Crosspane".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        status(self.menu.state)
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![icon(self.menu.state)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Crosspane".into(),
            description: self.menu.tooltip.clone(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        convert(&self.menu.items)
    }
}

fn convert(items: &[TrayItem]) -> Vec<MenuItem<TrayModel>> {
    items
        .iter()
        .map(|item| match item {
            TrayItem::Label(label) => StandardItem {
                label: label.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            TrayItem::Action { id, label, enabled } => {
                let (id, enabled) = (*id, *enabled);
                StandardItem {
                    label: label.clone(),
                    enabled,
                    activate: Box::new(move |tray: &mut TrayModel| {
                        // ksni's D-Bus Event method does not check `enabled`.
                        if enabled {
                            tray.chosen(id);
                        }
                    }),
                    ..Default::default()
                }
                .into()
            }
            TrayItem::Toggle {
                id,
                label,
                checked,
                enabled,
            } => {
                let (id, enabled) = (*id, *enabled);
                CheckmarkItem {
                    label: label.clone(),
                    checked: *checked,
                    enabled,
                    activate: Box::new(move |tray: &mut TrayModel| {
                        if enabled {
                            tray.chosen(id);
                        }
                    }),
                    ..Default::default()
                }
                .into()
            }
            TrayItem::Separator => MenuItem::Separator,
            TrayItem::Submenu { label, items } => SubMenu {
                label: label.clone(),
                enabled: !items.is_empty(),
                submenu: convert(items),
                ..Default::default()
            }
            .into(),
        })
        .collect()
}

fn status(state: TrayState) -> Status {
    match state {
        TrayState::Offline => Status::Passive,
        TrayState::Idle | TrayState::Active | TrayState::Attention => Status::Active,
    }
}

fn icon(state: TrayState) -> Icon {
    // SNI requires network-order ARGB bytes, independent of the CPU endianness.
    let argb = match state {
        TrayState::Idle => [0xff, 0xd1, 0xd5, 0xdb],
        TrayState::Active => [0xff, 0x3b, 0x82, 0xf6],
        TrayState::Attention => [0xff, 0xf5, 0x9e, 0x0b],
        TrayState::Offline => [0x80, 0x4b, 0x55, 0x63],
    };
    let mut data = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let front = rounded(x, y, 10, 10, 20, 4);
            let front_border = front && !rounded(x, y, 12, 12, 16, 2);
            let back_border = rounded(x, y, 3, 3, 20, 4) && !rounded(x, y, 5, 5, 16, 2);
            data.extend_from_slice(if front_border || (back_border && !front) {
                &argb
            } else {
                &[0; 4]
            });
        }
    }
    Icon {
        width: 32,
        height: 32,
        data,
    }
}

fn rounded(x: i32, y: i32, left: i32, top: i32, size: i32, radius: i32) -> bool {
    let (x, y) = (x - left, y - top);
    if x < 0 || y < 0 || x >= size || y >= size {
        return false;
    }
    let dx = x - x.clamp(radius, size - radius - 1);
    let dy = y - y.clamp(radius, size - radius - 1);
    dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ksni::Tray;

    #[test]
    fn menu_conversion_preserves_tree_flags_checkmarks_and_ids() {
        let (send, events) = mpsc::channel();
        let service = Arc::new(Mutex::new(ServiceState {
            sink: Some(Arc::new(move |event| send.send(event).unwrap())),
            ..Default::default()
        }));
        let original = TrayMenu {
            items: vec![
                TrayItem::Label("Status".into()),
                TrayItem::Action {
                    id: TrayItemId(u32::MAX),
                    label: "Connect".into(),
                    enabled: true,
                },
                TrayItem::Action {
                    id: TrayItemId(2),
                    label: "Unavailable".into(),
                    enabled: false,
                },
                TrayItem::Toggle {
                    id: TrayItemId(3),
                    label: "Checked".into(),
                    checked: true,
                    enabled: true,
                },
                TrayItem::Separator,
                TrayItem::Submenu {
                    label: "Peers".into(),
                    items: vec![TrayItem::Submenu {
                        label: "Nested".into(),
                        items: vec![
                            TrayItem::Toggle {
                                id: TrayItemId(4),
                                label: "Unchecked".into(),
                                checked: false,
                                enabled: false,
                            },
                            TrayItem::Action {
                                id: TrayItemId(5),
                                label: "Nested action".into(),
                                enabled: true,
                            },
                        ],
                    }],
                },
                TrayItem::Submenu {
                    label: "Empty".into(),
                    items: vec![],
                },
            ],
            ..Default::default()
        };
        let mut tray = TrayModel {
            menu: original.clone(),
            service,
        };
        let converted = tray.menu();
        assert_eq!(converted.len(), original.items.len());
        let MenuItem::Standard(label) = &converted[0] else {
            panic!("label must be a standard item");
        };
        assert_eq!(label.label, "Status");
        assert!(!label.enabled);
        (label.activate)(&mut tray);
        let MenuItem::Standard(action) = &converted[1] else {
            panic!("action must be a standard item");
        };
        assert_eq!(action.label, "Connect");
        assert!(action.enabled);
        (action.activate)(&mut tray);
        let MenuItem::Standard(disabled) = &converted[2] else {
            panic!("disabled action must be a standard item");
        };
        assert_eq!(disabled.label, "Unavailable");
        assert!(!disabled.enabled);
        // Even a direct D-Bus click on a disabled item must not reach the sink.
        (disabled.activate)(&mut tray);
        let MenuItem::Checkmark(toggle) = &converted[3] else {
            panic!("toggle must be a checkmark item");
        };
        assert_eq!(toggle.label, "Checked");
        assert!(toggle.checked && toggle.enabled);
        (toggle.activate)(&mut tray);
        assert_eq!(
            tray.menu, original,
            "a toggle must not change its check mark"
        );
        assert!(matches!(converted[4], MenuItem::Separator));
        let MenuItem::SubMenu(submenu) = &converted[5] else {
            panic!("submenu must be preserved");
        };
        assert_eq!(submenu.label, "Peers");
        assert!(submenu.enabled);
        assert_eq!(submenu.submenu.len(), 1);
        let MenuItem::SubMenu(nested) = &submenu.submenu[0] else {
            panic!("nested submenu must be preserved");
        };
        assert_eq!(nested.label, "Nested");
        assert!(nested.enabled);
        assert_eq!(nested.submenu.len(), 2);
        let MenuItem::Checkmark(unchecked) = &nested.submenu[0] else {
            panic!("nested toggle must be preserved");
        };
        assert_eq!(unchecked.label, "Unchecked");
        assert!(!unchecked.checked && !unchecked.enabled);
        (unchecked.activate)(&mut tray);
        let MenuItem::Standard(nested_action) = &nested.submenu[1] else {
            panic!("nested action must be preserved");
        };
        assert_eq!(nested_action.label, "Nested action");
        assert!(nested_action.enabled);
        (nested_action.activate)(&mut tray);
        let MenuItem::SubMenu(empty) = &converted[6] else {
            panic!("empty submenu must be preserved");
        };
        assert_eq!(empty.label, "Empty");
        assert!(!empty.enabled);
        assert!(empty.submenu.is_empty());

        for id in [u32::MAX, 3, 5] {
            assert_eq!(
                events.try_recv().unwrap(),
                TrayEvent::Chosen(TrayItemId(id))
            );
        }
        assert!(events.try_recv().is_err());

        tray.menu = TrayMenu::default();
        assert!(tray.menu().is_empty(), "set replaces the whole menu");
        // An already-dispatched callback captures the id from its old menu.
        (action.activate)(&mut tray);
        assert_eq!(
            events.try_recv().unwrap(),
            TrayEvent::Chosen(TrayItemId(u32::MAX))
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn icon_states_have_32_pixel_panes_and_network_order_argb() {
        let states = [
            (TrayState::Idle, [0xff, 0xd1, 0xd5, 0xdb]),
            (TrayState::Active, [0xff, 0x3b, 0x82, 0xf6]),
            (TrayState::Attention, [0xff, 0xf5, 0x9e, 0x0b]),
            (TrayState::Offline, [0x80, 0x4b, 0x55, 0x63]),
        ];
        let mut images = Vec::new();
        for (state, argb) in states {
            let pixmap = icon(state);
            assert_eq!((pixmap.width, pixmap.height), (32, 32));
            assert_eq!(pixmap.data.len(), 32 * 32 * 4);
            let pixel = |x: usize, y: usize| &pixmap.data[(y * 32 + x) * 4..(y * 32 + x + 1) * 4];
            assert_eq!(pixel(3, 10), argb, "back pane, ARGB byte order");
            assert_eq!(pixel(10, 18), argb, "front pane, ARGB byte order");
            for (x, y) in [(0, 0), (3, 3), (10, 10), (15, 15), (31, 31)] {
                assert_eq!(pixel(x, y), [0; 4], "rounded corners and clear interiors");
            }
            assert_eq!(pixel(22, 18), [0; 4], "front pane hides the back border");
            assert!(
                pixmap
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p == &argb || p == &[0; 4])
            );
            assert!(images.iter().all(|previous| previous != &pixmap.data));
            images.push(pixmap.data);
        }
    }

    #[test]
    fn status_mapping_and_sni_metadata() {
        for (state, expected) in [
            (TrayState::Idle, Status::Active),
            (TrayState::Active, Status::Active),
            (TrayState::Attention, Status::Active),
            (TrayState::Offline, Status::Passive),
        ] {
            let tray = TrayModel {
                menu: TrayMenu {
                    state,
                    tooltip: "First line\nSecond line".into(),
                    items: vec![],
                },
                service: Arc::new(Mutex::new(ServiceState::default())),
            };
            assert_eq!(tray.status(), expected);
            assert_eq!(tray.id(), "crosspane");
            assert_eq!(tray.title(), "Crosspane");
            assert_eq!(tray.category(), Category::ApplicationStatus);
            assert_eq!(tray.tool_tip().description, tray.menu.tooltip);
            assert_eq!(tray.icon_pixmap().len(), 1);
        }
    }
}
