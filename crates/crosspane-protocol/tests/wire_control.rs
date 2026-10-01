use crosspane_protocol::msg::{
    Capability, ControlMessage, EndReason, Hello, Placement, Refusal, RevocationNotice,
};
use crosspane_protocol::negotiate::{Negotiated, negotiate};
use crosspane_protocol::wire::{
    Frame, HEADER_LEN, KIND_CONTROL, KIND_KEY, WIRE_VERSION, WireError, decode_control,
    encode_control,
};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeMm,
};
use crosspane_types::id::{DisplayId, NodeId, SessionId};
use crosspane_types::input::LockKeys;
use proptest::prelude::*;

// Exercise the control codec without WP-1.1's currently stubbed FrameDecoder.
fn encoded_frame(message: &ControlMessage) -> Frame {
    let mut bytes = vec![0xca, 0xfe];
    assert_eq!(encode_control(message, &mut bytes), Ok(()));
    assert_eq!(&bytes[..2], &[0xca, 0xfe]);
    let bytes = &bytes[2..];
    assert_eq!(&bytes[..4], &[WIRE_VERSION, KIND_CONTROL, 0, 0]);
    let payload = bytes[HEADER_LEN..].to_vec();
    assert_eq!(&bytes[4..HEADER_LEN], &(payload.len() as u32).to_le_bytes());
    Frame {
        kind: KIND_CONTROL,
        payload,
    }
}

fn round_trip(message: ControlMessage) {
    assert_eq!(decode_control(&encoded_frame(&message)), Ok(message));
}

fn assert_bad_encode(message: ControlMessage) {
    let mut bytes = vec![0xca, 0xfe];
    assert!(matches!(
        encode_control(&message, &mut bytes),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(bytes, [0xca, 0xfe]);
}

fn assert_bad_payload(payload: Vec<u8>) {
    assert!(matches!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload,
        }),
        Err(WireError::BadValue(_))
    ));
}

// Independent protobuf fixtures allow invalid wire values without exposing the private pb types.
fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 0x80 {
        bytes.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn number(tag: u32, value: u64) -> Vec<u8> {
    [varint(u64::from(tag) << 3), varint(value)].concat()
}

fn bytes_field(tag: u32, bytes: &[u8]) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 2),
        varint(bytes.len() as u64),
        bytes.to_vec(),
    ]
    .concat()
}

fn double(tag: u32, value: f64) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 1),
        value.to_le_bytes().to_vec(),
    ]
    .concat()
}

fn body(tag: u32, bytes: &[u8]) -> Vec<u8> {
    bytes_field(tag, bytes)
}

fn sample_display() -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(3),
        name: "Panel".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(600.0, 340.0),
            pixel_size: PixelSize::new(3840, 2160),
            scale: 2.0,
            logical_origin: PointLogical::new(-1920.0, 0.0),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}

fn display_bytes(display: &DisplayInfo) -> Vec<u8> {
    let g = display.geometry;
    [
        number(1, u64::from(display.id.0)),
        bytes_field(2, display.name.as_bytes()),
        double(3, g.physical_size.width),
        double(4, g.physical_size.height),
        number(5, u64::from(g.pixel_size.width)),
        number(6, u64::from(g.pixel_size.height)),
        double(7, g.scale),
        double(8, g.logical_origin.x),
        double(9, g.logical_origin.y),
        number(10, u64::from(display.refresh_millihz)),
    ]
    .concat()
}

fn display_body(display: &DisplayInfo) -> Vec<u8> {
    body(2, &bytes_field(1, &display_bytes(display)))
}

fn sample_placement() -> Placement {
    Placement {
        node: NodeId([7; 32]),
        display: DisplayId(3),
        origin: PointMm::new(1.0, 2.0),
        version: 9,
    }
}

fn placement_bytes(node: &[u8], x: f64, y: f64) -> Vec<u8> {
    [bytes_field(1, node), double(3, x), double(4, y)].concat()
}

