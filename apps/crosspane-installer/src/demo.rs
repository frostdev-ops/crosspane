//! Permanently disconnected fixtures. These values are illustrations, never installation proof.
//!
//! Each fixture mirrors what the live installer shows on that screen, so a review screenshot
//! looks like the real thing. A few named variants show other states of the same screen.

use crate::view::*;

pub const DEMO_LABEL: &str = "Demo — no changes made";
pub const HIDE_LABEL: &str = "Hide projected windows on a virtual display (recommended)";
pub const MIRROR_LABEL: &str = "Mirror instead (windows stay visible on this Mac)";
pub const MICROPHONE_COPY: &str =
    "lets Crosspane hear the Crosspane speakers device; your real microphone is never opened";
pub const MICROPHONE_DETAIL: &str =
    "This lets Crosspane hear the Crosspane speakers device; your real microphone is never opened.";
pub const REMOVE_AUDIO_LABEL: &str =
    "Remove the Crosspane audio driver (affects every user on this Mac)";

pub const SCREENS: [(ScreenId, &str); 15] = [
    (ScreenId::Welcome, "welcome"),
    (ScreenId::Compatibility, "compatibility"),
    (ScreenId::InstallPlan, "install-plan"),
    (ScreenId::Installing, "installing"),
    (ScreenId::Permissions, "permissions"),
    (ScreenId::AudioComponent, "audio-component"),
    (ScreenId::Network, "network"),
    (ScreenId::HidingChoice, "hiding-choice"),
    (ScreenId::Connect, "connect"),
    (ScreenId::MatchNumbers, "match-numbers"),
    (ScreenId::Grants, "grants"),
    (ScreenId::Layout, "layout"),
    (ScreenId::Practice, "practice"),
    (ScreenId::Summary, "summary"),
    (ScreenId::RepairRemove, "repair-remove"),
];

/// Other states of a screen, for review: the name and the screen it belongs to.
pub const VARIANTS: [(&str, ScreenId); 8] = [
    ("install-failed", ScreenId::InstallPlan),
    ("install-prerequisites", ScreenId::InstallPlan),
    ("connect-searching", ScreenId::Connect),
    ("connect-waiting", ScreenId::Connect),
    ("connect-address", ScreenId::Connect),
    ("match-pick", ScreenId::MatchNumbers),
    ("practice-choose", ScreenId::Practice),
    ("summary-waiting", ScreenId::Summary),
];

/// The screen a review name shows: one of [`SCREENS`] or a [`VARIANTS`] state of one.
pub fn screen_named(name: &str) -> Option<ScreenId> {
    let name = motion_variant(name).0;
    SCREENS
        .iter()
        .find(|(_, key)| *key == name)
        .map(|(id, _)| *id)
        .or_else(|| {
            VARIANTS
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, id)| *id)
        })
}

