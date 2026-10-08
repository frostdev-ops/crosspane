//! Driver package and device model for the elevated helper (WP-W4.1c, block A2-driver). OS-free:
//! the native helper observes the facts, and this module decides what may be installed or removed.
//! Identities come from `drivers/windows-idd/CrosspaneIdd.inf`.
use super::{DriverState, HARDWARE_ID, is_published_name};

pub const DRIVER_INF: &str = "CrosspaneIdd.inf";
pub const DRIVER_CATALOG: &str = "CrosspaneIdd.cat";
pub const DRIVER_BINARY: &str = "CrosspaneIdd.dll";
pub const DRIVER_PROVIDER: &str = "Crosspane";
pub const DISPLAY_CLASS_GUID: &str = "{4D36E968-E325-11CE-BFC1-08002BE10318}";
pub const ROOT_PREFIX: &str = r"ROOT\";
pub const MAX_PACKAGES: usize = 16;
pub const MAX_DEVICES: usize = 16;

const NOT_SOURCE: &str = "the driver package next to the helper is not Crosspane's";
const FOREIGN_PACKAGE: &str = "a foreign package uses the Crosspane driver name";
const FOREIGN_DEVICE: &str = "a foreign device uses the Crosspane hardware ID";
const OVER_BOUNDS: &str = "the Crosspane driver inventory exceeds its bounds";
const SEVERAL_PACKAGES: &str = "more than one Crosspane driver package is staged";
const OTHER_VERSION: &str = "a different Crosspane driver version is staged";
const SEVERAL_DEVICES: &str = "more than one Crosspane adapter device exists";
const OTHER_DRIVER: &str = "the Crosspane adapter device uses another driver";
const BOUND_ELSEWHERE: &str = "the Crosspane adapter device is bound to another driver";

/// One driver store package, as observed, or the source package next to the helper.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFacts {
    /// `oemNN.inf`; empty for the source package next to the helper.
    pub published: String,
    pub original_inf: String,
    pub original_catalog: String,
    pub provider: String,
    pub class_guid: String,
    pub driver_version: String,
    pub hardware_ids: Vec<String>,
}

