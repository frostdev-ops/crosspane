#![no_main]

use libfuzzer_sys::fuzz_target;

use crosspane_protocol::wire::{decode_pointer, encode_pointer};

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = decode_pointer(data) {
        assert_eq!(encode_pointer(&msg).unwrap(), data);
    }
});
