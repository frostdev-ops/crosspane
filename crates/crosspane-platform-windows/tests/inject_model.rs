#![allow(clippy::unwrap_used)]

use crosspane_platform::{error::PlatformError, session::IoGate};
use crosspane_platform_windows::model::inject::*;
use crosspane_types::{
    geom::VectorLogical,
    hid::{HidUsage, MouseButton},
    input::{LockKeys, ScrollDelta, ScrollPhase},
};
use std::collections::VecDeque;

#[derive(Default)]
struct Fake {
    foreground: VecDeque<Result<Foreground, PlatformError>>,
    sends: Vec<Packet>,
    failed: bool,
    lock_state: Option<LockKeys>,
}
impl InjectionPort for Fake {
    fn foreground(&mut self) -> Result<Foreground, PlatformError> {
        self.foreground.pop_front().unwrap_or(Ok(focus(1, 0x2000)))
    }
    fn submit(&mut self, packet: Packet) -> Result<(), PlatformError> {
        self.sends.push(packet);
        if self.failed {
            Err(PlatformError::Backend("injected failure".into()))
        } else {
            Ok(())
        }
    }
    fn locks(&mut self) -> Result<LockKeys, PlatformError> {
        Ok(self.lock_state.unwrap_or(LockKeys {
            caps_lock: Some(false),
            num_lock: Some(false),
            scroll_lock: Some(false),
        }))
    }
}
fn focus(window: u64, integrity: u32) -> Foreground {
    Foreground {
        window,
        process: 11,
        thread: 12,
        born: 13,
        generation: 0,
        integrity,
    }
}
fn driver(fake: Fake) -> Driver<Fake> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    Driver::new(fake, gate, 0x2000, RepeatSettings::new(0, 31).unwrap())
}

#[test]
fn guard_decision_table_and_releases() {
    for integrity in [0x2000, 0x1000] {
        let mut d = driver(Fake {
            foreground: [Ok(focus(1, integrity))].into(),
            ..Default::default()
        });
        d.key(HidUsage::keyboard(4), true, 0).unwrap();
        assert_eq!(d.port().sends.len(), 1);
    }
    for observation in [Ok(focus(1, 0x3000)), Err(PlatformError::SecureInput)] {
        let mut d = driver(Fake {
            foreground: [observation].into(),
            ..Default::default()
        });
        assert!(matches!(
            d.key(HidUsage::keyboard(4), true, 0),
            Err(PlatformError::SecureInput)
        ));
        assert!(d.port().sends.is_empty());
        d.gate().set_engine_permits(false);
        d.recover_keys(&[HidUsage::keyboard(4)]).unwrap();
        assert_eq!(d.port().sends.len(), 1);
    }
}

#[test]
fn focus_change_releases_both_ledgers_before_refusing_submission() {
    let mut d = driver(Fake {
        foreground: [
            Ok(focus(1, 0x2000)),
            Ok(focus(1, 0x2000)),
            Ok(focus(2, 0x2000)),
        ]
        .into(),
        ..Default::default()
    });
    // A typing key (not a modifier): a modifier alone would follow the focus change instead.
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.button(MouseButton::PRIMARY, true).unwrap();
    assert!(d.submit_guarded(Packet::Move { x: 0, y: 0 }).is_err());
    assert_eq!(
        &d.port().sends[2..],
        &[
            key_packet(HidUsage::keyboard(4), false).unwrap(),
            Packet::Button {
                button: MouseButton::PRIMARY,
                down: false
            }
        ]
    );
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
    assert!(!d.repeating());
}

/// A fake whose every foreground observation is scripted, so a click that activates another
/// window can be replayed without the default `focus(1, ..)` refilling the queue.
fn scripted(observations: impl IntoIterator<Item = Result<Foreground, PlatformError>>) -> Fake {
    Fake {
        foreground: observations.into_iter().collect(),
        ..Default::default()
    }
}
fn down(button: MouseButton) -> Packet {
    Packet::Button { button, down: true }
}
fn up(button: MouseButton) -> Packet {
    Packet::Button {
        button,
        down: false,
    }
}
fn ups(sends: &[Packet], button: MouseButton) -> usize {
    sends.iter().filter(|p| **p == up(button)).count()
}

#[test]
fn inject_drag_follows_the_click_into_a_newly_activated_window() {
    // Move onto an unfocused window, press (this makes window 2 foreground), then drag.
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)),
        Ok(focus(1, 0x2000)),
        Ok(focus(2, 0x2000)),
        Ok(focus(2, 0x2000)),
    ]));
    d.submit_guarded(Packet::Move { x: 1, y: 1 }).unwrap();
    d.button(MouseButton::PRIMARY, true).unwrap();
    d.submit_guarded(Packet::Move { x: 2, y: 2 }).unwrap();
    d.submit_guarded(Packet::Move { x: 3, y: 3 }).unwrap();
    assert_eq!(
        d.port().sends,
        [
            Packet::Move { x: 1, y: 1 },
            down(MouseButton::PRIMARY),
            Packet::Move { x: 2, y: 2 },
            Packet::Move { x: 3, y: 3 },
        ]
    );
    assert!(d.held_buttons().contains(&MouseButton::PRIMARY));
    // The one up still goes out, exactly once, whatever the foreground is by then.
    d.button(MouseButton::PRIMARY, false).unwrap();
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1);
    assert!(d.held_buttons().is_empty());
}

