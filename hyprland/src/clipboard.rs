//! Event-driven clipboard read via `zwlr_data_control_manager_v1`, with an
//! X11-selection fallback for a game running under Xwayland without
//! `PROTON_ENABLE_WAYLAND=1` (gated on `compositor.rs`'s per-client `xwayland`
//! flag — the same signal upstream's `host.mjs` used).
//!
//! **The actual bug being worked around**: XWM only mirrors the X11 and
//! Wayland clipboard selections into each other on *ownership* change, not
//! *content* change. Reading the selection the game itself owns — native
//! Wayland when it's a native/`PROTON_ENABLE_WAYLAND=1` client, X11 when it's
//! plain Xwayland — sidesteps that entirely, rather than trying to read
//! whichever side happens to be convenient.
//!
//! Improves on the prior prototype's polling approach for the native-Wayland
//! path: no subprocess spawn per poll tick, no `EE2_FOCUS_GRACE`-style grace
//! window. `zwlr_data_control_device_v1.selection` fires on every genuine
//! copy action a native client performs (a fresh offer object each time,
//! even for back-to-back copies of visually identical text) — there's no
//! stale-read race to paper over the way polling had, so no "clear the
//! selection first" step is needed either: the caller just drains whatever
//! was already buffered, triggers the copy, and awaits the next event.
//!
//! The X11 fallback stays a plain `xsel` subprocess call (matching
//! `host.mjs`), not the minimal-XCB-connection design HYPRLAND-NOTES.md
//! describes — that's a real scope reduction, not an oversight: this is the
//! secondary path (only reached when the game runs under Xwayland without
//! Wayland-native Proton), invoked once per price-check rather than per
//! keystroke, and X11 doesn't have as convenient an event-subscription
//! primitive as `zwlr_data_control` to build on. Revisit if the subprocess
//! spawn turns out to matter in practice.
//!
//! Stage 3 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §5.
//! Fallback if `wlr-data-control` misbehaves: revert to `wl-paste` polling
//! after clearing the selection (known to work, just slower) — keep this
//! module's public interface unchanged either way.

use std::io;
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1, EVT_DATA_OFFER_OPCODE},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

/// How long `receive()` waits for a source client to finish writing an
/// offered selection before giving up on that one read.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Item-text prefixes PoE2 clients emit at the start of copied item text, in
/// every language upstream supports — ported verbatim from upstream's
/// `main/src/shortcuts/HostClipboard.ts`. Used to confirm a clipboard read
/// is genuinely an item copy, not some unrelated selection change that
/// happened to land in the same window.
const ITEM_PREFIXES: &[&str] = &[
    "Item Class: ",
    "Rarity: ",
    "Класс предмета: ",
    "Редкость: ",
    "Classe d'objet: ",
    "Rareté: ",
    "Gegenstandsklasse: ",
    "Seltenheit: ",
    "Classe do Item: ",
    "Raridade: ",
    "Clase de objeto: ",
    "Rareza: ",
    "ชนิดไอเทม: ",
    "아이템 종류: ",
    "아이템 희귀도: ",
    "物品種類: ",
    "稀有度: ",
    "物品类别: ",
    "アイテムクラス: ",
    "レアリティ: ",
];

pub fn is_poe_item(text: &str) -> bool {
    ITEM_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

#[derive(Debug)]
pub enum ClipboardEvent {
    /// The native Wayland `CLIPBOARD` selection changed and was read as the
    /// given text (best-guess MIME type — see `pick_mime_type`).
    Selection(String),
}

struct State {
    tx: mpsc::UnboundedSender<ClipboardEvent>,
    current_offer: Option<ZwlrDataControlOfferV1>,
    // Set on `Event::Finished` — `run()`'s dispatch loop checks this after
    // every call and stops instead of continuing to call
    // `blocking_dispatch` on a device the compositor has already
    // invalidated, which would otherwise loop forever, never producing
    // another `Selection` event, with only the one `Finished` eprintln to
    // show it happened.
    finished: bool,
}

/// The offer's user-data: MIME types it has announced so far, filled in by
/// `Dispatch<ZwlrDataControlOfferV1, _>::event` as `Offer` events arrive,
/// consulted once the owning `selection` event finalizes which offer is
/// current.
type OfferMimeTypes = Mutex<Vec<String>>;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only dynamic add/remove of globals after the initial roundtrip
        // would show up here — the seat and data-control manager aren't
        // expected to appear or vanish during this process's lifetime.
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Capabilities/name don't matter — the seat object is only used to
        // create the data-control device.
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The manager interface has no events.
    }
}

