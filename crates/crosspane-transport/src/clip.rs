//! Clipboard streams are expected, bounded and cancelled independently of input/control/media.
//! Each physical connection holds at most eight expectations and two tasks/16 MiB per direction.
//! Expectation and transfer deadlines are two seconds; cancellation retires queued completions too.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_protocol::clip::{
    CLIP_DATA_HEADER_LEN, ClipDataBytes, ClipDataHeader, MAX_CLIP_IMAGE, decode_clip_data_header,
    encode_clip_data_header,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::ClipFetchId;
use crosspane_types::{ClipKind, id::NodeId};
use quinn::{Connection, RecvStream, VarInt};
use tokio::sync::watch;
use tokio::time::timeout_at;

use crate::hub::lock;

pub(crate) const STREAM_CLIP: u8 = 0x04;
pub(crate) const PRIORITY_CLIP: i32 = 0;
pub(crate) const MAX_TASKS: usize = 2;
const MAX_BYTES: usize = MAX_CLIP_IMAGE as usize;
const MAX_EXPECTED: usize = 8;
const DEADLINE: Duration = Duration::from_secs(2);
const RESET: VarInt = VarInt::from_u32(4);
pub(crate) const UNAVAILABLE: &str = "clipboard is unavailable";

#[derive(Default)]
struct Usage {
    tasks: usize,
    bytes: usize,
}

#[derive(Default)]
struct State {
    expected: HashMap<ClipFetchId, (ClipKind, Instant)>,
    usage: [Usage; 2],
}

/// State belongs to one physical connection. Replacement never transfers expectations.
pub(crate) struct Plane {
    pub(crate) enabled: AtomicBool,
    retired: AtomicBool,
    state: Mutex<State>,
    cancelled: watch::Sender<()>,
    #[cfg(test)]
    stopped_registrations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    cancel_after_finish: AtomicBool,
    #[cfg(test)]
    before_publish: Mutex<Option<Arc<std::sync::Barrier>>>,
}

impl Plane {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            cancelled: watch::channel(()).0,
            #[cfg(test)]
            stopped_registrations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            cancel_after_finish: AtomicBool::new(false),
            #[cfg(test)]
            before_publish: Mutex::new(None),
        })
    }

    pub(crate) fn available(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && !self.retired.load(Ordering::Acquire)
    }

    pub(crate) fn expect(&self, fetch: ClipFetchId, kind: ClipKind) -> Result<(), LinkError> {
        let mut state = lock(&self.state);
        if !self.available() {
            return Err(LinkError::Invalid(UNAVAILABLE));
        }
        let now = Instant::now();
        state.expected.retain(|_, (_, until)| *until > now);
        if state.expected.contains_key(&fetch) {
            return Err(LinkError::Invalid("duplicate clipboard expectation"));
        }
        if state.expected.len() >= MAX_EXPECTED {
            return Err(LinkError::Congested);
        }
        state.expected.insert(fetch, (kind, now + DEADLINE));
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        let mut state = lock(&self.state);
        state.expected.clear();
        self.cancelled.send_replace(());
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.cancel();
    }

    fn reserve(self: &Arc<Self>, direction: usize, bytes: usize) -> Result<Work, LinkError> {
        let mut state = lock(&self.state);
        if !self.available() {
            return Err(LinkError::Invalid(UNAVAILABLE));
        }
        let usage = &mut state.usage[direction];
        if usage.tasks >= MAX_TASKS || bytes > MAX_BYTES - usage.bytes {
            return Err(LinkError::Congested);
        }
        usage.tasks += 1;
        usage.bytes += bytes;
        Ok(Work {
            plane: self.clone(),
            direction,
            bytes,
            cancelled: self.cancelled.subscribe(),
            deadline: Instant::now() + DEADLINE,
        })
    }

    pub(crate) fn sending(
        self: &Arc<Self>,
        header: ClipDataHeader,
        len: usize,
    ) -> Result<Work, LinkError> {
        if header.len as usize != len || encode_clip_data_header(header).is_err() {
            return Err(LinkError::Invalid("invalid clipboard length"));
        }
        self.reserve(1, len)
    }
}

