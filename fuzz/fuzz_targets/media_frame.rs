#![no_main]

use crosspane_media::{
    tiles::{TileDecoder, TileEncoder},
    wire::{FrameHeader, read_header},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let header = FrameHeader {
        projection: 1,
        seq: 1,
        key: true,
        captured_ns: 0,
        width: 65,
        height: 65,
    };
    let mut key = Vec::new();
    let mut encoder = TileEncoder::new();
    assert!(
        encoder
            .encode(header, &vec![0x33; 65 * 65 * 4], 65 * 4, true, &mut key)
            .is_ok()
    );
    let mut decoder = TileDecoder::new();
    assert!(decoder.apply(&key).is_ok());
    let before = decoder.canvas().0.to_vec();
    let size = decoder.canvas().1;
    if decoder.apply(data).is_err() {
        assert_eq!(decoder.canvas(), (before.as_slice(), size));
    }
    let _ = read_header(data);
});
