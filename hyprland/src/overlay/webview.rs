//! WebKit6 WebView loading the embedded `renderer/dist`, transparent
//! background.
//!
//! User-agent string must contain the substring `Electron/44.0.0` —
//! `renderer/src/web/background/IPC.ts`'s `isElectron()` transport check
//! gates overlay-specific renderer behavior (cursor-position binding,
//! overlay IPC events) on this exact substring match. Without it the
//! renderer runs as if it were a plain browser tab.

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

/// The `window.webkit.messageHandlers.<name>` the pointer-hover bridge posts
/// to — see `hover_bridge_script`'s doc comment.
const HOVER_MESSAGE_HANDLER: &str = "eeHover";

/// Injected at document-start into every page this `WebView` ever loads
/// (there's only ever one, the embedded renderer, but `UserScript`s aren't
/// per-navigation). Bridges DOM focus onto `KeyboardMode`:
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

/// Injected next to `focus_bridge_script`; reports whether the pointer is
/// over an overlay *widget* (`true`) or over the transparent background
/// (`false`), so `state.rs` can make the keyboard follow the pointer
/// (needed for the Shift stat-tooltip in trade results, which listens for
/// `keydown` on `window`).
///
/// What counts as "background", derived from `OverlayWindow.vue` without
/// touching it: `#overlay-window`'s first element child is the full-size
/// `<div>` carrying `@click="handleBackgroundClick"`; the root itself,
/// `<html>` and `<body>` are the same thing for this purpose. Every other
/// element is a widget (or other visible overlay content). Widgets' own
/// transparent areas are `pointer-events-none` in the renderer
/// (`PriceCheckWindow.vue`), so hit-testing already sends those to the
/// background div — the event target is the right discriminator. Posts on
/// every `mouseover` (fires only on element transitions); `state.rs`
/// ignores repeats, so no dedupe state lives here to go stale across
/// hide/show.
fn hover_bridge_script() -> String {
    format!(
        r#"(function() {{
            function isBackground(el) {{
                if (!el || el === document.documentElement || el === document.body) return true;
                if (el.id === "overlay-window") return true;
                var parent = el.parentElement;
                return !!parent && parent.id === "overlay-window" && parent.firstElementChild === el;
            }}
            document.addEventListener("mouseover", function(e) {{
                window.webkit.messageHandlers.{HOVER_MESSAGE_HANDLER}.postMessage(!isBackground(e.target));
            }}, true);
            document.documentElement.addEventListener("mouseleave", function() {{
                window.webkit.messageHandlers.{HOVER_MESSAGE_HANDLER}.postMessage(false);
            }});
        }})();"#
    )
}

/// Hands an http(s) URL to the desktop's default handler through
/// `gtk4::UriLauncher` (the OpenURI portal when sandboxed, the default
/// app otherwise). Port of upstream's `setWindowOpenHandler` →
/// `shell.openExternal` (`main/src/windowing/OverlayWindow.ts`); anything
/// but http/https is refused, same restraint `openExternal` callers in the
/// renderer rely on (only ever pass web links).
fn open_external(uri: &str) {
    let lower = uri.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        eprintln!("ee2-hyprland: window.open: ignoring non-http(s) URL {uri:?}");
        return;
    }
    let uri_owned = uri.to_owned();
    gtk4::UriLauncher::new(uri).launch(
        None::<&gtk4::Window>,
        None::<&gtk4::gio::Cancellable>,
        move |result| {
            if let Err(err) = result {
                eprintln!("ee2-hyprland: window.open: failed to launch {uri_owned:?}: {err}");
            }
        },
    );
}

/// Builds a transparent WebView loading `url`, tagged with the UA substring
/// the renderer's transport check requires.
///
/// Transparency itself comes from the *window* (`window.rs`'s `build()`:
/// a `CssProvider` defeating an opaque theme `.background`), not the page.
/// `set_background_color(TRANSPARENT)` below is the spec-correct call for a
/// transparent `WebView` but has no independent effect on its own.
/// Deliberately **not** using a `UserStyleSheet` to force `html`/`body`
/// transparent: an attempt at that reproducibly broke
/// `KeyboardMode::Exclusive` (hotkeys stopped reliably capturing input)
/// through an unconfirmed mechanism — the window-level fix doesn't need
/// it, so don't reintroduce it. The `UserContentManager` used for the
/// focus/hover bridge *scripts* is a different, verified-safe use of the
/// same object; after touching it, check that typing into a text field
/// inside the overlay (which needs `Exclusive`) still works.
///
/// **`HardwareAccelerationPolicy::Never` is load-bearing** (HYPRLAND-NOTES.md
/// §9): with accelerated compositing, WebKit never attaches a buffer again
/// after gtk4-layer-shell recreates the surface's `zwlr_layer_surface_v1`
/// role on a hide/show cycle, and GTK's frame clock spins in an empty
/// configure/commit loop at the refresh rate — nothing is ever displayed
/// again. The non-accelerated path re-attaches a real buffer on every
/// cycle. This is the per-`WebView` equivalent of the process-wide
/// `WEBKIT_DISABLE_COMPOSITING_MODE=1`.
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

    content_manager.add_script(&UserScript::new(
        &hover_bridge_script(),
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content_manager.register_script_message_handler(HOVER_MESSAGE_HANDLER, None);
    content_manager.connect_script_message_received(Some(HOVER_MESSAGE_HANDLER), |_ucm, value| {
        state::set_pointer_over_widget(value.to_boolean());
    });

    let webview = WebView::builder()
        .user_content_manager(&content_manager)
        .build();

    let settings = WebViewExt::settings(&webview).expect("WebView always has settings");
    let default_ua = settings.user_agent().unwrap_or_default();
    settings.set_user_agent(Some(&format!("{default_ua} {ELECTRON_UA_SUBSTRING}")));
    settings.set_enable_developer_extras(true);
    // Renderer console.log/warn/error land on our stdout (the journal) —
    // the first place to look when the renderer misbehaves.
    settings.set_enable_write_console_messages_to_stdout(true);
    settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);

    webview.set_background_color(&gdk::RGBA::TRANSPARENT);

    // `window.open(...)` (Trade / wiki / poe2db / update links) arrives as a
    // `create` request. Open the URL externally and return `None` — no new
    // WebView/window is ever created. `builtinBrowser = true` is not
    // supported (Electron `<webview>` tag, absent in WebKit).
    webview.connect_create(|_webview, action| {
        let action = action.clone();
        match action.request().and_then(|request| request.uri()) {
            Some(uri) => open_external(&uri),
            None => eprintln!("ee2-hyprland: window.open: request without a URL, ignoring"),
        }
        None
    });

    webview.load_uri(url);

    webview
}