/// Holds task/byte credit until completion, reset, timeout or cancellation.
pub(crate) struct Work {
    plane: Arc<Plane>,
    direction: usize,
    bytes: usize,
    cancelled: watch::Receiver<()>,
    deadline: Instant,
}

impl Work {
    fn valid(&self) -> bool {
        self.plane.available() && !self.cancelled.has_changed().unwrap_or(true)
    }

    fn admit(&mut self, header: ClipDataHeader) -> Option<Instant> {
        let mut state = lock(&self.plane.state);
        let (kind, deadline) = *state.expected.get(&header.fetch)?;
        let usage = &mut state.usage[self.direction];
        let bytes = header.len as usize;
        if !self.valid()
            || kind != header.kind
            || deadline <= Instant::now()
            || bytes > MAX_BYTES - usage.bytes
        {
            return None;
        }
        usage.bytes += bytes;
        self.bytes = bytes;
        state.expected.remove(&header.fetch);
        self.deadline = deadline;
        Some(deadline)
    }

    async fn wait<T>(&mut self, deadline: Instant, future: impl Future<Output = T>) -> Option<T> {
        if !self.valid() {
            return None;
        }
        tokio::select! {
            biased;
            _ = self.cancelled.changed() => None,
            result = timeout_at(deadline.into(), future) => result.ok(),
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let mut state = lock(&self.plane.state);
        let usage = &mut state.usage[self.direction];
        usage.tasks -= 1;
        usage.bytes -= self.bytes;
    }
}

pub(crate) async fn write(
    conn: Connection,
    header: ClipDataHeader,
    data: Arc<[u8]>,
    mut work: Work,
) {
    let deadline = work.deadline;
    #[cfg(test)]
    let observed = work.plane.clone();
    let Some(Ok(mut send)) = work.wait(deadline, conn.open_uni()).await else {
        return;
    };
    let writing = async {
        let header = encode_clip_data_header(header).map_err(|_| ())?;
        send.set_priority(PRIORITY_CLIP).map_err(|_| ())?;
        send.write_all(&[STREAM_CLIP]).await.map_err(|_| ())?;
        send.write_all(&header).await.map_err(|_| ())?;
        send.write_all(&data).await.map_err(|_| ())?;
        send.finish().map_err(|_| ())?;
        #[cfg(test)]
        tests::finished(&observed);
        Ok::<(), ()>(())
    };
    if work.wait(deadline, writing).await == Some(Ok(())) {
        // A cancelled Quinn stopped() future retains connection-wide waiter state. Never
        // register one: retain our bounded credit until this fixed expiry or cancellation.
        let _ = work
            .wait(deadline, tokio::time::sleep_until(deadline.into()))
            .await;
    }
    let _ = send.reset(RESET);
}

/// Keep cancellation/byte credit until the session actually delivers a completed response.
pub(crate) struct Received {
    event: LinkEvent,
    work: Work,
    recv: RecvStream,
}

impl Received {
    #[cfg(test)]
    pub(crate) fn publication_barrier(&self) -> Option<Arc<std::sync::Barrier>> {
        lock(&self.work.plane.before_publish).clone()
    }

