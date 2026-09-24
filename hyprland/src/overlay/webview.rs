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
use webkit6::{
    HardwareAccelerationPolicy, UserContentInjectedFrames, UserContentManager, UserScript,
    UserScriptInjectionTime, WebView,
};

use crate::state;

const ELECTRON_UA_SUBSTRING: &str = "Electron/44.0.0";

/// The `window.webkit.messageHandlers.<name>` this WebView's focus bridge
/// posts to — see `focus_bridge_script`'s doc comment.
const FOCUS_MESSAGE_HANDLER: &str = "eeFocus";

/// Injected at document-start into every page this `WebView` ever loads
/// (there's only ever one, the embedded renderer, but `UserScript`s aren't
/// per-navigation). Bridges DOM focus onto `KeyboardMode` (H2B, PLAN.md):
/// `document`-level `focusin`/`focusout` delegation (these bubble, unlike
/// plain `focus`/`blur`, so one pair of listeners at the root catches every
/// element the renderer ever mounts, including ones that don't exist yet
/// when this script runs) posts a plain boolean whenever the focus target
/// is something that actually wants to receive typed keys — `<input>`,
/// `<textarea>`, `<select>`, or anything `isContentEditable` (the DOM's own
/// query for "would a real browser deliver text input here", covering
/// inherited `contenteditable` correctly without reimplementing that
/// inheritance rule by hand). This is deliberately generic, not tied to
/// `HotkeyInput.vue`'s specific `<input>` — the renderer also has ordinary
/// text fields (stash-search text, settings forms) that need the same
/// treatment, and a document-level delegation selector covers all of them
/// without touching renderer source. The renderer is never modified; this
/// is purely an injected observer, nothing it does can be detected from
/// page script (no properties added to any DOM node, no wrapped
/// listeners).
fn focus_bridge_script() -> String {
    format!(
        r#"(function() {{
            function wantsKeyboard(el) {{
                if (!el) return false;
                if (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT") return true;
                return !!el.isContentEditable;
            }}
            document.addEventListener("focusin", function(e) {{
                if (wantsKeyboard(e.target)) {{
                    window.webkit.messageHandlers.{FOCUS_MESSAGE_HANDLER}.postMessage(true);
                }}
            }});
            document.addEventListener("focusout", function(e) {{
                if (wantsKeyboard(e.target)) {{
                    window.webkit.messageHandlers.{FOCUS_MESSAGE_HANDLER}.postMessage(false);
                }}
            }});
        }})();"#
    )
}

/// Builds a transparent WebView loading `url`, tagged with the UA substring
/// the renderer's transport check requires.
///
/// **The Stage 1 WebView-transparency bug is fixed** — root cause and the
/// actual fix are in `window.rs`'s `build()` doc comment (a system GTK4
/// theme's opaque `.background` rule, defeated with a `CssProvider` on the
/// *window*, not the page). `set_background_color(TRANSPARENT)` below is
/// kept as the spec-correct call for a transparent `WebView` — real testing
/// during Phase A (2026-09-24) found it has no independent effect either
/// way on this system, since the window-level fix alone already gets GDK to
/// stop reporting an opaque region at all — but there's no reason to set it
/// to anything else. Deliberately **not** using a `UserStyleSheet` to force
/// `html`/`body` transparent from here: an earlier attempt at that (Stage 5,
/// 2026-09-24) reproducibly broke `KeyboardMode::Exclusive` (hotkeys stopped
/// reliably capturing input) through an unconfirmed mechanism — the
/// window-level fix doesn't need it, so don't reintroduce it.
///
/// **Render-storm fix, now via per-`WebView` API instead of a process-wide
/// env var** (PLAN.md, "Storm mechanism" / Session 7 / G1): originally
/// `WEBKIT_DISABLE_COMPOSITING_MODE=1` set as a process env var in
/// `main.rs`, before any WebKit object existed. Verified in G1
/// (`hyprland/diag/G1_*`, both the lab's `EE2_LAB_HWACCEL=never` mode and
/// the real binary's tray-toggle cycle) that
/// `settings.set_hardware_acceleration_policy(Never)` reproduces the exact
/// same fix — WebKit falls through its non-accelerated rendering path,
/// which re-attaches a real buffer through the window's own surface on
/// every hide/show cycle instead of getting stuck after
/// gtk4-layer-shell recreates the surface's `zwlr_layer_surface_v1` role.
/// This is the documented, per-object way to ask for the same thing
/// `WEBKIT_DISABLE_COMPOSITING_MODE` does process-wide, so it replaces the
/// `unsafe std::env::set_var` call that used to live in `main.rs`.
///
/// **H2B, 2026-09-24: now built with a `UserContentManager`** (the
/// focus-bridge script above, registering `eeFocus`) — the module docs'
/// note right above about a `UserStyleSheet` reproducibly breaking
/// `KeyboardMode::Exclusive` is about a *stylesheet* specifically, not
/// `UserContentManager` itself, but since both go through the same object
/// this is exactly the class of change that note warns to be careful
/// with — H2B's own live verification checklist includes actually typing
/// into a real input field (which only works at all if `Exclusive` still
/// delivers keystrokes), so a regression here wouldn't go unnoticed.
pub fn build(url: &str) -> WebView {
    let content_manager = UserContentManager::new();
    content_manager.add_script(&UserScript::new(
        &focus_bridge_script(),
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content_manager.register_script_message_handler(FOCUS_MESSAGE_HANDLER, None);
    content_manager.connect_script_message_received(Some(FOCUS_MESSAGE_HANDLER), |_ucm, value| {
        // Fires on the GTK main thread (WebKit dispatches signals on the
        // thread running the WebView's own `GMainContext`, which is this
        // process's one and only main loop) — `state::set_input_focused`
        // touches GTK-thread-only state directly, no marshaling needed,
        // same as `window.rs`'s `connect_realize` callback.
        state::set_input_focused(value.to_boolean());
    });

    let webview = WebView::builder()
        .user_content_manager(&content_manager)
        .build();

    let settings = WebViewExt::settings(&webview).expect("WebView always has settings");
    let default_ua = settings.user_agent().unwrap_or_default();
    settings.set_user_agent(Some(&format!("{default_ua} {ELECTRON_UA_SUBSTRING}")));
    settings.set_enable_developer_extras(true);
    // Stage 1's "renderer loads without console errors" check is verified
    // through this — renderer console.log/warn/error land on our stdout.
    settings.set_enable_write_console_messages_to_stdout(true);
    settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);

    webview.set_background_color(&gdk::RGBA::TRANSPARENT);
    webview.load_uri(url);

    webview
}
