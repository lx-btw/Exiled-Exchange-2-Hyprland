//! Hover/click tracking for the transient (non-`focusOverlay`) price-check
//! widget — ports upstream's `main/src/windowing/WidgetAreaTracker.ts`
//! 1:1 in logic, adapted to this fork's async/evdev environment.
//!
//! Upstream drives this off uiohook's `mousemove`/`mousedown`, which deliver
//! exact absolute coordinates on every physical event. evdev mice only ever
//! report *relative* deltas, so this instead treats each motion/click event
//! as a trigger to ask Hyprland for the authoritative absolute cursor
//! position (`compositor::cursor_pos()`) — coalesced so that a fast mouse
//! flick doesn't queue up a backlog of `.socket.sock` round-trips: at most
//! one `cursorpos` query is ever in flight, and if more motion arrives while
//! one is pending, only the latest one is answered once the in-flight query
//! returns (no polling timer, no fixed interval — purely event-driven).
//!
//! Mouse devices are opened read-only and never `EVIOCGRAB`'d (only
//! keyboards are captured, see `input/hotkeys.rs`): the game must keep
//! seeing every mouse event completely normally.
//!
//! Also reads `REL_WHEEL` off the same devices for `stashScroll`
//! (`handle_wheel`).

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Weak};

use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode};
use tokio::sync::{mpsc, Mutex as TokioMutex};

use crate::compositor;
use crate::input::hotkeys::ModifierState;
use crate::input::inject::VIRTUAL_KEYBOARD_NAME;
use crate::input::keymap::Modifier;
use crate::state::{self, Overlay};

/// A widget's on-screen rectangle, already converted to Hyprland's global
/// layout coordinates by the caller (`state.rs`) — see
/// `overlay::window::current_monitor_origin`'s docs for why that conversion
/// has to happen at the boundary rather than in here.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// Ports `WidgetAreaTracker.ts`'s `isPointInsideRect` — strict
    /// inequalities, matching upstream exactly (a point exactly on the
    /// border doesn't count as inside).
    fn contains(&self, x: f64, y: f64) -> bool {
        x > self.x && x < self.x + self.width && y > self.y && y < self.y + self.height
    }
}

/// One `OVERLAY->MAIN::track-area` session's parameters, already converted
/// to global coordinates. `hold`/`close_threshold`/`from`/`area` mirror
/// upstream's `holdKey`/`closeThreshold`/`from`/`area` fields 1:1.
#[derive(Debug, Clone, Copy)]
pub struct Session {
    pub hold: Option<Modifier>,
    pub close_threshold: f64,
    pub from: (f64, f64),
    pub area: Rect,
}

pub enum Cmd {
    Start(Session),
    Stop,
}

/// `state.rs` holds one of these (from `attach_area_tracker`) to start/stop
/// tracking — `OVERLAY->MAIN::track-area` sends `Start`, a fresh
/// `run_copy_item` success or a `toggle-overlay` sends `Stop`, matching every
/// call site of upstream's `areaTracker.removeListeners()`.
pub type Handle = mpsc::UnboundedSender<Cmd>;

