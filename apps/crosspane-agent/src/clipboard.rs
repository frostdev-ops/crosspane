//! One serialized owner of ClipboardHost. No host mutex crosses a network wait, and bytes
//! never enter logs. Withdrawal waits only for the native call already executing. At most
//! 64 jobs wait (16 cleanup slots reserved); queued and in-flight fulfilments share 16 MiB.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_engine::{Input, Output, io::ClipBytes};
use crosspane_platform::{ClipboardEvent, ClipboardHost, PlatformError};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{ClipFailure, ControlMessage};
use crosspane_types::id::NodeId;
use zeroize::Zeroize;

use crate::agent::Event;

const START_WAIT: Duration = Duration::from_secs(4);
const STOP_WAIT: Duration = Duration::from_millis(3500);
const FETCH_WAIT: Duration = Duration::from_secs(2);
const JOB_LIMIT: usize = 64;
const CLEANUP_RESERVE: usize = 16;
const BYTE_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) fn failure(error: &PlatformError) -> ClipFailure {
    match error {
        PlatformError::Locked => ClipFailure::Locked,
        PlatformError::TooLarge => ClipFailure::TooLarge,
        _ => ClipFailure::Unavailable,
    }
}

struct Job {
    output: Output,
    epoch: u64,
    until: Instant,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    epochs: HashMap<NodeId, u64>,
    stopped: bool,
    buffered: usize,
    owned: Option<u64>,
    retiring: Option<u64>,
    withdrawing: Option<u64>,
}

fn bytes(output: &Output) -> usize {
    match output {
        Output::ClipFulfil {
            data: Some(data), ..
        } => data.0.len(),
        _ => 0,
    }
}

fn urgent(output: &Output) -> bool {
    matches!(
        output,
        Output::ClipWithdraw { .. } | Output::ClipFulfil { data: None, .. }
    )
}

impl Queue {
    fn discard(&mut self, matches: impl Fn(&Output) -> bool) -> Vec<Input> {
        let mut completed = Vec::new();
        let mut removed = 0;
        self.jobs.retain_mut(|job| {
            if !matches(&job.output) {
                return true;
            }
            removed += bytes(&job.output);
            match &mut job.output {
                Output::ClipFulfil {
                    data: Some(data), ..
                } => data.0.zeroize(),
                Output::ClipPromise { offer, .. } => {
                    completed.push(Input::Clipboard(ClipboardEvent::PromiseLost {
                        offer: *offer,
                    }))
                }
                _ => {}
            }
            false
        });
        self.buffered -= removed;
        completed
    }

    fn push(&mut self, output: Output, epoch: u64) {
        self.buffered += bytes(&output);
        let position = if matches!(&output, Output::ClipWithdraw { .. }) {
            0
        } else if urgent(&output) {
            self.jobs
                .iter()
                .position(|j| !urgent(&j.output))
                .unwrap_or(self.jobs.len())
        } else {
            self.jobs.len()
        };
        self.jobs.insert(
            position,
            Job {
                output,
                epoch,
                until: Instant::now() + FETCH_WAIT,
            },
        );
    }

