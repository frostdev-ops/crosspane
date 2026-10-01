//! The tray / menu-bar icon (05 "UI", WP-1.34): a status icon with a menu the agent builds.
//!
//! The agent owns the menu: it rebuilds a [`TrayMenu`] whenever its state changes and hands the
//! whole tree to [`TrayHost::set`]. Backends only show it and report which item was chosen. There
//! is no policy here: no item does anything by itself.

use std::sync::Arc;

use crate::{EventSink, PlatformError};

/// Identifies a menu item the user can choose. The agent assigns ids; they are only meaningful
/// for the menu they came with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrayItemId(pub u32);

/// The icon's state, shown by its look (e.g. colour or badge): at a glance the user sees whether
/// input or windows are crossing machines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TrayState {
    /// Running, nothing crossing.
    #[default]
    Idle,
    /// This node's keyboard and mouse drive another node, or windows are projected (04 §5:
    /// the user must be able to see it).
    Active,
    /// Something needs the user: a missing permission, a pairing waiting for confirmation.
    Attention,
    /// Disconnected from every peer.
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayItem {
    /// A line of text that can't be chosen (status, headings).
    Label(String),
    /// A choosable item. A disabled one is shown greyed out and never reported.
    Action {
        id: TrayItemId,
        label: String,
        enabled: bool,
    },
    /// A choosable item with a check mark.
    Toggle {
        id: TrayItemId,
        label: String,
        checked: bool,
        enabled: bool,
    },
    Separator,
    /// A nested menu. An empty one is shown disabled.
    Submenu {
        label: String,
        items: Vec<TrayItem>,
    },
}

/// The whole tray: icon state, tooltip and menu.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayMenu {
    pub state: TrayState,
    /// One or two short lines, shown on hover where the platform supports it.
    pub tooltip: String,
    pub items: Vec<TrayItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// The user chose this item of the current menu.
    Chosen(TrayItemId),
}

/// Shows the status icon and its menu (NSStatusItem on macOS, StatusNotifierItem on Linux).
///
/// The icon appears on the first `set` and stays until the host is dropped. Every `set` replaces
/// the previous menu completely; an item chosen from a menu that has since been replaced may
/// still be reported once with its old id, and the agent ignores ids it doesn't know.
pub trait TrayHost: Send {
    /// Start delivering events. Called once.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<TrayEvent>>) -> Result<(), PlatformError>;
    /// Show `menu`, replacing the previous one. Must not block for long: backends hand the tree
    /// to their own thread or the main thread and return.
    fn set(&mut self, menu: &TrayMenu) -> Result<(), PlatformError>;
}
