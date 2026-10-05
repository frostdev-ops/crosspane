//! The tray / menu-bar menu (05 "UI"): what the agent shows and what each item does.
//!
//! [`build`] is pure: the agent gathers a [`TrayView`] snapshot, builds the menu, hands it to the
//! platform's `TrayHost` when it changed, and maps a chosen item back to its [`TrayAction`].

use std::collections::BTreeMap;
use std::net::SocketAddr;

use crosspane_engine::ProjectionKey;
use crosspane_input::arrange::Side;
use crosspane_platform::{Permission, TrayItem, TrayItemId, TrayMenu, TrayState};
use crosspane_protocol::msg::Capability;
use crosspane_types::id::{NodeId, WindowId};

/// A peer's windows as last seen by this node (refreshed in the background).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RemoteWindows {
    /// Not asked yet, or no answer.
    #[default]
    Unknown,
    /// The peer doesn't let this node browse.
    NotAllowed,
    /// Its windows: id and a label.
    List(Vec<(WindowId, String)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerView {
    pub node: NodeId,
    pub name: String,
    pub connected: bool,
    /// Where it sits next to this node, if set explicitly.
    pub side: Option<Side>,
    pub windows: RemoteWindows,
    /// What this node lets it do.
    pub allows_input: bool,
    pub allows_browse: bool,
}

/// The pairing state as the menu needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairingView {
    /// idle, listening, connecting, confirm, pick, waiting, paired, failed.
    pub phase: String,
    pub sas: Option<String>,
    pub candidates: Vec<String>,
    pub peer: Option<String>,
    pub error: Option<String>,
    /// Machines on the network with a pairing window open: name and address.
    pub offers: Vec<(String, SocketAddr)>,
}

/// Everything the menu shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayView {
    pub name: String,
    /// Factual network access status on platforms that can query it.
    pub network_line: Option<String>,
    pub peers: Vec<PeerView>,
    /// This node's windows that can be sent: id and label.
    pub local_windows: Vec<(WindowId, String)>,
    /// Live projections in either direction: key and description.
    pub projections: Vec<(ProjectionKey, String)>,
    /// The peer this node's keyboard and mouse drive now.
    pub controlling: Option<String>,
    /// The peer driving this node now.
    pub controlled_by: Option<String>,
    /// Peers playing sound on this machine's speakers now (04 §5: shared audio is never
    /// invisible), from the engine's `AudioIndicators`.
    pub speakers: Vec<String>,
    /// Crossing disarmed (after a panic or release).
    pub disarmed: bool,
    pub missing_permissions: Vec<Permission>,
    pub pairing: PairingView,
}

/// What a chosen item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Release,
    Panic,
    Rearm,
    Restart,
    Quit,
    Project {
        window: WindowId,
        to: NodeId,
    },
    Pull {
        peer: NodeId,
        window: WindowId,
    },
    Return(ProjectionKey),
    Layout {
        peer: NodeId,
        side: Side,
    },
    Allow {
        peer: NodeId,
        capability: Capability,
        allow: bool,
    },
    PairListen,
    PairJoin(SocketAddr),
    PairConfirm(bool),
    PairPick(usize),
    /// Ask the OS for this one permission: its prompt, or System Settings at its pane when the
    /// prompt was already answered (WP-4.33).
    AskPermission(Permission),
    /// Open the settings app (`crosspane-ui`).
    OpenApp,
}

/// Builds items and remembers which id does what.
#[derive(Default)]
struct Builder {
    next: u32,
    actions: BTreeMap<TrayItemId, TrayAction>,
}

impl Builder {
    fn action(&mut self, label: impl Into<String>, action: TrayAction) -> TrayItem {
        let id = self.id(action);
        TrayItem::Action {
            id,
            label: label.into(),
            enabled: true,
        }
    }

    fn toggle(&mut self, label: impl Into<String>, checked: bool, action: TrayAction) -> TrayItem {
        let id = self.id(action);
        TrayItem::Toggle {
            id,
            label: label.into(),
            checked,
            enabled: true,
        }
    }

