//! GTK4 window on a `gtk4-layer-shell` overlay-layer surface. Direct Rust port
//! of the prior prototype's `overlay.py`, proven on this system:
//!
//! - Anchor all four edges + `exclusive_zone(-1)` so other layer-shell clients
//!   (bars, notification daemons) don't reserve space that shrinks us.
//! - Monitor matched by **origin coordinates, not size** — a rotated monitor
//!   reports transposed dimensions from GDK, but its origin is always correct.
//! - Keyboard mode is **`EXCLUSIVE`, never `ON_DEMAND`** — at `ON_DEMAND`,
//!   `follow_mouse` hands focus back to the game on the first mouse movement
//!   over the overlay.
//! - Input region toggle (passthrough = empty region, interactive =
//!   full-window rectangle) — the region parameter is not nullable, always
//!   construct an explicit region for both states, never pass null/None.
//!
//! Stage 1 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §6.
//! Fallback if WebKit6 mis-renders the upstream renderer: this module (and
//! `webview.rs`) is the only thing that changes to switch engines — swap for
//! C++/Qt6 + QtWebEngine + LayerShellQt. Everything downstream (`server.rs`,
//! `compositor.rs`, `input/`, `clipboard.rs`) is engine-agnostic by design.
