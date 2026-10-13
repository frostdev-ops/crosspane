//! Window-process claims that bind projected-app audio PIDs to their window's process identity.

use crosspane_platform_windows::model::process_claims::ProcessClaims;

const PID: u32 = 4242;
const CREATED: u64 = 0x01dc_0000_1234_5678;

#[test]
fn no_claim_refuses() {
    let claims = ProcessClaims::new();
    assert!(!claims.admits(PID, CREATED));
    assert!(!claims.admits(0, 0));
}

#[test]
fn creation_time_mismatch_refuses() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    assert!(!claims.admits(PID, CREATED + 1));
    assert!(!claims.admits(PID + 1, CREATED));
}

#[test]
fn match_with_live_window_admits() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    assert!(claims.admits(PID, CREATED));
}

#[test]
fn last_window_removed_refuses() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    claims.remove(1);
    assert!(!claims.admits(PID, CREATED));
    // Removing an unknown window is harmless.
    claims.remove(1);
    claims.remove(99);
    assert!(!claims.admits(PID, CREATED));
}

#[test]
fn two_windows_of_one_pid_keep_the_claim_until_the_last_goes() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    claims.record(2, PID, CREATED);
    claims.remove(1);
    assert!(claims.admits(PID, CREATED));
    // A refresh of the surviving window changes nothing.
    claims.record(2, PID, CREATED);
    assert!(claims.admits(PID, CREATED));
    claims.remove(2);
    assert!(!claims.admits(PID, CREATED));
}

#[test]
fn reused_pid_replaces_the_old_claim() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    claims.record(2, PID, CREATED);
    // A window of a new process with the same PID: the old process is gone.
    let reused = CREATED + 10_000_000;
    claims.record(3, PID, reused);
    assert!(!claims.admits(PID, CREATED));
    assert!(claims.admits(PID, reused));
    // The old process's windows no longer hold anything; removing them keeps the new claim.
    claims.remove(1);
    claims.remove(2);
    assert!(claims.admits(PID, reused));
    claims.remove(3);
    assert!(!claims.admits(PID, reused));
}

#[test]
fn refreshed_window_with_new_creation_time_replaces_its_claim() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    let reused = CREATED + 1;
    claims.record(1, PID, reused);
    assert!(!claims.admits(PID, CREATED));
    assert!(claims.admits(PID, reused));
}

#[test]
fn window_moving_to_another_pid_leaves_its_old_claim() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    claims.record(1, PID + 1, CREATED);
    assert!(!claims.admits(PID, CREATED));
    assert!(claims.admits(PID + 1, CREATED));
}

#[test]
fn zero_identity_is_never_recorded() {
    let mut claims = ProcessClaims::new();
    claims.record(1, PID, CREATED);
    claims.record(1, PID, 0);
    assert!(!claims.admits(PID, CREATED));
    claims.record(0, PID, CREATED);
    claims.record(2, 0, CREATED);
    assert!(!claims.admits(PID, CREATED));
    assert!(!claims.admits(0, CREATED));
}
