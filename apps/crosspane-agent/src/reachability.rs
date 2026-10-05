//! Pure Windows discovery selection facts. No socket, firewall or OS observation happens here.
use crosspane_platform::{Interface, LinkClass};
use crosspane_transport::discovery::DiscoveryInterface;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscoverySelection {
    UnavailableAll,
    Pending,
    NoEligible,
    Selected(Vec<DiscoveryInterface>),
}
impl DiscoverySelection {
    pub(crate) fn token(&self) -> &'static str {
        match self {
            Self::UnavailableAll => "links_unavailable_all",
            Self::Pending => "interfaces_pending",
            Self::NoEligible => "manual_no_eligible",
            Self::Selected(_) => "selected",
        }
    }
    pub(crate) fn count(&self) -> usize {
        match self {
            Self::Selected(keys) => keys.len(),
            _ => 0,
        }
    }
}
fn unicast(addr: IpAddr) -> bool {
    let canonical = match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(addr),
        _ => addr,
    };
    !canonical.is_loopback() && !canonical.is_unspecified() && !canonical.is_multicast()
}
pub(crate) fn select(available: bool, observed: bool, rows: &[Interface]) -> DiscoverySelection {
    if !available {
        return DiscoverySelection::UnavailableAll;
    }
    if !observed {
        return DiscoverySelection::Pending;
    }
    // Count exact keys across all rows, including excluded rows: an unknown/down row must not
    // be accidentally admitted by an identical friendly name and address on a known row.
    let mut occurrences = BTreeMap::<DiscoveryInterface, usize>::new();
    let mut eligible = BTreeSet::new();
    for row in rows {
        let unique: BTreeSet<_> = row.addrs.iter().copied().collect();
        for addr in unique {
            let key = DiscoveryInterface {
                name: row.name.clone(),
                addr,
            };
            *occurrences.entry(key.clone()).or_default() += 1;
            if row.up
                && matches!(
                    row.class,
                    LinkClass::DirectUsb4Tb
                        | LinkClass::DirectEthernet
                        | LinkClass::Lan
                        | LinkClass::Wifi
                )
                && !row.name.is_empty()
                && unicast(addr)
            {
                eligible.insert(key);
            }
        }
    }
    eligible.retain(|key| occurrences.get(key) == Some(&1));
    if eligible.is_empty() {
        DiscoverySelection::NoEligible
    } else {
        DiscoverySelection::Selected(eligible.into_iter().collect())
    }
}

/// Presence of the intended executable-scoped rule, never an inbound-connectivity verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RuleEvidence {
    Present,
    Missing,
    Mismatch,
    #[default]
    Unavailable,
}
impl RuleEvidence {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Present => "Present",
            Self::Missing => "Missing",
            Self::Mismatch => "Mismatch",
            Self::Unavailable => "Unavailable",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
            Self::Mismatch => "mismatched",
            Self::Unavailable => "query unavailable",
        }
    }
}

/// Narrow DTO emitted only for the owned Crosspane firewall-name family. No foreign rule scan.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirewallRule {
    pub name: String,
    pub group: String,
    pub program: String,
    pub enabled: String,
    pub direction: String,
    pub action: String,
    pub profile: String,
    pub edge: String,
    pub protocol: String,
    pub local_port: Vec<String>,
    pub remote_port: Vec<String>,
    pub local_address: Vec<String>,
    pub remote_address: Vec<String>,
    pub interface_type: Vec<String>,
    pub interface_alias: Vec<String>,
    pub service: String,
    pub package: String,
    pub authentication: String,
    pub encryption: String,
    pub override_block: bool,
    pub local_user: String,
    pub remote_user: String,
    pub remote_machine: String,
    pub dynamic_target: String,
    pub loose_source_mapping: bool,
    pub local_only_mapping: bool,
}
const RULE_PREFIX: &str = "Crosspane.Agent.UDP.Private.";

