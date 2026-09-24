//! evdev hotkey listener. Opens `/dev/input/event*` read-only — **never**
//! `EVIOCGRAB` — and watches for the shortcut combos defined in the user's
//! `config.json` in parallel with everything else on the system. The game
//! keeps receiving every keypress completely normally.
//!
//! Merges modifier state across all open keyboard devices into one logical
//! state machine (handles multiple physical keyboards): held keys are
//! tracked per `(device, keycode)` pair rather than in one shared bitset, so
//! releasing a modifier on one keyboard can't spuriously clear it while a
//! second physical keyboard still holds its own copy of that key — the
//! failure mode a single shared bitset would have.
//!
//! Matching is **exact modifier-set equality**, not "at least these held":
//! upstream's `globalShortcut.register()` treats `"Ctrl + D"` and
//! `"Ctrl + Alt + D"` as independent accelerators, so pressing Ctrl+Alt+D
//! must not also fire a registered "Ctrl + D". No synthetic key-release step
//! is needed here the way upstream's uiohook-based `register()` handler
//! needs one (`entry.shortcut.split(" + ").forEach(key => keyToggle(key,
//! "up"))`) — that exists only because Electron's `globalShortcut` consumes
//! the real event before the OS delivers a natural key-up; since we never
//! grab anything, the physical keyboard's own up-events always arrive
//! normally regardless of what we do here.
//!
//! Ported from upstream's `main/src/shortcuts/Shortcuts.ts` dispatch logic,
//! but the registration mechanism (`globalShortcut`) is not — this listens,
//! it never grabs or registers anything with the compositor.
//!
//! Stage 2 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §3.
//! Fallback if merging modifier state across multiple keyboards proves
//! unreliable: track only the device that produced the last keydown.

use std::collections::HashSet;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use evdev::{Device, EventSummary, KeyCode};
use tokio::sync::mpsc;

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

/// Live Ctrl/Shift/Alt held-state, merged across every open keyboard device
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
    shift: AtomicBool,
    alt: AtomicBool,
}

impl ModifierState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ctrl: AtomicBool::new(false),
            shift: AtomicBool::new(false),
            alt: AtomicBool::new(false),
        })
    }

    pub fn ctrl(&self) -> bool {
        self.ctrl.load(Ordering::Relaxed)
    }
    /// Unused for now — `WidgetAreaTracker.ts`'s `holdKey` check only ever
    /// compares against `Ctrl`/`Alt` (`e.ctrlKey`/`e.altKey`), never Shift.
    /// Tracked anyway for symmetry with `hotkeys.rs`'s own held-modifier
    /// bookkeeping, which does use all three.
    #[allow(dead_code)]
    pub fn shift(&self) -> bool {
        self.shift.load(Ordering::Relaxed)
    }
    pub fn alt(&self) -> bool {
        self.alt.load(Ordering::Relaxed)
    }

    fn update(&self, held: &HashSet<(RawFd, KeyCode)>) {
        self.ctrl
            .store(is_held(held, Modifier::Ctrl.keycodes()), Ordering::Relaxed);
        self.shift
            .store(is_held(held, Modifier::Shift.keycodes()), Ordering::Relaxed);
        self.alt
            .store(is_held(held, Modifier::Alt.keycodes()), Ordering::Relaxed);
    }
}

/// Opens every device evdev finds under `/dev/input` that looks like a
/// keyboard (reports `KEY_A` — filters out mice/joysticks, which also emit
/// `EV_KEY` for their buttons but never letter keys) — **except our own**
/// `inject.rs` virtual device. That device advertises the same letter/
/// modifier capability set a real keyboard does, so without this exclusion
/// every key we inject would loop right back into the matcher below: a
/// shortcut bound to exactly `Ctrl+Alt+C` (plausible — that's PoE2's own
/// advanced-item-description combo) would re-trigger itself indefinitely,
/// and any injected modifier would pollute the global held-modifier state
/// for every other shortcut while it's down.
fn open_keyboards() -> Vec<Device> {
    evdev::enumerate()
        .filter_map(|(_path, device)| {
            if device.name() == Some(VIRTUAL_KEYBOARD_NAME) {
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
/// `set_game_active`) gates matching: ports upstream's `Shortcuts.ts`
/// `poeWindow.on("active-change", ...)` → `register()`/`unregister()`, so
/// a shortcut like `Shift + Space` only fires while PoE2 is the focused
/// window, not from every other application too. Modifier/held-key
/// bookkeeping still runs regardless of this flag — only the match-and-fire
/// step is skipped — so state stays accurate for whenever the game becomes
/// active again, instead of going stale while it's not.
pub async fn watch(
    bindings: Bindings,
    game_active: Arc<AtomicBool>,
    modifiers: Arc<ModifierState>,
    tx: mpsc::UnboundedSender<usize>,
) {
    let devices = open_keyboards();
    if devices.is_empty() {
        eprintln!("ee2-hyprland: no keyboard-like /dev/input device found — hotkeys disabled");
        return;
    }

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

    for device in devices {
        let fd = device.as_raw_fd();
        let events_tx = events_tx.clone();
        let mut stream = match device.into_event_stream() {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("ee2-hyprland: failed to open input device for async reads: {err}");
                continue;
            }
        };
        tokio::spawn(async move {
            loop {
                match stream.next_event().await {
                    Ok(event) => {
                        if events_tx.send((fd, DeviceMsg::Event(event))).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        eprintln!("ee2-hyprland: input device read error: {err}");
                        let _ = events_tx.send((fd, DeviceMsg::Closed));
                        return;
                    }
                }
            }
        });
    }
    drop(events_tx);

    let mut held: HashSet<(RawFd, KeyCode)> = HashSet::new();

    while let Some((fd, msg)) = events_rx.recv().await {
        let event = match msg {
            DeviceMsg::Closed => {
                held.retain(|(held_fd, _)| *held_fd != fd);
                modifiers.update(&held);
                continue;
            }
            DeviceMsg::Event(event) => event,
        };

        let EventSummary::Key(_, code, value) = event.destructure() else {
            continue;
        };
        match value {
            // Keydown (1) — record it, then check for a match. Repeat (2)
            // carries no new information for shortcut matching; ignore it.
            1 => {
                held.insert((fd, code));
                modifiers.update(&held);

                if !game_active.load(Ordering::Relaxed) {
                    continue;
                }

                let ctrl = is_held(&held, Modifier::Ctrl.keycodes());
                let shift = is_held(&held, Modifier::Shift.keycodes());
                let alt = is_held(&held, Modifier::Alt.keycodes());

                let current = bindings.lock().unwrap().clone();
                for (shortcut, index) in &current {
                    if shortcut.key == code
                        && shortcut.ctrl == ctrl
                        && shortcut.shift == shift
                        && shortcut.alt == alt
                        && tx.send(*index).is_err()
                    {
                        return;
                    }
                }
            }
            0 => {
                held.remove(&(fd, code));
                modifiers.update(&held);
            }
            _ => {}
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
