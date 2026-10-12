//! Pure, bounded capture watchdog decisions; no native input or hook-presence claims.
//! The lead-approved positive probe permits one F24 pair to escape after keyboard-hook loss.
//! A failed matching-up retry remains unverified; this model never turns failure into cleanup.
//! A probe is only sent to a foreground that UIPI admits (`probe_decision`); an elevated or secure
//! foreground ends the capture as a loss (or refuses the activation) instead of faulting.

/// Cadence plus acknowledgment deadline is 500 ms, excluding native/scheduling delays.
pub const PROBE_INTERVAL_MS: u64 = 250;
pub const PROBE_TIMEOUT_MS: u64 = 250;
/// Silent-hook evidence: the hook count stays unchanged for this long after the mismatch opens.
/// The mismatch opens at the first raw advance observed while the hook count is unchanged.
pub const MOUSE_MISMATCH_MS: u64 = 500;
/// Silent-hook evidence: raw mouse reports seen after the mismatch opened, with no hook callback.
pub const MOUSE_MISMATCH_REPORTS: u64 = 16;
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
    /// Opened by the first raw advance while `hooks` is unchanged: (since ms, raw count then).
    mismatch: Option<(u64, u64)>,
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

    /// The gate epoch this watchdog was armed for.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A closed gate, stale epoch or stale pump is lost at once. Otherwise a silent hook is lost
    /// only when its mismatch has lasted `MOUSE_MISMATCH_MS` and seen `MOUSE_MISMATCH_REPORTS` raw
    /// reports since it opened; any hook advance clears the mismatch.
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
            self.mismatch.get_or_insert((now, raw));
        }
        self.hooks = hooks;
        self.raw = raw;
        self.mismatch.is_some_and(|(since, raw_at_open)| {
            now.saturating_sub(since) >= MOUSE_MISMATCH_MS
                && raw.saturating_sub(raw_at_open) >= MOUSE_MISMATCH_REPORTS
        })
    }
}

/// How the foreground window relates to this process, for deciding whether a probe can be sent.
///
/// UIPI silently drops injected input aimed at a higher-integrity window while `SendInput` still
/// reports success, so a probe sent there is never acknowledged and says nothing about our hooks.
/// Mirrors the injector's foreground admission (`model::inject`): a missing foreground (the secure
/// desktop), an unreadable token and a higher integrity are all refused; nothing here bypasses
/// UIPI or reads window content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundClass {
    /// No foreground window: the secure desktop (UAC prompt, lock screen), another input desktop,
    /// or a window in transition. Input cannot be verified.
    Absent,
    /// A foreground window whose process or token integrity could not be read, or ours could not.
    /// Treated exactly like `Higher` (fail closed), as the injector does.
    Unknown,
    /// The foreground process runs at a strictly higher integrity than this process (elevated).
    Higher,
    /// The foreground process runs at this process's integrity or below: UIPI admits our input.
    LowerOrEqual,
}

/// Classifies the foreground from the integrity RIDs the OS reported. `present` is whether a
/// foreground window exists; `foreground` and `own` are token integrity RIDs, `None` when unreadable.
pub fn classify_foreground(
    present: bool,
    foreground: Option<u32>,
    own: Option<u32>,
) -> ForegroundClass {
    if !present {
        return ForegroundClass::Absent;
    }
    match (foreground, own) {
        (Some(foreground), Some(own)) if foreground > own => ForegroundClass::Higher,
        (Some(_), Some(_)) => ForegroundClass::LowerOrEqual,
        _ => ForegroundClass::Unknown,
    }
}

/// Where the capture is when a probe is considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbePhase {
    /// Activation warm-up: the capture was requested but is not yet active.
    Warming,
    /// The capture is active.
    Active,
}

/// Which side of the probe the decision is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeStage {
    /// A probe is due and nothing has been submitted yet.
    BeforeSubmit,
    /// A submitted probe was not acknowledged in time.
    Unacknowledged,
}

/// What to do about a probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeDecision {
    /// Submit the probe.
    Probe,
    /// End the capture as `Lost` because the foreground is elevated or secure and input cannot be
    /// verified. A loss, never a fault: the backend stays usable.
    EndLostElevated,
    /// Refuse the activation before it happens (`PlatformError::SecureInput`). Never a fault.
    Refuse,
    /// The probe was submitted to a foreground that admits it and still vanished: the keyboard hook
    /// is gone. Terminal (the pre-existing behaviour).
    Fault,
}