/// The fixture a review name shows.
pub fn fixture_named(name: &str) -> Option<WizardView> {
    let (name, motion) = motion_variant(name);
    let screen = screen_named(name)?;
    let mut view = fixture(screen);
    match name {
        "install-prerequisites" => {
            let checks = [
                "Operating system",
                "Processor architecture",
                "Hyprland version",
                "Login session",
                "Session manager",
                "Manager environment",
                "Wayland protocols",
                "Required libraries",
                "Video support",
                "Sound support",
                "System keyring",
            ];
            view.rows.retain(|row| !row.is_check());
            for (index, label) in checks.iter().enumerate().rev() {
                view.rows
                    .insert(1, row(check_row_id(index), label, "", RowState::Verified));
            }
        }
        "install-failed" => {
            view.title = "Setup stopped".into();
            view.message = "Fix the step marked below, then try again.".into();
            view.rows = install_rows(&[
                RowState::Verified,
                RowState::Failed,
                RowState::Unchecked,
                RowState::Unchecked,
                RowState::Unchecked,
            ]);
            if let Some(payload) = view.rows.iter_mut().find(|row| row.id == 20) {
                payload.detail = "There isn't enough free space in your home folder. Free up \
                                  about 200 MB, then try again."
                    .into();
            }
            view.buttons = vec![
                button(2, ButtonRole::Back, "Back", ButtonKind::Link),
                button(120, ButtonRole::Retry, "Try again", ButtonKind::Primary),
            ];
        }
        "connect-searching" => {
            view.rows = vec![row(
                950,
                "Looking for the other computer…",
                "Open Crosspane setup on the other computer too. The two find each other on your \
                 network.",
                RowState::Working,
            )];
            view.buttons = vec![
                button(2, ButtonRole::Back, "Back", ButtonKind::Link),
                button(
                    2005,
                    ButtonRole::Ordinary,
                    "Enter its address",
                    ButtonKind::Link,
                ),
                button(
                    2001,
                    ButtonRole::Ordinary,
                    "Let the other computer find this one",
                    ButtonKind::Link,
                ),
            ];
        }
        "connect-waiting" => {
            view.rows = vec![row(
                950,
                "Ready to be found",
                "On the other computer, choose this one when Crosspane setup finds it. Then \
                 compare the number on both screens.",
                RowState::Working,
            )];
            view.buttons = vec![ButtonView {
                enabled: false,
                ..button(2, ButtonRole::Back, "Back", ButtonKind::Link)
            }];
            view.link_caption = None;
        }
        "connect-address" => {
            view.message =
                "Enter the other computer's address. The port is 47811 unless it was changed."
                    .into();
            view.rows.clear();
            view.fields.push(FieldView::PeerAddress {
                id: 10,
                value: String::new(),
                enabled: true,
            });
            view.link_caption = None;
            view.buttons = vec![
                button(2, ButtonRole::Back, "Back", ButtonKind::Link),
                ButtonView {
                    enabled: false,
                    ..button(2002, ButtonRole::Ordinary, "Join", ButtonKind::Primary)
                },
                ButtonView {
                    enabled: false,
                    ..button(
                        2003,
                        ButtonRole::Ordinary,
                        "Reconnect a computer paired before",
                        ButtonKind::Link,
                    )
                },
                button(
                    2005,
                    ButtonRole::Ordinary,
                    "Search automatically instead",
                    ButtonKind::Link,
                ),
            ];
        }
        "match-pick" => {
            view.title = "Which number do you see on the other computer?".into();
            view.message = "Pick the number shown on mac-studio.".into();
            view.illustration.sas = None;
            view.buttons = ["482 913", "730 155", "096 284"]
                .iter()
                .enumerate()
                .map(|(i, number)| {
                    button(
                        2110 + i as u16,
                        ButtonRole::Confirm,
                        number,
                        ButtonKind::Choice,
                    )
                })
                .collect();
        }
        "practice-choose" => {
            view.message =
                "Start the same practice on both computers. A small window checks that it worked."
                    .into();
            view.illustration.practice = None;
            view.rows = vec![
                RowView {
                    human_confirmed: true,
                    ..row(
                        70,
                        "Control the other computer from this keyboard and mouse",
                        "",
                        RowState::Verified,
                    )
                },
                RowView {
                    human_confirmed: true,
                    ..row(
                        71,
                        "Let the other computer control this one",
                        "",
                        RowState::Verified,
                    )
                },
            ];
            view.buttons = [
                (3002, "Send a window from this computer"),
                (3003, "Receive a window sent from the other computer"),
                (3004, "Let the other computer take a window from here"),
                (3005, "Take a window from the other computer"),
                (3006, "Play sound from this computer on the other one"),
                (3007, "Hear the other computer's sound here"),
                (3008, "Find the Crosspane menu and settings"),
            ]
            .iter()
            .map(|(id, label)| button(*id, ButtonRole::Ordinary, label, ButtonKind::Choice))
            .chain([
                button(1, ButtonRole::Next, "Finish later", ButtonKind::Link),
                button(2, ButtonRole::Back, "Back", ButtonKind::Link),
            ])
            .collect();
        }
        "summary-waiting" => {
            view.title = "Crosspane is installed".into();
            view.message = "A few steps are left.".into();
            view.summary = SummaryView::InstalledWaiting;
            view.progress.completed = vec![
                ProgressGroup::Install,
                ProgressGroup::PermissionsNetwork,
                ProgressGroup::Connect,
                ProgressGroup::Arrange,
            ];
            for practice in view.rows.iter_mut().skip(5) {
                practice.state = RowState::Unchecked;
                practice.human_confirmed = false;
            }
            view.buttons = vec![
                button(6, ButtonRole::Next, "Finish setup", ButtonKind::Primary),
                ButtonView {
                    enabled: false,
                    ..button(4001, ButtonRole::Retry, "Check again", ButtonKind::Link)
                },
                button(
                    4,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    ButtonKind::Link,
                ),
                button(3, ButtonRole::Cancel, "Close", ButtonKind::Link),
            ];
        }
        _ => {}
    }
    if let Some(motion) = motion {
        view.motion = motion;
    }
    Some(view)
}

