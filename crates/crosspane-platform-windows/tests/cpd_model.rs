//! CPD1 codec rows (WP-W3.1b T2). The layouts below are written from
//! `drivers/windows-idd/include/crosspane_idd_v1.h` (its `CPD_ASSERT` lines), separately from the
//! codec's private offsets, so a wrong offset in either place fails here.

use crosspane_platform_windows::model::cpd::{
    ADD_REQUEST_BYTES, ADD_RESPONSE_BYTES, AddReply, CONTROL_INTERFACE_GUID, CpdError, CpdMode,
    HARDWARE_ID, HEARTBEAT_REQUEST_BYTES, HEARTBEAT_RESPONSE_BYTES, HeartbeatReply, IOCTL_ADD,
    IOCTL_HEARTBEAT, IOCTL_LIST, IOCTL_REMOVE, LEASE_MS, LIST_REQUEST_BYTES, LIST_RESPONSE_BYTES,
    ListReply, MAGIC, MAJOR, MINOR, MONITOR_ACTIVE, MONITOR_RETIRED, MONITORS_PER_OPEN,
    REMOVE_REQUEST_BYTES, REMOVE_RESPONSE_BYTES, decode_add, decode_heartbeat, decode_list,
    decode_remove, encode_add, encode_heartbeat, encode_list, encode_remove, valid_mode,
};
use proptest::prelude::*;

const MODE_720: CpdMode = CpdMode {
    width: 1280,
    height: 720,
    width_mm: 339,
    height_mm: 191,
};
const MODE_1080: CpdMode = CpdMode {
    width: 1920,
    height: 1080,
    width_mm: 527,
    height_mm: 296,
};
const REQ: u64 = 0x1122_3344_5566_7788;
const SEQ: u64 = 9;

// CPD_HEADER (32 bytes): magic u32 @0, major u16 @4, minor u16 @6, struct_bytes u32 @8,
// flags u32 @12, request_id u64 @16, reserved u64 @24.
const MAGIC_AT: usize = 0;
const MAJOR_AT: usize = 4;
const MINOR_AT: usize = 6;
const STRUCT_BYTES_AT: usize = 8;
const FLAGS_AT: usize = 12;
const REQUEST_ID_AT: usize = 16;
const HEADER_RESERVED_AT: usize = 24;

fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// A valid header for a frame of `struct_bytes` bytes that echoes `request_id`.
fn header(buf: &mut [u8], struct_bytes: u32, request_id: u64) {
    put_u32(buf, MAGIC_AT, 0x3144_5043);
    put_u16(buf, MAJOR_AT, 1);
    put_u16(buf, MINOR_AT, 0);
    put_u32(buf, STRUCT_BYTES_AT, struct_bytes);
    put_u32(buf, FLAGS_AT, 0);
    put_u64(buf, REQUEST_ID_AT, request_id);
    put_u64(buf, HEADER_RESERVED_AT, 0);
}

/// ADD reply: monitor_id u64 @32, remaining_ms @40, reserved @44.
fn add_reply(request_id: u64, monitor_id: u64, remaining_ms: u32) -> [u8; 48] {
    let mut reply = [0_u8; 48];
    header(&mut reply, 48, request_id);
    put_u64(&mut reply, 32, monitor_id);
    put_u32(&mut reply, 40, remaining_ms);
    reply
}

/// REMOVE reply: monitor_id u64 @32, state @40, reserved @44.
fn remove_reply(request_id: u64, monitor_id: u64, state: u32) -> [u8; 48] {
    let mut reply = [0_u8; 48];
    header(&mut reply, 48, request_id);
    put_u64(&mut reply, 32, monitor_id);
    put_u32(&mut reply, 40, state);
    reply
}

/// HEARTBEAT reply: remaining_ms @32, active_count @36, accepted_sequence u64 @40.
fn heartbeat_reply(request_id: u64, remaining_ms: u32, active: u32, accepted: u64) -> [u8; 48] {
    let mut reply = [0_u8; 48];
    header(&mut reply, 48, request_id);
    put_u32(&mut reply, 32, remaining_ms);
    put_u32(&mut reply, 36, active);
    put_u64(&mut reply, 40, accepted);
    reply
}

/// LIST reply: count @32, capacity @36, remaining_ms @40, reserved @44, entry @48 (all zero).
fn list_reply(request_id: u64, count: u32, capacity: u32, remaining_ms: u32) -> [u8; 96] {
    let mut reply = [0_u8; 96];
    header(&mut reply, 96, request_id);
    put_u32(&mut reply, 32, count);
    put_u32(&mut reply, 36, capacity);
    put_u32(&mut reply, 40, remaining_ms);
    reply
}

