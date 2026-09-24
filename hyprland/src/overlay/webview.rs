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
///
/// **The Stage 1 WebView-transparency bug is still open** (see PLAN.md).
/// Three isolated fix attempts during Stage 5 (2026-09-24), each tested
/// alone against a live Hyprland session:
///
/// 1. A `window.rs` GTK-level CSS provider forcing the window's own
///    background transparent (working around a system theme's opaque
///    `.background` rule) — no visible change.
/// 2. `webview.set_background_color()` swapped between `TRANSPARENT` and
///    opaque magenta — pixel-identical screenshots either way. This call
///    has *zero* effect on what's actually on screen on this system.
/// 3. A `UserStyleLevel::User` `UserStyleSheet` forcing `color-scheme:
///    normal` and `background-color: transparent` on `html`/`body`
///    (`renderer/index.html` sets `<meta name="color-scheme" content="dark">`
///    and can't be edited directly — fork boundary) — also no visible
///    change, *and* this one additionally broke `KeyboardMode::Exclusive`:
///    hotkey presses (Ctrl+D, Shift+Space) stopped reliably capturing
///    input, leaking through to the game (character moved/rolled) even
///    though `render-state`/dispatch logs showed everything else working
///    correctly. Mechanism unconfirmed — nothing about installing a
///    `UserContentManager` stylesheet should plausibly touch GTK/layer-shell
///    input handling, but it reproduced twice and reverting it alone (with
///    `window.rs` already reverted from attempt 1) restored correct
///    keyboard capture both times.
///
/// Net result: the opaque backdrop survives every tested fix, suggesting a
/// lower-level compositing issue (WebKit6/GSK not preserving an alpha
/// channel through to the Wayland buffer at all, regardless of what CSS or
/// the WebKit API say) rather than a simple CSS-cascade problem. Revisit
/// with lower-level tools (`GTK_DEBUG`/`WEBKIT_*` env vars, inspecting the
/// actual buffer format reaching the compositor) rather than more CSS
/// attempts — and never bundle a fix attempt here with anything else, given
/// how attempt 3 masked a real input-handling regression behind what looked
/// like a purely cosmetic change.
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
