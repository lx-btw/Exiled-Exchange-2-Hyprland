# Hyprland maintenance

This fork is in maintenance mode: small fixes and upstream updates. See the
[public README](../.github/README.md) for installation and user-visible limitations,
[AGENTS.md](../AGENTS.md) for the fork boundary, and
[HYPRLAND-NOTES.md](HYPRLAND-NOTES.md) for design rationale.

## CI and repository setup

`hyprland-ci.yml` runs in an Arch container on pushes to `hyprland` and
`upstream-sync` (Markdown-only pushes are skipped), PRs into `hyprland`,
`hyprland-v*` tags and manual dispatch. It checks the fork boundary, formatting,
Clippy and tests, builds the Arch package and AppImage, and uploads both formats
with the AppImage checksum. Artifacts are retained for seven days. Tag builds
publish a GitHub Release.

Repository setup:

- Set `hyprland` as the default branch, so scheduled workflows run from it.
- Allow merge commits only: squash/rebase breaks the requirement that `master`
  remains an ancestor of `hyprland`.
- Add a write-enabled deploy key and store its private half in the Actions secret
  `SYNC_DEPLOY_KEY`. `GITHUB_TOKEN` cannot push upstream commits touching workflows,
  and its pushes would not trigger fork CI.
- Allow GitHub Actions to create pull requests in Settings → Actions → General.
- Disable upstream's `main.yml`, `pages.yml` and `test.yml` workflows. The sync job
  also disables active upstream workflows before advancing `master` and cancels
  runs started by that push.

`hyprland-upstream-sync.yml` runs daily and on manual dispatch. It fast-forwards
`master` from `upstream/master`, merges it into the bot-owned `upstream-sync`
branch and opens or updates a PR into `hyprland`. Conflicts fail the job with a
list of files. The job refuses to advance `master` when upstream contains workflow
files GitHub has not registered yet: register and disable them first, or manually
dispatch with `allow_unregistered_workflows=true` (they may start before being
cancelled). Public repositories' schedules can be disabled after 60 days without
repository activity.

CI skips the two live-Wayland clipboard write tests:
`clipboard::tests::write_text_round_trips_without_a_wl_copy_subprocess` and
`clipboard::tests::write_text_restoring_restores_after_the_wait`. Run them in a
live Hyprland session with `wl-paste` available. CI does not replace an in-game
WebKit smoke test. If the tray test fails inside a future Arch container due to
glycin/bwrap, see the commented container security options in the workflow.

## Releases

Bump `pkgver` in `hyprland/packaging/PKGBUILD` and adjust `pkgrel` as needed.
Commit the change, tag it `hyprland-v<pkgver>` and push the tag. CI checks that the
tag matches `pkgver`, then publishes the Arch package, AppImage and checksum.
The fork has its own version sequence, independent of upstream's version: the
`hyprland-` tag prefix distinguishes fork releases from upstream's `v*` tags,
including `v0.1.0`. Upstream updates do not automatically change the fork version;
the release notes should identify the upstream version and commit used as the
base. Verify the release assets and perform the in-game smoke test below.

## Updating upstream

Use the daily `upstream-sync` → `hyprland` PR:

1. Review `ipc/types.ts` and `ipc/KeyToCode.ts` against `server.rs` and
   `state.rs::handle_ws_event`. Skim the corresponding upstream host changes in
   `main/src` (shortcuts, clipboard, overlay, widget tracking, proxy, server,
   host files, tray and logging) and `renderer/src/web/background/IPC.ts`
   (the Electron user-agent check). Make required changes on the fork side only.
2. Wait for green `hyprland-ci`, then merge with **Create a merge commit**.
3. Update the local `hyprland` and `master` branches. Use the CI package artifact
   or rebuild with `cd hyprland/packaging && makepkg -f`; the PKGBUILD rebuilds
   the renderer before compiling the binary, which embeds `renderer/dist`.
4. Install the package and smoke-test in game: hidden at idle, Ctrl+D prices and
   league loading, Shift+Space interactivity, WASD over the background, widget
   clicks, Esc, stash search, Settings save, and a journal without errors.

For merge conflicts or unavailable CI, start from a clean working tree:

```sh
git fetch upstream
git checkout master
git merge --ff-only upstream/master
git diff $(git merge-base hyprland master) master -- ipc/types.ts ipc/KeyToCode.ts
git checkout hyprland
git merge master
```

Review as above and resolve conflicts only in fork files. This check must be empty:

```sh
git diff master hyprland --stat -- main renderer ipc dataParser docs
```

Rebuild the renderer (`npm ci`, `npm run make-index-files`, `npm run build` in
`renderer/`), then run `cargo build --release --locked`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and `cargo test`
in `hyprland/`. Build/install the package and smoke-test as above. Push `master`
and `hyprland` to `origin`; never push to `upstream` (its push URL should be
`DISABLE`). Never create local commits on `master`.

## Bug triage

Read `journalctl --user -u ee2-hyprland -b` (`-f` to follow). Renderer console
output is included; `ee_log!` messages also appear in Settings. Useful log prefixes:

- `game focus:`, `game on screen:`, `focus-change out:` — game detection and focus.
- `set_interactive(...)`, `render-state`, `overlay-attached`, `ws in:` — state changes.
- `surface:`, `webview pointer over widget:`, `webview input focused:` — surface,
  monitor, keyboard mode and pointer handling.
- `capture:`, `accepted shortcut`, `unparseable shortcut`, `dispatching hotkey` —
  input device access, layout and bindings.
- `copy-item:`, `area-tracker:`, `stash-scroll:`, `proxy request to` — action failures.

A second launch activates the existing `GApplication`; it does not run the new
binary. Stop the service with `systemctl --user stop ee2-hyprland`, or identify and
stop the exact PID with `pgrep -x ee2-hyprland`, before testing another build.

The binary exempts loopback from configured proxies at startup. Requests made
with `curl` to its local server still need `--noproxy '*'`. Use
`EE2_GAME_CLASS=<exact-window-class>` to test with another window; it replaces both
default game classes. For rendering/focus traces use `WAYLAND_DEBUG=1` and consult
[the technical notes](HYPRLAND-NOTES.md#9-webkitgtk-accelerated-compositing-is-disabled).

Settings live in `$HOME/.config/exiled-exchange-2/apt-data/config.json`.
In the Steam library containing PoE2, the game config is under
`steamapps/compatdata/2694490/pfx/drive_c/users/steamuser/Documents/My Games/Path of Exile 2/poe2_production_Config.ini`,
and the client log is under
`steamapps/common/Path of Exile 2/logs/Client.txt`. The default library is usually
`$HOME/.local/share/Steam`; other library locations are supported.

## Diagnostic caveats

- Clipboard data-control reconnects do not recover GTK's primary connection after
  a compositor restart; restart the app. A clipboard receiver that never reads
  can still block the writer thread despite the caller's three-second timeout.
- Only a bare Ctrl, Shift or Alt can be represented by `showModsKey`; unsupported
  game bindings retain the previous value and log a warning.
- Key injection while the pointer is over an interactive widget can reach the
  overlay. Stash-search uses a fixed 50 ms delay before injection.
- Portrait viewport placement and native-Wayland `@last` replies have not been
  separately verified. The restore-last-chat key sequence is deliberately omitted
  because it can leave chat open on Proton/Wayland.
- WebKit uses software rendering; long result-list repaint cost is unmeasured.
  The single WebView broadcasts upstream per-client events, and embedded assets
  have no `Cache-Control`/`ETag` headers.
