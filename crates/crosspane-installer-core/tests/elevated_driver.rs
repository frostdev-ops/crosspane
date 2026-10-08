//! T13: the driver package and device model (WP-W4.1c, block A2-driver). Pure facts in, plans
//! out: ownership, the install steps, version and foreign-object mismatches, the removal order and
//! the driver state table.
use crosspane_installer_core::elevated::DriverState;
use crosspane_installer_core::elevated::driver::{
    DISPLAY_CLASS_GUID, DRIVER_CATALOG, DRIVER_INF, DRIVER_PROVIDER, DeviceFacts, InstallPlan,
    InstallStep, MAX_DEVICES, MAX_PACKAGES, Ownership, PackageFacts, RemovalPlan, RemovalStep,
    device_ownership, driver_state, package_ownership, plan_install, plan_removal,
};

const VERSION: &str = "0.1.0.0";
const HARDWARE: &str = r"Crosspane\IddTwinV1";
const ADAPTER: &str = r"ROOT\CROSSPANEIDD\0000";

const NOT_SOURCE: &str = "the driver package next to the helper is not Crosspane's";
const FOREIGN_PACKAGE: &str = "a foreign package uses the Crosspane driver name";
const FOREIGN_DEVICE: &str = "a foreign device uses the Crosspane hardware ID";
const OVER_BOUNDS: &str = "the Crosspane driver inventory exceeds its bounds";
const SEVERAL_PACKAGES: &str = "more than one Crosspane driver package is staged";
const OTHER_VERSION: &str = "a different Crosspane driver version is staged";
const SEVERAL_DEVICES: &str = "more than one Crosspane adapter device exists";
const OTHER_DRIVER: &str = "the Crosspane adapter device uses another driver";
const BOUND_ELSEWHERE: &str = "the Crosspane adapter device is bound to another driver";

/// A Crosspane package as the driver store reports it. An empty `published` is the source.
fn package(published: &str, version: &str) -> PackageFacts {
    PackageFacts {
        published: published.to_owned(),
        original_inf: DRIVER_INF.to_owned(),
        original_catalog: DRIVER_CATALOG.to_owned(),
        provider: DRIVER_PROVIDER.to_owned(),
        class_guid: DISPLAY_CLASS_GUID.to_owned(),
        driver_version: version.to_owned(),
        hardware_ids: vec![HARDWARE.to_owned()],
    }
}

fn source() -> PackageFacts {
    package("", VERSION)
}

fn adapter_at(
    instance: &str,
    driver_inf: Option<&str>,
    problem: Option<u32>,
    present: bool,
) -> DeviceFacts {
    DeviceFacts {
        instance_id: instance.to_owned(),
        hardware_ids: vec![HARDWARE.to_owned()],
        driver_inf: driver_inf.map(str::to_owned),
        problem,
        present,
    }
}

fn adapter(driver_inf: Option<&str>, problem: Option<u32>, present: bool) -> DeviceFacts {
    adapter_at(ADAPTER, driver_inf, problem, present)
}

fn unrelated_device() -> DeviceFacts {
    DeviceFacts {
        instance_id: r"ROOT\OTHER\0000".to_owned(),
        hardware_ids: vec![r"Other\Device".to_owned()],
        driver_inf: Some("oem9.inf".to_owned()),
        problem: Some(0),
        present: true,
    }
}

/// A software device with our hardware ID. Only the root enumerator is ours.
fn swd_device(present: bool) -> DeviceFacts {
    DeviceFacts {
        instance_id: r"SWD\CROSSPANEIDD\0000".to_owned(),
        hardware_ids: vec![HARDWARE.to_owned()],
        driver_inf: None,
        problem: Some(0),
        present,
    }
}

fn remove_device(instance: &str) -> RemovalStep {
    RemovalStep::RemoveDevice(instance.to_owned())
}

fn delete_package(published: &str) -> RemovalStep {
    RemovalStep::DeletePackage(published.to_owned())
}

