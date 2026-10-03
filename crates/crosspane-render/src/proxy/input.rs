use std::{
    collections::BTreeSet,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use crosspane_types::{
    geom::{PointDevice, VectorLogical},
    hid::{HidUsage, MouseButton},
    input::{ScrollDelta, ScrollPhase},
};
use winit::event::{ElementState, MouseScrollDelta, TouchPhase};

use super::HostEvent;

#[derive(Default)]
pub(super) struct InputState {
    keys: BTreeSet<HidUsage>,
    buttons: BTreeSet<MouseButton>,
    pub(super) position: PointDevice,
    arm_until: Option<Instant>,
    up_until: Option<Instant>,
    closing: bool,
}

/// Called only on the host thread. Failed/nonblocking acknowledgement retires the arm.
pub(super) fn install_arm(input: Option<&mut InputState>, until: Instant, done: SyncSender<bool>) {
    let mut input = input;
    let installed = input.as_mut().is_some_and(|input| input.arm(until));
    if done.try_send(installed).is_err()
        && let Some(input) = input
    {
        input.disarm();
    }
}

impl InputState {
    fn arm(&mut self, until: Instant) -> bool {
        if self.closing {
            return false;
        }
        self.arm_until = Some(until);
        true
    }

    pub(super) fn disarm(&mut self) {
        self.arm_until = None;
    }

    pub(super) fn close(&mut self) {
        self.disarm();
        self.up_until = None;
        self.closing = true;
    }

    /// Client releases on pointer leave do not describe the engine's native device lease.
    pub(super) fn drag_button<E: std::fmt::Display>(
        &mut self,
        button: winit::event::MouseButton,
        state: ElementState,
        now: Instant,
        start_drag: impl FnOnce() -> Result<(), E>,
    ) -> bool {
        if button != winit::event::MouseButton::Left {
            self.disarm();
            return false;
        }
        if !state.is_pressed() {
            return self.up_until.take().is_some_and(|until| now < until);
        }
        // A new unarmed press retires a missing trailing up, so its pair routes normally.
        self.up_until = None;
        if self.arm_until.take().is_some_and(|until| now < until) {
            self.up_until = Some(now + Duration::from_secs(5));
            if let Err(error) = start_drag() {
                tracing::warn!(%error, "proxy native drag failed");
            }
            true
        } else {
            false
        }
    }

    pub(super) fn key(&mut self, usage: HidUsage, state: ElementState) {
        if state.is_pressed() {
            self.keys.insert(usage);
        } else {
            self.keys.remove(&usage);
        }
    }

    pub(super) fn button(&mut self, button: MouseButton, state: ElementState) {
        if state.is_pressed() {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }

    pub(super) fn release(&mut self, id: u64, events: &mut dyn FnMut(HostEvent)) {
        for usage in std::mem::take(&mut self.keys) {
            events(HostEvent::Key {
                id,
                usage,
                down: false,
            });
        }
        for button in std::mem::take(&mut self.buttons) {
            events(HostEvent::Button {
                id,
                button,
                down: false,
                position: self.position,
            });
        }
    }
}

pub(super) fn mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    use winit::event::MouseButton as W;
    Some(match button {
        W::Left => MouseButton::PRIMARY,
        W::Right => MouseButton::SECONDARY,
        W::Middle => MouseButton::TERTIARY,
        W::Back => MouseButton::BACK,
        W::Forward => MouseButton::FORWARD,
        // The frozen HID button type only represents u8; do not truncate larger codes.
        W::Other(n) => MouseButton(u8::try_from(n).ok()?),
    })
}

pub(super) fn scroll(delta: MouseScrollDelta, phase: TouchPhase, scale: f64) -> ScrollDelta {
    // Winit positive x moves content right (reveals left), while HID positive x scrolls
    // right (reveals right). Both positive y conventions reveal content above (scroll up).
    // PixelDelta is physical; ScrollDelta's pixels are controller logical pixels.
    match delta {
        MouseScrollDelta::LineDelta(x, y) => ScrollDelta {
            v120_x: (-f64::from(x) * 120.0).round() as i32,
            v120_y: (f64::from(y) * 120.0).round() as i32,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        },
        MouseScrollDelta::PixelDelta(p) => ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: Some(VectorLogical::new(-p.x / scale, p.y / scale)),
            phase: match phase {
                TouchPhase::Started => ScrollPhase::Began,
                TouchPhase::Moved => ScrollPhase::Changed,
                TouchPhase::Ended => ScrollPhase::Ended,
                TouchPhase::Cancelled => ScrollPhase::Cancelled,
            },
            stop_x: false,
            stop_y: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(input: Option<&mut InputState>, until: Instant) -> bool {
        let (done, ack) = std::sync::mpsc::sync_channel(1);
        install_arm(input, until, done);
        ack.try_recv().unwrap()
    }

    fn press(input: &mut InputState, now: Instant) -> bool {
        input.drag_button(
            winit::event::MouseButton::Left,
            ElementState::Pressed,
            now,
            || Ok::<_, &str>(()),
        )
    }

    fn up(input: &mut InputState, now: Instant) -> bool {
        input.drag_button(
            winit::event::MouseButton::Left,
            ElementState::Released,
            now,
            || -> Result<(), &str> { panic!("up never starts drag") },
        )
    }

    #[test]
    fn arm_is_installed_before_ack_replaced_and_unknown_or_closing_is_refused() {
        let now = Instant::now();
        let mut input = InputState::default();
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(armed(Some(&mut input), now + Duration::from_secs(2)));
        assert!(press(&mut input, now + Duration::from_millis(1500)));
        assert!(!armed(None, now + Duration::from_secs(1)));
        input.close();
        assert!(!armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(!up(&mut input, now));
        assert!(!press(&mut input, now));
        let mut input = InputState::default();
        let (done, ack) = std::sync::mpsc::sync_channel(1);
        drop(ack);
        install_arm(Some(&mut input), now + Duration::from_secs(1), done);
        assert!(!press(&mut input, now));
    }

    #[test]
    fn buffered_ack_survives_late_receiver_and_failed_ack_only_retires_unused_arm() {
        let now = Instant::now();
        let mut input = InputState::default();
        let (done, ack) = std::sync::mpsc::sync_channel(1);
        install_arm(Some(&mut input), now + Duration::from_secs(1), done);
        // The caller starts receiving only after the host has installed and answered.
        assert_eq!(ack.recv_timeout(Duration::from_millis(10)), Ok(true));
        assert!(press(&mut input, now));
        for disconnected in [false, true] {
            let (done, ack) = std::sync::mpsc::sync_channel(1);
            if disconnected {
                drop(ack);
            } else {
                done.try_send(false).unwrap();
            }
            install_arm(Some(&mut input), now + Duration::from_secs(1), done);
            assert!(up(&mut input, now));
            assert!(!press(&mut input, now));
            assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
            assert!(press(&mut input, now));
        }
    }

    #[test]
    fn owed_up_survives_disarm_rearm_and_other_button_once_without_consuming_new_click() {
        let now = Instant::now();
        for action in [0, 1, 2] {
            let mut input = InputState::default();
            assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
            assert!(press(&mut input, now));
            match action {
                0 => input.disarm(),
                1 => assert!(armed(Some(&mut input), now + Duration::from_secs(2))),
                _ => assert!(!input.drag_button(
                    winit::event::MouseButton::Right,
                    ElementState::Pressed,
                    now,
                    || Ok::<_, &str>(())
                )),
            }
            assert!(up(&mut input, now + Duration::from_millis(1)));
            assert!(!up(&mut input, now + Duration::from_millis(2)));
            assert_eq!(
                press(&mut input, now + Duration::from_millis(3)),
                action == 1
            );
            assert_eq!(up(&mut input, now + Duration::from_millis(4)), action == 1);
        }
        let mut input = InputState::default();
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(press(&mut input, now));
        input.disarm();
        assert!(!up(&mut input, now + Duration::from_secs(5)));
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(press(&mut input, now));
        input.disarm();
        assert!(!press(&mut input, now + Duration::from_millis(1)));
        assert!(!up(&mut input, now + Duration::from_millis(2)));
    }

    #[test]
    fn one_press_one_trailing_up_and_failed_drag_leave_pressed_bookkeeping_untouched() {
        let now = Instant::now();
        for fail in [false, true] {
            let mut input = InputState::default();
            input.key(HidUsage::keyboard(4), ElementState::Pressed);
            assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
            let mut starts = 0;
            assert!(input.drag_button(
                winit::event::MouseButton::Left,
                ElementState::Pressed,
                now,
                || {
                    starts += 1;
                    if fail { Err("fake failure") } else { Ok(()) }
                }
            ));
            assert_eq!(starts, 1);
            let mut released = Vec::new();
            input.release(7, &mut |event| released.push(event));
            assert_eq!(
                released,
                vec![HostEvent::Key {
                    id: 7,
                    usage: HidUsage::keyboard(4),
                    down: false
                }]
            );
            assert!(up(&mut input, now + Duration::from_millis(1)));
            assert!(!up(&mut input, now + Duration::from_millis(2)));
            assert!(!press(&mut input, now + Duration::from_millis(3)));
            assert!(!up(&mut input, now + Duration::from_millis(4)));
        }
    }

    #[test]
    fn expiry_other_buttons_disarm_and_close_never_consume_and_missing_up_is_bounded() {
        let now = Instant::now();
        let mut input = InputState::default();
        assert!(armed(Some(&mut input), now));
        assert!(!press(&mut input, now));
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(!press(&mut input, now + Duration::from_secs(2)));
        for button in [
            winit::event::MouseButton::Right,
            winit::event::MouseButton::Other(256),
        ] {
            assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
            assert!(!input.drag_button(button, ElementState::Pressed, now, || Ok::<_, &str>(())));
            assert!(!press(&mut input, now));
        }
        for consumed in [false, true] {
            assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
            if consumed {
                assert!(press(&mut input, now));
            }
            input.disarm();
            assert_eq!(up(&mut input, now), consumed);
            assert!(!press(&mut input, now));
        }
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(press(&mut input, now));
        assert!(!up(&mut input, now + Duration::from_secs(5)));
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        assert!(press(&mut input, now));
        assert!(!press(&mut input, now + Duration::from_millis(1)));
        assert!(!up(&mut input, now + Duration::from_millis(2)));
        assert!(armed(Some(&mut input), now + Duration::from_secs(1)));
        input.close();
        assert!(!press(&mut input, now));
    }

    #[test]
    fn scroll_sign_units_and_gesture_phases() {
        let lines = scroll(
            MouseScrollDelta::LineDelta(1.25, 1.0),
            TouchPhase::Moved,
            2.0,
        );
        assert_eq!((lines.v120_x, lines.v120_y), (-150, 120));
        assert_eq!(lines.phase, ScrollPhase::Discrete);
        assert_eq!(lines.pixels, None);
        for (touch, phase) in [
            (TouchPhase::Started, ScrollPhase::Began),
            (TouchPhase::Moved, ScrollPhase::Changed),
            (TouchPhase::Ended, ScrollPhase::Ended),
            (TouchPhase::Cancelled, ScrollPhase::Cancelled),
        ] {
            let delta = scroll(
                MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(6.0, 8.0)),
                touch,
                2.0,
            );
            assert_eq!(delta.pixels, Some(VectorLogical::new(-3.0, 4.0)));
            assert_eq!(delta.phase, phase);
            let zero = scroll(
                MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 0.0)),
                touch,
                2.0,
            );
            assert_eq!(zero.phase, phase);
        }
    }

    #[test]
    fn releases_precede_focus_loss_and_only_include_held_inputs() {
        let mut state = InputState::default();
        state.key(HidUsage::keyboard(4), ElementState::Pressed);
        state.key(HidUsage::keyboard(5), ElementState::Pressed);
        state.key(HidUsage::keyboard(5), ElementState::Released);
        state.button(MouseButton::PRIMARY, ElementState::Pressed);
        let mut events = Vec::new();
        state.release(7, &mut |event| events.push(event));
        events.push(HostEvent::Focus {
            id: 7,
            focused: false,
        });
        assert_eq!(
            events,
            vec![
                HostEvent::Key {
                    id: 7,
                    usage: HidUsage::keyboard(4),
                    down: false
                },
                HostEvent::Button {
                    id: 7,
                    button: MouseButton::PRIMARY,
                    down: false,
                    position: PointDevice::zero()
                },
                HostEvent::Focus {
                    id: 7,
                    focused: false
                }
            ]
        );
        state.release(7, &mut |_| panic!("already released"));
        for (winit, hid) in [
            (winit::event::MouseButton::Left, 1),
            (winit::event::MouseButton::Right, 2),
            (winit::event::MouseButton::Middle, 3),
            (winit::event::MouseButton::Back, 4),
            (winit::event::MouseButton::Forward, 5),
            (winit::event::MouseButton::Other(9), 9),
        ] {
            assert_eq!(mouse_button(winit), Some(MouseButton(hid)));
        }
        assert_eq!(mouse_button(winit::event::MouseButton::Other(256)), None);
    }
}
