//! Event-driven clipboard read via `zwlr_data_control_manager_v1`, with an
//! X11-selection fallback for a game running under Xwayland without
//! `PROTON_ENABLE_WAYLAND=1` (gated on `compositor.rs`'s per-client `xwayland`
//! flag — the same signal upstream's `host.mjs` used).
//!
//! Improves on the prior prototype's polling approach: no subprocess spawn per
//! poll tick, no `EE2_FOCUS_GRACE`-style grace window, because there's no race
//! with a stale read to paper over.
//!
//! Stage 3 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §5.
//! Fallback if `wlr-data-control` misbehaves: revert to `wl-paste` polling after
//! clearing the selection (known to work, just slower) — keep this module's
//! public interface unchanged either way.