#[test]
fn install_steps_follow_the_staged_package_and_the_adapter() {
    let staged = package("oem3.inf", VERSION);
    let cases: [(Option<PackageFacts>, Option<DeviceFacts>, InstallPlan); 7] = [
        (
            None,
            None,
            InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::CreateDevice]),
        ),
        (
            None,
            Some(adapter(None, Some(28), true)),
            InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::Bind]),
        ),
        (
            None,
            Some(adapter(None, None, false)),
            InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::Bind]),
        ),
        (
            Some(staged.clone()),
            None,
            InstallPlan::Apply(vec![InstallStep::CreateDevice]),
        ),
        (
            Some(staged.clone()),
            Some(adapter(Some("oem3.inf"), Some(28), true)),
            InstallPlan::Apply(vec![InstallStep::Bind]),
        ),
        (
            Some(staged.clone()),
            Some(adapter(Some("oem3.inf"), Some(0), false)),
            InstallPlan::Apply(vec![InstallStep::Bind]),
        ),
        (
            Some(staged),
            Some(adapter(Some("oem3.inf"), Some(0), true)),
            InstallPlan::AlreadyInstalled,
        ),
    ];
    for (packages, device, expected) in cases {
        let packages: Vec<PackageFacts> = packages.into_iter().collect();
        let devices: Vec<DeviceFacts> = device.into_iter().collect();
        assert_eq!(
            plan_install(&source(), &packages, &devices),
            expected,
            "packages {}, devices {}",
            packages.len(),
            devices.len()
        );
    }
}

#[test]
fn install_binds_an_existing_unbound_device_to_the_staged_or_a_new_package() {
    let staged = [package("oem3.inf", VERSION)];
    // An unnamed device with no driver is not bound, so the staged package binds it.
    let unnamed = [adapter(None, Some(0), true)];
    assert_eq!(
        plan_install(&source(), &staged, &unnamed),
        InstallPlan::Apply(vec![InstallStep::Bind])
    );
    // No package and a device that is not bound: stage, then bind.
    assert_eq!(
        plan_install(&source(), &[], &unnamed),
        InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::Bind])
    );
}

#[test]
fn install_refuses_a_device_bound_to_a_driver_other_than_the_staged_one() {
    let staged = [package("oem3.inf", VERSION)];
    let other = [adapter(Some("oem9.inf"), Some(0), true)];
    assert_eq!(
        plan_install(&source(), &staged, &other),
        InstallPlan::Mismatch(OTHER_DRIVER)
    );
    assert_eq!(
        plan_install(&source(), &[], &other),
        InstallPlan::Mismatch(OTHER_DRIVER)
    );
    // The name comparison ignores ASCII case, so this one is already installed.
    let upper = [adapter(Some("OEM3.INF"), Some(0), true)];
    assert_eq!(
        plan_install(&source(), &staged, &upper),
        InstallPlan::AlreadyInstalled
    );
}

#[test]
fn install_refuses_a_staged_package_of_another_version() {
    let old = [package("oem3.inf", "0.0.9.0")];
    assert_eq!(
        plan_install(&source(), &old, &[]),
        InstallPlan::Mismatch(OTHER_VERSION)
    );
    let bound = [adapter(Some("oem3.inf"), Some(0), true)];
    assert_eq!(
        plan_install(&source(), &old, &bound),
        InstallPlan::Mismatch(OTHER_VERSION)
    );
}

#[test]
fn install_refuses_a_source_that_is_not_the_crosspane_package() {
    let mut foreign = source();
    foreign.provider = "Contoso".to_owned();
    assert_eq!(
        plan_install(&foreign, &[], &[]),
        InstallPlan::Mismatch(NOT_SOURCE)
    );
    let mut other_inf = source();
    other_inf.original_inf = "Other.inf".to_owned();
    assert_eq!(
        plan_install(&other_inf, &[], &[]),
        InstallPlan::Mismatch(NOT_SOURCE)
    );
    let named = package("oem5.inf", VERSION);
    assert_eq!(
        plan_install(&named, &[], &[]),
        InstallPlan::Mismatch(NOT_SOURCE)
    );
}

#[test]
fn a_foreign_or_unnamed_package_blocks_install_removal_and_state() {
    let mut foreign = package("oem7.inf", VERSION);
    foreign.provider = "Contoso".to_owned();
    let unnamed = package("", VERSION);
    for packages in [vec![foreign], vec![unnamed]] {
        assert_eq!(
            plan_install(&source(), &packages, &[]),
            InstallPlan::Mismatch(FOREIGN_PACKAGE)
        );
        assert_eq!(
            plan_removal(&packages, &[]),
            RemovalPlan::Mismatch(FOREIGN_PACKAGE)
        );
        assert_eq!(driver_state(&packages, &[]), DriverState::Mismatch);
    }
}

