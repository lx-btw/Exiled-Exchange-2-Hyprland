# Hyprland platform layer — technical notes

This document is the single source of truth for *why* the platform layer in this
directory is built the way it is. `PLAN.md` (repo root) holds the current status,
known limitations and maintenance procedures; this file explains *why each decision
was made*, so a future session (or contributor) doesn't have to re-derive it or
accidentally undo it.

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
     ├─ /dev/input/event* keyboards ───────────►│  input/hotkeys.rs + input/grab.rs
     │   (EVIOCGRAB while the game is focused)  │    evdev state machine → dispatch;
     │  /dev/uinput mirror ◄────────────────────┤    everything not consumed re-emitted
     │                                          │
     ├─ /dev/input/event* mice (read only) ────►│  area_tracker.rs (hover, stashScroll)
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
     │   Client.txt (inotify) ─────────────────►│  game_log.rs, file_writer.rs, logger.rs
     │                                          │
     │                                          │  server.rs (axum, embeds renderer/dist)
     │                                          │    HTTP + WS /events ── same ipc/types.ts
     │                                          │    protocol upstream's Electron host uses
     ▼                                          ▼
   overlay/ (GTK4 + gtk4-layer-shell + WebKit6, same process)
     layer-shell surface, outside the window/focus model entirely
