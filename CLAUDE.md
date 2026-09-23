# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repository is

A private fork of [Kvan7/Exiled-Exchange-2](https://github.com/Kvan7/Exiled-Exchange-2)
(a PoE2 price-check overlay, itself forked from Awakened PoE Trade). Upstream targets
Windows/X11 via Electron. This fork adds a native Hyprland/Wayland platform layer.

**The fork boundary is absolute and load-bearing — read this before touching anything:**

- `main/`, `renderer/`, `ipc/`, `dataParser/`, `docs/` are upstream. **Never patch them.**
  Not a one-line fix, not a config tweak. They must stay byte-for-byte mergeable with
  `git merge upstream/master` on the `master` branch at all times.
- `hyprland/` is the only directory this fork ever writes to. It's a from-scratch Rust
  platform layer (GTK4 + `gtk4-layer-shell` + WebKit6 + evdev/uinput) that replaces
  Electron's `main/` entirely, talking to the unmodified `renderer/` over the same
  HTTP+WebSocket protocol upstream's Electron host uses (`ipc/types.ts` is the contract —
  see below).
- Branches: `master` mirrors `upstream/master` exactly (fast-forward only, never commit
  here). `hyprland` carries all our code, rebased/merged from `master` as upstream moves.
  Remote `upstream` has push disabled on purpose (`git remote set-url --push upstream DISABLE`)
  — it is fetch-only, treat a push failure there as a sign something is misconfigured,
  not an obstacle to route around.

