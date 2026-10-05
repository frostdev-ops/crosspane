#[cfg(windows)]
#[path = "../src/clock.rs"]
mod clock;

#[test]
fn producers_use_the_shared_boot_clock_not_local_epochs() {
    for source in [
        include_str!("../src/capture.rs"),
        include_str!("../src/hotkey.rs"),
        include_str!("../src/frame_capture.rs"),
    ] {
        assert!(
            source.contains("crate::clock::now()"),
            "producer missing node clock"
        );
        assert!(!source.contains("MonoTime::from_nanos(start.elapsed()"));
        assert!(!source.contains("GetTickCount64"));
        assert!(!source.contains("static EPOCH: OnceLock<Instant>"));
    }
}

#[cfg(windows)]
#[test]
fn unbiased_ticks_convert_at_one_epoch_and_saturate() {
    assert_eq!(clock::from_ticks(123).as_nanos(), 12_300);
    assert_eq!(clock::from_ticks(u64::MAX).as_nanos(), u64::MAX);
    let first = clock::now();
    let second = clock::now();
    assert!(second >= first);
}
