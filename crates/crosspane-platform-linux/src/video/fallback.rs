//! Per-size fallback for the FFmpeg encoder (WP-2.40).
//!
//! NVENC refuses some coded sizes (below its minimum frame size: 146x50 on the dev machine's
//! RTX 3080 Ti) and the CUDA input pool can fail for a size. Region video asks for small coded
//! sizes during motion, so one refusal must not push a whole projection onto libx264 for good.
//! The encoder keeps its preferred backend. A refusal moves only the session for that size to
//! software, and the next size change tries the preferred backend again.
//!
//! The rules, all per encoder:
//!
//! 1. **Refused sizes.** A refusal is recorded in a set of at most [`CAPACITY`] entries (least
//!    recently used dropped, whatever its strikes), each with the reason: the NVENC session
//!    refusal or a CUDA pool or session failure. Entries are keyed by size and reason. Looking an
//!    entry up or refusing it again counts as a use.
//! 2. **Retry, then stay refused.** The first refusal of a size is a strike: the size is tried
//!    again the next time it comes up, because a refusal can be transient (for example a session
//!    limit). A success clears the entry: a session that opens, or for CUDA input a frame that a
//!    CUDA session encodes (a session that opens but takes no frame is a failure too). A second
//!    refusal makes the size permanently refused:
//!    it is never attempted again, until 16 other refused sizes have been used since and the
//!    entry is the least recently used one to make room.
//!
//!    "The next time it comes up" is an actual change of coded size, seen on every path: a pool
//!    request, a CPU encode and a native encode all note their coded size. While one size goes on
//!    (the per-frame `input_pool` calls, or a rebuild after a bitrate change), a refusal that is
//!    not permanent is not tried again.
//! 3. **Minimum size.** NVENC reports a frame size it can't take as `EINVAL`. Such a refusal, from
//!    a session or from the session probe on a CUDA pool, of a size with a dimension below
//!    [`REFERENCE`] (the size `FfmpegCodecs::new` proved NVENC takes) teaches the minimum: that
//!    dimension must reach [`REFERENCE`]. Sizes below the learned minimum in either dimension go
//!    straight to software (CPU input for pools), with no attempt and no log. The bound is coarse
//!    on purpose. A refused NVENC open costs ~150 ms on the encode thread (measured: ~90 ms for
//!    an accepted one, ~2 ms for libx264), so finding the exact minimum by probing would stall
//!    far longer than it saves, and software handles frames this small easily. A refusal teaches
//!    at most one limit per dimension.
//! 4. **Same size, same decision.** Rebuilding a session at the coded size that already fell back
//!    (a bitrate change) keeps the fallback instead of trying NVENC again, and the per-frame
//!    `input_pool` calls at a refused size answer without retrying.
//!
//! The decisions are generic over the session and pool types, so unit tests drive them with
//! fakes and no GPU.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crosspane_media::codec::{CodecError, EncodedVideo};
use crosspane_types::geom::PixelSize;
use ffmpeg_next as ffmpeg;

use super::Backend;

/// Refused sizes remembered per encoder.
pub(super) const CAPACITY: usize = 16;

/// A coded size NVENC is known to take: `FfmpegCodecs::new` only prefers NVENC after opening a
/// session at it. A size refused with `EINVAL` below it in a dimension is below NVENC's minimum.
pub(super) const REFERENCE: PixelSize = PixelSize::new(256, 256);

/// Refusals after which a size is never attempted again.
const PERMANENT: u8 = 2;

/// Why a size was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    /// NVENC refused to open a session for the size.
    NvencSession,
    /// The CUDA input pool, or the NVENC session on it, failed for the size.
    CudaInput,
}

impl Reason {
    fn index(self) -> usize {
        match self {
            Self::NvencSession => 0,
            Self::CudaInput => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NvencSession => "NVENC session refused",
            Self::CudaInput => "CUDA input failed",
        }
    }
}

/// NVENC's refusal of a frame size it can't take (`EINVAL`), as `Session::new` reports it.
pub(super) fn size_error() -> CodecError {
    super::failed(ffmpeg::Error::Other {
        errno: ffmpeg::error::EINVAL,
    })
}

#[derive(Clone, Debug)]
struct Refused {
    size: PixelSize,
    reason: Reason,
    refusals: u8,
}

/// The refused-size cache and the decisions made from it.
#[derive(Debug)]
pub(super) struct Fallback {
    /// Least recently used first.
    refused: Vec<Refused>,
    /// NVENC's minimum coded size as far as it has been learned (0: no limit seen).
    minimum: PixelSize,
    /// Whether a refusal for each reason has been logged at `info!` yet.
    logged: [bool; 2],
    /// Sessions and pools that fell back from the preferred path, shared with `FfmpegCodecs`.
    fallbacks: Arc<AtomicU64>,
    /// The coded size and backend of the session opened last.
    session: Option<(PixelSize, Backend)>,
    /// The coded size any path (pool request, CPU or native encode) asked about last.
    current: Option<PixelSize>,
    /// The pool for [`Self::current`] is off for now: `input_pool` asks on every frame, and one
    /// refusal answers all of them. A change of size lifts it, while a permanent refusal stays in
    /// `refused`.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pool_suppressed: bool,
}

impl Fallback {
    pub(super) fn new(fallbacks: Arc<AtomicU64>) -> Self {
        Self {
            refused: Vec::new(),
            minimum: PixelSize::new(0, 0),
            logged: [false; 2],
            fallbacks,
            session: None,
            current: None,
            pool_suppressed: false,
        }
    }

    /// Note that a path works on the coded size `size` now. An actual change of size lifts the
    /// temporary suppression of the pool, so the size that comes back is tried again. Permanent
    /// refusals are untouched.
    pub(super) fn encounter(&mut self, size: PixelSize) {
        if self.current != Some(size) {
            self.current = Some(size);
            self.pool_suppressed = false;
        }
    }

    /// Open a session for the coded size `size` on `preferred`, or on software when NVENC is
    /// preferred and refuses or is known to refuse the size. `open` is `Session::new`.
    pub(super) fn open<S>(
        &mut self,
        preferred: Backend,
        size: PixelSize,
        mut open: impl FnMut(Backend, PixelSize) -> Result<S, CodecError>,
    ) -> Result<(Backend, S), CodecError> {
        if preferred != Backend::Nvenc {
            let session = open(preferred, size)?;
            self.session = Some((size, preferred));
            return Ok((preferred, session));
        }
        // A rebuild at the size that already fell back keeps that decision: nothing new is known.
        let decided = self.session == Some((size, Backend::X264));
        let mut refusal = None;
        if !decided && !self.below_minimum(size) {
            if self.is_refused(size, Reason::NvencSession) {
                tracing::debug!(
                    width = size.width,
                    height = size.height,
                    "size stays on software: NVENC refused it before"
                );
            } else {
                match open(Backend::Nvenc, size) {
                    Ok(session) => {
                        self.forgive(size, Reason::NvencSession);
                        self.session = Some((size, Backend::Nvenc));
                        return Ok((Backend::Nvenc, session));
                    }
                    Err(error) => {
                        self.refuse(size, Reason::NvencSession, &error);
                        refusal = Some(error);
                    }
                }
            }
        }
        if !decided {
            self.count();
        }
        let session = open(Backend::X264, size).map_err(|x264| match refusal {
            Some(nvenc) => CodecError::Failed(format!("h264_nvenc: {nvenc}; libx264: {x264}")),
            None => x264,
        })?;
        self.session = Some((size, Backend::X264));
        Ok((Backend::X264, session))
    }