/// Fills the LIST entry at 48: monitor_id u64 +0, mode +8 (8 u32s), state +40, reserved +44.
fn put_entry(reply: &mut [u8], monitor_id: u64, mode: CpdMode, state: u32) {
    put_u64(reply, 48, monitor_id);
    put_u32(reply, 56, mode.width);
    put_u32(reply, 60, mode.height);
    put_u32(reply, 64, 60);
    put_u32(reply, 68, 1);
    put_u32(reply, 72, mode.width_mm);
    put_u32(reply, 76, mode.height_mm);
    put_u32(reply, 80, 32);
    put_u32(reply, 84, 0);
    put_u32(reply, 88, state);
    put_u32(reply, 92, 0);
}

/// A LIST reply with one live monitor in `MODE_720`.
fn list_one(request_id: u64, monitor_id: u64, remaining_ms: u32) -> [u8; 96] {
    let mut reply = list_reply(request_id, 1, 1, remaining_ms);
    put_entry(&mut reply, monitor_id, MODE_720, MONITOR_ACTIVE);
    reply
}

fn ctl_code(function: u32, access: u32) -> u32 {
    (0x8337 << 16) | (access << 14) | (function << 2)
}

/// Flips the low bit of each header byte in turn. Every flip must be refused: the request id
/// bytes (16..24) as `Echo`, every other header byte as `Header`.
fn assert_every_header_byte_refused<T>(good: &[u8], decode: impl Fn(&[u8]) -> Result<T, CpdError>) {
    assert!(
        decode(good).is_ok(),
        "the fixture must decode before it is corrupted"
    );
    for at in 0..32 {
        let mut bad = good.to_vec();
        bad[at] ^= 0x01;
        let want = if (16..24).contains(&at) {
            CpdError::Echo
        } else {
            CpdError::Header
        };
        assert_eq!(decode(&bad).err(), Some(want), "header byte {at}");
    }
}

/// Every length other than the exact one is refused as `Size`.
fn assert_wrong_length_refused<T>(good: &[u8], decode: impl Fn(&[u8]) -> Result<T, CpdError>) {
    assert_eq!(decode(&[]).err(), Some(CpdError::Size));
    assert_eq!(decode(&good[..good.len() - 1]).err(), Some(CpdError::Size));
    let mut long = good.to_vec();
    long.push(0);
    assert_eq!(decode(&long).err(), Some(CpdError::Size));
}

#[test]
fn constants_match_the_header() {
    assert_eq!(MAGIC, 0x3144_5043);
    assert_eq!(MAGIC.to_le_bytes(), *b"CPD1");
    assert_eq!(MAJOR, 1);
    assert_eq!(MINOR, 0);
    assert_eq!(
        format!(
            "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
            CONTROL_INTERFACE_GUID >> 96,
            (CONTROL_INTERFACE_GUID >> 80) & 0xffff,
            (CONTROL_INTERFACE_GUID >> 64) & 0xffff,
            (CONTROL_INTERFACE_GUID >> 48) & 0xffff,
            CONTROL_INTERFACE_GUID & 0xffff_ffff_ffff,
        ),
        "FEA027FB-8535-40EB-B887-1401D63EF273"
    );
    assert_eq!(HARDWARE_ID, r"Crosspane\IddTwinV1");
    assert_eq!(MONITOR_ACTIVE, 1);
    assert_eq!(MONITOR_RETIRED, 2);
    assert_eq!(MONITORS_PER_OPEN, 1);
    assert_eq!(LEASE_MS, 5_000);
}

#[test]
fn frame_sizes_match_the_header() {
    // CPD_ADD_REQUEST 64, CPD_ADD_RESPONSE 48, CPD_REMOVE_REQUEST 48, CPD_REMOVE_RESPONSE 48,
    // CPD_LIST_REQUEST 32, CPD_LIST_RESPONSE 96, CPD_HEARTBEAT_REQUEST 48, CPD_HEARTBEAT_RESPONSE 48.
    assert_eq!(ADD_REQUEST_BYTES, 64);
    assert_eq!(ADD_RESPONSE_BYTES, 48);
    assert_eq!(REMOVE_REQUEST_BYTES, 48);
    assert_eq!(REMOVE_RESPONSE_BYTES, 48);
    assert_eq!(LIST_REQUEST_BYTES, 32);
    assert_eq!(LIST_RESPONSE_BYTES, 96);
    assert_eq!(HEARTBEAT_REQUEST_BYTES, 48);
    assert_eq!(HEARTBEAT_RESPONSE_BYTES, 48);
}

