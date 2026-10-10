#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::platform::linux::{firewall::*, native_io::*};
use crosspane_installer_core::OperationId;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn cidr(value: &str) -> LanCidr {
    LanCidr::parse(value).unwrap()
}
fn rule_file(network: &str, kind: RuleKind, comment: Option<&str>) -> Vec<u8> {
    let v6 = network.contains(':');
    let (port, emitted) = match kind {
        RuleKind::Lan => ("47811:47812", "-m multiport --dports 47811:47812"),
        RuleKind::Mdns => ("5353", "--dport 5353"),
    };
    let comment = comment
        .map(|c| {
            format!(
                " comment={}",
                c.bytes().map(|b| format!("{b:02x}")).collect::<String>()
            )
        })
        .unwrap_or_default();
    format!("*filter\n:ufw-user-input - [0:0]\n### RULES ###\n### tuple ### allow udp {port} {} any {network} in{comment}\n-A {}-user-input -p udp {emitted} -s {network} -j ACCEPT\n### END RULES ###\nCOMMIT\n",
        if v6 { "::/0" } else { "0.0.0.0/0" }, if v6 { "ufw6" } else { "ufw" }).into_bytes()
}
fn empty_rules() -> Vec<u8> {
    b"*filter\n### RULES ###\n### END RULES ###\nCOMMIT\n".to_vec()
}
fn link(interface: &str, address: &str, bits: u32) -> Value {
    json!({"ifname":interface,"flags":["UP","LOWER_UP"],"link_type":"ether",
        "addr_info":[{"family":if address.contains(':') {"inet6"} else {"inet"},"scope":"global","local":address,"prefixlen":bits}]})
}
fn links_bytes(links: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&links).unwrap()
}
struct Reads {
    config: Vec<u8>,
    unit: (Option<i32>, Vec<u8>, Vec<u8>),
    ipv4: Result<Vec<u8>, NativeError>,
    ipv6: Result<Vec<u8>, NativeError>,
    addresses: Vec<u8>,
    routes4: Vec<u8>,
    routes6: Vec<u8>,
    calls: Mutex<Vec<String>>,
}
impl Default for Reads {
    fn default() -> Self {
        Self {
            config: b"ENABLED=yes\n".to_vec(),
            unit: (Some(0), b"active\n".to_vec(), vec![]),
            ipv4: Ok(empty_rules()),
            ipv6: Ok(empty_rules()),
            addresses: links_bytes(vec![link("enp1s0", "192.168.4.31", 24)]),
            routes4: br#"[{"dst":"default","dev":"enp1s0"}]"#.to_vec(),
            routes6: b"[]".to_vec(),
            calls: Mutex::new(vec![]),
        }
    }
}
impl FirewallReader for Reads {
    fn file(&self, request: SystemRead, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        deadline.check()?;
        self.calls.lock().unwrap().push(format!("file:{request:?}"));
        match request {
            SystemRead::UfwConfig => Ok(self.config.clone()),
            SystemRead::UfwRules => self.ipv4.clone(),
            SystemRead::UfwRules6 => self.ipv6.clone(),
            _ => panic!("unapproved file"),
        }
    }
    fn command(
        &self,
        request: FirewallRead,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        deadline.check()?;
        self.calls.lock().unwrap().push(format!("read:{request:?}"));
        let (code, stdout, stderr) = match request {
            FirewallRead::Activity => self.unit.clone(),
            FirewallRead::Addresses => (Some(0), self.addresses.clone(), vec![]),
            FirewallRead::Default4 => (Some(0), self.routes4.clone(), vec![]),
            FirewallRead::Default6 => (Some(0), self.routes6.clone(), vec![]),
        };
        Ok(CommandOutput {
            code,
            stdout,
            stderr,
        })
    }
}
#[test]
fn activity_requires_both_enabled_config_and_exact_active_unit_without_privileged_reads() {
    for (config, unit, expected) in [
        ("ENABLED=yes\n", (Some(0), "active\n"), Activity::Active),
        ("ENABLED=no\n", (Some(0), "active\n"), Activity::Inactive),
        ("ENABLED=yes\n", (Some(3), "inactive\n"), Activity::Inactive),
        ("ENABLED=yes\n", (None, "active\n"), Activity::Unknown),
        (
            "ENABLED=yes\n",
            (Some(0), "activating\n"),
            Activity::Unknown,
        ),
        ("ENABLED=maybe\n", (Some(0), "active\n"), Activity::Unknown),
        (
            "ENABLED=yes\nENABLED=no\n",
            (Some(0), "active\n"),
            Activity::Unknown,
        ),
        ("", (Some(0), "active\n"), Activity::Unknown),
    ] {
        let reader = Reads {
            config: config.as_bytes().to_vec(),
            unit: (unit.0, unit.1.as_bytes().to_vec(), vec![]),
            ..Reads::default()
        };
        let facts = observe(&reader, ManagerSelection::Ufw, &deadline()).unwrap();
        assert_eq!(facts.activity, expected);
        assert_eq!(
            *reader.calls.lock().unwrap(),
            [
                "file:UfwConfig",
                "read:Activity",
                "file:UfwRules",
                "file:UfwRules6",
                "read:Addresses",
                "read:Default4",
                "read:Default6"
            ]
        );
    }
    for manager in [
        ManagerSelection::Manual,
        ManagerSelection::Multiple,
        ManagerSelection::Unknown,
    ] {
        assert_eq!(
            observe(&Reads::default(), manager, &deadline())
                .unwrap()
                .manager,
            manager
        );
    }
}
#[test]
fn unreadable_or_malformed_ipv4_ipv6_inventory_is_unknown_never_absent() {
    for error in [
        NativeError::PermissionDenied,
        NativeError::Unavailable,
        NativeError::Foreign,
        NativeError::Timeout,
    ] {
        let reader = Reads {
            ipv4: Err(error),
            ipv6: Err(error),
            ..Reads::default()
        };
        let facts = observe(&reader, ManagerSelection::Ufw, &deadline()).unwrap();
        for network in ["192.168.4.0/24", "fd00::/16"] {
            assert_eq!(
                facts.presence(&cidr(network), RuleKind::Lan),
                Presence::Unknown(InspectionIssue::Unreadable(error))
            );
        }
    }
    for bytes in [
        b"garbage".to_vec(),
        vec![b'x'; MAX_UFW_BYTES + 1],
        b"*filter\nCOMMIT\n\0".to_vec(),
    ] {
        let reader = Reads {
            ipv4: Ok(bytes),
            ..Reads::default()
        };
        assert!(matches!(
            observe(&reader, ManagerSelection::Ufw, &deadline())
                .unwrap()
                .presence(&cidr("192.168.4.0/24"), RuleKind::Lan),
            Presence::Unknown(_)
        ));
    }
}
#[test]
fn supported_ipv4_ipv6_full_specs_require_exact_comment_and_matching_emitted_rule() {
    for (network, v6) in [("192.168.4.0/24", false), ("FD00:0000::/16", true)] {
        for (kind, comment) in [
            (RuleKind::Lan, "Crosspane (LAN)"),
            (RuleKind::Mdns, "Crosspane (mDNS)"),
        ] {
            let bytes = rule_file(network, kind, Some(comment));
            let parsed = parse_rules(&bytes, v6).unwrap();
            assert_eq!(parsed.rules.len(), 1);
            assert!(!parsed.uncertain);
            assert_eq!(parsed.rules[0].cidr, cidr(network));
            assert_eq!(parsed.rules[0].kind, kind);
            assert_eq!(parsed.rules[0].comment.as_deref(), Some(comment));
            let text = String::from_utf8(bytes).unwrap();
            for changed in [
                text.replace("-j ACCEPT", "-j DROP"),
                text.replace("-p udp", "-p tcp"),
                text.replace(" comment=", " evil="),
            ] {
                assert_eq!(
                    parse_rules(changed.as_bytes(), v6),
                    Err(InspectionIssue::Malformed)
                );
            }
        }
    }
}
#[test]
fn duplicate_equivalent_and_modified_comments_are_kept_without_retagging() {
    let mut reader = Reads {
        ipv4: Ok(rule_file("192.168.4.0/24", RuleKind::Lan, None)),
        ..Reads::default()
    };
    let presence = |reader: &Reads| {
        observe(reader, ManagerSelection::Ufw, &deadline())
            .unwrap()
            .presence(&cidr("192.168.4.0/24"), RuleKind::Lan)
    };
    assert_eq!(presence(&reader), Presence::Equivalent);
    reader.ipv4 = Ok(rule_file(
        "192.168.4.0/24",
        RuleKind::Lan,
        Some("Administrator's LAN"),
    ));
    assert_eq!(presence(&reader), Presence::Equivalent);
    reader.ipv4 = Ok(rule_file(
        "192.168.5.0/24",
        RuleKind::Lan,
        Some("Crosspane (LAN)"),
    ));
    assert_eq!(presence(&reader), Presence::Modified);
    let single = String::from_utf8(rule_file(
        "192.168.4.0/24",
        RuleKind::Lan,
        Some("Crosspane (LAN)"),
    ))
    .unwrap();
    let body = single
        .split("### RULES ###\n")
        .nth(1)
        .unwrap()
        .split("### END RULES ###")
        .next()
        .unwrap();
    reader.ipv4 = Ok(single
        .replace("### END RULES ###", &format!("{body}### END RULES ###"))
        .into_bytes());
    assert_eq!(presence(&reader), Presence::Modified);
}
#[test]
fn comment_hex_controls_non_ascii_and_orphan_rules_fail_closed_without_panicking() {
    let valid = String::from_utf8(rule_file(
        "192.168.4.0/24",
        RuleKind::Lan,
        Some("Crosspane (LAN)"),
    ))
    .unwrap();
    let original = valid
        .split("comment=")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap();
    for hex in ["0", "gg", "0a", "c3a9", "éé"] {
        let result = std::panic::catch_unwind(|| {
            parse_rules(valid.replace(original, hex).as_bytes(), false)
        });
        assert!(result.is_ok(), "{hex}");
        assert_eq!(result.unwrap(), Err(InspectionIssue::Malformed));
    }
    assert_eq!(parse_rules(b"*filter\n### RULES ###\n-A ufw-user-input -p udp --dport 5353 -j ACCEPT\n### END RULES ###\nCOMMIT\n",false),Err(InspectionIssue::Malformed));
}
#[test]
fn physical_links_normalize_families_and_prefer_default_routes_without_widening() {
    let addresses = links_bytes(vec![
        link("enp1s0", "192.168.4.31", 24),
        link("wlan0", "192.168.5.99", 24),
        link("enp1s0", "fd12:3456::abcd", 64),
    ]);
    let links = parse_links(&addresses, br#"[{"dst":"default","dev":"wlan0"}]"#, b"[]").unwrap();
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].cidr.as_str(), "192.168.4.0/24");
    assert_eq!(links[2].cidr.as_str(), "fd12:3456::/64");
    assert!(!links[0].default_route);
    assert!(links[1].default_route);
}
#[test]
fn vpn_container_loopback_down_non_global_and_broad_links_are_not_candidates() {
    let mut rows = Vec::new();
    for kind in ["bridge", "veth", "tun", "wireguard", "vlan"] {
        let mut row = link("virtual0", "10.1.2.3", 24);
        row["linkinfo"] = json!({"info_kind":kind});
        rows.push(row);
    }
    for flags in [
        json!(["UP"]),
        json!(["LOWER_UP"]),
        json!(["UP", "LOWER_UP", "LOOPBACK"]),
    ] {
        let mut row = link("lo", "127.0.0.1", 8);
        row["flags"] = flags;
        rows.push(row);
    }
    for (address, bits) in [
        ("169.254.4.1", 16),
        ("224.1.1.1", 24),
        ("10.1.1.1", 0),
        ("fe80::123", 64),
        ("ff02::1", 16),
    ] {
        rows.push(link("virtual0", address, bits));
    }
    let mut row = link("enp1s0", "192.168.4.31", 24);
    row["addr_info"][0]["scope"] = json!("link");
    rows.push(row);
    let mut row = link("enp1s0", "192.168.4.31", 24);
    row["link_type"] = json!("none");
    rows.push(row);
    assert!(
        parse_links(&links_bytes(rows), b"[]", b"[]")
            .unwrap()
            .is_empty()
    );
}
#[test]
fn malformed_oversized_network_and_cancelled_observation_stay_pending() {
    for bytes in [
        b"{}".to_vec(),
        b"[null]".to_vec(),
        vec![b'x'; MAX_FIREWALL_COMMAND_BYTES + 1],
        links_bytes(vec![link("bad\nname", "192.168.4.31", 24)]),
    ] {
        assert!(parse_links(&bytes, b"[]", b"[]").is_err());
    }
    assert!(
        parse_links(
            &links_bytes(vec![link("enp1s0", "192.168.4.31", 24)]),
            br#"[{"dev":"enp1s0"}]"#,
            b"[]"
        )
        .is_err()
    );
    let cancellation = Cancellation::default();
    cancellation.cancel();
    assert_eq!(
        observe(
            &Reads::default(),
            ManagerSelection::Ufw,
            &Deadline::new(5000, cancellation).unwrap()
        )
        .unwrap_err(),
        FirewallError::Native(NativeError::Cancelled)
    );
}

