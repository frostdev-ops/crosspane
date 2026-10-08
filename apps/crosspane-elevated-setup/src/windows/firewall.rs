//! Firewall adapter (WP-W4.1c T16): reads and edits Crosspane's own rules through COM
//! `INetFwPolicy2` on one owned MTA thread. Other products' rule names are checked on raw UTF-16
//! units and never converted, stored or printed.

use super::{HelperError, HelperResult};
use crosspane_installer_core::elevated::{
    RULE_PREFIX,
    firewall::{ComRule, MAX_FAMILY},
};
use std::{marker::PhantomData, rc::Rc};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        NetworkManagement::WindowsFirewall::{
            INetFwPolicy2, INetFwRule, INetFwRule2, INetFwRule3, INetFwRules, NET_FW_ACTION,
            NET_FW_MODIFY_STATE_OK, NET_FW_RULE_DIRECTION, NetFwPolicy2, NetFwRule,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IDispatch,
            },
            Ole::IEnumVARIANT,
            Variant::{VARIANT, VT_DISPATCH, VT_EMPTY, VT_NULL, VariantClear},
        },
    },
    core::{BSTR, Interface},
};

/// The longest COM string the helper reads, in UTF-16 units.
const MAX_TEXT_UNITS: usize = 32_768;

/// `CoInitializeEx(MTA)` on this thread, balanced on drop. The guard is not `Send`.
struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn new() -> HelperResult<Self> {
        // SAFETY: initializes COM only for the calling thread. `Drop` balances it, and the guard
        // cannot leave this thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(com)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// One COM session over the firewall policy. Every call runs on the thread that opened it.
pub(crate) struct Firewall {
    policy: INetFwPolicy2,
    // Declared LAST: COM references are released before the apartment is uninitialized.
    _apartment: Apartment,
}

impl Firewall {
    /// Opens the session. The apartment comes first, so a failed open uninitializes it again.
    pub(crate) fn open() -> HelperResult<Self> {
        let apartment = Apartment::new()?;
        // SAFETY: creates the in-process `NetFwPolicy2` object on the apartment initialized above.
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }.map_err(com)?;
        Ok(Self {
            policy,
            _apartment: apartment,
        })
    }

    /// Every rule whose name starts with `RULE_PREFIX`, with its properties read. Other rules are
    /// checked by name only. More than `MAX_FAMILY` such rules is `Oversize`.
    pub(crate) fn family(&self) -> HelperResult<Vec<ComRule>> {
        let rules = self.rules()?;
        // SAFETY: a read-only enumerator over this session's live rule collection.
        let unknown = unsafe { rules._NewEnum() }.map_err(com)?;
        let enumerator: IEnumVARIANT = unknown.cast().map_err(com)?;
        let mut family = Vec::new();
        loop {
            let mut slot = OwnedVariant(VARIANT::default());
            let mut fetched = 0_u32;
            // SAFETY: one writable VARIANT slot and a valid out-count. `slot` clears what `Next`
            // fills in, on every path.
            unsafe { enumerator.Next(std::slice::from_mut(&mut slot.0), &mut fetched) }
                .ok()
                .map_err(com)?;
            if fetched == 0 {
                return Ok(family);
            }
            let rule: INetFwRule = dispatch_of(&slot.0)?.cast().map_err(com)?;
            // SAFETY: a read-only getter on a rule object owned by this apartment.
            let name = unsafe { rule.Name() }.map_err(com)?;
            if !has_family_prefix(&name) {
                continue;
            }
            if family.len() >= MAX_FAMILY {
                return Err(HelperError::Oversize);
            }
            family.push(read_rule(&rule, &name)?);
        }
    }

    /// Creates one rule from `rule`, setting its COM properties in the fixed order, then adds it.
    /// Names outside Crosspane's prefix are refused before anything is created.
    pub(crate) fn add(&self, rule: &ComRule) -> HelperResult<()> {
        if !is_crosspane_name(&rule.name) {
            return Err(HelperError::Refused(
                "firewall rule is not a Crosspane rule",
            ));
        }
        // SAFETY: creates an in-process rule object on the initialized apartment, with no outer
        // aggregate.
        let created: INetFwRule =
            unsafe { CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER) }.map_err(com)?;
        // SAFETY: setters on the rule created above, which is not in the collection yet. COM copies
        // every BSTR it is given.
        unsafe {
            created
                .SetName(&BSTR::from(rule.name.as_str()))
                .map_err(com)?;
            created
                .SetDescription(&BSTR::from(rule.description.as_str()))
                .map_err(com)?;
            created
                .SetApplicationName(&BSTR::from(rule.application_name.as_str()))
                .map_err(com)?;
            created.SetProtocol(rule.protocol).map_err(com)?;
            created
                .SetRemoteAddresses(&BSTR::from(rule.remote_addresses.as_str()))
                .map_err(com)?;
            created
                .SetDirection(NET_FW_RULE_DIRECTION(rule.direction))
                .map_err(com)?;
            created
                .SetEnabled(VARIANT_BOOL::from(rule.enabled))
                .map_err(com)?;
            created
                .SetGrouping(&BSTR::from(rule.grouping.as_str()))
                .map_err(com)?;
            created.SetProfiles(rule.profiles).map_err(com)?;
            created
                .SetEdgeTraversal(VARIANT_BOOL::from(rule.edge_traversal))
                .map_err(com)?;
            created.SetAction(NET_FW_ACTION(rule.action)).map_err(com)?;
            created
                .cast::<INetFwRule2>()
                .map_err(com)?
                .SetEdgeTraversalOptions(rule.edge_traversal_options)
                .map_err(com)?;
            created
                .cast::<INetFwRule3>()
                .map_err(com)?
                .SetSecureFlags(rule.secure_flags)
                .map_err(com)?;
            self.rules()?.Add(&created).map_err(com)?;
        }
        Ok(())
    }

    /// Removes the rule with exactly this name. Names outside Crosspane's prefix are refused.
    pub(crate) fn remove(&self, name: &str) -> HelperResult<()> {
        if !is_crosspane_name(name) {
            return Err(HelperError::Refused(
                "firewall rule is not a Crosspane rule",
            ));
        }
        let name = BSTR::from(name);
        // SAFETY: removes by exact name through this session's live rule collection.
        unsafe { self.rules()?.Remove(&name) }.map_err(com)
    }

    /// True when local firewall rules apply, that is `LocalPolicyModifyState` is `OK`.
    pub(crate) fn local_rules_apply(&self) -> HelperResult<bool> {
        // SAFETY: a read-only getter on this session's live policy object.
        let state = unsafe { self.policy.LocalPolicyModifyState() }.map_err(com)?;
        Ok(state.0 == NET_FW_MODIFY_STATE_OK.0)
    }

    fn rules(&self) -> HelperResult<INetFwRules> {
        // SAFETY: a read-only getter on this session's live policy object.
        unsafe { self.policy.Rules() }.map_err(com)
    }
}

