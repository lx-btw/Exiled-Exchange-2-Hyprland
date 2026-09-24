//! Overlay mode (passthrough/interactive) and game-focus tracking — the
//! state `server.rs` and `overlay/window.rs` both need to agree on. Ported
//! from upstream's `main/src/windowing/OverlayWindow.ts` state machine and
//! the prior prototype's `host.mjs` (`overlayInteractive`, `tellOverlay()`,
//! `onActiveWindow()`), adapted to a single-process design: `host.mjs` sent
//! `tellOverlay()` commands over a unix socket to a *separate* overlay
//! process; here the overlay lives in the same process, so this just calls
//! `overlay::window::set_input_region()`/`set_keyboard_mode()` directly
//! (marshaled onto the GTK main thread — see `register_window`/
//! `set_interactive`).
//!
//! Owns the whole price-check flow end to end (`run_copy_item`): find the
//! game window, inject `Ctrl+Alt+C`, read the clipboard back, broadcast
//! `item-text` to the renderer, switch the overlay interactive. This is
//! "port `host.mjs`'s `runPriceCheck()`", not new design.
//!
//! Stage 4 (see PLAN.md).

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch, Mutex as TokioMutex};

use crate::area_tracker;
use crate::clipboard::{self, ClipboardEvent};
use crate::compositor;
use crate::config::{ConfigStore, GameConfig};
use crate::input::hotkeys::Bindings;
use crate::input::inject::VirtualKeyboard;
use crate::input::keymap::{self, Modifier, Shortcut};
use crate::overlay;

/// How long after switching to interactive mode an incoming `focus-game`
/// event is ignored — ported verbatim from the prior prototype's
/// `EE2_FOCUS_GRACE` (default 600ms): the renderer was observed sending a
/// burst of `focus-game` events ~18ms after interactive mode enables, with
/// no corresponding mouse event on the page (cause never fully identified
/// upstream either, see HYPRLAND-NOTES.md §8).
///
/// **Resolved, Stage 5**: `render-state` does *not* replace this — read
/// `OverlayWindow.vue`'s actual source rather than continuing to guess.
/// `render-state`'s `shouldShow` payload is `isOverlayVisible`, a completely
/// different signal: "does any widget currently have visible content" (used
/// by `set_visible` below). `focus-game` is sent by `handleBackgroundClick`/
/// `closePriceCheck` — explicit "user wants focus back on the game" actions
/// (background click, closing a widget). The two are unrelated; nothing
/// about `render-state` bears on why the renderer's `focus-game` burst
/// happens right after activation. This grace window stays.
const FOCUS_GAME_GRACE: Duration = Duration::from_millis(600);

/// How long to wait for the clipboard to show item text after injecting the
/// copy combo — matches `host.mjs::copyItemText`'s `timeoutMs`.
const COPY_ITEM_TIMEOUT: Duration = Duration::from_millis(2500);

thread_local! {
    static WINDOW: RefCell<Option<gtk4::ApplicationWindow>> = const { RefCell::new(None) };
    // Both GTK-main-thread-only, mirroring state that's authoritative
    // elsewhere (`Overlay::overlay_interactive`'s `bool`, and the WebView's
    // own DOM focus) — cached here purely so `apply_keyboard_mode` (H2B,
    // below) can be called synchronously from a WebKit signal handler
    // (`webview.rs`, fires on this same thread) without needing the async
    // `TokioMutex<Overlay>` lock, which a synchronous GTK/WebKit C callback
    // can't await into.
    static INTERACTIVE: Cell<bool> = const { Cell::new(false) };
    static INPUT_FOCUSED: Cell<bool> = const { Cell::new(false) };
}

/// Must be called once, on the GTK main thread, right after the overlay
/// window is created — registers it so `set_interactive` (callable from any
/// thread) has something to marshal `overlay::window` calls onto. Without
/// this, there is no safe way to touch the window from the tokio tasks that
/// receive hotkey/WS/compositor events, since GTK objects aren't `Send`.
pub fn register_window(window: gtk4::ApplicationWindow) {
    WINDOW.with(|w| *w.borrow_mut() = Some(window));
}

/// Applies `KeyboardMode::Exclusive` iff the overlay is currently
/// mouse-interactive *and* an input element inside the WebView currently
/// has DOM focus — `None` otherwise. H2B (PLAN.md): before this, keyboard
/// mode was tied 1:1 to interactive/passthrough (`set_passthrough_on_main_thread`
/// set `Exclusive` the instant the overlay became interactive at all), so
/// `Shift+Space` opening the overlay stole every keystroke from the game
/// immediately, not just while actually typing into a field — this is what
/// makes WASD keep working while the overlay is open and mouse-clickable.
/// Called from two places, both already on the GTK main thread:
/// `set_passthrough_on_main_thread` (when `Overlay.overlay_interactive`
/// changes) and `set_input_focused` (when DOM focus changes, `webview.rs`).
fn apply_keyboard_mode() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let exclusive = INTERACTIVE.with(|i| i.get()) && INPUT_FOCUSED.with(|f| f.get());
            overlay::window::set_keyboard_mode(window, exclusive);
        }
    });
}

