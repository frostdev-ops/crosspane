#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use crosspane_protocol::msg::Capability;
use crosspane_security::identity::{DeviceIdentity, node_id};
use crosspane_security::trust::{PeerEntry, Revoked, TrustError, TrustStore, default_grants};
use crosspane_types::id::NodeId;
use proptest::prelude::*;

const CAPABILITIES: [Capability; 4] = [
    Capability::InputAccept,
    Capability::WindowShare,
    Capability::WindowBrowse,
    Capability::WindowPresent,
];

fn entry(identity: &DeviceIdentity) -> PeerEntry {
    PeerEntry {
        node: identity.node(),
        spki: identity.spki().to_vec(),
        name: "Test peer".to_owned(),
        granted: default_grants(),
        paired_at_ms: 42,
    }
}

#[test]
fn pin_rejects_mismatched_nodes_and_bad_spkis() {
    let identity = DeviceIdentity::generate().unwrap();
    let peer = entry(&identity);
    let mut store = TrustStore::new();
    store.pin(peer.clone()).unwrap();
    let before = store.clone();
    let mut mismatched = peer.clone();
    mismatched.node.0[0] ^= 1;
    assert_eq!(store.pin(mismatched), Err(TrustError::NodeMismatch));

    let mut wrong_prefix = peer.spki.clone();
    wrong_prefix[0] ^= 1;
    let mut off_curve = peer.spki.clone();
    off_curve[27..].fill(0);
    for spki in [wrong_prefix, peer.spki[..90].to_vec(), off_curve, vec![]] {
        let mut bad = peer.clone();
        bad.node = node_id(&spki);
        bad.spki = spki;
        assert_eq!(store.pin(bad), Err(TrustError::InvalidSpki));
        assert_eq!(store, before);
    }
}

#[test]
fn trusted_only_for_pinned_non_revoked_keys() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    assert_eq!(store.trusted(c.spki()), None);
    assert_eq!(store.trusted(&[]), None);
    store.pin(entry(&a)).unwrap();
    let peer = entry(&c);
    store.pin(peer.clone()).unwrap();
    assert_eq!(store.trusted(c.spki()), Some(c.node()));
    assert_eq!(store.trusted(a.spki()), Some(a.node()));
    assert_eq!(store.trusted(b.spki()), None);
    let mut changed = c.spki().to_vec();
    changed[90] ^= 1;
    assert_eq!(store.trusted(&changed), None);

    assert_eq!(store.forget(c.node()), Some(peer.clone()));
    assert_eq!(store.forget(c.node()), None);
    assert_eq!(store.trusted(c.spki()), None);
    store.pin(peer).unwrap();
    let notice = TrustStore::issue_revocation(&a, c.node(), 100).unwrap();
    store.apply_revocation(&notice, b.node()).unwrap();
    assert_eq!(store.trusted(c.spki()), None);
    assert!(store.is_revoked(c.node()));
    assert_eq!(store.trusted(a.spki()), Some(a.node()));
}

#[test]
fn permissions_use_local_defaults_and_follow_set_grant() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    assert_eq!(
        default_grants(),
        BTreeSet::from([Capability::WindowShare, Capability::WindowPresent])
    );
    for capability in CAPABILITIES {
        assert!(!store.allows(c.node(), capability));
        assert_eq!(
            store.set_grant(c.node(), capability, true),
            Err(TrustError::UnknownPeer)
        );
    }
    store.pin(entry(&a)).unwrap();
    store.pin(entry(&c)).unwrap();
    for capability in CAPABILITIES {
        let default = default_grants().contains(&capability);
        assert_eq!(store.allows(c.node(), capability), default);
        store.set_grant(c.node(), capability, !default).unwrap();
        assert_eq!(store.allows(c.node(), capability), !default);
        // Granting C a capability never changes the independently pinned A.
        assert_eq!(store.allows(a.node(), capability), default);
        store.set_grant(c.node(), capability, true).unwrap();
    }
    let notice = TrustStore::issue_revocation(&a, c.node(), 100).unwrap();
    store.apply_revocation(&notice, b.node()).unwrap();
    for capability in CAPABILITIES {
        assert!(!store.allows(c.node(), capability));
        assert_eq!(
            store.set_grant(c.node(), capability, true),
            Err(TrustError::UnknownPeer)
        );
    }
}

#[test]
fn signed_revocation_forgets_c_and_is_duplicate_on_repeat() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&a)).unwrap();
    let c_entry = entry(&c);
    store.pin(c_entry.clone()).unwrap();
    let notice = TrustStore::issue_revocation(&a, c.node(), 1234).unwrap();
    assert_eq!(notice.issuer, a.node());
    assert_eq!(notice.revoked, c.node());
    assert_eq!(notice.issued_at_ms, 1234);
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Ok(Revoked::Applied {
            forgotten: Some(c_entry)
        })
    );
    assert_eq!(store.get(c.node()), None);
    assert!(store.is_revoked(c.node()));
    let applied = store.clone();
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Ok(Revoked::Duplicate)
    );
    assert_eq!(store, applied);

    let unknown = NodeId([0xff; 32]);
    let notice = TrustStore::issue_revocation(&a, unknown, 1235).unwrap();
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Ok(Revoked::Applied { forgotten: None })
    );
    assert!(store.is_revoked(unknown));
}

