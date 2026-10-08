//! Owned, read-only Crosspane-rule query. No elevation, mutation, profile query or denial inference.
use crate::reachability::{FirewallRule, RuleEvidence, rule_evidence};
use std::os::windows::process::CommandExt;
use std::{
    io::{Read, Result as IoResult},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const QUERY_WAIT: Duration = Duration::from_secs(5);
const QUERY_SPACING: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(25);
const MAX_OUTPUT: u64 = 64 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// TODO(W4.1c): replace prefix lookup with the exact recorded name once the installer journal
// supplies its stable install ID. Only this Crosspane name family is ever queried, including
// filter associations. The persistent store is a rule-presence fact, not effective policy.
const QUERY: &str = r#"
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
try {
    $module = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\Modules\NetSecurity\NetSecurity.psd1'
    Import-Module $module -ErrorAction Stop
    $ruleErrors = @()
    # COM INetFwRule::Name is the PowerShell DisplayName; the PowerShell Name is a generated GUID.
    $rules = @(NetSecurity\Get-NetFirewallRule -DisplayName 'Crosspane.Agent.UDP.Private.*' -ErrorAction SilentlyContinue -ErrorVariable ruleErrors)
    foreach ($errorRecord in $ruleErrors) {
        if ($errorRecord.CategoryInfo.Category -ne [System.Management.Automation.ErrorCategory]::ObjectNotFound) { throw 'query unavailable' }
    }
    if ($rules.Count -gt 128) { throw 'query too large' }
    $rows = @()
    foreach ($rule in $rules) {
        $application = @($rule | NetSecurity\Get-NetFirewallApplicationFilter)
        $port = @($rule | NetSecurity\Get-NetFirewallPortFilter)
        $address = @($rule | NetSecurity\Get-NetFirewallAddressFilter)
        $type = @($rule | NetSecurity\Get-NetFirewallInterfaceTypeFilter)
        $interface = @($rule | NetSecurity\Get-NetFirewallInterfaceFilter)
        $service = @($rule | NetSecurity\Get-NetFirewallServiceFilter)
        $security = @($rule | NetSecurity\Get-NetFirewallSecurityFilter)
        foreach ($filter in @($application.Count,$port.Count,$address.Count,$type.Count,$interface.Count,$service.Count,$security.Count)) {
            if ($filter -ne 1) { throw 'ambiguous filter' }
        }
        $rows += [ordered]@{
            name=[string]$rule.DisplayName; group=[string]$rule.Group; program=[string]$application[0].Program
            enabled=[string]$rule.Enabled; direction=[string]$rule.Direction; action=[string]$rule.Action
            profile=[string]$rule.Profile; edge=[string]$rule.EdgeTraversalPolicy; protocol=[string]$port[0].Protocol
            local_port=@($port[0].LocalPort | ForEach-Object {[string]$_}); remote_port=@($port[0].RemotePort | ForEach-Object {[string]$_})
            local_address=@($address[0].LocalAddress | ForEach-Object {[string]$_}); remote_address=@($address[0].RemoteAddress | ForEach-Object {[string]$_})
            interface_type=@($type[0].InterfaceType | ForEach-Object {[string]$_}); interface_alias=@($interface[0].InterfaceAlias | ForEach-Object {[string]$_})
            service=[string]$service[0].Service; package=[string]$application[0].Package
            authentication=[string]$security[0].Authentication; encryption=[string]$security[0].Encryption
            override_block=[bool]$security[0].OverrideBlockRules; local_user=[string]$security[0].LocalUser
            remote_user=[string]$security[0].RemoteUser; remote_machine=[string]$security[0].RemoteMachine
            dynamic_target=[string]$port[0].DynamicTarget
            loose_source_mapping=[bool]$rule.LooseSourceMapping; local_only_mapping=[bool]$rule.LocalOnlyMapping
        }
    }
    [Console]::Out.Write((@{available=$true;rules=@($rows)} | ConvertTo-Json -Depth 5 -Compress))
} catch {
    # Never emit native error messages, foreign rule metadata, or machine/user identifiers.
    [Console]::Out.Write('{"available":false,"rules":[]}')
}
"#;

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub rule: RuleEvidence,
    pub checked: Option<Instant>,
}

