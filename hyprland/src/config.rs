//! XDG paths and config loading.
//!
//! Reads the same `config.json` the upstream renderer edits via
//! `CLIENT->MAIN::save-config` (`~/.config/exiled-exchange-2/apt-data/config.json`),
//! so a rebind in-app takes effect without restarting this process. Also the home
//! for reading `poe2_production_Config.ini` (`showModsKey` — see Stage 5, ports
//! upstream's `main/src/host-files/GameConfig.ts`).
//!
//! Stage 0/2 (see PLAN.md).
