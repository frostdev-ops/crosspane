//! Notification-area presentation and command mapping without Windows types or calls.

use crosspane_platform::{
    PlatformError,
    tray::{TrayEvent, TrayItem, TrayItemId, TrayMenu, TrayState},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Item,
    Separator,
    Submenu(Vec<Row>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub text: Vec<u16>,
    pub command: u32,
    pub enabled: bool,
    pub checked: bool,
    pub kind: RowKind,
}

/// Commands are snapshot-local ordinals, never truncated agent ids. Zero means cancellation.
#[derive(Clone, Debug)]
pub struct MenuSnapshot {
    pub rows: Vec<Row>,
    pub tooltip: [u16; 128],
    choices: Vec<(TrayItemId, bool)>,
}

impl MenuSnapshot {
    pub fn new(menu: &TrayMenu) -> Result<Self, PlatformError> {
        let mut choices = Vec::new();
        let rows = convert(&menu.items, &mut choices)?;
        let mut tooltip = [0; 128];
        let mut offset = 0;
        for c in menu.tooltip.chars() {
            let c = if c == '\0' { '�' } else { c };
            if offset + c.len_utf16() >= tooltip.len() {
                break;
            }
            let mut units = [0; 2];
            let units = c.encode_utf16(&mut units);
            tooltip[offset..offset + units.len()].copy_from_slice(units);
            offset += units.len();
        }
        Ok(Self {
            rows,
            tooltip,
            choices,
        })
    }
    pub fn chosen(&self, command: u32) -> Option<TrayEvent> {
        let (id, enabled) = self.choices.get(command.checked_sub(1)? as usize)?;
        enabled.then_some(TrayEvent::Chosen(*id))
    }
}

fn convert(
    items: &[TrayItem],
    choices: &mut Vec<(TrayItemId, bool)>,
) -> Result<Vec<Row>, PlatformError> {
    items
        .iter()
        .map(|item| {
            let (label, enabled, checked, kind, id) = match item {
                TrayItem::Label(text) => (text.as_str(), false, false, RowKind::Item, None),
                TrayItem::Separator => ("", false, false, RowKind::Separator, None),
                TrayItem::Action { id, label, enabled } => {
                    (label.as_str(), *enabled, false, RowKind::Item, Some(*id))
                }
                TrayItem::Toggle {
                    id,
                    label,
                    enabled,
                    checked,
                } => (label.as_str(), *enabled, *checked, RowKind::Item, Some(*id)),
                TrayItem::Submenu { label, items } => (
                    label.as_str(),
                    !items.is_empty(),
                    false,
                    RowKind::Submenu(convert(items, choices)?),
                    None,
                ),
            };
            let command = if let Some(id) = id {
                let count = choices
                    .len()
                    .checked_add(1)
                    .and_then(|count| u32::try_from(count).ok())
                    .ok_or_else(|| PlatformError::Backend("too many tray commands".into()))?;
                choices.push((id, enabled));
                count
            } else {
                0
            };
            Ok(Row {
                text: menu_text(label),
                command,
                enabled,
                checked,
                kind,
            })
        })
        .collect()
}

/// Native menus treat ampersands as mnemonics; agent labels are literal text.
pub fn menu_text(text: &str) -> Vec<u16> {
    text.replace('&', "&&")
        .replace('\0', "�")
        .replace('\t', " ")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    Add,
    Modify,
}

#[derive(Debug, Default)]
pub struct IconLifecycle {
    requested: bool,
    installed: bool,
}

impl IconLifecycle {
    pub fn request(&mut self) -> Notify {
        self.requested = true;
        if self.installed {
            Notify::Modify
        } else {
            Notify::Add
        }
    }
    pub fn complete(&mut self, _operation: Notify, success: bool) {
        self.installed = success;
    }
    pub fn taskbar_created(&mut self) -> Option<Notify> {
        self.installed = false;
        self.requested.then_some(Notify::Add)
    }
}

/// Version-4 callbacks pack the icon id in the high word and the event in the low word.
/// WM_CONTEXTMENU has no defined wParam coordinates; (-1,-1) requests native cursor fallback.
pub fn popup_request(position: usize, notification: isize, icon: u16) -> Option<(i32, i32)> {
    let packed = notification as usize;
    if (packed >> 16) & 0xffff != icon as usize || !matches!(packed & 0xffff, 0x7b | 0x400 | 0x401)
    {
        return None;
    }
    if packed & 0xffff == 0x7b {
        return Some((-1, -1));
    }
    Some((
        position as u16 as i16 as i32,
        (position >> 16) as u16 as i16 as i32,
    ))
}

/// Preserve the embedded brand image and add a state badge; input is premultiplied BGRA.
pub fn badge(state: TrayState, pixels: &mut [u8]) -> Result<(), PlatformError> {
    if pixels.len() != 32 * 32 * 4 {
        return Err(PlatformError::Backend("invalid tray image".into()));
    }
    let color = match state {
        TrayState::Idle => return Ok(()),
        TrayState::Active => [0xf6, 0x82, 0x3b, 0xff],
        TrayState::Attention => [0x0b, 0x9e, 0xf5, 0xff],
        TrayState::Offline => [0x63, 0x55, 0x4b, 0xff],
    };
    if state == TrayState::Offline {
        for byte in pixels.iter_mut() {
            *byte /= 2;
        }
    }
    for y in 1i32..10 {
        for x in 23i32..32 {
            if (x - 27).pow(2) + (y - 5).pow(2) <= 16 {
                let offset = (y as usize * 32 + x as usize) * 4;
                pixels[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    Ok(())
}
