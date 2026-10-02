//! keyd-style capture mechanics for `input/hotkeys.rs`: the mirror `uinput`
//! device everything a grabbed physical keyboard doesn't consume gets
//! re-emitted through, the per-device grab/ungrab bookkeeping, and the
//! Hyprland-IPC XKB layout sync a grab/ungrab transition needs. See
//! `hotkeys.rs`'s module docs for the full capture policy (what gets
//! grabbed, when, and why) — this module is the "how", not the "whether".
//!
//! Design rationale: `HYPRLAND-NOTES.md` §3 (capture policy) and §9
//! (per-device XKB layout sync — includes the two things confirmed live
//! against this system rather than assumed: the Hyprland device-name
//! mangling `hypr_device_name()` reproduces, and why `switchxkblayout` has
//! to be sent without a `dispatch` prefix on this system's config).
//!
//! Stage 5, H2A (see PLAN.md).

use crate::logger::ee_log;
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode};
use tokio::sync::mpsc;

use crate::compositor;

/// This device's name — excluded from `hotkeys.rs::open_keyboards()`'s
/// enumeration the same way `inject.rs::VIRTUAL_KEYBOARD_NAME` already is.
/// Without the exclusion, everything re-emitted here would loop straight
/// back into the matcher (and the grab-candidate list) as if a second
/// physical keyboard had appeared.
pub const MIRROR_KEYBOARD_NAME: &str = "ee2-hyprland input passthrough";

/// The `uinput` device everything a grabbed physical keyboard *doesn't*
/// consume gets re-emitted through. Deliberately a second device from
/// `inject.rs`'s `VirtualKeyboard` (the `Ctrl+Alt+C` copy-item injector) —
/// mixing that device's own timed sequences with live passthrough typing
/// arriving from a grabbed keyboard at the same time would race the two.
struct Mirror {
    device: VirtualDevice,
}

impl Mirror {
    /// `keyboards` is every physical device about to be watched, read
    /// *before* any of them are moved into their own reader task — the
    /// mirror's capability set is the union of everything they can each
    /// individually produce, not `keymap.rs`'s curated `KEY_TABLE` (which
    /// has no punctuation/symbol keys, and would silently drop them from
    /// chat text typed through a grabbed keyboard). `uinput` requires the
    /// full capability set declared upfront, same constraint
    /// `inject.rs::VirtualKeyboard::new()` documents.
    fn new(keyboards: &[Device]) -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for device in keyboards {
            if let Some(supported) = device.supported_keys() {
                for key in supported.iter() {
                    keys.insert(key);
                }
            }
        }

        let device = VirtualDevice::builder()?
            .name(MIRROR_KEYBOARD_NAME)
            .with_keys(&keys)?
            .build()?;

        Ok(Self { device })
    }

    /// Re-emits one key edge (`value`: 0 up, 1 down, 2 repeat) exactly as
    /// read from the physical device. `emit()` appends its own
    /// `SYN_REPORT`, same as `inject.rs`'s single-event emit calls.
    fn forward(&mut self, code: KeyCode, value: i32) {
        if let Err(err) = self
            .device
            .emit(&[InputEvent::new(EventType::KEY.0, code.0, value)])
        {
            ee_log!("mirror device emit failed: {err}");
        }
    }
}

