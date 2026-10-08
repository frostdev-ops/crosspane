//! Read-only reader for Crosspane's firewall rule family (WP-W1.8b T3). The agent runs it in
//! process through COM `INetFwPolicy2` on the calling thread. It creates the policy object and
//! calls getters only: no setter, `Add` or `Remove` is ever called, and no process is started.
//! Foreign rule names are compared as raw UTF-16 and are never converted, stored or logged.

use crosspane_installer_core::elevated::{
    RULE_PREFIX,
    firewall::{ComRule, MAX_FAMILY},
};
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        NetworkManagement::WindowsFirewall::{
            INetFwPolicy2, INetFwRule, INetFwRule2, INetFwRule3, NetFwPolicy2,
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

/// The longest COM string this reader accepts, in UTF-16 units.
pub(crate) const MAX_TEXT_UNITS: usize = 32_768;

/// The most rules enumerated in one read, Crosspane's and other products' alike.
pub(crate) const MAX_SCANNED: usize = 65_536;

/// Why a read produced no family. `Com` carries an HRESULT only, never a name or a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// A COM call failed with this HRESULT.
    Com(i32),
    /// More rules, or a longer string, than the caps allow.
    Oversize,
    /// A string is not valid UTF-16 or holds a NUL, a COM item is not a dispatch, or the thread
    /// already has another apartment model.
    Unavailable,
    /// The caller set `stop`.
    Stopped,
    /// The deadline passed.
    Deadline,
}

/// Every rule whose name starts with `RULE_PREFIX` (ASCII case-insensitive), with all
/// `INetFwRule`/`2`/`3` getters read. Checks `stop` and `deadline` before each `Next`. Runs
/// `CoInitializeEx(MTA)` on the calling thread and balances it on every path.
pub(crate) fn read_family(stop: &AtomicBool, deadline: Instant) -> Result<Vec<ComRule>, ReadError> {
    ensure_running(stop, deadline)?;
    // Declared FIRST, so it is dropped LAST: COM references below are released before the
    // apartment is uninitialized.
    let _apartment = Apartment::new()?;
    // SAFETY: creates the in-process `NetFwPolicy2` object on the apartment initialized above.
    // Only read-only members are called on it and on the objects it returns.
    let policy: INetFwPolicy2 =
        unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }.map_err(com)?;
    // SAFETY: a read-only getter on the policy object created above.
    let rules = unsafe { policy.Rules() }.map_err(com)?;
    // SAFETY: a read-only enumerator over the live rule collection, converted to `IEnumVARIANT`
    // before use.
    let unknown = unsafe { rules._NewEnum() }.map_err(com)?;
    let enumerator: IEnumVARIANT = unknown.cast().map_err(com)?;
    let mut family = Vec::new();
    let mut scanned = 0_usize;
    loop {
        ensure_running(stop, deadline)?;
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
        scanned += 1;
        if scanned > MAX_SCANNED {
            return Err(ReadError::Oversize);
        }
        let rule: INetFwRule = dispatch_of(&slot.0)?.cast().map_err(com)?;
        // SAFETY: a read-only getter on a rule object from the collection read above.
        let name = unsafe { rule.Name() }.map_err(com)?;
        // Foreign names get the length cap only, and are compared raw. Nothing is converted here.
        if name.len() > MAX_TEXT_UNITS {
            return Err(ReadError::Oversize);
        }
        if !has_family_prefix(&name) {
            continue;
        }
        // A family member is validated in full before any of its text is used.
        check_units(&name)?;
        if family.len() >= MAX_FAMILY {
            return Err(ReadError::Oversize);
        }
        family.push(read_rule(&rule, &name)?);
    }
}

/// Stops the read when `stop` is set, and reports `Deadline` once `deadline` has passed.
fn ensure_running(stop: &AtomicBool, deadline: Instant) -> Result<(), ReadError> {
    if stop.load(Ordering::Acquire) {
        return Err(ReadError::Stopped);
    }
    if Instant::now() >= deadline {
        return Err(ReadError::Deadline);
    }
    Ok(())
}

/// `CoInitializeEx(MTA)` on this thread, balanced on drop. The guard is not `Send`.
struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn new() -> Result<Self, ReadError> {
        // SAFETY: initializes COM only for the calling thread. `Drop` balances a success, and the
        // guard cannot leave this thread.
        let hresult = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // A thread already in another apartment model is not ours to change, so nothing is
        // balanced on this path.
        if hresult == RPC_E_CHANGED_MODE {
            return Err(ReadError::Unavailable);
        }
        hresult.ok().map_err(com)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful `CoInitializeEx` on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// A COM failure as its HRESULT. The HRESULT carries no rule text.
fn com(error: windows::core::Error) -> ReadError {
    ReadError::Com(error.code().0)
}

/// A VARIANT this reader owns. `VariantClear` runs on drop and releases whatever it holds.
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
fn dispatch_of(value: &VARIANT) -> Result<IDispatch, ReadError> {
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
    dispatch.ok_or(ReadError::Unavailable)
}

/// Reads every COM-visible property of one family rule. `name` is the name the caller already read.
fn read_rule(rule: &INetFwRule, name: &BSTR) -> Result<ComRule, ReadError> {
    let rule2: INetFwRule2 = rule.cast().map_err(com)?;
    let rule3: INetFwRule3 = rule.cast().map_err(com)?;
    // SAFETY: read-only getters on rule objects from the collection. The Interfaces VARIANT is
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

/// Checks one COM string before anything is done with it: at most `MAX_TEXT_UNITS` UTF-16 units
/// (`Oversize`), then no NUL and valid UTF-16 (`Unavailable`). Nothing is converted or kept.
fn check_units(value: &[u16]) -> Result<(), ReadError> {
    if value.len() > MAX_TEXT_UNITS {
        return Err(ReadError::Oversize);
    }
    let valid = char::decode_utf16(value.iter().copied())
        .all(|unit| matches!(unit, Ok(character) if character != '\0'));
    if valid {
        Ok(())
    } else {
        Err(ReadError::Unavailable)
    }
}

/// A COM string as UTF-8, after `check_units`.
fn text(value: &BSTR) -> Result<String, ReadError> {
    check_units(value)?;
    char::decode_utf16(value.iter().copied())
        .map(|unit| unit.map_err(|_| ReadError::Unavailable))
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
