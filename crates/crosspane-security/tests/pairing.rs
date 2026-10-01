// A broken fixture or protocol assertion should fail the test immediately.
#![allow(clippy::unwrap_used)]

use aws_lc_rs::{digest, hmac};
use crosspane_protocol::msg::Capability;
use crosspane_security::pairing::{
    AbortReason, Event, Initiator, Joiner, Local, PairingError, PairingMsg, Peer, Sas, commitment,
    derive_sas,
};
use crosspane_security::rng::Rng;
use crosspane_types::id::NodeId;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

const EXPORTER: [u8; 32] = [0x5a; 32];
const NI: [u8; 32] = [0x11; 32];
const NJ: [u8; 32] = [0x22; 32];
const CAPS: [Capability; 4] = [
    Capability::InputAccept,
    Capability::WindowShare,
    Capability::WindowBrowse,
    Capability::WindowPresent,
];
const REASONS: [AbortReason; 5] = [
    AbortReason::Protocol,
    AbortReason::CommitmentMismatch,
    AbortReason::CodeMismatch,
    AbortReason::UserRejected,
    AbortReason::Expired,
];

/// SplitMix64 is deterministic test data, never a production source of randomness.
struct TestRng(u64);

impl Rng for TestRng {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut word = self.0;
            word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            word ^= word >> 31;
            chunk.copy_from_slice(&word.to_be_bytes()[..chunk.len()]);
        }
    }
}

struct NonceRng {
    nonce: Option<[u8; 32]>,
    rest: TestRng,
}

impl NonceRng {
    fn new(nonce: [u8; 32], seed: u64) -> Self {
        Self {
            nonce: Some(nonce),
            rest: TestRng(seed),
        }
    }
}

impl Rng for NonceRng {
    fn fill(&mut self, buf: &mut [u8]) {
        if let Some(nonce) = self.nonce.take() {
            assert_eq!(buf.len(), 32);
            buf.copy_from_slice(&nonce);
        } else {
            self.rest.fill(buf);
        }
    }
}

fn spki(tag: u8) -> Vec<u8> {
    let mut key = vec![tag; 91];
    key[0] = 0x30;
    key
}

fn local(tag: u8) -> Local {
    Local {
        spki: spki(tag),
        name: format!("node {tag}"),
        grants: if tag == 1 {
            CAPS[..2].to_vec()
        } else {
            CAPS[2..].to_vec()
        },
    }
}

fn node(spki: &[u8]) -> NodeId {
    NodeId(
        digest::digest(&digest::SHA256, spki)
            .as_ref()
            .try_into()
            .unwrap(),
    )
}

fn peer(local: &Local) -> Peer {
    Peer {
        node: node(&local.spki),
        spki: local.spki.clone(),
        name: local.name.clone(),
        grants_to_us: local.grants.clone(),
    }
}

fn sent(events: &[Event]) -> PairingMsg {
    match &events[0] {
        Event::Send(msg) => {
            // Exercise the payload codec on the simulated transport as well.
            let payload = msg.encode();
            let decoded = PairingMsg::decode(&payload).unwrap();
            assert_eq!(&decoded, msg);
            decoded
        }
        event => panic!("expected Send, got {event:?}"),
    }
}

fn shown(events: &[Event]) -> Sas {
    match events {
        [Event::Send(PairingMsg::Reveal { .. }), Event::ShowSas(sas)] => *sas,
        _ => panic!("expected reveal followed by SAS, got {events:?}"),
    }
}

fn offered(events: &[Event]) -> [Sas; 3] {
    match events {
        [Event::ShowCandidates(candidates)] => *candidates,
        _ => panic!("expected candidates, got {events:?}"),
    }
}

fn failure(reason: AbortReason, error: PairingError) -> Vec<Event> {
    vec![Event::Send(PairingMsg::Abort(reason)), Event::Failed(error)]
}

fn confirmed(tag: u8) -> PairingMsg {
    let local = local(tag);
    PairingMsg::Confirmed {
        name: local.name,
        grants: local.grants,
    }
}

