//! HTTP + WebSocket host server implementing the `ipc/types.ts` protocol —
//! the seam this whole fork is built on. Same routes as upstream's Electron
//! host (`/`, `/config`, `/proxy/<url>`, WS `/events`), same IPC event names,
//! ported from the prior prototype's `host.mjs` and cross-checked against
//! upstream's own `main/src/server.ts`. Static files come from the embedded
//! `renderer/dist` (via `rust-embed`) instead of a filesystem read.
//!
//! One deliberate scope reduction from upstream's real `server.ts`: no
//! `lastActiveClient`/multi-window tracking. Upstream supports multiple
//! simultaneously-connected clients (a settings window, a library window,
//! ...) and routes some events to "whichever was last active". This fork
//! has exactly one WebView, so `sendEventTo("broadcast" | "last-active" |
//! "any", ...)` all collapse to the same thing: broadcast to every current
//! `/events` connection (matching `host.mjs`'s own simpler `broadcast()`).
//!
//! Stage 1 (static serving) / Stage 4 (`/config`, `/proxy/<url>`, WS
//! `/events`) — see PLAN.md.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRef, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use rust_embed::Embed;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use crate::proxy::Proxy;
use crate::state::Overlay;

#[derive(Embed)]
#[folder = "../renderer/dist"]
struct RendererAssets;

#[derive(Clone)]
pub struct AppState {
    pub overlay: Arc<Mutex<Overlay>>,
    pub proxy: Arc<Proxy>,
    pub events_tx: broadcast::Sender<Value>,
}

impl FromRef<AppState> for Arc<Proxy> {
    fn from_ref(state: &AppState) -> Self {
        state.proxy.clone()
    }
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match RendererAssets::get(path) {
        Some(file) => (
            [(header::CONTENT_TYPE, file.metadata.mimetype().to_owned())],
            file.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// `HostState` (`ipc/types.ts`): `contents` is the saved config (`None` on
/// first run, same as upstream), `version` and `updater` are fixed —
/// there's no auto-updater in this fork, so `updater` is always
/// `update-not-available`, matching `host.mjs`'s own stand-in (upstream's
/// real value comes from Electron's `AppUpdater`, N/A here).
async fn get_config(State(state): State<AppState>) -> Response {
    let contents = state.overlay.lock().await.config_contents().await;
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "updater": {"state": "update-not-available", "checkedAt": checked_at},
        "contents": contents,
    }))
    .into_response()
}

async fn events_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = state.events_tx.subscribe();

    let initial = state.overlay.lock().await.initial_events().await;
    for event in initial {
        if sink
            .send(Message::Text(event.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if sink
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // A slow client that falls behind the broadcast buffer just
                // misses the events in between — reconnecting/resyncing via
                // `/config` is the renderer's own job, same as a dropped
                // network connection would require anyway.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let overlay = state.overlay.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payload = value.get("payload").cloned().unwrap_or(Value::Null);
            overlay.lock().await.handle_ws_event(&name, payload).await;
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/config", get(get_config))
        .route("/proxy/{*rest}", any(crate::proxy::handle))
        .route("/events", get(events_handler))
        .fallback(get(asset))
        .with_state(state)
}

/// Binds an OS-assigned localhost port and serves the full IPC protocol on
/// it in the background. Returns the bound address once the listener is
/// ready — the caller points the WebView at `http://{addr}/`.
pub async fn spawn_server(state: AppState) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router(state)).await {
            eprintln!("ee2-hyprland: server exited: {err}");
        }
    });
    Ok(addr)
}
