//! Delivery of asynchronous events from backends.

/// Where a backend delivers events.
///
/// - `send` never blocks: implementations queue the event (the engine forwards it into its own
///   channels). Backends call it from their own threads.
/// - Nothing is dropped. The one exception: consecutive `CaptureEvent::Motion` events of kind
///   `Accelerated` for the same display may be merged by summing `dx`/`dy` and keeping the last
///   timestamp, never across any other event. `Unaccelerated` motion is never merged, because the
///   engine's acceleration curve needs each sample. Discrete input (keys, buttons, scroll phases)
///   and safety events (session state, capture end, overlay availability) are always delivered.
/// - Events of one subscription arrive in the order the backend observed them; a backend that
///   observes on several threads serialises its calls to `send`.
pub trait EventSink<E>: Send + Sync {
    fn send(&self, event: E);
}

impl<E, F> EventSink<E> for F
where
    F: Fn(E) + Send + Sync,
{
    fn send(&self, event: E) {
        self(event)
    }
}