fn revocation_bytes(revoked: &[u8], issuer: &[u8], signature: &[u8]) -> Vec<u8> {
    [
        bytes_field(1, revoked),
        bytes_field(2, issuer),
        bytes_field(4, signature),
    ]
    .concat()
}

#[test]
fn golden_ping() {
    let mut bytes = Vec::new();
    assert_eq!(
        encode_control(&ControlMessage::Ping { t0: 1 }, &mut bytes),
        Ok(())
    );
    assert_eq!(bytes, [0x01, 0x40, 0, 0, 4, 0, 0, 0, 0x52, 2, 8, 1]);
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: vec![0x52, 2, 8, 1]
        }),
        Ok(ControlMessage::Ping { t0: 1 })
    );
}

#[test]
fn golden_goodbye() {
    let message = ControlMessage::Goodbye {
        message: "bye".into(),
    };
    let mut bytes = Vec::new();
    assert_eq!(encode_control(&message, &mut bytes), Ok(()));
    assert_eq!(
        bytes,
        [
            0x01, 0x40, 0, 0, 7, 0, 0, 0, 0x62, 5, 0x0a, 3, 0x62, 0x79, 0x65
        ]
    );
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: vec![0x62, 5, 0x0a, 3, 0x62, 0x79, 0x65]
        }),
        Ok(message)
    );
}

#[test]
fn unknown_or_absent_body() {
    for payload in [vec![0x9a, 0x06, 0], Vec::new()] {
        assert_eq!(
            decode_control(&Frame {
                kind: KIND_CONTROL,
                payload
            }),
            Err(WireError::UnknownControl)
        );
    }
}

fn text(max_chars: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..=max_chars)
        .prop_map(|chars| chars.into_iter().collect())
}

fn finite() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("finite", |value| value.is_finite())
}

fn positive() -> impl Strategy<Value = f64> {
    finite().prop_filter("positive", |value| *value > 0.0)
}

fn display() -> impl Strategy<Value = DisplayInfo> {
    (
        any::<u32>(),
        text(64),
        positive(),
        positive(),
        1..=u32::MAX,
        1..=u32::MAX,
        positive(),
        finite(),
        finite(),
        any::<u32>(),
        prop::sample::select(vec![
            ColorSpace::Srgb,
            ColorSpace::DisplayP3,
            ColorSpace::Bt709,
        ]),
        any::<bool>(),
    )
        .prop_map(
            |(id, name, w, h, pw, ph, scale, x, y, refresh, color_space, hdr)| DisplayInfo {
                id: DisplayId(id),
                name,
                geometry: DisplayGeometry {
                    physical_size: SizeMm::new(w, h),
                    pixel_size: PixelSize::new(pw, ph),
                    scale,
                    logical_origin: PointLogical::new(x, y),
                },
                refresh_millihz: refresh,
                color_space,
                hdr,
            },
        )
}

