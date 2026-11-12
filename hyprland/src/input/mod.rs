//! Hotkey listening and keyd-style capture (`hotkeys.rs`, `grab.rs`), key
//! injection (`inject.rs`), and the shortcut-string ↔ evdev-keycode table
//! (`keymap.rs`) they share.

pub(crate) mod devices;
mod grab;
pub mod hotkeys;
pub mod inject;
pub mod keymap;