/// Called from `webview.rs`'s WebKit script-message handler (fires on the
/// GTK main thread already, no marshaling needed — see that call site) with
/// whatever the injected focus-bridge script last observed. Deliberately
/// not gated on `INTERACTIVE` here — `apply_keyboard_mode` already factors
/// that in, and caching the raw DOM-focus signal unconditionally means a
/// field that was focused before the overlay went passthrough (then became
/// interactive again without a fresh click) correctly regains keyboard
/// focus too, matching how a real desktop app's focus state persists across
/// a window being hidden and shown again.
pub fn set_input_focused(focused: bool) {
    INPUT_FOCUSED.with(|f| f.set(focused));
    apply_keyboard_mode();
}

fn set_passthrough_on_main_thread(passthrough: bool) {
    gtk4::glib::MainContext::default().invoke(move || {
        WINDOW.with(|w| {
            if let Some(window) = w.borrow().as_ref() {
                // Becoming interactive (passthrough=false) shows the window
                // *first*, in this same GTK-thread callback, before setting
                // `KeyboardMode::Exclusive` — found live (2026-09-24), once
                // `main.rs` started the surface hidden by default at
                // startup: `set_keyboard_mode`/`set_input_region` applied to
                // a not-yet-mapped layer surface silently didn't take
                // effect, so the overlay opened with no visible content
                // *and* without ever actually capturing keyboard input —
                // hotkey presses (Ctrl+D, Shift+Space) leaked straight
                // through to the game underneath (character moved/rolled)
                // exactly as if nothing were listening. Waiting on the
                // renderer's own independent `render-state` event to show
                // the window instead arrives too late for this: it's a full
                // WS round-trip that can only complete *after* this
                // function has already run and already (silently) failed to
                // grab the keyboard.
                if !passthrough {
                    overlay::window::set_visible(window, true);
                }
                overlay::window::set_input_region(window, passthrough);
            }
        });
        INTERACTIVE.with(|i| i.set(!passthrough));
        apply_keyboard_mode();
    });
}

/// The overlay's current monitor's origin in Hyprland's global layout
/// coordinates — see `overlay::window::current_monitor_origin`'s docs for
/// why `run_copy_item`/`handle_ws_event`'s `track-area` handling need this at
/// all. `None` only if the window doesn't exist yet or GDK can't resolve any
/// monitor (vanishingly unlikely — see that function's own fallback).
async fn overlay_monitor_origin() -> Option<(i32, i32)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    gtk4::glib::MainContext::default().invoke(move || {
        WINDOW.with(|w| {
            let origin = w
                .borrow()
                .as_ref()
                .and_then(overlay::window::current_monitor_origin);
            let _ = tx.send(origin);
        });
    });
    rx.await.ok().flatten()
}

fn set_visible_on_main_thread(visible: bool) {
    gtk4::glib::MainContext::default().invoke(move || {
        WINDOW.with(|w| {
            if let Some(window) = w.borrow().as_ref() {
                overlay::window::set_visible(window, visible);
            }
        });
    });
}

/// A parsed `ShortcutAction` (`ipc/types.ts`) this fork can actually act on.
/// Action types not listed here (`ocr-text`, `paste-in-chat`, `test-only`)
/// are still Stage 5+ and are dropped during parsing — see `parse_bindings`.
#[derive(Debug, Clone)]
enum Action {
    CopyItem { target: String, focus_overlay: bool },
    ToggleOverlay,
    TriggerEvent { target: String },
    StashSearch { text: String },
}

#[derive(Debug, Clone)]
struct Binding {
    shortcut: Shortcut,
    action: Action,
}

