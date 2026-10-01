#![allow(clippy::unwrap_used, clippy::expect_used)]

use aws_lc_rs::digest::{SHA256, digest};
use crosspane_security::identity::{
    DeviceIdentity, IdentityError, SPKI_LEN, node_id, point_from_spki, spki_from_point, verify,
};

const PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

#[test]
fn generated_spki_has_frozen_format() {
    let identity = DeviceIdentity::generate().unwrap();
    assert_eq!(identity.spki().len(), SPKI_LEN);
    assert_eq!(&identity.spki()[..26], &PREFIX);
    let point = point_from_spki(identity.spki()).unwrap();
    assert_eq!(point.len(), 65);
    assert_eq!(point[0], 0x04);
    assert_eq!(spki_from_point(point).unwrap(), identity.spki());
}

#[test]
fn node_is_sha256_of_spki() {
    let identity = DeviceIdentity::generate().unwrap();
    assert_eq!(
        identity.node().0.as_slice(),
        digest(&SHA256, identity.spki()).as_ref()
    );
    assert_eq!(node_id(identity.spki()), identity.node());
}

#[test]
fn sign_verify_round_trip() {
    let identity = DeviceIdentity::generate().unwrap();
    for msg in [b"crosspane signature test".as_slice(), b"".as_slice()] {
        let signature = identity.sign(msg).unwrap();
        assert_eq!(signature[0], 0x30);
        assert!(verify(identity.spki(), msg, &signature));
    }
}

#[test]
fn verify_rejects_wrong_keys_messages_signatures_and_spkis() {
    let identity = DeviceIdentity::generate().unwrap();
    let other = DeviceIdentity::generate().unwrap();
    let msg = b"crosspane identity test";
    let signature = identity.sign(msg).unwrap();
    assert!(!verify(other.spki(), msg, &signature));
    let mut flipped = msg.to_vec();
    flipped[0] ^= 1;
    assert!(!verify(identity.spki(), &flipped, &signature));
    assert!(!verify(
        identity.spki(),
        msg,
        &signature[..signature.len() - 1]
    ));
    assert!(!verify(identity.spki(), msg, &[0xff; 72]));
    assert!(!verify(identity.spki(), msg, &[]));

    let mut wrong_prefix = identity.spki().to_vec();
    wrong_prefix[22] ^= 1;
    let mut too_long = identity.spki().to_vec();
    too_long.push(0);
    let mut off_curve = PREFIX.to_vec();
    off_curve.extend_from_slice(&[0x04]);
    off_curve.extend_from_slice(&[0; 64]);
    let mut wrong_point_format = identity.spki().to_vec();
    wrong_point_format[26] = 0x02;
    for spki in [
        wrong_prefix,
        identity.spki()[..SPKI_LEN - 1].to_vec(),
        too_long,
        off_curve,
        wrong_point_format,
        vec![],
    ] {
        assert!(!verify(&spki, msg, &signature));
        assert_eq!(point_from_spki(&spki), Err(IdentityError::InvalidSpki));
    }
    for point in [
        vec![],
        vec![0; 65],
        [vec![4], vec![0; 64]].concat(),
        vec![4; 64],
    ] {
        assert_eq!(spki_from_point(&point), Err(IdentityError::InvalidSpki));
    }
}

#[test]
fn pkcs8_round_trip_keeps_identity() {
    let identity = DeviceIdentity::generate().unwrap();
    let loaded = DeviceIdentity::from_pkcs8(identity.pkcs8()).unwrap();
    assert_eq!(loaded.node(), identity.node());
    assert_eq!(loaded.spki(), identity.spki());
    assert_eq!(loaded.pkcs8(), identity.pkcs8());
    let msg = b"loaded identity";
    assert!(verify(identity.spki(), msg, &loaded.sign(msg).unwrap()));
    assert!(matches!(
        DeviceIdentity::from_pkcs8(&[]),
        Err(IdentityError::InvalidKey)
    ));
    assert!(matches!(
        DeviceIdentity::from_pkcs8(&[0xff; 138]),
        Err(IdentityError::InvalidKey)
    ));
}

#[test]
fn debug_prints_only_short_node_id() {
    let identity = DeviceIdentity::generate().unwrap();
    let debug = format!("{identity:?}");
    assert_eq!(
        debug,
        format!("DeviceIdentity({})", identity.node().short())
    );
    assert_eq!(format!("{identity:#?}"), debug);
    let private_hex: String = identity
        .pkcs8()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(!debug.contains(&private_hex));
    assert!(!debug.contains(&format!("{:?}", identity.pkcs8().as_slice())));
    assert!(!debug.contains(&format!("{:?}", identity.spki())));
}
