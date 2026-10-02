//! Hyprland IPC client: `.socket.sock` (commands) and `.socket2.sock` (event
//! stream), reached via `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`.
//!
//! Replaces every `hyprctl` subprocess spawn the prior prototype used
//! (`clients`, `cursorpos`, `activewindow`, `monitors`) with direct unix-socket
//! I/O. Game-window lookup (by class), cursor position, monitor geometry, and
//! the `activewindow` event subscription port from the prior `host.mjs`'s
//! `findGame()`, `cursorPos()`, and `watchHyprland()` — same wire format,
//! verified live against the socket on this system while porting: the
//! command socket takes a `j/<command>` request and closes the connection
//! after writing its JSON reply (so a full read is "read until EOF", not a
//! framed protocol); the event socket pushes newline-delimited
//! `EVENTNAME>>data\n` lines with no request needed.
//!
//! Also the source of truth for a client's `xwayland` flag, used by
//! `clipboard.rs` to decide which selection (X11 or native Wayland) to read.
//!
//! Stage 2 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §7.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Every request/response IPC call gets this long to complete before it's
/// treated as failed — Hyprland's command socket is normally sub-millisecond
/// local IPC, so anything near this is already wrong; without a bound, a
/// stalled or crashed compositor handler hangs the caller forever with no
/// recovery short of killing the process.
const IPC_TIMEOUT: Duration = Duration::from_secs(5);

/// The window `class` Hyprland reports for Path of Exile 2. Corrected live
/// (2026-09-24, Stage 5): the original guess, `steam_app_2694490` (the
/// Steam App ID, guessed by analogy with Steam's native-Linux class-naming
/// convention), does not match this system's actual class — confirmed via
/// `hyprctl clients -j` against a real running instance
/// (Steam+Proton, `PROTON_ENABLE_WAYLAND=1`, native Wayland client, not
/// Xwayland) reporting class `pathofexilesteam.exe`, i.e. Proton's own
/// `basename.exe`-style naming, not Steam's `steam_app_<id>` wrapper class.
/// `find_game` returned nothing against the old default on a real game
/// window every single time, silently — see `EE2_GAME_CLASS`'s own escape
/// hatch below, which is how this was diagnosed without a rebuild-per-guess
/// cycle. Overridable for other installs/launch methods, or for testing
/// against a stand-in window — same escape hatch the prior prototype's
/// `host.mjs` had.
pub fn game_class() -> String {
    std::env::var("EE2_GAME_CLASS").unwrap_or_else(|_| "pathofexilesteam.exe".to_string())
}

fn socket_dir() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    Some(PathBuf::from(runtime_dir).join("hypr").join(sig))
}

/// A client (window), as reported by `clients`/`activewindow`. Only the
/// fields this fork actually uses — Hyprland's real schema has many more
/// (`address`, `pid`, `mapped`, `floating`, `fullscreen`, ...); unknown
/// fields are ignored by default rather than rejected, so a Hyprland
/// version with extra fields still parses fine.
///
/// `at`/`size` (Phase C, PLAN.md, `stashScroll`'s `isStashArea` port) are
/// already in Hyprland's global layout space — the same frame
/// `cursor_pos()` reports in, confirmed live against a real window (`at:
/// [0, 155]` for a window on `DP-1`, whose real origin is `y: 155` — see
/// `overlay::window::current_monitor_origin`'s docs for the same
/// monitor-origin fact from the renderer side). No conversion needed to
/// compare the two directly, unlike `track-area`'s renderer-frame
/// coordinates.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub class: String,
    pub title: String,
    pub xwayland: bool,
    pub at: (i32, i32),
    pub size: (i32, i32),
    /// Visibility inputs (`game_origin`). Defaulted so a reply missing them
    /// (e.g. `activewindow`) still parses.
    #[serde(default)]
    pub workspace: WorkspaceRef,
    #[serde(default)]
    pub monitor: i32,
    #[serde(default)]
    pub mapped: bool,
    #[serde(default)]
    pub hidden: bool,
}

/// `{"id": .., "name": ..}` — only the id matters. Special workspaces have
/// negative ids.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
}