#[test]
fn inject_drag_is_followed_through_several_foreground_changes_and_a_lower_integrity_window() {
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)),
        Ok(focus(2, 0x1000)), // lower integrity than ours
        Ok(Foreground {
            generation: 1,
            ..focus(2, 0x1000)
        }),
        Ok(focus(3, 0x2000)),
    ]));
    d.button(MouseButton::PRIMARY, true).unwrap();
    for n in 0..3 {
        d.submit_guarded(Packet::Move { x: n, y: n }).unwrap();
    }
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 0);
    assert_eq!(d.port().sends.len(), 4);
    d.release_buttons().unwrap();
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1);
}

#[test]
fn inject_drag_into_a_higher_unknown_or_null_foreground_releases_exactly_once() {
    let null = Foreground {
        window: 0,
        ..focus(1, 0x2000)
    };
    for next in [
        Ok(focus(2, 0x3000)),
        Ok(null),
        Ok(Foreground {
            process: 0,
            ..focus(2, 0x2000)
        }),
        Ok(Foreground {
            thread: 0,
            ..focus(2, 0x2000)
        }),
        Ok(Foreground {
            born: 0,
            ..focus(2, 0x2000)
        }),
        Err(PlatformError::SecureInput),
    ] {
        let label = format!("{next:?}");
        let mut d = driver(scripted([Ok(focus(1, 0x2000)), next]));
        d.button(MouseButton::PRIMARY, true).unwrap();
        assert!(
            matches!(
                d.submit_guarded(Packet::Move { x: 1, y: 1 }),
                Err(PlatformError::SecureInput)
            ),
            "{label}"
        );
        // The held button was released exactly once, and the move never went out.
        assert_eq!(
            d.port().sends,
            [down(MouseButton::PRIMARY), up(MouseButton::PRIMARY)],
            "{label}"
        );
        assert!(d.held_buttons().is_empty(), "{label}");
        // A later release of the already-drained button adds no ledger state and no second up
        // from the driver itself; only the explicit release the caller asked for goes out.
        d.release_buttons().unwrap();
        assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1, "{label}");
    }
}

#[test]
fn inject_drag_failed_release_stays_owed_until_it_succeeds_once() {
    let mut d = driver(scripted([Ok(focus(1, 0x2000)), Ok(focus(2, 0x3000))]));
    d.button(MouseButton::PRIMARY, true).unwrap();
    d.port_mut().failed = true;
    assert!(d.submit_guarded(Packet::Move { x: 1, y: 1 }).is_err());
    // The up was attempted once and failed, so it is still owed.
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1);
    assert!(d.held_buttons().contains(&MouseButton::PRIMARY));
    d.port_mut().failed = false;
    d.release_buttons().unwrap();
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 2); // retry of the failed one
    assert!(d.held_buttons().is_empty());
    d.release_buttons().unwrap();
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 2); // settled: nothing more owed
}

#[test]
fn inject_foreground_change_with_a_key_held_still_drains_both_ledgers() {
    // Only a pointer drag, with or without held modifiers, follows the click: a held typing key
    // keeps the original fail-safe.
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)),
        Ok(focus(1, 0x2000)),
        Ok(focus(2, 0x1000)),
    ]));
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.button(MouseButton::PRIMARY, true).unwrap();
    assert!(matches!(
        d.submit_guarded(Packet::Move { x: 1, y: 1 }),
        Err(PlatformError::SecureInput)
    ));
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1);
}

#[test]
fn inject_foreground_change_with_nothing_held_is_normal_admission() {
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)),
        Ok(focus(2, 0x1000)),
        Ok(focus(3, 0x2000)),
    ]));
    d.submit_guarded(Packet::Move { x: 1, y: 1 }).unwrap();
    d.submit_guarded(Packet::Move { x: 2, y: 2 }).unwrap();
    d.button(MouseButton::PRIMARY, true).unwrap();
    assert_eq!(d.port().sends.len(), 3);
}

fn shift() -> HidUsage {
    HidUsage::keyboard(0xe1)
}
fn ctrl() -> HidUsage {
    HidUsage::keyboard(0xe0)
}
fn letter() -> HidUsage {
    HidUsage::keyboard(4)
}
fn key_down(usage: HidUsage) -> Packet {
    key_packet(usage, true).unwrap()
}
fn key_up(usage: HidUsage) -> Packet {
    key_packet(usage, false).unwrap()
}
fn key_ups(sends: &[Packet], usage: HidUsage) -> usize {
    sends.iter().filter(|p| **p == key_up(usage)).count()
}
/// `downs` observations of window 1 (one per admitted down), then `after`.
fn change_after(downs: usize, after: Result<Foreground, PlatformError>) -> Fake {
    scripted(
        std::iter::repeat_with(|| Ok(focus(1, 0x2000)))
            .take(downs)
            .chain([after]),
    )
}