#[test]
fn ioctl_values_are_the_ctl_codes() {
    // CTL_CODE(0x8337, function, METHOD_BUFFERED, access): ADD, REMOVE and HEARTBEAT use
    // READ|WRITE (3), LIST uses READ (1).
    assert_eq!(IOCTL_ADD, ctl_code(0x800, 3));
    assert_eq!(IOCTL_REMOVE, ctl_code(0x801, 3));
    assert_eq!(IOCTL_LIST, ctl_code(0x802, 1));
    assert_eq!(IOCTL_HEARTBEAT, ctl_code(0x803, 3));
    assert_eq!(IOCTL_ADD, 0x8337_e000);
    assert_eq!(IOCTL_REMOVE, 0x8337_e004);
    assert_eq!(IOCTL_LIST, 0x8337_6008);
    assert_eq!(IOCTL_HEARTBEAT, 0x8337_e00c);
}

#[test]
fn valid_mode_table() {
    // (width, height, width_mm, height_mm, accepted)
    let cases: &[(u32, u32, u32, u32, bool)] = &[
        (1280, 720, 339, 191, true),
        (1920, 1080, 527, 296, true),
        (1280, 720, 10, 10, true),
        (1280, 720, 2000, 2000, true),
        (1920, 1080, 10, 2000, true),
        (1280, 720, 9, 191, false),
        (1280, 720, 2001, 191, false),
        (1280, 720, 339, 9, false),
        (1280, 720, 339, 2001, false),
        (1280, 720, 0, 0, false),
        (1920, 720, 339, 191, false),
        (1280, 1080, 339, 191, false),
        (1024, 768, 339, 191, false),
        (3840, 2160, 527, 296, false),
        (0, 0, 339, 191, false),
        (1920, 1080, u32::MAX, 296, false),
    ];
    for &(width, height, width_mm, height_mm, accepted) in cases {
        let mode = CpdMode {
            width,
            height,
            width_mm,
            height_mm,
        };
        assert_eq!(valid_mode(mode), accepted, "{mode:?}");
    }
    for mm in 0..=2_001 {
        for (width, height) in [(1280, 720), (1920, 1080)] {
            let mode = CpdMode {
                width,
                height,
                width_mm: mm,
                height_mm: mm,
            };
            assert_eq!(valid_mode(mode), (10..=2_000).contains(&mm), "{mode:?}");
        }
    }
}

#[test]
fn encode_add_matches_the_header_layout() {
    let frame = encode_add(REQ, MODE_1080).expect("valid mode");
    let mut want = [0_u8; 64];
    header(&mut want, 64, REQ);
    // CPD_MODE at 32: width, height, refresh num, refresh den, width_mm, height_mm, bpp, reserved.
    put_u32(&mut want, 32, 1920);
    put_u32(&mut want, 36, 1080);
    put_u32(&mut want, 40, 60);
    put_u32(&mut want, 44, 1);
    put_u32(&mut want, 48, 527);
    put_u32(&mut want, 52, 296);
    put_u32(&mut want, 56, 32);
    put_u32(&mut want, 60, 0);
    assert_eq!(frame, want);
}

#[test]
fn encode_remove_matches_the_header_layout() {
    let frame = encode_remove(REQ, 0x0102_0304).expect("valid id");
    let mut want = [0_u8; 48];
    header(&mut want, 48, REQ);
    put_u64(&mut want, 32, 0x0102_0304);
    put_u64(&mut want, 40, 0);
    assert_eq!(frame, want);
}

#[test]
fn encode_list_matches_the_header_layout() {
    let frame = encode_list(REQ).expect("valid id");
    let mut want = [0_u8; 32];
    header(&mut want, 32, REQ);
    assert_eq!(frame, want);
}

#[test]
fn encode_heartbeat_matches_the_header_layout() {
    let frame = encode_heartbeat(REQ, 0x0a0b_0c0d).expect("valid ids");
    let mut want = [0_u8; 48];
    header(&mut want, 48, REQ);
    put_u64(&mut want, 32, 0x0a0b_0c0d);
    put_u64(&mut want, 40, 0);
    assert_eq!(frame, want);
}

