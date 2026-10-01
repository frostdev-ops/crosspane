#![allow(clippy::unwrap_used, clippy::expect_used)]

use core::time::Duration;

use crosspane_security::window::{MAX_ATTEMPTS, PAIRING_WINDOW, PairingWindow, WindowError};
use crosspane_types::time::MonoTime;

#[test]
fn expires_at_120_seconds() {
    assert_eq!(PAIRING_WINDOW, Duration::from_secs(120));
    let start = MonoTime::from_nanos(1_000_000_000);
    let mut window = PairingWindow::open(start);
    let deadline = start.checked_add(PAIRING_WINDOW).unwrap();
    assert!(window.is_open(start));
    assert!(window.is_open(MonoTime::from_nanos(deadline.as_nanos() - 1)));
    assert!(!window.is_open(deadline));
    assert_eq!(window.admit(deadline), Err(WindowError::Expired));
    let later = deadline.checked_add(Duration::from_secs(1)).unwrap();
    assert!(!window.is_open(later));
    assert_eq!(window.admit(later), Err(WindowError::Expired));
}

#[test]
fn three_attempts_then_too_many_attempts() {
    assert_eq!(MAX_ATTEMPTS, 3);
    let now = MonoTime::ZERO;
    let mut window = PairingWindow::open(now);
    for attempt in 1..=MAX_ATTEMPTS {
        assert!(window.is_open(now));
        assert_eq!(window.admit(now), Ok(attempt));
    }
    assert!(!window.is_open(now));
    assert_eq!(window.admit(now), Err(WindowError::TooManyAttempts));
    assert_eq!(window.admit(now), Err(WindowError::TooManyAttempts));
}

#[test]
fn expiry_takes_precedence_over_attempt_limit() {
    let mut window = PairingWindow::open(MonoTime::ZERO);
    for _ in 0..MAX_ATTEMPTS {
        window.admit(MonoTime::ZERO).unwrap();
    }
    assert_eq!(
        window.admit(MonoTime::ZERO),
        Err(WindowError::TooManyAttempts)
    );
    let expired = MonoTime::ZERO.checked_add(PAIRING_WINDOW).unwrap();
    assert_eq!(window.admit(expired), Err(WindowError::Expired));
}