#[test]
fn inject_shift_click_drag_follows_the_click_into_a_newly_activated_window() {
    // Shift is held, the button goes down against window 1, and the click activates window 2.
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)), // Shift down
        Ok(focus(1, 0x2000)), // button down, admitted against the old foreground
        Ok(focus(2, 0x2000)), // first drag move: window 2 is foreground now
        Ok(focus(2, 0x2000)),
    ]));
    d.key(shift(), true, 0).unwrap();
    assert!(!d.repeating(), "a modifier never arms the typematic repeat");
    d.button(MouseButton::PRIMARY, true).unwrap();
    d.submit_guarded(Packet::Move { x: 2, y: 2 }).unwrap();
    d.submit_guarded(Packet::Move { x: 3, y: 3 }).unwrap();
    assert_eq!(
        d.port().sends,
        [
            key_down(shift()),
            down(MouseButton::PRIMARY),
            Packet::Move { x: 2, y: 2 },
            Packet::Move { x: 3, y: 3 },
        ]
    );
    // Only ownership moved: both owed ups are still in the ledger, and nothing repeats.
    assert!(d.held_keys().contains(&shift()));
    assert!(d.held_buttons().contains(&MouseButton::PRIMARY));
    assert!(!d.repeating());
    d.repeat(1_000_000).unwrap();
    assert_eq!(d.port().sends.len(), 4);
    d.button(MouseButton::PRIMARY, false).unwrap();
    d.key(shift(), false, 1).unwrap();
    assert_eq!(ups(&d.port().sends, MouseButton::PRIMARY), 1);
    assert_eq!(key_ups(&d.port().sends, shift()), 1);
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
    // Settled: a later cleanup sends nothing more.
    d.release_everything().unwrap();
    assert_eq!(d.port().sends.len(), 6);
}

#[test]
fn inject_ctrl_alone_follows_foreground_changes_for_moves_and_wheel() {
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)), // Ctrl down
        Ok(focus(2, 0x2000)), // move: another window took the foreground
        Ok(focus(2, 0x2000)), // wheel, same window
        Ok(focus(3, 0x1000)), // move: a lower-integrity window
    ]));
    d.key(ctrl(), true, 0).unwrap();
    let zoom = Packet::Wheel {
        horizontal: false,
        v120: 120,
    };
    d.submit_guarded(Packet::Move { x: 1, y: 1 }).unwrap();
    d.submit_guarded(zoom).unwrap();
    d.submit_guarded(Packet::Move { x: 2, y: 2 }).unwrap();
    assert_eq!(
        d.port().sends,
        [
            key_down(ctrl()),
            Packet::Move { x: 1, y: 1 },
            zoom,
            Packet::Move { x: 2, y: 2 },
        ]
    );
    assert_eq!(d.held_keys().len(), 1);
    d.key(ctrl(), false, 1).unwrap();
    assert_eq!(key_ups(&d.port().sends, ctrl()), 1);
    d.release_everything().unwrap();
    assert_eq!(key_ups(&d.port().sends, ctrl()), 1);
}

#[test]
fn inject_ctrl_wheel_through_scroll_follows_a_foreground_change() {
    // `scroll` guards once up front and once per packet; the first sees the new window and adopts
    // it, so the per-packet guard and the scroll context agree.
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)), // Ctrl down
        Ok(focus(2, 0x2000)), // scroll guard
        Ok(focus(2, 0x2000)), // per-packet guard
    ]));
    d.key(ctrl(), true, 0).unwrap();
    d.scroll(ScrollDelta {
        v120_y: 120,
        ..pixels(0.0, 0.0)
    })
    .unwrap();
    assert_eq!(
        d.port().sends,
        [key_down(ctrl()), wheel(false, 120)],
        "the wheel reached the new window"
    );
    d.release_keys().unwrap();
    assert_eq!(key_ups(&d.port().sends, ctrl()), 1);
}

#[test]
fn inject_each_of_the_eight_modifiers_follows_a_foreground_change() {
    // Left/Right Ctrl, Shift, Alt and Win.
    for id in 0xe0..=0xe7 {
        let usage = HidUsage::keyboard(id);
        let mut d = driver(scripted([Ok(focus(1, 0x2000)), Ok(focus(2, 0x2000))]));
        d.key(usage, true, 0).unwrap();
        d.submit_guarded(Packet::Move { x: 1, y: 1 }).unwrap();
        assert_eq!(
            d.port().sends,
            [key_down(usage), Packet::Move { x: 1, y: 1 }],
            "{id:#x}"
        );
        assert!(!d.repeating(), "{id:#x}");
        d.key(usage, false, 1).unwrap();
        assert_eq!(key_ups(&d.port().sends, usage), 1, "{id:#x}");
        assert!(d.held_keys().is_empty(), "{id:#x}");
    }
}

#[test]
fn inject_modifier_with_a_typing_key_held_releases_everything_once_on_a_foreground_change() {
    for button in [false, true] {
        for shift_first in [false, true] {
            let mut d = driver(change_after(2 + usize::from(button), Ok(focus(2, 0x2000))));
            if shift_first {
                d.key(shift(), true, 0).unwrap();
                d.key(letter(), true, 1).unwrap();
            } else {
                d.key(letter(), true, 0).unwrap();
                d.key(shift(), true, 1).unwrap();
            }
            if button {
                d.button(MouseButton::PRIMARY, true).unwrap();
            }
            let owed = 2 + usize::from(button);
            assert_eq!(d.port().sends.len(), owed);
            assert!(matches!(
                d.submit_guarded(Packet::Move { x: 1, y: 1 }),
                Err(PlatformError::SecureInput)
            ));
            // The move never went out; each down got exactly one up; the repeat is gone.
            assert_eq!(d.port().sends.len(), owed * 2, "{button} {shift_first}");
            assert_eq!(key_ups(&d.port().sends, shift()), 1);
            assert_eq!(key_ups(&d.port().sends, letter()), 1);
            assert_eq!(
                ups(&d.port().sends, MouseButton::PRIMARY),
                usize::from(button)
            );
            assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
            assert!(!d.repeating());
            d.repeat(1_000_000).unwrap();
            d.release_everything().unwrap();
            assert_eq!(
                d.port().sends.len(),
                owed * 2,
                "settled: nothing more is sent"
            );
        }
    }
}