impl Dispatch<ZwlrDataControlOfferV1, OfferMimeTypes> for State {
    fn event(
        _: &mut Self,
        _offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        mime_types: &OfferMimeTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            mime_types.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { .. } => {
                // MIME types accumulate on the new offer's own user-data via
                // Dispatch<ZwlrDataControlOfferV1, _> — nothing to do here.
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                // Per the protocol: destroy the *previous* offer now that a
                // new one (or none) has arrived — it's the new one we still
                // need to read.
                if let Some(previous) = state.current_offer.take() {
                    previous.destroy();
                }

                let Some(offer) = id else { return };

                let mime_types = offer
                    .data::<OfferMimeTypes>()
                    .map(|m| m.lock().unwrap().clone())
                    .unwrap_or_default();

                match pick_mime_type(&mime_types).and_then(|mime| receive(conn, &offer, mime).ok())
                {
                    Some(text) => {
                        let _ = state.tx.send(ClipboardEvent::Selection(text));
                    }
                    None => {
                        eprintln!(
                            "ee2-hyprland: clipboard selection offered no usable text MIME type"
                        );
                    }
                }

                state.current_offer = Some(offer);
            }
            // We only ever read the regular CLIPBOARD selection (same as
            // upstream's `xsel -b`/`wl-paste`, not `-p`/primary) — destroy
            // immediately so it doesn't leak.
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy();
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id: None } => {}
            zwlr_data_control_device_v1::Event::Finished => {
                eprintln!(
                    "ee2-hyprland: clipboard data-control device invalidated (seat removed?), stopping"
                );
                state.finished = true;
            }
            _ => {}
        }
    }

    event_created_child!(Self, ZwlrDataControlDeviceV1, [
        EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, Mutex::new(Vec::new())),
    ]);
}

/// Prefers an explicit UTF-8 plain-text MIME type, falls back to whatever
/// plain-text-ish type was actually offered — offer contents are
/// source-client-defined, so don't assume any particular one is present.
fn pick_mime_type(offered: &[String]) -> Option<&str> {
    const PREFERRED: &[&str] = &[
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "STRING",
        "TEXT",
    ];
    PREFERRED
        .iter()
        .find_map(|&want| {
            offered
                .iter()
                .find(|m| m.as_str() == want)
                .map(String::as_str)
        })
        .or_else(|| offered.first().map(String::as_str))
}

/// Requests `mime_type` from `offer` and reads it back through a pipe — the
/// standard Wayland data-transfer dance: we create the pipe, hand the
/// compositor our write end (which it passes to the source client), close
/// our copy of the write end so we see EOF once the source closes theirs,
/// and read the read end to completion.
fn receive(
    conn: &Connection,
    offer: &ZwlrDataControlOfferV1,
    mime_type: &str,
) -> io::Result<String> {
    let (mut reader, writer) = std::io::pipe()?;

    offer
        .send_request(zwlr_data_control_offer_v1::Request::Receive {
            mime_type: mime_type.to_string(),
            fd: writer.as_fd(),
        })
        .map_err(io::Error::other)?;
    // `send_request` only queues the message — the fd-bearing `sendmsg`
    // that actually duplicates our write end into the source client's
    // process doesn't necessarily happen until the connection is flushed
    // (or the next dispatch does it implicitly). Flush explicitly *before*
    // dropping our copy below: if we closed it while the message were still
    // sitting unsent in an internal buffer, there'd be nothing left to
    // duplicate.
    conn.flush().map_err(io::Error::other)?;
    // Our copy of the write end — without closing it, the read below would
    // block forever even after the source client closes *its* copy, since
    // the pipe only reaches EOF once every write-end reference is gone.
    drop(writer);

    // The read itself runs on a throwaway thread with a bound on how long
    // we wait for it: this whole function runs inside the clipboard
    // watcher's Dispatch::event() callback, on the one dedicated thread
    // that also processes every future selection change. A source client
    // that never finishes writing (hung, killed mid-write, adversarial)
    // would otherwise wedge that thread forever, silently ending clipboard
    // reads for the rest of the process's life. If this times out, the
    // spawned thread is abandoned rather than joined — an acceptable leak
    // for what should be a near-instant local read that never actually
    // times out in practice.
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let result = std::io::Read::read_to_string(&mut reader, &mut buf).map(|_| buf);
        let _ = result_tx.send(result);
    });

    result_rx
        .recv_timeout(RECEIVE_TIMEOUT)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard offer receive timed out"))?
}