    fn id(&mut self, action: TrayAction) -> TrayItemId {
        self.next += 1;
        let id = TrayItemId(self.next);
        self.actions.insert(id, action);
        id
    }
}

fn label(text: impl Into<String>) -> TrayItem {
    TrayItem::Label(text.into())
}

fn submenu(label: impl Into<String>, items: Vec<TrayItem>) -> TrayItem {
    TrayItem::Submenu {
        label: label.into(),
        items,
    }
}

/// Why each macOS permission is needed, in the user's terms (05 scenario 1: "each grant
/// explained").
pub fn permission_reason(permission: Permission) -> &'static str {
    match permission {
        Permission::ScreenRecording => {
            "Screen Recording — to show this Mac's windows on another machine"
        }
        Permission::Accessibility => {
            "Accessibility — to type and click on this Mac from another machine, and to move projected windows"
        }
        Permission::InputMonitoring => {
            "Input Monitoring — to move this Mac's keyboard and mouse to another machine"
        }
        Permission::Microphone => {
            "Microphone — to hear the Crosspane speakers device; your real microphone is never opened"
        }
        _ => "another permission",
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Left => "Left of this machine",
        Side::Right => "Right of this machine",
        Side::Above => "Above this machine",
        Side::Below => "Below this machine",
    }
}

