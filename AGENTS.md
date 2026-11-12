# AGENTS.md

This file provides guidance to Codex when working with code in this repository.

## What this repository is

A fork of [Kvan7/Exiled-Exchange-2](https://github.com/Kvan7/Exiled-Exchange-2)
(a PoE2 price-check overlay, itself forked from Awakened PoE Trade). Upstream targets
Windows/X11 via Electron. This fork adds a native Hyprland/Wayland platform layer in
`hyprland/` that replaces Electron's `main/` entirely.

The fork is feature-complete and live-verified; it is in **maintenance mode** (small
bug fixes and pulling upstream updates). User setup and known limitations are in
[the public README](.github/README.md); CI, releases, upstream updates and bug triage
are in [`hyprland/MAINTENANCE.md`](hyprland/MAINTENANCE.md). Design rationale for
every module: `hyprland/HYPRLAND-NOTES.md`.

## Fork boundary — absolute and load-bearing, read before touching anything

- `main/`, `renderer/`, `ipc/`, `dataParser/`, `docs/` are upstream. **Never patch
  them.** Not a one-line fix, not a config tweak. They must stay byte-for-byte
  mergeable with `upstream/master` at all times.
- `hyprland/` is the only directory with fork code (plus the root `AGENTS.md`,
  `.github/README.md` and `.github/workflows/hyprland-*.yml` — new files
  only; upstream's `.github/workflows/{main,pages,test}.yml` stay untouched and are
  disabled in the repo settings; the root `README.md` is upstream's). It talks to the
  unmodified `renderer/` over the same HTTP+WebSocket protocol upstream's Electron
  host uses; `ipc/types.ts` is the contract.
- Branches: `master` mirrors `upstream/master` exactly (fast-forward only, never
  commit there). `hyprland` carries all fork code, merges `master` as upstream moves,
  and is the default branch on `origin` (scheduled workflows run from the default
  branch). Sync PRs from the bot branch `upstream-sync` are merged with a **merge
  commit only** — squash/rebase breaks "`master` is an ancestor of `hyprland`".
  Remote `upstream` has push disabled on purpose (`git remote set-url --push
  upstream DISABLE`) — fetch-only; a push failure there means something is
  misconfigured, not an obstacle to route around.
- **Input rule: the app never edits or binds anything in Hyprland's config** — no
  `bind`, no window/layer rules, no `keyword`, no `hyprctl keyword`/`eval`/`dispatch`
  that changes compositor state. (A prior prototype's config binds broke the same
  key combos in other applications system-wide — the problem this fork exists to
  fix.) **evdev `EVIOCGRAB` capture is allowed**, scoped to game focus: only real
  physical keyboards (never our own uinput devices, never the mouse), only while
  PoE2 is the focused window, transitions only when nothing is held on that device,
  and everything except a matched hotkey's own trigger key is re-emitted through a
  uinput mirror device. See `input/hotkeys.rs` / `input/grab.rs`, HYPRLAND-NOTES.md
  §3.
- Key injection goes through our own uinput virtual keyboard (US layout). XTEST and
  `hl.dsp.send_shortcut` are known-broken on Hyprland for this — do not reintroduce
  either.
- No installer side effects: no udev rules, no autostart; the package only installs
  files.

Easy-to-undo decisions (rationale in HYPRLAND-NOTES.md): layer-shell keyboard mode is
only ever `None`/`Exclusive`, never `OnDemand` (follow_mouse would hand focus back to
the game); the clipboard is read from whichever selection the game owns (`xwayland`
flag), never "the convenient one"; the WebView runs with
`HardwareAccelerationPolicy::Never` (otherwise it stops rendering after the first
hide/show); the window's transparent-background `CssProvider` must stay.

## `hyprland/` module map (`hyprland/src/`)

- `main.rs` — wiring: GTK on the main thread, tokio runtime on a background thread.
  Ordering warning there: the uinput virtual keyboard must exist before hotkey
  watching starts.
- `state.rs` — the state machine (`Overlay`): price-check / stash-search /
  paste-in-chat flows, interactive vs passthrough, surface visibility, keyboard
  mode, WS event dispatcher (`handle_ws_event`). Ports `OverlayWindow.ts` and the
  action handlers of `Shortcuts.ts`/`text-box.ts`.
- `server.rs` — axum HTTP + WS server implementing `ipc/types.ts` (`/`, `/config`,
  `/proxy/<url>`, WS `/events`); embeds `renderer/dist` via `rust-embed`.
- `proxy.rs` — trade-API relay with its own cookie jar (`proxy.ts`).
- `compositor.rs` — Hyprland IPC over `.socket.sock`/`.socket2.sock` (focus, clients,
  monitors, cursor, devices, `switchxkblayout`); no `hyprctl` subprocesses.
- `input/hotkeys.rs` — evdev hotkey matcher and capture driver; `input/grab.rs` —
  `EVIOCGRAB` bookkeeping, mirror uinput device, per-device XKB layout sync;
  `input/inject.rs` — uinput virtual keyboard (copy-item, stash-search, chat,
  stash-scroll); `input/keymap.rs` — shortcut string ↔ evdev keycode.
- `area_tracker.rs` — hover/click tracking for the price-check widget
  (`WidgetAreaTracker.ts`) and Ctrl+wheel `stashScroll`, read-only mouse devices.
- `clipboard.rs` — wlr-data-control read (event-driven) and write (own data
  source), `restoreClipboard`; `xsel` for an Xwayland game.
- `overlay/window.rs` — GTK4 + gtk4-layer-shell surface (layer, input region,
  keyboard mode, monitor, visibility); `overlay/webview.rs` — WebKit6 WebView
  (Electron UA substring, focus/hover bridges, `window.open`, acceleration off).
- `config.rs` — upstream `config.json` (`ConfigStore.ts`) and
  `poe2_production_Config.ini` `showModsKey` (`GameConfig.ts`).
- `game_log.rs` — inotify `Client.txt` tail → `game-log` (`GameLogWatcher.ts`).
- `file_writer.rs` — `write-data` sink (`FileWriter.ts`).
- `logger.rs` — `ee_log!`: stderr + in-memory history + `log-entry` (`RemoteLogger.ts`).
- `tray.rs` — StatusNotifierItem via `ksni` (`AppTray.ts`).
- `hyprland/packaging/` — PKGBUILD, systemd user unit, `ee2-hyprland-wrap` (Steam
  launch wrapper), `.desktop`, `README.md` (device access).

## Commands

```shell
# renderer (upstream Vue app) — the binary embeds renderer/dist at compile time,
# so build it before cargo (and after every upstream sync)
cd renderer
npm ci
npm run make-index-files   # must run before build — generates data-driven assets
npm run build              # vue-tsc --noEmit && vite build → renderer/dist

# fork
cd hyprland
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test                 # clipboard tests use the live Hyprland session + wl-paste

# package (the user installs it: sudo pacman -U ee2-hyprland-*.pkg.tar.zst)
cd hyprland/packaging
makepkg -f
```

Don't run `sudo`/`pacman` yourself; installing is the user's step. `main/` is never
built here — it's kept only as a porting reference (`main/src/*.ts`).

## CI

- `.github/workflows/hyprland-ci.yml` (Arch container): fork-boundary allowlist
  check, `fmt`, `clippy`, `cargo test` minus the two live-Wayland clipboard write
  tests, `makepkg` and AppImage artifacts; a tag `hyprland-v<pkgver>` publishes
  a GitHub Release with both formats and checksums.
- `.github/workflows/hyprland-upstream-sync.yml` (daily): fast-forwards `master`
  from `upstream/master` (pushes via the deploy-key secret `SYNC_DEPLOY_KEY`),
  merges it into `upstream-sync` and opens/updates the PR into `hyprland`.
- Setup, release procedure and caveats: `hyprland/MAINTENANCE.md`, "CI and repository setup".

## Runtime and debugging

- Logs: `journalctl --user -u ee2-hyprland -b` (all lines prefixed `ee2-hyprland:`,
  renderer console output included). `hyprland/MAINTENANCE.md` lists the triage lines
  that matter (`set_interactive(...)`, `game focus`, `surface:`, `capture:`,
  `copy-item:` …).
- Single instance: a second launch only activates the already-running
  `GApplication`. Stop the service (`systemctl --user stop ee2-hyprland`) or kill
  the exact PID before testing a new build.
- Proxy: the shell exports `HTTP(S)_PROXY` without `NO_PROXY`. The binary adds
  `127.0.0.1,localhost` to `NO_PROXY`/`no_proxy` itself at startup (otherwise the
  WebView can't load its own loopback assets), so a manual
  `./target/release/ee2-hyprland` needs nothing extra; `curl` to it needs `--noproxy '*'`.
  `cargo test` works as-is.
- `EE2_GAME_CLASS=<class>` points "the game" at another window for testing without
  PoE2. Defaults recognize `pathofexilesteam.exe` (native Wayland) and
  `steam_app_2694490` (Xwayland); the variable replaces both with one exact class.
- Surface/rendering problems: `WAYLAND_DEBUG=1`; HYPRLAND-NOTES.md §9.

## Upstream protocol boundary

`ipc/types.ts` and `ipc/KeyToCode.ts` define the entire wire contract between the
host and `renderer/`: HTTP (`/`, `/config`, `/proxy/<url>`) plus WebSocket `/events`
carrying named JSON events (`MAIN->OVERLAY::focus-change`, `CLIENT->MAIN::save-config`,
`OVERLAY->MAIN::render-state`, …). As long as `hyprland/` speaks this protocol,
`renderer/` needs zero modification. When upstream changes `ipc/types.ts`, that diff
is the list of what `server.rs`/`state.rs` must catch up on — check it first on every
sync (procedure in `hyprland/MAINTENANCE.md`).