```

`state.rs` ties it together: the price-check / stash-search / paste-in-chat flows,
interactive vs passthrough, and the layer surface's visibility.

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

## 3. Why hotkeys are captured via evdev, never via a Hyprland bind

The prior local prototype (`~/ee2-hypr`) registered hotkeys as Hyprland config binds
(`hl.bind("CTRL+D", hl.dsp.exec_cmd(...))`). This worked, but **it redefines that key
combo globally** — any other application's binding to the same combo breaks, because
Hyprland now routes it to us instead. That's the exact complaint this fork exists to
fix: **the app never edits or binds anything in Hyprland's config.** No `bind`, no
`windowrule` (a layer surface isn't addressable by window rules anyway — it isn't a
window), no `keyword`, no `hyprctl keyword`/`eval`. Verified empirically on the
target system (Hyprland 0.56.2): `hyprctl keyword bind` silently registers a bind
that never fires
([hyprwm/Hyprland#13994](https://github.com/hyprwm/Hyprland/discussions/13994)), and
runtime `hl.bind`/`hl.unbind` via `hyprctl eval` has caused compositor crashes on
this exact version ([yogeshojha/blip#9](https://github.com/yogeshojha/blip/issues/9))
— dynamic Hyprland-config manipulation is avoided entirely, not merely because it's
inelegant but because it's measurably unreliable on the version in use.

The rule is about Hyprland's *config*, not about evdev. Merely *reading*
`/dev/input` (no grab) is not enough: a hotkey's own trigger key — `D` in `Ctrl+D`,
`Space` in `Shift+Space` — then still reaches the game and performs its native
action (a step, a dodge-roll) on every press. So what *is* used, deliberately,
keyd-style (`input/hotkeys.rs` + `input/grab.rs`), is `EVIOCGRAB` on real physical
keyboard devices, scoped and guarded so it can't leak into the "redefines a combo
globally" failure mode above:

- **Only real hardware is ever grabbed** — never `inject.rs`'s injection device or
  `grab.rs`'s own mirror device, both excluded by name (otherwise their output would
  loop back into the matcher). The mouse is never grabbed.
- **Grab is active only while the game is focused.** The instant focus leaves PoE2,
  the device stops being grabbed (subject to the next point) and every other
  application on the system gets its keys back completely normally — there is no
  window where this redefines a combo for anything else running.
- **A grab/ungrab transition only happens at a moment nothing is held on that
  device**, checked against `hotkeys.rs`'s own per-`(fd, code)` held-key tracking
  and deferred to the device's next key-release otherwise. Grabbing mid-hold would
  swallow a modifier's eventual release event, which is exactly the "stuck
  modifier" failure class `hl.dsp.send_shortcut` already has on this Hyprland
  version (§4) — this rule exists to make sure capture can't reproduce it by a
  different route.
- **Whatever isn't the matched hotkey's own trigger-key edge is re-emitted
  immediately** through a second `uinput` mirror device (`grab.rs::Mirror`,
  deliberately separate from `inject.rs`'s device — mixing the `Ctrl+Alt+C`
  injector's sequences with live passthrough typing on one device would race the
  two). Modifiers always pass through, on every device, unconditionally — only a
  matched hotkey's own base key (press, repeat, release) is ever consumed. The
  mirror's capability set is the union of the physical keyboards' keys (not the
  injector's curated table), so punctuation typed into chat survives.
- **Per-device XKB layout state is synchronized at each transition** — see §8. A
  physical device grabbed mid-`ru` would otherwise go silent (typed keys reach the
  mirror, which starts at its own independent default `us` state) or come back
  desynced from whatever the user toggled to while typing through the mirror.
- **No timers.** `evdev::Device` ungrabs itself on `Drop`, and `EVIOCGRAB` is
  fd-scoped kernel state already released by the kernel on any process exit,
  `SIGKILL` included — a crash needs no cleanup code here at all.

Matching follows upstream's `globalShortcut` semantics: **exact modifier-set
equality** (`Ctrl + D` must not fire on `Ctrl + Alt + D`), and hotkeys only fire
while PoE2 is the focused window (upstream registers/unregisters on
`poeWindow.on("active-change")`). Held keys are tracked per `(device, keycode)`,
so a second keyboard can't spuriously clear a modifier held on the first. A bare
`Esc` closes an interactive overlay at the evdev level (consumed while grabbed),
like upstream's `before-input-event` handler, which intercepts it before the
renderer's DOM — even while a text field has focus.

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

The copy combo is `Ctrl+<showModsKey>+C`, not `Ctrl+C` — `showModsKey` is PoE2's
own `show_advanced_item_descriptions` bind (`Alt` by default), without which item
text comes back without stat ranges (upstream `main/src/shortcuts/Shortcuts.ts:293`).
It is read from `poe2_production_Config.ini` like upstream's `GameConfig.ts`; only a
bare Ctrl/Shift/Alt binding is representable (see `config.rs`). Every injected
sequence (copy-item, stash-search, paste-in-chat, stashScroll's arrow taps) uses a
30 ms stagger between key events, the value the prior prototype tuned against real
PoE2 — PoE2 drops the modifier below ~20 ms, which the KDE fork measured
independently. Modifiers are released unconditionally even if a write fails
mid-sequence, so a uinput error can never leave a key reported held.

## 5. Clipboard: event-driven, not polled

The prior prototype polled `wl-paste`/`xsel` in a loop after clearing the
selection, with a timeout — functional, but a spawned subprocess per poll tick.

This fork binds `zwlr_data_control_manager_v1` (the wlroots clipboard-watching
protocol; Hyprland implements it) directly via `wayland-client` +
`wayland-protocols-wlr`, and reacts to an event fired the moment the selection
changes — no polling loop and no "clear first" step: every genuine copy produces a
fresh `selection` event with a fresh offer object, even for back-to-back copies of
identical text. The protocol sends one `selection` immediately on binding
(pre-existing content), so callers drain buffered events right before triggering a
copy. (`wlr-data-control-unstable-v1` is marked deprecated in favour of
`ext-data-control-v1`; worth migrating once relevant.)

The **actual bug being worked around either way**: XWM only mirrors the X11 and
Wayland clipboard selections into each other on ownership change, not content
change. If the game copies to the selection it owns and that data is read from the
*other* side (e.g. game running under Xwayland, host reads the Wayland selection),
a second copy in a row can read back stale data from the first, because the mirror
never re-fired. The fix on both the prior prototype and here is the same: read
whichever selection the game's window actually owns, identified via Hyprland's
per-client `xwayland` flag (`hyprctl clients -j` reports this; `compositor.rs`
surfaces it the same way `host.mjs`'s `findGame()` did) — not "whichever selection
API happens to be convenient." The X11 side is a plain `xsel` subprocess (polled
every 100 ms for copy-item, `DISPLAY` fallback `:1` — where Hyprland's rootless
Xwayland lands on this system); it is the rare path (a game under Xwayland without
`PROTON_ENABLE_WAYLAND=1`).

Writes (stash-search, paste-in-chat, restores) on the native path go through our
own `zwlr_data_control_source_v1` on a second, dedicated connection and thread
(`run_writer`, a single `poll` on the connection fd plus an `eventfd` for new jobs
— no timer, no `wl-copy`). Every source carries a private marker MIME type so the
read side skips our own writes.

**`restoreClipboard` uses a fixed 120 ms wait** (upstream's `RESTORE_AFTER`), not
an event. The obvious signal — the source's `Send` event, "someone read it" —
cannot distinguish the game pasting from a clipboard-history daemon
(`wl-clip-persist`, `wl-paste --watch … | cliphist store`): those read every new
selection within microseconds. A `Send`-driven restore was tried and failed exactly
this way. "What's on the clipboard now" (the value to restore) *is* event-fed: a
cache updated from every external `selection` event, no subprocess.

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
- **Input region toggling, always explicit.** Passthrough mode uses an empty input
  region (`cairo::Region::create()`); interactive mode uses a full-window
  rectangle. An explicit region is constructed for both states rather than passing
  `None`, mirroring the prior prototype's non-nullable Python/cairo bindings even
  though the Rust binding technically allows it.
- **Monitor is matched by origin coordinates, not size.** A rotated monitor reports
  transposed dimensions from GDK but its origin `(x, y)` is always correct — matching
  by size breaks on rotated setups, matching by origin doesn't.
- **`exclusive_zone(-1)`** so other layer-shell clients (status bars, notification
  daemons) don't reserve space that would shrink the overlay.

Specific to this fork:

- **Opaque theme background.** A GTK4 theme may paint `.background` (every
  top-level `GtkWindow` has that class) fully opaque — e.g. a Catppuccin
  `~/.config/gtk-4.0/gtk.css` (`#1e1e2e`). GDK then correctly reports a
  full-surface `wl_surface.set_opaque_region` and Hyprland correctly stops
  compositing the game underneath: a solid full-monitor backdrop. Not a Hyprland or
  WebKit bug. Fixed with a `CssProvider` at `STYLE_PROVIDER_PRIORITY_USER` forcing
  `window, window.background { background-color: transparent; }` (`window.rs`);
  with it, `set_opaque_region` is never sent. A `UserStyleSheet` forcing
  `html`/`body` transparent is **not** used: it reproducibly broke
  `KeyboardMode::Exclusive` through an unconfirmed mechanism.