fn placement() -> impl Strategy<Value = Placement> {
    (
        any::<[u8; 32]>(),
        any::<u32>(),
        finite(),
        finite(),
        any::<u64>(),
    )
        .prop_map(|(node, display, x, y, version)| Placement {
            node: NodeId(node),
            display: DisplayId(display),
            origin: PointMm::new(x, y),
            version,
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn round_trip_hello(
        minor in any::<u32>(), name in text(64),
        features in prop::collection::vec(text(16), 0..=64),
        displays in prop::collection::vec(display(), 0..=16),
    ) {
        round_trip(ControlMessage::Hello(Hello { minor, name, features, displays }));
    }

    #[test]
    fn round_trip_displays(displays in prop::collection::vec(display(), 0..=16)) {
        round_trip(ControlMessage::Displays(displays));
    }

    #[test]
    fn round_trip_layout(placements in prop::collection::vec(placement(), 0..=64)) {
        round_trip(ControlMessage::Layout(placements));
    }

    #[test]
    fn round_trip_start_control(
        session in any::<u64>(), display in any::<u32>(), x in finite(), y in finite(),
        caps_lock in prop::option::of(any::<bool>()), num_lock in prop::option::of(any::<bool>()),
        scroll_lock in prop::option::of(any::<bool>()),
    ) {
        round_trip(ControlMessage::StartControl {
            session: SessionId(session), entry_display: DisplayId(display), entry: PointDevice::new(x, y),
            lock_keys: LockKeys { caps_lock, num_lock, scroll_lock },
        });
    }

    #[test]
    fn round_trip_control_started(session in any::<u64>()) {
        round_trip(ControlMessage::ControlStarted { session: SessionId(session) });
    }

    #[test]
    fn round_trip_control_refused(
        session in any::<u64>(),
        reason in prop::sample::select(vec![Refusal::Permission, Refusal::Locked, Refusal::SecureInput, Refusal::Busy, Refusal::InjectorFailed]),
    ) {
        round_trip(ControlMessage::ControlRefused { session: SessionId(session), reason });
    }

    #[test]
    fn round_trip_end_control(
        session in any::<u64>(),
        reason in prop::sample::select(vec![EndReason::Released, EndReason::Panic, EndReason::TargetLocked, EndReason::ControllerLocked, EndReason::LinkLost, EndReason::Revoked]),
    ) {
        round_trip(ControlMessage::EndControl { session: SessionId(session), reason });
    }

    #[test]
    fn round_trip_grants(
        capabilities in prop::collection::vec(prop::sample::select(vec![Capability::InputAccept, Capability::WindowShare, Capability::WindowBrowse, Capability::WindowPresent]), 0..=16),
    ) {
        round_trip(ControlMessage::Grants(capabilities));
    }

    #[test]
    fn round_trip_revocation(
        revoked in any::<[u8; 32]>(), issuer in any::<[u8; 32]>(), issued_at_ms in any::<u64>(),
        signature in prop::collection::vec(any::<u8>(), 1..=80),
    ) {
        round_trip(ControlMessage::Revocation(RevocationNotice {
            revoked: NodeId(revoked), issuer: NodeId(issuer), issued_at_ms, signature,
        }));
    }

    #[test]
    fn round_trip_ping(t0 in any::<u64>()) {
        round_trip(ControlMessage::Ping { t0 });
    }

    #[test]
    fn round_trip_pong(t0 in any::<u64>(), t1 in any::<u64>(), t2 in any::<u64>()) {
        round_trip(ControlMessage::Pong { t0, t1, t2 });
    }

    #[test]
    fn round_trip_goodbye(message in text(64)) {
        round_trip(ControlMessage::Goodbye { message });
    }
}

#[test]
fn rejects_malformed_protobuf() {
    for payload in [
        vec![0x52, 2, 8],
        vec![0x52, 1, 0x80],
        body(12, &[0x0a, 1, 0xff]),
    ] {
        assert_eq!(
            decode_control(&Frame {
                kind: KIND_CONTROL,
                payload
            }),
            Err(WireError::BadControl)
        );
    }
}

#[test]
fn rejects_wrong_node_length() {
    for len in [0, 31, 33] {
        let wrong = vec![7; len];
        assert_bad_payload(body(3, &bytes_field(1, &placement_bytes(&wrong, 0.0, 0.0))));
        assert_bad_payload(body(9, &revocation_bytes(&wrong, &[7; 32], &[1])));
        assert_bad_payload(body(9, &revocation_bytes(&[7; 32], &wrong, &[1])));
    }
}

#[test]
fn rejects_wrong_signature_length() {
    for len in [0, 81] {
        let signature = vec![1; len];
        assert_bad_payload(body(9, &revocation_bytes(&[7; 32], &[8; 32], &signature)));
        assert_bad_encode(ControlMessage::Revocation(RevocationNotice {
            revoked: NodeId([7; 32]),
            issuer: NodeId([8; 32]),
            issued_at_ms: 0,
            signature,
        }));
    }
}

#[test]
fn rejects_non_finite_double() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for field in [3, 4, 7, 8, 9] {
            let mut raw = display_bytes(&sample_display());
            raw.extend(double(field, value));
            assert_bad_payload(body(2, &bytes_field(1, &raw)));
            let mut display = sample_display();
            match field {
                3 => display.geometry.physical_size.width = value,
                4 => display.geometry.physical_size.height = value,
                7 => display.geometry.scale = value,
                8 => display.geometry.logical_origin.x = value,
                9 => display.geometry.logical_origin.y = value,
                _ => unreachable!(),
            }
            assert_bad_encode(ControlMessage::Displays(vec![display]));
        }
        for (x, y) in [(value, 0.0), (0.0, value)] {
            assert_bad_payload(body(3, &bytes_field(1, &placement_bytes(&[7; 32], x, y))));
            let mut placement = sample_placement();
            placement.origin = PointMm::new(x, y);
            assert_bad_encode(ControlMessage::Layout(vec![placement]));
            assert_bad_payload(body(4, &[double(3, x), double(4, y)].concat()));
            assert_bad_encode(ControlMessage::StartControl {
                session: SessionId(0),
                entry_display: DisplayId(0),
                entry: PointDevice::new(x, y),
                lock_keys: LockKeys::default(),
            });
        }
    }
}

