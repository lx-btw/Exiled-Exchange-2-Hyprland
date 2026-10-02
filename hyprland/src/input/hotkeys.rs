//! evdev hotkey listener **and** keyd-style input capture. Opens
//! `/dev/input/event*` for every device that looks like a keyboard (see
//! `open_keyboards()`) and watches for the configured combos — but now also
//! `EVIOCGRAB`s each one (via `grab.rs::Capture`) while the game is
//! focused, re-emitting everything through a separate `uinput` mirror
//! device except the matched trigger key of a fired hotkey.
//!
//! **Capture policy — read this before changing anything here or in
//! `grab.rs`.** Corrected 2026-09-24: earlier revisions of this file,
//! `CLAUDE.md`, and `HYPRLAND-NOTES.md` §3 stated a blanket "never
//! `EVIOCGRAB`" rule. That was a misreading of the actual constraint — the
//! real, still-load-bearing rule is narrower: this app never touches
//! Hyprland's own *config* (no `bind`, no `windowrule`, no `keyword`, no
//! `hyprctl keyword`/`eval`), not that it never grabs a device. Without a
//! grab, a hotkey's own trigger key (`D` in `Ctrl+D`, `Space` in
//! `Shift+Space`) still reached the game underneath and performed whatever
//! it's bound to there (a step, a dodge-roll) on *every* press — see
//! PLAN.md's H2 entry.
//!
//! - Only real hardware keyboards are ever grabbed — never `inject.rs`'s
//!   injection device or `grab.rs`'s own mirror device (both excluded by
//!   name in `open_keyboards()`, same mechanism that already excluded the
//!   injector before capture existed). The mouse is never grabbed (it
//!   isn't enumerated here at all — see `open_keyboards()`'s `KEY_A`
//!   filter).
//! - Grab is active only while the game is focused — `game_active` is now
//!   a `watch::Receiver<bool>` rather than the old `Arc<AtomicBool>`; the
//!   switch to `watch` is what lets this loop react to a focus change
//!   immediately (via `select!`), instead of only noticing on the next
//!   keystroke, which would otherwise leak exactly the first press after
//!   every alt-tab back into the game.
//! - A grab/ungrab transition only happens at a moment nothing is held on
//!   that specific device — checked against this loop's own per-`(fd,
//!   code)` `held` set, filtered to one device. If something's held when
//!   the desired state changes, the transition is deferred until that
//!   device's own next key-release. This is what stops a modifier from
//!   being reported stuck to Hyprland: a modifier already pressed *before*
//!   a grab engages has already had its press event delivered to whatever
//!   had focus at the time; grabbing mid-hold would swallow its eventual
//!   release, and nothing downstream would ever see that key go up.
//! - Whatever a grabbed device produces that isn't consumed is re-emitted,
//!   immediately, through `grab.rs::Capture`'s mirror device — a *second*,
//!   separate `uinput` device from `inject.rs`'s (mixing the `Ctrl+Alt+C`
//!   injector's own timed sequences with live passthrough typing on the
//!   same device would race the two). Consumed = only a matched hotkey's
//!   own trigger-key press, its repeats, and its release; every modifier
//!   key passes through untouched, on every device, always.
//! - Per-device XKB layout (`us,ru` / `grp:alt_shift_toggle`) is
//!   synchronized at each transition (`grab.rs`, HYPRLAND-NOTES.md §9), so
//!   a device grabbed mid-`ru` doesn't suddenly type through the mirror's
//!   own independent (always `us`-default) XKB state, and releasing the
//!   grab hands the physical device back whatever layout the user last
//!   toggled to while typing through the mirror.
//! - No timers anywhere in this path. `evdev::Device` ungrabs itself on
//!   `Drop`, and `EVIOCGRAB` is fd-scoped kernel state the kernel already
//!   releases on any process exit, `SIGKILL` included.
//! - **Known limitation, not implemented**: Caps Lock / Num Lock LED state
//!   isn't forwarded from the mirror back to the grabbed physical keyboard
//!   — see `grab.rs`'s module docs for why (the evdev crate's safe uinput
//!   builder API has no way to declare `EV_LED` capability without an
//!   `unsafe` ioctl call this codebase doesn't otherwise need).
//!
//! Everything below is unchanged from before capture existed: modifier
//! state is still merged across every open keyboard device into one
//! logical state machine — held keys tracked per `(device, keycode)` pair
//! rather than a shared bitset, so releasing a modifier on one keyboard
//! can't spuriously clear it while a second physical keyboard still holds
//! its own copy — and matching is still **exact modifier-set equality**,
//! not "at least these held": upstream's `globalShortcut.register()` treats
//! `"Ctrl + D"` and `"Ctrl + Alt + D"` as independent accelerators, so
//! pressing Ctrl+Alt+D must not also fire a registered "Ctrl + D".
//!
//! Ported from upstream's `main/src/shortcuts/Shortcuts.ts` dispatch logic,
//! but the registration mechanism (`globalShortcut`) is not — this never
//! registers a bind with the compositor, it captures at the evdev layer.
//!
//! Stage 2/5, H2A (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §3, §9.
//! Fallback if merging modifier state across multiple keyboards proves
//! unreliable: track only the device that produced the last keydown.

