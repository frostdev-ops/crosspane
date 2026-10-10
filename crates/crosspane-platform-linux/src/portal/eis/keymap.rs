//! Lock-key state from an EIS keyboard's keymap and its `modifiers` events.

use std::os::fd::AsFd;

use crosspane_types::input::LockKeys;
use reis::ei::keyboard::KeymapType;
use reis::event::Keymap;
use xkbcommon::xkb;

/// The largest keymap taken from a compositor (real ones are tens of kilobytes).
const MAX_KEYMAP_BYTES: usize = 4 << 20;

/// Which bits of the locked-modifier mask are Caps Lock and Num Lock in a keyboard's keymap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LockMasks {
    caps: Option<u32>,
    num: Option<u32>,
}

impl LockMasks {
    /// From keymap text; `None` when it doesn't compile.
    pub(super) fn from_text(text: String) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )?;
        let mask = |name| {
            let index = keymap.mod_get_index(name);
            (index < 32).then(|| 1u32 << index)
        };
        Some(LockMasks {
            caps: mask(xkb::MOD_NAME_CAPS),
            num: mask(xkb::MOD_NAME_NUM),
        })
    }

    /// From the keymap fd of `ei_keyboard.keymap`. The fd is read with `pread`, so the position
    /// the compositor left it at doesn't matter.
    pub(super) fn from_keymap(keymap: &Keymap) -> Option<Self> {
        if keymap.type_ != KeymapType::Xkb {
            return None;
        }
        let size = usize::try_from(keymap.size).ok()?;
        if size == 0 || size > MAX_KEYMAP_BYTES {
            return None;
        }
        let mut bytes = vec![0u8; size];
        let mut filled = 0;
        while filled < size {
            let read = rustix::io::pread(
                keymap.fd.as_fd(),
                &mut bytes[filled..],
                u64::try_from(filled).ok()?,
            )
            .ok()?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        bytes.truncate(filled);
        // The size on the wire counts the terminating NUL.
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        Self::from_text(String::from_utf8(bytes).ok()?)
    }

    /// Caps and Num Lock in a `locked` modifier mask. A lock the keymap doesn't have stays `None`.
    pub(super) fn read(&self, locked: u32) -> LockKeys {
        LockKeys {
            caps_lock: self.caps.map(|mask| locked & mask != 0),
            num_lock: self.num.map(|mask| locked & mask != 0),
            scroll_lock: None,
        }
    }
}

#[cfg(test)]
pub(super) fn test_keymap_text() -> Option<String> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "evdev",
        "pc105",
        "us",
        "",
        None,
        xkb::COMPILE_NO_FLAGS,
    )?;
    Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_num_come_from_the_locked_mask() {
        let Some(text) = test_keymap_text() else {
            eprintln!("skipped: the xkb data files are not installed");
            return;
        };
        let masks = LockMasks::from_text(text).unwrap();
        // Standard evdev: Lock is real modifier 1, Num Lock is Mod2 (real modifier 4).
        assert_eq!(masks.caps, Some(1 << 1));
        assert_eq!(masks.num, Some(1 << 4));
        assert_eq!(
            masks.read(0),
            LockKeys {
                caps_lock: Some(false),
                num_lock: Some(false),
                scroll_lock: None
            }
        );
        let caps = masks.read(1 << 1);
        assert_eq!((caps.caps_lock, caps.num_lock), (Some(true), Some(false)));
        let both = masks.read((1 << 1) | (1 << 4));
        assert_eq!((both.caps_lock, both.num_lock), (Some(true), Some(true)));
        // Depressed-only bits elsewhere in the mask don't read as a lock.
        let other = masks.read(1 << 0);
        assert_eq!(
            (other.caps_lock, other.num_lock),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn garbage_keymap_text_is_not_a_keymap() {
        assert_eq!(LockMasks::from_text("not a keymap".into()), None);
    }
}
