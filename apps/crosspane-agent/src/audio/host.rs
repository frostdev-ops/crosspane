//! The host thread: the only thread that calls the [`AudioHost`].
//!
//! Every host call is bounded to 2 s by the trait, but 2 s is far too long for anything on the
//! data path, so no other thread ever waits for one. The data thread hands requests to this
//! thread's inbox and picks the replies up on its next pass. The inbox is bounded by the data
//! thread's capacity tables: one outstanding operation per peer (at most 4) plus one outstanding
//! open per session (at most 8).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crosspane_engine::io::AudioKey;
use crosspane_platform::{
    AudioEvent, AudioHost, AudioKind, AudioPlayback, EventSink, PlatformError, VirtualPorts,
};
use crosspane_types::id::NodeId;

use super::{ExitGuard, Shared, lock};

/// How often a waiting host thread re-checks for shutdown even if nobody woke it.
const IDLE_RECHECK: Duration = Duration::from_millis(100);

/// One host call to make.
#[derive(Debug)]
pub(super) enum Request {
    AddPeer { peer: NodeId, name: String },
    RemovePeer { peer: NodeId },
    OpenPlayback { key: AudioKey },
}

/// The outcome of a [`Request`], carrying the full key or peer so a stale one can be recognised.
pub(super) enum Reply {
    PeerAdded {
        peer: NodeId,
        result: Result<VirtualPorts, PlatformError>,
    },
    PeerRemoved {
        peer: NodeId,
        result: Result<(), PlatformError>,
    },
    PlaybackOpened {
        key: AudioKey,
        result: Result<AudioPlayback, PlatformError>,
    },
}

/// The host thread's request queue.
pub(super) struct Inbox {
    queue: Mutex<VecDeque<Request>>,
    wake: Condvar,
    /// Requests the host thread has taken and not finished (zero or one).
    in_flight: AtomicUsize,
}

impl Inbox {
    pub(super) fn new() -> Self {
        Inbox {
            queue: Mutex::new(VecDeque::with_capacity(16)),
            wake: Condvar::new(),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Nothing is queued and nothing is running: every reply that will come is already in the
    /// data thread's queue (tests wait on this).
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        let queue = lock(&self.queue);
        queue.is_empty() && self.in_flight.load(Ordering::SeqCst) == 0
    }

    /// Queue a request. The data thread never queues more than its capacity tables allow.
    pub(super) fn push(&self, request: Request) {
        lock(&self.queue).push_back(request);
        self.wake.notify_one();
    }

    /// Withdraw an open for `key` that the host thread has not started yet. `false` means it is
    /// already in flight (or done), and its reply will arrive and be treated as stale.
    pub(super) fn cancel_open(&self, key: AudioKey) -> bool {
        let mut queue = lock(&self.queue);
        let before = queue.len();
        queue.retain(|request| !matches!(request, Request::OpenPlayback { key: k } if *k == key));
        queue.len() != before
    }

    /// Requests waiting for the host thread (not counting one it is running).
    #[cfg(test)]
    pub(super) fn queued(&self) -> usize {
        lock(&self.queue).len()
    }

    /// Wake the host thread (shutdown). Taking the lock first means a thread that has just seen
    /// "not shut down" is already waiting by the time this notifies.
    pub(super) fn wake(&self) {
        drop(lock(&self.queue));
        self.wake.notify_all();
    }

    /// Wait for the next request, or `None` once shutdown is requested.
    fn next(&self, shared: &Shared) -> Option<Request> {
        let mut queue = lock(&self.queue);
        loop {
            if shared.is_shutdown() {
                return None;
            }
            if let Some(request) = queue.pop_front() {
                // Counted before the lock is released, so `is_idle` never sees a gap.
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                return Some(request);
            }
            queue = self
                .wake
                .wait_timeout(queue, IDLE_RECHECK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// The host thread's body. `ready` carries the outcome of `subscribe`; on failure the thread ends.
pub(super) fn run(
    mut host: Box<dyn AudioHost>,
    shared: Arc<Shared>,
    sink: Arc<dyn EventSink<AudioEvent>>,
    ready: SyncSender<Result<(), String>>,
) {
    // First, so that it is dropped before `host` (see `ExitGuard`): a death is reported before
    // whatever the host's destructor may take.
    let _guard = ExitGuard(Arc::clone(&shared));
    let subscribed = host
        .subscribe(sink)
        .map_err(|error| format!("audio host events are unavailable: {error}"));
    let usable = subscribed.is_ok();
    // `start` may have given up waiting; nothing to do about that here.
    let _ = ready.send(subscribed);
    if !usable {
        return;
    }
    while let Some(request) = shared.host.next(&shared) {
        let reply = match request {
            Request::AddPeer { peer, name } => Reply::PeerAdded {
                peer,
                result: host.add_peer(peer, &name),
            },
            Request::RemovePeer { peer } => Reply::PeerRemoved {
                peer,
                result: host.remove_peer(peer),
            },
            Request::OpenPlayback { key } => Reply::PlaybackOpened {
                key,
                result: host.open_playback(AudioKind::Speaker.format()),
            },
        };
        // Publishing and shutdown are ordered by the reply queue's lock: a reply is either seen
        // by the data thread or handed back here, never left in a queue nobody will clean. A
        // handle that opened late is dropped (stopped) right here.
        if shared.publish_reply(reply).is_err() {
            break;
        }
        // After the publish, so `is_idle` implies the reply is visible to the data thread.
        shared.host.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    // `host` drops here, and with it whatever the backend still holds.
}
