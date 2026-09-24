//! WebKit6 WebView loading the embedded `renderer/dist`, transparent
//! background.
//!
//! User-agent string must contain the substring `Electron/44.0.0` —
//! `renderer/src/web/background/IPC.ts`'s `isElectron()` transport check
//! gates overlay-specific renderer behavior (cursor-position binding,
//! overlay IPC events) on this exact substring match. Without it the
//! renderer runs as if it were a plain browser tab.
//!
//! Stage 1 (see PLAN.md).

use gtk4::gdk;
use webkit6::prelude::*;
use webkit6::WebView;

const ELECTRON_UA_SUBSTRING: &str = "Electron/44.0.0";

/// Builds a transparent WebView loading `url`, tagged with the UA substring
/// the renderer's transport check requires.
pub fn build(url: &str) -> WebView {
    let webview = WebView::new();

    let settings = WebViewExt::settings(&webview).expect("WebView always has settings");
    let default_ua = settings.user_agent().unwrap_or_default();
    settings.set_user_agent(Some(&format!("{default_ua} {ELECTRON_UA_SUBSTRING}")));
    settings.set_enable_developer_extras(true);
    // Stage 1's "renderer loads without console errors" check is verified
    // through this — renderer console.log/warn/error land on our stdout.
    settings.set_enable_write_console_messages_to_stdout(true);

    webview.set_background_color(&gdk::RGBA::TRANSPARENT);
    webview.load_uri(url);

    webview
}