#[test]
fn rejects_unspecified_or_unknown_enums() {
    for value in [0, 99, u64::MAX] {
        assert_bad_payload(body(6, &number(2, value)));
        assert_bad_payload(body(7, &number(2, value)));
        // Both packed and unpacked repeated enum fields are valid protobuf.
        assert_bad_payload(body(8, &bytes_field(1, &varint(value))));
        assert_bad_payload(body(8, &number(1, value)));
    }
    for value in [3, 99, u64::MAX] {
        let mut raw = display_bytes(&sample_display());
        raw.extend(number(11, value));
        assert_bad_payload(body(2, &bytes_field(1, &raw)));
        for field in 1..=3 {
            assert_bad_payload(body(4, &bytes_field(5, &number(field, value))));
        }
    }
}

#[test]
fn absent_lock_keys_means_unknown() {
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: body(4, &[])
        }),
        Ok(ControlMessage::StartControl {
            session: SessionId(0),
            entry_display: DisplayId(0),
            entry: PointDevice::new(0.0, 0.0),
            lock_keys: LockKeys::default(),
        })
    );
}

#[test]
fn rejects_invalid_display_geometry() {
    for field in [3, 4, 5, 6, 7] {
        let mut display = sample_display();
        match field {
            3 => display.geometry.physical_size.width = 0.0,
            4 => display.geometry.physical_size.height = 0.0,
            5 => display.geometry.pixel_size.width = 0,
            6 => display.geometry.pixel_size.height = 0,
            7 => display.geometry.scale = 0.0,
            _ => unreachable!(),
        }
        assert_bad_payload(display_body(&display));
        assert_bad_payload(body(1, &bytes_field(4, &display_bytes(&display))));
        assert_bad_encode(ControlMessage::Displays(vec![display.clone()]));
        assert_bad_encode(ControlMessage::Hello(Hello {
            minor: 0,
            name: String::new(),
            features: Vec::new(),
            displays: vec![display],
        }));
    }
    for field in [3, 4, 7] {
        let mut raw = display_bytes(&sample_display());
        raw.extend(double(field, -1.0));
        assert_bad_payload(body(2, &bytes_field(1, &raw)));
    }
}