static ROOTS: AtomicU64 = AtomicU64::new(1);
static MUTATIONS: Mutex<()> = Mutex::new(());
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
type Hook = Box<dyn FnOnce() + Send>;
struct SharedReads(Arc<Mutex<Reads>>);
impl FirewallReader for SharedReads {
    fn file(&self, r: SystemRead, d: &Deadline) -> Result<Vec<u8>, NativeError> {
        self.0.lock().unwrap().file(r, d)
    }
    fn command(&self, r: FirewallRead, d: &Deadline) -> Result<CommandOutput, NativeError> {
        self.0.lock().unwrap().command(r, d)
    }
}
#[derive(Default)]
struct NativeFake {
    executable: Mutex<PathBuf>,
    calls: Mutex<Vec<CommandSpec>>,
}
impl CommandRunner for NativeFake {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        self.calls.lock().unwrap().push(c.clone());
        let stdout = if c.executable() == std::path::Path::new("/bin/ps") {
            if c.argv()[1] == "lstart=" {
                START.to_vec()
            } else {
                b"crosspane-agent\n".to_vec()
            }
        } else if c.executable() == std::path::Path::new("/usr/bin/systemctl") {
            b"active\n".to_vec()
        } else {
            assert_eq!(c.executable(), std::path::Path::new("/usr/bin/ip"));
            b"[]".to_vec()
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: vec![],
        })
    }
}
impl ProcessProbe for NativeFake {
    fn snapshot(&self, pid: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        assert_eq!(pid, 4242);
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: self.executable.lock().unwrap().clone(),
            generation: 8,
        })
    }
}
#[derive(Default)]
struct Auth {
    calls: Mutex<Vec<PkexecCommand>>,
    outcomes: Mutex<VecDeque<PkexecOutcome>>,
    hook: Mutex<Option<Hook>>,
    stall: AtomicBool,
    reaped: AtomicBool,
    dropped: AtomicBool,
    order: Mutex<Vec<&'static str>>,
    terms: AtomicU64,
}
struct AuthRunner(Arc<Auth>);
struct AuthChild {
    state: Arc<Auth>,
    outcome: Option<PkexecOutcome>,
}
impl PkexecRunner for AuthRunner {
    fn spawn(&self, c: &PkexecCommand, d: &Deadline) -> Result<Box<dyn PkexecChild>, NativeError> {
        d.check()?;
        self.0.calls.lock().unwrap().push(c.clone());
        self.0.order.lock().unwrap().push("spawn");
        self.0.dropped.store(false, Ordering::Release);
        self.0.reaped.store(false, Ordering::Release);
        if let Some(hook) = self.0.hook.lock().unwrap().take() {
            hook();
        }
        let outcome = self
            .0
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| exited(0, "", ""));
        Ok(Box::new(AuthChild {
            state: self.0.clone(),
            outcome: Some(outcome),
        }))
    }
}
impl PkexecChild for AuthChild {
    fn poll(&mut self) -> Result<Option<PkexecOutcome>, NativeError> {
        if self.state.stall.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.state.reaped.store(true, Ordering::Release);
        Ok(self.outcome.take())
    }
    fn terminate(&mut self) {
        self.state.terms.fetch_add(1, Ordering::AcqRel);
        self.state.reaped.store(true, Ordering::Release);
    }
    fn reaped(&mut self) -> bool {
        self.state.reaped.load(Ordering::Acquire)
    }
}
impl Drop for AuthChild {
    fn drop(&mut self) {
        self.state.dropped.store(true, Ordering::Release);
    }
}
struct Store {
    auth: Arc<Auth>,
    intents: Vec<FirewallIntent>,
    results: Vec<RuleResult>,
    fail_intent: bool,
    fail_outcome: bool,
    hook: Option<Hook>,
}
impl IntentStore for Store {
    fn record_intent(&mut self, _: &SupportProof, i: &FirewallIntent) -> Result<(), NativeError> {
        self.auth.order.lock().unwrap().push("intent");
        if self.fail_intent {
            return Err(NativeError::Unavailable);
        }
        self.intents.push(i.clone());
        if let Some(hook) = self.hook.take() {
            hook();
        }
        Ok(())
    }
    fn record_outcome(
        &mut self,
        _: &SupportProof,
        _: &FirewallIntent,
        o: RuleResult,
    ) -> Result<(), NativeError> {
        self.auth.order.lock().unwrap().push("outcome");
        self.results.push(o);
        if self.fail_outcome {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
}
struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    native: Arc<NativeFake>,
    reads: Arc<Mutex<Reads>>,
    auth: Arc<Auth>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/crosspane-fw-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let native = Arc::new(NativeFake::default());
        let auth = Arc::new(Auth::default());
        let mut io = LinuxNativeIo::scratch(&root, native.clone(), native.clone()).unwrap();
        *native.executable.lock().unwrap() = io.target().agent_path();
        io.set_scratch_pkexec_runner(Arc::new(AuthRunner(auth.clone())))
            .unwrap();
        Self {
            root,
            io: Arc::new(io),
            native,
            reads: Arc::new(Mutex::new(Reads::default())),
            auth,
        }
    }
    fn facts(&self) -> SupportObservations {
        SupportObservations {
            uid: self.io.target().paths().uid,
            desktop: crosspane_installer::platform::linux::detect::Desktop::Hyprland,
            architecture: std::env::consts::ARCH.into(),
            arch_based: true,
            compositor_version: [0, 56, 0],
            protocols_ready: true,
            runtime_libraries_ready: true,
            compositor_managed: true,
            graphical_target_active: true,
            graphical_sessions: 1,
            session_id: "scratch".into(),
            session_type: "wayland".into(),
            seat: "seat0".into(),
            active: true,
        }
    }
    fn proof(&self) -> SupportProof {
        self.io.scratch_support(self.facts()).unwrap()
    }
    fn firewall(&self) -> LinuxFirewall {
        LinuxFirewall::scratch(self.io.clone(), Arc::new(SharedReads(self.reads.clone()))).unwrap()
    }
    fn store(&self) -> Store {
        Store {
            auth: self.auth.clone(),
            intents: vec![],
            results: vec![],
            fail_intent: false,
            fail_outcome: false,
            hook: None,
        }
    }
    fn wait(&self) {
        let end = Instant::now() + Duration::from_secs(2);
        while !self.auth.dropped.load(Ordering::Acquire) {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn request(kind: RuleKind, op: u64, revision: u64) -> PlanRequest {
    PlanRequest {
        operation: OperationId(op),
        revision,
        kind,
        selected: None,
        ports: [47811, 47812],
    }
}
fn exited(code: i32, stdout: &str, stderr: &str) -> PkexecOutcome {
    PkexecOutcome::Exited {
        code,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}
#[test]
fn add_plan_exact_preview_argv_and_intent_before_dispatch_never_verify_networking() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    let mut fw = f.firewall();
    let mut store = f.store();
    let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = fw.plan(&snapshot, request(RuleKind::Lan, 3, 7)).unwrap();
    assert!(plan.preview().starts_with("pkexec /usr/bin/ufw allow from 192.168.4.0/24 to any port 47811:47812 proto udp comment 'Crosspane (LAN)'\n"));
    assert!(plan.preview().contains("no interface restriction"));
    assert!(!plan.preview().contains("sudo"));
    let consent = plan.consent(OperationId(3), 7).unwrap();
    let result = fw
        .apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &deadline(),
        )
        .unwrap();
    f.wait();
    assert_eq!(result.result, RuleResult::PendingVerification);
    assert_eq!(result.inventory, Presence::Absent);
    assert!(result.manual.is_none());
    assert_eq!(
        *f.auth.order.lock().unwrap(),
        ["intent", "spawn", "outcome"]
    );
    let calls = f.auth.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].argv(),
        [
            "--wait",
            "/usr/bin/pkexec",
            "/usr/bin/ufw",
            "allow",
            "from",
            "192.168.4.0/24",
            "to",
            "any",
            "port",
            "47811:47812",
            "proto",
            "udp",
            "comment",
            "Crosspane (LAN)"
        ]
    );
    assert_eq!(store.intents[0].operation, OperationId(3));
    assert_eq!(store.intents[0].revision, 7);
    assert_eq!(store.intents[0].target, *f.io.target().paths());
}
#[test]
fn exact_or_equivalent_administrator_rules_skip_add_without_retagging_or_intent() {
    let _serial = MUTATIONS.lock().unwrap();
    for comment in [None, Some("Administrator rule"), Some("Crosspane (LAN)")] {
        let f = Fixture::new();
        f.reads.lock().unwrap().ipv4 = Ok(rule_file("192.168.4.0/24", RuleKind::Lan, comment));
        let mut fw = f.firewall();
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let mut store = f.store();
        assert_eq!(
            fw.apply(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                consent,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::Kept
        );
        assert!(f.auth.calls.lock().unwrap().is_empty());
        assert!(store.intents.is_empty());
    }
}
#[test]
fn stale_operation_revision_replanning_and_foreign_snapshots_authorize_zero_mutations() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    let mut fw = f.firewall();
    let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
    assert_eq!(
        plan.consent(OperationId(2), 1).unwrap_err(),
        FirewallError::Stale
    );
    assert_eq!(
        plan.consent(OperationId(1), 2).unwrap_err(),
        FirewallError::Stale
    );
    let consent = plan.consent(OperationId(1), 1).unwrap();
    let next = fw.plan(&s, request(RuleKind::Lan, 2, 2)).unwrap();
    let mut store = f.store();
    assert_eq!(
        fw.apply(
            &f.proof(),
            ManagerSelection::Ufw,
            next,
            consent,
            &mut store,
            &deadline()
        )
        .unwrap_err(),
        FirewallError::Stale
    );
    assert!(f.auth.calls.lock().unwrap().is_empty());
    assert!(store.intents.is_empty());
    let mut other = f.firewall();
    other.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(
        other.plan(&s, request(RuleKind::Lan, 3, 3)).unwrap_err(),
        FirewallError::Stale
    );
}
#[test]
fn consent_from_another_controller_with_identical_numbers_refuses_dispatch() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    let mut first = f.firewall();
    let mut second = f.firewall();
    let a = first.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let b = second.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = first.plan(&a, request(RuleKind::Lan, 7, 7)).unwrap();
    let foreign = second.plan(&b, request(RuleKind::Lan, 7, 7)).unwrap();
    let mut store = f.store();
    assert_eq!(
        first
            .apply(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                foreign.consent(OperationId(7), 7).unwrap(),
                &mut store,
                &deadline()
            )
            .unwrap_err(),
        FirewallError::Stale
    );
    assert!(store.intents.is_empty());
    assert!(f.auth.calls.lock().unwrap().is_empty());
}
#[test]
fn changed_link_backend_and_post_intent_facts_refuse_dispatch() {
    let _serial = MUTATIONS.lock().unwrap();
    for point in 0..3 {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let mut store = f.store();
        let mutate = {
            let reads = f.reads.clone();
            move || {
                reads.lock().unwrap().addresses =
                    links_bytes(vec![link("enp1s0", "192.168.8.4", 24)]);
            }
        };
        if point == 0 {
            mutate();
        } else if point == 2 {
            store.hook = Some(Box::new(mutate));
        }
        assert_eq!(
            fw.apply(
                &f.proof(),
                if point == 1 {
                    ManagerSelection::Manual
                } else {
                    ManagerSelection::Ufw
                },
                plan,
                consent,
                &mut store,
                &deadline()
            )
            .unwrap_err(),
            FirewallError::Changed
        );
        assert!(f.auth.calls.lock().unwrap().is_empty());
        assert_eq!(store.intents.len(), usize::from(point == 2));
    }
}
#[test]
fn intent_failure_refuses_and_outcome_publication_failure_is_unknown_without_resend() {
    let _serial = MUTATIONS.lock().unwrap();
    for fail_before in [true, false] {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let mut store = f.store();
        store.fail_intent = fail_before;
        store.fail_outcome = !fail_before;
        let result = fw.apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &deadline(),
        );
        if fail_before {
            assert_eq!(
                result.unwrap_err(),
                FirewallError::Native(NativeError::Unavailable)
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        } else {
            assert_eq!(result.unwrap().result, RuleResult::OutcomeUnknown);
            f.wait();
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
        }
        assert_eq!(
            fw.plan(&s, request(RuleKind::Lan, 2, 2)).unwrap_err(),
            if fail_before {
                FirewallError::Stale
            } else {
                FirewallError::CurrentRequired
            }
        );
    }
}
#[test]
fn prompt_unavailable_literals_are_exact_and_all_other_failures_stay_unknown() {
    for text in [
        "Error creating textual authentication agent: No controlling terminal found",
        "Error executing command as another user: No authentication agent found.",
    ] {
        for suffix in ["", "\n"] {
            assert_eq!(
                classify_result(Ok(exited(127, "", &format!("{text}{suffix}"))), false),
                RuleResult::PromptUnavailable
            );
        }
        for (code, out, err) in [
            (126, "", text),
            (127, "extra", text),
            (127, "", &format!("{text}\nextra")),
            (127, "", &format!("{text}\n\n")),
            (127, "", &format!("{text}\0")),
            (127, "", &format!("{text}\r\n")),
        ] {
            assert_eq!(
                classify_result(Ok(exited(code, out, err)), false),
                RuleResult::OutcomeUnknown
            );
        }
    }
    for text in [
        "Request dismissed",
        "Not authorized",
        "Error creating textual authentication agent:",
        "Could not delete non-existent rule",
    ] {
        assert_eq!(
            classify_result(Ok(exited(127, "", text)), false),
            RuleResult::OutcomeUnknown
        );
    }
    for outcome in [
        Ok(PkexecOutcome::TimedOut),
        Ok(PkexecOutcome::Signalled),
        Err(NativeError::Timeout),
        Err(NativeError::Busy),
    ] {
        assert_eq!(classify_result(outcome, false), RuleResult::OutcomeUnknown);
    }
    for truncated in [true, false] {
        let out = PkexecOutcome::Exited {
            code: 127,
            stdout: vec![],
            stderr: vec![b'x'; MAX_COMMAND_BYTES + usize::from(!truncated)],
            stdout_truncated: false,
            stderr_truncated: truncated,
        };
        assert_eq!(classify_result(Ok(out), false), RuleResult::OutcomeUnknown);
    }
}
#[test]
fn nonexistent_delete_literal_is_absent_only_for_exact_bounded_combined_output() {
    for message in [
        "Could not delete non-existent rule",
        "Could not delete non-existent rule\n",
    ] {
        assert_eq!(
            classify_result(Ok(exited(1, message, "")), true),
            RuleResult::Absent
        );
        assert_eq!(
            classify_result(Ok(exited(1, "", message)), true),
            RuleResult::Absent
        );
        assert_eq!(
            classify_result(Ok(exited(1, message, "extra")), true),
            RuleResult::OutcomeUnknown
        );
        assert_eq!(
            classify_result(Ok(exited(1, &format!(" {message}"), "")), true),
            RuleResult::OutcomeUnknown
        );
        assert_eq!(
            classify_result(Ok(exited(1, message, "")), false),
            RuleResult::OutcomeUnknown
        );
    }
}
#[test]
fn prompt_unavailable_returns_inert_manual_guidance_and_requires_new_detection() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    f.auth.outcomes.lock().unwrap().push_back(exited(
        127,
        "",
        "Error creating textual authentication agent: No controlling terminal found\n",
    ));
    let mut fw = f.firewall();
    let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
    let consent = plan.consent(OperationId(1), 1).unwrap();
    let mut store = f.store();
    let result = fw
        .apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &deadline(),
        )
        .unwrap();
    f.wait();
    assert_eq!(result.result, RuleResult::PromptUnavailable);
    assert_eq!(
        result.manual.unwrap(),
        "sudo /usr/bin/ufw allow from 192.168.4.0/24 to any port 47811:47812 proto udp comment 'Crosspane (LAN)'"
    );
    assert_eq!(
        fw.plan(&s, request(RuleKind::Lan, 2, 2)).unwrap_err(),
        FirewallError::CurrentRequired
    );
    assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
    let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(
        fw.plan(&fresh, request(RuleKind::Lan, 2, 2)).unwrap_err(),
        FirewallError::CurrentRequired
    );
    assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
}