#[test]
fn a_foreign_device_blocks_install_removal_and_state() {
    // A root device with our hardware ID and another one: not exactly one, so foreign.
    let mut extra = adapter(None, Some(0), true);
    extra.hardware_ids.push(r"Other\Device".to_owned());
    let software = swd_device(true);
    for device in [extra, software] {
        let devices = [device];
        assert_eq!(
            plan_install(&source(), &[], &devices),
            InstallPlan::Mismatch(FOREIGN_DEVICE)
        );
        assert_eq!(
            plan_removal(&[], &devices),
            RemovalPlan::Mismatch(FOREIGN_DEVICE)
        );
        assert_eq!(driver_state(&[], &devices), DriverState::Mismatch);
    }
}

#[test]
fn an_swd_device_is_ignored_when_absent_and_refused_when_present() {
    let absent = [swd_device(false)];
    assert_eq!(
        device_ownership(&absent[0]),
        Ownership::Unrelated,
        "an absent SWD device is not ours"
    );
    assert_eq!(
        plan_install(&source(), &[], &absent),
        InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::CreateDevice])
    );
    assert_eq!(driver_state(&[], &absent), DriverState::Absent);
    assert_eq!(plan_removal(&[], &absent), RemovalPlan::AlreadyAbsent);

    let present = [swd_device(true)];
    assert_eq!(
        device_ownership(&present[0]),
        Ownership::Foreign,
        "a present SWD device with our hardware ID is foreign"
    );
    assert_eq!(
        plan_install(&source(), &[], &present),
        InstallPlan::Mismatch(FOREIGN_DEVICE)
    );
    assert_eq!(
        plan_removal(&[], &present),
        RemovalPlan::Mismatch(FOREIGN_DEVICE)
    );
    assert_eq!(driver_state(&[], &present), DriverState::Mismatch);
}

#[test]
fn unrelated_objects_are_ignored() {
    let mut unrelated = package("oem9.inf", VERSION);
    unrelated.original_inf = "Other.inf".to_owned();
    let packages = [unrelated];
    let devices = [unrelated_device()];
    assert_eq!(
        plan_install(&source(), &packages, &devices),
        InstallPlan::Apply(vec![InstallStep::Stage, InstallStep::CreateDevice])
    );
    assert_eq!(
        plan_removal(&packages, &devices),
        RemovalPlan::AlreadyAbsent
    );
    assert_eq!(driver_state(&packages, &devices), DriverState::Absent);
}

#[test]
fn two_adapter_devices_are_refused_for_install_and_state_and_removed_in_order() {
    let staged = [package("oem3.inf", VERSION)];
    let devices = [
        adapter_at(r"ROOT\CROSSPANEIDD\0001", Some("oem3.inf"), Some(0), true),
        adapter_at(ADAPTER, Some("oem3.inf"), Some(0), true),
    ];
    assert_eq!(
        plan_install(&source(), &staged, &devices),
        InstallPlan::Mismatch(SEVERAL_DEVICES)
    );
    assert_eq!(driver_state(&staged, &devices), DriverState::Mismatch);
    assert_eq!(
        plan_removal(&staged, &devices),
        RemovalPlan::Apply(vec![
            remove_device(ADAPTER),
            remove_device(r"ROOT\CROSSPANEIDD\0001"),
            delete_package("oem3.inf"),
        ])
    );
}

#[test]
fn two_staged_packages_are_refused_for_install_and_state_and_deleted_in_order() {
    let staged = [package("oem3.inf", VERSION), package("oem12.inf", VERSION)];
    assert_eq!(
        plan_install(&source(), &staged, &[]),
        InstallPlan::Mismatch(SEVERAL_PACKAGES)
    );
    assert_eq!(driver_state(&staged, &[]), DriverState::Mismatch);
    assert_eq!(
        plan_removal(&staged, &[]),
        RemovalPlan::Apply(vec![
            delete_package("oem12.inf"),
            delete_package("oem3.inf"),
        ])
    );
}

#[test]
fn removal_lists_devices_first_then_packages_each_sorted() {
    let packages = [package("oem3.inf", VERSION), package("oem12.inf", VERSION)];
    let devices = [
        adapter_at(r"ROOT\CROSSPANEIDD\0002", Some("oem3.inf"), Some(0), true),
        adapter_at(r"ROOT\CROSSPANEIDD\0001", None, Some(28), false),
    ];
    assert_eq!(
        plan_removal(&packages, &devices),
        RemovalPlan::Apply(vec![
            remove_device(r"ROOT\CROSSPANEIDD\0001"),
            remove_device(r"ROOT\CROSSPANEIDD\0002"),
            delete_package("oem12.inf"),
            delete_package("oem3.inf"),
        ])
    );
}

