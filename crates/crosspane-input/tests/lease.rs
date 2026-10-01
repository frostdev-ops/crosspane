#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError, MemoryJournal};
use crosspane_input::lease::{Action, ControllerLease, TargetLedger};
use crosspane_input::timing::LEASE_TIMEOUT;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

const ITEMS: [Held; 8] = [
    Held::Key(HidUsage::keyboard(4)),
    Held::Key(HidUsage::keyboard(5)),
    Held::Key(HidUsage::keyboard(0xE1)),
    Held::Key(HidUsage {
        page: 0x0C,
        id: 0xB5,
    }),
    Held::Key(HidUsage {
        page: u16::MAX,
        id: u16::MAX,
    }),
    Held::Button(MouseButton::PRIMARY),
    Held::Button(MouseButton::SECONDARY),
    Held::Button(MouseButton(u8::MAX)),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Down(Held),
    Up(Held),
    Applied(Action),
}

#[derive(Debug, Default)]
struct Observed {
    journal: MemoryJournal,
    events: Vec<Event>,
}

#[derive(Clone, Debug, Default)]
struct ObservedJournal(Arc<Mutex<Observed>>);

impl Journal for ObservedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut observed = self.0.lock().unwrap();
        observed.journal.record_down(item)?;
        observed.events.push(Event::Down(item));
        Ok(())
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut observed = self.0.lock().unwrap();
        observed.journal.record_up(item)?;
        observed.events.push(Event::Up(item));
        Ok(())
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        self.0.lock().unwrap().journal.held()
    }
}

fn time(ms: u64) -> MonoTime {
    MonoTime::from_nanos(ms * 1_000_000)
}

fn selected(mask: u8) -> Vec<Held> {
    ITEMS
        .iter()
        .enumerate()
        .filter_map(|(index, &item)| (mask & (1 << index) != 0).then_some(item))
        .rev()
        .collect()
}

#[derive(Clone, Debug)]
enum Operation {
    Input(u8, bool),
    HeartbeatAll,
    HeartbeatSubset(u8),
    HeartbeatExtras(u8),
    Tick,
    ReleaseAll,
    Confirm(u8),
    ConfirmAll,
}

fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        4 => (0_u8..8, any::<bool>()).prop_map(|(item, down)| Operation::Input(item, down)),
        1 => Just(Operation::HeartbeatAll),
        2 => any::<u8>().prop_map(Operation::HeartbeatSubset),
        2 => any::<u8>().prop_map(Operation::HeartbeatExtras),
        3 => Just(Operation::Tick),
        1 => Just(Operation::ReleaseAll),
        2 => any::<u8>().prop_map(Operation::Confirm),
        1 => Just(Operation::ConfirmAll),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10_000,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn invariant_2(steps in prop::collection::vec((0_u16..=400, operation()), 1..128)) {
        let journal = ObservedJournal::default();
        let (mut ledger, recovery) = TargetLedger::open(journal.clone()).unwrap();
        prop_assert!(recovery.is_empty());
        let mut now = MonoTime::ZERO;
        let mut lease_start = None;
        let mut pressed = BTreeSet::new();
        let mut pending = BTreeSet::new();

        for (advance, operation) in steps {
            now = now.saturating_add(Duration::from_millis(u64::from(advance)));
            let before_events = journal.0.lock().unwrap().events.len();
            let mut expected = Vec::new();
            let mut confirmation = None;
            let mut heartbeat = None;
            let actions = match operation {
                Operation::Input(index, down) => {
                    lease_start.get_or_insert(now);
                    let item = ITEMS[usize::from(index)];
                    if down && !pressed.contains(&item) {
                        expected.push(Action::Press(item));
                    } else if !down && pressed.contains(&item) {
                        expected.push(Action::Release(item));
                    }
                    ledger.on_input(item, down, now).unwrap().into_iter().collect::<Vec<_>>()
                }
                Operation::HeartbeatAll | Operation::HeartbeatSubset(_) | Operation::HeartbeatExtras(_) => {
                    let listed = match operation {
                        Operation::HeartbeatAll => pressed.iter().copied().collect::<Vec<_>>(),
                        Operation::HeartbeatSubset(mask) => selected(mask)
                            .into_iter().filter(|item| pressed.contains(item)).collect(),
                        Operation::HeartbeatExtras(mask) => {
                            let mut listed = selected(mask);
                            // This item is never pressed, so this mode always contains an extra.
                            listed.push(Held::Button(MouseButton(42)));
                            listed
                        }
                        _ => unreachable!(),
                    };
                    let listed_set: BTreeSet<_> = listed.iter().copied().collect();
                    expected.extend(pressed.difference(&listed_set).copied().map(Action::Release));
                    lease_start = Some(now);
                    let actions = ledger.on_heartbeat(&listed, now);
                    heartbeat = Some(listed_set);
                    actions
                }
                Operation::Tick => {
                    let expired = lease_start.is_some_and(|start| {
                        now.saturating_duration_since(start) > LEASE_TIMEOUT
                    });
                    if expired {
                        expected.extend(pressed.iter().copied().map(Action::Release));
                    }
                    let actions = ledger.on_tick(now);
                    if expired {
                        prop_assert!(ledger.held().is_empty());
                    }
                    actions
                }
                Operation::ReleaseAll => {
                    expected.extend(pressed.iter().copied().map(Action::Release));
                    ledger.release_all()
                }
                Operation::Confirm(mask) => {
                    let items = selected(mask);
                    ledger.confirm_released(&items).unwrap();
                    confirmation = Some(items);
                    Vec::new()
                }
                Operation::ConfirmAll => {
                    let items = pending.iter().copied().collect::<Vec<_>>();
                    ledger.confirm_released(&items).unwrap();
                    confirmation = Some(items);
                    Vec::new()
                }
            };
            prop_assert_eq!(&actions, &expected);
            {
                let mut observed = journal.0.lock().unwrap();
                if let Some(items) = confirmation {
                    let expected_events: Vec<_> = items.into_iter()
                        .filter(|item| pending.remove(item))
                        .map(Event::Up).collect();
                    prop_assert_eq!(&observed.events[before_events..], expected_events.as_slice());
                } else if actions.iter().any(|action| matches!(action, Action::Press(_))) {
                    prop_assert_eq!(observed.events.len(), before_events + 1);
                } else {
                    // Release requests cannot clear the journal before injector confirmation.
                    prop_assert_eq!(observed.events.len(), before_events);
                }
                for action in actions {
                    match action {
                        Action::Press(item) => {
                            prop_assert_eq!(observed.events.last(), Some(&Event::Down(item)));
                            prop_assert!(observed.journal.held().unwrap().contains(&item));
                            prop_assert!(pressed.insert(item));
                            pending.remove(&item);
                        }
                        Action::Release(item) => {
                            prop_assert!(pressed.remove(&item));
                            pending.insert(item);
                        }
                    }
                    observed.events.push(Event::Applied(action));
                }
            }

            let held = ledger.held();
            let held_set: BTreeSet<_> = held.iter().copied().collect();
            prop_assert_eq!(&held_set, &pressed);
            prop_assert!(held.windows(2).all(|pair| pair[0] < pair[1]));
            if let Some(listed) = heartbeat {
                prop_assert!(held_set.is_subset(&listed));
            }
            let journal_held = journal.held().unwrap();
            let journal_set: BTreeSet<_> = journal_held.iter().copied().collect();
            prop_assert!(held_set.is_subset(&journal_set));
            prop_assert_eq!(journal_set, pressed.union(&pending).copied().collect::<BTreeSet<_>>());
            if pending.is_empty() {
                prop_assert_eq!(journal_held, held);
            }
            let deadline = if pressed.is_empty() {
                None
            } else {
                lease_start.map(|start| start.saturating_add(LEASE_TIMEOUT))
            };
            prop_assert_eq!(ledger.next_deadline(), deadline);
        }
        ledger.confirm_released(&pending.iter().copied().collect::<Vec<_>>()).unwrap();
        prop_assert_eq!(journal.held().unwrap(), ledger.held());
    }
}

