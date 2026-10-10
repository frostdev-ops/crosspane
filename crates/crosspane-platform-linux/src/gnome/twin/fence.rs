//! The twin's pointer fence (WP-G2.4 B2): a state machine over the Shell bridge's
//! `SetPointerFence` / `ClearPointerFence`.
//!
//! The fence keeps the **local** pointer out of the twin (four barriers around its rectangle that
//! only let motion pass outward). Barriers stop injected absolute motion too (the same code path
//! in Mutter), so the EIS injector tells this machine about every move that targets the twin
//! ([`Fence::note_move`]): the fence is cleared, the move goes through, and the fence is armed
//! again [`super::FENCE_REARM`] after the last such move ([`Fence::rearm_due`], called by the
//! twin's timer thread when the deadline passes).
//!
//! The machine owns no clock and no thread: every call takes the time, and the calls that change
//! when the next action is due return that deadline. A failed bridge call is logged at `warn`
//! (once per run of failures, then `debug`) and never fails anything: a fence that cannot be set
//! is a missing nicety, one that cannot be cleared costs the move that needed it and is tried
//! again by the next one.

use std::time::{Duration, Instant};

use super::Layout;

/// A rectangle in global logical coordinates: `(x, y, width, height)`.
pub type Rect = (i32, i32, i32, i32);

/// The fence of one twin.
#[derive(Debug, Default)]
pub struct Fence {
    /// The rectangle to fence; `None` while there is no twin.
    rect: Option<Rect>,
    /// The bridge holds a fence for `rect` right now.
    armed: bool,
    /// When the last move that targeted the twin went through.
    last_move: Option<Instant>,
    /// The last bridge call failed (so the next failure is only a `debug` line).
    failing: bool,
}

impl Fence {
    /// A twin exists at `rect` (new, grown or moved): fence it. A fence that was lowered for a
    /// move less than `rearm` ago stays lowered, and comes back at the usual deadline, which is
    /// returned.
    pub fn target(
        &mut self,
        layout: &dyn Layout,
        rect: Rect,
        now: Instant,
        rearm: Duration,
    ) -> Option<Instant> {
        self.rect = Some(rect);
        if !self.armed
            && let Some(due) = self.last_move.map(|at| at + rearm)
            && now < due
        {
            return Some(due);
        }
        self.arm(layout, now + rearm)
    }

    /// A move targets the twin: lower the fence if it is up. Returns the deadline at which the
    /// fence is to come back.
    pub fn note_move(
        &mut self,
        layout: &dyn Layout,
        now: Instant,
        rearm: Duration,
    ) -> Option<Instant> {
        self.rect?;
        self.last_move = Some(now);
        if self.armed {
            match layout.clear_pointer_fence() {
                Ok(()) => {
                    self.armed = false;
                    self.failing = false;
                }
                Err(error) => self.report("clear", &error),
            }
        }
        Some(now + rearm)
    }

    /// The timer thread looks: if the fence is down and `rearm` has passed since the last move,
    /// put it up. Returns when to look next, if the fence is still down.
    pub fn rearm_due(
        &mut self,
        layout: &dyn Layout,
        now: Instant,
        rearm: Duration,
    ) -> Option<Instant> {
        self.rect?;
        if self.armed {
            return None;
        }
        let due = self.last_move.map_or(now, |at| at + rearm);
        if now < due {
            return Some(due);
        }
        self.arm(layout, now + rearm)
    }

    /// The twin is gone: take the fence down and forget the rectangle.
    pub fn clear(&mut self, layout: &dyn Layout) {
        let was = self.rect.take();
        if was.is_some() || self.armed {
            self.armed = false;
            if let Err(error) = layout.clear_pointer_fence() {
                self.report("clear", &error);
            }
        }
        self.last_move = None;
    }

    /// Whether the bridge holds a fence for the twin now.
    #[cfg(test)]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Sets the fence for the current rectangle. On failure returns when to try again.
    fn arm(&mut self, layout: &dyn Layout, retry_at: Instant) -> Option<Instant> {
        let (x, y, width, height) = self.rect?;
        match layout.set_pointer_fence(x, y, width, height) {
            Ok(()) => {
                self.armed = true;
                self.failing = false;
                None
            }
            Err(error) => {
                self.armed = false;
                self.report("set", &error);
                Some(retry_at)
            }
        }
    }

    fn report(&mut self, what: &str, error: &crosspane_platform::PlatformError) {
        if self.failing {
            tracing::debug!(%error, "pointer fence {what} failed again");
        } else {
            tracing::warn!(%error, "pointer fence {what} failed; projection continues without it");
        }
        self.failing = true;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crosspane_platform::PlatformError;

    use super::*;

    /// Records the fence calls; `fail` makes the next calls fail.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        fail: Mutex<bool>,
    }

    impl Recorder {
        fn log(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn failing(&self, fail: bool) {
            *self.fail.lock().unwrap() = fail;
        }

        fn answer(&self) -> Result<(), PlatformError> {
            if *self.fail.lock().unwrap() {
                Err(PlatformError::Timeout)
            } else {
                Ok(())
            }
        }
    }

    impl Layout for Recorder {
        fn save_layout(&self) -> Result<u32, PlatformError> {
            unreachable!()
        }

        fn restore_layout(&self, _: u32, _: &[u64]) -> Result<u32, PlatformError> {
            unreachable!()
        }

        fn set_pointer_fence(
            &self,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set {x},{y} {width}x{height}"));
            self.answer()
        }

        fn clear_pointer_fence(&self) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push("clear".into());
            self.answer()
        }
    }

