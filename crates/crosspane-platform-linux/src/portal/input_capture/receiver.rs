//! The EIS receiver of an InputCapture session (`reis`, receiver role).
//!
//! The portal's `ConnectToEIS` hands the client a socket to the compositor's EIS server. This
//! module is the client: it performs the handshake as a *receiver*, binds pointer, button, scroll
//! and keyboard on the seat (mutter creates no pointer device unless it gets all three pointer
//! capabilities), and turns what the compositor emits while an activation is held into
//! [`Tagged`] inputs for the activation state machine.
//!
//! Each input carries the *emulation sequence* of its device (`start_emulating`'s sequence number,
//! which mutter sets to the activation id), so the machine can tell which activation it belongs
//! to. Events reach the machine already grouped by `reis` into device frames: it reports a
//! `Frame` input after the events of each frame.
//!
//! Key codes, button codes and typed data are never logged; `REIS_DEBUG` would make `reis` print
//! every message, key codes included, so the connection is refused while it is set (as the EIS
//! sender does).
//!
//! The receiver is owned and driven by one thread (the event converter is not `Send`).

use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crosspane_platform::PlatformError;
use reis::PendingRequestResult;
use reis::ei;
use reis::ei::button::ButtonState;
use reis::ei::keyboard::KeyState;
use reis::event::{self, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::EiHandshaker;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

use super::pressure::{Input, Scroll};

/// The name the compositor shows for this client.
const CLIENT_NAME: &str = "Crosspane";

/// One input and the emulation sequence of the device it came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Tagged {
    /// `None`: the device was not emulating (the input is unattributable and dropped).
    pub seq: Option<u32>,
    pub input: Input,
}

/// Which devices the compositor has announced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Devices {
    pub pointer: bool,
    pub keyboard: bool,
}

fn backend(message: &str) -> PlatformError {
    PlatformError::Backend(message.into())
}

/// Whether `value` of `REIS_DEBUG` turns on reis's message tracing: any non-empty value does.
fn debug_requested(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Refuse to connect while `REIS_DEBUG` would make reis log key codes.
fn refuse_debug(value: Option<&OsStr>) -> Result<(), PlatformError> {
    if debug_requested(value) {
        Err(backend(
            "REIS_DEBUG is set; refusing to attach (it would log key codes)",
        ))
    } else {
        Ok(())
    }
}

fn timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// Wait until the socket is readable, `deadline` passes or a signal arrives.
fn wait_readable(context: &ei::Context, deadline: Instant) -> Result<(), PlatformError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(PlatformError::Timeout);
    }
    let mut fds = [PollFd::new(context, PollFlags::IN)];
    match poll(&mut fds, Some(&timespec(remaining))) {
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(_) => Err(backend("could not poll the EIS socket")),
    }
}

/// A device that carries pointer or keyboard input.
struct Dev {
    device: event::Device,
    /// The sequence of its `start_emulating`, while it is emulating.
    seq: Option<u32>,
    pointer: bool,
    keyboard: bool,
}

/// The connection to the compositor's EIS server.
pub(super) struct Receiver {
    context: ei::Context,
    converter: EiEventConverter,
    devices: Vec<Dev>,
    /// The seat was announced and its capabilities bound.
    bound: bool,
    /// The compositor ended the connection, or it failed.
    dead: bool,
}