#[test]
fn inject_typing_key_after_a_modifier_follow_is_admitted_and_a_further_change_releases_once() {
    let mut d = driver(scripted([
        Ok(focus(1, 0x2000)), // Shift down
        Ok(focus(2, 0x2000)), // move: adopt window 2
        Ok(focus(2, 0x2000)), // letter down: window 2 owns now, nothing changed
        Ok(focus(3, 0x2000)), // move: the letter is held, so this releases
    ]));
    d.key(shift(), true, 0).unwrap();
    d.submit_guarded(Packet::Move { x: 1, y: 1 }).unwrap();
    d.key(letter(), true, 1).unwrap();
    assert!(d.repeating());
    assert!(matches!(
        d.submit_guarded(Packet::Move { x: 2, y: 2 }),
        Err(PlatformError::SecureInput)
    ));
    assert!(!d.repeating());
    assert_eq!(key_ups(&d.port().sends, shift()), 1);
    assert_eq!(key_ups(&d.port().sends, letter()), 1);
    assert!(d.held_keys().is_empty());
    d.repeat(1_000_000).unwrap();
    // Shift down, move, letter down, then one up each; the second move never went out.
    assert_eq!(d.port().sends.len(), 5);
}

#[test]
fn inject_owed_lock_toggle_up_releases_once_even_with_only_a_modifier_or_nothing_else_held() {
    for with_shift in [false, true] {
        let guards = 1 + usize::from(with_shift); // set_locks guards once, a held Shift once
        let mut d = driver(change_after(guards, Ok(focus(2, 0x2000))));
        if with_shift {
            d.key(shift(), true, 0).unwrap();
        }
        // The Caps down and up both fail, so the toggle's up stays owed.
        d.port_mut().failed = true;
        assert!(
            d.set_locks(LockKeys {
                caps_lock: Some(true),
                ..Default::default()
            })
            .is_err()
        );
        d.port_mut().failed = false;
        let caps = HidUsage::keyboard(0x39);
        let before = d.port().sends.len();
        assert!(matches!(
            d.submit_guarded(Packet::Move { x: 1, y: 1 }),
            Err(PlatformError::SecureInput)
        ));
        // The owed Caps up (the first, failed attempt plus the retry) and the one Shift up.
        assert_eq!(key_ups(&d.port().sends, caps), 2, "{with_shift}");
        assert_eq!(
            key_ups(&d.port().sends, shift()),
            usize::from(with_shift),
            "{with_shift}"
        );
        assert_eq!(
            d.port().sends.len(),
            before + 1 + usize::from(with_shift),
            "the move never went out"
        );
        assert!(d.held_keys().is_empty());
        d.release_everything().unwrap();
        assert_eq!(key_ups(&d.port().sends, caps), 2, "settled");
    }
}

#[test]
fn inject_modifiers_into_a_higher_unknown_or_null_foreground_release_exactly_once() {
    let null = Foreground {
        window: 0,
        ..focus(1, 0x2000)
    };
    // `None` is an unreadable foreground (a secure desktop, an unstable read).
    for next in [
        Some(focus(2, 0x3000)),
        Some(null),
        Some(Foreground {
            process: 0,
            ..focus(2, 0x2000)
        }),
        Some(Foreground {
            thread: 0,
            ..focus(2, 0x2000)
        }),
        // An incomplete identity (no process creation time) never inherits held modifiers.
        Some(Foreground {
            born: 0,
            ..focus(2, 0x2000)
        }),
        None,
    ] {
        for (modifiers, button) in [
            (vec![shift()], false),
            (vec![ctrl()], false),
            (vec![ctrl(), shift()], false),
            (vec![shift()], true),
        ] {
            let label = format!("{next:?} {modifiers:?} {button}");
            let downs = modifiers.len() + usize::from(button);
            let mut d = driver(change_after(downs, next.ok_or(PlatformError::SecureInput)));
            for usage in &modifiers {
                d.key(*usage, true, 0).unwrap();
            }
            if button {
                d.button(MouseButton::PRIMARY, true).unwrap();
            }
            assert!(
                matches!(
                    d.submit_guarded(Packet::Move { x: 1, y: 1 }),
                    Err(PlatformError::SecureInput)
                ),
                "{label}"
            );
            // Nothing but the downs and one up each: the move never went out.
            assert_eq!(d.port().sends.len(), downs * 2, "{label}");
            for usage in &modifiers {
                assert_eq!(key_ups(&d.port().sends, *usage), 1, "{label}");
            }
            assert_eq!(
                ups(&d.port().sends, MouseButton::PRIMARY),
                usize::from(button),
                "{label}"
            );
            assert!(
                d.held_keys().is_empty() && d.held_buttons().is_empty(),
                "{label}"
            );
            d.release_everything().unwrap();
            assert_eq!(d.port().sends.len(), downs * 2, "{label}: settled");
        }
    }
}

