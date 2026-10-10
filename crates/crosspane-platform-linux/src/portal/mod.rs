//! Compositor-neutral XDG desktop portal adapters (WP-G0.1), shared by the GNOME and KDE backends:
//! the RemoteDesktop session and its EIS socket, EIS key and pointer injection, and the
//! GlobalShortcuts release chord.
//!
//! Consent is caller-prepared: the agent starts a portal session at startup, off the input path.
//! The first start may show the desktop's consent dialog; the persisted restore token makes later
//! starts silent. Trait methods on the input path never wait for a dialog. Until a session is
//! active, the injectors are not live and the agent doesn't grant `InputAccept`.

pub mod eis;
pub mod session;
pub mod shortcuts;
