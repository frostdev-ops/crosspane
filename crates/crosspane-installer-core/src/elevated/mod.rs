//! Pure contract of the elevated setup helper (WP-W4.1c): arguments, rule names, outcomes and
//! state values. OS-free; the native helper and the unelevated launcher live in the Windows crates.
pub mod driver;
pub mod firewall;
pub mod status;
use serde::{Deserialize, Serialize};

pub const HELPER_IMAGE: &str = "crosspane-elevated-setup.exe";
pub const DRIVER_DIRECTORY: &str = "driver";
pub const HARDWARE_ID: &str = r"Crosspane\IddTwinV1";
pub const AGENT_SUFFIX: &str = r"\Programs\Crosspane\crosspane-agent.exe";
pub const RULE_PREFIX: &str = "Crosspane.Agent.UDP.Private.";
pub const GROUP_PREFIX: &str = "Crosspane.";
pub const MAX_INSTALL_ID: usize = 64;
pub const MAX_PROGRAM: usize = 32_767;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ElevatedError {
    #[error("unsupported elevated setup arguments")]
    Arguments,
    #[error("invalid install id")]
    InstallId,
    #[error("invalid agent program path")]
    Program,
    #[error("invalid elevated setup status report")]
    Report,
    #[error("elevated setup consent does not match the action")]
    Consent,
    #[error("elevated setup journal unavailable")]
    Journal,
}

/// `^[A-Za-z0-9-]{1,64}$`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstallId(String);

