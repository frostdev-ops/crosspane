//! Content-free MIME decisions and a transient pipe reader; no payload implements `Debug`.

use std::io::PipeReader;
use std::os::fd::AsFd;
use std::time::Instant;

use crosspane_platform::{ClipKinds, IoGate, PlatformError};
use crosspane_types::ClipKind;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use zeroize::Zeroizing;

use super::{TICK, backend};

pub(crate) const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

pub(crate) fn mime(types: &[String], kind: ClipKind) -> Option<&'static str> {
    let wanted: &[&'static str] = match kind {
        ClipKind::Text => &TEXT,
        ClipKind::Image => &["image/png"],
    };
    wanted
        .iter()
        .copied()
        .find(|mime| types.iter().any(|offered| offered == mime))
}

pub(crate) fn kinds(types: &[String]) -> ClipKinds {
    ClipKinds {
        text: mime(types, ClipKind::Text).is_some(),
        image: mime(types, ClipKind::Image).is_some(),
    }
}

pub(crate) fn own(types: &[String], marker: &str) -> bool {
    types.iter().any(|mime| mime == marker)
}

pub(crate) fn nonblocking(fd: &impl AsFd) -> Result<(), PlatformError> {
    let flags =
        rustix::fs::fcntl_getfl(fd).map_err(|_| backend("could not inspect clipboard pipe"))?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|_| backend("could not configure clipboard pipe"))
}

pub(crate) fn check(gate: &IoGate, epoch: u64, deadline: Instant) -> Result<(), PlatformError> {
    if !gate.is_open() || gate.epoch() != epoch {
        return Err(PlatformError::Locked);
    }
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    Ok(())
}

pub(crate) fn receive(
    reader: PipeReader,
    kind: ClipKind,
    max: usize,
    deadline: Instant,
    gate: &IoGate,
    epoch: u64,
) -> Result<Vec<u8>, PlatformError> {
    let mut data = Zeroizing::new(Vec::new());
    let mut chunk = Zeroizing::new([0_u8; 4096]);
    loop {
        check(gate, epoch, deadline)?;
        let count = chunk
            .len()
            .min(max.saturating_add(1).saturating_sub(data.len()));
        match rustix::io::read(&reader, &mut chunk[..count]) {
            Ok(0) => {
                let valid = kind != ClipKind::Text || std::str::from_utf8(&data).is_ok();
                check(gate, epoch, deadline)?;
                if !valid {
                    return Err(PlatformError::NotFound);
                }
                return Ok(std::mem::take(&mut *data));
            }
            Ok(count) => {
                data.extend_from_slice(&chunk[..count]);
                if data.len() > max {
                    return Err(PlatformError::TooLarge);
                }
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => {
                let wait = TICK.min(deadline.saturating_duration_since(Instant::now()));
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: wait.as_nanos() as i64,
                };
                let mut fds = [PollFd::new(&reader, PollFlags::IN)];
                match poll(&mut fds, Some(&timeout)) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return Err(backend("could not poll clipboard pipe")),
                }
            }
            Err(_) => return Err(backend("could not read clipboard pipe")),
        }
    }
}
