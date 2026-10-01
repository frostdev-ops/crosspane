//! Audio device adapters. Policy and grants live in the engine, never in these adapters.

use std::fmt::Debug;
use std::sync::Arc;

pub use crosspane_types::audio::{AudioFormat, AudioKind};
use crosspane_types::id::NodeId;
use rtrb::{Consumer, Producer};

use crate::{EventSink, PlatformError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioDeviceError {
    Unavailable,
    PermissionDenied,
    Locked,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioEvent {
    VirtualActive {
        peer: NodeId,
        kind: AudioKind,
        active: bool,
    },
    /// None denotes a default physical device or an unavailable backend; Some denotes a peer's
    /// virtual device. No sample data or arbitrary OS strings are carried in events.
    DeviceError {
        peer: Option<NodeId>,
        kind: AudioKind,
        error: AudioDeviceError,
    },
}

/// PCM endpoints for the peer's local virtual devices. These are owned by the audio thread.
/// Each channel is preallocated; callback underrun produces silence and overrun drops input.
#[derive(Debug)]
pub struct VirtualPorts {
    pub speaker_out: Consumer<f32>,
    pub mic_in: Producer<f32>,
}

/// A backend-owned stop handle. Stop is idempotent and bounded; it never waits in an audio
/// callback. It must disable sample production/consumption immediately, including on Drop.
pub trait AudioStop: Send + Debug {
    fn stop(&mut self);
}

#[derive(Debug)]
pub struct AudioCapture {
    pub pcm: Consumer<f32>,
    stop: Box<dyn AudioStop>,
}

impl AudioCapture {
    pub fn new(pcm: Consumer<f32>, stop: Box<dyn AudioStop>) -> Self {
        Self { pcm, stop }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

#[derive(Debug)]
pub struct AudioPlayback {
    pub pcm: Producer<f32>,
    stop: Box<dyn AudioStop>,
}

impl AudioPlayback {
    pub fn new(pcm: Producer<f32>, stop: Box<dyn AudioStop>) -> Self {
        Self { pcm, stop }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// Constructed with the node's IoGate. Both physical open methods fail closed while it is
/// closed. Callbacks check it before retaining/consuming samples and latch any observed closure.
/// The agent additionally drops physical handles on every session/gate closure event, even
/// transient closures between callbacks. Neither mechanism automatically restarts a stream.
/// Methods are bounded to 2 s, stop to 50 ms.
/// Callbacks allocate nothing, take no locks, and perform no IPC or logging of sample data.
pub trait AudioHost: Send {
    fn add_peer(&mut self, peer: NodeId, name: &str) -> Result<VirtualPorts, PlatformError>;
    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError>;
    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError>;
    fn open_capture(&mut self, format: AudioFormat) -> Result<AudioCapture, PlatformError>;
    fn open_playback(&mut self, format: AudioFormat) -> Result<AudioPlayback, PlatformError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Debug)]
    struct Stop(Arc<AtomicUsize>);
    impl AudioStop for Stop {
        fn stop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn dropping_each_physical_handle_stops_its_backend_once() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(480);
        drop(AudioCapture::new(consumer, Box::new(Stop(stopped.clone()))));
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        drop(AudioPlayback::new(
            producer,
            Box::new(Stop(stopped.clone())),
        ));
        assert_eq!(stopped.load(Ordering::SeqCst), 2);
    }
}
