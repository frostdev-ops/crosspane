//! One overlapped handle to the twin control interface (WP-W3.1b). A lane carries one twin: the
//! driver ties a handle's lease and its single monitor together, so the handle is the twin's
//! lifetime. Each IOCTL gets its own boxed slot, freed only once its IO has completed. An IO that
//! can't be shown to have completed after a cancel leaks its slot and loses the lane, so the
//! kernel never writes into freed memory.
#![allow(unsafe_code)]

use crate::model::{
    cpd::{
        self, ADD_RESPONSE_BYTES, HEARTBEAT_RESPONSE_BYTES, IOCTL_ADD, IOCTL_HEARTBEAT, IOCTL_LIST,
        IOCTL_REMOVE, LIST_RESPONSE_BYTES, REMOVE_RESPONSE_BYTES,
    },
    twin::{
        CANCEL_GRACE_MS, HEARTBEAT_TIMEOUT_MS, HeartbeatSeq, TwinError, TwinMode, beat_outcome,
        classify,
    },
};
use std::{
    fmt, iter, mem,
    ptr::{self, null, null_mut},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, FALSE,
        GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, TRUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED},
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

/// What one IOCTL hands the kernel. It is boxed so its address is stable.
struct Slot {
    input: Vec<u8>,
    output: Vec<u8>,
    /// Bytes the kernel reports as transferred.
    transferred: u32,
    overlapped: OVERLAPPED,
    /// Manual-reset completion event. The slot owns it and closes it on drop.
    event: HANDLE,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: `event` is this slot's own handle from CreateEventW, closed only here.
        unsafe { CloseHandle(self.event) };
    }
}

/// One overlapped handle to the twin control interface. Each twin gets its own lane. Every method
/// takes `&self`, so the heartbeat thread can beat a lane while another thread is inside ADD.
pub(crate) struct Lane {
    handle: HANDLE,
    /// Next CPD1 request id. Starts at 1, because the codec refuses 0.
    requests: AtomicU64,
    /// Heartbeats that have timed out in a row.
    misses: AtomicU32,
    /// Set when a heartbeat loses the lane, or when an unresolved IO poisons it.
    lost: AtomicBool,
    /// Heartbeat sequence. It is held across each heartbeat, so sequences reach the driver in order.
    sequence: Mutex<HeartbeatSeq>,
}

// SAFETY: the handle is an overlapped file handle, and the kernel accepts its IOCTLs from any
// thread. Its one close runs in Drop, which needs exclusive ownership.
unsafe impl Send for Lane {}
// SAFETY: every method takes `&self` and touches only atomics, the sequence Mutex and per-call
// slots, so shared use from several threads is sound.
unsafe impl Sync for Lane {}