    fn retire(&mut self, completed: &mut Vec<Input>) {
        if let Some(offer) = self.owned
            && self.retiring.is_none()
        {
            // Withdrawal cancels every not-yet-supplied native paste for this generation.
            completed.extend(self.discard(|output| {
                matches!(
                    output,
                    Output::ClipPromise { .. }
                        | Output::ClipFulfil { .. }
                        | Output::ClipWithdraw { .. }
                )
            }));
            self.retiring = Some(offer);
            if self.withdrawing != Some(offer) {
                self.push(Output::ClipWithdraw { offer }, 0);
            }
            completed.push(Input::Clipboard(ClipboardEvent::PromiseLost { offer }));
        }
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    counts: Counts,
}

#[derive(Default)]
struct Counts([AtomicU64; 9]);

impl Counts {
    fn bump(&self, index: usize) {
        let _ = self.0[index].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(1))
        });
    }
    fn failed(&self, reason: ClipFailure) {
        self.bump(match reason {
            ClipFailure::Expired => 4,
            ClipFailure::Locked => 5,
            ClipFailure::NotGranted => 6,
            ClipFailure::TooLarge => 7,
            ClipFailure::Unavailable => 8,
        });
    }
    fn status(&self) -> serde_json::Value {
        let count = |i: usize| self.0[i].load(Ordering::Relaxed);
        serde_json::json!({
            "offers_sent":count(0), "offers_received":count(1),
            "fetches_served":count(2), "fetches_made":count(3),
            "expired":count(4), "locked":count(5), "not_granted":count(6),
            "too_large":count(7), "unavailable":count(8),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct Worker {
    shared: Arc<Shared>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn status(&self) -> serde_json::Value {
        self.shared.counts.status()
    }

    pub(crate) fn failed(&self, reason: ClipFailure) {
        self.shared.counts.failed(reason);
    }

    pub(crate) fn record_input(&self, input: &Input) {
        if let Input::Link(LinkEvent::Control { msg, .. }) = input {
            match msg {
                ControlMessage::ClipOffer(_) => self.shared.counts.bump(1),
                ControlMessage::ClipFetchFailed(failed) => self.failed(failed.reason),
                _ => {}
            }
        }
    }

    pub(crate) fn record_output(&self, output: &Output) {
        match output {
            Output::SendControl { msg, .. } => match msg {
                ControlMessage::ClipOffer(_) => self.shared.counts.bump(0),
                ControlMessage::ClipFetch(_) => self.shared.counts.bump(3),
                ControlMessage::ClipFetchFailed(failed) => self.failed(failed.reason),
                _ => {}
            },
            Output::SendClipData { .. } => self.shared.counts.bump(2),
            _ => {}
        }
    }

    pub(crate) fn start(
        mut host: Box<dyn ClipboardHost>,
        events: mpsc::Sender<Event>,
    ) -> Result<Self, ClipFailure> {
        let shared = Arc::new(Shared::default());
        let (ready, admitted) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::channel();
        let owner = shared.clone();
        let thread = std::thread::Builder::new()
            .name("clipboard-worker".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let sink = events.clone();
                    let result = host
                        .subscribe(Arc::new(move |event| {
                            let _ = sink.send(Event::Input(Input::Clipboard(event)));
                        }))
                        .and_then(|()| host.kinds())
                        .map_err(|e| failure(&e));
                    let Ok(kinds) = result else {
                        let _ = ready.send(result.map(|_| ()));
                        return;
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    let _ = events.send(Event::Input(Input::Clipboard(ClipboardEvent::Changed {
                        kinds,
                    })));
                    run(&mut *host, &owner, &events);
                }));
                if result.is_err() {
                    let _ = events.send(Event::Shutdown);
                }
                drop(host);
                let _ = finished.send(());
            })
            .map_err(|_| ClipFailure::Unavailable)?;
        if let Err(reason) = admitted
            .recv_timeout(START_WAIT)
            .unwrap_or(Err(ClipFailure::Unavailable))
        {
            lock(&shared.queue).stopped = true;
            shared.wake.notify_one();
            return Err(reason);
        }
        Ok(Self {
            shared,
            done,
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, mut output: Output) -> Vec<Input> {
        let mut queue = lock(&self.shared.queue);
        let mut completed = Vec::new();
        let epoch = match &output {
            Output::ClipRead { peer, .. } => *queue.epochs.get(peer).unwrap_or(&0),
            _ => 0,
        };
        if let Output::ClipWithdraw { offer } = &output {
            // Cancellation remains possible until claim. Never install an already-retired offer.
            completed.extend(queue.discard(|output| matches!(output, Output::ClipPromise { offer: pending, .. } if pending == offer)));
            if queue.owned != Some(*offer) || queue.withdrawing == Some(*offer) || queue.jobs.iter().any(|job| matches!(job.output, Output::ClipWithdraw { offer: pending } if pending == *offer)) {
                return completed;
            }
        }
        if let Output::ClipFulfil { paste, data } = &mut output {
            let empty = queue.jobs.iter().any(|job| matches!(&job.output, Output::ClipFulfil { paste: pending, data: None } if pending == paste));
            completed.extend(queue.discard(|output| matches!(output, Output::ClipFulfil { paste: pending, .. } if pending == paste)));
            if (empty
                || queue.retiring.is_some()
                || queue.stopped
                || queue.jobs.len() >= JOB_LIMIT - CLEANUP_RESERVE
                || data
                    .as_ref()
                    .is_some_and(|d| d.0.len() > BYTE_LIMIT - queue.buffered))
                && let Some(mut rejected) = data.take()
            {
                self.failed(ClipFailure::Unavailable);
                rejected.0.zeroize();
            }
            // Once retirement is queued, native withdrawal will empty all outstanding pastes.
            if queue.retiring.is_some() {
                return completed;
            }
        }
        let limit = if urgent(&output) {
            JOB_LIMIT
        } else {
            JOB_LIMIT - CLEANUP_RESERVE
        };
        if queue.stopped
            || queue.jobs.len() >= limit
            || (queue.retiring.is_some() && matches!(output, Output::ClipPromise { .. }))
        {
            match output {
                Output::ClipRead { peer, fetch, .. } => {
                    self.failed(ClipFailure::Unavailable);
                    completed.push(Input::ClipReadDone {
                        peer,
                        fetch,
                        result: Err(ClipFailure::Unavailable),
                    });
                }
                Output::ClipPromise { offer, .. } => {
                    self.failed(ClipFailure::Unavailable);
                    completed.push(Input::Clipboard(ClipboardEvent::PromiseLost { offer }))
                }
                Output::ClipWithdraw { .. } | Output::ClipFulfil { .. } => {
                    self.failed(ClipFailure::Unavailable);
                    queue.retire(&mut completed)
                }
                _ => {}
            }
        } else {
            queue.push(output, epoch);
        }
        self.shared.wake.notify_one();
        completed
    }

    pub(crate) fn cancel_peer(&self, peer: NodeId) {
        let mut queue = lock(&self.shared.queue);
        let epoch = queue.epochs.entry(peer).or_default();
        *epoch = epoch.saturating_add(1);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let mut queue = lock(&self.shared.queue);
        queue.stopped = true;
        queue.discard(|_| true);
        drop(queue);
        self.shared.wake.notify_one();
        if self.done.recv_timeout(STOP_WAIT).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

fn valid(shared: &Shared, peer: NodeId, epoch: u64, until: Instant) -> bool {
    let queue = lock(&shared.queue);
    !queue.stopped
        && epoch != u64::MAX
        && queue.epochs.get(&peer).copied().unwrap_or(0) == epoch
        && Instant::now() < until
}

fn run(host: &mut dyn ClipboardHost, shared: &Shared, events: &mpsc::Sender<Event>) {
    loop {
        let job = {
            let mut queue = lock(&shared.queue);
            while queue.jobs.is_empty() && !queue.stopped {
                queue = shared
                    .wake
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if queue.stopped {
                break;
            }
            let job = queue.jobs.pop_front();
            if let Some(Job {
                output: Output::ClipPromise { offer, .. },
                ..
            }) = &job
            {
                // Claim and ownership identity are atomic with producer-side cancellation.
                queue.owned = Some(*offer);
            }
            if let Some(Job {
                output: Output::ClipWithdraw { offer },
                ..
            }) = &job
            {
                queue.withdrawing = Some(*offer);
            }
            job
        };
        let Some(job) = job else { continue };
        let supplied = bytes(&job.output);
        match job.output {
            Output::ClipRead {
                peer,
                fetch,
                kind,
                max_bytes,
            } => {
                let mut result = if valid(shared, peer, job.epoch, job.until) {
                    host.read(kind, max_bytes)
                        .map(ClipBytes)
                        .map_err(|e| failure(&e))
                } else {
                    Err(ClipFailure::Unavailable)
                };
                if !valid(shared, peer, job.epoch, job.until) {
                    if let Ok(bytes) = &mut result {
                        bytes.0.zeroize();
                    }
                    result = Err(ClipFailure::Unavailable);
                }
                let _ = events.send(Event::Input(Input::ClipReadDone {
                    peer,
                    fetch,
                    result,
                }));
            }
            Output::ClipPromise { offer, kinds } => {
                if let Err(error) = host.promise(offer, kinds) {
                    shared.counts.failed(failure(&error));
                    tracing::info!(offer, text = kinds.text, image = kinds.image,
                        reason = ?failure(&error), "clipboard promise failed");
                    let _ = events.send(Event::Input(Input::Clipboard(
                        ClipboardEvent::PromiseLost { offer },
                    )));
                }
            }
            Output::ClipWithdraw { offer } => {
                let result = host.withdraw(offer);
                if let Err(error) = &result {
                    shared.counts.failed(failure(error));
                    tracing::info!(offer, reason = ?failure(error), "clipboard withdrawal failed");
                }
                let mut queue = lock(&shared.queue);
                queue.withdrawing = None;
                if result.is_ok() {
                    if queue.owned == Some(offer) {
                        queue.owned = None;
                    }
                    if queue.retiring == Some(offer) {
                        queue.retiring = None;
                    }
                }
            }
            Output::ClipFulfil { paste, data } => host.fulfil(paste, data.map(|d| d.0)),
            _ => {}
        }
        lock(&shared.queue).buffered -= supplied;
    }
    let offer = lock(&shared.queue).owned;
    if let Some(offer) = offer
        && let Err(error) = host.withdraw(offer)
    {
        shared.counts.failed(failure(&error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_platform::{ClipKinds, EventSink, LocalPasteId};
    use crosspane_protocol::msg::ClipFetchId;
    use crosspane_types::ClipKind;

    const READ_BOUND: Duration = Duration::from_millis(200);

    #[test]
    fn clipboard_counters_saturate_and_expose_only_fixed_numeric_fields() {
        let counts = Counts::default();
        counts.0[0].store(u64::MAX, Ordering::Relaxed);
        counts.bump(0);
        let status = counts.status();
        assert_eq!(status["offers_sent"], u64::MAX);
        assert_eq!(status.as_object().unwrap().len(), 9);
        assert!(
            status
                .as_object()
                .unwrap()
                .values()
                .all(serde_json::Value::is_u64)
        );
    }

    struct Failing;

    impl ClipboardHost for Failing {
        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<ClipboardEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn kinds(&self) -> Result<ClipKinds, PlatformError> {
            Ok(ClipKinds::default())
        }
        fn read(&mut self, _: ClipKind, _: usize) -> Result<Vec<u8>, PlatformError> {
            Err(PlatformError::NotFound)
        }
        fn promise(&mut self, _: u64, _: ClipKinds) -> Result<(), PlatformError> {
            Err(PlatformError::Locked)
        }
        fn fulfil(&mut self, _: LocalPasteId, _: Option<Vec<u8>>) {}
        fn withdraw(&mut self, _: u64) -> Result<(), PlatformError> {
            Err(PlatformError::TooLarge)
        }
    }

    #[test]
    fn clipboard_local_promise_and_withdraw_failures_use_mapped_counter_kinds() {
        let (events, inputs) = mpsc::channel();
        let worker = Worker::start(Box::new(Failing), events).unwrap();
        worker.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        loop {
            if let Event::Input(Input::Clipboard(ClipboardEvent::PromiseLost { offer: 1 })) =
                inputs.recv_timeout(Duration::from_secs(1)).unwrap()
            {
                break;
            }
        }
        assert_eq!(worker.status()["locked"], 1);
        worker.submit(Output::ClipWithdraw { offer: 1 });
        let until = Instant::now() + Duration::from_secs(1);
        while worker.status()["too_large"] != 1 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert_eq!(worker.status()["unavailable"], 0);
    }

    #[test]
    fn clipboard_queue_read_and_promise_failures_count_explicit_rejections() {
        let f = Fixture::new();
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for fetch in 1..=49 {
            f.read(fetch);
        }
        f.submit(Output::ClipPromise {
            offer: 2,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(f.worker.status()["unavailable"], 2);
        assert!(lock(&f.completed).iter().any(|input| matches!(
            input,
            Input::Clipboard(ClipboardEvent::PromiseLost { offer: 2 })
        )));
    }

    #[test]
    fn clipboard_queue_payload_budget_failure_has_no_inferred_empty_answer_reason() {
        let f = Fixture::new();
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for paste in 1..=2 {
            f.submit(Output::ClipFulfil {
                paste: LocalPasteId(paste),
                data: Some(ClipBytes(vec![1; BYTE_LIMIT])),
            });
        }
        assert_eq!(f.worker.status()["unavailable"], 1);
        f.submit(Output::ClipFulfil {
            paste: LocalPasteId(3),
            data: None,
        });
        assert_eq!(f.worker.status()["unavailable"], 1);
    }

    struct Paused {
        calls: mpsc::Sender<Call>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Paused {
        fn wait(&self, call: Call) {
            self.calls.send(call).unwrap();
            let (mutex, wake) = &*self.release;
            let mut released = lock(mutex);
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
    }

    impl ClipboardHost for Paused {
        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<ClipboardEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn kinds(&self) -> Result<ClipKinds, PlatformError> {
            Ok(ClipKinds::default())
        }
        fn read(&mut self, _: ClipKind, _: usize) -> Result<Vec<u8>, PlatformError> {
            self.wait(Call::Read);
            Ok(vec![1])
        }
        fn promise(&mut self, offer: u64, _: ClipKinds) -> Result<(), PlatformError> {
            self.calls.send(Call::Promise(offer)).unwrap();
            Ok(())
        }
        fn fulfil(&mut self, _: LocalPasteId, data: Option<Vec<u8>>) {
            self.wait(Call::Fulfil(data.is_some()));
        }
        fn withdraw(&mut self, _: u64) -> Result<(), PlatformError> {
            self.wait(Call::Withdraw);
            Ok(())
        }
    }

    struct Fixture {
        worker: Worker,
        calls: mpsc::Receiver<Call>,
        inputs: mpsc::Receiver<Event>,
        release: Arc<(Mutex<bool>, Condvar)>,
        completed: Mutex<Vec<Input>>,
    }

    impl Fixture {
        fn new() -> Self {
            let (calls, observed) = mpsc::channel();
            let (events, inputs) = mpsc::channel();
            let release = Arc::new((Mutex::new(false), Condvar::new()));
            let worker = Worker::start(
                Box::new(Paused {
                    calls,
                    release: release.clone(),
                }),
                events,
            )
            .unwrap();
            Self {
                worker,
                calls: observed,
                inputs,
                release,
                completed: Mutex::new(Vec::new()),
            }
        }
        fn submit(&self, output: Output) {
            lock(&self.completed).extend(self.worker.submit(output));
        }
        fn next(&self) -> Call {
            self.calls.recv_timeout(Duration::from_secs(1)).unwrap()
        }
        fn read(&self, fetch: u64) {
            self.submit(Output::ClipRead {
                peer: NodeId([1; 32]),
                fetch: ClipFetchId(fetch),
                kind: ClipKind::Text,
                max_bytes: 100,
            });
        }
        fn unblock(&self) {
            *lock(&self.release.0) = true;
            self.release.1.notify_all();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.unblock();
        }
    }

    #[test]
    fn saturated_read_jobs_remain_bounded_while_native_read_is_blocked() {
        let f = Fixture::new();
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for fetch in 1..=1000 {
            f.read(fetch);
        }
        assert!(lock(&f.worker.shared.queue).jobs.len() <= 64);
        let rejected: Vec<_> = lock(&f.completed)
            .iter()
            .filter_map(|input| match input {
                Input::ClipReadDone { fetch, result, .. } => {
                    assert_eq!(result, &Err(ClipFailure::Unavailable));
                    Some(fetch.0)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rejected, (49..=1000).collect::<Vec<_>>());
        f.worker.cancel_peer(NodeId([1; 32]));
        f.unblock();
        let mut admitted = Vec::new();
        while admitted.len() < 49 {
            if let Event::Input(Input::ClipReadDone { fetch, result, .. }) =
                f.inputs.recv_timeout(Duration::from_secs(1)).unwrap()
            {
                assert_eq!(result, Err(ClipFailure::Unavailable));
                admitted.push(fetch.0);
            }
        }
        assert_eq!(admitted, (0..=48).collect::<Vec<_>>());
        assert!(f.calls.try_recv().is_err());
    }

    #[test]
    fn burst_supersessions_coalesce_retired_jobs_and_withdraw_after_one_read() {
        let f = Fixture::new();
        f.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(f.next(), Call::Promise(1));
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for offer in 2..=1001 {
            f.submit(Output::ClipPromise {
                offer,
                kinds: ClipKinds {
                    text: true,
                    image: false,
                },
            });
            f.submit(Output::ClipWithdraw { offer });
        }
        f.submit(Output::ClipWithdraw { offer: 1 });
        assert!(lock(&f.worker.shared.queue).jobs.len() <= 64);
        f.unblock();
        assert_eq!(f.next(), Call::Withdraw);
        assert!(f.calls.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn queued_fulfilments_share_the_16_mib_budget() {
        let f = Fixture::new();
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for paste in 1..=2 {
            f.submit(Output::ClipFulfil {
                paste: LocalPasteId(paste),
                data: Some(ClipBytes(vec![1; 16 * 1024 * 1024])),
            });
        }
        let bytes: usize = lock(&f.worker.shared.queue)
            .jobs
            .iter()
            .map(|job| match &job.output {
                Output::ClipFulfil {
                    data: Some(data), ..
                } => data.0.len(),
                _ => 0,
            })
            .sum();
        assert!(bytes <= 16 * 1024 * 1024);
    }

    #[test]
    fn in_flight_fulfilment_still_consumes_the_shared_byte_budget() {
        let f = Fixture::new();
        f.submit(Output::ClipFulfil {
            paste: LocalPasteId(1),
            data: Some(ClipBytes(vec![1; 16 * 1024 * 1024])),
        });
        assert_eq!(f.next(), Call::Fulfil(true));
        f.submit(Output::ClipFulfil {
            paste: LocalPasteId(2),
            data: Some(ClipBytes(vec![1; 16 * 1024 * 1024])),
        });
        assert!(
            lock(&f.worker.shared.queue)
                .jobs
                .iter()
                .all(|job| !matches!(&job.output, Output::ClipFulfil { data: Some(_), .. }))
        );
        f.unblock();
        assert_eq!(f.next(), Call::Fulfil(false));
    }

    #[test]
    fn saturated_cleanup_retires_owned_generation_with_one_native_withdraw() {
        let f = Fixture::new();
        f.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(f.next(), Call::Promise(1));
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for paste in 1..=1000 {
            f.submit(Output::ClipFulfil {
                paste: LocalPasteId(paste),
                data: None,
            });
        }
        assert!(lock(&f.worker.shared.queue).jobs.len() <= 64);
        f.unblock();
        assert_eq!(f.next(), Call::Withdraw);
        assert!(f.calls.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(
            lock(&f.completed)
                .iter()
                .filter(|input| matches!(
                    input,
                    Input::Clipboard(ClipboardEvent::PromiseLost { offer: 1 })
                ))
                .count(),
            1
        );
    }

    #[test]
    fn reserved_cleanup_survives_normal_saturation_and_repeated_withdrawals() {
        let f = Fixture::new();
        f.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(f.next(), Call::Promise(1));
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for fetch in 1..=48 {
            f.read(fetch);
        }
        f.submit(Output::ClipPromise {
            offer: 2,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert!(lock(&f.completed).iter().any(|input| matches!(
            input,
            Input::Clipboard(ClipboardEvent::PromiseLost { offer: 2 })
        )));
        f.submit(Output::ClipFulfil {
            paste: LocalPasteId(1),
            data: None,
        });
        for _ in 0..1000 {
            f.submit(Output::ClipWithdraw { offer: 1 });
        }
        assert_eq!(lock(&f.worker.shared.queue).jobs.len(), 50);
        f.worker.cancel_peer(NodeId([1; 32]));
        f.unblock();
        assert_eq!(f.next(), Call::Withdraw);
        assert_eq!(f.next(), Call::Fulfil(false));
        assert!(f.calls.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn duplicate_fulfilment_coalesces_without_reviving_a_queued_empty_answer() {
        let f = Fixture::new();
        f.read(0);
        assert_eq!(f.next(), Call::Read);
        for data in [
            Some(ClipBytes(vec![1; 1024])),
            None,
            Some(ClipBytes(vec![1; 1024])),
        ] {
            f.submit(Output::ClipFulfil {
                paste: LocalPasteId(1),
                data,
            });
        }
        assert_eq!(lock(&f.worker.shared.queue).jobs.len(), 1);
        assert_eq!(lock(&f.worker.shared.queue).buffered, 0);
        f.unblock();
        assert_eq!(f.next(), Call::Fulfil(false));
        assert!(f.calls.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn claimed_withdrawal_also_coalesces_repeated_withdrawals_and_cleanup_overflow() {
        let f = Fixture::new();
        f.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(f.next(), Call::Promise(1));
        f.submit(Output::ClipWithdraw { offer: 1 });
        assert_eq!(f.next(), Call::Withdraw);
        for _ in 0..1000 {
            f.submit(Output::ClipWithdraw { offer: 1 });
        }
        assert!(lock(&f.worker.shared.queue).jobs.is_empty());
        for paste in 1..=1000 {
            f.submit(Output::ClipFulfil {
                paste: LocalPasteId(paste),
                data: None,
            });
        }
        assert!(lock(&f.worker.shared.queue).jobs.is_empty());
        f.unblock();
        assert!(f.calls.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(
            lock(&f.completed)
                .iter()
                .filter(|input| matches!(
                    input,
                    Input::Clipboard(ClipboardEvent::PromiseLost { offer: 1 })
                ))
                .count(),
            1
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Read,
        Withdraw,
        Promise(u64),
        Fulfil(bool),
    }

    struct Blocked {
        calls: mpsc::Sender<Call>,
    }

    impl ClipboardHost for Blocked {
        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<ClipboardEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn kinds(&self) -> Result<ClipKinds, PlatformError> {
            Ok(ClipKinds::default())
        }
        fn read(&mut self, _: ClipKind, _: usize) -> Result<Vec<u8>, PlatformError> {
            self.calls.send(Call::Read).unwrap();
            std::thread::sleep(READ_BOUND);
            Ok(b"discarded fake fixture".to_vec())
        }
        fn promise(&mut self, offer: u64, _: ClipKinds) -> Result<(), PlatformError> {
            self.calls.send(Call::Promise(offer)).unwrap();
            Ok(())
        }
        fn fulfil(&mut self, _: LocalPasteId, data: Option<Vec<u8>>) {
            self.calls.send(Call::Fulfil(data.is_some())).unwrap();
        }
        fn withdraw(&mut self, _: u64) -> Result<(), PlatformError> {
            self.calls.send(Call::Withdraw).unwrap();
            Ok(())
        }
    }

    #[test]
    fn blocked_read_withdraw_waits_only_one_native_read_with_a_deep_queue() {
        let (calls, observed) = mpsc::channel();
        let (events, _inputs) = mpsc::channel();
        let worker = Worker::start(Box::new(Blocked { calls }), events).unwrap();
        let _ = worker.submit(Output::ClipPromise {
            offer: 1,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            Call::Promise(1)
        );
        let read = |i| Output::ClipRead {
            peer: NodeId([i; 32]),
            fetch: ClipFetchId(1),
            kind: ClipKind::Text,
            max_bytes: 1024,
        };
        let _ = worker.submit(read(1));
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            Call::Read
        );
        let started = Instant::now();
        for i in 2..=9 {
            let _ = worker.submit(read(i));
        }
        let _ = worker.submit(Output::ClipWithdraw { offer: 1 });
        assert_eq!(
            observed
                .recv_timeout(READ_BOUND + Duration::from_millis(100))
                .unwrap(),
            Call::Withdraw
        );
        assert!(started.elapsed() <= READ_BOUND + Duration::from_millis(100));
    }

    #[test]
    fn empty_fulfil_and_withdraw_retire_queued_install_before_the_next_read() {
        let (calls, observed) = mpsc::channel();
        let (events, _inputs) = mpsc::channel();
        let worker = Worker::start(Box::new(Blocked { calls }), events).unwrap();
        let _ = worker.submit(Output::ClipRead {
            peer: NodeId([1; 32]),
            fetch: ClipFetchId(1),
            kind: ClipKind::Text,
            max_bytes: 100,
        });
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            Call::Read
        );
        let _ = worker.submit(Output::ClipPromise {
            offer: 2,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        });
        let _ = worker.submit(Output::ClipRead {
            peer: NodeId([2; 32]),
            fetch: ClipFetchId(2),
            kind: ClipKind::Text,
            max_bytes: 100,
        });
        let _ = worker.submit(Output::ClipFulfil {
            paste: LocalPasteId(3),
            data: None,
        });
        let _ = worker.submit(Output::ClipWithdraw { offer: 2 });
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            Call::Fulfil(false)
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            Call::Read
        );
        drop(worker);
        assert!(
            !observed
                .try_iter()
                .any(|call| matches!(call, Call::Promise(2)))
        );
    }

    #[test]
    fn invalidated_inflight_and_queued_reads_complete_serves_without_more_native_io() {
        let (calls, observed) = mpsc::channel();
        let (events, inputs) = mpsc::channel();
        let worker = Worker::start(Box::new(Blocked { calls }), events).unwrap();
        let peer = NodeId([3; 32]);
        for fetch in 1..=2 {
            let _ = worker.submit(Output::ClipRead {
                peer,
                fetch: ClipFetchId(fetch),
                kind: ClipKind::Text,
                max_bytes: 100,
            });
            if fetch == 1 {
                assert_eq!(
                    observed.recv_timeout(Duration::from_secs(1)).unwrap(),
                    Call::Read
                );
            }
        }
        worker.cancel_peer(peer);
        let mut done = Vec::new();
        while done.len() != 2 {
            if let Event::Input(Input::ClipReadDone { fetch, result, .. }) =
                inputs.recv_timeout(Duration::from_secs(1)).unwrap()
            {
                assert_eq!(result, Err(ClipFailure::Unavailable));
                done.push(fetch.0);
            }
        }
        assert_eq!(done, [1, 2]);
        assert!(observed.try_recv().is_err());
    }

    #[test]
    fn platform_failures_have_exact_frozen_mapping_without_formatting_backend_text() {
        let errors = [
            PlatformError::Locked,
            PlatformError::TooLarge,
            PlatformError::NotFound,
            PlatformError::Timeout,
            PlatformError::Backend("opaque".into()),
            PlatformError::PermissionDenied(crosspane_platform::Permission::Accessibility),
            PlatformError::SecureInput,
            PlatformError::PointerButtonHeld,
            PlatformError::InteractionRequired,
            PlatformError::Unsupported("fake"),
        ];
        for error in errors {
            let expected = match &error {
                PlatformError::Locked => ClipFailure::Locked,
                PlatformError::TooLarge => ClipFailure::TooLarge,
                _ => ClipFailure::Unavailable,
            };
            assert_eq!(failure(&error), expected);
        }
    }
}