/// | Foreground | Stage | Phase | Decision |
/// |---|---|---|---|
/// | lower or equal | before submit | any | `Probe` |
/// | lower or equal | unacknowledged | any | `Fault` |
/// | absent, unknown or higher | before submit | warming | `Refuse` |
/// | absent, unknown or higher | before submit | active | `EndLostElevated` |
/// | absent, unknown or higher | unacknowledged | any | `EndLostElevated` |
///
/// An unacknowledged probe is re-classified when it times out, so a foreground that turned
/// elevated between the check and `SendInput` is a loss, not a fault. There it ends the capture
/// even while warming: an unverified keyboard hook must not activate, and cancelling the pending
/// activation is the one race-free way to stop it.
pub fn probe_decision(
    class: ForegroundClass,
    phase: ProbePhase,
    stage: ProbeStage,
) -> ProbeDecision {
    match (class, stage, phase) {
        (ForegroundClass::LowerOrEqual, ProbeStage::BeforeSubmit, _) => ProbeDecision::Probe,
        (ForegroundClass::LowerOrEqual, ProbeStage::Unacknowledged, _) => ProbeDecision::Fault,
        (_, ProbeStage::BeforeSubmit, ProbePhase::Warming) => ProbeDecision::Refuse,
        _ => ProbeDecision::EndLostElevated,
    }
}

#[cfg(test)]
mod foreground_tests {
    use super::*;

    const LOW: u32 = 0x1000;
    const MEDIUM: u32 = 0x2000;
    const HIGH: u32 = 0x3000;
    const SYSTEM: u32 = 0x4000;
    const CLASSES: [ForegroundClass; 4] = [
        ForegroundClass::Absent,
        ForegroundClass::Unknown,
        ForegroundClass::Higher,
        ForegroundClass::LowerOrEqual,
    ];
    const PHASES: [ProbePhase; 2] = [ProbePhase::Warming, ProbePhase::Active];
    const STAGES: [ProbeStage; 2] = [ProbeStage::BeforeSubmit, ProbeStage::Unacknowledged];

    #[test]
    fn a_missing_foreground_is_absent_whatever_the_integrity() {
        for own in [None, Some(MEDIUM)] {
            for foreground in [None, Some(LOW), Some(MEDIUM), Some(SYSTEM)] {
                assert_eq!(
                    classify_foreground(false, foreground, own),
                    ForegroundClass::Absent
                );
            }
        }
    }

    #[test]
    fn an_unreadable_integrity_on_either_side_is_unknown() {
        assert_eq!(
            classify_foreground(true, None, Some(MEDIUM)),
            ForegroundClass::Unknown
        );
        assert_eq!(
            classify_foreground(true, Some(MEDIUM), None),
            ForegroundClass::Unknown
        );
        assert_eq!(
            classify_foreground(true, None, None),
            ForegroundClass::Unknown
        );
    }

    #[test]
    fn only_a_strictly_higher_integrity_is_higher() {
        for (foreground, own, expected) in [
            (HIGH, MEDIUM, ForegroundClass::Higher),
            (SYSTEM, MEDIUM, ForegroundClass::Higher),
            (SYSTEM, HIGH, ForegroundClass::Higher),
            (MEDIUM + 1, MEDIUM, ForegroundClass::Higher),
            (MEDIUM, MEDIUM, ForegroundClass::LowerOrEqual),
            (HIGH, HIGH, ForegroundClass::LowerOrEqual),
            (LOW, MEDIUM, ForegroundClass::LowerOrEqual),
            (0, MEDIUM, ForegroundClass::LowerOrEqual),
            (MEDIUM, HIGH, ForegroundClass::LowerOrEqual),
        ] {
            assert_eq!(
                classify_foreground(true, Some(foreground), Some(own)),
                expected,
                "foreground {foreground:#x} against own {own:#x}"
            );
        }
    }

    #[test]
    fn the_decision_table_is_exactly_as_documented() {
        use ProbeDecision::*;
        use ProbePhase::*;
        use ProbeStage::*;
        for class in [
            ForegroundClass::Absent,
            ForegroundClass::Unknown,
            ForegroundClass::Higher,
        ] {
            assert_eq!(probe_decision(class, Warming, BeforeSubmit), Refuse);
            assert_eq!(probe_decision(class, Active, BeforeSubmit), EndLostElevated);
            assert_eq!(
                probe_decision(class, Warming, Unacknowledged),
                EndLostElevated
            );
            assert_eq!(
                probe_decision(class, Active, Unacknowledged),
                EndLostElevated
            );
        }
        let ok = ForegroundClass::LowerOrEqual;
        assert_eq!(probe_decision(ok, Warming, BeforeSubmit), Probe);
        assert_eq!(probe_decision(ok, Active, BeforeSubmit), Probe);
        assert_eq!(probe_decision(ok, Warming, Unacknowledged), Fault);
        assert_eq!(probe_decision(ok, Active, Unacknowledged), Fault);
    }

    #[test]
    fn absent_unknown_and_higher_are_never_told_apart() {
        for phase in PHASES {
            for stage in STAGES {
                let higher = probe_decision(ForegroundClass::Higher, phase, stage);
                assert_eq!(
                    probe_decision(ForegroundClass::Absent, phase, stage),
                    higher
                );
                assert_eq!(
                    probe_decision(ForegroundClass::Unknown, phase, stage),
                    higher
                );
            }
        }
    }