/// Compare canonical DOS/UNC text without resolving or opening another program's path. The
/// installer records the canonical program path; only our current executable is canonicalized.
/// Non-ASCII case differences conservatively mismatch instead of inventing a Windows fold.
fn program_text(path: &str) -> Option<String> {
    let path = path
        .strip_prefix(r"\\?\UNC\")
        .map(|tail| format!(r"\\{tail}"))
        .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(path).to_owned());
    let absolute_drive = path.as_bytes().get(1..3) == Some(b":\\")
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    let absolute_unc = path.starts_with(r"\\")
        && path[2..]
            .split('\\')
            .take(2)
            .filter(|part| !part.is_empty())
            .count()
            == 2;
    if path.len() > 32_768
        || !(absolute_drive || absolute_unc)
        || path.contains(['\0', '/', '"', '%', '*', '?'])
        || path.split('\\').any(|part| part == "." || part == "..")
    {
        return None;
    }
    Some(path)
}
fn one(values: &[String], expected: &str) -> bool {
    values.len() == 1 && values[0] == expected
}
impl FirewallRule {
    fn matches_spec(&self) -> bool {
        let Some(id) = self.name.strip_prefix(RULE_PREFIX) else {
            return false;
        };
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && self.group == format!("Crosspane.{id}")
            && self.enabled == "True"
            && self.direction == "Inbound"
            && self.action == "Allow"
            && self.profile == "Private"
            && self.edge == "Block"
            && matches!(self.protocol.as_str(), "UDP" | "17")
            && one(&self.local_port, "Any")
            && one(&self.remote_port, "Any")
            && one(&self.local_address, "Any")
            && one(&self.remote_address, "LocalSubnet")
            && one(&self.interface_type, "Any")
            && one(&self.interface_alias, "Any")
            && self.service == "Any"
            && self.package == "Any"
            && self.authentication == "NotRequired"
            && self.encryption == "NotRequired"
            && !self.override_block
            && self.local_user == "Any"
            && self.remote_user == "Any"
            && self.remote_machine == "Any"
            && self.dynamic_target == "Any"
            && !self.loose_source_mapping
            && !self.local_only_mapping
    }
}

