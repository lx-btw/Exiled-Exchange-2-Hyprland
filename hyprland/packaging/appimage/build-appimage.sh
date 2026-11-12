#!/usr/bin/env bash
#
# Builds a portable AppImage of ee2-hyprland with quick-sharun from
# pkgforge-dev/Anylinux-AppImages:
#   https://pkgforge-dev.github.io/Anylinux-AppImages/HOW-TO-MAKE-THESE.html
#
# Usage: build-appimage.sh [path-to-binary]
#   default binary: hyprland/target/release/ee2-hyprland (relative to the repo
#   root this script lives in); CI passes /usr/bin/ee2-hyprland after
#   installing the pacman package. The renderer is embedded in the binary
#   (rust-embed), so the binary is the only fork-specific input.
# Output: hyprland/packaging/appimage/out/ee2-hyprland-<pkgver>-x86_64.AppImage
#   plus <file>.sha256. pkgver is read from ../PKGBUILD so the two artifacts
#   of a release always carry the same version.
#
# Why quick-sharun and not a hand-rolled AppDir: it bundles glibc and the
# dynamic loader (no "build on an old distro" requirement, works on musl
# hosts), runs everything through sharun's `ld.so --library-path` instead of
# LD_LIBRARY_PATH (nothing leaks into child processes such as xsel), and it
# has WebKitGTK-specific handling we'd otherwise have to reproduce — all
# verified against the pinned quick-sharun.sh below:
#   - any linked libwebkit*gtk-*.so pulls in its helper dir
#     (/usr/lib/webkitgtk-6.0: WebKitWebProcess, WebKitNetworkProcess, ...),
#     plus glib-networking, p11-kit and the Mesa software EGL stack;
#   - /usr/lib and /usr/bin inside libwebkitgtk-6.0.so are byte-patched to a
#     /tmp/<random> prefix that its path-mapping hook symlinks to the mounted
#     AppImage at runtime, so the helpers are spawned from the AppImage;
#   - bwrap and xdg-dbus-proxy are bundled behind sharun's bwrap wrapper, so
#     WebKit's own sandbox works and still sees the AppImage paths.
#
# Requirements (checked below, never installed): the binary's runtime deps
# installed at their normal /usr locations (gtk4, gtk4-layer-shell,
# webkitgtk-6.0 — quick-sharun copies them from the host, so build on Arch),
# plus patchelf, bubblewrap, xdg-dbus-proxy and curl. zsync is NOT needed:
# the pinned appimagetool has zsync generation built in and only does it when
# update info (UPINFO) is set, which this script never sets.
set -euo pipefail

# --- pinned upstream tooling --------------------------------------------------
# quick-sharun.sh is fetched from a fixed commit and checked against its
# sha256. Everything *it* downloads (sharun + helper libs, appimagetool with
# its embedded uruntime/mkdwarfs, cross-libc-dlopen) is pinned to release
# URLs + sha256 inside that very file, so pinning this one file pins the
# whole toolchain. To bump:
#   1. pick a commit from https://github.com/pkgforge-dev/Anylinux-AppImages/commits/main
#   2. curl -fsSL https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/<commit>/useful-tools/quick-sharun.sh | sha256sum
#   3. update both values below, rebuild, re-run the checks (ldd over the
#      extracted AppDir, a real launch) before publishing.
QUICK_SHARUN_COMMIT=ff9260f61b9953bd07dc76d6d8b204ed816e8731 # 2026-10-02
QUICK_SHARUN_SHA256=8026711b271c0d67d37075cd7e8c50dbd9fa635c012f9edb92a2c0d41573664b
QUICK_SHARUN_URL="https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/${QUICK_SHARUN_COMMIT}/useful-tools/quick-sharun.sh"

# --- paths ----------------------------------------------------------------------
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/../../.." && pwd)

bin=${1:-$repo_root/hyprland/target/release/ee2-hyprland}
build_dir=$script_dir/build # gitignored: downloads, AppDir
out_dir=$script_dir/out     # gitignored: the AppImage + .sha256

arch=$(uname -m)
if [[ $arch != x86_64 ]]; then
  # PKGBUILD is arch=('x86_64') too; nothing else has ever been tested.
  echo "error: only x86_64 is supported (this is $arch)" >&2
  exit 1
fi