    #[test]
    fn only_an_admitting_foreground_is_ever_probed_or_faulted() {
        for class in CLASSES {
            for phase in PHASES {
                for stage in STAGES {
                    let decision = probe_decision(class, phase, stage);
                    let admitting = class == ForegroundClass::LowerOrEqual;
                    assert_eq!(
                        matches!(decision, ProbeDecision::Probe | ProbeDecision::Fault),
                        admitting,
                        "{class:?} {phase:?} {stage:?} gave {decision:?}"
                    );
                    // A fault needs a probe that was really submitted; a refusal needs an
                    // activation that has not happened.
                    assert_eq!(
                        decision == ProbeDecision::Fault,
                        admitting && stage == ProbeStage::Unacknowledged
                    );
                    assert_eq!(
                        decision == ProbeDecision::Refuse,
                        !admitting
                            && stage == ProbeStage::BeforeSubmit
                            && phase == ProbePhase::Warming
                    );
                }
            }
        }
    }

    #[test]
    fn an_elevated_foreground_never_faults_and_an_admitted_probe_always_does_when_lost() {
        for phase in PHASES {
            for class in [
                ForegroundClass::Absent,
                ForegroundClass::Unknown,
                ForegroundClass::Higher,
            ] {
                assert_ne!(
                    probe_decision(class, phase, ProbeStage::Unacknowledged),
                    ProbeDecision::Fault
                );
            }
            assert_eq!(
                probe_decision(
                    ForegroundClass::LowerOrEqual,
                    phase,
                    ProbeStage::Unacknowledged
                ),
                ProbeDecision::Fault
            );
        }
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    /// One observation at a fresh pump, gate open and epoch 7.
    fn observe(w: &mut Watchdog, now: u64, hooks: u64, raw: u64) -> bool {
        w.lost(now, true, 7, now, hooks, raw)
    }

    #[test]
    fn one_raw_report_without_hook_is_not_lost() {
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        assert!(!observe(&mut w, 10, 0, 1));
        for now in (20..=610).step_by(10) {
            assert!(!observe(&mut w, now, 0, 1), "lost at {now} ms");
        }
    }

    #[test]
    fn sixteen_reports_over_500ms_without_hook_is_lost() {
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        // One report every 10 ms. The mismatch opens at t=10 with raw 1, so the first tick with
        // 500 ms elapsed and 16 reports after it is t=510 (raw 51).
        let lost_at = (10..=1000)
            .step_by(10)
            .find(|&now| observe(&mut w, now, 0, 1 + (now - 10) / 10));
        assert_eq!(lost_at, Some(510));
    }

    #[test]
    fn sixteen_reports_in_a_burst_still_wait_for_the_full_window() {
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        // Twenty reports in the first 100 ms meet the report count, not the time.
        for (now, raw) in [(10, 1), (20, 5), (50, 10), (100, 21)] {
            assert!(!observe(&mut w, now, 0, raw), "lost at {now} ms");
        }
        for now in (110..=500).step_by(10) {
            assert!(!observe(&mut w, now, 0, 21), "lost at {now} ms");
        }
        assert!(observe(&mut w, 510, 0, 21));
    }

    #[test]
    fn fifteen_reports_are_never_enough_however_long_the_silence() {
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        // The mismatch opens at t=10 with raw 1. Raw then reaches 16: 15 reports after it.
        let mut raw = 0;
        for now in (10..=3000).step_by(10) {
            if (now - 10) % 100 == 0 && raw < 16 {
                raw += 1;
            }
            assert!(!observe(&mut w, now, 0, raw), "lost at {now} ms");
        }
    }

    #[test]
    fn closed_gate_is_lost_immediately_even_mid_mismatch() {
        assert!(Watchdog::new(7).lost(0, false, 7, 0, 0, 0));
        assert!(Watchdog::new(7).lost(1, true, 8, 1, 0, 0));
        assert!(Watchdog::new(7).lost(100, true, 7, 0, 0, 0));
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        assert!(!observe(&mut w, 10, 0, 1));
        assert!(w.lost(20, false, 7, 20, 0, 1));
    }

    #[test]
    fn advancing_hooks_never_lose_even_with_fast_raw() {
        let mut w = Watchdog::new(7);
        for step in 0..=300u64 {
            let now = step * 10;
            assert!(!observe(&mut w, now, step, step * 5), "lost at {now} ms");
        }
    }

    #[test]
    fn hook_advance_clears_an_open_mismatch_and_restarts_the_window() {
        let mut w = Watchdog::new(7);
        assert!(!observe(&mut w, 0, 0, 0));
        // Raw runs with the hook silent until t=400, then the hook advances at t=410.
        for now in (10..=400).step_by(10) {
            let raw = 1 + (now - 10) / 10;
            assert!(!observe(&mut w, now, 0, raw), "lost at {now} ms");
        }
        // The mismatch reopens at t=420 with raw 42. The next loss is 500 ms later: t=920.
        let lost_at = (410..=1000)
            .step_by(10)
            .find(|&now| observe(&mut w, now, 1, 1 + (now - 10) / 10));
        assert_eq!(lost_at, Some(920));
    }
}
