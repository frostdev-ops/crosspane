//! Ignored, Limited-only owned playback probe. Build this target with the read-only P9g
//! compatibility manifest; never run it from elevated cargo. No whole-endpoint loopback.
//! Only scalar receipts are retained. One trial per invocation; no retry after failure.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{AudioHost, AudioKind, IoGate};
use crosspane_platform_windows::model;
#[path = "../src/audio.rs"]
mod native;
use native::WindowsAudioHost;
use std::{
    ffi::c_void,
    io::{BufRead, BufReader, Read, Write},
    mem::{self, ManuallyDrop},
    os::windows::io::AsRawHandle,
    process::{Child, Command, Stdio},
    ptr::{self, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HANDLE as ComHandle,
        Media::Audio::*,
        System::Com::{
            BLOB, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
            StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
        },
        System::Variant::VT_BLOB,
    },
    core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{
        JobObjects::*, SystemInformation::*, SystemServices::VER_GREATER_EQUAL, Threading::*,
    },
};

type ProbeResult<T> = Result<T, &'static str>;
fn sdk<T>(result: windows::core::Result<T>) -> ProbeResult<T> {
    result.map_err(|_| "WASAPI_FAILED")
}
struct Handle(windows_sys::Win32::Foundation::HANDLE);
// SAFETY: immutable owned kernel handles support cross-thread event/job operations.
unsafe impl Send for Handle {}
// SAFETY: Arc controls the one close after all own users finish.
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: final owner of this successful handle, never an arbitrary supplied handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn event() -> ProbeResult<Handle> {
    // SAFETY: unnamed, non-inherited manual-reset event in the fixture only.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        Err("EVENT_FAILED")
    } else {
        Ok(Handle(handle))
    }
}
fn limited() -> bool {
    let mut token = null_mut();
    // SAFETY: read-only own process token; never touches another process's token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    // SAFETY: initialized exact own-token output and retained handle.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        ) != 0
            && size == mem::size_of::<TOKEN_ELEVATION>() as u32
            && elevation.TokenIsElevated == 0
    }
}
fn supported_version() -> bool {
    let mut version = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: mem::size_of::<OSVERSIONINFOEXW>() as u32,
        dwMajorVersion: 10,
        dwMinorVersion: 0,
        dwBuildNumber: 20348,
        ..Default::default()
    };
    let mut mask = 0;
    for kind in [VER_MAJORVERSION, VER_MINORVERSION, VER_BUILDNUMBER] {
        // SAFETY: public pure condition-mask construction.
        mask = unsafe { VerSetConditionMask(mask, kind, VER_GREATER_EQUAL as u8) };
    }
    // SAFETY: public version refusal using the probe-only Windows10 supportedOS manifest.
    unsafe {
        VerifyVersionInfoW(
            &mut version,
            VER_MAJORVERSION | VER_MINORVERSION | VER_BUILDNUMBER,
            mask,
        ) != 0
    }
}
struct Apartment;
impl Apartment {
    fn new() -> ProbeResult<Self> {
        // SAFETY: owned test thread enters MTA, balanced after every COM owner drops.
        sdk(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() })?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's initialization after capture resources retire.
        unsafe {
            CoUninitialize();
        }
    }
}
struct Watchdog {
    done: Arc<AtomicBool>,
    tone_until: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new(job: Option<Arc<Handle>>, milliseconds: u64) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let tone_until = Arc::new(AtomicU64::new(0));
        let stop = done.clone();
        let tone = tone_until.clone();
        let handle = thread::spawn(move || {
            // SAFETY: monotonic public clock, no settings or owner observation.
            let end = unsafe { GetTickCount64() }.saturating_add(milliseconds);
            while !stop.load(Ordering::Acquire) {
                // SAFETY: read-only monotonic clock.
                let now = unsafe { GetTickCount64() };
                let tone_deadline = tone.load(Ordering::Acquire);
                if now >= end || tone_deadline != 0 && now >= tone_deadline {
                    // SAFETY: only the genuine newly created child's job, or this child itself.
                    unsafe {
                        if let Some(job) = &job {
                            TerminateJobObject(job.0, 124);
                        } else {
                            TerminateProcess(GetCurrentProcess(), 124);
                        }
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        Self {
            done,
            tone_until,
            thread: Some(handle),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[repr(C)]
struct Activation {
    table: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
    refs: AtomicU32,
    done: Arc<Handle>,
    parameters: AUDIOCLIENT_ACTIVATION_PARAMS,
    variant: PROPVARIANT,
}
unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return HRESULT(0x80004003u32 as i32);
    }
    // SAFETY: valid COM iid/output; this free-threaded atomics/event-only object is agile.
    unsafe {
        *out = null_mut();
        if *iid == IUnknown::IID
            || *iid == IActivateAudioInterfaceCompletionHandler::IID
            || *iid == GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90)
        {
            *out = this;
            add_ref(this);
            HRESULT(0)
        } else {
            HRESULT(0x80004002u32 as i32)
        }
    }
}
unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: live owned activation object with atomic COM reference count.
    unsafe {
        (&*this.cast::<Activation>())
            .refs
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: final COM release alone reclaims the exact original boxed object.
    let n = unsafe {
        (&*this.cast::<Activation>())
            .refs
            .fetch_sub(1, Ordering::AcqRel)
            - 1
    };
    if n == 0 {
        // SAFETY: no caller/operation reference remains; blob points within this same Box.
        unsafe {
            drop(Box::from_raw(this.cast::<Activation>()));
        }
    }
    n
}
unsafe extern "system" fn complete(this: *mut c_void, _: *mut c_void) -> HRESULT {
    // SAFETY: OS retains handler until callback; only owned event signaling, no data/logging.
    unsafe {
        SetEvent((&*this.cast::<Activation>()).done.0);
    }
    HRESULT(0)
}
static ACTIVATION: IActivateAudioInterfaceCompletionHandler_Vtbl =
    IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: IUnknown_Vtbl {
            QueryInterface: query,
            AddRef: add_ref,
            Release: release,
        },
        ActivateCompleted: complete,
    };
fn activate_own() -> ProbeResult<IAudioClient> {
    let done = Arc::new(event()?);
    let mut object = Box::new(Activation {
        table: &ACTIVATION,
        refs: AtomicU32::new(1),
        done: done.clone(),
        parameters: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    // SAFETY: genuine fixture PID; no arbitrary PID parameter is accepted.
                    TargetProcessId: unsafe { GetCurrentProcessId() },
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        },
        variant: PROPVARIANT::default(),
    });
    object.variant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut object.parameters as *mut AUDIOCLIENT_ACTIVATION_PARAMS)
                            .cast(),
                    },
                },
                ..Default::default()
            }),
        },
    };
    let pointer = Box::into_raw(object);
    // SAFETY: reprC exact vtable pointer, handler takes initial COM reference.
    let handler = unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(pointer.cast()) };
    // SAFETY: heap parameters/event live with handler and OS operation through late completion.
    let operation = sdk(unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&(*pointer).variant),
            &handler,
        )
    })?;
    // SAFETY: retained event; independent child watchdog contains native stalls.
    if unsafe { WaitForSingleObject(done.0, 1000) } != WAIT_OBJECT_0 {
        return Err("ACTIVATION_TIMEOUT");
    }
    let mut result = HRESULT(0x8000000au32 as i32);
    let mut unknown = None;
    // SAFETY: owned completed async operation and initialized outputs in this MTA.
    sdk(unsafe { operation.GetActivateResult(&mut result, &mut unknown) })?;
    if result.is_err() {
        return Err("ACTIVATION_REFUSED");
    }
    sdk(unknown.ok_or("ACTIVATION_EMPTY")?.cast())
}
struct Capture {
    // Keep registered event alive until Stop and COM-interface release complete.
    packets: IAudioCaptureClient,
    client: IAudioClient,
    event: Arc<Handle>,
    running: bool,
}
impl Capture {
    fn open() -> ProbeResult<Self> {
        let event = Arc::new(event()?);
        let client = activate_own()?;
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 384_000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: own-tree capture only, shared/event-driven, no endpoint or microphone capture.
        sdk(unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                0,
                0,
                &format,
                None,
            )
        })?;
        // SAFETY: event retained through client destruction, exact service requested.
        sdk(unsafe { client.SetEventHandle(ComHandle(event.0)) })?;
        // SAFETY: capture service belongs to this initialized own-process client.
        let packets = sdk(unsafe { client.GetService::<IAudioCaptureClient>() })?;
        // SAFETY: successfully initialized own-process shared capture.
        sdk(unsafe { client.Start() })?;
        Ok(Self {
            packets,
            client,
            event,
            running: true,
        })
    }
    fn finish(mut self) -> ProbeResult<()> {
        self.running = false;
        // SAFETY: exact own initialized capture; failure is reported without a blind second Stop.
        sdk(unsafe { self.client.Stop() })
    }
    fn drain(&self, stats: &mut Stats) -> ProbeResult<()> {
        for _ in 0..256 {
            // SAFETY: own completed capture service; data never leaves aggregate counters.
            if sdk(unsafe { self.packets.GetNextPacketSize() })? == 0 {
                return Ok(());
            }
            let mut bytes = null_mut();
            let mut frames = 0;
            let mut flags = 0;
            // SAFETY: exact initialized outputs and same-thread packet acquire/release.
            sdk(unsafe {
                self.packets
                    .GetBuffer(&mut bytes, &mut frames, &mut flags, None, None)
            })?;
            struct Packet<'a>(&'a IAudioCaptureClient, u32, bool);
            impl Packet<'_> {
                fn release(mut self) -> ProbeResult<()> {
                    self.2 = true;
                    // SAFETY: exactly one release; an uncertain failed release is never retried.
                    sdk(unsafe { self.0.ReleaseBuffer(self.1) })
                }
            }
            impl Drop for Packet<'_> {
                fn drop(&mut self) {
                    if !self.2 {
                        // SAFETY: error unwinding releases the outstanding own packet only once.
                        let _ = unsafe { self.0.ReleaseBuffer(self.1) };
                    }
                }
            }
            let packet = Packet(&self.packets, frames, false);
            if frames > 192_000 {
                return Err("CAPTURE_BOUND");
            }
            stats.frames += u64::from(frames);
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                if bytes.is_null() {
                    return Err("CAPTURE_NULL");
                }
                // SAFETY: negotiated stereo32 packet has frames*8 valid bytes; unaligned copies.
                let data = unsafe { std::slice::from_raw_parts(bytes, frames as usize * 8) };
                for sample in data.as_chunks::<4>().0 {
                    let value = f32::from_le_bytes(*sample);
                    if !value.is_finite() {
                        return Err("CAPTURE_NONFINITE");
                    }
                    stats.peak = stats.peak.max(value.abs());
                    if value.abs() > 0.000_01 {
                        stats.nonzero += 1;
                        stats.last_nonzero = Some(Instant::now());
                    }
                }
            }
            packet.release()?;
        }
        Err("CAPTURE_QUEUE_BOUND")
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if self.running {
            self.running = false;
            // SAFETY: initialized own-process capture, no retry or settings calls.
            let _ = unsafe { self.client.Stop() };
        }
        let _ = &self.event;
    }
}
#[derive(Default)]
struct Stats {
    frames: u64,
    nonzero: u64,
    peak: f32,
    last_nonzero: Option<Instant>,
}

