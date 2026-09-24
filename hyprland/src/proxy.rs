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

/// True for any host this relay will actually reach out to. Split out from
/// the inline `PROXY_HOSTS.contains` check so test builds can allow the
/// loopback address a fake upstream test server binds to, without that
/// address ever being reachable in a production build (`cfg(test)` compiles
/// the extra arm out entirely).
fn host_allowed(host: &str) -> bool {
    if PROXY_HOSTS.contains(&host) {
        return true;
    }
    #[cfg(test)]
    if host.starts_with("127.0.0.1:") {
        return true;
    }
    false
}

/// The scheme to use for a given (already-allowlisted) host. Always
/// `https` in production — every entry in `PROXY_HOSTS` is a real TLS
/// endpoint. The one exception is test builds hitting the loopback fake
/// upstream `host_allowed` carves out above, which speaks plain HTTP; this
/// is the single substitution point the tests need and it has no effect on
/// the production path (production never reaches a `127.0.0.1:` host in
/// the first place, since `host_allowed` above refuses it).
fn scheme_for(host: &str) -> &'static str {
    #[cfg(test)]
    if host.starts_with("127.0.0.1:") {
        return "http";
    }
    let _ = host;
    "https"
}

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

/// RFC 9110 §7.6.1's fixed hop-by-hop header names: meaningful only for a
/// single transport hop, never valid to forward through a relay. Upstream's
/// own `proxy.ts` never had to think about this — a single Electron
/// `net.request` call is one hop, so Node/Chromium's network stack already
/// strips these before `HttpProxy` ever sees them. Ours is a real two-hop
/// chain (WebView -> us -> trade host) on both legs, so both `request_headers`
/// and `response_headers` need to strip them explicitly.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// The fixed hop-by-hop set above, plus whatever a `Connection` header's
/// value itself names (RFC 9110 §7.6.1: `Connection` lists additional
/// header names that are hop-by-hop for this specific exchange only).
fn hop_by_hop_headers(headers: &HeaderMap) -> std::collections::HashSet<String> {
    let mut named: std::collections::HashSet<String> =
        HOP_BY_HOP.iter().map(|s| s.to_string()).collect();
    for value in headers.get_all(header::CONNECTION) {
        let Ok(value) = value.to_str() else { continue };
        named.extend(
            value
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty()),
        );
    }
    named
}

