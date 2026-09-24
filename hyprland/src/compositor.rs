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
/// (`address`, `pid`, `at`, `size`, `mapped`, `floating`, `fullscreen`,
/// ...); unknown fields are ignored by default rather than rejected, so a
/// Hyprland version with extra fields still parses fine. Add fields back
/// here if a later stage needs them (monitor geometry for overlay
/// placement, say) rather than carrying them unused from the start.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub class: String,
    pub title: String,
    pub xwayland: bool,
}

#[derive(Debug, Deserialize)]
struct CursorPos {
    x: i32,
    y: i32,
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

/// Finds the game window among `clients` — `clients().await` filtered by
/// `class == game_class()`, split out so callers that already have a client
/// list (e.g. from a fresher `clients()` call) don't need a second query.
pub fn find_game(clients: &[Client]) -> Option<&Client> {
    let class = game_class();
    clients.iter().find(|c| c.class == class)
}

/// Subscribes to `.socket2.sock` and sends `(class, title)` on `tx` every
/// time the focused window changes. Resolves and sends the *current* focus
/// first — `activewindow` events fire only on change, so without this the
/// caller's idea of focus would be wrong until the first alt-tab (the same
/// gap `host.mjs::watchHyprland()` closes the same way).
pub async fn watch_active_window(tx: mpsc::UnboundedSender<(String, String)>) -> io::Result<()> {
    let dir = socket_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HYPRLAND_INSTANCE_SIGNATURE not set — not running under Hyprland",
        )
    })?;

    match active_window().await {
        Ok(Some(client)) => {
            let _ = tx.send((client.class, client.title));
        }
        Ok(None) => {}
        Err(err) => eprintln!("ee2-hyprland: initial active_window() query failed: {err}"),
    }

    let mut stream = UnixStream::connect(dir.join(".socket2.sock")).await?;
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
                if tx.send((class.to_string(), title.to_string())).is_err() {
                    return Ok(());
                }
            }
        }
    }
}
