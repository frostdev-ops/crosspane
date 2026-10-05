use crosspane_platform::tray::*;
use crosspane_platform_windows::model::tray::*;

fn fixture() -> TrayMenu {
    TrayMenu {
        state: TrayState::Active,
        tooltip: "Line one\nLine two".into(),
        items: vec![
            TrayItem::Label("Status & details".into()),
            TrayItem::Action {
                id: TrayItemId(0),
                label: "Open".into(),
                enabled: true,
            },
            TrayItem::Toggle {
                id: TrayItemId(u32::MAX),
                label: "Checked".into(),
                enabled: true,
                checked: true,
            },
            TrayItem::Action {
                id: TrayItemId(7),
                label: "Disabled".into(),
                enabled: false,
            },
            TrayItem::Separator,
            TrayItem::Submenu {
                label: "Nested".into(),
                items: vec![
                    TrayItem::Toggle {
                        id: TrayItemId(8),
                        label: "Off".into(),
                        checked: false,
                        enabled: false,
                    },
                    TrayItem::Action {
                        id: TrayItemId(9),
                        label: "Child".into(),
                        enabled: true,
                    },
                ],
            },
            TrayItem::Submenu {
                label: "Empty".into(),
                items: vec![],
            },
        ],
    }
}

#[test]
fn tray_tree_flags_and_checkmarks_match_frozen_menu() {
    let original = fixture();
    let snapshot = MenuSnapshot::new(&original).unwrap();
    assert_eq!(snapshot.rows.len(), 7);
    assert!(!snapshot.rows[0].enabled);
    assert!(snapshot.rows[1].enabled && !snapshot.rows[1].checked);
    assert!(snapshot.rows[2].enabled && snapshot.rows[2].checked);
    assert!(!snapshot.rows[3].enabled);
    assert_eq!(snapshot.rows[4].kind, RowKind::Separator);
    let RowKind::Submenu(children) = &snapshot.rows[5].kind else {
        panic!("nested menu lost")
    };
    assert_eq!(children.len(), 2);
    assert!(!children[0].enabled && !children[0].checked);
    assert!(children[1].enabled);
    assert!(!snapshot.rows[6].enabled);
    assert_eq!(
        original,
        fixture(),
        "backend cannot toggle or mutate agent state"
    );
}

#[test]
fn tray_commands_preserve_zero_max_and_nested_agent_ids() {
    let snapshot = MenuSnapshot::new(&fixture()).unwrap();
    for (command, id) in [(1, 0), (2, u32::MAX), (5, 9)] {
        assert_eq!(
            snapshot.chosen(command),
            Some(TrayEvent::Chosen(TrayItemId(id)))
        );
    }
    for command in [0, 3, 4, 6, u32::MAX] {
        assert_eq!(snapshot.chosen(command), None);
    }
}

#[test]
fn tray_complete_replacement_keeps_an_already_open_old_snapshot() {
    let old = MenuSnapshot::new(&fixture()).unwrap();
    let new = MenuSnapshot::new(&TrayMenu::default()).unwrap();
    assert!(new.rows.is_empty());
    assert_eq!(new.chosen(1), None);
    assert_eq!(old.chosen(1), Some(TrayEvent::Chosen(TrayItemId(0))));
}

#[test]
fn tray_text_is_literal_and_never_truncates_at_embedded_nul() {
    let expected = "A && B�C D\0".encode_utf16().collect::<Vec<_>>();
    assert_eq!(menu_text("A & B\0C\tD"), expected);
}

#[test]
fn tray_tooltip_is_bounded_nul_terminated_without_split_surrogates() {
    let mut menu = fixture();
    assert_eq!(
        String::from_utf16(&MenuSnapshot::new(&menu).unwrap().tooltip[..17]).unwrap(),
        menu.tooltip
    );
    menu.tooltip = format!("{}😀end", "a".repeat(126));
    let tip = MenuSnapshot::new(&menu).unwrap().tooltip;
    assert!(tip[..126].iter().all(|c| *c == b'a' as u16));
    assert_eq!(&tip[126..], &[0, 0]);
}

#[test]
fn tray_first_set_adds_later_sets_modify_and_failed_add_retries() {
    let mut lifecycle = IconLifecycle::default();
    assert_eq!(lifecycle.taskbar_created(), None);
    assert_eq!(lifecycle.request(), Notify::Add);
    lifecycle.complete(Notify::Add, false);
    assert_eq!(lifecycle.request(), Notify::Add);
    lifecycle.complete(Notify::Add, true);
    assert_eq!(lifecycle.request(), Notify::Modify);
    lifecycle.complete(Notify::Modify, false);
    assert_eq!(lifecycle.request(), Notify::Add);
}

#[test]
fn tray_taskbar_created_readds_only_after_first_set() {
    let mut lifecycle = IconLifecycle::default();
    assert_eq!(lifecycle.taskbar_created(), None);
    lifecycle.request();
    lifecycle.complete(Notify::Add, true);
    assert_eq!(lifecycle.taskbar_created(), Some(Notify::Add));
    assert_eq!(lifecycle.request(), Notify::Add);
}

#[test]
fn tray_version4_callback_validates_owner_and_signed_coordinates() {
    let position = (0xfff0usize << 16) | 0xffe0;
    for event in [0x400, 0x401] {
        assert_eq!(
            popup_request(position, (1 << 16) | event, 1),
            Some((-32, -16))
        );
        assert_eq!(popup_request(position, (2 << 16) | event, 1), None);
    }
    assert_eq!(popup_request(position, (1 << 16) | 0x7b, 1), Some((-1, -1)));
    assert_eq!(popup_request(position, (1 << 16) | 0x205, 1), None);
}

#[test]
fn tray_brand_preserved_with_distinct_premultiplied_state_badges() {
    let original = vec![0x55; 32 * 32 * 4];
    let mut all = Vec::new();
    for state in [
        TrayState::Idle,
        TrayState::Active,
        TrayState::Attention,
        TrayState::Offline,
    ] {
        let mut pixels = original.clone();
        badge(state, &mut pixels).unwrap();
        if state == TrayState::Idle {
            assert!(pixels == original, "idle must preserve brand art");
        } else {
            assert!(pixels != original, "state must be visible");
        }
        assert!(all.iter().all(|previous| previous != &pixels));
        all.push(pixels);
    }
    assert!(badge(TrayState::Idle, &mut [0; 3]).is_err());
}
