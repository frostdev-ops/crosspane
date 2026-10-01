//! Codec-2 cursor frames (WP-2.16).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::tiles::TileDecoder;
use crosspane_media::wire::{
    Codec, FrameHeader, MAX_CURSOR, MediaError, read_codec, read_cursor, read_header, read_video,
    write_cursor, write_default_cursor,
};
use proptest::prelude::*;

fn header(width: u32, height: u32) -> FrameHeader {
    FrameHeader {
        projection: 7,
        seq: 3,
        key: false,
        captured_ns: 99,
        width,
        height,
    }
}

fn image(width: u32, height: u32) -> Vec<u8> {
    (0..width * height * 4).map(|i| (i % 251) as u8).collect()
}

#[test]
fn a_cursor_round_trips() {
    let pixels = image(32, 24);
    let mut out = Vec::new();
    write_cursor(header(32, 24), (5, 7), &pixels, &mut out).unwrap();
    assert_eq!(read_codec(&out), Ok(Codec::Cursor));
    assert_eq!(read_header(&out), Ok(header(32, 24)));
    let frame = read_cursor(&out).unwrap();
    assert!(!frame.default);
    assert_eq!(frame.header, header(32, 24));
    assert_eq!(frame.hotspot, (5, 7));
    assert_eq!(frame.pixels, pixels.as_slice());
}

#[test]
fn write_refuses_bad_cursors() {
    let mut out = vec![1, 2, 3];
    let key = FrameHeader {
        key: true,
        ..header(4, 4)
    };
    assert_eq!(
        write_cursor(key, (0, 0), &image(4, 4), &mut out),
        Err(MediaError::BadReserved)
    );
    assert!(out.is_empty());
    for (w, h) in [(0, 4), (4, 0), (MAX_CURSOR + 1, 4), (4, MAX_CURSOR + 1)] {
        assert_eq!(
            write_cursor(header(w, h), (0, 0), &image(w, h), &mut out),
            Err(MediaError::BadSize)
        );
    }
    assert_eq!(
        write_cursor(header(4, 4), (0, 0), &image(4, 3), &mut out),
        Err(MediaError::BadPayload)
    );
    assert_eq!(
        write_cursor(header(4, 4), (4, 0), &image(4, 4), &mut out),
        Err(MediaError::BadPayload)
    );
    assert_eq!(
        write_cursor(header(4, 4), (0, 4), &image(4, 4), &mut out),
        Err(MediaError::BadPayload)
    );
    write_cursor(
        header(MAX_CURSOR, MAX_CURSOR),
        (MAX_CURSOR - 1, MAX_CURSOR - 1),
        &image(MAX_CURSOR, MAX_CURSOR),
        &mut out,
    )
    .unwrap();
}

fn valid() -> Vec<u8> {
    let mut out = Vec::new();
    write_cursor(header(4, 4), (1, 2), &image(4, 4), &mut out).unwrap();
    out
}

#[test]
fn read_refuses_malformed_cursors() {
    // Key flag set.
    let mut frame = valid();
    frame[5] = 1;
    assert_eq!(read_cursor(&frame), Err(MediaError::BadReserved));
    // Unknown flag bit.
    let mut frame = valid();
    frame[5] = 4;
    assert_eq!(read_cursor(&frame), Err(MediaError::BadReserved));
    // Tile-size field set (bytes 40..42).
    let mut frame = valid();
    frame[40] = 64;
    assert_eq!(read_cursor(&frame), Err(MediaError::BadCodec));
    // Too large an image (width at bytes 32..36).
    let mut frame = valid();
    frame[32..36].copy_from_slice(&(MAX_CURSOR + 1).to_le_bytes());
    assert_eq!(read_cursor(&frame), Err(MediaError::BadSize));
    // Count that doesn't match the image (bytes 44..48).
    let mut frame = valid();
    frame[44..48].copy_from_slice(&(8 + 4 * 4 * 4 - 1_u32).to_le_bytes());
    assert_eq!(read_cursor(&frame), Err(MediaError::BadPayload));
    // Truncated and trailing payloads.
    let frame = valid();
    assert_eq!(
        read_cursor(&frame[..frame.len() - 1]),
        Err(MediaError::Truncated)
    );
    let mut frame = valid();
    frame.push(0);
    assert_eq!(read_cursor(&frame), Err(MediaError::Trailing));
    // Hotspot outside the image (bytes 48..52 and 52..56).
    let mut frame = valid();
    frame[48..52].copy_from_slice(&4_u32.to_le_bytes());
    assert_eq!(read_cursor(&frame), Err(MediaError::BadPayload));
    let mut frame = valid();
    frame[52..56].copy_from_slice(&4_u32.to_le_bytes());
    assert_eq!(read_cursor(&frame), Err(MediaError::BadPayload));
}

#[test]
fn default_cursor_frames_round_trip_and_are_strict() {
    let mut out = Vec::new();
    write_default_cursor(header(40, 40), &mut out).unwrap();
    let frame = read_cursor(&out).unwrap();
    assert!(frame.default);
    assert_eq!((frame.header.width, frame.header.height), (1, 1));
    assert_eq!((frame.header.projection, frame.header.seq), (7, 3));
    assert_eq!(frame.hotspot, (0, 0));
    // The default flag with a real image is malformed.
    let mut image = Vec::new();
    write_cursor(header(4, 4), (1, 1), &[255; 64], &mut image).unwrap();
    image[5] = 2;
    assert_eq!(read_cursor(&image), Err(MediaError::BadPayload));
    // The default and key flags together are reserved.
    out[5] = 3;
    assert_eq!(read_cursor(&out), Err(MediaError::BadReserved));
}

#[test]
fn other_codecs_refuse_cursor_frames() {
    let frame = valid();
    assert_eq!(read_video(&frame).map(|_| ()), Err(MediaError::BadCodec));
    let mut decoder = TileDecoder::new();
    assert!(decoder.apply(&frame).is_err());
}

proptest! {
    #[test]
    fn cursors_round_trip(
        width in 1..=64_u32,
        height in 1..=64_u32,
        hx in 0..64_u32,
        hy in 0..64_u32,
        seq in any::<u64>(),
        projection in any::<u64>(),
        captured_ns in any::<u64>(),
        fill in any::<u8>(),
    ) {
        let hotspot = (hx % width, hy % height);
        let header = FrameHeader { projection, seq, key: false, captured_ns, width, height };
        let pixels = vec![fill; (width * height * 4) as usize];
        let mut out = Vec::new();
        write_cursor(header, hotspot, &pixels, &mut out).unwrap();
        let frame = read_cursor(&out).unwrap();
        prop_assert_eq!(frame.header, header);
        prop_assert_eq!(frame.hotspot, hotspot);
        prop_assert_eq!(frame.pixels, pixels.as_slice());
    }

    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..200)) {
        let _ = read_cursor(&data);
    }
}
