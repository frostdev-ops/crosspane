//! The node's single boot clock, excluding time asleep/hibernating, like Linux's
//! CLOCK_MONOTONIC and macOS mach_absolute_time. Never use a producer-local epoch.
//! Windows 10's documented VOID query has no failure result requiring a fallback.
//! https://learn.microsoft.com/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryunbiasedinterrupttimeprecise
#![allow(unsafe_code)]

use crosspane_types::time::MonoTime;
use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise;

/// Read the common node clock. Its arbitrary boot epoch is meaningful only locally.
pub fn now() -> MonoTime {
    let mut ticks = 0;
    // SAFETY: documented infallible query with a valid, initialized u64 output.
    unsafe { QueryUnbiasedInterruptTimePrecise(&mut ticks) };
    from_ticks(ticks)
}

pub(crate) fn from_ticks(ticks: u64) -> MonoTime {
    MonoTime::from_nanos(ticks.saturating_mul(100))
}
