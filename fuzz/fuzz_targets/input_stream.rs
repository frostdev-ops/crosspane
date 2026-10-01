#![no_main]

use libfuzzer_sys::fuzz_target;

use crosspane_protocol::wire::{FrameDecoder, MAX_INPUT_PAYLOAD, decode_input, encode_input};

fuzz_target!(|data: &[u8]| {
    let mut decoder = FrameDecoder::new(MAX_INPUT_PAYLOAD);
    let mut remaining = data;
    while !remaining.is_empty() {
        // Use the next stream byte to choose a chunk, but also feed it to the decoder.
        let len = (usize::from(remaining[0]) + 1).min(remaining.len());
        decoder.push(&remaining[..len]);
        remaining = &remaining[len..];
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let Ok(msg) = decode_input(&frame) {
                        let mut encoded = Vec::new();
                        encode_input(&msg, &mut encoded).unwrap();
                        let mut round_trip = FrameDecoder::new(MAX_INPUT_PAYLOAD);
                        round_trip.push(&encoded);
                        let frame = round_trip.next_frame().unwrap().unwrap();
                        assert_eq!(decode_input(&frame).unwrap(), msg);
                        assert_eq!(round_trip.next_frame().unwrap(), None);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    assert_eq!(decoder.next_frame(), Err(error));
                    return;
                }
            }
        }
    }
});
