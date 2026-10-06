#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Pure selected-observation comparisons; no native process or filesystem calls.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use agent_contract::{BootstrapPhase, BootstrapV1, InstanceStatus};
use native_io::{
    NativeError,
    files::FileIdentity,
    identity::{Sid, TokenFacts},
};
use transport::security::*;

fn process() -> ProcessFacts {
    ProcessFacts {
        token: TokenFacts {
            user: Sid::from_bytes(vec![1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0]).unwrap(),
            logon: Sid::from_bytes(vec![
                1, 3, 0, 0, 0, 0, 0, 5, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0,
            ])
            .unwrap(),
            session: 1,
            elevated: false,
            integrity: 0x2000,
            authentication_id: 7,
            impersonating: false,
        },
        pid: 4242,
        created: 123,
        alive: true,
        image: r"\\?\C:\owned\crosspane-agent.exe".into(),
        file: FileIdentity {
            volume: 11,
            file: [1; 16],
        },
    }
}
fn bootstrap() -> BootstrapV1 {
    BootstrapV1 {
        schema_version: 1,
        instance_id: 7,
        pid: 4242,
        started_unix_ms: 99,
        phase: BootstrapPhase::Ready,
        phase_seq: 3,
        keystore: None,
        reason: None,
        runtime_dir: r"\\?\C:\owned\runtime".into(),
    }
}
fn status() -> InstanceStatus {
    let b = bootstrap();
    InstanceStatus {
        id: b.instance_id,
        pid: b.pid,
        uid: None,
        exe: process().image,
        runtime_dir: b.runtime_dir,
        started_unix_ms: b.started_unix_ms,
    }
}
#[test]
fn selected_process_refuses_wrong_user_logon_session_and_elevated_peer() {
    let expected = process();
    assert!(process_matches(&expected, &expected).is_ok());
    for change in 0..6 {
        let mut current = expected.clone();
        match change {
            0 => {
                current.token.user =
                    Sid::from_bytes(vec![1, 1, 0, 0, 0, 0, 0, 5, 22, 0, 0, 0]).unwrap()
            }
            1 => {
                current.token.logon = Sid::from_bytes(vec![
                    1, 3, 0, 0, 0, 0, 0, 5, 5, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0,
                ])
                .unwrap()
            }
            2 => current.token.session += 1,
            3 => current.token.elevated = true,
            4 => current.token.impersonating = true,
            _ => current.token.authentication_id += 1,
        }
        assert_eq!(
            peer_matches(&expected, &current),
            Err(NativeError::Foreign),
            "case {change}"
        );
    }
}
#[test]
fn selected_process_retains_creation_file_volume_image_and_liveness() {
    let expected = process();
    for change in 0..6 {
        let mut current = expected.clone();
        match change {
            0 => current.pid += 1,
            1 => current.created += 1,
            2 => current.file.file = [2; 16],
            3 => current.file.volume += 1,
            4 => current.image.push_str(".other"),
            _ => current.alive = false,
        }
        assert_eq!(
            process_matches(&expected, &current),
            Err(NativeError::Foreign),
            "case {change}"
        );
    }
}
#[test]
fn bootstrap_requires_same_instance_pid_start_runtime_and_monotonic_phase() {
    let expected = bootstrap();
    assert!(bootstrap_matches(&expected, &expected).is_ok());
    let mut progress = expected.clone();
    progress.phase_seq += 1;
    assert!(bootstrap_matches(&expected, &progress).is_ok());
    for change in 0..6 {
        let mut current = expected.clone();
        match change {
            0 => current.instance_id += 1,
            1 => current.pid += 1,
            2 => current.started_unix_ms += 1,
            3 => current.runtime_dir.push_str(".other"),
            4 => current.phase_seq -= 1,
            _ => current.phase = BootstrapPhase::Failed,
        }
        assert_eq!(
            bootstrap_matches(&expected, &current),
            Err(NativeError::Foreign),
            "case {change}"
        );
    }
}
#[test]
fn status_is_correlated_to_selected_bootstrap_and_fixed_image_not_authority() {
    assert!(status_matches(&bootstrap(), &process().image, &status()).is_ok());
    for change in 0..7 {
        let mut current = status();
        match change {
            0 => current.id += 1,
            1 => current.pid += 1,
            2 => current.started_unix_ms += 1,
            3 => current.runtime_dir.push_str(".other"),
            4 => current.exe.push_str(".other"),
            5 => current.uid = Some(1),
            _ => current.id = 0,
        }
        assert_eq!(
            status_matches(&bootstrap(), &process().image, &current),
            Err(NativeError::Foreign),
            "case {change}"
        );
    }
}
#[test]
fn observation_debug_does_not_disclose_paths_sids_or_ids() {
    assert_eq!(format!("{:?}", process()), "ProcessFacts");
}
