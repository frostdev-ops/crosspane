#![cfg(feature = "video")]
use crosspane_media::{codec::CodecError, picture::nv12_to_bgra};
use crosspane_platform_windows::model::video::*;
use crosspane_types::geom::PixelSize;
use proptest::prelude::*;

#[test]
fn odd_dimensions_are_padded_without_changing_real_size() {
    let p = Params::new(PixelSize::new(101, 75), 8_000_000, 30).unwrap();
    assert_eq!(p.size, PixelSize::new(101, 75));
    assert_eq!(p.coded().unwrap(), PixelSize::new(102, 76));
}
#[test]
fn parameter_overflow_and_unbounded_allocation_are_rejected() {
    for size in [
        PixelSize::new(0, 2),
        PixelSize::new(u32::MAX, 2),
        PixelSize::new(8193, 2),
    ] {
        assert!(Params::new(size, 1, 30).and_then(Params::coded).is_err());
    }
    for (rate, fps) in [(0, 30), (1, 0), (1, u32::MAX)] {
        assert!(Params::new(PixelSize::new(2, 2), rate, fps).is_err());
    }
}

#[test]
fn hardware_refusal_falls_back_but_first_usable_candidate_wins() {
    let fail = || Err::<u32, _>(CodecError::Unavailable("fake refusal".into()));
    assert_eq!(prefer_hardware([fail(), fail()], || Ok(3)).unwrap(), 3);
    assert_eq!(
        prefer_hardware([fail(), Ok(2), Ok(4)], || panic!("software must not run")).unwrap(),
        2
    );
    assert!(prefer_hardware([fail()], fail).is_err());
}

