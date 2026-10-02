//! Event-driven clipboard read and write via `zwlr_data_control_manager_v1`,
//! with an X11-selection fallback for a game running under Xwayland without
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
//! Native-Wayland reads are event-driven, not polled: no subprocess spawn
//! per poll tick. `zwlr_data_control_device_v1.selection` fires on every genuine
//! copy action a native client performs (a fresh offer object each time,
//! even for back-to-back copies of visually identical text) — there's no
//! stale-read race to paper over the way polling had, so no "clear the
//! selection first" step is needed either: the caller just drains whatever
//! was already buffered, triggers the copy, and awaits the next event.
//!
//! Native-Wayland writes go through our own `zwlr_data_control_source_v1`
//! (`run_writer`), no `wl-copy` subprocess.
//!
//! The X11 fallback is a plain `xsel` subprocess call (matching `host.mjs`),
//! not an XCB client — a deliberate scope choice: this is the secondary
//! path (only reached when the game runs under Xwayland without
//! Wayland-native Proton), invoked once per action rather than per
//! keystroke, and X11 has no event-subscription primitive as convenient as
//! `zwlr_data_control` to build on. Revisit if the subprocess spawn turns
//! out to matter in practice; callers only see `xwayland: bool`, so a real
//! XCB implementation could replace it without touching them.
//!
//! Design rationale: HYPRLAND-NOTES.md §5.

use crate::logger::ee_log;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags};
use tokio::sync::{mpsc, oneshot};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1, EVT_DATA_OFFER_OPCODE},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
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

                let Some(offer) = id else {
                    // Selection cleared entirely (no offer at all) — nothing
                    // currently on the clipboard to report as "current".
                    if let Some(cell) = LAST_TEXT.get() {
                        *cell.lock().unwrap() = None;
                    }
                    return;
                };

                let mime_types = offer
                    .data::<OfferMimeTypes>()
                    .map(|m| m.lock().unwrap().clone())
                    .unwrap_or_default();

                // A selection we ourselves just wrote (`write_text`/
                // `write_text_restoring`) carries this marker MIME type —
                // skip reading it back. Otherwise this loop would issue a
                // `Receive` against every source we create (a round-trip
                // through our own writer thread), report our own text as a
                // `ClipboardEvent` (copy-item waits for the *game's* copy),
                // and overwrite `LAST_TEXT` — which must keep the last
                // external content for `restoreClipboard`.
                if mime_types.iter().any(|m| m == OWN_WRITE_MARKER_MIME) {
                    state.current_offer = Some(offer);
                    return;
                }

                let text =
                    pick_mime_type(&mime_types).and_then(|mime| receive(conn, &offer, mime).ok());

                match &text {
                    Some(text) => {
                        let _ = state.tx.send(ClipboardEvent::Selection(text.clone()));
                    }
                    None => {
                        eprintln!(
                            "ee2-hyprland: clipboard selection offered no usable text MIME type"
                        );
                    }
                }
                // Cached unconditionally (not just on a MIME-type hit) so
                // `current_text()` reflects reality either way — see that
                // function's docs for why this cache exists at all.
                if let Some(cell) = LAST_TEXT.get() {
                    *cell.lock().unwrap() = text;
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
                ee_log!("clipboard data-control device invalidated (seat removed?), stopping");
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
///
/// Also starts the write-side counterpart (`run_writer`, its own connection
/// and thread — see that function's docs for why it's separate from the
/// read loop above) and publishes its job queue into `WRITE_QUEUE` for
/// `write_text` to reach. Both are one-time, process-lifetime setup, so
/// `Overlay::new` (the sole caller) doing both here rather than exposing a
/// second `spawn`-like entry point keeps that call site simple.
pub fn spawn() -> io::Result<mpsc::UnboundedReceiver<ClipboardEvent>> {
    let _ = LAST_TEXT.set(Mutex::new(None));

    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("ee2-hyprland-clipboard".into())
        .spawn(move || {
            if let Err(err) = run(tx) {
                ee_log!("clipboard watcher exited: {err}");
            }
        })?;

    let wake = rustix::event::eventfd(0, rustix::event::EventfdFlags::empty())?;
    let queue = Arc::new(WriteQueue {
        jobs: Mutex::new(VecDeque::new()),
        wake,
    });
    let writer_queue = queue.clone();
    std::thread::Builder::new()
        .name("ee2-hyprland-clipboard-write".into())
        .spawn(move || {
            if let Err(err) = run_writer(writer_queue) {
                ee_log!("clipboard writer exited: {err}");
            }
        })?;
    // Only fails if called twice — `spawn()` itself is documented
    // single-call, so a failure here would mean a caller bug, not a
    // runtime condition worth propagating as an `io::Error`.
    let _ = WRITE_QUEUE.set(queue);

    Ok(rx)
}

/// A pending `write_text`/`write_text_restoring` call's payload, queued for
/// `run_writer`'s thread to pick up and turn into a real
/// `zwlr_data_control_source_v1`.
struct WriteJob {
    text: String,
    done: oneshot::Sender<io::Result<()>>,
}

/// The channel between `write_text` (called from tokio tasks) and
/// `run_writer`'s thread (blocked in `poll`, not on an async runtime).
/// `wake` is an `eventfd`: `write_text` bumps its counter after pushing a
/// job, which is what wakes `run_writer` out of `poll` without a timer —
/// same role a self-pipe would play, minus the pipe's two-fd bookkeeping.
struct WriteQueue {
    jobs: Mutex<VecDeque<WriteJob>>,
    wake: OwnedFd,
}

/// Set once, by `spawn()`, before any real `write_text` call can happen
/// (`Overlay::new` constructs the clipboard before anything else that could
/// race it) — `write_text` treats an unset queue as a caller-ordering bug,
/// not a transient condition to retry.
static WRITE_QUEUE: OnceLock<Arc<WriteQueue>> = OnceLock::new();

/// The most recently observed native-Wayland clipboard text — updated on
/// every real `Selection` event `State`'s `Dispatch` impl processes (see
/// above), *not* filtered by `is_poe_item`: this exists purely to answer
/// "what's on the clipboard right now" for `current_text()`
/// (`restoreClipboard`'s "read the buffer before we overwrite it" need),
/// which wants the user's real previous content, not just item copies.
/// Reading it is an instant mutex lock, not a synchronous protocol
/// round-trip or a subprocess spawn — by the time any caller asks, the
/// answer is already sitting here from the last real event. Set once by
/// `spawn()`, same lifetime contract as `WRITE_QUEUE`.
static LAST_TEXT: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Dispatch target for `run_writer`'s own connection — deliberately separate
/// from `State` (the read loop's dispatch target) even though the
/// registry/seat/manager handling is identical: keeping the two connections
/// and their `Dispatch` impls independent means neither loop's state or
/// timing assumptions leak into the other.
struct WriterState;

impl Dispatch<WlRegistry, GlobalListContents> for WriterState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WriterState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for WriterState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WriterState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We only ever create this device to call `set_selection` on it —
        // reading back `data_offer`/`selection`/`primary_selection` is the
        // *read* side's concern (`State`, the other connection). The
        // compositor still creates an offer object on `DataOffer` for every
        // selection change (including our own writes) — destroyed
        // immediately, right here, rather than left to leak for the
        // connection's whole lifetime. `Selection`/`PrimarySelection`
        // reference that *same* already-destroyed id, not a new one, so
        // there's nothing left to do when those arrive.
        if let zwlr_data_control_device_v1::Event::DataOffer { id } = event {
            id.destroy();
        }
    }

    event_created_child!(Self, ZwlrDataControlDeviceV1, [
        EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WriterState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only the `Offer` (mime-type announcement) event ever arrives here
        // — nothing to do with it on a device we only ever write through.
    }
}