/// A COM failure as its HRESULT. The HRESULT carries no rule text.
fn com(error: windows::core::Error) -> HelperError {
    HelperError::Com(error.code().0)
}

/// A VARIANT this helper owns. `VariantClear` runs on drop and releases whatever it holds.
struct OwnedVariant(VARIANT);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: the VARIANT is exclusively owned here, and clearing it releases only the
        // references it holds.
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

/// The dispatch an enumerated item holds. The reference is cloned, so the VARIANT still releases
/// its own. Any other type, or a null dispatch, is refused.
fn dispatch_of(value: &VARIANT) -> HelperResult<IDispatch> {
    // SAFETY: reads the tag first, and the dispatch member only when that tag selects it. COM wrote
    // the VARIANT.
    let dispatch = unsafe {
        let inner = &value.Anonymous.Anonymous;
        if inner.vt == VT_DISPATCH {
            (*inner.Anonymous.pdispVal).clone()
        } else {
            None
        }
    };
    dispatch.ok_or(HelperError::Unavailable("firewall rule item"))
}

/// Reads every COM-visible property of one family rule. `name` is the name the caller already read.
fn read_rule(rule: &INetFwRule, name: &BSTR) -> HelperResult<ComRule> {
    let rule2: INetFwRule2 = rule.cast().map_err(com)?;
    let rule3: INetFwRule3 = rule.cast().map_err(com)?;
    // SAFETY: read-only getters on rule objects owned by this apartment. The Interfaces VARIANT is
    // wrapped as soon as it is returned, so it is cleared on every path.
    unsafe {
        let interfaces = OwnedVariant(rule.Interfaces().map_err(com)?);
        Ok(ComRule {
            name: text(name)?,
            description: text(&rule.Description().map_err(com)?)?,
            application_name: text(&rule.ApplicationName().map_err(com)?)?,
            service_name: text(&rule.ServiceName().map_err(com)?)?,
            protocol: rule.Protocol().map_err(com)?,
            local_ports: text(&rule.LocalPorts().map_err(com)?)?,
            remote_ports: text(&rule.RemotePorts().map_err(com)?)?,
            local_addresses: text(&rule.LocalAddresses().map_err(com)?)?,
            remote_addresses: text(&rule.RemoteAddresses().map_err(com)?)?,
            icmp_types_and_codes: text(&rule.IcmpTypesAndCodes().map_err(com)?)?,
            direction: rule.Direction().map_err(com)?.0,
            interfaces_any: is_any_interface(&interfaces.0),
            interface_types: text(&rule.InterfaceTypes().map_err(com)?)?,
            enabled: rule.Enabled().map_err(com)?.as_bool(),
            grouping: text(&rule.Grouping().map_err(com)?)?,
            profiles: rule.Profiles().map_err(com)?,
            edge_traversal: rule.EdgeTraversal().map_err(com)?.as_bool(),
            action: rule.Action().map_err(com)?.0,
            edge_traversal_options: rule2.EdgeTraversalOptions().map_err(com)?,
            local_app_package_id: text(&rule3.LocalAppPackageId().map_err(com)?)?,
            local_user_owner: text(&rule3.LocalUserOwner().map_err(com)?)?,
            local_user_authorized_list: text(&rule3.LocalUserAuthorizedList().map_err(com)?)?,
            remote_user_authorized_list: text(&rule3.RemoteUserAuthorizedList().map_err(com)?)?,
            remote_machine_authorized_list: text(
                &rule3.RemoteMachineAuthorizedList().map_err(com)?,
            )?,
            secure_flags: rule3.SecureFlags().map_err(com)?,
        })
    }
}