Full rationale, the platform-layer design, and why each upstream approach (Electron
`globalShortcut`+XTEST, KWin/KDE's DBus-bridge approach) doesn't work on Hyprland:
`hyprland/HYPRLAND-NOTES.md`. Implementation plan and progress: `PLAN.md`.

## Commands

### Upstream (`main/`, `renderer/`) — only touch when syncing from upstream

```shell
# renderer (Vue3 UI, runs inside the WebView we provide)
cd renderer
npm install
npm run make-index-files   # must run before dev/build — generates data-driven assets
npm run dev                # vite dev server
npm run build               # vue-tsc --noEmit && vite build  → renderer/dist
npm run check-types
npm run lint / lint-fix
npm run test                 # vitest
npm run format

# main (Electron host) — reference only; this fork does not build or run this
cd main
npm install
npm run dev                  # node build/script.mjs
npm run build                # tsc --noEmit && node build/script.mjs --prod
npm run check-types
npm run lint / fix
```

`renderer/dist` is the only upstream build artifact this fork consumes — it gets
embedded into the Rust binary via `rust-embed`. `main/` is kept in the tree only so
`main/src/*.ts` remains readable as a porting reference; it is never built here.

### This fork (`hyprland/`)

```shell
cd hyprland
cargo build
cargo build --release
cargo run -- <command>        # see hyprland/src/main.rs for the command list
cargo test
cargo clippy
```

There is no CI/build pipeline wired up yet (Stage 6 in `PLAN.md`). Until then, building
and running is manual.

## Architecture

### Upstream protocol boundary (do not change without checking `ipc/types.ts`)

`ipc/types.ts` and `ipc/KeyToCode.ts` define the entire wire contract between the
Electron/our-Rust host and the `renderer/` UI: an HTTP surface (`/`, `/config`,
`/proxy/<url>`) plus a WebSocket at `/events` carrying named JSON events
(`MAIN->OVERLAY::focus-change`, `CLIENT->MAIN::save-config`, `OVERLAY->MAIN::focus-game`,
etc.). This is the seam the whole fork is built on: as long as `hyprland/` speaks this
protocol correctly, `renderer/` needs zero modification, ever. When upstream changes
`ipc/types.ts`, that diff is the complete list of what `hyprland/src/server.rs` needs
to catch up on — check it first on every sync from upstream.

### Upstream's own structure (for porting reference, in `main/src/`)

- `windowing/OverlayWindow.ts` — overlay show/hide/interactive state machine. Ported
  to `hyprland/src/state.rs` + `hyprland/src/overlay/`.
- `windowing/GameWindow.ts` — game window attach/focus/geometry via `electron-overlay-window`
  (X11-only). Replaced by `hyprland/src/compositor.rs` talking to Hyprland's own IPC
  sockets directly.
- `shortcuts/Shortcuts.ts` — hotkey registration (`globalShortcut`) and dispatch
  (`copy-item`, `paste-in-chat`, `stash-search`, `trigger-event`, `toggle-overlay`,
  `stashScroll`). Ported to `hyprland/src/input/hotkeys.rs`, reading the same
  `config.json` shortcut definitions but dispatched from evdev instead of Electron's
  global-shortcut API — no compositor-level keybind is ever registered.
- `shortcuts/HostClipboard.ts` — clipboard polling via Electron's Chromium clipboard
  (X11-only, and racy on Wayland — see `HYPRLAND-NOTES.md`). Replaced by
  `hyprland/src/clipboard.rs`, event-driven via `wlr-data-control`.
- `shortcuts/text-box.ts` — chat-typing / stash-search key sequences via `uiohook`
  XTEST. Replaced by `hyprland/src/input/inject.rs`, via our own `uinput` virtual
  keyboard.
- `host-files/GameConfig.ts` — reads `poe2_production_Config.ini` for user keybinds
  (e.g. `show_advanced_item_descriptions` / `showModsKey`). Not yet ported — see
  `PLAN.md` Stage 5.
- `host-files/GameLogWatcher.ts` — tails the PoE2 client log (chat, zone changes).
  Not yet ported.
- `server.ts` — the Electron host's HTTP+WS server implementing the `ipc/` protocol.
  Ported to `hyprland/src/server.rs`.
- `proxy.ts` — outbound HTTPS relay to trade-site APIs (CORS workaround) with a cookie
  jar. Ported to `hyprland/src/proxy.rs`.
- `AppTray.ts` — system tray menu. Ported to `hyprland/src/tray.rs` via `ksni`.
- `vision/` (OCR for heist gems) — Windows-only upstream already; out of scope here.

### `hyprland/` layout

See `PLAN.md` for the full module breakdown and the crate versions each depends on.
In short: `compositor.rs` (Hyprland's `.socket.sock`/`.socket2.sock` IPC, no `hyprctl`
subprocess spawns), `input/` (evdev listen + uinput inject — hotkeys are **read, never
grabbed**, so nothing here ever writes to a Hyprland keybind config), `clipboard.rs`
(`wlr-data-control`, event-driven), `overlay/` (GTK4 + `gtk4-layer-shell` +
WebKit6 — layer-shell surfaces sit outside the compositor's window/focus model
entirely, which is *why* Electron doesn't work here), `server.rs`/`proxy.rs`
(the `ipc/` protocol implementation), `tray.rs`, `state.rs`.

### Key design decisions worth knowing before changing platform-layer code

- **No Hyprland config is ever touched** — no binds, no window rules, no keywords.
  This is a hard requirement carried over from a prior prototype whose config-bind
  approach broke the same key combos in other applications system-wide.
- **Hotkeys are read via evdev, not intercepted** (`EVIOCGRAB` is never used). The game
  sees every keypress normally; this only works because the bound combos aren't
  assigned to anything inside PoE2 itself.
- **Key injection goes through our own `uinput` virtual device**, advertising a
  US-only layout — this sidesteps the user's `us,ru` layout-switching setup entirely,
  since layout switching in Hyprland is per-physical-device. XTEST and
  `hl.dsp.send_shortcut` are both known-broken on Hyprland for this (desynced
  modifier state / dropped modifiers) — do not reintroduce either.
- **Layer-shell keyboard mode must be `EXCLUSIVE`**, never `ON_DEMAND` — at
  `ON_DEMAND`, `follow_mouse` hands keyboard focus back to the game on the first
  mouse movement over the overlay.
- **Clipboard is read from whichever selection the game actually owns** (X11 via
  Hyprland's per-client `xwayland` flag, or the native Wayland selection) — XWM only
  mirrors X11↔Wayland selections on ownership change, not content change, so reading
  the "wrong" side sees stale data on a second copy.
