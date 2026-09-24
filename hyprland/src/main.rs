//! Entrypoint for the Hyprland-native platform layer.
//!
//! See `PLAN.md` (repo root) for the implementation stages and
//! `HYPRLAND-NOTES.md` for the design rationale behind each module below.
//! Stage 1: WebKit spike (layer-shell overlay + WebView). Stage 2: Hyprland
//! IPC, evdev hotkeys, uinput injection. Stage 3: event-driven clipboard
//! read-back. Stage 4 (this wiring): the full `ipc/types.ts` protocol
//! server (`server.rs`) and the state machine tying everything together
//! (`state.rs`) — hotkeys are no longer hardcoded to a single test combo;
//! they come from whatever `HostConfig.shortcuts` the renderer pushes over
//! WS once it mounts (`OverlayWindow.vue`'s `pushHostConfig()`, sent
//! automatically on mount, no user action needed), and can change live if
//! the user edits a hotkey in Settings.

mod clipboard;
mod compositor;
mod config;
mod input;
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
    // GTK owns the main thread's event loop (`app.run_with_args` below), so
    // everything else — server, hotkeys, compositor focus tracking — runs
    // on its own tokio runtime on a background OS thread instead of
    // sharing this one.
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(async {
            let (events_tx, _events_rx) = broadcast::channel(32);

            let (overlay, hotkey_bindings, hotkeys_active) = match state::Overlay::new(events_tx.clone()) {
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

            tokio::spawn(tray::start(overlay.clone(), format!("http://{addr}/")));

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
            // reintroduce the self-triggering feedback loop PLAN.md
            // documents as an already-happened real bug (Stage 2 review) —
            // keep `Overlay::new()` (or anything else that constructs a
            // `VirtualKeyboard`) strictly before this line.
            let (hotkey_tx, mut hotkey_rx) = mpsc::unbounded_channel();
            tokio::spawn(input::hotkeys::watch(hotkey_bindings, hotkeys_active, hotkey_tx));
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
                    while let Some((class, _title)) = focus_rx.recv().await {
                        overlay.lock().await.set_game_active(&class);
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
        // a network hiccup, a proxy env var intercepting our own loopback
        // asset requests (a real, previously-flagged risk: see PLAN.md
        // Stage 1's `NO_PROXY` note), a crash — leaves this surface
        // sitting fully visible with nothing to hide it, and the still-
        // open Stage 1 WebView-transparency bug means that's an opaque
        // backdrop covering the entire screen, indefinitely, with no
        // visual way to tell what's clickable underneath. This happened
        // for real during Stage 5 manual testing (2026-09-24) and forced
        // a blind logout — see PLAN.md's Stage 5 entry.
        window.set_visible(false);
    });

    app.run_with_args::<&str>(&[])
}
