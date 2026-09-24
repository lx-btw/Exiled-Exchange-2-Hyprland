//! Overlay mode (passthrough/interactive) and game-focus tracking — the
//! state `server.rs` and `overlay/window.rs` both need to agree on. Ported
//! from upstream's `main/src/windowing/OverlayWindow.ts` state machine and
//! the prior prototype's `host.mjs` (`overlayInteractive`, `tellOverlay()`,
//! `onActiveWindow()`), adapted to a single-process design: `host.mjs` sent
//! `tellOverlay()` commands over a unix socket to a *separate* overlay
//! process; here the overlay lives in the same process, so this just calls
//! `overlay::window::set_passthrough()` directly (marshaled onto the GTK
//! main thread — see `register_window`/`set_interactive`).
//!
//! Owns the whole price-check flow end to end (`run_copy_item`): find the
//! game window, inject `Ctrl+Alt+C`, read the clipboard back, broadcast
//! `item-text` to the renderer, switch the overlay interactive. This is
//! "port `host.mjs`'s `runPriceCheck()`", not new design.
//!
//! Stage 4 (see PLAN.md).

use std::cell::RefCell;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};

use crate::clipboard::{self, ClipboardEvent};
use crate::compositor;
use crate::config::{ConfigStore, GameConfig};
use crate::input::hotkeys::Bindings;
use crate::input::inject::VirtualKeyboard;
use crate::input::keymap::{self, Shortcut};
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
}

/// Must be called once, on the GTK main thread, right after the overlay
/// window is created — registers it so `set_interactive` (callable from any
/// thread) has something to marshal `overlay::window::set_passthrough`
/// calls onto. Without this, there is no safe way to touch the window from
/// the tokio tasks that receive hotkey/WS/compositor events, since GTK
/// objects aren't `Send`.
pub fn register_window(window: gtk4::ApplicationWindow) {
    WINDOW.with(|w| *w.borrow_mut() = Some(window));
}

fn set_passthrough_on_main_thread(passthrough: bool) {
    gtk4::glib::MainContext::default().invoke(move || {
        WINDOW.with(|w| {
            if let Some(window) = w.borrow().as_ref() {
                overlay::window::set_passthrough(window, passthrough);
            }
        });
    });
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
    overlay_interactive: bool,
    interactive_since: Instant,
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
    /// `Bindings` handle `hotkeys::watch()` needs — created here (empty)
    /// rather than in `main.rs`, since `update_host_config` is what
    /// actually populates it and that only makes sense as an `Overlay`
    /// method.
    pub fn new(events_tx: broadcast::Sender<Value>) -> std::io::Result<(Self, Bindings)> {
        let keyboard = VirtualKeyboard::new()?;
        let clipboard_rx = clipboard::spawn()?;
        let hotkey_bindings: Bindings = Arc::new(StdMutex::new(Vec::new()));

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
            overlay_interactive: false,
            interactive_since: Instant::now(),
            self_ref: Weak::new(),
        };
        Ok((overlay, hotkey_bindings))
    }

    /// See `self_ref`'s docs — must be called once, right after `Overlay` is
    /// wrapped in its `Arc<TokioMutex<_>>`, before any WS traffic can arrive.
    pub fn attach_self_ref(&mut self, self_ref: Weak<TokioMutex<Overlay>>) {
        self.self_ref = self_ref;
    }

    pub async fn config_contents(&self) -> Option<String> {
        self.config_store.load().await
    }

    fn broadcast(&self, event: Value) {
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
        self.send_focus_change(false);
    }

    fn set_interactive(&mut self, interactive: bool) {
        if interactive == self.overlay_interactive {
            return;
        }
        self.overlay_interactive = interactive;
        if interactive {
            self.interactive_since = Instant::now();
        }
        set_passthrough_on_main_thread(!interactive);
        // A layer surface isn't a window and never appears in `activewindow`
        // events, so this is the *only* way the renderer learns the overlay
        // itself became active — without it, it holds `overlay: false`
        // forever and never renders anything.
        self.send_focus_change(false);
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

        let mut guard = overlay.lock().await;
        guard.broadcast(json!({
            "name": "MAIN->CLIENT::item-text",
            "payload": {
                "target": target,
                "clipboard": text,
                "position": {"x": x, "y": y},
                "focusOverlay": focus_overlay,
            },
        }));

        // There's now something for the overlay to show — same as
        // `host.mjs`, always switch interactive on a successful copy
        // regardless of `focusOverlay` (upstream's own Electron path is
        // pickier about this via `wasUsedRecently`; the prior prototype's
        // simpler unconditional switch is the proven behavior on this
        // platform layer specifically).
        guard.set_interactive(true);
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
                set_visible_on_main_thread(should_show);
            }
            "CLIENT->MAIN::used-recently"
            | "CLIENT->MAIN::write-data"
            | "CLIENT->MAIN::re-parse-log"
            | "OVERLAY->MAIN::track-area" => {
                // Logged, not acted on — same as host.mjs. Stash-search's
                // hover-based close (`track-area`) and session/OCR logging
                // (`write-data`) are still Stage 5 territory.
            }
            other => {
                eprintln!("ee2-hyprland: unhandled WS event: {other}");
            }
        }
    }
}