/// Parses `HostConfig.shortcuts` (the array the renderer sends in
/// `CLIENT->MAIN::update-host-config`) into what this fork can act on.
/// Entries with an unparseable shortcut string or an unsupported action
/// type are dropped, not stored inert — logged once so a silently-ignored
/// hotkey isn't a total mystery, matching `host.mjs::bindLines()`'s own
/// "action not yet supported" logging for the same cases.
fn parse_bindings(shortcuts: &[Value]) -> Vec<Binding> {
    shortcuts
        .iter()
        .filter_map(|entry| {
            let shortcut_str = entry.get("shortcut")?.as_str()?;
            let Some(shortcut) = keymap::parse_shortcut(shortcut_str) else {
                eprintln!("ee2-hyprland: unparseable shortcut \"{shortcut_str}\", skipping");
                return None;
            };

            let action_val = entry.get("action")?;
            let action_type = action_val.get("type")?.as_str()?;
            let action = match action_type {
                "copy-item" => {
                    let Some(target) = action_val.get("target").and_then(Value::as_str) else {
                        eprintln!(
                            "ee2-hyprland: shortcut \"{shortcut_str}\": copy-item action missing \"target\", skipping"
                        );
                        return None;
                    };
                    Action::CopyItem {
                        target: target.to_string(),
                        focus_overlay: action_val
                            .get("focusOverlay")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }
                }
                "toggle-overlay" => Action::ToggleOverlay,
                "trigger-event" => {
                    let Some(target) = action_val.get("target").and_then(Value::as_str) else {
                        eprintln!(
                            "ee2-hyprland: shortcut \"{shortcut_str}\": trigger-event action missing \"target\", skipping"
                        );
                        return None;
                    };
                    Action::TriggerEvent {
                        target: target.to_string(),
                    }
                }
                "stash-search" => {
                    let Some(text) = action_val.get("text").and_then(Value::as_str) else {
                        eprintln!(
                            "ee2-hyprland: shortcut \"{shortcut_str}\": stash-search action missing \"text\", skipping"
                        );
                        return None;
                    };
                    Action::StashSearch {
                        text: text.to_string(),
                    }
                }
                other => {
                    eprintln!(
                        "ee2-hyprland: shortcut \"{shortcut_str}\": action \"{other}\" not yet supported, skipping"
                    );
                    return None;
                }
            };

            eprintln!("ee2-hyprland: accepted shortcut \"{shortcut_str}\" -> {action_type}");
            Some(Binding { shortcut, action })
        })
        .collect()
}

/// The copy-item flow's exclusive resources, behind their *own* lock,
/// separate from the rest of `Overlay`'s state. `run_copy_item` holds this
/// lock (not `Overlay`'s) across its slow clipboard wait (up to
/// `COPY_ITEM_TIMEOUT` — 2.5s worst case) precisely so that unrelated WS
/// message handling (`handle_ws_event`) and focus tracking
/// (`set_game_active`) — both called via `Overlay`'s own lock — don't block
/// behind an in-progress price check. `check_in_flight` lives here rather
/// than on `Overlay` for the same reason: it's this lock, not `Overlay`'s,
/// that already serializes concurrent copy-item attempts.
struct CopyItemState {
    keyboard: VirtualKeyboard,
    clipboard_rx: mpsc::UnboundedReceiver<ClipboardEvent>,
    check_in_flight: bool,
}

