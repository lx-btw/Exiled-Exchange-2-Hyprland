//! Host log → renderer's Settings log viewer. Ports `main/src/RemoteLogger.ts`:
//! every message is timestamped, appended to an in-memory history and
//! broadcast as `MAIN->CLIENT::log-entry`; a freshly-connected client gets
//! the whole history (`server.rs::handle_socket`, same as upstream's
//! `sendEventTo("last-active", ...)` on connection).
//!
//! The one logging layer for the process: `ee_log!` also writes the line to
//! stderr (journal), so a call site never needs both an `eprintln!` and a
//! separate remote write. Messages use upstream's convention — a level word
//! first (`info`/`warn`/`error`), then a `[Component]` tag.
//!
//! Deviation from upstream: `history` is capped (`HISTORY_CAP`), trimmed from
//! the front at a line boundary — upstream's grows without bound.

use std::sync::{Mutex, OnceLock};

use serde_json::json;
use tokio::sync::broadcast;

const HISTORY_CAP: usize = 256 * 1024;

struct Logger {
    history: Mutex<String>,
    events_tx: broadcast::Sender<serde_json::Value>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Call once, before anything logs through `ee_log!` remotely (before that,
/// messages only reach stderr).
pub fn init(events_tx: broadcast::Sender<serde_json::Value>) {
    let _ = LOGGER.set(Logger {
        history: Mutex::new(String::new()),
        events_tx,
    });
}

pub fn write(message: &str) {
    eprintln!("ee2-hyprland: {message}");
    let Some(logger) = LOGGER.get() else { return };
    let time = gtk4::glib::DateTime::now_local()
        .ok()
        .and_then(|t| t.format("%H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let line = format!("[{time}] {message}\n");
    {
        let mut history = logger.history.lock().unwrap();
        history.push_str(&line);
        if history.len() > HISTORY_CAP {
            let mut cut = history.len() - HISTORY_CAP;
            while !history.is_char_boundary(cut) {
                cut += 1;
            }
            // drop through the end of the (probably partial) first line
            let cut = history[cut..].find('\n').map_or(cut, |i| cut + i + 1);
            history.drain(..cut);
        }
    }
    // No receivers yet is normal.
    let _ = logger.events_tx.send(json!({
        "name": "MAIN->CLIENT::log-entry",
        "payload": {"message": line},
    }));
}

/// The `log-entry` event for a newly connected client, `None` while empty.
pub fn history_event() -> Option<serde_json::Value> {
    let history = LOGGER.get()?.history.lock().unwrap();
    if history.is_empty() {
        return None;
    }
    Some(json!({
        "name": "MAIN->CLIENT::log-entry",
        "payload": {"message": history.as_str()},
    }))
}

macro_rules! ee_log {
    ($($arg:tt)*) => { $crate::logger::write(&format!($($arg)*)) };
}
pub(crate) use ee_log;
