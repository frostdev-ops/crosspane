//! Pure Windows discovery selection facts. No socket, firewall or OS observation happens here.
use crosspane_installer_core::elevated::firewall::ComRule;
use crosspane_installer_core::elevated::firewall::{MAX_FAMILY, firewall_state};
use crosspane_installer_core::elevated::{
    AgentProgram, FirewallState, InstallId, RuleScope, is_local_drive_path,
};
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

/// Largest `Installer\elevated-setup.json` the agent reads. A larger file is `Unreadable`.
pub(crate) const MAX_RECORD_READ: usize = 64 * 1024;

/// What the installer's `elevated-setup` record says about the install id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecordedId {
    /// No record exists, so no exact rule name was recorded.
    Absent,
    /// A record exists but is not one this agent can trust. Never a presence claim.
    Unreadable,
    Id(String),
}

/// The only envelope fields this reader checks. Serde ignores every other field.
#[derive(serde::Deserialize)]
struct RecordEnvelope {
    schema_version: u32,
    kind: String,
    data: RecordData,
}

#[derive(serde::Deserialize)]
struct RecordData {
    install_id: String,
}

fn valid_install_id(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Reads the install id from the installer's record, matching its `{schema_version, kind, data}`
/// envelope. `None` gives `Absent`. Oversized, non-JSON, another kind or schema, or a
/// `data.install_id` outside `^[A-Za-z0-9-]{1,64}$` gives `Unreadable`. Other fields are ignored.
pub(crate) fn recorded_install_id(bytes: Option<&[u8]>) -> RecordedId {
    let Some(bytes) = bytes else {
        return RecordedId::Absent;
    };
    if bytes.len() > MAX_RECORD_READ {
        return RecordedId::Unreadable;
    }
    let Ok(envelope) = serde_json::from_slice::<RecordEnvelope>(bytes) else {
        return RecordedId::Unreadable;
    };
    let id = envelope.data.install_id;
    if envelope.kind != "elevated-setup" || envelope.schema_version != 1 || !valid_install_id(&id) {
        return RecordedId::Unreadable;
    }
    RecordedId::Id(id)
}

/// The elevated helper's verify predicate (`firewall_state`) over the same COM family.
/// `program`: the canonicalized current executable. An invalid install id, or a program that is
/// not a local-drive path → Unavailable. A local-drive path that `AgentProgram::parse` refuses →
/// Missing. Otherwise the FirewallState maps 1:1, and NotRequested → Unavailable.
pub(crate) fn com_rule_evidence(
    program: &str,
    install_id: &str,
    family: &[ComRule],
) -> RuleEvidence {
    // `firewall_state` would call a larger family Mismatch. The frozen error table says
    // Unavailable, so the cap is checked here first, whatever the reader did.
    if family.len() > MAX_FAMILY {
        return RuleEvidence::Unavailable;
    }
    let Ok(id) = InstallId::parse(install_id) else {
        return RuleEvidence::Unavailable;
    };
    // `canonicalize` returns a verbatim `\\?\` path. `AgentProgram` takes the plain DOS form.
    let text = program.strip_prefix(r"\\?\").unwrap_or(program);
    if !is_local_drive_path(text) {
        return RuleEvidence::Unavailable;
    }
    let Ok(agent) = AgentProgram::parse(text) else {
        return RuleEvidence::Missing;
    };
    match firewall_state(family, Some(&RuleScope { id, program: agent })) {
        FirewallState::Present => RuleEvidence::Present,
        FirewallState::Missing => RuleEvidence::Missing,
        FirewallState::Mismatch => RuleEvidence::Mismatch,
        FirewallState::Unavailable | FirewallState::NotRequested => RuleEvidence::Unavailable,
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
    use crosspane_installer_core::elevated::firewall::desired_rule;

    const PROGRAM: &str = r"C:\Users\x\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
    const VERBATIM: &str = r"\\?\C:\Users\x\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
    const OTHER_PROGRAM: &str = r"D:\Other\Programs\Crosspane\crosspane-agent.exe";
    const DEV_PROGRAM: &str = r"C:\src\target\debug\crosspane-agent.exe";

    /// One single-field change to the exact rule.
    type Perturb = fn(&mut ComRule);

    fn scope_for(id: &str, program: &str) -> RuleScope {
        RuleScope {
            id: InstallId::parse(id).unwrap(),
            program: AgentProgram::parse(program).unwrap(),
        }
    }
    /// The exact rule the helper creates for the fixture install ID and `PROGRAM`.
    fn exact() -> ComRule {
        desired_rule(&scope_for("fixture-id", PROGRAM))
    }
    fn evidence(family: &[ComRule]) -> RuleEvidence {
        com_rule_evidence(VERBATIM, "fixture-id", family)
    }
    #[test]
    fn exact_rule_is_present_for_the_verbatim_program() {
        assert_eq!(evidence(&[exact()]), RuleEvidence::Present);
        assert_eq!(
            com_rule_evidence(PROGRAM, "fixture-id", &[exact()]),
            RuleEvidence::Present
        );
    }
    #[test]
    fn empty_package_id_is_present_and_a_package_condition_is_a_mismatch() {
        let mut rule = exact();
        rule.local_app_package_id = String::new();
        assert_eq!(evidence(&[rule]), RuleEvidence::Present);
        let mut packaged = exact();
        packaged.local_app_package_id = "S-1-15-2-1".into();
        assert_eq!(evidence(&[packaged]), RuleEvidence::Mismatch);
    }
    #[test]
    fn empty_family_and_unrelated_install_are_missing() {
        assert_eq!(evidence(&[]), RuleEvidence::Missing);
        let other_install = desired_rule(&scope_for("other-id", OTHER_PROGRAM));
        assert_eq!(evidence(&[other_install]), RuleEvidence::Missing);
    }
    #[test]
    fn any_differing_field_or_a_duplicate_is_a_mismatch() {
        let cases: [(&str, Perturb); 5] = [
            ("profiles", |rule| rule.profiles = 6),
            ("secure flags", |rule| rule.secure_flags = 1),
            ("remote user list", |rule| {
                rule.remote_user_authorized_list = "S-1-5-1".into();
            }),
            ("interfaces any", |rule| rule.interfaces_any = false),
            ("edge traversal", |rule| rule.edge_traversal = true),
        ];
        for (field, perturb) in cases {
            let mut rule = exact();
            perturb(&mut rule);
            assert_eq!(evidence(&[rule]), RuleEvidence::Mismatch, "{field}");
        }
        assert_eq!(evidence(&[exact(), exact()]), RuleEvidence::Mismatch);
    }
    #[test]
    fn bad_id_non_local_or_oversized_input_is_unavailable() {
        let rule = exact();
        for id in ["", "a b"] {
            assert_eq!(
                com_rule_evidence(VERBATIM, id, std::slice::from_ref(&rule)),
                RuleEvidence::Unavailable,
                "{id:?}"
            );
        }
        for program in [
            r"\\server\share\Programs\Crosspane\crosspane-agent.exe",
            r"Programs\Crosspane\crosspane-agent.exe",
        ] {
            assert_eq!(
                com_rule_evidence(program, "fixture-id", std::slice::from_ref(&rule)),
                RuleEvidence::Unavailable,
                "{program}"
            );
        }
        assert_eq!(evidence(&vec![rule; 129]), RuleEvidence::Unavailable);
    }
    #[test]
    fn dev_build_is_missing_even_with_the_exact_rule_present() {
        let dev_rule = ComRule {
            application_name: DEV_PROGRAM.into(),
            ..exact()
        };
        assert_eq!(
            com_rule_evidence(DEV_PROGRAM, "fixture-id", std::slice::from_ref(&dev_rule)),
            RuleEvidence::Missing
        );
        assert_eq!(
            com_rule_evidence(
                r"\\?\C:\src\target\debug\crosspane-agent.exe",
                "fixture-id",
                &[dev_rule]
            ),
            RuleEvidence::Missing
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
    fn read(bytes: &[u8]) -> RecordedId {
        recorded_install_id(Some(bytes))
    }
    /// The installer's envelope with a `data` object that carries extra fields.
    fn record(kind: &str, schema: &str, id: &str) -> Vec<u8> {
        format!(
            r#"{{"schema_version":{schema},"kind":"{kind}","data":{{"install_id":"{id}","entries":[]}}}}"#
        )
        .into_bytes()
    }
    /// A valid record padded with a string to exactly `len` bytes.
    fn padded_record(len: usize) -> Vec<u8> {
        let head = |pad: &str| {
            format!(
                r#"{{"schema_version":1,"kind":"elevated-setup","data":{{"install_id":"abc","pad":"{pad}"}}}}"#
            )
        };
        head(&"x".repeat(len - head("").len())).into_bytes()
    }
    #[test]
    fn recorded_install_id_absent_is_not_unreadable() {
        assert_eq!(recorded_install_id(None), RecordedId::Absent);
    }
    #[test]
    fn recorded_install_id_reads_only_the_installer_envelope() {
        assert_eq!(
            read(&record("elevated-setup", "1", "fixture-id")),
            RecordedId::Id("fixture-id".into())
        );
        assert_eq!(
            read(&record("elevated-setup", "1", &"a".repeat(64))),
            RecordedId::Id("a".repeat(64))
        );
        assert_eq!(
            read(&record("elevated-setup", "1", "0123ABCD-ef")),
            RecordedId::Id("0123ABCD-ef".into())
        );
        assert_eq!(read(b""), RecordedId::Unreadable);
        assert_eq!(read(b"{not json"), RecordedId::Unreadable);
        assert_eq!(
            read(&record("receipt", "1", "fixture-id")),
            RecordedId::Unreadable
        );
        assert_eq!(
            read(&record("elevated-setup", "2", "fixture-id")),
            RecordedId::Unreadable
        );
        assert_eq!(
            read(&record("elevated-setup", "\"1\"", "fixture-id")),
            RecordedId::Unreadable
        );
        assert_eq!(
            read(br#"{"schema_version":1,"kind":"elevated-setup","data":{}}"#),
            RecordedId::Unreadable
        );
        assert_eq!(
            read(br#"{"schema_version":1,"kind":"elevated-setup","data":[]}"#),
            RecordedId::Unreadable
        );
        let too_long = "a".repeat(65);
        for id in ["*", "'", "", "fixture*", "a b", "a_b", too_long.as_str()] {
            assert_eq!(
                read(&record("elevated-setup", "1", id)),
                RecordedId::Unreadable,
                "{id:?}"
            );
        }
    }
    #[test]
    fn recorded_install_id_is_bounded_by_the_read_limit() {
        assert_eq!(
            read(&padded_record(MAX_RECORD_READ)),
            RecordedId::Id("abc".into())
        );
        assert_eq!(
            read(&padded_record(MAX_RECORD_READ + 1)),
            RecordedId::Unreadable
        );
    }
    #[test]
    fn rule_name_is_matched_exactly_per_install_id() {
        let mut variant = exact();
        variant.name = "crosspane.agent.udp.private.fixture-id".into();
        assert_eq!(evidence(&[variant]), RuleEvidence::Mismatch);
        // Another install ID under the same program covers this agent, so it is Mismatch.
        let covering = desired_rule(&scope_for("other-id", PROGRAM));
        assert_eq!(evidence(&[covering]), RuleEvidence::Mismatch);
        // The exact name under another program is a different rule, so it is Mismatch.
        let mut elsewhere = exact();
        elsewhere.application_name = OTHER_PROGRAM.into();
        assert_eq!(evidence(&[elsewhere]), RuleEvidence::Mismatch);
    }
}
