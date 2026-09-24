//! Standalone GTK4 layer-shell transparency test lab — **not** part of the
//! shipped `ee2-hyprland` binary. Exists purely to iterate on the Stage 1
//! WebView-transparency bug (see PLAN.md) without going through the real
//! app's server/hotkey/WS/game-focus pipeline for every single test.
//!
//! Run with:
//!   cargo run --example transparency_lab
//! Controlled entirely via env vars (no recompile needed to try a new
//! combination):
//!   EE2_LAB_ENGINE=gtk|webkit      (default: webkit)
//!     "gtk"    — a plain GTK4 widget tree, no WebKit at all. Tests whether
//!                layer-shell + GTK4 can do transparency on this system at
//!                all, independent of WebKit as a variable.
//!     "webkit" — a WebView loading a trivial inline HTML page (matches
//!                Stage 1's original "trivial page" test).
//!   EE2_LAB_CSS=1                  apply a window.rs-style GTK CssProvider
//!                                  forcing the window's own background
//!                                  transparent (reverted attempt #1).
//!   EE2_LAB_STYLE=1                (webkit only) apply a webview.rs-style
//!                                  WebKit UserStyleSheet forcing html/body
//!                                  transparent (reverted attempt #3).
//!   EE2_LAB_COLOR_SCHEME=1         (webkit only) include
//!                                  <meta name="color-scheme" content="dark">
//!                                  in the trivial page, matching
//!                                  renderer/index.html.
//!   EE2_LAB_BG=transparent|magenta (webkit only) WebView's own
//!                                  set_background_color — default
//!                                  transparent; magenta is the "does this
//!                                  API do anything at all" sanity check
//!                                  from today's live session.
//!   EE2_LAB_ANCHOR=top             anchor Top+Left+Right only (not
//!                                  Bottom), height from EE2_LAB_HEIGHT
//!                                  (default 400), exclusive_zone left at
//!                                  the default (0) unless
//!                                  EE2_LAB_EXCLUSIVE_NEG1=1. For the Phase A
//!                                  threshold-bisection test: does the
//!                                  surface stop blending once its height
//!                                  reaches the DP-1 work-area height
//!                                  (1393px, i.e. monitor height 1440 minus
//!                                  the 47px reserved top panel)?
//!   EE2_LAB_HEIGHT=N                height in px for EE2_LAB_ANCHOR=top.
//!   EE2_LAB_EXCLUSIVE_NEG1=1        (EE2_LAB_ANCHOR=top only) also set
//!                                  exclusive_zone(-1), matching the real
//!                                  overlay's full-monitor surface.
//!   EE2_LAB_MONITOR=<connector>     pin the layer surface to a specific
//!                                  output (e.g. DP-2) via
//!                                  gtk_layer_set_monitor, instead of
//!                                  whatever output GTK picks by default.
//!                                  Lets a test target an output with no
//!                                  other clients on its active workspace,
//!                                  without switching workspaces.
//!   EE2_LAB_INTERACTIVE=1          KeyboardMode::Exclusive plus a real
//!                                  (non-empty) input region, matching the
//!                                  real overlay's state right after
//!                                  `toggle-overlay` fires — added for the
//!                                  render-storm diagnosis (PLAN.md E4) to
//!                                  test whether keyboard-exclusive is what
//!                                  triggers it, independent of WebKit
//!                                  content or translucency. Default
//!                                  (unset) keeps this lab's original
//!                                  always-passthrough/`KeyboardMode::None`
//!                                  posture.
//!   EE2_LAB_ANIMATE=1              (webkit only) adds an infinite CSS
//!                                  `@keyframes` spin animation to the
//!                                  trivial page — tests whether
//!                                  continuously-animating/repainting
//!                                  content is what triggers the render
//!                                  storm (PLAN.md E4), since a static page
//!                                  alone does not.
//!   EE2_LAB_STORM_REPRO=buggy|fixed|runtime
//!                                  Minimal repro/verification for the
//!                                  storm mechanism narrowed from the E0
//!                                  trace (PLAN.md, "Storm mechanism"):
//!                                  starts hidden (same `present()` then
//!                                  `set_visible(false)` sequence as
//!                                  `main.rs`), then 2s later fires one
//!                                  GTK-callback transition.
//!                                  "buggy" replays the exact real ordering
//!                                  (`state.rs::set_passthrough_on_main_thread`
//!                                  + `window.rs::set_passthrough`):
//!                                  `set_visible(true)`, then input region,
//!                                  then `KeyboardMode::Exclusive` — i.e.
//!                                  keyboard/region land *after* the
//!                                  unbuffered initial commit `set_visible`
//!                                  triggers. "fixed" reorders it: input
//!                                  region + keyboard mode while still
//!                                  hidden, `set_visible(true)` last (fix
//!                                  variant A attempt). "runtime" never
//!                                  hides at all (surface mapped and
//!                                  rendering since creation) — only the
//!                                  keyboard mode + input region change
//!                                  fires at the 2s mark, isolating that
//!                                  from the hide/show remap (gtk4-layer-shell
//!                                  destroys and recreates the
//!                                  `zwlr_layer_surface_v1` role object on
//!                                  every `set_visible` cycle). Overrides
//!                                  EE2_LAB_INTERACTIVE (initial state is
//!                                  always passthrough/None, matching
//!                                  `main.rs`'s real startup, regardless of
//!                                  that flag).
//!
//! Every mode draws a semi-transparent magenta marker box in the corner
//! (rgba, 40% alpha) as a position/blend sanity check distinct from the
//! transparency question itself — if the marker shows solid magenta
//! instead of a blend with the desktop behind it, alpha isn't being
//! respected at all, regardless of what the rest of the surface does.
//!
//! Exits on Escape (`KeyboardMode::None` throughout — doesn't grab
//! anything, same click/keyboard-passthrough posture the real overlay
//! starts in, so it never fights with the game for input while testing;
//! kill from outside with `pkill -f transparency_lab` if Escape doesn't
//! reach it for the same reason the real overlay's keyboard capture is
//! under investigation).