    const REARM: Duration = Duration::from_millis(1500);
    const RECT: Rect = (4520, 0, 1152, 736);

    fn after(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_new_twin_is_fenced_at_once() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        assert_eq!(fence.target(&layout, RECT, t0, REARM), None);
        assert_eq!(layout.log(), ["set 4520,0 1152x736"]);
        assert!(fence.is_armed());
    }

    #[test]
    fn a_move_lowers_the_fence_once_and_the_deadline_follows_the_last_move() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        // First move: the fence comes down.
        assert_eq!(
            fence.note_move(&layout, after(t0, 100), REARM),
            Some(after(t0, 1600))
        );
        // Later moves cost no bridge call and push the deadline out.
        assert_eq!(
            fence.note_move(&layout, after(t0, 300), REARM),
            Some(after(t0, 1800))
        );
        assert_eq!(
            fence.note_move(&layout, after(t0, 900), REARM),
            Some(after(t0, 2400))
        );
        assert_eq!(layout.log(), ["set 4520,0 1152x736", "clear"]);
        assert!(!fence.is_armed());
    }

    #[test]
    fn the_fence_comes_back_only_after_the_quiet_time() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        fence.note_move(&layout, after(t0, 100), REARM);
        // Too early: nothing happens, and the deadline is reported again.
        assert_eq!(
            fence.rearm_due(&layout, after(t0, 1000), REARM),
            Some(after(t0, 1600))
        );
        assert!(!fence.is_armed());
        // On the deadline it is raised again.
        assert_eq!(fence.rearm_due(&layout, after(t0, 1600), REARM), None);
        assert!(fence.is_armed());
        assert_eq!(
            layout.log(),
            ["set 4520,0 1152x736", "clear", "set 4520,0 1152x736"]
        );
        // And nothing more to do while it is up.
        assert_eq!(fence.rearm_due(&layout, after(t0, 9000), REARM), None);
        // The next move lowers it again.
        fence.note_move(&layout, after(t0, 9100), REARM);
        assert_eq!(layout.log().last().unwrap(), "clear");
    }

    #[test]
    fn a_twin_that_grows_mid_interaction_stays_unfenced_until_the_deadline() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        fence.note_move(&layout, after(t0, 100), REARM);
        // Grown 500 ms after the last move: the new rectangle is remembered, nothing is set yet.
        let bigger = (4520, 0, 1504, 736);
        assert_eq!(
            fence.target(&layout, bigger, after(t0, 600), REARM),
            Some(after(t0, 1600))
        );
        assert_eq!(layout.log(), ["set 4520,0 1152x736", "clear"]);
        // The deadline raises the fence for the new rectangle.
        assert_eq!(fence.rearm_due(&layout, after(t0, 1600), REARM), None);
        assert_eq!(layout.log().last().unwrap(), "set 4520,0 1504x736");
    }

    #[test]
    fn a_twin_that_grows_while_fenced_replaces_the_fence_at_once() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        fence.target(&layout, (4520, 0, 1504, 736), after(t0, 50), REARM);
        assert_eq!(layout.log(), ["set 4520,0 1152x736", "set 4520,0 1504x736"]);
        assert!(fence.is_armed());
    }

    #[test]
    fn a_long_quiet_twin_is_fenced_again_when_it_changes() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        fence.note_move(&layout, after(t0, 10), REARM);
        // Long after the last move the machine has not yet been asked to re-arm (the timer was
        // late): a new rectangle fences immediately.
        fence.target(&layout, (4520, 0, 1504, 736), after(t0, 5000), REARM);
        assert_eq!(layout.log().last().unwrap(), "set 4520,0 1504x736");
        assert!(fence.is_armed());
    }

    #[test]
    fn moves_without_a_twin_do_nothing() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        assert_eq!(fence.note_move(&layout, t0, REARM), None);
        assert_eq!(fence.rearm_due(&layout, t0, REARM), None);
        fence.clear(&layout);
        assert!(layout.log().is_empty());
    }

    #[test]
    fn clearing_takes_the_fence_down_and_forgets_the_twin() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        fence.clear(&layout);
        assert_eq!(layout.log(), ["set 4520,0 1152x736", "clear"]);
        assert!(!fence.is_armed());
        // Nothing comes back, and moves are ignored.
        assert_eq!(fence.rearm_due(&layout, after(t0, 9000), REARM), None);
        assert_eq!(fence.note_move(&layout, after(t0, 9001), REARM), None);
        assert_eq!(layout.log().len(), 2);
    }

    #[test]
    fn a_failed_set_is_retried_at_the_next_deadline_and_never_fatal() {
        let layout = Recorder::default();
        layout.failing(true);
        let mut fence = Fence::default();
        let t0 = Instant::now();
        assert_eq!(
            fence.target(&layout, RECT, t0, REARM),
            Some(after(t0, 1500))
        );
        assert!(!fence.is_armed());
        layout.failing(false);
        assert_eq!(fence.rearm_due(&layout, after(t0, 1500), REARM), None);
        assert!(fence.is_armed());
    }

    #[test]
    fn a_failed_clear_keeps_the_fence_up_for_the_next_move_to_retry() {
        let layout = Recorder::default();
        let mut fence = Fence::default();
        let t0 = Instant::now();
        fence.target(&layout, RECT, t0, REARM);
        layout.failing(true);
        fence.note_move(&layout, after(t0, 10), REARM);
        assert!(fence.is_armed());
        layout.failing(false);
        fence.note_move(&layout, after(t0, 20), REARM);
        assert!(!fence.is_armed());
        assert_eq!(layout.log(), ["set 4520,0 1152x736", "clear", "clear"]);
    }
}
