use std::collections::BTreeSet;

use crosspane_input::{Held, router::Router};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::NodeId;
use proptest::prelude::*;

const NODES: [NodeId; 3] = [NodeId([0; 32]), NodeId([1; 32]), NodeId([2; 32])];
const ITEMS: [Held; 11] = [
    Held::Key(HidUsage::keyboard(0x04)),
    Held::Key(HidUsage::keyboard(0x05)),
    Held::Key(HidUsage::keyboard(0x06)),
    Held::Key(HidUsage::keyboard(0x07)),
    Held::Key(HidUsage::keyboard(0x08)),
    Held::Key(HidUsage::keyboard(0x09)),
    Held::Key(HidUsage::keyboard(0x0a)),
    Held::Key(HidUsage::keyboard(0x0b)),
    Held::Button(MouseButton::PRIMARY),
    Held::Button(MouseButton::SECONDARY),
    Held::Button(MouseButton::TERTIARY),
];
const CASES: u32 = 10_000;

#[derive(Clone, Debug)]
enum Operation {
    Transition { item: usize, current: Option<usize> },
    ReleaseAll(usize),
    Switch(usize),
}

fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        8 => (0..ITEMS.len(), proptest::option::of(0..NODES.len()))
            .prop_map(|(item, current)| Operation::Transition { item, current }),
        1 => (0..NODES.len()).prop_map(Operation::ReleaseAll),
        1 => (0..NODES.len()).prop_map(Operation::Switch),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        // Do not write regression files outside this work package's write list.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn invariant_1(
        initial_current in 0..NODES.len(),
        operations in proptest::collection::vec(operation(), 0..=256),
    ) {
        let mut router = Router::new();
        let mut current = initial_current;
        let mut physical = [false; ITEMS.len()];
        let mut pressed: [BTreeSet<Held>; 3] = std::array::from_fn(|_| BTreeSet::new());
        let mut downs = [0_usize; 3];
        let mut ups = [0_usize; 3];
        let mut released = [0_usize; 3];

        for operation in operations {
            match operation {
                Operation::Transition { item, current: next } => {
                    if let Some(next) = next {
                        current = next;
                    }
                    // Alternate physical down/up for each item. A synthetic release does not
                    // change physical state, so its eventual physical up is still generated.
                    physical[item] = !physical[item];
                    let down = physical[item];
                    let item = ITEMS[item];
                    let expected = if down {
                        Some(current)
                    } else {
                        pressed.iter().position(|items| items.contains(&item))
                    };
                    let delivered = router.route(item, down, NODES[current]);
                    prop_assert_eq!(delivered, expected.map(|target| NODES[target]));

                    if let Some(target) = expected {
                        if down {
                            prop_assert!(pressed.iter().all(|items| !items.contains(&item)));
                            prop_assert!(pressed[target].insert(item));
                            downs[target] += 1;
                        } else {
                            prop_assert!(pressed[target].remove(&item));
                            ups[target] += 1;
                        }
                    }
                }
                Operation::ReleaseAll(target) => {
                    let items = router.release_all(NODES[target]);
                    prop_assert_eq!(&items, &pressed[target].iter().copied().collect::<Vec<_>>());
                    for item in items {
                        prop_assert!(pressed[target].remove(&item));
                        released[target] += 1;
                    }
                }
                Operation::Switch(next) => current = next,
            }

            for (target, items) in pressed.iter().enumerate() {
                prop_assert_eq!(router.held_on(NODES[target]), items.iter().copied().collect::<Vec<_>>());
                prop_assert_eq!(downs[target], ups[target] + released[target] + items.len());
            }
            let nodes: Vec<_> = NODES.iter().copied().zip(&pressed)
                .filter_map(|(node, items)| (!items.is_empty()).then_some(node)).collect();
            prop_assert_eq!(router.nodes_with_held(), nodes);
            prop_assert_eq!(router.no_buttons_held(), pressed.iter()
                .all(|items| items.iter().all(|item| !matches!(item, Held::Button(_)))));
        }

        for (target, items) in pressed.iter_mut().enumerate() {
            let releases = router.release_all(NODES[target]);
            prop_assert_eq!(&releases, &items.iter().copied().collect::<Vec<_>>());
            for item in releases {
                prop_assert!(items.remove(&item));
                released[target] += 1;
            }
            prop_assert!(items.is_empty());
            prop_assert!(router.held_on(NODES[target]).is_empty());
            prop_assert_eq!(downs[target], ups[target] + released[target]);
        }
        prop_assert!(router.nodes_with_held().is_empty());
        prop_assert!(router.no_buttons_held());
    }
}