#[test]
fn inject_held_modifier_with_a_closed_gate_releases_once_and_never_follows() {
    let mut d = driver(scripted([Ok(focus(1, 0x2000)), Ok(focus(2, 0x2000))]));
    d.key(shift(), true, 0).unwrap();
    d.gate().set_engine_permits(false);
    assert!(matches!(
        d.submit_guarded(Packet::Move { x: 1, y: 1 }),
        Err(PlatformError::Locked)
    ));
    assert_eq!(d.port().sends, [key_down(shift()), key_up(shift())]);
    assert!(d.held_keys().is_empty());
}

#[test]
fn inject_failed_modifier_release_stays_owed_until_it_succeeds_once() {
    let mut d = driver(change_after(1, Ok(focus(2, 0x3000))));
    d.key(shift(), true, 0).unwrap();
    d.port_mut().failed = true;
    assert!(d.submit_guarded(Packet::Move { x: 1, y: 1 }).is_err());
    assert_eq!(key_ups(&d.port().sends, shift()), 1);
    assert!(
        d.held_keys().contains(&shift()),
        "the failed up is still owed"
    );
    d.port_mut().failed = false;
    d.release_keys().unwrap();
    assert_eq!(key_ups(&d.port().sends, shift()), 2); // the retry of the failed one
    assert!(d.held_keys().is_empty());
    d.release_keys().unwrap();
    assert_eq!(key_ups(&d.port().sends, shift()), 2); // settled
}

#[test]
fn partial_down_and_failed_release_remain_owed() {
    let mut d = driver(Fake {
        failed: true,
        ..Default::default()
    });
    assert!(d.key(HidUsage::keyboard(4), true, 0).is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    assert!(d.release_keys().is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    d.port_mut().failed = false;
    d.release_keys().unwrap();
    assert!(d.held_keys().is_empty());
}

#[test]
fn repeat_gate_epoch_and_focus_generation_cancel_without_new_ownership() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.repeat(249).unwrap();
    assert_eq!(d.port().sends.len(), 1);
    d.repeat(250).unwrap();
    assert_eq!(d.port().sends.len(), 2);
    assert_eq!(d.held_keys().len(), 1);
    d.gate().set_engine_permits(false);
    d.gate().set_engine_permits(true);
    assert!(matches!(d.repeat(1000), Err(PlatformError::Locked)));
    assert_eq!(d.port().sends.len(), 3); // owed up is allowed even after gate closure
    assert!(!d.repeating());
    assert!(d.held_keys().is_empty());
    d.key(HidUsage::keyboard(5), true, 1002).unwrap();
    d.port_mut().foreground.push_back(Ok(Foreground {
        generation: 1,
        ..focus(1, 0x2000)
    }));
    assert!(d.repeat(1300).is_err());
    assert!(d.held_keys().is_empty());
}

#[test]
fn scancode_mapping_extended_pause_and_no_unicode_fallback() {
    assert_eq!(
        key_packet(HidUsage::keyboard(4), true).unwrap(),
        Packet::Key {
            scan: 0x1e,
            extended: false,
            down: true
        }
    );
    assert_eq!(
        key_packet(HidUsage::keyboard(0xe4), false).unwrap(),
        Packet::Key {
            scan: 0x1d,
            extended: true,
            down: false
        }
    );
    assert_eq!(
        key_packet(HidUsage::keyboard(0x48), true).unwrap(),
        Packet::Key {
            scan: 0x45,
            extended: false,
            down: true
        }
    );
    assert!(key_packet(HidUsage { page: 12, id: 4 }, true).is_err());
}

#[test]
fn release_cancels_repeat_and_lock_toggle_keeps_existing_held_set() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.set_locks(LockKeys {
        caps_lock: Some(true),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(d.held_keys().len(), 1);
    d.key(HidUsage::keyboard(4), false, 1).unwrap();
    d.repeat(1000).unwrap();
    assert!(!d.repeating());
    assert!(d.held_keys().is_empty());
}

#[test]
fn repeat_settings_boundaries_are_not_guessed() {
    assert_eq!(RepeatSettings::new(0, 0).unwrap().delay_ms, 250);
    assert_eq!(RepeatSettings::new(3, 31).unwrap().delay_ms, 1000);
    assert_eq!(RepeatSettings::new(0, 0).unwrap().interval_ms, 400);
    assert!(RepeatSettings::new(4, 0).is_err());
    assert!(RepeatSettings::new(0, 32).is_err());
}

#[test]
fn move_scroll_button_and_lock_each_require_fresh_guard() {
    for operation in 0..4 {
        for observation in [Ok(focus(1, 0x3000)), Err(PlatformError::SecureInput)] {
            let mut d = driver(Fake {
                foreground: [observation].into(),
                ..Default::default()
            });
            let result = match operation {
                0 => d.submit_guarded(Packet::Move { x: 1, y: 2 }),
                1 => d.submit_guarded(Packet::Wheel {
                    horizontal: true,
                    v120: 60,
                }),
                2 => d.button(MouseButton::PRIMARY, true),
                _ => d.set_locks(LockKeys {
                    caps_lock: Some(true),
                    ..Default::default()
                }),
            };
            assert!(matches!(result, Err(PlatformError::SecureInput)));
            assert!(d.port().sends.is_empty());
        }
    }
}

#[test]
fn failed_unknown_release_and_recovered_buttons_are_owed() {
    let mut d = driver(Fake {
        failed: true,
        ..Default::default()
    });
    assert!(d.key(HidUsage::keyboard(5), false, 0).is_err());
    assert!(d.held_keys().contains(&HidUsage::keyboard(5)));
    assert!(
        d.recover_buttons(&[MouseButton::BACK, MouseButton::FORWARD])
            .is_err()
    );
    assert_eq!(d.held_buttons().len(), 2);
    d.gate().set_engine_permits(false);
    d.port_mut().failed = false;
    d.release_everything().unwrap();
    assert!(d.held_keys().is_empty() && d.held_buttons().is_empty());
}

#[test]
fn secure_refusal_keeps_failed_cleanup_owed_and_never_submits_new_down() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().failed = true;
    d.port_mut()
        .foreground
        .push_back(Err(PlatformError::SecureInput));
    assert!(matches!(
        d.key(HidUsage::keyboard(5), true, 1),
        Err(PlatformError::SecureInput)
    ));
    assert!(d.held_keys().contains(&HidUsage::keyboard(4)));
    assert!(!d.held_keys().contains(&HidUsage::keyboard(5)));
    assert!(!d.repeating());
}

#[test]
fn lock_failure_owed_up_is_separate_from_existing_key_ownership() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().failed = true;
    assert!(
        d.set_locks(LockKeys {
            caps_lock: Some(true),
            ..Default::default()
        })
        .is_err()
    );
    assert_eq!(
        d.held_keys().iter().copied().collect::<Vec<_>>(),
        [HidUsage::keyboard(4)]
    );
    d.port_mut().failed = false;
    d.release_keys().unwrap();
    assert!(
        d.port()
            .sends
            .contains(&key_packet(HidUsage::keyboard(0x39), false).unwrap())
    );
    d.key(HidUsage::keyboard(0x39), true, 1).unwrap();
    assert!(
        d.set_locks(LockKeys {
            caps_lock: Some(true),
            ..Default::default()
        })
        .is_err()
    );
    assert!(d.held_keys().contains(&HidUsage::keyboard(0x39)));
}