pub struct Overlay {
    config_store: ConfigStore,
    game_config: GameConfig,
    copy_item: Arc<TokioMutex<CopyItemState>>,
    bindings: Vec<Binding>,
    hotkey_bindings: Bindings,
    events_tx: broadcast::Sender<Value>,
    game_active: bool,
    /// Mirrors `game_active` out to `hotkeys.rs::watch()` — a `watch::Sender`
    /// rather than a plain `Arc<AtomicBool>` (changed 2026-09-24, H2A) so
    /// that loop can react to a focus change the instant it happens (via
    /// `select!` on `.changed()`), not only on the next keystroke. That
    /// distinction didn't matter before capture existed (matching was
    /// stateless either way), but it does now: capture needs to grab a
    /// device the moment the game gains focus, or the very first hotkey
    /// press after every alt-tab leaks its trigger key into the game (see
    /// PLAN.md's H2 entry) — an `Arc<AtomicBool>` gives no way to wait for
    /// that transition, only to poll it. Ports upstream's `Shortcuts.ts`:
    /// `poeWindow.on("active-change", ...)` calls `globalShortcut`
    /// `register()`/`unregister()`, so hotkeys only fire (and capture only
    /// grabs) while PoE2 is the focused window. Found missing live
    /// (2026-09-24): without some form of this gate, e.g. `Shift + Space`
    /// toggled the overlay from *any* focused window, not just the game — a
    /// real behavioral gap from upstream, not a hypothetical.
    hotkeys_active: watch::Sender<bool>,
    overlay_interactive: bool,
    /// Mirrors `overlay_interactive` out to `hotkeys.rs::watch()`, same
    /// `watch` shape and reason as `hotkeys_active` — added 2026-09-24, H2B,
    /// for evdev-level `Esc` handling: upstream's `OverlayWindow.ts`
    /// hardcodes `Escape` (in its `before-input-event` handler, entirely
    /// separate from `Shortcuts.ts`'s user-configurable bindings) to always
    /// close an interactive overlay, so `hotkeys.rs`'s central loop needs to
    /// know "is the overlay currently interactive" independent of whether
    /// this specific keypress happens to match a configured hotkey.
    overlay_interactive_tx: watch::Sender<bool>,
    interactive_since: Instant,
    /// Ports upstream's `OverlayWindow.wasUsedRecently` (default `true`),
    /// updated from `CLIENT->MAIN::used-recently`'s `isOverlay` field —
    /// `run_copy_item` only auto-focuses the overlay on a `focusOverlay`
    /// copy (the locked/`Ctrl+Alt+D` hotkey) when this is also true, exactly
    /// matching `Shortcuts.ts`'s `if (action.focusOverlay &&
    /// this.overlay.wasUsedRecently)`.
    used_recently: bool,
    /// `area_tracker::watch`'s command channel, wired up once from
    /// `main.rs` via `attach_area_tracker` — `None` only in the brief window
    /// before that call happens, during which `track-area`/copy-item/
    /// toggle-overlay silently skip starting or stopping a tracking session
    /// rather than panicking.
    area_tracker: Option<area_tracker::Handle>,
    /// A weak handle back to the `Arc<TokioMutex<Overlay>>` this instance
    /// lives inside — set once in `main.rs` right after that `Arc` is
    /// constructed (`Overlay::new` can't fill it in itself, chicken-and-egg:
    /// the `Arc` doesn't exist yet). `handle_ws_event` is the one caller:
    /// it runs with the lock already held (`server.rs`'s
    /// `overlay.lock().await.handle_ws_event(...)`), so a WS-triggered
    /// `stash-search` (an "everyday" trigger path — it's a button click
    /// *inside* an already-open widget, not a hotkey) needs this to spawn
    /// `run_stash_search` as its own task rather than running the whole
    /// clipboard-write-plus-key-injection sequence inline and blocking
    /// every other WS message and focus-change update for its duration —
    /// the same class of problem `run_copy_item`'s split-lock design
    /// avoids, see its docs.
    self_ref: Weak<TokioMutex<Overlay>>,
}

impl Overlay {
    /// `events_tx` is shared with `server.rs`, which subscribes a fresh
    /// receiver per WS connection. Returns the constructed state plus the
    /// `Bindings` handle and the `hotkeys_active` flag `hotkeys::watch()`
    /// needs — created here (empty/false) rather than in `main.rs`, since
    /// `update_host_config`/`set_game_active` are what actually populate
    /// them and that only makes sense as `Overlay` methods.
    pub fn new(
        events_tx: broadcast::Sender<Value>,
    ) -> std::io::Result<(Self, Bindings, watch::Receiver<bool>, watch::Receiver<bool>)> {
        let keyboard = VirtualKeyboard::new()?;
        let clipboard_rx = clipboard::spawn()?;
        let hotkey_bindings: Bindings = Arc::new(StdMutex::new(Vec::new()));
        let (hotkeys_active_tx, hotkeys_active_rx) = watch::channel(false);
        let (overlay_interactive_tx, overlay_interactive_rx) = watch::channel(false);

        let overlay = Self {
            config_store: ConfigStore::new(),
            game_config: GameConfig::new(),
            copy_item: Arc::new(TokioMutex::new(CopyItemState {
                keyboard,
                clipboard_rx,
                check_in_flight: false,
            })),
            bindings: Vec::new(),
            hotkey_bindings: hotkey_bindings.clone(),
            events_tx,
            game_active: false,
            hotkeys_active: hotkeys_active_tx,
            overlay_interactive: false,
            overlay_interactive_tx,
            interactive_since: Instant::now(),
            used_recently: true,
            area_tracker: None,
            self_ref: Weak::new(),
        };
        Ok((
            overlay,
            hotkey_bindings,
            hotkeys_active_rx,
            overlay_interactive_rx,
        ))
    }

    /// See `self_ref`'s docs — must be called once, right after `Overlay` is
    /// wrapped in its `Arc<TokioMutex<_>>`, before any WS traffic can arrive.
    pub fn attach_self_ref(&mut self, self_ref: Weak<TokioMutex<Overlay>>) {
        self.self_ref = self_ref;
    }

    /// Must be called once from `main.rs`, right after `area_tracker::watch`
    /// is spawned — see `area_tracker`'s own docs.
    pub fn attach_area_tracker(&mut self, handle: area_tracker::Handle) {
        self.area_tracker = Some(handle);
    }

    fn stop_area_tracker(&self) {
        if let Some(tx) = &self.area_tracker {
            let _ = tx.send(area_tracker::Cmd::Stop);
        }
    }

    pub async fn config_contents(&self) -> Option<String> {
        self.config_store.load().await
    }

