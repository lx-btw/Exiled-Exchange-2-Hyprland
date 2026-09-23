# Hyprland platform layer — technical notes

This document is the single source of truth for *why* the platform layer in this
directory is built the way it is. `PLAN.md` (repo root) tracks *what's done and
what's next*; this file explains *why each decision was made*, so a future session
(or contributor) doesn't have to re-derive it or accidentally undo it.

It mirrors the structure of `LP0101/Exiled-Exchange-2`'s `WAYLAND-NATIVE-NOTES.md`
(their KDE-Wayland fork's own design doc) on purpose — that document was read in
full while planning this fork, both for the compositor-agnostic lessons it contains
(clipboard races, XTEST failure modes) and to confirm which of its KDE-specific
workarounds (InputProxy, KWin scripts, KGlobalAccel) don't apply on Hyprland and
can be skipped.

## 1. The problem upstream doesn't solve

Upstream targets Windows + X11/XWayland-only Linux. Its overlay backend,
[`electron-overlay-window`](https://www.npmjs.com/package/electron-overlay-window),
finds the game window via `XQueryTree` and attaches via X11 mechanisms; its hotkeys
go through Electron's `globalShortcut` (`XGrabKey`); key synthesis is `uiohook-napi`
(XTEST); clipboard reads go through Electron's Chromium clipboard (X11 `CLIPBOARD`
selection).

None of this reaches a game running as (or through) a Wayland compositor cleanly:

| Layer | Why it breaks on Hyprland |
|---|---|
| Hotkey delivery (`globalShortcut.register`) | Relies on `XGrabKey` via Xwayland; Hyprland doesn't route compositor-level Wayland key events through it the way X11 window managers do. |
| Item-copy synthesis (`uiohook-napi.keyTap`, XTEST) | Desyncs modifier state in Xwayland on Hyprland specifically and can hang the whole session — the stuck state lives in Xwayland, so killing the app doesn't clear it. Recovery needs `hyprctl keyword input:kb_options "" && hyprctl keyword input:kb_options "<original>"`. |
| Clipboard read (`clipboard.readText()`) | Reads the X11 `CLIPBOARD` selection. XWM only mirrors the Wayland selection into it on *ownership* change, not *content* change — a second copy from the game (same owner) never triggers a sync, so polling reads stale/empty data. |
| Overlay window (`electron-overlay-window` attach + `setIgnoreMouseEvents`) | Ordinary Electron windows participate in Hyprland's workspace/focus model (`pin`/`no_follow_mouse` only treat symptoms), `app_id` is never reliably set on Wayland so window rules can't target it, and `setIgnoreMouseEvents(true)` needs a freshly-painted frame to apply on Electron 43+ ([electron#51808](https://github.com/electron/electron/issues/51808)) — upstream ships Electron 40 (where it still works) specifically to dodge this regression. |

Every one of those gets replaced here. Hyprland's own IPC (unix sockets, not a
compositor-side scripting sandbox) is the source of truth instead of X11 or a
compositor-specific bridge.

## 2. Architecture overview

```
PoE2 (Wayland or Xwayland client)
     │ keypress / selection change / focus change
     ▼
   Hyprland compositor                    hyprland/ (this fork, one Rust process)
     │                                          │
     │  (no bind registered — we don't          ├─ compositor.rs
     │   touch Hyprland's config at all)        │    .socket.sock (commands)
     │                                          │    .socket2.sock (events: activewindow, …)
     │                                          │
     ├─ /dev/input/event* (read, no grab) ─────►│  input/hotkeys.rs
     │                                          │    evdev state machine → dispatch
     │                                          │
     │                                          │  input/inject.rs
     │  /dev/uinput ◄───────────────────────────┤    Ctrl+Alt+C, chat text, …
     │       │                                  │
     │       └─► PoE2 receives it as a real      │
     │           keypress, copies item text      │
     │           to whichever selection it owns  │
     │                                          │
     ├─ wlr-data-control (or X11 selection) ────►│  clipboard.rs
     │   event on selection change               │    → item text
     │                                          │
     │                                          │  server.rs (axum, embeds renderer/dist)
     │                                          │    HTTP + WS /events ── same ipc/types.ts
     │                                          │    protocol upstream's Electron host uses
     ▼                                          ▼
   overlay/ (GTK4 + gtk4-layer-shell + WebKit6, same process)
     layer-shell surface, outside the window/focus model entirely
```

Two things make this simpler than the KDE-Wayland fork's design:

- Hyprland exposes window/monitor/cursor state and an event stream over its own
  plain unix sockets — no compositor-side scripting sandbox or DBus bridge needed
  (KWin requires a loaded script + DBus round-trips for the same data).
- `gtk4-layer-shell`'s `KeyboardMode::EXCLUSIVE` grants the overlay real keyboard
  focus directly. The KDE fork's InputProxy (a 1×1 invisible focusable window
  forwarding synthetic key events into a `focusable:false` main window) exists
  purely because KWin never grants focus to a `focusable:false` window — a
  layer-shell surface doesn't have that problem in the first place, on any
  wlroots-based or KWin compositor. Not needed here.

The one place still worth being careful: input synthesis has no compositor-level
API on any Wayland compositor (KWin's scripts can't type into clients either) — it
goes around the compositor entirely, straight to `/dev/uinput`.

## 3. Why hotkeys are read, not intercepted

The prior local prototype (`~/ee2-hypr`) registered hotkeys as Hyprland config binds
(`hl.bind("CTRL+D", hl.dsp.exec_cmd(...))`). This worked, but **it redefines that key
combo globally** — any other application's binding to the same combo breaks, because
Hyprland now routes it to us instead. That's the exact complaint this fork exists to
fix.

Instead, `input/hotkeys.rs` opens `/dev/input/event*` read-only (**never**
`EVIOCGRAB`) and watches for the configured combos in parallel with everything else
on the system. The game keeps receiving every keypress completely normally; we just
also see it and react. This only works because the bound combos (default
`Ctrl+D`/`Ctrl+Alt+D`/`Shift+Space`/`F5`/`F9`, user-configurable via the renderer's
own settings UI same as upstream) aren't assigned to anything inside PoE2 itself —
if a user rebinds a hotkey to something PoE2 also uses, that's a user-created
conflict no interception scheme could avoid either.

Corollary: **no Hyprland config file is ever touched.** No `bind`, no
`windowrule` (a layer surface isn't addressable by window rules anyway — it isn't a
window), no `keyword`. Verified empirically on the target system (Hyprland 0.56.2):
`hyprctl keyword bind` silently registers a bind that never fires
([hyprwm/Hyprland#13994](https://github.com/hyprwm/Hyprland/discussions/13994)), and
runtime `hl.bind`/`hl.unbind` via `hyprctl eval` has caused compositor crashes on
this exact version ([yogeshojha/blip#9](https://github.com/yogeshojha/blip/issues/9))
— dynamic Hyprland-config manipulation is avoided entirely, not merely because it's
inelegant but because it's measurably unreliable on the version in use.

## 4. Key injection: uinput, not XTEST or compositor dispatch

- **XTEST** (what upstream's `uiohook-napi` uses) desyncs modifier state in Xwayland
  on Hyprland specifically and can hang the whole session.
- **`hl.dsp.send_shortcut`** (Hyprland's own synthetic-dispatch mechanism) doesn't
  deliver modifiers to games
  ([hyprwm/Hyprland#6576](https://github.com/hyprwm/Hyprland/issues/6576), closed
  not-planned) and leaves a synthetic modifier stuck in XKB state
  ([hyprwm/Hyprland#discussions/14099](https://github.com/hyprwm/Hyprland/discussions/14099)).
- **`uinput`** creates a device the kernel — and therefore the compositor — cannot
  distinguish from a physical keyboard. Neither failure mode above can reproduce,
  because there's no synthetic-event code path involved at all from the
  compositor's perspective.

One virtual `uinput` keyboard is created at startup, advertising a **US-only**
layout. This is deliberate: layout switching (this system uses `kb_layout =
"us,ru"` with `grp:alt_shift_toggle`) is per-physical-device in Hyprland, and a
virtual device that never participates in that switching means injected combos are
never accidentally interpreted through the wrong layout — a class of bug that
otherwise depends on which layout the user happened to be in when they last touched
a physical keyboard.

The `Ctrl+Alt+C` sequence (not `Ctrl+C` — `Alt` is PoE2's own
`show_advanced_item_descriptions` bind, without which item text comes back
incomplete; see upstream `main/src/shortcuts/Shortcuts.ts:293`) uses the same
30ms stagger between key events that the prior prototype empirically tuned — reused
rather than re-derived, since it was already validated against real PoE2 behavior.

## 5. Clipboard: event-driven, not polled

The prior prototype polled `wl-paste`/`xsel` in a loop after clearing the selection,
with a timeout — functional, but it's a spawned subprocess per poll tick, and it
needed a `EE2_FOCUS_GRACE` window to paper over a race between the poll loop and a
burst of spurious `focus-game` events from the renderer.

This fork binds `zwlr_data_control_manager_v1` (the wlroots clipboard-watching
protocol; Hyprland implements it) directly via `wayland-client` +
`wayland-protocols-wlr`, and reacts to an event fired the moment the selection
changes — no polling loop, no timeout, and no race to paper over with a grace
window.

The **actual bug being worked around either way**: XWM only mirrors the X11 and
Wayland clipboard selections into each other on ownership change, not content
change. If the game copies to the selection it owns and that data is read from the
*other* side (e.g. game running under Xwayland, host reads the Wayland selection),
a second copy in a row can read back stale data from the first, because the mirror
never re-fired. The fix on both the prior prototype and here is the same: read
whichever selection the game's window actually owns, identified via Hyprland's
per-client `xwayland` flag (`hyprctl clients -j` reports this; `compositor.rs`
surfaces it the same way `host.mjs`'s `findGame()` did) — not "whichever selection
API happens to be convenient."

## 6. Overlay window: layer-shell, not a toplevel window

Layer-shell surfaces (`wlr-layer-shell`, `zwlr_layer_shell_v1`) live on their own
compositor layer, are not part of any workspace, and don't participate in the
window/focus model at all — which is precisely the set of problems Electron's
ordinary `BrowserWindow` has on Wayland (see §1). This is *why* layer-shell was
chosen over any toplevel-window approach, Electron or otherwise; it isn't a
Hyprland-specific choice; wlroots and KWin both implement it.

Specifics carried over from the prior prototype's `overlay.py` (already proven on
this system):

- **Keyboard mode must be `EXCLUSIVE`, never `ON_DEMAND`.** At `ON_DEMAND`,
  `follow_mouse` (Hyprland's default mouse-focus behavior) hands keyboard focus
  back to the game the instant the mouse moves over the overlay, making it
  impossible to actually type into or interact with a widget.
- **Input region toggling, not nullable.** Passthrough mode uses an empty input
  region (`cairo::Region::default()` in the prior GTK3 code — the `gtk4-layer-shell`
  equivalent is the same idea); interactive mode uses a full-window rectangle.
  Passing a null/None region to reset it throws (the parameter isn't nullable in the
  bindings) — always construct an explicit region for both states.
- **Monitor is matched by origin coordinates, not size.** A rotated monitor reports
  transposed dimensions from GDK but its origin `(x, y)` is always correct — matching
  by size breaks on rotated setups, matching by origin doesn't.
- **`exclusive_zone(-1)`** so other layer-shell clients (status bars, notification
  daemons) don't reserve space that would shrink the overlay.

WebKit6 WebView is loaded with a user-agent string containing the substring
`Electron/44.0.0` — `renderer/src/web/background/IPC.ts`'s `isElectron()` transport
check gates overlay-specific renderer behavior on this substring match; without it
the renderer runs as if it were a plain browser tab and doesn't bind to cursor
position or overlay-specific IPC events at all.

## 7. Compositor state: Hyprland's own sockets, not `hyprctl` subprocesses

The prior prototype's `host.mjs` shelled out to `hyprctl` for every piece of state
it needed (`clients`, `cursorpos`, `activewindow`, `monitors`) — 4 of its 7
per-keypress subprocess spawns (the other 3 were `ydotool`/`wl-paste`/`xsel`,
addressed in §4–5 above). `compositor.rs` instead speaks directly to
`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (request/response,
same commands `hyprctl` itself sends) and `.socket2.sock` (a plain event stream —
`activewindow>>class,title` and friends, newline-delimited) over unix sockets. Same
data, same semantics, zero subprocess spawns per keypress.

## 8. Known open questions (not yet resolved — see `PLAN.md` Stage 5)

- **`OVERLAY->MAIN::render-state {shouldShow}`**: upstream added this IPC event
  after the prior prototype was built; that prototype only logged it without
  handling it. It's presumably a more precise overlay show/hide signal than the
  `focus-game`-burst-plus-grace-window heuristic the old code used
  (`EE2_FOCUS_GRACE`, default 600ms — the renderer was observed sending a burst of
  `focus-game` events ~18ms after interactive mode enabled, with no corresponding
  mouse event on the page, cause never fully identified). Needs a running renderer
  to observe what actually drives it before it can replace anything.
- **`showModsKey`**: hardcoded as `Alt` in the prior prototype. Should be read from
  the user's `poe2_production_Config.ini` (`show_advanced_item_descriptions`) the
  way upstream's `main/src/host-files/GameConfig.ts` does, so a rebind in-game
  doesn't silently desync the injected combo.
