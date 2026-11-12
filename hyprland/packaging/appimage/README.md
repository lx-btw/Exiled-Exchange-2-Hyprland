# AppImage (work in progress)

Build on x86_64 Arch Linux: the builder copies installed runtime libraries,
including glibc, GTK4, WebKitGTK, Mesa and LLVM. This is separate from the native
Arch package. No installer, device rules or autostart changes are included.
Device permissions still apply; see [../README.md](../README.md).

For an Xwayland game (`PROTON_ENABLE_WAYLAND=0`), launch with the game's `DISPLAY`
in the environment. The image bundles `xsel` and its libraries, and puts its
wrapper first in `PATH`; no host `xsel` installation is needed. Native Wayland
games do not use this helper. Both Steam/Proton game
classes are recognized automatically; `EE2_GAME_CLASS` is only an exact override
for other window classes.

The rebuilt image was verified with an Xwayland game on November 12 without an
override: Shift+Space, consecutive price checks of different items, clicks and
auto-hide, stash-search, chat commands and stash scrolling passed. Switching
focus to another monitor's window also passed: an unpinned price check closes,
while a pinned one stays over the game. Reply-to-whisper (`@last` prefix) and the
remaining chat commands also passed. Moving the game to a differently scaled
portrait screen exposed a stale monitor-lookup bug; the native fix passed a
round trip back to the primary monitor and restoration of borderless mode.
The latest AppImage includes this fix. Full portrait placement is not verified:
the upstream panel-width formula can exceed a narrow viewport, and the game
itself changed window mode during the move.

The current native build also rediscovers keyboard and mouse devices every second.
On the latest AppImage, in-game acceptance passed for Ctrl+D, Shift+Space, and
keyboard/mouse unplug/replug recovery. Its build dependency-closure check, mounted
startup and main/WebKit runtime library checks passed locally.
SHA256: `6edebef7ae054778bb79b6cd76262bbc3c5e26fe7c71e22878c0568443a84a06`.

The image rebuilt with bundled `xsel` also passed consecutive price checks,
stash search and chat commands. Its mounted main/WebKit GPU mappings passed;
the extracted artifact's helper also ran with no system executable directories
in `PATH`.

## Build

Install the usual renderer/Rust build dependencies (`base-devel`, `nodejs`,
`npm`, `rust`, `gtk4`, `gtk4-layer-shell`, `webkitgtk-6.0`) and the AppImage
requirements: `mesa`, `llvm-libs`, `xsel`, `patchelf`, `bubblewrap`, `xdg-dbus-proxy`,
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
output. It checks the dependency closure of every shipped dynamic ELF and runs
the bundled `xsel --version` with no host executable directories in `PATH`.

The fork CI builds the Arch package and AppImage from the same release binary
and runtime libraries. Successful runs upload separate `ee2-hyprland-pkg` and
`ee2-hyprland-appimage` artifacts. A `hyprland-v<pkgver>` tag publishes both formats and
their checksums in one GitHub Release. CI validates the packaged dependencies
and helper startup; it does not establish gameplay compatibility on other distros.
The first CI-produced image was also downloaded, SHA-256 checked and verified
locally for WebKit startup, library mappings and in-game price checks.

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

## Other distributions and troubleshooting

This is an x86_64 **Hyprland-session** application, including when the game uses
Xwayland. Ubuntu's default GNOME session or a plain X11 session does not provide
the required Hyprland IPC, layer-shell and clipboard protocols. GTK, WebKitGTK,
glibc, software Mesa, `bwrap`, `xdg-dbus-proxy` and `xsel` are bundled; access to
`/dev/uinput` and physical `/dev/input/event*` devices still comes from the host
session. See the device-access guide linked above if startup reports a permissions
error. Run as the desktop user.

Debian, Ubuntu, Fedora and NixOS gameplay is **not verified**. Community testing
is needed; the checks above validate packaging on the build host.

### FUSE and temporary paths

If mounting fails, try the extraction-mode command above. Extraction is a
[documented AppImage fallback](https://docs.appimage.org/user-guide/troubleshooting/fuse.html)
and has been checked locally. It addresses FUSE mounting, not namespace policy
or device permissions. An executable file location is required; an extracted
image also needs sufficient writable temporary space.

The pinned sharun tooling patches WebKit helper paths to a generated `/tmp`
prefix and maps that prefix to the current AppDir. The host must permit this
temporary path mapping; do not remove those paths while the app is running.
Include any failing helper path in a report. Concurrent launches under separate
users or sessions have not been verified.

### Ubuntu AppArmor and restricted namespaces

Ubuntu can restrict unprivileged user namespaces through AppArmor. WebKit's
bubblewrap sandbox needs them; bundled libraries cannot grant host permission.
Ubuntu documents [per-application permission through AppArmor profiles](https://documentation.ubuntu.com/security/security-features/privilege-restriction/apparmor/).
After a failed launch, inspect stderr and these read-only diagnostics:

```sh
sysctl kernel.apparmor_restrict_unprivileged_userns
journalctl -k -b --since "5 minutes ago" | grep 'apparmor="DENIED"'
```

A matching `userns_create` denial is useful evidence; the sysctl value alone
does not prove that this application was denied. Kernel logs may require
administrator access. The relocated bundled helper may not match a profile for
`/usr/bin/bwrap`. An administrator needs to validate a narrowly scoped policy on
that host; an untested profile is not shipped. WebKit's sandbox stays enabled,
and the application does not change AppArmor or global namespace settings.
Extraction mode does not resolve a namespace denial.

### Reporting a compatibility problem

Include the distro/version, architecture, Hyprland version, AppImage SHA-256,
game mode (`PROTON_ENABLE_WAYLAND=0` or `1`), launch stderr and whether extraction
mode behaves differently. For sandbox denials, include the matching AppArmor or
SELinux audit record if available. Report which checks above pass: startup,
price check, clicks, hide/show, stash search, chat and stash scrolling. If Python
is available, also include the runtime checker's result for the active AppDir.