/// True for the "any interface" VARIANT that COM reports: empty or null.
fn is_any_interface(value: &VARIANT) -> bool {
    // SAFETY: reads the tag of a VARIANT that COM filled in. The tag is valid for every VARIANT.
    let tag = unsafe { value.Anonymous.Anonymous.vt };
    tag == VT_EMPTY || tag == VT_NULL
}

/// A COM string as UTF-8, at most `MAX_TEXT_UNITS` UTF-16 units. NUL or invalid UTF-16 is refused.
fn text(value: &BSTR) -> HelperResult<String> {
    if value.len() > MAX_TEXT_UNITS {
        return Err(HelperError::Oversize);
    }
    char::decode_utf16(value.iter().copied())
        .map(|unit| match unit {
            Ok('\0') | Err(_) => Err(HelperError::Unavailable("firewall rule text")),
            Ok(character) => Ok(character),
        })
        .collect()
}

/// True when a name, as UTF-16 units, starts with `RULE_PREFIX`. The comparison is ASCII
/// case-insensitive, as the rule store compares names. Nothing is converted or kept.
fn has_family_prefix(name: &[u16]) -> bool {
    let prefix = RULE_PREFIX.as_bytes();
    name.len() >= prefix.len()
        && name.iter().zip(prefix).all(|(unit, byte)| {
            u8::try_from(*unit).is_ok_and(|unit| unit.eq_ignore_ascii_case(byte))
        })
}

/// True when a name starts with `RULE_PREFIX`, ASCII case-insensitively.
fn is_crosspane_name(name: &str) -> bool {
    let prefix = RULE_PREFIX.as_bytes();
    name.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}