/// Spawns the clipboard watcher on a dedicated OS thread. `wayland-client`'s
/// `Dispatch`-based event loop is a blocking, synchronous-callback design
/// that doesn't fit tokio without a custom event source — a dedicated
/// thread is simpler, and there's exactly one of these for the whole
/// process's lifetime.
pub fn spawn() -> io::Result<mpsc::UnboundedReceiver<ClipboardEvent>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("ee2-hyprland-clipboard".into())
        .spawn(move || {
            if let Err(err) = run(tx) {
                eprintln!("ee2-hyprland: clipboard watcher exited: {err}");
            }
        })?;
    Ok(rx)
}

fn run(tx: mpsc::UnboundedSender<ClipboardEvent>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let _device = manager.get_data_device(&seat, &qh, ());

    let mut state = State {
        tx,
        current_offer: None,
        finished: false,
    };

    while !state.finished {
        event_queue.blocking_dispatch(&mut state)?;
    }

    Err("data-control device invalidated (Finished event)".into())
}

/// Drops any events already buffered before a copy is triggered — without
/// this, the first `next_selection`/`read_item_text` call after startup
/// could return whatever was already on the clipboard (the protocol sends
/// one `selection` event immediately upon binding, reflecting pre-existing
/// content) instead of the fresh copy the caller is about to trigger.
pub fn drain_stale(rx: &mut mpsc::UnboundedReceiver<ClipboardEvent>) {
    while rx.try_recv().is_ok() {}
}

/// Reads the X11 `CLIPBOARD` selection via `xsel`, matching `host.mjs`'s
/// `viaX11` path exactly (including the `DISPLAY` fallback) — see module
/// docs for why this is a subprocess call rather than an XCB client.
async fn read_x11_selection() -> io::Result<String> {
    // `:1`, not the more conventional `:0` — deliberate, not a typo: this
    // is the prior prototype's own validated fallback for how Hyprland's
    // lazily-spawned rootless Xwayland lands on *this* system, carried
    // over unchanged rather than "corrected" toward the generic default.
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".to_string());
    let output = tokio::process::Command::new("xsel")
        .args(["-b", "-o"])
        .env("DISPLAY", display)
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Waits (up to `timeout`) for a clipboard read that looks like item text —
/// call `drain_stale` immediately before triggering the copy that this is
/// meant to observe, so a leftover buffered event can't be mistaken for it.
///
/// `xwayland`: which selection the game actually owns (`compositor.rs`'s
/// `Client::xwayland` flag) — X11 has no equivalent to the Wayland event
/// stream here, so that path polls `xsel` every 100ms instead of awaiting
/// `rx`, same cadence `host.mjs::copyItemText`'s poll loop used.
pub async fn read_item_text(
    rx: &mut mpsc::UnboundedReceiver<ClipboardEvent>,
    xwayland: bool,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;

    if xwayland {
        loop {
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            if let Ok(text) = read_x11_selection().await {
                if is_poe_item(&text) {
                    return Some(text);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ClipboardEvent::Selection(text))) if is_poe_item(&text) => {
                    return Some(text);
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return None,
            }
        }
    }
}