/// Each live `ZwlrDataControlSourceV1`'s user-data: the exact text it needs
/// to hand back on every `Send`, however many times a reader asks (a
/// clipboard-history daemon and/or the game itself may request it more than
/// once before the next `write_text` replaces this selection — see
/// `write_text_restoring`'s docs on why this file doesn't try to treat
/// `Send` as "the game specifically consumed it").
impl Dispatch<ZwlrDataControlSourceV1, Mutex<String>> for WriterState {
    fn event(
        _: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        text: &Mutex<String>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                use std::io::Write;
                let text = text.lock().unwrap().clone();
                // Closing `file` (end of scope) drops the fd, which is the
                // EOF signal the reading side (the game, `wl-paste`, a
                // clipboard manager) is waiting for.
                let mut file = std::fs::File::from(fd);
                if let Err(err) = file.write_all(text.as_bytes()) {
                    ee_log!("clipboard write: failed to write paste data: {err}");
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                // A later `write_text` (or another client's own copy) took
                // over the selection — this source has no further use.
                source.destroy();
            }
            _ => {}
        }
    }
}

/// The MIME types every offered source advertises — deliberately the same
/// three `pick_mime_type`'s `PREFERRED` list favors on the read side, minus
/// `STRING`/`TEXT` (legacy X11-only names with no real Wayland-native
/// client asking for them). The bytes handed back on `Send` are identical
/// regardless of which of these the requester picked — this is plain UTF-8
/// text, not something that actually varies by encoding label.
const OFFERED_MIME_TYPES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// A private, made-up MIME type offered alongside the real text ones on
/// every source this process creates — lets `State`'s (read-side) Selection
/// handler recognize "this is our own write" and skip reading it back (see
/// that handler's docs for why that matters). Real clients (the game,
/// `wl-paste`, …) are expected to ignore MIME types they don't recognize,
/// same as they'd ignore any other type this process didn't happen to
/// offer — this isn't part of any real protocol, just an in-process marker
/// riding along on the one channel (the offer's own MIME list) both sides
/// of this file already read.
const OWN_WRITE_MARKER_MIME: &str = "application/x-ee2-hyprland-own-write";

