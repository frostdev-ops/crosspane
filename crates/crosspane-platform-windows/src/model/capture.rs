//! Pure, bounded capture watchdog decisions; no native input or hook-presence claims.
//! The lead-approved positive probe permits one F24 pair to escape after keyboard-hook loss.
//! A failed matching-up retry remains unverified; this model never turns failure into cleanup.

/// Cadence plus acknowledgment deadline is 500 ms, excluding native/scheduling delays.
pub const PROBE_INTERVAL_MS: u64 = 250;
pub const PROBE_TIMEOUT_MS: u64 = 250;
pub const MOUSE_MISMATCH_MS: u64 = 100;
pub const PUMP_TIMEOUT_MS: u64 = 100;
pub const F24_SCAN: u32 = 0x76;
pub const F24_VK: u32 = 0x87;

pub const IDLE: u64 = 0;
pub const ACTIVE: u64 = 1;
pub const PENDING: u64 = 2;

/// Counter and phase share one atomic, so cancellation invalidates the actual commit operation.
pub fn reserve(control: &std::sync::atomic::AtomicU64) -> Option<u64> {
    use std::sync::atomic::Ordering::*;
    let current = control.load(Acquire);
    if current & 3 != IDLE {
        return None;
    }
    control
        .compare_exchange(current, current | PENDING, AcqRel, Acquire)
        .ok()
        .map(|_| current | PENDING)
}

pub fn publish(
    control: &std::sync::atomic::AtomicU64,
    expected: u64,
    store_id: impl FnOnce(),
) -> bool {
    use std::sync::atomic::Ordering::*;
    if control.load(Acquire) != expected {
        return false;
    }
    store_id();
    control
        .compare_exchange(expected, (expected & !3) | ACTIVE, AcqRel, Acquire)
        .is_ok()
}

