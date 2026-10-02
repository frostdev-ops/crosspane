//! Permanently disconnected fixtures. These values are illustrations, never installation proof.

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

pub fn screen_named(name: &str) -> Option<ScreenId> {
    SCREENS
        .iter()
        .find(|(_, key)| *key == name)
        .map(|(id, _)| *id)
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

pub fn fixture(screen: ScreenId) -> WizardView {
    let (title, message) = match screen {
        ScreenId::Welcome => (
            "Your computers. One workspace.",
            "Bring your keyboard, windows and sound across your computers. Review each change before it happens.",
        ),
        ScreenId::Compatibility => (
            "Check this machine",
            "Compatibility and session checks appear here. A waiting check still needs evidence.",
        ),
        ScreenId::InstallPlan => (
            "Review the installation",
            "Crosspane installs a user agent. The optional Mac audio driver is a separate change for every user on this Mac.",
        ),
        ScreenId::Installing => (
            "Installing Crosspane",
            "Completed work and work still waiting are shown separately. You can stop before the next operation.",
        ),
        ScreenId::Permissions => (
            "Allow only what you need",
            "The signed Crosspane agent asks for these permissions in your graphical session. Review each system prompt.",
        ),
        ScreenId::AudioComponent => (
            "Bring sound across",
            "The optional Crosspane audio driver adds a virtual speakers device. Its installation affects every user on this Mac.",
        ),
        ScreenId::Network => (
            "Find your other machine",
            "Crosspane needs a reachable local network path. Waiting for discovery is not proof that the network is ready.",
        ),
        ScreenId::HidingChoice => (
            "Where should projected windows live?",
            "Choose explicitly. Hide uses an Apple private interface approved by Crosspane's owner and falls back to mirroring if that interface stops working.",
        ),
        ScreenId::Connect => (
            "Connect your machines",
            "Select a discovered peer or enter its address. You decide which machine to trust.",
        ),
        ScreenId::MatchNumbers => (
            "Match the numbers on both machines",
            "Compare both displays before confirming. These clearly labelled demo numbers do not establish trust.",
        ),
        ScreenId::Grants => (
            "Choose each direction",
            "Give only the access you want. Each direction is a separate choice; receiving access does not grant it back.",
        ),
        ScreenId::Layout => (
            "Arrange your displays",
            "Place the displays as they sit on your desk. Apply sends an intent; confirmed positions arrive from the controller.",
        ),
        ScreenId::Practice => (
            "Try your workspace",
            "Practice checks need actual results and your confirmation. Release control or stop whenever you need to.",
        ),
        ScreenId::Summary => (
            "Your workspace",
            "This ready illustration is demo data. No machine was installed, connected or granted access.",
        ),
        ScreenId::RepairRemove => (
            "Repair or remove Crosspane",
            "Removal stops the user agent first. Deleting identity and removing the shared audio driver are separate choices.",
        ),
    };
    let mut view = WizardView {
        revision: 1,
        escape: EscapeMapping::Back,
        fields: Vec::new(),
        screen,
        title: title.into(),
        message: message.into(),
        machine: Some("This machine · review fixture".into()),
        peer: None,
        rows: Vec::new(),
        buttons: vec![
            button(2, ButtonRole::Back, "Back", ButtonKind::Secondary),
            button(1, ButtonRole::Next, "Continue", ButtonKind::Primary),
        ],
        summary: SummaryView::NotInstalled,
        hiding_choice: None,
        motion: MotionPreference::Auto,
        system_reduced_motion: None,
        layout: None,
        progress: ProgressView {
            current: Some(match screen {
                ScreenId::Welcome
                | ScreenId::Compatibility
                | ScreenId::InstallPlan
                | ScreenId::Installing
                | ScreenId::RepairRemove => ProgressGroup::Install,
                ScreenId::Permissions
                | ScreenId::AudioComponent
                | ScreenId::Network
                | ScreenId::HidingChoice => ProgressGroup::PermissionsNetwork,
                ScreenId::Connect | ScreenId::MatchNumbers | ScreenId::Grants => {
                    ProgressGroup::Connect
                }
                ScreenId::Layout => ProgressGroup::Arrange,
                ScreenId::Practice => ProgressGroup::Practice,
                ScreenId::Summary => ProgressGroup::Ready,
            }),
            completed: Vec::new(),
        },
        illustration: IllustrationView::default(),
        demo: true,
    };
    match screen {
        ScreenId::Welcome => {
            view.escape = EscapeMapping::Close;
            view.buttons.remove(0);
        }
        ScreenId::Compatibility => {
            view.rows = vec![
                row(
                    1,
                    "Graphical session",
                    "Waiting for a supported session check",
                    RowState::Unchecked,
                ),
                row(
                    2,
                    "Agent identity",
                    "No signed identity has been inspected",
                    RowState::Unchecked,
                ),
                row(3, "Audio component", "Optional", RowState::NeedsAction),
            ];
        }
        ScreenId::InstallPlan => {
            view.rows = vec![
                row(
                    1,
                    "User agent and startup",
                    "User-owned installation",
                    RowState::NeedsAction,
                ),
                row(
                    2,
                    "System audio driver",
                    "Separate system package; affects all users",
                    RowState::NeedsAction,
                ),
            ];
        }
        ScreenId::Installing => {
            view.rows = vec![
                row(
                    1,
                    "User bundle",
                    "Illustrated completed operation",
                    RowState::Verified,
                ),
                row(
                    2,
                    "Agent startup",
                    "Waiting for readiness receipt",
                    RowState::Waiting,
                ),
            ];
            view.buttons = vec![button(5, ButtonRole::Stop, "Stop", ButtonKind::Secondary)];
        }
        ScreenId::Permissions => {
            view.illustration.permission_row = Some(1);
            view.rows = vec![
                row(
                    1,
                    "Accessibility",
                    "Allows the agent to control permitted windows and input",
                    RowState::NeedsAction,
                ),
                row(
                    2,
                    "Input Monitoring",
                    "Allows the agent to observe input for permitted control",
                    RowState::NeedsAction,
                ),
                row(
                    3,
                    "Screen Recording",
                    "Allows capture of projected windows",
                    RowState::NeedsAction,
                ),
                row(
                    5,
                    "Local Network",
                    "Waiting for signed agent discovery evidence",
                    RowState::Waiting,
                ),
                row(4, "Microphone", MICROPHONE_DETAIL, RowState::NeedsAction),
            ];
        }
        ScreenId::AudioComponent => {
            view.rows = vec![row(
                1,
                "Virtual speakers capture",
                MICROPHONE_DETAIL,
                RowState::NeedsAction,
            )];
        }
        ScreenId::Network => {
            // This supplied traffic illustration is intentional; discovery below remains Waiting.
            view.illustration.traffic_observed = true;
            view.rows = vec![
                row(
                    1,
                    "Local discovery",
                    "Waiting for agent discovery evidence",
                    RowState::Waiting,
                ),
                row(
                    2,
                    "Peer connection",
                    "No authenticated peer connection",
                    RowState::Unchecked,
                ),
            ];
        }
        ScreenId::HidingChoice => {}
        ScreenId::Connect => {
            view.fields.push(FieldView::PeerAddress {
                id: 10,
                value: String::new(),
                enabled: true,
            });
            view.rows.push(row(
                1,
                "Other machines",
                "No peer selected in this fixture",
                RowState::Waiting,
            ));
        }
        ScreenId::MatchNumbers => {
            view.illustration.sas = Some("DEMO 123 456".into());
            view.peer = Some("Other machine · review fixture".into());
            view.buttons[1] = button(
                7,
                ButtonRole::Confirm,
                "The numbers match",
                ButtonKind::Primary,
            );
        }
        ScreenId::Grants => {
            for (id, label) in [
                (11, "Let this machine control the other machine"),
                (12, "Let the other machine control this machine"),
                (13, "Project this machine's windows to the other machine"),
                (14, "Receive the other machine's projected windows"),
            ] {
                view.fields.push(FieldView::Toggle {
                    id,
                    role: ToggleRole::Grant,
                    label: label.into(),
                    checked: false,
                    enabled: true,
                });
            }
        }
        ScreenId::Layout => {
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
        }
        ScreenId::Practice => {
            view.illustration.practice = Some(PracticeIllustration::Pointer);
            view.rows = vec![
                row(
                    1,
                    "Keyboard control",
                    "Waiting for a practice result",
                    RowState::Waiting,
                ),
                row(
                    2,
                    "Window projection",
                    "Waiting for visible return confirmation",
                    RowState::Waiting,
                ),
                row(
                    3,
                    "Sound",
                    "Waiting for your listening confirmation",
                    RowState::Waiting,
                ),
            ];
            view.buttons = vec![
                button(
                    6,
                    ButtonRole::Release,
                    "Release control",
                    ButtonKind::Primary,
                ),
                button(5, ButtonRole::Stop, "Stop practice", ButtonKind::Secondary),
            ];
        }
        ScreenId::Summary => {
            view.progress.completed = vec![
                ProgressGroup::Install,
                ProgressGroup::PermissionsNetwork,
                ProgressGroup::Connect,
                ProgressGroup::Arrange,
                ProgressGroup::Practice,
                ProgressGroup::Ready,
            ];
            view.summary = SummaryView::WorkspaceReady;
            view.rows = vec![row(
                1,
                "Workspace ready · demo illustration",
                "Supplied review state only",
                RowState::Verified,
            )];
        }
        ScreenId::RepairRemove => {
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
                button(4, ButtonRole::Retry, "Repair", ButtonKind::Primary),
                button(
                    8,
                    ButtonRole::Ordinary,
                    "Remove Crosspane",
                    ButtonKind::Destructive,
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
                if button.role == ButtonRole::Cancel {
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
