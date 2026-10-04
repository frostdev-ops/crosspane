//! Selected user-manager facts and bounded operations, never readiness or consent policy.
use super::{
    native_io::*,
    payload::{RenderedResource, sha256},
};
use crate::agent_contract::{
    AgentReply, BootstrapPhase, BootstrapV1, CallFailure, DecodedReply, HealthSnapshot,
    ObservationSource, PendingHealthReason, StatusAdmission,
};
use crosspane_installer_core::MutationOutcome;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub const UNIT: &str = "crosspane-agent.service";
const UNIT_TEMPLATE: &[u8] =
    include_bytes!("../../../../../packaging/linux/crosspane-agent.service");
const SETTINGS_TEMPLATE: &[u8] =
    include_bytes!("../../../../../packaging/linux/crosspane-settings.desktop");
const INSTALLER_TEMPLATE: &[u8] =
    include_bytes!("../../../../../packaging/linux/crosspane-installer.desktop");
type Result<T> = std::result::Result<T, ServiceError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("unknown or malformed manager observation")]
    Unknown,
    #[error("foreign unit, resource or agent instance")]
    Foreign,
    #[error("selected service observation changed; inspect before retry")]
    OutcomeUnknown,
    #[error(transparent)]
    Native(#[from] NativeError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Reload,
    Enable,
    Disable,
    Start,
    Stop,
    Restart,
}
impl ServiceAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Reload => "daemon-reload",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFacts {
    pub fragment: PathBuf,
    pub enabled: bool,
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: u32,
    pub needs_reload: bool,
    pub source: ObservationSource,
}
/// Opaque, single-use plan; its original manager facts must still match before mutation.
#[derive(Debug)]
pub struct ServicePlan {
    action: ServiceAction,
    before: ServiceFacts,
    previous_instance: Option<u64>,
}
#[derive(Debug)]
pub struct ServiceResult {
    pub action: ServiceAction,
    pub before: ServiceFacts,
    pub after: Option<ServiceFacts>,
    pub previous_instance: Option<u64>,
    /// Verifies only the requested manager state. Start/restart remain Unknown until inspection.
    pub outcome: MutationOutcome,
}
/// Bootstrap progress is distinct from the actual decoded, identity-matched Status facts.
pub enum AgentEvidence {
    /// Only this selected manager unit is inactive; this is not a clean-stop or parking proof.
    ManagerInactive,
    Starting(BootstrapV1),
    WaitingForKeystore(BootstrapV1),
    Failed(BootstrapV1),
    PendingStatus(BootstrapV1),
    PendingHealthContract(PendingHealthReason, BootstrapV1),
    StatusFailure(CallFailure),
    Matched(Box<HealthSnapshot>),
}

pub struct LinuxService {
    io: Arc<LinuxNativeIo>,
    environment: ChildEnvironment,
    resources: Vec<RenderedResource>,
    environment_values: Vec<String>,
    pending: Mutex<Option<PendingOperation>>,
}
impl std::fmt::Debug for AgentEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentEvidence { .. }")
    }
}
impl std::fmt::Debug for LinuxService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxService { .. }")
    }
}

