//! Firewall rule model for the elevated helper (WP-W4.1c T9). OS-free: the COM adapter reads the
//! rule family in process and passes it here, so every decision is a pure function of that data.
use super::{FirewallState, RuleScope};

pub const NET_FW_IP_PROTOCOL_UDP: i32 = 17;
pub const NET_FW_RULE_DIR_IN: i32 = 1;
pub const NET_FW_ACTION_ALLOW: i32 = 1;
pub const NET_FW_PROFILE2_PRIVATE: i32 = 2;
pub const NET_FW_EDGE_TRAVERSAL_TYPE_DENY: i32 = 0;
pub const NET_FW_AUTHENTICATE_NONE: i32 = 0;
pub const REMOTE_ADDRESSES: &str = "LocalSubnet";
pub const RULE_DESCRIPTION: &str = "Lets the Crosspane agent receive connections from peers on your local subnet. Added by Crosspane setup.";
pub const MAX_FAMILY: usize = 128;

const OVER_LIMIT: &str = "too many Crosspane firewall rules";
const COVERED: &str = "another Crosspane rule already covers this agent";
const DIFFERS: &str = "the Crosspane firewall rule differs from its specification";

/// INetFwRule/2/3 values as COM reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComRule {
    pub name: String,
    pub description: String,
    pub application_name: String,
    pub service_name: String,
    pub protocol: i32,
    pub local_ports: String,
    pub remote_ports: String,
    pub local_addresses: String,
    pub remote_addresses: String,
    pub icmp_types_and_codes: String,
    pub direction: i32,
    pub interfaces_any: bool,
    pub interface_types: String,
    pub enabled: bool,
    pub grouping: String,
    pub profiles: i32,
    pub edge_traversal: bool,
    pub action: i32,
    pub edge_traversal_options: i32,
    pub local_app_package_id: String,
    pub local_user_owner: String,
    pub local_user_authorized_list: String,
    pub remote_user_authorized_list: String,
    pub remote_machine_authorized_list: String,
    pub secure_flags: i32,
}

/// The rule the helper creates for `scope`. `rule_matches` accepts exactly this rule.
pub fn desired_rule(scope: &RuleScope) -> ComRule {
    ComRule {
        name: scope.id.rule_name(),
        // Documented text for the operator. `rule_matches` does not compare it.
        description: RULE_DESCRIPTION.to_owned(),
        application_name: scope.program.as_str().to_owned(),
        protocol: NET_FW_IP_PROTOCOL_UDP,
        local_ports: "*".to_owned(),
        remote_ports: "*".to_owned(),
        local_addresses: "*".to_owned(),
        remote_addresses: REMOTE_ADDRESSES.to_owned(),
        direction: NET_FW_RULE_DIR_IN,
        interfaces_any: true,
        interface_types: "All".to_owned(),
        enabled: true,
        grouping: scope.id.rule_group(),
        profiles: NET_FW_PROFILE2_PRIVATE,
        action: NET_FW_ACTION_ALLOW,
        edge_traversal_options: NET_FW_EDGE_TRAVERSAL_TYPE_DENY,
        secure_flags: NET_FW_AUTHENTICATE_NONE,
        ..Default::default()
    }
}

/// True when `observed` is the rule `desired_rule(scope)` describes. Ports, addresses and the ICMP
/// filter accept COM's wildcard spellings. `description` is not compared.
pub fn rule_matches(observed: &ComRule, scope: &RuleScope) -> bool {
    let desired = desired_rule(scope);
    observed.name == desired.name
        && observed.grouping == desired.grouping
        && scope.program.same_path(&observed.application_name)
        && observed.protocol == desired.protocol
        && observed.direction == desired.direction
        && observed.action == desired.action
        && observed.profiles == desired.profiles
        && observed.enabled == desired.enabled
        && observed.edge_traversal == desired.edge_traversal
        && observed.edge_traversal_options == desired.edge_traversal_options
        && observed.secure_flags == desired.secure_flags
        && observed
            .remote_addresses
            .eq_ignore_ascii_case(REMOTE_ADDRESSES)
        && is_wildcard(&observed.local_ports)
        && is_wildcard(&observed.remote_ports)
        && is_wildcard(&observed.local_addresses)
        && is_wildcard(&observed.icmp_types_and_codes)
        && matches!(observed.interface_types.as_str(), "All" | "")
        && observed.interfaces_any
        && observed.service_name.is_empty()
        && observed.local_app_package_id.is_empty()
        && observed.local_user_owner.is_empty()
        && observed.local_user_authorized_list.is_empty()
        && observed.remote_user_authorized_list.is_empty()
        && observed.remote_machine_authorized_list.is_empty()
}

/// COM's "any" spellings for a port, address or ICMP filter.
fn is_wildcard(text: &str) -> bool {
    text.is_empty() || text == "*"
}

/// What the helper does with one rule. `Mismatch` carries the reason shown to the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallPlan {
    Add,
    AlreadyPresent,
    Remove,
    AlreadyAbsent,
    Mismatch(&'static str),
}

/// The family-wide checks shared by every plan, then the single rule with the exact name. Names
/// compare ASCII case-insensitively, as the rule store does. A case variant is therefore the same
/// rule, and it can only be a mismatch, never an invisible duplicate.
fn exact_rule<'a>(
    family: &'a [ComRule],
    scope: &RuleScope,
) -> Result<Option<&'a ComRule>, &'static str> {
    if family.len() > MAX_FAMILY {
        return Err(OVER_LIMIT);
    }
    let name = scope.id.rule_name();
    let mut exact = None;
    let mut count = 0;
    for rule in family {
        let same_name = rule.name.eq_ignore_ascii_case(&name);
        if !same_name && scope.program.same_path(&rule.application_name) {
            return Err(COVERED);
        }
        if same_name {
            count += 1;
            exact = Some(rule);
        }
    }
    match count {
        0 => Ok(None),
        1 => Ok(exact),
        _ => Err(DIFFERS),
    }
}

/// Adds the rule when the family has none under the exact name.
pub fn plan_add(family: &[ComRule], scope: &RuleScope) -> FirewallPlan {
    match exact_rule(family, scope) {
        Err(reason) => FirewallPlan::Mismatch(reason),
        Ok(None) => FirewallPlan::Add,
        Ok(Some(rule)) if rule_matches(rule, scope) => FirewallPlan::AlreadyPresent,
        Ok(Some(_)) => FirewallPlan::Mismatch(DIFFERS),
    }
}

/// Removes the rule only when exactly one rule under the exact name matches the specification.
pub fn plan_remove(family: &[ComRule], scope: &RuleScope) -> FirewallPlan {
    match exact_rule(family, scope) {
        Err(reason) => FirewallPlan::Mismatch(reason),
        Ok(None) => FirewallPlan::AlreadyAbsent,
        Ok(Some(rule)) if rule_matches(rule, scope) => FirewallPlan::Remove,
        Ok(Some(_)) => FirewallPlan::Mismatch(DIFFERS),
    }
}

/// `None` means no firewall rule is requested. Otherwise the state follows the same rules as the
/// plans.
pub fn firewall_state(family: &[ComRule], scope: Option<&RuleScope>) -> FirewallState {
    let Some(scope) = scope else {
        return FirewallState::NotRequested;
    };
    match exact_rule(family, scope) {
        Err(_) => FirewallState::Mismatch,
        Ok(None) => FirewallState::Missing,
        Ok(Some(rule)) if rule_matches(rule, scope) => FirewallState::Present,
        Ok(Some(_)) => FirewallState::Mismatch,
    }
}
