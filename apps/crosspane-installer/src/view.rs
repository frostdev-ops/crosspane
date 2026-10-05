//! Immutable presentation snapshots and revision-correlated user intent.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenId {
    Welcome,
    Compatibility,
    InstallPlan,
    Installing,
    Permissions,
    AudioComponent,
    Network,
    HidingChoice,
    Connect,
    MatchNumbers,
    Grants,
    Layout,
    Practice,
    Summary,
    RepairRemove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Unchecked,
    Working,
    NeedsAction,
    Waiting,
    Verified,
    Failed,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummaryView {
    NotInstalled,
    InstalledWaiting,
    WorkspaceReady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressGroup {
    Install,
    PermissionsNetwork,
    Connect,
    Arrange,
    Practice,
    Ready,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgressView {
    pub current: Option<ProgressGroup>,
    pub completed: Vec<ProgressGroup>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PracticeIllustration {
    Pointer,
    Window,
    Tone,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IllustrationView {
    pub permission_row: Option<u16>,
    pub traffic_observed: bool,
    pub sas: Option<String>,
    pub practice: Option<PracticeIllustration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionPreference {
    Auto,
    Reduced,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidingChoice {
    Hide,
    Mirror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonRole {
    Next,
    Back,
    Retry,
    Stop,
    Release,
    Confirm,
    Cancel,
    Ordinary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeMapping {
    None,
    Back,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleRole {
    DeleteIdentity,
    RemoveAudioDriver,
    Grant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldView {
    PeerAddress {
        id: u16,
        value: String,
        enabled: bool,
    },
    Toggle {
        id: u16,
        role: ToggleRole,
        label: String,
        checked: bool,
        enabled: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowView {
    pub id: u16,
    pub label: String,
    pub detail: String,
    pub state: RowState,
    pub human_confirmed: bool,
}

/// Rows with ids in this range are checklist entries: the shell draws them as a compact list
/// inside the card of the ordinary row before them, never as cards of their own. Their states
/// read as `Working` "Checking…", `Verified` "Passed", `Failed` "Failed: `detail`" and `Waiting`
/// "Couldn't confirm: `detail`". Step ids never reach this range.
pub const CHECK_ROW_IDS: std::ops::Range<u16> = 900..932;

/// The row id of checklist entry `index`, saturating at the last id of the range.
pub fn check_row_id(index: usize) -> u16 {
    let span = usize::from(CHECK_ROW_IDS.end - CHECK_ROW_IDS.start - 1);
    CHECK_ROW_IDS.start + index.min(span) as u16
}

impl RowView {
    /// Whether this row is a checklist entry under the card before it.
    pub fn is_check(&self) -> bool {
        CHECK_ROW_IDS.contains(&self.id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ButtonView {
    pub id: u16,
    pub role: ButtonRole,
    pub label: String,
    pub enabled: bool,
    pub kind: ButtonKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WizardView {
    pub revision: u64,
    pub escape: EscapeMapping,
    pub fields: Vec<FieldView>,
    pub screen: ScreenId,
    pub title: String,
    pub message: String,
    pub machine: Option<String>,
    pub peer: Option<String>,
    pub rows: Vec<RowView>,
    pub buttons: Vec<ButtonView>,
    pub summary: SummaryView,
    pub hiding_choice: Option<HidingChoice>,
    pub motion: MotionPreference,
    pub system_reduced_motion: Option<bool>,
    pub layout: Option<LayoutPreview>,
    pub progress: ProgressView,
    pub illustration: IllustrationView,
    pub demo: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutPreview {
    pub confirmed: Vec<crosspane_ui_kit::layout::DisplayRect>,
    pub local_node: String,
    pub peer_order: Vec<String>,
    pub busy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WizardAction {
    pub revision: u64,
    pub intent: WizardIntent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WizardIntent {
    Button(u16),
    Back,
    Close,
    SetMotion(MotionPreference),
    EditPeerAddress { field: u16, value: String },
    SetToggle { field: u16, checked: bool },
    ChooseHiding(HidingChoice),
    Layout(crosspane_ui_kit::layout::LayoutAction),
}