// Review variants exercise the same screen without changing its data or any production mode.
fn motion_variant(name: &str) -> (&str, Option<MotionPreference>) {
    for (suffix, motion) in [
        ("-reduced", MotionPreference::Reduced),
        ("-off", MotionPreference::Off),
    ] {
        if let Some(name) = name.strip_suffix(suffix) {
            return (name, Some(motion));
        }
    }
    (name, None)
}

fn row(id: u16, label: &str, detail: &str, state: RowState) -> RowView {
    RowView {
        id,
        label: label.into(),
        detail: detail.into(),
        state,
        human_confirmed: false,
    }
}

fn button(id: u16, role: ButtonRole, label: &str, kind: ButtonKind) -> ButtonView {
    ButtonView {
        id,
        role,
        label: label.into(),
        enabled: true,
        kind,
    }
}

/// The install page's steps, in the Linux order, in the given states.
fn install_rows(states: &[RowState; 5]) -> Vec<RowView> {
    let steps = [
        (10, "Check this computer"),
        (20, "Install Crosspane"),
        (21, "Start Crosspane when you sign in"),
        (22, "Restart Crosspane"),
        (23, "Check that Crosspane is running"),
    ];
    let mut rows = Vec::new();
    for ((id, label), state) in steps.iter().zip(states) {
        rows.push(row(*id, label, "", *state));
        if *id == 10 {
            let checks = [
                ("Operating system", "Arch-based"),
                ("Hyprland session", "0.56.2, managed by uwsm"),
                ("Graphics and video", "NVIDIA, hardware encoding"),
                ("Required libraries", ""),
            ];
            for (index, (check, value)) in checks.iter().enumerate() {
                let state = if *state == RowState::Working && index == checks.len() - 1 {
                    RowState::Working
                } else if *state == RowState::Unchecked {
                    RowState::Unchecked
                } else {
                    RowState::Verified
                };
                rows.push(row(check_row_id(index), check, value, state));
            }
        }
    }
    rows
}

fn progress(screen: ScreenId) -> ProgressView {
    let order = [
        ProgressGroup::Install,
        ProgressGroup::PermissionsNetwork,
        ProgressGroup::Connect,
        ProgressGroup::Arrange,
        ProgressGroup::Practice,
        ProgressGroup::Ready,
    ];
    let current = match screen {
        ScreenId::Welcome
        | ScreenId::Compatibility
        | ScreenId::InstallPlan
        | ScreenId::Installing
        | ScreenId::RepairRemove => ProgressGroup::Install,
        ScreenId::Permissions
        | ScreenId::AudioComponent
        | ScreenId::Network
        | ScreenId::HidingChoice => ProgressGroup::PermissionsNetwork,
        ScreenId::Connect | ScreenId::MatchNumbers => ProgressGroup::Connect,
        ScreenId::Grants | ScreenId::Layout => ProgressGroup::Arrange,
        ScreenId::Practice => ProgressGroup::Practice,
        ScreenId::Summary => ProgressGroup::Ready,
    };
    let at = order.iter().position(|g| *g == current).unwrap_or(0);
    ProgressView {
        current: Some(current),
        completed: if screen == ScreenId::Summary {
            order.to_vec()
        } else {
            order[..at].to_vec()
        },
    }
}

