//! XDG paths and config loading.
//!
//! Reads/writes the same `config.json` the upstream renderer edits via
//! `CLIENT->MAIN::save-config` (`~/.config/exiled-exchange-2/apt-data/config.json`
//! — Electron's `app.getPath("userData")` resolves to `$XDG_CONFIG_HOME` or
//! `~/.config`, joined with `package.json`'s `name: "exiled-exchange-2"`),
//! so a rebind in-app takes effect without restarting this process. Ported
//! directly from upstream's `main/src/host-files/ConfigStore.ts` — same
//! path, same `isTemporary` behavior (see `save()`).
//!
//! Also `GameConfig`: reads `poe2_production_Config.ini` for `showModsKey`
//! (`ACTION_KEYS.show_advanced_item_descriptions`) — ported from upstream's
//! `main/src/host-files/GameConfig.ts`. Verified against a real installed
//! copy of this file on this system (Steam+Proton, App ID 2694490): UTF-8
//! BOM at the start (upstream's `contents.trimStart()` comment — "remove
//! BOM" — is not a hypothetical), `[ACTION_KEYS]`/`[WASD_ACTION_KEYS]` as
//! separate sections (only `[ACTION_KEYS]` is upstream's source of truth,
//! `[WASD_ACTION_KEYS]` is a different keybind profile and ignored, same as
//! upstream), and `show_advanced_item_descriptions=18` in its default,
//! unrebound state — `18` being the Windows virtual-key code for Alt (see
//! `ipc/KeyToCode.ts`), confirming the default really does round-trip to
//! this fork's own hardcoded `Alt` default.
//!
//! Stage 0/2/4/5 (see PLAN.md).

use std::io;
use std::path::PathBuf;

use crate::input::keymap::Modifier;

/// The `apt-data` folder itself (not `config.json`'s full path) — also
/// what `tray.rs`'s "Open Config Folder" opens, same target upstream's
/// `AppTray.ts` uses (`path.join(app.getPath("userData"), "apt-data")`).
pub fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME must be set");
            PathBuf::from(home).join(".config")
        });
    base.join("exiled-exchange-2").join("apt-data")
}

/// `overlayKey` from the saved config (renderer default: "Shift + Space").
pub async fn overlay_key() -> String {
    ConfigStore::new()
        .load()
        .await
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("overlayKey")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "Shift + Space".to_string())
}

/// Tracks the same one quirk upstream's `ConfigStore` has: once a temporary
/// save happens, every subsequent save (temporary or not) goes to a
/// `.tmp`-suffixed sibling path instead of the real config file, for the
/// rest of the process's life. This is how upstream avoids clobbering a
/// user's real config when it decides to save a temporary repaired-default
/// config (e.g. after a parse failure) — once that's happened, nothing
/// this process does should touch the original file again.
pub struct ConfigStore {
    path: PathBuf,
    is_tmp: bool,
}

impl ConfigStore {
    pub fn new() -> Self {
        Self {
            path: config_dir().join("config.json"),
            is_tmp: false,
        }
    }

    /// Async via `spawn_blocking`, not a plain sync call — this is invoked
    /// while holding `state::Overlay`'s shared lock (from `server.rs`'s
    /// `/config` handler, which the renderer's bootstrap blocks on and
    /// re-fires on every reload), so a blocking disk read here would tie up
    /// a tokio worker thread — and everything else sharing that lock —
    /// for the syscall's duration.
    pub async fn load(&self) -> Option<String> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || std::fs::read_to_string(path).ok())
            .await
            .unwrap_or(None)
    }

    pub async fn save(&mut self, contents: &str, is_temporary: bool) -> io::Result<()> {
        if is_temporary && !self.is_tmp {
            let mut path = self.path.clone().into_os_string();
            path.push(".tmp");
            self.path = path.into();
            self.is_tmp = true;
        }

        let path = self.path.clone();
        let contents = contents.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, contents)
        })
        .await
        .unwrap_or_else(|join_err| Err(io::Error::other(join_err)))
    }
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Same Linux candidate paths upstream's `GameConfig.ts` tries when the
/// renderer hasn't told us an explicit path via `HostConfig.gameConfig`:
/// the standalone client under `~/Documents`, or Steam+Proton's compat
/// prefix for PoE2's own App ID (`2694490` — the same one
/// `compositor.rs::game_class()` defaults to).
fn candidate_game_config_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        PathBuf::from(&home).join("Documents/My Games/Path of Exile 2/poe2_production_Config.ini"),
        PathBuf::from(&home).join(
            ".local/share/Steam/steamapps/compatdata/2694490/pfx/drive_c/users/steamuser/Documents/My Games/Path of Exile 2/poe2_production_Config.ini",
        ),
    ]
}