#[test]
fn repeat_last_key_no_catchup_and_every_release_or_recovery_cancels() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.key(HidUsage::keyboard(5), true, 1).unwrap();
    d.repeat(100_000).unwrap();
    assert_eq!(d.port().sends.len(), 3);
    assert_eq!(
        d.port().sends[2],
        key_packet(HidUsage::keyboard(5), true).unwrap()
    );
    d.recover_buttons(&[]).unwrap();
    d.repeat(200_000).unwrap();
    assert_eq!(d.port().sends.len(), 3);
}

#[test]
fn modifier_down_preserves_typematic_and_unmapped_up_cancels_it() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.key(HidUsage::keyboard(0xe1), true, 1).unwrap();
    d.repeat(250).unwrap();
    assert_eq!(
        d.port().sends.last(),
        Some(&key_packet(HidUsage::keyboard(4), true).unwrap())
    );
    assert!(d.key(HidUsage { page: 12, id: 4 }, false, 251).is_err());
    assert!(!d.repeating());
}

#[test]
fn unmapped_button_up_cancels_typematic_before_refusal() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    assert!(d.button(MouseButton(6), false).is_err());
    assert!(!d.repeating());
    d.repeat(1000).unwrap();
    assert_eq!(d.port().sends.len(), 1);
}

#[test]
fn pid_reuse_with_different_start_time_drains_before_new_button() {
    let mut d = driver(Fake::default());
    d.key(HidUsage::keyboard(4), true, 0).unwrap();
    d.port_mut().foreground.push_back(Ok(Foreground {
        born: 14,
        ..focus(1, 0x2000)
    }));
    assert!(d.button(MouseButton::PRIMARY, true).is_err());
    assert!(d.held_buttons().is_empty());
    assert_eq!(
        d.port().sends.last(),
        Some(&key_packet(HidUsage::keyboard(4), false).unwrap())
    );
}

fn monitor(
    path: &str,
    rect: [i32; 4],
    primary: bool,
) -> crosspane_platform_windows::model::geometry::MonitorProbe {
    crosspane_platform_windows::model::geometry::MonitorProbe {
        device_path: path.into(),
        name: path.into(),
        rc_monitor: rect,
        rc_work: rect,
        primary,
        dpi: 96,
        refresh_millihz: 60_000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

#[test]
fn absolute_device_mapping_negative_origin_retained_ids_and_invalid_points() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("left", [-100, 0, 0, 100], false),
        monitor("primary", [0, 0, 100, 100], true),
    ];
    let mut ids = DisplayIds::default();
    let left = ids.assign("left").unwrap();
    let right = ids.assign("primary").unwrap();
    assert_eq!(
        absolute_move(&probes, &mut ids, left, PointDevice::new(0.0, 0.0))
            .unwrap()
            .0,
        Packet::Move { x: 0, y: 0 }
    );
    assert_eq!(
        absolute_move(&probes, &mut ids, right, PointDevice::new(99.0, 99.0))
            .unwrap()
            .0,
        Packet::Move { x: 65535, y: 65535 }
    );
    for point in [
        PointDevice::new(100.0, 0.0),
        PointDevice::new(-1.0, 0.0),
        PointDevice::new(f64::NAN, 1.0),
    ] {
        assert!(absolute_move(&probes, &mut ids, left, point).is_err());
    }
    assert!(absolute_move(&probes[1..], &mut ids, left, PointDevice::new(0.0, 0.0)).is_err());
    assert_eq!(ids.assign("left").unwrap(), left);
}

fn twin(path: &str, rect: [i32; 4]) -> crosspane_platform_windows::model::geometry::MonitorProbe {
    crosspane_platform_windows::model::geometry::MonitorProbe {
        twin: true,
        ..monitor(path, rect, false)
    }
}

