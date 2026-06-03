//! Upstream proxy chaining: instead of connecting to the target directly, the
//! relay can tunnel through a third-party public proxy (SOCKS5, SOCKS4/4a, or
//! HTTP CONNECT). The browser picks one (from the fetched list, manual entry,
//! or "auto" = let the server rotate), and we perform that proxy's handshake on
//! a fresh TCP connection, returning a stream that's already wired through to
//! the target — the byte relay above it is identical to the direct path.
//!
//! Security: the *upstream* proxy's address is resolved here and required to be
//! globally routable (a public proxy can't be a cover for hitting our own
//! loopback/LAN). The *target* host:port is still gated by the shared
//! `Allowlist` in the caller before we get here; because the upstream resolves
//! the target itself, we hand it the hostname (SOCKS5 domain / SOCKS4a /
//! HTTP CONNECT) rather than re-resolving — there's nothing to pin on our side.
//!
//! Public proxies are UNTRUSTED: they can read and tamper with the bytes that
//! pass through them. They're for reaching non-sensitive endpoints, never for
//! traffic that carries credentials.

use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use wwwvm_net::allow::is_globally_routable;

/// How the upstream proxy speaks. Deserialized from the browser's connect frame
/// (`kind`) and from the fetched proxy list (`type`); lowercase wire form.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    Socks5,
    Socks4,
    Http,
}

/// A concrete upstream proxy to chain through (browser-selected or auto-picked).
#[derive(Deserialize, Clone, Debug)]
pub struct Upstream {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let k = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Socks4 => "socks4",
            ProxyKind::Http => "http",
        };
        write!(f, "{k}://{}:{}", self.host, self.port)
    }
}

/// Per-upstream connect+handshake budget. Public proxies are flaky, so keep
/// this tight — for "auto" we want to fail a dead proxy fast and rotate on.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(8);
/// How many proxies "auto" tries before giving up (rotating through the list).
const AUTO_MAX_TRIES: usize = 6;

/// Resolve the upstream proxy's own address and require it to be globally
/// routable, then open a TCP connection to it. Rejects loopback/private/
/// reserved targets so a manual or auto upstream can't be aimed at our host.
async fn dial_upstream(up: &Upstream) -> Result<TcpStream> {
    let addr = tokio::net::lookup_host((up.host.as_str(), up.port))
        .await
        .with_context(|| format!("resolve upstream {}", up.host))?
        .find(|sa| match sa.ip() {
            IpAddr::V4(ip) => is_globally_routable(ip),
            IpAddr::V6(_) => false, // the bridge is v4-only
        })
        .ok_or_else(|| anyhow!("upstream {} has no globally-routable v4 address", up.host))?;
    let stream = tokio::time::timeout(UPSTREAM_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("upstream {} connect timed out", up))??;
    Ok(stream)
}

/// Open a tunnel to `target_host:target_port` through one specific upstream.
pub async fn open_via(up: &Upstream, target_host: &str, target_port: u16) -> Result<TcpStream> {
    let mut stream = dial_upstream(up).await?;
    let handshake = async {
        match up.kind {
            ProxyKind::Socks5 => socks5_handshake(&mut stream, target_host, target_port).await,
            ProxyKind::Socks4 => socks4_handshake(&mut stream, target_host, target_port).await,
            ProxyKind::Http => http_connect(&mut stream, target_host, target_port).await,
        }
    };
    tokio::time::timeout(UPSTREAM_TIMEOUT, handshake)
        .await
        .map_err(|_| anyhow!("upstream {} handshake timed out", up))??;
    Ok(stream)
}

