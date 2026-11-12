//! keyd-style capture mechanics for `input/hotkeys.rs`: the mirror `uinput`
//! device everything a grabbed physical keyboard doesn't consume gets
//! re-emitted through, the per-device grab/ungrab bookkeeping, and the
//! Hyprland-IPC XKB layout sync a grab/ungrab transition needs. See
//! `hotkeys.rs`'s module docs for the full capture policy (what gets
//! grabbed, when, and why) — this module is the "how", not the "whether".
//!
//! Design rationale: `HYPRLAND-NOTES.md` §3 (capture policy) and §8
//! (per-device XKB layout sync — including the Hyprland device-name
//! mangling `hypr_device_name()` reproduces, and why `switchxkblayout` has
//! to be sent without a `dispatch` prefix).

use super::devices::DeviceId;
use crate::logger::ee_log;
use std::collections::{HashMap, HashSet};
use std::io;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
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
    // Declare all known keyboard keys once, including keys on future keyboards.
    fn new() -> io::Result<Self> {
        let keys = mirror_keys();

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

fn mirror_keys() -> AttributeSet<KeyCode> {
    (1..0x300)
        .map(KeyCode::new)
        .filter(|key| format!("{key:?}").starts_with("KEY_"))
        .collect()
}

pub(super) fn can_mirror(device: &evdev::Device) -> bool {
    let keys = mirror_keys();
    if device
        .supported_relative_axes()
        .is_some_and(|axes| axes.iter().next().is_some())
    {
        return false;
    }
    device
        .supported_keys()
        .is_some_and(|supported| supported.iter().all(|key| keys.contains(key)))
}

// A shared mirror must release a key only after its last physical holder.
fn update_mirrored(
    held: &mut HashSet<(DeviceId, KeyCode)>,
    id: DeviceId,
    code: KeyCode,
    value: i32,
) -> bool {
    if value == 1 {
        held.insert((id, code));
    }
    if value == 0 {
        held.remove(&(id, code));
        return !held.iter().any(|(_, key)| *key == code);
    }
    true
}

/// Hyprland's own device-name mangling (`keyboards[].name` in `hyprctl -j
/// devices`): lowercase, non-alphanumeric runs collapsed to a single `-`,
/// trimmed. Needed because `compositor::keyboards()`/`switchxkblayout`
/// address a device by this mangled name, but `evdev::Device::name()`
/// returns the raw kernel name, and there's no vid/pid/phys field in
/// `hyprctl -j devices`'s JSON to join on instead. Checked against real
/// `hyprctl -j devices` output (see this function's unit test), not
/// derived from Hyprland's source.
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
                // mangled name, same as `j/devices` reports them — the raw
                // `MIRROR_KEYBOARD_NAME` (with a literal space) never
                // matches anything, and Hyprland replies without an error
                // for a nonexistent device, so an unmangled name here
                // would fail silently.
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
    names: HashMap<DeviceId, String>,
    senders: HashMap<DeviceId, mpsc::UnboundedSender<bool>>,
    grabbed: HashMap<DeviceId, bool>,
    eaten: HashMap<DeviceId, HashSet<KeyCode>>,
    mirrored: HashSet<(DeviceId, KeyCode)>,
    pending: HashSet<DeviceId>,
}

impl Capture {
    pub fn new() -> io::Result<Self> {
        let mirror = Mirror::new()?;
        let names = HashMap::new();
        Ok(Self {
            mirror,
            names,
            senders: HashMap::new(),
            grabbed: HashMap::new(),
            eaten: HashMap::new(),
            mirrored: HashSet::new(),
            pending: HashSet::new(),
        })
    }

    /// Registers `fd`'s reader task's grab-command channel — must be called
    /// once per device, right after that device's reader task is spawned
    /// (`hotkeys.rs::watch()`), before any transition can target it.
    pub fn register_device(
        &mut self,
        fd: DeviceId,
        name: Option<&str>,
        sender: mpsc::UnboundedSender<bool>,
    ) {
        if let Some(name) = name {
            self.names.insert(fd, hypr_device_name(name));
        }
        self.senders.insert(fd, sender);
    }

    /// Drops every entry for `fd` — called when that device's reader task
    /// exits (error or unplug), so a stale "grabbed"/"eaten" entry for a
    /// now-gone device can't linger.
    pub fn forget_device(&mut self, fd: DeviceId) {
        let keys: Vec<_> = self
            .mirrored
            .iter()
            .filter_map(|(id, key)| (*id == fd).then_some(*key))
            .collect();
        for key in keys {
            self.forward(fd, key, 0);
        }
        self.pending.remove(&fd);
        self.senders.remove(&fd);
        self.grabbed.remove(&fd);
        self.names.remove(&fd);
        self.eaten.remove(&fd);
    }

