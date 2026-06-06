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
//! For a publicly-reachable bind, resource limits guard against abuse:
//! `WWWVM_PROXY_MAX_CONNS` (global concurrent, default 512),
//! `WWWVM_PROXY_MAX_CONNS_PER_IP` (per peer IP, default 32),
//! `WWWVM_PROXY_IDLE_TIMEOUT_SECS` (close idle tunnels, default off), and
//! `WWWVM_PROXY_MAX_BYTES` (cap bytes/connection, default off). A `0` disables
//! the corresponding limit. Combined with a SPECIFIC allowlist (so the relay
//! can only reach the hosts your demo needs — e.g. apk mirrors) this is safe to
//! expose; `*` (open relay) is not.
//!
//! Run (specific host, bound to loopback — the safe default):
//!     WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80' \
//!     WWWVM_PROXY_ORIGINS='http://localhost:8080' \
//!       cargo run -p wwwvm-proxy -- 127.0.0.1:8080

#![forbid(unsafe_code)]

mod upstream;

use std::collections::HashMap;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
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

/// Resource limits for public hosting (abuse mitigation). Concurrency caps are
/// ON by default; the byte and idle caps are opt-in (`0` = off) because they
/// can curtail legitimate long transfers, so the operator turns them on for an
/// exposed deployment.
struct Limits {
    /// Max concurrent connections across all clients (0 = unlimited).
    max_conns: usize,
    /// Max concurrent connections per peer IP (0 = unlimited).
    max_per_ip: usize,
    /// Close a connection idle (no bytes either way) this long (0 = off).
    idle: Duration,
    /// Cap total bytes relayed per connection, both directions summed (0 = off).
    max_bytes: u64,
}

impl Limits {
    fn from_env() -> Self {
        let g = |k: &str, d: u64| -> u64 {
            env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(d)
        };
        Limits {
            max_conns: g("WWWVM_PROXY_MAX_CONNS", 512) as usize,
            max_per_ip: g("WWWVM_PROXY_MAX_CONNS_PER_IP", 32) as usize,
            idle: Duration::from_secs(g("WWWVM_PROXY_IDLE_TIMEOUT_SECS", 0)),
            max_bytes: g("WWWVM_PROXY_MAX_BYTES", 0),
        }
    }
}

/// Concurrency gate: a global semaphore + per-IP counters. `try_acquire` hands
/// back a guard held for the connection's lifetime, or `None` when a cap is
/// hit — so an over-limit client is rejected immediately rather than queued
/// (queuing under a flood is itself a resource sink).
struct Gate {
    sem: Option<Arc<Semaphore>>, // None = unlimited global
    per_ip: StdMutex<HashMap<IpAddr, usize>>,
    max_per_ip: usize,
}

impl Gate {
    fn new(l: &Limits) -> Arc<Self> {
        Arc::new(Gate {
            sem: (l.max_conns > 0).then(|| Arc::new(Semaphore::new(l.max_conns))),
            per_ip: StdMutex::new(HashMap::new()),
            max_per_ip: l.max_per_ip,
        })
    }

    fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnGuard> {
        // Take the global slot first; if the per-IP cap then rejects, the permit
        // is dropped on the early return and the global slot is freed again.
        let permit = match &self.sem {
            Some(s) => Some(s.clone().try_acquire_owned().ok()?),
            None => None,
        };
        {
            let mut map = self.per_ip.lock().unwrap();
            let n = map.entry(ip).or_insert(0);
            if self.max_per_ip > 0 && *n >= self.max_per_ip {
                return None;
            }
            *n += 1;
        }
        Some(ConnGuard {
            _permit: permit,
            gate: self.clone(),
            ip,
        })
    }
}