pub fn cancel(control: &std::sync::atomic::AtomicU64) -> u64 {
    use std::sync::atomic::Ordering::*;
    control
        .fetch_update(AcqRel, Acquire, |v| Some((v & !3).wrapping_add(4)))
        .unwrap_or_else(|v| v)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeAction {
    None,
    Pair,
    Up,
    Lost,
    CleanupUnverified,
}

/// The down obligation is recorded before submitting, including a partial batch.
#[derive(Debug)]
pub struct Probe {
    nonce: usize,
    next: u64,
    pending: Option<u64>,
    seen: u8,
    owed_up: bool,
    retries: u8,
    retry_at: u64,
    lost: bool,
}

impl Probe {
    pub fn new(nonce: usize, now: u64) -> Option<Self> {
        (nonce != 0).then_some(Self {
            nonce,
            next: now,
            pending: None,
            seen: 0,
            owed_up: false,
            retries: 0,
            retry_at: now,
            lost: false,
        })
    }

    pub fn acknowledge(
        &mut self,
        nonce: usize,
        scan: u32,
        vk: u32,
        injected: bool,
        up: bool,
    ) -> bool {
        if nonce != self.nonce || scan != F24_SCAN || vk != F24_VK || !injected {
            return false;
        }
        if self.pending.is_some() {
            self.seen |= if up { 2 } else { 1 };
        }
        true
    }

    pub fn poll(&mut self, now: u64) -> ProbeAction {
        if self.owed_up && self.lost && now >= self.retry_at {
            if self.retries == 3 {
                self.owed_up = false;
                return ProbeAction::CleanupUnverified;
            }
            self.retries += 1;
            self.retry_at = now.saturating_add(500);
            return ProbeAction::Up;
        }
        if self.lost {
            return ProbeAction::None;
        }
        if let Some(sent) = self.pending {
            if self.seen == 3 {
                self.pending = None;
                self.owed_up = false;
                self.next = now.saturating_add(PROBE_INTERVAL_MS);
            } else if now.saturating_sub(sent) >= PROBE_TIMEOUT_MS {
                self.lost = true;
                return ProbeAction::Lost;
            }
            return ProbeAction::None;
        }
        if now >= self.next {
            self.pending = Some(now);
            self.seen = 0;
            self.owed_up = true;
            return ProbeAction::Pair;
        }
        ProbeAction::None
    }

    pub fn submitted(&mut self, count: u32) -> ProbeAction {
        if count != 2 {
            self.lost = true;
            self.owed_up = count == 1;
            return ProbeAction::Lost;
        }
        self.owed_up = false;
        ProbeAction::None
    }

    pub fn up_submitted(&mut self, count: u32) {
        if count == 1 {
            self.owed_up = false;
        }
    }

    pub fn stop(&mut self, now: u64) {
        if self.lost {
            return;
        }
        self.lost = true;
        self.retry_at = now;
        // A complete accepted batch already contains an up; loss of acknowledgement does not
        // authorize another up. Only a partial submission owes recovery.
        self.owed_up &= self.pending.is_some() && self.seen != 3;
    }

    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn complete(&self) -> bool {
        self.seen == 3 && !self.lost
    }

    pub fn cleanup_pending(&self) -> bool {
        self.owed_up
    }
}

/// Constant-time callback ledger. 1 is locally held; 2 owes a swallowed physical tail.
/// Injected events are excluded by the caller before entering this function.
pub fn key_transition(state: u8, captured: bool, up: bool) -> (u8, bool) {
    if up {
        (0, state == 2 || (captured && state != 1))
    } else if state == 2 {
        (2, true)
    } else if state == 1 || !captured {
        (1, false)
    } else {
        (2, true)
    }
}

/// The physical-tail generation is separate from a reusable public CaptureId.
pub fn ledger_token(state: u8, current: Option<u64>, tail: u64) -> Option<u64> {
    current.map(|token| if state == 2 { tail } else { token })
}

/// Bounded array index: scans retain the extended bit, zero-scan keys retain their literal VK.
pub fn key_slot(scan: u32, extended: bool, vk: u32) -> Option<usize> {
    if scan > 255 || vk > 255 {
        return None;
    }
    Some(if scan == 0 {
        512 + vk as usize
    } else {
        scan as usize + usize::from(extended) * 256
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    KeyboardHook,
    MouseHook,
    MouseRaw,
    CursorWindow,
    PointerClip,
}

/// Shared production/fake acquisition order. Failure and cancellation unwind in reverse order;
/// the caller checks its generation and gate before AND after every native call.
pub fn acquire<E>(mut step: impl FnMut(Resource, bool) -> Result<(), E>) -> Result<(), E> {
    let order = [
        Resource::KeyboardHook,
        Resource::MouseHook,
        Resource::MouseRaw,
        Resource::CursorWindow,
        Resource::PointerClip,
    ];
    for (index, resource) in order.iter().copied().enumerate() {
        if let Err(error) = step(resource, true) {
            for resource in order[..=index].iter().rev().copied() {
                let _ = step(resource, false);
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Mouse evidence is compared in windows, never one-for-one packet counts.
#[derive(Debug)]
pub struct Watchdog {
    epoch: u64,
    hooks: u64,
    raw: u64,
    mismatch: Option<u64>,
}

impl Watchdog {
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            hooks: 0,
            raw: 0,
            mismatch: None,
        }
    }

    pub fn lost(
        &mut self,
        now: u64,
        gate: bool,
        epoch: u64,
        pump: u64,
        hooks: u64,
        raw: u64,
    ) -> bool {
        if !gate || epoch != self.epoch || now.saturating_sub(pump) >= PUMP_TIMEOUT_MS {
            return true;
        }
        if hooks != self.hooks {
            self.mismatch = None;
        } else if raw != self.raw {
            self.mismatch.get_or_insert(now);
        }
        self.hooks = hooks;
        self.raw = raw;
        self.mismatch
            .is_some_and(|at| now.saturating_sub(at) >= MOUSE_MISMATCH_MS)
    }
}
