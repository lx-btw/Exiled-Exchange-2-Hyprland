# Exiled Exchange 2 — Hyprland

A Hyprland-native fork of [Kvan7/Exiled-Exchange-2](https://github.com/Kvan7/Exiled-Exchange-2),
the Path of Exile 2 price-check overlay (itself a fork of
[Awakened PoE Trade](https://github.com/SnosMe/awakened-poe-trade) by SnosMe). It
targets **Hyprland sessions only**, with native Arch packages and an x86_64
AppImage. The game can use native Wayland or Xwayland. Other distributions need
community testing; Windows, macOS and other desktop sessions are outside this fork.

## What's different from upstream

Upstream's Electron host (`main/`) is replaced by a single Rust binary,
`ee2-hyprland` ([`hyprland/`](../hyprland)). The Vue UI (`renderer/`) and the
`ipc/` protocol are used **unmodified**: the binary embeds the upstream renderer
build and serves it over the same HTTP + WebSocket protocol (`ipc/types.ts`) that
upstream's Electron host speaks, so price checking, widgets and settings behave as
upstream's do.

| Concern | Upstream (Electron / X11) | This fork |
|---|---|---|
| Overlay window | `electron-overlay-window` (X11 attach) | GTK4 + gtk4-layer-shell surface hosting a WebKitGTK 6 WebView |
| Hotkeys | Electron `globalShortcut` (`XGrabKey`) | evdev; keyboards are grabbed (`EVIOCGRAB`) only while the game is focused, unmatched keys re-emitted through a uinput mirror |
| Key injection (item copy, chat, stash search) | `uiohook-napi` (XTEST) | uinput virtual keyboard |
| Clipboard | Chromium's X11 clipboard | wlr-data-control for Wayland; xsel for Xwayland |
| Game window / monitor state | X11 | Hyprland IPC sockets (`.socket.sock`, `.socket2.sock`) |
| Tray | Electron `Tray` | StatusNotifierItem via `ksni` |

The rationale for every design decision is in
[`hyprland/HYPRLAND-NOTES.md`](../hyprland/HYPRLAND-NOTES.md).

## Install

### Prebuilt Arch package

Download `ee2-hyprland-*.pkg.tar.zst` from this repository's
[Releases](https://github.com/lx-btw/Exiled-Exchange-2/releases) (built by CI from version tags) and install it:

```sh
sudo pacman -U ee2-hyprland-*.pkg.tar.zst
```

### AppImage (x86_64)

Download the `ee2-hyprland-appimage` artifact from a successful
[hyprland-ci run](https://github.com/lx-btw/Exiled-Exchange-2/actions/workflows/hyprland-ci.yml)
and extract the ZIP. Artifacts are retained for seven days. Fork releases tagged
`hyprland-v<version>` also include the AppImage and its `.sha256` file.
For version 0.1.0, run from the directory containing both files:

```sh
sha256sum -c ee2-hyprland-0.1.0-x86_64.AppImage.sha256
chmod +x ee2-hyprland-0.1.0-x86_64.AppImage
./ee2-hyprland-0.1.0-x86_64.AppImage
```

GTK, WebKitGTK, software Mesa and the Xwayland clipboard helper are bundled.
The device permissions below still apply. See the [AppImage guide](../hyprland/packaging/appimage/README.md)
for FUSE extraction, Ubuntu AppArmor diagnostics and compatibility reports.

### From source

```sh
git clone https://github.com/lx-btw/Exiled-Exchange-2.git
cd Exiled-Exchange-2/hyprland/packaging
makepkg -f
sudo pacman -U ee2-hyprland-*.pkg.tar.zst
```

The PKGBUILD builds from the checkout's working tree (no `source=()` array): it runs
the renderer build (`npm ci`, `npm run make-index-files`, `npm run build`) and then
`cargo build --release --locked`. Build dependencies: `nodejs`, `npm`, `rust`.

Runtime dependencies: `gtk4`, `gtk4-layer-shell`, `webkitgtk-6.0`.

### Device access

Neither format installs udev rules, so the user running the overlay needs:

- `/dev/uinput` read+write — the virtual keyboard. Without it the app refuses to start.
- `/dev/input/event*` read — hotkeys and hover tracking. Without it the overlay runs
  with hotkeys disabled (logged).

On many systems this already works (for example, `steam` ships a `uaccess` rule for
`/dev/uinput`). Otherwise grant it yourself, e.g. via the `input` group. Details and
example rules: [`hyprland/packaging/README.md`](../hyprland/packaging/README.md).

### Running

- Native Wayland (`PROTON_ENABLE_WAYLAND=1 %command%`) and Xwayland
  (`PROTON_ENABLE_WAYLAND=0 %command%`) game classes are recognized automatically.
  Xwayland clipboard operations require a working `DISPLAY`. The AppImage
  includes `xsel`; native-package users need the optional `xsel` dependency.
- Optionally tie it to the game instead: `PROTON_ENABLE_WAYLAND=1 ee2-hyprland-wrap %command%`
  starts the service before the game and stops it when the game exits.
- Logs: `journalctl --user -u ee2-hyprland -b`.

Settings are stored in the same file as upstream's
(`~/.config/exiled-exchange-2/apt-data/config.json`), so an existing upstream
configuration carries over.

## Known limitations

The main user-visible limitations are listed below. See the
[maintenance guide](../hyprland/MAINTENANCE.md#diagnostic-caveats) for diagnostic caveats.

- `price-check.builtinBrowser` is unsupported; trade links open in the system browser.
  Keep that setting off.
- Input devices are rediscovered every second; keyboard and mouse unplug/replug
  recovery has been checked in game.
- Clipboard data-control connections retry after disconnects. If the compositor
  itself restarts, the app's primary GTK Wayland connection still requires an
  application restart.
- Caps Lock / Num Lock LEDs don't update while the game is focused (the lock state
  itself is correct).
- Hotkeys can use letters, digits, F-keys and common named keys; no numpad/media keys,
  and a bare modifier can't be a hotkey.
- AppImage compatibility on Debian, Ubuntu, Fedora and NixOS needs community
  testing. Host device permissions and sandbox policy still apply; see the
  [AppImage guide](../hyprland/packaging/appimage/README.md).
- A foreign layer-shell client taking the keyboard (e.g. rofi) while the overlay is
  interactive is not detected as focus loss.
- No self-updater: updates come from installing a newer package.

## Other compositors

Not planned, but the architecture doesn't rule it out. Only
`hyprland/src/compositor.rs` is Hyprland-specific: game window focus, monitor layout,
cursor position and per-device keyboard layout switching (`switchxkblayout`). The
rest — layer-shell, wlr-data-control, evdev and uinput — is compositor-agnostic.

- **sway, river, niri**: they implement layer-shell and data-control, so a port would
  mostly mean an alternative IPC backend for `compositor.rs`.
- **KDE Plasma**: layer-shell and data-control exist, but window focus and geometry
  would need KWin scripting (see
  [LP0101/Exiled-Exchange-2](https://github.com/LP0101/Exiled-Exchange-2), a KDE
  Wayland fork).
- **GNOME**: impractical — Mutter offers neither layer-shell nor data-control.

## Development

Fork rules and orientation: [`AGENTS.md`](../AGENTS.md); CI, releases, upstream
updates and bug triage: [`hyprland/MAINTENANCE.md`](../hyprland/MAINTENANCE.md).

```sh
cd renderer && npm ci && npm run make-index-files && npm run build   # embedded at compile time
cd ../hyprland
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test        # clipboard tests need a live Hyprland session and wl-paste
```

Branches: `master` mirrors `upstream/master` exactly; `hyprland` (the default branch)
carries all fork code and merges `master`. Fork code lives only in `hyprland/` (plus
this README, `AGENTS.md` and the `hyprland-*` workflows); upstream
directories are never patched. A daily workflow fast-forwards `master` and opens a
pull request merging it into `hyprland`.

## License and credits

MIT, same as upstream — see [`LICENSE`](../LICENSE).

- [SnosMe](https://github.com/SnosMe) — Awakened PoE Trade
- [Kvan7](https://github.com/Kvan7) and contributors — Exiled Exchange 2, whose
  renderer this fork uses unmodified
- [LP0101/Exiled-Exchange-2](https://github.com/LP0101/Exiled-Exchange-2) — its KDE
  Wayland design notes informed this fork's design