/// Runs on its own dedicated OS thread and its own Wayland connection,
/// separate from `run()`'s read loop above — a source has to keep answering
/// `Send` for as long as it holds the selection (potentially long after the
/// `write_text` call that created it has already returned), which means
/// this thread's dispatch loop needs to run indefinitely alongside the read
/// loop's, not interleaved with it on the same fd/thread.
///
/// Multiplexes two fds with a single blocking `poll` (no timer, no polling
/// interval): the connection's own fd (new events from the compositor —
/// mainly `Send`/`Cancelled` on whatever source currently holds the
/// selection) and `queue.wake` (a new job pushed by `write_text`). This is
/// the standard "external event loop" pattern `wayland-client`'s own docs
/// describe for `EventQueue::prepare_read`, just with a second fd folded
/// into the same `poll` call instead of only ever waiting on the
/// connection.
fn run_writer(queue: Arc<WriteQueue>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init::<WriterState>(&conn)?;
    let qh = event_queue.handle();

    let seat: WlSeat = globals.bind(&qh, 1..=1, ())?;
    let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let device = manager.get_data_device(&seat, &qh, ());

    let mut state = WriterState;

    loop {
        event_queue.dispatch_pending(&mut state)?;
        conn.flush()?;

        // Handing off queued jobs here (rather than only right after the
        // wake fd is seen readable) covers the case where a job was pushed
        // between the loop's last wait and this point — checking
        // unconditionally on every pass means that race can't drop a job.
        while let Some(job) = queue.jobs.lock().unwrap().pop_front() {
            let source = manager.create_data_source(&qh, Mutex::new(job.text));
            for mime in OFFERED_MIME_TYPES {
                source.offer(mime.to_string());
            }
            source.offer(OWN_WRITE_MARKER_MIME.to_string());
            device.set_selection(Some(&source));
            conn.flush()?;
            // `write_text` only needs to know the selection was handed to
            // the compositor, not that a paste actually happened yet — same
            // "fire and let it linger" contract `wl-copy` (without
            // `--foreground`) has.
            let _ = job.done.send(Ok(()));
        }

        let Some(read_guard) = event_queue.prepare_read() else {
            // Events already queued from the dispatch above — go handle
            // those (and any newly-queued jobs) before trying to read again.
            continue;
        };
        let conn_fd = read_guard.connection_fd();
        let mut fds = [
            PollFd::new(&conn_fd, PollFlags::IN),
            PollFd::new(&queue.wake, PollFlags::IN),
        ];
        poll(&mut fds, None)?;

        if fds[1].revents().contains(PollFlags::IN) {
            // Drain the eventfd counter — left unread, it stays readable
            // and `poll` would return immediately forever after.
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(&queue.wake, &mut buf);
        }
        if fds[0].revents().contains(PollFlags::IN) {
            let _ = read_guard.read();
        } else {
            drop(read_guard);
        }
    }
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

/// Writes `text` to the clipboard the game will actually read from,
/// gated on the same `xwayland` flag every other clipboard function here
/// takes. Xwayland still goes through the `xsel -b -i` subprocess (matching
/// `read_x11_selection`'s own fallback) — documented secondary path, kept
/// deliberately: X11 doesn't have as convenient a source-object primitive
/// as `zwlr_data_control_source_v1` to build on, and this is the rarer of
/// the two cases (only reached for a game running under plain Xwayland
/// without `PROTON_ENABLE_WAYLAND=1`).
///
/// The native-Wayland path hands the text to `run_writer`'s thread over
/// `WRITE_QUEUE`, which creates a real `zwlr_data_control_source_v1` on the
/// connection it already holds open and answers `Send` itself. Thin
/// fire-and-forget wrapper around `write_text_xwayland`/`write_text_native`
/// — `write_text_restoring` is the one that also arranges a later restore.
pub async fn write_text(text: &str, xwayland: bool) -> io::Result<()> {
    if xwayland {
        write_text_xwayland(text).await
    } else {
        write_text_native(text).await
    }
}

/// The Xwayland write itself — `write_text`'s xwayland arm and
/// `write_text_restoring`'s restore write (Xwayland path) both need exactly
/// this, split out so neither re-derives the `DISPLAY` fallback or the
/// piped-stdin dance.
async fn write_text_xwayland(text: &str) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":1".to_string());
    let mut child = tokio::process::Command::new("xsel")
        .args(["-b", "-i"])
        .env("DISPLAY", display)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin was piped above");
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin); // EOF, so the reading end (xsel) can return

    let status = child.wait().await?;
    if !status.success() {
        return Err(io::Error::other(format!("xsel exited with {status}")));
    }
    Ok(())
}

