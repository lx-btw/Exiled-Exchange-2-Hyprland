//! Hyprland IPC client: `.socket.sock` (commands) and `.socket2.sock` (event
//! stream), reached via `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`.
//!
//! Replaces every `hyprctl` subprocess spawn the prior prototype used
//! (`clients`, `cursorpos`, `activewindow`, `monitors`) with direct unix-socket
//! I/O. Game-window lookup (by class), cursor position, monitor geometry, and
//! the `activewindow` event subscription port from the prior `host.mjs`'s
//! `findGame()`, `cursorPos()`, and `watchHyprland()` — same wire format.
//!
//! Also the source of truth for a client's `xwayland` flag, used by
//! `clipboard.rs` to decide which selection (X11 or native Wayland) to read.
//!
//! Stage 2 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §7.
