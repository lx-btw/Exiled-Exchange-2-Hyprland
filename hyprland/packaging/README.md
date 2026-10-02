# Packaging

Build and run without a terminal, lifecycle tied to the game. CI builds the same
package (`.github/workflows/hyprland-ci.yml`) and attaches it to a GitHub Release
for `v<pkgver>` tags; the user-facing overview is `.github/README.md`.

## Build

```sh
cd hyprland/packaging
makepkg -f
```

Builds straight from this checkout's working tree (`renderer/` + `cargo
build --release` in `hyprland/`) — see PKGBUILD's own comments for why
there's no `source=()` array. Install with `sudo pacman -U
ee2-hyprland-*.pkg.tar.zst` yourself; this doesn't run pacman for you.

## Device access: /dev/uinput and /dev/input/event*

This package installs **no udev rule**. Two devices need to be accessible
to whatever user runs `ee2-hyprland`:

- **`/dev/uinput`**, read+write — `input/inject.rs`'s virtual keyboard
  (hotkey key injection, paste-in-chat, stash-search typing) is created
  through it. If this fails, startup aborts immediately (`main.rs` treats a
  `VirtualKeyboard::new()` failure as fatal, by design — see that
  function's doc comment: "a hotkey firing with nothing to inject into is a
  silent no-op from the user's perspective, better to fail loudly at
  startup"). Check `journalctl --user -u ee2-hyprland -b` for the printed
  `io::Error` (typically `Permission denied (os error 13)`).
- **`/dev/input/event*`**, read — `input/hotkeys.rs`'s evdev listener opens
  every keyboard-like device at startup to watch for the configured
  hotkeys, and *only* the ones it can actually watch also become
  `EVIOCGRAB` candidates while the game is focused (`input/grab.rs`). If
  every open fails (or there's genuinely no keyboard-like device), this
  doesn't abort — it logs `"no keyboard-like /dev/input device found —
  hotkeys disabled"` and the overlay runs with hotkeys silently inert
  instead of the whole process refusing to start. Same log location.

**Why no rule is shipped**: a package silently dropping a udev rule at
install time is exactly the kind of unrequested, system-wide side effect
this fork's own design principle for Hyprland's config argues against
(CLAUDE.md's fork-boundary section: the app never edits or binds anything
in Hyprland's config, because a prototype's config binds broke the same key
combos in other applications system-wide). The same reasoning extends to
udev here: granting a process
`/dev/uinput` write access is a real, host-wide capability change (any
process with it can inject arbitrary input system-wide, not just into this
game), so it should be something the person installing this explicitly
chose, not a rule that appeared as a side effect of `pacman -U`.

**On this machine (2026-09-28), no action needed** — access already works
without adding anything:

```
$ ls -la /dev/uinput
crw-rw----+ 1 root input 10, 223 ... /dev/uinput
$ ls -la /dev/input/event0
crw-rw-rw-  1 root input 13, 64 ... /dev/input/event0
```

`/dev/input/event*` is world-readable here already. `/dev/uinput` is
`root:input` mode `0660` plus a `uaccess` ACL tag
(`/usr/lib/udev/rules.d/60-steam-input.rules`'s `KERNEL=="uinput", ...,
TAG+="uaccess"` — shipped by `steam`, for Steam Input controller emulation)
and a `GROUP="input", MODE="0660"` rule
(`/usr/lib/udev/rules.d/80-uinput.rules`, shipped by `ydotool`). Either one
alone would be enough; both happen to be installed already. The user
account (`aleksey`) is not even in the `input` group — the `uaccess` tag
(udev/logind granting the active seat's user ACL access to the device) is
what's actually doing the work.

**On a machine without either of those rules already installed**, grant
access yourself, e.g. one of:

- add your user to the `input` group (`sudo usermod -aG input $USER`,
  re-login) — works if `/dev/uinput` already has a `GROUP="input"` rule
  from some other package (`ydotool`'s is a common one), otherwise add that
  rule yourself first;
- or a rule you write yourself, e.g. `/etc/udev/rules.d/99-ee2-hyprland.rules`:
  ```
  KERNEL=="uinput", GROUP="input", MODE="0660"
  ```
  (`udevadm control --reload-rules && udevadm trigger` after adding it).

Either way, this is a deliberate choice you make on your own machine, not
something `pacman -U ee2-hyprland-*.pkg.tar.zst` does for you.