fn child() -> ProbeResult<()> {
    if !limited() {
        return Err("ELEVATED_REFUSED");
    }
    let mut begin = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut begin)
        .map_err(|_| "HANDSHAKE_FAILED")?;
    if begin != "OWNED_JOB_READY\n" {
        return Err("HANDSHAKE_REFUSED");
    }
    let watchdog = Watchdog::new(None, 14_000);
    println!("AUDIO_CHILD_EARLY limited=1 owned_job_handshake=1 trials_planned=1");
    std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
    if !supported_version() {
        println!("AUDIO_VERSION_REFUSED trials_started=0");
        return Ok(());
    }
    let _apartment = Apartment::new()?;
    // SAFETY: read-only default render presence; no other-process/device/settings access.
    let enumerator: IMMDeviceEnumerator =
        sdk(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) })?;
    // SAFETY: default console render observation only; absence ends measurements.
    if unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.is_err() {
        println!("AUDIO_NO_ENDPOINT trials_started=0");
        return Ok(());
    }
    println!("AUDIO_ENDPOINT_PRESENT");
    // Lead1db841c4 admits exactly one resumed trial, the second STARTED trial overall.
    {
        let number = 2;
        println!("AUDIO_TRIAL_STARTED trial={number} source_peak_dbfs=-60");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let mut host = WindowsAudioHost::new(gate).map_err(|_| "HOST_FAILED")?;
        host.subscribe(Arc::new(|_| {}))
            .map_err(|_| "SUBSCRIBE_FAILED")?;
        println!("AUDIO_STAGE trial={number} stage=own_capture_opening");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        let capture = Capture::open()?;
        println!("AUDIO_STAGE trial={number} stage=playback_opening");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        let mut playback = host
            .open_playback(AudioKind::Speaker.format())
            .map_err(|_| "OPEN_FAILED")?;
        println!("AUDIO_STAGE trial={number} stage=playback_opened");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        let mut stats = Stats::default();
        let start = Instant::now();
        // SAFETY: monotonic clock only; independent watchdog begins before the first sample.
        watchdog.tone_until.store(
            unsafe { GetTickCount64() }.saturating_add(1900),
            Ordering::Release,
        );
        let mut phase = 0u64;
        println!("AUDIO_STAGE trial={number} stage=pcm_submission");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        while start.elapsed() < Duration::from_millis(1200) {
            while playback.pcm.slots() >= 960 {
                for _ in 0..480 {
                    let sample = (std::f32::consts::TAU * (phase % 48) as f32 / 48.0).sin() * 0.001;
                    phase += 1;
                    playback.pcm.push(sample).map_err(|_| "PCM_CAPACITY")?;
                    playback.pcm.push(sample).map_err(|_| "PCM_CAPACITY")?;
                }
            }
            capture.drain(&mut stats)?;
            thread::sleep(Duration::from_millis(2));
        }
        let stopped_at = Instant::now();
        println!("AUDIO_STAGE trial={number} stage=playback_dropping");
        std::io::stdout().flush().map_err(|_| "OUTPUT_FAILED")?;
        drop(playback);
        let drop_ms = stopped_at.elapsed().as_secs_f64() * 1000.0;
        watchdog.tone_until.store(0, Ordering::Release);
        let before_tail = stats.frames;
        while stopped_at.elapsed() < Duration::from_millis(300) {
            capture.drain(&mut stats)?;
            thread::sleep(Duration::from_millis(2));
        }
        let last_ms = stats
            .last_nonzero
            .map(|t| {
                if t >= stopped_at {
                    t.duration_since(stopped_at).as_secs_f64() * 1000.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        println!(
            "AUDIO_METRICS trial={number} frames={} nonzero={} peak={:.7} drop_ms={drop_ms:.3} tail_frames={} last_nonzero_receipt_after_drop_ms={last_ms:.3}",
            stats.frames,
            stats.nonzero,
            stats.peak,
            stats.frames - before_tail
        );
        let renderer_retired = host.fixture_retired();
        capture.finish()?;
        drop(host);
        println!(
            "AUDIO_CLIENTS_RETIRED trial={number} renderer_thread_retired={renderer_retired} capture_stop_ok=1"
        );
        if !renderer_retired {
            return Err("RENDERER_NOT_RETIRED");
        }
        if drop_ms > 50.0 {
            return Err("STOP_BOUND_FAILED");
        }
        if stats.frames == 0 || stats.nonzero == 0 {
            return Err("NO_OWN_FRAMES");
        }
        if stats.peak > 0.001_1 {
            return Err("OWN_LEVEL_UNEXPECTED");
        }
        if last_ms > 250.0 {
            return Err("TAIL_NOT_SETTLED");
        }
    }
    Ok(())
}

struct OwnedChild {
    child: Child,
    job: Arc<Handle>,
    assigned: bool,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: only the genuine child created below (or its assigned private job).
            unsafe {
                if self.assigned {
                    TerminateJobObject(self.job.0, 124);
                } else {
                    TerminateProcess(self.child.as_raw_handle(), 124);
                }
                WaitForSingleObject(self.child.as_raw_handle(), 1000);
            }
        }
    }
}

