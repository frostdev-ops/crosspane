//! The pure half of [`super`] (WP-G2.4 B1): which logical monitors are the user's, and where the
//! twin goes. No D-Bus, no I/O.
//!
//! # Logical sizes
//!
//! `ApplyMonitorsConfig` carries no sizes: Mutter derives each logical monitor's rectangle from the
//! mode, the transform and the scale, and then checks that the monitors touch and do not overlap.
//! This module has to predict the same rectangles, so it copies Mutter's arithmetic (mutter 50.4):
//!
//! - `derive_logical_monitor_size` in `src/backends/meta-monitor-manager.c` (the D-Bus apply path)
//!   first swaps the mode's width and height for an odd transform (`mtk_monitor_transform_is_rotated`
//!   is `transform % 2`, so 90, 270 and the flipped variants of both), then, in *logical* layout
//!   mode, divides by the scale and rounds with `roundf` on a `float`. In *physical* layout mode
//!   the mode size is used unchanged. `scale_logical_monitor_width` in
//!   `src/backends/meta-monitor-config-manager.c` does the same for stored configs.
//! - `meta_logical_monitor_new` (`src/backends/meta-logical-monitor.c`) takes that rectangle as the
//!   logical monitor's rect, which `GetCurrentState` reports back.
//! - `mtk_rectangle_is_adjacent_to` (`mtk/mtk/mtk-rectangle.c`) and `is_connected_to_all`
//!   (`src/backends/meta-monitor-config-utils.c`) require every logical monitor to share an edge
//!   segment (not merely a corner) with another, transitively. A twin whose left edge is the right
//!   edge of the rightmost physical monitor and whose top is that monitor's top shares at least
//!   one row with it, and a rectangle starting at or beyond every other monitor's right edge cannot
//!   overlap any of them.

use super::{DisplayState, LogicalConfig, LogicalState, ModeState, TWIN_PREFIX};

/// `layout-mode` value of the physical layout mode (`1` is logical, the default).
const LAYOUT_MODE_PHYSICAL: u32 = 2;

/// `state` minus every logical monitor that holds a `Meta-*` connector. Every other field of each
/// kept monitor (position, scale, transform, primary) is as `state` reports it.
pub fn physical(state: &DisplayState) -> Vec<LogicalState> {
    state
        .logical
        .iter()
        .filter(|logical| {
            !logical
                .connectors
                .iter()
                .any(|connector| connector.starts_with(TWIN_PREFIX))
        })
        .cloned()
        .collect()
}

