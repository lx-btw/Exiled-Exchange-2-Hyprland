//! Hotkey listening (`hotkeys.rs`), key injection (`inject.rs`), and the
//! shortcut-string ↔ evdev-keycode table (`keymap.rs`) they share.
//!
//! Stage 2 (see PLAN.md).

pub mod hotkeys;
pub mod inject;
pub mod keymap;
