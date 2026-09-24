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
//!   full-window rectangle) — always construct an explicit region for both
//!   states, never pass `None`, mirroring the caution the prior prototype's
//!   (non-nullable) Python/cairo bindings required, even though this Rust
//!   binding's signature technically allows it.
//!
//! Stage 1 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §6.
//! Fallback if WebKit6 mis-renders the upstream renderer: this module (and
//! `webview.rs`) is the only thing that changes to switch engines — swap for
//! C++/Qt6 + QtWebEngine + LayerShellQt. Everything downstream (`server.rs`,
//! `compositor.rs`, `input/`, `clipboard.rs`) is engine-agnostic by design.

use gtk4::cairo::{RectangleInt, Region};
use gtk4::gdk;
use gtk4::gio::prelude::*;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use webkit6::WebView;

/// Builds the layer-shell overlay window and mounts `webview` inside it.
/// Starts in passthrough mode — nothing steals input until something (a
/// hotkey, Stage 2+) switches it to interactive.
pub fn build(app: &gtk4::Application, webview: &WebView) -> gtk4::ApplicationWindow {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Exiled Exchange 2")
        .build();

    // Layer-shell setup must happen before the window is realized.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_namespace(Some("ee2-hyprland"));

    window.set_child(Some(webview));

    // The surface doesn't exist until realize — apply the initial
    // passthrough input region then, not now.
    window.connect_realize(|window| set_passthrough(window, true));

    window
}

/// Toggles the surface's input region between empty (mouse/keyboard pass
/// through to the game underneath) and full-window (overlay is
/// interactive). Also flips keyboard mode between `None` and `Exclusive` —
/// never `OnDemand`, see module docs.
pub fn set_passthrough(window: &gtk4::ApplicationWindow, passthrough: bool) {
    let Some(surface) = window.surface() else {
        return;
    };

    let region = if passthrough {
        Region::create()
    } else {
        let (width, height) = match window_size_or_monitor_fallback(window) {
            Some(size) => size,
            None => {
                // Layout hasn't been allocated *and* no monitor is known
                // yet — vanishingly unlikely (GDK knows about connected
                // monitors before any window exists), but if it happens,
                // don't silently pretend we succeeded: a caller (state.rs)
                // that already recorded "interactive" in its own tracked
                // state would otherwise desync from reality with no way to
                // notice or retry, since nothing here reports failure back.
                eprintln!(
                    "ee2-hyprland: set_passthrough(false): no window size and no monitor found — ignoring"
                );
                return;
            }
        };
        Region::create_rectangle(&RectangleInt::new(0, 0, width, height))
    };
    surface.set_input_region(Some(&region));

    window.set_keyboard_mode(if passthrough {
        KeyboardMode::None
    } else {
        KeyboardMode::Exclusive
    });
}

/// Shows or hides the layer-shell surface entirely, driven by
/// `OVERLAY->MAIN::render-state`'s `shouldShow` (`state.rs`) — the
/// renderer's own signal for "is there any widget content visible right
/// now" (`OverlayWindow.vue`'s `isOverlayVisible`), not to be confused with
/// interactive-vs-passthrough. Hiding when there's nothing to show also
/// happens to paper over the still-unresolved WebView transparency issue
/// from Stage 1 (`webview.rs`'s `set_background_color(TRANSPARENT)` doesn't
/// produce real alpha — see PLAN.md): an unmapped layer surface shows
/// nothing at all, transparent or not, so the solid backdrop that bug
/// produces is simply never on screen while idle.
pub fn set_visible(window: &gtk4::ApplicationWindow, visible: bool) {
    window.set_visible(visible);
}

/// The window's own allocated size, or — if layout hasn't run yet (e.g.
/// this is called immediately after `present()`, before the first
/// size-allocate cycle completes) — the first monitor's geometry as a
/// stand-in. Correct as a stand-in specifically *because* this window is
/// always anchored to all four edges of whichever monitor it's on, so
/// "monitor size" and "eventual window size" are the same thing; this
/// isn't a general-purpose substitution.
fn window_size_or_monitor_fallback(window: &gtk4::ApplicationWindow) -> Option<(i32, i32)> {
    let (width, height) = (window.width(), window.height());
    if width > 0 && height > 0 {
        return Some((width, height));
    }

    let display = gtk4::prelude::RootExt::display(window);
    let monitor = window
        .surface()
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| display.monitors().item(0)?.downcast::<gdk::Monitor>().ok())?;
    let geometry = monitor.geometry();
    Some((geometry.width(), geometry.height()))
}

/// Finds the monitor whose **origin** matches `(x, y)`. Used once
/// `compositor.rs` (Stage 2) can report which monitor the game window is
/// actually on — unreferenced until then.
#[allow(dead_code)]
pub fn find_monitor_by_origin(display: &gdk::Display, x: i32, y: i32) -> Option<gdk::Monitor> {
    display
        .monitors()
        .iter::<gdk::Monitor>()
        .flatten()
        .find(|monitor| {
            let geometry = monitor.geometry();
            geometry.x() == x && geometry.y() == y
        })
}