#[test]
fn timeout_after_apply_is_unknown_and_explicit_redetection_preserves_actual_rule() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    f.auth.stall.store(true, Ordering::Release);
    let reads = f.reads.clone();
    *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
        reads.lock().unwrap().ipv4 = Ok(rule_file(
            "192.168.4.0/24",
            RuleKind::Lan,
            Some("Crosspane (LAN)"),
        ))
    }));
    let mut fw = f.firewall();
    let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
    let consent = plan.consent(OperationId(1), 1).unwrap();
    let mut store = f.store();
    let result = fw
        .apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &Deadline::new(20, Cancellation::default()).unwrap(),
        )
        .unwrap();
    f.wait();
    assert_eq!(result.result, RuleResult::OutcomeUnknown);
    assert!(matches!(result.inventory, Presence::Unknown(_)));
    assert_eq!(f.auth.terms.load(Ordering::Acquire), 1);
    assert_eq!(
        fw.plan(&s, request(RuleKind::Lan, 2, 2)).unwrap_err(),
        FirewallError::CurrentRequired
    );
    assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
    let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(
        fresh
            .facts()
            .presence(&cidr("192.168.4.0/24"), RuleKind::Lan),
        Presence::Owned
    );
    assert_eq!(
        fw.plan(&fresh, request(RuleKind::Lan, 2, 2)).unwrap_err(),
        FirewallError::CurrentRequired
    );
    assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
}
#[test]
fn dismissed_prompt_or_changed_authorization_state_cannot_make_networking_verified() {
    let _serial = MUTATIONS.lock().unwrap();
    for dismissed in [true, false] {
        let f = Fixture::new();
        if dismissed {
            f.auth
                .outcomes
                .lock()
                .unwrap()
                .push_back(exited(126, "", "Request dismissed\n"));
        }
        let reads = f.reads.clone();
        *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
            reads.lock().unwrap().addresses = links_bytes(vec![link("enp1s0", "192.168.8.2", 24)])
        }));
        let mut fw = f.firewall();
        let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let mut store = f.store();
        assert_eq!(
            fw.apply(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                consent,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::OutcomeUnknown
        );
        f.wait();
        assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
    }
}
#[test]
fn unknown_inventory_add_requires_explicit_preview_and_exit_zero_keeps_presence_unknown() {
    let _serial = MUTATIONS.lock().unwrap();
    let f = Fixture::new();
    f.reads.lock().unwrap().ipv4 = Err(NativeError::PermissionDenied);
    let mut fw = f.firewall();
    let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
    assert!(plan.preview().contains("presence remains Unknown"));
    let consent = plan.consent(OperationId(1), 1).unwrap();
    let mut store = f.store();
    let result = fw
        .apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &deadline(),
        )
        .unwrap();
    f.wait();
    assert_eq!(result.result, RuleResult::PendingVerification);
    assert_eq!(
        result.inventory,
        Presence::Unknown(InspectionIssue::Unreadable(NativeError::PermissionDenied))
    );
}
#[test]
fn multiple_links_need_choice_changed_ports_and_manual_managers_never_mutate() {
    let f = Fixture::new();
    f.reads.lock().unwrap().addresses = links_bytes(vec![
        link("enp1s0", "192.168.4.31", 24),
        link("wlan0", "192.168.5.9", 24),
    ]);
    f.reads.lock().unwrap().routes4 = b"[]".to_vec();
    let mut fw = f.firewall();
    let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(
        fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap_err(),
        FirewallError::Changed
    );
    let selected = s.facts().links.as_ref().unwrap()[1].clone();
    let mut r = request(RuleKind::Lan, 2, 2);
    r.selected = Some(selected);
    assert!(fw.plan(&s, r).unwrap().preview().contains("192.168.5.0/24"));
    let mut r = request(RuleKind::Lan, 3, 3);
    r.selected = Some(LanLink {
        interface: "invented".into(),
        cidr: cidr("10.0.0.0/8"),
        default_route: true,
    });
    assert_eq!(fw.plan(&s, r).unwrap_err(), FirewallError::Changed);
    for ports in [[47811, 47813], [0, 47812], [5353, 5353]] {
        let mut r = request(RuleKind::Lan, 4, 4);
        r.ports = ports;
        assert_eq!(fw.plan(&s, r).unwrap_err(), FirewallError::Manual);
    }
    for manager in [
        ManagerSelection::Manual,
        ManagerSelection::Multiple,
        ManagerSelection::Unknown,
    ] {
        let s = fw.detect(manager, &deadline()).unwrap();
        assert_eq!(
            fw.plan(&s, request(RuleKind::Lan, 5, 5)).unwrap_err(),
            FirewallError::Manual
        );
    }
    assert!(f.auth.calls.lock().unwrap().is_empty());
}
#[test]
fn revoked_wrong_target_and_cancelled_post_intent_proofs_authorize_zero_dispatch() {
    let _serial = MUTATIONS.lock().unwrap();
    for mode in 0..3 {
        let f = Fixture::new();
        let other = Fixture::new();
        let mut fw = f.firewall();
        let s = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&s, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let proof = if mode == 0 { other.proof() } else { f.proof() };
        let mut store = f.store();
        let cancellation = Cancellation::default();
        if mode == 1 {
            let io = f.io.clone();
            let p = proof.clone();
            let mut changed = f.facts();
            changed.active = false;
            store.hook = Some(Box::new(move || {
                assert!(p.revalidate(&io, &changed).is_err());
            }));
        }
        if mode == 2 {
            let c = cancellation.clone();
            store.hook = Some(Box::new(move || c.cancel()));
        }
        assert!(
            fw.apply(
                &proof,
                ManagerSelection::Ufw,
                plan,
                consent,
                &mut store,
                &Deadline::new(5000, cancellation).unwrap()
            )
            .is_err()
        );
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }
}
#[test]
fn production_reader_uses_only_merged_exact_ordinary_read_vectors_with_scratch_runner() {
    let f = Fixture::new();
    let facts = observe(f.io.as_ref(), ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(facts.activity, Activity::Unknown);
    assert!(matches!(
        facts.ipv4,
        Err(InspectionIssue::Unreadable(NativeError::Foreign))
    ));
    let calls = f.native.calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls.iter().map(|c| c.argv().to_vec()).collect::<Vec<_>>(),
        [
            vec!["is-active", "ufw.service"],
            vec!["-j", "-d", "addr", "show"],
            vec!["-j", "route", "show", "default"],
            vec!["-j", "-6", "route", "show", "default"]
        ]
    );
    assert!(f.auth.calls.lock().unwrap().is_empty());
}

