//! Outbound HTTPS relay to trade-site APIs (CORS workaround) with a cookie
//! jar, serving `/proxy/<url>` requests from `server.rs`. Direct port of
//! upstream's `main/src/proxy.ts` host allow-list and header filtering, plus
//! the prior prototype's `host.mjs::handleProxy()` manual cookie jar —
//! upstream gets that for free from Electron's `net.request({
//! useSessionCookies: true })`; without an Electron session we track it
//! ourselves, same as the prior prototype had to.
//!
//! Stage 4 (see PLAN.md).

use std::collections::HashMap;
use std::sync::Mutex;

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// Hosts the renderer is allowed to reach through this relay — official
/// trade realms plus the price-data APIs upstream itself proxies to.
/// Anything else is refused outright; this exists so the WebView's
/// `fetch()` can reach these specific APIs despite CORS, not as a
/// general-purpose relay. Exact same list as upstream's `main/src/proxy.ts`.
const PROXY_HOSTS: &[&str] = &[
    "www.pathofexile.com",
    "ru.pathofexile.com",
    "pathofexile.tw",
    "poe.kakaogames.com",
    "jp.pathofexile.com",
    "de.pathofexile.com",
    "es.pathofexile.com",
    "br.pathofexile.com",
    "fr.pathofexile.com",
    "poe.ninja",
    "www.poeprices.info",
    "api.exiledexchange2.dev",
];

/// Bounds a single proxied request end to end (connect + response). Without
/// this, a trade-site host that accepts the TCP connection but never
/// responds — or responds arbitrarily slowly — leaves the request pending
/// forever with no way for the renderer to tell a hung proxy from a dead
/// one; upstream's Electron `net.request` has its own implicit timeout
/// behavior this fork doesn't otherwise inherit.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct Proxy {
    /// host -> (cookie name -> cookie value).
    jar: Mutex<HashMap<String, HashMap<String, String>>>,
    client: reqwest::Client,
}

impl Proxy {
    pub fn new() -> Self {
        Self {
            jar: Mutex::new(HashMap::new()),
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client with just a timeout should never fail to build"),
        }
    }
}

impl Default for Proxy {
    fn default() -> Self {
        Self::new()
    }
}

/// Strips headers that must not be forwarded as-is: `sec-*` (browser-only,
/// meaningless/wrong cross-origin), `host`/`origin` (would name us, not the
/// real target), `content-length` (recomputed for whatever body we actually
/// send), `connection` (hop-by-hop), and `cookie` (we send our own jar's
/// cookies instead of whatever the WebView's own cookie store had, which is
/// empty anyway since it never talks to these hosts directly).
fn request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if n.starts_with("sec-")
            || matches!(
                n,
                "host" | "origin" | "content-length" | "connection" | "cookie"
            )
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Strips headers that describe the *upstream* transfer, not the one we're
/// actually sending back (`content-encoding`/`content-length` — we hand
/// back plain decoded bytes, axum recomputes the real length), and
/// `set-cookie` — never forwarded to the renderer, it goes into our own jar
/// instead and gets replayed as a `Cookie` header on the next request.
fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if matches!(n, "content-encoding" | "content-length" | "set-cookie") {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

fn store_cookies(jar: &mut HashMap<String, String>, headers: &HeaderMap) {
    for raw in headers.get_all(header::SET_COOKIE) {
        let Ok(raw) = raw.to_str() else { continue };
        let pair = raw.split(';').next().unwrap_or("");
        if let Some(idx) = pair.find('=') {
            let (key, value) = pair.split_at(idx);
            jar.insert(key.trim().to_string(), value[1..].trim().to_string());
        }
    }
}

fn cookie_header(jar: &HashMap<String, String>) -> Option<HeaderValue> {
    if jar.is_empty() {
        return None;
    }
    let joined = jar
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    HeaderValue::from_str(&joined).ok()
}

/// Handles `/proxy/<host>/<path>`, forwarding to `https://<host>/<path>`
/// (query string included, via `OriginalUri` rather than a path extractor,
/// so it's forwarded byte-for-byte the way `host.mjs`'s
/// `req.url.slice("/proxy/".length)` did).
pub async fn handle(
    State(proxy): State<std::sync::Arc<Proxy>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(path_and_query) = uri.path_and_query().map(|pq| pq.as_str()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(target) = path_and_query.strip_prefix("/proxy/") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let host = target.split('/').next().unwrap_or("").to_string();
    if !PROXY_HOSTS.contains(&host.as_str()) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let mut out_headers = request_headers(&headers);
    out_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(concat!(
            "Exiled-Exchange-2-Hyprland/",
            env!("CARGO_PKG_VERSION")
        )),
    );
    if let Some(cookie) = proxy.jar.lock().unwrap().get(&host).and_then(cookie_header) {
        out_headers.insert(header::COOKIE, cookie);
    }

    let url = format!("https://{target}");
    let response = match proxy
        .client
        .request(method, &url)
        .headers(out_headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("ee2-hyprland: proxy request to {host} failed: {err}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    {
        let mut jar = proxy.jar.lock().unwrap();
        store_cookies(jar.entry(host.clone()).or_default(), response.headers());
    }

    let status = response.status();
    let resp_headers = response_headers(response.headers());
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(err) => {
            eprintln!("ee2-hyprland: proxy response body read failed ({host}): {err}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    (status, resp_headers, body).into_response()
}
