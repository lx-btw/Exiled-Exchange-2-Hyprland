//! Overlay window: `window.rs` (GTK4 + `gtk4-layer-shell` setup: layer,
//! anchors, input region, keyboard mode, monitor placement, visibility) and
//! `webview.rs` (WebKit6 WebView hosting the upstream renderer, plus the
//! injected focus/hover bridges). Everything here runs on the GTK main
//! thread; `state.rs` marshals calls onto it.

pub mod webview;
pub mod window;