/// The config to apply: `physical` plus `twin`, placed at the right edge of the rightmost physical
/// logical monitor (largest `x + logical width`; ties go to the smallest `y`) and top-aligned with
/// it, at `twin_scale`. Each connector uses its *current* mode in `state`.
///
/// `None` when `physical` is empty or holds a logical monitor without connectors, when a physical
/// connector or the twin has no current mode in `state` (or is unknown), when the twin is also in
/// `physical`, or when a size cannot be derived (a non-positive or non-finite scale, a size or an
/// edge that does not fit in `i32`).
pub fn plan(
    state: &DisplayState,
    physical: &[LogicalState],
    twin: &str,
    twin_scale: f64,
) -> Option<Vec<LogicalConfig>> {
    if physical.is_empty() {
        return None;
    }
    let mut configs = Vec::with_capacity(physical.len() + 1);
    // (right edge, top) of the rightmost physical logical monitor so far.
    let mut anchor: Option<(i32, i32)> = None;
    for logical in physical {
        let first = logical.connectors.first()?;
        if logical.connectors.iter().any(|connector| connector == twin) {
            return None;
        }
        let monitors = logical
            .connectors
            .iter()
            .map(|connector| {
                Some((
                    connector.clone(),
                    current_mode(state, connector)?.id.clone(),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        // Mutter sizes a logical monitor from its first monitor's mode.
        let (width, _) = logical_size(
            state.layout_mode,
            current_mode(state, first)?,
            logical.scale,
            logical.transform,
        )?;
        let right = logical.x.checked_add(width)?;
        anchor = match anchor {
            Some((best_right, best_y))
                if best_right > right || (best_right == right && best_y <= logical.y) =>
            {
                Some((best_right, best_y))
            }
            _ => Some((right, logical.y)),
        };
        configs.push(LogicalConfig {
            x: logical.x,
            y: logical.y,
            scale: logical.scale,
            transform: logical.transform,
            primary: logical.primary,
            monitors,
        });
    }
    let (x, y) = anchor?;
    let twin_mode = current_mode(state, twin)?;
    // The size must be derivable and its far edges representable (`twin_rect` relies on it).
    let (width, height) = logical_size(state.layout_mode, twin_mode, twin_scale, 0)?;
    x.checked_add(width)?;
    y.checked_add(height)?;
    configs.push(LogicalConfig {
        x,
        y,
        scale: twin_scale,
        transform: 0,
        primary: false,
        monitors: vec![(twin.to_owned(), twin_mode.id.clone())],
    });
    Some(configs)
}

/// The largest of `supported` that is at most `wanted` (with 1e-6 of slack for the float
/// round trip), else `1.0`. Non-finite and non-positive entries are ignored.
pub fn pick_scale(supported: &[f64], wanted: f64) -> f64 {
    supported
        .iter()
        .copied()
        .filter(|scale| scale.is_finite() && *scale > 0.0 && *scale <= wanted + 1e-6)
        .reduce(f64::max)
        .unwrap_or(1.0)
}

/// The twin's logical rectangle `(x, y, width, height)` in `plan`: where the logical monitor that
/// holds `twin` sits, with the size Mutter derives from its first monitor's mode in `state`.
/// `None` when `plan` has no such logical monitor, or `state` lacks its connector or mode id.
pub fn twin_rect(
    plan: &[LogicalConfig],
    state: &DisplayState,
    twin: &str,
) -> Option<(i32, i32, i32, i32)> {
    let config = plan.iter().find(|config| {
        config
            .monitors
            .iter()
            .any(|(connector, _)| connector == twin)
    })?;
    let (first_connector, first_mode) = config.monitors.first()?;
    let mode = state
        .monitors
        .iter()
        .find(|monitor| &monitor.connector == first_connector)?
        .modes
        .iter()
        .find(|mode| &mode.id == first_mode)?;
    let (width, height) = logical_size(state.layout_mode, mode, config.scale, config.transform)?;
    Some((config.x, config.y, width, height))
}

/// The monitor's current mode in `state`.
fn current_mode<'a>(state: &'a DisplayState, connector: &str) -> Option<&'a ModeState> {
    state
        .monitors
        .iter()
        .find(|monitor| monitor.connector == connector)?
        .modes
        .iter()
        .find(|mode| mode.current)
}

/// The logical size Mutter derives for `mode` under `transform` and `scale` (see the module docs).
/// `layout_mode` is `GetCurrentState`'s `layout-mode`: absent means logical.
pub(super) fn logical_size(
    layout_mode: Option<u32>,
    mode: &ModeState,
    scale: f64,
    transform: u32,
) -> Option<(i32, i32)> {
    let (width, height) = if transform % 2 == 1 {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    if layout_mode == Some(LAYOUT_MODE_PHYSICAL) {
        return Some((width, height));
    }
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    // `(int) roundf (mode_width / scale)` with `float` arithmetic; `f32::round` also rounds half
    // away from zero.
    let scale = scale as f32;
    Some((
        divide_and_round(width, scale)?,
        divide_and_round(height, scale)?,
    ))
}

fn divide_and_round(length: i32, scale: f32) -> Option<i32> {
    let value = (length as f32 / scale).round();
    // 2^31 is not representable as `i32`; anything from there up (and NaN) is out of range.
    (1.0..2_147_483_648.0)
        .contains(&value)
        .then_some(value as i32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::MonitorState;
    use super::*;

    fn mode(id: &str, width: i32, height: i32, current: bool) -> ModeState {
        ModeState {
            id: id.to_owned(),
            width,
            height,
            refresh: 60.0,
            preferred_scale: 1.0,
            supported_scales: vec![1.0],
            current,
            preferred: false,
        }
    }

    fn monitor(connector: &str, modes: Vec<ModeState>) -> MonitorState {
        MonitorState {
            connector: connector.to_owned(),
            vendor: "V".to_owned(),
            product: "P".to_owned(),
            serial: "S".to_owned(),
            modes,
            is_builtin: false,
        }
    }

    fn logical(
        x: i32,
        y: i32,
        scale: f64,
        transform: u32,
        primary: bool,
        connectors: &[&str],
    ) -> LogicalState {
        LogicalState {
            x,
            y,
            scale,
            transform,
            primary,
            connectors: connectors.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn config(
        x: i32,
        y: i32,
        scale: f64,
        transform: u32,
        primary: bool,
        monitors: &[(&str, &str)],
    ) -> LogicalConfig {
        LogicalConfig {
            x,
            y,
            scale,
            transform,
            primary,
            monitors: monitors
                .iter()
                .map(|(c, m)| ((*c).to_owned(), (*m).to_owned()))
                .collect(),
        }
    }

    fn physical_monitors() -> Vec<MonitorState> {
        vec![
            monitor(
                "DP-3",
                vec![
                    mode("3440x1440@59.973", 3440, 1440, false),
                    mode("3440x1440@164.999", 3440, 1440, true),
                ],
            ),
            monitor("DP-2", vec![mode("1920x1080@74.973", 1920, 1080, true)]),
            monitor("HDMI-1", vec![mode("1920x1080@60.000", 1920, 1080, true)]),
        ]
    }

    /// The user's layout from the live probe (2026-10-10): DP-3 primary, DP-2 above-left of it,
    /// HDMI-1 rotated by 270 degrees on the left.
    fn user_layout() -> Vec<LogicalState> {
        vec![
            logical(1080, 1080, 1.0, 0, true, &["DP-3"]),
            logical(1817, 0, 1.0, 0, false, &["DP-2"]),
            logical(0, 600, 1.0, 3, false, &["HDMI-1"]),
        ]
    }

    /// What Mutter reports right after the twin appears: its linear layout (rotation dropped, the
    /// twin at the far right), with the twin's mode as negotiated.
    fn linear_state_with_twin() -> DisplayState {
        let mut monitors = physical_monitors();
        monitors.push(monitor(
            "Meta-0",
            vec![mode("1800x1169@60.000", 1800, 1169, true)],
        ));
        DisplayState {
            serial: 42,
            monitors,
            logical: vec![
                logical(0, 0, 1.0, 0, true, &["DP-3"]),
                logical(3440, 0, 1.0, 0, false, &["DP-2"]),
                logical(5360, 0, 1.0, 0, false, &["HDMI-1"]),
                logical(7280, 0, 1.0, 0, false, &["Meta-0"]),
            ],
            layout_mode: Some(1),
        }
    }

    fn state(monitors: Vec<MonitorState>, logical: Vec<LogicalState>) -> DisplayState {
        DisplayState {
            serial: 1,
            monitors,
            logical,
            layout_mode: Some(1),
        }
    }

    #[test]
    fn live_probe_puts_the_twin_right_of_dp3_top_aligned() {
        let after = linear_state_with_twin();
        let plan = plan(&after, &user_layout(), "Meta-0", 1.0).unwrap();
        assert_eq!(
            plan,
            vec![
                config(1080, 1080, 1.0, 0, true, &[("DP-3", "3440x1440@164.999")]),
                config(1817, 0, 1.0, 0, false, &[("DP-2", "1920x1080@74.973")]),
                config(0, 600, 1.0, 3, false, &[("HDMI-1", "1920x1080@60.000")]),
                config(4520, 1080, 1.0, 0, false, &[("Meta-0", "1800x1169@60.000")]),
            ]
        );
        assert_eq!(
            twin_rect(&plan, &after, "Meta-0"),
            Some((4520, 1080, 1800, 1169))
        );
    }

    #[test]
    fn physical_drops_every_logical_monitor_with_a_meta_connector() {
        let mut after = linear_state_with_twin();
        // A logical monitor that mirrors a physical monitor and a twin is dropped as a whole.
        after
            .logical
            .push(logical(9000, 0, 1.0, 0, false, &["DP-9", "Meta-3"]));
        // A name that merely contains the prefix is a physical monitor.
        after
            .logical
            .push(logical(9100, 0, 1.0, 0, false, &["eDP-Meta-1"]));
        let kept = physical(&after);
        let connectors: Vec<_> = kept
            .iter()
            .map(|l| l.connectors.iter().map(String::as_str).collect::<Vec<_>>())
            .collect();
        assert_eq!(
            connectors,
            vec![
                vec!["DP-3"],
                vec!["DP-2"],
                vec!["HDMI-1"],
                vec!["eDP-Meta-1"]
            ]
        );
        // Everything else about the kept monitors is untouched.
        assert_eq!(kept[0], after.logical[0]);
        assert_eq!(kept[3], after.logical[5]);
        let no_twin = DisplayState {
            logical: user_layout(),
            ..linear_state_with_twin()
        };
        assert_eq!(physical(&no_twin), user_layout());
    }

    #[test]
    fn rotation_by_90_or_270_swaps_the_logical_size() {
        // HDMI-1 is 1920x1080 turned by 270: 1080 wide, 1920 tall, right edge x = 0 + 1080.
        let mut after = linear_state_with_twin();
        let user = vec![logical(0, 600, 1.0, 3, true, &["HDMI-1"])];
        let plan = plan(&after, &user, "Meta-0", 1.0).unwrap();
        assert_eq!(plan[1].x, 1080);
        assert_eq!(plan[1].y, 600);
        // Same with 90 degrees and the flipped odd transforms; the even ones keep 1920.
        for (transform, right) in [
            (1, 1080),
            (5, 1080),
            (7, 1080),
            (0, 1920),
            (2, 1920),
            (4, 1920),
        ] {
            let user = vec![logical(0, 0, 1.0, transform, true, &["HDMI-1"])];
            let plan = plan_for(&after, &user);
            assert_eq!(plan[1].x, right, "transform {transform}");
        }
        // The twin itself is planned unrotated whatever the physical monitors do.
        after.layout_mode = None;
        assert_eq!(
            plan_for(&after, &[logical(0, 0, 1.0, 3, true, &["HDMI-1"])])[1].transform,
            0
        );
    }

    fn plan_for(after: &DisplayState, user: &[LogicalState]) -> Vec<LogicalConfig> {
        plan(after, user, "Meta-0", 1.0).unwrap()
    }

    #[test]
    fn fractional_and_integer_scales_use_mutters_rounding() {
        let mode = |w, h| mode("m", w, h, true);
        // `(int) roundf (mode / scale)` in `float`, after the transform's swap.
        assert_eq!(
            logical_size(Some(1), &mode(2560, 1440), 1.0, 0),
            Some((2560, 1440))
        );
        assert_eq!(
            logical_size(Some(1), &mode(2560, 1440), 1.5, 0),
            Some((1707, 960))
        );
        assert_eq!(
            logical_size(Some(1), &mode(2560, 1440), 2.0, 0),
            Some((1280, 720))
        );
        assert_eq!(
            logical_size(Some(1), &mode(2560, 1440), 1.25, 0),
            Some((2048, 1152))
        );
        assert_eq!(
            logical_size(Some(1), &mode(3840, 2160), 1.75, 0),
            Some((2194, 1234))
        );
        assert_eq!(
            logical_size(Some(1), &mode(3840, 2160), 2.0, 1),
            Some((1080, 1920))
        );
        assert_eq!(
            logical_size(Some(1), &mode(3840, 2160), 2.0, 3),
            Some((1080, 1920))
        );
        assert_eq!(
            logical_size(Some(1), &mode(3840, 2160), 2.0, 2),
            Some((1920, 1080))
        );
        // `roundf` rounds halves away from zero (2.5 -> 3, 1.5 -> 2); round-to-even would not.
        assert_eq!(logical_size(Some(1), &mode(5, 3), 2.0, 0), Some((3, 2)));
        // An absent layout-mode means logical.
        assert_eq!(
            logical_size(None, &mode(2560, 1440), 2.0, 0),
            Some((1280, 720))
        );
        // Physical layout mode: the mode size, whatever the scale.
        assert_eq!(
            logical_size(Some(2), &mode(3840, 2160), 2.0, 0),
            Some((3840, 2160))
        );
        assert_eq!(
            logical_size(Some(2), &mode(3840, 2160), 2.0, 1),
            Some((2160, 3840))
        );
        // Unusable inputs.
        assert_eq!(logical_size(Some(1), &mode(2560, 1440), 0.0, 0), None);
        assert_eq!(logical_size(Some(1), &mode(2560, 1440), -1.0, 0), None);
        assert_eq!(logical_size(Some(1), &mode(2560, 1440), f64::NAN, 0), None);
        assert_eq!(logical_size(Some(1), &mode(0, 1440), 1.0, 0), None);
    }

    #[test]
    fn scaled_monitors_move_the_anchor_by_their_logical_width() {
        // A 3840x2160 panel at scale 2 is 1920 logical wide; the other, at scale 1, is 2560.
        let monitors = vec![
            monitor("DP-1", vec![mode("3840x2160@60.000", 3840, 2160, true)]),
            monitor("DP-2", vec![mode("2560x1440@60.000", 2560, 1440, true)]),
            monitor("Meta-1", vec![mode("1280x720@60.000", 1280, 720, true)]),
        ];
        let user = vec![
            logical(0, 0, 2.0, 0, true, &["DP-1"]),
            logical(1920, 0, 1.0, 0, false, &["DP-2"]),
        ];
        let after = state(monitors, user.clone());
        let plan = plan(&after, &user, "Meta-1", 2.0).unwrap();
        // DP-2's right edge 1920 + 2560 beats DP-1's 1920.
        assert_eq!(
            plan[2],
            config(4480, 0, 2.0, 0, false, &[("Meta-1", "1280x720@60.000")])
        );
        // At scale 2 the twin's mode is 640x360 logical.
        assert_eq!(
            twin_rect(&plan, &after, "Meta-1"),
            Some((4480, 0, 640, 360))
        );
    }

    #[test]
    fn ties_for_the_rightmost_edge_go_to_the_smallest_y() {
        let monitors = vec![
            monitor("DP-1", vec![mode("1920x1080@60.000", 1920, 1080, true)]),
            monitor("DP-2", vec![mode("1920x1080@60.000", 1920, 1080, true)]),
            monitor("Meta-0", vec![mode("800x600@60.000", 800, 600, true)]),
        ];
        // The right edges are equal (1920); DP-1 is lower, DP-2 higher; listing order must not matter.
        let low = logical(0, 500, 1.0, 0, true, &["DP-1"]);
        let high = logical(0, -300, 1.0, 0, false, &["DP-2"]);
        for user in [vec![low.clone(), high.clone()], vec![high, low]] {
            let after = state(monitors.clone(), user.clone());
            let plan = plan(&after, &user, "Meta-0", 1.0).unwrap();
            let twin = plan.last().unwrap();
            assert_eq!((twin.x, twin.y), (1920, -300));
        }
        // A strictly further right edge wins over a smaller y.
        let user = vec![
            logical(0, -300, 1.0, 0, true, &["DP-2"]),
            logical(1, 500, 1.0, 0, false, &["DP-1"]),
        ];
        let after = state(monitors, user.clone());
        let twin = plan(&after, &user, "Meta-0", 1.0).unwrap().pop().unwrap();
        assert_eq!((twin.x, twin.y), (1921, 500));
    }

    #[test]
    fn mirrored_logical_monitors_keep_both_connectors() {
        let monitors = vec![
            monitor("DP-1", vec![mode("1920x1080@60.000", 1920, 1080, true)]),
            monitor("HDMI-1", vec![mode("1920x1080@50.000", 1920, 1080, true)]),
            monitor("Meta-0", vec![mode("800x600@60.000", 800, 600, true)]),
        ];
        let user = vec![logical(0, 0, 1.0, 0, true, &["DP-1", "HDMI-1"])];
        let after = state(monitors, user.clone());
        let plan = plan(&after, &user, "Meta-0", 1.0).unwrap();
        assert_eq!(
            plan[0].monitors,
            vec![
                ("DP-1".to_owned(), "1920x1080@60.000".to_owned()),
                ("HDMI-1".to_owned(), "1920x1080@50.000".to_owned())
            ]
        );
        assert_eq!((plan[1].x, plan[1].y), (1920, 0));
    }

    #[test]
    fn the_primary_flag_is_kept_and_the_twin_is_never_primary() {
        let after = linear_state_with_twin();
        let plan = plan(&after, &user_layout(), "Meta-0", 1.0).unwrap();
        let primaries: Vec<_> = plan.iter().map(|c| c.primary).collect();
        assert_eq!(primaries, [true, false, false, false]);
    }

    #[test]
    fn no_current_mode_or_unknown_connector_is_none() {
        let after = linear_state_with_twin();
        // The twin is absent.
        assert_eq!(plan(&after, &user_layout(), "Meta-7", 1.0), None);
        // The twin has no current mode.
        let mut no_twin_mode = after.clone();
        no_twin_mode.monitors[3].modes[0].current = false;
        assert_eq!(plan(&no_twin_mode, &user_layout(), "Meta-0", 1.0), None);
        // A physical connector has no current mode.
        let mut no_mode = after.clone();
        no_mode.monitors[1].modes[0].current = false;
        assert_eq!(plan(&no_mode, &user_layout(), "Meta-0", 1.0), None);
        // A physical connector the state does not know.
        let mut user = user_layout();
        user.push(logical(9000, 0, 1.0, 0, false, &["DP-9"]));
        assert_eq!(plan(&after, &user, "Meta-0", 1.0), None);
        // Nothing to attach the twin to, or a logical monitor without connectors.
        assert_eq!(plan(&after, &[], "Meta-0", 1.0), None);
        assert_eq!(
            plan(&after, &[logical(0, 0, 1.0, 0, true, &[])], "Meta-0", 1.0),
            None
        );
        // The twin is also among the physical monitors.
        assert_eq!(
            plan(
                &after,
                &[logical(0, 0, 1.0, 0, true, &["Meta-0"])],
                "Meta-0",
                1.0
            ),
            None
        );
        // Unusable scales.
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                plan(&after, &user_layout(), "Meta-0", scale),
                None,
                "{scale}"
            );
        }
        let mut bad_scale = user_layout();
        bad_scale[0].scale = 0.0;
        assert_eq!(plan(&after, &bad_scale, "Meta-0", 1.0), None);
        // Edges that do not fit in i32.
        let mut far = user_layout();
        far[0].x = i32::MAX - 10;
        assert_eq!(plan(&after, &far, "Meta-0", 1.0), None);
    }

    #[test]
    fn twin_rect_needs_the_twin_in_the_plan_and_its_mode_in_the_state() {
        let after = linear_state_with_twin();
        let plan = plan(&after, &user_layout(), "Meta-0", 1.0).unwrap();
        assert_eq!(twin_rect(&plan, &after, "Meta-9"), None);
        assert_eq!(twin_rect(&plan[..3], &after, "Meta-0"), None);
        let mut gone = after.clone();
        gone.monitors.pop();
        assert_eq!(twin_rect(&plan, &gone, "Meta-0"), None);
        // The mode id in the plan, not the state's current mode, decides the size.
        let mut regrown = after.clone();
        regrown.monitors[3].modes = vec![
            mode("1800x1169@60.000", 1800, 1169, false),
            mode("2048x1280@60.000", 2048, 1280, true),
        ];
        assert_eq!(
            twin_rect(&plan, &regrown, "Meta-0"),
            Some((4520, 1080, 1800, 1169))
        );
        // Physical layout mode ignores the scale.
        let mut physical_mode = after;
        physical_mode.layout_mode = Some(2);
        let mut at_two = plan.clone();
        at_two[3].scale = 2.0;
        assert_eq!(
            twin_rect(&at_two, &physical_mode, "Meta-0"),
            Some((4520, 1080, 1800, 1169))
        );
    }

    #[test]
    fn pick_scale_takes_the_largest_not_above_the_wish() {
        let supported = [1.0, 1.25, 1.5, 1.75, 2.0];
        assert_eq!(pick_scale(&supported, 1.5), 1.5);
        assert_eq!(pick_scale(&supported, 2.0), 2.0);
        assert_eq!(pick_scale(&supported, 3.0), 2.0);
        assert_eq!(pick_scale(&supported, 1.6), 1.5);
        assert_eq!(pick_scale(&supported, 1.0), 1.0);
        // The float round trip of a wish a hair below a listed scale still counts.
        assert_eq!(pick_scale(&supported, 1.5 - 5e-7), 1.5);
        assert_eq!(pick_scale(&supported, 1.5 - 1e-3), 1.25);
        // Order does not matter.
        assert_eq!(pick_scale(&[2.0, 1.0, 1.5], 1.7), 1.5);
        // Nothing small enough, nothing listed, or nonsense: 1.0.
        assert_eq!(pick_scale(&[2.0, 3.0], 1.5), 1.0);
        assert_eq!(pick_scale(&[], 2.0), 1.0);
        assert_eq!(pick_scale(&supported, 0.5), 1.0);
        assert_eq!(pick_scale(&supported, f64::NAN), 1.0);
        assert_eq!(pick_scale(&[f64::NAN, 0.0, -2.0, 1.5], 3.0), 1.5);
    }
}