#[test]
fn unpinned_or_revoked_issuer_is_rejected() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&c)).unwrap();
    let before = store.clone();
    let notice = TrustStore::issue_revocation(&a, c.node(), 100).unwrap();
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Err(TrustError::UntrustedIssuer)
    );
    assert_eq!(store, before);
    store.pin(entry(&a)).unwrap();
    let revoke_a = TrustStore::issue_revocation(&c, a.node(), 101).unwrap();
    store.apply_revocation(&revoke_a, b.node()).unwrap();
    let before = store.clone();
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Err(TrustError::UntrustedIssuer)
    );
    assert_eq!(store, before);
}

#[test]
fn tampered_signed_fields_or_signature_are_rejected_without_mutation() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    for identity in [&a, &b, &c] {
        store.pin(entry(identity)).unwrap();
    }
    let notice = TrustStore::issue_revocation(&a, c.node(), 100).unwrap();
    let mut wrong_revoked = notice.clone();
    wrong_revoked.revoked = b.node();
    let mut wrong_issuer = notice.clone();
    wrong_issuer.issuer = c.node();
    let mut wrong_time = notice.clone();
    wrong_time.issued_at_ms ^= 1;
    let mut wrong_signature = notice.clone();
    wrong_signature.signature[0] ^= 1;
    let before = store.clone();
    for tampered in [wrong_revoked, wrong_issuer, wrong_time, wrong_signature] {
        assert_eq!(
            store.apply_revocation(&tampered, b.node()),
            Err(TrustError::BadSignature)
        );
        assert_eq!(store, before);
    }
    store.apply_revocation(&notice, b.node()).unwrap();
    let mut tampered_duplicate = notice;
    tampered_duplicate.issued_at_ms ^= 1;
    let before = store.clone();
    assert_eq!(
        store.apply_revocation(&tampered_duplicate, b.node()),
        Err(TrustError::BadSignature)
    );
    assert_eq!(store, before);
}

#[test]
fn revoking_self_is_ignored_after_authentication() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&a)).unwrap();
    store.pin(entry(&b)).unwrap();
    let notice = TrustStore::issue_revocation(&a, b.node(), 100).unwrap();
    let before = store.clone();
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Ok(Revoked::IgnoredSelf)
    );
    assert_eq!(store, before);
    assert!(!store.is_revoked(b.node()));
    assert_eq!(
        store.apply_revocation(&notice, b.node()),
        Ok(Revoked::IgnoredSelf)
    );
    let mut tampered = notice;
    tampered.signature.clear();
    assert_eq!(
        store.apply_revocation(&tampered, b.node()),
        Err(TrustError::BadSignature)
    );
    assert_eq!(store, before);
}

#[test]
fn fresh_pin_clears_revocation_and_replaces_entry() {
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&a)).unwrap();
    store.pin(entry(&c)).unwrap();
    let notice = TrustStore::issue_revocation(&a, c.node(), 100).unwrap();
    store.apply_revocation(&notice, b.node()).unwrap();
    assert!(store.is_revoked(c.node()));
    let mut replacement = entry(&c);
    replacement.name = "Fresh pairing".to_owned();
    replacement.paired_at_ms = 200;
    replacement.granted = BTreeSet::from([Capability::InputAccept]);
    store.pin(replacement.clone()).unwrap();
    assert!(!store.is_revoked(c.node()));
    assert_eq!(store.trusted(c.spki()), Some(c.node()));
    assert_eq!(store.get(c.node()), Some(&replacement));
    assert!(store.allows(c.node(), Capability::InputAccept));
    assert!(!store.allows(c.node(), Capability::WindowShare));
    replacement.name = "Updated pin".to_owned();
    store.pin(replacement.clone()).unwrap();
    assert_eq!(store.get(c.node()), Some(&replacement));
    assert_eq!(store.peers().len(), 2);
}

proptest! {
    #[test]
    fn json_round_trip(
        peers in prop::collection::vec((any::<String>(), 0u8..16, any::<u64>(), any::<bool>()), 0..6),
        revoked in prop::collection::vec(any::<[u8; 32]>(), 0..6),
    ) {
        let a = DeviceIdentity::generate().unwrap();
        let own = DeviceIdentity::generate().unwrap();
        let mut store = TrustStore::new();
        store.pin(entry(&a)).unwrap();
        for (name, mask, paired_at_ms, revoke) in peers {
            let identity = DeviceIdentity::generate().unwrap();
            let granted = CAPABILITIES.iter().enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, capability)| *capability)
                .collect();
            store.pin(PeerEntry {
                node: identity.node(),
                spki: identity.spki().to_vec(),
                name,
                granted,
                paired_at_ms,
            }).unwrap();
            if revoke {
                let notice = TrustStore::issue_revocation(&a, identity.node(), paired_at_ms).unwrap();
                store.apply_revocation(&notice, own.node()).unwrap();
            }
        }
        for node in revoked {
            let notice = TrustStore::issue_revocation(&a, NodeId(node), u64::MAX).unwrap();
            store.apply_revocation(&notice, own.node()).unwrap();
        }
        let json = store.to_json();
        let loaded = TrustStore::from_json(&json).unwrap();
        prop_assert_eq!(&loaded, &store);
        prop_assert_eq!(loaded.to_json(), json.clone());
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        for peer in json["peers"].as_array().unwrap() {
            let spki = peer["spki"].as_str().unwrap();
            prop_assert_eq!(spki.len(), 182);
            prop_assert!(spki.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        }
    }
}