// Parse the bounded systemctl property word representation; unsupported escapes fail closed.
fn words(value: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let (mut word, mut quoted, mut present) = (String::new(), false, false);
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                present = true;
            }
            '\\' => {
                let c = chars.next().ok_or(ServiceError::Unknown)?;
                if !matches!(c, '\\' | '"' | '$' | '`' | ' ') {
                    return Err(ServiceError::Unknown);
                }
                word.push(c);
                present = true;
            }
            ' ' if !quoted => {
                if present {
                    result.push(std::mem::take(&mut word));
                    present = false;
                }
            }
            c if c.is_control() => return Err(ServiceError::Unknown),
            c => {
                word.push(c);
                present = true;
            }
        }
    }
    if quoted {
        return Err(ServiceError::Unknown);
    }
    if present {
        result.push(word);
    }
    Ok(result)
}
fn literal(value: &str, desktop: bool) -> Result<String> {
    if !value.starts_with('"') || !value.ends_with('"') {
        return Err(ServiceError::Foreign);
    }
    let value = if desktop {
        let mut decoded = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.next() != Some('\\') {
                return Err(ServiceError::Foreign);
            }
            decoded.push(c);
        }
        decoded
    } else {
        value.into()
    };
    let mut parsed = words(&value)?;
    if parsed.len() != 1 {
        return Err(ServiceError::Foreign);
    }
    let mut output = String::new();
    let value = parsed.remove(0);
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '%' && chars.next() != Some('%') {
            return Err(ServiceError::Foreign);
        }
        output.push(c);
    }
    Ok(output)
}
fn template_record(record: &RenderedResource, template: &[u8], expected: &[String]) -> Result<()> {
    if record.template_sha256 != sha256(template)
        || record.rendered_sha256 != sha256(&record.bytes)
        || record.bytes.len() > MAX_COMMAND_BYTES
    {
        return Err(ServiceError::Foreign);
    }
    let text = std::str::from_utf8(&record.bytes).map_err(|_| ServiceError::Foreign)?;
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(ServiceError::Foreign);
    }
    let template = std::str::from_utf8(template).map_err(|_| ServiceError::Foreign)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != template.lines().count() {
        return Err(ServiceError::Foreign);
    }
    let mut values = expected.iter();
    for (line, template) in lines.into_iter().zip(template.lines()) {
        if template.contains("{{") {
            let (key, _) = template.split_once('=').ok_or(ServiceError::Foreign)?;
            let value = line
                .strip_prefix(&format!("{key}="))
                .ok_or(ServiceError::Foreign)?;
            let value = if key == "ExecStart" {
                value.strip_suffix(" run").ok_or(ServiceError::Foreign)?
            } else {
                value
            };
            if literal(value, key == "Exec")? != *values.next().ok_or(ServiceError::Foreign)? {
                return Err(ServiceError::Foreign);
            }
        } else if line != template {
            return Err(ServiceError::Foreign);
        }
    }
    if values.next().is_some() {
        return Err(ServiceError::Foreign);
    }
    Ok(())
}

