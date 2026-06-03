//! HTTP service for the content-addressed snapshot store: admin-token-gated
//! `PUT` for pages/manifests, open `GET`/`HEAD`. Thin glue around
//! [`wwwvm_snapstore::http::route`] (which has the tested routing/auth logic) —
//! this just parses HTTP/1.1 and writes responses (one request per connection,
//! `Connection: close`).
//!
//! Run:
//!   WWWVM_SNAPSTORE_TOKEN=secret WWWVM_SNAPSTORE_ROOT=/data/snaps \
//!     cargo run -p wwwvm-snapstore --bin snapstore-server -- 127.0.0.1:8090
//!
//! Reads are immutable, so a CDN/Caddy can serve `GET /pages/* /manifests/*`
//! straight from the root dir; only the authenticated `PUT`s need this service.

#![forbid(unsafe_code)]

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wwwvm_snapstore::{http, Store};

/// Max request-body bytes. A manifest (non-RAM snapshot bytes + a page-hash list
/// of 32 B × pages) is a few MB even for a 1 GiB-RAM VM; 128 MiB is generous.
const MAX_BODY: usize = 128 * 1024 * 1024;
/// Max request-head bytes (guards against a client that never sends `\r\n\r\n`).
const MAX_HEAD: usize = 16 * 1024;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bind: SocketAddr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8090".into())
        .parse()
        .expect("bind address (e.g. 127.0.0.1:8090)");
    let root = env::var("WWWVM_SNAPSTORE_ROOT").unwrap_or_else(|_| "snapstore-data".into());
    let store = Arc::new(Store::open(&root)?);
    let token = Arc::new(
        env::var("WWWVM_SNAPSTORE_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    );
    match token.as_deref() {
        Some(_) => log::info!("admin token set — uploads (PUT) require it"),
        None => log::warn!(
            "WWWVM_SNAPSTORE_TOKEN unset — store is READ-ONLY (every PUT → 401). \
             Set it to enable uploads."
        ),
    }

    let listener = TcpListener::bind(bind).await?;
    log::info!("wwwvm-snapstore listening on {bind} (root {root})");

    loop {
        let (sock, peer) = listener.accept().await?;
        let store = store.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(sock, store, token).await {
                log::debug!("{peer}: {e}");
            }
        });
    }
}

async fn serve(
    mut sock: TcpStream,
    store: Arc<Store>,
    token: Arc<Option<String>>,
) -> std::io::Result<()> {
    // Read until the blank line ending the request head.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return respond(&mut sock, &resp(413, "head too large"), false).await;
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(()); // client closed before sending a full request
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = &buf[..head_end];
    let Some((method, path, content_len, auth)) = parse_head(head) else {
        return respond(&mut sock, &resp(400, "bad request"), false).await;
    };
    // CORS preflight: the browser sends OPTIONS before a cross-origin PUT (which
    // carries Authorization + a body). Answer it with the allow headers; the
    // real auth still happens on the PUT itself.
    if method == "OPTIONS" {
        return respond(&mut sock, &resp(204, ""), true).await;
    }
    let is_head = method == "HEAD";

    // Read the body (already-buffered remainder + the rest up to Content-Length).
    let body_start = head_end + 4;
    let mut body = buf[body_start..].to_vec();
    if content_len > MAX_BODY {
        return respond(&mut sock, &resp(413, "body too large"), false).await;
    }
    while body.len() < content_len {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);

    let response = http::route(
        &store,
        token.as_deref(),
        &method,
        &path,
        auth.as_deref(),
        &body,
    );
    respond(&mut sock, &response, is_head).await
}

/// Parse the request line + the headers we care about. Returns
/// `(method, path, content_length, authorization)`.
fn parse_head(head: &[u8]) -> Option<(String, String, usize, Option<String>)> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    // (HTTP version ignored — we always answer 1.1 + Connection: close.)

    let mut content_len = 0usize;
    let mut auth = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "content-length" => content_len = value.parse().unwrap_or(0),
                "authorization" => auth = Some(value.to_string()),
                _ => {}
            }
        }
    }
    Some((
        method,
        path,
        content_len,
        http::bearer(auth.as_deref()).map(str::to_string),
    ))
}

/// Build a small text response (for the server's own error cases).
fn resp(status: u16, msg: &str) -> http::Response {
    http::Response {
        status,
        content_type: "text/plain; charset=utf-8",
        body: msg.as_bytes().to_vec(),
    }
}

/// Write an HTTP/1.1 response and close. For HEAD, omit the body but keep an
/// accurate-enough Content-Length of 0 (clients use the status for existence).
async fn respond(sock: &mut TcpStream, r: &http::Response, is_head: bool) -> std::io::Result<()> {
    let body: &[u8] = if is_head { &[] } else { &r.body };
    // Permissive CORS so the browser uploader can reach the store cross-origin
    // (e.g. page on :8081, store on :8090). Writes are still gated by the admin
    // token on the request itself — CORS only governs which origins the browser
    // lets read the response / send the header.
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, HEAD, PUT, OPTIONS\r\n\
         Access-Control-Allow-Headers: Authorization, Content-Type\r\n\
         Access-Control-Max-Age: 86400\r\n\
         Connection: close\r\n\r\n",
        r.status,
        http::reason(r.status),
        r.content_type,
        body.len(),
    );
    sock.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        sock.write_all(body).await?;
    }
    sock.flush().await
}

/// First index of `needle` in `hay`, or `None`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
