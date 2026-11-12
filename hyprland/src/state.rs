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
//! `item-text` to the renderer, optionally switch the overlay interactive.
//! Also stash-search / paste-in-chat (`run_stash_search`,
//! `run_paste_in_chat`), the layer surface's visibility (`push_surface`:
//! game on screen, render-state, welcome banner, `hideOverlayOnBlur`), and
//! the WS event dispatcher (`handle_ws_event`).

use std::cell::{Cell, RefCell};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use gtk4::prelude::WidgetExt;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex as TokioMutex};

use crate::area_tracker;
use crate::clipboard::{self, ClipboardEvent};
use crate::compositor;
use crate::config::{ConfigStore, GameConfig};
use crate::file_writer::FileWriter;
use crate::game_log::GameLogWatcher;
use crate::input::hotkeys::Bindings;
use crate::input::inject::VirtualKeyboard;
use crate::input::keymap::{self, Modifier, Shortcut};
use crate::logger::ee_log;
use crate::overlay;

/// How long to wait for the clipboard to show item text after injecting the
/// copy combo — matches `host.mjs::copyItemText`'s `timeoutMs`.
const COPY_ITEM_TIMEOUT: Duration = Duration::from_millis(2500);

/// How long the welcome banner needs the (otherwise hidden) surface visible.
/// Mirrors `renderer/src/web/overlay/LoadingAnimation.vue`: the widget stays
/// up for its `setTimeout(..., 2500)`, then the `animate__backOutDown` leave
/// transition plays for animate.css's default `--animate-duration` (1 s,
/// `renderer/node_modules/animate.css/animate.css`; nothing in `renderer/`
/// overrides it). Hiding the surface any earlier would clip the leave
/// animation.
const ATTACH_BANNER_SHOW: Duration = Duration::from_millis(2500);
const ATTACH_BANNER_LEAVE: Duration = Duration::from_millis(1000);
const ATTACH_BANNER_DURATION: Duration = Duration::from_millis(
    ATTACH_BANNER_SHOW.as_millis() as u64 + ATTACH_BANNER_LEAVE.as_millis() as u64,
);