    /// `pub(crate)`, not private — `area_tracker.rs` broadcasts
    /// `MAIN->OVERLAY::hide-exclusive-widget` directly (ports
    /// `WidgetAreaTracker.ts`'s `this.server.sendEventTo("broadcast", ...)`).
    pub(crate) fn broadcast(&self, event: Value) {
        // No receivers (nothing connected to /events yet) is a normal,
        // expected state, not an error worth logging.
        let _ = self.events_tx.send(event);
    }

    fn send_focus_change(&self, using_hotkey: bool) {
        self.broadcast(json!({
            "name": "MAIN->OVERLAY::focus-change",
            "payload": {
                "game": self.game_active,
                "overlay": self.overlay_interactive,
                "usingHotkey": using_hotkey,
            },
        }));
    }

    /// The events a freshly-connected WS client needs sent immediately —
    /// `host.mjs`'s connection handler sent these three eagerly rather than
    /// waiting for the client to ask.
    pub fn initial_events(&self) -> Vec<Value> {
        vec![
            json!({"name": "MAIN->OVERLAY::overlay-attached"}),
            json!({"name": "MAIN->OVERLAY::visibility", "payload": {"isVisible": true}}),
            json!({
                "name": "MAIN->OVERLAY::focus-change",
                "payload": {
                    "game": self.game_active,
                    "overlay": self.overlay_interactive,
                    "usingHotkey": false,
                },
            }),
        ]
    }

    /// Called by the compositor focus-watcher whenever the focused window's
    /// class changes.
    pub fn set_game_active(&mut self, class: &str) {
        let active = class == compositor::game_class();
        if active == self.game_active {
            return;
        }
        self.game_active = active;
        let _ = self.hotkeys_active.send(active);
        self.send_focus_change(false);
    }

    /// `pub(crate)`, not private — `area_tracker.rs` calls this directly,
    /// the same way it calls `assertOverlayActive`/`assertGameActive` on
    /// upstream's `OverlayWindow`.
    pub(crate) fn is_interactive(&self) -> bool {
        self.overlay_interactive
    }

    pub(crate) fn set_interactive(&mut self, interactive: bool) {
        if interactive == self.overlay_interactive {
            return;
        }
        self.overlay_interactive = interactive;
        if interactive {
            self.interactive_since = Instant::now();
        }
        set_passthrough_on_main_thread(!interactive);
        let _ = self.overlay_interactive_tx.send(interactive);
        // A layer surface isn't a window and never appears in `activewindow`
        // events, so this is the *only* way the renderer learns the overlay
        // itself became active — without it, it holds `overlay: false`
        // forever and never renders anything.
        self.send_focus_change(false);
    }

    /// Ports upstream's `OverlayWindow.ts::handleExtraCommands`'s `Escape`
    /// case (via `assertGameActive`) — called from `hotkeys.rs`'s evdev-level
    /// `Esc` handling. `Esc` isn't a user-configurable shortcut; upstream
    /// hardcodes it the same way, entirely separate from `Shortcuts.ts`'s
    /// bindings. **Unlike `toggle_overlay`, this only ever deactivates** —
    /// verified against upstream's actual source (not the original task's
    /// assumption that Escape would behave differently while a text field
    /// has focus): `before-input-event` intercepts Escape before the
    /// renderer's own DOM ever sees it, unconditionally, so upstream closes
    /// the overlay even mid-typing rather than just blurring the field.
    pub(crate) fn close_overlay(&mut self) {
        self.stop_area_tracker();
        self.set_interactive(false);
    }

    /// Dispatches a hotkey firing (the index `hotkeys::watch` sent) to the
    /// matching action. Takes the shared `Arc` rather than `&mut self` —
    /// `run_copy_item`'s slow clipboard wait must not hold `Overlay`'s own
    /// lock the whole time, so it needs to be able to drop and re-acquire
    /// around it (see `run_copy_item` docs).
    pub async fn on_hotkey_fired(overlay: &Arc<TokioMutex<Overlay>>, index: usize) {
        let action = {
            let guard = overlay.lock().await;
            guard.bindings.get(index).map(|b| b.action.clone())
        };
        let Some(action) = action else {
            return;
        };
        match action {
            Action::CopyItem {
                target,
                focus_overlay,
            } => Self::run_copy_item(overlay, &target, focus_overlay).await,
            Action::ToggleOverlay => {
                let mut guard = overlay.lock().await;
                guard.toggle_overlay();
            }
            Action::TriggerEvent { target } => {
                let guard = overlay.lock().await;
                guard.broadcast(json!({
                    "name": "MAIN->CLIENT::widget-action",
                    "payload": {"target": target},
                }));
            }
            Action::StashSearch { text } => {
                Self::run_stash_search(overlay, &text).await;
            }
        }
    }