/// The menu for `view`, and what each item does.
pub fn build(view: &TrayView) -> (TrayMenu, BTreeMap<TrayItemId, TrayAction>) {
    let mut b = Builder::default();
    let mut items = Vec::new();
    let connected: Vec<&PeerView> = view.peers.iter().filter(|p| p.connected).collect();

    // Status.
    items.push(label(format!("Crosspane — {}", view.name)));
    if let Some(line) = &view.network_line {
        items.push(label(line.clone()));
    }
    if let Some(peer) = &view.controlling {
        items.push(label(format!("Keyboard and mouse → {peer}")));
        items.push(b.action("Take input back", TrayAction::Release));
    }
    if let Some(peer) = &view.controlled_by {
        items.push(label(format!("Controlled by {peer}")));
    }
    for peer in &view.speakers {
        items.push(label(format!("{peer} is playing sound on these speakers")));
    }
    if view.peers.is_empty() {
        items.push(label("No paired machines"));
    } else if connected.is_empty() {
        items.push(label("No machine connected"));
    } else {
        let names: Vec<&str> = connected.iter().map(|p| p.name.as_str()).collect();
        items.push(label(format!("Connected: {}", names.join(", "))));
    }

    // Permissions (macOS onboarding).
    if !view.missing_permissions.is_empty() {
        items.push(TrayItem::Separator);
        items.push(label("Needs permission (click to allow):"));
        for &permission in &view.missing_permissions {
            items.push(b.action(
                permission_reason(permission),
                TrayAction::AskPermission(permission),
            ));
        }
    }

    // Windows.
    if !connected.is_empty() {
        items.push(TrayItem::Separator);
        for peer in &connected {
            let windows = match &peer.windows {
                RemoteWindows::List(list) if !list.is_empty() => list
                    .iter()
                    .map(|(window, text)| {
                        b.action(
                            text.clone(),
                            TrayAction::Pull {
                                peer: peer.node,
                                window: *window,
                            },
                        )
                    })
                    .collect(),
                RemoteWindows::List(_) => vec![label("No windows")],
                RemoteWindows::NotAllowed => vec![label(format!(
                    "Not allowed: on {}, allow {} to browse",
                    peer.name, view.name
                ))],
                RemoteWindows::Unknown => vec![label("Loading…")],
            };
            items.push(submenu(
                format!("Show a window of {} here", peer.name),
                windows,
            ));
        }
        for peer in &connected {
            let windows = if view.local_windows.is_empty() {
                vec![label("No windows")]
            } else {
                view.local_windows
                    .iter()
                    .map(|(window, text)| {
                        b.action(
                            text.clone(),
                            TrayAction::Project {
                                window: *window,
                                to: peer.node,
                            },
                        )
                    })
                    .collect()
            };
            items.push(submenu(format!("Send a window to {}", peer.name), windows));
        }
    }
    if !view.projections.is_empty() {
        let returns = view
            .projections
            .iter()
            .map(|(key, text)| b.action(format!("Give back: {text}"), TrayAction::Return(*key)))
            .collect();
        items.push(submenu("Projected windows", returns));
    }

    // Machines: layout and permissions per peer, pairing.
    items.push(TrayItem::Separator);
    let mut machines = Vec::new();
    for peer in &view.peers {
        let mut entries = vec![label(if peer.connected {
            "Connected"
        } else {
            "Offline"
        })];
        for side in [Side::Left, Side::Right, Side::Above, Side::Below] {
            entries.push(b.toggle(
                side_label(side),
                peer.side == Some(side),
                TrayAction::Layout {
                    peer: peer.node,
                    side,
                },
            ));
        }
        entries.push(TrayItem::Separator);
        entries.push(b.toggle(
            "May control this machine",
            peer.allows_input,
            TrayAction::Allow {
                peer: peer.node,
                capability: Capability::InputAccept,
                allow: !peer.allows_input,
            },
        ));
        entries.push(b.toggle(
            "May browse and pull my windows",
            peer.allows_browse,
            TrayAction::Allow {
                peer: peer.node,
                capability: Capability::WindowBrowse,
                allow: !peer.allows_browse,
            },
        ));
        machines.push(submenu(peer.name.clone(), entries));
    }
    machines.push(TrayItem::Separator);
    machines.extend(pairing_items(&mut b, &view.pairing));
    items.push(submenu("Machines", machines));

    // Safety and lifecycle.
    items.push(TrayItem::Separator);
    items.push(b.action("Settings…", TrayAction::OpenApp));
    if view.disarmed {
        items.push(b.action("Re-arm edge crossing", TrayAction::Rearm));
    }
    items.push(b.action("Stop everything (panic)", TrayAction::Panic));
    items.push(b.action("Restart Crosspane", TrayAction::Restart));
    items.push(b.action("Quit Crosspane", TrayAction::Quit));

    let state = if view.controlling.is_some()
        || view.controlled_by.is_some()
        || !view.projections.is_empty()
        || !view.speakers.is_empty()
    {
        TrayState::Active
    } else if !view.missing_permissions.is_empty()
        || matches!(view.pairing.phase.as_str(), "confirm" | "pick")
    {
        TrayState::Attention
    } else if connected.is_empty() {
        TrayState::Offline
    } else {
        TrayState::Idle
    };
    let tooltip = match (&view.controlling, &view.controlled_by) {
        (Some(peer), _) => format!("Crosspane: input → {peer}"),
        (_, Some(peer)) => format!("Crosspane: controlled by {peer}"),
        _ if !view.speakers.is_empty() => {
            format!("Crosspane: sound from {}", view.speakers.join(", "))
        }
        _ if connected.is_empty() => "Crosspane: no machine connected".to_owned(),
        _ => format!("Crosspane: {} connected", connected.len()),
    };
    (
        TrayMenu {
            state,
            tooltip,
            items,
        },
        b.actions,
    )
}

fn pairing_items(b: &mut Builder, pairing: &PairingView) -> Vec<TrayItem> {
    match pairing.phase.as_str() {
        "listening" => vec![label("Pairing: waiting for the other machine (120 s)…")],
        "connecting" | "waiting" => {
            let mut items = vec![label("Pairing…")];
            if let Some(sas) = &pairing.sas {
                items.push(label(format!("Code: {}", spaced(sas))));
            }
            items
        }
        "confirm" => vec![
            label(format!(
                "Does the other machine show {}?",
                pairing.sas.as_deref().map(spaced).unwrap_or_default()
            )),
            b.action("Yes, the codes match", TrayAction::PairConfirm(true)),
            b.action("No", TrayAction::PairConfirm(false)),
        ],
        "pick" => {
            let mut items = vec![label("Pick the code the other machine shows:")];
            for (i, code) in pairing.candidates.iter().enumerate() {
                items.push(b.action(spaced(code), TrayAction::PairPick(i)));
            }
            items
        }
        _ => {
            let mut items = Vec::new();
            match pairing.phase.as_str() {
                "paired" => {
                    if let Some(peer) = &pairing.peer {
                        items.push(label(format!("Paired with {peer}")));
                    }
                }
                "failed" => items.push(label(format!(
                    "Pairing failed: {}",
                    pairing.error.as_deref().unwrap_or("unknown error")
                ))),
                _ => {}
            }
            items.push(b.action("Pair a new machine…", TrayAction::PairListen));
            for (name, addr) in &pairing.offers {
                items.push(b.action(format!("Pair with {name}"), TrayAction::PairJoin(*addr)));
            }
            items
        }
    }
}