thread_local! {
    static WINDOW: RefCell<Option<gtk4::ApplicationWindow>> = const { RefCell::new(None) };
    // Both GTK-main-thread-only, mirroring state that's authoritative
    // elsewhere (`Overlay::overlay_interactive`'s `bool`, and the WebView's
    // own DOM focus) — cached here purely so `apply_keyboard_mode` (below)
    // can be called synchronously from a WebKit signal handler
    // (`webview.rs`, fires on this same thread) without needing the async
    // `TokioMutex<Overlay>` lock, which a synchronous GTK/WebKit C callback
    // can't await into.
    static INTERACTIVE: Cell<bool> = const { Cell::new(false) };
    static INPUT_FOCUSED: Cell<bool> = const { Cell::new(false) };
    // Whether the pointer is currently over an overlay widget rather than
    // the transparent background (`webview.rs`'s hover bridge). Reset to
    // `false` whenever the overlay goes passthrough — no `mouseout` arrives
    // while the surface has an empty input region, so a stale `true` would
    // otherwise grab the keyboard the instant it turned interactive again.
    static POINTER_OVER_WIDGET: Cell<bool> = const { Cell::new(false) };
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
/// mouse-interactive *and* either an input element inside the WebView has
/// DOM focus (priority rule) or the pointer is over an overlay widget
/// ("keyboard follows pointer": the renderer's Shift stat-tooltip needs
/// `keydown` on `window`) — `None` otherwise (over the transparent
/// background, i.e. over the game). Tying keyboard mode 1:1 to
/// interactive/passthrough instead would make `Shift+Space` steal every
/// keystroke from the game the moment the overlay opens; this is what
/// keeps WASD working while the overlay is open and mouse-clickable.
/// Called on the GTK main thread from `apply_surface` (interactive state
/// changes) and from `set_input_focused`/`set_pointer_over_widget`
/// (`webview.rs`'s bridges).
fn apply_keyboard_mode() {
    WINDOW.with(|w| {
        if let Some(window) = w.borrow().as_ref() {
            let exclusive = INTERACTIVE.with(|i| i.get())
                && (INPUT_FOCUSED.with(|f| f.get()) || POINTER_OVER_WIDGET.with(|p| p.get()));
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
    if INPUT_FOCUSED.with(|f| f.replace(focused)) != focused {
        eprintln!("ee2-hyprland: webview input focused: {focused}");
    }
    apply_keyboard_mode();
}

/// Called from `webview.rs`'s hover-bridge script-message handler (GTK main
/// thread). Repeats are dropped so the layer surface only sees a keyboard
/// mode change on an actual transition.
pub fn set_pointer_over_widget(over: bool) {
    if POINTER_OVER_WIDGET.with(|p| p.replace(over)) != over {
        eprintln!("ee2-hyprland: webview pointer over widget: {over}");
        apply_keyboard_mode();
    }
}

/// The desired state of the layer surface, applied as a whole (idempotent)
/// on the GTK main thread — one path for every cause (interactive toggle,
/// render-state, banner, game visibility/focus, config), so none can leave
/// the surface shown with a stale input region or keyboard mode.
struct Surface {
    visible: bool,
    interactive: bool,
    /// Origin of the monitor showing the game; the surface is pinned to it.
    origin: Option<(i32, i32)>,
}

fn apply_surface(surface: Surface) {
    gtk4::glib::MainContext::default().invoke(move || {
        let was_interactive = INTERACTIVE.with(|i| i.replace(surface.interactive));
        if !surface.interactive {
            POINTER_OVER_WIDGET.with(|p| p.set(false));
        }
        WINDOW.with(|w| {
            if let Some(window) = w.borrow().as_ref() {
                if surface.visible {
                    // Shown *first*, then region/keyboard mode: both are
                    // silently ignored on a not-yet-mapped layer surface
                    // (the overlay then opens without taking input).
                    overlay::window::set_monitor_by_origin(window, surface.origin);
                    if surface.interactive && !was_interactive && window.is_mapped() {
                        // Expanding a mapped passthrough region was observed to leave pointer focus on the game.
                        overlay::window::set_visible(window, false);
                    }
                    overlay::window::set_visible(window, true);
                    overlay::window::set_input_region(window, !surface.interactive);
                } else {
                    overlay::window::set_visible(window, false);
                }
            }
        });
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

/// A parsed `ShortcutAction` (`ipc/types.ts`) this fork can actually act on.
/// Action types not listed here (`ocr-text` — heist-gem OCR, Windows-only
/// even upstream — and `test-only`) are dropped during parsing — see
/// `parse_bindings`.
#[derive(Debug, Clone)]
enum Action {
    CopyItem { target: String, focus_overlay: bool },
    ToggleOverlay,
    TriggerEvent { target: String },
    StashSearch { text: String },
    PasteInChat { text: String, send: bool },
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
                "paste-in-chat" => {
                    let Some(text) = action_val.get("text").and_then(Value::as_str) else {
                        eprintln!(
                            "ee2-hyprland: shortcut \"{shortcut_str}\": paste-in-chat action missing \"text\", skipping"
                        );
                        return None;
                    };
                    Action::PasteInChat {
                        text: text.to_string(),
                        send: action_val
                            .get("send")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
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

/// PoE2's chat-command prefixes that already clear/replace whatever's in
/// the chat box on their own — ported verbatim from
/// `text-box.ts`'s `AUTO_CLEAR`. When the pasted text starts with one of
/// these, `classify_chat_paste` skips the `Ctrl+A` select-existing step,
/// matching upstream exactly.
const CHAT_AUTO_CLEAR: [char; 6] = ['#', '%', '@', '$', '&', '/'];

/// Which of `typeInChat`'s three branches a `paste-in-chat` text falls
/// into — decides both the clipboard text actually written (the `@last`
/// placeholder itself is never pasted) and which key sequence
/// `run_paste_in_chat` uses to open the chat box.
#[derive(Debug, Clone, Copy)]
enum ChatPasteKind {
    /// `"@last ..."` — reply to whoever last whispered you (`Ctrl+Enter`).
    ReplyToLast,
    /// `"... @last"` — same, but the placeholder was at the end; needs the
    /// extra `Home, Home, Delete` upstream's own comment says is for
    /// controller-input focus.
    ReplyToLastTrailing,
    /// A plain command/message; the `bool` is whether to `Ctrl+A` first
    /// (skipped when the text already starts with a channel prefix that
    /// clears the box on its own).
    Plain(bool),
}

/// Ports `text-box.ts::typeInChat`'s branch selection and text-slicing.
/// Upstream checks `text.startsWith("@last")` (no trailing space required
/// by the check itself) then unconditionally slices off `"@last ".length`
/// (6) characters — a loose check carried over faithfully rather than
/// tightened, but done here with `str::get` instead of upstream's
/// unchecked JS `.slice()` so a malformed/multi-byte edge case can't land
/// mid-character and panic; it just yields an empty remainder instead.
fn classify_chat_paste(text: &str) -> (String, ChatPasteKind) {
    const LAST: &str = "@last";
    if text.starts_with(LAST) {
        let rest = text.get(LAST.len() + 1..).unwrap_or("").to_string();
        (rest, ChatPasteKind::ReplyToLast)
    } else if let Some(rest) = text.strip_suffix(LAST) {
        (rest.to_string(), ChatPasteKind::ReplyToLastTrailing)
    } else {
        let select_existing = !text
            .chars()
            .next()
            .is_some_and(|c| CHAT_AUTO_CLEAR.contains(&c));
        (text.to_string(), ChatPasteKind::Plain(select_existing))
    }
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
    file_writer: Arc<FileWriter>,
    game_log: GameLogWatcher,
    game_active: bool,
    /// Mirrors `game_active` out to `hotkeys.rs::watch()` — a `watch::Sender`
    /// rather than a plain `Arc<AtomicBool>` so that loop can react to a
    /// focus change the instant it happens (via `select!` on `.changed()`),
    /// not only on the next keystroke: capture needs to grab a device the
    /// moment the game gains focus, or the very first hotkey press after
    /// every alt-tab leaks its trigger key into the game. Ports upstream's
    /// `Shortcuts.ts`: `poeWindow.on("active-change", ...)` calls
    /// `globalShortcut` `register()`/`unregister()`, so hotkeys only fire
    /// (and capture only grabs) while PoE2 is the focused window — without
    /// this gate, e.g. `Shift + Space` would toggle the overlay from *any*
    /// focused window.
    hotkeys_active: watch::Sender<bool>,
    overlay_interactive: bool,
    /// Mirrors `overlay_interactive` out to `hotkeys.rs::watch()`, same
    /// `watch` shape and reason as `hotkeys_active` — for evdev-level `Esc`
    /// handling: upstream's `OverlayWindow.ts`
    /// hardcodes `Escape` (in its `before-input-event` handler, entirely
    /// separate from `Shortcuts.ts`'s user-configurable bindings) to always
    /// close an interactive overlay, so `hotkeys.rs`'s central loop needs to
    /// know "is the overlay currently interactive" independent of whether
    /// this specific keypress happens to match a configured hotkey.
    overlay_interactive_tx: watch::Sender<bool>,
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
    /// `stashScroll`: `HostConfig.stashScroll`, mirrored
    /// here the same way `used_recently` mirrors `used-recently` — plain
    /// polled state rather than a `watch` channel, since `area_tracker.rs`
    /// already has to `upgrade()`+lock `self_ref`-style to reach `Overlay`
    /// for every wheel event's `game_active()` check anyway (a compositor
    /// round-trip dwarfs one extra field read). Default `false`, matching
    /// upstream's `Shortcuts` class's own field default (`private
    /// stashScroll = false`) rather than the renderer's saved-config
    /// default (`true`) — nothing here acts on it until the real
    /// `update-host-config` arrives, same as `bindings` starting empty.
    stash_scroll_enabled: bool,
    /// `restoreClipboard` (`HostConfig`): whether `run_copy_item`/
    /// `run_stash_search`/`run_paste_in_chat` should write the user's prior
    /// clipboard content back once their own write has settled — mirrored
    /// from `update-host-config` the same way `stash_scroll_enabled` is.
    /// Default `false`, matching upstream's `HostClipboard.shouldRestore`
    /// (unset/falsy until `updateOptions` runs).
    restore_clipboard_enabled: bool,
    /// Ports `HostClipboard`'s `isRestored`-based throttle: `restoreShortly`
    /// (`run_stash_search`/`run_paste_in_chat`, both write to the game's
    /// clipboard and inject keys) refuses to start a new one while a
    /// previous one hasn't settled yet, guarding against the same "Too many
    /// actions" disconnect upstream's own comment names. One flag shared
    /// across both actions, same as upstream's single `HostClipboard`
    /// instance backs both `typeInChat` and `stashSearch`.
    chat_action_in_flight: Arc<AtomicBool>,
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
    /// `overlay-attached` already sent for the current game window — reset
    /// by `on_window_opened`, so alt-tabbing back doesn't repeat the banner.
    game_attached: bool,
    /// `overlay-attached` was due before the renderer was ready (service
    /// started with the game already running) — delivered on the first
    /// `render-state`. Not on WS connect: the Vue components haven't
    /// registered their `Host.onEvent` handlers yet at that point, so the
    /// event would be dropped.
    attach_pending: bool,
    /// The welcome banner currently holds the surface visible; `generation`
    /// distinguishes overlapping banners' timers.
    banner_active: bool,
    banner_generation: u64,
    /// Last `render-state` `shouldShow`, so the banner's end knows whether
    /// the renderer wants the surface to stay.
    render_should_show: bool,
    /// A `render-state` has been received: the renderer's Vue app is up.
    renderer_ready: bool,
    /// The game window is on screen: its workspace is the active one on its
    /// monitor (`compositor::game_origin`). A layer surface is bound to a
    /// monitor, not a workspace, so without this it follows the user to
    /// every other workspace. The surface may only be shown while true.
    game_visible: bool,
    /// Origin of the monitor showing the game (the surface is pinned there).
    game_origin: Option<(i32, i32)>,
    /// `HostConfig.hideOverlayOnBlur` (renderer default `false`).
    hide_overlay_on_blur: bool,
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
            file_writer: FileWriter::new(),
            game_log: GameLogWatcher::new(),
            game_active: false,
            hotkeys_active: hotkeys_active_tx,
            overlay_interactive: false,
            overlay_interactive_tx,
            used_recently: true,
            area_tracker: None,
            stash_scroll_enabled: false,
            restore_clipboard_enabled: false,
            chat_action_in_flight: Arc::new(AtomicBool::new(false)),
            self_ref: Weak::new(),
            game_attached: false,
            attach_pending: false,
            banner_active: false,
            banner_generation: 0,
            render_should_show: false,
            renderer_ready: false,
            game_visible: false,
            game_origin: None,
            hide_overlay_on_blur: false,
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
        eprintln!(
            "ee2-hyprland: focus-change out: game={} overlay={} usingHotkey={using_hotkey}",
            self.game_active, self.overlay_interactive
        );
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
    /// `host.mjs`'s connection handler sent these eagerly rather than
    /// waiting for the client to ask. `overlay-attached` is *not* among
    /// them (it fires when the game window attaches, see
    /// `announce_attached`).
    pub async fn initial_events(&self) -> Vec<Value> {
        let mut events = Vec::new();
        events.extend(crate::logger::history_event());
        events.push(json!({"name": "MAIN->OVERLAY::visibility", "payload": {"isVisible": true}}));
        events.push(json!({
            "name": "MAIN->OVERLAY::focus-change",
            "payload": {
                "game": self.game_active,
                "overlay": self.overlay_interactive,
                "usingHotkey": false,
            },
        }));
        events
    }

    /// A new window mapped: a fresh game window re-arms the once-per-window
    /// welcome banner (fires on its first focus, `set_game_active`).
    pub fn on_window_opened(&mut self, class: &str) {
        if compositor::is_game_class(class) {
            self.game_attached = false;
        }
    }

    /// Ports upstream's `handleOverlayAttached` (`OverlayWindow.ts`): tell
    /// the renderer the overlay is attached to the game window, once per
    /// game window.
    async fn announce_attached(&mut self) {
        self.game_attached = true;
        // No `render-state` yet means the renderer hasn't mounted.
        if !self.renderer_ready {
            self.attach_pending = true;
            return;
        }
        self.deliver_attached().await;
    }

    async fn deliver_attached(&mut self) {
        eprintln!("ee2-hyprland: overlay-attached");
        self.begin_attach_banner().await;
        self.broadcast(json!({"name": "MAIN->OVERLAY::overlay-attached"}));
    }

    /// `showAttachNotification` from the saved config (renderer default:
    /// true). The renderer checks it too before showing the banner; this
    /// side only decides whether the surface needs to be shown for it.
    async fn show_attach_notification(&self) -> bool {
        self.config_store
            .load()
            .await
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .and_then(|v| v.get("showAttachNotification")?.as_bool())
            .unwrap_or(true)
    }

    /// Shows the surface click-through for `ATTACH_BANNER_DURATION`, then
    /// hides it again unless `render-state` wants it visible by then.
    async fn begin_attach_banner(&mut self) {
        if !self.show_attach_notification().await {
            return;
        }
        self.banner_active = true;
        self.banner_generation += 1;
        let generation = self.banner_generation;
        self.push_surface();
        let weak = self.self_ref.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ATTACH_BANNER_DURATION).await;
            let Some(overlay) = weak.upgrade() else {
                return;
            };
            let mut guard = overlay.lock().await;
            if guard.banner_generation == generation {
                guard.banner_active = false;
                guard.push_surface();
            }
        });
    }

    /// Ports upstream's `syncWindowVisibility` + `showOverlay`
    /// (`OverlayWindow.ts`) with the layer-shell differences: the surface is
    /// only ever shown while the game window is on screen; interactive and
    /// the welcome banner show it; otherwise it follows the renderer's
    /// `shouldShow`, and with `hideOverlayOnBlur` additionally only while
    /// the game is the focused window.
    fn surface_should_show(&self) -> bool {
        if !self.game_visible {
            return false;
        }
        if self.overlay_interactive || self.banner_active {
            return true;
        }
        self.render_should_show && (!self.hide_overlay_on_blur || self.game_active)
    }

    fn push_surface(&self) {
        apply_surface(Surface {
            visible: self.surface_should_show(),
            interactive: self.overlay_interactive,
            origin: self.game_origin,
        });
    }

    /// Called whenever `compositor::game_origin()` is re-queried (workspace
    /// switch, game moved between workspaces/monitors, opened/closed, ...).
    /// Game not on screen → drop the surface entirely, non-interactive and
    /// click-through, with hover tracking reset (upstream's
    /// `removeListeners`). Game on screen again → shown per the current
    /// `render-state`, not interactive.
    pub fn set_game_placement(&mut self, origin: Option<(i32, i32)>) {
        if origin.is_some() == self.game_visible && origin == self.game_origin {
            return;
        }
        eprintln!("ee2-hyprland: game on screen: {origin:?}");
        self.game_visible = origin.is_some();
        self.game_origin = origin;
        if !self.game_visible {
            self.stop_area_tracker();
            self.set_interactive(false, "game left the screen");
        }
        self.push_surface();
    }

    /// Called by the compositor focus-watcher whenever the focused window's
    /// class changes.
    ///
    /// The layer surface isn't a window: the game *stays* the active window
    /// while the user works with an interactive overlay. So the game losing
    /// focus means the user went to another window — the overlay drops to
    /// passthrough (no input region, no keyboard) so input reaches that
    /// window. The game regaining focus also resets it
    /// (`handlePoeWindowActiveChange`).
    ///
    /// An *empty* class (no window focused) while the overlay is interactive
    /// is not "another window": it is Hyprland reporting our own layer
    /// surface giving up `Exclusive` keyboard focus (pointer moved to the
    /// transparent background → `KeyboardMode::None`), immediately followed
    /// by the game being refocused. Treating it as a focus loss dropped a
    /// pinned (clicked) widget the moment the cursor left it.
    pub async fn set_game_active(&mut self, class: &str) {
        if class.is_empty() && self.game_active && self.overlay_interactive {
            ee_log!("game focus: ignoring empty class (overlay released keyboard)");
            return;
        }
        let active = compositor::is_game_class(class);
        if active == self.game_active {
            return;
        }
        eprintln!("ee2-hyprland: game focus: {active} (class {class:?})");
        self.game_active = active;
        // First: releases the evdev grab (hotkeys.rs) before anything else
        // reacts to the focus change.
        let _ = self.hotkeys_active.send(active);
        if !active {
            self.stop_area_tracker();
        }
        if self.overlay_interactive {
            // Broadcasts focus-change itself, with the new `game`/`overlay`.
            self.set_interactive(false, "game focus changed");
        } else {
            self.send_focus_change(false);
        }
        self.push_surface();
        if active && !self.game_attached {
            self.announce_attached().await;
        }
    }

    /// `pub(crate)`, not private — `area_tracker.rs` calls this directly,
    /// the same way it calls `assertOverlayActive`/`assertGameActive` on
    /// upstream's `OverlayWindow`.
    pub(crate) fn is_interactive(&self) -> bool {
        self.overlay_interactive
    }

    /// `pub(crate)`, not private — `area_tracker.rs`'s `stashScroll` wheel
    /// handling checks both this and `stash_scroll_enabled` before treating
    /// a Ctrl+wheel as anything, same as `Shortcuts.ts`'s own `!e.ctrlKey ||
    /// !this.poeWindow.isActive || !this.stashScroll` early-return.
    pub(crate) fn game_active(&self) -> bool {
        self.game_active
    }

    pub(crate) fn stash_scroll_enabled(&self) -> bool {
        self.stash_scroll_enabled
    }

    pub(crate) fn set_interactive(&mut self, interactive: bool, reason: &str) {
        if interactive == self.overlay_interactive {
            return;
        }
        ee_log!("set_interactive({interactive}): {reason}");
        if interactive && !self.game_visible {
            ee_log!("game not on screen, not activating the overlay");
            return;
        }
        self.overlay_interactive = interactive;
        self.push_surface();
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
    /// bindings. **Unlike `toggle_overlay`, this only ever deactivates.**
    /// Upstream's `before-input-event` intercepts Escape before the
    /// renderer's own DOM ever sees it, unconditionally, so upstream closes
    /// the overlay even while a text field has focus rather than just
    /// blurring the field — this matches that.
    pub(crate) fn close_overlay(&mut self) {
        self.stop_area_tracker();
        self.set_interactive(false, "Esc");
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
            Action::PasteInChat { text, send } => {
                Self::run_paste_in_chat(overlay, &text, send).await;
            }
        }
    }

    /// The `toggle-overlay` hotkey action.
    pub(crate) fn toggle_overlay(&mut self) {
        // Matches `Shortcuts.ts`'s `toggle-overlay` handler: drop any
        // in-progress hover tracking before flipping state, same as it calls
        // `areaTracker.removeListeners()` right before `toggleActiveState()`.
        self.stop_area_tracker();
        let interactive = self.overlay_interactive;
        self.set_interactive(!interactive, "toggle-overlay");
    }

    /// Ports `host.mjs::runPriceCheck()`: find the game window, inject the
    /// copy combo, read back whichever selection the game actually owns,
    /// broadcast the result, and — for a `focusOverlay` hotkey only — switch
    /// the overlay interactive.
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
        let (copy_item, mods_key, restore_enabled) = {
            let guard = overlay.lock().await;
            (
                guard.copy_item.clone(),
                guard.game_config.show_mods_key(),
                guard.restore_clipboard_enabled,
            )
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
                ee_log!("copy-item: failed to query clients: {err}");
                state.check_in_flight = false;
                return;
            }
        };
        let Some(game) = compositor::find_game(&clients) else {
            eprintln!(
                "ee2-hyprland: copy-item: game window ({}) not found",
                compositor::game_class_label()
            );
            state.check_in_flight = false;
            return;
        };
        let xwayland = game.xwayland;

        // `readItemText`'s `textBefore` capture (`HostClipboard.ts`) —
        // grabbed before we clobber the clipboard with the copy-item
        // trigger, restored below regardless of whether item text was
        // actually found, matching upstream's own both-paths behavior.
        // Unlike upstream, this doesn't need the KDE/Proton-10
        // "force-empty-first" workaround: that exists to protect a
        // *polling* read from mistaking a stale clipboard value for a
        // fresh copy, and this fork's native-Wayland read is event-driven
        // (a genuinely fresh `Selection` event per copy, see
        // `clipboard.rs`'s module docs) — nothing here can confuse "old
        // value" with "new value" the way polling could.
        let previous = if restore_enabled {
            clipboard::current_text(xwayland).await
        } else {
            None
        };

        clipboard::drain_stale(&mut state.clipboard_rx);
        if let Err(err) = state.keyboard.copy_item(mods_key).await {
            ee_log!("copy-item: injection failed: {err}");
            state.check_in_flight = false;
            return;
        }

        let (text, position) = tokio::join!(
            clipboard::read_item_text(&mut state.clipboard_rx, xwayland, COPY_ITEM_TIMEOUT),
            compositor::cursor_pos(),
        );

        state.check_in_flight = false;
        drop(state); // release copy_item before touching Overlay's own lock again

        if let Some(previous) = previous {
            if let Err(err) = clipboard::write_text(&previous, xwayland).await {
                ee_log!("copy-item: failed to restore clipboard: {err}");
            }
        }

        let Some(text) = text else {
            ee_log!("copy-item: no item text within timeout (xwayland={xwayland})");
            return;
        };
        let (x, y) = position.unwrap_or((0, 0));
        // `compositor::cursor_pos()` is in Hyprland's global layout frame;
        // the renderer's own position math (`PriceCheckWindow.vue`) works in
        // monitor-local logical pixels (`window.screenX`/`screenY`, always
        // `(0, 0)` on Wayland — see `current_monitor_origin`'s docs), so this
        // has to be converted before it's usable there. Falls back to the
        // raw global position if the monitor origin can't be resolved
        // rather than dropping the whole price check over it.
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
            guard.set_interactive(true, "focusOverlay hotkey");
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
    /// main thread asynchronously (see `apply_surface`), so a short delay
    /// follows before injecting — not otherwise guarded against here.
    async fn run_stash_search(overlay: &Arc<TokioMutex<Overlay>>, text: &str) {
        let (copy_item, restore_enabled, in_flight) = {
            let mut guard = overlay.lock().await;
            guard.set_interactive(false, "stash-search");
            (
                guard.copy_item.clone(),
                guard.restore_clipboard_enabled,
                guard.chat_action_in_flight.clone(),
            )
        };

        if !Self::try_begin_chat_action(&in_flight) {
            eprintln!("ee2-hyprland: stash-search: previous chat action still settling, ignoring");
            return;
        }

        // Give the passthrough switch above time to actually land (GTK
        // main-thread marshal, not synchronous with the call above) before
        // keys get injected — same order-of-magnitude as INJECT_STAGGER
        // and upstream's own RESTORE_AFTER, not independently tuned.
        //
        // **Why not a real event:** the obvious candidate,
        // `gtk4::Window::is-active`/`connect_is_active_notify` (which for a
        // *toplevel* does track wl_keyboard enter/leave), never fires on
        // this layer-shell window — measured through repeated
        // interactive/passthrough transitions. `gtk_window_is_active` is
        // wired to `xdg_toplevel`'s ACTIVATED state, which a
        // `zwlr_layer_surface_v1` role doesn't have. The only signal that
        // genuinely reflects our surface's keyboard focus is the seat's raw
        // `wl_keyboard.enter`/`leave` (matching our own `wl_surface` id
        // against the event's `surface` argument) — a new raw Wayland
        // listener, not a swap-in for one call site. Hence the fixed wait.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let settled = async {
            let clients = match compositor::clients().await {
                Ok(clients) => clients,
                Err(err) => {
                    ee_log!("stash-search: failed to query clients: {err}");
                    return None;
                }
            };
            let Some(game) = compositor::find_game(&clients) else {
                eprintln!(
                    "ee2-hyprland: stash-search: game window ({}) not found",
                    compositor::game_class_label()
                );
                return None;
            };
            let xwayland = game.xwayland;

            let previous = if restore_enabled {
                clipboard::current_text(xwayland).await
            } else {
                None
            };
            let settled_rx = match clipboard::write_text_restoring(text, xwayland, previous).await {
                Ok(rx) => rx,
                Err(err) => {
                    ee_log!("stash-search: failed to write clipboard: {err}");
                    return None;
                }
            };

            let mut state = copy_item.lock().await;
            if let Err(err) = state.keyboard.stash_search().await {
                ee_log!("stash-search: injection failed: {err}");
            }
            drop(state);
            Some(settled_rx)
        }
        .await;

        Self::release_chat_action_after(in_flight, settled);
    }

    /// Ports `text-box.ts::typeInChat()`: writes the (placeholder-stripped)
    /// text to the clipboard the game will paste from, opens the chat box
    /// with the right key sequence for which `@last` form (if any) was
    /// used (`classify_chat_paste`), pastes, and optionally sends. Shares
    /// `chat_action_in_flight`/`restoreClipboard` handling with
    /// `run_stash_search` — see that function's docs, and
    /// `clipboard::write_text_restoring`'s (`RESTORE_AFTER`'s docs
    /// specifically) for why the restore/throttle-release mechanism both
    /// funnel through is a fixed wait rather than event-driven.
    ///
    /// Unlike `run_stash_search`, this does **not** call
    /// `set_interactive(false)` first — upstream's `typeInChat` has no
    /// equivalent `assertGameActive()` call either, since paste-in-chat is
    /// only ever reachable via a hotkey (fired while the game already has
    /// focus, `hotkeys_active` gates that), never a click on a button
    /// inside the overlay the way stash-search's WS path is.
    async fn run_paste_in_chat(overlay: &Arc<TokioMutex<Overlay>>, text: &str, send: bool) {
        let (copy_item, restore_enabled, in_flight) = {
            let guard = overlay.lock().await;
            (
                guard.copy_item.clone(),
                guard.restore_clipboard_enabled,
                guard.chat_action_in_flight.clone(),
            )
        };

        if !Self::try_begin_chat_action(&in_flight) {
            eprintln!("ee2-hyprland: paste-in-chat: previous chat action still settling, ignoring");
            return;
        }

        let settled = async {
            let clients = match compositor::clients().await {
                Ok(clients) => clients,
                Err(err) => {
                    ee_log!("paste-in-chat: failed to query clients: {err}");
                    return None;
                }
            };
            let Some(game) = compositor::find_game(&clients) else {
                eprintln!(
                    "ee2-hyprland: paste-in-chat: game window ({}) not found",
                    compositor::game_class_label()
                );
                return None;
            };
            let xwayland = game.xwayland;

            let (clip_text, kind) = classify_chat_paste(text);

            let previous = if restore_enabled {
                clipboard::current_text(xwayland).await
            } else {
                None
            };
            let settled_rx =
                match clipboard::write_text_restoring(&clip_text, xwayland, previous).await {
                    Ok(rx) => rx,
                    Err(err) => {
                        ee_log!("paste-in-chat: failed to write clipboard: {err}");
                        return None;
                    }
                };

            let mut state = copy_item.lock().await;
            let inject_result: io::Result<()> = async {
                match kind {
                    ChatPasteKind::ReplyToLast => state.keyboard.open_chat_reply_to_last().await?,
                    ChatPasteKind::ReplyToLastTrailing => {
                        state.keyboard.open_chat_reply_to_last().await?;
                        state.keyboard.chat_move_home_twice_and_delete().await?;
                    }
                    ChatPasteKind::Plain(select_existing) => {
                        state.keyboard.open_chat(select_existing).await?;
                    }
                }
                state.keyboard.paste_clipboard().await?;
                if send {
                    state.keyboard.send_chat_message().await?;
                    // Deliberately NOT porting upstream's restore-last-chat
                    // tail here (`Enter, Enter, ArrowUp, ArrowUp, Escape`,
                    // `text-box.ts`) — the KDE fork found this leaves
                    // PoE2's own chat box open on Proton/Wayland instead of
                    // closing it, causing a second send and opening the
                    // game's own menu. Plain `send: true` commands work
                    // without it; whether an `@last` reply needs any tail
                    // has not been checked against the real game (needs an
                    // incoming whisper) — do that before adding one back.
                }
                Ok(())
            }
            .await;
            drop(state);

            if let Err(err) = inject_result {
                ee_log!("paste-in-chat: injection failed: {err}");
            }
            Some(settled_rx)
        }
        .await;

        Self::release_chat_action_after(in_flight, settled);
    }

    /// Ports `HostClipboard.isRestored`'s throttle check: `false` (already
    /// in flight) means the caller should drop this call entirely, same as
    /// upstream's `restoreShortly` early-return — "not only do we not
    /// overwrite clipboard, but we don't exec callback either".
    fn try_begin_chat_action(flag: &Arc<AtomicBool>) -> bool {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Clears `chat_action_in_flight` once `settled` resolves (or
    /// immediately, if the action never got far enough to produce a settle
    /// handle at all) — spawned so `run_stash_search`/`run_paste_in_chat`
    /// don't have to block their own caller on this.
    fn release_chat_action_after(
        in_flight: Arc<AtomicBool>,
        settled: Option<oneshot::Receiver<()>>,
    ) {
        tokio::spawn(async move {
            if let Some(rx) = settled {
                let _ = rx.await;
            }
            in_flight.store(false, Ordering::SeqCst);
        });
    }

    /// The dispatcher for everything arriving over WS `/events` (the
    /// `CLIENT->MAIN::*`/`OVERLAY->MAIN::*` events of `ipc/types.ts`).
    /// Anything unknown is logged and ignored.
    pub async fn handle_ws_event(&mut self, name: &str, payload: Value) {
        eprintln!("ee2-hyprland: ws in: {name}");
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
                    ee_log!("failed to save config: {err}");
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

                self.stash_scroll_enabled = payload
                    .get("stashScroll")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                self.restore_clipboard_enabled = payload
                    .get("restoreClipboard")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                self.hide_overlay_on_blur = payload
                    .get("hideOverlayOnBlur")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.push_surface();

                self.game_log.restart(
                    payload
                        .get("clientLog")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    payload
                        .get("readClientLog")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    &self.events_tx,
                    &self.file_writer,
                );
                self.file_writer.restart(
                    payload
                        .get("libraryAlpha")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    payload.get("libraryOutputPath").and_then(Value::as_str),
                );
            }
            "CLIENT->MAIN::user-action" => {
                match payload.get("action").and_then(Value::as_str) {
                    Some("quit") => std::process::exit(0),
                    Some("check-for-update") => {
                        // No self-update mechanism in this fork (packaged
                        // and updated like any other native Linux
                        // package, not an installer with its own
                        // updater) — respond the
                        // same shape `/config`'s own initial `updater`
                        // value already uses (`server.rs::get_config`), so
                        // Settings' About screen resolves the "Checking…"
                        // state it optimistically shows on click into a
                        // definitive one instead of hanging there forever.
                        let checked_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        self.broadcast(json!({
                            "name": "MAIN->CLIENT::updater-state",
                            "payload": {"state": "update-not-available", "checkedAt": checked_at},
                        }));
                    }
                    Some("update-and-restart") => {
                        // Unreachable in practice: the About screen only
                        // shows the button that sends this when a prior
                        // `updater-state` reported `update-downloaded`,
                        // which this fork never sends (see
                        // `check-for-update` above) — logged rather than
                        // silently ignored in case that assumption ever
                        // breaks.
                        eprintln!(
                            "ee2-hyprland: update-and-restart requested but this fork has no self-update mechanism, ignoring"
                        );
                    }
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
                // Sent by `OverlayWindow.vue`'s `handleBackgroundClick`/
                // `PriceCheckWindow.vue`'s `closePriceCheck` — explicit
                // "user wants focus back on the game" actions (background
                // click, closing a widget). Acted on unconditionally, like
                // upstream's `assertGameActive()`: the spurious
                // post-activation `focus-game` burst the prior prototype
                // needed a grace window for does not occur here (measured
                // across `toggle-overlay`, `Ctrl+Alt+D` and `Ctrl+D`+hover
                // activations).
                self.set_interactive(false, "OVERLAY->MAIN::focus-game");
            }
            "OVERLAY->MAIN::render-state" => {
                // `shouldShow` = `OverlayWindow.vue`'s `isOverlayVisible` —
                // a different signal from `focus-game` above (see that
                // arm's docs): "does any widget currently have visible
                // content", not "user wants focus back". Hides the layer
                // surface entirely when nothing has visible content.
                let should_show = payload
                    .get("shouldShow")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                eprintln!("ee2-hyprland: render-state shouldShow={should_show}");
                self.render_should_show = should_show;
                // The banner's own hold on the surface isn't part of
                // `shouldShow` (`surface_should_show` keeps it up).
                self.renderer_ready = true;
                self.push_surface();
                if std::mem::take(&mut self.attach_pending) {
                    self.deliver_attached().await;
                }
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
            "CLIENT->MAIN::write-data" => self.file_writer.handle(payload),
            "CLIENT->MAIN::re-parse-log" => self.game_log.reparse(),
            other => {
                eprintln!("ee2-hyprland: unhandled WS event: {other}");
            }
        }
    }
}

/// `stashScroll`: ports `Shortcuts.ts`'s Ctrl+wheel
/// tab-switch injection. `area_tracker.rs` is the one caller — it already
/// holds only a `Weak<TokioMutex<Overlay>>` (never the strong `Arc`), same
/// as its existing `set_overlay_interactive`/`broadcast_hide_exclusive_widget`
/// helpers, so this takes the same shape rather than being an `Overlay`
/// method. Locks `Overlay` just long enough to clone the `copy_item` handle
/// — same split-lock reasoning as `run_copy_item`/`run_stash_search`: the
/// injection itself (two `INJECT_STAGGER`-spaced key taps) must not block
/// WS message handling or focus-change updates for its duration.
pub(crate) async fn inject_stash_scroll_tab(overlay: &Weak<TokioMutex<Overlay>>, right: bool) {
    let Some(overlay) = overlay.upgrade() else {
        return;
    };
    let copy_item = overlay.lock().await.copy_item.clone();
    let mut state = copy_item.lock().await;
    if let Err(err) = state.keyboard.stash_scroll_tab(right).await {
        ee_log!("stash-scroll: injection failed: {err}");
    }
}