#[test]
fn rejects_over_cap_strings() {
    // 129 Unicode characters are 258 bytes, so character count would be insufficient.
    let long = "é".repeat(129);
    assert_bad_payload(body(12, &bytes_field(1, long.as_bytes())));
    assert_bad_encode(ControlMessage::Goodbye {
        message: long.clone(),
    });
    assert_bad_payload(body(1, &bytes_field(2, long.as_bytes())));
    assert_bad_encode(ControlMessage::Hello(Hello {
        minor: 0,
        name: long.clone(),
        features: Vec::new(),
        displays: Vec::new(),
    }));
    let mut display = sample_display();
    display.name = long;
    assert_bad_payload(display_body(&display));
    assert_bad_encode(ControlMessage::Displays(vec![display]));
    let feature = "é".repeat(33);
    assert_bad_payload(body(1, &bytes_field(3, feature.as_bytes())));
    assert_bad_encode(ControlMessage::Hello(Hello {
        minor: 0,
        name: String::new(),
        features: vec![feature],
        displays: Vec::new(),
    }));

    // Inclusive byte limits must remain accepted.
    round_trip(ControlMessage::Goodbye {
        message: "é".repeat(128),
    });
    let mut display = sample_display();
    display.name = "é".repeat(128);
    round_trip(ControlMessage::Hello(Hello {
        minor: 0,
        name: "é".repeat(128),
        features: vec!["é".repeat(32); 64],
        displays: vec![display; 16],
    }));
}

#[test]
fn rejects_over_cap_lists() {
    assert_bad_payload(body(1, &bytes_field(3, b"e1").repeat(65)));
    assert_bad_encode(ControlMessage::Hello(Hello {
        minor: 0,
        name: String::new(),
        features: vec!["e1".into(); 65],
        displays: Vec::new(),
    }));
    let displays = bytes_field(1, &display_bytes(&sample_display())).repeat(17);
    assert_bad_payload(body(2, &displays));
    assert_bad_payload(body(
        1,
        &bytes_field(4, &display_bytes(&sample_display())).repeat(17),
    ));
    assert_bad_encode(ControlMessage::Displays(vec![sample_display(); 17]));
    assert_bad_encode(ControlMessage::Hello(Hello {
        minor: 0,
        name: String::new(),
        features: Vec::new(),
        displays: vec![sample_display(); 17],
    }));
    assert_bad_payload(body(
        3,
        &bytes_field(1, &placement_bytes(&[7; 32], 0.0, 0.0)).repeat(65),
    ));
    assert_bad_encode(ControlMessage::Layout(vec![sample_placement(); 65]));
    assert_bad_payload(body(8, &bytes_field(1, &[1; 17])));
    assert_bad_encode(ControlMessage::Grants(vec![Capability::InputAccept; 17]));
    round_trip(ControlMessage::Layout(vec![sample_placement(); 64]));
    round_trip(ControlMessage::Grants(vec![Capability::InputAccept; 16]));
}

#[test]
fn rejects_wrong_kind() {
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_KEY,
            payload: vec![0x52, 2, 8, 1]
        }),
        Err(WireError::BadKind(KIND_KEY))
    );
}

fn hello(minor: u32, features: &[&str]) -> Hello {
    Hello {
        minor,
        name: String::new(),
        features: features.iter().map(|feature| (*feature).into()).collect(),
        displays: Vec::new(),
    }
}

#[test]
fn negotiate_min_version() {
    for (local, remote, expected) in [
        (0, 3, 0),
        (7, 3, 3),
        (3, 7, 3),
        (u32::MAX, u32::MAX, u32::MAX),
    ] {
        assert_eq!(negotiate(local, &[], &hello(remote, &[])).minor, expected);
    }
}

#[test]
fn negotiate_sorted_deduplicated_intersection() {
    let local = hello(5, &["z", "e1", "a", "z", "local"]);
    let remote = hello(7, &["z", "a", "z", "e1", "remote"]);
    assert_eq!(
        negotiate(local.minor, &local.features, &remote),
        Negotiated {
            minor: 5,
            features: vec!["a".into(), "e1".into(), "z".into()],
        }
    );
}

#[test]
fn negotiate_empty_intersection() {
    let local = hello(0, &["e1"]);
    for remote in [hello(0, &["other"]), hello(0, &[])] {
        assert!(negotiate(0, &local.features, &remote).features.is_empty());
    }
    assert!(negotiate(0, &[], &local).features.is_empty());
}

#[test]
fn negotiate_ignores_unknown_remote_feature() {
    let local = hello(0, &["e1"]);
    assert_eq!(
        negotiate(0, &local.features, &hello(0, &["future-feature", "e1"])).features,
        vec!["e1".to_string()]
    );
}
