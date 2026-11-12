# AppImage (work in progress)

Build on x86_64 Arch Linux: the builder copies installed runtime libraries,
including glibc, GTK4, WebKitGTK, Mesa and LLVM. This is separate from the native
Arch package. No installer, device rules or autostart changes are included.
Device permissions still apply; see [../README.md](../README.md).

For an Xwayland game (`PROTON_ENABLE_WAYLAND=0`), install `xsel` on the host and
launch with the game's `DISPLAY` in the environment. The image currently does
not bundle `xsel`. Native Wayland games do not need it. Both Steam/Proton game
classes are recognized automatically; `EE2_GAME_CLASS` is only an exact override
for other window classes.

The rebuilt image was verified with an Xwayland game on November 12 without an
override: Shift+Space, consecutive price checks of different items, clicks and
auto-hide, stash-search, chat commands and stash scrolling passed. Switching
focus to another monitor's window also passed: an unpinned price check closes,
while a pinned one stays over the game. Reply-to-whisper (`@last` prefix) also
passed. Moving the game to other monitors and trailing forms such as
`/invite @last` still need live checks.

## Build

Install the usual renderer/Rust build dependencies (`base-devel`, `nodejs`,
`npm`, `rust`, `gtk4`, `gtk4-layer-shell`, `webkitgtk-6.0`) and the AppImage
requirements: `mesa`, `llvm-libs`, `patchelf`, `bubblewrap`, `xdg-dbus-proxy`,
`curl` and `binutils`. The script checks required tools rather than installing
anything. Downloads require network access; quick-sharun and its downloaded
helpers are pinned with SHA-256 checksums.

From the repository root:

```sh
(cd renderer && npm ci && npm run make-index-files && npm run build)
(cd hyprland && cargo build --release --locked)
bash hyprland/packaging/appimage/build-appimage.sh
```

An existing binary can be passed as the first argument, for example
`bash hyprland/packaging/appimage/build-appimage.sh /usr/bin/ee2-hyprland`.
Output is `out/ee2-hyprland-<pkgver>-x86_64.AppImage` and its `.sha256` file.
Each build replaces this directory's scratch `build/` and the same-version
output. It checks the dependency closure of every shipped dynamic ELF.

## Rendering

GTK uses cairo. WebKit's acceleration remains disabled, but WebKitGTK still
needs EGL. Therefore the AppImage includes Mesa and LLVM for software rendering
with llvmpipe, sets `LIBGL_ALWAYS_SOFTWARE=1`, and pins
`__EGL_VENDOR_LIBRARY_FILENAMES` to the bundled Mesa JSON. The Vulkan loader can
remain as a linked dependency; Vulkan ICDs and manifests are excluded. This
avoids mixing host GPU drivers with the bundled glibc. The native Arch package
keeps its normal GTK renderer.

## Launch and check

Stop an existing instance first (including `systemctl --user stop ee2-hyprland`
if using the service). A second launch activates the existing GApplication
instead of testing the new image. Launch from a terminal to retain stderr:

```sh
./hyprland/packaging/appimage/out/ee2-hyprland-0.1.0-x86_64.AppImage
```

For systems without working FUSE, use extraction mode (startup and library
mappings verified locally):

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./hyprland/packaging/appimage/out/ee2-hyprland-0.1.0-x86_64.AppImage
```

After the renderer loads, check the **actual application PID**, not the AppImage
runtime or a shell wrapper. `APPDIR` must be the current mounted or extracted
AppDir, for example `/tmp/.mount_ee2hy...`, not this repository directory:

```sh
python3 hyprland/packaging/appimage/check-runtime.py PID APPDIR
```

The checker inspects the main process and descendant WebKitWebProcess mappings.
It rejects host GPU libraries, requires a WebKitWebProcess and bundled Mesa EGL
in at least one checked process. Each process need not load Mesa: a WebKit
process may map only the bundled EGL loader. Read access to `/proc/PID/maps` is
required; run as the same user in the same session.

A local mounted-image check passed with the renderer sending `render-state`, bundled
Mesa/EGL/LLVM in the main process and no host GPU libraries in either checked
process. The rebuilt software-rendering image also passed live game checks for
price-check rendering, clicks, auto-hide, repeated opening and Shift+Space on
November 12. Stash search, chat commands and stash scrolling also passed with
an Xwayland game. For future builds, repeat these checks:

The image includes two native workarounds confirmed in game on November 12:
with GTK 4.24/WebKitGTK 2.54, initially transparent windows can fail to submit
their first buffer. A nearly transparent GTK background for the first visible
frame restores HTML. Remapping a mapped passthrough surface when it becomes
interactive restores mouse input to price checks. Native rendering, clicks,
auto-hide and the Shift+Space overlay passed. The rebuilt image also passed
mounted startup, the runtime library check and these same overlay checks in game.

1. Start PoE2 and focus the game; open a price check and verify content/position.
2. Hide and reopen the overlay repeatedly; verify transparent areas and input
   passthrough, interactive mode, and return to the game.
3. Check stash search, paste in chat and stash scrolling; verify ordinary typing
   and hotkeys in other applications after switching focus.
4. Re-run the runtime checker while the overlay is active and inspect stderr.

Ubuntu 24.04+ AppArmor user-namespace restrictions are not handled yet. WebKit's
sandbox remains enabled; a namespace failure needs investigation on that host.
Debian, Fedora and NixOS checks, Ubuntu handling, `/tmp` path-mapping caveats and
release/CI integration remain pending. The host accepts newer Hyprland monitor
replies whose empty `specialWorkspace` has no `id`; rebuild the binary before
packaging to include this compatibility fix.