#[test]
fn crash_recovery() {
    let journal = ObservedJournal::default();
    let (mut ledger, recovery) = TargetLedger::open(journal.clone()).unwrap();
    assert!(recovery.is_empty());
    assert_eq!(ledger.next_deadline(), None);
    for item in [ITEMS[7], ITEMS[2], ITEMS[0]] {
        assert_eq!(
            ledger.on_input(item, true, time(10)).unwrap(),
            Some(Action::Press(item))
        );
    }
    let held = ledger.held();
    drop(ledger);
    let (mut ledger, recovery) = TargetLedger::open(journal.clone()).unwrap();
    assert_eq!(recovery, held);
    ledger.recovered(&recovery[..1]).unwrap();
    assert_eq!(journal.held().unwrap(), recovery[1..]);
    ledger.recovered(&recovery[1..]).unwrap();
    assert!(ledger.held().is_empty());
    assert!(journal.held().unwrap().is_empty());
}

#[test]
fn controller_heartbeat_due_times() {
    let mut lease = ControllerLease::new(time(1_000));
    assert_eq!(lease.next_heartbeat(true), time(1_000));
    assert_eq!(lease.next_heartbeat(false), time(1_000));
    lease.heartbeat_sent(time(1_020));
    lease.sent(1, time(1_020));
    assert_eq!(lease.next_heartbeat(true), time(1_070));
    assert_eq!(lease.next_heartbeat(false), time(1_270));
    lease.sent(2, time(1_030));
    assert_eq!(lease.next_heartbeat(true), time(1_070));
    lease.heartbeat_sent(MonoTime::from_nanos(u64::MAX - 1));
    assert_eq!(lease.next_heartbeat(false), MonoTime::from_nanos(u64::MAX));
}

#[test]
fn controller_lost_minimum_and_rtt_scaled_timeout() {
    let mut lease = ControllerLease::new(time(1_000));
    assert!(!lease.lost(time(9_000), None));
    lease.sent(1, time(1_000));
    assert!(!lease.lost(time(999), None));
    assert!(!lease.lost(time(1_150), None));
    assert!(lease.lost(MonoTime::from_nanos(time(1_150).as_nanos() + 1), None));
    assert!(lease.lost(time(1_151), Some(Duration::from_millis(10))));
    let rtt = Some(Duration::from_millis(100));
    assert!(!lease.lost(time(1_400), rtt));
    assert!(lease.lost(MonoTime::from_nanos(time(1_400).as_nanos() + 1), rtt));
    assert!(!lease.lost(MonoTime::from_nanos(u64::MAX), Some(Duration::MAX)));
}

#[test]
fn controller_acks_clear_oldest_unacknowledged_message() {
    let mut lease = ControllerLease::new(time(0));
    lease.sent(1, time(0));
    lease.sent(2, time(50));
    lease.sent(3, time(100));
    assert!(lease.lost(time(151), None));
    lease.acked(1, time(151));
    assert!(!lease.lost(time(151), None));
    assert!(lease.lost(time(201), None));
    lease.acked(2, time(201));
    assert!(!lease.lost(time(201), None));
    assert!(lease.lost(time(251), None));
    lease.sent(5, time(300));
    lease.acked(5, time(350));
    assert!(!lease.lost(time(9_000), None));
}

#[test]
fn controller_stale_and_unknown_acks_are_ignored() {
    let mut lease = ControllerLease::new(time(0));
    lease.acked(100, time(0));
    lease.sent(1, time(0));
    lease.sent(3, time(50));
    for seq in [0, 2, 4, u32::MAX] {
        lease.acked(seq, time(151));
        assert!(lease.lost(time(151), None));
    }
    lease.acked(1, time(151));
    assert!(!lease.lost(time(151), None));
    for seq in [0, 1, 2, 4, u32::MAX] {
        lease.acked(seq, time(201));
        assert!(lease.lost(time(201), None));
    }
    lease.acked(3, time(201));
    assert!(!lease.lost(time(9_000), None));
}

#[test]
fn controller_ack_deadline_matches_lost() {
    use core::time::Duration;
    let mut lease = ControllerLease::new(time(0));
    assert_eq!(lease.ack_deadline(None), None);
    lease.sent(1, time(10));
    assert_eq!(lease.ack_deadline(None), Some(time(160)));
    assert_eq!(
        lease.ack_deadline(Some(Duration::from_millis(100))),
        Some(time(410))
    );
    assert!(!lease.lost(time(160), None));
    assert!(lease.lost(time(161), None));
    lease.acked(1, time(20));
    assert_eq!(lease.ack_deadline(None), None);
}
