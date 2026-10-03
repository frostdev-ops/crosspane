//! Lazy clipboard promises (CLIP-v0 §5). Backends never log content or emit `Changed` for their
//! own promises. Drop and withdrawal answer pending local pastes empty; read and promise return
//! `Locked` while the gate is closed.

use std::sync::Arc;

use crosspane_types::ClipKind;

use crate::{EventSink, PlatformError};

/// Kinds currently on the clipboard, learned without reading content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipKinds {
    pub text: bool,
    pub image: bool,
}

/// Backend-allocated, per pending local paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalPasteId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// The local clipboard changed for a reason other than our own promise. `kinds` comes from
    /// types or patterns only; no content was read.
    Changed { kinds: ClipKinds },
    /// A local app is pasting from our promise `offer` and needs `kind`. Answer with `fulfil`
    /// within the deadline, or the backend answers empty itself.
    PasteRequested {
        paste: LocalPasteId,
        offer: u64,
        kind: ClipKind,
    },
    /// Our promise `offer` is no longer the clipboard owner: someone copied something else.
    PromiseLost { offer: u64 },
}

pub trait ClipboardHost: Send {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<ClipboardEvent>>) -> Result<(), PlatformError>;
    /// What the clipboard holds, without reading content and without any OS prompt.
    fn kinds(&self) -> Result<ClipKinds, PlatformError>;
    /// Read the clipboard's `kind`, at most `max_bytes`. Only ever called to answer an admitted
    /// peer fetch (a person pasting on the peer). On macOS this may raise the system pasteboard
    /// alert. Over the limit returns `Err(PlatformError::TooLarge)`, not a truncation.
    fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError>;
    /// Replace the local clipboard with a promise for `kinds`; content is fetched through
    /// `PasteRequested`. Never raises a prompt.
    fn promise(&mut self, offer: u64, kinds: ClipKinds) -> Result<(), PlatformError>;
    /// Answer a pending local paste. `None` answers empty (failure, expiry, lock).
    fn fulfil(&mut self, paste: LocalPasteId, data: Option<Vec<u8>>);
    /// Withdraw our promise `offer` if it is still the clipboard owner; a no-op otherwise.
    fn withdraw(&mut self, offer: u64) -> Result<(), PlatformError>;
}
