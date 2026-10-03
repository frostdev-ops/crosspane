//! An in-memory lazy clipboard host. Only `read` increments the content-read counter.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crosspane_platform::{
    ClipKinds, ClipboardEvent, ClipboardHost, EventSink, IoGate, LocalPasteId, PlatformError,
};
use crosspane_types::ClipKind;

pub struct FakeClipboardHost {
    gate: Arc<IoGate>,
    sink: Option<Arc<dyn EventSink<ClipboardEvent>>>,
    text: Option<Vec<u8>>,
    image: Option<Vec<u8>>,
    promise: Option<(u64, ClipKinds)>,
    answers: BTreeMap<u64, Option<Vec<u8>>>,
    pending: BTreeSet<u64>,
    pub reads: usize,
}
impl Default for FakeClipboardHost {
    fn default() -> Self {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        Self::new(gate)
    }
}
impl std::fmt::Debug for FakeClipboardHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeClipboardHost")
            .field("reads", &self.reads)
            .field("text_bytes", &self.text.as_ref().map(Vec::len))
            .field("image_bytes", &self.image.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}
impl FakeClipboardHost {
    pub fn new(gate: Arc<IoGate>) -> Self {
        Self {
            gate,
            sink: None,
            text: None,
            image: None,
            promise: None,
            answers: BTreeMap::new(),
            pending: BTreeSet::new(),
            reads: 0,
        }
    }
    /// Copies a fixture without reading it. Unlike our own promises, this emits `Changed`.
    pub fn copy(&mut self, text: Option<Vec<u8>>, image: Option<Vec<u8>>) {
        self.cancel_pastes();
        self.promise = None;
        self.text = text;
        self.image = image;
        self.emit(ClipboardEvent::Changed {
            kinds: self.native_kinds(),
        });
    }
    pub fn current_promise(&self) -> Option<(u64, ClipKinds)> {
        self.promise
    }
    pub fn paste(&mut self, paste: LocalPasteId, kind: ClipKind) {
        if let Some((offer, _)) = self.promise {
            self.pending.insert(paste.0);
            self.emit(ClipboardEvent::PasteRequested { paste, offer, kind });
        }
    }
    pub fn answer(&self, paste: LocalPasteId) -> Option<&Option<Vec<u8>>> {
        self.answers.get(&paste.0)
    }
    fn emit(&self, event: ClipboardEvent) {
        if let Some(s) = &self.sink {
            s.send(event);
        }
    }
    fn native_kinds(&self) -> ClipKinds {
        ClipKinds {
            text: self.text.is_some(),
            image: self.image.is_some(),
        }
    }
    fn cancel_pastes(&mut self) {
        for paste in std::mem::take(&mut self.pending) {
            self.answers.entry(paste).or_insert(None);
        }
    }
}
impl ClipboardHost for FakeClipboardHost {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<ClipboardEvent>>) -> Result<(), PlatformError> {
        self.sink = Some(sink);
        Ok(())
    }
    fn kinds(&self) -> Result<ClipKinds, PlatformError> {
        Ok(self
            .promise
            .map_or_else(|| self.native_kinds(), |(_, kinds)| kinds))
    }
    fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        self.reads += 1;
        if self.promise.is_some() {
            return Err(PlatformError::NotFound);
        }
        let data = match kind {
            ClipKind::Text => &self.text,
            ClipKind::Image => &self.image,
        };
        let data = data.as_ref().ok_or(PlatformError::NotFound)?;
        if data.len() > max_bytes {
            return Err(PlatformError::TooLarge);
        }
        if data.is_empty() || (kind == ClipKind::Text && std::str::from_utf8(data).is_err()) {
            return Err(PlatformError::NotFound);
        }
        Ok(data.clone())
    }
    fn promise(&mut self, offer: u64, kinds: ClipKinds) -> Result<(), PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !kinds.text && !kinds.image {
            return Err(PlatformError::Backend("empty clipboard promise".into()));
        }
        self.cancel_pastes();
        self.text = None;
        self.image = None;
        self.promise = Some((offer, kinds));
        Ok(())
    }
    fn fulfil(&mut self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        if self.pending.remove(&paste.0) {
            self.answers.entry(paste.0).or_insert(data);
        }
    }
    fn withdraw(&mut self, offer: u64) -> Result<(), PlatformError> {
        if self.promise.is_some_and(|(current, _)| current == offer) {
            self.cancel_pastes();
            self.promise = None;
        }
        Ok(())
    }
}
