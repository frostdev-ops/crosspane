//! Crosspane's shared egui theme, brand art and layout widget (WP-4.2).
//!
//! This library is rendering and geometry only. It is used by the settings app and the installer,
//! and it deliberately knows nothing about either of them:
//!
//! - No native or platform code, no `eframe`, and no agent, socket, tray or file access.
//! - No bundled or default fonts: callers configure fonts on the [`egui::Context`] before any text
//!   is laid out.
//! - No brand art of its own: callers embed the images and pass the bytes to [`art::Art::load`].
//! - No placement policy: the layout widget reports a pure [`layout::LayoutAction`] and the caller
//!   decides how (and whether) to send it anywhere.
//!
//! Modules: [`theme`] (the dark palette, style and small shape-based controls), [`art`] (bounded
//! PNG decoding with independent fallbacks and the cover-scaled backdrop), [`layout`] (millimetre
//! geometry, the local editor and the desk widget) and [`crossings`] (the visual contact spans
//! between displays of different machines).

pub mod art;
pub mod crossings;
pub mod layout;
pub mod theme;
