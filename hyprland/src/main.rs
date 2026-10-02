//! Entrypoint for the Hyprland-native platform layer: wires the modules
//! below together. `HYPRLAND-NOTES.md` has the design rationale.
//!
//! Threads: GTK owns the main thread (layer-shell overlay window + WebView,
//! `overlay/`); everything else — the `ipc/types.ts` protocol server
//! (`server.rs`), the state machine (`state.rs`), evdev hotkeys/capture
//! (`input/`), hover tracking (`area_tracker.rs`), Hyprland IPC
//! (`compositor.rs`), tray — runs on a tokio runtime on a background
//! thread. Hotkeys aren't configured here: they come from whatever
//! `HostConfig.shortcuts` the renderer pushes over WS once it mounts
//! (`OverlayWindow.vue`'s `pushHostConfig()`), and change live when the
//! user edits a hotkey in Settings.

mod area_tracker;
mod clipboard;
mod compositor;
mod config;
mod file_writer;
mod game_log;
mod input;
mod logger;
mod overlay;
mod proxy;
mod server;
mod state;
mod tray;

use std::sync::Arc;

use gtk4::gio::prelude::*;
use gtk4::prelude::*;
use tokio::sync::{broadcast, mpsc, Mutex};

fn main() -> gtk4::glib::ExitCode {
    exempt_loopback_from_proxy();

    // GTK owns the main thread's event loop (`app.run_with_args` below), so
    // everything else — server, hotkeys, compositor focus tracking — runs
    // on its own tokio runtime on a background OS thread instead of
    // sharing this one.
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(async {
            let (events_tx, _events_rx) = broadcast::channel(256);
            logger::init(events_tx.clone());

            let (overlay, hotkey_bindings, hotkeys_active, overlay_interactive) =
                match state::Overlay::new(events_tx.clone()) {
                    Ok(result) => result,
                    Err(err) => {
                        eprintln!("ee2-hyprland: failed to initialize overlay state: {err}");
                        return;
                    }
                };
            let overlay = Arc::new(Mutex::new(overlay));
            overlay
                .lock()
                .await
                .attach_self_ref(Arc::downgrade(&overlay));

            let app_state = server::AppState {
                overlay: overlay.clone(),
                proxy: Arc::new(proxy::Proxy::new()),
                events_tx,
            };

            let addr = match server::spawn_server(app_state).await {
                Ok(addr) => addr,
                Err(err) => {
                    eprintln!("ee2-hyprland: failed to start server: {err}");
                    // Return without sending: `addr_tx` drops here, so the
                    // main thread's `recv()` fails fast instead of blocking
                    // forever on a server that will never come up.
                    return;
                }
            };
            let _ = addr_tx.send(addr);

            tokio::spawn(tray::start(format!("http://{addr}/")));

            // Hotkeys: `hotkey_bindings` starts empty and is only ever
            // populated once the renderer pushes real config over WS
            // (`Overlay::handle_ws_event`) — nothing fires (and no
            // system-wide key injection happens) before that.
            //
            // Ordering matters here and isn't enforced by the compiler:
            // `state::Overlay::new()` above already constructed the
            // `VirtualKeyboard` (inside `Overlay::new`, before this point),
            // and `hotkeys::watch()`'s device enumeration excludes it by
            // name (`input::inject::VIRTUAL_KEYBOARD_NAME`). If a future
            // refactor ever made hotkey watching start *before* the virtual
            // device exists, that exclusion would silently miss it and
            // the matcher would read back our own injected keys — a
            // self-triggering feedback loop (e.g. a hotkey bound to
            // `Ctrl+Alt+C` re-firing itself). Keep `Overlay::new()` (or
            // anything else that constructs a `VirtualKeyboard`) strictly
            // before this line.
            let modifiers = input::hotkeys::ModifierState::new();
            let (hotkey_tx, mut hotkey_rx) = mpsc::unbounded_channel();
            // `close_overlay_tx`: the evdev-level `Esc` handling
            // (`hotkeys.rs`) — a built-in action, not a `bindings` entry, so
            // it gets its own channel rather than an index into
            // `hotkey_bindings` (see `hotkeys.rs`'s own docs on why `Esc`
            // isn't just another configurable hotkey).
            let (close_overlay_tx, mut close_overlay_rx) = mpsc::unbounded_channel();
            tokio::spawn(input::hotkeys::watch(
                hotkey_bindings,
                hotkeys_active,
                overlay_interactive,
                modifiers.clone(),
                hotkey_tx,
                close_overlay_tx,
            ));
            {
                let overlay = overlay.clone();
                tokio::spawn(async move {
                    while let Some(index) = hotkey_rx.recv().await {
                        eprintln!("ee2-hyprland: dispatching hotkey index {index}");
                        state::Overlay::on_hotkey_fired(&overlay, index).await;
                        eprintln!("ee2-hyprland: hotkey index {index} dispatch complete");
                    }
                });
            }
            {
                let overlay = overlay.clone();
                tokio::spawn(async move {
                    while close_overlay_rx.recv().await.is_some() {
                        overlay.lock().await.close_overlay();
                    }
                });
            }

            // Hover price-check area tracking (`OVERLAY->MAIN::track-area`,
            // see `area_tracker.rs`) — same "spawn the background loop here,
            // wire it into `Overlay` right after" shape as the hotkey
            // listener above.
            let (tracker_tx, tracker_rx) = mpsc::unbounded_channel();
            tokio::spawn(area_tracker::watch(
                Arc::downgrade(&overlay),
                modifiers,
                tracker_rx,
            ));
            overlay.lock().await.attach_area_tracker(tracker_tx);

            // Compositor focus tracking: drives `MAIN->OVERLAY::focus-change`
            // (`game` field) — the renderer's own show/hide logic depends on
            // knowing whether PoE2 is the focused window. Reconnects on any
            // exit, including a clean EOF (Hyprland closing the socket
            // without an OS-level error, e.g. a compositor restart) — that
            // path returns `Ok(())`, not `Err`, and without this loop the
            // watcher would end silently, freezing `game_active` at its
            // last value for the rest of the process's life with no
            // diagnostic at all.
            let (focus_tx, mut focus_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                loop {
                    match compositor::watch_active_window(focus_tx.clone()).await {
                        Ok(()) => eprintln!(
                            "ee2-hyprland: focus watcher: Hyprland event socket closed, reconnecting in 2s"
                        ),
                        Err(err) => eprintln!(
                            "ee2-hyprland: focus watcher error: {err}, reconnecting in 2s"
                        ),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
            {
                let overlay = overlay.clone();
                tokio::spawn(async move {
                    while let Some(event) = focus_rx.recv().await {
                        match event {
                            compositor::WatchEvent::ActiveWindow(class, _title) => {
                                overlay.lock().await.set_game_active(&class).await
                            }
                            compositor::WatchEvent::WindowOpened(class) => {
                                overlay.lock().await.on_window_opened(&class)
                            }
                            compositor::WatchEvent::LayoutChanged => {
                                // Queried outside the Overlay lock; on error the
                                // previous placement is kept.
                                match compositor::game_origin().await {
                                    Ok(origin) => overlay.lock().await.set_game_placement(origin),
                                    Err(err) => eprintln!(
                                        "ee2-hyprland: game placement query failed: {err}"
                                    ),
                                }
                            }
                        }
                    }
                });
            }

            // Keep the runtime alive so the spawned tasks keep running.
            std::future::pending::<()>().await;
        });
    });

    let addr = match addr_rx.recv() {
        Ok(addr) => addr,
        Err(_) => {
            eprintln!("ee2-hyprland: server failed to start — exiting");
            return gtk4::glib::ExitCode::FAILURE;
        }
    };
    let url = format!("http://{addr}/");

    let app = gtk4::Application::builder()
        .application_id("io.github.lx_btw.ee2_hyprland")
        .build();

    app.connect_activate(move |app| {
        let webview = overlay::webview::build(&url);
        let window = overlay::window::build(app, &webview);
        state::register_window(window.clone());
        window.present();
        // Start hidden — `present()` above only exists to force `realize`
        // (so `window.rs`'s `connect_realize` sets up passthrough input
        // region + `KeyboardMode::None` right away). The surface only
        // becomes visible on the renderer's own first `render-state`
        // event (`state.rs::handle_ws_event`) or an interactive action.
        //
        // Without this, a renderer that fails to mount for *any* reason —
        // a network hiccup, an `http_proxy` env var intercepting our own
        // loopback asset requests (why `exempt_loopback_from_proxy` exists),
        // a crash — leaves a full-monitor surface mapped with nothing to
        // hide it. Any rendering problem (e.g. an opaque theme background,
        // see `overlay/window.rs`) then covers the whole screen with no way
        // to tell what's clickable underneath — it has happened, and needed
        // a blind logout to recover.
        window.set_visible(false);
    });

    app.run_with_args::<&str>(&[])
}

/// Adds `127.0.0.1` and `localhost` to `NO_PROXY`/`no_proxy`, keeping
/// whatever is already there. With a global `http_proxy` and no exemption,
/// WebKit's network process routes the WebView's requests to our own
/// loopback server (`server.rs`) through the proxy and the renderer never
/// mounts (HYPRLAND-NOTES.md §10). Done here rather than only in the
/// systemd unit so a direct launch (manual run, AppImage) works too.
/// Outbound trade-site requests (`proxy.rs`) still go through the proxy.
fn exempt_loopback_from_proxy() {
    for var in ["NO_PROXY", "no_proxy"] {
        let current = std::env::var(var).ok();
        let merged = with_loopback_exempt(current.as_deref());
        // `set_var` is a safe fn in edition 2021, but it races with any
        // concurrent env read (getenv in C code included). This runs first
        // thing in `main`, before the tokio thread, GTK or WebKit exist, so
        // the process is still single-threaded. Keep it that way.
        std::env::set_var(var, merged);
    }
}

/// Appends the loopback entries missing from a comma-separated no-proxy
/// list. Existing entries are kept verbatim; comparison is on trimmed
/// entries, case-insensitively (hostnames).
fn with_loopback_exempt(current: Option<&str>) -> String {
    let current = current.unwrap_or("").trim();
    let mut out = current.to_owned();
    for host in ["127.0.0.1", "localhost"] {
        let present = current
            .split(',')
            .any(|entry| entry.trim().eq_ignore_ascii_case(host));
        if !present {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(host);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::with_loopback_exempt;

    #[test]
    fn unset_or_empty() {
        assert_eq!(with_loopback_exempt(None), "127.0.0.1,localhost");
        assert_eq!(with_loopback_exempt(Some("")), "127.0.0.1,localhost");
        assert_eq!(with_loopback_exempt(Some("  ")), "127.0.0.1,localhost");
    }

    #[test]
    fn keeps_unrelated_entries() {
        assert_eq!(
            with_loopback_exempt(Some(".corp.example,10.0.0.0/8")),
            ".corp.example,10.0.0.0/8,127.0.0.1,localhost"
        );
    }

    #[test]
    fn already_present() {
        assert_eq!(
            with_loopback_exempt(Some("localhost,127.0.0.1")),
            "localhost,127.0.0.1"
        );
        assert_eq!(
            with_loopback_exempt(Some("example.com,localhost")),
            "example.com,localhost,127.0.0.1"
        );
    }

    #[test]
    fn whitespace_around_entries() {
        assert_eq!(
            with_loopback_exempt(Some(" example.com , 127.0.0.1 ")),
            "example.com , 127.0.0.1,localhost"
        );
        assert_eq!(
            with_loopback_exempt(Some("LOCALHOST")),
            "LOCALHOST,127.0.0.1"
        );
    }
}