/// Finds `show_advanced_item_descriptions`'s raw value inside `[ACTION_KEYS]`
/// specifically — `[WASD_ACTION_KEYS]` is a separate keybind profile, not
/// upstream's source of truth for this.
fn find_show_mods_key_value(contents: &str) -> Option<String> {
    let mut in_action_keys = false;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_action_keys = section == "ACTION_KEYS";
            continue;
        }
        if in_action_keys {
            if let Some(value) = line.strip_prefix("show_advanced_item_descriptions=") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// `value` is PoE2's own ini encoding: a Windows virtual-key code, optionally
/// followed by a space and a modifier digit (`1`=Shift, `2`=Ctrl, `3`=Alt —
/// upstream's `parseConfigHotkey`). Only recognizes the common case this
/// fork can actually represent: a *bare* Ctrl/Shift/Alt binding with no
/// modifier of its own (VK codes 16/17/18 — `ipc/KeyToCode.ts`). A user who
/// rebinds `show_advanced_item_descriptions` to a non-modifier key, or to a
/// modifier+key combo, isn't representable by this fork's `Shortcut` model
/// (one base key plus up to three modifiers) without a design change to
/// that model — `read_show_mods_key_blocking` logs and keeps the previous
/// value rather than guessing.
fn modifier_from_ini_value(value: &str) -> Option<Modifier> {
    let mut parts = value.split_whitespace();
    let key_main = parts.next()?;
    if parts.next().is_some() {
        return None; // has its own modifier — not a bare single key
    }
    match key_main {
        "16" => Some(Modifier::Shift),
        "17" => Some(Modifier::Ctrl),
        "18" => Some(Modifier::Alt),
        _ => None,
    }
}

fn read_show_mods_key_blocking(requested_path: &str) -> Option<Modifier> {
    let path = if requested_path.is_empty() {
        match candidate_game_config_paths()
            .into_iter()
            .find(|p| p.is_file())
        {
            Some(path) => path,
            None => {
                eprintln!(
                    "ee2-hyprland: showModsKey: game config file not found in any default location"
                );
                return None;
            }
        }
    } else {
        PathBuf::from(requested_path)
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "ee2-hyprland: showModsKey: failed to read {}: {err}",
                path.display()
            );
            return None;
        }
    };
    // `contents.trimStart()` upstream — this file has a UTF-8 BOM.
    let contents = contents.trim_start_matches('\u{feff}');

    let Some(raw) = find_show_mods_key_value(contents) else {
        eprintln!(
            "ee2-hyprland: showModsKey: show_advanced_item_descriptions not found in {}",
            path.display()
        );
        return None;
    };

    let modifier = modifier_from_ini_value(&raw);
    if modifier.is_none() {
        eprintln!(
            "ee2-hyprland: showModsKey: \"{raw}\" isn't a bare Ctrl/Shift/Alt binding this fork can represent — keeping the previous value"
        );
    }
    modifier
}

/// `showModsKey`: the modifier PoE2 itself uses for
/// `show_advanced_item_descriptions` — `inject.rs`'s copy-item combo must
/// include it alongside `Ctrl+C`, or the game omits stat ranges from the
/// copied text (upstream `main/src/shortcuts/Shortcuts.ts:293`). Read from
/// the user's actual game config instead of assumed, since a user who
/// rebinds it away from the default `Alt` would otherwise get silently
/// wrong (incomplete) item text forever with no indication why.
pub struct GameConfig {
    show_mods_key: Modifier,
    last_requested_path: Option<String>,
}

impl GameConfig {
    pub fn new() -> Self {
        Self {
            show_mods_key: Modifier::Alt,
            last_requested_path: None,
        }
    }

    pub fn show_mods_key(&self) -> Modifier {
        self.show_mods_key
    }

    /// `requested_path` is `HostConfig.gameConfig` — empty string means
    /// "guess it" (the candidate list), same contract as upstream's
    /// `readConfig(cfg.gameConfig ?? "")`. Skips re-reading if the
    /// requested path hasn't changed since last time (upstream does this
    /// too — avoids a redundant file read on every `update-host-config`,
    /// which fires on every settings save, not just when this path
    /// actually changes). On any failure, logs and keeps whatever was last
    /// successfully read (or the `Alt` default) rather than resetting —
    /// a transient read failure shouldn't wipe out a previously-known-good
    /// value.
    pub async fn read(&mut self, requested_path: &str) {
        if self.last_requested_path.as_deref() == Some(requested_path) {
            return;
        }
        self.last_requested_path = Some(requested_path.to_string());

        let requested = requested_path.to_string();
        let key = tokio::task::spawn_blocking(move || read_show_mods_key_blocking(&requested))
            .await
            .ok()
            .flatten();

        if let Some(key) = key {
            self.show_mods_key = key;
        }
    }
}

impl Default for GameConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_alt_binding_parses() {
        // The exact line this system's real, installed poe2_production_Config.ini
        // has for an unrebound `show_advanced_item_descriptions` — verified
        // live against that file, see module docs.
        let ini = "[ACTION_KEYS]\nshow_advanced_item_descriptions=18\n";
        let raw = find_show_mods_key_value(ini).unwrap();
        assert_eq!(modifier_from_ini_value(&raw), Some(Modifier::Alt));
    }

    #[test]
    fn rebound_to_ctrl_or_shift_parses() {
        assert_eq!(modifier_from_ini_value("17"), Some(Modifier::Ctrl));
        assert_eq!(modifier_from_ini_value("16"), Some(Modifier::Shift));
    }

    #[test]
    fn rebound_to_non_modifier_key_is_unrepresentable() {
        // e.g. rebound to F1 (VK 112) with no modifier of its own.
        assert_eq!(modifier_from_ini_value("112"), None);
    }

    #[test]
    fn rebound_with_its_own_modifier_is_unrepresentable() {
        // e.g. rebound to Ctrl+F1 — "112 2" (F1, modifier digit 2 = Ctrl).
        assert_eq!(modifier_from_ini_value("112 2"), None);
    }

    #[test]
    fn only_action_keys_section_is_read_not_wasd_action_keys() {
        let ini = "[WASD_ACTION_KEYS]\nshow_advanced_item_descriptions=17\n\
                   [ACTION_KEYS]\nshow_advanced_item_descriptions=18\n";
        let raw = find_show_mods_key_value(ini).unwrap();
        assert_eq!(modifier_from_ini_value(&raw), Some(Modifier::Alt));
    }
}