use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

fn main() -> gtk4::glib::ExitCode {
    let engine = std::env::var("EE2_LAB_ENGINE").unwrap_or_else(|_| "webkit".to_string());
    let apply_css = env_flag("EE2_LAB_CSS");
    let apply_style = env_flag("EE2_LAB_STYLE");
    let color_scheme = env_flag("EE2_LAB_COLOR_SCHEME");
    let bg_magenta = std::env::var("EE2_LAB_BG").as_deref() == Ok("magenta");
    let storm_repro = std::env::var("EE2_LAB_STORM_REPRO").ok();

    eprintln!(
        "transparency-lab: engine={engine} css={apply_css} style={apply_style} color_scheme={color_scheme} bg_magenta={bg_magenta}"
    );

    let app = gtk4::Application::builder()
        .application_id("io.github.lx-btw.ee2-hyprland.transparency-lab")
        .build();

    app.connect_activate(move |app| {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("transparency-lab")
            .build();

        if apply_css {
            if let Some(display) = gtk4::gdk::Display::default() {
                let provider = gtk4::CssProvider::new();
                provider
                    .load_from_data("window, window.background { background-color: transparent; }");
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_USER,
                );
            }
        }

        window.init_layer_shell();
        if let Ok(connector) = std::env::var("EE2_LAB_MONITOR") {
            if let Some(display) = gtk4::gdk::Display::default() {
                let monitors = display.monitors();
                let found = (0..monitors.n_items()).find_map(|i| {
                    let m = monitors.item(i)?.downcast::<gtk4::gdk::Monitor>().ok()?;
                    (m.connector().as_deref() == Some(connector.as_str())).then_some(m)
                });
                match &found {
                    Some(_) => eprintln!("transparency-lab: pinned to monitor {connector}"),
                    None => {
                        eprintln!("transparency-lab: monitor {connector} not found, using default")
                    }
                }
                window.set_monitor(found.as_ref());
            }
        }
        window.set_layer(if env_flag("EE2_LAB_LAYER_TOP") {
            Layer::Top
        } else {
            Layer::Overlay
        });
        if env_flag("EE2_LAB_SMALL") {
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Left, true);
            let (w, h) = std::env::var("EE2_LAB_SIZE")
                .ok()
                .and_then(|s| {
                    let (w, h) = s.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or((300, 200));
            window.set_default_size(w, h);
        } else if std::env::var("EE2_LAB_ANCHOR").as_deref() == Ok("top") {
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Right, true);
            window.set_anchor(Edge::Bottom, false);
            let height: i32 = std::env::var("EE2_LAB_HEIGHT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(400);
            window.set_default_size(-1, height);
            if env_flag("EE2_LAB_EXCLUSIVE_NEG1") {
                window.set_exclusive_zone(-1);
            }
        } else {
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                window.set_anchor(edge, true);
            }
            if !env_flag("EE2_LAB_NO_EXCLUSIVE") {
                window.set_exclusive_zone(-1);
            }
        }
        window.set_namespace(Some("ee2-transparency-lab"));
        // EE2_LAB_INTERACTIVE=1: matches the real overlay's state right
        // after `toggle-overlay` fires (`state.rs::set_interactive`) —
        // KeyboardMode::Exclusive plus a full-window input region, instead
        // of this lab's default always-passthrough/None posture. Added to
        // test whether keyboard-exclusive-over-a-live-fullscreen-game
        // (independent of WebKit/app logic/translucency alone, both
        // already ruled out) is what triggers the render storm.
        let interactive = storm_repro.is_none() && env_flag("EE2_LAB_INTERACTIVE");
        window.set_keyboard_mode(if interactive {
            KeyboardMode::Exclusive
        } else {
            KeyboardMode::None
        });

        match engine.as_str() {
            "gtk" => build_gtk_mode(&window),
            _ => build_webkit_mode(&window, apply_style, color_scheme, bg_magenta),
        }

        window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            if interactive {
                // Full-window input region — same as the real overlay's
                // interactive mode. Exact size doesn't need to be
                // pixel-perfect for this diagnostic (unlike the real fix in
                // `window.rs`, which needs correct hit-testing); a large
                // fixed rectangle gets the same real effect on the
                // compositor side (a genuinely non-empty input region, not
                // click-through) without needing to track allocation.
                surface.set_input_region(Some(&gtk4::cairo::Region::create_rectangle(
                    &gtk4::cairo::RectangleInt::new(0, 0, 10000, 10000),
                )));
            } else {
                // Passthrough for mouse — this lab never wants to steal
                // clicks, only observe rendering.
                surface.set_input_region(Some(&gtk4::cairo::Region::create()));
            }
            if env_flag("EE2_LAB_OPAQUE_REGION_NONE") {
                surface.set_opaque_region(None);
            }
        });

        if env_flag("EE2_LAB_OPAQUE_REGION_EVERY_FRAME") {
            window.add_tick_callback(|window, _clock| {
                if let Some(surface) = window.surface() {
                    surface.set_opaque_region(None);
                }
                gtk4::glib::ControlFlow::Continue
            });
        }

        window.present();

        if let Some(mode) = storm_repro.clone() {
            // Same startup sequence as `main.rs`: `present()` only to force
            // `realize` (so `connect_realize` above runs and sets the
            // initial passthrough input region), then hide — except
            // "runtime", which never hides at all (isolates the hide/show
            // remap itself, vs. the keyboard/region change, as the storm
            // trigger).
            if mode != "runtime" {
                window.set_visible(false);
            }
            let window = window.clone();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                eprintln!("transparency-lab: storm_repro={mode} transition firing");
                let full_region = || {
                    gtk4::cairo::Region::create_rectangle(&gtk4::cairo::RectangleInt::new(
                        0, 0, 10000, 10000,
                    ))
                };
                if mode == "show_only" {
                    // Isolates the remap from the keyboard/region change
                    // entirely: just `set_visible(true)`, keyboard mode and
                    // input region left untouched (still None/empty from
                    // startup).
                    window.set_visible(true);
                } else if mode == "runtime" {
                    // No hide/show at all — the surface has been mapped
                    // and showing real content since creation. Only the
                    // keyboard mode + input region change at runtime, to
                    // isolate that from the hide/show remap (gtk4-layer-shell
                    // destroys and recreates the zwlr_layer_surface_v1 role
                    // object on every hide/show cycle, a candidate cause
                    // noted in PLAN.md's E4 follow-up).
                    if let Some(surface) = window.surface() {
                        surface.set_input_region(Some(&full_region()));
                    }
                    window.set_keyboard_mode(KeyboardMode::Exclusive);
                } else if mode == "fixed" {
                    // Variant A: set keyboard mode + input region while
                    // still hidden, show last — so `keyboard_interactivity`
                    // goes out in the initial surface state, before the
                    // initial commit, instead of after it.
                    if let Some(surface) = window.surface() {
                        surface.set_input_region(Some(&full_region()));
                    }
                    window.set_keyboard_mode(KeyboardMode::Exclusive);
                    window.set_visible(true);
                } else {
                    // "buggy" (default for any other value): replays the
                    // real ordering bug — `set_visible_on_main_thread`/
                    // `set_passthrough_on_main_thread`'s
                    // `set_visible(true)` first (gtk4-layer-shell maps the
                    // surface and sends the initial commit with whatever
                    // keyboard mode + input region it already had — None +
                    // empty), then `set_passthrough`'s input region and
                    // keyboard mode changes land after that initial commit.
                    window.set_visible(true);
                    if let Some(surface) = window.surface() {
                        surface.set_input_region(Some(&full_region()));
                    }
                    window.set_keyboard_mode(KeyboardMode::Exclusive);
                }
            });
        }
    });

    app.run_with_args::<&str>(&[])
}

