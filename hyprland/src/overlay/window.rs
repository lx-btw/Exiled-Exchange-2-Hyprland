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
//! - **Input region and keyboard mode are two independent knobs, not one**
//!   (split 2026-09-24, H2B — see `state.rs`'s `apply_keyboard_mode`). The
//!   input region still just follows interactive/passthrough, same as
//!   always; keyboard mode additionally depends on whether an input element
//!   *inside* the WebView currently has focus (`webview.rs`'s
//!   focusin/focusout bridge), so the overlay can be mouse-interactive
//!   (full input region) while the game still receives keyboard input by
//!   default — only actually typing into a field inside the overlay steals
//!   the keyboard. `set_keyboard_mode` here still only ever chooses between
//!   `None` and `Exclusive`, never `OnDemand`, same invariant as always.
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
///
/// **The Stage 1 WebView-transparency bug is fixed (Phase A, 2026-09-24;
/// see PLAN.md).** Root cause: the system GTK4 theme's
/// `~/.config/gtk-4.0/gtk.css` unconditionally paints `.background` (a CSS
/// class every top-level `GtkWindow` gets) fully opaque
/// (`background-color: #1e1e2e`). That's a real, correctly-rendered opaque
/// pixel in this window's own buffer, so GDK's Wayland backend correctly
/// reports it to the compositor via `wl_surface.set_opaque_region` covering
/// the whole surface — confirmed with `WAYLAND_DEBUG=1`
/// (`hyprland/examples/transparency_lab.rs`): without this fix, a
/// `wl_region.add(0, 0, <monitor W>, <monitor H>)` +
/// `wl_surface.set_opaque_region(...)` is sent right before every commit;
/// with it, `set_opaque_region` is never sent at all (GDK computes there's
/// no opaque content and lets the Wayland-spec no-opaque-region default
/// apply), and Hyprland correctly alpha-blends. This was never a Hyprland
/// occlusion-culling behavior, a "stuck"/unclearable opaque region, or a
/// WebKit/GSK alpha-channel problem — see PLAN.md's Phase A writeup for the
/// full trace and the (surprisingly wrong) earlier hypotheses this rules
/// out. The `CssProvider` below defeats that theme rule the standard GTK
/// way (a `STYLE_PROVIDER_PRIORITY_USER` provider outranks the theme's
/// `PRIORITY_THEME` one for the same selector), which is what makes this a
/// clean fix through GTK's own public behavior rather than a
/// protocol-level workaround.
pub fn build(app: &gtk4::Application, webview: &WebView) -> gtk4::ApplicationWindow {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Exiled Exchange 2")
        .build();

    if let Some(display) = gtk4::gdk::Display::default() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data("window, window.background { background-color: transparent; }");
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
    }

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
    // passthrough input region and (keyboard-)None mode then, not now.
    window.connect_realize(|window| {
        set_input_region(window, true);
        set_keyboard_mode(window, false);
    });

    window
}

/// Toggles the surface's input region between empty (mouse passes through
/// to the game underneath) and full-window (overlay is mouse-interactive).
/// Split from keyboard mode (`set_keyboard_mode`, below) — see module docs'
/// H2B entry for why they're no longer one call.
pub fn set_input_region(window: &gtk4::ApplicationWindow, passthrough: bool) {
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
                    "ee2-hyprland: set_input_region(false): no window size and no monitor found — ignoring"
                );
                return;
            }
        };
        Region::create_rectangle(&RectangleInt::new(0, 0, width, height))
    };
    surface.set_input_region(Some(&region));
}