impl InstallId {
    pub fn parse(text: &str) -> Result<Self, ElevatedError> {
        if is_install_id(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ElevatedError::InstallId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `RULE_PREFIX` followed by the install ID.
    pub fn rule_name(&self) -> String {
        format!("{RULE_PREFIX}{}", self.0)
    }

    /// `GROUP_PREFIX` followed by the install ID.
    pub fn rule_group(&self) -> String {
        format!("{GROUP_PREFIX}{}", self.0)
    }
}

fn is_install_id(text: &str) -> bool {
    (1..=MAX_INSTALL_ID).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

impl TryFrom<String> for InstallId {
    type Error = ElevatedError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if is_install_id(&text) {
            Ok(Self(text))
        } else {
            Err(ElevatedError::InstallId)
        }
    }
}

impl From<InstallId> for String {
    fn from(id: InstallId) -> Self {
        id.0
    }
}

/// Canonical absolute DOS path of the installed agent (see rules below).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentProgram(String);

impl AgentProgram {
    pub fn parse(text: &str) -> Result<Self, ElevatedError> {
        if is_agent_program(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ElevatedError::Program)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// ASCII case-insensitive equality after stripping a leading `\\?\` from `other`.
    pub fn same_path(&self, other: &str) -> bool {
        self.0
            .eq_ignore_ascii_case(other.strip_prefix(r"\\?\").unwrap_or(other))
    }
}

/// The rules of the A2-mod block. A drive-letter start excludes the verbatim `\\?\` and UNC
/// forms, because neither begins with `X:\`. Text is sliced only after the drive check passes,
/// so the slices always fall on character boundaries.
fn is_agent_program(text: &str) -> bool {
    let bytes = text.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    let suffix = AGENT_SUFFIX.as_bytes();
    drive
        && (1..=MAX_PROGRAM).contains(&bytes.len())
        && !text[2..].contains(':')
        && !text.contains(['/', '"', '%', '*', '?', '<', '>', '|'])
        && !text.chars().any(char::is_control)
        && text[3..].split('\\').all(is_path_component)
        && bytes.len() >= suffix.len()
        && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn is_path_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.ends_with('.')
        && !component.ends_with(' ')
}

impl TryFrom<String> for AgentProgram {
    type Error = ElevatedError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if is_agent_program(&text) {
            Ok(Self(text))
        } else {
            Err(ElevatedError::Program)
        }
    }
}

impl From<AgentProgram> for String {
    fn from(program: AgentProgram) -> Self {
        program.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleScope {
    pub id: InstallId,
    pub program: AgentProgram,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    Status(Option<RuleScope>),
    InstallDriver,
    RemoveDriver,
    AddFirewall(RuleScope),
    RemoveFirewall(RuleScope),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerbName {
    Status,
    InstallDriver,
    RemoveDriver,
    AddFirewall,
    RemoveFirewall,
}

impl VerbName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::InstallDriver => "install-driver",
            Self::RemoveDriver => "remove-driver",
            Self::AddFirewall => "add-firewall",
            Self::RemoveFirewall => "remove-firewall",
        }
    }
}

impl Verb {
    pub fn name(&self) -> VerbName {
        match self {
            Self::Status(_) => VerbName::Status,
            Self::InstallDriver => VerbName::InstallDriver,
            Self::RemoveDriver => VerbName::RemoveDriver,
            Self::AddFirewall(_) => VerbName::AddFirewall,
            Self::RemoveFirewall(_) => VerbName::RemoveFirewall,
        }
    }

    /// Only `status` is read-only. Every other verb needs an elevated token.
    pub fn mutates(&self) -> bool {
        !matches!(self, Self::Status(_))
    }

    pub fn scope(&self) -> Option<&RuleScope> {
        match self {
            Self::Status(scope) => scope.as_ref(),
            Self::AddFirewall(scope) | Self::RemoveFirewall(scope) => Some(scope),
            Self::InstallDriver | Self::RemoveDriver => None,
        }
    }
}

/// Parses the frozen command line, at exact positions. Anything else is `Arguments`; a bad ID
/// is `InstallId` and a bad program path is `Program`.
pub fn parse_arguments(arguments: &[&str]) -> Result<Verb, ElevatedError> {
    match arguments {
        ["status"] => Ok(Verb::Status(None)),
        ["install-driver"] => Ok(Verb::InstallDriver),
        ["remove-driver"] => Ok(Verb::RemoveDriver),
        ["status", "--install-id", id, "--program", program] => {
            Ok(Verb::Status(Some(scope(id, program)?)))
        }
        ["add-firewall", "--install-id", id, "--program", program] => {
            Ok(Verb::AddFirewall(scope(id, program)?))
        }
        ["remove-firewall", "--install-id", id, "--program", program] => {
            Ok(Verb::RemoveFirewall(scope(id, program)?))
        }
        _ => Err(ElevatedError::Arguments),
    }
}

fn scope(id: &str, program: &str) -> Result<RuleScope, ElevatedError> {
    Ok(RuleScope {
        id: InstallId::parse(id)?,
        program: AgentProgram::parse(program)?,
    })
}

/// The argument vector of `verb`. `parse_arguments` reads it back unchanged.
pub fn render_arguments(verb: &Verb) -> Vec<String> {
    render(verb, false)
}

/// ShellExecuteExW lpParameters: rendered arguments joined by one space; the program in `"…"`.
pub fn command_line(verb: &Verb) -> String {
    render(verb, true).join(" ")
}

fn render(verb: &Verb, quote_program: bool) -> Vec<String> {
    let mut rendered = vec![verb.name().as_str().to_owned()];
    if let Some(scope) = verb.scope() {
        // `AgentProgram` forbids `"`, so the quotes need no escaping.
        let program = if quote_program {
            format!("\"{}\"", scope.program.as_str())
        } else {
            scope.program.as_str().to_owned()
        };
        rendered.extend([
            "--install-id".to_owned(),
            scope.id.as_str().to_owned(),
            "--program".to_owned(),
            program,
        ]);
    }
    rendered
}

/// `^oem[0-9]{1,5}\.inf$`, ASCII case-insensitive.
pub fn is_published_name(text: &str) -> bool {
    let bytes = text.as_bytes();
    if !(8..=12).contains(&bytes.len()) {
        return false;
    }
    let (head, rest) = bytes.split_at(3);
    let (digits, tail) = rest.split_at(rest.len() - 4);
    head.eq_ignore_ascii_case(b"oem")
        && tail.eq_ignore_ascii_case(b".inf")
        && (1..=5).contains(&digits.len())
        && digits.iter().all(u8::is_ascii_digit)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Done,
    AlreadyDone,
    RebootRequired,
    Refused,
    NotElevated,
    Mismatch,
    Failed,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Done => 0,
            Self::AlreadyDone => 16,
            Self::RebootRequired => 17,
            Self::Refused => 32,
            Self::NotElevated => 33,
            Self::Mismatch => 48,
            Self::Failed => 64,
        }
    }

    /// The inverse of `exit_code`. A panic exits 101, which is not an `Outcome`; the launcher
    /// treats it as unverified.
    pub fn from_exit_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Done),
            16 => Some(Self::AlreadyDone),
            17 => Some(Self::RebootRequired),
            32 => Some(Self::Refused),
            33 => Some(Self::NotElevated),
            48 => Some(Self::Mismatch),
            64 => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn succeeded(self) -> bool {
        matches!(self, Self::Done | Self::AlreadyDone | Self::RebootRequired)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverState {
    Absent,
    PackageOnly,
    DeviceWithoutDriver,
    Installed,
    Mismatch,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirewallState {
    Present,
    Missing,
    Mismatch,
    Unavailable,
    NotRequested,
}