impl LinuxService {
    pub(crate) fn target_binding(&self) -> super::native_io::TargetBinding {
        self.io.target_binding()
    }
    /// Consumes only WP-4.7b rendered records; neither a record nor this adapter authorizes file writes.
    pub fn new(
        io: Arc<LinuxNativeIo>,
        session: BTreeMap<String, String>,
        resources: Vec<RenderedResource>,
        deadline: &Deadline,
    ) -> Result<Self> {
        deadline.check()?;
        let p = io.target().paths();
        let expected_paths = [
            p.config_home.join("systemd/user").join(UNIT),
            p.data_home.join("applications/crosspane-settings.desktop"),
            p.data_home.join("applications/crosspane-installer.desktop"),
        ];
        let environment_values = vec![
            format!("XDG_CONFIG_HOME={}", p.config_home.display()),
            format!("XDG_STATE_HOME={}", p.state_home.display()),
            format!("XDG_RUNTIME_DIR={}", p.runtime_home.display()),
            format!(
                "CROSSPANE_RUNTIME_DIR={}",
                io.target().runtime_dir().display()
            ),
        ];
        if resources.len() != 3 {
            return Err(ServiceError::Foreign);
        }
        let mut resources = resources;
        resources.sort_by_key(|r| expected_paths.iter().position(|p| p == &r.target));
        for (record, path) in resources.iter().zip(&expected_paths) {
            if &record.target != path || record.source != io.target().source() {
                return Err(ServiceError::Foreign);
            }
        }
        let mut unit_values = vec![io.target().agent_path().to_string_lossy().into_owned()];
        unit_values.extend(environment_values.clone());
        template_record(&resources[0], UNIT_TEMPLATE, &unit_values)?;
        template_record(
            &resources[1],
            SETTINGS_TEMPLATE,
            &[p.prefix
                .join("bin/crosspane-ui")
                .to_string_lossy()
                .into_owned()],
        )?;
        template_record(
            &resources[2],
            INSTALLER_TEMPLATE,
            &[p.prefix
                .join("bin/crosspane-installer")
                .to_string_lossy()
                .into_owned()],
        )?;
        let environment = io.manager_environment(session, deadline)?;
        Ok(Self {
            io,
            environment,
            resources,
            environment_values,
            pending: Mutex::new(None),
        })
    }
    fn files(&self, deadline: &Deadline) -> Result<()> {
        self.io.validate_target()?;
        for record in &self.resources {
            deadline.check()?;
            if self.io.read(&record.target, MAX_COMMAND_BYTES, false)? != record.bytes {
                return Err(ServiceError::Foreign);
            }
        }
        Ok(())
    }
    fn command(&self, verb: &str) -> Result<CommandSpec> {
        let mut argv = vec!["--user".into(), verb.into()];
        if verb == "show" {
            argv.push("--all".into());
        }
        if verb != "daemon-reload" {
            argv.push(UNIT.into());
        }
        if verb == "show" {
            argv.extend(["-p".into(), MANAGER_PROPERTIES.into()]);
        }
        Ok(CommandSpec::new(
            "/usr/bin/systemctl".into(),
            argv,
            self.environment.clone(),
            MAX_COMMAND_BYTES,
        )?)
    }
    fn query(&self, verb: &str, deadline: &Deadline) -> Result<CommandOutput> {
        let result = self.io.run(&self.command(verb)?, deadline)?;
        if !result.stderr.is_empty() {
            return Err(ServiceError::Unknown);
        }
        Ok(result)
    }
    fn properties(&self, deadline: &Deadline) -> Result<BTreeMap<String, String>> {
        let output = self.query("show", deadline)?;
        if output.code != Some(0) {
            return Err(ServiceError::Unknown);
        }
        let text = std::str::from_utf8(&output.stdout).map_err(|_| ServiceError::Unknown)?;
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let (key, value) = line.split_once('=').ok_or(ServiceError::Unknown)?;
            if !MANAGER_PROPERTIES.split(',').any(|p| p == key)
                || value.chars().any(char::is_control)
                || map.insert(key.into(), value.into()).is_some()
            {
                return Err(ServiceError::Unknown);
            }
        }
        if map.len() != MANAGER_PROPERTIES.split(',').count() {
            return Err(ServiceError::Unknown);
        }
        Ok(map)
    }
    // Audit: official systemd.unit/service manuals and dbus-{unit,service,execute,cgroup}.
    // Seal propagation, emergency actions, hooks (including Ex privilege flags), environment,
    // termination/lifecycle, slice/delegation/OOM, executable filesystem/PAM redirection,
    // and directory creation/removal. Empty lists also make their modes and quotas inert.
    // ExecStopPre does not exist. Sockets= is sealed through effective dependency edges.
    // Type=simple makes GuessMainPID and ReloadSignal inert; action arguments are inert with
    // actions=none. Quotas/accounting and access restrictions cannot select other code/units.
    // Missing exported properties (including newer ExecReloadPost) refuse unsupported managers.
    fn facts(&self, properties: &BTreeMap<String, String>) -> Result<ServiceFacts> {
        let get = |key: &str| {
            properties
                .get(key)
                .map(String::as_str)
                .ok_or(ServiceError::Unknown)
        };
        for (key, expected) in [
            ("Id", UNIT),
            ("LoadState", "loaded"),
            ("DropInPaths", ""),
            ("User", ""),
            ("Group", ""),
            ("DynamicUser", "no"),
            ("PartOf", "graphical-session.target"),
            ("Requisite", "graphical-session.target"),
            ("KillSignal", "15"),
            ("TimeoutStopUSec", "10s"),
            ("Restart", "on-failure"),
            ("RestartUSec", "3s"),
            ("ExecStartPre", ""),
            ("ExecStartPost", ""),
            ("ExecStop", ""),
            ("ExecStopPost", ""),
            ("ExecReload", ""),
            ("ExecCondition", ""),
            ("Type", "simple"),
            ("Wants", ""),
            ("BindsTo", ""),
            ("Upholds", ""),
            ("OnFailure", ""),
            ("Conflicts", "shutdown.target"),
            ("Before", "shutdown.target"),
            ("DefaultDependencies", "yes"),
            ("KillMode", "control-group"),
            ("SendSIGKILL", "yes"),
            ("FinalKillSignal", "9"),
            ("RestartKillSignal", "15"),
            ("SendSIGHUP", "no"),
            ("UnsetEnvironment", ""),
            ("PassEnvironment", ""),
            ("WorkingDirectory", ""),
            ("UMask", "0022"),
            ("BusName", ""),
            ("PIDFile", ""),
            ("RemainAfterExit", "no"),
            ("NotifyAccess", "none"),
            ("ExecSearchPath", ""),
            ("StandardInput", "null"),
            ("StandardOutput", "journal"),
            ("StandardError", "inherit"),
            ("TTYPath", "/dev/console"),
            ("EnvironmentFiles", ""),
            ("RootDirectory", ""),
            ("RootImage", ""),
            ("StartLimitIntervalUSec", "2min"),
            ("StartLimitBurst", "30"),
            ("OnSuccess", ""),
            ("PropagatesStopTo", ""),
            ("PropagatesReloadTo", ""),
            ("ReloadPropagatedFrom", ""),
            ("StopPropagatedFrom", ""),
            ("JoinsNamespaceOf", ""),
            ("RequiresMountsFor", ""),
            ("WantsMountsFor", ""),
            ("RequiredBy", ""),
            ("RequisiteOf", ""),
            ("BoundBy", ""),
            ("UpheldBy", ""),
            ("ConsistsOf", ""),
            ("ConflictedBy", ""),
            ("OnSuccessOf", ""),
            ("OnFailureOf", ""),
            ("Triggers", ""),
            ("TriggeredBy", ""),
            ("Following", ""),
            ("SliceOf", ""),
            ("DelegateControllers", ""),
            ("DelegateSubgroup", ""),
            ("Conditions", ""),
            ("Asserts", ""),
            ("ExecConditionEx", ""),
            ("ExecStartPreEx", ""),
            ("ExecStartPostEx", ""),
            ("ExecStopEx", ""),
            ("ExecStopPostEx", ""),
            ("ExecReloadEx", ""),
            ("ExecReloadPost", ""),
            ("ExecReloadPostEx", ""),
            ("RestartPreventExitStatus", ""),
            ("RestartForceExitStatus", ""),
            ("SuccessExitStatus", ""),
            ("OpenFile", ""),
            ("ExtraFileDescriptorNames", ""),
            ("BindPaths", ""),
            ("BindReadOnlyPaths", ""),
            ("TemporaryFileSystem", ""),
            ("MountImages", ""),
            ("ExtensionImages", ""),
            ("ExtensionDirectories", ""),
            ("PAMName", ""),
            ("Slice", "app.slice"),
            ("Delegate", "no"),
            ("OOMPolicy", "stop"),
            ("ManagedOOMSwap", "auto"),
            ("ManagedOOMMemoryPressure", "auto"),
            ("ManagedOOMPreference", "none"),
            ("SuccessAction", "none"),
            ("FailureAction", "none"),
            ("StartLimitAction", "none"),
            ("JobTimeoutAction", "none"),
            ("OnSuccessJobMode", "fail"),
            ("OnFailureJobMode", "replace"),
            ("StopWhenUnneeded", "no"),
            ("RefuseManualStart", "no"),
            ("RefuseManualStop", "no"),
            ("AllowIsolate", "no"),
            ("IgnoreOnIsolate", "no"),
            ("SurviveFinalKillSignal", "no"),
            ("JobTimeoutUSec", "infinity"),
            ("JobRunningTimeoutUSec", "infinity"),
            ("CollectMode", "inactive"),
            ("RestartMode", "normal"),
            ("RestartSteps", "0"),
            ("RestartMaxDelayUSec", "infinity"),
            ("TimeoutStartFailureMode", "terminate"),
            ("TimeoutStopFailureMode", "terminate"),
            ("RuntimeMaxUSec", "infinity"),
            ("RuntimeRandomizedExtraUSec", "0"),
            ("WatchdogUSec", "0"),
            ("ExitType", "main"),
            ("FileDescriptorStoreMax", "0"),
            ("NFileDescriptorStore", "0"),
            ("FileDescriptorStorePreserve", "restart"),
            ("RootDirectoryStartOnly", "no"),
            ("RootEphemeral", "no"),
            ("RuntimeDirectory", ""),
            ("StateDirectory", ""),
            ("CacheDirectory", ""),
            ("LogsDirectory", ""),
            ("ConfigurationDirectory", ""),
            ("RuntimeDirectorySymlink", ""),
            ("StateDirectorySymlink", ""),
            ("CacheDirectorySymlink", ""),
            ("LogsDirectorySymlink", ""),
            ("RootMStack", ""),
            ("RuntimeDirectoryPreserve", "no"),
        ] {
            if get(key)? != expected {
                return Err(ServiceError::Foreign);
            }
        }
        // systemd.unit/dbus-unit and systemd.service/dbus-service define this finite seal.
        // All propagation/inverse edges and hooks are empty; actions cannot affect other units.
        // Sockets= is represented by Wants/After/TriggeredBy, not a service property.
        // app.slice is the default non-instanced user slice, adding Requires and After.
        for (key, expected) in [
            ("Requires", "app.slice basic.target"),
            ("After", "app.slice basic.target graphical-session.target"),
        ] {
            let mut actual: Vec<_> = get(key)?.split(' ').collect();
            actual.sort_unstable();
            if actual != expected.split(' ').collect::<Vec<_>>() {
                return Err(ServiceError::Foreign);
            }
        }
        if get("FragmentPath")? != self.resources[0].target.to_string_lossy() {
            return Err(ServiceError::Foreign);
        }
        let executable = self.io.target().agent_path().to_string_lossy().into_owned();
        for (key, flags) in [("ExecStart", "ignore_errors=no"), ("ExecStartEx", "flags=")] {
            let exec = get(key)?
                .strip_prefix("{ ")
                .and_then(|s| s.strip_suffix(" }"))
                .ok_or(ServiceError::Foreign)?;
            let fields: Vec<_> = exec.split(" ; ").collect();
            if fields.len() != 8
                || fields[0] != format!("path={executable}")
                || fields[1] != format!("argv[]={executable} run")
                || fields[2] != flags
                || !fields[3].starts_with("start_time=")
                || !fields[4].starts_with("stop_time=")
                || !fields[5].starts_with("pid=")
                || !fields[6].starts_with("code=")
                || !fields[7].starts_with("status=")
            {
                return Err(ServiceError::Foreign);
            }
        }
        let mut actual = words(get("Environment")?)?;
        let mut expected = self.environment_values.clone();
        actual.sort();
        expected.sort();
        if actual != expected {
            return Err(ServiceError::Foreign);
        }
        let enabled = match get("UnitFileState")? {
            "enabled" => true,
            "disabled" => false,
            _ => return Err(ServiceError::Foreign),
        };
        if get("WantedBy")?
            != if enabled {
                "graphical-session.target"
            } else {
                ""
            }
        {
            return Err(ServiceError::Foreign);
        }
        let active = get("ActiveState")?;
        if !matches!(
            active,
            "active" | "inactive" | "failed" | "activating" | "deactivating"
        ) {
            return Err(ServiceError::Unknown);
        }
        let main_pid = get("MainPID")?
            .parse::<u32>()
            .map_err(|_| ServiceError::Unknown)?;
        if main_pid.to_string() != get("MainPID")?
            || (active == "active" && main_pid == 0)
            || (active == "inactive" && main_pid != 0)
        {
            return Err(ServiceError::Unknown);
        }
        Ok(ServiceFacts {
            fragment: self.resources[0].target.clone(),
            enabled,
            active_state: active.into(),
            sub_state: get("SubState")?.into(),
            main_pid,
            needs_reload: match get("NeedDaemonReload")? {
                "yes" => true,
                "no" => false,
                _ => return Err(ServiceError::Unknown),
            },
            source: self.io.target().source(),
        })
    }
    /// All properties come from one stable, pinned manager observation and exact installed bytes.
    pub fn observe(&self, deadline: &Deadline) -> Result<ServiceFacts> {
        self.files(deadline)?;
        let properties = self.properties(deadline)?;
        let facts = self.facts(&properties)?;
        let cat = self.query("cat", deadline)?;
        let mut expected = format!("# {}\n", self.resources[0].target.display()).into_bytes();
        expected.extend(&self.resources[0].bytes);
        if cat.code != Some(0) || cat.stdout != expected {
            return Err(ServiceError::Foreign);
        }
        for (verb, value, code) in [
            (
                "is-active",
                facts.active_state.as_str(),
                if facts.active_state == "active" { 0 } else { 3 },
            ),
            (
                "is-enabled",
                if facts.enabled { "enabled" } else { "disabled" },
                if facts.enabled { 0 } else { 1 },
            ),
        ] {
            let result = self.query(verb, deadline)?;
            if result.code != Some(code) || result.stdout != format!("{value}\n").as_bytes() {
                return Err(ServiceError::Unknown);
            }
        }
        if self.properties(deadline)? != properties {
            return Err(ServiceError::OutcomeUnknown);
        }
        self.files(deadline)?;
        Ok(facts)
    }
    pub fn plan(
        &self,
        proof: &SupportProof,
        action: ServiceAction,
        deadline: &Deadline,
    ) -> Result<ServicePlan> {
        proof.check(&self.io)?;
        if self
            .pending
            .lock()
            .map_err(|_| ServiceError::Unknown)?
            .as_ref()
            .is_some_and(|p| !p.completed())
        {
            return Err(NativeError::Busy.into());
        }
        // Every plan reconciles fresh pinned-manager facts under the normal payload lock.
        // Outstanding launch/cleanup in an earlier service instance also keeps this lock busy.
        let _lock = self.io.install_lease(proof)?;
        let before = self.observe(deadline)?;
        if before.needs_reload && action != ServiceAction::Reload {
            return Err(ServiceError::Unknown);
        }
        let previous_instance = if before.main_pid != 0 {
            let (bootstrap, _) = self.io.bootstrap(deadline)?;
            if bootstrap.pid != before.main_pid {
                return Err(ServiceError::Foreign);
            }
            Some(bootstrap.instance_id)
        } else {
            None
        };
        if action == ServiceAction::Start && previous_instance.is_some() {
            return Err(ServiceError::Foreign);
        }
        if matches!(action, ServiceAction::Start | ServiceAction::Restart)
            && previous_instance.is_none()
        {
            self.absent_agent(deadline)?;
        }
        proof.check(&self.io)?;
        *self.pending.lock().map_err(|_| ServiceError::Unknown)? = None;
        Ok(ServicePlan {
            action,
            before,
            previous_instance,
        })
    }
    fn absent_agent(&self, deadline: &Deadline) -> Result<()> {
        deadline.check()?;
        if self.io.metadata(self.io.target().runtime_dir())?.is_none() {
            return Ok(());
        }
        if self
            .io
            .metadata(&self.io.target().runtime_dir().join("bootstrap.json"))?
            .is_some()
        {
            // Stale or unmanaged progress cannot authorize a new start-capable operation.
            return Err(if self.io.bootstrap(deadline).is_ok() {
                ServiceError::Foreign
            } else {
                ServiceError::Unknown
            });
        }
        if self.io.metadata(&self.io.target().socket_path())?.is_some() {
            return Err(ServiceError::Foreign);
        }
        Ok(())
    }
    /// Exactly one command per consumed plan; failures never cause an automatic resend.
    pub fn apply(
        &self,
        proof: &SupportProof,
        plan: ServicePlan,
        deadline: &Deadline,
    ) -> Result<ServiceResult> {
        proof.check(&self.io)?;
        // Serialize with WP-4.7b replacements. The lock is an admitted private state file,
        // never an installed unit or desktop entry.
        let lease = self.io.install_lease(proof)?;
        if self.observe(deadline)? != plan.before {
            return Err(ServiceError::OutcomeUnknown);
        }
        if let Some(id) = plan.previous_instance
            && self.io.bootstrap(deadline)?.0.instance_id != id
        {
            return Err(ServiceError::Foreign);
        }
        if matches!(plan.action, ServiceAction::Start | ServiceAction::Restart)
            && plan.previous_instance.is_none()
        {
            self.absent_agent(deadline)?;
        }
        proof.check(&self.io)?;
        let ManagerMutation {
            result: output,
            pending,
        } = self.io.run_manager_mutation(
            proof,
            &self.command(plan.action.verb())?,
            lease,
            deadline,
        );
        *self.pending.lock().map_err(|_| ServiceError::Unknown)? = pending;
        let mut result = ServiceResult {
            action: plan.action,
            before: plan.before,
            after: None,
            previous_instance: plan.previous_instance,
            outcome: MutationOutcome::Unknown,
        };
        let Ok(output) = output else {
            return Ok(result);
        };
        // Bounded informational diagnostics (including enable/disable symlink messages) do
        // not establish success or failure; the subsequent pinned manager facts do.
        if output.code != Some(0) {
            return Ok(result);
        }
        let Ok(after) = self.observe(deadline) else {
            return Ok(result);
        };
        let verified = match plan.action {
            ServiceAction::Reload => !after.needs_reload,
            ServiceAction::Enable => after.enabled,
            ServiceAction::Disable => !after.enabled,
            ServiceAction::Stop => after.active_state == "inactive" && after.main_pid == 0,
            ServiceAction::Start | ServiceAction::Restart => false,
        };
        if verified {
            result.outcome = MutationOutcome::Verified;
        }
        result.after = Some(after);
        Ok(result)
    }
    /// A fresh Status is admitted only for the manager-selected PID and native bootstrap identity.
    /// Backend/gate fields remain facts for core policy; waiting progress never substitutes health.
    pub fn agent(
        &self,
        facts: &ServiceFacts,
        reply: Option<&AgentReply>,
        expected_id: u64,
        now_ms: u64,
        previous_instance: Option<u64>,
        deadline: &Deadline,
    ) -> Result<AgentEvidence> {
        if self.observe(deadline)? != *facts {
            return Err(ServiceError::OutcomeUnknown);
        }
        if facts.active_state == "inactive" && facts.main_pid == 0 {
            return Ok(AgentEvidence::ManagerInactive);
        }
        let (bootstrap, identity) = self.io.bootstrap(deadline)?;
        if bootstrap.pid != facts.main_pid || previous_instance == Some(bootstrap.instance_id) {
            return Err(ServiceError::Foreign);
        }
        match bootstrap.phase {
            BootstrapPhase::Starting => return Ok(AgentEvidence::Starting(bootstrap)),
            BootstrapPhase::WaitingForKeystore => {
                return Ok(AgentEvidence::WaitingForKeystore(bootstrap));
            }
            BootstrapPhase::Failed => return Ok(AgentEvidence::Failed(bootstrap)),
            BootstrapPhase::Ready => {}
        }
        let Some(reply) = reply else {
            return Ok(AgentEvidence::PendingStatus(bootstrap));
        };
        if expected_id == 0
            || reply.id != expected_id
            || reply.source != self.io.target().source()
            || now_ms < reply.observed_at_ms
            || now_ms - reply.observed_at_ms > 5000
        {
            return Err(ServiceError::Foreign);
        }
        let health = match &reply.result {
            Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => health,
            Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(reason))) => {
                return Ok(AgentEvidence::PendingHealthContract(*reason, bootstrap));
            }
            Err(failure) => return Ok(AgentEvidence::StatusFailure(failure.clone())),
            Ok(_) => return Err(ServiceError::Unknown),
        };
        self.io
            .admit_instance(&health.installer().instance, &bootstrap, &identity)?;
        if self.io.bootstrap(deadline)? != (bootstrap, identity)
            || self.observe(deadline)? != *facts
        {
            return Err(ServiceError::Foreign);
        }
        Ok(AgentEvidence::Matched(health.clone()))
    }
}
