//! Comparison decisions only. Facts cannot construct a native selected endpoint.
pub(crate) use super::super::native_io::process::status_matches;
#[cfg(test)]
#[allow(unused_imports)] // Source-included fixtures use these; library unit tests do not.
pub(crate) use super::super::native_io::process::{
    ProcessFacts, bootstrap_matches, process_matches,
};
use super::super::native_io::{NativeError, NativeResult};
#[cfg(test)]
#[allow(dead_code)] // Exercised by source-included fixtures, not library unit tests.
pub(crate) fn peer_matches(expected: &ProcessFacts, current: &ProcessFacts) -> NativeResult<()> {
    process_matches(expected, current)
}
pub(crate) struct Response {
    bytes: Vec<u8>,
}
impl Response {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }
    pub(crate) fn append(&mut self, bytes: &[u8]) -> NativeResult<Option<Vec<u8>>> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > crate::agent_contract::MAX_RESPONSE_BYTES)
        {
            return Err(NativeError::Oversize);
        }
        self.bytes.extend_from_slice(bytes);
        if let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            if end + 1 != self.bytes.len() {
                return Err(NativeError::Invalid);
            }
            return Ok(Some(std::mem::take(&mut self.bytes)));
        }
        Ok(None)
    }
}

#[cfg(windows)]
#[cfg_attr(test, allow(unused_imports))]
pub(crate) use native::NativeEndpoint;

