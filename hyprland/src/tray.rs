//! System tray icon via `ksni` (StatusNotifierItem). Replaces upstream's
//! `main/src/AppTray.ts` / the prior prototype's `overlay.py::build_tray()`
//! (Python/AppIndicator3) — same menu shape, one deliberate improvement
//! already validated by the prior prototype: upstream's "Settings/League"
//! item just shows a dialog telling the user to press the overlay hotkey
//! in-game; here it is a desktop notification (`org.freedesktop.Notifications`
//! via gio's D-Bus API) with the same text. Like upstream, the tray never
//! opens the overlay itself — since Session 18 the surface only exists
//! while the game is on screen, so "Open in Browser" is the way to reach
//! the settings without the game.
//!
//! Stage 5 (see PLAN.md).

use std::path::PathBuf;

use ksni::menu::StandardItem;
use ksni::{MenuItem, TrayMethods};

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
                label: "Settings/League".into(),
                activate: Box::new(|_tray: &mut Self| {
                    tokio::spawn(async {
                        let key = crate::config::overlay_key().await;
                        let result =
                            tokio::task::spawn_blocking(move || notify_settings_hint(&key)).await;
                        if let Ok(Err(err)) = result {
                            eprintln!("ee2-hyprland: tray: failed to show notification: {err}");
                        }
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
                label: "Open config folder".into(),
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

/// Same text as upstream's `AppTray.ts` dialog.
fn settings_hint(overlay_key: &str) -> String {
    format!(
        "Open Path of Exile 2 and press \"{overlay_key}\". Click on the button with cog icon there."
    )
}

/// `org.freedesktop.Notifications.Notify` over the session bus, through
/// gio's own D-Bus client (already linked via GTK) — no extra dependency.
///
/// Blocking calls (run via `spawn_blocking`): gio's futures aren't `Send`,
/// so they can't live on the tokio runtime.
fn notify_settings_hint(overlay_key: &str) -> Result<(), gtk4::glib::Error> {
    use std::collections::HashMap;

    use gtk4::gio::{self, prelude::*};
    use gtk4::glib::Variant;

    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
    let params = (
        "Exiled Exchange 2",
        0u32,
        ICON_NAME,
        "Settings",
        settings_hint(overlay_key).as_str(),
        Vec::<&str>::new(),
        HashMap::<&str, Variant>::new(),
        -1i32,
    )
        .to_variant();
    connection.call_sync(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        None::<&gio::Cancellable>,
    )?;
    Ok(())
}

/// Spawns the tray as a background D-Bus service task. A missing/broken
/// StatusNotifierWatcher (no D-Bus session, no tray host running) is logged
/// and otherwise ignored — the tray is a convenience, not something the
/// rest of the app depends on to function.
pub async fn start(server_url: String) {
    let tray = AppTray {
        icons: load_icons(),
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
    fn settings_hint_matches_upstream_text() {
        assert_eq!(
            settings_hint("Shift + Space"),
            "Open Path of Exile 2 and press \"Shift + Space\". Click on the button with cog icon there."
        );
    }

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
