//! Read-only pure projection; no native reader or mutation port is constructed.
#[path = "../src/platform/windows/integration/diagnose.rs"]
#[allow(dead_code)]
mod diagnose;
#[path = "../src/platform/windows/integration/domains.rs"]
#[allow(dead_code)]
mod domains;
// The test graph's native entry is intentionally not constructed.
#[test]
fn read_only_report_has_no_mutation_capability() {
    let snapshot = domains::Snapshot {
        supported: true,
        inventory: false,
        sources: false,
        payload: domains::State::Missing,
        task: domains::State::Disabled,
        agent: domains::State::Unknown,
        cold: domains::Cold::Existing,
        terminal_history: true,
        unsettled: true,
        correlation: vec![1, 2, 3],
        source: crosspane_installer_core::ObservationSource::Live,
    };
    let before = snapshot.clone();
    let facts = diagnose::facts(&snapshot);
    assert_eq!(snapshot, before);
    assert!(facts.iter().any(|f| f.value == "user_disabled"));
    assert!(facts.iter().any(|f| f.value == "deferred"));
    assert!(facts.iter().filter(|f| f.blocking).count() >= 4);
    assert!(facts.iter().all(|f| !f.value.contains("1, 2, 3")));
}
