//! CPU-frame, realtime H.264 through public Media Foundation transforms.
//!
//! Each facade serializes jobs on its own MTA. Hardware transforms are tried first;
//! incompatible hardware falls back to the inbox Microsoft software codec. Native
//! jobs and event waits have a two-second deadline. A native call already running
//! cannot be preempted; a timed-out facade is retired and never accepts another frame.
//! Windows receive GPU output uses one copy-only NV12 import and fresh immutable Y/UV
//! textures. Optional GPU encode uses the source's protected D3D11 device; both paths
//! retain the original CPU fallback.
#![allow(unsafe_code)]

use crate::model::video::{self as model, Clock, Events, Headers, Params, References};
use crosspane_media::{
    codec::{CodecError, EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder},
    picture::{Decoded, Nv12, YuvColour, YuvMatrix, nv12_to_bgra},
};
use crosspane_types::geom::PixelSize;
pub use decode_gpu::{MfDecodeGpu, MfGpuPicture, MfPathObserver};
pub use model::{MfDecodePath, MfGpuFallback};
use std::{
    mem::ManuallyDrop,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::{Com::*, Variant::VARIANT},
    },
    core::{GUID, Interface},
};

const BOUND: Duration = Duration::from_secs(2);
const MAX_PACKET: usize = 64 * 1024 * 1024;
const MAX_IMAGE: usize = model::MAX_DIMENSION as usize * model::MAX_DIMENSION as usize * 3 / 2;
#[cfg(feature = "gpu")]
pub mod gpu;

fn api<T>(result: windows::core::Result<T>, operation: &str) -> Result<T, CodecError> {
    result.map_err(|error| {
        CodecError::Failed(format!(
            "{operation}: HRESULT {:08x}",
            error.code().0 as u32
        ))
    })
}
fn failure(reason: &str) -> CodecError {
    model::failed(reason)
}

/// Per-session hardware-first factory. Construction does not touch native resources.
#[derive(Debug, Default)]
pub struct MfCodecs {
    decode_gpu: Option<Arc<MfDecodeGpu>>,
    #[cfg(feature = "gpu")]
    gpu: Option<Arc<crate::gpu::WindowsGpu>>,
}
impl MfCodecs {
    pub fn new() -> Self {
        Self::default()
    }
    /// Receive-only GPU handshake; encoding and explicit CPU decoding are unchanged.
    pub fn with_decode_gpu(mut self, gpu: Arc<MfDecodeGpu>) -> Self {
        self.decode_gpu = Some(gpu);
        self
    }
    #[cfg(feature = "gpu")]
    pub fn with_gpu(mut self, gpu: Arc<crate::gpu::WindowsGpu>) -> Self {
        self.gpu = Some(gpu);
        self
    }
    pub fn encoder_cpu(
        &self,
        size: PixelSize,
        bitrate: u32,
        fps: u32,
    ) -> Result<MfEncoder, CodecError> {
        let params = Params::new(size, bitrate, fps)?;
        let (worker, name) = Worker::new(Mode::Encoder(params))?;
        Ok(MfEncoder {
            worker,
            name,
            bitrate,
            fps,
            clock: Clock::default(),
            #[cfg(feature = "gpu")]
            gpu: None,
            #[cfg(feature = "gpu")]
            pool: None,
        })
    }
    pub fn decoder_cpu(&self) -> Result<MfDecoder, CodecError> {
        let (worker, name) = Worker::new(Mode::Decoder)?;
        Ok(MfDecoder {
            worker,
            name,
            clock: Clock::default(),
        })
    }
}
impl VideoCodecs for MfCodecs {
    fn encoder(
        &self,
        size: PixelSize,
        bits_per_second: u32,
        fps: u32,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        #[cfg(feature = "gpu")]
        if let Some(gpu) = &self.gpu {
            let params = Params::new(size, bits_per_second, fps)?;
            let (worker, name) =
                Worker::new_with_source_gpu(Mode::Encoder(params), Some(gpu.clone()))?;
            return Ok(Box::new(MfEncoder {
                worker,
                name,
                bitrate: bits_per_second,
                fps,
                clock: Clock::default(),
                gpu: Some(gpu.clone()),
                pool: None,
            }));
        }
        Ok(Box::new(self.encoder_cpu(size, bits_per_second, fps)?))
    }
    fn decoder(&self) -> Result<Box<dyn VideoDecoder>, CodecError> {
        let (worker, name) = Worker::new_with_gpu(Mode::Decoder, self.decode_gpu.clone())?;
        Ok(Box::new(MfDecoder {
            worker,
            name,
            clock: Clock::default(),
        }))
    }
}

