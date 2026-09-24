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
        } else {
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                window.set_anchor(edge, true);
            }
            if !env_flag("EE2_LAB_NO_EXCLUSIVE") {
                window.set_exclusive_zone(-1);
            }
        }
        window.set_namespace(Some("ee2-transparency-lab"));
        window.set_keyboard_mode(KeyboardMode::None);

        match engine.as_str() {
            "gtk" => build_gtk_mode(&window),
            _ => build_webkit_mode(&window, apply_style, color_scheme, bg_magenta),
        }

        window.connect_realize(|window| {
            let Some(surface) = window.surface() else {
                return;
            };
            // Passthrough for mouse — this lab never wants to steal
            // clicks, only observe rendering.
            surface.set_input_region(Some(&gtk4::cairo::Region::create()));
            if env_flag("EE2_LAB_OPAQUE_REGION_NONE") {
                surface.set_opaque_region(None);
            }
        });

        window.present();
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
    let html = format!(
        r#"<!doctype html><html><head>{color_scheme_meta}</head>
<body style="margin:0;background:transparent">
<div style="position:fixed;top:20px;left:20px;width:260px;height:80px;
            background:rgba(255,0,255,0.4);color:white;font:16px sans-serif;
            padding:10px;box-sizing:border-box;">
WebKit marker rgba(255,0,255,0.4)
</div>
</body></html>"#
    );
    webview.load_html(&html, None);

    window.set_child(Some(&webview));
}
