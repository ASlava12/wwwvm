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
//! Run (specific host, bound to loopback — the safe default):
//!     WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80' \
//!     WWWVM_PROXY_ORIGINS='http://localhost:8080' \
//!       cargo run -p wwwvm-proxy -- 127.0.0.1:8080

#![forbid(unsafe_code)]

use std::env;
use std::net::{IpAddr, SocketAddr};
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
}

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

    let listener = TcpListener::bind(bind).await.context("bind")?;
    log::info!("wwwvm-proxy listening on {bind}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let allow = allow.clone();
        let origins = origins.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, peer, allow, origins).await {
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

    // Resolve the host OURSELVES and pin a vetted, globally-routable address.
    // Passing the hostname to connect() would re-resolve at connect time
    // (reopening a DNS-rebinding window) and could reach a loopback/private/
    // internal IP (SSRF). The allowlist authorizes a *name*; the IP it points
    // at must still be a real external address. (v4-only — the bridge is v4.)
    let addr = tokio::net::lookup_host((connect.host.as_str(), connect.port))
        .await
        .context("resolve")?
        .find(|sa| match sa.ip() {
            IpAddr::V4(ip) => wwwvm_net::allow::is_globally_routable(ip),
            IpAddr::V6(_) => false,
        });
    let Some(addr) = addr else {
        let _ = ws_sink
            .send(Message::Text(format!(
                "ERR {}:{} no globally-routable address",
                connect.host, connect.port
            )))
            .await;
        return Err(anyhow!(
            "{peer}: refused {} — no globally-routable address (SSRF guard)",
            connect.host
        ));
    };

    log::info!(
        "{peer} -> {} [{}]:{}",
        connect.host,
        addr.ip(),
        connect.port
    );
    let tcp = TcpStream::connect(addr).await.context("upstream connect")?;
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
