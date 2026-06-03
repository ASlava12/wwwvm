//! WebSocket ↔ TCP gateway.
//!
//! Lets the browser-resident VM reach the outside world. Clients open
//! a WebSocket, send a single JSON connect frame
//! (`{"host":"…","port":N}`), and after that every binary WS message is
//! forwarded as TCP bytes, with TCP bytes coming back as binary WS
//! messages.
//!
//! Hosts are matched against an allowlist read from the
//! `WWWVM_PROXY_ALLOWLIST` env var (comma-separated `host:port`,
//! `host:*`, or `*`). Default is empty — deny everything — so a
//! misconfigured deployment fails closed rather than open. `*` is an
//! OPEN RELAY and must never be used on a reachable bind.
//!
//! `WWWVM_PROXY_ORIGINS` (comma-separated) locks WebSocket handshakes to
//! specific browser Origins (guards against Cross-Site WebSocket Hijacking);
//! unset accepts any Origin (fine for a loopback dev demo only).
//!
//! The connect frame may also chain through a third-party public proxy instead
//! of connecting to the target directly: `{"host","port","upstream":{"kind":
//! "socks5"|"socks4"|"http","host","port"}}` tunnels through that one, and
//! `{"host","port","auto":true}` lets the server pick & rotate one from the
//! pool in `WWWVM_PROXY_UPSTREAMS_FILE` (the JSON `scripts/fetch-proxies.py`
//! writes). See `upstream.rs`. Public proxies are untrusted — non-sensitive
//! traffic only.
//!
//! Run (specific host, bound to loopback — the safe default):
//!     WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80' \
//!     WWWVM_PROXY_ORIGINS='http://localhost:8080' \
//!       cargo run -p wwwvm-proxy -- 127.0.0.1:8080

#![forbid(unsafe_code)]

mod upstream;

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

#[derive(Deserialize)]
struct ConnectFrame {
    host: String,
    port: u16,
    /// Optional: tunnel through this specific upstream proxy instead of
    /// connecting to the target directly (browser-selected / manual entry).
    #[serde(default)]
    upstream: Option<upstream::Upstream>,
    /// Optional: let the server pick & rotate an upstream from its pool
    /// (`WWWVM_PROXY_UPSTREAMS_FILE`). Ignored if `upstream` is set.
    #[serde(default)]
    auto: bool,
}

/// Control text frame meaning "the guest half-closed its write side": shut down
/// the upstream TCP write side but keep the socket open for the response. Our
/// client only ever sends binary data frames otherwise, so a text frame is
/// unambiguously control.
const HALF_CLOSE: &str = "FIN";