/// "Auto" mode: try proxies from `list` (round-robin from a rotating offset) until
/// one completes its handshake to the target, or `AUTO_MAX_TRIES` have failed.
pub async fn open_auto(
    list: &[Upstream],
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    if list.is_empty() {
        bail!(
            "auto upstream requested but the proxy list is empty (set WWWVM_PROXY_UPSTREAMS_FILE)"
        );
    }
    // Round-robin start offset so concurrent guests don't all hammer entry 0.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let start = NEXT.fetch_add(1, Ordering::Relaxed);
    let tries = AUTO_MAX_TRIES.min(list.len());
    let mut last_err = anyhow!("no upstream tried");
    for i in 0..tries {
        let up = &list[(start + i) % list.len()];
        match open_via(up, target_host, target_port).await {
            Ok(s) => {
                log::info!("auto: chained through {up}");
                return Ok(s);
            }
            Err(e) => {
                log::debug!("auto: {up} failed: {e:#}");
                last_err = e;
            }
        }
    }
    Err(last_err.context(format!("auto: all {tries} upstream attempts failed")))
}

// ---- SOCKS5 (RFC 1928), no authentication ----

async fn socks5_handshake(s: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    // Greeting: VER=5, one method, NO-AUTH (0x00).
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut sel = [0u8; 2];
    s.read_exact(&mut sel)
        .await
        .context("socks5 greeting reply")?;
    if sel[0] != 0x05 || sel[1] != 0x00 {
        bail!("socks5: server rejected no-auth (got {:?})", sel);
    }
    // Request: VER=5, CMD=CONNECT(1), RSV=0, ATYP=domain(3), len, name, port.
    let name = host.as_bytes();
    if name.len() > 255 {
        bail!("socks5: hostname too long ({} bytes)", name.len());
    }
    let mut req = Vec::with_capacity(7 + name.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, name.len() as u8]);
    req.extend_from_slice(name);
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    // Reply: VER, REP, RSV, ATYP, BND.ADDR, BND.PORT.
    let mut head = [0u8; 4];
    s.read_exact(&mut head)
        .await
        .context("socks5 connect reply")?;
    if head[1] != 0x00 {
        bail!("socks5: CONNECT failed, REP={:#04x}", head[1]);
    }
    // Drain the bound address so the stream is positioned at the relayed bytes.
    let addr_len = match head[3] {
        0x01 => 4,                          // IPv4
        0x04 => 16,                         // IPv6
        0x03 => read_u8(s).await? as usize, // domain: length-prefixed
        other => bail!("socks5: bad reply ATYP {other:#04x}"),
    };
    let mut skip = vec![0u8; addr_len + 2]; // + 2-byte BND.PORT
    s.read_exact(&mut skip).await.context("socks5 bound addr")?;
    Ok(())
}

async fn read_u8(s: &mut TcpStream) -> Result<u8> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b).await?;
    Ok(b[0])
}

// ---- SOCKS4 / SOCKS4a (no auth) ----

async fn socks4_handshake(s: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    // SOCKS4a: dest IP 0.0.0.x (x != 0) signals "use the trailing hostname";
    // the proxy resolves it. We always use 4a so we never resolve the target.
    let name = host.as_bytes();
    let mut req = Vec::with_capacity(9 + name.len() + 1);
    req.push(0x04); // VER
    req.push(0x01); // CMD = CONNECT
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&[0, 0, 0, 1]); // 0.0.0.1 → SOCKS4a marker
    req.push(0x00); // empty USERID, NUL-terminated
    req.extend_from_slice(name);
    req.push(0x00); // hostname NUL terminator
    s.write_all(&req).await?;
    // Reply: VN=0, CD, then 6 bytes ignored. CD 0x5A = granted.
    let mut reply = [0u8; 8];
    s.read_exact(&mut reply).await.context("socks4 reply")?;
    if reply[1] != 0x5A {
        bail!("socks4: request rejected, CD={:#04x}", reply[1]);
    }
    Ok(())
}

// ---- HTTP CONNECT tunneling ----

async fn http_connect(s: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    let req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;
    // Read headers until the blank line, bounded so a misbehaving proxy can't
    // make us buffer forever. We only need the status line + terminator.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await?;
        if n == 0 {
            bail!("http connect: upstream closed before response");
        }
        buf.push(byte[0]);
        if buf.len() > 8192 {
            bail!("http connect: response headers too large");
        }
    }
    let status_line = buf
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(&[]);
    let line = String::from_utf8_lossy(status_line);
    // "HTTP/1.1 200 Connection established"
    let ok = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .map(|c| (200..300).contains(&c))
        .unwrap_or(false);
    if !ok {
        bail!("http connect: upstream refused — {:?}", line);
    }
    Ok(())
}