#[test]
fn absolute_move_maps_a_twin_by_id_into_its_rect() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    // Bounding box [0, 256) x [0, 128): spans 255 and 127, so 65535 / span is exact here.
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
    ];
    let mut ids = DisplayIds::default();
    let primary = ids.assign("primary").unwrap();
    let twin_id = ids.assign("twin").unwrap();
    let (packet, probe) =
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(0.0, 0.0)).unwrap();
    assert_eq!(packet, Packet::Move { x: 32896, y: 0 });
    assert_eq!(probe, probes[1]);
    let (packet, probe) =
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(127.0, 127.0)).unwrap();
    assert_eq!(packet, Packet::Move { x: 65535, y: 65535 });
    assert_eq!(probe, probes[1]);
    assert_eq!(
        absolute_move(&probes, &mut ids, primary, PointDevice::new(127.0, 127.0))
            .unwrap()
            .0,
        Packet::Move { x: 32639, y: 65535 }
    );
}

#[test]
fn absolute_move_refuses_unknown_twin_ids_points_and_degenerate_rects() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
        twin("flat", [256, 0, 256, 128]),
    ];
    let mut ids = DisplayIds::default();
    let twin_id = ids.assign("twin").unwrap();
    let flat = ids.assign("flat").unwrap();
    let ghost = ids.assign("ghost").unwrap();
    for (display, point) in [
        (ghost, PointDevice::new(0.0, 0.0)),
        (twin_id, PointDevice::new(128.0, 0.0)),
        (twin_id, PointDevice::new(-1.0, 0.0)),
        (flat, PointDevice::new(0.0, 0.0)),
    ] {
        assert!(matches!(
            absolute_move(&probes, &mut ids, display, point),
            Err(PlatformError::NotFound)
        ));
    }
}

#[test]
fn absolute_move_refuses_two_twin_probes_for_one_id() {
    use crosspane_platform_windows::model::geometry::DisplayIds;
    use crosspane_types::geom::PointDevice;
    let probes = [
        monitor("primary", [0, 0, 128, 128], true),
        twin("twin", [128, 0, 256, 128]),
        twin("twin", [256, 0, 384, 128]),
        twin("other", [384, 0, 512, 128]),
    ];
    let mut ids = DisplayIds::default();
    let twin_id = ids.assign("twin").unwrap();
    let other_id = ids.assign("other").unwrap();
    assert!(matches!(
        absolute_move(&probes, &mut ids, twin_id, PointDevice::new(0.0, 0.0)),
        Err(PlatformError::NotFound)
    ));
    // Another twin path does not share the ambiguity.
    assert!(absolute_move(&probes, &mut ids, other_id, PointDevice::new(0.0, 0.0)).is_ok());
    // With one of the two duplicates gone, the same id resolves.
    assert!(absolute_move(&probes[..2], &mut ids, twin_id, PointDevice::new(0.0, 0.0)).is_ok());
}

#[test]
fn fractional_wheel_units_do_not_add_pixels_or_submit_zero_stops() {
    let delta = ScrollDelta {
        v120_x: -15,
        v120_y: 60,
        pixels: Some(VectorLogical::new(-200.0, 300.0)),
        phase: ScrollPhase::Changed,
        stop_x: false,
        stop_y: false,
    };
    assert_eq!(
        scroll_packets(delta),
        [
            Packet::Wheel {
                horizontal: true,
                v120: -15
            },
            Packet::Wheel {
                horizontal: false,
                v120: 60
            }
        ]
    );
    assert!(
        scroll_packets(ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: None,
            phase: ScrollPhase::Ended,
            stop_x: true,
            stop_y: true
        })
        .is_empty()
    );
}

fn pixels(x: f64, y: f64) -> ScrollDelta {
    ScrollDelta {
        v120_x: 0,
        v120_y: 0,
        pixels: Some(VectorLogical::new(x, y)),
        phase: ScrollPhase::Changed,
        stop_x: false,
        stop_y: false,
    }
}

fn wheel(horizontal: bool, v120: i32) -> Packet {
    Packet::Wheel { horizontal, v120 }
}

#[test]
fn pixel_only_sources_use_each_axis_setting_and_v120_precedence() {
    let mut d = driver(Fake::default());
    d.set_scroll_settings(ScrollSettings::new(Some(6), Some(3)));
    d.scroll(pixels(-12.0, 12.0)).unwrap();
    d.scroll(ScrollDelta {
        v120_x: -15,
        ..pixels(500.0, 10.0)
    })
    .unwrap();
    assert_eq!(
        d.port().sends,
        [
            wheel(true, -12),
            wheel(false, 24),
            wheel(true, -15),
            wheel(false, 20)
        ]
    );
    for setting in [None, Some(0), Some(u32::MAX)] {
        let mut d = driver(Fake::default());
        d.set_scroll_settings(ScrollSettings::new(setting, setting));
        d.scroll(pixels(10.0, -10.0)).unwrap();
        assert_eq!(d.port().sends, [wheel(true, 20), wheel(false, -20)]);
    }
}

#[test]
fn signed_subunits_accumulate_independently_and_sign_changes_drop_debt() {
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.2, -0.3)).unwrap();
    assert!(d.port().sends.is_empty());
    d.scroll(pixels(0.2, -0.3)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, -1)]);
    d.scroll(pixels(0.2, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, -1), wheel(true, 1)]);
    d.port_mut().sends.clear();
    d.reset_scroll();
    d.scroll(pixels(0.4, 0.0)).unwrap();
    d.scroll(pixels(-0.2, 0.0)).unwrap();
    d.scroll(pixels(-0.3, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, -1)]);
}

