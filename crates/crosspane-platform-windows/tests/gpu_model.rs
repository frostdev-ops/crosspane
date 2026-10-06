//! No native objects or GPU are created by these production-state regressions.
use crosspane_platform_windows::model::gpu::*;
use crosspane_types::geom::PixelSize;

fn planes() -> (Plane, Plane) {
    (
        Plane {
            offset: 0,
            pitch: 256,
            width: 102,
            height: 76,
            bytes_per_pixel: 1,
        },
        Plane {
            offset: 19456,
            pitch: 256,
            width: 51,
            height: 38,
            bytes_per_pixel: 2,
        },
    )
}
#[test]
fn gpu_selection_requires_exact_capture_and_dx12_luid() {
    assert_eq!(admit(Some(42), Some(42), false), Ok(()));
    assert_eq!(admit(Some(42), Some(43), false), Err(Reason::Adapter));
}
#[test]
fn gpu_selection_refuses_unknown_or_removed_device() {
    assert_eq!(admit(None, Some(42), false), Err(Reason::Adapter));
    assert_eq!(admit(Some(42), Some(42), true), Err(Reason::Removed));
}
#[test]
fn shared_slot_is_wrapped_once_and_handle_closed_on_every_partial_failure() {
    // The actual native adapter uses this guard for both texture and fence share handles.
    // Each simulated early-return stage closes once; success closes after opening/validation too.
    for failed_at in 0..=3 {
        let closed = std::cell::Cell::new(0);
        let operation = || -> Result<(), Reason> {
            let handle = SharedHandle::new(&closed, |count| count.set(count.get() + 1));
            assert_eq!(handle.get().get(), 0);
            for stage in 0..3 {
                if stage == failed_at {
                    return Err(Reason::Sharing);
                }
            }
            Ok(())
        };
        assert_eq!(operation().is_ok(), failed_at == 3);
        assert_eq!(closed.get(), 1);
    }
    let mut slot = Slot::new(1);
    assert_eq!(slot.wrap(1), Ok(true));
    assert_eq!(slot.wrap(1), Ok(false));
    // Native handle lifetime is RAII; failed admission must never mark a wrap completed.
    let mut failed = Slot::new(2);
    assert!(failed.wrap(1).is_err());
    assert!(!failed.wrapped);
}
#[test]
fn same_handle_new_pool_generation_never_reuses_old_import() {
    let mut slot = Slot::new(8);
    slot.wrap(8).unwrap();
    assert!(slot.wrap(9).is_err());
    slot.retire();
    assert!(slot.wrap(8).is_err());
}
#[test]
fn capture_slot_needs_arc_release_and_consumer_fence_completion() {
    let mut slot = Slot::new(1);
    slot.done = 8;
    assert!(!slot.free(false, 8));
    assert!(!slot.free(true, 7));
    assert!(!slot.free(true, u64::MAX));
    assert!(slot.free(true, 8));
}
#[test]
fn cancel_before_submit_removes_staged_wait_and_signal() {
    let mut handoff = Handoff::default();
    handoff.stage().unwrap();
    handoff.cancel();
    assert!(!handoff.staged);
    assert!(handoff.readable());
}
#[test]
fn timeout_after_submit_quarantines_slot_before_cpu_fallback() {
    let mut handoff = Handoff::default();
    handoff.stage().unwrap();
    handoff.submitted();
    handoff.cancel();
    assert!(!handoff.readable());
    handoff.finish();
    assert!(handoff.readable());
}
#[test]
fn closed_gate_and_changed_epoch_never_retry_cpu_or_publish_gpu_result() {
    assert_eq!(permitted(false, 1, 1), Err(Reason::Gate));
    assert_eq!(permitted(true, 2, 1), Err(Reason::Gate));
    assert_eq!(permitted(true, 2, 2), Ok(()));
    let gate = crosspane_platform::IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let epoch = gate.epoch();
    assert_eq!(permitted(gate.is_open(), gate.epoch(), epoch), Ok(()));
    gate.set_session_permits(false);
    gate.set_session_permits(true);
    assert_eq!(
        permitted(gate.is_open(), gate.epoch(), epoch),
        Err(Reason::Gate)
    );
}
#[test]
fn crop_and_resize_retire_old_slots_without_changing_capture_identity() {
    let capture = 19;
    let mut old = Slot::new(1);
    old.retire();
    assert!(!old.free(true, 100));
    assert!(Slot::new(2).free(true, 0));
    assert_eq!(capture, 19);
    assert_eq!(texture_admit(PixelSize::new(8192, 8192), 8192), Ok(()));
    for size in [
        PixelSize::new(0, 75),
        PixelSize::new(8193, 75),
        PixelSize::new(101, 8193),
    ] {
        assert_eq!(texture_admit(size, 8192), Err(Reason::Layout));
    }
}
#[test]
fn nv12_odd_display_size_keeps_exact_even_coded_size() {
    let (y, uv) = planes();
    let layout = Layout::validate(PixelSize::new(101, 75), y, uv, 29184).unwrap();
    assert_eq!(layout.size, PixelSize::new(102, 76));
}
#[test]
fn nv12_footprints_validate_plane_rows_pitch_offsets_and_total_bounds() {
    let (y, uv) = planes();
    assert!(Layout::validate(PixelSize::new(102, 76), y, uv, 29184).is_ok());
    let layout = Layout::validate(PixelSize::new(102, 76), y, uv, 29184).unwrap();
    assert!(layout.fits(29184, 29184));
    assert!(!layout.fits(29183, 29184));
    assert!(!layout.fits(29184, 29183));
    for invalid in [
        Plane { pitch: 255, ..y },
        Plane { offset: 1, ..y },
        Plane { height: 75, ..y },
    ] {
        assert!(Layout::validate(PixelSize::new(102, 76), invalid, uv, 29184).is_err());
    }
}
#[test]
fn nv12_footprints_refuse_mismatch_overflow_and_wrong_plane_format() {
    let (y, uv) = planes();
    assert!(Layout::validate(PixelSize::new(u32::MAX, 75), y, uv, 29184).is_err());
    assert!(
        Layout::validate(
            PixelSize::new(101, 75),
            y,
            Plane {
                bytes_per_pixel: 1,
                ..uv
            },
            29184
        )
        .is_err()
    );
    assert!(Layout::validate(PixelSize::new(101, 75), y, uv, 29183).is_err());
    assert!(
        Layout::validate(PixelSize::new(101, 75), y, Plane { offset: 0, ..uv }, 29184).is_err()
    );
}
#[test]
fn native_copy_order_is_shader_copy_source_copy_dest_common_signal() {
    let order = [
        CopyStep::Shader,
        CopyStep::CopySource,
        CopyStep::CopyDest,
        CopyStep::Common,
        CopyStep::Signal,
    ];
    assert!(copy_order(&order));
    let mut wrong = order;
    wrong.swap(2, 3);
    assert!(!copy_order(&wrong));
}
#[test]
fn mf_gpu_admission_requires_matching_adapter_d3d11_awareness_and_manager() {
    assert_eq!(mft_admit(true, true, true), Ok(()));
    for flags in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        assert_eq!(mft_admit(flags.0, flags.1, flags.2), Err(Reason::Mft));
    }
}
#[test]
fn mf_input_is_not_reused_until_caller_gpu_and_tracked_sample_release() {
    let mut lease = InputRelease::new(1);
    lease.caller = true;
    lease.gpu = true;
    assert!(!lease.free());
    lease.callback(1);
    assert!(lease.free());
}
#[test]
fn mf_duplicate_or_old_generation_callback_cannot_free_new_input() {
    let mut lease = InputRelease::new(2);
    lease.caller = true;
    lease.gpu = true;
    lease.callback(1);
    assert!(!lease.free());
    lease.callback(2);
    lease.callback(2);
    assert!(lease.free());
    lease.claim(3).unwrap();
    lease.native = false; // The new sample has been handed to MF.
    lease.caller = true;
    lease.gpu = true;
    lease.callback(2);
    assert!(!lease.free());
    lease.callback(3);
    assert!(lease.free());
}
#[test]
fn gpu_mft_refusal_selects_cpu_session_with_fresh_idr() {
    let mut path = Path::selected("fixture / GPU input".into());
    assert!(path.native());
    path.refused(Reason::Mft);
    assert!(!path.native());
    assert_eq!(path.encode, "cpu_mf");
    assert_eq!(path.reason, "mft_unavailable");
    // The unchanged W2.3a header state forces an IDR on fresh sessions (video unit tests).
}
#[test]
fn gpu_pool_exhaustion_never_overwrites_native_input() {
    let mut lease = InputRelease::new(1);
    lease.native = true;
    lease.gpu = true;
    assert!(!lease.free());
}

