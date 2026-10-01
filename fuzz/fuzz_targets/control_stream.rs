#![no_main]

use libfuzzer_sys::fuzz_target;

// Written in WP-1.1 (input_stream, pointer_datagram) and WP-1.2 (control_stream).
fuzz_target!(|data: &[u8]| {
    let _ = data;
});
