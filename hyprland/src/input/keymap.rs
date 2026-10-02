//! Shortcut-string (upstream's `"Ctrl + Alt + D"` format, from `config.json`
//! / the renderer-computed `HostConfig.shortcuts`) ↔ evdev keycode
//! (`linux/input-event-codes.h`) table, shared by `hotkeys.rs` (listening)
//! and `inject.rs` (injection).
//!
//! The string vocabulary mirrors upstream's `ipc/KeyToCode.ts` key names
//! (`Ctrl`, `Alt`, `Shift`, `A`..`Z`, `F1`..`F12`, `Space`, ...) — same names
//! the renderer's hotkey-recording UI produces via `eventToString()`
//! (`main/src/shortcuts/Shortcuts.ts`) — but maps them to evdev `KeyCode`
//! instead of upstream's Windows virtual-key codes, since we read/inject at
//! the evdev layer, not through Electron/uiohook.

use evdev::KeyCode;

/// The three modifiers upstream's shortcut strings use. Left/right physical
/// keys of the same kind are treated as interchangeable, matching how
/// `KeyboardEvent.ctrlKey`/`shiftKey`/`altKey` already merge them upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
}

impl Modifier {
    pub const ALL: [Modifier; 3] = [Modifier::Ctrl, Modifier::Shift, Modifier::Alt];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Ctrl" => Some(Self::Ctrl),
            "Shift" => Some(Self::Shift),
            "Alt" => Some(Self::Alt),
            _ => None,
        }
    }

    /// Both physical keycodes this modifier is satisfied by.
    pub fn keycodes(self) -> [KeyCode; 2] {
        match self {
            Self::Ctrl => [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL],
            Self::Shift => [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT],
            Self::Alt => [KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT],
        }
    }

    /// The left-hand physical key — what `inject.rs` synthesizes, matching
    /// `host.mjs::injectCopy()`'s use of `LEFTCTRL`/`LEFTALT`.
    pub fn left_keycode(self) -> KeyCode {
        self.keycodes()[0]
    }
}

/// Every non-modifier key name `key_from_name`/`base_keys` recognize, paired
/// with its evdev keycode — the single source of truth both draw from, so
/// they can't silently drift apart (a name accepted by `key_from_name` but
/// missing from the virtual keyboard's declared uinput capabilities would
/// parse fine and then fail to inject at runtime with no error). Covers what
/// upstream's default shortcuts and hotkey UI realistically produce —
/// letters, digits, function keys, and the common named keys — not the full
/// `KeyToCode.ts` table (Numpad/media keys are vanishingly unlikely as
/// user-configured hotkeys and can be added if that changes).
const KEY_TABLE: &[(&str, KeyCode)] = &[
    ("A", KeyCode::KEY_A),
    ("B", KeyCode::KEY_B),
    ("C", KeyCode::KEY_C),
    ("D", KeyCode::KEY_D),
    ("E", KeyCode::KEY_E),
    ("F", KeyCode::KEY_F),
    ("G", KeyCode::KEY_G),
    ("H", KeyCode::KEY_H),
    ("I", KeyCode::KEY_I),
    ("J", KeyCode::KEY_J),
    ("K", KeyCode::KEY_K),
    ("L", KeyCode::KEY_L),
    ("M", KeyCode::KEY_M),
    ("N", KeyCode::KEY_N),
    ("O", KeyCode::KEY_O),
    ("P", KeyCode::KEY_P),
    ("Q", KeyCode::KEY_Q),
    ("R", KeyCode::KEY_R),
    ("S", KeyCode::KEY_S),
    ("T", KeyCode::KEY_T),
    ("U", KeyCode::KEY_U),
    ("V", KeyCode::KEY_V),
    ("W", KeyCode::KEY_W),
    ("X", KeyCode::KEY_X),
    ("Y", KeyCode::KEY_Y),
    ("Z", KeyCode::KEY_Z),
    ("0", KeyCode::KEY_0),
    ("1", KeyCode::KEY_1),
    ("2", KeyCode::KEY_2),
    ("3", KeyCode::KEY_3),
    ("4", KeyCode::KEY_4),
    ("5", KeyCode::KEY_5),
    ("6", KeyCode::KEY_6),
    ("7", KeyCode::KEY_7),
    ("8", KeyCode::KEY_8),
    ("9", KeyCode::KEY_9),
    ("F1", KeyCode::KEY_F1),
    ("F2", KeyCode::KEY_F2),
    ("F3", KeyCode::KEY_F3),
    ("F4", KeyCode::KEY_F4),
    ("F5", KeyCode::KEY_F5),
    ("F6", KeyCode::KEY_F6),
    ("F7", KeyCode::KEY_F7),
    ("F8", KeyCode::KEY_F8),
    ("F9", KeyCode::KEY_F9),
    ("F10", KeyCode::KEY_F10),
    ("F11", KeyCode::KEY_F11),
    ("F12", KeyCode::KEY_F12),
    ("Space", KeyCode::KEY_SPACE),
    ("Enter", KeyCode::KEY_ENTER),
    ("Tab", KeyCode::KEY_TAB),
    ("Escape", KeyCode::KEY_ESC),
    ("Backspace", KeyCode::KEY_BACKSPACE),
    ("CapsLock", KeyCode::KEY_CAPSLOCK),
    ("Insert", KeyCode::KEY_INSERT),
    ("Delete", KeyCode::KEY_DELETE),
    ("Home", KeyCode::KEY_HOME),
    ("End", KeyCode::KEY_END),
    ("PageUp", KeyCode::KEY_PAGEUP),
    ("PageDown", KeyCode::KEY_PAGEDOWN),
    ("ArrowLeft", KeyCode::KEY_LEFT),
    ("ArrowRight", KeyCode::KEY_RIGHT),
    ("ArrowUp", KeyCode::KEY_UP),
    ("ArrowDown", KeyCode::KEY_DOWN),
];