#[test]
fn removal_refuses_a_healthy_adapter_bound_to_another_driver() {
    let packages = [package("oem3.inf", VERSION)];
    let devices = [adapter(Some("oem9.inf"), Some(0), true)];
    assert_eq!(
        plan_removal(&packages, &devices),
        RemovalPlan::Mismatch(BOUND_ELSEWHERE)
    );
}

#[test]
fn removal_proceeds_for_an_adapter_that_is_absent_or_failed() {
    // Only a present, healthy device bound elsewhere blocks removal; the others are ours to remove.
    let failed = [adapter(Some("oem9.inf"), Some(28), true)];
    assert_eq!(
        plan_removal(&[], &failed),
        RemovalPlan::Apply(vec![remove_device(ADAPTER)])
    );
    let absent = [adapter(Some("oem9.inf"), Some(0), false)];
    assert_eq!(
        plan_removal(&[], &absent),
        RemovalPlan::Apply(vec![remove_device(ADAPTER)])
    );
}

#[test]
fn removal_of_a_ghost_adapter_and_its_package_is_planned() {
    let packages = [package("oem3.inf", VERSION)];
    let ghost = [adapter(Some("oem3.inf"), Some(0), false)];
    assert_eq!(
        plan_removal(&packages, &ghost),
        RemovalPlan::Apply(vec![remove_device(ADAPTER), delete_package("oem3.inf")])
    );
    assert_eq!(plan_removal(&[], &[]), RemovalPlan::AlreadyAbsent);
}

#[test]
fn inventories_over_the_bounds_are_refused() {
    let packages: Vec<PackageFacts> = (0..=MAX_PACKAGES)
        .map(|n| package(&format!("oem{n}.inf"), VERSION))
        .collect();
    assert_eq!(
        plan_install(&source(), &packages, &[]),
        InstallPlan::Mismatch(OVER_BOUNDS)
    );
    assert_eq!(
        plan_removal(&packages, &[]),
        RemovalPlan::Mismatch(OVER_BOUNDS)
    );
    assert_eq!(driver_state(&packages, &[]), DriverState::Mismatch);

    let devices: Vec<DeviceFacts> = (0..=MAX_DEVICES)
        .map(|n| adapter_at(&format!(r"ROOT\CROSSPANEIDD\{n:04}"), None, Some(0), true))
        .collect();
    assert_eq!(
        plan_install(&source(), &[], &devices),
        InstallPlan::Mismatch(OVER_BOUNDS)
    );
    assert_eq!(
        plan_removal(&[], &devices),
        RemovalPlan::Mismatch(OVER_BOUNDS)
    );

    // The bound itself is allowed.
    let at_bound = &packages[..MAX_PACKAGES];
    match plan_removal(at_bound, &[]) {
        RemovalPlan::Apply(steps) => assert_eq!(steps.len(), MAX_PACKAGES),
        other => panic!("expected the removal plan at the bound, got {other:?}"),
    }
}

#[test]
fn driver_state_table() {
    let one = package("oem3.inf", VERSION);
    let two = package("oem12.inf", VERSION);
    let bound = adapter(Some("oem3.inf"), Some(0), true);
    let unbound = adapter(None, Some(28), true);
    let ghost = adapter(Some("oem3.inf"), Some(0), false);
    let cases: Vec<(Vec<PackageFacts>, Vec<DeviceFacts>, DriverState)> = vec![
        (vec![], vec![], DriverState::Absent),
        (vec![one.clone()], vec![], DriverState::PackageOnly),
        (
            vec![one.clone()],
            vec![bound.clone()],
            DriverState::Installed,
        ),
        (
            vec![one.clone()],
            vec![unbound.clone()],
            DriverState::DeviceWithoutDriver,
        ),
        (
            vec![one.clone()],
            vec![ghost],
            DriverState::DeviceWithoutDriver,
        ),
        (
            vec![],
            vec![unbound.clone()],
            DriverState::DeviceWithoutDriver,
        ),
        // Nothing to bind to, so the device is not bound.
        (
            vec![],
            vec![bound.clone()],
            DriverState::DeviceWithoutDriver,
        ),
        (
            vec![one.clone(), two.clone()],
            vec![],
            DriverState::Mismatch,
        ),
        (
            vec![],
            vec![unbound.clone(), bound.clone()],
            DriverState::Mismatch,
        ),
        (
            vec![one.clone()],
            vec![bound.clone(), unbound.clone()],
            DriverState::Mismatch,
        ),
        (
            vec![one.clone(), two.clone()],
            vec![bound.clone()],
            DriverState::Mismatch,
        ),
    ];
    for (packages, devices, expected) in cases {
        assert_eq!(
            driver_state(&packages, &devices),
            expected,
            "packages {}, devices {}",
            packages.len(),
            devices.len()
        );
    }
}