    /// `pub(crate)`, not private — `tray.rs`'s "Toggle Overlay" menu item
    /// calls this directly, the same effect as the `toggle-overlay`
    /// hotkey action.
    pub(crate) fn toggle_overlay(&mut self) {
        // Matches `Shortcuts.ts`'s `toggle-overlay` handler: drop any
        // in-progress hover tracking before flipping state, same as it calls
        // `areaTracker.removeListeners()` right before `toggleActiveState()`.
        self.stop_area_tracker();
        let interactive = self.overlay_interactive;
        self.set_interactive(!interactive);
    }

    /// Ports `host.mjs::runPriceCheck()`: find the game window, inject the
    /// copy combo, read back whichever selection the game actually owns,
    /// broadcast the result, and switch the overlay interactive so it has
    /// something to show.
    ///
    /// Deliberately does *not* hold `Overlay`'s own lock for the slow part
    /// (the clipboard wait, up to `COPY_ITEM_TIMEOUT`) — only the narrower
    /// `copy_item` lock, which nothing else contends for. `Overlay`'s lock
    /// is only taken twice, briefly: once up front to clone the `Arc`, once
    /// at the end to broadcast and flip interactive. Without this split, a
    /// price check taking the full 2.5s timeout (item not found) would
    /// block every WS message and every focus-change update — including an
    /// Alt-Tab away from the game — for that whole window.
    ///
    /// `check_in_flight` (on `CopyItemState`, guarded by the same lock this
    /// function holds throughout) guards against a second invocation
    /// landing on top of a first still in progress and corrupting its
    /// clipboard read — kept even though this fork's evdev matching (unlike
    /// upstream's uiohook-based one) shouldn't itself produce duplicate
    /// fires, as cheap insurance either way.
    async fn run_copy_item(overlay: &Arc<TokioMutex<Overlay>>, target: &str, focus_overlay: bool) {
        let (copy_item, mods_key) = {
            let guard = overlay.lock().await;
            (guard.copy_item.clone(), guard.game_config.show_mods_key())
        };
        let mut state = copy_item.lock().await;

        if state.check_in_flight {
            eprintln!("ee2-hyprland: copy-item already in progress, ignoring");
            return;
        }
        state.check_in_flight = true;

        let clients = match compositor::clients().await {
            Ok(clients) => clients,
            Err(err) => {
                eprintln!("ee2-hyprland: copy-item: failed to query clients: {err}");
                state.check_in_flight = false;
                return;
            }
        };
        let Some(game) = compositor::find_game(&clients) else {
            eprintln!(
                "ee2-hyprland: copy-item: game window ({}) not found",
                compositor::game_class()
            );
            state.check_in_flight = false;
            return;
        };
        let xwayland = game.xwayland;

        clipboard::drain_stale(&mut state.clipboard_rx);
        if let Err(err) = state.keyboard.copy_item(mods_key).await {
            eprintln!("ee2-hyprland: copy-item: injection failed: {err}");
            state.check_in_flight = false;
            return;
        }

        let (text, position) = tokio::join!(
            clipboard::read_item_text(&mut state.clipboard_rx, xwayland, COPY_ITEM_TIMEOUT),
            compositor::cursor_pos(),
        );

        state.check_in_flight = false;
        drop(state); // release copy_item before touching Overlay's own lock again

        let Some(text) = text else {
            eprintln!("ee2-hyprland: copy-item: no item text within timeout (xwayland={xwayland})");
            return;
        };
        let (x, y) = position.unwrap_or((0, 0));
        // `compositor::cursor_pos()` is in Hyprland's global layout frame;
        // the renderer's own position math (`PriceCheckWindow.vue`) works in
        // monitor-local logical pixels (`window.screenX`/`screenY`, always
        // `(0, 0)` on Wayland — see `current_monitor_origin`'s docs), so this
        // has to be converted before it's usable there. Falls back to the
        // raw global position if the monitor origin can't be resolved (the
        // pre-existing, less-correct behavior) rather than dropping the
        // whole price check over it.
        let (ox, oy) = overlay_monitor_origin().await.unwrap_or_else(|| {
            eprintln!(
                "ee2-hyprland: copy-item: no monitor origin resolved, using unconverted position"
            );
            (0, 0)
        });
        let (x, y) = (x - ox, y - oy);
        eprintln!(
            "ee2-hyprland: copy-item: success, {} chars at ({x},{y}), broadcasting item-text",
            text.len()
        );

        let mut guard = overlay.lock().await;
        // Ports `Shortcuts.ts`'s `this.areaTracker.removeListeners()`, called
        // right before broadcasting a fresh `item-text` on every successful
        // copy — any hover tracking left over from a previous price check
        // must not keep reacting to mouse movement once a new one has
        // started.
        guard.stop_area_tracker();
        guard.broadcast(json!({
            "name": "MAIN->CLIENT::item-text",
            "payload": {
                "target": target,
                "clipboard": text,
                "position": {"x": x, "y": y},
                "focusOverlay": focus_overlay,
            },
        }));

        // Ports `Shortcuts.ts`'s `if (action.focusOverlay &&
        // this.overlay.wasUsedRecently) { this.overlay.assertOverlayActive();
        // }` — only the locked (`focusOverlay: true`, e.g. `Ctrl+Alt+D`)
        // hotkey jumps straight to interactive. A plain hover copy
        // (`focusOverlay: false`, e.g. `Ctrl+D`) stays passthrough: the
        // renderer shows the widget on its own (`render-state`) and
        // `OVERLAY->MAIN::track-area` (handled below) is what makes hovering
        // onto it — or holding the price-check modifier — actually grab
        // input.
        if focus_overlay && guard.used_recently {
            guard.set_interactive(true);
        }
    }

