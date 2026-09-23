//! Overlay mode (passthrough/interactive) and game-focus tracking — the
//! state `server.rs` and `overlay/window.rs` both need to agree on. Ported
//! from upstream's `main/src/windowing/OverlayWindow.ts` state machine and
//! the prior prototype's `host.mjs` (`overlayInteractive`, `tellOverlay()`,
//! `onActiveWindow()`).
//!
//! Stage 4 (see PLAN.md).