/// Maps a non-modifier key name (the last token in a shortcut string) to its
/// evdev keycode.
pub fn key_from_name(name: &str) -> Option<KeyCode> {
    KEY_TABLE
        .iter()
        .find_map(|&(n, code)| (n == name).then_some(code))
}

/// Every base key `key_from_name` recognizes — used by `inject.rs` to
/// declare the virtual keyboard's capability set once at device creation
/// (uinput requires this upfront; a device can't gain new key capabilities
/// after `build()`).
pub fn base_keys() -> impl Iterator<Item = KeyCode> {
    KEY_TABLE.iter().map(|&(_, code)| code)
}

/// All six modifier keycodes (both physical sides of each of the three
/// modifiers) — the other half of the virtual keyboard's capability set.
pub fn modifier_keys() -> impl Iterator<Item = KeyCode> {
    Modifier::ALL.into_iter().flat_map(Modifier::keycodes)
}

/// A parsed shortcut string: the modifiers that must be held (and *only*
/// those — an extra held modifier not in this set means no match, since
/// upstream's `globalShortcut` registers exact accelerators: "Ctrl + D" and
/// "Ctrl + Alt + D" are independent registrations, not a subset match) plus
/// the base key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: KeyCode,
}

/// Parses upstream's `"Ctrl + Alt + D"` format: zero or more modifiers in
/// any order, space-` + `-space separated, then exactly one base key last.
/// Returns `None` for anything that doesn't parse — a malformed or
/// not-yet-supported key name in a user's config should be skipped, not
/// crash the listener. Callers that source shortcuts from user config
/// should log a `None` result rather than silently dropping it.
///
/// **Known gap**: upstream's `eventToString()` (`Shortcuts.ts:350`)
/// can produce a bare `"Ctrl"`/`"Shift"`/`"Alt"` string when a user's
/// recorded trigger is a modifier pressed alone — this doesn't fit the
/// modifiers-plus-base-key `Shortcut` shape and isn't parsed here.
/// `state.rs::parse_bindings` feeds real `HostConfig.shortcuts` strings from
/// the renderer through this parser, so such a hotkey is dropped with an
/// `eprintln` (journal only, not surfaced to the renderer/UI). Representing
/// it correctly needs a design decision first — the natural encoding
/// (`Shortcut { key: KEY_LEFTCTRL, ctrl: true, .. }`) is self-referential:
/// `hotkeys.rs`'s exact-match check computes `ctrl` from the same keydown
/// that would be the trigger, so "Ctrl alone" and "Ctrl held" can't both be
/// expressed without resolving that ambiguity deliberately.
pub fn parse_shortcut(s: &str) -> Option<Shortcut> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;

    let tokens: Vec<&str> = s.split(" + ").collect();
    let (modifiers, base) = tokens.split_at(tokens.len().checked_sub(1)?);
    let key = key_from_name(base.first()?)?;

    for token in modifiers {
        match Modifier::from_name(token)? {
            Modifier::Ctrl => ctrl = true,
            Modifier::Shift => shift = true,
            Modifier::Alt => alt = true,
        }
    }

    Some(Shortcut {
        ctrl,
        shift,
        alt,
        key,
    })
}
