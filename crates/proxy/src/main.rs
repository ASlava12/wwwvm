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
//! misconfigured deployment fails closed rather than open.
//!
//! Run:
//!     WWWVM_PROXY_ALLOWLIST='*' cargo run -p wwwvm-proxy -- 0.0.0.0:9000

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

    let listener = TcpListener::bind(bind).await.context("bind")?;
    log::info!("wwwvm-proxy listening on {bind}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let allow = allow.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, peer, allow).await {
                log::warn!("{peer}: {e:#}");
            }
        });
    }
}

async fn handle(stream: TcpStream, peer: SocketAddr, allow: Arc<Allowlist>) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream)
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

    // ws -> tcp
    let ws_to_tcp = async move {
        while let Some(msg) = ws_stream.next().await {
            match msg? {
                Message::Binary(b) => tcp_wr.write_all(&b).await?,
                Message::Text(t) => tcp_wr.write_all(t.as_bytes()).await?,
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = tcp_wr.shutdown().await;
        Ok::<_, anyhow::Error>(())
    };

    // tcp -> ws
    let tcp_to_ws = async move {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = tcp_rd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ws_sink.send(Message::Binary(buf[..n].to_vec())).await?;
        }
        let _ = ws_sink.send(Message::Close(None)).await;
        Ok::<_, anyhow::Error>(())
    };

    let (a, b) = tokio::join!(ws_to_tcp, tcp_to_ws);
    a?;
    b?;
    Ok(())
}