/// No WebKit at all: a plain GTK4 `Label` with an inline-CSS rgba
/// background as the marker, transparent window otherwise. If this mode
/// also shows an opaque backdrop, the bug is in GTK4/layer-shell/Hyprland
/// compositing generically — not WebKit-specific.
fn build_gtk_mode(window: &gtk4::ApplicationWindow) {
    let marker = gtk4::Label::new(Some("GTK marker rgba(255,0,255,0.4)"));
    marker.set_halign(gtk4::Align::Start);
    marker.set_valign(gtk4::Align::Start);
    marker.set_margin_top(20);
    marker.set_margin_start(20);
    marker.set_width_request(260);
    marker.set_height_request(80);

    marker.add_css_class("lab-marker");
    if let Some(display) = gtk4::gdk::Display::default() {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            ".lab-marker { background-color: rgba(255,0,255,0.4); color: white; padding: 10px; }",
        );
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    window.set_child(Some(&marker));
}

fn build_webkit_mode(
    window: &gtk4::ApplicationWindow,
    apply_style: bool,
    color_scheme: bool,
    bg_magenta: bool,
) {
    use webkit6::prelude::*;
    use webkit6::WebView;

    let webview = WebView::new();

    let bg = if bg_magenta {
        gtk4::gdk::RGBA::new(1.0, 0.0, 1.0, 1.0)
    } else {
        gtk4::gdk::RGBA::TRANSPARENT
    };
    webview.set_background_color(&bg);

    if apply_style {
        if let Some(content_manager) = webview.user_content_manager() {
            let stylesheet = webkit6::UserStyleSheet::new(
                ":root { color-scheme: normal !important; }\n\
                 html, body { background-color: transparent !important; }",
                webkit6::UserContentInjectedFrames::AllFrames,
                webkit6::UserStyleLevel::User,
                &[],
                &[],
            );
            content_manager.add_style_sheet(&stylesheet);
        }
    }

    let color_scheme_meta = if color_scheme {
        r#"<meta name="color-scheme" content="dark">"#
    } else {
        ""
    };
    // EE2_LAB_ANIMATE=1: an infinite CSS animation (mimics a perpetual
    // loading spinner / continuous repaint activity), to test whether
    // continuously-animating content over a live fullscreen game is what
    // triggers the configure-storm regression, independent of translucency
    // or keyboard-exclusive interactivity alone (both already ruled out
    // this session against the real game).
    let animate = env_flag("EE2_LAB_ANIMATE");
    let spinner = if animate {
        r#"<div style="position:fixed;top:120px;left:20px;width:40px;height:40px;
                border:6px solid rgba(255,255,255,0.2);border-top-color:#fff;
                border-radius:50%;animation:spin 1s linear infinite;"></div>
<style>@keyframes spin { to { transform: rotate(360deg); } }</style>"#
    } else {
        ""
    };
    let html = format!(
        r#"<!doctype html><html><head>{color_scheme_meta}</head>
<body style="margin:0;background:transparent">
<div style="position:fixed;top:20px;left:20px;width:260px;height:80px;
            background:rgba(255,0,255,0.4);color:white;font:16px sans-serif;
            padding:10px;box-sizing:border-box;">
WebKit marker rgba(255,0,255,0.4)
</div>
{spinner}
</body></html>"#
    );
    webview.load_html(&html, None);

    window.set_child(Some(&webview));
}
