//! System tray icon via `ksni` (StatusNotifierItem). Replaces upstream's
//! `main/src/AppTray.ts` / the prior prototype's `overlay.py::build_tray()`
//! (Python/AppIndicator3) — same menu shape, one deliberate improvement
//! already validated by the prior prototype: upstream's "Settings/League"
//! item just shows a dialog telling the user to press the overlay hotkey
//! in-game; both the prior prototype and this fork can just toggle the
//! overlay directly instead, since neither needs Electron's separate-window
//! model to do it.
//!
//! Stage 5 (see PLAN.md).

use std::path::PathBuf;
use std::sync::Arc;

use ksni::menu::StandardItem;
use ksni::{MenuItem, TrayMethods};
use tokio::sync::Mutex as TokioMutex;

use crate::state::Overlay;

pub struct AppTray {
    overlay: Arc<TokioMutex<Overlay>>,
    server_url: String,
    config_dir: PathBuf,
}

impl ksni::Tray for AppTray {
    fn id(&self) -> String {
        "io.github.lx_btw.ee2_hyprland".into()
    }

    fn title(&self) -> String {
        "Exiled Exchange 2".into()
    }

    fn icon_name(&self) -> String {
        // A generic system icon, not the app's own bundled one (embedded
        // in `renderer/dist/icon.png`) — ksni wants raw decoded ARGB32
        // pixels (`Icon { width, height, data }`), not PNG bytes, and
        // nothing in this fork currently does that decode step (`gdk4`'s
        // `gdk_pixbuf`, already a transitive dependency via `gtk4`, could —
        // left as future polish, doesn't affect the tray's actual function).
        "applications-games".to_string()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Toggle Overlay".into(),
                activate: Box::new(|tray: &mut Self| {
                    let overlay = tray.overlay.clone();
                    tokio::spawn(async move {
                        overlay.lock().await.toggle_overlay();
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open in Browser".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Err(err) = std::process::Command::new("xdg-open")
                        .arg(&tray.server_url)
                        .spawn()
                    {
                        eprintln!("ee2-hyprland: tray: failed to launch xdg-open: {err}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Config Folder".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Err(err) = std::process::Command::new("xdg-open")
                        .arg(&tray.config_dir)
                        .spawn()
                    {
                        eprintln!("ee2-hyprland: tray: failed to launch xdg-open: {err}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                // Same as CLIENT->MAIN::user-action{action:"quit"} —
                // immediate process exit, not a graceful GTK shutdown (see
                // state.rs's own handler for why that's an accepted,
                // documented scope limit for now).
                activate: Box::new(|_tray: &mut Self| {
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawns the tray as a background D-Bus service task. A missing/broken
/// StatusNotifierWatcher (no D-Bus session, no tray host running) is logged
/// and otherwise ignored — the tray is a convenience, not something the
/// rest of the app depends on to function.
pub async fn start(overlay: Arc<TokioMutex<Overlay>>, server_url: String) {
    let tray = AppTray {
        overlay,
        server_url,
        config_dir: crate::config::config_dir(),
    };
    if let Err(err) = tray.spawn().await {
        eprintln!("ee2-hyprland: tray: failed to start: {err}");
    }
}
