//! Bounded, transient paste descriptors. Payloads move directly into zeroizing writer buffers;
//! none of these types implements Debug, and no payload is cached after answering a paste.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, LocalPasteId};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use zeroize::Zeroizing;

use super::{TICK, read};

const LIMIT: usize = 8;
pub(crate) const LIFETIME: Duration = Duration::from_millis(2500);

#[derive(Clone, Copy)]
struct Lease {
    offer: u64,
    epoch: u64,
    deadline: Instant,
}

impl Lease {
    fn expired(&self, gate: &IoGate, now: Instant) -> bool {
        now >= self.deadline || !gate.is_open() || gate.epoch() != self.epoch
    }
}

struct Pending {
    lease: Lease,
    fd: OwnedFd,
}

struct Writing {
    lease: Lease,
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    pending: HashMap<LocalPasteId, Pending>,
    writing: Vec<Writing>,
    closed: bool,
}

impl Inner {
    fn expire(&mut self, gate: &IoGate, now: Instant) {
        self.pending
            .retain(|_, pending| !pending.lease.expired(gate, now));
        for index in (0..self.writing.len()).rev() {
            if self.writing[index].thread.is_finished() {
                let _ = self.writing.swap_remove(index).thread.join();
            } else if self.writing[index].lease.expired(gate, now) {
                self.writing[index].cancelled.store(true, Ordering::Release);
            }
        }
    }
}

pub(crate) struct Pastes {
    pub(super) gate: Arc<IoGate>,
    inner: Mutex<Inner>,
}

impl Pastes {
    pub(crate) fn new(gate: Arc<IoGate>) -> Arc<Self> {
        Arc::new(Self {
            gate,
            inner: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The lock contains only owned descriptors and thread handles, never a user callback.
        // Even after a panic, recovering it lets cleanup close every descriptor.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn hold(&self, offer: u64, fd: OwnedFd, now: Instant) -> Option<LocalPasteId> {
        let lease = Lease {
            offer,
            epoch: self.gate.epoch(),
            deadline: now + LIFETIME,
        };
        let mut inner = self.lock();
        inner.expire(&self.gate, now);
        if inner.closed
            || inner.pending.len() + inner.writing.len() >= LIMIT
            || lease.expired(&self.gate, now)
            || read::nonblocking(&fd).is_err()
        {
            return None;
        }
        inner.next = inner.next.checked_add(1)?;
        let paste = LocalPasteId(inner.next);
        inner.pending.insert(paste, Pending { lease, fd });
        Some(paste)
    }

    pub(crate) fn fulfil(&self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        let data = data.map(Zeroizing::new);
        let mut inner = self.lock();
        inner.expire(&self.gate, Instant::now());
        let Some(pending) = inner.pending.remove(&paste) else {
            return;
        };
        let Some(data) = data else {
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let (gate, stop, lease) = (self.gate.clone(), cancelled.clone(), pending.lease);
        // Keep the ledger lock until the writer is recorded: moving a pending fd into a
        // writer must not temporarily free a slot and allow a ninth descriptor.
        if let Ok(thread) = std::thread::Builder::new()
            .name("crosspane-clipboard-paste".into())
            .spawn(move || write(pending, data, gate, stop))
        {
            inner.writing.push(Writing {
                lease,
                cancelled,
                thread,
            });
        }
        // Spawn failure drops the closure, closing its fd and zeroizing its buffer.
    }

    pub(crate) fn expire(&self, now: Instant) {
        self.lock().expire(&self.gate, now);
    }

    pub(crate) fn cancel(&self, offer: u64) {
        let mut inner = self.lock();
        inner
            .pending
            .retain(|_, pending| pending.lease.offer != offer);
        for writing in &inner.writing {
            if writing.lease.offer == offer {
                writing.cancelled.store(true, Ordering::Release);
            }
        }
    }

    pub(crate) fn close(&self) {
        let writing = {
            let mut inner = self.lock();
            inner.closed = true;
            inner.pending.clear();
            for writing in &inner.writing {
                writing.cancelled.store(true, Ordering::Release);
            }
            std::mem::take(&mut inner.writing)
        };
        for writing in writing {
            let _ = writing.thread.join();
        }
    }
}

impl Drop for Pastes {
    fn drop(&mut self) {
        self.close();
    }
}

fn write(pending: Pending, data: Zeroizing<Vec<u8>>, gate: Arc<IoGate>, stop: Arc<AtomicBool>) {
    let mut offset = 0;
    while offset < data.len() {
        if stop.load(Ordering::Acquire)
            || read::check(&gate, pending.lease.epoch, pending.lease.deadline).is_err()
        {
            return;
        }
        match rustix::io::write(&pending.fd, &data[offset..]) {
            Ok(0) => return,
            Ok(count) => offset += count,
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => {
                let wait = TICK.min(
                    pending
                        .lease
                        .deadline
                        .saturating_duration_since(Instant::now()),
                );
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: wait.as_nanos() as i64,
                };
                let mut fds = [PollFd::new(&pending.fd, PollFlags::OUT)];
                if let Err(error) = poll(&mut fds, Some(&timeout))
                    && error != rustix::io::Errno::INTR
                {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}