    /// Ports `text-box.ts::stashSearch()`: write `text` to the clipboard the
    /// game will actually read from, make sure the game rather than the
    /// overlay has keyboard focus, then inject `Ctrl+F`/`Ctrl+V`/`Enter`.
    ///
    /// The focus step matters more here than it does for `run_copy_item`:
    /// stash-search is normally triggered by clicking a button *inside* an
    /// already-open, interactive widget, so the layer-shell surface is very
    /// likely holding `KeyboardMode::EXCLUSIVE` at the moment this runs —
    /// without switching to passthrough first, the injected keys would
    /// land in the WebView instead of the game (upstream's own equivalent,
    /// `assertGameActive()`, does the same "give focus back to the game"
    /// step via `OverlayController.focusTarget()` before injecting).
    /// `set_interactive(false)`'s passthrough switch marshals onto the GTK
    /// main thread asynchronously (see `set_passthrough_on_main_thread`),
    /// so a short delay follows before injecting — not otherwise guarded
    /// against here.
    async fn run_stash_search(overlay: &Arc<TokioMutex<Overlay>>, text: &str) {
        let copy_item = {
            let mut guard = overlay.lock().await;
            guard.set_interactive(false);
            guard.copy_item.clone()
        };

        // Give the passthrough switch above time to actually land (GTK
        // main-thread marshal, not synchronous with the call above) before
        // keys get injected — same order-of-magnitude as INJECT_STAGGER
        // and upstream's own RESTORE_AFTER, not independently tuned.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let clients = match compositor::clients().await {
            Ok(clients) => clients,
            Err(err) => {
                eprintln!("ee2-hyprland: stash-search: failed to query clients: {err}");
                return;
            }
        };
        let Some(game) = compositor::find_game(&clients) else {
            eprintln!(
                "ee2-hyprland: stash-search: game window ({}) not found",
                compositor::game_class()
            );
            return;
        };
        let xwayland = game.xwayland;

        if let Err(err) = clipboard::write_text(text, xwayland).await {
            eprintln!("ee2-hyprland: stash-search: failed to write clipboard: {err}");
            return;
        }