use crate::logger::ee_log;
use std::collections::HashSet;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use evdev::{Device, EventSummary, KeyCode};
use tokio::sync::{mpsc, watch};

use super::grab::Capture;
use super::inject::VIRTUAL_KEYBOARD_NAME;
use super::keymap::{Modifier, Shortcut};

/// The live binding list, shared with whoever owns it (`state.rs`) so it can
/// be replaced whenever the renderer pushes updated shortcuts —
/// `CLIENT->MAIN::update-host-config` can arrive any time after startup, not
/// just once, and upstream's own `Shortcuts.ts::updateActions()` reacts to
/// it live rather than requiring a restart. The device-reading loop below
/// stays open the whole time; only this list is swapped out.
pub type Bindings = Arc<Mutex<Vec<(Shortcut, usize)>>>;

fn is_held(held: &HashSet<(RawFd, KeyCode)>, codes: [KeyCode; 2]) -> bool {
    held.iter().any(|(_, k)| codes.contains(k))
}

/// Live Ctrl/Alt held-state, merged across every open keyboard device
/// the same way `watch()`'s own `held` set is — shared with `area_tracker.rs`
/// (ports `WidgetAreaTracker.ts`'s `e.ctrlKey`/`e.altKey` on each mouse
/// event) so the area tracker doesn't need to open and track its own
/// keyboard devices just to answer "is the price-check hold key currently
/// down". Updated on every keydown *and* keyup, unconditionally — unlike
/// hotkey matching itself, this must stay accurate even while the game isn't
/// focused (upstream's `WidgetAreaTracker` mouse tracking isn't gated on
/// game focus either, only the `globalShortcut` registrations are).
pub struct ModifierState {
    ctrl: AtomicBool,
    alt: AtomicBool,
}

impl ModifierState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ctrl: AtomicBool::new(false),
            alt: AtomicBool::new(false),
        })
    }

    pub fn ctrl(&self) -> bool {
        self.ctrl.load(Ordering::Relaxed)
    }
    pub fn alt(&self) -> bool {
        self.alt.load(Ordering::Relaxed)
    }

    fn update(&self, held: &HashSet<(RawFd, KeyCode)>) {
        self.ctrl
            .store(is_held(held, Modifier::Ctrl.keycodes()), Ordering::Relaxed);
        self.alt
            .store(is_held(held, Modifier::Alt.keycodes()), Ordering::Relaxed);
    }
}

/// Opens every device evdev finds under `/dev/input` that looks like a
/// keyboard (reports `KEY_A` — filters out mice/joysticks, which also emit
/// `EV_KEY` for their buttons but never letter keys) — **except our own**
/// `inject.rs` and `grab.rs` virtual devices. Both advertise the same
/// letter/modifier capability set a real keyboard does, so without this
/// exclusion every key either of them emits would loop right back into the
/// matcher below (and, for capture, straight back into the grab-candidate
/// list as if a second physical keyboard had appeared) — the exact
/// self-triggering feedback loop PLAN.md documents as an already-happened
/// real bug for `inject.rs`'s device (Stage 2 review).
fn open_keyboards() -> Vec<Device> {
    evdev::enumerate()
        .filter_map(|(_path, device)| {
            let name = device.name();
            if name == Some(VIRTUAL_KEYBOARD_NAME)
                || name == Some(super::grab::MIRROR_KEYBOARD_NAME)
            {
                return None;
            }
            let is_keyboard = device
                .supported_keys()
                .is_some_and(|keys| keys.contains(KeyCode::KEY_A));
            is_keyboard.then_some(device)
        })
        .collect()
}

