//! CPU-frame, realtime H.264 through public Media Foundation transforms.
//!
//! Each facade serializes jobs on its own MTA. Hardware transforms are tried first;
//! incompatible hardware falls back to the inbox Microsoft software codec. Native
//! jobs and event waits have a two-second deadline. A native call already running
//! cannot be preempted; a timed-out facade is retired and never accepts another frame.
//! GPU input/output and D3D zero-copy are deliberately left to WP-W2.3b.
#![allow(unsafe_code)]

use crate::model::video::{self as model, Clock, Events, Headers, Params, References};
use crosspane_media::{
    codec::{CodecError, EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder},
    picture::{Nv12, YuvColour, YuvMatrix, nv12_to_bgra},
};
use crosspane_types::geom::PixelSize;
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
pub struct MfCodecs;
impl MfCodecs {
    pub fn new() -> Self {
        Self
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
        Ok(Box::new(self.encoder_cpu(size, bits_per_second, fps)?))
    }
    fn decoder(&self) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Ok(Box::new(self.decoder_cpu()?))
    }
}

/// CPU encoder; all COM objects remain on its worker, without unsafe Send wrappers.
pub struct MfEncoder {
    worker: Worker,
    name: String,
    bitrate: u32,
    fps: u32,
    clock: Clock,
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
}
impl Worker {
    fn new(mode: Mode) -> Result<(Self, String), CodecError> {
        let (send, receive) = mpsc::sync_channel::<Envelope>(1);
        let (started, start) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let deadline = Instant::now() + BOUND;
        let join = thread::Builder::new()
            .name("crosspane-mf".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = match Runtime::new() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = started.try_send(Err(error));
                            return;
                        }
                    };
                    let mut codec = match Native::new(mode, deadline, &flag) {
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
                    while let Ok(envelope) = receive.recv() {
                        if check(envelope.deadline, &flag).is_err() {
                            break;
                        }
                        let result = codec.job(envelope.job, envelope.deadline, &flag);
                        let result = check(envelope.deadline, &flag).and(result);
                        let _ = envelope.reply.try_send(result);
                    }
                    drop(codec);
                    drop(runtime);
                }));
                let _ = finished.try_send(());
            })
            .map_err(|error| CodecError::Unavailable(error.to_string()))?;
        let mut worker = Self {
            send: Some(send),
            stop,
            done,
            join: Some(join),
        };
        match start.recv_timeout(BOUND) {
            Ok(Ok(name)) => Ok((worker, name)),
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
}
impl Native {
    fn new(mode: Mode, deadline: Instant, stop: &AtomicBool) -> Result<Self, CodecError> {
        Ok(Self {
            session: Session::select(mode, false, deadline, stop)?,
            mode,
            first: true,
            headers: Headers::default(),
            references: References::default(),
            aperture: None,
        })
    }
    fn job(&mut self, job: Job, deadline: Instant, stop: &AtomicBool) -> Result<Reply, CodecError> {
        let result = match job {
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
        self.references.check(bytes)?;
        let idr = model::nals(bytes).any(|n| n[0] & 0x1f == 5);
        if self.first && !idr {
            return Err(failure("decoder requires IDR"));
        }
        if idr {
            self.aperture = Some(model::h264_aperture(bytes)?);
        } else if model::nals(bytes).any(|nal| nal[0] & 31 == 7)
            && self.aperture != Some(model::h264_aperture(bytes)?)
        {
            return Err(failure("geometry change requires IDR"));
        }
        let aperture = self
            .aperture
            .ok_or_else(|| failure("missing coded geometry"))?;
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
    fn configure(
        transform: IMFTransform,
        hardware: bool,
        name: String,
        mode: Mode,
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
    fn event(&mut self, deadline: Instant, stop: &AtomicBool) -> Result<(), CodecError> {
        let generator = self
            .events
            .as_ref()
            .ok_or_else(|| failure("missing async event generator"))?;
        loop {
            check(deadline, stop)?;
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
        let asynchronous = self.events.is_some();
        if asynchronous {
            while !self.credits.can_submit() {
                self.event(deadline, stop)?;
            }
            self.credits.submit(at)?;
        }
        check(deadline, stop)?;
        api(
            // SAFETY: prepared stream, owned sample; async input credit was consumed exactly once.
            unsafe { self.transform.ProcessInput(self.input, input, 0) },
            "ProcessInput",
        )?;
        for _ in 0..4 {
            if asynchronous {
                while !self.credits.can_output() {
                    self.event(deadline, stop)?;
                }
            }
            check(deadline, stop)?;
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