if [[ ! -x $bin ]]; then
  echo "error: binary not found or not executable: $bin" >&2
  echo "build it first (AGENTS.md, Commands) or pass a path, e.g. /usr/bin/ee2-hyprland" >&2
  exit 1
fi
bin=$(readlink -f -- "$bin")

pkgver=$(sed -n 's/^pkgver=//p' "$repo_root/hyprland/packaging/PKGBUILD")
if [[ -z $pkgver ]]; then
  echo "error: could not read pkgver from hyprland/packaging/PKGBUILD" >&2
  exit 1
fi
outname=ee2-hyprland-$pkgver-$arch.AppImage

# --- required tools -------------------------------------------------------------
# quick-sharun's own DEPENDENCIES list (it aborts on the first missing one;
# we report all of them at once), plus curl for our download, and bwrap /
# xdg-dbus-proxy: quick-sharun only bundles them if they're on PATH and
# silently skips them otherwise — which would leave WebKit's sandbox to
# whatever the user's host happens to have.
required=(awk cp env find grep ldd mv patchelf rm sed sha256sum sleep strings
  tar tr curl bwrap xdg-dbus-proxy readelf od paste)
missing=()
for tool in "${required[@]}"; do
  command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if ((${#missing[@]})); then
  echo "error: missing required tools: ${missing[*]}" >&2
  echo "(on Arch: patchelf, bubblewrap, xdg-dbus-proxy, curl, binutils for strings/readelf)" >&2
  exit 1
fi

# --- fresh scratch dir ------------------------------------------------------------
# quick-sharun is incremental against an existing AppDir (it skips steps
# whose output exists), so a stale AppDir from an older binary or older
# pinned tooling could silently survive. Always start clean.
rm -rf "${build_dir:?}"
mkdir -p "$build_dir/tmp" "$build_dir/shim" "$out_dir"
rm -f "$out_dir/$outname" "$out_dir/$outname.sha256"

qs=$build_dir/quick-sharun
curl -fsSL --retry 3 -o "$qs" "$QUICK_SHARUN_URL"
if ! echo "$QUICK_SHARUN_SHA256  $qs" | sha256sum -c --quiet -; then
  echo "error: sha256 mismatch for quick-sharun.sh at $QUICK_SHARUN_COMMIT" >&2
  exit 1
fi
chmod +x "$qs"

# Icon file name must match the desktop entry's Icon= key (desktop
# integration tools look it up by that name). main/build/icons/ is
# upstream's electron-builder icon set — read-only use, same as PKGBUILD.
cp "$repo_root/main/build/icons/512x512.png" "$build_dir/ee2-hyprland.png"

# quick-sharun runs `export $(dbus-launch)` at the top of every invocation
# so the app it runs in strace mode has a session bus. With STRACE_MODE=0
# nothing uses that bus, and on a desktop machine it would just leave an
# orphan dbus-daemon behind per invocation. A failing dbus-launch on PATH
# makes it take its own NO_DBUS fallback.
printf '#!/bin/sh\nexit 1\n' >"$build_dir/shim/dbus-launch"
chmod +x "$build_dir/shim/dbus-launch"

# --- quick-sharun configuration -------------------------------------------------
export PATH="$build_dir/shim:$PATH"
export TMPDIR=$build_dir/tmp # quick-sharun's download/scratch location
export APPDIR=$build_dir/AppDir
export OUTPATH=$out_dir
export OUTNAME=$outname
export MAIN_BIN=ee2-hyprland
export DESKTOP=$script_dir/ee2-hyprland.desktop
# appimagetool writes X-AppImage-Version into the embedded desktop entry from
# $VERSION ("UNKNOWN" otherwise); OUTNAME above already fixes the file name.
export VERSION=$pkgver
export ICON=$build_dir/ee2-hyprland.png

# STRACE_MODE=0: strace mode finds dlopen()ed libs by *running* the binary
# for a few seconds under LD_DEBUG=libs. For this app that means a full
# second instance on the build machine — HTTP server, uinput virtual
# keyboard, evdev hotkey capture — on a private dbus session (so
# GApplication's single-instance check wouldn't even see a running copy).
# In a CI container there's no Wayland display, so it would find nothing
# anyway. Everything this app dlopens is covered by quick-sharun's explicit
# GTK4 / WebKitGTK deployment rules instead; the launch check
# (process maps all inside the AppImage mount) is what verifies that.
export STRACE_MODE=0

# DEPLOY_DATADIR=0: its heuristic greps `strings` of the main binary for
# /usr/share/<name> directory names to copy in. Our binary embeds the whole
# renderer bundle, so that heuristic only produces noise; the binary needs
# no /usr/share data of its own (GTK/GLib schemas, xkb, fontconfig, etc. are
# added per-library by separate rules that this does not disable).
export DEPLOY_DATADIR=0

# DEPLOY_GSTREAMER=0: libgstreamer itself is still bundled (libwebkitgtk
# links it), but no plugins: the overlay plays no audio/video. Outside CI,
# quick-sharun's "minimal" GStreamer mode doesn't prune anything and would
# copy every plugin installed on the host. Override with
# EE2_DEPLOY_GSTREAMER=1 if media ever turns out to be needed.
export DEPLOY_GSTREAMER=${EE2_DEPLOY_GSTREAMER:-0}

# WebKitWebProcess needs EGL even with HardwareAccelerationPolicy::Never.
# Bundle Mesa + its LLVM dependency for llvmpipe; ALWAYS_SOFTWARE=1 would
# omit them and silently fall back to the host EGL stack instead.
export ALWAYS_SOFTWARE=0
export USE_HOST_DRIVERS_EXPERIMENTAL=0
export DEPLOY_OPENGL=1
export DEPLOY_VULKAN=0

# No update information: we don't publish .zsync files, and the pinned
# appimagetool would otherwise guess a gh-releases-zsync string from CI's
# GITHUB_REPOSITORY and write a .zsync next to the AppImage.
unset UPINFO GITHUB_REPOSITORY

# --- build ------------------------------------------------------------------------
cd "$build_dir"
"$qs" "$bin"

# --- prune the AppDir -------------------------------------------------------------
# quick-sharun deploys by generic rules; these are the parts that are
# provably dead weight for this app. Removing a library that some shipped
# ELF still links is never allowed: the closure check below fails the build
# if a prune ever breaks one (e.g. a future libheif becoming a dependency of
# something we keep).
prune() {
  local f
  for f in "$@"; do
    [[ -e $f || -L $f ]] || continue
    rm -rf -- "$f"
    echo "pruned: ${f#"$APPDIR"/}"
  done
}
shopt -s nullglob
# The Vulkan loader is linked by GTK/WebKit and stays, but no Vulkan ICD
# or manifest is needed. quick-sharun copies manifests even with deployment
# disabled when it sees that loader in the linked dependency closure.
prune "$APPDIR"/share/vulkan "$APPDIR"/lib/libvulkan_*.so*
# Only Mesa may provide EGL; the build host may also have NVIDIA manifests.
for vendor in "$APPDIR"/share/glvnd/egl_vendor.d/*.json; do
  [[ ${vendor##*/} == 50_mesa.json ]] || prune "$vendor"
