//! Target-bound local settlement using the admitted additive model path. Ordinary cleanup
//! releases retain their permissive semantics. This fixture submits no native input.
#![allow(clippy::unwrap_used)]

use crosspane_platform::{IoGate, PlatformError};
mod geometry {
    pub use crosspane_platform_windows::model::geometry::*;
}
#[path = "../src/model/hook.rs"]
#[allow(dead_code)]
mod hook;
#[path = "../src/model/inject.rs"]
#[allow(dead_code)]
mod inject;
use crosspane_types::{hid::MouseButton, input::LockKeys};
use hook::DragSettlementResult;
use inject::{Driver, Foreground, InjectionPort, LocalSettlementOutcome, Packet, RepeatSettings};

struct Port {
    foreground: Result<Foreground, PlatformError>,
    submissions: Vec<Packet>,
    accepted: bool,
}

impl InjectionPort for Port {
    fn foreground(&mut self) -> Result<Foreground, PlatformError> {
        match self.foreground {
            Ok(observed) => Ok(observed),
            Err(_) => Err(PlatformError::SecureInput),
        }
    }

    fn submit(&mut self, packet: Packet) -> Result<(), PlatformError> {
        self.submissions.push(packet);
        if self.accepted {
            Ok(())
        } else {
            Err(PlatformError::Backend("test zero submission".into()))
        }
    }

    fn locks(&mut self) -> Result<LockKeys, PlatformError> {
        Ok(LockKeys::default())
    }
}

fn target() -> Foreground {
    Foreground {
        window: 100,
        process: 200,
        thread: 300,
        born: 400,
        generation: 500,
        integrity: 0x2000,
    }
}

fn driver(foreground: Result<Foreground, PlatformError>, accepted: bool) -> Driver<Port> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    Driver::new(
        Port {
            foreground,
            submissions: Vec::new(),
            accepted,
        },
        gate,
        0x2000,
        RepeatSettings::new(0, 0).unwrap(),
    )
}

fn local_settlement(driver: &mut Driver<Port>, original: Foreground) -> Result<(), PlatformError> {
    let outcome = driver.settle_local_button(
        original,
        || Ok(()),
        |port| {
            port.submissions.push(Packet::Button {
                button: MouseButton::PRIMARY,
                down: false,
            });
            if port.accepted {
                LocalSettlementOutcome {
                    result: DragSettlementResult::Accepted,
                    error: None,
                }
            } else {
                LocalSettlementOutcome {
                    result: DragSettlementResult::KnownZero,
                    error: Some(PlatformError::Backend("test zero submission".into())),
                }
            }
        },
    )?;
    outcome.error.map_or(Ok(()), Err)
}

#[test]
fn target_bound_settlement_refuses_each_foreground_identity_change_without_up() {
    for field in 0..5 {
        let mut changed = target();
        match field {
            0 => changed.window += 1,
            1 => changed.process += 1,
            2 => changed.thread += 1,
            3 => changed.born += 1,
            4 => changed.generation += 1,
            _ => unreachable!(),
        }
        let mut driver = driver(Ok(changed), true);
        assert!(matches!(
            local_settlement(&mut driver, target()),
            Err(PlatformError::SecureInput)
        ));
        assert!(driver.port().submissions.is_empty());
        assert!(driver.held_buttons().is_empty());
    }
}

#[test]
fn target_bound_settlement_refuses_higher_and_unknown_integrity_without_up() {
    let mut high = target();
    high.integrity = 0x3000;
    for observation in [Ok(high), Err(PlatformError::SecureInput)] {
        let mut driver = driver(observation, true);
        assert!(matches!(
            local_settlement(&mut driver, target()),
            Err(PlatformError::SecureInput)
        ));
        assert!(driver.port().submissions.is_empty());
        assert!(driver.held_buttons().is_empty());
    }
}

#[test]
fn target_bound_settlement_refuses_closed_gate_without_up() {
    let mut driver = driver(Ok(target()), true);
    driver.gate().set_engine_permits(false);
    assert!(matches!(
        local_settlement(&mut driver, target()),
        Err(PlatformError::Locked)
    ));
    assert!(driver.port().submissions.is_empty());
    assert!(driver.held_buttons().is_empty());
}

#[test]
fn known_zero_local_settlement_does_not_leave_duplicate_synthetic_release_owed() {
    let mut driver = driver(Ok(target()), false);
    assert!(local_settlement(&mut driver, target()).is_err());
    assert_eq!(driver.port().submissions.len(), 1);
    assert!(driver.held_buttons().is_empty());
    driver.release_buttons().unwrap();
    assert_eq!(driver.port().submissions.len(), 1);
}

#[test]
fn accepted_unledgered_primary_settlement_submits_one_up_and_no_down() {
    let mut driver = driver(Ok(target()), true);
    local_settlement(&mut driver, target()).unwrap();
    assert_eq!(
        driver.port().submissions,
        vec![Packet::Button {
            button: MouseButton::PRIMARY,
            down: false
        }]
    );
    assert!(driver.held_buttons().is_empty());
}