impl Lane {
    /// Opens the control interface for overlapped IO. Failure gives `Open(GetLastError())`.
    pub(crate) fn open(interface: &str) -> Result<Lane, TwinError> {
        if interface.is_empty() || interface.contains('\0') {
            return Err(TwinError::Open(ERROR_INVALID_PARAMETER));
        }
        let path: Vec<u16> = interface.encode_utf16().chain(iter::once(0)).collect();
        // SAFETY: `path` is a NUL-terminated wide string that outlives the call. The handle is
        // opened read/write for overlapped IO, with no security attributes and no template.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(TwinError::Open(last_error()));
        }
        Ok(Lane {
            handle,
            requests: AtomicU64::new(1),
            misses: AtomicU32::new(0),
            lost: AtomicBool::new(false),
            sequence: Mutex::new(HeartbeatSeq::default()),
        })
    }

    /// Adds the lane's one twin in `mode` and returns its monitor id.
    pub(crate) fn add(&self, mode: TwinMode, timeout_ms: u32) -> Result<u32, TwinError> {
        let request = self.next_request();
        let frame = cpd::encode_add(request, mode)?;
        let reply = self.call(IOCTL_ADD, &frame, ADD_RESPONSE_BYTES, timeout_ms)?;
        Ok(cpd::decode_add(request, &reply)?.monitor_id)
    }

    /// Removes the lane's twin, which must be `monitor_id`.
    pub(crate) fn remove(&self, monitor_id: u32, timeout_ms: u32) -> Result<(), TwinError> {
        let request = self.next_request();
        let frame = cpd::encode_remove(request, monitor_id)?;
        let reply = self.call(IOCTL_REMOVE, &frame, REMOVE_RESPONSE_BYTES, timeout_ms)?;
        cpd::decode_remove(request, monitor_id, &reply)?;
        Ok(())
    }

    /// Lists the lane's monitor. `None` means the lane owns no monitor.
    pub(crate) fn list(&self, timeout_ms: u32) -> Result<Option<u32>, TwinError> {
        let request = self.next_request();
        let frame = cpd::encode_list(request)?;
        let reply = self.call(IOCTL_LIST, &frame, LIST_RESPONSE_BYTES, timeout_ms)?;
        Ok(cpd::decode_list(request, &reply)?
            .monitor
            .map(|(monitor_id, _)| monitor_id))
    }

    /// Renews the lease once. The sequence is held for the whole call, so heartbeats reach the
    /// driver in order, and each reply must echo its own sequence.
    pub(crate) fn heartbeat(&self, timeout_ms: u32) -> Result<(), TwinError> {
        let mut sequence = self.sequence.lock().map_err(|_| TwinError::LeaseLost)?;
        let value = sequence.next()?;
        let request = self.next_request();
        let frame = cpd::encode_heartbeat(request, value)?;
        let reply = self.call(
            IOCTL_HEARTBEAT,
            &frame,
            HEARTBEAT_RESPONSE_BYTES,
            timeout_ms,
        )?;
        cpd::decode_heartbeat(request, value, &reply)?;
        Ok(())
    }

    /// Beats once at `HEARTBEAT_TIMEOUT_MS` and records the outcome. A timeout counts a miss, and
    /// enough misses lose the lane. Any other error loses it at once. Only the heartbeat thread
    /// calls this, so the miss count needs no further synchronisation.
    pub(crate) fn beat(&self) {
        let result = self.heartbeat(HEARTBEAT_TIMEOUT_MS);
        let (misses, lost) = beat_outcome(&result, self.misses.load(Ordering::Acquire));
        self.misses.store(misses, Ordering::Release);
        if lost {
            self.lost.store(true, Ordering::Release);
        }
    }

    /// Whether the lane has lost its lease or been poisoned. A lost lane refuses further IO.
    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn next_request(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed)
    }

    /// Runs one IOCTL and returns the reply bytes the kernel wrote. A timeout cancels the IO. An
    /// IO that still hasn't completed after the grace wait is leaked, and the lane is lost.
    fn call(
        &self,
        ioctl: u32,
        input: &[u8],
        output_len: usize,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, TwinError> {
        if self.lost() {
            return Err(TwinError::LeaseLost);
        }
        let input_len = u32::try_from(input.len())
            .map_err(|_| TwinError::Protocol("control frame is too large"))?;
        let output_bytes = u32::try_from(output_len)
            .map_err(|_| TwinError::Protocol("control reply is too large"))?;
        let event = create_event()?;
        let mut slot = Box::new(Slot {
            input: input.to_vec(),
            output: vec![0_u8; output_len],
            transferred: 0,
            overlapped: OVERLAPPED {
                hEvent: event,
                ..Default::default()
            },
            event,
        });
        // SAFETY: `self.handle` is this lane's open overlapped handle. The input and output
        // buffers, the OVERLAPPED, the transferred count and the event all belong to `slot`, which
        // is neither freed nor moved until this IO completes or is leaked.
        let issued = unsafe {
            DeviceIoControl(
                self.handle,
                ioctl,
                slot.input.as_ptr().cast(),
                input_len,
                slot.output.as_mut_ptr().cast(),
                output_bytes,
                &mut slot.transferred,
                &mut slot.overlapped,
            )
        };
        let waited = if issued == FALSE {
            let code = last_error();
            if code != ERROR_IO_PENDING {
                // Refused synchronously, so nothing is outstanding and the slot can drop.
                return Err(TwinError::Refused(classify(code)));
            }
            // SAFETY: `slot.event` is this slot's own live event handle.
            unsafe { WaitForSingleObject(slot.event, timeout_ms) }
        } else {
            // Completed synchronously; the reply is read below.
            WAIT_OBJECT_0
        };
        if waited != WAIT_OBJECT_0 {
            let failure = if waited == WAIT_TIMEOUT {
                TwinError::Timeout(operation(ioctl))
            } else {
                TwinError::Native("WaitForSingleObject", last_error())
            };
            return Err(self.abandon(slot, ioctl, failure));
        }
        // SAFETY: the handle and the OVERLAPPED are this slot's own operation. With `FALSE` the call
        // never waits, and a completed operation reports its transferred bytes.
        let done = unsafe {
            GetOverlappedResult(self.handle, &slot.overlapped, &mut slot.transferred, FALSE)
        };
        if done == FALSE {
            let code = last_error();
            if code != ERROR_IO_INCOMPLETE {
                return Err(TwinError::Refused(classify(code)));
            }
            return Err(self.abandon(slot, ioctl, TwinError::Timeout(operation(ioctl))));
        }
        let len = (slot.transferred as usize).min(slot.output.len());
        let mut output = mem::take(&mut slot.output);
        output.truncate(len);
        Ok(output)
    }

    /// Gives up on an IO that did not complete in time. It cancels the IO and waits the grace
    /// period. If the IO completed, its slot is freed and `failure` is returned. If not, the slot
    /// is leaked, because the kernel may still write to it, the lane is lost, and the result is a
    /// timeout.
    fn abandon(&self, slot: Box<Slot>, ioctl: u32, failure: TwinError) -> TwinError {
        // SAFETY: `self.handle` is this lane's open handle, and the OVERLAPPED names this slot's own
        // operation. Cancelling writes nothing to our memory. A failed cancel leaves the IO to the
        // grace wait.
        unsafe { CancelIoEx(self.handle, &slot.overlapped) };
        // SAFETY: `slot.event` is this slot's own live event handle.
        let grace = unsafe { WaitForSingleObject(slot.event, CANCEL_GRACE_MS) };
        if grace == WAIT_OBJECT_0 {
            // The event fired, so the kernel no longer touches the slot. Dropping it is safe.
            drop(slot);
            return failure;
        }
        self.lost.store(true, Ordering::Release);
        // The kernel may still write the buffers, the OVERLAPPED or the event. Leak the slot so
        // that memory is never freed under it. The lane is lost, so nothing else reuses it.
        Box::leak(slot);
        TwinError::Timeout(operation(ioctl))
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful CreateFileW and is closed only here, once. A
        // leaked IO keeps its memory, so the close cannot free anything the kernel writes.
        unsafe { CloseHandle(self.handle) };
    }
}

impl fmt::Debug for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lane")
            .field("lost", &self.lost())
            .finish_non_exhaustive()
    }
}

/// A new unnamed, non-inheritable, manual-reset event that starts unsignaled.
fn create_event() -> Result<HANDLE, TwinError> {
    // SAFETY: no security attributes and no name, so no global object is opened or shared.
    let event = unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) };
    if event.is_null() {
        Err(TwinError::Native("CreateEventW", last_error()))
    } else {
        Ok(event)
    }
}

/// The calling thread's last Win32 error. Read it right after the failing call.
fn last_error() -> u32 {
    // SAFETY: GetLastError reads only the calling thread's error slot.
    unsafe { GetLastError() }
}

/// The operation a timeout message names.
fn operation(ioctl: u32) -> &'static str {
    match ioctl {
        IOCTL_ADD => "twin ADD reply",
        IOCTL_REMOVE => "twin REMOVE reply",
        IOCTL_LIST => "twin LIST reply",
        IOCTL_HEARTBEAT => "twin heartbeat reply",
        _ => "twin control reply",
    }
}