/// Whether the layer surface currently holds real Wayland keyboard focus
/// (`Exclusive`) or not (`None`) — **never `OnDemand`**, see module docs.
/// Driven by `state.rs::apply_keyboard_mode`, combining "is the overlay
/// mouse-interactive at all" with "does an input element inside the WebView
/// currently have focus" — unlike `set_input_region`, this can flip while
/// the overlay stays mouse-interactive the whole time (clicking into, then
/// out of, a text field).
pub fn set_keyboard_mode(window: &gtk4::ApplicationWindow, exclusive: bool) {
    let mode = if exclusive {
        KeyboardMode::Exclusive
    } else {
        KeyboardMode::None
    };
    if window.keyboard_mode() != mode {
        eprintln!("ee2-hyprland: surface: keyboard mode {mode:?}");
    }
    window.set_keyboard_mode(mode);
}

/// Shows or hides the layer-shell surface entirely, driven by
/// `OVERLAY->MAIN::render-state`'s `shouldShow` (`state.rs`) — the
/// renderer's own signal for "is there any widget content visible right
/// now" (`OverlayWindow.vue`'s `isOverlayVisible`), not to be confused with
/// interactive-vs-passthrough. Also a safety net independent of the
/// transparency fix above: an unmapped layer surface shows nothing at all,
/// so any future rendering regression stays invisible instead of painting
/// over the desktop/game.
pub fn set_visible(window: &gtk4::ApplicationWindow, visible: bool) {
    if window.is_visible() != visible {
        eprintln!("ee2-hyprland: surface: visible={visible}");
    }
    window.set_visible(visible);
}

/// The monitor this window's surface actually landed on — since `main.rs`
/// never calls `set_monitor`, that's whichever output the compositor
/// assigned the layer surface by default. Falls back to the display's first
/// monitor if the surface isn't realized yet (mirrors
/// `window_size_or_monitor_fallback`'s own fallback).
fn current_monitor(window: &gtk4::ApplicationWindow) -> Option<gdk::Monitor> {
    let display = gtk4::prelude::RootExt::display(window);
    window
        .surface()
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| display.monitors().item(0)?.downcast::<gdk::Monitor>().ok())
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

    let geometry = current_monitor(window)?.geometry();
    Some((geometry.width(), geometry.height()))
}

/// The overlay's monitor's origin in Hyprland's global layout coordinates —
/// e.g. `(0, 155)` for a `DP-1` sitting above a shorter second monitor.
/// `compositor::cursor_pos()` and `hyprctl monitors`/`clients` all report
/// positions in this global frame, but WebKit's `window.screenX`/`screenY`
/// on our layer-shell surface are **always `(0, 0)`** — confirmed live
/// (2026-09-24, Stage 5 hover-price-check work): Wayland doesn't expose a
/// client's absolute on-screen position to its own content, by design. So
/// `PriceCheckWindow.vue`'s `area`/`from` math (built from `window.screenX`
/// and `window.innerWidth`) operates in *monitor-local* logical pixels, and
/// `state.rs` has to convert `item-text.position` into that same frame
/// before sending it (and convert `OVERLAY->MAIN::track-area`'s `from`/`area`
/// back to the global frame before comparing against live `cursor_pos()`
/// reads) — this is the one place that conversion needs a monitor origin
/// from, since nothing else in this fork tracks it.
pub fn current_monitor_origin(window: &gtk4::ApplicationWindow) -> Option<(i32, i32)> {
    let geometry = current_monitor(window)?.geometry();
    Some((geometry.x(), geometry.y()))
}

/// Pins the surface to the monitor whose origin is `origin` (the one
/// showing the game window), or lets the compositor choose if `None`/no
/// match. gtk4-layer-shell remaps a mapped surface itself when this changes,
/// so it is only called on an actual change.
pub fn set_monitor_by_origin(window: &gtk4::ApplicationWindow, origin: Option<(i32, i32)>) {
    let display = gtk4::prelude::RootExt::display(window);
    let wanted = origin.and_then(|(x, y)| find_monitor_by_origin(&display, x, y));
    if window.monitor() != wanted {
        eprintln!("ee2-hyprland: surface: monitor -> {origin:?}");
        window.set_monitor(wanted.as_ref());
    }
}

/// Finds the monitor whose **origin** matches `(x, y)`.
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