    /// The CUDA input pool for the coded size `size`, or `None` (CPU BGRA input) when setting it
    /// up fails or failed for this size before. `setup` creates the pool and proves NVENC opens
    /// a session on it. Other sizes are not affected.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn pool<P>(
        &mut self,
        size: PixelSize,
        setup: impl FnOnce() -> Result<P, CodecError>,
    ) -> Option<P> {
        self.encounter(size);
        if self.pool_suppressed {
            return None;
        }
        // NVENC refusing the size makes a pool hopeless, whatever the pool itself would do.
        if self.below_minimum(size)
            || self.is_refused(size, Reason::CudaInput)
            || self.is_refused(size, Reason::NvencSession)
        {
            self.pool_suppressed = true;
            self.count();
            return None;
        }
        // A pool that sets up is not yet forgiven: only a frame encoded on it is (`run_native`), so
        // a size whose sessions open but take no frame still reaches its second strike.
        match setup() {
            Ok(pool) => Some(pool),
            Err(error) => {
                self.pool_failed(size, &error);
                None
            }
        }
    }

    /// The pool or a session on it failed for the coded size `size`: CPU input from here on, for
    /// this size only.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn pool_failed(&mut self, size: PixelSize, error: &CodecError) {
        self.encounter(size);
        self.refuse(size, Reason::CudaInput, error);
        self.pool_suppressed = true;
        self.count();
    }

    /// A session on the pool for the coded size `size` opened. That alone doesn't forgive an
    /// earlier failure of the size: a session that takes no frame still has to count against it.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn native_opened(&mut self, size: PixelSize) {
        self.encounter(size);
        self.session = Some((size, Backend::Nvenc));
    }

    /// A frame of the coded size `size` was encoded on a CUDA session: CUDA input works for the
    /// size, so an earlier failure of it was transient.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn native_encoded(&mut self, size: PixelSize) {
        self.forgive(size, Reason::CudaInput);
    }

    fn count(&self) {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    fn below_minimum(&self, size: PixelSize) -> bool {
        size.width < self.minimum.width || size.height < self.minimum.height
    }

    fn position(&self, size: PixelSize, reason: Reason) -> Option<usize> {
        self.refused
            .iter()
            .position(|entry| entry.size == size && entry.reason == reason)
    }

    /// Whether `size` stays refused for `reason`. Looking it up counts as a use.
    fn is_refused(&mut self, size: PixelSize, reason: Reason) -> bool {
        let Some(index) = self.position(size, reason) else {
            return false;
        };
        let entry = self.refused.remove(index);
        let permanent = entry.refusals >= PERMANENT;
        self.refused.push(entry);
        permanent
    }

    fn forgive(&mut self, size: PixelSize, reason: Reason) {
        if let Some(index) = self.position(size, reason) {
            self.refused.remove(index);
        }
    }

    fn refuse(&mut self, size: PixelSize, reason: Reason, error: &CodecError) {
        let refusals = match self.position(size, reason) {
            Some(index) => {
                let mut entry = self.refused.remove(index);
                entry.refusals = entry.refusals.saturating_add(1).min(PERMANENT);
                let refusals = entry.refusals;
                self.refused.push(entry);
                refusals
            }
            None => {
                if self.refused.len() >= CAPACITY {
                    // The least recently used entry goes, whatever its strikes.
                    self.refused.remove(0);
                }
                self.refused.push(Refused {
                    size,
                    reason,
                    refusals: 1,
                });
                1
            }
        };
        let learned = self.learn_minimum(size, error);
        let first = !std::mem::replace(&mut self.logged[reason.index()], true);
        if first {
            tracing::info!(
                reason = reason.label(),
                width = size.width,
                height = size.height,
                refusals,
                minimum_learned = learned,
                minimum_width = self.minimum.width,
                minimum_height = self.minimum.height,
                %error,
                "encoder falls back for this size"
            );
        } else {
            tracing::debug!(
                reason = reason.label(),
                width = size.width,
                height = size.height,
                refusals,
                minimum_learned = learned,
                minimum_width = self.minimum.width,
                minimum_height = self.minimum.height,
                %error,
                "encoder falls back for this size"
            );
        }
    }

    /// Learn NVENC's minimum from the refusal `error` of `size`. Returns whether it changed.
    fn learn_minimum(&mut self, size: PixelSize, error: &CodecError) -> bool {
        if *error != size_error() {
            return false;
        }
        let mut minimum = self.minimum;
        if size.width < REFERENCE.width {
            minimum.width = minimum.width.max(REFERENCE.width);
        }
        if size.height < REFERENCE.height {
            minimum.height = minimum.height.max(REFERENCE.height);
        }
        let changed = minimum != self.minimum;
        self.minimum = minimum;
        changed
    }

    #[cfg(test)]
    fn refusals(&self, size: PixelSize, reason: Reason) -> Option<u8> {
        self.position(size, reason)
            .map(|index| self.refused[index].refusals)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.refused.len()
    }
}

/// The encoder's session slot: decides when the session is (re)opened, and on which backend.
#[derive(Debug)]
pub(super) struct Sessions<S> {
    preferred: Backend,
    active: Backend,
    pub(super) fallback: Fallback,
    session: Option<S>,
    /// The frame size the session was opened for. A different one reopens it, even when both
    /// code to the same even size.
    size: PixelSize,
    /// The session takes CUDA frames rather than CPU BGRA.
    native: bool,
    rebuild: bool,
    pts: i64,
}

impl<S> Sessions<S> {
    pub(super) fn new(preferred: Backend, fallbacks: Arc<AtomicU64>) -> Self {
        Self {
            preferred,
            active: preferred,
            fallback: Fallback::new(fallbacks),
            session: None,
            size: PixelSize::new(0, 0),
            native: false,
            rebuild: false,
            pts: 0,
        }
    }

    /// The backend of the current session (of the last one when it failed).
    pub(super) fn backend(&self) -> Backend {
        self.active
    }

    /// Reopen the session before the next frame, at the same size.
    pub(super) fn request_rebuild(&mut self) {
        self.rebuild = true;
    }

    fn stale(&self, size: PixelSize, native: bool) -> bool {
        size != self.size || self.rebuild || self.session.is_none() || self.native != native
    }

    fn install(&mut self, session: S, backend: Backend, size: PixelSize, native: bool) {
        self.session = Some(session);
        self.active = backend;
        self.size = size;
        self.native = native;
        self.rebuild = false;
        self.pts = 0;
    }

