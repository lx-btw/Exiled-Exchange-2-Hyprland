//! Entrypoint for the Hyprland-native platform layer.
//!
//! See `PLAN.md` (repo root) for the implementation stages and
//! `HYPRLAND-NOTES.md` for the design rationale behind each module below.
//! Stage 0 is just this skeleton — every module is a stub until its stage lands.

mod clipboard;
mod compositor;
mod config;
mod input;
mod overlay;
mod proxy;
mod server;
mod state;
mod tray;

fn main() {
    eprintln!(
        "ee2-hyprland: not yet implemented — see PLAN.md for implementation stages"
    );
    std::process::exit(1);
}