fn aperture(x: u32, y: u32, width: u32, height: u32) -> Aperture {
    Aperture {
        x,
        y,
        size: PixelSize::new(width, height),
    }
}
fn area_blob(x: i16, y: i16, width: i32, height: i32) -> Vec<u8> {
    [
        0_u16.to_le_bytes().as_slice(),
        x.to_le_bytes().as_slice(),
        0_u16.to_le_bytes().as_slice(),
        y.to_le_bytes().as_slice(),
        width.to_le_bytes().as_slice(),
        height.to_le_bytes().as_slice(),
    ]
    .concat()
}
#[test]
fn public_aperture_blob_rejects_fractional_negative_and_non_even_geometry() {
    assert_eq!(
        Aperture::from_blob(&area_blob(2, 4, 102, 76)).unwrap(),
        aperture(2, 4, 102, 76)
    );
    for blob in [
        area_blob(-2, 0, 102, 76),
        area_blob(0, 0, -2, 76),
        area_blob(1, 0, 102, 76),
        area_blob(0, 0, 101, 76),
        vec![0; 15],
    ] {
        assert!(Aperture::from_blob(&blob).is_err());
    }
    let mut fractional = area_blob(0, 0, 102, 76);
    fractional[0] = 1;
    assert!(Aperture::from_blob(&fractional).is_err());
}
#[test]
fn odd_coded_aperture_copies_only_valid_rows_from_macroblock_storage() {
    let storage = PixelSize::new(112, 80);
    let expected = aperture(0, 0, 102, 76);
    let stride = 128;
    let mut bytes = vec![0xee; stride * 80 * 3 / 2];
    for row in 0..76 {
        bytes[row * stride..][..102].fill(row as u8);
    }
    for row in 0..38 {
        bytes[stride * 80 + row * stride..][..102].fill(128 + row as u8);
    }
    let got = decoded_nv12(
        storage,
        stride as u32,
        Some(expected),
        expected,
        &bytes,
        Default::default(),
    )
    .unwrap();
    assert_eq!(got.size, PixelSize::new(102, 76));
    assert_eq!((got.y_stride, got.uv_stride), (102, 102));
    assert_eq!(got.y.len(), 102 * 76);
    assert_eq!(got.uv.len(), 102 * 38);
    assert!(
        got.y
            .as_chunks::<102>()
            .0
            .iter()
            .enumerate()
            .all(|(row, data)| data.iter().all(|v| *v == row as u8))
    );
    assert!(
        got.uv
            .as_chunks::<102>()
            .0
            .iter()
            .enumerate()
            .all(|(row, data)| data.iter().all(|v| *v == 128 + row as u8))
    );
}
#[test]
fn missing_mismatched_or_uncontained_aperture_and_invalid_stride_refuse() {
    let storage = PixelSize::new(112, 80);
    let expected = aperture(0, 0, 102, 76);
    let bytes = vec![0; 112 * 80 * 3 / 2];
    for (actual, stride) in [
        (None, 112),
        (Some(aperture(0, 0, 100, 76)), 112),
        (Some(aperture(2, 0, 102, 76)), 112),
        (Some(expected), 111),
        (Some(expected), 113),
        (Some(expected), u32::MAX),
    ] {
        assert!(
            decoded_nv12(
                storage,
                stride,
                actual,
                expected,
                &bytes,
                Default::default()
            )
            .is_err()
        );
    }
    let outside = aperture(12, 6, 102, 76);
    assert!(
        decoded_nv12(
            storage,
            112,
            Some(outside),
            outside,
            &bytes,
            Default::default()
        )
        .is_err()
    );
    assert!(
        decoded_nv12(
            storage,
            112,
            Some(expected),
            expected,
            &bytes[..bytes.len() - 560],
            Default::default()
        )
        .is_err()
    );
}
#[test]
fn validated_even_offsets_use_storage_height_for_uv_and_preserve_colour() {
    let expected = aperture(2, 2, 4, 4);
    let mut bytes = vec![0xee; 8 * 8 * 3 / 2];
    for row in 0..4 {
        bytes[(row + 2) * 8 + 2..][..4].fill(41);
    }
    for row in 0..2 {
        bytes[8 * 8 + (row + 1) * 8 + 2..][..4].fill(123);
    }
    let colour = crosspane_media::picture::YuvColour {
        matrix: crosspane_media::picture::YuvMatrix::Bt601,
        full_range: true,
    };
    let got = decoded_nv12(
        PixelSize::new(8, 8),
        8,
        Some(expected),
        expected,
        &bytes,
        colour,
    )
    .unwrap();
    assert_eq!(got.y, vec![41; 16]);
    assert_eq!(got.uv, vec![123; 8]);
    assert_eq!(got.colour, colour);
}
#[test]
fn bt709_limited_conversion_and_odd_padding_repeat_edge_pixels() {
    let size = PixelSize::new(3, 3);
    let mut pixels = vec![0xee; 16 * 3];
    for y in 0..3 {
        for x in 0..3 {
            pixels[y * 16 + x * 4..][..4].copy_from_slice(&[48, 80, 112, 255]);
        }
    }
    let picture = bgra_to_nv12(&pixels, 16, size).unwrap();
    assert_eq!(picture.size, PixelSize::new(4, 4));
    assert_eq!(picture.colour, Default::default());
    let mut decoded = Vec::new();
    nv12_to_bgra(&picture, picture.size, &mut decoded).unwrap();
    for pixel in decoded.as_chunks::<4>().0 {
        for (got, wanted) in pixel[..3].iter().zip([48_u8, 80, 112]) {
            assert!(got.abs_diff(wanted) <= 2);
        }
        assert_eq!(pixel[3], 255);
    }
}
#[test]
fn short_stride_and_last_row_are_rejected_before_conversion() {
    assert!(matches!(
        bgra_to_nv12(&[0; 16], 4, PixelSize::new(2, 2)),
        Err(CodecError::BadInput(_))
    ));
    assert!(matches!(
        bgra_to_nv12(&[0; 15], 8, PixelSize::new(2, 2)),
        Err(CodecError::BadInput(_))
    ));
}
#[test]
fn rational_timestamps_are_monotonic_without_thirty_fps_drift() {
    let mut clock = Clock::default();
    let mut end = 0;
    for _ in 0..30 {
        let (at, duration) = clock.next(30).unwrap();
        assert_eq!(at, end);
        assert!(duration > 0);
        end = at + duration;
    }
    assert_eq!(end, 10_000_000);
}
fn au(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [0, 0, 0, 1].into_iter().chain(nal.iter().copied()))
        .collect()
}
#[test]
fn every_idr_gets_inline_sps_pps_from_the_current_session() {
    let mut headers = Headers::default();
    let first = au(&[&[0x67, 1], &[0x68, 2], &[0x65, 3]]);
    assert_eq!(headers.packet(&first, true).unwrap(), (first.clone(), true));
    assert_eq!(
        headers.packet(&au(&[&[0x65, 3]]), true).unwrap(),
        (first, true)
    );
}
#[test]
fn forced_idr_refusal_and_missing_headers_are_failures() {
    let mut headers = Headers::default();
    assert!(headers.packet(&au(&[&[0x65, 3]]), false).is_err());
    assert!(headers.packet(&au(&[&[0x41, 3]]), true).is_err());
    assert!(headers.packet(&[1, 2, 3], false).is_err());
}
#[test]
fn async_events_require_input_credit_and_timestamp_matched_output() {
    let mut events = Events::default();
    assert!(events.submit(1).is_err());
    events.need_input().unwrap();
    events.submit(1).unwrap();
    assert!(events.submit(2).is_err());
    assert!(events.complete(1).is_err());
    events.have_output().unwrap();
    assert!(events.complete(2).is_err());
    events.complete(1).unwrap();
    assert!(events.complete(1).is_err());
}
proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_annex_b_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..1024)) {
        let _ = Headers::default().packet(&bytes, false);
    }
}