pub(crate) struct Watch {
    snapshot: Arc<Mutex<Snapshot>>,
    demand: SyncSender<()>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Watch {
    pub(crate) fn new() -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (demand, requests) = mpsc::sync_channel(1);
        // Agent unit fixtures never launch PowerShell or query the owner's rule store.
        #[cfg(test)]
        let thread = {
            drop(requests);
            None
        };
        #[cfg(not(test))]
        let thread = {
            let state = Arc::clone(&snapshot);
            let stopping = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("crosspane-firewall".into())
                .spawn(move || {
                    let mut last_started = None;
                    // Exactly one startup query. Subsequent work requires a status request and spacing.
                    loop {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let started = Instant::now();
                        if admit_query(&mut last_started, started) {
                            let rule = query(&stopping);
                            if let Ok(mut snapshot) = state.lock() {
                                *snapshot = Snapshot {
                                    rule,
                                    checked: Some(Instant::now()),
                                };
                            }
                        }
                        // No periodic query: the timeout checks shutdown only.
                        loop {
                            if stopping.load(Ordering::Acquire) {
                                return;
                            }
                            match requests.recv_timeout(POLL) {
                                Ok(()) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                        }
                    }
                })
                .ok()
        };
        Self {
            snapshot,
            demand,
            stop,
            thread,
        }
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }
    /// Nonblocking/coalesced; the worker owns both query admission and the child.
    pub(crate) fn demand(&self) {
        let _ = self.demand.try_send(());
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.demand.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Retains the exact spawned process handle. Every path settles it before pipe-reader join.
struct OwnedChild(Child);
impl OwnedChild {
    fn settle(&mut self) -> bool {
        // Child owns PROCESS_TERMINATE/SYNCHRONIZE on Windows. Never discover/kill by PID.
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        self.0.wait().is_ok()
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryOutput {
    available: bool,
    rules: Vec<FirewallRule>,
}

fn read_bounded(mut stdout: impl Read) -> IoResult<Vec<u8>> {
    let mut bytes = Vec::new();
    // Close an oversized pipe instead of allocating/draining indefinitely. The owner then
    // settles the child. Stderr is null, so it cannot block or expose query diagnostics.
    stdout
        .by_ref()
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn admit_query(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < QUERY_SPACING) {
        return false;
    }
    *last = Some(now);
    true
}
struct RunOutput {
    bytes: Option<Vec<u8>>,
    timed_out: bool,
    reaped: bool,
}

/// The production fixed query and a test-only static sleep use the same private runner. There
/// is no config, command or environment selector for the script in shipping code.
fn run_owned(script: &str, stop: &AtomicBool) -> Option<RunOutput> {
    let started = Instant::now();
    let system_root = std::env::var_os("SystemRoot")?;
    let executable = std::path::PathBuf::from(system_root)
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if !executable.is_absolute() || stop.load(Ordering::Acquire) {
        return None;
    }
    let child = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut child = OwnedChild(child);
    let stdout = child.0.stdout.take()?;
    let reader = std::thread::Builder::new()
        .name("crosspane-firewall-output".into())
        .spawn(move || read_bounded(stdout))
        .ok()?;
    let mut timed_out = false;
    let status = loop {
        if stop.load(Ordering::Acquire) {
            break None;
        }
        if started.elapsed() >= QUERY_WAIT {
            timed_out = true;
            break None;
        }
        match child.0.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => break None,
        }
    };
    // Timeout/error/shutdown forcibly terminates and reaps the retained child before joining
    // its bounded output reader. Success is reaped too. No late work outlives this query.
    let reaped = child.settle();
    let bytes = reader.join().ok().and_then(Result::ok).filter(|bytes| {
        reaped && status.is_some_and(|status| status.success()) && bytes.len() as u64 <= MAX_OUTPUT
    });
    Some(RunOutput {
        bytes,
        timed_out,
        reaped,
    })
}
fn query(stop: &AtomicBool) -> RuleEvidence {
    let run = || -> Option<RuleEvidence> {
        let program = std::env::current_exe().ok()?.canonicalize().ok()?;
        let program = program.to_str()?;
        let result = run_owned(QUERY, stop)?;
        if result.timed_out || !result.reaped {
            return None;
        }
        let output: QueryOutput = serde_json::from_slice(&result.bytes?).ok()?;
        if !output.available {
            return None;
        }
        Some(rule_evidence(program, &output.rules))
    };
    run().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_is_bounded_without_child_or_network() {
        assert_eq!(read_bounded(&b"fixture"[..]).unwrap(), b"fixture");
        assert_eq!(
            read_bounded(std::io::repeat(b'x')).unwrap().len() as u64,
            MAX_OUTPUT + 1
        );
        assert!(
            serde_json::from_slice::<QueryOutput>(
                b"{\"available\":true,\"rules\":[],\"unexpected\":true}"
            )
            .is_err()
        );
    }
    #[test]
    fn inert_watch_has_unverified_unavailable_facts_and_no_spawn() {
        let watch = Watch::new();
        watch.demand();
        assert_eq!(watch.snapshot().rule, RuleEvidence::Unavailable);
        assert!(watch.snapshot().checked.is_none());
        assert!(watch.thread.is_none());
        // Exercise constant/static query coverage without executing it.
        assert_eq!(QUERY_WAIT.as_secs(), 5);
        assert_eq!(QUERY_SPACING.as_secs(), 60);
        assert!(QUERY.contains("-DisplayName 'Crosspane.Agent.UDP.Private.*'"));
        assert!(QUERY.contains("name=[string]$rule.DisplayName;"));
        assert!(!QUERY.contains("-Name 'Crosspane.Agent.UDP.Private.*'"));
        // Referencing the production function verifies its compilation; it is never called.
        let _query: fn(&AtomicBool) -> RuleEvidence = query;
    }
    #[test]
    fn status_demand_is_coalesced_and_rate_limited() {
        let now = Instant::now();
        let mut last = None;
        assert!(admit_query(&mut last, now));
        assert!(!admit_query(&mut last, now + Duration::from_secs(59)));
        assert!(!admit_query(&mut last, now));
        assert!(admit_query(&mut last, now + Duration::from_secs(60)));
        assert!(!admit_query(&mut last, now + Duration::from_secs(60)));
    }
    fn limited_opt_in() {
        assert_eq!(
            std::env::var("CROSSPANE_W18_REACHABILITY").as_deref(),
            Ok("1")
        );
        assert!(
            !crate::windows::security::is_elevated().unwrap(),
            "Limited token required"
        );
    }
    #[test]
    #[ignore = "Limited owned PowerShell only; W1.8 explicit opt-in after held review"]
    fn owned_query_timeout_kills_and_reaps() {
        limited_opt_in();
        let started = Instant::now();
        let result = run_owned(
            "Start-Sleep -Seconds 30; [Console]::Out.Write('late')",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result.timed_out && result.reaped && result.bytes.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "owned timeout did not settle promptly"
        );
        eprintln!("owned_query_timeout timed_out=true reaped=true late_output=false");
    }
    #[test]
    #[ignore = "Limited read-only Crosspane rule query only; W1.8 explicit opt-in after held review"]
    fn owned_crosspane_rule_read_only_probe() {
        limited_opt_in();
        let rule = query(&AtomicBool::new(false));
        eprintln!("owned_crosspane_rule_query evidence={}", rule.token());
    }
}
