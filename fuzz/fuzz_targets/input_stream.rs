#![no_main]

use libfuzzer_sys::fuzz_target;

use crosspane_protocol::msg::{InputMessage, TargetStatus};
use crosspane_protocol::wire::{FrameDecoder, MAX_INPUT_PAYLOAD, decode_input, encode_input};
use crosspane_types::geom::PixelSize;
use crosspane_types::id::{ProjectionId, SessionId, WindowId};

fuzz_target!(|data: &[u8]| {
    // Reach the new fixed kind without depending on a mutator first guessing its header.
    let value = data.first().copied().unwrap_or(0);
    for status in [
        TargetStatus::NativeMove {
            window: WindowId(u64::from(value)),
            proxy: (value & 1 != 0).then_some(ProjectionId(u64::from(value))),
            grab: (i32::from(value), -i32::from(value)),
            size: PixelSize::new(u32::from(value), u32::MAX),
        },
        TargetStatus::NativeMoveEnded {
            window: WindowId(u64::from(value)),
        },
    ] {
        let message = InputMessage::Status {
            session: SessionId(u64::from(value)),
            status,
        };
        let mut bytes = Vec::new();
        encode_input(&message, &mut bytes).unwrap();
        let mut framed = FrameDecoder::new(MAX_INPUT_PAYLOAD);
        framed.push(&bytes);
        let frame = framed.next_frame().unwrap().unwrap();
        assert_eq!(frame.payload.len(), 42);
        assert_eq!(decode_input(&frame).unwrap(), message);
    }
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