    /// Make the session one that takes CPU frames of `size` (coded size `coded`), reopening it
    /// only when the frame size, the bitrate or the input path changed. `open` is `Session::new`.
    pub(super) fn ensure_cpu(
        &mut self,
        size: PixelSize,
        coded: PixelSize,
        open: impl FnMut(Backend, PixelSize) -> Result<S, CodecError>,
    ) -> Result<(), CodecError> {
        self.fallback.encounter(coded);
        if self.stale(size, false) {
            self.session = None;
            let (backend, session) = self.fallback.open(self.preferred, coded, open)?;
            self.install(session, backend, size, false);
        }
        Ok(())
    }

    /// Make the session one that takes CUDA frames of `size`. `open` is `Session::new_cuda`; its
    /// failure disables the pool for this coded size only.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn ensure_native(
        &mut self,
        size: PixelSize,
        coded: PixelSize,
        open: impl FnOnce() -> Result<S, CodecError>,
    ) -> Result<(), CodecError> {
        self.fallback.encounter(coded);
        if self.stale(size, true) {
            self.session = None;
            match open() {
                Ok(session) => {
                    self.fallback.native_opened(coded);
                    self.install(session, Backend::Nvenc, size, true);
                }
                Err(error) => {
                    self.fallback.pool_failed(coded, &error);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Encode one frame with the session: `encode` gets it, whether the frame must be a key frame
    /// (the first of a session, or forced) and the frame's timestamp. A failure drops the session.
    pub(super) fn run(
        &mut self,
        force_key: bool,
        encode: impl FnOnce(&mut S, bool, i64) -> Result<EncodedVideo, CodecError>,
    ) -> Result<EncodedVideo, CodecError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| CodecError::Failed("encoder session missing".into()))?;
        let result = encode(session, force_key || self.pts == 0, self.pts);
        if result.is_err() {
            self.session = None;
        } else {
            self.pts = self.pts.saturating_add(1);
        }
        result
    }

    /// [`Self::run`] on a CUDA session of coded size `coded`: a failure to submit the frame counts
    /// like a failure to open the session. It switches CUDA input off for this coded size only (CPU
    /// input from the next frame on, with the usual retry at the next size change), so a size whose
    /// sessions open but never take a frame doesn't reopen CUDA sessions every frame.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    pub(super) fn run_native(
        &mut self,
        coded: PixelSize,
        force_key: bool,
        encode: impl FnOnce(&mut S, bool, i64) -> Result<EncodedVideo, CodecError>,
    ) -> Result<EncodedVideo, CodecError> {
        let result = self.run(force_key, encode);
        match &result {
            Ok(_) => self.fallback.native_encoded(coded),
            Err(error) => self.fallback.pool_failed(coded, error),
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct Fake {
        backend: Backend,
        size: PixelSize,
    }

    /// A stand-in for the machine's encoders: records every open and refuses NVENC by rule.
    struct Device {
        opens: Vec<(Backend, PixelSize)>,
        /// The error NVENC refuses a size with, if it does.
        nvenc: Box<dyn Fn(PixelSize) -> Option<CodecError>>,
    }

    impl Device {
        fn new(nvenc: impl Fn(PixelSize) -> Option<CodecError> + 'static) -> Self {
            Self {
                opens: Vec::new(),
                nvenc: Box::new(nvenc),
            }
        }

        fn open(&mut self, backend: Backend, size: PixelSize) -> Result<Fake, CodecError> {
            self.opens.push((backend, size));
            if backend == Backend::Nvenc
                && let Some(error) = (self.nvenc)(size)
            {
                return Err(error);
            }
            Ok(Fake { backend, size })
        }

        fn nvenc_opens(&self, size: PixelSize) -> usize {
            self.opens
                .iter()
                .filter(|open| **open == (Backend::Nvenc, size))
                .count()
        }

        fn opens_of(&self, size: PixelSize) -> usize {
            self.opens.iter().filter(|open| open.1 == size).count()
        }
    }

    fn size(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    /// The RTX 3080 Ti's rule: EINVAL below 146x50.
    fn real_minimum(size: PixelSize) -> Option<CodecError> {
        (size.width < 146 || size.height < 50).then(size_error)
    }

    fn busy(_: PixelSize) -> Option<CodecError> {
        Some(CodecError::Failed("Cannot allocate memory".into()))
    }

    /// An encoder (session slot) and the counter of its fallbacks. Every test builds its encoders
    /// here: that installs the log recorder before the test thread can reach a log callsite, see
    /// [`logged`].
    fn sessions_for(preferred: Backend) -> (Sessions<Fake>, Arc<AtomicU64>) {
        install_recorder();
        let counter = Arc::new(AtomicU64::new(0));
        (Sessions::new(preferred, counter.clone()), counter)
    }

    /// An encoder preferring NVENC, and the counter of its fallbacks.
    fn encoder() -> (Sessions<Fake>, Arc<AtomicU64>) {
        sessions_for(Backend::Nvenc)
    }

    fn fallbacks(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// One frame of `at` pixels through the encoder: reopen if needed, then encode.
    fn frame(
        sessions: &mut Sessions<Fake>,
        device: &mut Device,
        at: PixelSize,
    ) -> Result<EncodedVideo, CodecError> {
        let coded = size(at.width + at.width % 2, at.height + at.height % 2);
        sessions.ensure_cpu(at, coded, |backend, size| device.open(backend, size))?;
        sessions.run(false, |session, key, _| {
            assert_eq!(session.size, coded);
            Ok(EncodedVideo { key })
        })
    }

    #[test]
    fn refusal_falls_back_for_that_size_and_hardware_returns() {
        let (mut sessions, counter) = encoder();
        let mut device = Device::new(real_minimum);
        assert!(
            frame(&mut sessions, &mut device, size(100, 40))
                .unwrap()
                .key
        );
        assert_eq!(sessions.backend(), Backend::X264);
        assert_eq!(
            device.opens,
            [
                (Backend::Nvenc, size(100, 40)),
                (Backend::X264, size(100, 40))
            ]
        );
        assert_eq!(fallbacks(&counter), 1);
        // The preferred backend stays NVENC: the next supported size goes back to it.
        assert!(
            frame(&mut sessions, &mut device, size(1280, 720))
                .unwrap()
                .key
        );
        assert_eq!(sessions.backend(), Backend::Nvenc);
        assert_eq!(
            device.opens.last(),
            Some(&(Backend::Nvenc, size(1280, 720)))
        );
        assert_eq!(fallbacks(&counter), 1);
        // And again after another refused size.
        frame(&mut sessions, &mut device, size(90, 30)).unwrap();
        assert_eq!(sessions.backend(), Backend::X264);
        frame(&mut sessions, &mut device, size(1920, 1080)).unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
    }

    #[test]
    fn transient_refusal_is_forgiven_by_a_success() {
        let (mut sessions, _) = encoder();
        let refuse = Arc::new(Mutex::new(true));
        let rule = refuse.clone();
        let mut device = Device::new(move |_| {
            rule.lock()
                .unwrap()
                .then(|| CodecError::Failed("Cannot allocate memory".into()))
        });
        let a = size(640, 480);
        let b = size(1280, 720);
        frame(&mut sessions, &mut device, a).unwrap();
        assert_eq!(sessions.fallback.refusals(a, Reason::NvencSession), Some(1));
        *refuse.lock().unwrap() = false;
        frame(&mut sessions, &mut device, b).unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
        // The size is tried again and now works: its entry is gone.
        frame(&mut sessions, &mut device, a).unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
        assert_eq!(sessions.fallback.refusals(a, Reason::NvencSession), None);
    }

    #[test]
    fn size_refused_twice_stays_refused() {
        let (mut sessions, counter) = encoder();
        let a = size(640, 480);
        let b = size(1280, 720);
        let refuse = Arc::new(Mutex::new(true));
        let rule = refuse.clone();
        // `a` is refused while `refuse` is set, and so is every size 400 rows high.
        let mut device = Device::new(move |size| {
            ((size == a && *rule.lock().unwrap()) || size.height == 400).then(busy_error)
        });
        frame(&mut sessions, &mut device, a).unwrap();
        frame(&mut sessions, &mut device, b).unwrap();
        // The second encounter tries NVENC once more and is refused again.
        frame(&mut sessions, &mut device, a).unwrap();
        assert_eq!(device.nvenc_opens(a), 2);
        assert_eq!(sessions.fallback.refusals(a, Reason::NvencSession), Some(2));
        frame(&mut sessions, &mut device, b).unwrap();
        // From now on it is never attempted, even when the hardware would take it again.
        *refuse.lock().unwrap() = false;
        for _ in 0..3 {
            frame(&mut sessions, &mut device, a).unwrap();
            assert_eq!(sessions.backend(), Backend::X264);
            frame(&mut sessions, &mut device, b).unwrap();
            assert_eq!(sessions.backend(), Backend::Nvenc);
        }
        assert_eq!(device.nvenc_opens(a), 2);
        assert_eq!(fallbacks(&counter), 5);
        // A size in use stays refused however many other sizes are refused: every lookup
        // refreshes it, so it is never the least recently used.
        for width in 300..340 {
            frame(&mut sessions, &mut device, size(width, 400)).unwrap();
            frame(&mut sessions, &mut device, a).unwrap();
        }
        assert_eq!(sessions.fallback.len(), CAPACITY);
        assert_eq!(sessions.fallback.refusals(a, Reason::NvencSession), Some(2));
        assert_eq!(device.nvenc_opens(a), 2);
    }

    #[test]
    fn learned_minimum_skips_attempts_below_it() {
        let (mut sessions, counter) = encoder();
        let mut device = Device::new(real_minimum);
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        assert_eq!(sessions.fallback.minimum, REFERENCE);
        assert_eq!(fallbacks(&counter), 1);
        let attempts = device.opens.len();
        // Below the minimum in either dimension: software, no NVENC attempt.
        let small = [
            size(64, 64),
            size(254, 1000),
            size(1000, 254),
            size(200, 200),
            size(16, 2000),
        ];
        for small in small {
            frame(&mut sessions, &mut device, small).unwrap();
            assert_eq!(sessions.backend(), Backend::X264);
        }
        assert_eq!(device.opens.len(), attempts + small.len());
        assert!(
            device.opens[attempts..]
                .iter()
                .all(|open| open.0 == Backend::X264)
        );
        assert_eq!(fallbacks(&counter), 1 + small.len() as u64);
        // At the minimum NVENC is tried again.
        frame(&mut sessions, &mut device, REFERENCE).unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
    }

    #[test]
    fn minimum_is_learned_only_from_size_errors_on_small_sizes() {
        // A refusal that isn't a size error teaches nothing.
        let (mut sessions, _) = encoder();
        let mut device = Device::new(busy);
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        assert_eq!(sessions.fallback.minimum, size(0, 0));
        frame(&mut sessions, &mut device, size(64, 64)).unwrap();
        assert_eq!(device.nvenc_opens(size(64, 64)), 1);

        // Neither does a size error at a size NVENC should take (nothing below the reference).
        let (mut sessions, _) = encoder();
        let mut device = Device::new(|_| Some(size_error()));
        frame(&mut sessions, &mut device, size(1280, 720)).unwrap();
        assert_eq!(sessions.fallback.minimum, size(0, 0));

        // One dimension below the reference teaches only that dimension.
        let (mut sessions, _) = encoder();
        let mut device = Device::new(real_minimum);
        frame(&mut sessions, &mut device, size(1000, 40)).unwrap();
        assert_eq!(sessions.fallback.minimum, size(0, 256));
        frame(&mut sessions, &mut device, size(100, 1000)).unwrap();
        assert_eq!(sessions.fallback.minimum, REFERENCE);
        assert_eq!(device.nvenc_opens(size(100, 1000)), 1);
        // Sizes taken at 256 and above never refuse again.
        let attempts = device.opens.len();
        frame(&mut sessions, &mut device, size(256, 256)).unwrap();
        frame(&mut sessions, &mut device, size(3440, 1440)).unwrap();
        assert_eq!(device.opens.len(), attempts + 2);
    }

    #[test]
    fn refused_set_is_a_bounded_lru() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(busy);
        let sizes: Vec<_> = (0..CAPACITY as u32 + 4)
            .map(|index| size(300 + index * 2, 400))
            .collect();
        for size in &sizes {
            frame(&mut sessions, &mut device, *size).unwrap();
            assert!(sessions.fallback.len() <= CAPACITY);
        }
        assert_eq!(sessions.fallback.len(), CAPACITY);
        // The least recently used went; the newest stayed.
        for gone in &sizes[..4] {
            assert_eq!(
                sessions.fallback.refusals(*gone, Reason::NvencSession),
                None
            );
        }
        for kept in &sizes[4..] {
            assert_eq!(
                sessions.fallback.refusals(*kept, Reason::NvencSession),
                Some(1)
            );
        }
        // A forgotten size starts again: one strike, tried again next time.
        frame(&mut sessions, &mut device, sizes[0]).unwrap();
        assert_eq!(
            sessions.fallback.refusals(sizes[0], Reason::NvencSession),
            Some(1)
        );
        assert_eq!(sessions.fallback.len(), CAPACITY);
    }

    #[test]
    fn bound_holds_when_every_entry_is_permanent() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(busy);
        let sizes: Vec<_> = (0..CAPACITY as u32 + 1)
            .map(|index| size(300 + index * 2, 400))
            .collect();
        // Two passes: every size is refused twice and so permanent.
        for _ in 0..2 {
            for at in &sizes[..CAPACITY] {
                frame(&mut sessions, &mut device, *at).unwrap();
            }
        }
        assert_eq!(sessions.fallback.len(), CAPACITY);
        assert!(
            sizes[..CAPACITY]
                .iter()
                .all(|at| sessions.fallback.refusals(*at, Reason::NvencSession) == Some(2))
        );
        // Looking a permanent size up refreshes it.
        frame(&mut sessions, &mut device, sizes[0]).unwrap();
        assert_eq!(device.nvenc_opens(sizes[0]), 2);
        // One more refused size: the least recently used permanent entry makes room, the bound
        // holds, and the entry that was just looked up stays.
        frame(&mut sessions, &mut device, sizes[CAPACITY]).unwrap();
        assert_eq!(sessions.fallback.len(), CAPACITY);
        assert_eq!(
            sessions.fallback.refusals(sizes[1], Reason::NvencSession),
            None
        );
        assert_eq!(
            sessions.fallback.refusals(sizes[0], Reason::NvencSession),
            Some(2)
        );
        assert_eq!(
            sessions
                .fallback
                .refusals(sizes[CAPACITY], Reason::NvencSession),
            Some(1)
        );
    }

    /// A full set (CAPACITY entries): `old` is permanently refused and the least recently used,
    /// and `news` (one more than fits) are refused once each, after it.
    fn full_set_led_by_a_permanent_entry() -> (Sessions<Fake>, Device, PixelSize, Vec<PixelSize>) {
        let (mut sessions, _) = encoder();
        let old = size(640, 480);
        let mut device =
            Device::new(move |size| (size == old || size.height == 400).then(busy_error));
        let news: Vec<_> = (0..CAPACITY as u32)
            .map(|index| size(300 + index * 2, 400))
            .collect();
        // Refused, then refused again at the next encounter: permanent.
        frame(&mut sessions, &mut device, old).unwrap();
        frame(&mut sessions, &mut device, size(1280, 720)).unwrap();
        frame(&mut sessions, &mut device, old).unwrap();
        assert_eq!(
            sessions.fallback.refusals(old, Reason::NvencSession),
            Some(2)
        );
        for at in &news[..CAPACITY - 1] {
            frame(&mut sessions, &mut device, *at).unwrap();
        }
        assert_eq!(sessions.fallback.len(), CAPACITY);
        (sessions, device, old, news)
    }

    #[test]
    fn eviction_is_least_recently_used_whatever_the_strikes() {
        let (mut sessions, mut device, old, news) = full_set_led_by_a_permanent_entry();
        // One more refused size: the oldest entry makes room, though it is the permanent one.
        // The newer entries refused once stay.
        frame(&mut sessions, &mut device, news[CAPACITY - 1]).unwrap();
        assert_eq!(sessions.fallback.len(), CAPACITY);
        assert_eq!(sessions.fallback.refusals(old, Reason::NvencSession), None);
        for at in &news {
            assert_eq!(
                sessions.fallback.refusals(*at, Reason::NvencSession),
                Some(1)
            );
        }
        // The forgotten size starts again with one strike, and the then oldest entry goes.
        let attempts = device.nvenc_opens(old);
        frame(&mut sessions, &mut device, old).unwrap();
        assert_eq!(device.nvenc_opens(old), attempts + 1);
        assert_eq!(
            sessions.fallback.refusals(old, Reason::NvencSession),
            Some(1)
        );
        assert_eq!(
            sessions.fallback.refusals(news[0], Reason::NvencSession),
            None
        );
        assert_eq!(sessions.fallback.len(), CAPACITY);
    }

    #[test]
    fn a_permanent_entry_in_use_outlives_older_one_strike_entries() {
        let (mut sessions, mut device, old, news) = full_set_led_by_a_permanent_entry();
        // Using the permanent size refreshes it without a NVENC attempt.
        let attempts = device.nvenc_opens(old);
        frame(&mut sessions, &mut device, old).unwrap();
        assert_eq!(device.nvenc_opens(old), attempts);
        // So the oldest entry is now a one-strike one.
        frame(&mut sessions, &mut device, news[CAPACITY - 1]).unwrap();
        assert_eq!(sessions.fallback.len(), CAPACITY);
        assert_eq!(
            sessions.fallback.refusals(old, Reason::NvencSession),
            Some(2)
        );
        assert_eq!(
            sessions.fallback.refusals(news[0], Reason::NvencSession),
            None
        );
        for at in &news[1..] {
            assert_eq!(
                sessions.fallback.refusals(*at, Reason::NvencSession),
                Some(1)
            );
        }
    }

    #[test]
    fn no_rebuild_when_the_size_is_unchanged() {
        for refused in [false, true] {
            let (mut sessions, counter) = encoder();
            let mut device =
                Device::new(move |size| (refused && size == self::size(640, 480)).then(size_error));
            let a = size(640, 480);
            let mut keys = Vec::new();
            for _ in 0..6 {
                keys.push(frame(&mut sessions, &mut device, a).unwrap().key);
            }
            // One session for six frames, and only the first is a key frame.
            assert_eq!(keys, [true, false, false, false, false, false]);
            assert_eq!(device.opens_of(a), if refused { 2 } else { 1 });
            assert_eq!(fallbacks(&counter), u64::from(refused));
            // A size change rebuilds; going back rebuilds once more.
            let b = size(1280, 720);
            assert!(frame(&mut sessions, &mut device, b).unwrap().key);
            assert!(!frame(&mut sessions, &mut device, b).unwrap().key);
            assert_eq!(device.opens_of(b), 1);
            assert!(frame(&mut sessions, &mut device, a).unwrap().key);
            assert!(!frame(&mut sessions, &mut device, a).unwrap().key);
            assert_eq!(device.opens_of(b), 1);
        }
    }

    #[test]
    fn source_size_change_rebuilds_even_when_the_coded_size_is_the_same() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(|_| None);
        assert!(
            frame(&mut sessions, &mut device, size(101, 75))
                .unwrap()
                .key
        );
        assert!(
            !frame(&mut sessions, &mut device, size(101, 75))
                .unwrap()
                .key
        );
        assert!(
            frame(&mut sessions, &mut device, size(102, 76))
                .unwrap()
                .key
        );
        assert_eq!(device.opens.len(), 2);
    }

    #[test]
    fn rebuild_at_the_same_size_keeps_the_fallback() {
        let (mut sessions, counter) = encoder();
        let a = size(640, 480);
        let mut device = Device::new(move |size| (size == a).then(size_error));
        frame(&mut sessions, &mut device, a).unwrap();
        assert_eq!(device.nvenc_opens(a), 1);
        // A bitrate change reopens the session once, on software, without asking NVENC again:
        // that would be a second refusal for one encounter.
        sessions.request_rebuild();
        assert!(frame(&mut sessions, &mut device, a).unwrap().key);
        assert!(!frame(&mut sessions, &mut device, a).unwrap().key);
        assert_eq!(device.nvenc_opens(a), 1);
        assert_eq!(device.opens_of(a), 3);
        assert_eq!(sessions.fallback.refusals(a, Reason::NvencSession), Some(1));
        assert_eq!(fallbacks(&counter), 1);

        // On hardware the same rebuild goes back to NVENC.
        let (mut sessions, _) = encoder();
        let mut device = Device::new(|_| None);
        frame(&mut sessions, &mut device, a).unwrap();
        sessions.request_rebuild();
        assert!(frame(&mut sessions, &mut device, a).unwrap().key);
        assert_eq!(device.nvenc_opens(a), 2);
    }

    #[test]
    fn a_failed_session_is_reopened_for_the_same_size() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(|_| None);
        let a = size(640, 480);
        frame(&mut sessions, &mut device, a).unwrap();
        sessions
            .run(false, |_, _, _| {
                Err(CodecError::Failed("encode failed".into()))
            })
            .unwrap_err();
        assert!(frame(&mut sessions, &mut device, a).unwrap().key);
        assert_eq!(device.opens_of(a), 2);
    }

    #[test]
    fn software_preferred_never_falls_back() {
        let (mut sessions, counter) = sessions_for(Backend::X264);
        let mut device = Device::new(busy);
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        assert_eq!(sessions.backend(), Backend::X264);
        assert_eq!(device.opens, [(Backend::X264, size(100, 40))]);
        assert_eq!(fallbacks(&counter), 0);
        // A failing software encoder is an error, with nothing to fall back to.
        let (mut failing, _) = sessions_for(Backend::X264);
        let error = failing
            .ensure_cpu(size(100, 40), size(100, 40), |_, _| {
                Err(CodecError::Failed("no libx264".into()))
            })
            .unwrap_err();
        assert_eq!(error, CodecError::Failed("no libx264".into()));
    }

    #[test]
    fn both_backends_failing_reports_both() {
        let (mut sessions, _) = encoder();
        let error = sessions
            .ensure_cpu(size(100, 40), size(100, 40), |backend, _| {
                Err(CodecError::Failed(format!("{} is broken", backend.name())))
            })
            .unwrap_err();
        let CodecError::Failed(message) = error else {
            panic!("expected Failed");
        };
        assert!(message.contains("h264_nvenc is broken"), "{message}");
        assert!(message.contains("libx264 is broken"), "{message}");
        // Nothing was installed.
        assert!(sessions.session.is_none());
    }

    // The pool decisions, with a fake pool type.

    /// A fake `Session::new_cuda`-plus-pool: fails for the sizes the rule says.
    struct Gpu {
        setups: Vec<PixelSize>,
        refuse: Box<dyn Fn(PixelSize) -> bool>,
    }

    impl Gpu {
        fn new(refuse: impl Fn(PixelSize) -> bool + 'static) -> Self {
            Self {
                setups: Vec::new(),
                refuse: Box::new(refuse),
            }
        }

        fn setup(&mut self, size: PixelSize) -> Result<PixelSize, CodecError> {
            self.setups.push(size);
            if (self.refuse)(size) {
                Err(CodecError::Failed("CUDA pool failed".into()))
            } else {
                Ok(size)
            }
        }

        fn setups_of(&self, size: PixelSize) -> usize {
            self.setups.iter().filter(|setup| **setup == size).count()
        }
    }

    #[test]
    fn pool_failure_disables_the_pool_for_that_size_only() {
        let (mut sessions, counter) = encoder();
        let mut device = Device::new(|_| None);
        let bad = size(640, 480);
        let good = size(1280, 720);
        let mut gpu = Gpu::new(move |size| size == bad);
        // The good size gets a pool; the bad one doesn't.
        assert_eq!(sessions.fallback.pool(good, || gpu.setup(good)), Some(good));
        assert_eq!(sessions.fallback.pool(bad, || gpu.setup(bad)), None);
        assert_eq!(fallbacks(&counter), 1);
        assert_eq!(sessions.fallback.refusals(bad, Reason::CudaInput), Some(1));
        // The per-frame calls at the refused size don't retry the setup or count again.
        for _ in 0..5 {
            assert_eq!(sessions.fallback.pool(bad, || gpu.setup(bad)), None);
        }
        assert_eq!(gpu.setups_of(bad), 1);
        assert_eq!(fallbacks(&counter), 1);
        // The other size still has its pool. The GPU source was not switched off.
        assert_eq!(sessions.fallback.pool(good, || gpu.setup(good)), Some(good));
        // CPU BGRA input is the fallback and still works at the refused size, on NVENC.
        frame(&mut sessions, &mut device, bad).unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
        assert_eq!(fallbacks(&counter), 1);
    }

    #[test]
    fn pool_refused_twice_stays_refused() {
        let (mut sessions, counter) = encoder();
        let bad = size(640, 480);
        let good = size(1280, 720);
        let ok = Arc::new(Mutex::new(false));
        let rule = ok.clone();
        let mut gpu = Gpu::new(move |size| size == bad && !*rule.lock().unwrap());
        sessions.fallback.pool(bad, || gpu.setup(bad));
        sessions.fallback.pool(good, || gpu.setup(good));
        // The next encounter retries once, and the second refusal is final.
        sessions.fallback.pool(bad, || gpu.setup(bad));
        assert_eq!(gpu.setups_of(bad), 2);
        sessions.fallback.pool(good, || gpu.setup(good));
        *ok.lock().unwrap() = true;
        assert_eq!(sessions.fallback.pool(bad, || gpu.setup(bad)), None);
        assert_eq!(gpu.setups_of(bad), 2);
        assert_eq!(fallbacks(&counter), 3);
    }

    #[test]
    fn pool_for_a_size_nvenc_refuses_is_not_tried() {
        let (mut sessions, counter) = encoder();
        let a = size(640, 480);
        let mut device = Device::new(move |size| (size == a).then(busy_error));
        let mut gpu = Gpu::new(|_| false);
        // NVENC refuses the size twice: no pool for it from then on.
        frame(&mut sessions, &mut device, a).unwrap();
        frame(&mut sessions, &mut device, size(1280, 720)).unwrap();
        frame(&mut sessions, &mut device, a).unwrap();
        assert_eq!(sessions.fallback.pool(a, || gpu.setup(a)), None);
        assert_eq!(gpu.setups_of(a), 0);
        // A permanent refusal of the size by NVENC also makes the pool decision a fallback.
        assert_eq!(fallbacks(&counter), 3);
        // Another size gets its pool.
        let b = size(800, 600);
        assert_eq!(sessions.fallback.pool(b, || gpu.setup(b)), Some(b));
    }

    fn busy_error() -> CodecError {
        CodecError::Failed("Cannot allocate memory".into())
    }

    #[test]
    fn pool_below_the_learned_minimum_is_not_tried() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(real_minimum);
        let mut gpu = Gpu::new(|_| false);
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        for small in [size(64, 64), size(200, 300), size(1000, 128)] {
            assert_eq!(sessions.fallback.pool(small, || gpu.setup(small)), None);
        }
        assert!(gpu.setups.is_empty());
        let large = size(1280, 720);
        assert_eq!(
            sessions.fallback.pool(large, || gpu.setup(large)),
            Some(large)
        );
    }

    #[test]
    fn pool_size_error_teaches_the_minimum_to_the_cpu_path_too() {
        let (mut sessions, counter) = encoder();
        let mut device = Device::new(real_minimum);
        let small = size(100, 40);
        // The pool is asked for first: NVENC's refusal of a session on it teaches the minimum.
        assert_eq!(
            sessions
                .fallback
                .pool(small, || Err::<PixelSize, _>(size_error())),
            None
        );
        assert_eq!(sessions.fallback.minimum, REFERENCE);
        // So the CPU session at that size goes straight to software, with no NVENC attempt.
        frame(&mut sessions, &mut device, small).unwrap();
        assert_eq!(device.opens, [(Backend::X264, small)]);
        assert_eq!(sessions.backend(), Backend::X264);
        // One refusal, one fallback for the pool and one for the session.
        assert_eq!(fallbacks(&counter), 2);
    }

    #[test]
    fn native_session_failure_disables_the_pool_for_that_size_only() {
        let (mut sessions, counter) = encoder();
        let bad = size(640, 480);
        let good = size(1280, 720);
        // The pool opens, but NVENC then refuses a session on it for `bad`.
        let error = sessions
            .ensure_native(bad, bad, || {
                Err(CodecError::Failed("session failed".into()))
            })
            .unwrap_err();
        assert_eq!(error, CodecError::Failed("session failed".into()));
        assert_eq!(fallbacks(&counter), 1);
        let mut gpu = Gpu::new(|_| false);
        // No pool for `bad` any more (and no retry on the next frame), but one for `good`.
        assert_eq!(sessions.fallback.pool(bad, || gpu.setup(bad)), None);
        assert!(gpu.setups.is_empty());
        assert_eq!(sessions.fallback.pool(good, || gpu.setup(good)), Some(good));
        sessions
            .ensure_native(good, good, || {
                Ok(Fake {
                    backend: Backend::Nvenc,
                    size: good,
                })
            })
            .unwrap();
        assert_eq!(sessions.backend(), Backend::Nvenc);
        let session = sessions.session.as_ref().unwrap();
        assert!(session.size == good && session.backend == Backend::Nvenc);
        // Same size, same path: no reopening. Switching to CPU input reopens once.
        sessions
            .ensure_native(good, good, || panic!("no rebuild"))
            .unwrap();
        let mut device = Device::new(|_| None);
        sessions
            .ensure_cpu(good, good, |backend, size| device.open(backend, size))
            .unwrap();
        assert_eq!(device.opens.len(), 1);
    }

    /// The agent's view of an encoder with a CUDA pool, as `FfmpegEncoder` drives it: `input_pool`
    /// asks `Fallback::pool` for a size it holds no pool for, `encode_native` opens a CUDA session
    /// and feeds it, and forgets the pool when either fails. CPU rows take over for any frame
    /// without a pool or whose native encode failed.
    struct Rig {
        sessions: Sessions<Fake>,
        counter: Arc<AtomicU64>,
        device: Device,
        pool: Option<PixelSize>,
        /// Sizes whose pool fails to set up.
        pool_fails: Vec<PixelSize>,
        /// Sizes whose CUDA session opens but takes no frame.
        submit_fails: Vec<PixelSize>,
        pool_setups: Vec<PixelSize>,
        native_opens: Vec<PixelSize>,
    }

    impl Rig {
        fn new() -> Self {
            let (sessions, counter) = encoder();
            Self {
                sessions,
                counter,
                device: Device::new(|_| None),
                pool: None,
                pool_fails: Vec::new(),
                submit_fails: Vec::new(),
                pool_setups: Vec::new(),
                native_opens: Vec::new(),
            }
        }

        fn input_pool(&mut self, at: PixelSize) -> bool {
            if self.pool != Some(at) {
                let refused = self.pool_fails.contains(&at);
                let setups = &mut self.pool_setups;
                self.pool = self.sessions.fallback.pool(at, || {
                    setups.push(at);
                    if refused {
                        Err(CodecError::Failed("CUDA pool failed".into()))
                    } else {
                        Ok(at)
                    }
                });
            }
            self.pool.is_some()
        }

        fn encode_native(&mut self, at: PixelSize) -> Result<EncodedVideo, CodecError> {
            assert_eq!(self.pool, Some(at));
            let opens = &mut self.native_opens;
            let result = match self.sessions.ensure_native(at, at, || {
                opens.push(at);
                Ok(Fake {
                    backend: Backend::Nvenc,
                    size: at,
                })
            }) {
                Ok(()) => {
                    let fails = self.submit_fails.contains(&at);
                    self.sessions.run_native(at, false, move |_, key, _| {
                        if fails {
                            Err(CodecError::Failed("send_frame failed".into()))
                        } else {
                            Ok(EncodedVideo { key })
                        }
                    })
                }
                Err(error) => Err(error),
            };
            if result.is_err() {
                self.pool = None;
            }
            result
        }

        /// One frame of CPU rows at `at` (an even size, so it is its own coded size).
        fn cpu(&mut self, at: PixelSize) {
            let device = &mut self.device;
            self.sessions
                .ensure_cpu(at, at, |backend, size| device.open(backend, size))
                .unwrap();
            self.sessions
                .run(false, |_, key, _| Ok(EncodedVideo { key }))
                .unwrap();
        }

        /// One frame as the agent drives it. Returns whether it went through CUDA.
        fn frame(&mut self, at: PixelSize) -> bool {
            if self.input_pool(at) && self.encode_native(at).is_ok() {
                return true;
            }
            self.cpu(at);
            false
        }

        fn setups_of(&self, at: PixelSize) -> usize {
            self.pool_setups
                .iter()
                .filter(|setup| **setup == at)
                .count()
        }

        fn native_opens_of(&self, at: PixelSize) -> usize {
            self.native_opens.iter().filter(|open| **open == at).count()
        }
    }

    #[test]
    fn native_submission_failure_disables_cuda_input_for_that_size() {
        let mut rig = Rig::new();
        let a = size(640, 480);
        let b = size(1280, 720);
        rig.submit_fails.push(a);
        // The pool and a session on it open, but the session takes no frame: the frame falls back
        // to CPU rows, and the failure counts like one to open the session.
        assert!(!rig.frame(a));
        assert_eq!(rig.native_opens, [a]);
        assert_eq!(
            rig.sessions.fallback.refusals(a, Reason::CudaInput),
            Some(1)
        );
        assert_eq!(fallbacks(&rig.counter), 1);
        // The following frames of the size are CPU rows: no new pool, no new CUDA session, one CPU
        // session, nothing counted again.
        for _ in 0..5 {
            assert!(!rig.frame(a));
        }
        assert_eq!(rig.pool_setups, [a]);
        assert_eq!(rig.native_opens, [a]);
        assert_eq!(rig.device.opens_of(a), 1);
        assert_eq!(fallbacks(&rig.counter), 1);
        // Another size keeps its pool and CUDA input.
        assert!(rig.frame(b));
        assert!(rig.frame(b));
        assert_eq!(rig.native_opens, [a, b]);
        // The size is tried once more when it comes back; a second failure is final.
        assert!(!rig.frame(a));
        assert_eq!(rig.native_opens_of(a), 2);
        assert_eq!(
            rig.sessions.fallback.refusals(a, Reason::CudaInput),
            Some(2)
        );
        assert_eq!(fallbacks(&rig.counter), 2);
        // From then on it is neither set up nor opened, and each return counts as a fallback.
        assert!(rig.frame(b));
        for _ in 0..3 {
            assert!(!rig.frame(a));
            assert!(rig.frame(b));
        }
        assert_eq!(rig.setups_of(a), 2);
        assert_eq!(rig.native_opens_of(a), 2);
        assert_eq!(fallbacks(&rig.counter), 5);
    }

    #[test]
    fn pool_suppression_lifts_on_a_size_change_seen_by_the_cpu_path() {
        let mut rig = Rig::new();
        let a = size(640, 480);
        let b = size(1280, 720);
        rig.pool_fails.push(a);
        // CUDA refuses the pool at `a`.
        assert!(!rig.frame(a));
        assert_eq!(rig.pool_setups, [a]);
        // The size changes to `b`, encoded with CPU rows only: there is no pool request for it.
        rig.cpu(b);
        // CUDA recovers and the size comes back: the pool is tried again, and works.
        rig.pool_fails.clear();
        assert!(rig.frame(a));
        assert_eq!(rig.pool_setups, [a, a]);
        assert_eq!(rig.sessions.fallback.refusals(a, Reason::CudaInput), None);
        assert!(rig.frame(a));
        assert_eq!(rig.native_opens, [a]);
    }

    #[test]
    fn repeated_cpu_size_changes_retry_once_more_and_keep_the_permanent_strike() {
        let mut rig = Rig::new();
        let a = size(640, 480);
        let b = size(1280, 720);
        rig.pool_fails.push(a);
        for _ in 0..4 {
            assert!(!rig.frame(a));
            rig.cpu(b);
        }
        // The first encounter is a strike, the second a retry that fails again: final.
        assert_eq!(rig.pool_setups, [a, a]);
        assert_eq!(
            rig.sessions.fallback.refusals(a, Reason::CudaInput),
            Some(2)
        );
        // Size changes don't lift a permanent refusal, even when CUDA would take the size now.
        rig.pool_fails.clear();
        for _ in 0..3 {
            assert!(!rig.frame(a));
            rig.cpu(b);
        }
        assert_eq!(rig.pool_setups, [a, a]);
    }

    #[test]
    fn a_native_encode_of_another_size_lifts_the_suppression_too() {
        let (mut sessions, _) = encoder();
        let a = size(640, 480);
        let b = size(1280, 720);
        let mut gpu = Gpu::new(|_| false);
        sessions
            .fallback
            .pool_failed(a, &CodecError::Failed("CUDA pool failed".into()));
        assert_eq!(sessions.fallback.pool(a, || gpu.setup(a)), None);
        // A native session at `b` is a size change seen without a pool request.
        sessions
            .ensure_native(b, b, || {
                Ok(Fake {
                    backend: Backend::Nvenc,
                    size: b,
                })
            })
            .unwrap();
        assert_eq!(sessions.fallback.pool(a, || gpu.setup(a)), Some(a));
        assert_eq!(gpu.setups_of(a), 1);
    }

    #[test]
    fn counts_each_fallback_once() {
        let (mut sessions, counter) = encoder();
        let mut device = Device::new(real_minimum);
        // Refused (counted), then below the minimum (counted, no attempt).
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        frame(&mut sessions, &mut device, size(64, 64)).unwrap();
        assert_eq!(fallbacks(&counter), 2);
        // Frames at a size don't count again.
        for _ in 0..10 {
            frame(&mut sessions, &mut device, size(64, 64)).unwrap();
        }
        assert_eq!(fallbacks(&counter), 2);
        // Hardware sizes don't count.
        frame(&mut sessions, &mut device, size(1280, 720)).unwrap();
        assert_eq!(fallbacks(&counter), 2);
    }

    // Logging: the first refusal for each reason is at info, repeats at debug.

    // The tests run on parallel threads of one process, all through the same callsites. A
    // subscriber scoped to one test (`with_default`) races with them over the callsites' cached
    // interest: events were lost depending on the order the threads got there. So the process gets
    // one subscriber, installed before any test thread can reach a callsite (every test builds its
    // encoder through `sessions_for`, which installs it), and each test captures through a
    // thread-local that only its own thread's events reach.

    thread_local! {
        /// The levels this thread logged since `logged` started it; `None` outside `logged`.
        static CAPTURE: std::cell::RefCell<Option<Vec<tracing::Level>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Feeds the events of this module into the capture of the thread that logged them.
    struct Recorder;

    impl tracing::Subscriber for Recorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target().ends_with("::fallback") {
                CAPTURE.with(|capture| {
                    if let Some(levels) = capture.borrow_mut().as_mut() {
                        levels.push(*event.metadata().level());
                    }
                });
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn install_recorder() {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            tracing::subscriber::set_global_default(Recorder)
                .expect("no other subscriber is installed in the unit tests");
        });
    }

    /// The levels of the events that `run` logs on this thread.
    fn logged(run: impl FnOnce()) -> Vec<tracing::Level> {
        install_recorder();
        CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        run();
        CAPTURE
            .with(|capture| capture.borrow_mut().take())
            .unwrap_or_default()
    }

    #[test]
    fn first_refusal_per_reason_is_info_and_repeats_are_debug() {
        use tracing::Level;
        let levels = logged(|| {
            let (mut sessions, _) = encoder();
            let mut device = Device::new(busy);
            // Two NVENC session refusals (different sizes), then two CUDA input failures.
            frame(&mut sessions, &mut device, size(300, 400)).unwrap();
            frame(&mut sessions, &mut device, size(302, 400)).unwrap();
            let error = CodecError::Failed("pool".into());
            sessions.fallback.pool_failed(size(304, 400), &error);
            sessions.fallback.pool_failed(size(306, 400), &error);
        });
        assert_eq!(
            levels,
            [Level::INFO, Level::DEBUG, Level::INFO, Level::DEBUG]
        );
    }

    #[test]
    fn a_native_submission_failure_is_logged_like_a_pool_failure() {
        use tracing::Level;
        let levels = logged(|| {
            let mut rig = Rig::new();
            rig.submit_fails.push(size(640, 480));
            rig.submit_fails.push(size(1280, 720));
            assert!(!rig.frame(size(640, 480)));
            assert!(!rig.frame(size(1280, 720)));
        });
        assert_eq!(levels, [Level::INFO, Level::DEBUG]);
    }

    #[test]
    fn sizes_below_the_minimum_log_nothing() {
        let (mut sessions, _) = encoder();
        let mut device = Device::new(real_minimum);
        frame(&mut sessions, &mut device, size(100, 40)).unwrap();
        let levels = logged(|| {
            for width in 40..60 {
                frame(&mut sessions, &mut device, size(width * 2, 40)).unwrap();
            }
        });
        assert!(levels.is_empty(), "{levels:?}");
    }
}