done
# WebKitGTK's developer tools, deployed only because the whole helper dir
# /usr/lib/webkitgtk-6.0 is. WebKit spawns only its *Process helpers.
prune "$APPDIR"/{bin,shared/bin,lib/webkitgtk-6.0}/{MiniBrowser,jsc}
# glycin HEIF and JPEG XL loaders: glycin is GTK's decoder for image
# formats GTK has no built-in loader for; the only images GTK itself shows
# here are its own PNG/SVG icons (the tray icon is raw ARGB data over DBus).
# The renderer's images are decoded inside WebKit, which links libjxl and
# libavif itself (those stay). The heif loader is what dragged in libheif and
# its codecs (x265 alone is 21 MiB); no other shipped ELF links them.
# glycin-image-rs (PNG/JPEG/GIF/WebP/...) and glycin-svg stay.
prune "$APPDIR"/{bin,shared/bin}/glycin-{heif,jxl} \
  "$APPDIR"/share/glycin-loaders/*/conf.d/glycin-{heif,jxl}.conf \
  "$APPDIR"/lib/lib{heif,de265,x264,x265,openh264,jxl_threads}.so*
# GTK's own translations (gtk40.mo, ~9 MiB in 100+ languages; it's the only
# domain quick-sharun's locale debloat left): the overlay shows no GTK-drawn
# text, all UI is the renderer's web page with its own i18n. Without them
# GTK's few internal strings (e.g. the emoji chooser) are simply English.
prune "$APPDIR"/share/locale
shopt -u nullglob
# lib.path (sharun's library search dirs) was generated before the prune.
"$APPDIR"/sharun -g

# sharun expands ${SHARUN_DIR} in .env at runtime (as in quick-sharun's
# CROSS_LIBC_DLOPEN_ROOT), so this remains valid after mounting anywhere.
# .env overrides caller settings, including a global GSK_RENDERER=ngl.
cat >>"$APPDIR/.env" <<'EOF'
GSK_RENDERER=cairo
GDK_DISABLE=gl,vulkan
GDK_GL=disable
LIBGL_ALWAYS_SOFTWARE=1
__EGL_VENDOR_LIBRARY_FILENAMES=${SHARUN_DIR}/share/glvnd/egl_vendor.d/50_mesa.json
LIBGL_DRIVERS_PATH=${SHARUN_DIR}/lib/dri
SHARUN_ALLOW_SYS_VKICD=0
EOF

# These dlopen inputs are invisible to the ELF closure check below.
for file in lib/libEGL_mesa.so.0 lib/dri/swrast_dri.so \
  share/glvnd/egl_vendor.d/50_mesa.json; do
  if [[ ! -f $APPDIR/$file ]]; then
    echo "error: bundled software EGL input is missing: $file" >&2
    exit 1
  fi
done
shopt -s nullglob
llvm=("$APPDIR"/lib/libLLVM.so*)
vulkan=("$APPDIR"/lib/libvulkan_*.so* "$APPDIR"/share/vulkan/icd.d/*.json)
shopt -u nullglob
if ((${#llvm[@]} == 0 || ${#vulkan[@]} != 0)); then
  echo "error: software EGL must include LLVM and exclude Vulkan ICDs" >&2
  exit 1
fi
if ! grep -Eq '"library_path"[[:space:]]*:[[:space:]]*"libEGL_mesa.so.0"' \
  "$APPDIR/share/glvnd/egl_vendor.d/50_mesa.json"; then
  echo "error: Mesa EGL manifest must resolve the bundled libEGL_mesa.so.0" >&2
  exit 1
fi

# --- closure check ------------------------------------------------------------------
# Resolve every shipped ELF's dependencies with the *bundled* loader and only
# the bundled library dirs (exactly what sharun does at runtime): nothing may
# be missing and nothing may come from outside the AppDir. Mesa drivers
# are dlopen()ed; their presence is checked above, while a real launch must
# also verify that process maps contain no host GPU libraries.
libpath=$(sed -e 's|^+|'"$APPDIR"'/lib|' "$APPDIR"/lib/lib.path | paste -sd: -)
bad=0
while IFS= read -r -d '' f; do
  [[ -L $f ]] && continue
  [[ $(head -c4 -- "$f" | od -An -tx1 | tr -d ' ') == 7f454c46 ]] || continue
  # static ELFs (sharun and its hardlinks) have nothing to resolve, and
  # ld.so --list crashes on them
  readelf -d -- "$f" 2>/dev/null | grep -q '(NEEDED)' || continue
  if ! out=$("$APPDIR"/lib/ld-linux-x86-64.so.2 --library-path "$libpath" --list "$f" 2>&1); then
    echo "closure: $f: $out" >&2 # a missing library makes ld.so fail outright
    bad=1
    continue
  fi
  while IFS= read -r line; do
    case $line in
    *"not found"*) echo "closure: $f: $line" >&2; bad=1 ;;
    *"=> /"*)
      case $line in *"=> $APPDIR/"*) ;; *) echo "closure: $f: outside AppDir: $line" >&2; bad=1 ;; esac
      ;;
    esac
  done <<<"$out"
done < <(find "$APPDIR" -type f -print0)
if ((bad)); then
  echo "error: AppDir dependency closure is broken (see above)" >&2
  exit 1
fi
echo "closure: ok"
echo "AppDir size: $(du -sh "$APPDIR" | cut -f1)"

"$qs" --make-appimage

if [[ ! -f $out_dir/$outname ]]; then
  echo "error: expected $out_dir/$outname was not produced" >&2
  exit 1
fi
# appimagetool also drops an `appinfo` metadata file into OUTPATH (used by
# pkgforge's own release tooling); we don't publish it.
rm -f "$out_dir/appinfo"
(cd "$out_dir" && sha256sum "$outname" >"$outname.sha256")

echo
echo "AppImage: $out_dir/$outname ($(du -h "$out_dir/$outname" | cut -f1))"
echo "sha256:   $(cut -d' ' -f1 "$out_dir/$outname.sha256")"