#[test]
fn package_ownership_follows_the_inf_identities() {
    assert_eq!(package_ownership(&source()), Ownership::Ours);
    assert_eq!(
        package_ownership(&package("oem3.inf", VERSION)),
        Ownership::Ours
    );
    // Every name compares ASCII case-insensitively.
    let mut upper = package("OEM3.INF", VERSION);
    upper.original_inf = "crosspaneidd.INF".to_owned();
    upper.original_catalog = "CROSSPANEIDD.CAT".to_owned();
    upper.provider = "CROSSPANE".to_owned();
    upper.class_guid = DISPLAY_CLASS_GUID.to_ascii_lowercase();
    upper.hardware_ids = vec![r"crosspane\iddtwinv1".to_owned()];
    assert_eq!(package_ownership(&upper), Ownership::Ours);

    // Another INF name is not ours, whatever else it shares.
    let mut other_inf = package("oem3.inf", VERSION);
    other_inf.original_inf = "Other.inf".to_owned();
    assert_eq!(package_ownership(&other_inf), Ownership::Unrelated);

    let mut catalog = package("oem3.inf", VERSION);
    catalog.original_catalog = "Other.cat".to_owned();
    assert_eq!(package_ownership(&catalog), Ownership::Foreign);

    let mut provider = package("oem3.inf", VERSION);
    provider.provider = "Contoso".to_owned();
    assert_eq!(package_ownership(&provider), Ownership::Foreign);

    let mut class = package("oem3.inf", VERSION);
    class.class_guid = "{4D36E968-E325-11CE-BFC1-08002BE10319}".to_owned();
    assert_eq!(package_ownership(&class), Ownership::Foreign);

    let mut hardware = package("oem3.inf", VERSION);
    hardware.hardware_ids = vec![r"Other\Device".to_owned()];
    assert_eq!(package_ownership(&hardware), Ownership::Foreign);

    // A published name must be `oemNN.inf` with one to five digits.
    for published in ["CrosspaneIdd.inf", "oem.inf", "oem123456.inf", "oem3.cat"] {
        assert_eq!(
            package_ownership(&package(published, VERSION)),
            Ownership::Foreign,
            "published name {published}"
        );
    }
}

#[test]
fn device_ownership_needs_our_hardware_id_and_a_root_or_present_instance() {
    assert_eq!(
        device_ownership(&adapter(None, Some(0), true)),
        Ownership::Ours
    );
    // A root ghost node is still ours, so it can be removed.
    assert_eq!(
        device_ownership(&adapter(None, None, false)),
        Ownership::Ours
    );
    // The enumerator compares ASCII case-insensitively.
    let lower = adapter_at(r"root\crosspaneidd\0000", None, Some(0), true);
    assert_eq!(device_ownership(&lower), Ownership::Ours);
    // Without our hardware ID the device is unrelated, present or not.
    assert_eq!(device_ownership(&unrelated_device()), Ownership::Unrelated);
    let mut unrelated_absent = unrelated_device();
    unrelated_absent.present = false;
    assert_eq!(device_ownership(&unrelated_absent), Ownership::Unrelated);
    // Our ID on a non-root device is unrelated when absent, and foreign when present.
    assert_eq!(device_ownership(&swd_device(false)), Ownership::Unrelated);
    assert_eq!(device_ownership(&swd_device(true)), Ownership::Foreign);
    // A root node with another hardware ID as well is foreign.
    let mut two_ids = adapter(None, Some(0), true);
    two_ids.hardware_ids.push(r"Other\Device".to_owned());
    assert_eq!(device_ownership(&two_ids), Ownership::Foreign);
}