// The connection allowlist now lives in `wwwvm-net` so the proxy and the
// in-process guest bridge share one deny-by-default implementation.
use wwwvm_net::Allowlist;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bind: SocketAddr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".into())
        .parse()
        .context("bind address")?;

    let allow = Arc::new(Allowlist::from_env());
    if allow.is_empty() {
        log::warn!(
            "WWWVM_PROXY_ALLOWLIST is empty — every connect attempt will be rejected. \
             Set it to e.g. `*` (any) or `example.com:443,localhost:*` (specific)."
        );
    }
    // An open relay (`*`) is dangerous; on a non-loopback bind it lets ANY
    // reachable client tunnel TCP to ANY host through this process. Warn hard.
    if allow.allows_anything() {
        let public = !bind.ip().is_loopback();
        log::warn!(
            "WWWVM_PROXY_ALLOWLIST permits `*` (ANY host:port) — this is an OPEN RELAY.{}",
            if public {
                " Bound to a NON-loopback address: anyone who can reach this port can \
                 tunnel through you. Use a SPECIFIC allowlist (e.g. dl-cdn.alpinelinux.org:443) \
                 and/or bind to 127.0.0.1. `*` is for throwaway local testing only."
            } else {
                " (Bound to loopback — local testing only; never expose this.)"
            }
        );
    }

    // Cross-Site WebSocket Hijacking guard: a browser lets any page open a
    // WebSocket to us, sending that page's Origin. With a permissive allowlist
    // a malicious page could drive the relay. If WWWVM_PROXY_ORIGINS is set
    // (comma-separated), only those Origins are accepted; otherwise any Origin
    // is allowed (fine for a loopback dev demo) and we say so once.
    let origins: Arc<Option<Vec<String>>> = Arc::new(match env::var("WWWVM_PROXY_ORIGINS") {
        Ok(s) if !s.trim().is_empty() => {
            let list: Vec<String> = s.split(',').map(|o| o.trim().to_string()).collect();
            log::info!("accepting WebSocket Origins: {}", list.join(", "));
            Some(list)
        }
        _ => {
            log::warn!(
                "WWWVM_PROXY_ORIGINS unset — accepting WebSocket handshakes from ANY Origin \
                 (Cross-Site WebSocket Hijacking possible). Set it to the page's origin \
                 (e.g. http://localhost:8080) to lock this down."
            );
            None
        }
    });

    // Optional pool of upstream proxies for "auto" mode — the JSON file written
    // by scripts/fetch-proxies.py. Read fresh per request so a cron refresh is
    // picked up without restarting. Unset → "auto" requests are rejected.
    let upstreams_file: Arc<Option<PathBuf>> =
        Arc::new(env::var_os("WWWVM_PROXY_UPSTREAMS_FILE").map(PathBuf::from));
    match upstreams_file.as_ref() {
        Some(p) => log::info!("auto-upstream pool: {}", p.display()),
        None => log::info!(
            "WWWVM_PROXY_UPSTREAMS_FILE unset — 'auto' upstream mode disabled \
             (direct + explicit-upstream still work)"
        ),
    }

    let listener = TcpListener::bind(bind).await.context("bind")?;
    log::info!("wwwvm-proxy listening on {bind}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let allow = allow.clone();
        let origins = origins.clone();
        let upstreams_file = upstreams_file.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, peer, allow, origins, upstreams_file).await {
                log::warn!("{peer}: {e:#}");
            }
        });
    }
}