/// Held for a connection's lifetime; releasing it (Drop) frees the global
/// semaphore permit and decrements the per-IP counter.
struct ConnGuard {
    _permit: Option<OwnedSemaphorePermit>,
    gate: Arc<Gate>,
    ip: IpAddr,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut map = self.gate.per_ip.lock().unwrap();
        if let Some(n) = map.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// Refuse to start as an open relay (`*` allowlist) on a public (non-loopback)
/// bind unless explicitly overridden — a forgotten `*` on a reachable port is
/// the worst footgun. Pure decision so it's unit-tested.
fn refuse_open_relay(allows_anything: bool, is_loopback: bool, override_set: bool) -> bool {
    allows_anything && !is_loopback && !override_set
}

/// Build a rustls server config from PEM cert + key files (for serving `wss://`
/// directly, so an https-hosted page doesn't need a separate TLS reverse proxy).
/// Uses the `ring` crypto provider explicitly so we don't depend on a process
/// default being installed.
fn load_tls(cert_path: &str, key_path: &str) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::{crypto::ring, ServerConfig};

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(
        File::open(cert_path).with_context(|| format!("open TLS cert {cert_path}"))?,
    ))
    .collect::<std::result::Result<_, _>>()
    .with_context(|| format!("parse TLS cert {cert_path}"))?;
    if certs.is_empty() {
        bail!("no certificates found in {cert_path}");
    }
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(
        File::open(key_path).with_context(|| format!("open TLS key {key_path}"))?,
    ))
    .with_context(|| format!("parse TLS key {key_path}"))?
    .ok_or_else(|| anyhow!("no private key found in {key_path}"))?;

    let cfg = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("TLS cert/key")?;
    Ok(Arc::new(cfg))
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
    if allow.is_empty() {
        log::warn!(
            "WWWVM_PROXY_ALLOWLIST is empty — every connect attempt will be rejected. \
             Set it to e.g. `*` (any) or `example.com:443,localhost:*` (specific)."
        );
    }
    // An open relay (`*`) is dangerous; on a non-loopback bind it lets ANY
    // reachable client tunnel TCP to ANY host through this process. Refuse to
    // start in that configuration unless the operator explicitly opts in — a
    // forgotten `*` on a public bind is the single biggest footgun here.
    if allow.allows_anything() {
        let public = !bind.ip().is_loopback();
        let override_set = env::var_os("WWWVM_PROXY_I_REALLY_WANT_AN_OPEN_RELAY").is_some();
        if refuse_open_relay(true, bind.ip().is_loopback(), override_set) {
            bail!(
                "refusing to start: WWWVM_PROXY_ALLOWLIST permits `*` (OPEN RELAY) on a \
                 non-loopback bind ({bind}). Anyone who can reach this port could tunnel TCP \
                 to any host through you. Use a SPECIFIC allowlist (e.g. \
                 dl-cdn.alpinelinux.org:443) and/or bind to 127.0.0.1. To override (you \
                 understand the risk), set WWWVM_PROXY_I_REALLY_WANT_AN_OPEN_RELAY=1."
            );
        }
        log::warn!(
            "WWWVM_PROXY_ALLOWLIST permits `*` (ANY host:port) — this is an OPEN RELAY.{}",
            if public {
                " Override is set — running an open relay on a public bind. You own the risk."
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

    // Resource limits (abuse mitigation for a publicly-reachable bind). The
    // connection caps are on by default; `0` means a given limit is off.
    let limits = Arc::new(Limits::from_env());
    let gate = Gate::new(&limits);
    let off = |n: u64| {
        if n == 0 {
            "off".to_string()
        } else {
            n.to_string()
        }
    };
    log::info!(
        "limits: max_conns={}, per_ip={}, idle_timeout={}, max_bytes/conn={}",
        off(limits.max_conns as u64),
        off(limits.max_per_ip as u64),
        if limits.idle.is_zero() {
            "off".into()
        } else {
            format!("{}s", limits.idle.as_secs())
        },
        off(limits.max_bytes),
    );

    // Optional TLS so we can serve wss:// directly (an https-hosted page can't
    // open ws://). Both cert+key must be set together; neither → plain ws.
    let tls: Option<TlsAcceptor> = match (
        env::var("WWWVM_PROXY_TLS_CERT")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        env::var("WWWVM_PROXY_TLS_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    ) {
        (Some(cert), Some(key)) => {
            let cfg = load_tls(&cert, &key)?;
            log::info!("TLS enabled — serving wss:// (cert {cert})");
            Some(TlsAcceptor::from(cfg))
        }
        (None, None) => {
            log::info!(
                "TLS disabled — plain ws:// (set WWWVM_PROXY_TLS_CERT + WWWVM_PROXY_TLS_KEY \
                 to serve wss:// for an https page)"
            );
            None
        }
        _ => bail!("WWWVM_PROXY_TLS_CERT and WWWVM_PROXY_TLS_KEY must be set together"),
    };

    let listener = TcpListener::bind(bind).await.context("bind")?;
    log::info!("wwwvm-proxy listening on {bind}");

    loop {
        let (stream, peer) = listener.accept().await?;
        // Reject immediately (and drop the socket) when a concurrency cap is hit.
        let guard = match gate.try_acquire(peer.ip()) {
            Some(g) => g,
            None => {
                log::warn!("{peer}: rejected — connection limit reached");
                continue;
            }
        };
        let allow = allow.clone();
        let origins = origins.clone();
        let upstreams_file = upstreams_file.clone();
        let limits = limits.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _guard = guard; // released (caps freed) when the connection ends
                                // TLS handshake (if enabled) runs in the task so a slow/failing one
                                // can't stall the accept loop. Then the same handler runs over
                                // either the plain TCP stream or the TLS stream.
            let res = match tls {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        handle(tls_stream, peer, allow, origins, upstreams_file, limits).await
                    }
                    Err(e) => Err(anyhow!("TLS handshake failed: {e}")),
                },
                None => handle(stream, peer, allow, origins, upstreams_file, limits).await,
            };
            if let Err(e) = res {
                log::warn!("{peer}: {e:#}");
            }
        });
    }
}

