//! Translating Crosspane input into EIS requests (pure).

use crosspane_platform::PlatformError;
use crosspane_types::hid::{HidUsage, MouseButton, hid_to_evdev};
use crosspane_types::input::{ScrollDelta, ScrollPhase};

use super::ledger::Axes;

/// `BTN_LEFT` .. `BTN_EXTRA` of `linux/input-event-codes.h`.
pub(super) const BTN_LEFT: u32 = 0x110;
pub(super) const BTN_RIGHT: u32 = 0x111;
pub(super) const BTN_MIDDLE: u32 = 0x112;
pub(super) const BTN_SIDE: u32 = 0x113;
pub(super) const BTN_EXTRA: u32 = 0x114;

/// `KEY_CAPSLOCK` and `KEY_NUMLOCK`.
pub(super) const KEY_CAPSLOCK: u16 = 58;
pub(super) const KEY_NUMLOCK: u16 = 69;

/// The evdev code of a HID usage. Unmapped usages are reported without naming the usage.
pub(super) fn key_code(usage: HidUsage) -> Result<u16, PlatformError> {
    hid_to_evdev(usage).ok_or(PlatformError::Unsupported("HID usage has no evdev code"))
}

/// The evdev button code of a HID button (numbered as on the HID Button page).
pub(super) fn button_code(button: MouseButton) -> Result<u32, PlatformError> {
    match button.0 {
        1 => Ok(BTN_LEFT),
        2 => Ok(BTN_RIGHT),
        3 => Ok(BTN_MIDDLE),
        4 => Ok(BTN_SIDE),
        5 => Ok(BTN_EXTRA),
        _ => Err(PlatformError::Unsupported("unmapped HID pointer button")),
    }
}

/// One scroll displacement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    /// `ei_scroll.scroll`: logical pixels.
    Smooth { x: f32, y: f32 },
    /// `ei_scroll.scroll_discrete`: 120ths of a wheel detent.
    Discrete { x: i32, y: i32 },
}

/// The end of a gesture on some axes (`ei_scroll.scroll_stop`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stop {
    pub axes: Axes,
    pub cancel: bool,
}

/// What one `ScrollDelta` becomes: a displacement, then (in a frame of its own, because EIS forbids
/// a stop and a displacement on one axis in the same frame) a stop.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ScrollPlan {
    pub motion: Option<Motion>,
    pub stop: Option<Stop>,
}

/// Plan a scroll. `ScrollDelta` follows the HID convention (positive y scrolls up, positive x
/// right); EIS, like libinput and Wayland, counts positive as down and right-to-left content
/// movement, so both axes flip, exactly as the Hyprland backend does for the same input.
///
/// `pixels` is used for smooth gestures and `v120` for wheel detents (and when no pixels came);
/// the two are never both sent.
pub(super) fn plan_scroll(delta: &ScrollDelta) -> Result<ScrollPlan, PlatformError> {
    let ending = matches!(
        delta.phase,
        ScrollPhase::Ended | ScrollPhase::Cancelled | ScrollPhase::MomentumEnded
    );
    let wheel = delta.phase == ScrollPhase::Discrete && (delta.v120_x != 0 || delta.v120_y != 0);
    let has_v120 = delta.v120_x != 0 || delta.v120_y != 0;

    let mut motion = None;
    match delta.pixels.filter(|_| !wheel) {
        Some(pixels) => {
            let (x, y) = (-pixels.x, -pixels.y);
            if !fits_f32(x) || !fits_f32(y) {
                return Err(PlatformError::Unsupported("scroll delta out of range"));
            }
            if x != 0.0 || y != 0.0 {
                motion = Some(Motion::Smooth {
                    x: x as f32,
                    y: y as f32,
                });
            } else if has_v120 {
                motion = Some(discrete(delta));
            }
        }
        None if has_v120 => motion = Some(discrete(delta)),
        None => {}
    }

    let axes = Axes {
        x: delta.stop_x || ending,
        y: delta.stop_y || ending,
    };
    let stop = axes.any().then_some(Stop {
        axes,
        cancel: delta.phase == ScrollPhase::Cancelled,
    });
    Ok(ScrollPlan { motion, stop })
}

fn discrete(delta: &ScrollDelta) -> Motion {
    Motion::Discrete {
        x: delta.v120_x.saturating_neg(),
        y: delta.v120_y.saturating_neg(),
    }
}