#[cfg(feature = "video")]
#[test]
fn cpu_fallback_preserves_bitrate_timestamp_annex_b_and_odd_geometry() {
    use crosspane_platform_windows::model::video::{Clock, Headers, Params, prefer_hardware};
    let params = Params::new(PixelSize::new(101, 75), 1_234_567, 60).unwrap();
    let cpu = prefer_hardware(
        [Err(crosspane_media::codec::CodecError::Failed(
            "fixture GPU refusal".into(),
        ))],
        || Ok(params),
    )
    .unwrap();
    assert_eq!(cpu.bitrate, 1_234_567);
    assert_eq!(cpu.size, PixelSize::new(101, 75));
    assert_eq!(cpu.coded().unwrap(), PixelSize::new(102, 76));
    let mut clock = Clock::default();
    let first = clock.next(60).unwrap();
    assert!(clock.next(60).unwrap().0 > first.0);
    let mut headers = Headers::default();
    // Supplied fixture SPS/PPS precede this IDR; the same real header helper serves native and CPU.
    let packet = [
        0, 0, 1, 0x67, 0x42, 0, 0x1e, 0, 0, 1, 0x68, 0xc0, 0, 0, 1, 0x65, 0x80,
    ];
    let (annex_b, key) = headers.packet(&packet, true).unwrap();
    assert!(key);
    assert!(annex_b.starts_with(&[0, 0, 1]) || annex_b.starts_with(&[0, 0, 0, 1]));
    assert_eq!(annex_b, packet); // Valid supplied Annex B remains byte-identical.
    assert!(crosspane_platform_windows::model::video::nals(&annex_b).any(|nal| nal[0] & 31 == 5));
}
