#![cfg(target_os = "linux")]
use crosspane_installer::platform::linux::{
    firewall::{RuleKind, receipts::AdmittedReceipt},
    removal::executor::UninstallPlanner,
};

#[test]
fn consented_uninstall_and_exact_receipt_kind_surface_exists() {
    let _ = UninstallPlanner::default();
    let _: fn(&AdmittedReceipt) -> RuleKind = AdmittedReceipt::kind;
}
