#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_platform::{ClipKinds, PlatformError};
use crosspane_platform_windows::model::clipboard::*;
use crosspane_types::ClipKind;

fn utf16(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn snapshot(sequence: u32, owner: usize) -> Snapshot {
    Snapshot {
        sequence,
        owner,
        kinds: ClipKinds {
            text: true,
            image: false,
        },
    }
}
#[test]
fn kinds_and_priority_are_metadata_only() {
    let all = Formats {
        text: true,
        png: true,
        dib_v5: true,
        dib: true,
    };
    assert_eq!(
        all.kinds(),
        ClipKinds {
            text: true,
            image: true
        }
    );
    assert_eq!(all.select(ClipKind::Text, false).unwrap(), Format::Text);
    assert_eq!(all.select(ClipKind::Image, false).unwrap(), Format::Png);
    assert_eq!(
        Formats { png: false, ..all }
            .select(ClipKind::Image, false)
            .unwrap(),
        Format::DibV5
    );
    assert_eq!(
        Formats {
            png: false,
            dib_v5: false,
            ..all
        }
        .select(ClipKind::Image, false)
        .unwrap(),
        Format::Dib
    );
}
#[test]
fn own_content_and_missing_kind_are_not_found() {
    let formats = Formats {
        text: true,
        ..Default::default()
    };
    assert!(matches!(
        formats.select(ClipKind::Text, true),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        formats.select(ClipKind::Image, false),
        Err(PlatformError::NotFound)
    ));
}
#[test]
fn utf16_crlf_nul_and_non_ascii_convert_exactly() {
    let mut native = utf16("fixture\r\n\u{e9}\u{1f600}\r");
    native.extend_from_slice(&utf16("ignored after NUL"));
    assert_eq!(
        text(&native, 128).unwrap(),
        "fixture\n\u{e9}\u{1f600}\r".as_bytes()
    );
}
#[test]
fn utf8_size_is_after_crlf_normalization_and_never_truncates() {
    assert_eq!(text(&utf16("\r\n"), 1).unwrap(), b"\n");
    assert_eq!(text(&utf16("\u{e9}"), 2).unwrap().len(), 2);
    assert!(matches!(
        text(&utf16("\u{e9}"), 1),
        Err(PlatformError::TooLarge)
    ));
    assert!(matches!(text(&utf16("x"), 0), Err(PlatformError::TooLarge)));
}
#[test]
fn invalid_surrogate_unterminated_and_empty_text_are_not_found() {
    for bytes in [
        vec![0, 0],
        vec![],
        vec![0, 0xd8, 0, 0],
        vec![b'x', 0],
        vec![b'x'],
    ] {
        assert!(matches!(text(&bytes, 10), Err(PlatformError::NotFound)));
    }
}
#[test]
fn nul_ignores_allocation_padding_even_when_large_or_odd() {
    let mut data = utf16("x");
    data.resize(2 * TEXT_CAP + 3, 0xff);
    assert_eq!(text(&data, 1).unwrap(), b"x");
}
#[test]
fn text_kind_cap_holds_even_if_caller_limit_is_larger() {
    assert_eq!(
        text(&utf16(&"x".repeat(TEXT_CAP)), usize::MAX)
            .unwrap()
            .len(),
        TEXT_CAP
    );
    assert!(matches!(
        text(&utf16(&"x".repeat(TEXT_CAP + 1)), usize::MAX),
        Err(PlatformError::TooLarge)
    ));
}
#[test]
fn gate_close_reopen_epoch_and_expiry_are_fail_closed() {
    assert!(matches!(
        admit(false, 1, 1, false),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        admit(true, 1, 3, false),
        Err(PlatformError::Locked)
    ));
    assert!(matches!(
        admit(true, 1, 1, true),
        Err(PlatformError::Timeout)
    ));
    assert!(admit(true, 1, 1, false).is_ok());
}
#[test]
fn contention_uses_exact_250ms_budget_with_no_late_attempt() {
    assert_eq!(retry_delay(0).unwrap(), 5);
    assert_eq!(retry_delay(249).unwrap(), 1);
    assert!(matches!(retry_delay(250), Err(PlatformError::Timeout)));
    assert!(matches!(retry_delay(u64::MAX), Err(PlatformError::Timeout)));
}
#[test]
fn snapshot_race_unknown_sequence_or_owner_replacement_is_refused() {
    assert!(coherent((1, 4), (1, 4)));
    assert!(!coherent((1, 4), (2, 4)));
    assert!(!coherent((1, 4), (1, 5)));
    assert!(!coherent((0, 4), (0, 4)));
}
#[test]
fn first_metadata_and_duplicate_suppression_are_exact() {
    let mut watch = Watch::default();
    assert!(watch.observe(snapshot(1, 4), 9, true).is_some());
    assert!(watch.observe(snapshot(1, 4), 9, true).is_none());
    assert!(watch.observe(snapshot(2, 4), 9, true).is_some());
}
#[test]
fn closed_gate_changes_reconcile_only_latest_once() {
    let mut watch = Watch::default();
    watch.observe(snapshot(1, 4), 9, true);
    assert!(watch.observe(snapshot(2, 4), 9, false).is_none());
    assert!(watch.observe(snapshot(3, 5), 9, false).is_none());
    assert!(watch.observe(snapshot(3, 5), 9, true).is_some());
    assert!(watch.observe(snapshot(3, 5), 9, true).is_none());
}
#[test]
fn own_window_echo_is_suppressed_without_hiding_next_external_copy() {
    let mut watch = Watch::default();
    assert!(watch.observe(snapshot(1, 9), 9, true).is_none());
    assert!(watch.observe(snapshot(2, 4), 9, true).is_some());
    assert!(watch.observe(snapshot(3, 9), 9, false).is_none());
    assert!(watch.observe(snapshot(3, 9), 9, true).is_none());
    assert!(watch.observe(snapshot(4, 4), 9, true).is_some());
}

fn dib(width: i32, height: i32, bits: u16, header: usize) -> Vec<u8> {
    let stride = (width.unsigned_abs() as usize * usize::from(bits)).div_ceil(32) * 4;
    let mut data = vec![0; header + stride * height.unsigned_abs() as usize];
    data[..4].copy_from_slice(&(header as u32).to_le_bytes());
    data[4..8].copy_from_slice(&width.to_le_bytes());
    data[8..12].copy_from_slice(&height.to_le_bytes());
    data[12..14].copy_from_slice(&1u16.to_le_bytes());
    data[14..16].copy_from_slice(&bits.to_le_bytes());
    if header >= 108 {
        data[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes());
    }
    data
}
fn decoded(bytes: Vec<u8>) -> Vec<u8> {
    let mut reader = ::png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    pixels.truncate(info.buffer_size());
    pixels
}
#[test]
fn registered_png_is_exact_and_capped_without_truncation() {
    let fixture = b"exact owned PNG representation";
    assert_eq!(png(fixture, fixture.len()).unwrap(), fixture);
    assert!(matches!(
        png(fixture, fixture.len() - 1),
        Err(PlatformError::TooLarge)
    ));
    assert!(matches!(png(&[], 10), Err(PlatformError::NotFound)));
    assert!(matches!(
        png(&vec![0; IMAGE_CAP + 1], usize::MAX),
        Err(PlatformError::TooLarge)
    ));
}
#[test]
fn dib_24bit_bottom_up_and_top_down_have_correct_rgb_and_padding() {
    for height in [2, -2] {
        let mut image = dib(1, height, 24, 40);
        let (first, second) = if height > 0 { (44, 40) } else { (40, 44) };
        image[first..first + 3].copy_from_slice(&[3, 2, 1]);
        image[second..second + 3].copy_from_slice(&[6, 5, 4]);
        assert_eq!(
            decoded(dib_png(&image, IMAGE_CAP).unwrap()),
            vec![1, 2, 3, 255, 4, 5, 6, 255]
        );
    }
}
#[test]
fn dib_v5_bitfields_preserve_alpha_but_rgb_reserved_high_byte_is_opaque() {
    let mut image = dib(1, -1, 32, 124);
    for (offset, mask) in [
        (40, 0x00ff0000u32),
        (44, 0x0000ff00),
        (48, 0xff),
        (52, 0xff000000),
    ] {
        image[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
    }
    image[16..20].copy_from_slice(&3u32.to_le_bytes());
    image[124..128].copy_from_slice(&[3, 2, 1, 128]);
    assert_eq!(
        decoded(dib_png(&image, IMAGE_CAP).unwrap()),
        vec![1, 2, 3, 128]
    );
    image[16..20].fill(0);
    assert_eq!(
        decoded(dib_png(&image, IMAGE_CAP).unwrap()),
        vec![1, 2, 3, 255]
    );
}
#[test]
fn dib_indexed_palette_and_rgb16_are_supported() {
    for bits in [1, 4, 8] {
        let mut image = dib(1, -1, bits, 40);
        image.splice(40..40, [3, 2, 1, 0, 6, 5, 4, 0]);
        image[32..36].copy_from_slice(&2u32.to_le_bytes());
        image[48] = match bits {
            1 => 0x80,
            4 => 0x10,
            _ => 1,
        };
        assert_eq!(
            decoded(dib_png(&image, IMAGE_CAP).unwrap()),
            vec![4, 5, 6, 255]
        );
    }
    let mut image = dib(1, -1, 16, 40);
    image[40..42].copy_from_slice(&0x7c00u16.to_le_bytes());
    assert_eq!(
        decoded(dib_png(&image, IMAGE_CAP).unwrap()),
        vec![255, 0, 0, 255]
    );
}
#[test]
fn dib_malformed_dimensions_strides_masks_and_profiles_are_refused() {
    assert!(dib_png(&[], IMAGE_CAP).is_err());
    let mut image = dib(1, 1, 24, 40);
    image.pop();
    assert!(dib_png(&image, IMAGE_CAP).is_err());
    let mut image = dib(1, -1, 32, 124);
    for compression in [1u32, 2, 4, 5] {
        image[16..20].copy_from_slice(&compression.to_le_bytes());
        assert!(
            matches!(dib_png(&image, IMAGE_CAP), Err(PlatformError::Backend(message)) if message == "unsupported clipboard image layout")
        );
    }
    image[16..20].fill(0);
    image[56..60].fill(0); // calibrated color is deliberately unsupported.
    assert!(dib_png(&image, IMAGE_CAP).is_err());
    image[56..60].copy_from_slice(&0x73524742u32.to_le_bytes());
    image[112..116].copy_from_slice(&124u32.to_le_bytes());
    assert!(dib_png(&image, IMAGE_CAP).is_err());
    let mut image = dib(1, -1, 32, 56);
    image[16..20].copy_from_slice(&3u32.to_le_bytes());
    for (at, mask) in [
        (40, 0xff0000u32),
        (44, 0xff00),
        (48, 0xff),
        (52, 0xff000000),
    ] {
        image[at..at + 4].copy_from_slice(&mask.to_le_bytes());
    }
    for invalid in [0u32, 0x00ff0000, 0x00005500] {
        image[44..48].copy_from_slice(&invalid.to_le_bytes());
        assert!(dib_png(&image, IMAGE_CAP).is_err());
    }
    for (at, value) in [(4, 0i32), (4, -1), (8, 0)] {
        let mut image = dib(1, -1, 24, 40);
        image[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert!(dib_png(&image, IMAGE_CAP).is_err());
    }
    let mut image = dib(1, -1, 8, 40);
    image[32..36].copy_from_slice(&1u32.to_le_bytes());
    assert!(dib_png(&image, IMAGE_CAP).is_err()); // missing declared palette.
}
#[test]
fn dib_external_rgb565_masks_and_incomplete_mask_set_are_checked() {
    let mut image = dib(1, -1, 16, 40);
    image[16..20].copy_from_slice(&3u32.to_le_bytes());
    let masks: [u32; 3] = [0xf800, 0x07e0, 0x001f];
    image.splice(40..40, masks.into_iter().flat_map(u32::to_le_bytes));
    image[52..54].copy_from_slice(&0x07e0u16.to_le_bytes());
    assert_eq!(
        decoded(dib_png(&image, IMAGE_CAP).unwrap()),
        vec![0, 255, 0, 255]
    );
    image.truncate(48);
    assert!(dib_png(&image, IMAGE_CAP).is_err());
}
#[test]
fn dib_decoded_work_and_png_output_caps_are_independent() {
    let mut image = dib(1, 1, 24, 40);
    image[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
    assert!(matches!(
        dib_png(&image, IMAGE_CAP),
        Err(PlatformError::TooLarge)
    ));
    image[4..8].copy_from_slice(&1i32.to_le_bytes());
    assert!(matches!(dib_png(&image, 8), Err(PlatformError::TooLarge)));
    assert_eq!(DIB_WORK_CAP, 64 * 1024 * 1024);
}

use crosspane_platform::LocalPasteId;
use std::time::Instant;
use zeroize::Zeroizing;
fn promised() -> (RenderLedger, Instant) {
    let mut ledger = RenderLedger::default();
    let now = Instant::now();
    ledger
        .install(
            41,
            ClipKinds {
                text: true,
                image: true,
            },
            2,
            true,
        )
        .unwrap();
    (ledger, now)
}
#[test]
fn render_only_advertised_formats_one_slot_and_fresh_ids() {
    let (mut ledger, now) = promised();
    let first = ledger.begin(Format::Text, now, 2, true).unwrap();
    assert_eq!((first.offer, first.deadline), (41, now + RENDER_WAIT));
    assert!(ledger.begin(Format::Png, now, 2, true).is_none());
    ledger.finish(LocalPasteId(first.paste.0 + 1));
    assert!(ledger.rendering());
    ledger.finish(first.paste);
    let next = ledger.begin(Format::DibV5, now, 2, true).unwrap();
    assert!(next.paste.0 > first.paste.0);
    ledger.finish(next.paste);
    assert!(ledger.begin(Format::Dib, now, 2, true).is_none());
    ledger
        .install(
            42,
            ClipKinds {
                text: false,
                image: true,
            },
            2,
            true,
        )
        .unwrap();
    assert!(ledger.begin(Format::Text, now, 2, true).is_none());
    assert!(ledger.begin(Format::Png, now, 3, true).is_none());
    assert!(ledger.begin(Format::Png, now, 2, false).is_none());
}
#[test]
fn render_fulfil_once_none_unknown_duplicate_and_exact_deadline() {
    let (mut ledger, now) = promised();
    let render = ledger.begin(Format::Png, now, 2, true).unwrap();
    ledger.answer(
        LocalPasteId(render.paste.0 + 1),
        Some(Zeroizing::new(vec![9])),
        now,
        2,
        true,
    );
    assert!(ledger.poll(render, now, 2, true).is_none());
    ledger.answer(render.paste, Some(Zeroizing::new(vec![1])), now, 2, true);
    ledger.answer(render.paste, Some(Zeroizing::new(vec![2])), now, 2, true);
    assert_eq!(&**ledger.poll(render, now, 2, true).unwrap().unwrap(), &[1]);
    assert!(ledger.valid(render, now, 2, true));
    assert!(!ledger.valid(render, now + RENDER_WAIT, 2, true));
    ledger.finish(render.paste);
    let render = ledger.begin(Format::Png, now, 2, true).unwrap();
    ledger.answer(render.paste, None, now, 2, true);
    assert!(ledger.poll(render, now, 2, true).unwrap().is_none());
}
#[test]
fn render_timeout_gate_epoch_and_cancel_discard_ready_data() {
    for mode in 0..4 {
        let (mut ledger, now) = promised();
        let render = ledger.begin(Format::Text, now, 2, true).unwrap();
        ledger.answer(render.paste, Some(Zeroizing::new(vec![1])), now, 2, true);
        if mode == 0 {
            ledger.cancel(Some(41));
        }
        let at = if mode == 1 { now + RENDER_WAIT } else { now };
        let epoch = if mode == 2 { 4 } else { 2 };
        let open = mode != 3;
        assert!(ledger.poll(render, at, epoch, open).unwrap().is_none());
        assert!(!ledger.valid(render, at, epoch, open));
        ledger.finish(render.paste);
    }
}
#[test]
fn render_replacement_retires_old_reply_and_slot_until_callback_finishes() {
    let (mut ledger, now) = promised();
    let old = ledger.begin(Format::Text, now, 2, true).unwrap();
    ledger
        .install(
            42,
            ClipKinds {
                text: true,
                image: false,
            },
            2,
            true,
        )
        .unwrap();
    ledger.answer(old.paste, Some(Zeroizing::new(vec![1])), now, 2, true);
    assert!(ledger.poll(old, now, 2, true).unwrap().is_none());
    assert!(ledger.begin(Format::Text, now, 2, true).is_none());
    ledger.finish(old.paste);
    let new = ledger.begin(Format::Text, now, 2, true).unwrap();
    assert_eq!(new.offer, 42);
    assert!(new.generation > old.generation);
    ledger.answer(old.paste, Some(Zeroizing::new(vec![2])), now, 2, true);
    assert!(ledger.poll(new, now, 2, true).is_none());
}
#[test]
fn render_actual_owner_loss_is_once_and_withdraw_or_drop_is_silent_empty() {
    let (mut ledger, now) = promised();
    let render = ledger.begin(Format::Text, now, 2, true).unwrap();
    assert!(ledger.lost(true).is_none());
    assert_eq!(ledger.lost(false), Some(41));
    assert!(ledger.lost(false).is_none());
    assert!(ledger.poll(render, now, 2, true).unwrap().is_none());
    ledger.finish(render.paste);
    ledger
        .install(
            42,
            ClipKinds {
                text: true,
                image: false,
            },
            2,
            true,
        )
        .unwrap();
    let render = ledger.begin(Format::Text, now, 2, true).unwrap();
    assert!(!ledger.withdraw(41));
    assert_eq!(ledger.current().unwrap().offer, 42);
    assert!(ledger.withdraw(42));
    assert!(ledger.poll(render, now, 2, true).unwrap().is_none());
    assert!(ledger.lost(false).is_none());
    ledger.finish(render.paste);
    ledger
        .install(
            43,
            ClipKinds {
                text: true,
                image: false,
            },
            2,
            true,
        )
        .unwrap();
    let render = ledger.begin(Format::Text, now, 2, true).unwrap();
    ledger.close();
    assert!(ledger.poll(render, now, 2, true).unwrap().is_none());
    ledger.finish(render.paste);
    assert!(ledger.begin(Format::Text, now, 2, true).is_none());
    assert!(
        ledger
            .install(
                44,
                ClipKinds {
                    text: true,
                    image: false
                },
                2,
                true
            )
            .is_err()
    );
}
#[test]
fn render_cancel_after_poll_fences_delivery_and_late_fulfil() {
    let (mut ledger, now) = promised();
    let render = ledger.begin(Format::Text, now, 2, true).unwrap();
    ledger.answer(render.paste, Some(Zeroizing::new(vec![1])), now, 2, true);
    assert!(ledger.poll(render, now, 2, true).unwrap().is_some());
    ledger.cancel(None);
    assert!(!ledger.valid(render, now, 2, true));
    assert!(ledger.pending(render.paste).is_none());
    ledger.answer(render.paste, Some(Zeroizing::new(vec![2])), now, 2, true);
    assert!(ledger.poll(render, now, 2, true).unwrap().is_none());
}
#[test]
fn promise_empty_and_locked_priorities_preserve_existing_promise() {
    let (mut ledger, _) = promised();
    assert!(matches!(
        ledger.install(42, ClipKinds::default(), 3, false),
        Err(PlatformError::Locked)
    ));
    assert!(
        matches!(ledger.install(42,ClipKinds::default(),2,true),Err(PlatformError::Backend(reason))if reason=="empty clipboard promise")
    );
    assert_eq!(ledger.current().unwrap().offer, 41);
}
#[test]
fn render_text_native_is_crlf_nul_terminated_and_rejects_truncation() {
    assert_eq!(
        text_native(b"a\nb\r\nc\rd").unwrap(),
        utf16("a\r\nb\r\nc\rd")
    );
    assert_eq!(
        text_native("\u{1f642}".as_bytes()).unwrap(),
        utf16("\u{1f642}")
    );
    for data in [&b"x\0y"[..], &[0xff][..], &[][..]] {
        assert!(matches!(text_native(data), Err(PlatformError::NotFound)));
    }
    assert!(matches!(
        text_native(&vec![b'x'; TEXT_CAP + 1]),
        Err(PlatformError::TooLarge)
    ));
}
#[test]
fn render_png_to_dibv5_roundtrips_alpha_and_preserves_work_cap() {
    let mut raw = dib(1, -1, 32, 124);
    for (at, value) in [
        (16, 3u32),
        (40, 0xff0000),
        (44, 0xff00),
        (48, 0xff),
        (52, 0xff000000),
    ] {
        raw[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    raw[124..128].copy_from_slice(&[3, 2, 1, 128]);
    let png = dib_png(&raw, IMAGE_CAP).unwrap();
    let native = png_dibv5(&png).unwrap();
    assert_eq!(
        decoded(dib_png(&native, IMAGE_CAP).unwrap()),
        vec![1, 2, 3, 128]
    );
    assert!(png_dibv5(b"not a png").is_err());
    assert!(matches!(
        png_dibv5(&vec![0; IMAGE_CAP + 1]),
        Err(PlatformError::TooLarge)
    ));
}

#[test]
fn render_png_oversized_header_is_refused_before_row_or_plane_allocation() {
    let mut raw = Vec::new();
    let mut encoder = png::Encoder::new(&mut raw, 16385, 1024);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    drop(encoder.write_header().unwrap());
    assert!(raw.len() < 1024);
    assert!(matches!(png_dibv5(&raw), Err(PlatformError::TooLarge)));
}