#[test]
fn lifecycle_and_per_axis_stops_reset_fractional_debt() {
    for phase in [
        ScrollPhase::Began,
        ScrollPhase::Ended,
        ScrollPhase::Cancelled,
        ScrollPhase::MomentumEnded,
    ] {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.4)).unwrap();
        d.scroll(ScrollDelta {
            phase,
            ..pixels(0.1, 0.1)
        })
        .unwrap();
        assert!(d.port().sends.is_empty());
    }
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.3, 0.3)).unwrap();
    d.scroll(ScrollDelta {
        stop_x: true,
        ..pixels(0.0, 0.0)
    })
    .unwrap();
    d.scroll(pixels(0.3, 0.3)).unwrap();
    assert_eq!(d.port().sends, [wheel(false, 1)]);
    d.port_mut().sends.clear();
    d.reset_scroll();
    for phase in [ScrollPhase::MomentumBegan, ScrollPhase::MomentumChanged] {
        d.scroll(ScrollDelta {
            phase,
            ..pixels(0.3, 0.0)
        })
        .unwrap();
    }
    assert_eq!(d.port().sends, [wheel(true, 1)]);
}

#[test]
fn nonfinite_pixels_are_rejected_and_large_emissions_clamp_without_backlog() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.4)).unwrap();
        assert!(
            d.scroll(ScrollDelta {
                v120_x: 1,
                ..pixels(value, 1.0)
            })
            .is_err()
        );
        d.scroll(pixels(0.1, 0.1)).unwrap();
        assert!(d.port().sends.is_empty());
    }
    let mut d = driver(Fake::default());
    d.scroll(pixels(f64::MAX, -f64::MAX)).unwrap();
    d.scroll(pixels(0.1, -0.1)).unwrap();
    assert_eq!(
        d.port().sends,
        [wheel(true, i32::MAX), wheel(false, i32::MIN)]
    );
}

#[test]
fn scroll_remainders_reset_on_generation_gate_release_and_submission_failures() {
    for reset in 0..6 {
        let mut d = driver(Fake::default());
        d.scroll(pixels(0.4, 0.0)).unwrap();
        match reset {
            0 => {
                d.release_keys().unwrap();
            }
            1 => {
                d.release_buttons().unwrap();
            }
            2 => {
                d.gate().set_engine_permits(false);
                assert!(d.scroll(pixels(0.0, 0.0)).is_err());
                d.gate().set_engine_permits(true);
            }
            3 => {
                d.gate().set_engine_permits(false);
                d.gate().set_engine_permits(true);
            }
            4 => {
                let mut changed = focus(1, 0x2000);
                changed.generation = 1;
                d.port_mut().foreground.push_back(Ok(changed));
            }
            _ => {
                d.port_mut().failed = true;
                assert!(d.scroll(pixels(0.2, 0.0)).is_err());
                d.port_mut().failed = false;
                d.port_mut().sends.clear();
            }
        }
        d.scroll(pixels(0.1, 0.0)).unwrap();
        assert!(d.port().sends.is_empty(), "reset case {reset}");
    }
}

#[test]
fn fresh_fence_between_scroll_axes_refuses_the_second_submission() {
    let mut d = driver(Fake {
        foreground: [
            Ok(focus(1, 0x2000)),
            Ok(focus(1, 0x2000)),
            Ok(focus(2, 0x2000)),
        ]
        .into(),
        ..Default::default()
    });
    assert!(matches!(
        d.scroll(pixels(10.0, 10.0)),
        Err(PlatformError::SecureInput)
    ));
    assert_eq!(d.port().sends, [wheel(true, 20)]);
    d.scroll(pixels(0.1, 0.1)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, 20)]);
}

#[test]
fn unrelated_key_and_button_releases_and_equal_lock_requests_keep_pixel_fractions() {
    let mut d = driver(Fake::default());
    d.scroll(pixels(0.4, 0.0)).unwrap();
    d.key(HidUsage::keyboard(0x73), false, 0).unwrap();
    d.button(MouseButton::PRIMARY, false).unwrap();
    d.set_locks(LockKeys {
        caps_lock: Some(false),
        ..LockKeys::default()
    })
    .unwrap();
    d.port_mut().sends.clear();
    d.scroll(pixels(0.1, 0.0)).unwrap();
    assert_eq!(d.port().sends, [wheel(true, 1)]);
}

#[test]
fn unknown_lock_requests_leave_unchanged_and_known_requests_never_guess_actual_state() {
    let mut d = driver(Fake {
        lock_state: Some(LockKeys::default()),
        ..Fake::default()
    });
    assert_eq!(d.locks().unwrap(), LockKeys::default());
    d.set_locks(LockKeys::default()).unwrap();
    for wanted in [
        LockKeys {
            caps_lock: Some(true),
            ..LockKeys::default()
        },
        LockKeys {
            num_lock: Some(false),
            ..LockKeys::default()
        },
        LockKeys {
            scroll_lock: Some(true),
            ..LockKeys::default()
        },
    ] {
        assert!(matches!(
            d.set_locks(wanted),
            Err(PlatformError::SecureInput)
        ));
    }
    assert!(d.port().sends.is_empty());
}
