//! Test-only: a fake EIS server (a compositor's input-capture side) on a socket pair.
//!
//! It plays mutter's part for the receiver: it announces a seat, creates a pointer device
//! (pointer, button, scroll) and a keyboard device when the client binds, resumes them, and then
//! does what the test tells it to (start and stop emulating with a sequence number, motion,
//! buttons, keys, scrolls, frames, removing its devices, disconnecting). No real compositor or
//! portal is involved.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver as Channel, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use reis::PendingRequestResult;
use reis::enumflags2::BitFlags;
use reis::handshake::EisHandshaker;
use reis::request::{self, EisRequest, EisRequestConverter};
use reis::{eis, request::DeviceCapability as ServerCapability};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

pub(super) const WAIT: Duration = Duration::from_secs(3);

/// What the fake compositor saw.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Seen {
    Hello { name: String, sender: bool },
    Bound(BitFlags<ServerCapability>),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Target {
    Pointer,
    Keyboard,
}

/// What the test makes the fake compositor do.
pub(super) enum Cmd {
    Start(Target, u32),
    Stop(Target),
    Frame(Target),
    Motion(f32, f32),
    Button(u32, bool),
    Key(u32, bool),
    Scroll(f32, f32),
    Discrete(i32, i32),
    ScrollStop { x: bool, y: bool, cancel: bool },
    Pause(Target),
    RemoveDevices,
    Disconnect,
}

pub(super) struct Fake {
    seen: Channel<Seen>,
    cmds: Option<Sender<Cmd>>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    pub(super) fn start(devices: bool) -> (Fake, OwnedFd) {
        let (client, server) = UnixStream::pair().unwrap();
        let (seen_tx, seen) = mpsc::channel();
        let (cmds, cmd_rx) = mpsc::channel();
        let thread = thread::spawn(move || serve(server, devices, &seen_tx, &cmd_rx));
        (
            Fake {
                seen,
                cmds: Some(cmds),
                thread: Some(thread),
            },
            OwnedFd::from(client),
        )
    }

    pub(super) fn send(&self, cmd: Cmd) {
        // The server may be gone already (the client closed the socket): nothing to tell then.
        let _ = self.cmds.as_ref().unwrap().send(cmd);
    }

    pub(super) fn next(&self) -> Seen {
        self.seen.recv_timeout(WAIT).expect("the fake saw nothing")
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.cmds = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(stream: UnixStream, devices: bool, seen: &Sender<Seen>, cmds: &Channel<Cmd>) {
    let Ok(ctx) = eis::Context::new(stream) else {
        return;
    };
    let mut shaker = EisHandshaker::new(&ctx, 0);
    let mut converter: Option<EisRequestConverter> = None;
    let mut pointer: Option<request::Device> = None;
    let mut keyboard: Option<request::Device> = None;
    loop {
        let mut fds = [PollFd::new(&ctx, PollFlags::IN)];
        let _ = poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 5_000_000,
            }),
        );
        if ctx.read().is_err() {
            return;
        }
        while let Some(result) = ctx.pending_request() {
            let PendingRequestResult::Request(req) = result else {
                continue;
            };
            match converter.as_mut() {
                None => {
                    if let Some(response) = shaker.handle_request(req).unwrap() {
                        let _ = seen.send(Seen::Hello {
                            name: response.name.clone().unwrap_or_default(),
                            sender: response.context_type == eis::handshake::ContextType::Sender,
                        });
                        let conv = EisRequestConverter::new(&ctx, response, 0);
                        let _seat = conv.handle().add_seat(Some("seat0"), BitFlags::all());
                        converter = Some(conv);
                    }
                }
                Some(conv) => conv.handle_request(req).unwrap(),
            }
        }
        if let Some(conv) = converter.as_mut() {
            while let Some(request) = conv.next_request() {
                if let EisRequest::Bind(bind) = request {
                    let _ = seen.send(Seen::Bound(bind.capabilities));
                    if devices {
                        let p = bind.seat.add_device(
                            Some("pointer"),
                            eis::device::DeviceType::Virtual,
                            ServerCapability::Pointer
                                | ServerCapability::Button
                                | ServerCapability::Scroll,
                            |_| {},
                        );
                        p.resumed();
                        let k = bind.seat.add_device(
                            Some("keyboard"),
                            eis::device::DeviceType::Virtual,
                            BitFlags::from(ServerCapability::Keyboard),
                            |_| {},
                        );
                        k.resumed();
                        pointer = Some(p);
                        keyboard = Some(k);
                    }
                }
            }
        }
        loop {
            match cmds.try_recv() {
                Ok(cmd) => run(cmd, &pointer, &keyboard, converter.as_ref()),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let _ = ctx.flush();
    }
}

fn run(
    cmd: Cmd,
    pointer: &Option<request::Device>,
    keyboard: &Option<request::Device>,
    converter: Option<&EisRequestConverter>,
) {
    let target = |t: Target| match t {
        Target::Pointer => pointer.as_ref(),
        Target::Keyboard => keyboard.as_ref(),
    };
    match cmd {
        Cmd::Start(t, seq) => {
            if let Some(d) = target(t) {
                d.start_emulating(seq);
            }
        }
        Cmd::Stop(t) => {
            if let Some(d) = target(t) {
                d.stop_emulating();
            }
        }
        Cmd::Frame(t) => {
            if let Some(d) = target(t) {
                d.frame(1_000);
            }
        }
        Cmd::Pause(t) => {
            if let Some(d) = target(t) {
                d.paused();
            }
        }
        Cmd::Motion(x, y) => {
            if let Some(p) = pointer.as_ref().and_then(|d| d.interface::<eis::Pointer>()) {
                p.motion_relative(x, y);
            }
        }
        Cmd::Button(code, down) => {
            if let Some(b) = pointer.as_ref().and_then(|d| d.interface::<eis::Button>()) {
                b.button(
                    code,
                    if down {
                        eis::button::ButtonState::Press
                    } else {
                        eis::button::ButtonState::Released
                    },
                );
            }
        }
        Cmd::Key(code, down) => {
            if let Some(k) = keyboard
                .as_ref()
                .and_then(|d| d.interface::<eis::Keyboard>())
            {
                k.key(
                    code,
                    if down {
                        eis::keyboard::KeyState::Press
                    } else {
                        eis::keyboard::KeyState::Released
                    },
                );
            }
        }
        Cmd::Scroll(x, y) => {
            if let Some(s) = pointer.as_ref().and_then(|d| d.interface::<eis::Scroll>()) {
                s.scroll(x, y);
            }
        }
        Cmd::Discrete(x, y) => {
            if let Some(s) = pointer.as_ref().and_then(|d| d.interface::<eis::Scroll>()) {
                s.scroll_discrete(x, y);
            }
        }
        Cmd::ScrollStop { x, y, cancel } => {
            if let Some(s) = pointer.as_ref().and_then(|d| d.interface::<eis::Scroll>()) {
                s.scroll_stop(u32::from(x), u32::from(y), u32::from(cancel));
            }
        }
        Cmd::RemoveDevices => {
            for d in [pointer, keyboard].into_iter().flatten() {
                d.remove();
            }
        }
        Cmd::Disconnect => {
            if let Some(conv) = converter {
                conv.handle()
                    .disconnected(reis::ei::connection::DisconnectReason::Disconnected, None);
            }
        }
    }
}