        let mut state = copy_item.lock().await;
        if let Err(err) = state.keyboard.stash_search().await {
            eprintln!("ee2-hyprland: stash-search: injection failed: {err}");
        }
    }

    /// The big dispatcher for everything arriving over WS `/events`. Ports
    /// `host.mjs`'s `ws.on("message", ...)` switch — same event set, same
    /// scope (`used-recently`/`write-data`/`re-parse-log`/`render-state`/
    /// `track-area` are all log-only there too; Stage 5 is where any of
    /// them might get real handling).
    pub async fn handle_ws_event(&mut self, name: &str, payload: Value) {
        match name {
            "CLIENT->MAIN::save-config" => {
                let contents = payload
                    .get("contents")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_temporary = payload
                    .get("isTemporary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Err(err) = self.config_store.save(contents, is_temporary).await {
                    eprintln!("ee2-hyprland: failed to save config: {err}");
                }
                self.broadcast(json!({
                    "name": "MAIN->CLIENT::config-changed",
                    "payload": {"contents": contents},
                }));
            }
            "CLIENT->MAIN::update-host-config" => {
                let shortcuts = payload
                    .get("shortcuts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.bindings = parse_bindings(&shortcuts);
                let hotkey_list: Vec<(Shortcut, usize)> = self
                    .bindings
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (b.shortcut, i))
                    .collect();
                eprintln!(
                    "ee2-hyprland: host config accepted, {} usable hotkey(s)",
                    hotkey_list.len()
                );
                *self.hotkey_bindings.lock().unwrap() = hotkey_list;

                let game_config_path = payload
                    .get("gameConfig")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.game_config.read(game_config_path).await;
            }
            "CLIENT->MAIN::user-action" => {
                match payload.get("action").and_then(Value::as_str) {
                    Some("quit") => std::process::exit(0),
                    Some("stash-search") => {
                        let Some(text) = payload.get("text").and_then(Value::as_str) else {
                            eprintln!(
                                "ee2-hyprland: stash-search user-action missing \"text\", ignoring"
                            );
                            return;
                        };
                        let text = text.to_string();
                        // See `self_ref`'s docs: spawned so this doesn't
                        // hold the lock `server.rs` is calling us under for
                        // the whole clipboard-write-plus-injection sequence.
                        match self.self_ref.upgrade() {
                            Some(overlay) => {
                                tokio::spawn(async move {
                                    Self::run_stash_search(&overlay, &text).await;
                                });
                            }
                            None => eprintln!(
                                "ee2-hyprland: stash-search: self_ref not attached yet, ignoring"
                            ),
                        }
                    }
                    _ => {}
                }
            }
            "OVERLAY->MAIN::focus-game" => {
                // The renderer sends a burst of these right after activation
                // (see FOCUS_GAME_GRACE docs) — ignore that burst, otherwise
                // the overlay flips straight back to passthrough before the
                // user can interact with what they just triggered it for.
                let age = self.interactive_since.elapsed();
                if self.overlay_interactive && age < FOCUS_GAME_GRACE {
                    return;
                }
                self.set_interactive(false);
            }
            "OVERLAY->MAIN::render-state" => {
                // `shouldShow` = `OverlayWindow.vue`'s `isOverlayVisible` —
                // unrelated to interactive/passthrough (see FOCUS_GAME_GRACE
                // docs). Hides the layer surface entirely when nothing has
                // visible content, which also happens to mask the
                // still-open Stage 1 WebView-transparency issue while idle.
                let should_show = payload
                    .get("shouldShow")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                eprintln!("ee2-hyprland: render-state shouldShow={should_show}");
                set_visible_on_main_thread(should_show);
            }
            "CLIENT->MAIN::used-recently" => {
                // Ports `OverlayWindow.ts`'s `this.wasUsedRecently =
                // e.isOverlay` — see `run_copy_item`'s use of this field.
                self.used_recently = payload
                    .get("isOverlay")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            "OVERLAY->MAIN::track-area" => {
                let Some(hold_key) = payload.get("holdKey").and_then(Value::as_str) else {
                    eprintln!("ee2-hyprland: track-area missing \"holdKey\", ignoring");
                    return;
                };
                let close_threshold = payload
                    .get("closeThreshold")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * payload.get("dpr").and_then(Value::as_f64).unwrap_or(1.0);
                let (Some(from), Some(area)) = (payload.get("from"), payload.get("area")) else {
                    eprintln!("ee2-hyprland: track-area missing \"from\"/\"area\", ignoring");
                    return;
                };
                let f = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);

                // `from`/`area` arrive in the renderer's monitor-local frame
                // (see `overlay_monitor_origin`'s docs) — convert back to
                // Hyprland's global frame so `area_tracker.rs` can compare
                // them directly against live `compositor::cursor_pos()`
                // reads without a per-event conversion.
                let (ox, oy) = overlay_monitor_origin().await.unwrap_or_else(|| {
                    eprintln!("ee2-hyprland: track-area: no monitor origin resolved, using unconverted coordinates");
                    (0, 0)
                });
                let (ox, oy) = (ox as f64, oy as f64);

                let session = area_tracker::Session {
                    hold: Modifier::from_name(hold_key),
                    close_threshold,
                    from: (f(from, "x") + ox, f(from, "y") + oy),
                    area: area_tracker::Rect {
                        x: f(area, "x") + ox,
                        y: f(area, "y") + oy,
                        width: f(area, "width"),
                        height: f(area, "height"),
                    },
                };
                match &self.area_tracker {
                    Some(tx) => {
                        let _ = tx.send(area_tracker::Cmd::Start(session));
                    }
                    None => eprintln!(
                        "ee2-hyprland: track-area: area tracker not attached yet, ignoring"
                    ),
                }
            }
            "CLIENT->MAIN::write-data" | "CLIENT->MAIN::re-parse-log" => {
                // Logged, not acted on — same as host.mjs. Session/OCR
                // logging (`write-data`) is still Stage 5+ territory.
            }
            other => {
                eprintln!("ee2-hyprland: unhandled WS event: {other}");
            }
        }
    }
}