#[test]
fn encoders_refuse_zero_ids_and_bad_modes() {
    assert_eq!(encode_add(0, MODE_720), Err(CpdError::ZeroId));
    assert_eq!(encode_remove(0, 7), Err(CpdError::ZeroId));
    assert_eq!(encode_remove(REQ, 0), Err(CpdError::ZeroId));
    assert_eq!(encode_list(0), Err(CpdError::ZeroId));
    assert_eq!(encode_heartbeat(0, SEQ), Err(CpdError::ZeroId));
    assert_eq!(encode_heartbeat(REQ, 0), Err(CpdError::ZeroId));
    let bad = CpdMode {
        width: 1024,
        height: 768,
        width_mm: 339,
        height_mm: 191,
    };
    assert_eq!(encode_add(REQ, bad), Err(CpdError::Mode));
}

#[test]
fn valid_replies_decode() {
    assert_eq!(
        decode_add(REQ, &add_reply(REQ, 7, LEASE_MS)),
        Ok(AddReply {
            monitor_id: 7,
            remaining_ms: LEASE_MS,
        })
    );
    assert_eq!(
        decode_remove(REQ, 7, &remove_reply(REQ, 7, MONITOR_RETIRED)),
        Ok(())
    );
    assert_eq!(
        decode_list(REQ, &list_reply(REQ, 0, 1, 0)),
        Ok(ListReply {
            remaining_ms: 0,
            monitor: None,
        })
    );
    assert_eq!(
        decode_list(REQ, &list_one(REQ, 7, 4_999)),
        Ok(ListReply {
            remaining_ms: 4_999,
            monitor: Some((7, MODE_720)),
        })
    );
    assert_eq!(
        decode_heartbeat(REQ, SEQ, &heartbeat_reply(REQ, 5_000, 1, SEQ)),
        Ok(HeartbeatReply {
            remaining_ms: 5_000,
            active_count: 1,
        })
    );
    assert_eq!(
        decode_heartbeat(REQ, SEQ, &heartbeat_reply(REQ, 0, 0, SEQ)),
        Ok(HeartbeatReply {
            remaining_ms: 0,
            active_count: 0,
        })
    );
}

#[test]
fn every_header_byte_is_checked() {
    assert_every_header_byte_refused(&add_reply(REQ, 7, 1), |r| decode_add(REQ, r));
    assert_every_header_byte_refused(&remove_reply(REQ, 7, MONITOR_RETIRED), |r| {
        decode_remove(REQ, 7, r)
    });
    assert_every_header_byte_refused(&list_reply(REQ, 0, 1, 0), |r| decode_list(REQ, r));
    assert_every_header_byte_refused(&list_one(REQ, 7, 0), |r| decode_list(REQ, r));
    assert_every_header_byte_refused(&heartbeat_reply(REQ, 1, 1, SEQ), |r| {
        decode_heartbeat(REQ, SEQ, r)
    });
}

#[test]
fn wrong_length_is_refused() {
    assert_wrong_length_refused(&add_reply(REQ, 7, 1), |r| decode_add(REQ, r));
    assert_wrong_length_refused(&remove_reply(REQ, 7, MONITOR_RETIRED), |r| {
        decode_remove(REQ, 7, r)
    });
    assert_wrong_length_refused(&list_reply(REQ, 0, 1, 0), |r| decode_list(REQ, r));
    assert_wrong_length_refused(&list_one(REQ, 7, 0), |r| decode_list(REQ, r));
    assert_wrong_length_refused(&heartbeat_reply(REQ, 1, 1, SEQ), |r| {
        decode_heartbeat(REQ, SEQ, r)
    });
}