#[test]
#[ignore = "Limited own shared tone only; global win-gui lock and probe manifest required"]
fn owned_shared_playback_probe() {
    std::panic::set_hook(Box::new(|_| eprintln!("AUDIO_PROBE_FIXED_FAILURE")));
    assert!(limited(), "ELEVATED_REFUSED");
    // SAFETY: unnamed own job; flags constrain only the genuine newly created child.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    assert!(!raw.is_null(), "JOB_FAILED");
    let job = Arc::new(Handle(raw));
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    // SAFETY: exact owned job and initialized size; no existing owner process is assigned.
    assert_ne!(
        // SAFETY: exact owned job and initialized limits, no owner process is assigned.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                mem::size_of_val(&limits) as u32,
            )
        },
        0,
        "JOB_LIMIT_FAILED"
    );
    let spawned = Command::new(std::env::current_exe().expect("OWN_EXE"))
        .args([
            "--exact",
            "owned_audio_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CROSSPANE_AUDIO_CHILD", "owned")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("OWN_CHILD");
    let mut owned = OwnedChild {
        child: spawned,
        job: job.clone(),
        assigned: false,
    };
    // SAFETY: handle belongs to the exact new child, which blocks on its private stdin handshake.
    assert_ne!(
        // SAFETY: the newly spawned child is blocked on its private stdin handshake.
        unsafe { AssignProcessToJobObject(job.0, owned.child.as_raw_handle()) },
        0,
        "JOB_ASSIGN_FAILED"
    );
    owned.assigned = true;
    let _watchdog = Watchdog::new(Some(job.clone()), 15_000);
    owned
        .child
        .stdin
        .take()
        .expect("OWN_STDIN")
        .write_all(b"OWNED_JOB_READY\n")
        .expect("OWN_BEGIN");
    let stdout = owned.child.stdout.take().expect("OWN_STDOUT");
    let stderr = owned.child.stderr.take().expect("OWN_STDERR");
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        BufReader::new(stdout)
            .take(16_385)
            .read_to_end(&mut output)
            .expect("OWN_OUTPUT");
        output
    });
    let errors = thread::spawn(move || {
        let mut output = Vec::new();
        BufReader::new(stderr)
            .take(4097)
            .read_to_end(&mut output)
            .expect("OWN_ERROR_OUTPUT");
        output
    });
    let status = owned.child.wait().expect("OWN_WAIT");
    let output = reader.join().expect("OWN_READER");
    let errors = errors.join().expect("OWN_ERROR_READER");
    assert!(
        output.len() <= 16_384 && errors.len() <= 4096,
        "OWN_OUTPUT_BOUND"
    );
    print!("{}", String::from_utf8(output).expect("OWN_OUTPUT_UTF8"));
    if !errors.is_empty() {
        println!("AUDIO_CHILD_ERROR_BYTES count={}", errors.len());
    }
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: own job accounting only, exact initialized output.
    let okay = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            mem::size_of_val(&accounting) as u32,
            null_mut(),
        )
    } != 0;
    println!(
        "AUDIO_CLEANUP child_exited=1 job_empty={} child_success={} child_exit_code={:?}",
        okay && accounting.ActiveProcesses == 0,
        status.success(),
        status.code()
    );
    assert!(okay && accounting.ActiveProcesses == 0, "OWN_JOB_NOT_EMPTY");
    assert!(status.success(), "OWN_CHILD_FAILED");
}

#[test]
#[ignore = "internal genuine child entry; no endpoint access without Limited+private handshake"]
fn owned_audio_child() {
    std::panic::set_hook(Box::new(|_| eprintln!("AUDIO_CHILD_FIXED_FAILURE")));
    assert_eq!(
        std::env::var("CROSSPANE_AUDIO_CHILD").ok().as_deref(),
        Some("owned"),
        "OWN_CHILD_REFUSED"
    );
    if let Err(code) = child() {
        println!("AUDIO_TRIAL_FAILED code={code}");
        panic!("OWN_TRIAL_FAILED");
    }
}