// The tungstenite handshake callback returns Result<Response, ErrorResponse>,
// and ErrorResponse (an http::Response) is large — that's the library's
// signature, not ours, so silence the size lint here.
#[allow(clippy::result_large_err)]
async fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    allow: Arc<Allowlist>,
    origins: Arc<Option<Vec<String>>>,
    upstreams_file: Arc<Option<PathBuf>>,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    // Reject disallowed Origins during the handshake (before any TCP connect).
    let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
        if let Some(allowed) = origins.as_ref() {
            let origin = req
                .headers()
                .get("origin")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !allowed.iter().any(|o| o == origin) {
                let mut err = ErrorResponse::new(Some(format!("origin {origin:?} not allowed")));
                *err.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
                return Err(err);
            }
        }
        Ok(resp)
    })
    .await
    .context("websocket handshake")?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    let frame_msg = tokio::time::timeout(Duration::from_secs(10), ws_stream.next())
        .await
        .map_err(|_| anyhow!("connect frame timeout"))?
        .ok_or_else(|| anyhow!("client closed before connect"))??;
    let frame_text = match frame_msg {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b).context("connect frame not utf-8")?,
        other => return Err(anyhow!("unexpected first frame: {:?}", other)),
    };
    let connect: ConnectFrame = serde_json::from_str(&frame_text).context("connect frame json")?;
    if !allow.permits(&connect.host, connect.port) {
        let _ = ws_sink
            .send(Message::Text(format!(
                "ERR {}:{} not in allowlist",
                connect.host, connect.port
            )))
            .await;
        return Err(anyhow!(
            "{peer}: refused {}:{} — not in allowlist",
            connect.host,
            connect.port
        ));
    }

    // Establish the byte stream to the target. Three ways, in priority order:
    //   1. explicit upstream  — chain through the browser-selected proxy
    //   2. auto               — server picks & rotates from its pool
    //   3. direct (default)   — connect to the target ourselves
    // On any failure we tell the client (ERR text frame) before bailing so the
    // browser can surface why a flow didn't open.
    let tcp_result: Result<TcpStream> = if let Some(up) = &connect.upstream {
        log::info!("{peer} -> {}:{} via {up}", connect.host, connect.port);
        upstream::open_via(up, &connect.host, connect.port).await
    } else if connect.auto {
        match upstreams_file.as_ref() {
            Some(path) => match upstream::load_auto_list(path).await {
                Ok(list) => {
                    log::info!(
                        "{peer} -> {}:{} via auto ({} upstreams)",
                        connect.host,
                        connect.port,
                        list.len()
                    );
                    upstream::open_auto(&list, &connect.host, connect.port).await
                }
                Err(e) => Err(e),
            },
            None => Err(anyhow!(
                "auto upstream requested but WWWVM_PROXY_UPSTREAMS_FILE is unset"
            )),
        }
    } else {
        // Direct: resolve the host OURSELVES and pin a vetted, globally-routable
        // address. Passing the hostname to connect() would re-resolve at connect
        // time (reopening a DNS-rebinding window) and could reach a loopback/
        // private/internal IP (SSRF). The allowlist authorizes a *name*; the IP
        // it points at must still be external. (v4-only — the bridge is v4.)
        let addr = tokio::net::lookup_host((connect.host.as_str(), connect.port))
            .await
            .context("resolve")?
            .find(|sa| match sa.ip() {
                IpAddr::V4(ip) => wwwvm_net::allow::is_globally_routable(ip),
                IpAddr::V6(_) => false,
            });
        match addr {
            Some(addr) => {
                log::info!(
                    "{peer} -> {} [{}]:{}",
                    connect.host,
                    addr.ip(),
                    connect.port
                );
                TcpStream::connect(addr).await.context("upstream connect")
            }
            None => Err(anyhow!(
                "no globally-routable address for {} (SSRF guard)",
                connect.host
            )),
        }
    };
    let tcp = match tcp_result {
        Ok(tcp) => tcp,
        Err(e) => {
            let _ = ws_sink
                .send(Message::Text(format!(
                    "ERR {}:{} {e:#}",
                    connect.host, connect.port
                )))
                .await;
            return Err(anyhow!("{peer}: {} failed: {e:#}", connect.host));
        }
    };
    let (mut tcp_rd, mut tcp_wr) = tcp.into_split();

    // ws -> tcp (guest → upstream). Log the first chunk + total so a stalled
    // relay is diagnosable (did the guest's request reach the server at all?).
    let ws_to_tcp = async move {
        let mut total = 0u64;
        while let Some(msg) = ws_stream.next().await {
            match msg? {
                Message::Binary(b) => {
                    if total == 0 {
                        log::info!("{peer} ws→tcp: first {} bytes", b.len());
                    }
                    total += b.len() as u64;
                    tcp_wr.write_all(&b).await?
                }
                // A text frame is a control signal from our client (which only
                // ever sends binary data frames otherwise). "FIN" = the guest
                // half-closed its write side: shut down the upstream write side
                // and stop reading this direction, but DON'T close the socket —
                // the tcp→ws task keeps delivering the response.
                Message::Text(t) if t == HALF_CLOSE => break,
                Message::Text(t) => {
                    total += t.len() as u64;
                    tcp_wr.write_all(t.as_bytes()).await?
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        log::info!("{peer} ws→tcp: done, {total} bytes total");
        let _ = tcp_wr.shutdown().await;
        Ok::<_, anyhow::Error>(())
    };

    // tcp -> ws (upstream → guest).
    let tcp_to_ws = async move {
        let mut buf = vec![0u8; 4096];
        let mut total = 0u64;
        loop {
            let n = tcp_rd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if total == 0 {
                log::info!("{peer} tcp→ws: first {n} bytes");
            }
            total += n as u64;
            ws_sink.send(Message::Binary(buf[..n].to_vec())).await?;
        }
        log::info!("{peer} tcp→ws: done, {total} bytes total");
        let _ = ws_sink.send(Message::Close(None)).await;
        Ok::<_, anyhow::Error>(())
    };

    let (a, b) = tokio::join!(ws_to_tcp, tcp_to_ws);
    a?;
    b?;
    Ok(())
}