/// States 0..=3 are the four active states; 4 has paired successfully.
fn initiator_at(stage: usize) -> Initiator {
    let (mut i, opening) = Initiator::start(local(1), EXPORTER, &mut NonceRng::new(NI, 1));
    assert_eq!(
        opening,
        vec![Event::Send(PairingMsg::Commit(commitment(&NI, &spki(1))))]
    );
    if stage >= 1 {
        assert_eq!(
            shown(&i.on_message(PairingMsg::Reveal {
                nonce: NJ,
                spki: spki(2)
            })),
            true_sas()
        );
    }
    if stage >= 2 {
        assert_eq!(i.on_message(PairingMsg::Matched), vec![Event::AskConfirm]);
    }
    if stage >= 3 {
        assert_eq!(i.user_confirmed(true), vec![Event::Send(confirmed(1))]);
    }
    if stage >= 4 {
        assert_eq!(
            i.on_message(confirmed(2)),
            vec![Event::Paired(peer(&local(2)))]
        );
    }
    i
}

fn joiner_at(stage: usize) -> Joiner {
    let (mut j, opening) = Joiner::start(local(2), EXPORTER, &mut NonceRng::new(NJ, 2));
    assert!(opening.is_empty());
    if stage >= 1 {
        assert_eq!(
            j.on_message(PairingMsg::Commit(commitment(&NI, &spki(1)))),
            vec![Event::Send(PairingMsg::Reveal {
                nonce: NJ,
                spki: spki(2)
            })]
        );
    }
    if stage >= 2 {
        assert!(
            offered(&j.on_message(PairingMsg::Reveal {
                nonce: NI,
                spki: spki(1)
            }))
            .contains(&true_sas())
        );
    }
    if stage >= 3 {
        assert_eq!(
            j.user_picked(true_sas()),
            vec![Event::Send(PairingMsg::Matched)]
        );
    }
    if stage >= 4 {
        assert_eq!(
            j.on_message(confirmed(1)),
            vec![Event::Send(confirmed(2)), Event::Paired(peer(&local(1)))]
        );
    }
    j
}

fn true_sas() -> Sas {
    derive_sas(&EXPORTER, &spki(1), &spki(2), &NI, &NJ)
}

fn all_messages() -> [PairingMsg; 5] {
    [
        PairingMsg::Commit(commitment(&NI, &spki(1))),
        PairingMsg::Reveal {
            nonce: NI,
            spki: spki(1),
        },
        PairingMsg::Matched,
        confirmed(1),
        PairingMsg::Abort(AbortReason::Expired),
    ]
}

#[test]
fn published_vectors() {
    let ni = core::array::from_fn(|i| i as u8);
    let nj = core::array::from_fn(|i| 0x20 + i as u8);
    let ki = [vec![0x30], (0..90).map(|i| 0xa0 + i % 16).collect()].concat();
    let kj = [vec![0x30], (0..90).map(|i| 0xb0 + i % 16).collect()].concat();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(
        hex(&commitment(&ni, &ki)),
        "86592563a8289fd4eb930a5a596065139ad3bd2678b1349aa7f9ace955427911"
    );

    let mut transcript = b"crosspane pairing sas v1\0".to_vec();
    for key in [&ki, &kj] {
        transcript.extend_from_slice(&(key.len() as u16).to_be_bytes());
        transcript.extend_from_slice(key);
    }
    transcript.extend_from_slice(&ni);
    transcript.extend_from_slice(&nj);
    let mac = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &EXPORTER), &transcript);
    assert_eq!(
        hex(mac.as_ref()),
        "72eac8a5fc44b71b4260fc5a54a864372fdbb043e029c7648314a663e52e2e38"
    );
    assert_eq!(derive_sas(&EXPORTER, &ki, &kj, &ni, &nj), Sas(599003));
    assert_eq!(derive_sas(&EXPORTER, &kj, &ki, &nj, &ni), Sas(177026));
    let mut changed = EXPORTER;
    changed[0] = 0x5b;
    assert_eq!(derive_sas(&changed, &ki, &kj, &ni, &nj), Sas(271009));
    assert_eq!(Sas(599003).to_string(), "599003");
    assert_eq!(Sas(7).to_string(), "000007");
    assert_eq!(Sas(0).to_string(), "000000");
    assert_eq!(Sas(999999).to_string(), "999999");
}

