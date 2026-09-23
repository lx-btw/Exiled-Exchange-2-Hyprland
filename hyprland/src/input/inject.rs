//! `uinput` virtual keyboard, created once at startup, advertising a US-only
//! layout — sidesteps the user's `kb_layout = "us,ru"` /
//! `grp:alt_shift_toggle` setup entirely, since layout switching is
//! per-physical-device and a virtual device never participates in it.
//!
//! `Ctrl+Alt+C` sequence (not `Ctrl+C` — `Alt` is PoE2's own
//! `show_advanced_item_descriptions` bind) ports the 30ms-stagger timing the
//! prior prototype's `host.mjs::injectCopy()` already empirically tuned.
//! Also handles the chat-typing sequence (upstream `text-box.ts`'s
//! `typeInChat`) for `paste-in-chat` actions.
//!
//! Deliberately NOT XTEST and NOT `hl.dsp.send_shortcut` — both are
//! known-broken on Hyprland for this (desynced modifier state / dropped
//! modifiers, respectively). Do not reintroduce either.
//!
//! Stage 2 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §4.