    pub fn is_grabbed(&self, fd: DeviceId) -> bool {
        *self.grabbed.get(&fd).unwrap_or(&false)
    }

    pub fn forward(&mut self, fd: DeviceId, code: KeyCode, value: i32) {
        if !update_mirrored(&mut self.mirrored, fd, code, value) {
            return;
        }
        self.mirror.forward(code, value);
    }

    pub fn acknowledge(&mut self, fd: DeviceId, grabbed: bool) {
        self.pending.remove(&fd);
        if self.is_grabbed(fd) != grabbed {
            ee_log!(
                "capture: {} input device (reader {fd})",
                if grabbed { "grabbed" } else { "released" }
            );
        }
        self.grabbed.insert(fd, grabbed);
    }

    pub fn mark_eaten(&mut self, fd: DeviceId, code: KeyCode) {
        self.eaten.entry(fd).or_default().insert(code);
    }

    pub fn is_eaten(&self, fd: DeviceId, code: KeyCode) -> bool {
        self.eaten.get(&fd).is_some_and(|set| set.contains(&code))
    }

    /// Removes `code` from `fd`'s eaten set, if present, and reports
    /// whether it was — so the caller (a keyup) knows not to forward the
    /// release of a key whose press was consumed.
    pub fn take_eaten(&mut self, fd: DeviceId, code: KeyCode) -> bool {
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
        fd: DeviceId,
        want: bool,
        held: &HashSet<(DeviceId, KeyCode)>,
    ) {
        if self.is_grabbed(fd) == want || self.pending.contains(&fd) {
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
        self.pending.insert(fd);
    }

    /// `maybe_transition` for every device in `fds` — called on startup and
    /// whenever `game_active` changes, since that's a level change (not
    /// itself a per-device key event) that every currently-idle device
    /// needs to react to immediately rather than waiting for its next
    /// keystroke.
    pub async fn recheck_all(
        &mut self,
        fds: &[DeviceId],
        want: bool,
        held: &HashSet<(DeviceId, KeyCode)>,
    ) {
        for &fd in fds {
            self.maybe_transition(fd, want, held).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hypr_device_name, mirror_keys, update_mirrored, MIRROR_KEYBOARD_NAME};
    use evdev::KeyCode;
    use std::collections::HashSet;

    #[test]
    fn removed_keyboard_releases_only_its_last_mirrored_keys() {
        let mut held = HashSet::new();
        let ctrl = KeyCode::KEY_LEFTCTRL;
        let alt = KeyCode::KEY_LEFTALT;
        update_mirrored(&mut held, 1, ctrl, 1);
        update_mirrored(&mut held, 2, ctrl, 1);
        update_mirrored(&mut held, 1, alt, 1);
        assert!(!update_mirrored(&mut held, 1, ctrl, 0));
        assert!(update_mirrored(&mut held, 1, alt, 0));
        assert!(held.contains(&(2, ctrl)));
        assert!(update_mirrored(&mut held, 2, ctrl, 0));
        assert!(held.is_empty());
    }

    #[test]
    fn mirror_covers_extended_keyboard_keys_without_pointer_buttons() {
        let keys = mirror_keys();
        for key in [
            KeyCode::KEY_A,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_F24,
            KeyCode::KEY_VOLUMEUP,
        ] {
            assert!(keys.contains(key));
        }
        for key in [
            KeyCode::KEY_RESERVED,
            KeyCode::BTN_LEFT,
            KeyCode::BTN_SOUTH,
            KeyCode::BTN_DPAD_UP,
        ] {
            assert!(!keys.contains(key));
        }
    }

    #[test]
    fn matches_live_hyprland_device_names() {
        // Real names from `hyprctl -j devices` — not guessed at from the
        // mangling algorithm alone.
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

    /// `sync_mirror_to_physical`/`sync_physical_to_mirror` must look the
    /// mirror up by its mangled name — the raw `MIRROR_KEYBOARD_NAME` (a
    /// literal space in it) never matches anything in `j/devices`, which
    /// silently breaks the layout sync. This pins the exact mangled string
    /// both call sites resolve to.
    #[test]
    fn mirror_name_is_mangled_before_use() {
        assert_eq!(
            hypr_device_name(MIRROR_KEYBOARD_NAME),
            "ee2-hyprland-input-passthrough"
        );
    }
}