/// Watches `bindings` (parsed shortcut, opaque index into the caller's own
/// action list — read fresh on every keydown, so updating it takes effect
/// immediately) across every keyboard device present at startup, and sends
/// the matching index over `tx` each time one fires. Runs until every
/// keyboard device errors out or is unplugged (this doesn't watch for
/// hotplugged devices — see module docs' fallback note if that turns out to
/// matter in practice).
///
/// `game_active` (`state.rs`'s `hotkeys_active`, updated from
/// `set_game_active`) gates both matching and capture: ports upstream's
/// `Shortcuts.ts` `poeWindow.on("active-change", ...)` →
/// `register()`/`unregister()`, so a shortcut like `Shift + Space` only
/// fires while PoE2 is the focused window, and a device is only grabbed
/// while it is too. Modifier/held-key bookkeeping still runs regardless of
/// this flag — only match-and-fire and the grab itself are gated — so state
/// stays accurate for whenever the game becomes active again.
///
/// `overlay_interactive` (`state.rs`'s `Overlay.overlay_interactive_tx`) and
/// `close_overlay_tx` are H2B's `Esc` handling (see the `KEY_ESC` case
/// below) — `Esc` isn't in `bindings` at all (it isn't a user-configurable
/// shortcut upstream either; `OverlayWindow.ts` hardcodes it separately
/// from `Shortcuts.ts`), so it gets its own watch channel to read and its
/// own action channel to fire, rather than a synthetic `bindings` entry.
pub async fn watch(
    bindings: Bindings,
    mut game_active: watch::Receiver<bool>,
    overlay_interactive: watch::Receiver<bool>,
    modifiers: Arc<ModifierState>,
    tx: mpsc::UnboundedSender<usize>,
    close_overlay_tx: mpsc::UnboundedSender<()>,
) {
    let devices = open_keyboards();
    if devices.is_empty() {
        eprintln!("ee2-hyprland: no keyboard-like /dev/input device found — hotkeys disabled");
        return;
    }

    // `Capture::new` needs to read every device's name/capabilities before
    // any of them are moved into their own reader task below. A failure
    // here (uinput unavailable, permission denied) degrades to the
    // pre-capture behavior — hotkeys still match and fire, they just never
    // grab anything and the H2 leak-on-first-press bug is back — rather
    // than taking the whole hotkey listener down with it.
    let mut capture = match Capture::new(&devices) {
        Ok(capture) => Some(capture),
        Err(err) => {
            ee_log!(
                "failed to create input-capture mirror device: {err} — \
                 hotkeys will still fire but their trigger key will leak into the game \
                 (pre-H2 behavior)"
            );
            None
        }
    };
    let capture_enabled = capture.is_some();

    enum DeviceMsg {
        Event(evdev::InputEvent),
        // The reader for this fd exited (error or unplug) — the central
        // loop must drop every `held` entry for it, or a modifier physically
        // released on that now-gone device would stay "held" forever and
        // spuriously satisfy every shortcut requiring it, sourced from any
        // *other* keyboard, until the process restarts.
        Closed,
    }

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<(RawFd, DeviceMsg)>();
    let mut fds: Vec<RawFd> = Vec::new();

    for device in devices {
        let fd = device.as_raw_fd();
        fds.push(fd);
        let events_tx = events_tx.clone();

        // Always created, even when capture is disabled: the receiver is
        // moved into the reader task below either way, but its branch is
        // gated on `capture_enabled` (see that task) so it's never polled
        // when there's no sender registered anywhere to feed it — polling
        // a channel whose only sender was just dropped returns `None`
        // forever, which would busy-loop `select!` without that guard.
        let (grab_tx, mut grab_rx) = mpsc::unbounded_channel::<bool>();
        if let Some(capture) = capture.as_mut() {
            capture.register_device(fd, grab_tx);
        }

        let mut stream = match device.into_event_stream() {
            Ok(stream) => stream,
            Err(err) => {
                ee_log!("failed to open input device for async reads: {err}");
                continue;
            }
        };
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = stream.next_event() => {
                        match event {
                            Ok(event) => {
                                if events_tx.send((fd, DeviceMsg::Event(event))).is_err() {
                                    return;
                                }
                            }
                            Err(err) => {
                                ee_log!("input device read error: {err}");
                                let _ = events_tx.send((fd, DeviceMsg::Closed));
                                return;
                            }
                        }
                    }
                    Some(want_grabbed) = grab_rx.recv(), if capture_enabled => {
                        // The grab/ungrab syscall itself: only this task
                        // owns the `Device` handle for this fd, so the
                        // central loop below can only ask for it via this
                        // channel, never perform it directly.
                        let result = if want_grabbed {
                            stream.device_mut().grab()
                        } else {
                            stream.device_mut().ungrab()
                        };
                        if let Err(err) = result {
                            ee_log!(
                                "failed to {} input device: {err}",
                                if want_grabbed { "grab" } else { "ungrab" }
                            );
                        }
                    }
                }
            }
        });
    }
    drop(events_tx);

    let mut held: HashSet<(RawFd, KeyCode)> = HashSet::new();

    // Engage capture immediately if the game is already focused when this
    // starts — don't wait for the first keystroke to notice (that keystroke
    // would otherwise leak straight through, the exact H2 bug this exists
    // to fix).
    if let Some(capture) = capture.as_mut() {
        let active = *game_active.borrow();
        capture.recheck_all(&fds, active, &held).await;
    }

    loop {
        tokio::select! {
            msg = events_rx.recv() => {
                let Some((fd, msg)) = msg else { break };

                let event = match msg {
                    DeviceMsg::Closed => {
                        held.retain(|(held_fd, _)| *held_fd != fd);
                        modifiers.update(&held);
                        if let Some(capture) = capture.as_mut() {
                            capture.forget_device(fd);
                        }
                        continue;
                    }
                    DeviceMsg::Event(event) => event,
                };

                let EventSummary::Key(_, code, value) = event.destructure() else {
                    continue;
                };
                let grabbed = capture.as_ref().is_some_and(|c| c.is_grabbed(fd));

                match value {
                    // Keydown (1) — record it, check for a match, and (while
                    // grabbed) eat exactly the matched hotkey's own trigger
                    // key so it doesn't also perform its native in-game
                    // action. Repeat (2) carries no new information for
                    // shortcut *matching*, but still needs forwarding or
                    // eating depending on whether its press was eaten.
                    1 => {
                        held.insert((fd, code));
                        modifiers.update(&held);

                        let mut consumed = false;
                        let game_is_active = *game_active.borrow();
                        if game_is_active {
                            let ctrl = is_held(&held, Modifier::Ctrl.keycodes());
                            let shift = is_held(&held, Modifier::Shift.keycodes());
                            let alt = is_held(&held, Modifier::Alt.keycodes());

                            let current = bindings.lock().unwrap().clone();
                            for (shortcut, index) in &current {
                                if shortcut.key == code
                                    && shortcut.ctrl == ctrl
                                    && shortcut.shift == shift
                                    && shortcut.alt == alt
                                {
                                    if grabbed {
                                        consumed = true;
                                    }
                                    if tx.send(*index).is_err() {
                                        return;
                                    }
                                }
                            }

                            // H2B: `Esc` always closes an interactive
                            // overlay — a bare press only (no modifiers),
                            // matching upstream's `handleExtraCommands`
                            // switch, which compares against the literal
                            // string `"Escape"` (a held modifier produces
                            // `"Shift + Escape"` etc., which doesn't match
                            // that case and falls through untouched).
                            if code == KeyCode::KEY_ESC
                                && !ctrl
                                && !shift
                                && !alt
                                && *overlay_interactive.borrow()
                            {
                                if grabbed {
                                    consumed = true;
                                }
                                let _ = close_overlay_tx.send(());
                            }
                        }

                        if let Some(capture) = capture.as_mut() {
                            if consumed {
                                capture.mark_eaten(fd, code);
                            } else if grabbed {
                                capture.forward(code, 1);
                            }
                        }
                    }
                    2 => {
                        if grabbed {
                            let eaten = capture.as_ref().is_some_and(|c| c.is_eaten(fd, code));
                            if !eaten {
                                if let Some(capture) = capture.as_mut() {
                                    capture.forward(code, 2);
                                }
                            }
                        }
                    }
                    0 => {
                        held.remove(&(fd, code));
                        modifiers.update(&held);

                        if grabbed {
                            let eaten = capture.as_mut().is_some_and(|c| c.take_eaten(fd, code));
                            if !eaten {
                                if let Some(capture) = capture.as_mut() {
                                    capture.forward(code, 0);
                                }
                            }
                        }

                        if let Some(capture) = capture.as_mut() {
                            let active = *game_active.borrow();
                            capture.maybe_transition(fd, active, &held).await;
                        }
                    }
                    _ => {}
                }
            }
            changed = game_active.changed() => {
                if changed.is_err() {
                    // Sender (state.rs's Overlay) dropped — the process is
                    // shutting down. Keep reading events on whatever state
                    // capture last settled into rather than exiting this
                    // loop early.
                    continue;
                }
                if let Some(capture) = capture.as_mut() {
                    let active = *game_active.borrow();
                    capture.recheck_all(&fds, active, &held).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::keymap::parse_shortcut;

    #[test]
    fn price_check_hold_combo_matches_upstream_default() {
        // hotkeyHold: "Ctrl", hotkey: "D" (PriceCheckWindow.vue's
        // initInstance default) merge to "Ctrl + D".
        let shortcut = parse_shortcut("Ctrl + D").unwrap();
        assert!(shortcut.ctrl && !shortcut.shift && !shortcut.alt);
        assert_eq!(shortcut.key, evdev::KeyCode::KEY_D);
    }
}