- **Keyboard model: the keyboard is where the pointer is; a focused text field
  takes priority.** Input region and keyboard mode are independent. While the
  overlay is mouse-interactive, `KeyboardMode` is `Exclusive` iff a text field
  inside it has DOM focus *or* the pointer is over an overlay widget; over the
  transparent background (= over the game) it is `None` and the game keeps the
  keyboard (WASD keeps working with the overlay open). Reason for the hover rule:
  the renderer's Shift stat-tooltip (`TradeItem.vue`/`TradeListing.vue`) listens for
  `keydown`/`keyup` `Shift` on `window`, so the page must actually receive keys while
  hovered — no synthetic events. Both signals come from injected
  `UserContentManager` scripts (`webview.rs`: `eeFocus` focusin/out, `eeHover`
  mouseover). "Background" = `#overlay-window`'s first element child (the
  `handleBackgroundClick` div), the root, html, body; everything else is a widget.
  Passthrough (empty input region) resets the hover flag — the pointer is the
  game's there. Consequence: a hotkey injected into the game while hovering a widget
  (e.g. `copy-item`) goes to the overlay.
- **Keyboard mode and input region are applied after the surface is mapped** —
  both are silently ignored on a not-yet-mapped layer surface. `state.rs`'s
  `apply_surface` applies the whole desired state (visible, monitor, region,
  keyboard) idempotently on the GTK main thread for every cause.