pub fn fixture(screen: ScreenId) -> WizardView {
    let mut view = WizardView {
        revision: 1,
        escape: EscapeMapping::Back,
        fields: Vec::new(),
        screen,
        title: String::new(),
        message: String::new(),
        machine: Some("omarchy".into()),
        peer: None,
        rows: Vec::new(),
        buttons: vec![button(2, ButtonRole::Back, "Back", ButtonKind::Link)],
        summary: SummaryView::NotInstalled,
        hiding_choice: None,
        motion: MotionPreference::Auto,
        system_reduced_motion: None,
        layout: None,
        progress: progress(screen),
        illustration: IllustrationView::default(),
        demo: true,
        link_caption: None,
    };
    let install_message = "This carries on by itself.";
    match screen {
        ScreenId::Welcome => {
            view.escape = EscapeMapping::Close;
            view.title = "Set up Crosspane".into();
            view.message = "Use one keyboard and mouse across two computers, move windows between \
                            them and share sound. Setup asks only when it needs you."
                .into();
            view.buttons = vec![
                button(1, ButtonRole::Next, "Start setup", ButtonKind::Primary),
                button(
                    4,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    ButtonKind::Link,
                ),
            ];
        }
        ScreenId::Compatibility => {
            view.title = "Installing Crosspane".into();
            view.message = install_message.into();
            view.rows = install_rows(&[
                RowState::Working,
                RowState::Unchecked,
                RowState::Unchecked,
                RowState::Unchecked,
                RowState::Unchecked,
            ]);
            view.rows[0].detail = "Checking now…".into();
            if let Some(payload) = view.rows.iter_mut().find(|row| row.id == 20) {
                payload.detail = "Crosspane's files need to be installed for your account.".into();
            }
        }
        ScreenId::InstallPlan => {
            view.title = "Installing Crosspane".into();
            view.message = install_message.into();
            view.rows = install_rows(&[
                RowState::Verified,
                RowState::Working,
                RowState::Unchecked,
                RowState::Unchecked,
                RowState::Unchecked,
            ]);
            if let Some(payload) = view.rows.iter_mut().find(|row| row.id == 20) {
                payload.detail = "Installing Crosspane 0.0.1 for your account only: programs in \
                                  ~/.local/bin, a startup entry and menu entries. Nothing outside \
                                  your home folder changes."
                    .into();
            }
        }
        ScreenId::Installing => {
            view.title = "Installing Crosspane".into();
            view.message = install_message.into();
            view.rows = install_rows(&[
                RowState::Verified,
                RowState::Verified,
                RowState::Verified,
                RowState::Verified,
                RowState::Waiting,
            ]);
            if let Some(agent) = view.rows.iter_mut().find(|row| row.id == 23) {
                agent.detail =
                    "Waiting for your system keyring. Unlock it when asked; setup carries on."
                        .into();
            }
            view.buttons.push(button(
                123,
                ButtonRole::Retry,
                "Check again",
                ButtonKind::Link,
            ));
        }
        ScreenId::Permissions => {
            view.title = "Allow Crosspane on this Mac".into();
            view.message = "Allow each permission below. macOS shows a request, or opens System \
                            Settings where you turn Crosspane on. Setup notices each one by itself \
                            and moves on once all are allowed."
                .into();
            view.rows = vec![
                row(
                    30,
                    "Crosspane has the Mac permissions it needs",
                    "1 of 4 allowed. Allow each one below.",
                    RowState::NeedsAction,
                ),
                row(
                    960,
                    "Device Control and Data Access",
                    "Answer the macOS request, or turn Crosspane on in System Settings. Setup \
                     notices it by itself.",
                    RowState::Waiting,
                ),
                row(
                    961,
                    "Input Monitoring",
                    "Allow Device Control and Data Access first.",
                    RowState::Waiting,
                ),
                row(
                    962,
                    "Screen & System Audio Recording",
                    "Allowed.",
                    RowState::Verified,
                ),
                row(
                    963,
                    "Microphone",
                    "macOS still reports this off for Crosspane. Restart Crosspane so it reads \
                     it again, or reset Crosspane's entry and ask again.",
                    RowState::Waiting,
                ),
            ];
            view.buttons.extend([
                button(2600, ButtonRole::Confirm, "Allow", ButtonKind::Choice),
                button(2601, ButtonRole::Confirm, "It's on", ButtonKind::Link),
                ButtonView {
                    enabled: false,
                    ..button(2610, ButtonRole::Confirm, "Allow", ButtonKind::Choice)
                },
                button(2630, ButtonRole::Confirm, "Allow", ButtonKind::Choice),
                button(
                    2633,
                    ButtonRole::Confirm,
                    "Restart Crosspane and check again",
                    ButtonKind::Link,
                ),
                button(
                    2632,
                    ButtonRole::Confirm,
                    "Reset Crosspane's entry and ask again",
                    ButtonKind::Link,
                ),
            ]);
        }
        ScreenId::AudioComponent => {
            view.title = "Bring sound across".into();
            view.message = "Adds the speakers the other computer plays to. Installing asks for an \
                            administrator password, restarts this Mac's sound for a moment and \
                            affects every user on this Mac."
                .into();
            view.rows = vec![row(
                31,
                "Let Crosspane hear its own speakers",
                MICROPHONE_DETAIL,
                RowState::NeedsAction,
            )];
            view.buttons.push(button(
                1031,
                ButtonRole::Confirm,
                "Install the sound driver",
                ButtonKind::Primary,
            ));
        }
        ScreenId::Network => {
            view.title = "Let Crosspane through the firewall".into();
            view.message = "ufw is on and has no rule for Crosspane. Setup can add this rule for \
                            your local network only:\n\npkexec /usr/bin/ufw allow from \
                            192.168.1.0/24 to any port 47811:47812 proto udp comment 'Crosspane \
                            (LAN)'\n\nYour system asks for your password before it runs."
                .into();
            // This supplied traffic illustration is intentional; the step below is still open.
            view.illustration.traffic_observed = true;
            view.rows = vec![row(
                30,
                "Allow Crosspane on your network",
                "ufw is on and has no rule for Crosspane.",
                RowState::NeedsAction,
            )];
            view.buttons.extend([
                button(1, ButtonRole::Next, "Not now", ButtonKind::Link),
                button(
                    1030,
                    ButtonRole::Confirm,
                    "Allow Crosspane on the network",
                    ButtonKind::Primary,
                ),
            ]);
        }
        ScreenId::HidingChoice => {
            view.title = "Windows you send from this Mac".into();
            view.message =
                "What happens here while one of this Mac's windows is shown on the other \
                            computer. Hide uses an Apple private interface and falls back to \
                            mirroring if it stops working."
                    .into();
            view.buttons.push(button(
                1,
                ButtonRole::Next,
                "Apply and restart Crosspane",
                ButtonKind::Primary,
            ));
        }
        ScreenId::Connect => {
            view.title = "Pair with the other computer".into();
            view.message = "Both computers will show a number to compare.".into();
            view.rows = vec![row(
                951,
                "Found mac-studio on your network",
                "Pair with it to continue.",
                RowState::Note,
            )];
            view.link_caption = Some("Other ways to connect".into());
            view.buttons.extend([
                button(
                    2010,
                    ButtonRole::Ordinary,
                    "Pair with mac-studio",
                    ButtonKind::Primary,
                ),
                button(
                    2005,
                    ButtonRole::Ordinary,
                    "Enter its address",
                    ButtonKind::Link,
                ),
            ]);
        }
        ScreenId::MatchNumbers => {
            view.title = "Do the numbers match?".into();
            view.message =
                "Confirm only if mac-studio shows the same number. If it doesn't, stop: \
                            something else may be trying to pair."
                    .into();
            view.escape = EscapeMapping::None;
            view.illustration.sas = Some("482 913".into());
            view.peer = Some("mac-studio".into());
            view.buttons = vec![
                button(
                    8,
                    ButtonRole::Cancel,
                    "They don't match",
                    ButtonKind::Destructive,
                ),
                button(
                    7,
                    ButtonRole::Confirm,
                    "The numbers match",
                    ButtonKind::Primary,
                ),
            ];
        }
        ScreenId::Grants => {
            view.title = "What may mac-studio do here?".into();
            view.message =
                "Choose what to allow here. You can change this later in Settings.".into();
            view.peer = Some("mac-studio".into());
            for (index, label) in [
                "Control this computer's keyboard and mouse",
                "Receive windows sent from this computer",
                "See this computer's window list and take windows",
                "Show its windows on this computer",
                "Play its sound on this computer's speakers",
            ]
            .iter()
            .enumerate()
            {
                view.fields.push(FieldView::Toggle {
                    id: 10 + index as u16,
                    role: ToggleRole::Grant,
                    label: (*label).into(),
                    checked: false,
                    enabled: true,
                });
            }
            view.buttons.push(button(
                2202,
                ButtonRole::Confirm,
                "Allow all and continue",
                ButtonKind::Primary,
            ));
        }
        ScreenId::Layout => {
            view.title = "Arrange your screens".into();
            view.message = "Drag the screens to match your desk.".into();
            let display = |node: &str, machine: &str, x| crosspane_ui_kit::layout::DisplayRect {
                node: node.into(),
                machine: machine.into(),
                display: 1,
                name: "Main display".into(),
                origin: [x, 0.0],
                size: [300.0, 190.0],
                pixels: [1920, 1200],
            };
            view.layout = Some(LayoutPreview {
                confirmed: vec![
                    display("local", "This machine", 0.0),
                    display("peer", "Other machine", 300.0),
                ],
                local_node: "local".into(),
                peer_order: vec!["peer".into()],
                busy: false,
            });
            view.buttons.push(button(
                2301,
                ButtonRole::Confirm,
                "Use this layout",
                ButtonKind::Primary,
            ));
        }
        ScreenId::Practice => {
            view.title = "Try each feature once".into();
            view.message = "Choose the Crosspane speakers for mac-studio as the output, play the \
                            test sound, then confirm what you heard."
                .into();
            view.illustration.practice = Some(PracticeIllustration::Tone);
            view.rows = vec![row(
                76,
                "Play sound from this computer on the other one",
                "Waiting for the test sound to reach the other computer.",
                RowState::Working,
            )];
            view.buttons = vec![
                button(
                    3104,
                    ButtonRole::Confirm,
                    "I heard the test sound on the other computer",
                    ButtonKind::Choice,
                ),
                button(
                    3107,
                    ButtonRole::Confirm,
                    "Nothing else was playing sound during the test",
                    ButtonKind::Choice,
                ),
                button(
                    5,
                    ButtonRole::Stop,
                    "Stop this practice",
                    ButtonKind::Secondary,
                ),
                button(
                    6,
                    ButtonRole::Ordinary,
                    "Play the test sound",
                    ButtonKind::Primary,
                ),
            ];
        }
        ScreenId::Summary => {
            view.escape = EscapeMapping::Close;
            view.title = "Your workspace is ready".into();
            view.message = "Everything was checked just now.".into();
            view.summary = SummaryView::WorkspaceReady;
            view.rows = [
                "Check this computer",
                "Install Crosspane",
                "Start Crosspane when you sign in",
                "Pair with the other computer",
                "Arrange the screens",
                "Control the other computer from this keyboard and mouse",
                "Send a window from this computer",
                "Play sound from this computer on the other one",
            ]
            .iter()
            .enumerate()
            .map(|(index, label)| RowView {
                human_confirmed: index >= 5,
                ..row(index as u16 + 1, label, "", RowState::Verified)
            })
            .collect();
            view.buttons = vec![
                button(3, ButtonRole::Cancel, "Done", ButtonKind::Primary),
                button(4001, ButtonRole::Retry, "Check again", ButtonKind::Link),
                button(
                    4,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    ButtonKind::Link,
                ),
            ];
        }
        ScreenId::RepairRemove => {
            view.title = "Remove or repair Crosspane".into();
            view.message = "Removal stops Crosspane first. The two options below are separate \
                            choices."
                .into();
            view.progress.current = None;
            view.fields = vec![
                FieldView::Toggle {
                    id: 20,
                    role: ToggleRole::DeleteIdentity,
                    label: "Delete this machine's Crosspane identity and trust".into(),
                    checked: false,
                    enabled: true,
                },
                FieldView::Toggle {
                    id: 21,
                    role: ToggleRole::RemoveAudioDriver,
                    label: REMOVE_AUDIO_LABEL.into(),
                    checked: true,
                    enabled: true,
                },
            ];
            view.buttons = vec![
                button(2, ButtonRole::Back, "Back", ButtonKind::Link),
                button(
                    5005,
                    ButtonRole::Ordinary,
                    "Review repair",
                    ButtonKind::Secondary,
                ),
                button(
                    5001,
                    ButtonRole::Ordinary,
                    "Review what will be removed",
                    ButtonKind::Secondary,
                ),
            ];
        }
    }
    view
}