/// One observed device instance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceFacts {
    pub instance_id: String,
    pub hardware_ids: Vec<String>,
    pub driver_inf: Option<String>,
    pub problem: Option<u32>,
    pub present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Ours,
    Foreign,
    Unrelated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStep {
    Stage,
    CreateDevice,
    Bind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallPlan {
    Apply(Vec<InstallStep>),
    AlreadyInstalled,
    Mismatch(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalStep {
    RemoveDevice(String),
    DeletePackage(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalPlan {
    Apply(Vec<RemovalStep>),
    AlreadyAbsent,
    Mismatch(&'static str),
}

/// Classifies a driver store package. Names, the provider, the class GUID and hardware IDs compare
/// ASCII case-insensitively, as Windows compares them.
pub fn package_ownership(package: &PackageFacts) -> Ownership {
    if !package.original_inf.eq_ignore_ascii_case(DRIVER_INF) {
        return Ownership::Unrelated;
    }
    let identity = package
        .original_catalog
        .eq_ignore_ascii_case(DRIVER_CATALOG)
        && package.provider.eq_ignore_ascii_case(DRIVER_PROVIDER)
        && package.class_guid.eq_ignore_ascii_case(DISPLAY_CLASS_GUID)
        && has_hardware_id(&package.hardware_ids);
    let published = package.published.is_empty() || is_published_name(&package.published);
    if identity && published {
        Ownership::Ours
    } else {
        Ownership::Foreign
    }
}

/// Classifies a device instance. A root device with exactly our hardware ID is ours even when it
/// is absent, so that a ghost node can still be removed.
pub fn device_ownership(device: &DeviceFacts) -> Ownership {
    if !has_hardware_id(&device.hardware_ids) {
        return Ownership::Unrelated;
    }
    let root = is_root(&device.instance_id);
    if root && device.hardware_ids.len() == 1 {
        Ownership::Ours
    } else if !root && !device.present {
        Ownership::Unrelated
    } else {
        Ownership::Foreign
    }
}

/// Plans the one-time install from the source package, in order. Steps follow `InstallStep`'s
/// declaration order.
pub fn plan_install(
    source: &PackageFacts,
    packages: &[PackageFacts],
    devices: &[DeviceFacts],
) -> InstallPlan {
    if package_ownership(source) != Ownership::Ours || !source.published.is_empty() {
        return InstallPlan::Mismatch(NOT_SOURCE);
    }
    if packages.len() > MAX_PACKAGES || devices.len() > MAX_DEVICES {
        return InstallPlan::Mismatch(OVER_BOUNDS);
    }
    if packages.iter().any(not_manageable) {
        return InstallPlan::Mismatch(FOREIGN_PACKAGE);
    }
    if devices
        .iter()
        .any(|device| device_ownership(device) == Ownership::Foreign)
    {
        return InstallPlan::Mismatch(FOREIGN_DEVICE);
    }
    let ours_packages = owned_packages(packages);
    let ours_devices = owned_devices(devices);
    if ours_packages.len() > 1 {
        return InstallPlan::Mismatch(SEVERAL_PACKAGES);
    }
    let staged = ours_packages.first().copied();
    if staged.is_some_and(|package| package.driver_version != source.driver_version) {
        return InstallPlan::Mismatch(OTHER_VERSION);
    }
    if ours_devices.len() > 1 {
        return InstallPlan::Mismatch(SEVERAL_DEVICES);
    }
    let adapter = ours_devices.first().copied();
    let named = adapter.and_then(|device| device.driver_inf.as_deref());
    if named.is_some_and(|inf| !names_package(inf, &ours_packages)) {
        return InstallPlan::Mismatch(OTHER_DRIVER);
    }
    let mut steps = Vec::new();
    if staged.is_none() {
        steps.push(InstallStep::Stage);
    }
    match adapter {
        None => steps.push(InstallStep::CreateDevice),
        Some(device) => {
            if !staged.is_some_and(|package| is_bound(device, package)) {
                steps.push(InstallStep::Bind);
            }
        }
    }
    if steps.is_empty() {
        InstallPlan::AlreadyInstalled
    } else {
        InstallPlan::Apply(steps)
    }
}

/// Plans the removal: our devices first, then our packages, each sorted.
pub fn plan_removal(packages: &[PackageFacts], devices: &[DeviceFacts]) -> RemovalPlan {
    if packages.len() > MAX_PACKAGES || devices.len() > MAX_DEVICES {
        return RemovalPlan::Mismatch(OVER_BOUNDS);
    }
    if packages.iter().any(not_manageable) {
        return RemovalPlan::Mismatch(FOREIGN_PACKAGE);
    }
    if devices
        .iter()
        .any(|device| device_ownership(device) == Ownership::Foreign)
    {
        return RemovalPlan::Mismatch(FOREIGN_DEVICE);
    }
    let ours_packages = owned_packages(packages);
    let ours_devices = owned_devices(devices);
    let bound_elsewhere = ours_devices.iter().any(|device| {
        device.present
            && device.problem == Some(0)
            && device
                .driver_inf
                .as_deref()
                .is_some_and(|inf| !names_package(inf, &ours_packages))
    });
    if bound_elsewhere {
        return RemovalPlan::Mismatch(BOUND_ELSEWHERE);
    }
    let mut instances: Vec<String> = ours_devices
        .iter()
        .map(|device| device.instance_id.clone())
        .collect();
    instances.sort_unstable();
    let mut published: Vec<String> = ours_packages
        .iter()
        .map(|package| package.published.clone())
        .collect();
    published.sort_unstable();
    let steps: Vec<RemovalStep> = instances
        .into_iter()
        .map(RemovalStep::RemoveDevice)
        .chain(published.into_iter().map(RemovalStep::DeletePackage))
        .collect();
    if steps.is_empty() {
        RemovalPlan::AlreadyAbsent
    } else {
        RemovalPlan::Apply(steps)
    }
}

/// Summarises the observed Crosspane objects by their counts: no package and no device is
/// `Absent`; one package and one bound device is `Installed`; anything else that is not one of the
/// listed shapes is `Mismatch`.
pub fn driver_state(packages: &[PackageFacts], devices: &[DeviceFacts]) -> DriverState {
    if packages.iter().any(not_manageable)
        || devices
            .iter()
            .any(|device| device_ownership(device) == Ownership::Foreign)
    {
        return DriverState::Mismatch;
    }
    let ours_packages = owned_packages(packages);
    let ours_devices = owned_devices(devices);
    match (ours_packages.as_slice(), ours_devices.as_slice()) {
        ([], []) => DriverState::Absent,
        ([_], []) => DriverState::PackageOnly,
        ([package], [device]) if is_bound(device, package) => DriverState::Installed,
        ([] | [_], [_]) => DriverState::DeviceWithoutDriver,
        _ => DriverState::Mismatch,
    }
}

/// Present, healthy (`problem == Some(0)`) and driven by this package.
fn is_bound(device: &DeviceFacts, package: &PackageFacts) -> bool {
    device.present
        && device.problem == Some(0)
        && !package.published.is_empty()
        && device
            .driver_inf
            .as_deref()
            .is_some_and(|inf| inf.eq_ignore_ascii_case(&package.published))
}

/// Foreign, or observed without a published name. Such an object is never acted on, and it makes
/// the observed state a mismatch.
fn not_manageable(package: &PackageFacts) -> bool {
    package_ownership(package) == Ownership::Foreign || package.published.is_empty()
}

fn owned_packages(packages: &[PackageFacts]) -> Vec<&PackageFacts> {
    packages
        .iter()
        .filter(|package| package_ownership(package) == Ownership::Ours)
        .collect()
}

fn owned_devices(devices: &[DeviceFacts]) -> Vec<&DeviceFacts> {
    devices
        .iter()
        .filter(|device| device_ownership(device) == Ownership::Ours)
        .collect()
}

/// True when `inf` names one of `packages` (ASCII case-insensitive).
fn names_package(inf: &str, packages: &[&PackageFacts]) -> bool {
    packages
        .iter()
        .any(|package| inf.eq_ignore_ascii_case(&package.published))
}

fn has_hardware_id(ids: &[String]) -> bool {
    ids.iter().any(|id| id.eq_ignore_ascii_case(HARDWARE_ID))
}

fn is_root(instance_id: &str) -> bool {
    instance_id
        .as_bytes()
        .get(..ROOT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ROOT_PREFIX.as_bytes()))
}