- **Visibility follows the game window, not the user.** A layer surface belongs to
  a monitor, not a workspace, so on its own it would follow the user to every other
  workspace. The surface is only mapped while the game window is on screen (its
  workspace is the active one on its monitor and no special workspace covers it —
  `compositor::game_origin`), pinned to the game's monitor
  (`set_monitor_by_origin`), and then shown iff interactive, the welcome banner is
  up, or the renderer's `render-state.shouldShow` (with `hideOverlayOnBlur`: only
  while the game is focused) — upstream's `syncWindowVisibility`. It starts hidden
  and stays hidden until the renderer's first `render-state` or an interactive
  action, so a renderer that never mounts can't leave a full-monitor surface over
  the desktop.
- **Focus.** The layer surface isn't a window, so the game stays the active window
  while the overlay is interactive; any game-focus change drops the overlay to
  passthrough (and releases the evdev grab first). Exception: an *empty*-class
  `activewindow` while the overlay is interactive is Hyprland reporting our own
  surface giving up `Exclusive` (pointer moved to the background), immediately
  followed by the game being refocused — it is ignored, otherwise a pinned
  (clicked) widget would close the moment the cursor leaves it. Since the overlay
  never appears in `activewindow`, `set_interactive` is the only way the renderer
  learns the overlay became active (`focus-change {overlay}`).
