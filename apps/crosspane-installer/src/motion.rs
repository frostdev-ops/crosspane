//! Finite presentation motion; animation never advances a workflow.

use crate::view::{MotionPreference, RowState, ScreenId, SummaryView, WizardView};

/// A page change in full motion: the body slides and fades.
pub const TRANSITION_MS: u64 = 320;
pub const HOVER_SECONDS: f32 = 0.14;

/// How much the shell moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionLevel {
    /// Slides, eased fades, turning marks and a slow ambient drift.
    Full,
    /// Short fades only: nothing moves, pulses or turns.
    Reduced,
    /// Every change is instant.
    Off,
}

impl MotionLevel {
    /// The duration of a change that takes `full_ms` in full motion: a short fade when reduced,
    /// nothing when off.
    pub fn duration(self, full_ms: u64) -> u64 {
        match self {
            Self::Full => full_ms,
            Self::Reduced => full_ms.min(160),
            Self::Off => 0,
        }
    }

    /// Whether things may move, pulse or turn.
    pub fn moves(self) -> bool {
        self == Self::Full
    }

    /// The hover and press fade time, in seconds.
    pub fn hover_seconds(self) -> f32 {
        match self {
            Self::Full => HOVER_SECONDS,
            Self::Reduced => 0.08,
            Self::Off => 0.0,
        }
    }
}

/// The motion a preference resolves to. Auto follows the system's reduce-motion setting where
/// the platform reports one, and is full motion where none can be read (owner, 2026-10-05: the
/// installer is animated unless the person or the system asks otherwise).
pub fn motion_level(preference: MotionPreference, system: Option<bool>) -> MotionLevel {
    match preference {
        MotionPreference::Auto if system == Some(true) => MotionLevel::Reduced,
        MotionPreference::Auto | MotionPreference::Full => MotionLevel::Full,
        MotionPreference::Reduced => MotionLevel::Reduced,
        MotionPreference::Off => MotionLevel::Off,
    }
}

/// Whether movement is withheld (reduced or off).
pub fn reduced_motion(preference: MotionPreference, system: Option<bool>) -> bool {
    !motion_level(preference, system).moves()
}

/// Progress 0–1 of a change that started at `start_ms` and lasts `duration_ms`.
pub fn progress(now_ms: u64, start_ms: u64, duration_ms: u64) -> f32 {
    if duration_ms == 0 {
        1.0
    } else {
        (now_ms.saturating_sub(start_ms) as f32 / duration_ms as f32).clamp(0.0, 1.0)
    }
}

pub fn transition_fraction(elapsed_ms: u64, duration_ms: u64, reduced: bool) -> f32 {
    if reduced || duration_ms == 0 || elapsed_ms >= duration_ms {
        1.0
    } else {
        elapsed_ms as f32 / duration_ms as f32
    }
}

pub(crate) fn illustration_duration(view: &WizardView) -> u64 {
    match view.screen {
        ScreenId::Welcome | ScreenId::HidingChoice => 600,
        ScreenId::Permissions if view.illustration.permission_row.is_some() => 500,
        ScreenId::Network if view.illustration.traffic_observed => 200,
        ScreenId::Practice if view.illustration.practice.is_some() => 600,
        ScreenId::Summary if view.summary == SummaryView::WorkspaceReady => 200,
        _ => 0,
    }
}

pub(crate) fn illustration_allowed(view: &WizardView) -> bool {
    if view.screen == ScreenId::Permissions {
        return view
            .illustration
            .permission_row
            .and_then(|id| view.rows.iter().find(|row| row.id == id))
            .is_some_and(|row| {
                !matches!(
                    row.state,
                    RowState::Waiting | RowState::Failed | RowState::Unsupported
                )
            });
    }
    !view.rows.iter().any(|row| {
        matches!(
            row.state,
            RowState::Waiting | RowState::Failed | RowState::Unsupported
        )
    })
}

#[derive(Debug, Default)]
pub(crate) struct IllustrationMotion {
    elapsed_ms: u64,
    last_now_ms: u64,
    was_active: bool,
}

impl IllustrationMotion {
    pub(crate) fn reset(&mut self, now_ms: u64) {
        self.elapsed_ms = 0;
        self.last_now_ms = now_ms;
        self.was_active = false;
    }

    pub(crate) fn advance(
        &mut self,
        now_ms: u64,
        duration_ms: u64,
        reduced: bool,
        active: bool,
    ) -> (f32, bool) {
        let advancing = active && !reduced;
        if advancing && self.was_active {
            self.elapsed_ms = self
                .elapsed_ms
                .saturating_add(now_ms.saturating_sub(self.last_now_ms))
                .min(duration_ms);
        }
        self.last_now_ms = now_ms;
        self.was_active = advancing;
        let fraction = transition_fraction(self.elapsed_ms, duration_ms, reduced);
        (
            fraction,
            active && !reduced && self.elapsed_ms < duration_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn illustration_pause_preserves_position_and_resumes_without_wall_clock_jump() {
        let mut motion = IllustrationMotion::default();
        motion.reset(100);
        assert_eq!(motion.advance(100, 600, false, true), (0.0, true));
        assert_eq!(motion.advance(200, 600, false, true), (1.0 / 6.0, true));
        assert_eq!(motion.advance(5000, 600, false, false), (1.0 / 6.0, false));
        assert_eq!(motion.advance(5100, 600, false, true), (1.0 / 6.0, true));
        assert_eq!(motion.advance(6000, 600, false, true), (1.0, false));
        assert_eq!(motion.advance(7000, 600, false, true), (1.0, false));
        assert_eq!(motion.advance(8000, 600, true, true), (1.0, false));
    }

    #[test]
    fn illustration_resume_without_paused_frames_holds_its_position() {
        let mut motion = IllustrationMotion::default();
        motion.reset(0);
        assert_eq!(motion.advance(0, 600, false, true), (0.0, true));
        assert_eq!(motion.advance(100, 600, false, true), (1.0 / 6.0, true));
        assert_eq!(motion.advance(200, 600, false, false), (1.0 / 6.0, false));
        // The paused screen needs no repaint. Its next pass can be the resume itself.
        assert_eq!(motion.advance(5200, 600, false, true), (1.0 / 6.0, true));
        assert_eq!(motion.advance(5300, 600, false, true), (1.0 / 3.0, true));
    }
}