/// How long to wait before restoring the clipboard to its pre-write value
/// (or, with `restoreClipboard` off, just releasing the caller's throttle)
/// — the same fixed-wait approach upstream's `HostClipboard.RESTORE_AFTER`
/// uses, and the same value (120ms), on **both** paths.
///
/// Why not event-driven: the obvious signal on the native path — restore
/// once the write source gets its `Send` event — doesn't mean "the game
/// pasted it". Clipboard-history daemons (`wl-clip-persist`, `wl-paste
/// --watch … | cliphist store` — a common Hyprland setup) answer `Receive`
/// against *any* clipboard write within microseconds, indistinguishable at
/// the protocol level from the game itself pasting (the same class of
/// problem upstream's `HostClipboard.ts` comments call out for KDE's
/// "Prevent empty clipboard" and the Proton 10 clipboard bug). A
/// `Send`-driven restore was tried and failed exactly this way.
/// `current_text()` below is still the event-fed, no-timer, no-subprocess
/// primitive for "what's on the clipboard right now" — only "when has this
/// write been consumed" has no reliable signal at this protocol layer.
const RESTORE_AFTER: Duration = Duration::from_millis(120);

/// Writes `text` to the clipboard the game will read from, same as
/// `write_text`, but additionally arranges to write `restore_to` back (if
/// `Some`) `RESTORE_AFTER` later — ported from
/// `HostClipboard.restoreShortly`'s save-before/restore-after behavior (see
/// `RESTORE_AFTER`'s docs for why this is a fixed wait rather than
/// event-driven, on both paths). Returns a receiver that resolves once
/// `RESTORE_AFTER` has elapsed and the restore write (if any) was
/// attempted — whether or not `restore_to` was `Some`, so a caller wanting
/// the same "don't overlap two of these" protection upstream's
/// `isRestored` flag gave doesn't have to guess a wait either
/// (`state.rs`'s `chat_action_in_flight` is exactly this).
pub async fn write_text_restoring(
    text: &str,
    xwayland: bool,
    restore_to: Option<String>,
) -> io::Result<oneshot::Receiver<()>> {
    if xwayland {
        write_text_xwayland(text).await?;
    } else {
        write_text_native(text).await?;
    }

    let (settled_tx, settled_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(RESTORE_AFTER).await;
        if let Some(restore_to) = restore_to {
            let result = if xwayland {
                write_text_xwayland(&restore_to).await
            } else {
                write_text_native(&restore_to).await
            };
            if let Err(err) = result {
                ee_log!("clipboard: restore write failed: {err}");
            }
        }
        let _ = settled_tx.send(());
    });
    Ok(settled_rx)
}

/// The native-Wayland write itself — hands `text` to `run_writer`'s thread
/// over `WRITE_QUEUE`, which creates a real `zwlr_data_control_source_v1`
/// on the connection it already holds open and answers `Send` itself (no
/// `wl-copy` subprocess). Split out of `write_text`/
/// `write_text_restoring` since both need exactly this, with nothing else
/// attached.
async fn write_text_native(text: &str) -> io::Result<()> {
    let queue = WRITE_QUEUE
        .get()
        .ok_or_else(|| io::Error::other("clipboard writer not started (spawn() not called yet)"))?;
    let (done_tx, done_rx) = oneshot::channel();
    queue.jobs.lock().unwrap().push_back(WriteJob {
        text: text.to_string(),
        done: done_tx,
    });
    rustix::io::write(&queue.wake, &1u64.to_ne_bytes())?;
    done_rx
        .await
        .map_err(|_| io::Error::other("clipboard writer thread exited"))?
}