pub fn disconnected_view() -> WizardView {
    WizardView {
        revision: 1,
        escape: EscapeMapping::Close,
        fields: Vec::new(),
        screen: ScreenId::Welcome,
        title: "Crosspane installer".into(),
        message:
            "Installation is waiting for the production connection. No changes have been made."
                .into(),
        machine: None,
        peer: None,
        rows: vec![row(
            1,
            "Installer connection",
            "Disconnected",
            RowState::Waiting,
        )],
        buttons: vec![button(
            3,
            ButtonRole::Cancel,
            "Close",
            ButtonKind::Secondary,
        )],
        summary: SummaryView::NotInstalled,
        hiding_choice: None,
        motion: MotionPreference::Auto,
        system_reduced_motion: None,
        layout: None,
        progress: ProgressView::default(),
        illustration: IllustrationView::default(),
        demo: false,
        link_caption: None,
    }
}

/// The startup mode is private and immutable. Actions can only edit this in-memory snapshot.
#[derive(Debug)]
pub struct DisconnectedController {
    demo_mode: bool,
    view: WizardView,
}

impl DisconnectedController {
    pub fn new(screen: Option<ScreenId>) -> Self {
        Self {
            demo_mode: screen.is_some(),
            view: screen.map_or_else(disconnected_view, fixture),
        }
    }