#[test]
fn happy_path() {
    let li = local(1);
    let lj = local(2);
    let (mut i, commit) = Initiator::start(li.clone(), EXPORTER, &mut TestRng(11));
    let (mut j, opening) = Joiner::start(lj.clone(), EXPORTER, &mut TestRng(22));
    assert!(opening.is_empty());
    let reveal_j = j.on_message(sent(&commit));
    let reveal_i = i.on_message(sent(&reveal_j));
    let sas = shown(&reveal_i);
    let candidates = offered(&j.on_message(sent(&reveal_i)));
    assert_eq!(candidates.iter().filter(|&&code| code == sas).count(), 1);
    let matched = j.user_picked(sas);
    assert_eq!(matched, vec![Event::Send(PairingMsg::Matched)]);
    assert_eq!(i.on_message(sent(&matched)), vec![Event::AskConfirm]);
    let confirm_i = i.user_confirmed(true);
    assert_eq!(confirm_i, vec![Event::Send(confirmed(1))]);
    let confirm_j = j.on_message(sent(&confirm_i));
    assert_eq!(
        confirm_j,
        vec![Event::Send(confirmed(2)), Event::Paired(peer(&li))]
    );
    assert_eq!(
        i.on_message(sent(&confirm_j)),
        vec![Event::Paired(peer(&lj))]
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10_000,
        // A six-digit SAS can legitimately collide. Use a reproducible random sample rather
        // than making CI flaky (10,000 fresh trials have about a 1% collision probability).
        // This seed is fixed in advance; collisions must be reported, not filtered out.
        rng_seed: RngSeed::Fixed(0x4352_4f53_5350_414e),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn mitm_separate_exchanges(
        nonces in any::<[[u8; 32]; 4]>(),
        keys in any::<[[u8; 32]; 4]>(),
        exporters in any::<[[u8; 32]; 2]>(),
        same_exporter in any::<bool>(),
        attacker_starts_with_joiner in any::<bool>(),
        decoy_seed in any::<u64>(),
    ) {
        // The attacker acts as J toward honest I, and as I toward honest J. Its four keys
        // are distinct; varying their random suffixes, nonces, connection exporters and
        // scheduling exercises two valid exchanges, not an unauthenticated message splice.
        // The committed nonce/key cannot change after the honest joiner's reveal. That
        // commitment is what prevents an adaptive nonce search. Finite random cases only
        // check examples of that construction; they are not a proof against all strategies
        // and do not imply that a genuine 1-in-1,000,000 collision is impossible.
        let identities: Vec<Local> = keys.iter().enumerate().map(|(i, key)| {
            let mut spki = vec![0x30, i as u8];
            spki.extend_from_slice(key);
            Local { spki, name: format!("node {i}"), grants: Vec::new() }
        }).collect();
        let ei = exporters[0];
        let ej = if same_exporter { ei } else { exporters[1] };
        let (mut i, ci) = Initiator::start(identities[0].clone(), ei,
            &mut NonceRng::new(nonces[0], 0));
        let (mut j, opening) = Joiner::start(identities[1].clone(), ej,
            &mut NonceRng::new(nonces[1], decoy_seed));
        prop_assert!(opening.is_empty());
        let (mut attacker_i, ca) = Initiator::start(identities[2].clone(), ej,
            &mut NonceRng::new(nonces[2], 0));
        let (mut attacker_j, opening) = Joiner::start(identities[3].clone(), ei,
            &mut NonceRng::new(nonces[3], decoy_seed.wrapping_add(1)));
        prop_assert!(opening.is_empty());

        let mut rj = None;
        if attacker_starts_with_joiner {
            rj = Some(j.on_message(sent(&ca)));
        }
        let ra_j = attacker_j.on_message(sent(&ci));
        let ri = i.on_message(sent(&ra_j));
        let sas_i = shown(&ri);
        prop_assert!(offered(&attacker_j.on_message(sent(&ri))).contains(&sas_i));
        let rj = rj.unwrap_or_else(|| j.on_message(sent(&ca)));
        let ra_i = attacker_i.on_message(sent(&rj));
        let sas_j = shown(&ra_i);
        prop_assert!(offered(&j.on_message(sent(&ra_i))).contains(&sas_j));
        prop_assert_ne!(sas_i, sas_j);
    }
}

#[test]
fn commitment_mismatch() {
    for byte in 0..32 {
        let mut j = joiner_at(1);
        let mut changed = NI;
        changed[byte] ^= 1;
        assert_eq!(
            j.on_message(PairingMsg::Reveal {
                nonce: changed,
                spki: spki(1)
            }),
            failure(
                AbortReason::CommitmentMismatch,
                PairingError::CommitmentMismatch
            )
        );
    }
    let mut j = joiner_at(1);
    assert_eq!(
        j.on_message(PairingMsg::Reveal {
            nonce: NI,
            spki: spki(3)
        }),
        failure(
            AbortReason::CommitmentMismatch,
            PairingError::CommitmentMismatch
        )
    );
}

#[test]
fn wrong_pick() {
    let mut j = joiner_at(1);
    let candidates = offered(&j.on_message(PairingMsg::Reveal {
        nonce: NI,
        spki: spki(1),
    }));
    for wrong in candidates
        .into_iter()
        .filter(|&code| code != true_sas())
        .chain([Sas(1_000_000)])
    {
        let mut j = joiner_at(2);
        assert_eq!(
            j.user_picked(wrong),
            failure(AbortReason::CodeMismatch, PairingError::CodeMismatch)
        );
    }
}

#[test]
fn user_rejection() {
    let mut i = initiator_at(2);
    let mut j = joiner_at(3);
    let rejected = i.user_confirmed(false);
    assert_eq!(
        rejected,
        failure(AbortReason::UserRejected, PairingError::UserRejected)
    );
    assert_eq!(
        j.on_message(sent(&rejected)),
        vec![Event::Failed(PairingError::PeerAborted(
            AbortReason::UserRejected
        ))]
    );
}

#[test]
fn unexpected_messages_and_abort_in_every_active_state() {
    for stage in 0..4 {
        // Message indices: Commit, Reveal, Matched, Confirmed. Abort is legal everywhere.
        let expected_i = [Some(1), Some(2), None, Some(3)][stage];
        let expected_j = [Some(0), Some(1), None, Some(3)][stage];
        for (kind, msg) in all_messages()[..4].iter().enumerate() {
            if Some(kind) != expected_i {
                assert_eq!(
                    initiator_at(stage).on_message(msg.clone()),
                    failure(AbortReason::Protocol, PairingError::Unexpected),
                    "I state {stage}, {msg:?}"
                );
            }
            if Some(kind) != expected_j {
                assert_eq!(
                    joiner_at(stage).on_message(msg.clone()),
                    failure(AbortReason::Protocol, PairingError::Unexpected),
                    "J state {stage}, {msg:?}"
                );
            }
        }
        for reason in REASONS {
            let expected = vec![Event::Failed(PairingError::PeerAborted(reason))];
            assert_eq!(
                initiator_at(stage).on_message(PairingMsg::Abort(reason)),
                expected
            );
            assert_eq!(
                joiner_at(stage).on_message(PairingMsg::Abort(reason)),
                expected
            );
        }
        if stage != 2 {
            for accept in [false, true] {
                assert_eq!(
                    initiator_at(stage).user_confirmed(accept),
                    failure(AbortReason::Protocol, PairingError::Unexpected)
                );
            }
            assert_eq!(
                joiner_at(stage).user_picked(true_sas()),
                failure(AbortReason::Protocol, PairingError::Unexpected)
            );
        }
    }
    assert_eq!(
        initiator_at(0).on_message(PairingMsg::Reveal {
            nonce: NJ,
            spki: spki(1)
        }),
        failure(AbortReason::Protocol, PairingError::SameKey)
    );
    assert_eq!(
        joiner_at(1).on_message(PairingMsg::Reveal {
            nonce: NI,
            spki: spki(2)
        }),
        failure(AbortReason::Protocol, PairingError::SameKey)
    );
}

#[test]
fn terminal_states_ignore_every_call() {
    let mut initiators = vec![initiator_at(4)];
    let mut joiners = vec![joiner_at(4)];
    for stage in 0..4 {
        let mut i = initiator_at(stage);
        assert_eq!(
            i.on_message(PairingMsg::Commit([0; 32])),
            failure(AbortReason::Protocol, PairingError::Unexpected)
        );
        initiators.push(i);
        let mut j = joiner_at(stage);
        assert_eq!(
            j.on_message(PairingMsg::Matched),
            failure(AbortReason::Protocol, PairingError::Unexpected)
        );
        joiners.push(j);
    }
    for reason in REASONS {
        let mut i = initiator_at(0);
        i.on_message(PairingMsg::Abort(reason));
        initiators.push(i);
        let mut j = joiner_at(0);
        j.on_message(PairingMsg::Abort(reason));
        joiners.push(j);
    }
    let mut i = initiator_at(2);
    i.user_confirmed(false);
    initiators.push(i);
    let mut i = initiator_at(0);
    i.on_message(PairingMsg::Reveal {
        nonce: NJ,
        spki: spki(1),
    });
    initiators.push(i);
    let mut j = joiner_at(2);
    j.user_picked(Sas(1_000_000));
    joiners.push(j);
    let mut j = joiner_at(1);
    j.on_message(PairingMsg::Reveal {
        nonce: NJ,
        spki: spki(1),
    });
    joiners.push(j);
    let mut j = joiner_at(1);
    j.on_message(PairingMsg::Reveal {
        nonce: NI,
        spki: spki(2),
    });
    joiners.push(j);
    for i in &mut initiators {
        for msg in all_messages() {
            assert!(i.on_message(msg).is_empty());
        }
        assert!(i.user_confirmed(true).is_empty());
        assert!(i.user_confirmed(false).is_empty());
    }
    for j in &mut joiners {
        for msg in all_messages() {
            assert!(j.on_message(msg).is_empty());
        }
        assert!(j.user_picked(true_sas()).is_empty());
        assert!(j.user_picked(Sas(1_000_000)).is_empty());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn codec_round_trip_every_message(
        hash in any::<[u8; 32]>(),
        nonce in any::<[u8; 32]>(),
        spki in prop::collection::vec(any::<u8>(), 1..=128),
        name in prop::collection::vec(any::<char>(), 1..=16)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        grants in prop::sample::subsequence([CAPS.as_slice(), &[Capability::AudioSpeaker, Capability::AudioMic]].concat(), 0..=6),
        reason in prop::sample::select(REASONS.to_vec()),
    ) {
        for msg in [
            PairingMsg::Commit(hash), PairingMsg::Reveal { nonce, spki },
            PairingMsg::Matched, PairingMsg::Confirmed { name, grants }, PairingMsg::Abort(reason),
        ] {
            prop_assert_eq!(PairingMsg::decode(&msg.encode()), Ok(msg));
        }
    }
}

#[test]
fn codec_every_rejection_case() {
    let reject = |payload: &[u8]| {
        assert_eq!(
            PairingMsg::decode(payload),
            Err(PairingError::Malformed),
            "{payload:?}"
        );
    };
    reject(&[]);
    for kind in [0, 6, 255] {
        reject(&[kind]);
    }
    for msg in all_messages() {
        let payload = msg.encode();
        for len in 0..payload.len() {
            reject(&payload[..len]);
        }
        let mut trailing = payload;
        trailing.push(0);
        reject(&trailing);
    }
    for len in [0u16, 129, 256, u16::MAX] {
        let mut payload = vec![2];
        payload.extend_from_slice(&NI);
        payload.extend_from_slice(&len.to_be_bytes());
        payload.extend_from_slice(&vec![0; usize::from(len)]);
        reject(&payload);
    }
    for declared in [1u16, 90, 92, 128] {
        let mut payload = PairingMsg::Reveal {
            nonce: NI,
            spki: spki(1),
        }
        .encode();
        payload[33..35].copy_from_slice(&declared.to_be_bytes());
        reject(&payload);
    }
    reject(&[4, 0, 0]);
    let mut long_name = vec![4, 65];
    long_name.extend_from_slice(&[b'a'; 65]);
    long_name.push(0);
    reject(&long_name);
    for bad_utf8 in [vec![0xff], vec![0xc0, 0x80], vec![0xe2, 0x82]] {
        let mut payload = vec![4, bad_utf8.len() as u8];
        payload.extend_from_slice(&bad_utf8);
        payload.push(0);
        reject(&payload);
    }
    for code in [0, 7, 255] {
        reject(&[4, 1, b'a', 1, code]);
    }
    for code in 1..=6 {
        reject(&[4, 1, b'a', 2, code, code]);
    }
    for count in [9, 255] {
        let mut payload = vec![4, 1, b'a', count];
        payload.extend_from_slice(&vec![1; usize::from(count)]);
        reject(&payload);
    }
    for code in [0, 6, 255] {
        reject(&[5, code]);
    }
    for reason in REASONS {
        let msg = PairingMsg::Abort(reason);
        assert_eq!(PairingMsg::decode(&msg.encode()), Ok(msg));
    }
    for spki in [vec![0x30], vec![0x30; 128]] {
        let msg = PairingMsg::Reveal { nonce: NI, spki };
        assert_eq!(PairingMsg::decode(&msg.encode()), Ok(msg));
    }
    let msg = PairingMsg::Confirmed {
        name: "é".repeat(32),
        grants: CAPS.to_vec(),
    };
    assert_eq!(PairingMsg::decode(&msg.encode()), Ok(msg));
    assert_eq!(
        PairingMsg::Commit([7; 32]).encode(),
        [vec![1], vec![7; 32]].concat()
    );
    assert_eq!(PairingMsg::Matched.encode(), vec![3]);
    assert_eq!(
        PairingMsg::Confirmed {
            name: "é".into(),
            grants: CAPS.to_vec()
        }
        .encode(),
        vec![4, 2, 0xc3, 0xa9, 4, 1, 2, 3, 4]
    );
    assert_eq!(PairingMsg::Abort(AbortReason::Expired).encode(), vec![5, 5]);

    // The frozen infallible encoder must never return an invalid wire payload.
    for invalid in [
        PairingMsg::Reveal {
            nonce: NI,
            spki: Vec::new(),
        },
        PairingMsg::Reveal {
            nonce: NI,
            spki: vec![0; 129],
        },
        PairingMsg::Confirmed {
            name: String::new(),
            grants: Vec::new(),
        },
        PairingMsg::Confirmed {
            name: "é".repeat(33),
            grants: Vec::new(),
        },
        PairingMsg::Confirmed {
            name: "a".into(),
            grants: vec![CAPS[0]; 2],
        },
        PairingMsg::Confirmed {
            name: "a".into(),
            grants: vec![CAPS[0]; 9],
        },
    ] {
        assert!(std::panic::catch_unwind(|| invalid.encode()).is_err());
    }
}

#[test]
fn decoys_are_distinct_and_sas_position_is_uniform() {
    let mut positions = [0u32; 3];
    for seed in 0..3_000 {
        let (mut i, commit) = Initiator::start(local(1), EXPORTER, &mut TestRng(seed));
        let (mut j, opening) = Joiner::start(local(2), EXPORTER, &mut TestRng(seed + 3_000));
        assert!(opening.is_empty());
        let reveal_j = j.on_message(sent(&commit));
        let reveal_i = i.on_message(sent(&reveal_j));
        let sas = shown(&reveal_i);
        let candidates = offered(&j.on_message(sent(&reveal_i)));
        assert!(candidates.iter().all(|code| code.0 < 1_000_000));
        assert_ne!(candidates[0], candidates[1]);
        assert_ne!(candidates[0], candidates[2]);
        assert_ne!(candidates[1], candidates[2]);
        let position = candidates.iter().position(|&code| code == sas).unwrap();
        positions[position] += 1;
    }
    let chi_square: f64 = positions
        .iter()
        .map(|&count| (f64::from(count) - 1000.0).powi(2) / 1000.0)
        .sum();
    assert!(
        chi_square < 25.0,
        "positions {positions:?}, chi-square {chi_square}"
    );
}