// The tungstenite handshake callback returns Result<Response, ErrorResponse>,
// and ErrorResponse (an http::Response) is large — that's the library's
// signature, not ours, so silence the size lint here.
#[allow(clippy::result_large_err)]
async fn handle<S>(
    stream: S,
    peer: SocketAddr,
    allow: Arc<Allowlist>,
    origins: Arc<Option<Vec<String>>>,
    upstreams_file: Arc<Option<PathBuf>>,
    limits: Arc<Limits>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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

    // Shared across both directions for the resource caps: `counted` = total
    // bytes relayed (byte cap), `last_ms` = ms-since-`start` of the most recent
    // transfer (idle timeout). `Instant` is Copy; the atomics let both tasks
    // update without a lock.
    let counted = Arc::new(AtomicU64::new(0));
    let last_ms = Arc::new(AtomicU64::new(0));
    let start = std::time::Instant::now();
    let max_bytes = limits.max_bytes;

    // ws -> tcp (guest → upstream). Log the first chunk + total so a stalled
    // relay is diagnosable (did the guest's request reach the server at all?).
    let ws_to_tcp = {
        let counted = counted.clone();
        let last_ms = last_ms.clone();
        async move {
            let mut total = 0u64;
            while let Some(msg) = ws_stream.next().await {
                match msg? {
                    Message::Binary(b) => {
                        if total == 0 {
                            log::info!("{peer} ws→tcp: first {} bytes", b.len());
                        }
                        total += b.len() as u64;
                        tcp_wr.write_all(&b).await?;
                        last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                        let c =
                            counted.fetch_add(b.len() as u64, Ordering::Relaxed) + b.len() as u64;
                        if max_bytes > 0 && c > max_bytes {
                            return Err(anyhow!("byte cap ({max_bytes}) exceeded"));
                        }
                    }
                    // A text frame is a control signal from our client (which only
                    // ever sends binary data frames otherwise). "FIN" = the guest
                    // half-closed its write side: shut down the upstream write side
                    // and stop reading this direction, but DON'T close the socket —
                    // the tcp→ws task keeps delivering the response.
                    Message::Text(t) if t == HALF_CLOSE => break,
                    Message::Text(t) => {
                        total += t.len() as u64;
                        tcp_wr.write_all(t.as_bytes()).await?;
                        last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            log::info!("{peer} ws→tcp: done, {total} bytes total");
            let _ = tcp_wr.shutdown().await;
            Ok::<_, anyhow::Error>(())
        }
    };

    // tcp -> ws (upstream → guest).
    let tcp_to_ws = {
        let counted = counted.clone();
        let last_ms = last_ms.clone();
        async move {
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
                last_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                let c = counted.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                if max_bytes > 0 && c > max_bytes {
                    return Err(anyhow!("byte cap ({max_bytes}) exceeded"));
                }
            }
            log::info!("{peer} tcp→ws: done, {total} bytes total");
            let _ = ws_sink.send(Message::Close(None)).await;
            Ok::<_, anyhow::Error>(())
        }
    };

    let relay = async {
        let (a, b) = tokio::join!(ws_to_tcp, tcp_to_ws);
        a?;
        b?;
        Ok::<(), anyhow::Error>(())
    };

    // Idle-timeout watchdog (opt-in). Closes the connection if no bytes flow in
    // EITHER direction for `idle` — bounds stuck/abandoned tunnels without
    // killing an active long transfer, since any transfer refreshes `last_ms`.
    // On fire, dropping `relay` tears down both halves (and the sockets).
    if !limits.idle.is_zero() {
        let idle = limits.idle;
        let watchdog = async move {
            let tick = (idle / 2).max(Duration::from_secs(1));
            loop {
                tokio::time::sleep(tick).await;
                let idle_ms = start.elapsed().as_millis() as u64 - last_ms.load(Ordering::Relaxed);
                if idle_ms >= idle.as_millis() as u64 {
                    break;
                }
            }
        };
        tokio::select! {
            r = relay => r?,
            _ = watchdog => log::info!("{peer}: idle {}s — closing", idle.as_secs()),
        }
    } else {
        relay.await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_relay_refused_only_on_public_bind_without_override() {
        assert!(refuse_open_relay(true, false, false)); // `*` + public + no override → refuse
        assert!(!refuse_open_relay(true, false, true)); // override set → allow
        assert!(!refuse_open_relay(true, true, false)); // loopback → allow (warn only)
        assert!(!refuse_open_relay(false, false, false)); // specific allowlist → allow
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The gate must enforce BOTH the per-IP and the global cap, free a global
    /// slot when a per-IP rejection happens (no leak), and release everything on
    /// guard drop so a client that disconnects gets its budget back.
    #[test]
    fn gate_enforces_caps_and_releases_on_drop() {
        let limits = Limits {
            max_conns: 3,
            max_per_ip: 2,
            idle: Duration::ZERO,
            max_bytes: 0,
        };
        let gate = Gate::new(&limits);
        let (a, b) = (ip("1.1.1.1"), ip("2.2.2.2"));

        let g1 = gate.try_acquire(a).expect("a #1");
        let g2 = gate.try_acquire(a).expect("a #2");
        // a is at its per-IP cap (2); a third from a is refused — and this must
        // NOT consume a global slot (else b below couldn't get two).
        assert!(gate.try_acquire(a).is_none(), "per-IP cap reached for a");

        let g3 = gate.try_acquire(b).expect("b #1 (global slot 3/3)");
        // Global cap (3) now reached: even a fresh IP is refused.
        assert!(gate.try_acquire(b).is_none(), "global cap reached");

        // Dropping one of a's frees a global slot *and* a's per-IP count.
        drop(g2);
        let g4 = gate.try_acquire(a).expect("a reacquires after release");

        drop((g1, g3, g4));
        // Everything released → acquirable again, and the per-IP map is cleaned.
        assert!(gate.try_acquire(a).is_some());
        assert!(
            gate.per_ip.lock().unwrap().get(&b).is_none(),
            "b entry removed"
        );
    }

    /// `load_tls` parses a valid PEM cert+key into a usable ServerConfig and
    /// reports clear errors for a missing file and a file with no certificate.
    #[test]
    fn load_tls_accepts_valid_pem_and_rejects_bad() {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cp = dir.join(format!("wwwvm-test-cert-{pid}.pem"));
        let kp = dir.join(format!("wwwvm-test-key-{pid}.pem"));
        let empty = dir.join(format!("wwwvm-test-empty-{pid}.pem"));
        std::fs::write(&cp, ck.cert.pem()).unwrap();
        std::fs::write(&kp, ck.key_pair.serialize_pem()).unwrap();
        std::fs::write(&empty, b"not a pem").unwrap();

        let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
        // Happy path: valid cert + key build a ServerConfig.
        assert!(load_tls(&s(&cp), &s(&kp)).is_ok(), "valid PEM should load");
        // Missing cert file → error.
        assert!(load_tls("/no/such/cert.pem", &s(&kp)).is_err());
        // A cert file with no certificate in it → clear error.
        let err = load_tls(&s(&empty), &s(&kp)).unwrap_err();
        assert!(err.to_string().contains("no certificates"), "got: {err}");

        let _ = std::fs::remove_file(&cp);
        let _ = std::fs::remove_file(&kp);
        let _ = std::fs::remove_file(&empty);
    }

    /// `max_conns = 0` means an unbounded global pool (only per-IP applies).
    #[test]
    fn zero_max_conns_is_unlimited_global() {
        let limits = Limits {
            max_conns: 0,
            max_per_ip: 0,
            idle: Duration::ZERO,
            max_bytes: 0,
        };
        let gate = Gate::new(&limits);
        let a = ip("9.9.9.9");
        let guards: Vec<_> = (0..1000).filter_map(|_| gate.try_acquire(a)).collect();
        assert_eq!(guards.len(), 1000, "no global or per-IP cap");
    }
}