#[test]
fn conversion_averages_chroma_and_repeats_the_last_column_and_row() {
    let pixels = [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255];
    let picture = bgra_to_nv12(&pixels, 12, PixelSize::new(3, 1)).unwrap();
    assert_eq!(picture.y, [16, 235, 63, 63, 16, 235, 63, 63]);
    assert_eq!(picture.uv, [128, 128, 102, 240]);
}
#[test]
fn last_row_need_not_include_stride_padding() {
    let picture = bgra_to_nv12(&[128; 20], 12, PixelSize::new(2, 2)).unwrap();
    picture.validate().unwrap();
}
#[test]
fn three_byte_startcodes_and_headers_after_idr_are_normalized() {
    let data = [
        0, 0, 1, 0x65, 0x80, 0, 0, 1, 0x67, 0x80, 0, 0, 1, 0x68, 0x80,
    ];
    let (result, key) = Headers::default().packet(&data, false).unwrap();
    assert!(key);
    assert_eq!(
        nals(&result).map(|n| n[0] & 31).take(3).collect::<Vec<_>>(),
        [7, 8, 5]
    );
}
#[test]
fn annex_b_rejects_empty_invalid_and_non_picture_packets() {
    for data in [vec![], vec![0, 0, 1], au(&[&[0xff]]), au(&[&[0x67, 1]])] {
        assert!(Headers::default().packet(&data, false).is_err());
    }
}
#[test]
fn asynchronous_credit_overflow_and_spurious_output_fail_closed() {
    let mut events = Events::default();
    assert!(events.have_output().is_err());
    for _ in 0..16 {
        events.need_input().unwrap();
    }
    assert!(events.need_input().is_err());
}
struct Bits(Vec<bool>);
impl Bits {
    fn value(&mut self, v: u32, width: u8) {
        for shift in (0..width).rev() {
            self.0.push(v >> shift & 1 != 0);
        }
    }
    fn ue(&mut self, v: u32) {
        let width = (v + 1).ilog2();
        self.value(0, width as u8);
        self.value(v + 1, width as u8 + 1);
    }
    fn finish(mut self) -> Vec<u8> {
        self.0.push(true);
        while !self.0.len().is_multiple_of(8) {
            self.0.push(false);
        }
        self.0
            .chunks(8)
            .map(|bits| bits.iter().fold(0, |v, b| v * 2 + u8::from(*b)))
            .collect()
    }
}
fn reference_au(frame: u32, idr: bool, slice: u32) -> Vec<u8> {
    let mut packet = Vec::new();
    if idr {
        let mut sps = Bits(Vec::new());
        sps.value(66, 8);
        sps.value(0, 8);
        sps.value(30, 8);
        sps.ue(0);
        sps.ue(0);
        let mut pps = Bits(Vec::new());
        pps.ue(0);
        pps.ue(0);
        for (header, bytes) in [(0x67, sps.finish()), (0x68, pps.finish())] {
            packet.extend_from_slice(&[0, 0, 0, 1, header]);
            packet.extend_from_slice(&bytes);
        }
    }
    let mut bits = Bits(Vec::new());
    bits.ue(0);
    bits.ue(slice);
    bits.ue(0);
    bits.value(frame, 4);
    packet.extend_from_slice(&[0, 0, 0, 1, if idr { 0x65 } else { 0x41 }]);
    packet.extend_from_slice(&bits.finish());
    packet
}
fn geometry_sps(profile: u32, chroma: u32, progressive: bool, crop: [u32; 4]) -> Vec<u8> {
    let mut bits = Bits(Vec::new());
    bits.value(profile, 8);
    bits.value(0, 8);
    bits.value(30, 8);
    bits.ue(0);
    if profile == 100 {
        bits.ue(chroma);
        bits.ue(0);
        bits.ue(0);
        bits.value(0, 1);
        bits.value(0, 1);
    }
    bits.ue(0); // log2_max_frame_num_minus4
    bits.ue(2); // pic_order_cnt_type
    bits.ue(1); // max_num_ref_frames
    bits.value(0, 1); // gaps
    bits.ue(6);
    bits.ue(4); // macroblock allocation 112x80
    bits.value(u32::from(progressive), 1);
    if !progressive {
        bits.value(0, 1);
    }
    bits.value(1, 1); // direct_8x8_inference
    bits.value(1, 1); // frame cropping
    for offset in crop {
        bits.ue(offset);
    }
    bits.value(0, 1); // no VUI
    let mut output = vec![0, 0, 0, 1, 0x67];
    let mut zeros = 0;
    for byte in bits.finish() {
        if zeros == 2 && byte <= 3 {
            output.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        output.push(byte);
    }
    output
}
#[test]
fn sps_progressive_420_crop_establishes_exact_even_coded_geometry() {
    for profile in [66, 100] {
        let packet = geometry_sps(profile, 1, true, [0, 5, 0, 2]);
        assert_eq!(h264_aperture(&packet).unwrap(), aperture(0, 0, 102, 76));
    }
    assert_eq!(
        h264_aperture(&geometry_sps(66, 1, true, [1, 4, 1, 1])).unwrap(),
        aperture(2, 2, 102, 76)
    );
}
#[test]
fn sps_unsupported_ambiguous_overflow_and_malformed_geometry_refuse() {
    for packet in [
        geometry_sps(100, 2, true, [0, 5, 0, 2]),
        geometry_sps(66, 1, false, [0, 5, 0, 2]),
        geometry_sps(255, 1, true, [0, 5, 0, 2]),
        geometry_sps(66, 1, true, [0, 56, 0, 2]),
        geometry_sps(66, 1, true, [0, 5, u32::MAX - 1, 2]),
        vec![0, 0, 0, 1, 0x67, 66, 0, 30],
        vec![0, 0, 0, 1, 0x67, 0, 0, 3, 4],
    ] {
        assert!(h264_aperture(&packet).is_err());
    }
    let one = geometry_sps(66, 1, true, [0, 5, 0, 2]);
    assert!(h264_aperture(&[one.clone(), one].concat()).is_err());
}
#[test]
fn overflowing_sps_frame_number_length_never_panics() {
    let mut bits = Bits(Vec::new());
    bits.value(66, 8);
    bits.value(0, 8);
    bits.value(30, 8);
    bits.ue(0);
    bits.ue(u32::MAX - 1);
    let packet = [
        au(&[&[&[0x67], bits.finish().as_slice()].concat()]),
        reference_au(0, true, 2),
    ]
    .concat();
    assert!(References::default().check(&packet).is_err());
}
#[test]
fn missing_reference_requires_idr_and_idr_recovers_independently() {
    let mut refs = References::default();
    assert!(refs.check(&reference_au(1, false, 0)).is_err());
    refs.check(&reference_au(0, true, 2)).unwrap();
    refs.check(&reference_au(1, false, 0)).unwrap();
    assert!(refs.check(&reference_au(3, false, 0)).is_err());
    assert!(refs.check(&reference_au(2, false, 0)).is_err());
    refs.check(&reference_au(0, true, 2)).unwrap();
    refs.check(&reference_au(1, false, 0)).unwrap();
}
#[test]
fn duplicate_reference_and_b_slice_are_rejected() {
    let mut refs = References::default();
    refs.check(&reference_au(0, true, 2)).unwrap();
    refs.check(&reference_au(1, false, 0)).unwrap();
    assert!(refs.check(&reference_au(1, false, 0)).is_err());
    refs.check(&reference_au(0, true, 2)).unwrap();
    assert!(refs.check(&reference_au(1, false, 1)).is_err());
}
#[test]
fn reference_number_wrap_and_multislice_access_unit_are_allowed() {
    let mut refs = References::default();
    refs.check(&reference_au(0, true, 2)).unwrap();
    for frame in 1..16 {
        refs.check(&reference_au(frame, false, 0)).unwrap();
    }
    let one = reference_au(0, false, 0);
    let mut multiple = one.clone();
    multiple.extend_from_slice(&one);
    refs.check(&multiple).unwrap();
}
proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_reference_prefixes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = References::default().check(&bytes);
    }
    #[test]
    fn arbitrary_sps_and_aperture_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..8192)) {
        let _ = h264_aperture(&bytes);
        let _ = Aperture::from_blob(&bytes);
    }
}
