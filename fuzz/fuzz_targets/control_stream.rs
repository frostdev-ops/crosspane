#![no_main]

use crosspane_protocol::wire::{
    FrameDecoder, KIND_CONTROL, MAX_CONTROL_PAYLOAD, decode_control, encode_control,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut decoder = FrameDecoder::new(MAX_CONTROL_PAYLOAD);
    let mut offset = 0;
    while offset < data.len() {
        // Use input-derived chunk sizes without removing bytes from the framed stream.
        let end = (offset + usize::from(data[offset]) + 1).min(data.len());
        decoder.push(&data[offset..end]);
        offset = end;
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => return,
            };
            if frame.kind != KIND_CONTROL {
                continue;
            }
            if let Ok(message) = decode_control(&frame) {
                let mut encoded = Vec::new();
                assert_eq!(encode_control(&message, &mut encoded), Ok(()));
                let mut round_trip = FrameDecoder::new(MAX_CONTROL_PAYLOAD);
                round_trip.push(&encoded);
                let Ok(Some(reencoded)) = round_trip.next_frame() else {
                    panic!("a successfully encoded control message must form a frame");
                };
                assert_eq!(decode_control(&reencoded), Ok(message));
            }
        }
    }
});
