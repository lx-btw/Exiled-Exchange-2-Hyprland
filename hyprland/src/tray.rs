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

/// Same file `packaging/PKGBUILD` installs as the 512x512 hicolor icon.
const ICON_PNG: &[u8] = include_bytes!("../../main/build/icons/512x512.png");
const ICON_NAME: &str = "ee2-hyprland";
const ICON_SIZES: [i32; 5] = [16, 22, 32, 48, 64];

/// Decodes the embedded PNG with gdk-pixbuf and renders it at each of
/// `ICON_SIZES`, in SNI's `IconPixmap` format (ARGB32, network byte order,
/// i.e. bytes A,R,G,B per pixel).
fn load_icons() -> Vec<ksni::Icon> {
    use gtk4::gdk_pixbuf::{InterpType, PixbufLoader};
    use gtk4::prelude::*;

    let loader = PixbufLoader::new();
    if let Err(err) = loader.write(ICON_PNG).and_then(|()| loader.close()) {
        eprintln!("ee2-hyprland: tray: failed to decode icon: {err}");
        return Vec::new();
    }
    let Some(full) = loader.pixbuf() else {
        return Vec::new();
    };
    ICON_SIZES
        .iter()
        .filter_map(|&size| {
            let scaled = full.scale_simple(size, size, InterpType::Hyper)?;
            Some(ksni::Icon {
                width: size,
                height: size,
                data: rgba_to_argb(
                    &scaled.read_pixel_bytes(),
                    scaled.rowstride() as usize,
                    size as usize,
                    scaled.n_channels() as usize,
                ),
            })
        })
        .collect()
}

/// `pixels`: `size` rows of `rowstride` bytes, `channels` (3 = RGB, 4 =
/// RGBA) bytes per pixel.
fn rgba_to_argb(pixels: &[u8], rowstride: usize, size: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * size * 4);
    for row in pixels.chunks(rowstride).take(size) {
        for px in row.chunks(channels).take(size) {
            let alpha = if channels == 4 { px[3] } else { 255 };
            out.extend_from_slice(&[alpha, px[0], px[1], px[2]]);
        }
    }
    out
}

pub struct AppTray {
    icons: Vec<ksni::Icon>,
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
        // The themed name the package installs under hicolor
        // (`packaging/PKGBUILD`); `icon_pixmap` below is what's shown when
        // the theme has no such icon (no package installed, or a theme
        // that doesn't fall back to hicolor).
        ICON_NAME.to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
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
        icons: load_icons(),
        overlay,
        server_url,
        config_dir: crate::config::config_dir(),
    };
    if let Err(err) = tray.spawn().await {
        eprintln!("ee2-hyprland: tray: failed to start: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_becomes_argb_and_respects_rowstride() {
        // 2x2 RGBA with 2 padding bytes per row.
        let px = [
            1, 2, 3, 4, 5, 6, 7, 8, 0, 0, //
            9, 10, 11, 12, 13, 14, 15, 16, 0, 0,
        ];
        assert_eq!(
            rgba_to_argb(&px, 10, 2, 4),
            [4, 1, 2, 3, 8, 5, 6, 7, 12, 9, 10, 11, 16, 13, 14, 15]
        );
    }

    #[test]
    fn embedded_icon_decodes_at_every_size() {
        let icons = load_icons();
        assert_eq!(icons.len(), ICON_SIZES.len());
        for icon in icons {
            assert_eq!(icon.data.len() as i32, icon.width * icon.height * 4);
            assert!(icon.data.chunks(4).any(|p| p[0] != 0));
        }
    }
}