/// One entry of `j/monitors` — the fields needed to tell which workspace a
/// monitor currently shows, and where it sits in the global layout.
#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub id: i32,
    pub x: i32,
    pub y: i32,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WorkspaceRef,
    #[serde(rename = "specialWorkspace")]
    pub special_workspace: WorkspaceRef,
}

#[derive(Debug, Deserialize)]
struct CursorPos {
    x: i32,
    y: i32,
}

/// One entry of `hyprctl -j devices`'s `keyboards` array — only the fields
/// `input/grab.rs`'s per-device XKB layout sync actually needs (see
/// HYPRLAND-NOTES.md §9). `name` is Hyprland's own mangled device name
/// (lowercased, non-alphanumeric runs collapsed to `-`), not
/// `evdev::Device::name()`'s raw string — `grab.rs::hypr_device_name()`
/// reproduces the transform.
#[derive(Debug, Clone, Deserialize)]
pub struct KeyboardDevice {
    pub name: String,
    pub active_layout_index: i32,
}

#[derive(Deserialize)]
struct DevicesReply {
    keyboards: Vec<KeyboardDevice>,
}

/// Sends `command` (e.g. `"j/clients"` — the `j/` prefix requests JSON) to
/// `.socket.sock` and returns the full reply. Hyprland closes the connection
/// once it's written the response, so "read to EOF" is the whole protocol.
async fn command(command: &str) -> io::Result<String> {
    match tokio::time::timeout(IPC_TIMEOUT, command_inner(command)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Hyprland IPC command timed out",
        )),
    }
}

async fn command_inner(command: &str) -> io::Result<String> {
    let dir = socket_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HYPRLAND_INSTANCE_SIGNATURE not set — not running under Hyprland",
        )
    })?;

    let mut stream = UnixStream::connect(dir.join(".socket.sock")).await?;
    stream.write_all(command.as_bytes()).await?;
    stream.shutdown().await?;

    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

pub async fn monitors() -> io::Result<Vec<Monitor>> {
    let raw = command("j/monitors").await?;
    serde_json::from_str(&raw).map_err(io::Error::other)
}

/// Where the game window is *on screen right now*: the origin of the monitor
/// that currently displays it, or `None` if it isn't displayed (no game
/// window, unmapped/hidden, its workspace isn't the active one on its
/// monitor, or a special workspace is covering it). A layer surface belongs
/// to a monitor, not a workspace, so this is what the overlay's visibility
/// has to follow (ports `electron-overlay-window`'s "overlay is bound to the
/// game window").
pub async fn game_origin() -> io::Result<Option<(i32, i32)>> {
    let (clients, monitors) = tokio::join!(clients(), monitors());
    Ok(visible_game_origin(&clients?, &monitors?))
}

fn visible_game_origin(clients: &[Client], monitors: &[Monitor]) -> Option<(i32, i32)> {
    let class = game_class();
    clients
        .iter()
        .filter(|c| c.class == class && c.mapped && !c.hidden)
        .find_map(|game| {
            let ws = game.workspace.id;
            let monitor = if ws < 0 {
                // On a special workspace: shown iff some monitor has exactly
                // that special workspace open.
                monitors.iter().find(|m| m.special_workspace.id == ws)
            } else {
                // A regular workspace is covered while a special one is open
                // on the same monitor.
                monitors.iter().find(|m| {
                    m.id == game.monitor
                        && m.active_workspace.id == ws
                        && m.special_workspace.id == 0
                })
            };
            monitor.map(|m| (m.x, m.y))
        })
}

pub async fn clients() -> io::Result<Vec<Client>> {
    let raw = command("j/clients").await?;
    serde_json::from_str(&raw).map_err(io::Error::other)
}