/// Opens every evdev device that looks like a pointer (reports both `REL_X`
/// and `REL_Y` — the evdev crate's own docs note this is what standard mice
/// report) except this fork's own virtual device — mirrors
/// `hotkeys::open_keyboards`'s exclusion, even though today that device only
/// advertises keys, not relative axes, in case that ever changes.
fn open_mice() -> Vec<Device> {
    evdev::enumerate()
        .filter_map(|(_path, device)| {
            if device.name() == Some(VIRTUAL_KEYBOARD_NAME) {
                return None;
            }
            let is_mouse = device.supported_relative_axes().is_some_and(|axes| {
                axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y)
            });
            is_mouse.then_some(device)
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Move,
    Click,
}

enum DeviceMsg {
    Motion,
    Click,
    /// Raw `REL_WHEEL` value (sign is direction, matching upstream's
    /// `UiohookWheelEvent.rotation` — see `handle_wheel`), for
    /// `stashScroll`.
    Wheel(i32),
    Closed,
}

fn spawn_cursor_query(kind: Kind, pos_tx: mpsc::UnboundedSender<(Kind, io::Result<(i32, i32)>)>) {
    tokio::spawn(async move {
        let result = compositor::cursor_pos().await;
        let _ = pos_tx.send((kind, result));
    });
}

async fn overlay_is_interactive(overlay: &Weak<TokioMutex<Overlay>>) -> bool {
    match overlay.upgrade() {
        Some(o) => o.lock().await.is_interactive(),
        None => false,
    }
}

async fn set_overlay_interactive(
    overlay: &Weak<TokioMutex<Overlay>>,
    interactive: bool,
    reason: &str,
) {
    if let Some(o) = overlay.upgrade() {
        o.lock().await.set_interactive(interactive, reason);
    }
}

async fn broadcast_hide_exclusive_widget(overlay: &Weak<TokioMutex<Overlay>>) {
    if let Some(o) = overlay.upgrade() {
        o.lock()
            .await
            .broadcast(serde_json::json!({"name": "MAIN->OVERLAY::hide-exclusive-widget"}));
    }
}

/// Ports `WidgetAreaTracker.ts`'s `handleMouseMove`/`handleMouseDown` (merged
/// into one function, since both ultimately just need a resolved `(x, y)`
/// point plus a `Kind` to know which upstream handler's logic applies).
async fn handle_position(
    kind: Kind,
    x: f64,
    y: f64,
    session: &mut Option<Session>,
    modifiers: &ModifierState,
    overlay: &Weak<TokioMutex<Overlay>>,
) {
    let Some(s) = *session else { return };

    match kind {
        // handleMouseDown: click inside the widget always grabs it,
        // regardless of the hold key or how far the cursor travelled.
        Kind::Click => {
            if s.area.contains(x, y) {
                eprintln!(
                    "ee2-hyprland: area-tracker: click inside widget at ({x},{y}), activating"
                );
                *session = None;
                set_overlay_interactive(overlay, true, "area-tracker: click inside widget").await;
            }
        }
        Kind::Move => {
            let held_modifier = if modifiers.ctrl() {
                Some(Modifier::Ctrl)
            } else if modifiers.alt() {
                Some(Modifier::Alt)
            } else {
                None
            };
            let is_interactive = overlay_is_interactive(overlay).await;

            if !is_interactive && held_modifier != s.hold {
                let distance = ((x - s.from.0).powi(2) + (y - s.from.1).powi(2)).sqrt();
                if distance > s.close_threshold {
                    eprintln!(
                        "ee2-hyprland: area-tracker: cursor at ({x},{y}) is {distance:.1}px from ({:.1},{:.1}), past threshold {:.1}, hiding widget",
                        s.from.0, s.from.1, s.close_threshold
                    );
                    *session = None;
                    broadcast_hide_exclusive_widget(overlay).await;
                }
            } else if s.area.contains(x, y) {
                if !is_interactive {
                    eprintln!("ee2-hyprland: area-tracker: cursor at ({x},{y}) entered widget area, activating");
                    set_overlay_interactive(overlay, true, "area-tracker: entered widget area")
                        .await;
                }
            } else if is_interactive {
                eprintln!("ee2-hyprland: area-tracker: cursor at ({x},{y}) left widget area, deactivating");
                *session = None;
                set_overlay_interactive(overlay, false, "area-tracker: left widget area").await;
            }
        }
    }
}

/// Ports `Shortcuts.ts`'s `isStashArea` 1:1 — the sidebar-width ratio
/// (`370/600`, `GameWindow.ts::uiSidebarWidth`) and the vertical band
/// (`154/1600`..`1192/1600`) are its exact literal constants, not
/// independently derived from anything about this fork's own UI.
fn is_stash_area(cursor: (f64, f64), at: (i32, i32), size: (i32, i32)) -> bool {
    let (cx, cy) = cursor;
    let (bx, by) = (at.0 as f64, at.1 as f64);
    let bh = size.1 as f64;
    let sidebar_width = (bh * 370.0 / 600.0).round();

    if cx > bx + sidebar_width {
        return false;
    }
    cy > by + bh * 154.0 / 1600.0 && cy < by + bh * 1192.0 / 1600.0
}

/// `stashScroll`: ports `Shortcuts.ts`'s `wheel` handler
/// — `Ctrl` held, the game focused, the feature enabled in config, and the
/// cursor *outside* the stash grid itself (`is_stash_area`, above) all have
/// to hold before a wheel notch becomes an `ArrowLeft`/`ArrowRight` tap.
/// Mirrors upstream's own early-return order (`!e.ctrlKey ||
/// !this.poeWindow.isActive || !this.stashScroll`) for the cheap checks
/// first, before paying for the compositor round-trips `is_stash_area`
/// needs.
async fn handle_wheel(delta: i32, modifiers: &ModifierState, overlay: &Weak<TokioMutex<Overlay>>) {
    if delta == 0 || !modifiers.ctrl() {
        return;
    }

    let Some(strong) = overlay.upgrade() else {
        return;
    };
    {
        let guard = strong.lock().await;
        if !guard.game_active() || !guard.stash_scroll_enabled() {
            return;
        }
    }

    let (clients, cursor) = tokio::join!(compositor::clients(), compositor::cursor_pos());
    let clients = match clients {
        Ok(clients) => clients,
        Err(err) => {
            eprintln!("ee2-hyprland: stash-scroll: failed to query clients: {err}");
            return;
        }
    };
    let Some(game) = compositor::find_game(&clients) else {
        eprintln!(
            "ee2-hyprland: stash-scroll: game window ({}) not found, ignoring wheel",
            compositor::game_class()
        );
        return;
    };
    let (x, y) = match cursor {
        Ok(pos) => pos,
        Err(err) => {
            eprintln!("ee2-hyprland: stash-scroll: cursorpos query failed: {err}");
            return;
        }
    };

    if is_stash_area((x as f64, y as f64), game.at, game.size) {
        return; // over the stash grid itself — leave the wheel to the game
    }

    let right = delta > 0;
    eprintln!(
        "ee2-hyprland: stash-scroll: Ctrl+wheel outside stash area, tab {}",
        if right { "right" } else { "left" }
    );
    state::inject_stash_scroll_tab(overlay, right).await;
}

/// Runs until `cmd_rx` closes (process shutdown). Owns every mouse-like
/// input device found at startup — like `hotkeys::watch`, this doesn't
/// handle devices plugged in later.
pub async fn watch(
    overlay: Weak<TokioMutex<Overlay>>,
    modifiers: Arc<ModifierState>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let devices = open_mice();
    if devices.is_empty() {
        eprintln!("ee2-hyprland: no mouse-like /dev/input device found — hover price-check tracking disabled");
        return;
    }

    let (dev_tx, mut dev_rx) = mpsc::unbounded_channel::<(RawFd, DeviceMsg)>();

    for device in devices {
        let fd = device.as_raw_fd();
        let dev_tx = dev_tx.clone();
        let mut stream = match device.into_event_stream() {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("ee2-hyprland: failed to open mouse device for async reads: {err}");
                continue;
            }
        };
        tokio::spawn(async move {
            loop {
                match stream.next_event().await {
                    Ok(event) => {
                        let msg = match event.destructure() {
                            EventSummary::RelativeAxis(_, axis, _)
                                if axis == RelativeAxisCode::REL_X
                                    || axis == RelativeAxisCode::REL_Y =>
                            {
                                Some(DeviceMsg::Motion)
                            }
                            EventSummary::RelativeAxis(_, axis, value)
                                if axis == RelativeAxisCode::REL_WHEEL =>
                            {
                                Some(DeviceMsg::Wheel(value))
                            }
                            EventSummary::Key(_, KeyCode::BTN_LEFT, 1) => Some(DeviceMsg::Click),
                            _ => None,
                        };
                        if let Some(msg) = msg {
                            if dev_tx.send((fd, msg)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("ee2-hyprland: mouse device read error: {err}");
                        let _ = dev_tx.send((fd, DeviceMsg::Closed));
                        return;
                    }
                }
            }
        });
    }
    drop(dev_tx);

    let mut session: Option<Session> = None;
    let (pos_tx, mut pos_rx) = mpsc::unbounded_channel::<(Kind, io::Result<(i32, i32)>)>();
    let mut inflight = false;
    let mut pending: Option<Kind> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Cmd::Start(s)) => {
                        eprintln!(
                            "ee2-hyprland: area-tracker: tracking started, hold={:?} from={:?} area={:?} close_threshold={:.1}",
                            s.hold, s.from, s.area, s.close_threshold
                        );
                        session = Some(s);
                    }
                    Some(Cmd::Stop) => session = None,
                    None => return,
                }
            }
            msg = dev_rx.recv() => {
                let Some((_fd, msg)) = msg else { continue };
                let kind = match msg {
                    DeviceMsg::Motion => Kind::Move,
                    DeviceMsg::Click => Kind::Click,
                    DeviceMsg::Closed => continue,
                    DeviceMsg::Wheel(delta) => {
                        // Independent of the hover-tracking `session`
                        // above (`stashScroll` isn't gated on any widget
                        // being open) — spawned rather than awaited inline
                        // so a slow compositor round-trip or injection
                        // doesn't stall this loop's handling of concurrent
                        // motion/click events.
                        let modifiers = modifiers.clone();
                        let overlay = overlay.clone();
                        tokio::spawn(async move {
                            handle_wheel(delta, &modifiers, &overlay).await;
                        });
                        continue;
                    }
                };
                // Only bother querying the compositor while something is
                // actually being tracked — otherwise every ordinary mouse
                // movement during normal play would fire a `cursorpos` IPC
                // round-trip for nothing.
                if session.is_some() {
                    if inflight {
                        pending = Some(kind);
                    } else {
                        inflight = true;
                        spawn_cursor_query(kind, pos_tx.clone());
                    }
                }
            }
            Some((kind, result)) = pos_rx.recv() => {
                match result {
                    Ok((x, y)) => handle_position(kind, x as f64, y as f64, &mut session, &modifiers, &overlay).await,
                    Err(err) => eprintln!("ee2-hyprland: area-tracker: cursorpos query failed: {err}"),
                }
                inflight = false;
                if session.is_some() {
                    if let Some(next) = pending.take() {
                        inflight = true;
                        spawn_cursor_query(next, pos_tx.clone());
                    }
                } else {
                    pending = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `at`/`size` are the real numbers `hyprctl clients -j` reported for a
    // fullscreen PoE2 window on a 2560x1440 monitor at origin (0, 155) —
    // not synthetic, so this locks in the exact geometry `is_stash_area`
    // has to reproduce from upstream's `isStashArea`.
    const AT: (i32, i32) = (0, 155);
    const SIZE: (i32, i32) = (2560, 1440);

    #[test]
    fn inside_sidebar_and_vertical_band_is_stash_area() {
        assert!(is_stash_area((500.0, 600.0), AT, SIZE));
    }

    #[test]
    fn past_sidebar_width_is_not_stash_area() {
        // sidebar_width = round(1440 * 370 / 600) = 888
        assert!(!is_stash_area((900.0, 600.0), AT, SIZE));
    }

    #[test]
    fn above_the_vertical_band_is_not_stash_area() {
        // band starts at 155 + 1440 * 154 / 1600 = 293.6
        assert!(!is_stash_area((500.0, 200.0), AT, SIZE));
    }

    #[test]
    fn below_the_vertical_band_is_not_stash_area() {
        // band ends at 155 + 1440 * 1192 / 1600 = 1227.8
        assert!(!is_stash_area((500.0, 1300.0), AT, SIZE));
    }
}
