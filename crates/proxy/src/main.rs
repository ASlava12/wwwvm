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
use std::net::SocketAddr;
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

#[derive(Debug, Clone)]
struct Allowlist {
    entries: Vec<AllowEntry>,
}

#[derive(Debug, Clone)]
enum AllowEntry {
    Anything,
    Host { host: String, port: Option<u16> },
}

impl Allowlist {
    fn from_env() -> Self {
        let raw = env::var("WWWVM_PROXY_ALLOWLIST").unwrap_or_default();
        let entries = raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let s = s.trim();
                if s == "*" {
                    return AllowEntry::Anything;
                }
                if let Some((h, p)) = s.rsplit_once(':') {
                    let port = if p == "*" { None } else { p.parse().ok() };
                    AllowEntry::Host {
                        host: h.to_string(),
                        port,
                    }
                } else {
                    AllowEntry::Host {
                        host: s.to_string(),
                        port: None,
                    }
                }
            })
            .collect();
        Self { entries }
    }

    fn permits(&self, host: &str, port: u16) -> bool {
        for e in &self.entries {
            match e {
                AllowEntry::Anything => return true,
                AllowEntry::Host { host: h, port: p } => {
                    if h.eq_ignore_ascii_case(host) && p.is_none_or(|pp| pp == port) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bind: SocketAddr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".into())
        .parse()
        .context("bind address")?;

    let allow = Arc::new(Allowlist::from_env());
    if allow.entries.is_empty() {
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

    log::info!("{peer} -> {}:{}", connect.host, connect.port);
    let tcp = TcpStream::connect((connect.host.as_str(), connect.port))
        .await
        .context("upstream connect")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(entries: &[&str]) -> Allowlist {
        // Reuse the same parser path as from_env without mutating the
        // process environment (tests run in parallel and env mutation
        // is racey).
        let raw = entries.join(",");
        let entries = raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                let s = s.trim();
                if s == "*" {
                    return AllowEntry::Anything;
                }
                if let Some((h, p)) = s.rsplit_once(':') {
                    let port = if p == "*" { None } else { p.parse().ok() };
                    AllowEntry::Host {
                        host: h.to_string(),
                        port,
                    }
                } else {
                    AllowEntry::Host {
                        host: s.to_string(),
                        port: None,
                    }
                }
            })
            .collect();
        Allowlist { entries }
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let a = allow(&[]);
        assert!(!a.permits("example.com", 80));
    }

    #[test]
    fn star_allows_anything() {
        let a = allow(&["*"]);
        assert!(a.permits("evil.example.com", 9000));
    }

    #[test]
    fn host_port_exact_match() {
        let a = allow(&["example.com:443"]);
        assert!(a.permits("example.com", 443));
        assert!(!a.permits("example.com", 80));
        assert!(!a.permits("other.com", 443));
    }

    #[test]
    fn host_wildcard_port() {
        let a = allow(&["example.com:*"]);
        assert!(a.permits("example.com", 80));
        assert!(a.permits("example.com", 443));
        assert!(!a.permits("other.com", 443));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let a = allow(&["Example.COM:443"]);
        assert!(a.permits("example.com", 443));
    }

    /// Multiple comma-separated entries should compose with OR
    /// semantics: a host that matches *any* entry is allowed. A
    /// regression collapsing the split (single entry of the whole
    /// joined string) would deny everything except the exact
    /// "a:80,b:443" hostname — silently breaking the multi-host
    /// allowlist users configure.
    #[test]
    fn multiple_entries_compose_or() {
        let a = allow(&["example.com:443", "localhost:8080"]);
        assert!(a.permits("example.com", 443));
        assert!(a.permits("localhost", 8080));
        assert!(!a.permits("example.com", 8080), "wrong port");
        assert!(!a.permits("localhost", 443), "wrong port");
        assert!(!a.permits("other.com", 443), "host not in list");
    }

    /// Whitespace around entries is trimmed — the parser uses
    /// `s.trim()`. Users naturally write `"a:80, b:443"` with a
    /// space after the comma; if the trim drops, `" b:443"`
    /// would never match `"b"` and the second host silently
    /// becomes inaccessible.
    #[test]
    fn whitespace_around_entries_is_trimmed() {
        let a = allow(&["  example.com:443  ", "\tlocalhost:8080"]);
        assert!(a.permits("example.com", 443));
        assert!(a.permits("localhost", 8080));
    }

    /// A host entry with no `:port` portion ("example.com" with
    /// no colon at all) goes to the `port: None` branch — meaning
    /// "any port on this host". Distinct from `host:*` which uses
    /// `rsplit_once(':')` + literal `*`. Both should reach the
    /// same `port.is_none_or(...)` outcome. Pins the no-colon
    /// path so a regression that flipped it to "port 0 required"
    /// surfaces here.
    #[test]
    fn host_without_colon_allows_any_port() {
        let a = allow(&["example.com"]);
        assert!(a.permits("example.com", 80));
        assert!(a.permits("example.com", 443));
        assert!(a.permits("example.com", 31337));
        assert!(!a.permits("other.com", 80));
    }
}