    pub(crate) fn finish(mut self) -> Option<LinkEvent> {
        let valid = {
            let _generation = lock(&self.work.plane.state);
            self.work.valid() && Instant::now() < self.work.deadline
        };
        if valid {
            Some(self.event)
        } else {
            let _ = self.recv.stop(RESET);
            None
        }
    }
}

pub(crate) fn read(
    mut recv: RecvStream,
    plane: Arc<Plane>,
    peer: NodeId,
) -> impl Future<Output = Option<Received>> + Send {
    // Capture task credit and generation before the session spawns/polls the future.
    let work = plane.reserve(0, 0);
    async move {
        let result = async {
            let mut work = work.ok()?;
            let mut raw = [0; CLIP_DATA_HEADER_LEN];
            work.wait(work.deadline, recv.read_exact(&mut raw))
                .await?
                .ok()?;
            let header = decode_clip_data_header(&raw).ok()?;
            // No payload allocation occurs until cap, expected id/kind, deadline and byte budget pass.
            let deadline = work.admit(header)?;
            let mut data = vec![0; header.len as usize];
            let reading = async {
                recv.read_exact(&mut data).await.map_err(|_| ())?;
                let mut extra = [0];
                match recv.read(&mut extra).await {
                    Ok(None) => Ok(()),
                    _ => Err(()),
                }
            };
            if work.wait(deadline, reading).await != Some(Ok(()))
                || !work.valid()
                || Instant::now() >= deadline
            {
                return None;
            }
            Some((
                LinkEvent::ClipData {
                    peer,
                    fetch: header.fetch,
                    kind: header.kind,
                    data: ClipDataBytes(data.into()),
                },
                work,
            ))
        }
        .await;
        match result {
            Some((event, work)) => Some(Received { event, work, recv }),
            None => {
                let _ = recv.stop(RESET);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::*;
    use crate::{Transport, TransportConfig};
    use crosspane_protocol::clip::MAX_CLIP_TEXT;
    use std::sync::{Barrier, OnceLock, Weak};
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::timeout;

    pub(super) fn finished(plane: &Plane) {
        if plane.cancel_after_finish.load(Ordering::Acquire) {
            plane.cancel();
        }
    }

    struct SocketFixture {
        node: Node,
        peer: NodeId,
        client: Connection,
        server: Connection,
        plane: Arc<Plane>,
        _raw: Raw,
        _control: quinn::SendStream,
        _input: quinn::SendStream,
        _receiver: RawReceiver,
    }

    impl SocketFixture {
        async fn new(cancel_in_sink: bool) -> Self {
            let a = identity();
            let b = identity();
            let (local, remote) = if a.node() > b.node() { (a, b) } else { (b, a) };
            let slot: Arc<OnceLock<Weak<Transport>>> = Arc::new(OnceLock::new());
            let callback = slot.clone();
            let (tx, events) = unbounded_channel();
            let transport = Arc::new(
                Transport::bind(
                    TransportConfig {
                        bind: loopback(),
                        identity: local.clone(),
                        pins: Pins::of(&[&remote]),
                        hello: hello_with("node", &["e1", "clip/0"]),
                    },
                    Arc::new(move |event| {
                        if cancel_in_sink && let LinkEvent::ClipData { peer, .. } = &event {
                            callback
                                .get()
                                .unwrap()
                                .upgrade()
                                .unwrap()
                                .cancel_clip(*peer);
                        }
                        let _ = tx.send(event);
                    }),
                )
                .unwrap(),
            );
            slot.set(Arc::downgrade(&transport)).unwrap();
            let mut node = Node {
                transport,
                identity: local.clone(),
                id: local.node(),
                events,
            };
            let raw = Raw::new();
            let client = raw.connect(&remote, &local, node.addr()).await;
            let control =
                open_stream(&client, 1, &hello_frame_with("raw", &["e1", "clip/0"])).await;
            let input = open_stream(&client, 2, &[]).await;
            node.expect_hello(remote.node(), "raw").await;
            let (receiver, _) = RawReceiver::accept(&client).await;
            let tx = node.transport.inner.clipboard(remote.node()).unwrap();
            Self {
                node,
                peer: remote.node(),
                client,
                server: tx.conn,
                plane: tx.clip,
                _raw: raw,
                _control: control,
                _input: input,
                _receiver: receiver,
            }
        }

        // Owned QUIC streams exercise the production reader after session demultiplexing.
        async fn body(&self, bytes: &[u8], fin: bool) -> (quinn::SendStream, RecvStream) {
            let mut send = self.server.open_uni().await.unwrap();
            send.write_all(bytes).await.unwrap();
            if fin {
                send.finish().unwrap();
            }
            let recv = timeout(WAIT, self.client.accept_uni())
                .await
                .unwrap()
                .unwrap();
            (send, recv)
        }

        async fn completed(&self, fetch: u64) -> Received {
            self.plane
                .expect(ClipFetchId(fetch), ClipKind::Text)
                .unwrap();
            let mut body = encode_clip_data_header(header(fetch, ClipKind::Text, 1))
                .unwrap()
                .to_vec();
            body.push(0x5a);
            let (_send, recv) = self.body(&body, true).await;
            read(recv, self.plane.clone(), self.peer).await.unwrap()
        }
    }

    #[tokio::test]
    async fn repeated_cancel_after_fin_registers_no_stopped_waiters_on_one_connection() {
        let f = SocketFixture::new(false).await;
        f.plane.cancel_after_finish.store(true, Ordering::Release);
        let connection = f.server.stable_id();
        for id in 0..32 {
            let header = header(id, ClipKind::Text, 1);
            let work = f.plane.sending(header, 1).unwrap();
            timeout(
                Duration::from_millis(500),
                write(f.server.clone(), header, Arc::from([0x5a]), work),
            )
            .await
            .unwrap();
            assert_eq!(f.server.stable_id(), connection);
            assert_eq!(lock(&f.plane.state).usage[1].tasks, 0);
        }
        // The RED call-site observer counted Pending stopped() registrations, not Quinn's
        // private map. The fixed path leaves it zero; forbid any uninstrumented reintroduction.
        assert_eq!(f.plane.stopped_registrations.load(Ordering::Relaxed), 0);
        let production = include_str!("clip.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(!production.contains(".stopped("));
        assert!(f.server.close_reason().is_none());
    }

    #[tokio::test]
    async fn finished_send_retains_credit_until_fixed_expiry_without_ack_waiter() {
        let f = SocketFixture::new(false).await;
        let work = f.plane.sending(header(1, ClipKind::Text, 1), 1).unwrap();
        let until = work.deadline;
        let send = tokio::spawn(write(
            f.server.clone(),
            header(1, ClipKind::Text, 1),
            Arc::from([0x5a]),
            work,
        ));
        let mut recv = timeout(WAIT, f.client.accept_uni()).await.unwrap().unwrap();
        // Drain through FIN and allow ACK processing. Credit remains until the fixed timer.
        let data = recv.read_to_end(32).await.unwrap();
        assert_eq!(data.len(), 15);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!send.is_finished());
        assert_eq!(lock(&f.plane.state).usage[1].tasks, 1);
        timeout(DEADLINE + Duration::from_millis(500), send)
            .await
            .unwrap()
            .unwrap();
        assert!(Instant::now() >= until);
        assert_eq!(lock(&f.plane.state).usage[1].tasks, 0);
        assert_eq!(f.plane.stopped_registrations.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn spawned_unpolled_readers_capture_cancellation_and_release_both_slots() {
        let f = SocketFixture::new(false).await;
        let (_first, first) = f.body(&[0], false).await;
        let (_second, second) = f.body(&[0], false).await;
        // Current-thread spawn does not poll either factory's future before this cancellation.
        let first = tokio::spawn(read(first, f.plane.clone(), f.peer));
        let second = tokio::spawn(read(second, f.plane.clone(), f.peer));
        f.node.transport.cancel_clip(f.peer);
        let both = timeout(Duration::from_millis(150), async {
            tokio::join!(first, second)
        })
        .await;
        assert!(both.is_ok(), "unpolled old readers survived cancellation");
        let (first, second) = both.unwrap();
        assert!(first.unwrap().is_none() && second.unwrap().is_none());
        assert_eq!(lock(&f.plane.state).usage[0].tasks, 0);
        assert!(f.completed(10).await.finish().is_some());
    }

    #[tokio::test]
    async fn cancellation_before_publication_lock_retires_completed_data() {
        publication_cancel(false).await;
    }

    #[tokio::test]
    async fn closed_connection_cancellation_before_publication_retires_completed_data() {
        publication_cancel(true).await;
    }

    async fn publication_cancel(closed: bool) {
        let mut f = SocketFixture::new(false).await;
        let received = f.completed(1).await;
        let barrier = Arc::new(Barrier::new(2));
        *lock(&f.plane.before_publish) = Some(barrier.clone());
        let inner = f.node.transport.inner.clone();
        let id = inner.clipboard_connection_id(f.peer);
        let peer = f.peer;
        let publish = std::thread::spawn(move || inner.deliver_clip(peer, id, received));
        barrier.wait();
        if closed {
            f.server
                .close(VarInt::from_u32(0), b"owned regression close");
            assert!(f.server.close_reason().is_some());
            // No await after close: the current-thread session has not cleaned the registry.
            assert_eq!(f.node.transport.inner.clipboard_connection_id(peer), id);
        }
        f.node.transport.cancel_clip(peer);
        barrier.wait();
        publish.join().unwrap();
        assert!(
            f.node.events.try_recv().is_err(),
            "old data published after cancel returned (closed={closed})"
        );
        if closed {
            assert_eq!(lock(&f.plane.state).usage[0].tasks, 0);
            return;
        }
        *lock(&f.plane.before_publish) = None;
        let received = f.completed(2).await;
        f.node.transport.inner.deliver_clip(peer, id, received);
        assert!(matches!(
            f.node.next().await,
            LinkEvent::ClipData {
                fetch: ClipFetchId(2),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn sink_callback_can_cancel_its_own_hub_without_deadlock() {
        let mut f = SocketFixture::new(true).await;
        let received = f.completed(1).await;
        let queued = f.completed(2).await;
        let inner = f.node.transport.inner.clone();
        let peer = f.peer;
        let id = inner.clipboard_connection_id(peer);
        let (done, finished) = tokio::sync::oneshot::channel();
        let publish = std::thread::spawn(move || {
            inner.deliver_clip(peer, id, received);
            inner.deliver_clip(peer, id, queued);
            done.send(()).unwrap();
        });
        timeout(Duration::from_millis(500), finished)
            .await
            .unwrap()
            .unwrap();
        publish.join().unwrap();
        assert!(matches!(f.node.next().await, LinkEvent::ClipData { .. }));
        assert!(
            f.node.events.try_recv().is_err(),
            "callback cancellation did not retire queued data"
        );
        assert!(lock(&f.plane.state).expected.is_empty());
    }

    fn ready() -> Arc<Plane> {
        let plane = Plane::new();
        plane.enabled.store(true, Ordering::Release);
        plane
    }

    fn header(id: u64, kind: ClipKind, len: u32) -> ClipDataHeader {
        ClipDataHeader {
            fetch: ClipFetchId(id),
            kind,
            len,
        }
    }

    #[test]
    fn expectations_are_bounded_unique_and_expire_after_two_seconds() {
        let plane = ready();
        for id in 0..MAX_EXPECTED as u64 {
            plane.expect(ClipFetchId(id), ClipKind::Text).unwrap();
        }
        assert!(matches!(
            plane.expect(ClipFetchId(0), ClipKind::Text),
            Err(LinkError::Invalid(_))
        ));
        assert_eq!(
            plane.expect(ClipFetchId(100), ClipKind::Text),
            Err(LinkError::Congested)
        );
        let now = Instant::now();
        let until = lock(&plane.state).expected[&ClipFetchId(0)].1;
        assert!(until > now && until <= now + DEADLINE);
        lock(&plane.state)
            .expected
            .get_mut(&ClipFetchId(0))
            .unwrap()
            .1 = now;
        plane.expect(ClipFetchId(100), ClipKind::Text).unwrap();
        assert!(!lock(&plane.state).expected.contains_key(&ClipFetchId(0)));
    }

    #[test]
    fn tasks_and_bytes_have_independent_per_direction_limits_and_drop_returns_credit() {
        let plane = ready();
        let image = header(1, ClipKind::Image, MAX_CLIP_IMAGE);
        let tx = plane.sending(image, MAX_BYTES).unwrap();
        assert!(matches!(
            plane.sending(header(2, ClipKind::Text, 1), 1),
            Err(LinkError::Congested)
        ));
        let rx = plane.reserve(0, 0).unwrap();
        let rx2 = plane.reserve(0, 0).unwrap();
        assert!(matches!(plane.reserve(0, 0), Err(LinkError::Congested)));
        drop((tx, rx, rx2));
        assert_eq!(lock(&plane.state).usage[0].tasks, 0);
        assert_eq!(lock(&plane.state).usage[1].bytes, 0);
        assert!(plane.sending(image, MAX_BYTES).is_ok());
        assert_eq!((PRIORITY_CLIP, crate::media::PRIORITY_MEDIA), (0, 10));
    }

    #[test]
    fn admission_checks_id_kind_expiry_and_aggregate_bytes_before_claiming_credit() {
        let plane = ready();
        plane.expect(ClipFetchId(1), ClipKind::Image).unwrap();
        plane.expect(ClipFetchId(2), ClipKind::Image).unwrap();
        let mut first = plane.reserve(0, 0).unwrap();
        assert!(first.admit(header(3, ClipKind::Image, 1)).is_none());
        assert!(first.admit(header(1, ClipKind::Text, 1)).is_none());
        assert_eq!(lock(&plane.state).usage[0].bytes, 0);
        assert!(
            first
                .admit(header(1, ClipKind::Image, MAX_CLIP_IMAGE))
                .is_some()
        );
        let mut second = plane.reserve(0, 0).unwrap();
        assert!(second.admit(header(2, ClipKind::Image, 1)).is_none());
        assert!(lock(&plane.state).expected.contains_key(&ClipFetchId(2)));
        drop(first);
        lock(&plane.state)
            .expected
            .get_mut(&ClipFetchId(2))
            .unwrap()
            .1 = Instant::now();
        assert!(second.admit(header(2, ClipKind::Image, 1)).is_none());
        assert_eq!(lock(&plane.state).usage[0].bytes, 0);
    }

    #[tokio::test]
    async fn cancel_interrupts_work_and_new_generation_is_independent() {
        let plane = ready();
        plane.expect(ClipFetchId(1), ClipKind::Text).unwrap();
        let mut work = plane.sending(header(1, ClipKind::Text, 1), 1).unwrap();
        plane.cancel();
        assert!(!work.valid());
        assert!(
            work.wait(work.deadline, std::future::pending::<()>())
                .await
                .is_none()
        );
        assert!(lock(&plane.state).expected.is_empty());
        let later = plane.sending(header(2, ClipKind::Text, 1), 1).unwrap();
        assert!(later.valid());
        plane.retire();
        assert!(!later.valid());
        assert!(matches!(
            plane.expect(ClipFetchId(3), ClipKind::Text),
            Err(LinkError::Invalid(_))
        ));
    }

    #[test]
    fn sender_rejects_caps_and_length_mismatch_without_reserving_work() {
        let plane = ready();
        for (kind, len, actual) in [
            (ClipKind::Text, 1, 0),
            (
                ClipKind::Text,
                MAX_CLIP_TEXT + 1,
                (MAX_CLIP_TEXT + 1) as usize,
            ),
            (
                ClipKind::Image,
                MAX_CLIP_IMAGE + 1,
                (MAX_CLIP_IMAGE + 1) as usize,
            ),
        ] {
            assert!(matches!(
                plane.sending(header(1, kind, len), actual),
                Err(LinkError::Invalid(_))
            ));
        }
        assert_eq!(lock(&plane.state).usage[1].tasks, 0);
        assert_eq!(lock(&plane.state).usage[1].bytes, 0);
    }

    #[test]
    fn production_flow_control_has_headroom_for_aggregate_clipboard_and_media() {
        let bulk: usize = [MAX_BYTES, crate::media::MAX_FRAME].into_iter().sum();
        assert!(
            crate::SEND_WINDOW > bulk as u64,
            "shared send window cannot buffer both bulk kinds plus reliable traffic"
        );
        assert!(crate::CONNECTION_RECEIVE_WINDOW as usize > bulk);
        assert_eq!(
            crate::STREAM_RECEIVE_WINDOW as usize,
            crate::media::MAX_FRAME
        );
    }
}
