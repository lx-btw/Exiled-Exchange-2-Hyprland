//! HTTP + WebSocket host server implementing the `ipc/types.ts` protocol —
//! the seam this whole fork is built on. Same routes as upstream's Electron
//! host (`/`, `/config`, `/proxy/<url>`, WS `/events`), same IPC event names
//! (`MAIN->OVERLAY::focus-change`, `CLIENT->MAIN::save-config`, etc.), ported
//! directly from the prior prototype's `host.mjs`. Static files come from the
//! embedded `renderer/dist` (via `rust-embed`) instead of a filesystem read.
//!
//! **New vs. the prior prototype**: handle `OVERLAY->MAIN::render-state
//! {shouldShow}` — upstream added this event after the prototype was built;
//! it only logged it. See HYPRLAND-NOTES.md §8 for what needs investigating
//! before this can be wired up (Stage 5).
//!
//! Stage 4 (see PLAN.md).