pub(crate) fn rule_evidence(program: &str, rules: &[FirewallRule]) -> RuleEvidence {
    let Some(program) = program_text(program) else {
        return RuleEvidence::Unavailable;
    };
    if rules.len() > 128 {
        return RuleEvidence::Unavailable;
    }
    let mut matching = 0;
    let mut mismatch = false;
    for rule in rules {
        // The query is prefix scoped. Treat unexpected/oversized output as query failure.
        if !rule.name.starts_with(RULE_PREFIX) || rule.name.len() > 1024 || rule.group.len() > 1024
        {
            return RuleEvidence::Unavailable;
        }
        let Some(candidate) = program_text(&rule.program) else {
            mismatch = true;
            continue;
        };
        if !candidate.eq_ignore_ascii_case(&program) {
            continue;
        }
        if rule.matches_spec() {
            matching += 1;
        } else {
            mismatch = true;
        }
    }
    if mismatch || matching > 1 {
        RuleEvidence::Mismatch
    } else if matching == 1 {
        RuleEvidence::Present
    } else {
        RuleEvidence::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(name: &str, class: LinkClass, up: bool, addrs: &[&str]) -> Interface {
        Interface {
            name: name.into(),
            index: 7,
            class,
            up,
            mtu: None,
            speed_mbps: None,
            addrs: addrs.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }
    #[test]
    fn pending_unavailable_and_empty_are_distinct_facts() {
        let rows = [row("owned", LinkClass::Lan, true, &["192.0.2.1"])];
        assert_eq!(
            select(false, false, &rows),
            DiscoverySelection::UnavailableAll
        );
        assert_eq!(select(false, true, &rows).token(), "links_unavailable_all");
        assert_eq!(select(true, false, &rows), DiscoverySelection::Pending);
        assert_eq!(select(true, false, &rows).token(), "interfaces_pending");
        assert_eq!(select(true, true, &[]), DiscoverySelection::NoEligible);
        assert_eq!(select(true, true, &[]).token(), "manual_no_eligible");
        assert_eq!(select(true, true, &[]).count(), 0);
    }
    #[test]
    fn known_up_classes_admit_exact_keys_without_assuming_family_indices() {
        for class in [
            LinkClass::DirectUsb4Tb,
            LinkClass::DirectEthernet,
            LinkClass::Lan,
            LinkClass::Wifi,
        ] {
            let rows = [row(
                "owned",
                class,
                true,
                &["192.0.2.1", "fe80::1234", "192.0.2.1"],
            )];
            let selected = select(true, true, &rows);
            assert_eq!(selected.token(), "selected");
            assert_eq!(selected.count(), 2);
            let DiscoverySelection::Selected(keys) = selected else {
                panic!()
            };
            assert!(keys.iter().all(|k| k.name == "owned"));
            assert!(
                keys.iter()
                    .any(|k| k.addr == "fe80::1234".parse::<IpAddr>().unwrap())
            );
        }
    }
    #[test]
    fn unknown_down_loopback_unspecified_and_multicast_are_excluded() {
        let rows = [
            row("virtual", LinkClass::Unknown, true, &["192.0.2.1"]),
            row("down", LinkClass::Lan, false, &["192.0.2.2"]),
            row(
                "owned",
                LinkClass::Wifi,
                true,
                &[
                    "127.0.0.1",
                    "::1",
                    "::ffff:127.0.0.1",
                    "0.0.0.0",
                    "::",
                    "224.0.0.251",
                    "ff02::fb",
                ],
            ),
        ];
        assert_eq!(select(true, true, &rows), DiscoverySelection::NoEligible);
    }
    #[test]
    fn ambiguous_exact_keys_fail_closed_even_when_one_row_is_unknown() {
        let rows = [
            row("duplicate", LinkClass::Lan, true, &["192.0.2.1"]),
            row("duplicate", LinkClass::Unknown, true, &["192.0.2.1"]),
            row("unique", LinkClass::Wifi, true, &["192.0.2.2"]),
        ];
        let DiscoverySelection::Selected(keys) = select(true, true, &rows) else {
            panic!()
        };
        assert_eq!(
            keys,
            vec![DiscoveryInterface {
                name: "unique".into(),
                addr: "192.0.2.2".parse().unwrap()
            }]
        );
    }
    #[test]
    fn replacement_snapshot_revokes_previous_keys_instead_of_unioning() {
        let old = [row("old", LinkClass::Lan, true, &["192.0.2.1"])];
        let new = [row("new", LinkClass::Wifi, true, &["192.0.2.2"])];
        assert_ne!(select(true, true, &old), select(true, true, &new));
        let DiscoverySelection::Selected(keys) = select(true, true, &new) else {
            panic!()
        };
        assert!(keys.iter().all(|k| k.name == "new"));
    }
}

#[cfg(test)]
mod firewall_tests {
    use super::*;
    fn rule() -> FirewallRule {
        serde_json::from_value(serde_json::json!({
            "name":"Crosspane.Agent.UDP.Private.fixture-id", "group":"Crosspane.fixture-id",
            "program":r"C:\Crosspane\crosspane-agent.exe", "enabled":"True", "direction":"Inbound",
            "action":"Allow", "profile":"Private", "edge":"Block", "protocol":"UDP",
            "local_port":["Any"], "remote_port":["Any"], "local_address":["Any"],
            "remote_address":["LocalSubnet"], "interface_type":["Any"], "interface_alias":["Any"],
            "service":"Any", "package":"Any", "authentication":"NotRequired", "encryption":"NotRequired",
            "override_block":false, "local_user":"Any", "remote_user":"Any", "remote_machine":"Any",
            "dynamic_target":"Any", "loose_source_mapping":false, "local_only_mapping":false
        })).unwrap()
    }
    #[test]
    fn rule_presence_is_exact_program_and_full_spec_only() {
        let program = r"\\?\C:\Crosspane\crosspane-agent.exe";
        assert_eq!(rule_evidence(program, &[rule()]), RuleEvidence::Present);
        assert_eq!(rule_evidence(program, &[]), RuleEvidence::Missing);
        let mut other = rule();
        other.program = r"C:\Other\crosspane-agent.exe".into();
        assert_eq!(rule_evidence(program, &[other]), RuleEvidence::Missing);
        let mut wrong = rule();
        wrong.profile = "Private, Public".into();
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Mismatch);
        let mut wrong = rule();
        wrong.override_block = true;
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Mismatch);
        let mut wrong = rule();
        wrong.group = "Crosspane.someone-else".into();
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Mismatch);
        assert_eq!(
            rule_evidence(program, &[rule(), rule()]),
            RuleEvidence::Mismatch
        );
    }
    #[test]
    fn malformed_or_unscoped_query_cannot_claim_presence() {
        let program = r"C:\Crosspane\crosspane-agent.exe";
        let mut wrong = rule();
        wrong.name = "ForeignRule".into();
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Unavailable);
        let mut wrong = rule();
        wrong.program = r"%APPDATA%\Crosspane\agent.exe".into();
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Mismatch);
        let mut wrong = rule();
        wrong.remote_address.push("Any".into());
        assert_eq!(rule_evidence(program, &[wrong]), RuleEvidence::Mismatch);
        assert_eq!(
            rule_evidence("relative.exe", &[]),
            RuleEvidence::Unavailable
        );
        assert_eq!(
            rule_evidence(program, &vec![rule(); 129]),
            RuleEvidence::Unavailable
        );
    }
    #[test]
    fn evidence_and_selector_tokens_are_facts_not_denial() {
        assert_eq!(RuleEvidence::Present.token(), "Present");
        assert_eq!(RuleEvidence::Missing.label(), "missing");
        assert_eq!(RuleEvidence::Unavailable.label(), "query unavailable");
        assert_eq!(DiscoverySelection::Pending.token(), "interfaces_pending");
        assert_eq!(DiscoverySelection::Pending.count(), 0);
        assert_eq!(
            DiscoverySelection::Selected(vec![DiscoveryInterface {
                name: "fixture".into(),
                addr: "192.0.2.1".parse().unwrap()
            }])
            .count(),
            1
        );
    }
}
