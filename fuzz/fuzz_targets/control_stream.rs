#![no_main]

use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetch, ClipFetchFailed, ClipFetchId, ClipOffer, ClipOfferId,
    ClipWithdraw, ControlMessage,
};
use crosspane_protocol::wire::{
    FrameDecoder, KIND_CONTROL, MAX_CONTROL_PAYLOAD, decode_control, encode_control,
};
use crosspane_types::ClipKind;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Structured cases make every CLIP-v0 variant reachable even before corpus mutation has
    // discovered its newly frozen protobuf tag. The raw decoder below still probes invalid data.
    let value = data.first().copied().unwrap_or(0);
    let id = u64::from(value);
    let kind = if value & 1 == 0 {
        ClipKind::Text
    } else {
        ClipKind::Image
    };
    let kinds = if value & 2 == 0 {
        vec![kind]
    } else {
        vec![ClipKind::Text, ClipKind::Image]
    };
    let reason = [
        ClipFailure::Expired,
        ClipFailure::Locked,
        ClipFailure::NotGranted,
        ClipFailure::TooLarge,
        ClipFailure::Unavailable,
    ][usize::from(value) % 5];
    for message in [
        ControlMessage::ClipOffer(ClipOffer {
            offer: ClipOfferId(id),
            kinds,
        }),
        ControlMessage::ClipWithdraw(ClipWithdraw {
            offer: ClipOfferId(id),
        }),
        ControlMessage::ClipFetch(ClipFetch {
            fetch: ClipFetchId(id),
            offer: ClipOfferId(id),
            kind,
        }),
        ControlMessage::ClipFetchFailed(ClipFetchFailed {
            fetch: ClipFetchId(id),
            reason,
        }),
        ControlMessage::Grants(vec![Capability::ClipboardRead, Capability::ClipboardWrite]),
    ] {
        let mut encoded = Vec::new();
        assert_eq!(encode_control(&message, &mut encoded), Ok(()));
        let mut framed = FrameDecoder::new(MAX_CONTROL_PAYLOAD);
        framed.push(&encoded);
        let frame = framed.next_frame().unwrap().unwrap();
        assert_eq!(decode_control(&frame), Ok(message));
    }
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