/// Reads whatever's on the clipboard *right now*, without triggering a
/// fresh copy or waiting for one — the primitive `restoreClipboard` needs
/// to capture the user's real previous content before `write_text_restoring`
/// overwrites it. Native-Wayland: an instant read of `LAST_TEXT` (populated
/// from real `Selection` events as they happen — no subprocess, no timer).
/// Xwayland: one direct `xsel -b -o` read, same as `read_item_text`'s own
/// Xwayland path already does — not a poll loop, just a single synchronous
/// snapshot, since X11 has no equivalent of `LAST_TEXT`'s event-fed cache
/// to read from instead.
pub async fn current_text(xwayland: bool) -> Option<String> {
    if xwayland {
        read_x11_selection().await.ok()
    } else {
        LAST_TEXT
            .get()
            .and_then(|cell| cell.lock().unwrap().clone())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The test harness runs `#[tokio::test]`s in parallel by default (each
    /// on its own OS thread), but every test in this module drives the
    /// *same* real system clipboard through the *same* real Hyprland
    /// session — two tests writing concurrently would race each other's
    /// `wl-paste` reads regardless of either test's own logic being
    /// correct. `tokio::sync::Mutex` (not `std::sync::Mutex`) specifically
    /// because its guard is safely held across `.await` points, which
    /// every test here does throughout.
    static CLIPBOARD_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `write_text`'s native-Wayland path creates its own
    /// `zwlr_data_control_source_v1` on `run_writer`'s connection instead of
    /// spawning `wl-copy`. Round-trip check against the real clipboard —
    /// needs a live compositor with a seat and `wlr-data-control` support
    /// (a Hyprland session) and `wl-paste` on `PATH`.
    #[tokio::test]
    async fn write_text_round_trips_without_a_wl_copy_subprocess() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().await;

        async fn wl_copy_pids() -> Vec<u8> {
            // Snapshot before/after, compared for equality below — some
            // systems already run an unrelated long-lived `wl-copy`
            // (e.g. a clipboard manager holding the primary selection), so
            // "zero running" is the wrong assertion; "the set didn't grow"
            // is what actually proves `write_text` didn't spawn one.
            tokio::process::Command::new("pgrep")
                .args(["-x", "wl-copy"])
                .output()
                .await
                .expect("pgrep")
                .stdout
        }

        let marker = format!(
            "ee2-hyprland clipboard.rs test {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let before = wl_copy_pids().await;

        let _rx = spawn().expect("clipboard::spawn");
        write_text(&marker, /* xwayland */ false)
            .await
            .expect("write_text");

        let output = tokio::process::Command::new("wl-paste")
            .arg("-n")
            .output()
            .await
            .expect("wl-paste");
        assert!(output.status.success(), "wl-paste failed: {output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), marker);

        // No *new* `wl-copy` subprocess exists to serve this selection —
        // `run_writer`'s own source answered `wl-paste`'s `Receive` directly.
        let after = wl_copy_pids().await;
        assert_eq!(
            before, after,
            "a new wl-copy process appeared — write_text should never spawn one"
        );
    }

    /// The restoreClipboard mechanism: confirms both halves of
    /// `write_text_restoring` against the real system clipboard — the new
    /// text is actually readable immediately after the write, and the
    /// previous value lands back on the clipboard `RESTORE_AFTER` later.
    /// This test's own `wl-paste` read (like a clipboard-history watcher,
    /// see `RESTORE_AFTER`'s docs) reads the selection almost immediately;
    /// the fixed-wait design must not be thrown off by that.
    #[tokio::test]
    async fn write_text_restoring_restores_after_the_wait() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().await;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker_new = format!("ee2-hyprland clipboard.rs restore-test new {nanos}");
        let marker_old = format!("ee2-hyprland clipboard.rs restore-test old {nanos}");

        let _rx = spawn().expect("clipboard::spawn");

        let settled_rx = write_text_restoring(
            &marker_new,
            /* xwayland */ false,
            Some(marker_old.clone()),
        )
        .await
        .expect("write_text_restoring");

        let output = tokio::process::Command::new("wl-paste")
            .arg("-n")
            .output()
            .await
            .expect("wl-paste");
        assert_eq!(String::from_utf8_lossy(&output.stdout), marker_new);

        // Bounded well beyond `RESTORE_AFTER` — a hang here means the
        // restore/settle mechanism itself is broken, not just slow.
        tokio::time::timeout(Duration::from_secs(2), settled_rx)
            .await
            .expect("write_text_restoring's settle signal never arrived")
            .expect("settle sender dropped without firing");

        let output = tokio::process::Command::new("wl-paste")
            .arg("-n")
            .output()
            .await
            .expect("wl-paste");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            marker_old,
            "clipboard wasn't restored to the previous value after RESTORE_AFTER"
        );
    }
}
