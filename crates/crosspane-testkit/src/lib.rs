//! Fake platform implementations, in-memory/simulated network (loss, reorder, partition, delay),
//! deterministic clock, and golden-image helpers.

mod clipboard;
pub use clipboard::FakeClipboardHost;

/// Where a physical release goes; native settlement and suppressed tails are separate events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalRelease {
    Native,
    Captured,
    SuppressedTail,
}

/// Strict OS-visible button accounting for sans-I/O drag tests.
#[derive(Debug, Default)]
pub struct DragSeat {
    pub physical: bool,
    pub presses: u32,
    native: Option<crosspane_types::id::NodeId>,
    capturing: bool,
    tail: bool,
    continuation: bool,
    held: std::collections::BTreeSet<crosspane_types::id::NodeId>,
    totals: std::collections::BTreeMap<crosspane_types::id::NodeId, (u32, u32)>,
    arms: std::collections::BTreeSet<(crosspane_types::id::NodeId, u64, u32)>,
}

impl DragSeat {
    pub fn original_down(&mut self, seat: crosspane_types::id::NodeId) {
        assert!(!self.physical);
        self.physical = true;
        self.native = Some(seat);
        self.gesture();
        self.down(seat);
    }
    pub fn gesture(&mut self) {
        self.presses = 0;
        self.continuation = false;
    }
    pub fn settle(&mut self, seat: crosspane_types::id::NodeId) {
        assert_eq!(
            self.native.take(),
            Some(seat),
            "unmatched native settlement"
        );
        self.up(seat);
        self.capturing = true;
    }
    pub fn ordinary_capture(&mut self) {
        self.capturing = true;
    }
    pub fn captured_down(&mut self) {
        assert!(self.capturing && !self.physical);
        self.physical = true;
    }
    pub fn end_capture(&mut self) {
        self.capturing = false;
        self.tail = self.physical && self.native.is_none();
    }
    pub fn physical_up(&mut self) -> Option<PhysicalRelease> {
        if !self.physical {
            return None;
        }
        self.physical = false;
        if let Some(seat) = self.native.take() {
            self.up(seat);
            Some(PhysicalRelease::Native)
        } else if self.tail {
            self.tail = false;
            Some(PhysicalRelease::SuppressedTail)
        } else {
            assert!(self.capturing);
            Some(PhysicalRelease::Captured)
        }
    }
    pub fn press_at(&mut self) {
        assert!(self.physical, "PressAt after physical release");
        self.presses += 1;
        assert_eq!(self.presses, 1, "multiple continuation presses");
        self.continuation = true;
    }
    pub fn down(&mut self, node: crosspane_types::id::NodeId) {
        assert!(self.held.insert(node), "double primary down");
        self.totals.entry(node).or_default().0 += 1;
    }
    pub fn up(&mut self, node: crosspane_types::id::NodeId) {
        assert!(self.held.remove(&node), "unmatched OS-visible primary up");
        self.totals.entry(node).or_default().1 += 1;
    }
    pub fn release_all(&mut self, node: crosspane_types::id::NodeId) {
        if self.held.contains(&node) {
            self.up(node);
        }
    }
    pub fn has_hold(&self, node: crosspane_types::id::NodeId) -> bool {
        self.held.contains(&node)
    }
    pub fn armed(&self) -> bool {
        !self.arms.is_empty()
    }
    pub fn awaiting_continuation(&self) -> bool {
        self.continuation
    }
    pub fn arm(&mut self, node: crosspane_types::id::NodeId, projection: u64, token: u32) {
        self.arms.insert((node, projection, token));
    }
    pub fn disarm(&mut self, node: crosspane_types::id::NodeId, projection: u64, token: u32) {
        self.arms.remove(&(node, projection, token));
        self.continuation = false;
    }
    pub fn consume_arm(&mut self, node: crosspane_types::id::NodeId) {
        assert!(
            self.arms.iter().any(|(peer, _, _)| *peer == node),
            "unarmed primary press"
        );
        self.arms.retain(|(peer, _, _)| *peer != node);
        self.continuation = false;
    }
    pub fn balanced(&self) -> bool {
        !self.physical
            && !self.tail
            && self.held.is_empty()
            && self.arms.is_empty()
            && self.totals.values().all(|(downs, ups)| downs == ups)
    }
    pub fn totals(&self, node: crosspane_types::id::NodeId) -> (u32, u32) {
        self.totals.get(&node).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::id::NodeId;
    const A: NodeId = NodeId([1; 32]);
    const B: NodeId = NodeId([2; 32]);
    #[test]
    #[should_panic(expected = "unmatched OS-visible primary up")]
    fn drag_oracle_rejects_duplicate_up() {
        let mut seat = DragSeat::default();
        seat.down(A);
        seat.up(A);
        seat.up(A);
    }
    #[test]
    #[should_panic(expected = "unmatched OS-visible primary up")]
    fn drag_oracle_rejects_wrong_node_up() {
        let mut seat = DragSeat::default();
        seat.down(A);
        seat.up(B);
    }
    #[test]
    fn settlement_and_captured_physical_release_are_distinct() {
        let mut seat = DragSeat::default();
        seat.original_down(A);
        seat.settle(A);
        assert_eq!(seat.physical_up(), Some(PhysicalRelease::Captured));
        assert_eq!(seat.totals(A), (1, 1));
        assert!(seat.balanced());
    }
    #[test]
    fn failed_activation_after_settlement_suppresses_physical_tail() {
        let mut seat = DragSeat::default();
        seat.original_down(A);
        seat.settle(A);
        seat.end_capture();
        assert_eq!(seat.physical_up(), Some(PhysicalRelease::SuppressedTail));
        assert_eq!(seat.totals(A), (1, 1));
        assert!(seat.balanced());
    }
}
