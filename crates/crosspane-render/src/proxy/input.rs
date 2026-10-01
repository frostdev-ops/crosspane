use std::collections::BTreeSet;

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
}

impl InputState {
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
