//! Overlay window: `window.rs` (GTK4 + `gtk4-layer-shell` setup) and
//! `webview.rs` (WebKit6 WebView hosting the upstream renderer).
//!
//! Stage 1 (see PLAN.md) — implemented as the WebKit spike: layer-shell
//! surface + WebView loading the embedded renderer, transparent background,
//! input-region passthrough/interactive toggle. Not yet wired to real input
//! (Stage 2) or compositor-driven monitor placement (Stage 2/5).

pub mod webview;
pub mod window;
