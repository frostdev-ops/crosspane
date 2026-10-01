//! The node's monotonic clock on macOS: `mach_absolute_time` converted to nanoseconds. It is the
//! clock `CGEventGetTimestamp` uses, so event timestamps convert without an offset. The agent's
//! engine clock and every backend's `at: MonoTime` use this.

use std::sync::OnceLock;

use crosspane_types::time::MonoTime;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

fn timebase() -> (u64, u64) {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo::default();
        // SAFETY: `info` is a valid, writable MachTimebaseInfo; the call only fills it in.
        let status = unsafe { mach_timebase_info(&mut info) };
        if status != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (u64::from(info.numer), u64::from(info.denom))
        }
    })
}

/// Convert mach absolute-time ticks (e.g. `CGEventGetTimestamp`) to the node's clock.
pub fn from_ticks(ticks: u64) -> MonoTime {
    let (numer, denom) = timebase();
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    MonoTime::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Now, on the node's clock.
pub fn now() -> MonoTime {
    // SAFETY: no arguments; reads the system's monotonic tick counter.
    from_ticks(unsafe { mach_absolute_time() })
}