#[test]
fn round1_mixed_unrelated_and_uncertain_rules_keep_positive_administrator_evidence() {
    let _serial = MUTATIONS.lock().unwrap();
    for (network, address, bits) in [
        ("192.168.4.0/24", "192.168.4.31", 24),
        ("fd42::/64", "fd42::31", 64),
    ] {
        for comment in [None, Some("Crosspane (LAN)")] {
            let f = Fixture::new();
            let v6 = network.contains(':');
            let chain = if v6 { "ufw6" } else { "ufw" };
            let destination = if v6 { "::/0" } else { "0.0.0.0/0" };
            let unrelated = format!(
                "### tuple ### allow tcp 22 {destination} any {network} in\n-A {chain}-user-input -p tcp --dport 22 -s {network} -j ACCEPT\n"
            );
            let uncertain = format!(
                "### tuple ### allow udp 5353 {destination} any {network} in comment=gg\n-A {chain}-user-input -p udp --dport 5353 -s {network} -j ACCEPT\n"
            );
            let bytes = String::from_utf8(rule_file(network, RuleKind::Lan, comment))
                .unwrap()
                .replace(
                    "### RULES ###\n",
                    &format!("### RULES ###\n{unrelated}{uncertain}"),
                )
                .into_bytes();
            {
                let mut reads = f.reads.lock().unwrap();
                if v6 {
                    reads.ipv6 = Ok(bytes);
                } else {
                    reads.ipv4 = Ok(bytes);
                }
                reads.addresses = links_bytes(vec![link("enp1s0", address, bits)]);
            }
            let mut fw = f.firewall();
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert_eq!(
                snapshot.facts().presence(&cidr(network), RuleKind::Lan),
                if comment.is_some() {
                    Presence::Owned
                } else {
                    Presence::Equivalent
                }
            );
            let plan = fw.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
            let consent = plan.consent(OperationId(1), 1).unwrap();
            let mut store = f.store();
            assert_eq!(
                fw.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::Kept
            );
            assert!(store.intents.is_empty());
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn round1_ordered_unique_markers_and_tokenized_unpaired_statements_stay_unknown() {
    let valid = String::from_utf8(rule_file(
        "192.168.4.0/24",
        RuleKind::Lan,
        Some("Crosspane (LAN)"),
    ))
    .unwrap();
    for bytes in [
        valid.replace("COMMIT\n", "").replace("### RULES ###", "COMMIT\n### RULES ###"),
        valid.replace("### RULES ###", "### RULES ###\n### RULES ###"),
        valid.replace("### END RULES ###", "### END RULES ###\n### END RULES ###"),
        valid.replace("COMMIT\n", "COMMIT\nCOMMIT\n"),
        "*filter\n### RULES ###\n-A\tufw-user-input -p udp --dport 5353 -j ACCEPT\n### END RULES ###\nCOMMIT\n".into(),
    ] {
        let reads = Reads { ipv4: Ok(bytes.into_bytes()), ..Reads::default() };
        assert!(matches!(observe(&reads, ManagerSelection::Ufw, &deadline()).unwrap()
            .presence(&cidr("192.168.4.0/24"), RuleKind::Lan), Presence::Unknown(_)));
    }
}

#[test]
fn round1_enabled_case_duplicates_quoting_and_malformed_assignments_are_unknown() {
    for config in [
        "ENABLED=yes\nenabled=no\n",
        "ENABLED=yes\nEnabled=yes\n",
        "ENABLED=\"yes\"\n",
        "ENABLED=yes\nENABLED yes\n",
        "ENABLED=yes\nENABLED = no\n",
    ] {
        let reads = Reads {
            config: config.as_bytes().to_vec(),
            ..Reads::default()
        };
        assert_eq!(
            observe(&reads, ManagerSelection::Ufw, &deadline())
                .unwrap()
                .activity,
            Activity::Unknown,
            "{config}"
        );
    }
}

#[test]
fn round1_decoded_non_ascii_comment_is_unknown_in_both_families() {
    for network in ["192.168.4.0/24", "fd42::/64"] {
        let bytes = rule_file(network, RuleKind::Lan, Some("é"));
        let mut reads = Reads::default();
        if network.contains(':') {
            reads.ipv6 = Ok(bytes);
        } else {
            reads.ipv4 = Ok(bytes);
        }
        assert!(matches!(
            observe(&reads, ManagerSelection::Ufw, &deadline())
                .unwrap()
                .presence(&cidr(network), RuleKind::Lan),
            Presence::Unknown(_)
        ));
    }
}

#[test]
fn round1_mdns_requires_current_observations_and_is_pending_in_b1() {
    // Evidence retirement, post-intent agent checks and current readmission move to b2a.
    let f = Fixture::new();
    let mut fw = f.firewall();
    let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
    assert_eq!(
        fw.plan(&snapshot, request(RuleKind::Mdns, 1, 1))
            .unwrap_err(),
        FirewallError::MdnsPending
    );
    assert!(f.auth.calls.lock().unwrap().is_empty());
}

#[test]
fn round1_firewall_only_redetection_cannot_readmit_unresolved_lan_retry() {
    let _serial = MUTATIONS.lock().unwrap();
    for case in 0..4 {
        let f = Fixture::new();
        let mut store = f.store();
        if case == 3 {
            let reads = f.reads.clone();
            store.hook = Some(Box::new(move || {
                reads.lock().unwrap().addresses =
                    links_bytes(vec![link("enp1s0", "192.168.8.4", 24)]);
            }));
        } else {
            f.auth.outcomes.lock().unwrap().push_back(match case {
                0 => exited(126, "", "Request dismissed\n"),
                1 => exited(
                    127,
                    "",
                    "Error creating textual authentication agent: no terminal\n",
                ),
                _ => PkexecOutcome::TimedOut,
            });
        }
        let mut fw = f.firewall();
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let plan = fw.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
        let consent = plan.consent(OperationId(1), 1).unwrap();
        let result = fw.apply(
            &f.proof(),
            ManagerSelection::Ufw,
            plan,
            consent,
            &mut store,
            &deadline(),
        );
        if case == 3 {
            assert_eq!(result.unwrap_err(), FirewallError::Changed);
            assert_eq!(store.results, [RuleResult::NotDispatched]);
        } else {
            assert_eq!(
                result.unwrap().result,
                if case == 1 {
                    RuleResult::PromptUnavailable
                } else {
                    RuleResult::OutcomeUnknown
                }
            );
            f.wait();
        }
        for _ in 0..2 {
            let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let error = fw.plan(&fresh, request(RuleKind::Lan, 2, 2)).unwrap_err();
            assert_eq!(error, FirewallError::CurrentRequired);
            assert_eq!(
                error.to_string(),
                "re-detection with current agent observations required"
            );
            assert_eq!(
                fw.plan(&fresh, request(RuleKind::Mdns, 3, 3)).unwrap_err(),
                FirewallError::MdnsPending
            );
        }
        assert_eq!(f.auth.calls.lock().unwrap().len(), usize::from(case != 3));
    }
}

#[test]
fn unrelated_tcp_and_other_udp_ports_do_not_create_inventory_uncertainty() {
    for network in ["192.168.4.0/24", "fd42::/64"] {
        let v6 = network.contains(':');
        let destination = if v6 { "::/0" } else { "0.0.0.0/0" };
        let chain = if v6 { "ufw6" } else { "ufw" };
        for source in [network, destination] {
            let rows = format!(
                "### tuple ### allow tcp 22 {destination} any {source} in\n-A {chain}-user-input -p tcp --dport 22 -s {source} -j ACCEPT\n### tuple ### allow udp 53 {destination} any {source} in\n-A {chain}-user-input -p udp --dport 53 -s {source} -j ACCEPT\n"
            );
            let bytes = String::from_utf8(empty_rules())
                .unwrap()
                .replace("### END RULES ###", &format!("{rows}### END RULES ###"));
            let inventory = parse_rules(bytes.as_bytes(), v6).unwrap();
            assert!(inventory.rules.is_empty());
            assert!(!inventory.uncertain);
        }
    }
}
#[test]
fn unrelated_tuple_cannot_hide_a_relevant_or_ambiguous_emitted_statement() {
    for emitted in [
        "-A ufw-user-input -p udp --dport 5353 -s 192.168.4.0/24 -j ACCEPT",
        "-A ufw-user-input -p tcp --dport 22 -p udp --dport 5353 -j ACCEPT",
        "-A ufw-user-input -p udp --dport 22 --dport 5353 -j ACCEPT",
    ] {
        let row =
            format!("### tuple ### allow tcp 22 0.0.0.0/0 any 192.168.4.0/24 in\n{emitted}\n");
        let bytes = String::from_utf8(empty_rules())
            .unwrap()
            .replace("### END RULES ###", &format!("{row}### END RULES ###"));
        assert_eq!(
            parse_rules(bytes.as_bytes(), false),
            Err(InspectionIssue::Malformed)
        );
    }
}
#[test]
fn missing_emitted_rule_does_not_consume_the_next_positive_tuple() {
    let valid = String::from_utf8(rule_file("192.168.4.0/24", RuleKind::Lan, None)).unwrap();
    let bytes = valid.replace(
        "### RULES ###\n",
        "### RULES ###\n### tuple ### allow udp 5353 0.0.0.0/0 any 192.168.4.0/24 in\n",
    );
    let inventory = parse_rules(bytes.as_bytes(), false).unwrap();
    assert!(inventory.uncertain);
    assert_eq!(inventory.rules.len(), 1);
    assert_eq!(inventory.rules[0].kind, RuleKind::Lan);
    assert_eq!(inventory.rules[0].comment, None);
}
#[test]
fn unpaired_statements_outside_the_rules_section_never_prove_absence() {
    for marker in ["### RULES ###", "COMMIT"] {
        let bytes = String::from_utf8(empty_rules()).unwrap().replace(
            marker,
            &format!("-A\tufw-user-input -p udp --dport 5353 -j ACCEPT\n{marker}"),
        );
        assert_eq!(
            parse_rules(bytes.as_bytes(), false),
            Err(InspectionIssue::Malformed)
        );
    }
}

fn negated_unrelated_udp(network: &str) {
    let v6 = network.contains(':');
    let chain = if v6 { "ufw6" } else { "ufw" };
    let destination = if v6 { "::/0" } else { "0.0.0.0/0" };
    for emitted in [
        format!("-A {chain}-user-input -p udp ! --dport 53 -j ACCEPT"),
        format!("-A {chain}-user-input ! -p tcp --dport 53 -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport 53 -m unknown -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport 53 --dports 53 -m multiport -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport 53 --dport 53 -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dports 53 -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport +53 -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport 53,54 -j ACCEPT"),
        format!("-A {chain}-user-input --dport 53 -j ACCEPT"),
        format!("-A {chain}-user-input -p udp --dport 53 -s {network}/ -j ACCEPT"),
    ] {
        let row = format!("### tuple ### allow udp 53 {destination} any {network} in\n{emitted}\n");
        let bytes = String::from_utf8(empty_rules())
            .unwrap()
            .replace("### END RULES ###", &format!("{row}### END RULES ###"));
        assert_eq!(
            parse_rules(bytes.as_bytes(), v6),
            Err(InspectionIssue::Malformed)
        );
        for comment in [None, Some("Crosspane (LAN)")] {
            let bytes = String::from_utf8(rule_file(network, RuleKind::Lan, comment))
                .unwrap()
                .replace("### END RULES ###", &format!("{row}### END RULES ###"));
            let f = Fixture::new();
            {
                let mut reads = f.reads.lock().unwrap();
                if v6 {
                    reads.ipv6 = Ok(bytes.into_bytes());
                    reads.addresses = links_bytes(vec![link("enp1s0", "fd42::31", 64)]);
                } else {
                    reads.ipv4 = Ok(bytes.into_bytes());
                }
            }
            let mut fw = f.firewall();
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert_eq!(
                snapshot.facts().presence(&cidr(network), RuleKind::Lan),
                if comment.is_some() {
                    Presence::Owned
                } else {
                    Presence::Equivalent
                }
            );
            let plan = fw.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
            let consent = plan.consent(OperationId(1), 1).unwrap();
            assert_eq!(
                fw.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut f.store(),
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::Kept
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }
    }
}
#[test]
fn verify2_negated_unrelated_udp_ipv4_never_proves_absence_or_erases_positives() {
    negated_unrelated_udp("192.168.4.0/24");
}
#[test]
fn verify2_negated_unrelated_udp_ipv6_never_proves_absence_or_erases_positives() {
    negated_unrelated_udp("fd42::/64");
}

fn counter_unpaired(network: &str, outside: bool) {
    let v6 = network.contains(':');
    let chain = if v6 { "ufw6" } else { "ufw" };
    let marker = if outside {
        "COMMIT"
    } else {
        "### END RULES ###"
    };
    for row in [
        format!("[0:0] -A {chain}-user-input -p udp --dport 5353 -j ACCEPT\n"),
        format!("[12:345]\t-A {chain}-user-input -p udp --dport 5353 -j ACCEPT\n"),
        "unrecognized statement\n".into(),
        "[18446744073709551616:0] -A malformed\n".into(),
        "[0:0] # unsupported counter-prefixed comment\n".into(),
        "[0:0] \n".into(),
        "[0:0] *filter\n".into(),
    ] {
        let bytes = String::from_utf8(empty_rules())
            .unwrap()
            .replace(marker, &format!("{row}{marker}"));
        assert_eq!(
            parse_rules(bytes.as_bytes(), v6),
            Err(InspectionIssue::Malformed)
        );
        let bytes = String::from_utf8(rule_file(network, RuleKind::Lan, None))
            .unwrap()
            .replace(marker, &format!("{row}{marker}"));
        let parsed = parse_rules(bytes.as_bytes(), v6).unwrap();
        assert!(parsed.uncertain);
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(parsed.rules[0].cidr, cidr(network));
        assert_eq!(parsed.rules[0].comment, None);
    }
}
#[test]
fn verify2_counter_unpaired_ipv4_inside_is_unknown() {
    counter_unpaired("192.168.4.0/24", false);
}
#[test]
fn verify2_counter_unpaired_ipv6_inside_is_unknown() {
    counter_unpaired("fd42::/64", false);
}
#[test]
fn verify2_counter_unpaired_ipv4_outside_is_unknown() {
    counter_unpaired("192.168.4.0/24", true);
}
#[test]
fn verify2_counter_unpaired_ipv6_outside_is_unknown() {
    counter_unpaired("fd42::/64", true);
}

#[test]
fn verify2_counter_prefixed_matched_ipv4_ipv6_rules_retain_exact_evidence() {
    for (network, v6) in [("192.168.4.0/24", false), ("fd42::/64", true)] {
        for kind in [RuleKind::Lan, RuleKind::Mdns] {
            for comment in [None, Some("Administrator")] {
                let bytes = String::from_utf8(rule_file(network, kind, comment))
                    .unwrap()
                    .replace("\n-A ", "\n[12:345]\t-A ");
                let parsed = parse_rules(bytes.as_bytes(), v6).unwrap();
                assert!(!parsed.uncertain);
                assert_eq!(parsed.rules.len(), 1);
                assert_eq!(parsed.rules[0].cidr, cidr(network));
                assert_eq!(parsed.rules[0].kind, kind);
                assert_eq!(parsed.rules[0].comment.as_deref(), comment);
            }
        }
    }
}

mod current_tests {
    use super::*;
    use crosspane_installer::agent_contract::{
        AgentCall, AgentPlatform, AgentReply, CallFailure, DecodedReply, InstallerRequest,
        ObservationSource, StatusAdmission, decode_reply,
    };
    use crosspane_installer::platform::linux::firewall::current::*;
    use crosspane_types::id::NodeId;

    fn peer() -> NodeId {
        NodeId([2; 32])
    }
    // Every status is inert JSON decoded by the merged codec, never a native agent observation.
    #[allow(clippy::too_many_arguments)] // Explicit fixture identity, receipt and connection axes.
    fn status_reply(
        f: &Fixture,
        id: u64,
        at: u64,
        connected: bool,
        generation: Option<u64>,
        instance: u64,
        error: Option<&str>,
    ) -> AgentReply {
        let names = [
            "capture",
            "keys",
            "pointer",
            "overlay",
            "hotkeys",
            "keystore",
            "windows",
            "parking",
            "frames",
            "tray",
            "links",
            "gpu",
            "home",
            "audio",
            "discovery",
        ];
        let counters = json!({
            "e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,"e1_target_ended":0,
            "e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,"e1_command_releases":0,
            "e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,"e2_dest_returned":0,
            "e2_frames_presented":null,"e2_returns_failed":0
        });
        let bytes = serde_json::to_vec(&json!({"ok":true,"result":{
            "controlling":null,"controlled_by":null,"projections":[],"displays":[],"peers":[],"layout":[],
            "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
            "instance":{"id":instance,"pid":4242,"uid":f.io.target().paths().uid,
                "exe":f.io.target().agent_path(),"runtime_dir":f.io.target().runtime_dir(),"started_unix_ms":1},
            "config_revision":"9f86d081884c7d65","node":NodeId([1;32]),"recovery_pending":0,
            "startup_recovery":"nothing_parked","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
            "epochs":{"gate":1,"grants":1,"layout":1,"backends":1},
            "backends":names.map(|name| json!({"name":name,"state":"ready","reason":null})),
            "keystore":"os_store","permissions":[],"discovery":{"enabled":true,"running":true,"candidates":0,"error":error},
            "tray":{"created":true},"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},
            "settings_opened":0,"peers":[{"node":peer(),"name":"inert","connected":connected,"link_generation":generation,
                "features":[],"grants_given":[],"last_source_parking":null,"counters":counters}]
        }}})).unwrap();
        let result = decode_reply(&InstallerRequest::Status, &bytes, AgentPlatform::Linux);
        assert!(matches!(
            &result,
            Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
        ));
        AgentReply {
            id,
            observed_at_ms: at,
            source: ObservationSource::Demo,
            result,
        }
    }
    fn observations(
        f: &Fixture,
        a: u64,
        b: u64,
        connected: bool,
        generation: Option<u64>,
        instance: u64,
        error: Option<&str>,
    ) -> CurrentObservations {
        CurrentObservations {
            peer: peer(),
            connection: status_reply(f, 31, a, connected, generation, instance, None),
            discovery: status_reply(f, 32, b, connected, generation, instance, error),
        }
    }
    fn sequence(f: &Fixture, offset: u64) -> DialSequence {
        let address = "192.168.4.2:47811".parse().unwrap();
        DialSequence {
            peer: peer(),
            address,
            before: status_reply(f, 11, offset + 10, false, None, 9, None),
            call: AgentCall {
                id: 12,
                request: InstallerRequest::Dial { addr: address },
                timeout_ms: 5000,
            },
            acknowledgement: AgentReply {
                id: 12,
                observed_at_ms: offset + 20,
                source: ObservationSource::Demo,
                result: Ok(DecodedReply::Acknowledged),
            },
            connection: status_reply(f, 13, offset + 30, true, Some(2), 9, None),
            discovery: status_reply(f, 14, offset + 40, true, Some(2), 9, Some("browse_failed")),
        }
    }
    struct Input {
        observations: NativeResult<CurrentObservations>,
        stamp: NativeResult<u64>,
        calls: Vec<(TargetPaths, LanLink, NodeId)>,
        cancel: Option<Cancellation>,
    }
    struct Reader {
        input: Arc<Mutex<Input>>,
        auth: Arc<Auth>,
    }
    impl CurrentReader for Reader {
        fn read(
            &mut self,
            target: &LinuxTarget,
            link: &LanLink,
            peer: NodeId,
            d: &Deadline,
        ) -> NativeResult<CurrentObservations> {
            d.check()?;
            self.auth.order.lock().unwrap().push("current");
            let mut input = self.input.lock().unwrap();
            input
                .calls
                .push((target.paths().clone(), link.clone(), peer));
            if let Some(cancel) = &input.cancel {
                cancel.cancel();
            }
            input.observations.clone()
        }
        fn dispatch_stamp_ms(&self) -> NativeResult<u64> {
            self.auth.order.lock().unwrap().push("stamp");
            self.input.lock().unwrap().stamp
        }
    }
    fn install(f: &Fixture, fw: &mut LinuxFirewall) -> Arc<Mutex<Input>> {
        let input = Arc::new(Mutex::new(Input {
            observations: Ok(observations(
                f,
                60,
                70,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            )),
            stamp: Ok(100),
            calls: vec![],
            cancel: None,
        }));
        fw.install_current_reader(
            peer(),
            Box::new(Reader {
                input: input.clone(),
                auth: f.auth.clone(),
            }),
        );
        input
    }
    fn selected(snapshot: &FirewallSnapshot) -> LanLink {
        snapshot.facts().links.as_ref().unwrap()[0].clone()
    }
    fn admit(f: &Fixture, fw: &mut LinuxFirewall, offset: u64) -> FirewallSnapshot {
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let evidence =
            TrafficEvidence::after_dial(f.io.target(), selected(&snapshot), sequence(f, offset))
                .unwrap();
        fw.admit_dial(&snapshot, evidence).unwrap();
        snapshot
    }
    fn execute(
        f: &Fixture,
        fw: &mut LinuxFirewall,
        snapshot: &FirewallSnapshot,
        kind: RuleKind,
        op: u64,
        store: &mut Store,
        d: &Deadline,
    ) -> Result<FirewallResult, FirewallError> {
        let plan = fw.plan(snapshot, request(kind, op, op)).unwrap();
        let consent = plan.consent(OperationId(op), op).unwrap();
        fw.apply(&f.proof(), ManagerSelection::Ufw, plan, consent, store, d)
    }

    #[test]
    fn first_lan_without_reader_preserves_b1_dispatch_and_intent_contract() {
        let _serial = MUTATIONS.lock().unwrap();
        let f = Fixture::new();
        let mut fw = f.firewall();
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let mut store = f.store();
        assert_eq!(
            execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Lan,
                1,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::PendingVerification
        );
        f.wait();
        assert_eq!(
            *f.auth.order.lock().unwrap(),
            ["intent", "spawn", "outcome"]
        );
        assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn after_dial_requires_exact_correlated_ack_and_later_new_connected_generation() {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let link = selected(&snapshot);
        for case in 0..15 {
            let mut s = sequence(&f, 0);
            match case {
                0 => {
                    s.call.request = InstallerRequest::PairJoin {
                        addr: s.address,
                        allow_input: false,
                    }
                }
                1 => {
                    s.call.request = InstallerRequest::Dial {
                        addr: "192.168.4.3:47811".parse().unwrap(),
                    }
                }
                2 => s.acknowledgement.id += 1,
                3 => s.acknowledgement.result = Err(CallFailure::Unavailable),
                4 => s.acknowledgement.source = ObservationSource::Live,
                5 => s.connection.observed_at_ms = s.acknowledgement.observed_at_ms,
                6 => s.discovery.observed_at_ms = s.connection.observed_at_ms,
                7 => s.before = status_reply(&f, 11, 10, true, Some(2), 9, None),
                8 => s.connection = status_reply(&f, 13, 30, false, None, 9, None),
                9 => s.connection = status_reply(&f, 13, 30, true, Some(2), 10, None),
                10 => {
                    s.discovery = status_reply(&f, 14, 40, true, Some(3), 9, Some("browse_failed"))
                }
                11 => s.discovery = status_reply(&f, 14, 40, true, Some(2), 9, None),
                12 => s.call.timeout_ms = 0,
                13 => s.before.id = s.call.id,
                _ => s.discovery.result = Ok(DecodedReply::PairScan(vec![])),
            }
            assert!(
                TrafficEvidence::after_dial(f.io.target(), link.clone(), s).is_err(),
                "case{case}"
            );
        }
        for old in [None, Some(1)] {
            let mut s = sequence(&f, 0);
            s.before = status_reply(&f, 11, 10, old.is_some(), old, 9, None);
            assert!(TrafficEvidence::after_dial(f.io.target(), link.clone(), s).is_ok());
        }
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn evidence_is_bound_to_selected_target_peer_and_link_without_native_acquisition() {
        let f = Fixture::new();
        let other = Fixture::new();
        let mut fw = f.firewall();
        install(&f, &mut fw);
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let evidence = TrafficEvidence::after_dial(
            other.io.target(),
            selected(&snapshot),
            sequence(&other, 0),
        )
        .unwrap();
        assert_eq!(
            fw.admit_dial(&snapshot, evidence).unwrap_err(),
            FirewallError::MdnsPending
        );
        assert!(
            TrafficEvidence::after_dial(other.io.target(), selected(&snapshot), sequence(&f, 0))
                .is_err()
        );
        let mut evidence_link = selected(&snapshot);
        evidence_link.interface = "different".into();
        let evidence =
            TrafficEvidence::after_dial(f.io.target(), evidence_link, sequence(&f, 0)).unwrap();
        assert_eq!(
            fw.admit_dial(&snapshot, evidence).unwrap_err(),
            FirewallError::Stale
        );
        assert_eq!(
            fw.plan(&snapshot, request(RuleKind::Mdns, 1, 1))
                .unwrap_err(),
            FirewallError::MdnsPending
        );
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn mdns_has_separate_exact_preview_consent_and_one_post_intent_current_read() {
        let _serial = MUTATIONS.lock().unwrap();
        let f = Fixture::new();
        let mut fw = f.firewall();
        let input = install(&f, &mut fw);
        let snapshot = admit(&f, &mut fw, 0);
        let plan = fw.plan(&snapshot, request(RuleKind::Mdns, 2, 7)).unwrap();
        assert!(plan.preview().starts_with("pkexec /usr/bin/ufw allow from 192.168.4.0/24 to any port 5353 proto udp comment 'Crosspane (mDNS)'"));
        assert_eq!(
            plan.consent(OperationId(1), 7).unwrap_err(),
            FirewallError::Stale
        );
        let consent = plan.consent(OperationId(2), 7).unwrap();
        let mut store = f.store();
        let result = fw
            .apply(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                consent,
                &mut store,
                &deadline(),
            )
            .unwrap();
        f.wait();
        assert_eq!(result.result, RuleResult::PendingVerification);
        assert_eq!(result.inventory, Presence::Absent);
        assert_eq!(
            *f.auth.order.lock().unwrap(),
            ["intent", "current", "stamp", "spawn", "outcome"]
        );
        assert_eq!(
            input.lock().unwrap().calls,
            [(f.io.target().paths().clone(), selected(&snapshot), peer())]
        );
        assert_eq!(
            f.auth.calls.lock().unwrap()[0].argv(),
            [
                "--wait",
                "/usr/bin/pkexec",
                "/usr/bin/ufw",
                "allow",
                "from",
                "192.168.4.0/24",
                "to",
                "any",
                "port",
                "5353",
                "proto",
                "udp",
                "comment",
                "Crosspane (mDNS)"
            ]
        );
    }

    #[test]
    fn round1_mdns_later_success_disconnect_restart_and_generation_change_retire_evidence() {
        for case in 0..4 {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            let snapshot = admit(&f, &mut fw, 0);
            let saved =
                TrafficEvidence::after_dial(f.io.target(), selected(&snapshot), sequence(&f, 0))
                    .unwrap();
            input.lock().unwrap().observations = Ok(match case {
                0 => observations(&f, 80, 90, true, Some(2), 9, None),
                1 => observations(&f, 80, 90, false, None, 9, Some("browse_failed")),
                2 => observations(&f, 80, 90, true, Some(2), 10, Some("browse_failed")),
                _ => observations(&f, 80, 90, true, Some(3), 9, Some("browse_failed")),
            });
            fw.refresh_current(&snapshot, &selected(&snapshot), &deadline())
                .unwrap();
            assert_eq!(
                fw.plan(&snapshot, request(RuleKind::Mdns, 1, 1))
                    .unwrap_err(),
                FirewallError::MdnsPending
            );
            assert_eq!(
                fw.admit_dial(&snapshot, saved).unwrap_err(),
                FirewallError::MdnsPending
            );
            input.lock().unwrap().observations = Ok(observations(
                &f,
                100,
                110,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            ));
            fw.refresh_current(&snapshot, &selected(&snapshot), &deadline())
                .unwrap();
            assert_eq!(
                fw.plan(&snapshot, request(RuleKind::Mdns, 2, 2))
                    .unwrap_err(),
                FirewallError::MdnsPending
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn post_intent_discovery_success_disconnect_restart_and_reader_error_are_not_dispatched() {
        for case in 0..4 {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            let snapshot = admit(&f, &mut fw, 0);
            let mut store = f.store();
            let replaced = input.clone();
            let changed = match case {
                0 => Ok(observations(&f, 80, 90, true, Some(2), 9, None)),
                1 => Ok(observations(
                    &f,
                    80,
                    90,
                    false,
                    None,
                    9,
                    Some("browse_failed"),
                )),
                2 => Ok(observations(
                    &f,
                    80,
                    90,
                    true,
                    Some(2),
                    10,
                    Some("browse_failed"),
                )),
                _ => Err(NativeError::Unavailable),
            };
            store.hook = Some(Box::new(move || {
                replaced.lock().unwrap().observations = changed;
            }));
            assert_eq!(
                execute(
                    &f,
                    &mut fw,
                    &snapshot,
                    RuleKind::Mdns,
                    1,
                    &mut store,
                    &deadline()
                )
                .unwrap_err(),
                FirewallError::CurrentRequired
            );
            assert_eq!(store.results, [RuleResult::NotDispatched]);
            assert_eq!(store.intents.len(), 1);
            assert!(f.auth.calls.lock().unwrap().is_empty());
            let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert_eq!(
                fw.plan(&fresh, request(RuleKind::Mdns, 2, 2)).unwrap_err(),
                FirewallError::MdnsPending
            );
        }
    }

    #[test]
    fn round1_retry_needs_fresh_firewall_and_both_strictly_later_current_receipts() {
        let _serial = MUTATIONS.lock().unwrap();
        for (a, b, allowed) in [
            (100, 110, false),
            (110, 110, true),
            (110, 100, false),
            (99, 110, false),
        ] {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            f.auth
                .outcomes
                .lock()
                .unwrap()
                .push_back(exited(126, "", "Request dismissed\n"));
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let mut store = f.store();
            assert_eq!(
                execute(
                    &f,
                    &mut fw,
                    &snapshot,
                    RuleKind::Lan,
                    1,
                    &mut store,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::OutcomeUnknown
            );
            f.wait();
            assert!(
                fw.refresh_current(&snapshot, &selected(&snapshot), &deadline())
                    .is_err()
            );
            input.lock().unwrap().observations = Ok(observations(
                &f,
                a,
                b,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            ));
            let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let _ = fw.refresh_current(&fresh, &selected(&fresh), &deadline());
            let planned = fw.plan(&fresh, request(RuleKind::Lan, 2, 2));
            assert_eq!(planned.is_ok(), allowed, "{a},{b}");
            if allowed {
                input.lock().unwrap().stamp = Ok(150);
                let plan = planned.unwrap();
                let consent = plan.consent(OperationId(2), 2).unwrap();
                assert_eq!(
                    fw.apply(
                        &f.proof(),
                        ManagerSelection::Ufw,
                        plan,
                        consent,
                        &mut store,
                        &deadline()
                    )
                    .unwrap()
                    .result,
                    RuleResult::PendingVerification
                );
                f.wait();
            }
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1 + usize::from(allowed));
        }
    }

    #[test]
    fn unknown_watermark_cannot_be_invented_after_no_reader_unknown_dispatch() {
        let _serial = MUTATIONS.lock().unwrap();
        let f = Fixture::new();
        let mut fw = f.firewall();
        f.auth
            .outcomes
            .lock()
            .unwrap()
            .push_back(PkexecOutcome::TimedOut);
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let mut store = f.store();
        assert_eq!(
            execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Lan,
                1,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::OutcomeUnknown
        );
        f.wait();
        let input = install(&f, &mut fw);
        input.lock().unwrap().observations =
            Ok(observations(&f, 1000, 1100, true, Some(2), 9, None));
        let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        fw.refresh_current(&fresh, &selected(&fresh), &deadline())
            .unwrap();
        assert_eq!(
            fw.plan(&fresh, request(RuleKind::Lan, 2, 2)).unwrap_err(),
            FirewallError::CurrentRequired
        );
        assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn unavailable_or_backwards_dispatch_stamp_refuses_and_records_not_dispatched() {
        for stamp in [Err(NativeError::Unavailable), Ok(69)] {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            input.lock().unwrap().stamp = stamp;
            let snapshot = admit(&f, &mut fw, 0);
            let mut store = f.store();
            assert_eq!(
                execute(
                    &f,
                    &mut fw,
                    &snapshot,
                    RuleKind::Mdns,
                    1,
                    &mut store,
                    &deadline()
                )
                .unwrap_err(),
                FirewallError::CurrentRequired
            );
            assert_eq!(store.results, [RuleResult::NotDispatched]);
            assert!(f.auth.calls.lock().unwrap().is_empty());
            input.lock().unwrap().observations = Ok(observations(
                &f,
                200,
                210,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            ));
            input.lock().unwrap().stamp = Ok(220);
            let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            fw.refresh_current(&fresh, &selected(&fresh), &deadline())
                .unwrap();
            assert_eq!(
                fw.plan(&fresh, request(RuleKind::Mdns, 2, 2)).unwrap_err(),
                FirewallError::CurrentRequired
            );
        }
    }

    #[test]
    fn per_kind_watermarks_keep_lan_and_mdns_retries_and_consents_independent() {
        let _serial = MUTATIONS.lock().unwrap();
        let f = Fixture::new();
        let mut fw = f.firewall();
        let input = install(&f, &mut fw);
        f.auth
            .outcomes
            .lock()
            .unwrap()
            .push_back(PkexecOutcome::TimedOut);
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let mut store = f.store();
        assert_eq!(
            execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Lan,
                1,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::OutcomeUnknown
        );
        f.wait();
        input.lock().unwrap().observations = Ok(observations(
            &f,
            150,
            160,
            true,
            Some(2),
            9,
            Some("browse_failed"),
        ));
        input.lock().unwrap().stamp = Ok(200);
        f.auth
            .outcomes
            .lock()
            .unwrap()
            .push_back(PkexecOutcome::TimedOut);
        let snapshot = admit(&f, &mut fw, 100);
        assert_eq!(
            execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Mdns,
                2,
                &mut store,
                &deadline()
            )
            .unwrap()
            .result,
            RuleResult::OutcomeUnknown
        );
        f.wait();
        input.lock().unwrap().observations = Ok(observations(
            &f,
            170,
            180,
            true,
            Some(2),
            9,
            Some("browse_failed"),
        ));
        let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        fw.refresh_current(&fresh, &selected(&fresh), &deadline())
            .unwrap();
        assert!(fw.plan(&fresh, request(RuleKind::Lan, 3, 3)).is_ok());
        assert_eq!(
            fw.plan(&fresh, request(RuleKind::Mdns, 4, 4)).unwrap_err(),
            FirewallError::CurrentRequired
        );
        input.lock().unwrap().observations = Ok(observations(
            &f,
            210,
            220,
            true,
            Some(2),
            9,
            Some("browse_failed"),
        ));
        fw.refresh_current(&fresh, &selected(&fresh), &deadline())
            .unwrap();
        let mdns = fw.plan(&fresh, request(RuleKind::Mdns, 5, 5)).unwrap();
        let consent = mdns.consent(OperationId(5), 5).unwrap();
        let lan = fw.plan(&fresh, request(RuleKind::Lan, 6, 6)).unwrap();
        assert_eq!(
            fw.apply(
                &f.proof(),
                ManagerSelection::Ufw,
                lan,
                consent,
                &mut store,
                &deadline()
            )
            .unwrap_err(),
            FirewallError::Stale
        );
        assert_eq!(f.auth.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn current_read_source_identity_peer_and_receipt_regressions_refuse_retry() {
        let _serial = MUTATIONS.lock().unwrap();
        for case in 0..5 {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            f.auth.outcomes.lock().unwrap().push_back(exited(
                127,
                "",
                "Error creating textual authentication agent: none\n",
            ));
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let mut store = f.store();
            execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Lan,
                1,
                &mut store,
                &deadline(),
            )
            .unwrap();
            f.wait();
            let mut o = observations(&f, 110, 120, true, Some(2), 9, None);
            match case {
                0 => o.peer = NodeId([3; 32]),
                1 => o.connection.source = ObservationSource::Live,
                2 => o.discovery = status_reply(&f, 32, 120, true, Some(2), 10, None),
                3 => o.discovery.result = Err(CallFailure::Unavailable),
                _ => o.discovery.observed_at_ms = 109,
            }
            input.lock().unwrap().observations = Ok(o);
            let fresh = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert_eq!(
                fw.refresh_current(&fresh, &selected(&fresh), &deadline())
                    .unwrap_err(),
                FirewallError::CurrentRequired
            );
            assert_eq!(
                fw.plan(&fresh, request(RuleKind::Lan, 2, 2)).unwrap_err(),
                FirewallError::CurrentRequired
            );
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn reader_cancellation_after_intent_and_intent_failure_never_dispatch() {
        for fail_intent in [false, true] {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            let snapshot = admit(&f, &mut fw, 0);
            let cancel = Cancellation::default();
            input.lock().unwrap().cancel = Some(cancel.clone());
            let mut store = f.store();
            store.fail_intent = fail_intent;
            let result = execute(
                &f,
                &mut fw,
                &snapshot,
                RuleKind::Mdns,
                1,
                &mut store,
                &Deadline::new(5000, cancel).unwrap(),
            );
            assert!(result.is_err());
            assert!(f.auth.calls.lock().unwrap().is_empty());
            assert_eq!(input.lock().unwrap().calls.len(), usize::from(!fail_intent));
            assert_eq!(
                store.results,
                if fail_intent {
                    vec![]
                } else {
                    vec![RuleResult::NotDispatched]
                }
            );
        }
    }

    fn retirement_replay(replace_reader: bool) {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let input = install(&f, &mut fw);
        let snapshot = admit(&f, &mut fw, 0);
        let link = selected(&snapshot);
        input.lock().unwrap().observations = Ok(observations(&f, 80, 90, true, Some(2), 9, None));
        fw.refresh_current(&snapshot, &link, &deadline()).unwrap();
        if replace_reader {
            install(&f, &mut fw);
        }
        for call_id in [12, 16] {
            let mut replay = sequence(&f, 0);
            replay.call.id = call_id;
            replay.acknowledgement.id = call_id;
            replay.discovery = status_reply(&f, 14, 100, true, Some(2), 9, Some("browse_failed"));
            let evidence =
                TrafficEvidence::after_dial(f.io.target(), link.clone(), replay).unwrap();
            assert!(matches!(
                fw.admit_dial(&snapshot, evidence),
                Err(FirewallError::MdnsPending)
            ));
        }
        // A genuinely later, new call and generation still admit; no unbounded history is needed.
        let mut fresh = sequence(&f, 100);
        fresh.call.id = 17;
        fresh.acknowledgement.id = 17;
        fresh.before = status_reply(&f, 11, 110, true, Some(2), 9, None);
        fresh.connection = status_reply(&f, 13, 130, true, Some(3), 9, None);
        fresh.discovery = status_reply(&f, 14, 140, true, Some(3), 9, Some("browse_failed"));
        let evidence = TrafficEvidence::after_dial(f.io.target(), link, fresh).unwrap();
        fw.admit_dial(&snapshot, evidence).unwrap();
        assert!(fw.plan(&snapshot, request(RuleKind::Mdns, 1, 1)).is_ok());
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn round1_retired_attempt_cannot_rebuild_evidence_with_new_failure() {
        retirement_replay(false);
    }

    #[test]
    fn round1_reader_replacement_keeps_retired_attempt_and_sequence_floor() {
        retirement_replay(true);
    }

    #[test]
    fn round1_already_admitted_attempt_cannot_repeat_with_later_receipts() {
        let f = Fixture::new();
        let mut fw = f.firewall();
        install(&f, &mut fw);
        let snapshot = admit(&f, &mut fw, 0);
        for call_id in [12, 5] {
            let mut replay = sequence(&f, 50);
            replay.call.id = call_id;
            replay.acknowledgement.id = call_id;
            let evidence =
                TrafficEvidence::after_dial(f.io.target(), selected(&snapshot), replay).unwrap();
            assert!(matches!(
                fw.admit_dial(&snapshot, evidence),
                Err(FirewallError::MdnsPending)
            ));
        }
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn round1_traffic_evidence_debug_is_type_only() {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        let evidence =
            TrafficEvidence::after_dial(f.io.target(), selected(&snapshot), sequence(&f, 0))
                .unwrap();
        assert_eq!(format!("{evidence:?}"), "TrafficEvidence { .. }");
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn round1_concurrent_inbound_reachability_does_not_claim_dial_completion() {
        let f = Fixture::new();
        let mut fw = f.firewall();
        install(&f, &mut fw);
        let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
        // The asynchronous Dial has no completion field. This fixture models independent inbound
        // reachability of the selected peer/link after its Ack, which the lead accepts as evidence.
        let evidence =
            TrafficEvidence::after_dial(f.io.target(), selected(&snapshot), sequence(&f, 0))
                .unwrap();
        fw.admit_dial(&snapshot, evidence).unwrap();
        assert!(fw.plan(&snapshot, request(RuleKind::Mdns, 1, 1)).is_ok());
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    fn reordered_batch_replay(replace_reader: bool) {
        let f = Fixture::new();
        let mut fw = f.firewall();
        let input = install(&f, &mut fw);
        let snapshot = admit(&f, &mut fw, 0);
        let link = selected(&snapshot);
        let mut captured = sequence(&f, 40);
        captured.call.id = 16;
        captured.acknowledgement.id = 16;
        let evidence = TrafficEvidence::after_dial(f.io.target(), link.clone(), captured).unwrap();
        input.lock().unwrap().observations = Ok(observations(&f, 100, 90, true, Some(2), 9, None));
        assert!(matches!(
            fw.refresh_current(&snapshot, &link, &deadline()),
            Err(FirewallError::CurrentRequired)
        ));
        if replace_reader {
            install(&f, &mut fw);
        }
        assert!(matches!(
            fw.admit_dial(&snapshot, evidence),
            Err(FirewallError::MdnsPending)
        ));
        let mut fresh = sequence(&f, 100);
        fresh.call.id = 17;
        fresh.acknowledgement.id = 17;
        fresh.before = status_reply(&f, 11, 110, true, Some(2), 9, None);
        fresh.connection = status_reply(&f, 13, 130, true, Some(3), 9, None);
        fresh.discovery = status_reply(&f, 14, 140, true, Some(3), 9, Some("browse_failed"));
        let evidence = TrafficEvidence::after_dial(f.io.target(), link, fresh).unwrap();
        fw.admit_dial(&snapshot, evidence).unwrap();
        assert!(f.auth.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn verify2_reordered_batch_retires_all_older_sequence_receipts() {
        reordered_batch_replay(false);
    }

    #[test]
    fn verify2_reader_replacement_retains_reordered_batch_retirement_floor() {
        reordered_batch_replay(true);
    }

    #[test]
    fn verify2_only_individually_valid_status_receipts_advance_the_floor() {
        for invalid_connection in [false, true] {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            let snapshot = admit(&f, &mut fw, 0);
            let link = selected(&snapshot);
            let mut batch = observations(&f, 100, 100, true, Some(2), 9, None);
            let invalid = if invalid_connection {
                &mut batch.connection
            } else {
                &mut batch.discovery
            };
            invalid.source = ObservationSource::Live;
            invalid.observed_at_ms = 1000;
            input.lock().unwrap().observations = Ok(batch);
            assert!(matches!(
                fw.refresh_current(&snapshot, &link, &deadline()),
                Err(FirewallError::CurrentRequired)
            ));
            for (offset, call_id, allowed) in [(40, 16, false), (100, 17, true)] {
                let mut sequence = sequence(&f, offset);
                sequence.call.id = call_id;
                sequence.acknowledgement.id = call_id;
                let evidence =
                    TrafficEvidence::after_dial(f.io.target(), link.clone(), sequence).unwrap();
                assert_eq!(fw.admit_dial(&snapshot, evidence).is_ok(), allowed);
            }
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }
    }

    mod receipt_tests {
        use super::*;
        use crosspane_installer::platform::linux::firewall::receipts::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn journal(f: &Fixture) -> PathBuf {
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/ufw/journal.json")
        }
        fn intent(f: &Fixture, op: u64, kind: RuleKind, network: &str) -> FirewallIntent {
            FirewallIntent {
                operation: OperationId(op),
                revision: op,
                target: f.io.target().paths().clone(),
                link: LanLink {
                    interface: "enp1s0".into(),
                    cidr: cidr(network),
                    default_route: true,
                },
                kind,
            }
        }
        fn seed(
            f: &Fixture,
            fw: &mut LinuxFirewall,
            kind: RuleKind,
            network: &str,
        ) -> DurableIntentStore {
            let mut store = DurableIntentStore::open(fw, &f.proof()).unwrap();
            let i = intent(f, 1, kind, network);
            store.record_intent(&f.proof(), &i).unwrap();
            store
                .record_outcome(&f.proof(), &i, RuleResult::PendingVerification)
                .unwrap();
            store
        }
        fn installed(f: &Fixture, kind: RuleKind, network: &str) {
            let mut reads = f.reads.lock().unwrap();
            if network.contains(':') {
                reads.ipv6 = Ok(rule_file(network, kind, Some(kind_comment(kind))));
                reads.addresses = links_bytes(vec![link("enp1s0", "fd42::31", 64)]);
                reads.routes4 = b"[]".to_vec();
                reads.routes6 = br#"[{"dst":"default","dev":"enp1s0"}]"#.to_vec();
            } else {
                reads.ipv4 = Ok(rule_file(network, kind, Some(kind_comment(kind))));
            }
        }
        fn kind_comment(kind: RuleKind) -> &'static str {
            if kind == RuleKind::Lan {
                "Crosspane (LAN)"
            } else {
                "Crosspane (mDNS)"
            }
        }
        fn removal(
            f: &Fixture,
            fw: &mut LinuxFirewall,
            store: &DurableIntentStore,
            op: u64,
        ) -> RemovalPlan {
            let bytes = store.receipt(&f.proof(), OperationId(1)).unwrap();
            let receipt = store.admit_receipt(&f.proof(), &bytes).unwrap();
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            fw.plan_removal(&snapshot, receipt, OperationId(op), op)
                .unwrap()
        }
        fn remove(
            f: &Fixture,
            fw: &mut LinuxFirewall,
            store: &mut DurableIntentStore,
            op: u64,
        ) -> Result<FirewallResult, FirewallError> {
            let plan = removal(f, fw, store, op);
            let consent = plan.consent(OperationId(op), op).unwrap();
            fw.apply_removal(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                consent,
                store,
                &deadline(),
            )
        }

        #[test]
        fn durable_add_records_complete_intent_before_fake_dispatch_and_outcome_after() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            let mut fw = f.firewall();
            let mut store = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let path = journal(&f);
            *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
                let j: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                assert_eq!(j["records"][0]["operation"], 7);
                assert_eq!(j["records"][0]["delete"], false);
                assert_eq!(j["records"][0]["outcome"], Value::Null);
            }));
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = fw.plan(&snapshot, request(RuleKind::Lan, 7, 9)).unwrap();
            let consent = plan.consent(OperationId(7), 9).unwrap();
            assert_eq!(
                fw.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::PendingVerification
            );
            f.wait();
            let bytes = store.receipt(&f.proof(), OperationId(7)).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"version":1,"cidr":"192.168.4.0/24","kind":"Lan","result":0})
            );
            assert!(store.admit_receipt(&f.proof(), &bytes).is_ok());
            let metadata = fs::metadata(journal(&f)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(
                fs::metadata(journal(&f).parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
        }

        /// Atomic write/file-sync/rename/parent-sync failures delegate to 4.7a's tested native
        /// contract. These real scratch journals cover installer interruption at publication boundaries.
        #[test]
        fn durable_reconstruction_before_intent_after_intent_and_after_outcome_never_resends() {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let store = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            assert!(!journal(&f).exists());
            drop(store);
            let mut store = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let i = intent(&f, 1, RuleKind::Lan, "192.168.4.0/24");
            store.record_intent(&f.proof(), &i).unwrap();
            assert_eq!(
                f.io.lock(&f.proof(), &journal(&f).with_file_name("journal.lock"))
                    .unwrap_err(),
                NativeError::Busy
            );
            drop(store);
            let mut reconstructed = f.firewall();
            let store = DurableIntentStore::open(&mut reconstructed, &f.proof()).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &store.receipt(&f.proof(), OperationId(1)).unwrap()
                )
                .unwrap()["result"],
                3
            );
            let snapshot = reconstructed
                .detect(ManagerSelection::Ufw, &deadline())
                .unwrap();
            assert_eq!(
                reconstructed
                    .plan(&snapshot, request(RuleKind::Lan, 2, 2))
                    .unwrap_err(),
                FirewallError::CurrentRequired
            );
            drop(store);
            let f2 = Fixture::new();
            let mut second = f2.firewall();
            let store = seed(&f2, &mut second, RuleKind::Lan, "192.168.4.0/24");
            drop(store);
            let mut reconstructed = f2.firewall();
            let store = DurableIntentStore::open(&mut reconstructed, &f2.proof()).unwrap();
            let snapshot = reconstructed
                .detect(ManagerSelection::Ufw, &deadline())
                .unwrap();
            assert!(
                reconstructed
                    .plan(&snapshot, request(RuleKind::Lan, 2, 2))
                    .is_ok()
            );
            assert!(store.receipt(&f2.proof(), OperationId(1)).is_ok());
            assert!(f.auth.calls.lock().unwrap().is_empty());
            assert!(f2.auth.calls.lock().unwrap().is_empty());
            assert!(
                fs::read_dir(journal(&f).parent().unwrap())
                    .unwrap()
                    .all(|entry| !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".crosspane-"))
            );
        }

        #[test]
        fn durable_intent_refusal_has_zero_dispatch_and_prior_record_is_preserved() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let old = fs::read(journal(&f)).unwrap();
            fs::set_permissions(journal(&f), fs::Permissions::from_mode(0o400)).unwrap();
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = fw.plan(&snapshot, request(RuleKind::Lan, 2, 2)).unwrap();
            let consent = plan.consent(OperationId(2), 2).unwrap();
            assert!(
                fw.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .is_err()
            );
            assert_eq!(fs::read(journal(&f)).unwrap(), old);
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn failed_outcome_publication_preserves_pending_intent_and_returns_unknown() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            let mut fw = f.firewall();
            let mut store = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let path = journal(&f);
            *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
                fs::set_permissions(path, fs::Permissions::from_mode(0o400)).unwrap()
            }));
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = fw.plan(&snapshot, request(RuleKind::Lan, 2, 2)).unwrap();
            let consent = plan.consent(OperationId(2), 2).unwrap();
            assert_eq!(
                fw.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::OutcomeUnknown
            );
            f.wait();
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(journal(&f)).unwrap()).unwrap()["records"]
                    [0]["outcome"],
                Value::Null
            );
            drop(store);
            fs::set_permissions(journal(&f), fs::Permissions::from_mode(0o600)).unwrap();
            let mut rebuilt = f.firewall();
            let store = DurableIntentStore::open(&mut rebuilt, &f.proof()).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &store.receipt(&f.proof(), OperationId(2)).unwrap()
                )
                .unwrap()["result"],
                3
            );
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
        }

        #[test]
        fn receipt_rejects_cidr_kind_result_version_extra_fields_and_oversize_forgery() {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let valid = store.receipt(&f.proof(), OperationId(1)).unwrap();
            let original: Value = serde_json::from_slice(&valid).unwrap();
            for (key, value) in [
                ("cidr", json!("192.168.0.0/16")),
                ("cidr", json!("192.168.4.1/24")),
                ("kind", json!("Mdns")),
                ("result", json!(3)),
                ("result", json!(255)),
                ("version", json!(2)),
                ("comment", json!("Crosspane (LAN)")),
                ("path", json!("/etc/ufw/user.rules")),
                ("port", json!(22)),
            ] {
                let mut forged = original.clone();
                forged[key] = value;
                assert!(
                    store
                        .admit_receipt(&f.proof(), &serde_json::to_vec(&forged).unwrap())
                        .is_err(),
                    "{forged}"
                );
            }
            for bytes in [
                b"{".to_vec(),
                vec![b' '; MAX_RECEIPT_BYTES + 1],
                b"{\"version\":1,\"version\":1}".to_vec(),
            ] {
                assert!(store.admit_receipt(&f.proof(), &bytes).is_err());
            }
            assert_eq!(
                format!("{:?}", store.admit_receipt(&f.proof(), &valid).unwrap()),
                "AdmittedReceipt { .. }"
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn journal_target_mode_link_schema_size_and_record_bounds_fail_closed() {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let valid = fs::read(journal(&f)).unwrap();
            drop(store);
            let original: Value = serde_json::from_slice(&valid).unwrap();
            let mut mutations = vec![];
            for (key, value) in [("version", json!(2)), ("extra", json!(true))] {
                let mut j = original.clone();
                j[key] = value;
                mutations.push(serde_json::to_vec(&j).unwrap());
            }
            let mut foreign = original.clone();
            foreign["target"][1][0] = json!("/foreign");
            mutations.push(serde_json::to_vec(&foreign).unwrap());
            for (key, value) in [
                ("operation", json!(0)),
                ("outcome", json!(255)),
                ("cidr", json!("192.168.4.1/24")),
                ("interface", json!("evil\n")),
            ] {
                let mut j = original.clone();
                j["records"][0][key] = value;
                mutations.push(serde_json::to_vec(&j).unwrap());
            }
            let mut many = original.clone();
            many["records"] = json!(vec![
                original["records"][0].clone();
                MAX_JOURNAL_RECORDS + 1
            ]);
            mutations.push(serde_json::to_vec(&many).unwrap());
            mutations.push(vec![b' '; MAX_JOURNAL_BYTES + 1]);
            mutations.push(b"{\"records\":".to_vec());
            for bytes in mutations {
                fs::write(journal(&f), bytes).unwrap();
                assert!(DurableIntentStore::open(&mut fw, &f.proof()).is_err());
            }
            fs::write(journal(&f), &valid).unwrap();
            fs::set_permissions(journal(&f), fs::Permissions::from_mode(0o644)).unwrap();
            assert!(DurableIntentStore::open(&mut fw, &f.proof()).is_err());
            fs::set_permissions(journal(&f), fs::Permissions::from_mode(0o600)).unwrap();
            let saved = journal(&f).with_file_name("saved.json");
            fs::rename(journal(&f), &saved).unwrap();
            symlink(&saved, journal(&f)).unwrap();
            assert!(DurableIntentStore::open(&mut fw, &f.proof()).is_err());
            fs::remove_file(journal(&f)).unwrap();
            fs::hard_link(&saved, journal(&f)).unwrap();
            assert!(DurableIntentStore::open(&mut fw, &f.proof()).is_err());
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn durable_lock_serializes_competing_intents_and_releases_only_after_outcome_or_drop() {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let mut a = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let mut b = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let i = intent(&f, 1, RuleKind::Lan, "192.168.4.0/24");
            a.record_intent(&f.proof(), &i).unwrap();
            assert_eq!(
                b.record_intent(&f.proof(), &intent(&f, 2, RuleKind::Lan, "192.168.4.0/24")),
                Err(NativeError::Busy)
            );
            let mut wrong = i.clone();
            wrong.revision += 1;
            assert_eq!(
                a.record_outcome(&f.proof(), &wrong, RuleResult::PendingVerification),
                Err(NativeError::Foreign)
            );
            a.record_outcome(&f.proof(), &i, RuleResult::PendingVerification)
                .unwrap();
            assert_eq!(
                b.record_intent(&f.proof(), &intent(&f, 2, RuleKind::Mdns, "192.168.4.0/24")),
                Err(NativeError::OutcomeUnknown)
            );
            b = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            b.record_intent(&f.proof(), &intent(&f, 2, RuleKind::Mdns, "192.168.4.0/24"))
                .unwrap();
            drop(b);
            assert!(
                f.io.lock(&f.proof(), &journal(&f).with_file_name("journal.lock"))
                    .is_ok()
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn journal_capacity_and_duplicate_operation_refuse_before_publication() {
            let f = Fixture::new();
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            assert_eq!(
                store.record_intent(&f.proof(), &intent(&f, 1, RuleKind::Lan, "192.168.4.0/24")),
                Err(NativeError::Invalid)
            );
            for op in 2..=MAX_JOURNAL_RECORDS as u64 {
                let i = intent(&f, op, RuleKind::Lan, "192.168.4.0/24");
                store.record_intent(&f.proof(), &i).unwrap();
                store
                    .record_outcome(&f.proof(), &i, RuleResult::Kept)
                    .unwrap();
            }
            let bytes = fs::read(journal(&f)).unwrap();
            assert_eq!(
                store.record_intent(&f.proof(), &intent(&f, 99, RuleKind::Lan, "192.168.4.0/24")),
                Err(NativeError::Oversize)
            );
            assert_eq!(fs::read(journal(&f)).unwrap(), bytes);
        }

        #[test]
        fn exact_lan_ipv4_ipv6_removal_has_separate_consent_and_literal_global_toctou_preview() {
            let _serial = MUTATIONS.lock().unwrap();
            for network in ["192.168.4.0/24", "fd42::/64"] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, network);
                let mut fw = f.firewall();
                let mut store = seed(&f, &mut fw, RuleKind::Lan, network);
                let plan = removal(&f, &mut fw, &store, 2);
                assert!(plan.preview().starts_with(&format!("pkexec /usr/bin/ufw delete allow from {network} to any port 47811:47812 proto udp comment 'Crosspane (LAN)'\n")));
                assert!(plan.preview().contains(REMOVAL_TOCTOU));
                assert!(!plan.preview().contains(REMOVAL_LIMIT));
                assert_eq!(
                    plan.consent(OperationId(2), 3).unwrap_err(),
                    FirewallError::Stale
                );
                let consent = plan.consent(OperationId(2), 2).unwrap();
                let result = fw
                    .apply_removal(
                        &f.proof(),
                        ManagerSelection::Ufw,
                        plan,
                        consent,
                        &mut store,
                        &deadline(),
                    )
                    .unwrap();
                f.wait();
                assert_eq!(result.result, RuleResult::PendingVerification);
                assert_eq!(result.inventory, Presence::Owned);
                let calls = f.auth.calls.lock().unwrap();
                assert_eq!(
                    calls[0].argv(),
                    [
                        "--wait",
                        "/usr/bin/pkexec",
                        "/usr/bin/ufw",
                        "delete",
                        "allow",
                        "from",
                        network,
                        "to",
                        "any",
                        "port",
                        "47811:47812",
                        "proto",
                        "udp",
                        "comment",
                        "Crosspane (LAN)"
                    ]
                );
                assert_eq!(calls.len(), 1);
            }
        }

        #[test]
        fn mdns_removal_uses_current_same_kind_stamp_after_discovery_recovered_without_add_evidence()
         {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            installed(&f, RuleKind::Mdns, "192.168.4.0/24");
            let mut fw = f.firewall();
            let input = install(&f, &mut fw);
            input.lock().unwrap().observations =
                Ok(observations(&f, 60, 70, true, Some(2), 9, None));
            let mut store = seed(&f, &mut fw, RuleKind::Mdns, "192.168.4.0/24");
            f.auth
                .outcomes
                .lock()
                .unwrap()
                .push_back(exited(126, "", "cancelled"));
            assert_eq!(
                remove(&f, &mut fw, &mut store, 2).unwrap().result,
                RuleResult::OutcomeUnknown
            );
            f.wait();
            let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let selected = selected(&snapshot);
            input.lock().unwrap().observations =
                Ok(observations(&f, 100, 101, true, Some(2), 9, None));
            fw.refresh_current(&snapshot, &selected, &deadline())
                .unwrap();
            let receipt = store
                .admit_receipt(
                    &f.proof(),
                    &store.receipt(&f.proof(), OperationId(1)).unwrap(),
                )
                .unwrap();
            assert_eq!(
                fw.plan_removal(&snapshot, receipt, OperationId(3), 3)
                    .unwrap_err(),
                FirewallError::CurrentRequired
            );
            input.lock().unwrap().observations =
                Ok(observations(&f, 110, 111, true, Some(2), 9, None));
            input.lock().unwrap().stamp = Ok(120);
            fw.refresh_current(&snapshot, &selected, &deadline())
                .unwrap();
            let plan = removal(&f, &mut fw, &store, 3);
            assert!(plan.preview().contains("port 5353"));
            input.lock().unwrap().observations =
                Ok(observations(&f, 130, 140, true, Some(2), 9, None));
            input.lock().unwrap().stamp = Ok(150);
            let consent = plan.consent(OperationId(3), 3).unwrap();
            assert!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .is_ok()
            );
            f.wait();
            assert_eq!(
                f.auth.calls.lock().unwrap()[0].argv().last().unwrap(),
                "Crosspane (mDNS)"
            );
            assert_eq!(f.auth.calls.lock().unwrap().len(), 2);
        }

        #[test]
        fn mdns_removal_without_current_reader_or_valid_stamp_records_not_dispatched() {
            let f = Fixture::new();
            installed(&f, RuleKind::Mdns, "192.168.4.0/24");
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Mdns, "192.168.4.0/24");
            assert_eq!(
                remove(&f, &mut fw, &mut store, 2).unwrap_err(),
                FirewallError::CurrentRequired
            );
            let data: Value = serde_json::from_slice(&fs::read(journal(&f)).unwrap()).unwrap();
            assert_eq!(data["records"][1]["delete"], true);
            assert_eq!(data["records"][1]["outcome"], 1);
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn unreadable_inventory_requires_inspection_limit_preview_and_keeps_poststate_unknown() {
            let _serial = MUTATIONS.lock().unwrap();
            for network in ["192.168.4.0/24", "fd42::/64"] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, network);
                let mut fw = f.firewall();
                let mut store = seed(&f, &mut fw, RuleKind::Lan, network);
                if network.contains(':') {
                    f.reads.lock().unwrap().ipv6 = Err(NativeError::PermissionDenied);
                } else {
                    f.reads.lock().unwrap().ipv4 = Err(NativeError::PermissionDenied);
                }
                let plan = removal(&f, &mut fw, &store, 2);
                assert!(plan.preview().contains(REMOVAL_LIMIT));
                let consent = plan.consent(OperationId(2), 2).unwrap();
                let result = fw
                    .apply_removal(
                        &f.proof(),
                        ManagerSelection::Ufw,
                        plan,
                        consent,
                        &mut store,
                        &deadline(),
                    )
                    .unwrap();
                f.wait();
                assert_eq!(result.result, RuleResult::PendingVerification);
                assert_eq!(
                    result.inventory,
                    Presence::Unknown(InspectionIssue::Unreadable(NativeError::PermissionDenied))
                );
            }
        }

        #[test]
        fn duplicate_equivalent_modified_malformed_and_affected_uncertainty_never_authorize_removal()
         {
            for network in ["192.168.4.0/24", "fd42::/64"] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, network);
                let mut fw = f.firewall();
                let store = seed(&f, &mut fw, RuleKind::Lan, network);
                let exact =
                    String::from_utf8(rule_file(network, RuleKind::Lan, Some("Crosspane (LAN)")))
                        .unwrap();
                let body = exact
                    .split("### RULES ###\n")
                    .nth(1)
                    .unwrap()
                    .split("### END RULES ###")
                    .next()
                    .unwrap();
                for bytes in [
                    rule_file(network, RuleKind::Lan, None),
                    rule_file(network, RuleKind::Lan, Some("Administrator")),
                    rule_file(
                        if network.contains(':') {
                            "fd43::/64"
                        } else {
                            "192.168.5.0/24"
                        },
                        RuleKind::Lan,
                        Some("Crosspane (LAN)"),
                    ),
                    exact
                        .replace("### END RULES ###", &format!("{body}### END RULES ###"))
                        .into_bytes(),
                    exact
                        .replace("### END RULES ###", "unsupported\n### END RULES ###")
                        .into_bytes(),
                    b"malformed".to_vec(),
                    vec![b' '; MAX_UFW_BYTES + 1],
                ] {
                    if network.contains(':') {
                        f.reads.lock().unwrap().ipv6 = Ok(bytes);
                    } else {
                        f.reads.lock().unwrap().ipv4 = Ok(bytes);
                    }
                    let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
                    let receipt = store
                        .admit_receipt(
                            &f.proof(),
                            &store.receipt(&f.proof(), OperationId(1)).unwrap(),
                        )
                        .unwrap();
                    assert_eq!(
                        fw.plan_removal(&snapshot, receipt, OperationId(2), 2)
                            .unwrap_err(),
                        FirewallError::Kept
                    );
                }
                assert!(f.auth.calls.lock().unwrap().is_empty());
            }
        }

        #[test]
        fn stale_consent_snapshot_backend_and_predelete_edits_refuse_without_dispatch() {
            let f = Fixture::new();
            installed(&f, RuleKind::Lan, "192.168.4.0/24");
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let old = removal(&f, &mut fw, &store, 2);
            let consent = old.consent(OperationId(2), 2).unwrap();
            let current = removal(&f, &mut fw, &store, 3);
            assert_eq!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    current,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap_err(),
                FirewallError::Stale
            );
            let plan = removal(&f, &mut fw, &store, 4);
            let consent = plan.consent(OperationId(4), 4).unwrap();
            f.reads.lock().unwrap().ipv4 = Ok(rule_file("192.168.4.0/24", RuleKind::Lan, None));
            assert_eq!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap_err(),
                FirewallError::Changed
            );
            installed(&f, RuleKind::Lan, "192.168.4.0/24");
            let plan = removal(&f, &mut fw, &store, 5);
            let consent = plan.consent(OperationId(5), 5).unwrap();
            assert_eq!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Manual,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .unwrap_err(),
                FirewallError::Changed
            );
            assert_eq!(serde_json::from_slice::<Value>(&fs::read(journal(&f)).unwrap()).unwrap()["records"].as_array().unwrap().len(), 1);
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn authorization_time_edit_is_reported_and_never_restores_or_resends_rules() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            installed(&f, RuleKind::Lan, "192.168.4.0/24");
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let reads = f.reads.clone();
            *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
                let mut r = reads.lock().unwrap();
                r.ipv4 = Ok(rule_file(
                    "192.168.4.0/24",
                    RuleKind::Lan,
                    Some("Administrator"),
                ));
                r.addresses = links_bytes(vec![link("enp1s0", "192.168.5.31", 24)]);
            }));
            let result = remove(&f, &mut fw, &mut store, 2).unwrap();
            f.wait();
            assert_eq!(result.result, RuleResult::OutcomeUnknown);
            assert_eq!(result.inventory, Presence::Equivalent);
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
            assert_eq!(
                f.reads.lock().unwrap().ipv4.as_ref().unwrap(),
                &rule_file("192.168.4.0/24", RuleKind::Lan, Some("Administrator"))
            );
        }

        #[test]
        fn absent_result_and_fresh_absence_retire_receipts_but_exit_zero_owned_does_not() {
            let _serial = MUTATIONS.lock().unwrap();
            for (mode, expected) in [
                (0, RuleResult::Absent),
                (1, RuleResult::Absent),
                (2, RuleResult::PendingVerification),
                (3, RuleResult::Absent),
            ] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, "192.168.4.0/24");
                let mut fw = f.firewall();
                let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
                let old = store.receipt(&f.proof(), OperationId(1)).unwrap();
                if mode == 0 {
                    f.auth.outcomes.lock().unwrap().push_back(exited(
                        1,
                        "",
                        "Could not delete non-existent rule\n",
                    ));
                }
                if mode == 1 {
                    let reads = f.reads.clone();
                    *f.auth.hook.lock().unwrap() = Some(Box::new(move || {
                        reads.lock().unwrap().ipv4 = Ok(empty_rules())
                    }));
                }
                if mode == 3 {
                    f.reads.lock().unwrap().ipv4 = Ok(empty_rules());
                }
                assert_eq!(remove(&f, &mut fw, &mut store, 2).unwrap().result, expected);
                if mode != 3 {
                    f.wait();
                }
                assert_eq!(store.admit_receipt(&f.proof(), &old).is_ok(), mode == 2);
                assert_eq!(f.auth.calls.lock().unwrap().len(), usize::from(mode != 3));
            }
        }

        #[test]
        fn removal_prompt_unavailable_timeout_and_other_nonzero_keep_receipt_and_require_redetection()
         {
            let _serial = MUTATIONS.lock().unwrap();
            for (outcome, expected) in [
                (
                    exited(
                        127,
                        "",
                        "Error creating textual authentication agent: no terminal\n",
                    ),
                    RuleResult::PromptUnavailable,
                ),
                (
                    exited(
                        127,
                        "",
                        "Error executing command as another user: No authentication agent found.\n",
                    ),
                    RuleResult::PromptUnavailable,
                ),
                (PkexecOutcome::TimedOut, RuleResult::OutcomeUnknown),
                (exited(126, "", "cancelled"), RuleResult::OutcomeUnknown),
            ] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, "192.168.4.0/24");
                let mut fw = f.firewall();
                let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
                f.auth.outcomes.lock().unwrap().push_back(outcome);
                let result = remove(&f, &mut fw, &mut store, 2).unwrap();
                f.wait();
                assert_eq!(result.result, expected);
                assert_eq!(
                    result.manual.is_some(),
                    expected == RuleResult::PromptUnavailable
                );
                if let Some(manual) = result.manual {
                    assert!(
                        manual.starts_with("sudo /usr/bin/ufw delete allow from 192.168.4.0/24")
                    );
                }
                let bytes = store.receipt(&f.proof(), OperationId(1)).unwrap();
                let receipt = store.admit_receipt(&f.proof(), &bytes).unwrap();
                let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
                assert_eq!(
                    fw.plan_removal(&snapshot, receipt, OperationId(3), 3)
                        .unwrap_err(),
                    FirewallError::CurrentRequired
                );
                assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
            }
        }

        #[test]
        fn proof_revocation_foreign_intent_and_receipt_target_mismatch_refuse_all_io_mutations() {
            let f = Fixture::new();
            let other = Fixture::new();
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let old = fs::read(journal(&f)).unwrap();
            let mut foreign = intent(&f, 2, RuleKind::Lan, "192.168.4.0/24");
            foreign.target = other.io.target().paths().clone();
            assert_eq!(
                store.record_intent(&f.proof(), &foreign),
                Err(NativeError::Foreign)
            );
            assert!(store.receipt(&other.proof(), OperationId(1)).is_err());
            let proof = f.proof();
            let mut revoked = f.facts();
            revoked.active = false;
            assert!(proof.revalidate(&f.io, &revoked).is_err());
            assert!(
                store
                    .record_intent(&proof, &intent(&f, 2, RuleKind::Lan, "192.168.4.0/24"))
                    .is_err()
            );
            let receipt = store
                .admit_receipt(
                    &f.proof(),
                    &store.receipt(&f.proof(), OperationId(1)).unwrap(),
                )
                .unwrap();
            let mut other_fw = other.firewall();
            let snapshot = other_fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert!(
                other_fw
                    .plan_removal(&snapshot, receipt, OperationId(2), 2)
                    .is_err()
            );
            assert_eq!(fs::read(journal(&f)).unwrap(), old);
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn receipt_admission_revalidated_after_preview_and_cancelled_removal_never_dispatches() {
            let f = Fixture::new();
            installed(&f, RuleKind::Lan, "192.168.4.0/24");
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            let plan = removal(&f, &mut fw, &store, 2);
            let consent = plan.consent(OperationId(2), 2).unwrap();
            let original = fs::read(journal(&f)).unwrap();
            let mut data: Value = serde_json::from_slice(&original).unwrap();
            data["records"][0]["retired"] = json!(true);
            fs::write(journal(&f), serde_json::to_vec(&data).unwrap()).unwrap();
            assert!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &deadline()
                )
                .is_err()
            );
            fs::write(journal(&f), original).unwrap();
            let plan = removal(&f, &mut fw, &store, 3);
            let consent = plan.consent(OperationId(3), 3).unwrap();
            let cancellation = Cancellation::default();
            cancellation.cancel();
            assert!(
                fw.apply_removal(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store,
                    &Deadline::new(5000, cancellation).unwrap()
                )
                .is_err()
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn b2b_r1_native_oversize_and_integrity_inventory_errors_never_authorize_deletion() {
            for network in ["192.168.4.0/24", "fd42::/64"] {
                let f = Fixture::new();
                installed(&f, RuleKind::Lan, network);
                let mut fw = f.firewall();
                let store = seed(&f, &mut fw, RuleKind::Lan, network);
                for error in [
                    NativeError::Oversize,
                    NativeError::Foreign,
                    NativeError::Invalid,
                    NativeError::Unavailable,
                    NativeError::Timeout,
                    NativeError::Cancelled,
                    NativeError::Busy,
                    NativeError::OutcomeUnknown,
                    NativeError::Unsupported,
                ] {
                    if network.contains(':') {
                        f.reads.lock().unwrap().ipv6 = Err(error);
                    } else {
                        f.reads.lock().unwrap().ipv4 = Err(error);
                    }
                    let bytes = store.receipt(&f.proof(), OperationId(1)).unwrap();
                    let receipt = store.admit_receipt(&f.proof(), &bytes).unwrap();
                    let snapshot = fw.detect(ManagerSelection::Ufw, &deadline()).unwrap();
                    assert_eq!(
                        fw.plan_removal(&snapshot, receipt, OperationId(2), 2)
                            .unwrap_err(),
                        FirewallError::Kept,
                        "{network}: {error:?}"
                    );
                    assert!(f.auth.calls.lock().unwrap().is_empty());
                }
            }
        }

        struct EditingCurrentReader {
            reads: Arc<Mutex<Reads>>,
            observations: CurrentObservations,
            auth: Arc<Auth>,
        }
        impl CurrentReader for EditingCurrentReader {
            fn read(
                &mut self,
                _: &LinuxTarget,
                _: &LanLink,
                _: NodeId,
                d: &Deadline,
            ) -> NativeResult<CurrentObservations> {
                d.check()?;
                self.auth.order.lock().unwrap().push("current");
                self.reads.lock().unwrap().ipv4 = Ok(rule_file(
                    "192.168.4.0/24",
                    RuleKind::Lan,
                    Some("Administrator"),
                ));
                Ok(self.observations.clone())
            }
            fn dispatch_stamp_ms(&self) -> NativeResult<u64> {
                self.auth.order.lock().unwrap().push("stamp");
                Ok(100)
            }
        }
        #[test]
        fn b2b_r1_current_reader_inventory_change_is_refused_before_stamp_or_dispatch() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            installed(&f, RuleKind::Lan, "192.168.4.0/24");
            let mut fw = f.firewall();
            let mut store = seed(&f, &mut fw, RuleKind::Lan, "192.168.4.0/24");
            fw.install_current_reader(
                peer(),
                Box::new(EditingCurrentReader {
                    reads: f.reads.clone(),
                    observations: observations(&f, 60, 70, true, Some(2), 9, None),
                    auth: f.auth.clone(),
                }),
            );
            let result = remove(&f, &mut fw, &mut store, 2);
            if !f.auth.calls.lock().unwrap().is_empty() {
                f.wait();
            }
            assert_eq!(result.unwrap_err(), FirewallError::Changed);
            assert!(f.auth.calls.lock().unwrap().is_empty());
            assert_eq!(*f.auth.order.lock().unwrap(), ["current"]);
            let j: Value = serde_json::from_slice(&fs::read(journal(&f)).unwrap()).unwrap();
            assert_eq!(j["records"][1]["outcome"], 1);
            assert_eq!(j["records"][1]["delete"], true);
        }

        struct InterruptedOutcome(DurableIntentStore);
        impl IntentStore for InterruptedOutcome {
            fn record_intent(&mut self, p: &SupportProof, i: &FirewallIntent) -> NativeResult<()> {
                self.0.record_intent(p, i)
            }
            fn record_outcome(
                &mut self,
                _: &SupportProof,
                _: &FirewallIntent,
                _: RuleResult,
            ) -> NativeResult<()> {
                Err(NativeError::Unavailable)
            }
        }
        #[test]
        fn b2b_r1_preopened_second_controller_cannot_bypass_interrupted_attempt_recovery() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            let mut a = f.firewall();
            let mut b = f.firewall();
            let mut first =
                InterruptedOutcome(DurableIntentStore::open(&mut a, &f.proof()).unwrap());
            let mut second = DurableIntentStore::open(&mut b, &f.proof()).unwrap();
            let snapshot = a.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = a.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
            let consent = plan.consent(OperationId(1), 1).unwrap();
            assert_eq!(
                a.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut first,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::OutcomeUnknown
            );
            f.wait();
            drop(first);
            let original = fs::read(journal(&f)).unwrap();
            let snapshot = b.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = b.plan(&snapshot, request(RuleKind::Lan, 2, 2)).unwrap();
            let consent = plan.consent(OperationId(2), 2).unwrap();
            let result = b.apply(
                &f.proof(),
                ManagerSelection::Ufw,
                plan,
                consent,
                &mut second,
                &deadline(),
            );
            if f.auth.calls.lock().unwrap().len() > 1 {
                f.wait();
            }
            assert!(result.is_err());
            assert_eq!(f.auth.calls.lock().unwrap().len(), 1);
            assert_eq!(fs::read(journal(&f)).unwrap(), original);
            drop(second);
            let _recovered = DurableIntentStore::open(&mut b, &f.proof()).unwrap();
            let snapshot = b.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            assert_eq!(
                b.plan(&snapshot, request(RuleKind::Lan, 3, 3)).unwrap_err(),
                FirewallError::CurrentRequired
            );
        }

        #[test]
        fn b2b_r1_foreign_outcome_target_preserves_pending_bytes_and_owned_lock() {
            let f = Fixture::new();
            let other = Fixture::new();
            let mut fw = f.firewall();
            let mut store = DurableIntentStore::open(&mut fw, &f.proof()).unwrap();
            let i = intent(&f, 1, RuleKind::Lan, "192.168.4.0/24");
            store.record_intent(&f.proof(), &i).unwrap();
            let original = fs::read(journal(&f)).unwrap();
            let mut foreign = i.clone();
            foreign.target = other.io.target().paths().clone();
            assert_eq!(
                store.record_outcome(&f.proof(), &foreign, RuleResult::PendingVerification),
                Err(NativeError::Foreign)
            );
            assert_eq!(fs::read(journal(&f)).unwrap(), original);
            assert_eq!(
                f.io.lock(&f.proof(), &journal(&f).with_file_name("journal.lock"))
                    .unwrap_err(),
                NativeError::Busy
            );
            store
                .record_outcome(&f.proof(), &i, RuleResult::PendingVerification)
                .unwrap();
            assert!(
                f.io.lock(&f.proof(), &journal(&f).with_file_name("journal.lock"))
                    .is_ok()
            );
            assert!(f.auth.calls.lock().unwrap().is_empty());
        }

        #[test]
        fn b2b_r1_recovery_forgets_older_stamp_but_retains_reader_and_replay_history() {
            let _serial = MUTATIONS.lock().unwrap();
            let f = Fixture::new();
            let mut b = f.firewall();
            let input = install(&f, &mut b);
            let mut old_store = DurableIntentStore::open(&mut b, &f.proof()).unwrap();
            let snapshot = admit(&f, &mut b, 0);
            let plan = b.plan(&snapshot, request(RuleKind::Lan, 1, 1)).unwrap();
            let consent = plan.consent(OperationId(1), 1).unwrap();
            assert_eq!(
                b.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut old_store,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::PendingVerification
            );
            f.wait();
            let mut a = f.firewall();
            let newer = install(&f, &mut a);
            newer.lock().unwrap().observations = Ok(observations(
                &f,
                160,
                170,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            ));
            newer.lock().unwrap().stamp = Ok(200);
            let mut store_a = DurableIntentStore::open(&mut a, &f.proof()).unwrap();
            let snapshot = a.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let plan = a.plan(&snapshot, request(RuleKind::Lan, 2, 2)).unwrap();
            let consent = plan.consent(OperationId(2), 2).unwrap();
            f.auth
                .outcomes
                .lock()
                .unwrap()
                .push_back(exited(126, "", "dismissed"));
            assert_eq!(
                a.apply(
                    &f.proof(),
                    ManagerSelection::Ufw,
                    plan,
                    consent,
                    &mut store_a,
                    &deadline()
                )
                .unwrap()
                .result,
                RuleResult::OutcomeUnknown
            );
            f.wait();
            drop(old_store);
            let _recovered = DurableIntentStore::open(&mut b, &f.proof()).unwrap();
            input.lock().unwrap().observations = Ok(observations(
                &f,
                110,
                111,
                true,
                Some(2),
                9,
                Some("browse_failed"),
            ));
            let snapshot = b.detect(ManagerSelection::Ufw, &deadline()).unwrap();
            let link = selected(&snapshot);
            b.refresh_current(&snapshot, &link, &deadline()).unwrap();
            assert_eq!(
                b.plan(&snapshot, request(RuleKind::Lan, 3, 3)).unwrap_err(),
                FirewallError::CurrentRequired
            );
            assert_eq!(input.lock().unwrap().calls.len(), 2);
            // Recovery must retain both the receipt floor and already admitted call IDs.
            for (offset, call_id) in [(0, 16), (200, 12)] {
                let mut s = sequence(&f, offset);
                s.call.id = call_id;
                s.acknowledgement.id = call_id;
                let evidence = TrafficEvidence::after_dial(f.io.target(), link.clone(), s).unwrap();
                assert_eq!(
                    b.admit_dial(&snapshot, evidence).unwrap_err(),
                    FirewallError::MdnsPending
                );
            }
            assert_eq!(f.auth.calls.lock().unwrap().len(), 2);
        }
    }
}

#[test]
fn large_network_observation_ignores_unrelated_virtual_metadata_and_nondefault_routes() {
    let mut rows = vec![
        json!({"link_type":"loopback","unfamiliar":true}),
        json!({"linkinfo":{"info_kind":"veth"},"ifname":"virtual@other"}),
    ];
    rows.extend((0..400).map(|i| link(&format!("eth{i}"), "192.168.4.1", 24)));
    let links = parse_links(
        &links_bytes(rows),
        br#"[{"dst":"192.168.0.0/16","future":true},{"dst":"default","dev":"eth0"}]"#,
        b"[]",
    )
    .unwrap();
    assert_eq!(links.len(), 400);
    assert!(links[0].default_route);
    assert!(
        parse_links(
            &links_bytes(vec![link("eth0", "192.168.4.1", 24); 4097]),
            b"[]",
            b"[]"
        )
        .is_err()
    );
    assert!(
        parse_links(
            &links_bytes(vec![link("eth0", "192.168.4.1", 24)]),
            br#"[{"dst":"default","dev":null}]"#,
            b"[]"
        )
        .is_err()
    );
}