    /// A demo of the named screen or variant (see [`screen_named`]).
    pub fn named(name: &str) -> Option<Self> {
        fixture_named(name).map(|view| Self {
            demo_mode: true,
            view,
        })
    }

    pub fn view(&self) -> &WizardView {
        &self.view
    }

    /// Returns true only for an explicit close. No action can create a production job.
    pub fn accept(&mut self, action: WizardAction) -> bool {
        if action.revision != self.view.revision {
            return false;
        }
        match action.intent {
            WizardIntent::Close => return self.view.escape == EscapeMapping::Close,
            WizardIntent::SetMotion(value) => self.view.motion = value,
            WizardIntent::EditPeerAddress { field, value } => {
                if let Some(FieldView::PeerAddress {
                    value: current,
                    enabled: true,
                    ..
                }) = self.view.fields.iter_mut().find(
                    |entry| matches!(entry, FieldView::PeerAddress { id, .. } if *id == field),
                ) {
                    *current = value;
                }
            }
            WizardIntent::SetToggle { field, checked } if self.demo_mode => {
                if let Some(FieldView::Toggle {
                    checked: current,
                    enabled: true,
                    ..
                }) = self
                    .view
                    .fields
                    .iter_mut()
                    .find(|entry| matches!(entry, FieldView::Toggle { id, .. } if *id == field))
                {
                    *current = checked;
                    self.view.revision += 1;
                }
            }
            WizardIntent::ChooseHiding(choice)
                if self.demo_mode && self.view.screen == ScreenId::HidingChoice =>
            {
                self.view.hiding_choice = Some(choice);
                self.view.revision += 1;
            }
            WizardIntent::Button(id) => {
                let Some(button) = self
                    .view
                    .buttons
                    .iter()
                    .find(|button| button.id == id && button.enabled)
                else {
                    return false;
                };
                // "They don't match" is a refusal, not a close.
                if button.role == ButtonRole::Cancel && button.kind != ButtonKind::Destructive {
                    return true;
                }
                if self.demo_mode
                    && matches!(
                        button.role,
                        ButtonRole::Next | ButtonRole::Confirm | ButtonRole::Back
                    )
                {
                    if button.role == ButtonRole::Next
                        && self.view.screen == ScreenId::HidingChoice
                        && self.view.hiding_choice.is_none()
                    {
                        return false;
                    }
                    self.navigate(button.role == ButtonRole::Back);
                }
            }
            WizardIntent::Back if self.demo_mode && self.view.escape == EscapeMapping::Back => {
                self.navigate(true)
            }
            _ => {}
        }
        false
    }

    fn navigate(&mut self, back: bool) {
        let index = SCREENS
            .iter()
            .position(|(id, _)| *id == self.view.screen)
            .unwrap_or(0);
        let next = if back {
            index.saturating_sub(1)
        } else {
            (index + 1).min(SCREENS.len() - 1)
        };
        let revision = self.view.revision + 1;
        let motion = self.view.motion;
        self.view = fixture(SCREENS[next].0);
        self.view.revision = revision;
        self.view.motion = motion;
    }
}