impl Receiver {
    /// Handshake as a receiver on the portal's EIS socket. Bounded by `budget`.
    pub(super) fn connect(fd: OwnedFd, budget: Duration) -> Result<Receiver, PlatformError> {
        // Before the context exists: reis reads the variable when it is created.
        refuse_debug(std::env::var_os("REIS_DEBUG").as_deref())?;
        let deadline = Instant::now() + budget;
        let context = ei::Context::new(UnixStream::from(fd))
            .map_err(|_| backend("could not use the EIS socket"))?;
        let mut shaker = EiHandshaker::new(CLIENT_NAME, ei::handshake::ContextType::Receiver);
        let response = 'handshake: loop {
            wait_readable(&context, deadline)?;
            context
                .read()
                .map_err(|_| backend("EIS socket closed during the handshake"))?;
            while let Some(result) = context.pending_event() {
                match result {
                    PendingRequestResult::Request(event) => {
                        let step = shaker
                            .handle_event(event)
                            .map_err(|_| backend("EIS handshake failed"))?;
                        if let Some(response) = step {
                            break 'handshake response;
                        }
                    }
                    PendingRequestResult::ParseError(_) => {
                        return Err(backend("EIS protocol error during the handshake"));
                    }
                    PendingRequestResult::InvalidObject(_) => {}
                }
            }
        };
        let converter = EiEventConverter::new(&context, response);
        let mut receiver = Receiver {
            context,
            converter,
            devices: Vec::new(),
            bound: false,
            dead: false,
        };
        // Whatever followed the handshake in the same read.
        let mut ignored = Vec::new();
        receiver.dispatch(&mut ignored).map_err(backend)?;
        if receiver.dead {
            return Err(backend("EIS connection lost"));
        }
        Ok(receiver)
    }

    /// The socket, for polling.
    pub(super) fn context(&self) -> &ei::Context {
        &self.context
    }

    pub(super) fn is_dead(&self) -> bool {
        self.dead
    }

    /// Read until the seat is announced and bound (the compositor makes its devices then), or
    /// `budget` runs out. Nothing is captured before `Enable`, so no input is dropped here.
    pub(super) fn wait_bound(&mut self, budget: Duration) -> Result<(), PlatformError> {
        let deadline = Instant::now() + budget;
        while !self.bound {
            if self.dead {
                return Err(backend("EIS connection lost"));
            }
            wait_readable(&self.context, deadline)?;
            drop(self.pump());
        }
        Ok(())
    }

    /// The device kinds present now.
    pub(super) fn devices(&self) -> Devices {
        Devices {
            pointer: self.devices.iter().any(|d| d.pointer),
            keyboard: self.devices.iter().any(|d| d.keyboard),
        }
    }

    /// Read what the compositor sent and return the inputs in order. Failure marks the connection
    /// dead (after returning what arrived before it).
    pub(super) fn pump(&mut self) -> Vec<Tagged> {
        let mut out = Vec::new();
        if self.dead {
            return out;
        }
        let closed = self.context.read().is_err();
        if let Err(reason) = self.dispatch(&mut out) {
            tracing::debug!(reason, "EIS connection ended");
            self.dead = true;
        } else if closed {
            tracing::debug!("EIS peer closed the socket");
            self.dead = true;
        }
        if let Err(errno) = self.context.flush()
            && errno != Errno::AGAIN
        {
            self.dead = true;
        }
        out
    }

    fn dispatch(&mut self, out: &mut Vec<Tagged>) -> Result<(), &'static str> {
        while let Some(result) = self.context.pending_event() {
            match result {
                PendingRequestResult::Request(event) => self
                    .converter
                    .handle_event(event)
                    .map_err(|_| "EIS protocol violation")?,
                PendingRequestResult::ParseError(_) => return Err("EIS message did not parse"),
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        while let Some(event) = self.converter.next_event() {
            self.on_event(event, out)?;
        }
        Ok(())
    }

    fn find(&mut self, device: &event::Device) -> Option<&mut Dev> {
        self.devices.iter_mut().find(|d| d.device == *device)
    }

    fn tag(&mut self, device: &event::Device, input: Input, out: &mut Vec<Tagged>) {
        let seq = self.find(device).and_then(|d| d.seq);
        out.push(Tagged { seq, input });
    }

    fn on_event(&mut self, event: EiEvent, out: &mut Vec<Tagged>) -> Result<(), &'static str> {
        match event {
            EiEvent::Disconnected(disconnected) => {
                tracing::debug!(reason = ?disconnected.reason, "EIS compositor disconnected us");
                return Err("EIS compositor disconnected us");
            }
            EiEvent::SeatAdded(added) => {
                // mutter makes a pointer device only for pointer + button + scroll together.
                added.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                        | DeviceCapability::Keyboard,
                );
                self.bound = true;
                if let Err(errno) = self.context.flush()
                    && errno != Errno::AGAIN
                {
                    return Err("could not write the EIS bind");
                }
            }
            EiEvent::DeviceAdded(added) => {
                let device = added.device;
                let pointer = device.has_capability(DeviceCapability::Pointer);
                let keyboard = device.has_capability(DeviceCapability::Keyboard);
                tracing::debug!(pointer, keyboard, "EIS capture device added");
                if pointer || keyboard {
                    self.devices.push(Dev {
                        device,
                        seq: None,
                        pointer,
                        keyboard,
                    });
                }
            }
            EiEvent::DeviceRemoved(removed) => {
                self.devices.retain(|d| d.device != removed.device);
            }
            EiEvent::DevicePaused(paused) => {
                if let Some(dev) = self.find(&paused.device) {
                    dev.seq = None;
                }
            }
            EiEvent::DeviceStartEmulating(start) => {
                if let Some(dev) = self.find(&start.device) {
                    dev.seq = Some(start.sequence);
                }
            }
            EiEvent::DeviceStopEmulating(stop) => {
                if let Some(dev) = self.find(&stop.device) {
                    dev.seq = None;
                }
            }
            EiEvent::Frame(frame) => self.tag(&frame.device, Input::Frame, out),
            EiEvent::PointerMotion(motion) => self.tag(
                &motion.device,
                Input::Motion {
                    dx: f64::from(motion.dx),
                    dy: f64::from(motion.dy),
                },
                out,
            ),
            EiEvent::Button(button) => self.tag(
                &button.device,
                Input::Button {
                    code: button.button,
                    down: button.state == ButtonState::Press,
                },
                out,
            ),
            EiEvent::KeyboardKey(key) => {
                // An evdev code is 16 bits; anything else is not a key.
                if let Ok(code) = u16::try_from(key.key) {
                    self.tag(
                        &key.device,
                        Input::Key {
                            code,
                            down: key.state == KeyState::Press,
                        },
                        out,
                    );
                }
            }
            EiEvent::ScrollDelta(scroll) => self.tag(
                &scroll.device,
                Input::Scroll(Scroll::Smooth {
                    dx: f64::from(scroll.dx),
                    dy: f64::from(scroll.dy),
                }),
                out,
            ),
            EiEvent::ScrollDiscrete(scroll) => self.tag(
                &scroll.device,
                Input::Scroll(Scroll::Discrete {
                    dx: scroll.discrete_dx,
                    dy: scroll.discrete_dy,
                }),
                out,
            ),
            EiEvent::ScrollStop(stop) => self.tag(
                &stop.device,
                Input::Scroll(Scroll::Stop {
                    x: stop.x,
                    y: stop.y,
                }),
                out,
            ),
            EiEvent::ScrollCancel(stop) => self.tag(
                &stop.device,
                Input::Scroll(Scroll::Cancel {
                    x: stop.x,
                    y: stop.y,
                }),
                out,
            ),
            // Modifier masks, absolute pointers, touch, text and seats going away (their devices
            // follow): not part of the captured stream.
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reis::request::DeviceCapability as ServerCapability;

    use super::super::fake_eis::{Cmd, Fake, Seen, Target, WAIT};
    use super::*;

    /// Pump the receiver until `want` inputs arrived or the wait ran out.
    fn collect(receiver: &mut Receiver, want: usize) -> Vec<Tagged> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while got.len() < want && Instant::now() < deadline {
            let mut fds = [PollFd::new(receiver.context(), PollFlags::IN)];
            let _ = poll(&mut fds, Some(&timespec(Duration::from_millis(20))));
            got.extend(receiver.pump());
        }
        got
    }

    fn until(receiver: &mut Receiver, what: &str, mut done: impl FnMut(&Receiver) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(receiver) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let mut fds = [PollFd::new(receiver.context(), PollFlags::IN)];
            let _ = poll(&mut fds, Some(&timespec(Duration::from_millis(20))));
            let _ = receiver.pump();
        }
    }

    fn connected() -> (Fake, Receiver) {
        let (fake, fd) = Fake::start(true);
        let mut receiver = Receiver::connect(fd, WAIT).unwrap();
        assert_eq!(
            fake.next(),
            Seen::Hello {
                name: "Crosspane".into(),
                sender: false
            }
        );
        until(&mut receiver, "the devices", |r| {
            r.devices()
                == Devices {
                    pointer: true,
                    keyboard: true,
                }
        });
        (fake, receiver)
    }

    #[test]
    fn the_receiver_greets_as_a_receiver_and_binds_pointer_button_scroll_and_keyboard() {
        let (fake, receiver) = connected();
        assert_eq!(
            fake.next(),
            Seen::Bound(
                ServerCapability::Pointer
                    | ServerCapability::Button
                    | ServerCapability::Scroll
                    | ServerCapability::Keyboard
            )
        );
        assert!(!receiver.is_dead());
    }

    #[test]
    fn inputs_arrive_in_order_tagged_with_the_emulation_sequence_and_framed() {
        let (fake, mut receiver) = connected();
        fake.send(Cmd::Start(Target::Pointer, 5));
        fake.send(Cmd::Start(Target::Keyboard, 5));
        fake.send(Cmd::Motion(3.5, -1.0));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::Button(0x110, true));
        fake.send(Cmd::Button(0x110, false));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::Key(30, true));
        fake.send(Cmd::Frame(Target::Keyboard));
        let got = collect(&mut receiver, 7);
        let s = Some(5);
        assert_eq!(
            got,
            vec![
                Tagged {
                    seq: s,
                    input: Input::Motion { dx: 3.5, dy: -1.0 }
                },
                Tagged {
                    seq: s,
                    input: Input::Frame
                },
                Tagged {
                    seq: s,
                    input: Input::Button {
                        code: 0x110,
                        down: true
                    }
                },
                Tagged {
                    seq: s,
                    input: Input::Button {
                        code: 0x110,
                        down: false
                    }
                },
                Tagged {
                    seq: s,
                    input: Input::Frame
                },
                Tagged {
                    seq: s,
                    input: Input::Key {
                        code: 30,
                        down: true
                    }
                },
                // The keyboard's own frame.
                Tagged {
                    seq: s,
                    input: Input::Frame
                },
            ]
        );
    }

    #[test]
    fn scrolls_map_to_the_four_scroll_inputs() {
        let (fake, mut receiver) = connected();
        fake.send(Cmd::Start(Target::Pointer, 2));
        fake.send(Cmd::Scroll(1.5, 20.0));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::Discrete(0, 120));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::ScrollStop {
            x: false,
            y: true,
            cancel: false,
        });
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::ScrollStop {
            x: true,
            y: true,
            cancel: true,
        });
        fake.send(Cmd::Frame(Target::Pointer));
        let got: Vec<Input> = collect(&mut receiver, 8)
            .into_iter()
            .map(|t| t.input)
            .filter(|i| *i != Input::Frame)
            .collect();
        assert_eq!(
            got,
            vec![
                Input::Scroll(Scroll::Smooth { dx: 1.5, dy: 20.0 }),
                Input::Scroll(Scroll::Discrete { dx: 0, dy: 120 }),
                Input::Scroll(Scroll::Stop { x: false, y: true }),
                Input::Scroll(Scroll::Cancel { x: true, y: true }),
            ]
        );
    }

    #[test]
    fn input_outside_an_emulation_window_is_untagged_and_a_new_window_changes_the_tag() {
        let (fake, mut receiver) = connected();
        // Not emulating yet: unattributable.
        fake.send(Cmd::Motion(1.0, 0.0));
        fake.send(Cmd::Frame(Target::Pointer));
        let got = collect(&mut receiver, 2);
        assert_eq!(
            got.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![None, None]
        );
        fake.send(Cmd::Start(Target::Pointer, 7));
        fake.send(Cmd::Motion(1.0, 0.0));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::Stop(Target::Pointer));
        fake.send(Cmd::Motion(2.0, 0.0));
        fake.send(Cmd::Frame(Target::Pointer));
        fake.send(Cmd::Start(Target::Pointer, 8));
        fake.send(Cmd::Motion(3.0, 0.0));
        fake.send(Cmd::Frame(Target::Pointer));
        let seqs: Vec<Option<u32>> = collect(&mut receiver, 6).iter().map(|t| t.seq).collect();
        assert_eq!(seqs, vec![Some(7), Some(7), None, None, Some(8), Some(8)]);
        // A paused device is no longer emulating either.
        fake.send(Cmd::Pause(Target::Pointer));
        fake.send(Cmd::Motion(4.0, 0.0));
        fake.send(Cmd::Frame(Target::Pointer));
        let seqs: Vec<Option<u32>> = collect(&mut receiver, 2).iter().map(|t| t.seq).collect();
        assert_eq!(seqs, vec![None, None]);
    }

    #[test]
    fn removed_devices_leave_the_device_set_and_a_disconnect_kills_the_connection() {
        let (fake, mut receiver) = connected();
        fake.send(Cmd::RemoveDevices);
        until(&mut receiver, "the devices to go", |r| {
            r.devices() == Devices::default()
        });
        assert!(!receiver.is_dead());
        fake.send(Cmd::Disconnect);
        until(&mut receiver, "the disconnect", Receiver::is_dead);
    }

    #[test]
    fn a_closed_socket_kills_the_connection() {
        let (fake, mut receiver) = connected();
        drop(fake);
        until(&mut receiver, "the hang-up", Receiver::is_dead);
    }

    #[test]
    fn a_server_that_never_answers_times_out_the_handshake() {
        let (client, _server) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let result = Receiver::connect(OwnedFd::from(client), Duration::from_millis(100));
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn reis_debug_refuses_the_connection() {
        assert!(refuse_debug(None).is_ok());
        assert!(refuse_debug(Some(OsStr::new(""))).is_ok());
        for value in ["1", "true", "0", "anything"] {
            let result = refuse_debug(Some(OsStr::new(value)));
            assert!(
                matches!(&result, Err(PlatformError::Backend(m)) if m.contains("REIS_DEBUG")),
                "{value}"
            );
        }
    }
}