/// The currently focused client, or `None` if nothing is focused (Hyprland
/// replies literal `{}` in that case). Checked explicitly rather than
/// treating any deserialize failure as "nothing focused" — a genuine schema
/// mismatch or corrupt reply should surface as an `Err`, not silently masquerade
/// as no window being focused (Stage 4's overlay show/hide logic depends on
/// this distinction: silently wrong "nothing focused" answers would hide the
/// overlay or lose game-window tracking with no diagnostic at all).
pub async fn active_window() -> io::Result<Option<Client>> {
    let raw = command("j/activewindow").await?;
    if raw.trim() == "{}" {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(io::Error::other)
}

pub async fn cursor_pos() -> io::Result<(i32, i32)> {
    let raw = command("j/cursorpos").await?;
    let pos: CursorPos = serde_json::from_str(&raw).map_err(io::Error::other)?;
    Ok((pos.x, pos.y))
}

/// The `keyboards` array from `hyprctl -j devices` — used by
/// `input/grab.rs` to read a physical device's current XKB layout index
/// before grabbing it (so the mirror device can be set to match) and the
/// mirror's own index before ungrabbing (so the physical device gets back
/// whatever the user toggled to while it was grabbed). See
/// HYPRLAND-NOTES.md §9.
pub async fn keyboards() -> io::Result<Vec<KeyboardDevice>> {
    let raw = command("j/devices").await?;
    let reply: DevicesReply = serde_json::from_str(&raw).map_err(io::Error::other)?;
    Ok(reply.keyboards)
}

/// Sets `device`'s active XKB layout to `index` (0-based into its
/// `layout`/`options` list — `"us,ru"` here, so `0` is `us`, `1` is `ru`).
///
/// **Empirically confirmed to require the short-form command, not
/// `dispatch switchxkblayout ...`** (2026-09-24, see HYPRLAND-NOTES.md §9):
/// this system's Hyprland config loads a Lua scripting layer (the same one
/// §3 references via `hl.bind`/`hl.dsp`) that intercepts the `dispatch`
/// socket command and tries to parse its argument as Lua source — `dispatch
/// switchxkblayout <name> <index>` fails there with a Lua syntax error. The
/// short-form command (`switchxkblayout <name> <index>`, no `dispatch`
/// prefix — what `hyprctl`'s own CLI sends for a handful of common
/// dispatchers) bypasses that layer entirely. Verified live against a real
/// device: `switchxkblayout steelseries-steelseries-apex-7-tkl 0` returned
/// `ok`, confirmed by a follow-up `j/devices` read showing
/// `active_layout_index: 0`.
pub async fn switchxkblayout(device: &str, index: i32) -> io::Result<()> {
    command(&format!("switchxkblayout {device} {index}")).await?;
    Ok(())
}

/// Finds the game window among `clients` — `clients().await` filtered by
/// `class == game_class()`, split out so callers that already have a client
/// list (e.g. from a fresher `clients()` call) don't need a second query.
pub fn find_game(clients: &[Client]) -> Option<&Client> {
    let class = game_class();
    clients.iter().find(|c| c.class == class)
}

/// What `watch_active_window` reports.
#[derive(Debug)]
pub enum WatchEvent {
    /// The focused window changed (or, first, the current focus): `(class, title)`.
    ActiveWindow(String, String),
    /// A new window was mapped (`openwindow>>ADDRESS,WORKSPACE,CLASS,TITLE`);
    /// carries its class. Lets `state.rs` tell a fresh game window from
    /// mere re-focus of an existing one (welcome banner, once per window).
    WindowOpened(String),
    /// Something that can change which workspace/monitor shows the game
    /// window happened (workspace switch, monitor focus, special workspace,
    /// window moved/opened/closed, monitor added/removed) — the receiver
    /// re-queries `game_origin()`. Event-driven only, no polling.
    LayoutChanged,
}

/// Subscribes to `.socket2.sock` and sends [`WatchEvent`]s on `tx` every
/// time the focused window changes or a window opens. Resolves and sends the *current* focus
/// first — `activewindow` events fire only on change, so without this the
/// caller's idea of focus would be wrong until the first alt-tab (the same
/// gap `host.mjs::watchHyprland()` closes the same way).
pub async fn watch_active_window(tx: mpsc::UnboundedSender<WatchEvent>) -> io::Result<()> {
    let dir = socket_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HYPRLAND_INSTANCE_SIGNATURE not set — not running under Hyprland",
        )
    })?;

    let mut stream = UnixStream::connect(dir.join(".socket2.sock")).await?;

    // Connected before the initial queries so no event is missed in between.
    // Layout first: the focus handler may start the welcome banner, which
    // needs to know the game is on screen.
    let _ = tx.send(WatchEvent::LayoutChanged);
    match active_window().await {
        Ok(Some(client)) => {
            let _ = tx.send(WatchEvent::ActiveWindow(client.class, client.title));
        }
        Ok(None) => {}
        Err(err) => eprintln!("ee2-hyprland: initial active_window() query failed: {err}"),
    }

    // Bytes, not a String: decoding each raw read() chunk independently
    // would split a multi-byte UTF-8 character that happens to straddle a
    // 4096-byte read boundary, corrupting non-ASCII window classes/titles
    // (this system's own window titles include Cyrillic text). `\n` (0x0A)
    // never appears as a UTF-8 continuation byte, so scanning for it in the
    // raw bytes before decoding is always safe — decode only once a
    // complete line's bytes are assembled.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);

        while let Some(newline) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);

            let Some((event, data)) = line.split_once(">>") else {
                continue;
            };
            // `activewindowv2>>ADDRESS` also exists but carries strictly
            // less information than `activewindow>>class,title` for our
            // purposes, so it's ignored — same as the prior prototype.
            if event == "activewindow" {
                let Some((class, title)) = data.split_once(',') else {
                    continue;
                };
                if tx
                    .send(WatchEvent::ActiveWindow(
                        class.to_string(),
                        title.to_string(),
                    ))
                    .is_err()
                {
                    return Ok(());
                }
            } else if matches!(
                event,
                "workspacev2"
                    | "focusedmonv2"
                    | "activespecialv2"
                    | "movewindowv2"
                    | "moveworkspacev2"
                    | "closewindow"
                    | "monitoraddedv2"
                    | "monitorremovedv2"
            ) {
                if tx.send(WatchEvent::LayoutChanged).is_err() {
                    return Ok(());
                }
            } else if event == "openwindow" {
                if tx.send(WatchEvent::LayoutChanged).is_err() {
                    return Ok(());
                }
                // ADDRESS,WORKSPACE,CLASS,TITLE — class is the third field;
                // commas in the title only add fields after it.
                let Some(class) = data.split(',').nth(2) else {
                    continue;
                };
                if tx
                    .send(WatchEvent::WindowOpened(class.to_string()))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(ws: i64, monitor: i32) -> Client {
        Client {
            class: game_class(),
            title: String::new(),
            xwayland: false,
            at: (0, 0),
            size: (0, 0),
            workspace: WorkspaceRef { id: ws },
            monitor,
            mapped: true,
            hidden: false,
        }
    }

    fn monitor(id: i32, x: i32, active: i64, special: i64) -> Monitor {
        Monitor {
            id,
            x,
            y: 0,
            active_workspace: WorkspaceRef { id: active },
            special_workspace: WorkspaceRef { id: special },
        }
    }

    #[test]
    fn visible_only_on_the_active_workspace_of_its_monitor() {
        let monitors = [monitor(0, 0, 1, 0), monitor(1, 2560, 5, 0)];
        assert_eq!(
            visible_game_origin(&[client(1, 0)], &monitors),
            Some((0, 0))
        );
        assert_eq!(visible_game_origin(&[client(2, 0)], &monitors), None);
        assert_eq!(
            visible_game_origin(&[client(5, 1)], &monitors),
            Some((2560, 0))
        );
        // Workspace 1 active on monitor 0 doesn't make a game on monitor 1 visible.
        assert_eq!(visible_game_origin(&[client(1, 1)], &monitors), None);
        assert_eq!(visible_game_origin(&[], &monitors), None);
    }

    #[test]
    fn special_workspace_covers_a_regular_one() {
        let covered = [monitor(0, 0, 1, -99)];
        assert_eq!(visible_game_origin(&[client(1, 0)], &covered), None);
        // ...and a game on that special workspace is the visible one.
        assert_eq!(
            visible_game_origin(&[client(-99, 0)], &covered),
            Some((0, 0))
        );
        assert_eq!(visible_game_origin(&[client(-98, 0)], &covered), None);
    }

    #[test]
    fn unmapped_or_hidden_game_is_not_visible() {
        let monitors = [monitor(0, 0, 1, 0)];
        let mut c = client(1, 0);
        c.mapped = false;
        assert_eq!(visible_game_origin(&[c], &monitors), None);
        let mut c = client(1, 0);
        c.hidden = true;
        assert_eq!(visible_game_origin(&[c], &monitors), None);
    }
}