#[test]
fn request_and_monitor_ids_must_be_nonzero() {
    assert_eq!(decode_add(0, &add_reply(0, 7, 1)), Err(CpdError::ZeroId));
    assert_eq!(
        decode_remove(0, 7, &remove_reply(0, 7, MONITOR_RETIRED)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_remove(REQ, 0, &remove_reply(REQ, 0, MONITOR_RETIRED)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_list(0, &list_reply(0, 0, 1, 0)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_heartbeat(0, SEQ, &heartbeat_reply(0, 1, 1, SEQ)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_heartbeat(REQ, 0, &heartbeat_reply(REQ, 1, 1, 0)),
        Err(CpdError::ZeroId)
    );
    // An ADD reply must name a nonzero monitor, and the id must fit in u32.
    assert_eq!(
        decode_add(REQ, &add_reply(REQ, 0, 1)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_add(REQ, &add_reply(REQ, 1 << 32, 1)),
        Err(CpdError::Field)
    );
    assert_eq!(
        decode_add(REQ, &add_reply(REQ, u64::from(u32::MAX), 1)),
        Ok(AddReply {
            monitor_id: u32::MAX,
            remaining_ms: 1,
        })
    );
    // A LIST entry with a zero id is refused too.
    assert_eq!(
        decode_list(REQ, &list_one(REQ, 0, 1)),
        Err(CpdError::ZeroId)
    );
    assert_eq!(
        decode_list(REQ, &list_one(REQ, 1 << 32, 1)),
        Err(CpdError::Field)
    );
}

#[test]
fn add_reply_bounds_and_reserved_fields() {
    assert_eq!(
        decode_add(REQ, &add_reply(REQ, 7, LEASE_MS + 1)),
        Err(CpdError::Field)
    );
    let mut reserved = add_reply(REQ, 7, 1);
    put_u32(&mut reserved, 44, 1);
    assert_eq!(decode_add(REQ, &reserved), Err(CpdError::Field));
}

#[test]
fn remove_reply_echo_state_and_reserved_fields() {
    assert_eq!(
        decode_remove(REQ, 7, &remove_reply(REQ, 8, MONITOR_RETIRED)),
        Err(CpdError::Echo)
    );
    for state in [0, 1, 3, u32::MAX] {
        assert_eq!(
            decode_remove(REQ, 7, &remove_reply(REQ, 7, state)),
            Err(CpdError::Field),
            "state {state}"
        );
    }
    let mut reserved = remove_reply(REQ, 7, MONITOR_RETIRED);
    put_u32(&mut reserved, 44, 1);
    assert_eq!(decode_remove(REQ, 7, &reserved), Err(CpdError::Field));
}

#[test]
fn list_count_capacity_and_empty_entry() {
    // Count 0 must leave bytes 48..96 zero: any nonzero byte is refused.
    for at in 48..96 {
        let mut reply = list_reply(REQ, 0, 1, 0);
        reply[at] = 1;
        assert_eq!(decode_list(REQ, &reply), Err(CpdError::Field), "byte {at}");
    }
    // Count is 0 or 1, and capacity is exactly 1.
    for count in [2, 3, u32::MAX] {
        assert_eq!(
            decode_list(REQ, &list_reply(REQ, count, 1, 0)),
            Err(CpdError::Field)
        );
    }
    for capacity in [0, 2] {
        assert_eq!(
            decode_list(REQ, &list_reply(REQ, 0, capacity, 0)),
            Err(CpdError::Field)
        );
        let mut one = list_one(REQ, 7, 0);
        put_u32(&mut one, 36, capacity);
        assert_eq!(decode_list(REQ, &one), Err(CpdError::Field));
    }
    // The LIST header's own reserved field (at 44) must be zero.
    let mut reserved = list_reply(REQ, 0, 1, 0);
    put_u32(&mut reserved, 44, 1);
    assert_eq!(decode_list(REQ, &reserved), Err(CpdError::Field));
}

#[test]
fn list_live_entry_state_and_reserved_fields() {
    for state in [0, 2, u32::MAX] {
        let mut reply = list_reply(REQ, 1, 1, 0);
        put_entry(&mut reply, 7, MODE_720, state);
        assert_eq!(
            decode_list(REQ, &reply),
            Err(CpdError::Field),
            "state {state}"
        );
    }
    let mut reserved = list_one(REQ, 7, 0);
    put_u32(&mut reserved, 92, 1);
    assert_eq!(decode_list(REQ, &reserved), Err(CpdError::Field));
}

#[test]
fn list_live_entry_mode_is_checked() {
    // Each corruption of the mode in the live entry is refused as `Mode`.
    let corruptions: &[(usize, u32)] = &[
        (56, 1024),  // width
        (60, 768),   // height
        (64, 30),    // refresh numerator
        (68, 2),     // refresh denominator
        (72, 9),     // width_mm below the minimum
        (76, 2_001), // height_mm above the maximum
        (80, 24),    // bits per pixel
        (84, 1),     // mode reserved
    ];
    for &(at, value) in corruptions {
        let mut reply = list_one(REQ, 7, 0);
        put_u32(&mut reply, at, value);
        assert_eq!(
            decode_list(REQ, &reply),
            Err(CpdError::Mode),
            "entry byte {at}"
        );
    }
}

#[test]
fn list_remaining_is_bounded() {
    assert_eq!(
        decode_list(REQ, &list_reply(REQ, 0, 1, LEASE_MS)).map(|r| r.remaining_ms),
        Ok(LEASE_MS)
    );
    assert_eq!(
        decode_list(REQ, &list_reply(REQ, 0, 1, LEASE_MS + 1)),
        Err(CpdError::Field)
    );
    assert_eq!(
        decode_list(REQ, &list_one(REQ, 7, LEASE_MS + 1)),
        Err(CpdError::Field)
    );
}

#[test]
fn heartbeat_echo_count_and_bounds() {
    assert_eq!(
        decode_heartbeat(REQ, SEQ, &heartbeat_reply(REQ, 1, 1, SEQ + 1)),
        Err(CpdError::Echo)
    );
    assert_eq!(
        decode_heartbeat(REQ, SEQ, &heartbeat_reply(REQ, 1, 2, SEQ)),
        Err(CpdError::Field)
    );
    assert_eq!(
        decode_heartbeat(REQ, SEQ, &heartbeat_reply(REQ, LEASE_MS + 1, 1, SEQ)),
        Err(CpdError::Field)
    );
}

#[test]
fn list_mode_round_trips_for_every_accepted_mode() {
    for (width, height) in [(1280, 720), (1920, 1080)] {
        for (width_mm, height_mm) in [(10, 10), (10, 2_000), (2_000, 10), (339, 191)] {
            let mode = CpdMode {
                width,
                height,
                width_mm,
                height_mm,
            };
            let mut reply = list_reply(REQ, 1, 1, 42);
            put_entry(&mut reply, 3, mode, MONITOR_ACTIVE);
            assert_eq!(
                decode_list(REQ, &reply).map(|r| r.monitor),
                Ok(Some((3, mode)))
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn decoders_never_panic_on_random_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..=128),
        request_id in any::<u64>(),
        sequence in any::<u64>(),
        monitor_id in any::<u32>(),
    ) {
        let _ = decode_add(request_id, &bytes);
        let _ = decode_remove(request_id, monitor_id, &bytes);
        let _ = decode_list(request_id, &bytes);
        let _ = decode_heartbeat(request_id, sequence, &bytes);
    }

    #[test]
    fn decoders_never_panic_past_the_header(
        body in prop::collection::vec(any::<u8>(), 96),
        request_id in 1_u64..=u64::MAX,
        sequence in 1_u64..=u64::MAX,
        monitor_id in 1_u32..=u32::MAX,
    ) {
        // Overlay a valid header on random bytes, so the body checks run too.
        let mut add = [0_u8; 48];
        add.copy_from_slice(&body[..48]);
        header(&mut add, 48, request_id);
        let mut remove = [0_u8; 48];
        remove.copy_from_slice(&body[48..]);
        header(&mut remove, 48, request_id);
        let mut list = [0_u8; 96];
        list.copy_from_slice(&body);
        header(&mut list, 96, request_id);
        let _ = decode_add(request_id, &add);
        let _ = decode_remove(request_id, monitor_id, &remove);
        let _ = decode_heartbeat(request_id, sequence, &remove);

        if let Ok(reply) = decode_list(request_id, &list) {
            prop_assert!(reply.remaining_ms <= LEASE_MS);
            if let Some((id, mode)) = reply.monitor {
                prop_assert!(id != 0);
                prop_assert!(valid_mode(mode));
            }
        }
    }

    #[test]
    fn list_round_trip_for_random_live_monitors(
        monitor_id in 1_u32..=u32::MAX,
        remaining_ms in 0_u32..=LEASE_MS,
        wide in any::<bool>(),
        width_mm in 10_u32..=2_000,
        height_mm in 10_u32..=2_000,
        request_id in 1_u64..=u64::MAX,
    ) {
        let (width, height): (u32, u32) = if wide { (1920, 1080) } else { (1280, 720) };
        let mode = CpdMode { width, height, width_mm, height_mm };
        let mut reply = list_reply(request_id, 1, 1, remaining_ms);
        put_entry(&mut reply, u64::from(monitor_id), mode, MONITOR_ACTIVE);
        prop_assert_eq!(
            decode_list(request_id, &reply),
            Ok(ListReply { remaining_ms, monitor: Some((monitor_id, mode)) })
        );
    }
}