// ---- "auto" list loading (from the fetch-proxies.py output) ----

/// One entry of the `web/proxies.json` the cron parser writes.
#[derive(Deserialize)]
struct ProxyRecord {
    #[serde(rename = "type")]
    kind: String,
    host: String,
    port: u16,
}

#[derive(Deserialize)]
struct ProxyFile {
    #[serde(default)]
    proxies: Vec<ProxyRecord>,
}

/// Load the auto-rotation pool from the JSON file `fetch-proxies.py` writes.
/// Read fresh each call so a cron refresh is picked up without a restart;
/// `https` entries (which would need TLS to the proxy) are skipped.
pub async fn load_auto_list(path: &Path) -> Result<Vec<Upstream>> {
    let raw = tokio::fs::read(path)
        .await
        .with_context(|| format!("read proxy list {}", path.display()))?;
    let file: ProxyFile = serde_json::from_slice(&raw).context("parse proxy list json")?;
    let list = file
        .proxies
        .into_iter()
        .filter_map(|r| {
            let kind = match r.kind.as_str() {
                "socks5" => ProxyKind::Socks5,
                "socks4" => ProxyKind::Socks4,
                "http" => ProxyKind::Http,
                _ => return None, // skip https / unknown
            };
            Some(Upstream {
                kind,
                host: r.host,
                port: r.port,
            })
        })
        .collect();
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_kind_parses_lowercase() {
        let u: Upstream =
            serde_json::from_str(r#"{"kind":"socks5","host":"1.2.3.4","port":1080}"#).unwrap();
        assert_eq!(u.kind, ProxyKind::Socks5);
        assert_eq!(u.host, "1.2.3.4");
        assert_eq!(u.port, 1080);
        assert_eq!(u.to_string(), "socks5://1.2.3.4:1080");
    }

    #[test]
    fn proxy_file_skips_https_and_unknown() {
        let json = r#"{"updated":1,"proxies":[
            {"type":"socks5","host":"1.1.1.1","port":1080,"source":"x"},
            {"type":"https","host":"2.2.2.2","port":443,"source":"x"},
            {"type":"http","host":"3.3.3.3","port":8080,"source":"x"},
            {"type":"socks4","host":"4.4.4.4","port":1080,"source":"x"},
            {"type":"weird","host":"5.5.5.5","port":1,"source":"x"}
        ]}"#;
        let file: ProxyFile = serde_json::from_slice(json.as_bytes()).unwrap();
        let list: Vec<Upstream> = file
            .proxies
            .into_iter()
            .filter_map(|r| {
                let kind = match r.kind.as_str() {
                    "socks5" => ProxyKind::Socks5,
                    "socks4" => ProxyKind::Socks4,
                    "http" => ProxyKind::Http,
                    _ => return None,
                };
                Some(Upstream {
                    kind,
                    host: r.host,
                    port: r.port,
                })
            })
            .collect();
        assert_eq!(list.len(), 3, "https + weird dropped");
        assert_eq!(list[0].kind, ProxyKind::Socks5);
        assert_eq!(list[1].kind, ProxyKind::Http);
        assert_eq!(list[2].kind, ProxyKind::Socks4);
    }

    /// SOCKS5 hostnames are length-prefixed with one byte → 255 max. The
    /// handshake must reject longer names rather than truncate the prefix.
    #[tokio::test]
    async fn socks5_rejects_overlong_hostname() {
        // We can't easily run the network handshake in a unit test, but the
        // length guard is pure logic — exercise it via a loopback pair so the
        // write side has somewhere to go before the check fires.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // Answer the greeting so we reach the request-build stage.
            let mut g = [0u8; 3];
            let _ = s.read_exact(&mut g).await;
            let _ = s.write_all(&[0x05, 0x00]).await;
            // Don't need to read the (rejected) request.
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        let long = "a".repeat(256);
        let err = socks5_handshake(&mut c, &long, 80).await.unwrap_err();
        assert!(err.to_string().contains("too long"), "got: {err}");
        let _ = server.await;
    }
}
