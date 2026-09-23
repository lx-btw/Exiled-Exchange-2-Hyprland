//! evdev hotkey listener. Opens `/dev/input/event*` read-only — **never**
//! `EVIOCGRAB` — and watches for the shortcut combos defined in the user's
//! `config.json` in parallel with everything else on the system. The game
//! keeps receiving every keypress completely normally.
//!
//! Merges modifier state across all open keyboard devices into one logical
//! state machine (handles multiple physical keyboards) — a bitset of held
//! modifiers plus last-keycode, updated per event. `hotkeyHold` (e.g.
//! price-check's `Ctrl`) is checked against this live bitset; `stashScroll`
//! reads `REL_WHEEL` off the mouse device the same way.
//!
//! Ported from upstream's `main/src/shortcuts/Shortcuts.ts` dispatch logic,
//! but the registration mechanism (`globalShortcut`) is not — this listens,
//! it never grabs or registers anything with the compositor.
//!
//! Stage 2 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §3.
//! Fallback if merging modifier state across multiple keyboards proves
//! unreliable: track only the device that produced the last keydown.