/// "474007" → "474 007".
fn spaced(code: &str) -> String {
    if code.len() == 6 && code.is_char_boundary(3) {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use crosspane_types::id::ProjectionId;

    use super::*;

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn peer(n: u8, connected: bool) -> PeerView {
        PeerView {
            node: node(n),
            name: format!("peer{n}"),
            connected,
            side: Some(Side::Left),
            windows: RemoteWindows::List(vec![(WindowId(7), "Editor".into())]),
            allows_input: true,
            allows_browse: false,
        }
    }

    fn find(items: &[TrayItem], text: &str) -> Option<TrayItem> {
        for item in items {
            match item {
                TrayItem::Label(l) if l.contains(text) => return Some(item.clone()),
                TrayItem::Action { label, .. } | TrayItem::Toggle { label, .. }
                    if label.contains(text) =>
                {
                    return Some(item.clone());
                }
                TrayItem::Submenu { label, items } => {
                    if label.contains(text) {
                        return Some(item.clone());
                    }
                    if let Some(found) = find(items, text) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn action_of(
        menu: &TrayMenu,
        actions: &BTreeMap<TrayItemId, TrayAction>,
        text: &str,
    ) -> TrayAction {
        match find(&menu.items, text) {
            Some(TrayItem::Action { id, .. } | TrayItem::Toggle { id, .. }) => actions[&id].clone(),
            other => panic!("no choosable item {text:?}: {other:?}"),
        }
    }

    #[test]
    fn idle_with_a_connected_peer_offers_windows_both_ways() {
        let view = TrayView {
            name: "desk".into(),
            peers: vec![peer(1, true)],
            local_windows: vec![(WindowId(3), "Terminal".into())],
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Idle);
        assert!(find(&menu.items, "Connected: peer1").is_some());
        assert_eq!(
            action_of(&menu, &actions, "Editor"),
            TrayAction::Pull {
                peer: node(1),
                window: WindowId(7)
            }
        );
        assert_eq!(
            action_of(&menu, &actions, "Terminal"),
            TrayAction::Project {
                window: WindowId(3),
                to: node(1)
            }
        );
        // Every id maps to an action and ids are unique.
        let mut ids = Vec::new();
        fn collect(items: &[TrayItem], ids: &mut Vec<TrayItemId>) {
            for item in items {
                match item {
                    TrayItem::Action { id, .. } | TrayItem::Toggle { id, .. } => ids.push(*id),
                    TrayItem::Submenu { items, .. } => collect(items, ids),
                    _ => {}
                }
            }
        }
        collect(&menu.items, &mut ids);
        assert_eq!(ids.len(), actions.len());
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), actions.len());
    }

    #[test]
    fn controlling_is_active_and_offers_release() {
        let view = TrayView {
            name: "desk".into(),
            peers: vec![peer(1, true)],
            controlling: Some("peer1".into()),
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Active);
        assert!(menu.tooltip.contains("input → peer1"));
        assert_eq!(
            action_of(&menu, &actions, "Take input back"),
            TrayAction::Release
        );
    }

    #[test]
    fn peers_playing_on_the_speakers_are_listed_and_make_the_icon_active() {
        let mut view = TrayView {
            name: "desk".into(),
            peers: vec![peer(1, true)],
            ..TrayView::default()
        };
        let (menu, _) = build(&view);
        assert_eq!(menu.state, TrayState::Idle);
        assert!(find(&menu.items, "playing sound").is_none());

        view.speakers = vec!["peer1".into(), "laptop".into()];
        let (menu, _) = build(&view);
        assert_eq!(menu.state, TrayState::Active);
        assert!(find(&menu.items, "peer1 is playing sound on these speakers").is_some());
        assert!(find(&menu.items, "laptop is playing sound on these speakers").is_some());
        assert!(menu.tooltip.contains("peer1, laptop"));

        // Control still names itself in the tooltip first.
        view.controlling = Some("peer1".into());
        let (menu, _) = build(&view);
        assert!(menu.tooltip.contains("input → peer1"));
    }

    #[test]
    fn missing_permissions_are_explained_and_asked_one_at_a_time() {
        let view = TrayView {
            name: "mac".into(),
            peers: vec![peer(1, true)],
            missing_permissions: vec![Permission::Accessibility],
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Attention);
        assert_eq!(
            action_of(&menu, &actions, "Accessibility —"),
            TrayAction::AskPermission(Permission::Accessibility)
        );
    }

    #[test]
    fn browse_not_allowed_explains_what_to_do() {
        let mut p = peer(1, true);
        p.windows = RemoteWindows::NotAllowed;
        let view = TrayView {
            name: "desk".into(),
            peers: vec![p],
            ..TrayView::default()
        };
        let (menu, _) = build(&view);
        assert!(find(&menu.items, "on peer1, allow desk to browse").is_some());
    }

    #[test]
    fn layout_and_grants_toggle() {
        let view = TrayView {
            name: "desk".into(),
            peers: vec![peer(1, false)],
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Offline);
        match find(&menu.items, "Left of this machine") {
            Some(TrayItem::Toggle { checked, id, .. }) => {
                assert!(checked);
                assert_eq!(
                    actions[&id],
                    TrayAction::Layout {
                        peer: node(1),
                        side: Side::Left
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            action_of(&menu, &actions, "May browse"),
            TrayAction::Allow {
                peer: node(1),
                capability: Capability::WindowBrowse,
                allow: true
            }
        );
    }

    #[test]
    fn pairing_flows() {
        let mut view = TrayView {
            name: "desk".into(),
            pairing: PairingView {
                phase: "idle".into(),
                offers: vec![("mac".into(), "192.168.4.244:47811".parse().unwrap())],
                ..PairingView::default()
            },
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(
            action_of(&menu, &actions, "Pair a new machine"),
            TrayAction::PairListen
        );
        assert_eq!(
            action_of(&menu, &actions, "Pair with mac"),
            TrayAction::PairJoin("192.168.4.244:47811".parse().unwrap())
        );

        view.pairing = PairingView {
            phase: "pick".into(),
            candidates: vec!["111111".into(), "474007".into(), "222222".into()],
            ..PairingView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Attention);
        assert_eq!(
            action_of(&menu, &actions, "474 007"),
            TrayAction::PairPick(1)
        );

        view.pairing = PairingView {
            phase: "confirm".into(),
            sas: Some("474007".into()),
            ..PairingView::default()
        };
        let (menu, actions) = build(&view);
        assert!(find(&menu.items, "show 474 007?").is_some());
        assert_eq!(
            action_of(&menu, &actions, "Yes"),
            TrayAction::PairConfirm(true)
        );
    }

    #[test]
    fn projections_can_be_given_back() {
        let key = ProjectionKey {
            source: node(9),
            projection: ProjectionId(4),
        };
        let view = TrayView {
            name: "desk".into(),
            peers: vec![peer(1, true)],
            projections: vec![(key, "showing window 4 of peer9".into())],
            ..TrayView::default()
        };
        let (menu, actions) = build(&view);
        assert_eq!(menu.state, TrayState::Active);
        assert_eq!(
            action_of(&menu, &actions, "Give back"),
            TrayAction::Return(key)
        );
    }
}