fn fits_f32(value: f64) -> bool {
    value.is_finite() && value.abs() <= f64::from(f32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::geom::VectorLogical;

    fn delta() -> ScrollDelta {
        ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        }
    }

    #[test]
    fn hid_buttons_map_to_the_btn_codes() {
        for (button, code) in [
            (MouseButton::PRIMARY, 0x110),
            (MouseButton::SECONDARY, 0x111),
            (MouseButton::TERTIARY, 0x112),
            (MouseButton::BACK, 0x113),
            (MouseButton::FORWARD, 0x114),
        ] {
            assert_eq!(button_code(button).unwrap(), code);
        }
        assert!(matches!(
            button_code(MouseButton(0)),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(button_code(MouseButton(6)).is_err());
    }

    #[test]
    fn keys_map_through_the_shared_table() {
        // KeyA is KEY_A (30); the lock keys match the constants used for lock taps.
        assert_eq!(key_code(HidUsage::keyboard(0x04)).unwrap(), 30);
        assert_eq!(key_code(HidUsage::keyboard(0x39)).unwrap(), KEY_CAPSLOCK);
        assert_eq!(key_code(HidUsage::keyboard(0x53)).unwrap(), KEY_NUMLOCK);
    }

    #[test]
    fn an_unmapped_usage_names_no_usage() {
        let error = key_code(HidUsage {
            page: 0x0c,
            id: 0xfff0,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            PlatformError::Unsupported("HID usage has no evdev code")
        ));
        let text = error.to_string();
        assert!(!text.contains("fff0") && !text.contains("65520"), "{text}");
    }

    #[test]
    fn wheel_detents_flip_both_axes_and_stay_in_120ths() {
        let plan = plan_scroll(&ScrollDelta {
            v120_x: 120,
            v120_y: -240,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Discrete { x: -120, y: 240 }));
        assert_eq!(plan.stop, None);
        // Fractions of a detent pass through for the compositor to accumulate.
        let plan = plan_scroll(&ScrollDelta {
            v120_y: 30,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Discrete { x: 0, y: -30 }));
    }

    #[test]
    fn a_wheel_detent_ignores_pixels() {
        let plan = plan_scroll(&ScrollDelta {
            v120_y: 120,
            pixels: Some(VectorLogical::new(0.0, 15.0)),
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Discrete { x: 0, y: -120 }));
    }

    #[test]
    fn smooth_gestures_use_pixels_and_flip_the_sign() {
        let plan = plan_scroll(&ScrollDelta {
            v120_y: 36,
            pixels: Some(VectorLogical::new(2.5, -12.0)),
            phase: ScrollPhase::Changed,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Smooth { x: -2.5, y: 12.0 }));
        assert_eq!(plan.stop, None);
        // Momentum is the same path.
        let plan = plan_scroll(&ScrollDelta {
            pixels: Some(VectorLogical::new(0.0, 4.0)),
            phase: ScrollPhase::MomentumChanged,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Smooth { x: 0.0, y: -4.0 }));
    }

    #[test]
    fn zero_pixels_fall_back_to_v120_and_never_send_both() {
        let plan = plan_scroll(&ScrollDelta {
            v120_y: 60,
            pixels: Some(VectorLogical::new(0.0, 0.0)),
            phase: ScrollPhase::Changed,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Discrete { x: 0, y: -60 }));
        let nothing = plan_scroll(&ScrollDelta {
            phase: ScrollPhase::MayBegin,
            pixels: Some(VectorLogical::new(0.0, 0.0)),
            ..delta()
        })
        .unwrap();
        assert_eq!(nothing, ScrollPlan::default());
    }

    #[test]
    fn ending_phases_stop_both_axes_and_cancel_only_cancels() {
        for phase in [ScrollPhase::Ended, ScrollPhase::MomentumEnded] {
            let plan = plan_scroll(&ScrollDelta { phase, ..delta() }).unwrap();
            assert_eq!(plan.motion, None);
            assert_eq!(
                plan.stop,
                Some(Stop {
                    axes: Axes { x: true, y: true },
                    cancel: false
                })
            );
        }
        let plan = plan_scroll(&ScrollDelta {
            phase: ScrollPhase::Cancelled,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.stop.map(|s| s.cancel), Some(true));
    }

    #[test]
    fn explicit_stop_flags_stop_only_their_axis_after_the_displacement() {
        let plan = plan_scroll(&ScrollDelta {
            pixels: Some(VectorLogical::new(0.0, 3.0)),
            phase: ScrollPhase::Changed,
            stop_y: true,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Smooth { x: 0.0, y: -3.0 }));
        assert_eq!(
            plan.stop,
            Some(Stop {
                axes: Axes { x: false, y: true },
                cancel: false
            })
        );
    }

    #[test]
    fn out_of_range_and_non_finite_pixels_are_refused() {
        for bad in [f64::NAN, f64::INFINITY, 1e300] {
            let result = plan_scroll(&ScrollDelta {
                pixels: Some(VectorLogical::new(bad, 0.0)),
                phase: ScrollPhase::Changed,
                ..delta()
            });
            assert!(matches!(result, Err(PlatformError::Unsupported(_))));
        }
    }

    #[test]
    fn the_most_negative_v120_does_not_overflow() {
        let plan = plan_scroll(&ScrollDelta {
            v120_x: i32::MIN,
            ..delta()
        })
        .unwrap();
        assert_eq!(plan.motion, Some(Motion::Discrete { x: i32::MAX, y: 0 }));
    }
}
