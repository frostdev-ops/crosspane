#![no_main]
use crosspane_media::audio::wire::{decode_audio, encode_audio};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = decode_audio(data) {
        assert!(packet.stream.0 != 0);
        assert!((1..=400).contains(&packet.opus.len()));
        let encoded = encode_audio(&packet).expect("parsed packets encode");
        assert_eq!(encoded, data);
        assert_eq!(decode_audio(&encoded).expect("roundtrip"), packet);
    }
});