#[cfg(windows)]
pub(super) mod native {
    use super::super::super::native_io::{
        AgentObservation, SupportProof, WindowsNativeIo, process::Deadline,
    };
    use super::super::{Endpoint, failure, readonly};
    use super::*;
    use crate::agent_contract::{
        AgentPlatform, CallFailure, DecodedReply, InstallerRequest, ObservationSource,
        StatusAdmission, decode_installer_stop, decode_reply, encode_installer_stop,
        encode_request,
    };
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        sync::Arc,
    };
    use windows_sys::Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Pipes::*, Threading::*},
    };

    /// The only native endpoint factory takes genuine io/support and observes fixed leaves.
    /// It never accepts a PID, path, token, endpoint name or archived receipt from the caller.
    pub(crate) struct NativeEndpoint {
        io: Arc<WindowsNativeIo>,
        support: SupportProof,
        observation: AgentObservation,
        name: Vec<u16>,
    }
    impl NativeEndpoint {
        pub(crate) fn admit(
            io: Arc<WindowsNativeIo>,
            support: SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let observation = io.observe_agent(&support, deadline)?;
            let token = io.target().identity();
            // Frozen agent/ctl endpoint algorithm: three NUL-delimited UTF-16LE fields.
            let mut encoded = Vec::new();
            let user = token.user.sddl();
            let logon = token.logon.sddl();
            for value in [
                std::ffi::OsStr::new(&user),
                std::ffi::OsStr::new(&logon),
                observation.runtime_canonical(),
            ] {
                for unit in value.encode_wide().chain(std::iter::once(0)) {
                    encoded.extend_from_slice(&unit.to_le_bytes());
                }
            }
            let name = format!(
                r"\\.\pipe\Crosspane-{:032x}",
                xxhash_rust::xxh3::xxh3_128(&encoded)
            );
            Ok(Self {
                io,
                support,
                observation,
                name: name.encode_utf16().chain(std::iter::once(0)).collect(),
            })
        }
        fn verify_pipe(&self, pipe: &OwnedHandle, deadline: &Deadline) -> NativeResult<()> {
            deadline.check()?;
            let mut pid = 0;
            // SAFETY: retained connected pipe and complete writable PID output; query only.
            if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0
                || pid == 0
                || pid != self.observation.bootstrap().pid
            {
                return Err(NativeError::Foreign);
            }
            self.check(deadline)
        }
        fn connect(&self, deadline: &Deadline) -> NativeResult<OwnedHandle> {
            self.check(deadline)?;
            // SAFETY: generated local endpoint only, overlapped/non-inheritable and identification
            // SQOS; individual write right avoids FILE_CREATE_PIPE_INSTANCE from GENERIC_WRITE.
            let raw = unsafe {
                CreateFileW(
                    self.name.as_ptr(),
                    FILE_GENERIC_READ | FILE_WRITE_DATA | SYNCHRONIZE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                #[cfg(not(test))]
                // SAFETY: immediate thread-local error read after the actual failed pipe open;
                // capture has no effect except inside this worker's readonly repair diagnostic.
                super::super::super::native_io::files::native::repair_capture_error(unsafe {
                    GetLastError()
                });
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful CreateFile transferred one owned pipe handle.
            let pipe = unsafe { OwnedHandle::from_raw_handle(raw) };
            self.verify_pipe(&pipe, deadline)?;
            Ok(pipe)
        }
        fn verify_terminal_pipe(
            &self,
            pipe: &OwnedHandle,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            deadline.check()?;
            let mut pid = 0;
            // SAFETY: same connected retained pipe; complete server PID query output only.
            if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0
                || pid != self.observation.bootstrap().pid
                || pid == 0
            {
                return Err(NativeError::Foreign);
            }
            self.check_after_stop(deadline)
        }
        fn exchange(
            &self,
            bytes: Vec<u8>,
            deadline: &Deadline,
            terminal: bool,
            read_only: bool,
        ) -> Result<Vec<u8>, CallFailure> {
            let pipe = self.connect(deadline).map_err(failure)?;
            let mut sent = 0;
            while sent < bytes.len() {
                self.verify_pipe(&pipe, deadline).map_err(|e| {
                    if sent == 0 {
                        failure(e)
                    } else {
                        CallFailure::TimeoutOutcomeUnknown
                    }
                })?;
                let mut operation =
                    Operation::new(&pipe, bytes[sent..].to_vec()).map_err(failure)?;
                let count = operation
                    .transfer(true, deadline)
                    .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
                if count == 0 || count > bytes.len() - sent {
                    return Err(CallFailure::TimeoutOutcomeUnknown);
                }
                sent += count;
            }
            let mut response = Response::new();
            loop {
                let mut operation = Operation::new(&pipe, vec![0; 8192]).map_err(failure)?;
                let count = operation
                    .transfer(false, deadline)
                    .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
                if count == 0 {
                    return Err(CallFailure::InvalidResponse);
                }
                if let Some(bytes) = response
                    .append(&operation.bytes[..count])
                    .map_err(|_| CallFailure::InvalidResponse)?
                {
                    let origin = if terminal {
                        self.verify_terminal_pipe(&pipe, deadline)
                    } else {
                        self.verify_pipe(&pipe, deadline)
                    };
                    origin.map_err(|e| {
                        if read_only {
                            failure(e)
                        } else {
                            CallFailure::TimeoutOutcomeUnknown
                        }
                    })?;
                    deadline.check().map_err(failure)?;
                    return Ok(bytes);
                }
            }
        }
    }
    impl Endpoint for NativeEndpoint {
        fn admit_caller(&self) -> NativeResult<()> {
            super::super::super::native_io::identity::native::refuse_impersonation()
        }
        fn check(&self, deadline: &Deadline) -> NativeResult<()> {
            self.observation
                .revalidate(&self.io, &self.support, deadline)
        }
        fn source(&self) -> ObservationSource {
            ObservationSource::Live
        }
        fn selected_instance(&self) -> NativeResult<u64> {
            Ok(self.observation.bootstrap().instance_id)
        }
        fn stop_frame(
            &self,
            expected_instance: u64,
            deadline: &Deadline,
        ) -> Result<(), CallFailure> {
            let bytes =
                encode_installer_stop(expected_instance).map_err(CallFailure::InvalidCall)?;
            let reply = self.exchange(bytes, deadline, true, false)?;
            decode_installer_stop(&reply)
        }
        fn check_after_stop(&self, deadline: &Deadline) -> NativeResult<()> {
            let proof = self.io.admit_support(deadline)?;
            // Original exit is allowed, but context and original fixed image pins remain fresh.
            self.observation
                .observe_exit(&self.io, &proof, deadline)
                .map(|_| ())
        }
        fn admit_status(&self, reply: &DecodedReply) -> Result<(), CallFailure> {
            let DecodedReply::Status(StatusAdmission::Supported(health)) = reply else {
                return Err(CallFailure::Unavailable);
            };
            status_matches(
                self.observation.bootstrap(),
                self.observation.image_canonical(),
                &health.installer().instance,
            )
            .map_err(failure)
        }
        fn frame(
            &self,
            request: &InstallerRequest,
            deadline: &Deadline,
        ) -> Result<DecodedReply, CallFailure> {
            let bytes = encode_request(request).map_err(CallFailure::InvalidCall)?;
            let reply = self.exchange(bytes, deadline, false, readonly(request))?;
            decode_reply(request, &reply, AgentPlatform::Windows)
        }
    }
    /// Every in-flight buffer/OVERLAPPED/event remains owned until actual kernel completion.
    /// Cancelling a caller is not cancelling/freeing kernel I/O. Drop also drains before freeing.
    struct Operation<'a> {
        pipe: &'a OwnedHandle,
        event: OwnedHandle,
        overlap: Box<OVERLAPPED>,
        bytes: Vec<u8>,
        pending: bool,
    }
    impl<'a> Operation<'a> {
        fn new(pipe: &'a OwnedHandle, bytes: Vec<u8>) -> NativeResult<Self> {
            // SAFETY: unnamed manual-reset event, non-inheritable, owned only by this operation.
            let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if raw.is_null() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful event creation transferred exactly one owned handle.
            let event = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut overlap = Box::new(OVERLAPPED::default());
            overlap.hEvent = event.as_raw_handle();
            Ok(Self {
                pipe,
                event,
                overlap,
                bytes,
                pending: false,
            })
        }
        fn transfer(&mut self, write: bool, deadline: &Deadline) -> NativeResult<usize> {
            deadline.check()?;
            let mut count = 0;
            // SAFETY: pipe, event, stable boxed OVERLAPPED and complete owned buffer stay live
            // through synchronous completion or cancellation followed by actual completion.
            let ok = unsafe {
                if write {
                    WriteFile(
                        self.pipe.as_raw_handle(),
                        self.bytes.as_ptr(),
                        self.bytes.len() as u32,
                        &mut count,
                        self.overlap.as_mut(),
                    )
                } else {
                    ReadFile(
                        self.pipe.as_raw_handle(),
                        self.bytes.as_mut_ptr(),
                        self.bytes.len() as u32,
                        &mut count,
                        self.overlap.as_mut(),
                    )
                }
            };
            if ok == 0 {
                // SAFETY: immediately observes only the submission result on this owner thread.
                if unsafe { GetLastError() } != ERROR_IO_PENDING {
                    return Err(NativeError::Unavailable);
                }
                self.pending = true;
                loop {
                    let remaining = match deadline.remaining_ms() {
                        Ok(ms) => ms,
                        Err(error) => {
                            self.cancel_and_drain();
                            return Err(error);
                        }
                    };
                    // SAFETY: retained own event; short bounded wait, no message pumping or GUI.
                    match unsafe {
                        WaitForSingleObject(
                            self.event.as_raw_handle(),
                            remaining.clamp(1, 10) as u32,
                        )
                    } {
                        WAIT_OBJECT_0 => break,
                        WAIT_TIMEOUT => continue,
                        _ => {
                            self.cancel_and_drain();
                            return Err(NativeError::Unavailable);
                        }
                    }
                }
            }
            // SAFETY: complete/still-owned operation; event reported completion or synchronous success.
            let completed = unsafe {
                GetOverlappedResult(
                    self.pipe.as_raw_handle(),
                    self.overlap.as_ref(),
                    &mut count,
                    0,
                )
            };
            if completed == 0 {
                self.cancel_and_drain();
                return Err(NativeError::Unavailable);
            }
            self.pending = false;
            deadline.check()?;
            let count = count as usize;
            if count > self.bytes.len() {
                return Err(NativeError::Invalid);
            }
            Ok(count)
        }
        fn cancel_and_drain(&mut self) {
            if !self.pending {
                return;
            }
            // SAFETY: cancel only this exact owned OVERLAPPED; cancellation is not completion.
            unsafe {
                CancelIoEx(self.pipe.as_raw_handle(), self.overlap.as_ref());
            }
            loop {
                let mut count = 0;
                // SAFETY: all operation resources retained; this owned worker may outlive caller.
                let ok = unsafe {
                    GetOverlappedResult(
                        self.pipe.as_raw_handle(),
                        self.overlap.as_ref(),
                        &mut count,
                        1,
                    )
                };
                // SAFETY: immediately reads only the completion result on this thread.
                let error = unsafe { GetLastError() };
                if ok != 0 || error != ERROR_IO_INCOMPLETE {
                    self.pending = false;
                    break;
                }
            }
        }
    }
    impl Drop for Operation<'_> {
        fn drop(&mut self) {
            self.cancel_and_drain();
        }
    }
}
