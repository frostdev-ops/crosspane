//! The process-wide operation mutex (L15), held only for the length of one converge or uninstall.

use super::engine::text;
use super::error::{SetupError, SetupResult, note_io};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::{
    Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::SYNCHRONIZE,
    System::Threading::{CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject},
};

pub(crate) const OPERATION_MUTEX: &str = r"Local\Crosspane-Installer";

const NOT_READY: &str = "Crosspane Setup couldn't get ready to work. Try again.";

/// Thread-affine (`ReleaseMutex` must run on the acquiring thread): `!Send` by `PhantomData<*const ()>`.
#[derive(Debug)]
pub(crate) struct OperationLock {
    handle: OwnedHandle,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl OperationLock {
    /// `WAIT_OBJECT_0` or `WAIT_ABANDONED` = acquired; timeout 0 → `Err(before(text::BUSY))`.
    pub(crate) fn acquire() -> SetupResult<Self> {
        Self::acquire_named(OPERATION_MUTEX)
    }

    /// `acquire` for any mutex name, so tests can use a name of their own.
    fn acquire_named(name: &str) -> SetupResult<Self> {
        let wide = wide(name);
        // SAFETY: `wide` is a NUL-terminated UTF-16 name that outlives the call; the mutex gets the
        // default security attributes and is not initially owned.
        let raw = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if raw.is_null() {
            note_io("create operation mutex", &std::io::Error::last_os_error());
            return Err(SetupError::before(NOT_READY));
        }
        // SAFETY: CreateMutexW returned a new handle (to a new or the existing mutex) that nothing
        // else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: `handle` is a live mutex handle, and a zero timeout never blocks.
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            // An abandoned mutex belonged to a thread that died holding it: ownership is ours now.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                handle,
                _not_send: std::marker::PhantomData,
            }),
            WAIT_TIMEOUT => Err(SetupError::before(text::BUSY)),
            _ => {
                note_io("wait for operation mutex", &std::io::Error::last_os_error());
                Err(SetupError::before(NOT_READY))
            }
        }
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        // SAFETY: the type is `!Send`, so this is the thread that acquired the mutex, and the
        // handle is still open: it closes when the field drops after this.
        let _ = unsafe { ReleaseMutex(self.handle.as_raw_handle()) };
    }
}

/// `OpenMutexW` probe only; never waits, never holds.
pub(crate) fn mutex_exists(name: &str) -> bool {
    let wide = wide(name);
    // SAFETY: `wide` is a NUL-terminated UTF-16 name that outlives the call; the probe asks for
    // the right to wait only and does not inherit.
    let raw = unsafe { OpenMutexW(SYNCHRONIZE, 0, wide.as_ptr()) };
    if raw.is_null() {
        return false;
    }
    // SAFETY: OpenMutexW returned a new handle that nothing else owns; dropping it closes it.
    drop(unsafe { OwnedHandle::from_raw_handle(raw) });
    true
}

/// `name` as a NUL-terminated UTF-16 string.
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::SystemTime};

    /// A mutex name no other test or process uses.
    fn unique_name() -> String {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!(r"Local\Crosspane-Test-{}-{nanos}", std::process::id())
    }

    #[test]
    fn a_second_thread_is_busy_and_the_probe_follows_the_holder() {
        // Named mutexes are recursive per thread, so contention needs two threads.
        let name = unique_name();
        assert!(!mutex_exists(&name));

        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = thread::spawn({
            let name = name.clone();
            move || {
                let lock = OperationLock::acquire_named(&name).unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv();
                drop(lock);
            }
        });
        held_rx.recv().unwrap();

        let busy = OperationLock::acquire_named(&name).unwrap_err();
        assert_eq!(busy.text, text::BUSY);
        assert!(!busy.changed);
        assert!(mutex_exists(&name));

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(!mutex_exists(&name));

        // Released: the same name can be taken again, and goes away with the lock.
        let again = OperationLock::acquire_named(&name).unwrap();
        assert!(mutex_exists(&name));
        drop(again);
        assert!(!mutex_exists(&name));
    }

    #[test]
    fn a_mutex_abandoned_by_a_dead_holder_is_taken_over() {
        let name = unique_name();
        thread::spawn({
            let name = name.clone();
            move || {
                let lock = OperationLock::acquire_named(&name).unwrap();
                // Leave the mutex owned by a thread that ends without releasing it.
                std::mem::forget(lock);
            }
        })
        .join()
        .unwrap();

        let taken = OperationLock::acquire_named(&name);
        assert!(taken.is_ok());
    }
}