#[test]
fn corrupt_json_is_rejected() {
    let identity = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&identity)).unwrap();
    let valid: serde_json::Value = serde_json::from_str(&store.to_json()).unwrap();
    let mut wrong_node = valid.clone();
    wrong_node["peers"][0]["node"] = NodeId([0; 32]).to_string().into();
    let mut wrong_prefix = valid.clone();
    let hex = valid["peers"][0]["spki"].as_str().unwrap();
    wrong_prefix["peers"][0]["spki"] = format!("00{}", &hex[2..]).into();
    let mut off_curve = valid.clone();
    off_curve["peers"][0]["spki"] = format!("{}{}", &hex[..54], "00".repeat(64)).into();
    let mut missing = valid.clone();
    missing["peers"][0].as_object_mut().unwrap().remove("node");
    let mut bad_grant = valid.clone();
    bad_grant["peers"][0]["granted"] = serde_json::json!(["UnknownCapability"]);
    let mut overlap = valid.clone();
    overlap["revoked"] = serde_json::json!([identity.node()]);
    let mut duplicate = valid.clone();
    duplicate["peers"]
        .as_array_mut()
        .unwrap()
        .push(valid["peers"][0].clone());
    for bad in [
        wrong_node,
        wrong_prefix,
        off_curve,
        missing,
        bad_grant,
        overlap,
        duplicate,
    ] {
        assert_eq!(
            TrustStore::from_json(&bad.to_string()),
            Err(TrustError::Corrupt)
        );
    }
    for bad_spki in ["0", "zz", &hex[..180]] {
        let mut bad = valid.clone();
        bad["peers"][0]["spki"] = bad_spki.into();
        assert_eq!(
            TrustStore::from_json(&bad.to_string()),
            Err(TrustError::Corrupt)
        );
    }
    for json in [
        "",
        "{",
        "null",
        "[]",
        "{}",
        r#"{"peers":[],"revoked":["bad node"]}"#,
    ] {
        assert_eq!(TrustStore::from_json(json), Err(TrustError::Corrupt));
    }
    let empty = TrustStore::new();
    assert_eq!(TrustStore::from_json(&empty.to_json()), Ok(empty));
}

#[test]
fn local_revoke_forgets_and_refuses_until_pinned_again() {
    let c = DeviceIdentity::generate().unwrap();
    let mut store = TrustStore::new();
    store.pin(entry(&c)).unwrap();
    assert!(store.revoke(c.node()).is_some());
    assert!(store.is_revoked(c.node()));
    assert_eq!(store.trusted(c.spki()), None);
    assert!(!store.allows(c.node(), Capability::WindowShare));
    // Revoking again (or an unknown node) returns nothing and stays revoked.
    assert!(store.revoke(c.node()).is_none());
    assert!(store.is_revoked(c.node()));
    // It survives persistence.
    let reloaded = TrustStore::from_json(&store.to_json()).unwrap();
    assert!(reloaded.is_revoked(c.node()));
    // A fresh pairing pins it again.
    store.pin(entry(&c)).unwrap();
    assert!(!store.is_revoked(c.node()));
    assert_eq!(store.trusted(c.spki()), Some(c.node()));
}

#[test]
fn a_notice_older_than_the_current_pairing_is_stale() {
    let (a, b, c) = (
        DeviceIdentity::generate().unwrap(),
        DeviceIdentity::generate().unwrap(),
        DeviceIdentity::generate().unwrap(),
    );
    let mut store = TrustStore::new();
    store.pin(entry(&a)).unwrap();
    // `entry` pairs at 42 ms: a notice from 41 ms predates it, one from 43 ms doesn't.
    store.pin(entry(&c)).unwrap();
    let old = TrustStore::issue_revocation(&a, c.node(), 41).unwrap();
    assert!(matches!(
        store.apply_revocation(&old, b.node()),
        Ok(Revoked::Stale)
    ));
    assert_eq!(store.trusted(c.spki()), Some(c.node()));
    let new = TrustStore::issue_revocation(&a, c.node(), 43).unwrap();
    assert!(matches!(
        store.apply_revocation(&new, b.node()),
        Ok(Revoked::Applied { forgotten: Some(_) })
    ));
    assert_eq!(store.trusted(c.spki()), None);
}
