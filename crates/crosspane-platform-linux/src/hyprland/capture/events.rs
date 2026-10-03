//! Private in-process datagrams: no allocator or shared queue locks on the abort delivery path.
use crosspane_platform::{CaptureEvent, CaptureId, EndReason, MotionKind, PortalId};
use crosspane_types::{
    geom::{PointDevice, VectorLogical},
    hid::{HidUsage, MouseButton},
    id::{DisplayId, WindowId},
    input::{LockKeys, ScrollDelta, ScrollPhase},
    time::MonoTime,
};
pub(super) const SIZE: usize = 96;
pub(super) enum Packet {
    Event {
        epoch: u64,
        generation: u64,
        event: CaptureEvent,
    },
    Barrier(u64),
}
fn locks(v: Option<bool>) -> u64 {
    match v {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}
fn lock(v: u64) -> Option<bool> {
    match v {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}
fn bytes(w: [u64; 12]) -> [u8; SIZE] {
    let mut b = [0; SIZE];
    for (slot, value) in b.as_chunks_mut::<8>().0.iter_mut().zip(w) {
        *slot = value.to_le_bytes();
    }
    b
}
pub(super) fn barrier(token: u64) -> [u8; SIZE] {
    let mut w = [0; 12];
    w[0] = 11;
    w[1] = token;
    bytes(w)
}
pub(super) fn encode(epoch: u64, generation: u64, event: &CaptureEvent) -> Option<[u8; SIZE]> {
    let mut w = [0; 12];
    w[1] = epoch;
    w[2] = generation;
    match event {
        CaptureEvent::EdgePressed {
            portal,
            position,
            at,
        } => {
            w[0] = 0;
            w[3] = u64::from(portal.0);
            w[4] = position.to_bits();
            w[5] = at.as_nanos();
        }
        CaptureEvent::EdgeReleased { portal, at } => {
            w[0] = 1;
            w[3] = u64::from(portal.0);
            w[4] = at.as_nanos();
        }
        CaptureEvent::Started { id } => {
            w[0] = 2;
            w[3] = id.0;
        }
        CaptureEvent::Motion { dx, dy, kind, at } => {
            w[0] = 3;
            w[3] = dx.to_bits();
            w[4] = dy.to_bits();
            if let MotionKind::Accelerated { display } = kind {
                w[5] = 1;
                w[6] = u64::from(display.0);
            }
            w[7] = at.as_nanos();
        }
        CaptureEvent::Key { usage, down, at } => {
            w[0] = 4;
            w[3] = u64::from(usage.page);
            w[4] = u64::from(usage.id);
            w[5] = u64::from(*down);
            w[6] = at.as_nanos();
        }
        CaptureEvent::Button { button, down, at } => {
            w[0] = 5;
            w[3] = u64::from(button.0);
            w[4] = u64::from(*down);
            w[5] = at.as_nanos();
        }
        CaptureEvent::Scroll { delta, at } => {
            w[0] = 6;
            w[3] = delta.v120_x as u32 as u64;
            w[4] = delta.v120_y as u32 as u64;
            if let Some(v) = delta.pixels {
                w[5] = 1;
                w[6] = v.x.to_bits();
                w[7] = v.y.to_bits();
            }
            w[8] = match delta.phase {
                ScrollPhase::Discrete => 0,
                ScrollPhase::MayBegin => 1,
                ScrollPhase::Began => 2,
                ScrollPhase::Changed => 3,
                ScrollPhase::Ended => 4,
                ScrollPhase::Cancelled => 5,
                ScrollPhase::MomentumBegan => 6,
                ScrollPhase::MomentumChanged => 7,
                ScrollPhase::MomentumEnded => 8,
            };
            w[9] = u64::from(delta.stop_x);
            w[10] = u64::from(delta.stop_y);
            w[11] = at.as_nanos();
        }
        CaptureEvent::Ended { id, reason } => {
            w[0] = 7;
            w[3] = id.0;
            w[4] = match reason {
                EndReason::Requested => 0,
                EndReason::Lost => 1,
                EndReason::Aborted => 2,
            };
        }
        CaptureEvent::LockKeys(state) => {
            w[0] = 8;
            w[3] = locks(state.caps_lock);
            w[4] = locks(state.num_lock);
            w[5] = locks(state.scroll_lock);
        }
        CaptureEvent::KeyboardBlinded(v) => {
            w[0] = 9;
            w[3] = u64::from(*v);
        }
        CaptureEvent::LocalActivity { at } => {
            w[0] = 10;
            w[3] = at.as_nanos();
        }
        CaptureEvent::DragAtEdge {
            portal,
            position,
            window,
            grab,
            at,
        }
        | CaptureEvent::DragDroppedAtEdge {
            portal,
            position,
            window,
            grab,
            at,
        } => {
            w[0] = if matches!(event, CaptureEvent::DragDroppedAtEdge { .. }) {
                13
            } else {
                12
            };
            w[3] = u64::from(portal.0);
            w[4] = position.to_bits();
            w[5] = window.0;
            w[6] = grab.x.to_bits();
            w[7] = grab.y.to_bits();
            w[8] = at.as_nanos();
        }
        _ => return None,
    }
    Some(bytes(w))
}
pub(super) fn decode(b: &[u8; SIZE]) -> Option<Packet> {
    let mut w = [0; 12];
    for (slot, chunk) in w.iter_mut().zip(b.as_chunks::<8>().0) {
        *slot = u64::from_le_bytes(*chunk);
    }
    let f = |i: usize| f64::from_bits(w[i]);
    let at = |i: usize| MonoTime::from_nanos(w[i]);
    let event = match w[0] {
        0 => CaptureEvent::EdgePressed {
            portal: PortalId(w[3] as u32),
            position: f(4),
            at: at(5),
        },
        1 => CaptureEvent::EdgeReleased {
            portal: PortalId(w[3] as u32),
            at: at(4),
        },
        2 => CaptureEvent::Started {
            id: CaptureId(w[3]),
        },
        3 => CaptureEvent::Motion {
            dx: f(3),
            dy: f(4),
            kind: if w[5] == 0 {
                MotionKind::Unaccelerated
            } else {
                MotionKind::Accelerated {
                    display: DisplayId(w[6] as u32),
                }
            },
            at: at(7),
        },
        4 => CaptureEvent::Key {
            usage: HidUsage {
                page: w[3] as u16,
                id: w[4] as u16,
            },
            down: w[5] != 0,
            at: at(6),
        },
        5 => CaptureEvent::Button {
            button: MouseButton(w[3] as u8),
            down: w[4] != 0,
            at: at(5),
        },
        6 => CaptureEvent::Scroll {
            delta: ScrollDelta {
                v120_x: w[3] as i32,
                v120_y: w[4] as i32,
                pixels: (w[5] != 0).then(|| VectorLogical::new(f(6), f(7))),
                phase: match w[8] {
                    0 => ScrollPhase::Discrete,
                    1 => ScrollPhase::MayBegin,
                    2 => ScrollPhase::Began,
                    3 => ScrollPhase::Changed,
                    4 => ScrollPhase::Ended,
                    5 => ScrollPhase::Cancelled,
                    6 => ScrollPhase::MomentumBegan,
                    7 => ScrollPhase::MomentumChanged,
                    8 => ScrollPhase::MomentumEnded,
                    _ => return None,
                },
                stop_x: w[9] != 0,
                stop_y: w[10] != 0,
            },
            at: at(11),
        },
        7 => CaptureEvent::Ended {
            id: CaptureId(w[3]),
            reason: match w[4] {
                0 => EndReason::Requested,
                1 => EndReason::Lost,
                2 => EndReason::Aborted,
                _ => return None,
            },
        },
        8 => CaptureEvent::LockKeys(LockKeys {
            caps_lock: lock(w[3]),
            num_lock: lock(w[4]),
            scroll_lock: lock(w[5]),
        }),
        9 => CaptureEvent::KeyboardBlinded(w[3] != 0),
        10 => CaptureEvent::LocalActivity { at: at(3) },
        11 => return Some(Packet::Barrier(w[1])),
        12 => CaptureEvent::DragAtEdge {
            portal: PortalId(w[3] as u32),
            position: f(4),
            window: WindowId(w[5]),
            grab: PointDevice::new(f(6), f(7)),
            at: at(8),
        },
        13 => CaptureEvent::DragDroppedAtEdge {
            portal: PortalId(w[3] as u32),
            position: f(4),
            window: WindowId(w[5]),
            grab: PointDevice::new(f(6), f(7)),
            at: at(8),
        },
        _ => return None,
    };
    Some(Packet::Event {
        epoch: w[1],
        generation: w[2],
        event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_drag_packet_keeps_epoch_generation_window_and_device_grab() {
        let event = CaptureEvent::DragAtEdge {
            portal: PortalId(17),
            position: 0.375,
            window: WindowId(u64::MAX),
            grab: PointDevice::new(120.5, 45.25),
            at: MonoTime::from_nanos(12345),
        };
        let dropped = CaptureEvent::DragDroppedAtEdge {
            portal: PortalId(17),
            position: 0.375,
            window: WindowId(u64::MAX),
            grab: PointDevice::new(120.5, 45.25),
            at: MonoTime::from_nanos(12345),
        };
        for event in [event, dropped] {
            let Packet::Event {
                epoch,
                generation,
                event: decoded,
            } = decode(&encode(5, 9, &event).unwrap()).unwrap()
            else {
                panic!("wrong packet");
            };
            assert_eq!((epoch, generation, decoded), (5, 9, event));
        }
    }
}