- **Coordinates.** WebKit's `window.screenX`/`screenY` are always `(0, 0)` on a
  layer surface (Wayland doesn't expose absolute position to clients), so the
  renderer's widget math is monitor-local while Hyprland's cursor/client positions
  are global. `item-text.position` is converted global → local and
  `track-area`'s `from`/`area` local → global at the protocol boundary.
- **`window.open` → system browser.** WebKit's `create` signal is handled in
  `webview.rs`: http(s) URLs go to `gtk4::UriLauncher`, no new window is created,
  other schemes are logged and dropped (upstream: `setWindowOpenHandler` →
  `shell.openExternal`). **Known limitation:** `price-check.builtinBrowser = true`
  is unsupported — it relies on Electron's `<webview>` tag, absent in WebKit.
  Keep it off.

WebKit6 WebView is loaded with a user-agent string containing the substring
`Electron/44.0.0` — `renderer/src/web/background/IPC.ts`'s `isElectron()` transport
check gates overlay-specific renderer behavior on this substring match; without it
the renderer runs as if it were a plain browser tab and doesn't bind to cursor
position or overlay-specific IPC events at all. Accelerated compositing is
disabled on it — see §9.

## 7. Compositor state: Hyprland's own sockets, not `hyprctl` subprocesses

The prior prototype's `host.mjs` shelled out to `hyprctl` for every piece of state
it needed (`clients`, `cursorpos`, `activewindow`, `monitors`) — 4 of its 7
per-keypress subprocess spawns (the other 3 were `ydotool`/`wl-paste`/`xsel`,
addressed in §4–5 above). `compositor.rs` instead speaks directly to
`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (request/response:
write `j/<command>`, read to EOF — Hyprland closes the connection after replying;
5 s timeout) and `.socket2.sock` (a plain event stream of newline-delimited
`EVENT>>data` lines). Same data, same semantics, zero subprocess spawns per
keypress.

Events consumed: `activewindow>>class,title` (game focus; `activewindowv2` carries
strictly less information and is ignored), `openwindow` (a new game window re-arms
the once-per-window welcome banner), and the layout events (`workspacev2`,
`focusedmonv2`, `activespecialv2`, `movewindowv2`, `moveworkspacev2`,
`openwindow`/`closewindow`, `monitoraddedv2`/`monitorremovedv2`), each of which
triggers a re-query of `j/clients` + `j/monitors` to decide where (and whether) the
game is on screen. The current focus and layout are queried once on (re)connect,
since events only fire on change; the watcher reconnects every 2 s if the socket
closes (e.g. compositor restart). Lines are split on raw bytes before UTF-8
decoding, so a multi-byte character (Cyrillic window titles) can't be cut at a read
boundary. A literal `{}` reply to `j/activewindow` means "nothing focused"; any
other parse failure is an error.

The game window is found by class: `pathofexilesteam.exe` (Proton's naming under
Steam with `PROTON_ENABLE_WAYLAND=1`, not Steam's `steam_app_2694490`), overridable
with `EE2_GAME_CLASS`.

## 8. Per-device XKB layout sync for captured keyboards

`compositor.rs::keyboards()`/`switchxkblayout()` — needed because grabbing a
physical keyboard (§3) takes it out of Hyprland's own input pipeline entirely, but
its XKB layout state (`us,ru`, `grp:alt_shift_toggle`) is tracked *per device* by
Hyprland, and `grab.rs::Mirror` (the re-emit target for everything capture doesn't
consume) is a distinct uinput device with its own independent XKB state that starts
at index 0 (`us`) the moment it's created. Without syncing the two, a device
grabbed while the user had toggled to `ru` would have every subsequently-typed
character silently come out in the wrong layout through the mirror, and toggling
layout *while* grabbed (typing through the mirror) would leave the physical
device's own Hyprland-tracked state stale the moment it's ungrabbed.

The fix: on every grab, read the physical device's current `active_layout_index`
(`hyprctl -j devices`'s `keyboards[]` array) and set the mirror to the same index;
on every ungrab, read the mirror's current index back and set the physical device
to match. Both directions go through `switchxkblayout <device-name> <index>` sent
directly over `.socket.sock` — no `hyprctl` subprocess.

Two non-obvious facts, checked against the real system:

- **The device name `switchxkblayout` needs isn't `evdev::Device::name()`'s raw
  string.** Hyprland reports a mangled version in `hyprctl -j devices`
  (lowercased, non-alphanumeric runs collapsed to one `-`, trimmed) —
  `"SteelSeries SteelSeries Apex 7 TKL"` → `"steelseries-steelseries-apex-7-tkl"`.
  `grab.rs::hypr_device_name()` reproduces this transform (unit-tested against real
  names); there's no vid/pid/phys field in the JSON to join on instead, so
  name-matching is the only option, with graceful fallback (skip the sync, log it)
  if a transformed name isn't found. The mirror's own name must go through the same
  transform — Hyprland answers a `switchxkblayout` for a nonexistent device without
  an error, so an unmangled name fails silently.
- **`hyprctl dispatch switchxkblayout <name> <index>` fails with a Lua config**
  (`hyprland.lua`, the `hl.*` API — the same layer §3's `hl.bind`/`hl.dsp` belong
  to): it intercepts the `dispatch` socket command and tries to parse its argument
  as Lua source, which the plain dispatcher-args syntax doesn't satisfy. The
  **short-form command works**: `switchxkblayout <name> <index>` sent directly (no
  `dispatch` prefix) — this is what `hyprctl`'s own CLI sends for a handful of
  common dispatchers, and it bypasses the Lua layer entirely (replies `ok`).

**Known limitation, not implemented**: Caps Lock / Num Lock LED state isn't
forwarded from the mirror back to the grabbed physical keyboard. `evdev` 0.13.2's
safe `uinput` builder API has no `with_leds`-style method — only the raw
`sys::ui_set_ledbit` ioctl exists, unsafe and unwrapped by the crate — so declaring
`EV_LED` capability on the mirror device would need an `unsafe` call this codebase
doesn't otherwise need anywhere. Deferred rather than reached for `unsafe` for a
cosmetic indicator; the caps/num-lock *state* itself (tracked per device by
Hyprland regardless of the LED light) isn't affected, only the physical light.

## 9. WebKitGTK accelerated compositing is disabled

`overlay/webview.rs` sets `HardwareAccelerationPolicy::Never` on the WebView. It is
load-bearing.

**Mechanism.** Hiding the overlay (`set_visible(false)`) unmaps it; showing it again
makes gtk4-layer-shell destroy and recreate the surface's `zwlr_layer_surface_v1`
role on the same `wl_surface` (in a `WAYLAND_DEBUG` trace: `zwlr_layer_surface_v1.destroy`
then a fresh `get_layer_surface`). With accelerated (DMA-BUF/EGL) compositing,
WebKit never attaches a real buffer to the surface again after that. GTK's frame
clock keeps running instead: the client acks a `zwlr_layer_surface_v1.configure`
and commits *without* a buffer at the monitor's refresh rate (~60 Hz), forever.
Nothing is displayed after the first hide/show, and since an unbuffered surface is
never mapped it never gets keyboard focus either (hotkeys "do nothing" while the
logic runs fine). Elimination (with a since-removed standalone lab, and without the
game): a plain GTK widget through the same hide/show doesn't storm; a WebView whose
keyboard mode/input region change at runtime without ever being hidden doesn't
storm; reordering keyboard-mode/input-region changes around `set_visible(true)`
makes no difference; Hyprland layout, plugins, VRR, tearing and colour management
were each ruled out. So the trigger is the hide→show remap combined with WebKit's
accelerated path. (Versions at the time: webkitgtk-6.0 2.52.6, GTK 4.22.5,
gtk4-layer-shell 1.3.0, Hyprland 0.56.2. A WebKitGTK bug report draft exists in the
local, gitignored `hyprland/diag/G3_webkitgtk_bug_draft/`; not filed.)

**Why `Never` fixes it.** The non-accelerated path re-derives everything from the
surface on each frame and re-attaches a real buffer through the window's own
surface on every hide/show cycle: a handful of configures (about one per show) and
real buffer attaches on each, `wl_keyboard.enter`/`leave` arriving correctly. It is
the per-WebView equivalent of the process-wide `WEBKIT_DISABLE_COMPOSITING_MODE=1`
(which would have to be set before WebKit initializes); the settings call needs no
`unsafe` env mutation.

**Cost.** Measured with `top -b -d 1`: `ee2-hyprland`, `WebKitWebProcess` and
`WebKitNetworkProcess` all read 0.0 % CPU with the overlay hidden, shown and idle,
and shown while receiving scroll-wheel events. Caveat: that page had nothing long
to scroll; repainting a long, dense trade-result list was not measured.

**Testing re-enabling acceleration** (e.g. after a WebKitGTK update):

1. In `overlay/webview.rs`, temporarily replace `HardwareAccelerationPolicy::Never`
   with `HardwareAccelerationPolicy::Always` (WebKitGTK 6's default); `cargo build
   --release`.
2. Stop the installed instance first (`systemctl --user stop ee2-hyprland`) — the
   app is a single-instance `GApplication`, a second launch just activates the
   running one.
3. Run it with a trace: `WAYLAND_DEBUG=1 ./target/release/ee2-hyprland
   2> /tmp/wl.log`. The overlay is only shown while
   the game is on screen; either use the game, or point `EE2_GAME_CLASS` at a
   stand-in window's class (`hyprctl clients -j`) and focus it.
4. Show and hide the overlay at least three times (overlay hotkey, default
   `Shift + Space`, then `Esc`), leaving it shown for a few seconds each time.
5. Count: `grep -c 'zwlr_layer_surface_v1#[0-9]*\.configure' /tmp/wl.log` vs.
   `grep -c 'wl_surface#[0-9]*\.attach(wl_buffer' /tmp/wl.log` (real buffers;
   `attach(nil` is a hide). Broken: configures climbing by ~60 per second while
   shown and no real attaches after the first hide/show. Healthy: about one
   configure per show and real attaches every time it's shown.
6. Revert the change unless it is healthy *and* worth it.

## 10. Loopback traffic and proxy variables

The renderer is served from our own `127.0.0.1:<port>` server. WebKit's network
stack honors `http_proxy`/`https_proxy` even for loopback when `NO_PROXY` doesn't
exclude it: every asset request then fails (e.g. 503 from a local proxy), the
renderer never mounts, and it looks like a rendering failure rather than a network
one. The binary therefore appends `127.0.0.1` and `localhost` to `NO_PROXY` and
`no_proxy` first thing in `main()` (`exempt_loopback_from_proxy`; existing entries
are kept), before any thread exists — `set_var` isn't safe once other threads may
read the environment. That covers a manual run and any launcher without the unit
(e.g. an AppImage); the systemd unit sets the same values too, as belt-and-braces.
Outbound trade-site requests in `proxy.rs` still use the proxy, as intended.