/// Hyprland's own device-name mangling (`keyboards[].name` in `hyprctl -j
/// devices`): lowercase, non-alphanumeric runs collapsed to a single `-`,
/// trimmed. Needed because `compositor::keyboards()`/`switchxkblayout`
/// address a device by this mangled name, but `evdev::Device::name()`
/// returns the raw kernel name, and there's no vid/pid/phys field in
/// `hyprctl -j devices`'s JSON to join on instead. Confirmed empirically
/// against this system's live `hyprctl -j devices` output (2026-09-24, see
/// this function's own unit test) — not assumed from source.
fn hypr_device_name(evdev_name: &str) -> String {
    let mut out = String::with_capacity(evdev_name.len());
    let mut last_was_dash = true; // swallow leading separators
    for c in evdev_name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

/// Reads `hypr_name`'s current XKB layout index and applies it to the
/// mirror device — called right before a grab engages, so a device grabbed
/// mid-`ru` doesn't suddenly type through the mirror's own independent
/// (always `us`-default) XKB state.
async fn sync_mirror_to_physical(hypr_name: &str) {
    match compositor::keyboards().await {
        Ok(list) => match list.iter().find(|d| d.name == hypr_name) {
            Some(dev) => {
                // `switchxkblayout` addresses devices by Hyprland's own
                // mangled name, same as `j/devices` reports them — a plain
                // `MIRROR_KEYBOARD_NAME` (with a literal space) never
                // matches anything, confirmed live (2026-09-24, H2A
                // verification): the raw constant was used here
                // unmangled, so this call silently targeted a
                // nonexistent device name every single time (Hyprland's
                // socket still replies without an IO error, so nothing
                // surfaced it) — caught only via the *other* direction's
                // symmetric lookup logging "mirror device not found".
                if let Err(err) = compositor::switchxkblayout(
                    &hypr_device_name(MIRROR_KEYBOARD_NAME),
                    dev.active_layout_index,
                )
                .await
                {
                    ee_log!("capture: failed to sync mirror layout to \"{hypr_name}\": {err}");
                }
            }
            None => ee_log!(
                "capture: Hyprland reports no device named \"{hypr_name}\", skipping layout sync"
            ),
        },
        Err(err) => {
            ee_log!("capture: failed to query j/devices for layout sync: {err}")
        }
    }
}

/// The reverse of `sync_mirror_to_physical` — called right after an ungrab,
/// so the physical device gets back whatever layout the user toggled to
/// while typing through the mirror.
async fn sync_physical_to_mirror(hypr_name: &str) {
    let mirror_hypr_name = hypr_device_name(MIRROR_KEYBOARD_NAME);
    match compositor::keyboards().await {
        Ok(list) => match list.iter().find(|d| d.name == mirror_hypr_name) {
            Some(dev) => {
                if let Err(err) =
                    compositor::switchxkblayout(hypr_name, dev.active_layout_index).await
                {
                    ee_log!("capture: failed to restore \"{hypr_name}\" layout: {err}");
                }
            }
            None => {
                ee_log!("capture: mirror device not found in j/devices, skipping layout restore")
            }
        },
        Err(err) => {
            ee_log!("capture: failed to query j/devices for layout restore: {err}")
        }
    }
}

/// Owns the mirror device plus every piece of per-device bookkeeping
/// `hotkeys.rs`'s central loop needs to drive capture: which devices are
/// currently grabbed, each device's Hyprland-mangled name (for layout
/// sync), each device's command channel to its own reader task (grab is a
/// syscall on that task's own `Device` handle — see `hotkeys.rs::watch()`),
/// and which keys are currently "eaten" per device (a matched hotkey's own
/// trigger key, held down — its repeat/release must stay eaten too).
pub struct Capture {
    mirror: Mirror,
    names: HashMap<RawFd, String>,
    senders: HashMap<RawFd, mpsc::UnboundedSender<bool>>,
    grabbed: HashMap<RawFd, bool>,
    eaten: HashMap<RawFd, HashSet<KeyCode>>,
}

impl Capture {
    /// `devices` must be every device `hotkeys.rs::open_keyboards()` is
    /// about to watch, read before any of them are moved into their own
    /// reader task (same requirement `Mirror::new` has, for the same
    /// reason).
    pub fn new(devices: &[Device]) -> io::Result<Self> {
        let mirror = Mirror::new(devices)?;
        let mut names = HashMap::new();
        for device in devices {
            if let Some(name) = device.name() {
                names.insert(device.as_raw_fd(), hypr_device_name(name));
            }
        }
        Ok(Self {
            mirror,
            names,
            senders: HashMap::new(),
            grabbed: HashMap::new(),
            eaten: HashMap::new(),
        })
    }

    /// Registers `fd`'s reader task's grab-command channel — must be called
    /// once per device, right after that device's reader task is spawned
    /// (`hotkeys.rs::watch()`), before any transition can target it.
    pub fn register_device(&mut self, fd: RawFd, sender: mpsc::UnboundedSender<bool>) {
        self.senders.insert(fd, sender);
    }

    /// Drops every entry for `fd` — called when that device's reader task
    /// exits (error or unplug), so a stale "grabbed"/"eaten" entry for a
    /// now-gone device can't linger.
    pub fn forget_device(&mut self, fd: RawFd) {
        self.senders.remove(&fd);
        self.grabbed.remove(&fd);
        self.names.remove(&fd);
        self.eaten.remove(&fd);
    }

    pub fn is_grabbed(&self, fd: RawFd) -> bool {
        *self.grabbed.get(&fd).unwrap_or(&false)
    }

    pub fn forward(&mut self, code: KeyCode, value: i32) {
        self.mirror.forward(code, value);
    }

    pub fn mark_eaten(&mut self, fd: RawFd, code: KeyCode) {
        self.eaten.entry(fd).or_default().insert(code);
    }

    pub fn is_eaten(&self, fd: RawFd, code: KeyCode) -> bool {
        self.eaten.get(&fd).is_some_and(|set| set.contains(&code))
    }

    /// Removes `code` from `fd`'s eaten set, if present, and reports
    /// whether it was — so the caller (a keyup) knows not to forward the
    /// release of a key whose press was consumed.
    pub fn take_eaten(&mut self, fd: RawFd, code: KeyCode) -> bool {
        self.eaten.get_mut(&fd).is_some_and(|set| set.remove(&code))
    }

    /// Moves `fd` toward `want` (grabbed/ungrabbed) if it isn't already
    /// there — but only if nothing is currently held on that device
    /// (`held`, `hotkeys.rs`'s own per-`(fd, code)` tracking filtered to
    /// this fd); otherwise a no-op, left for the next call this device's
    /// own next key-release triggers. See `hotkeys.rs`'s module docs for
    /// why this deferral matters (a stuck-modifier failure mode, not just
    /// tidiness).
    pub async fn maybe_transition(
        &mut self,
        fd: RawFd,
        want: bool,
        held: &HashSet<(RawFd, KeyCode)>,
    ) {
        if self.is_grabbed(fd) == want {
            return;
        }
        if held.iter().any(|(held_fd, _)| *held_fd == fd) {
            return;
        }
        let Some(sender) = self.senders.get(&fd) else {
            return;
        };

        match self.names.get(&fd) {
            Some(hypr_name) if want => sync_mirror_to_physical(hypr_name).await,
            Some(hypr_name) => sync_physical_to_mirror(hypr_name).await,
            None => ee_log!(
                "capture: no Hyprland device name resolved for fd {fd}, skipping layout sync"
            ),
        }

        if let Err(err) = sender.send(want) {
            ee_log!(
                "capture: failed to send {} command to device reader task: {err}",
                if want { "grab" } else { "ungrab" }
            );
            return;
        }
        self.grabbed.insert(fd, want);
        ee_log!(
            "capture: {} input device (fd {fd})",
            if want { "grabbed" } else { "released" }
        );
    }

    /// `maybe_transition` for every device in `fds` — called on startup and
    /// whenever `game_active` changes, since that's a level change (not
    /// itself a per-device key event) that every currently-idle device
    /// needs to react to immediately rather than waiting for its next
    /// keystroke.
    pub async fn recheck_all(
        &mut self,
        fds: &[RawFd],
        want: bool,
        held: &HashSet<(RawFd, KeyCode)>,
    ) {
        for &fd in fds {
            self.maybe_transition(fd, want, held).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hypr_device_name, MIRROR_KEYBOARD_NAME};

    #[test]
    fn matches_live_hyprland_device_names() {
        // Captured live from this system's `hyprctl -j devices`
        // (2026-09-24) — not guessed at from the mangling algorithm alone.
        assert_eq!(
            hypr_device_name("SteelSeries SteelSeries Apex 7 TKL"),
            "steelseries-steelseries-apex-7-tkl"
        );
        assert_eq!(
            hypr_device_name("ASUSTeK ROG Gladius III Wireless Consumer Control"),
            "asustek-rog-gladius-iii-wireless-consumer-control"
        );
        assert_eq!(hypr_device_name("Power Button"), "power-button");
    }

    /// Regression test for a real bug found in the first live H2A
    /// verification (2026-09-24): `sync_mirror_to_physical`/
    /// `sync_physical_to_mirror` used to compare against the raw
    /// `MIRROR_KEYBOARD_NAME` constant (a literal space in it) instead of
    /// its mangled form — so the mirror device was never found in
    /// `j/devices`, and `switchxkblayout` was never once sent a name that
    /// resolved to anything, on every single one of 15 real grab/ungrab
    /// cycles in that session's log. Both call sites now go through
    /// `hypr_device_name` — this pins the exact mangled string they
    /// resolve to.
    #[test]
    fn mirror_name_is_mangled_before_use() {
        assert_eq!(
            hypr_device_name(MIRROR_KEYBOARD_NAME),
            "ee2-hyprland-input-passthrough"
        );
    }
}