/// Strips headers that must not be forwarded as-is: hop-by-hop (see
/// `hop_by_hop_headers`), `sec-*` (browser-only, meaningless/wrong
/// cross-origin), `host`/`origin` (would name us, not the real target),
/// `content-length` (recomputed for whatever body we actually send),
/// `cookie` (we send our own jar's cookies instead of whatever the
/// WebView's own cookie store had, which is empty anyway since it never
/// talks to these hosts directly), and `accept-encoding` — reqwest's own
/// gzip/brotli/zstd/deflate features set this themselves and auto-decompress
/// the response, but *only* when the request doesn't already carry an
/// `Accept-Encoding` set by the caller (reqwest docs, `ClientBuilder::gzip`
/// et al.); forwarding WebKit's own value would silently disable that and
/// hand the renderer raw compressed bytes as if they were JSON.
fn request_headers(headers: &HeaderMap) -> HeaderMap {
    let hop_by_hop = hop_by_hop_headers(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if n.starts_with("sec-")
            || hop_by_hop.contains(n)
            || matches!(
                n,
                "host" | "origin" | "content-length" | "cookie" | "accept-encoding"
            )
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Strips headers that must not be forwarded as-is: hop-by-hop (see
/// `hop_by_hop_headers` — this is what previously let a chunked
/// `Transfer-Encoding` through on top of a body we'd already fully
/// buffered via `.bytes()`, producing a framing mismatch WebKit reported as
/// a bare "Load failed" with no diagnostic on our side), `content-encoding`/
/// `content-length` (describe the *upstream* transfer — reqwest already
/// decompresses and strips these itself when its own decompression
/// features handled the body, but we still hand back plain bytes either
/// way and axum recomputes the real length), and `set-cookie` — never
/// forwarded to the renderer, it goes into our own jar instead and gets
/// replayed as a `Cookie` header on the next request.
fn response_headers(headers: &HeaderMap) -> HeaderMap {
    let hop_by_hop = hop_by_hop_headers(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if hop_by_hop.contains(n)
            || matches!(n, "content-encoding" | "content-length" | "set-cookie")
        {
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
    if !host_allowed(&host) {
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

    let url = format!("{}://{target}", scheme_for(&host));
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn hop_by_hop_headers_covers_the_fixed_set_and_connection_named_ones() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("x-custom"));
        headers.insert("x-custom", HeaderValue::from_static("drop me"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("x-rate-limit-ip", HeaderValue::from_static("1:5:60"));

        let named = hop_by_hop_headers(&headers);
        assert!(named.contains("connection"));
        assert!(named.contains("keep-alive"));
        assert!(named.contains("transfer-encoding"));
        assert!(named.contains("x-custom"));
        assert!(!named.contains("x-rate-limit-ip"));
    }

    #[test]
    fn request_headers_strips_hop_by_hop_and_accept_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        headers.insert(header::COOKIE, HeaderValue::from_static("a=b"));
        headers.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
        headers.insert("x-psapi-user-agent", HeaderValue::from_static("poe2"));

        let out = request_headers(&headers);
        assert!(out.get(header::CONNECTION).is_none());
        assert!(out.get(header::ACCEPT_ENCODING).is_none());
        assert!(out.get(header::COOKIE).is_none());
        assert!(out.get("sec-fetch-mode").is_none());
        assert_eq!(out.get("x-psapi-user-agent").unwrap(), "poe2");
    }

    #[test]
    fn response_headers_strips_hop_by_hop_and_transfer_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(header::SET_COOKIE, HeaderValue::from_static("a=b"));
        headers.insert("x-rate-limit-ip", HeaderValue::from_static("1:5:60"));

        let out = response_headers(&headers);
        assert!(out.get(header::CONNECTION).is_none());
        assert!(out.get(header::TRANSFER_ENCODING).is_none());
        assert!(out.get(header::CONTENT_ENCODING).is_none());
        assert!(out.get(header::SET_COOKIE).is_none());
        assert_eq!(out.get("x-rate-limit-ip").unwrap(), "1:5:60");
    }

    /// gzip of `{"leagues":[{"id":"Standard"},{"id":"Hardcore"}]}`, produced
    /// with `gzip -9 -n` (the `-n` drops the embedded mtime/name so this
    /// stays a fixed byte sequence rather than depending on when it was
    /// generated).
    const GZIPPED_LEAGUES_JSON: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xab, 0x56, 0xca, 0x49, 0x4d,
        0x4c, 0x2f, 0x4d, 0x2d, 0x56, 0xb2, 0x8a, 0xae, 0x56, 0xca, 0x4c, 0x51, 0xb2, 0x52, 0x0a,
        0x2e, 0x49, 0xcc, 0x4b, 0x49, 0x2c, 0x4a, 0x51, 0xaa, 0xd5, 0x81, 0x8a, 0x78, 0x00, 0x79,
        0xc9, 0xf9, 0x45, 0xa9, 0x4a, 0xb5, 0xb1, 0xb5, 0x00, 0x7c, 0xb4, 0x79, 0x77, 0x31, 0x00,
        0x00, 0x00,
    ];

    /// Spawns a raw-TCP fake upstream that answers exactly one request with
    /// a hand-built HTTP/1.1 response reproducing the real bug: a gzip body
    /// (`Content-Encoding: gzip`) sent as `Transfer-Encoding: chunked` over
    /// a `Connection: keep-alive` connection — precisely what Cloudflare
    /// sends for `/api/trade2/*` and what upstream's `Content-Length`-only
    /// `robots.txt` response doesn't exercise. Built by hand over raw TCP,
    /// not through axum/hyper, so nothing here can second-guess which
    /// headers actually go over the wire the way a higher-level server
    /// might.
    async fn spawn_fake_upstream() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(
                    n > 0,
                    "fake upstream: connection closed before a full request arrived"
                );
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            let (first, second) = GZIPPED_LEAGUES_JSON.split_at(GZIPPED_LEAGUES_JSON.len() / 2);
            let mut chunked = Vec::new();
            for chunk in [first, second] {
                chunked.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                chunked.extend_from_slice(chunk);
                chunked.extend_from_slice(b"\r\n");
            }
            chunked.extend_from_slice(b"0\r\n\r\n");

            let mut response = Vec::new();
            response.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
            response.extend_from_slice(b"Content-Encoding: gzip\r\n");
            response.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
            response.extend_from_slice(b"Connection: keep-alive\r\n");
            response.extend_from_slice(b"X-Rate-Limit-Ip: 1:5:60\r\n");
            response.extend_from_slice(b"Set-Cookie: cf_clearance=abc123; Path=/\r\n");
            response.extend_from_slice(b"\r\n");
            response.extend_from_slice(&chunked);

            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.ok();
        });
        addr
    }

    #[tokio::test]
    async fn handle_returns_decoded_json_for_a_chunked_gzip_upstream() {
        let addr = spawn_fake_upstream().await;
        let proxy = Arc::new(Proxy::new());
        let host = format!("127.0.0.1:{}", addr.port());
        let uri: axum::http::Uri = format!("/proxy/{host}/api/trade2/data/leagues")
            .parse()
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );

        let response = handle(
            State(proxy),
            Method::GET,
            OriginalUri(uri),
            headers,
            Bytes::new(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("x-rate-limit-ip").is_some(),
            "end-to-end header x-rate-limit-ip should reach the renderer"
        );
        assert!(response.headers().get(header::TRANSFER_ENCODING).is_none());
        assert!(response.headers().get(header::CONNECTION).is_none());
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body)
            .unwrap_or_else(|err| panic!("expected decoded JSON, got {body:?}: {err}"));
        assert_eq!(json["leagues"][0]["id"], "Standard");
        assert_eq!(json["leagues"][1]["id"], "Hardcore");
    }
}
