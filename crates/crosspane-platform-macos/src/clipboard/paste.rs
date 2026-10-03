use std::time::{Duration, Instant};

use crosspane_platform::LocalPasteId;
use zeroize::Zeroizing;

pub(super) const LIMIT: Duration = Duration::from_millis(2500);
pub(super) const SLICE: Duration = Duration::from_millis(10);

enum Answer {
    Waiting,
    Empty,
    Data(Zeroizing<Vec<u8>>),
    Taken,
}

struct Pending {
    id: LocalPasteId,
    epoch: u64,
    deadline: Instant,
    answer: Answer,
}

#[derive(Default)]
pub(super) struct Pastes {
    next: u64,
    active: Option<Pending>,
}

impl Pastes {
    pub fn begin(&mut self, now: Instant, epoch: u64) -> Option<LocalPasteId> {
        if self.active.is_some() {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        let id = LocalPasteId(self.next);
        self.active = Some(Pending {
            id,
            epoch,
            deadline: now + LIMIT,
            answer: Answer::Waiting,
        });
        Some(id)
    }

    pub fn answer(
        &mut self,
        id: LocalPasteId,
        data: Option<Zeroizing<Vec<u8>>>,
        now: Instant,
        epoch: u64,
        open: bool,
    ) {
        let Some(pending) = self.active.as_mut().filter(|p| p.id == id) else {
            return;
        };
        if now >= pending.deadline || epoch != pending.epoch || !open {
            pending.answer = Answer::Empty;
        } else if matches!(pending.answer, Answer::Waiting) {
            pending.answer = data.map_or(Answer::Empty, Answer::Data);
        }
    }

    pub fn poll(
        &mut self,
        id: LocalPasteId,
        now: Instant,
        epoch: u64,
        open: bool,
    ) -> Option<Option<Zeroizing<Vec<u8>>>> {
        let Some(pending) = self.active.as_mut().filter(|p| p.id == id) else {
            return Some(None);
        };
        if now >= pending.deadline || epoch != pending.epoch || !open {
            pending.answer = Answer::Empty;
        }
        match std::mem::replace(&mut pending.answer, Answer::Taken) {
            Answer::Waiting => {
                pending.answer = Answer::Waiting;
                None
            }
            Answer::Data(bytes) => Some(Some(bytes)),
            Answer::Empty | Answer::Taken => Some(None),
        }
    }

    pub fn valid_delivery(&self, id: LocalPasteId, now: Instant, epoch: u64) -> bool {
        self.active.as_ref().is_some_and(|p| {
            p.id == id && p.epoch == epoch && now < p.deadline && matches!(p.answer, Answer::Taken)
        })
    }

    pub fn cancel(&mut self) {
        if let Some(pending) = &mut self.active {
            pending.answer = Answer::Empty;
        }
    }

    pub fn finish(&mut self, id: LocalPasteId) {
        if self.active.as_ref().is_some_and(|p| p.id == id) {
            self.active = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_fresh_and_reentrant_begin_is_refused_until_callback_finishes() {
        let mut slots = Pastes::default();
        let now = Instant::now();
        let first = slots.begin(now, 2).unwrap();
        assert!(slots.begin(now, 2).is_none());
        slots.answer(
            first,
            Some(Zeroizing::new(b"owned-fixture".to_vec())),
            now,
            2,
            true,
        );
        assert!(slots.begin(now, 2).is_none());
        assert!(slots.poll(first, now, 2, true).unwrap().is_some());
        assert!(slots.begin(now, 2).is_none());
        slots.finish(first);
        assert!(slots.begin(now, 2).unwrap().0 > first.0);
    }

    #[test]
    fn unknown_duplicate_and_expired_answers_never_replace_the_first_answer() {
        let mut slots = Pastes::default();
        let now = Instant::now();
        let id = slots.begin(now, 1).unwrap();
        slots.answer(
            LocalPasteId(id.0 + 1),
            Some(Zeroizing::new(b"unknown".to_vec())),
            now,
            1,
            true,
        );
        assert!(slots.poll(id, now, 1, true).is_none());
        slots.answer(id, Some(Zeroizing::new(b"first".to_vec())), now, 1, true);
        slots.answer(
            id,
            Some(Zeroizing::new(b"duplicate".to_vec())),
            now,
            1,
            true,
        );
        assert_eq!(
            &**slots.poll(id, now, 1, true).unwrap().as_ref().unwrap(),
            b"first"
        );
        slots.answer(id, Some(Zeroizing::new(b"consumed".to_vec())), now, 1, true);
        assert!(slots.poll(id, now, 1, true).unwrap().is_none());
        slots.finish(id);
        let next = slots.begin(now, 1).unwrap();
        slots.answer(id, Some(Zeroizing::new(b"stale".to_vec())), now, 1, true);
        slots.answer(
            next,
            Some(Zeroizing::new(b"late".to_vec())),
            now + LIMIT,
            1,
            true,
        );
        assert!(slots.poll(next, now + LIMIT, 1, true).unwrap().is_none());
    }

    #[test]
    fn expiry_uses_fake_time_and_returns_empty_at_the_cap() {
        let mut slots = Pastes::default();
        let now = Instant::now();
        let id = slots.begin(now, 1).unwrap();
        assert!(
            slots
                .poll(id, now + LIMIT - Duration::from_nanos(1), 1, true)
                .is_none()
        );
        assert!(slots.poll(id, now + LIMIT, 1, true).unwrap().is_none());
    }

    #[test]
    fn cancellation_gate_close_and_epoch_change_discard_ready_data() {
        for mode in 0..3 {
            let mut slots = Pastes::default();
            let now = Instant::now();
            let id = slots.begin(now, 1).unwrap();
            slots.answer(
                id,
                Some(Zeroizing::new(b"owned-fixture".to_vec())),
                now,
                1,
                true,
            );
            if mode == 0 {
                slots.cancel();
            }
            let answer = slots.poll(id, now, if mode == 1 { 2 } else { 1 }, mode != 2);
            assert!(answer.unwrap().is_none());
        }
    }

    #[test]
    fn cancel_after_poll_prevents_native_delivery_and_none_answers_empty() {
        let mut slots = Pastes::default();
        let now = Instant::now();
        let id = slots.begin(now, 1).unwrap();
        slots.answer(
            id,
            Some(Zeroizing::new(b"owned-fixture".to_vec())),
            now,
            1,
            true,
        );
        let held = slots.poll(id, now, 1, true).unwrap();
        assert!(held.is_some());
        assert!(slots.valid_delivery(id, now, 1));
        slots.cancel();
        assert!(!slots.valid_delivery(id, now, 1));
        slots.finish(id);
        let next = slots.begin(now, 1).unwrap();
        slots.answer(next, None, now, 1, true);
        assert!(slots.poll(next, now, 1, true).unwrap().is_none());
    }

    #[test]
    fn id_exhaustion_fails_empty_without_wrapping() {
        let mut slots = Pastes {
            next: u64::MAX,
            active: None,
        };
        assert!(slots.begin(Instant::now(), 1).is_none());
    }
}
