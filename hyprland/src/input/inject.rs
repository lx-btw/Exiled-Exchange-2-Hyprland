//! `uinput` virtual keyboard, created once at startup, advertising a US-only
//! layout — sidesteps the user's `kb_layout = "us,ru"` /
//! `grp:alt_shift_toggle` setup entirely, since layout switching is
//! per-physical-device and a virtual device that never sends an
//! Alt+Shift-toggle sequence of its own simply never leaves XKB group 0
//! ("us", the first configured layout) — no special pinning code needed,
//! this was already verified empirically on this system by the prior
//! prototype.
//!
//! `Ctrl+<showModsKey>+C` sequence (not just `Ctrl+C` — the game's own
//! `show_advanced_item_descriptions` modifier, `Alt` by default, must be
//! held too or the copied text is missing stat ranges) ports the
//! 30ms-stagger timing the prior prototype's `host.mjs::injectCopy()`
//! already empirically tuned: `LEFTCTRL+<mods> down, wait 30ms, C down,
//! wait 30ms, C up, wait 30ms, <mods>+LEFTCTRL up`. `showModsKey` is read
//! from the user's actual `poe2_production_Config.ini` (`config.rs`'s
//! `GameConfig`, Stage 5) rather than assumed — `copy_item()` takes it as a
//! parameter, called fresh each time from whatever `state.rs` currently has
//! on file.
//!
//! Deliberately NOT XTEST and NOT `hl.dsp.send_shortcut` — both are
//! known-broken on Hyprland for this (desynced modifier state / dropped
//! modifiers, respectively). Do not reintroduce either.
//!
//! Stage 2/5 (see PLAN.md). Design rationale: HYPRLAND-NOTES.md §4.

use std::io;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

use super::keymap::{base_keys, modifier_keys, Modifier};

const INJECT_STAGGER: Duration = Duration::from_millis(30);

/// This device's `evdev` name — advertises the full key vocabulary
/// `hotkeys.rs` also listens for, so `hotkeys.rs` excludes exactly this
/// name from the devices it opens. Without that exclusion, an injected
/// `Ctrl+Alt+C` would loop back into the hotkey matcher on its own device
/// and (for a shortcut bound to exactly that combo, which is plausible
/// since it's PoE2's own advanced-item-description modifier) re-trigger the
/// very action that injected it.
pub const VIRTUAL_KEYBOARD_NAME: &str = "ee2-hyprland virtual keyboard";

pub struct VirtualKeyboard {
    device: VirtualDevice,
}

impl VirtualKeyboard {
    /// Creates the virtual keyboard. Must succeed before `hotkeys.rs` starts
    /// listening — a hotkey firing with nothing to inject into is a silent
    /// no-op from the user's perspective, better to fail loudly at startup.
    pub fn new() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in base_keys().chain(modifier_keys()) {
            keys.insert(key);
        }

        let device = VirtualDevice::builder()?
            .name(VIRTUAL_KEYBOARD_NAME)
            .with_keys(&keys)?
            .build()?;

        Ok(Self { device })
    }

    fn key(&mut self, code: KeyCode, down: bool) -> io::Result<()> {
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code.0, down as i32)])
    }

    /// `Ctrl+<mods_key>+C` — PoE2's copy-item-text combo. `mods_key`
    /// (upstream's `showModsKey`, `config::GameConfig::show_mods_key()`) is
    /// required alongside `Ctrl+C`: without it PoE2's
    /// `show_advanced_item_descriptions` mode is off and the copied text is
    /// missing stat ranges (upstream `main/src/shortcuts/Shortcuts.ts:293`).
    /// If `mods_key` happens to be `Ctrl` itself (an unusual rebind), it's
    /// not injected twice — `Ctrl` alone already covers it.
    pub async fn copy_item(&mut self, mods_key: Modifier) -> io::Result<()> {
        let ctrl = Modifier::Ctrl.left_keycode();
        let mods = mods_key.left_keycode();

        let result = self.copy_item_sequence(ctrl, mods).await;

        // Release unconditionally, even if the sequence above failed
        // partway through — a transient uinput write error must never
        // leave a modifier reported "held" on the virtual device, since
        // that would corrupt every keypress it delivers afterward until
        // the process restarts. Releasing an already-up key is a no-op, so
        // this is safe regardless of how far the sequence actually got.
        if mods != ctrl {
            let _ = self.key(mods, false);
        }
        let _ = self.key(ctrl, false);

        result
    }

    async fn copy_item_sequence(&mut self, ctrl: KeyCode, mods: KeyCode) -> io::Result<()> {
        self.key(ctrl, true)?;
        if mods != ctrl {
            self.key(mods, true)?;
        }
        tokio::time::sleep(INJECT_STAGGER).await;

        self.key(KeyCode::KEY_C, true)?;
        tokio::time::sleep(INJECT_STAGGER).await;

        self.key(KeyCode::KEY_C, false)?;
        tokio::time::sleep(INJECT_STAGGER).await;

        Ok(())
    }

    /// A plain key tap: down, stagger, up, stagger — same `INJECT_STAGGER`
    /// timing as `copy_item`, reused here rather than re-tuned since it's
    /// the same virtual device and the same class of "give the game time to
    /// see each edge" concern.
    async fn tap(&mut self, code: KeyCode) -> io::Result<()> {
        self.key(code, true)?;
        tokio::time::sleep(INJECT_STAGGER).await;
        self.key(code, false)?;
        tokio::time::sleep(INJECT_STAGGER).await;
        Ok(())
    }

    /// `Ctrl+<code>`, released unconditionally afterward (even on a
    /// mid-sequence write error) for the same reason `copy_item` does:
    /// a `uinput` write failure must never leave Ctrl reported "held" on
    /// the virtual device.
    async fn tap_with_ctrl(&mut self, code: KeyCode) -> io::Result<()> {
        let ctrl = Modifier::Ctrl.left_keycode();
        let result = async {
            self.key(ctrl, true)?;
            tokio::time::sleep(INJECT_STAGGER).await;
            self.tap(code).await
        }
        .await;
        let _ = self.key(ctrl, false);
        result
    }

    /// PoE2's stash-search combo, ported from upstream's
    /// `text-box.ts::stashSearch()`: `Ctrl+F` opens the stash search box,
    /// `Ctrl+V` pastes whatever `clipboard::write_text` already placed on
    /// the clipboard, `Enter` confirms. Caller's job to have already made
    /// sure the game (not the overlay) has keyboard focus — see
    /// `state.rs::run_stash_search`.
    pub async fn stash_search(&mut self) -> io::Result<()> {
        self.tap_with_ctrl(KeyCode::KEY_F).await?;
        self.tap_with_ctrl(KeyCode::KEY_V).await?;
        self.tap(KeyCode::KEY_ENTER).await?;
        Ok(())
    }

    /// `stashScroll` (Phase C, PLAN.md): ports `Shortcuts.ts`'s Ctrl+wheel
    /// tab-switch — a bare `ArrowRight`/`ArrowLeft` tap, no modifier
    /// (`Ctrl` is the *trigger* condition `area_tracker.rs` already checked
    /// before calling this, not part of the injected combo itself, matching
    /// upstream's own `uIOhook.keyTap(UiohookKey.ArrowRight)` call with no
    /// modifier argument).
    pub async fn stash_scroll_tab(&mut self, right: bool) -> io::Result<()> {
        self.tap(if right {
            KeyCode::KEY_RIGHT
        } else {
            KeyCode::KEY_LEFT
        })
        .await
    }
}
