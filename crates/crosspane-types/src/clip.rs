//! Clipboard kinds shared by the protocol and platform trait (CLIP-v0 §4/§5).

/// Clipboard content kind. Wire codes are 1 for text and 2 for image; 0 is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Image,
}