/// CPU encoder; all COM objects remain on its worker, without unsafe Send wrappers.
pub struct MfEncoder {
    worker: Worker,
    name: String,
    bitrate: u32,
    fps: u32,
    clock: Clock,
    #[cfg(feature = "gpu")]
    gpu: Option<Arc<crate::gpu::WindowsGpu>>,
    #[cfg(feature = "gpu")]
    pool: Option<Arc<gpu::Pool>>,
}
impl std::fmt::Debug for MfEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfEncoder")
            .field("mft", &self.name)
            .finish_non_exhaustive()
    }
}
impl MfEncoder {
    /// Wait for native shutdown and join; failure reports that a native call is still running.
    pub fn close(mut self) -> Result<(), CodecError> {
        self.worker.close()
    }
}
impl VideoEncoder for MfEncoder {
    #[cfg(feature = "gpu")]
    fn input_pool(
        &mut self,
        size: PixelSize,
    ) -> Result<Option<Arc<dyn crosspane_media::codec::NativeInputPool>>, CodecError> {
        use crosspane_media::codec::NativeInputPool;
        if !self.worker.selection.native() {
            return Ok(None);
        }
        let Some(gpu) = &self.gpu else {
            return Ok(None);
        };
        let params = Params::new(size, self.bitrate, self.fps)?;
        if self
            .pool
            .as_ref()
            .is_none_or(|pool| pool.display != params.size)
        {
            self.pool = Some(gpu::Pool::new(gpu.clone(), size)?);
        }
        Ok(self
            .pool
            .clone()
            .map(|pool| pool as Arc<dyn NativeInputPool>))
    }
    #[cfg(feature = "gpu")]
    fn encode_native(
        &mut self,
        input: &dyn crosspane_media::codec::NativeInput,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        out.clear();
        let params = Params::new(size, self.bitrate, self.fps)?;
        if input.size() != params.coded()? || input.colour() != YuvColour::default() {
            return Err(CodecError::BadInput("MF native geometry or colour"));
        }
        let (at, duration) = self.clock.next(self.fps)?;
        let reply = self.worker.request(Job::EncodeNative {
            input: gpu::lease(
                input,
                self.pool
                    .as_ref()
                    .ok_or(CodecError::BadInput("missing MF input pool"))?,
            )?,
            params,
            key: force_key,
            at,
            duration,
        });
        // A rejected native session is retired; CPU encoding starts with fresh header/IDR state.
        if reply.is_err() {
            self.worker
                .selection
                .refused(crate::model::gpu::Reason::Mft);
        }
        match reply? {
            Reply::Encoded { bytes, key, name } => {
                self.name = name;
                #[cfg(feature = "gpu")]
                {
                    self.worker.selection = crate::model::gpu::Path::selected(self.name.clone());
                }
                out.extend_from_slice(&bytes);
                Ok(EncodedVideo { key })
            }
            _ => Err(failure("unexpected native encoder reply")),
        }
    }
    fn encode(
        &mut self,
        pixels: &[u8],
        stride: u32,
        size: PixelSize,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedVideo, CodecError> {
        out.clear();
        let params = Params::new(size, self.bitrate, self.fps)?;
        #[cfg(feature = "gpu")]
        if self.worker.send.is_none() {
            // Old native work retains its own resources. A fresh CPU worker never reuses them.
            let (worker, name) = Worker::new(Mode::Encoder(params))?;
            self.worker = worker;
            self.name = name;
            self.gpu = None;
            self.pool = None;
        }
        let mut picture = model::bgra_to_nv12(pixels, stride, size)?;
        picture.y.append(&mut picture.uv);
        let (at, duration) = self.clock.next(self.fps)?;
        match self.worker.request(Job::Encode {
            params,
            bytes: picture.y,
            key: force_key,
            at,
            duration,
        })? {
            Reply::Encoded { bytes, key, name } => {
                self.name = name;
                #[cfg(feature = "gpu")]
                {
                    self.worker.selection = crate::model::gpu::Path::selected(self.name.clone());
                }
                out.extend_from_slice(&bytes);
                Ok(EncodedVideo { key })
            }
            _ => Err(failure("unexpected encoder reply")),
        }
    }
    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate = bits_per_second;
    }
    fn name(&self) -> &str {
        &self.name
    }
}
pub struct MfDecoder {
    worker: Worker,
    name: String,
    clock: Clock,
}
impl std::fmt::Debug for MfDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfDecoder")
            .field("mft", &self.name)
            .finish_non_exhaustive()
    }
}
impl MfDecoder {
    pub fn close(mut self) -> Result<(), CodecError> {
        self.worker.close()
    }
    fn picture(&mut self, data: &[u8]) -> Result<Nv12, CodecError> {
        let (at, duration) = self.clock.next(30)?;
        match self.worker.request(Job::Decode {
            // Even malformed/oversized input reaches the reference reset, without copying it.
            bytes: if data.len() <= MAX_PACKET {
                data.to_vec()
            } else {
                Vec::new()
            },
            at,
            duration,
        })? {
            Reply::Picture { picture, name } => {
                self.name = name;
                Ok(picture)
            }
            _ => Err(failure("unexpected decoder reply")),
        }
    }
}
impl VideoDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<PixelSize, CodecError> {
        out.clear();
        let picture = self.picture(data)?;
        nv12_to_bgra(&picture, picture.size, out)?;
        Ok(picture.size)
    }
    fn decode_nv12(&mut self, data: &[u8], out: &mut Nv12) -> Result<(), CodecError> {
        out.y.clear();
        out.uv.clear();
        out.size = PixelSize::default();
        out.y_stride = 0;
        out.uv_stride = 0;
        reuse_picture(out, self.picture(data)?);
        Ok(())
    }
    fn decode_native(&mut self, data: &[u8], reuse: &mut Arc<Nv12>) -> Result<Decoded, CodecError> {
        let (at, duration) = self.clock.next(30)?;
        match self.worker.request(Job::DecodeNative {
            bytes: if data.len() <= MAX_PACKET {
                data.to_vec()
            } else {
                Vec::new()
            },
            at,
            duration,
        })? {
            Reply::NativePicture { picture, name } => {
                self.name = name;
                Ok(Decoded::Native(picture))
            }
            Reply::Picture { picture, name } => {
                self.name = name;
                if Arc::get_mut(reuse).is_none() {
                    *reuse = Arc::default();
                }
                let out = Arc::get_mut(reuse).ok_or_else(|| failure("CPU picture is in use"))?;
                reuse_picture(out, picture);
                Ok(Decoded::Nv12(reuse.clone()))
            }
            _ => Err(failure("unexpected native decoder reply")),
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
}

fn reuse_picture(out: &mut Nv12, mut picture: Nv12) {
    if out.y.capacity() >= picture.y.len() {
        out.y.clear();
        out.y.extend_from_slice(&picture.y);
        std::mem::swap(&mut out.y, &mut picture.y);
    }
    if out.uv.capacity() >= picture.uv.len() {
        out.uv.clear();
        out.uv.extend_from_slice(&picture.uv);
        std::mem::swap(&mut out.uv, &mut picture.uv);
    }
    *out = picture;
}

#[derive(Clone, Copy)]
enum Mode {
    Encoder(Params),
    Decoder,
}
enum Job {
    DecodeNative {
        bytes: Vec<u8>,
        at: i64,
        duration: i64,
    },
    #[cfg(feature = "gpu")]
    EncodeNative {
        input: Arc<gpu::InputSlot>,
        params: Params,
        key: bool,
        at: i64,
        duration: i64,
    },
    Encode {
        params: Params,
        bytes: Vec<u8>,
        key: bool,
        at: i64,
        duration: i64,
    },
    Decode {
        bytes: Vec<u8>,
        at: i64,
        duration: i64,
    },
}
enum Reply {
    NativePicture {
        picture: Arc<MfGpuPicture>,
        name: String,
    },
    Encoded {
        bytes: Vec<u8>,
        key: bool,
        name: String,
    },
    Picture {
        picture: Nv12,
        name: String,
    },
}
struct Envelope {
    job: Job,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<Reply, CodecError>>,
}
struct Worker {
    send: Option<mpsc::SyncSender<Envelope>>,
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
    control: Option<mpsc::SyncSender<()>>,
    #[cfg(feature = "gpu")]
    selection: crate::model::gpu::Path,
}
impl Worker {
    fn new(mode: Mode) -> Result<(Self, String), CodecError> {
        Self::new_with_gpu(mode, None)
    }
    fn new_with_gpu(
        mode: Mode,
        gpu: Option<Arc<MfDecodeGpu>>,
    ) -> Result<(Self, String), CodecError> {
        Self::spawn(
            mode,
            gpu,
            #[cfg(feature = "gpu")]
            None,
        )
    }
    #[cfg(feature = "gpu")]
    fn new_with_source_gpu(
        mode: Mode,
        source_gpu: Option<Arc<crate::gpu::WindowsGpu>>,
    ) -> Result<(Self, String), CodecError> {
        Self::spawn(mode, None, source_gpu)
    }
    fn spawn(
        mode: Mode,
        gpu: Option<Arc<MfDecodeGpu>>,
        #[cfg(feature = "gpu")] source_gpu: Option<Arc<crate::gpu::WindowsGpu>>,
    ) -> Result<(Self, String), CodecError> {
        let (send, receive) = mpsc::sync_channel::<Envelope>(1);
        let (started, start) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::sync_channel(1);
        let (control_send, control_receive) = mpsc::sync_channel(1);
        let control = gpu.as_ref().map(|_| control_send.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let deadline = Instant::now() + BOUND;
        let join = thread::Builder::new()
            .name("crosspane-mf".into())
            .spawn(move || {
                let run = || {
                    let runtime = match Runtime::new() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = started.try_send(Err(error));
                            return;
                        }
                    };
                    let mut codec = match Native::new(
                        mode,
                        gpu,
                        control_send,
                        deadline,
                        &flag,
                        #[cfg(feature = "gpu")]
                        source_gpu,
                    ) {
                        Ok(codec) => codec,
                        Err(error) => {
                            let _ = started.try_send(Err(error));
                            return;
                        }
                    };
                    if check(deadline, &flag).is_err()
                        || started.try_send(Ok(codec.session.name.clone())).is_err()
                    {
                        return;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        loop {
                            // Control flags are drained BEFORE the next MFT input, including shutdown.
                            let blocked = codec.poll_leases();
                            if flag.load(Ordering::Acquire) && codec.leases.is_empty() {
                                break;
                            }
                            if flag.load(Ordering::Acquire) || blocked {
                                if codec
                                    .leases
                                    .iter()
                                    .all(decode_gpu::SourceLease::quarantined)
                                {
                                    // Unknown retired work stays owned without an active polling loop.
                                    let _ = control_receive.recv();
                                } else {
                                    let _ = control_receive.recv_timeout(model::GPU_POLL);
                                }
                                continue;
                            }
                            let received = if codec.gpu.is_some() {
                                match receive.try_recv() {
                                    Ok(envelope) => Some(envelope),
                                    Err(mpsc::TryRecvError::Empty) => {
                                        let _ = control_receive.recv();
                                        continue;
                                    }
                                    Err(mpsc::TryRecvError::Disconnected) => None,
                                }
                            } else {
                                receive.recv().ok()
                            };
                            let Some(envelope) = received else {
                                flag.store(true, Ordering::Release);
                                continue;
                            };
                            if check(envelope.deadline, &flag).is_err() {
                                flag.store(true, Ordering::Release);
                                continue;
                            }
                            let result = codec.job(envelope.job, envelope.deadline, &flag);
                            let result = check(envelope.deadline, &flag).and(result);
                            let _ = envelope.reply.try_send(result);
                        }
                    }));
                    if result.is_err() {
                        flag.store(true, Ordering::Release);
                        for lease in &codec.leases {
                            lease.quarantine();
                        }
                        // Quarantine retains MTA/sample/runtime owners until proven safe settlement.
                        while !codec.leases.is_empty() {
                            let polled =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    codec.poll_leases()
                                }));
                            if polled.is_err() {
                                for lease in &codec.leases {
                                    lease.quarantine();
                                }
                            }
                            if !codec.leases.is_empty() {
                                let _ = control_receive.recv();
                            }
                        }
                    }
                    drop(codec);
                    drop(runtime);
                };
                run();
                let _ = finished.try_send(());
            })
            .map_err(|error| CodecError::Unavailable(error.to_string()))?;
        let mut worker = Self {
            send: Some(send),
            stop,
            done,
            join: Some(join),
            control,
            #[cfg(feature = "gpu")]
            selection: crate::model::gpu::Path::default(),
        };
        match start.recv_timeout(BOUND) {
            Ok(Ok(name)) => {
                #[cfg(feature = "gpu")]
                {
                    worker.selection = crate::model::gpu::Path::selected(name.clone());
                }
                Ok((worker, name))
            }
            Ok(Err(error)) => {
                let _ = worker.close();
                Err(CodecError::Unavailable(error.to_string()))
            }
            Err(_) => {
                worker.stop.store(true, Ordering::Release);
                Err(CodecError::Unavailable(
                    "MFT initialization timed out or worker failed".into(),
                ))
            }
        }
    }
    fn request(&mut self, job: Job) -> Result<Reply, CodecError> {
        let (send, receive) = mpsc::sync_channel(1);
        let deadline = Instant::now() + BOUND;
        let sender = self
            .send
            .as_ref()
            .ok_or_else(|| CodecError::Unavailable("MFT worker retired".into()))?;
        sender
            .try_send(Envelope {
                job,
                deadline,
                reply: send,
            })
            .map_err(|_| failure("MFT worker unavailable or busy"))?;
        if let Some(control) = &self.control {
            let _ = control.try_send(());
        }
        match receive.recv_timeout(BOUND) {
            Ok(result) => {
                let result = deliver_reply(result, deadline, &self.stop);
                if let Err(error) = check(deadline, &self.stop) {
                    self.stop.store(true, Ordering::Release);
                    self.send.take();
                    return Err(error);
                }
                result
            }
            Err(_) => {
                self.stop.store(true, Ordering::Release);
                self.send.take();
                Err(failure("MFT frame deadline or worker failure"))
            }
        }
    }
    fn close(&mut self) -> Result<(), CodecError> {
        self.stop.store(true, Ordering::Release);
        self.send.take();
        if let Some(control) = &self.control {
            let _ = control.try_send(());
        }
        if let Some(join) = self.join.take() {
            if self.done.recv_timeout(BOUND).is_err() {
                return Err(failure("MFT native shutdown still running"));
            }
            join.join().map_err(|_| failure("MFT worker panicked"))?;
        }
        Ok(())
    }
}
fn deliver_reply(
    result: Result<Reply, CodecError>,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Reply, CodecError> {
    check(deadline, stop).and(result)
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn check(deadline: Instant, stop: &AtomicBool) -> Result<(), CodecError> {
    if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(failure("MFT deadline or cancellation"))
    } else {
        Ok(())
    }
}
struct Runtime;
impl Runtime {
    fn new() -> Result<Self, CodecError> {
        api(
            // SAFETY: this dedicated worker initializes and tears down its own MTA.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() },
            "CoInitializeEx",
        )?;
        if let Err(error) = api(
            // SAFETY: balanced MFStartup/MFShutdown; called off the MF work queues.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) },
            "MFStartup",
        ) {
            // SAFETY: successful COM initialization above belongs to this thread.
            unsafe {
                CoUninitialize();
            }
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: codecs have already dropped; balanced initialization on this MTA.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

struct Native {
    session: Session,
    mode: Mode,
    first: bool,
    headers: Headers,
    references: References,
    aperture: Option<model::Aperture>,
    gpu: Option<Arc<MfDecodeGpu>>,
    gpu_mta: Option<decode_gpu::GpuMta>,
    control: mpsc::SyncSender<()>,
    leases: Vec<decode_gpu::SourceLease>,
    #[cfg(feature = "gpu")]
    source_gpu: Option<Arc<crate::gpu::WindowsGpu>>,
}
impl Native {
    fn new(
        mode: Mode,
        gpu: Option<Arc<MfDecodeGpu>>,
        control: mpsc::SyncSender<()>,
        deadline: Instant,
        stop: &AtomicBool,
        #[cfg(feature = "gpu")] source_gpu: Option<Arc<crate::gpu::WindowsGpu>>,
    ) -> Result<Self, CodecError> {
        #[cfg(feature = "gpu")]
        let native = match (&source_gpu, mode) {
            (Some(gpu), Mode::Encoder(params)) => gpu::select(gpu, params, deadline, stop).ok(),
            _ => None,
        };
        Ok(Self {
            session: {
                #[cfg(feature = "gpu")]
                if let Some(native) = native {
                    native
                } else {
                    Session::select(mode, false, deadline, stop)?
                }
                #[cfg(not(feature = "gpu"))]
                {
                    Session::select(mode, false, deadline, stop)?
                }
            },
            mode,
            first: true,
            headers: Headers::default(),
            references: References::default(),
            aperture: None,
            gpu,
            gpu_mta: None,
            control,
            leases: Vec::new(),
            #[cfg(feature = "gpu")]
            source_gpu,
        })
    }
    fn poll_leases(&mut self) -> bool {
        model::gpu_drain_control(
            &mut self.leases,
            decode_gpu::SourceLease::poll,
            decode_gpu::SourceLease::blocks_input,
        )
    }
    fn blocks_input(&self) -> bool {
        self.leases
            .iter()
            .any(decode_gpu::SourceLease::blocks_input)
    }
    fn decode_admission(&mut self, bytes: &[u8]) -> Result<(bool, model::Aperture), CodecError> {
        self.references.check(bytes)?;
        let idr = model::has_nal(bytes, 5);
        if self.first && !idr {
            return Err(failure("decoder requires IDR"));
        }
        if idr {
            self.aperture = Some(model::h264_aperture(bytes)?);
        } else if model::has_nal(bytes, 7) && self.aperture != Some(model::h264_aperture(bytes)?) {
            return Err(failure("geometry change requires IDR"));
        }
        let aperture = self
            .aperture
            .ok_or_else(|| failure("missing coded geometry"))?;
        Ok((idr, aperture))
    }
    fn decode_native(
        &mut self,
        bytes: &[u8],
        at: i64,
        duration: i64,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Reply, CodecError> {
        let Some(bridge) = self.gpu.clone() else {
            return self.decode(bytes, at, duration, deadline, stop);
        };
        let (idr, expected) = self.decode_admission(bytes)?;
        if idr {
            // Only an independent IDR may change the device binding/decoder implementation.
            self.gpu_mta = None;
            if let Some(host) = bridge.host()
                && model::gpu_idr_binding(idr, Some(host.generation)).is_some()
                && let Ok(gpu) = decode_gpu::GpuMta::new(host)
                && let Ok(session) = Session::select_gpu(&gpu.manager, deadline, stop)
            {
                self.session = session;
                self.gpu_mta = Some(gpu);
            }
        }
        let Some(gpu) = self.gpu_mta.as_ref() else {
            return self.decode_admitted(bytes, at, duration, deadline, stop, idr, expected);
        };
        if bridge
            .host()
            .is_none_or(|host| host.generation != gpu.host.generation)
        {
            return Err(failure("MF GPU generation retired; requires a CPU IDR"));
        }
        let output = (|| {
            check(deadline, stop)?;
            let input = sample(bytes, at, duration)?;
            let output = self.session.exchange(&input, at, deadline, stop)?;
            let (area, colour) = self.session.gpu_description(expected)?;
            check(deadline, stop)?;
            let picture = gpu.prepare(
                &bridge,
                output,
                area,
                expected,
                colour,
                self.control.clone(),
                deadline,
                &mut self.leases,
            )?;
            self.first = false;
            Ok(Reply::NativePicture {
                picture,
                name: self.session.name.clone(),
            })
        })();
        if output.is_err() && idr && !self.blocks_input() {
            // An unavailable GPU path before checkout can retry the independent IDR on CPU.
            // Unknown checkout state instead propagates failure and stays quarantined.
            self.gpu_mta = None;
            self.session.invalid = true;
            self.decode_admitted(bytes, at, duration, deadline, stop, idr, expected)
        } else {
            output
        }
    }
    fn job(&mut self, job: Job, deadline: Instant, stop: &AtomicBool) -> Result<Reply, CodecError> {
        let result = match job {
            Job::DecodeNative {
                bytes,
                at,
                duration,
            } => self.decode_native(&bytes, at, duration, deadline, stop),
            #[cfg(feature = "gpu")]
            Job::EncodeNative {
                input,
                params,
                key,
                at,
                duration,
            } => self.encode_native(input, params, key, at, duration, deadline, stop),
            Job::Encode {
                params,
                bytes,
                key,
                at,
                duration,
            } => self.encode(params, bytes, key, at, duration, deadline, stop),
            Job::Decode {
                bytes,
                at,
                duration,
            } => self.decode(&bytes, at, duration, deadline, stop),
        };
        if result.is_err() {
            self.first = true;
            self.headers = Headers::default();
            self.references = References::default();
            self.aperture = None;
            self.session.invalid = true;
        }
        result
    }
    #[cfg(feature = "gpu")]
    #[allow(clippy::too_many_arguments)]
    fn encode_native(
        &mut self,
        input: Arc<gpu::InputSlot>,
        params: Params,
        key: bool,
        at: i64,
        duration: i64,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Reply, CodecError> {
        let previous = match self.mode {
            Mode::Encoder(params) => params,
            _ => return Err(failure("wrong GPU codec mode")),
        };
        if self.session.invalid || params.size != previous.size {
            let gpu = self
                .source_gpu
                .as_ref()
                .ok_or_else(|| failure("missing native GPU"))?;
            self.session = gpu::select(gpu, params, deadline, stop)?;
            self.headers = Headers::default();
            self.references = References::default();
            self.first = true;
        } else if params.bitrate != previous.bitrate {
            self.session
                .property(&CODECAPI_AVEncCommonMeanBitRate, params.bitrate.into())?;
        }
        if self.session.manager.is_none() {
            return Err(failure("GPU session unavailable"));
        }
        self.mode = Mode::Encoder(params);
        let force = key || self.first;
        if force {
            self.session
                .property(&CODECAPI_AVEncVideoForceKeyFrame, 1_u32.into())?;
        }
        // SAFETY: public input stream requirements; absent attribute means no extra bind requirement.
        let bind = match unsafe {
            self.session
                .transform
                .GetInputStreamAttributes(self.session.input)
                .and_then(|attrs| attrs.GetUINT32(&MF_SA_D3D11_BINDFLAGS))
        } {
            Ok(bind) => bind,
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => 0,
            Err(error) => return Err(failure(&error.to_string())),
        };
        let sample = input.sample(at, duration, bind)?;
        let output = self
            .session
            .exchange_guarded(&sample, at, deadline, stop, &|| input.permitted())?;
        let data = sample_bytes(&output, MAX_PACKET)?;
        let (bytes, key) = self.headers.packet(&data, force)?;
        self.references.check(&bytes)?;
        if key
            && model::h264_aperture(&bytes)?
                != (model::Aperture {
                    x: 0,
                    y: 0,
                    size: params.coded()?,
                })
        {
            return Err(failure("GPU encoder changed coded geometry"));
        }
        self.first = false;
        Ok(Reply::Encoded {
            bytes,
            key,
            name: self.session.name.clone(),
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &mut self,
        params: Params,
        bytes: Vec<u8>,
        key: bool,
        at: i64,
        duration: i64,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Reply, CodecError> {
        let previous = match self.mode {
            Mode::Encoder(params) => params,
            _ => return Err(failure("wrong codec mode")),
        };
        #[cfg(feature = "gpu")]
        if self.session.manager.is_some() {
            self.session.invalid = true;
        }
        if self.session.invalid || params.size != previous.size {
            self.session = Session::select(Mode::Encoder(params), false, deadline, stop)?;
            self.first = true;
            self.headers = Headers::default();
        } else if params.bitrate != previous.bitrate {
            self.session
                .property(&CODECAPI_AVEncCommonMeanBitRate, params.bitrate.into())?;
        }
        self.mode = Mode::Encoder(params);
        let force = key || self.first;
        if force {
            self.session
                .property(&CODECAPI_AVEncVideoForceKeyFrame, 1_u32.into())?;
        }
        let sample = sample(&bytes, at, duration)?;
        let output = self.session.exchange(&sample, at, deadline, stop);
        let output = match output {
            Err(_) if self.session.hardware => {
                // This CPU frame was not published. A software retry starts with an IDR.
                self.session = Session::select(self.mode, true, deadline, stop)?;
                self.headers = Headers::default();
                self.first = true;
                self.session
                    .property(&CODECAPI_AVEncVideoForceKeyFrame, 1_u32.into())?;
                self.session.exchange(&sample, at, deadline, stop)?
            }
            result => result?,
        };
        let data = sample_bytes(&output, MAX_PACKET)?;
        let (bytes, key) = self.headers.packet(&data, force || self.first)?;
        self.references.check(&bytes)?;
        if key
            && model::h264_aperture(&bytes)?
                != (model::Aperture {
                    x: 0,
                    y: 0,
                    size: params.coded()?,
                })
        {
            return Err(failure("encoder changed the coded geometry"));
        }
        self.first = false;
        Ok(Reply::Encoded {
            bytes,
            key,
            name: self.session.name.clone(),
        })
    }
    fn decode(
        &mut self,
        bytes: &[u8],
        at: i64,
        duration: i64,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Reply, CodecError> {
        let (idr, aperture) = self.decode_admission(bytes)?;
        self.decode_admitted(bytes, at, duration, deadline, stop, idr, aperture)
    }
    #[allow(clippy::too_many_arguments)]
    fn decode_admitted(
        &mut self,
        bytes: &[u8],
        at: i64,
        duration: i64,
        deadline: Instant,
        stop: &AtomicBool,
        idr: bool,
        aperture: model::Aperture,
    ) -> Result<Reply, CodecError> {
        // An IDR is independent of all prior samples, including after damage or resize.
        if idr || self.session.invalid {
            self.session = Session::select(Mode::Decoder, false, deadline, stop)?;
        }
        let sample = sample(bytes, at, duration)?;
        let output = self.session.exchange(&sample, at, deadline, stop);
        let output = match output {
            Err(_) if self.session.hardware && idr => {
                self.session = Session::select(Mode::Decoder, true, deadline, stop)?;
                self.session.exchange(&sample, at, deadline, stop)?
            }
            result => result?,
        };
        let picture = self.session.picture(&output, aperture)?;
        self.first = false;
        Ok(Reply::Picture {
            picture,
            name: self.session.name.clone(),
        })
    }
}

struct Session {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    credits: Events,
    input: u32,
    output: u32,
    hardware: bool,
    name: String,
    mode: Mode,
    invalid: bool,
    #[cfg(feature = "gpu")]
    manager: Option<IMFDXGIDeviceManager>,
}
impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: worker exclusively owns this live transform; no callback borrows it.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            #[cfg(feature = "gpu")]
            if self.manager.is_some() {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            }
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}
impl Session {
    fn select(
        mode: Mode,
        software_only: bool,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Self, CodecError> {
        let candidates = if software_only {
            Vec::new()
        } else {
            enumerate(mode).unwrap_or_default()
        };
        let attempts = candidates.into_iter().map(|activate| {
            check(deadline, stop)?;
            // SAFETY: enumeration returned an owned activation object, activated on this MTA.
            let candidate = api(
                unsafe { activate.ActivateObject::<IMFTransform>() },
                "ActivateObject",
            )
            .and_then(|transform| Self::configure(transform, true, friendly_name(&activate), mode));
            if candidate.is_ok() {
                return candidate;
            }
            // SAFETY: failed candidate is no longer used; release its activation cache.
            unsafe {
                let _ = activate.ShutdownObject();
            }
            candidate
        });
        model::prefer_hardware(attempts, || {
            check(deadline, stop)?;
            let (clsid, name) = match mode {
                Mode::Encoder(_) => (CMSH264EncoderMFT, "Microsoft H.264 encoder (software)"),
                Mode::Decoder => (CMSH264DecoderMFT, "Microsoft H.264 decoder (software)"),
            };
            // SAFETY: documented inbox class, no aggregation, instantiated on initialized MTA.
            let transform = api(
                unsafe { CoCreateInstance::<_, IMFTransform>(&clsid, None, CLSCTX_INPROC_SERVER) },
                "CoCreateInstance H.264",
            )?;
            Self::configure(transform, false, name.into(), mode)
        })
    }
    fn select_gpu(
        manager: &IMFDXGIDeviceManager,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<Self, CodecError> {
        let attempts = enumerate(Mode::Decoder)
            .unwrap_or_default()
            .into_iter()
            .map(|activate| {
                check(deadline, stop)?;
                // SAFETY: owned enumeration activation, used solely on the decoder MTA.
                let result = api(
                    unsafe { activate.ActivateObject::<IMFTransform>() },
                    "GPU ActivateObject",
                )
                .and_then(|transform| {
                    Self::configure_with_manager(
                        transform,
                        true,
                        friendly_name(&activate),
                        Mode::Decoder,
                        Some(manager),
                    )
                });
                if result.is_err() {
                    // SAFETY: failed candidate has no published output and its cache is no longer used.
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                }
                result
            });
        model::prefer_hardware(attempts, || {
            check(deadline, stop)?;
            // SAFETY: documented inbox decoder, initialized MTA, no aggregation.
            let transform = api(
                unsafe {
                    CoCreateInstance::<_, IMFTransform>(
                        &CMSH264DecoderMFT,
                        None,
                        CLSCTX_INPROC_SERVER,
                    )
                },
                "GPU inbox decoder",
            )?;
            Self::configure_with_manager(
                transform,
                false,
                "Microsoft H.264 DXGI decoder".into(),
                Mode::Decoder,
                Some(manager),
            )
        })
    }
    fn configure(
        transform: IMFTransform,
        hardware: bool,
        name: String,
        mode: Mode,
    ) -> Result<Self, CodecError> {
        Self::configure_with_manager(transform, hardware, name, mode, None)
    }
    fn configure_with_manager(
        transform: IMFTransform,
        hardware: bool,
        name: String,
        mode: Mode,
        manager: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self, CodecError> {
        let mut session = Self {
            transform,
            events: None,
            credits: Events::default(),
            input: 0,
            output: 0,
            hardware,
            name,
            mode,
            invalid: false,
            #[cfg(feature = "gpu")]
            manager: if matches!(mode, Mode::Encoder(_)) {
                manager.cloned()
            } else {
                None
            },
        };
        // SAFETY: live transform, documented stream and attribute getters and writable one-element IDs.
        unsafe {
            let mut inputs = 0;
            let mut outputs = 0;
            api(
                session.transform.GetStreamCount(&mut inputs, &mut outputs),
                "GetStreamCount",
            )?;
            if inputs != 1 || outputs != 1 {
                return Err(failure("H.264 MFT must have one input/output stream"));
            }
            let mut input = [0];
            let mut output = [0];
            match session.transform.GetStreamIDs(&mut input, &mut output) {
                Ok(()) => {
                    session.input = input[0];
                    session.output = output[0];
                }
                Err(error) if error.code() == windows::Win32::Foundation::E_NOTIMPL => (),
                error => {
                    api(error, "GetStreamIDs")?;
                }
            }
            if let Ok(attrs) = session.transform.GetAttributes()
                && attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0
            {
                api(
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1),
                    "async unlock",
                )?;
                session.events = Some(api(session.transform.cast(), "IMFMediaEventGenerator")?);
            }
        }
        if let Some(manager) = manager {
            // SAFETY: owned attributes and exact manager, configured BEFORE media types/streaming.
            unsafe {
                let attributes = api(session.transform.GetAttributes(), "GPU MFT attributes")?;
                if api(
                    attributes.GetUINT32(&MF_SA_D3D11_AWARE),
                    "MF D3D11 awareness",
                )? != 1
                {
                    return Err(failure("MFT is not D3D11 aware"));
                }
                api(
                    session
                        .transform
                        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize),
                    "MFT DXGI manager",
                )?;
            }
        }
        // The inbox H.264 decoder uniquely requires VT_UI4; other codecs use VT_BOOL.
        let low_latency = if matches!(mode, Mode::Decoder) && !hardware {
            1_u32.into()
        } else {
            true.into()
        };
        session.property(&CODECAPI_AVLowLatencyMode, low_latency)?;
        match mode {
            Mode::Encoder(params) => {
                session.property(&CODECAPI_AVEncMPVDefaultBPictureCount, 0_u32.into())?;
                session.property(
                    &CODECAPI_AVEncCommonRateControlMode,
                    (eAVEncCommonRateControlMode_CBR.0 as u32).into(),
                )?;
                session.property(&CODECAPI_AVEncCommonMeanBitRate, params.bitrate.into())?;
                session.property(&CODECAPI_AVEncMPVGOPSize, model::GOP.into())?;
                let output = video_type(MFVideoFormat_H264, Some(params))?;
                let input = video_type(MFVideoFormat_NV12, Some(params))?;
                // SAFETY: matching even-sized NV12/H.264 media types; encoder output set first.
                unsafe {
                    api(
                        output.SetUINT32(&MF_MT_AVG_BITRATE, params.bitrate),
                        "output bitrate",
                    )?;
                    api(
                        output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32),
                        "baseline profile",
                    )?;
                    api(
                        session.transform.SetOutputType(session.output, &output, 0),
                        "encoder SetOutputType",
                    )?;
                    api(
                        session.transform.SetInputType(session.input, &input, 0),
                        "encoder SetInputType",
                    )?;
                }
            }
            Mode::Decoder => {
                let input = video_type(MFVideoFormat_H264, None)?;
                // SAFETY: public documented minimal H.264 input; stream change supplies coded dimensions.
                unsafe {
                    api(
                        session.transform.SetInputType(session.input, &input, 0),
                        "decoder SetInputType",
                    )?;
                }
                session.output_type()?;
            }
        }
        // SAFETY: prepared owned transform beginning a fresh stream.
        unsafe {
            api(
                session
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0),
                "begin streaming",
            )?;
            api(
                session
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0),
                "start stream",
            )?;
        }
        Ok(session)
    }
    fn property(&self, key: &GUID, value: VARIANT) -> Result<(), CodecError> {
        let codec: ICodecAPI = api(self.transform.cast(), "ICodecAPI")?;
        // SAFETY: codec property GUID and correctly typed owning VARIANT, borrowed for this call.
        api(unsafe { codec.SetValue(key, &value) }, "ICodecAPI SetValue")
    }
    fn output_type(&mut self) -> Result<(), CodecError> {
        for index in 0..64 {
            // SAFETY: live transform and bounded enumeration; type remains owned across SetOutputType.
            let ty = unsafe { self.transform.GetOutputAvailableType(self.output, index) };
            let ty = match ty {
                Ok(ty) => ty,
                Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
                error => api(error, "GetOutputAvailableType")?,
            };
            // SAFETY: reading the documented subtype GUID of an owned media type.
            if unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                api(
                    // SAFETY: advertised NV12 output; select before receiving another sample.
                    unsafe { self.transform.SetOutputType(self.output, &ty, 0) },
                    "decoder SetOutputType",
                )?;
                return Ok(());
            }
        }
        Err(failure("decoder has no CPU NV12 output type"))
    }
    fn event(
        &mut self,
        deadline: Instant,
        stop: &AtomicBool,
        permitted: &dyn Fn() -> bool,
    ) -> Result<(), CodecError> {
        let generator = self
            .events
            .as_ref()
            .ok_or_else(|| failure("missing async event generator"))?;
        loop {
            check(deadline, stop)?;
            if !permitted() {
                return Err(failure("native capture retired"));
            }
            // SAFETY: exclusively owned generator, explicitly NONBLOCKING; no callback allocation.
            let event = unsafe { generator.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match event {
                Ok(event) => {
                    // SAFETY: live event; failed HRESULTs are preserved even if its type looks useful.
                    let (status, kind) = unsafe { (event.GetStatus(), event.GetType()) };
                    api(api(status, "event status")?.ok(), "MFT event")?;
                    match api(kind, "event type")? {
                        kind if kind == METransformNeedInput.0 as u32 => {
                            self.credits.need_input()?
                        }
                        kind if kind == METransformHaveOutput.0 as u32 => {
                            self.credits.have_output()?
                        }
                        kind if kind == MEError.0 as u32 => return Err(failure("MFT error event")),
                        _ => (),
                    }
                    return Ok(());
                }
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    thread::sleep(Duration::from_millis(1))
                }
                error => {
                    api(error, "GetEvent")?;
                }
            }
        }
    }
    fn exchange(
        &mut self,
        input: &IMFSample,
        at: i64,
        deadline: Instant,
        stop: &AtomicBool,
    ) -> Result<IMFSample, CodecError> {
        self.exchange_guarded(input, at, deadline, stop, &|| true)
    }
    fn exchange_guarded(
        &mut self,
        input: &IMFSample,
        at: i64,
        deadline: Instant,
        stop: &AtomicBool,
        permitted: &dyn Fn() -> bool,
    ) -> Result<IMFSample, CodecError> {
        let asynchronous = self.events.is_some();
        if asynchronous {
            while !self.credits.can_submit() {
                self.event(deadline, stop, permitted)?;
            }
            self.credits.submit(at)?;
        }
        check(deadline, stop)?;
        if !permitted() {
            return Err(failure("native capture retired"));
        }
        api(
            // SAFETY: prepared stream, owned sample; async input credit was consumed exactly once.
            unsafe { self.transform.ProcessInput(self.input, input, 0) },
            "ProcessInput",
        )?;
        for _ in 0..4 {
            if asynchronous {
                while !self.credits.can_output() {
                    self.event(deadline, stop, permitted)?;
                }
            }
            check(deadline, stop)?;
            if !permitted() {
                return Err(failure("native capture retired"));
            }
            match self.output_sample() {
                Err(error)
                    if error.code() == MF_E_TRANSFORM_STREAM_CHANGE
                        && matches!(self.mode, Mode::Decoder) =>
                {
                    self.output_type()?
                }
                result => {
                    let sample = api(result, "ProcessOutput")?;
                    // SAFETY: this MFT returned an owned sample with its propagated input timestamp.
                    let actual = api(unsafe { sample.GetSampleTime() }, "output timestamp")?;
                    if actual != at {
                        return Err(failure("MFT returned reordered or stale frame"));
                    }
                    if asynchronous {
                        self.credits.complete(actual)?;
                    }
                    // No incomplete output is accepted; a codec needing future input is incompatible.
                    return Ok(sample);
                }
            }
        }
        Err(failure("repeated MFT output format changes"))
    }
    fn output_sample(&self) -> windows::core::Result<IMFSample> {
        // SAFETY: querying the live output requirements; memory remains owned for ProcessOutput.
        let info = unsafe { self.transform.GetOutputStreamInfo(self.output)? };
        let provided = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output,
            ..Default::default()
        };
        if !provided {
            let limit = if matches!(self.mode, Mode::Decoder) {
                MAX_IMAGE
            } else {
                MAX_PACKET
            };
            if info.cbSize == 0 || info.cbSize as usize > limit {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ));
            }
            // SAFETY: requested bounded size/alignment mask per GetOutputStreamInfo; sample owns buffer.
            unsafe {
                let sample = MFCreateSample()?;
                let buffer =
                    MFCreateAlignedMemoryBuffer(info.cbSize, info.cbAlignment.saturating_sub(1))?;
                sample.AddBuffer(&buffer)?;
                output.pSample = ManuallyDrop::new(Some(sample));
            }
        }
        let mut status = 0;
        // SAFETY: one initialized output slot and status; release BOTH returned interfaces on all paths.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        // SAFETY: these ManuallyDrop owners are taken exactly once after ProcessOutput, including errors.
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        // SAFETY: take and drop the MFT's optional event collection ownership exactly once.
        unsafe {
            drop(ManuallyDrop::take(&mut output.pEvents));
        }
        result?;
        if status != 0 || output.dwStatus != 0 {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        sample.ok_or_else(|| {
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
        })
    }
    fn gpu_description(
        &self,
        expected: model::Aperture,
    ) -> Result<(model::Aperture, YuvColour), CodecError> {
        // SAFETY: owned negotiated output type; metadata only, never a CPU pixel read.
        let ty = api(
            unsafe { self.transform.GetOutputCurrentType(self.output) },
            "GPU output type",
        )?;
        // SAFETY: public attributes with their documented integer/GUID representations.
        let (size, subtype, matrix, range) = unsafe {
            (
                api(ty.GetUINT64(&MF_MT_FRAME_SIZE), "GPU output size")?,
                api(ty.GetGUID(&MF_MT_SUBTYPE), "GPU output subtype")?,
                optional_u32(&ty, &MF_MT_YUV_MATRIX)?.unwrap_or(0),
                optional_u32(&ty, &MF_MT_VIDEO_NOMINAL_RANGE)?.unwrap_or(0),
            )
        };
        if subtype != MFVideoFormat_NV12 {
            return Err(failure("GPU output is not NV12"));
        }
        let storage = PixelSize::new((size >> 32) as u32, size as u32);
        let area = output_aperture(&ty)?.unwrap_or(model::Aperture {
            x: 0,
            y: 0,
            size: storage,
        });
        model::Nv12CopyPlan::new(storage, 1, 0, area, expected)
            .map_err(|reason| failure(reason.as_str()))?;
        let colour = YuvColour {
            matrix: match matrix {
                0 | 1 => YuvMatrix::Bt709,
                2 => YuvMatrix::Bt601,
                _ => return Err(failure("unsupported YUV matrix")),
            },
            full_range: match range {
                0 | 2 => false,
                1 => true,
                _ => return Err(failure("unsupported nominal range")),
            },
        };
        Ok((area, colour))
    }
    fn picture(&self, sample: &IMFSample, expected: model::Aperture) -> Result<Nv12, CodecError> {
        // SAFETY: owned output type, reading only its public format attributes.
        let ty = api(
            unsafe { self.transform.GetOutputCurrentType(self.output) },
            "output type",
        )?;
        // SAFETY: each attribute has its specified integer/GUID representation.
        let (size, subtype, stride, matrix, range) = unsafe {
            (
                api(ty.GetUINT64(&MF_MT_FRAME_SIZE), "output size")?,
                api(ty.GetGUID(&MF_MT_SUBTYPE), "output subtype")?,
                optional_u32(&ty, &MF_MT_DEFAULT_STRIDE)?,
                optional_u32(&ty, &MF_MT_YUV_MATRIX)?.unwrap_or(0),
                optional_u32(&ty, &MF_MT_VIDEO_NOMINAL_RANGE)?.unwrap_or(0),
            )
        };
        let size = PixelSize::new((size >> 32) as u32, size as u32);
        if subtype != MFVideoFormat_NV12 {
            return Err(failure("invalid decoded NV12 dimensions or subtype"));
        }
        let stride = match stride {
            Some(stride) => stride,
            None => {
                // SAFETY: documented minimum stride of the negotiated NV12 storage format.
                let stride = api(
                    unsafe { MFGetStrideForBitmapInfoHeader(MFVideoFormat_NV12.data1, size.width) },
                    "NV12 minimum stride",
                )?;
                u32::try_from(stride).map_err(|_| failure("negative NV12 stride"))?
            }
        };
        let bytes = sample_bytes(sample, MAX_IMAGE)?;
        // IMFMediaBuffer::Lock gives contiguous minimum-stride storage, not a guessed GPU pitch.
        model::decoded_nv12(
            size,
            stride,
            output_aperture(&ty)?,
            expected,
            &bytes,
            YuvColour {
                matrix: match matrix {
                    0 | 1 => YuvMatrix::Bt709,
                    2 => YuvMatrix::Bt601,
                    _ => return Err(failure("unsupported YUV matrix")),
                },
                full_range: match range {
                    0 | 2 => false,
                    1 => true,
                    _ => return Err(failure("unsupported nominal range")),
                },
            },
        )
    }
}

fn optional_u32(ty: &IMFMediaType, key: &GUID) -> Result<Option<u32>, CodecError> {
    // SAFETY: owned media type and the caller's documented UINT32 attribute GUID.
    match unsafe { ty.GetUINT32(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        result => api(result, "media type UINT32").map(Some),
    }
}
fn output_aperture(ty: &IMFMediaType) -> Result<Option<model::Aperture>, CodecError> {
    if optional_u32(ty, &MF_MT_PAN_SCAN_ENABLED)?.unwrap_or(0) != 0 {
        return Err(failure("pan-scan output is not a coded image"));
    }
    // Microsoft specifies minimum display aperture first, geometric aperture for compatibility.
    for key in [MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_GEOMETRIC_APERTURE] {
        // SAFETY: public blob size query; the fixed buffer below matches the pinned MFVideoArea layout.
        match unsafe { ty.GetBlobSize(&key) } {
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => continue,
            result => {
                if api(result, "aperture size")? != 16 {
                    return Err(failure("invalid aperture blob size"));
                }
            }
        }
        const {
            assert!(std::mem::size_of::<MFVideoArea>() == 16);
        }
        let mut blob = [0; 16];
        let mut written = 0;
        api(
            // SAFETY: writable 16-byte blob, no alignment-dependent cast; parser validates all fields.
            unsafe { ty.GetBlob(&key, &mut blob, Some(&mut written)) },
            "aperture blob",
        )?;
        if written != 16 {
            return Err(failure("short aperture blob"));
        }
        return model::Aperture::from_blob(&blob).map(Some);
    }
    Ok(None)
}

fn enumerate(mode: Mode) -> Result<Vec<IMFActivate>, CodecError> {
    let (category, input, output) = match mode {
        Mode::Encoder(_) => (
            MFT_CATEGORY_VIDEO_ENCODER,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
        ),
        Mode::Decoder => (
            MFT_CATEGORY_VIDEO_DECODER,
            MFVideoFormat_H264,
            MFVideoFormat_NV12,
        ),
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: input,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: output,
    };
    let mut items = ptr::null_mut();
    let mut count = 0;
    api(
        // SAFETY: documented filtered enumeration, writable allocated-array/count outputs.
        unsafe {
            MFTEnumEx(
                category,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut items,
                &mut count,
            )
        },
        "MFTEnumEx hardware",
    )?;
    if items.is_null() {
        return if count == 0 {
            Ok(Vec::new())
        } else {
            Err(failure("null MFT enumeration"))
        };
    }
    let mut owned = Vec::new();
    // SAFETY: MFTEnumEx returned exactly count initialized Option<IMFActivate> entries and a CoTaskMem allocation.
    unsafe {
        for item in std::slice::from_raw_parts_mut(items, count as usize) {
            if let Some(item) = item.take() {
                owned.push(item);
            }
        }
        CoTaskMemFree(Some(items.cast()));
    }
    Ok(owned)
}
fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = [0_u16; 1024];
    let mut length = 0;
    // SAFETY: caller-owned UTF-16 buffer; returned length is checked before slicing.
    if unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, Some(&mut length)) }
        .is_ok()
        && (length as usize) < name.len()
    {
        format!(
            "{} (hardware)",
            String::from_utf16_lossy(&name[..length as usize])
        )
    } else {
        "H.264 MFT (hardware)".into()
    }
}
fn video_type(subtype: GUID, params: Option<Params>) -> Result<IMFMediaType, CodecError> {
    // SAFETY: public allocator returns an owned empty media type, populated with typed attributes.
    unsafe {
        let ty = api(MFCreateMediaType(), "MFCreateMediaType")?;
        api(
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video),
            "major type",
        )?;
        api(ty.SetGUID(&MF_MT_SUBTYPE, &subtype), "subtype")?;
        api(
            ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32),
            "progressive",
        )?;
        if let Some(params) = params {
            let coded = params.coded()?;
            api(
                ty.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(coded.width) << 32) | u64::from(coded.height),
                ),
                "frame size",
            )?;
            api(
                ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(params.fps) << 32) | 1),
                "frame rate",
            )?;
            api(
                ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1),
                "aspect ratio",
            )?;
            api(
                ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32),
                "BT.709 matrix",
            )?;
            api(
                ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32),
                "limited range",
            )?;
            api(
                ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32),
                "BT.709 primaries",
            )?;
            api(
                ty.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_sRGB.0 as u32),
                "sRGB transfer",
            )?;
        }
        Ok(ty)
    }
}
fn sample(bytes: &[u8], at: i64, duration: i64) -> Result<IMFSample, CodecError> {
    let length = u32::try_from(bytes.len()).map_err(|_| failure("input sample too large"))?;
    // SAFETY: public allocation and typed timestamp setters; initialized buffer ownership moves into the sample.
    unsafe {
        let sample = api(MFCreateSample(), "MFCreateSample")?;
        let buffer = api(MFCreateMemoryBuffer(length), "MFCreateMemoryBuffer")?;
        let mut lock = BufferLock::new(&buffer)?;
        if lock.maximum < length || lock.data.is_null() {
            return Err(failure("short native input allocation"));
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), lock.data, bytes.len());
        lock.unlock()?;
        api(buffer.SetCurrentLength(length), "SetCurrentLength")?;
        api(sample.AddBuffer(&buffer), "AddBuffer")?;
        api(sample.SetSampleTime(at), "SetSampleTime")?;
        api(sample.SetSampleDuration(duration), "SetSampleDuration")?;
        Ok(sample)
    }
}
struct BufferLock<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    maximum: u32,
    length: u32,
    locked: bool,
}
impl<'a> BufferLock<'a> {
    fn new(buffer: &'a IMFMediaBuffer) -> Result<Self, CodecError> {
        let mut owner = Self {
            buffer,
            data: ptr::null_mut(),
            maximum: 0,
            length: 0,
            locked: false,
        };
        api(
            // SAFETY: live owned buffer; pointer is borrowed ONLY until its balanced unlock.
            unsafe {
                buffer.Lock(
                    &mut owner.data,
                    Some(&mut owner.maximum),
                    Some(&mut owner.length),
                )
            },
            "buffer Lock",
        )?;
        owner.locked = true;
        Ok(owner)
    }
    fn unlock(&mut self) -> Result<(), CodecError> {
        self.locked = false;
        // SAFETY: this owner acquired the one matching lock; even errors are never retried as a second unlock.
        api(unsafe { self.buffer.Unlock() }, "buffer Unlock")
    }
}
impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        if self.locked {
            let _ = self.unlock();
        }
    }
}
fn sample_bytes(sample: &IMFSample, limit: usize) -> Result<Vec<u8>, CodecError> {
    // SAFETY: owned sample; the returned contiguous buffer retains its storage.
    let buffer = api(
        unsafe { sample.ConvertToContiguousBuffer() },
        "ConvertToContiguousBuffer",
    )?;
    let mut lock = BufferLock::new(&buffer)?;
    if lock.data.is_null()
        || lock.length == 0
        || lock.length > lock.maximum
        || lock.length as usize > limit
    {
        return Err(failure("invalid output buffer"));
    }
    // SAFETY: checked nonnull pointer and lengths supplied by the locked buffer; copy before unlock.
    let bytes = unsafe { std::slice::from_raw_parts(lock.data, lock.length as usize).to_vec() };
    lock.unlock()?;
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn nv12_decode_reuses_caller_plane_allocations() {
        let mut out = Nv12 {
            y: Vec::with_capacity(1024),
            uv: Vec::with_capacity(512),
            ..Default::default()
        };
        let y = out.y.as_ptr();
        let uv = out.uv.as_ptr();
        let picture = Nv12 {
            size: PixelSize::new(4, 4),
            y_stride: 4,
            uv_stride: 4,
            y: vec![41; 16],
            uv: vec![123; 8],
            ..Default::default()
        };
        reuse_picture(&mut out, picture.clone());
        assert_eq!(out.y.as_ptr(), y);
        assert_eq!(out.uv.as_ptr(), uv);
        assert_eq!(out, picture);
        out.validate().unwrap();
    }
    #[test]
    fn completed_frame_after_deadline_is_never_delivered() {
        let result = Ok(Reply::Encoded {
            bytes: vec![1],
            key: true,
            name: "fake".into(),
        });
        assert!(
            deliver_reply(
                result,
                Instant::now() - Duration::from_millis(1),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn cancelled_worker_never_delivers_a_completed_frame() {
        let result = Ok(Reply::Encoded {
            bytes: vec![1],
            key: true,
            name: "fake".into(),
        });
        assert!(deliver_reply(result, Instant::now() + BOUND, &AtomicBool::new(true)).is_err());
    }
}

/// Receiving-device interop; MF objects and check-in stay on their originating MTA.
mod decode_gpu {
    use super::*;
    use crosspane_media::picture::NativePicture;
    use std::{
        any::Any,
        collections::BTreeSet,
        fmt,
        sync::{Mutex, Weak},
    };
    use wgpu::hal::api::Dx12;
    use windows::Win32::Graphics::{
        Direct3D10::ID3D10Multithread,
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, ID3D11Device,
            ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
            ID3D11VideoDevice,
        },
        Direct3D11on12::{D3D11On12CreateDevice, ID3D11On12Device2},
        Direct3D12::{
            D3D12_FENCE_FLAG_NONE, D3D12_RESOURCE_DIMENSION_TEXTURE2D, ID3D12CommandQueue,
            ID3D12Device, ID3D12Fence, ID3D12Resource,
        },
        Dxgi::Common::DXGI_FORMAT_NV12,
    };

    pub type MfPathObserver = Arc<dyn Fn(MfDecodePath, Option<MfGpuFallback>) + Send + Sync>;

    #[derive(Clone)]
    pub(super) struct Host {
        pub generation: u64,
        pub device: wgpu::Device,
        pub queue: wgpu::Queue,
    }
    struct Bridge {
        host: Option<Host>,
        latest: Option<u64>,
        retired: BTreeSet<u64>,
        leases: Vec<Weak<Signal>>,
    }
    /// A metadata-only handshake with the actual receiving host device.
    pub struct MfDecodeGpu {
        state: Mutex<Bridge>,
        wake: Arc<dyn Fn() + Send + Sync>,
    }
    impl fmt::Debug for MfDecodeGpu {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("MfDecodeGpu").finish_non_exhaustive()
        }
    }
    impl MfDecodeGpu {
        pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
            Self {
                state: Mutex::new(Bridge {
                    host: None,
                    latest: None,
                    retired: BTreeSet::new(),
                    leases: Vec::new(),
                }),
                wake,
            }
        }
        pub fn set_host(&self, generation: u64, host: Option<(wgpu::Device, wgpu::Queue)>) {
            let leases = match self.state.lock() {
                Ok(mut state) => {
                    let update = model::gpu_host_update(
                        state.latest,
                        state.host.as_ref().map(|host| host.generation),
                        generation,
                        host.is_some(),
                        state.retired.contains(&generation),
                    );
                    match update {
                        model::GpuHostUpdate::Ignore => return,
                        model::GpuHostUpdate::Ready => {
                            if let Some(previous) = state.host.take() {
                                state.retired.insert(previous.generation);
                            }
                            state.host = host.map(|(device, queue)| Host {
                                generation,
                                device,
                                queue,
                            });
                        }
                        model::GpuHostUpdate::RetireCurrent => {
                            state.host = None;
                            state.retired.insert(generation);
                        }
                        model::GpuHostUpdate::RetireOther => {
                            state.retired.insert(generation);
                        }
                    }
                    state.latest = Some(state.latest.map_or(generation, |at| at.max(generation)));
                    state
                        .leases
                        .iter()
                        .filter_map(Weak::upgrade)
                        .filter(|signal| state.retired.contains(&signal.generation))
                        .collect::<Vec<_>>()
                }
                Err(_) => return,
            };
            for signal in leases {
                signal.quarantine_local(MfGpuFallback::DeviceLost);
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)()));
        }
        pub(super) fn host(&self) -> Option<Host> {
            let state = self.state.lock().ok()?;
            let host = state.host.as_ref()?;
            (!state.retired.contains(&host.generation)).then(|| host.clone())
        }
        pub(super) fn retire(&self, generation: u64, reason: MfGpuFallback) {
            let leases = match self.state.lock() {
                Ok(mut state) => {
                    if !state.retired.insert(generation) {
                        return;
                    }
                    state
                        .leases
                        .iter()
                        .filter_map(Weak::upgrade)
                        .filter(|signal| signal.generation == generation)
                        .collect::<Vec<_>>()
                }
                Err(_) => return,
            };
            for signal in leases {
                signal.quarantine_local(reason);
            }
        }
        fn reserve(&self, signal: &Arc<Signal>) -> Result<(), MfGpuFallback> {
            let mut state = self.state.lock().map_err(|_| MfGpuFallback::CopyFailed)?;
            state.leases.retain(|lease| lease.strong_count() != 0);
            if state.retired.contains(&signal.generation) {
                return Err(MfGpuFallback::DeviceLost);
            }
            if state
                .leases
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|lease| lease.state().is_none_or(|state| !state.returned))
                .count()
                >= model::GPU_COPY_JOBS
            {
                return Err(MfGpuFallback::PoolBusy);
            }
            state.leases.push(Arc::downgrade(signal));
            drop(state);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)()));
            Ok(())
        }
        /// Memory-only progress: the host itself performs the nonblocking Device::poll.
        pub fn poll_deadline(&self, now: Instant) -> Option<Instant> {
            let leases = self
                .state
                .lock()
                .ok()?
                .leases
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            let mut deadlines = Vec::new();
            for signal in leases {
                let Some(state) = signal.state() else {
                    continue;
                };
                if state.returned || state.quarantined {
                    continue;
                }
                if now >= signal.deadline {
                    self.retire(signal.generation, MfGpuFallback::Deadline);
                    signal.quarantine(MfGpuFallback::Deadline);
                } else {
                    deadlines.push(signal.deadline);
                }
            }
            model::gpu_poll_deadline(now, deadlines)
        }
        pub fn import_picture(
            &self,
            device: &wgpu::Device,
            picture: &dyn NativePicture,
        ) -> Result<[wgpu::Texture; 2], String> {
            let picture = picture
                .as_any()
                .downcast_ref::<MfGpuPicture>()
                .ok_or_else(|| "picture is not MF GPU output".to_owned())?;
            picture.signal.attempted.store(true, Ordering::Release);
            let result = self.import(device, picture);
            match result {
                Ok(planes) => {
                    picture.signal.notify(MfDecodePath::DxgiGpuCopy, None);
                    Ok(planes)
                }
                Err(reason) => {
                    self.retire(picture.signal.generation, reason);
                    picture.signal.cancel();
                    picture
                        .signal
                        .notify(MfDecodePath::AwaitingCpuIdr, Some(reason));
                    Err(reason.as_str().to_owned())
                }
            }
        }
        fn import(
            &self,
            device: &wgpu::Device,
            picture: &MfGpuPicture,
        ) -> Result<[wgpu::Texture; 2], MfGpuFallback> {
            let host = self.host().ok_or(MfGpuFallback::HostNotReady)?;
            model::gpu_decode_admission(
                Some(host.generation),
                picture.signal.generation,
                device == &host.device,
                device
                    .features()
                    .contains(wgpu::Features::TEXTURE_FORMAT_NV12),
            )?;
            let mut cached = picture
                .planes
                .lock()
                .map_err(|_| MfGpuFallback::CopyFailed)?;
            if let Some(planes) = &*cached {
                return Ok(planes.clone());
            }
            if Instant::now() >= picture.signal.deadline {
                return Err(MfGpuFallback::Deadline);
            }
            // SAFETY: guards only create/import resources on this exact retained host device.
            let hal = unsafe { device.as_hal::<Dx12>() }.ok_or(MfGpuFallback::UnsupportedDevice)?;
            // SAFETY: prepared NV12 storage is initialized, exclusive to this checkout,
            // and belongs to this exact device. One COPY_SRC wrapper tracks BOTH planes.
            let source = unsafe {
                let raw = wgpu::hal::dx12::Device::texture_from_raw(
                    picture.resource.clone(),
                    wgpu::TextureFormat::NV12,
                    wgpu::TextureDimension::D2,
                    wgpu::Extent3d {
                        width: picture.plan.storage.width,
                        height: picture.plan.storage.height,
                        depth_or_array_layers: picture.plan.layers,
                    },
                    1,
                    1,
                );
                device.create_texture_from_hal::<Dx12>(
                    raw,
                    &wgpu::TextureDescriptor {
                        label: Some("MF NV12 copy source"),
                        size: wgpu::Extent3d {
                            width: picture.plan.storage.width,
                            height: picture.plan.storage.height,
                            depth_or_array_layers: picture.plan.layers,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::NV12,
                        usage: wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::PRESENT,
                )
            };
            drop(hal);
            let make = |format, size: PixelSize| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("MF immutable decoded plane"),
                    size: wgpu::Extent3d {
                        width: size.width,
                        height: size.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
            };
            let size = picture.plan.area.size;
            let planes = model::allocate_gpu_planes(size, |uv, size| {
                make(
                    if uv {
                        wgpu::TextureFormat::Rg8Unorm
                    } else {
                        wgpu::TextureFormat::R8Unorm
                    },
                    size,
                )
            });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("MF split NV12 planes"),
            });
            for (plane, destination) in planes.iter().enumerate() {
                let ((x, y), size) = picture
                    .plan
                    .plane(plane as u32)
                    .ok_or(MfGpuFallback::InvalidSurface)?;
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &source,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x,
                            y,
                            z: picture.plan.slice,
                        },
                        aspect: if plane == 0 {
                            wgpu::TextureAspect::Plane0
                        } else {
                            wgpu::TextureAspect::Plane1
                        },
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: destination,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: size.width,
                        height: size.height,
                        depth_or_array_layers: 1,
                    },
                );
            }
            encoder.transition_resources(
                std::iter::empty(),
                std::iter::once(wgpu::TextureTransition {
                    texture: &source,
                    selector: None,
                    state: wgpu::TextureUses::PRESENT,
                }),
            );
            picture.signal.modify(|state| state.submit())??;
            // SAFETY: this is the exact retained host queue; only foreign fences are staged.
            let queue =
                unsafe { host.queue.as_hal::<Dx12>() }.ok_or(MfGpuFallback::UnsupportedDevice)?;
            queue.add_wait_fence(picture.fence.clone(), 1);
            queue.add_signal_fence(picture.fence.clone(), 2);
            drop(queue);
            host.queue.submit([encoder.finish()]);
            let signal = picture.signal.clone();
            host.queue.on_submitted_work_done(move || {
                // This is a wake, NOT proof of COMMON: the MTA also checks the native fence.
                signal.wake_control();
            });
            *cached = Some(planes.clone());
            picture.signal.wake_control();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)()));
            Ok(planes)
        }
    }

    #[derive(Default)]
    struct ObserverState {
        callback: Option<MfPathObserver>,
        outcome: Option<(MfDecodePath, Option<MfGpuFallback>)>,
        revision: u64,
        delivered: u64,
        draining: bool,
    }
    struct Signal {
        generation: u64,
        deadline: Instant,
        bridge: Weak<MfDecodeGpu>,
        lease: Mutex<model::GpuLeaseState>,
        attempted: AtomicBool,
        observer: Mutex<ObserverState>,
        control: mpsc::SyncSender<()>,
        wake: Arc<dyn Fn() + Send + Sync>,
    }
    impl Signal {
        fn state(&self) -> Option<model::GpuLeaseState> {
            self.lease.lock().ok().map(|s| *s)
        }
        fn modify<T>(
            &self,
            f: impl FnOnce(&mut model::GpuLeaseState) -> T,
        ) -> Result<T, MfGpuFallback> {
            self.lease
                .lock()
                .map(|mut state| f(&mut state))
                .map_err(|_| MfGpuFallback::CopyFailed)
        }
        fn wake_control(&self) {
            let _ = self.control.try_send(());
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)()));
        }
        fn cancel(&self) {
            let _ = self.modify(|state| state.cancelled = true);
            self.wake_control();
        }
        fn quarantine(&self, reason: MfGpuFallback) {
            self.quarantine_local(reason);
            if let Some(bridge) = self.bridge.upgrade() {
                bridge.retire(self.generation, reason);
            }
        }
        fn quarantine_local(&self, reason: MfGpuFallback) {
            let _ = self.modify(model::GpuLeaseState::quarantine);
            self.notify(MfDecodePath::AwaitingCpuIdr, Some(reason));
            self.wake_control();
        }
        fn notify(&self, path: MfDecodePath, reason: Option<MfGpuFallback>) {
            if let Ok(mut state) = self.observer.lock() {
                let outcome = model::gpu_path_update(state.outcome, (path, reason));
                if state.outcome == outcome {
                    return;
                }
                state.outcome = outcome;
                state.revision = state.revision.saturating_add(1);
            }
            self.dispatch();
        }
        fn dispatch(&self) {
            if let Ok(mut state) = self.observer.lock() {
                if state.draining {
                    return;
                }
                state.draining = true;
            } else {
                return;
            }
            loop {
                let next = match self.observer.lock() {
                    Ok(mut state) => {
                        if let Some(callback) = state.callback.clone()
                            && let Some(outcome) = state.outcome
                            && state.delivered != state.revision
                        {
                            state.delivered = state.revision;
                            Some((callback, outcome))
                        } else {
                            state.draining = false;
                            None
                        }
                    }
                    Err(_) => None,
                };
                let Some((callback, (path, reason))) = next else {
                    return;
                };
                // One drainer orders replay/retirement; callbacks run outside every mutex.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    callback(path, reason)
                }));
            }
        }
    }
    /// COM sample ownership is an MTA token. Reads cannot block or change rendering status.
    pub struct MfGpuPicture {
        plan: model::Nv12CopyPlan,
        colour: YuvColour,
        resource: ID3D12Resource,
        fence: ID3D12Fence,
        signal: Arc<Signal>,
        planes: Mutex<Option<[wgpu::Texture; 2]>>,
    }
    impl fmt::Debug for MfGpuPicture {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("MfGpuPicture")
                .field("size", &self.plan.area.size)
                .finish_non_exhaustive()
        }
    }
    impl MfGpuPicture {
        pub fn set_path_observer(&self, observer: MfPathObserver) {
            if let Ok(mut state) = self.signal.observer.lock() {
                state.callback = Some(observer);
                state.revision = state.revision.saturating_add(1);
            }
            self.signal.dispatch();
        }
    }
    impl NativePicture for MfGpuPicture {
        fn size(&self) -> PixelSize {
            self.plan.area.size
        }
        fn colour(&self) -> YuvColour {
            self.colour
        }
        fn to_nv12(&self, _out: &mut Nv12) -> Result<(), CodecError> {
            // No initial CPU cache: neither host fallback nor explicit snapshot can block/read back.
            model::native_cpu_cache(None, _out)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    impl Drop for MfGpuPicture {
        fn drop(&mut self) {
            let _ = self.signal.modify(|state| {
                state.picture_dropped = true;
                if !state.submitted {
                    state.cancelled = true;
                }
            });
            self.signal.wake_control();
        }
    }

    /// Constructed and used only on the existing COM MTA.
    pub(super) struct GpuMta {
        pub host: Host,
        device12: ID3D12Device,
        queue12: ID3D12CommandQueue,
        device11: ID3D11Device,
        context11: ID3D11DeviceContext,
        on12: ID3D11On12Device2,
        pub manager: IMFDXGIDeviceManager,
    }
    impl GpuMta {
        pub fn new(host: Host) -> Result<Self, CodecError> {
            if !host
                .device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
            {
                return Err(failure("receiving device has no NV12 feature"));
            }
            // SAFETY: only cloned native handles are kept; guards never cross a thread.
            let guard = unsafe { host.device.as_hal::<Dx12>() }
                .ok_or_else(|| failure("receiving device is not DX12"))?;
            let device12 = guard.raw_device().clone();
            let queue12 = guard.raw_queue().clone();
            drop(guard);
            let mut device11 = None;
            let mut context11 = None;
            let queues = [Some(api(queue12.cast(), "receiving queue IUnknown")?)];
            // SAFETY: exact retained DX12 device/DIRECT queue, one node; outputs are owned.
            api(
                // SAFETY: exact retained DX12 device/DIRECT queue; owned output slots.
                unsafe {
                    D3D11On12CreateDevice(
                        &device12,
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
                        None,
                        Some(&queues),
                        0,
                        Some(&mut device11),
                        Some(&mut context11),
                        None,
                    )
                },
                "D3D11On12CreateDevice",
            )?;
            let device11 = device11.ok_or_else(|| failure("missing D3D11-on-12 device"))?;
            let context11 = context11.ok_or_else(|| failure("missing D3D11-on-12 context"))?;
            let on12 = api(device11.cast::<ID3D11On12Device2>(), "ID3D11On12Device2")?;
            let video = api(device11.cast::<ID3D11VideoDevice>(), "ID3D11VideoDevice")?;
            let _: ID3D11VideoContext = api(context11.cast(), "ID3D11VideoContext")?;
            // SAFETY: driver-owned profile capability query; no decoder/resource is guessed.
            let supports = unsafe {
                video.CheckVideoDecoderFormat(
                    &windows::Win32::Graphics::Direct3D11::D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
                    DXGI_FORMAT_NV12,
                )
            };
            if !api(supports, "H.264 NV12 decode capability")?.as_bool() {
                return Err(failure("device cannot decode H.264 NV12"));
            }
            let multithread = api(device11.cast::<ID3D10Multithread>(), "ID3D10Multithread")?;
            // SAFETY: context-wide synchronization is enabled before any MFT sees this device.
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
            let mut token = 0;
            let mut manager = None;
            // SAFETY: output owners remain on this MTA and ResetDevice uses the exact device.
            unsafe {
                api(
                    MFCreateDXGIDeviceManager(&mut token, &mut manager),
                    "MFCreateDXGIDeviceManager",
                )?;
            }
            let manager = manager.ok_or_else(|| failure("missing MF DXGI manager"))?;
            // SAFETY: token belongs to this newly created manager; device lives with it.
            api(
                // SAFETY: this manager's creation token and retained matching device.
                unsafe { manager.ResetDevice(&device11, token) },
                "DXGI ResetDevice",
            )?;
            Ok(Self {
                host,
                device12,
                queue12,
                device11,
                context11,
                on12,
                manager,
            })
        }
        #[allow(clippy::too_many_arguments)]
        pub fn prepare(
            &self,
            bridge: &Arc<MfDecodeGpu>,
            sample: IMFSample,
            area: model::Aperture,
            expected: model::Aperture,
            colour: YuvColour,
            control: mpsc::SyncSender<()>,
            deadline: Instant,
            leases: &mut Vec<SourceLease>,
        ) -> Result<Arc<MfGpuPicture>, CodecError> {
            model::gpu_capacity(leases.len(), model::GPU_SOURCE_LEASES)
                .map_err(|reason| failure(reason.as_str()))?;
            // SAFETY: one owned output sample; never read CPU pixels or expose COM sample pointers.
            let count = api(
                unsafe { sample.GetBufferCount() },
                "GPU output buffer count",
            )?;
            if count != 1 {
                return Err(failure("GPU output must have one DXGI buffer"));
            }
            // SAFETY: verified existing buffer index; each returned interface is an owned reference.
            let buffer = api(unsafe { sample.GetBufferByIndex(0) }, "GPU output buffer")?;
            let dxgi = api(buffer.cast::<IMFDXGIBuffer>(), "IMFDXGIBuffer")?;
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: typed out-interface storage, matching IID; retain it on this MTA.
            api(
                // SAFETY: matching IID and typed out-interface storage retained on the MTA.
                unsafe {
                    dxgi.GetResource(
                        &ID3D11Texture2D::IID,
                        &mut texture as *mut _ as *mut *mut std::ffi::c_void,
                    )
                },
                "DXGI texture",
            )?;
            let texture = texture.ok_or_else(|| failure("missing DXGI texture"))?;
            // SAFETY: owned decoder texture/metadata; no foreign resources are queried.
            let (device, slice) = unsafe {
                (
                    api(texture.GetDevice(), "DXGI texture device")?,
                    api(dxgi.GetSubresourceIndex(), "DXGI texture surface index")?,
                )
            };
            if device != self.device11 {
                return Err(failure("DXGI device mismatch"));
            }
            let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
            // SAFETY: writable initialized descriptor borrowed for the live texture query.
            unsafe {
                texture.GetDesc(&mut desc);
            }
            if desc.Format != DXGI_FORMAT_NV12 || desc.MipLevels != 1 || desc.SampleDesc.Count != 1
            {
                return Err(failure("DXGI NV12 descriptor mismatch"));
            }
            let plan = model::Nv12CopyPlan::new(
                PixelSize::new(desc.Width, desc.Height),
                desc.ArraySize,
                slice,
                area,
                expected,
            )
            .map_err(|reason| failure(reason.as_str()))?;
            let resource11 = api(texture.cast::<ID3D11Resource>(), "D3D11 resource")?;
            // SAFETY: a new private fence on the exact host device; no shared handle escapes.
            let fence: ID3D12Fence = api(
                unsafe { self.device12.CreateFence(0, D3D12_FENCE_FLAG_NONE) },
                "GPU copy fence",
            )?;
            let signal = Arc::new(Signal {
                generation: self.host.generation,
                deadline,
                bridge: Arc::downgrade(bridge),
                lease: Mutex::new(model::GpuLeaseState {
                    picture_dropped: true,
                    ..model::GpuLeaseState::reserved()
                }),
                attempted: AtomicBool::new(false),
                observer: Mutex::new(ObserverState::default()),
                control,
                wake: bridge.wake.clone(),
            });
            bridge
                .reserve(&signal)
                .map_err(|reason| failure(reason.as_str()))?;
            // Reservation is visible BEFORE checkout/publication/another ProcessInput.
            leases.push(SourceLease {
                _sample: sample,
                resource11,
                on12: self.on12.clone(),
                device12: self.device12.clone(),
                fence: fence.clone(),
                signal: signal.clone(),
                checked_out: true,
            });
            let index = leases.len() - 1;
            let prepared = (|| {
                // SAFETY: the MTA execution gate owns the decoder resource exclusively; pending
                // work is waited on by the exact host queue. No further MFT input runs until return.
                let raw = unsafe {
                    self.on12.UnwrapUnderlyingResource::<_, _, ID3D12Resource>(
                        &leases[index].resource11,
                        &self.queue12,
                    )
                };
                let raw = match api(raw, "UnwrapUnderlyingResource") {
                    Ok(raw) => raw,
                    Err(error) => {
                        signal.quarantine(MfGpuFallback::CopyFailed);
                        return Err(error);
                    }
                };
                // SAFETY: flush/signal the unwrap prefix before publishing its prepared token.
                unsafe {
                    self.context11.Flush();
                    api(self.queue12.Signal(&fence, 1), "GPU checkout ready fence")?;
                }
                // SAFETY: the unwrapped owned resource is inspected before constructing any wrapper.
                let desc = unsafe { raw.GetDesc() };
                let mut raw_device = None::<ID3D12Device>;
                // SAFETY: matching device interface storage; retain and compare COM identity.
                api(
                    // SAFETY: typed device output slot for the retained unwrapped resource.
                    unsafe { raw.GetDevice(&mut raw_device) },
                    "unwrapped device",
                )?;
                if raw_device.as_ref() != Some(&self.device12)
                    || desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
                    || desc.Format != DXGI_FORMAT_NV12
                    || desc.MipLevels != 1
                    || desc.SampleDesc.Count != 1
                    || desc.Width != u64::from(plan.storage.width)
                    || desc.Height != plan.storage.height
                    || u32::from(desc.DepthOrArraySize) != plan.layers
                {
                    signal.cancel();
                    return Err(failure("unwrapped resource does not match MF surface"));
                }
                signal
                    .modify(|state| state.picture_dropped = false)
                    .map_err(|reason| failure(reason.as_str()))?;
                Ok(Arc::new(MfGpuPicture {
                    plan,
                    colour,
                    resource: raw,
                    fence,
                    signal: signal.clone(),
                    planes: Mutex::new(None),
                }))
            })();
            if prepared.is_err() {
                // Cancellation does not claim ready completion: poll still requires the native
                // ready fence and successful return; failed/unknown unwrap remains quarantined.
                signal.cancel();
                signal.quarantine(MfGpuFallback::CopyFailed);
            }
            prepared
        }
    }
    /// Samples, device-manager objects and return calls never cross the originating MTA.
    pub(super) struct SourceLease {
        _sample: IMFSample,
        resource11: ID3D11Resource,
        on12: ID3D11On12Device2,
        device12: ID3D12Device,
        fence: ID3D12Fence,
        signal: Arc<Signal>,
        checked_out: bool,
    }
    impl SourceLease {
        pub fn poll(&mut self) -> bool {
            let Some(mut state) = self.signal.state() else {
                return false;
            };
            if self.checked_out {
                // SAFETY: private owned fence, nonblocking query; UINT64_MAX means removed device.
                let completed = unsafe { self.fence.GetCompletedValue() };
                if completed == u64::MAX {
                    self.signal.quarantine(MfGpuFallback::DeviceLost);
                    return false;
                }
                if state.submitted && completed >= 2 {
                    let _ = self.signal.modify(|state| state.completion = true);
                    state.completion = true;
                }
                if state.check_in_ready(completed >= 1) {
                    // SAFETY: the exact retained native device must still be usable; failure
                    // preserves sample/resource ownership rather than fabricating completion.
                    if unsafe { self.device12.GetDeviceRemovedReason() }.is_err() {
                        self.signal.quarantine(MfGpuFallback::DeviceLost);
                        return false;
                    }
                    // SAFETY: completed ready/copy fence proves COMMON and no pending use;
                    // zero sync fences suffice because completion was observed, not assumed.
                    let result = unsafe {
                        self.on12.ReturnUnderlyingResource(
                            &self.resource11,
                            0,
                            ptr::null(),
                            ptr::null(),
                        )
                    };
                    if result.is_err() {
                        self.signal.quarantine(MfGpuFallback::CopyFailed);
                        return false;
                    }
                    self.checked_out = false;
                    let _ = self.signal.modify(model::GpuLeaseState::return_completed);
                    self.signal.wake_control();
                }
            }
            self.signal
                .state()
                .is_some_and(model::GpuLeaseState::release_sample)
        }
        pub fn quarantine(&self) {
            self.signal.quarantine(MfGpuFallback::CopyFailed);
        }
        pub fn blocks_input(&self) -> bool {
            self.checked_out
        }
        pub fn quarantined(&self) -> bool {
            !self.checked_out || self.signal.state().is_none_or(|state| state.quarantined)
        }
    }
}