#[test]
fn key_up_returns_to_original_node_after_switch() {
    let mut router = Router::new();
    let key = ITEMS[0];
    assert_eq!(router.route(key, false, NODES[0]), None);
    assert_eq!(router.route(key, true, NODES[0]), Some(NODES[0]));
    assert_eq!(router.route(key, false, NODES[1]), Some(NODES[0]));
    assert_eq!(router.route(key, false, NODES[1]), None);
    assert!(router.nodes_with_held().is_empty());
}

#[test]
fn up_after_release_all_is_dropped() {
    let mut router = Router::new();
    let key = ITEMS[0];
    let button = Held::Button(MouseButton::PRIMARY);
    assert_eq!(router.route(button, true, NODES[0]), Some(NODES[0]));
    assert_eq!(router.route(key, true, NODES[0]), Some(NODES[0]));
    assert_eq!(router.release_all(NODES[0]), vec![key, button]);
    assert!(router.release_all(NODES[0]).is_empty());
    assert_eq!(router.route(key, false, NODES[1]), None);
    assert_eq!(router.route(button, false, NODES[1]), None);
    assert!(router.no_buttons_held());
    assert!(router.nodes_with_held().is_empty());
    assert_eq!(router.route(key, true, NODES[1]), Some(NODES[1]));
    assert_eq!(router.route(key, false, NODES[2]), Some(NODES[1]));
}

#[test]
fn no_buttons_held_checks_every_node() {
    let mut router = Router::new();
    assert!(router.no_buttons_held());
    assert_eq!(router.route(ITEMS[0], true, NODES[0]), Some(NODES[0]));
    assert!(router.no_buttons_held());
    for node in NODES {
        let primary = Held::Button(MouseButton::PRIMARY);
        let secondary = Held::Button(MouseButton::SECONDARY);
        assert_eq!(router.route(primary, true, node), Some(node));
        assert_eq!(router.route(secondary, true, node), Some(node));
        assert!(!router.no_buttons_held());
        assert_eq!(router.route(primary, false, NODES[2]), Some(node));
        assert!(!router.no_buttons_held());
        let held = router.held_on(node);
        assert_eq!(router.release_all(node), held);
        assert!(router.no_buttons_held());
    }
}

#[test]
fn held_on_is_sorted_and_per_node() {
    let mut router = Router::default();
    let low_key = ITEMS[0];
    let high_key = ITEMS[7];
    let primary = Held::Button(MouseButton::PRIMARY);
    let secondary = Held::Button(MouseButton::SECONDARY);
    for item in [secondary, high_key, primary, low_key] {
        assert_eq!(router.route(item, true, NODES[1]), Some(NODES[1]));
    }
    assert_eq!(router.route(ITEMS[1], true, NODES[0]), Some(NODES[0]));
    assert_eq!(
        router.held_on(NODES[1]),
        vec![low_key, high_key, primary, secondary]
    );
    assert_eq!(router.held_on(NODES[0]), vec![ITEMS[1]]);
    assert!(router.held_on(NODES[2]).is_empty());
    assert_eq!(router.nodes_with_held(), vec![NODES[0], NODES[1]]);
    assert!(router.release_all(NODES[2]).is_empty());
    assert_eq!(
        router.release_all(NODES[1]),
        vec![low_key, high_key, primary, secondary]
    );
    assert_eq!(router.held_on(NODES[0]), vec![ITEMS[1]]);
    assert_eq!(router.nodes_with_held(), vec![NODES[0]]);
}

#[test]
fn repeated_down_is_dropped() {
    for item in [ITEMS[0], Held::Button(MouseButton::PRIMARY)] {
        let mut router = Router::new();
        assert_eq!(router.route(item, true, NODES[0]), Some(NODES[0]));
        assert_eq!(router.route(item, true, NODES[0]), None);
        assert_eq!(router.route(item, true, NODES[1]), None);
        assert_eq!(router.held_on(NODES[0]), vec![item]);
        assert!(router.held_on(NODES[1]).is_empty());
        assert_eq!(router.route(item, false, NODES[1]), Some(NODES[0]));
        assert_eq!(router.route(item, true, NODES[1]), Some(NODES[1]));
        assert_eq!(router.route(item, false, NODES[0]), Some(NODES[1]));
    }
}
