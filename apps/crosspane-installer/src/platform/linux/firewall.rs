//! Ordinary ufw inspection and exact, single-use consented mutations. No networking readiness policy.
pub mod current;
use super::native_io::*;
use crate::agent_contract::ObservationSource;
use crosspane_installer_core::OperationId;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::IpAddr, sync::Arc};

type Result<T> = std::result::Result<T, FirewallError>;
pub type Inspection<T> = std::result::Result<T, InspectionIssue>;
pub type NativeResult<T> = std::result::Result<T, NativeError>;
// Inventory: bounded ordinary-user observations, separate from mutation and traffic proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FirewallError {
    #[error("invalid or malformed firewall input")]
    Invalid,
    #[error("plan or consent was retired; inspect and preview again")]
    Stale,
    #[error("firewall or selected link changed; inspect again")]
    Changed,
    #[error("manual backend or changed ports require manual guidance")]
    Manual,
    #[error("mDNS repair requires current agent observations")]
    MdnsPending,
    #[error("re-detection with current agent observations required")]
    CurrentRequired,
    #[error("modified, ambiguous or administrator rules must be kept")]
    Kept,
    #[error(transparent)]
    Native(#[from] NativeError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerSelection {
    Ufw,
    Manual,
    Multiple,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Active,
    Inactive,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionIssue {
    Unreadable(NativeError),
    Malformed,
    Oversize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleKind {
    Lan,
    Mdns,
}
impl RuleKind {
    fn index(self) -> usize {
        usize::from(self == Self::Mdns)
    }
    fn fields(self) -> (&'static str, &'static str, UfwRule) {
        match self {
            Self::Lan => ("47811:47812", "Crosspane (LAN)", UfwRule::Lan),
            Self::Mdns => ("5353", "Crosspane (mDNS)", UfwRule::Mdns),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanLink {
    pub interface: String,
    pub cidr: LanCidr,
    pub default_route: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedRule {
    pub cidr: LanCidr,
    pub kind: RuleKind,
    pub comment: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Owned,
    Equivalent,
    Absent,
    Modified,
    Unknown(InspectionIssue),
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// Supported positives survive uncertainty; uncertainty prevents proving absence.
pub struct RuleInventory {
    pub rules: Vec<InspectedRule>,
    pub uncertain: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirewallFacts {
    pub manager: ManagerSelection,
    pub activity: Activity,
    pub links: Inspection<Vec<LanLink>>,
    pub ipv4: Inspection<RuleInventory>,
    pub ipv6: Inspection<RuleInventory>,
}
impl FirewallFacts {
    pub fn presence(&self, cidr: &LanCidr, kind: RuleKind) -> Presence {
        let rules = if cidr.as_str().contains(':') {
            &self.ipv6
        } else {
            &self.ipv4
        };
        let rules = match rules {
            Ok(inventory) => inventory,
            Err(issue) => return Presence::Unknown(*issue),
        };
        let matched: Vec<_> = rules
            .rules
            .iter()
            .filter(|r| r.kind == kind && r.cidr == *cidr)
            .collect();
        if matched.len() > 1 {
            return Presence::Modified;
        }
        match matched.first() {
            Some(r) if r.comment.as_deref() == Some(kind.fields().1) => Presence::Owned,
            Some(_) => Presence::Equivalent,
            None if rules
                .rules
                .iter()
                .any(|r| r.comment.as_deref() == Some(kind.fields().1)) =>
            {
                Presence::Modified
            }
            None if rules.uncertain => Presence::Unknown(InspectionIssue::Malformed),
            None => Presence::Absent,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum FirewallRead {
    Activity,
    Addresses,
    Default4,
    Default6,
}
/// Injected readers are admitted only for explicit scratch targets, never production.
pub trait FirewallReader: Send + Sync {
    fn file(&self, request: SystemRead, deadline: &Deadline) -> NativeResult<Vec<u8>>;
    fn command(&self, request: FirewallRead, deadline: &Deadline) -> NativeResult<CommandOutput>;
}
impl FirewallReader for LinuxNativeIo {
    fn file(&self, request: SystemRead, deadline: &Deadline) -> NativeResult<Vec<u8>> {
        self.read_system(request, deadline).map(|v| v.bytes)
    }
    fn command(&self, request: FirewallRead, deadline: &Deadline) -> NativeResult<CommandOutput> {
        let (executable, args): (&str, &[&str]) = match request {
            FirewallRead::Activity => ("/usr/bin/systemctl", &["is-active", "ufw.service"]),
            FirewallRead::Addresses => ("/usr/bin/ip", &["-j", "-d", "addr", "show"]),
            FirewallRead::Default4 => ("/usr/bin/ip", &["-j", "route", "show", "default"]),
            FirewallRead::Default6 => ("/usr/bin/ip", &["-j", "-6", "route", "show", "default"]),
        };
        self.run(
            &CommandSpec::new(
                executable.into(),
                args.iter().map(|a| (*a).into()).collect(),
                ChildEnvironment::selected(self.target(), BTreeMap::new())?,
                MAX_FIREWALL_COMMAND_BYTES,
            )?,
            deadline,
        )
    }
}
fn text(bytes: &[u8], limit: usize) -> Inspection<&str> {
    if bytes.len() > limit {
        return Err(InspectionIssue::Oversize);
    }
    let value = std::str::from_utf8(bytes).map_err(|_| InspectionIssue::Malformed)?;
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(InspectionIssue::Malformed);
    }
    Ok(value)
}
fn enabled(bytes: &[u8]) -> Option<bool> {
    let value = text(bytes, MAX_UFW_BYTES).ok()?;
    let mut enabled = None;
    for line in value.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let key = line.split(['=', ' ', '\t']).next()?;
        if key.eq_ignore_ascii_case("ENABLED") {
            if enabled.is_some() {
                return None;
            }
            enabled = Some(match line {
                "ENABLED=yes" => true,
                "ENABLED=no" => false,
                _ => return None,
            });
        }
    }
    enabled
}
fn network(value: &str) -> Result<LanCidr> {
    if value.contains('/') {
        return Ok(LanCidr::parse(value)?);
    }
    let ip: IpAddr = value.parse().map_err(|_| FirewallError::Invalid)?;
    Ok(LanCidr::parse(&format!(
        "{ip}/{}",
        if ip.is_ipv4() { 32 } else { 128 }
    ))?)
}
fn counter(value: &str) -> bool {
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .and_then(|v| v.split_once(':'))
        .is_some_and(|(packets, bytes)| {
            [packets, bytes].iter().all(|v| {
                !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && v.parse::<u64>().is_ok()
            })
        })
}
fn statement(line: &str) -> Option<&str> {
    let line = line.trim();
    if !line.starts_with('[') {
        return Some(line);
    }
    let (prefix, rest) = line.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    (counter(prefix) && !rest.is_empty()).then_some(rest)
}
fn unrecognized(line: &str) -> bool {
    let original = line.trim();
    if original.is_empty() || original.starts_with('#') || original == "*filter" {
        return false;
    }
    let Some(line) = statement(original) else {
        return true;
    };
    let words: Vec<_> = line.split_whitespace().collect();
    !matches!(words.as_slice(), [chain, "-", count]
        if original == line && chain.strip_prefix(':').is_some_and(|name| !name.is_empty()
            && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.+".contains(&b)))
        && counter(count))
}
/// Supported grammar: seven-field incoming allow/udp tuples, optional final hex ASCII comment,
/// and the matching single emitted ACCEPT rule. Unsupported forms never prove absence/ownership.
pub fn parse_rules(bytes: &[u8], ipv6: bool) -> Inspection<RuleInventory> {
    let lines: Vec<_> = text(bytes, MAX_UFW_BYTES)?.lines().collect();
    let mut boundaries = Vec::new();
    for marker in ["*filter", "### RULES ###", "### END RULES ###", "COMMIT"] {
        let indices: Vec<_> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                if marker == "COMMIT" {
                    l.split_whitespace().next() == Some(marker)
                } else {
                    **l == marker
                }
            })
            .map(|(i, _)| i)
            .collect();
        match indices.as_slice() {
            [i] if lines[*i] == marker => boundaries.push(*i),
            _ => return Err(InspectionIssue::Malformed),
        }
    }
    if boundaries[0] != 0
        || boundaries.windows(2).any(|w| w[0] >= w[1])
        || lines[boundaries[3] + 1..]
            .iter()
            .any(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        return Err(InspectionIssue::Malformed);
    }
    let mut inventory = RuleInventory {
        rules: Vec::new(),
        uncertain: lines[..boundaries[1]]
            .iter()
            .chain(&lines[boundaries[2] + 1..boundaries[3]])
            .any(|line| {
                unrecognized(line) || line.split_whitespace().take(3).eq(["###", "tuple", "###"])
            }),
    };
    let mut lines = lines[boundaries[1] + 1..boundaries[2]].iter().peekable();
    while let Some(line) = lines.next() {
        if let Some(tuple) = line.strip_prefix("### tuple ### ") {
            let actual = if lines.peek().is_some_and(|line| {
                statement(line).and_then(|l| l.split_whitespace().next()) == Some("-A")
            }) {
                lines.next()
            } else {
                None
            };
            match actual.and_then(|actual| parse_rule(tuple, actual, ipv6).ok()) {
                Some(Some(rule)) if inventory.rules.len() < 256 => inventory.rules.push(rule),
                Some(None) => (),
                _ => inventory.uncertain = true,
            }
        } else if unrecognized(line) || line.split_whitespace().take(3).eq(["###", "tuple", "###"])
        {
            inventory.uncertain = true;
        }
    }
    if inventory.uncertain && inventory.rules.is_empty() {
        Err(InspectionIssue::Malformed)
    } else {
        Ok(inventory)
    }
}
fn relevant(protocol: Option<&str>, ports: Option<&str>) -> bool {
    if protocol.is_some_and(|p| {
        matches!(
            p,
            "tcp" | "icmp" | "ipv6-icmp" | "esp" | "ah" | "gre" | "sctp" | "dccp"
        )
    }) {
        return false;
    }
    let Some(ports) = ports else { return true };
    !ports.split(',').all(|port| {
        let (low, high) = port.split_once(':').unwrap_or((port, port));
        match (low.parse::<u16>(), high.parse::<u16>()) {
            (Ok(low), Ok(high)) if low <= high => [5353, 47811, 47812]
                .iter()
                .all(|p| !(low..=high).contains(p)),
            _ => false,
        }
    })
}
fn parse_rule(tuple: &str, actual: &str, ipv6: bool) -> Inspection<Option<InspectedRule>> {
    let fields: Vec<_> = tuple.split_whitespace().collect();
    let actual = statement(actual).ok_or(InspectionIssue::Malformed)?;
    let actual_words: Vec<_> = actual.split_whitespace().collect();
    // Only recognized, non-negated match syntax can prove a rule unrelated.
    if !actual.is_ascii() || actual_words.len() < 2 || actual_words[0] != "-A"
        || actual_words.len() % 2 != 0 || actual_words.contains(&"!")
        || actual_words[2..].as_chunks::<2>().0.iter().enumerate().any(|(i, w)| {
            if actual_words[..2 + i * 2].contains(&w[0]) { return true; }
            match w[0] {
                "-p" => !matches!(w[1], "udp" | "tcp" | "icmp" | "ipv6-icmp" | "esp" | "ah" | "gre" | "sctp" | "dccp"),
                "-m" => w[1] != "multiport",
                "--dport" | "--dports" => (w[0] == "--dport" && w[1].contains(',')) || !w[1].split(',').all(|p| {
                    let (low, high) = p.split_once(':').unwrap_or((p, p));
                    [low, high].iter().all(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                        && matches!((low.parse::<u16>(), high.parse::<u16>()), (Ok(l), Ok(h)) if l <= h)
                }),
                "-s" => {
                    let (address, prefix) = w[1].split_once('/').map_or((w[1], None), |(a, p)| (a, Some(p)));
                    match address.parse::<IpAddr>() {
                        Ok(ip) => prefix.is_some_and(|p| !p.bytes().all(|b| b.is_ascii_digit())
                            || !matches!(p.parse::<u8>(), Ok(n) if n <= if ip.is_ipv4() { 32 } else { 128 })),
                        Err(_) => true,
                    }
                },
                "-j" => !matches!(w[1], "ACCEPT" | "DROP" | "REJECT"),
                _ => true,
            }
        })
    { return Err(InspectionIssue::Malformed); }
    let field = |name| {
        let values: Vec<_> = actual_words
            .windows(2)
            .filter(|w| w[0] == name)
            .map(|w| w[1])
            .collect();
        match values.as_slice() {
            [value] => Some(*value),
            _ => None,
        }
    };
    if ((field("--dport").is_some() || field("--dports").is_some())
        && !matches!(field("-p"), Some("udp" | "tcp")))
        || (field("--dport").is_some() && field("--dports").is_some())
        || (field("--dports").is_some() != (field("-m") == Some("multiport")))
    {
        return Err(InspectionIssue::Malformed);
    }
    let ports = match (field("--dport"), field("--dports")) {
        (Some(p), None) | (None, Some(p)) => Some(p),
        _ => None,
    };
    if !relevant(fields.get(1).copied(), fields.get(2).copied()) && !relevant(field("-p"), ports) {
        return Ok(None);
    }
    if !(7..=8).contains(&fields.len())
        || fields[0] != "allow"
        || fields[1] != "udp"
        || fields[3] != if ipv6 { "::/0" } else { "0.0.0.0/0" }
        || fields[4] != "any"
        || fields[6] != "in"
    {
        return Err(InspectionIssue::Malformed);
    }
    let kind = match fields[2] {
        "47811:47812" | "47811,47812" => RuleKind::Lan,
        "5353" => RuleKind::Mdns,
        _ => return Err(InspectionIssue::Malformed),
    };
    let cidr = network(fields[5]).map_err(|_| InspectionIssue::Malformed)?;
    if cidr.as_str().contains(':') != ipv6 {
        return Err(InspectionIssue::Malformed);
    }
    let comment = if fields.len() == 8 {
        let hex = fields[7]
            .strip_prefix("comment=")
            .ok_or(InspectionIssue::Malformed)?;
        if hex.len() > 512 || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(InspectionIssue::Malformed);
        }
        let decoded: std::result::Result<Vec<_>, _> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect();
        let comment = String::from_utf8(decoded.map_err(|_| InspectionIssue::Malformed)?)
            .map_err(|_| InspectionIssue::Malformed)?;
        if !comment.is_ascii() || comment.chars().any(char::is_control) {
            return Err(InspectionIssue::Malformed);
        }
        Some(comment)
    } else {
        None
    };
    let chain = if ipv6 {
        "ufw6-user-input"
    } else {
        "ufw-user-input"
    };
    let ports = if kind == RuleKind::Lan {
        format!("-m multiport --dports {}", fields[2])
    } else {
        "--dport 5353".into()
    };
    let expected = format!("-A {chain} -p udp {ports} -s {} -j ACCEPT", fields[5]);
    let actual = actual.split_whitespace().collect::<Vec<_>>().join(" ");
    if actual != expected {
        return Err(InspectionIssue::Malformed);
    }

    Ok(Some(InspectedRule {
        cidr,
        kind,
        comment,
    }))
}
fn json(bytes: &[u8]) -> Inspection<Vec<serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_str(text(bytes, MAX_FIREWALL_COMMAND_BYTES)?)
        .map_err(|_| InspectionIssue::Malformed)?;
    let list = value.as_array().ok_or(InspectionIssue::Malformed)?;
    if list.len() > 256 {
        return Err(InspectionIssue::Oversize);
    }
    Ok(list.clone())
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))
}
pub fn parse_links(addresses: &[u8], default4: &[u8], default6: &[u8]) -> Inspection<Vec<LanLink>> {
    let routes: Vec<_> = json(default4)?.into_iter().chain(json(default6)?).collect();
    let mut defaults = Vec::new();
    for route in routes {
        if route.get("dst").and_then(|v| v.as_str()) != Some("default") {
            return Err(InspectionIssue::Malformed);
        }
        let dev = route
            .get("dev")
            .and_then(|v| v.as_str())
            .filter(|s| name(s))
            .ok_or(InspectionIssue::Malformed)?;
        defaults.push(dev.to_owned());
    }
    let mut links = Vec::new();
    for link in json(addresses)? {
        let interface = link
            .get("ifname")
            .and_then(|v| v.as_str())
            .filter(|s| name(s))
            .ok_or(InspectionIssue::Malformed)?;
        let flags = link
            .get("flags")
            .and_then(|v| v.as_array())
            .ok_or(InspectionIssue::Malformed)?;
        if !["UP", "LOWER_UP"]
            .iter()
            .all(|flag| flags.iter().any(|v| v.as_str() == Some(flag)))
            || flags.iter().any(|v| v.as_str() == Some("LOOPBACK"))
            || link.get("link_type").and_then(|v| v.as_str()) != Some("ether")
            || link.get("linkinfo").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        for address in link
            .get("addr_info")
            .and_then(|v| v.as_array())
            .ok_or(InspectionIssue::Malformed)?
        {
            if address.get("scope").and_then(|v| v.as_str()) != Some("global") {
                continue;
            }
            let ip: IpAddr = address
                .get("local")
                .and_then(|v| v.as_str())
                .ok_or(InspectionIssue::Malformed)?
                .parse()
                .map_err(|_| InspectionIssue::Malformed)?;
            let bits = address
                .get("prefixlen")
                .and_then(|v| v.as_u64())
                .ok_or(InspectionIssue::Malformed)?;
            let Some(cidr) = attached_network(ip, bits) else {
                continue;
            };
            let row = LanLink {
                interface: interface.into(),
                cidr,
                default_route: defaults.iter().any(|d| d == interface),
            };
            if !links.contains(&row) {
                links.push(row);
            }
            if links.len() > 64 {
                return Err(InspectionIssue::Oversize);
            }
        }
    }
    Ok(links)
}
pub fn observe(
    reader: &dyn FirewallReader,
    manager: ManagerSelection,
    deadline: &Deadline,
) -> Result<FirewallFacts> {
    let config = reader
        .file(SystemRead::UfwConfig, deadline)
        .ok()
        .and_then(|b| enabled(&b));
    let unit = reader
        .command(FirewallRead::Activity, deadline)
        .ok()
        .and_then(|o| {
            if !o.stderr.is_empty() || o.stdout.len() > MAX_FIREWALL_COMMAND_BYTES {
                return None;
            }
            match (o.code, o.stdout.as_slice()) {
                (Some(0), b"active\n") => Some(true),
                (Some(3), b"inactive\n") => Some(false),
                _ => None,
            }
        });
    let activity = match (config, unit) {
        (Some(true), Some(true)) => Activity::Active,
        (Some(_), Some(_)) => Activity::Inactive,
        _ => Activity::Unknown,
    };
    let rules = |request, v6| match reader.file(request, deadline) {
        Ok(bytes) => parse_rules(&bytes, v6),
        Err(e) => Err(InspectionIssue::Unreadable(e)),
    };
    let ipv4 = rules(SystemRead::UfwRules, false);
    let ipv6 = rules(SystemRead::UfwRules6, true);
    let mut outputs = Vec::new();
    let links = (|| {
        for request in [
            FirewallRead::Addresses,
            FirewallRead::Default4,
            FirewallRead::Default6,
        ] {
            let output = reader
                .command(request, deadline)
                .map_err(InspectionIssue::Unreadable)?;
            if output.code != Some(0) || !output.stderr.is_empty() {
                return Err(InspectionIssue::Malformed);
            }
            outputs.push(output.stdout);
        }
        parse_links(&outputs[0], &outputs[1], &outputs[2])
    })();
    deadline.check()?;
    Ok(FirewallFacts {
        manager,
        activity,
        links,
        ipv4,
        ipv6,
    })
}

// Mutation: independent LAN/mDNS add. Durable stores and receipt-bound removal belong to b2b.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleResult {
    PendingVerification,
    NotDispatched,
    PromptUnavailable,
    OutcomeUnknown,
    Absent,
    Kept,
}
#[derive(Clone, Debug)]
pub struct FirewallIntent {
    pub operation: OperationId,
    pub revision: u64,
    pub target: TargetPaths,
    pub link: LanLink,
    pub kind: RuleKind,
}
/// B2b supplies the durable implementation. Failure to record intent refuses dispatch;
/// failure to record the result leaves OutcomeUnknown. Neither method authorizes a mutation.
pub trait IntentStore {
    fn record_intent(&mut self, proof: &SupportProof, intent: &FirewallIntent) -> NativeResult<()>;
    fn record_outcome(
        &mut self,
        proof: &SupportProof,
        intent: &FirewallIntent,
        outcome: RuleResult,
    ) -> NativeResult<()>;
}
#[derive(Debug)]
pub struct FirewallSnapshot {
    facts: FirewallFacts,
    generation: u64,
    owner: Arc<()>,
}
impl FirewallSnapshot {
    pub fn facts(&self) -> &FirewallFacts {
        &self.facts
    }
}
#[derive(Debug)]
pub struct PlanRequest {
    pub operation: OperationId,
    pub revision: u64,
    pub kind: RuleKind,
    pub selected: Option<LanLink>,
    pub ports: [u16; 2],
}
#[derive(Debug)]
pub struct FirewallPlan {
    intent: FirewallIntent,
    before: FirewallFacts,
    generation: u64,
    serial: u64,
    owner: Arc<()>,
}
#[derive(Debug)]
pub struct FirewallConsent {
    operation: OperationId,
    revision: u64,
    serial: u64,
    owner: Arc<()>,
}
impl FirewallPlan {
    pub fn consent(&self, operation: OperationId, revision: u64) -> Result<FirewallConsent> {
        if operation != self.intent.operation || revision != self.intent.revision {
            return Err(FirewallError::Stale);
        }
        Ok(FirewallConsent {
            operation,
            revision,
            serial: self.serial,
            owner: self.owner.clone(),
        })
    }
    /// Presentation quotes only; the native capability constructs its own fixed argv.
    pub fn preview(&self) -> String {
        format!(
            "{}\nInterface {} is context only; this rule has no interface restriction. {} Exit zero does not verify networking.",
            command_preview(&self.intent, "pkexec"),
            self.intent.link.interface,
            if matches!(
                self.before
                    .presence(&self.intent.link.cidr, self.intent.kind),
                Presence::Unknown(_)
            ) {
                "Inventory cannot be inspected; presence remains Unknown."
            } else {
                ""
            }
        )
    }
}
fn command_preview(intent: &FirewallIntent, launcher: &str) -> String {
    format!(
        "{launcher} /usr/bin/ufw allow from {} to any port {} proto udp comment '{}'",
        intent.link.cidr.as_str(),
        intent.kind.fields().0,
        intent.kind.fields().1
    )
}
#[derive(Debug)]
pub struct FirewallResult {
    pub result: RuleResult,
    pub inventory: Presence,
    pub manual: Option<String>,
}
/// C-locale, bounded literal classification. Nonzero exits never imply cancellation of ufw.
/// `deleting` is for b2's receipt-bound consumer; b1 never dispatches a deletion.
pub fn classify_result(outcome: NativeResult<PkexecOutcome>, deleting: bool) -> RuleResult {
    let Ok(PkexecOutcome::Exited {
        code,
        stdout,
        stderr,
        stdout_truncated: false,
        stderr_truncated: false,
    }) = outcome
    else {
        return RuleResult::OutcomeUnknown;
    };
    if stdout.len() > MAX_COMMAND_BYTES || stderr.len() > MAX_COMMAND_BYTES {
        return RuleResult::OutcomeUnknown;
    }
    if code == 127
        && stdout.is_empty()
        && let Ok(text) = std::str::from_utf8(&stderr)
    {
        let line = text.strip_suffix('\n').unwrap_or(text);
        if !line.chars().any(char::is_control)
            && (line.starts_with("Error creating textual authentication agent: ")
                || line
                    == "Error executing command as another user: No authentication agent found.")
        {
            return RuleResult::PromptUnavailable;
        }
    }
    let mut output = stdout;
    output.extend_from_slice(&stderr);
    if deleting
        && matches!(
            output.as_slice(),
            b"Could not delete non-existent rule\n" | b"Could not delete non-existent rule"
        )
    {
        return RuleResult::Absent;
    }
    if code == 0 {
        RuleResult::PendingVerification
    } else {
        RuleResult::OutcomeUnknown
    }
}
pub struct LinuxFirewall {
    io: Arc<LinuxNativeIo>,
    reader: Arc<dyn FirewallReader>,
    owner: Arc<()>,
    generation: u64,
    serial: u64,
    current: Option<u64>,
    observed: bool,
    unresolved: [bool; 2],
    current_checks: current::CurrentState,
}
impl std::fmt::Debug for LinuxFirewall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxFirewall { .. }")
    }
}
impl LinuxFirewall {
    pub fn new(io: Arc<LinuxNativeIo>) -> Self {
        Self {
            reader: io.clone(),
            io,
            owner: Arc::new(()),
            generation: 0,
            serial: 0,
            current: None,
            observed: false,
            unresolved: [false; 2],
            current_checks: current::CurrentState::default(),
        }
    }
    pub fn scratch(io: Arc<LinuxNativeIo>, reader: Arc<dyn FirewallReader>) -> Result<Self> {
        if io.target().source() != ObservationSource::Demo {
            return Err(NativeError::Foreign.into());
        }
        Ok(Self {
            reader,
            ..Self::new(io)
        })
    }
    pub fn detect(
        &mut self,
        manager: ManagerSelection,
        deadline: &Deadline,
    ) -> Result<FirewallSnapshot> {
        self.current = None;
        self.observed = false;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(FirewallError::Invalid)?;
        let facts = observe(self.reader.as_ref(), manager, deadline)?;
        self.observed = true;
        Ok(FirewallSnapshot {
            facts,
            generation: self.generation,
            owner: self.owner.clone(),
        })
    }
    pub fn plan(
        &mut self,
        snapshot: &FirewallSnapshot,
        request: PlanRequest,
    ) -> Result<FirewallPlan> {
        self.current = None;
        if request.kind == RuleKind::Mdns {
            self.current_checks.mdns(None)?;
        }
        if self.unresolved[request.kind.index()] {
            return Err(FirewallError::CurrentRequired);
        }
        if !self.observed
            || !Arc::ptr_eq(&snapshot.owner, &self.owner)
            || snapshot.generation != self.generation
            || request.operation.0 == 0
        {
            return Err(FirewallError::Stale);
        }
        if snapshot.facts.manager != ManagerSelection::Ufw
            || snapshot.facts.activity != Activity::Active
            || request.ports != [47811, 47812]
        {
            return Err(FirewallError::Manual);
        }
        let links = snapshot
            .facts
            .links
            .as_ref()
            .map_err(|_| FirewallError::Changed)?;
        let link = match &request.selected {
            Some(selected) if links.contains(selected) => selected.clone(),
            Some(_) => return Err(FirewallError::Changed),
            None => {
                let preferred: Vec<_> = links.iter().filter(|l| l.default_route).collect();
                if preferred.len() == 1 {
                    preferred[0].clone()
                } else if links.len() == 1 {
                    links[0].clone()
                } else {
                    return Err(FirewallError::Changed);
                }
            }
        };
        if request.kind == RuleKind::Mdns {
            self.current_checks.mdns(Some(&link))?;
        }
        if snapshot.facts.presence(&link.cidr, request.kind) == Presence::Modified {
            return Err(FirewallError::Kept);
        }
        self.serial = self.serial.checked_add(1).ok_or(FirewallError::Invalid)?;
        self.current = Some(self.serial);
        let intent = FirewallIntent {
            operation: request.operation,
            revision: request.revision,
            target: self.io.target().paths().clone(),
            link,
            kind: request.kind,
        };
        Ok(FirewallPlan {
            intent,
            before: snapshot.facts.clone(),
            generation: snapshot.generation,
            serial: self.serial,
            owner: self.owner.clone(),
        })
    }
    /// Consumes the plan before preflight. Every retry requires detect and a fresh preview/consent.
    pub fn apply(
        &mut self,
        proof: &SupportProof,
        manager: ManagerSelection,
        plan: FirewallPlan,
        consent: FirewallConsent,
        store: &mut dyn IntentStore,
        deadline: &Deadline,
    ) -> Result<FirewallResult> {
        let current = self.current.take();
        self.observed = false;
        if current != Some(plan.serial)
            || !Arc::ptr_eq(&plan.owner, &self.owner)
            || !Arc::ptr_eq(&consent.owner, &self.owner)
            || plan.generation != self.generation
            || consent.serial != plan.serial
            || consent.operation != plan.intent.operation
            || consent.revision != plan.intent.revision
        {
            return Err(FirewallError::Stale);
        }
        proof.check(&self.io)?;
        if manager != plan.before.manager
            || observe(self.reader.as_ref(), manager, deadline)? != plan.before
        {
            return Err(FirewallError::Changed);
        }
        let before = plan
            .before
            .presence(&plan.intent.link.cidr, plan.intent.kind);
        if matches!(before, Presence::Owned | Presence::Equivalent) {
            return Ok(FirewallResult {
                result: RuleResult::Kept,
                inventory: before,
                manual: None,
            });
        }
        store.record_intent(proof, &plan.intent)?;
        // An interrupted post-intent attempt also requires current-observation readmission.
        self.unresolved[plan.intent.kind.index()] = true;
        let preflight = (|| {
            if observe(self.reader.as_ref(), manager, deadline)? != plan.before {
                return Err(FirewallError::Changed);
            }
            proof.check(&self.io)?;
            deadline.check()?;
            self.current_checks
                .before_dispatch(self.io.target(), &plan.intent, deadline)
        })();
        if let Err(error) = preflight {
            store.record_outcome(proof, &plan.intent, RuleResult::NotDispatched)?;
            return Err(error);
        }
        let mut result = classify_result(
            self.io.pkexec_ufw(
                proof,
                UfwMutation {
                    delete: false,
                    cidr: plan.intent.link.cidr.clone(),
                    rule: plan.intent.kind.fields().2,
                },
                deadline,
            ),
            false,
        );
        let after = observe(self.reader.as_ref(), manager, deadline);
        if after
            .as_ref()
            .is_ok_and(|f| f.links != plan.before.links || f.activity != plan.before.activity)
        {
            result = RuleResult::OutcomeUnknown;
        }
        let inventory = after
            .ok()
            .map(|f| f.presence(&plan.intent.link.cidr, plan.intent.kind))
            .unwrap_or(Presence::Unknown(InspectionIssue::Unreadable(
                NativeError::Unavailable,
            )));
        if store.record_outcome(proof, &plan.intent, result).is_err() {
            result = RuleResult::OutcomeUnknown;
        }
        self.unresolved[plan.intent.kind.index()] = matches!(
            result,
            RuleResult::OutcomeUnknown | RuleResult::PromptUnavailable
        );
        Ok(FirewallResult {
            result,
            inventory,
            manual: (result == RuleResult::PromptUnavailable)
                .then(|| command_preview(&plan.intent, "sudo")),
        })
    }
}
fn attached_network(address: IpAddr, bits: u64) -> Option<LanCidr> {
    network(&address.to_string()).ok()?;
    let masked = match address {
        IpAddr::V4(a) if (8..=32).contains(&bits) => {
            IpAddr::V4((u32::from(a) & (u32::MAX << (32 - bits))).into())
        }
        IpAddr::V6(a) if (16..=128).contains(&bits) => {
            IpAddr::V6((u128::from(a) & (u128::MAX << (128 - bits))).into())
        }
        _ => return None,
    };
    LanCidr::parse(&format!("{masked}/{bits}")).ok()
}
