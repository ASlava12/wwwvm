//! A thread-free, socket-free connector for the TCP NAT — the browser's
//! drop-in replacement for the native [`crate::relay::spawn`] relay.
//!
//! The native relay backs each guest TCP flow with a background thread holding
//! a real [`std::net::TcpStream`]. Neither threads nor sockets work in a wasm
//! browser build, so instead the embedder (the JS side of the wasm demo)
//! tunnels each flow over a WebSocket to `crates/proxy`. This connector
//! terminates the NAT's side of every flow in the same pair of in-memory
//! channels a [`crate::relay::HostConn`] uses, and hands the embedder the OTHER
//! ends as a per-connection byte queue:
//!
//!   * [`QueueConnector::take_new`] — connections the guest just opened; the
//!     embedder opens one WebSocket per id (sending `{host,port}` to the proxy);
//!   * [`QueueConnector::drain_outbound`] — guest→host bytes to `ws.send`, plus
//!     a "closed" flag (the NAT closed/reaped the flow → close the socket);
//!   * [`QueueConnector::push_inbound`] — the WebSocket's replies, fed back as
//!     host→guest bytes (returns `false` under backpressure → the embedder
//!     re-queues and retries);
//!   * [`QueueConnector::host_closed`] — the WebSocket closed/errored → the
//!     guest gets a FIN.
//!
//! The NAT ([`crate::nat`]) does the same non-blocking `try_send`/`try_recv` it
//! does for the native relay — the `HostConn` it sees is identical — so the NAT
//! logic is unchanged across native, browser, and tests.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use crate::nat::Connect;
use crate::relay::HostConn;

/// Guest→host channel depth (chunks). Matches the native relay so the same
/// end-to-end TCP flow control applies (a full channel back-pressures the
/// guest by leaving bytes in its smoltcp socket).
const OUT_DEPTH: usize = 16;
/// Host→guest channel depth (chunks). A little deeper than [`OUT_DEPTH`]: the
/// embedder pushes whole WebSocket messages here and a full queue makes
/// [`QueueConnector::push_inbound`] return `false`, forcing a JS-side re-queue.
const IN_DEPTH: usize = 64;

/// One embedder-driven connection: the ends of the NAT's two channels that the
/// host (WebSocket) side pumps.
struct QueueConn {
    dst_ip: Ipv4Addr,
    dst_port: u16,
    /// Drains the NAT's `to_host` sender: guest→host payload to ship out.
    out_rx: Receiver<Vec<u8>>,
    /// Feeds the NAT's `from_host` receiver: host→guest payload received.
    in_tx: SyncSender<Vec<u8>>,
    /// Tripped on [`host_closed`](QueueConnector::host_closed); also the flag
    /// the native relay's reaper sets. Kept so the `HostConn` contract matches.
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct QueueState {
    next_id: u64,
    new: Vec<u64>,
    conns: HashMap<u64, QueueConn>,
}

/// Shareable driver handle. Pass [`connector`](Self::connector) to
/// [`crate::nat::NatStack::with_connect`]; keep the `QueueConnector` itself to
/// pump the connections. Cloning shares the same underlying state (single
/// thread only — this is `!Send`, which is exactly right for wasm).
#[derive(Clone, Default)]
pub struct QueueConnector {
    state: Rc<RefCell<QueueState>>,
}

impl QueueConnector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The connector closure to hand [`crate::nat::NatStack::with_connect`].
    /// The NAT invokes it once per admitted guest SYN; each call registers a
    /// new connection and returns the NAT's `HostConn`.
    pub fn connector(&self) -> Connect {
        let state = self.state.clone();
        Box::new(move |ip, port| {
            let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(OUT_DEPTH);
            let (in_tx, in_rx) = mpsc::sync_channel::<Vec<u8>>(IN_DEPTH);
            let stop = Arc::new(AtomicBool::new(false));
            let mut s = state.borrow_mut();
            let id = s.next_id;
            s.next_id += 1;
            s.new.push(id);
            s.conns.insert(
                id,
                QueueConn {
                    dst_ip: ip,
                    dst_port: port,
                    out_rx,
                    in_tx,
                    stop: stop.clone(),
                },
            );
            HostConn {
                to_host: out_tx,
                from_host: in_rx,
                stop,
            }
        })
    }

    /// Connections requested since the last call, each as `(id, dst_ip,
    /// dst_port)`. The embedder opens one WebSocket per id.
    pub fn take_new(&self) -> Vec<(u64, Ipv4Addr, u16)> {
        let mut s = self.state.borrow_mut();
        let ids = std::mem::take(&mut s.new);
        ids.into_iter()
            .filter_map(|id| s.conns.get(&id).map(|c| (id, c.dst_ip, c.dst_port)))
            .collect()
    }

    /// Guest→host bytes queued on `id` (concatenated; empty if none yet), and
    /// whether the NAT has truly closed/**reaped** the flow. `closed == true`
    /// (also for an unknown id) means the embedder should close the WebSocket
    /// and call [`host_closed`](Self::host_closed) to free the slot.
    ///
    /// `closed` keys on the reap flag (`stop`), NOT on `out_rx` disconnecting:
    /// the NAT drops `to_host` on a guest **write** half-close (FIN) while the
    /// host→guest direction stays alive (this mirrors the native relay, where
    /// dropping `to_host` ends only the writer thread — relay.rs). Treating a
    /// half-close as a full close would tear the WebSocket down and truncate
    /// the response the guest is still waiting to read.
    pub fn drain_outbound(&self, id: u64) -> (Vec<u8>, bool) {
        let s = self.state.borrow();
        let Some(conn) = s.conns.get(&id) else {
            return (Vec::new(), true);
        };
        let mut out = Vec::new();
        while let Ok(mut b) = conn.out_rx.try_recv() {
            out.append(&mut b);
        }
        (out, conn.stop.load(Ordering::Relaxed))
    }

    /// Feed host→guest bytes received on the WebSocket for `id`. Returns
    /// `false` if the per-connection queue is full (back-pressure — re-queue
    /// the bytes and retry next tick) or the connection is gone.
    pub fn push_inbound(&self, id: u64, bytes: &[u8]) -> bool {
        let s = self.state.borrow();
        let Some(conn) = s.conns.get(&id) else {
            return false;
        };
        match conn.in_tx.try_send(bytes.to_vec()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// The WebSocket for `id` closed or errored. Drops the connection's
    /// channels — the NAT sees `from_host` disconnect and sends the guest a
    /// FIN — and frees the slot. Idempotent.
    pub fn host_closed(&self, id: u64) {
        let mut s = self.state.borrow_mut();
        if let Some(conn) = s.conns.remove(&id) {
            conn.stop.store(true, Ordering::Relaxed);
            // Dropping `conn` here drops in_tx (→ guest FIN) and out_rx.
        }
    }

    /// Number of live connections (diagnostics/tests).
    pub fn conn_count(&self) -> usize {
        self.state.borrow().conns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nat::NatStack;
    use crate::{Allowlist, DnsForwarder};
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::tcp;
    use smoltcp::time::Instant;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GW_IP: [u8; 4] = [10, 0, 2, 2];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
    const HOST_IP: [u8; 4] = [93, 184, 216, 34]; // "example.com", allowlisted

    /// The connector's channel plumbing in isolation: simulate exactly what the
    /// NAT does to a `HostConn` (try_send guest bytes, try_recv host bytes) and
    /// confirm the embedder-facing queue mirrors it.
    #[test]
    fn queue_connector_round_trips_a_connection() {
        let qc = QueueConnector::new();
        let mut connect = qc.connector();

        // The NAT admits a SYN → invokes the connector.
        let conn = connect(Ipv4Addr::from(HOST_IP), 80);
        assert_eq!(qc.conn_count(), 1);
        assert_eq!(
            qc.take_new(),
            vec![(0, Ipv4Addr::from(HOST_IP), 80)],
            "embedder sees the new connection once"
        );
        assert!(qc.take_new().is_empty(), "drained");

        // Guest → host: the NAT sends payload into to_host; the embedder drains it.
        conn.to_host
            .try_send(b"GET / HTTP/1.0\r\n".to_vec())
            .unwrap();
        let (out, closed) = qc.drain_outbound(0);
        assert_eq!(out, b"GET / HTTP/1.0\r\n");
        assert!(!closed, "still open");

        // Host → guest: the embedder pushes the WebSocket reply; the NAT reads it.
        assert!(qc.push_inbound(0, b"HTTP/1.0 200 OK\r\n"));
        assert_eq!(conn.from_host.try_recv().unwrap(), b"HTTP/1.0 200 OK\r\n");

        // Guest WRITE half-close: the NAT drops `to_host` (only). The flow is
        // NOT reaped, so the embedder must keep the WebSocket open — `closed`
        // stays false and host→guest still flows.
        let HostConn {
            to_host,
            from_host,
            stop,
        } = conn;
        drop(to_host); // simulate the NAT's `flow.to_host = None` on CloseWait
        let (out, closed) = qc.drain_outbound(0);
        assert!(out.is_empty());
        assert!(!closed, "guest write half-close must NOT read as closed");
        assert!(qc.push_inbound(0, b"body"), "host→guest still open");
        assert_eq!(from_host.try_recv().unwrap(), b"body");

        // The NAT reaping the flow trips `stop` → now it reads as closed.
        stop.store(true, Ordering::Relaxed);
        assert!(qc.drain_outbound(0).1, "reaped flow reads as closed");

        // The embedder tears the slot down.
        qc.host_closed(0);
        assert_eq!(qc.conn_count(), 0);
        assert!(qc.drain_outbound(0).1, "unknown id reads as closed");
    }

    /// Full loopback: a real guest-side smoltcp TCP client connects THROUGH the
    /// NAT (handshake + data both ways via smoltcp), the queue connector is the
    /// "host", and an echo proves the guest↔host byte path works end-to-end —
    /// the same data path the browser drives over a WebSocket.
    #[test]
    fn guest_tcp_through_queue_nat_echoes() {
        // NAT side, driven by the queue connector.
        let qc = QueueConnector::new();
        let mut dns = DnsForwarder::new(GW_IP, GW_MAC, Allowlist::parse("example.com:80"));
        dns.cache_resolution("example.com", &[Ipv4Addr::from(HOST_IP)]);
        let mut nat = NatStack::with_connect(GW_IP, GW_MAC, GUEST_IP, dns, qc.connector());

        // Guest side: a smoltcp client with the guest MAC/IP and a default route
        // via the gateway, connecting out to the (allowlisted) host IP:80.
        let mut cdev = crate::device::GuestDevice::new();
        let ccfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
        let mut ciface = Interface::new(ccfg, &mut cdev, Instant::from_millis(0));
        ciface.update_ip_addrs(|a| {
            a.push(IpCidr::new(
                IpAddress::v4(GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], GUEST_IP[3]),
                24,
            ))
            .unwrap();
        });
        ciface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(GW_IP[0], GW_IP[1], GW_IP[2], GW_IP[3]))
            .unwrap();
        let mut csockets = SocketSet::new(Vec::new());
        let rx = tcp::SocketBuffer::new(vec![0u8; 8192]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 8192]);
        let chandle = csockets.add(tcp::Socket::new(rx, tx));
        {
            let csock = csockets.get_mut::<tcp::Socket>(chandle);
            csock
                .connect(
                    ciface.context(),
                    (
                        IpAddress::v4(HOST_IP[0], HOST_IP[1], HOST_IP[2], HOST_IP[3]),
                        80,
                    ),
                    49152,
                )
                .expect("client connect");
        }

        // Open connections the queue surfaces; echo any guest→host bytes back.
        let mut open: Vec<u64> = Vec::new();
        let mut got = Vec::<u8>::new();
        let mut sent_request = false;
        let mut t_ms: i64 = 0;
        for _ in 0..400 {
            let t = Instant::from_millis(t_ms);
            // Client stack → frames → NAT.
            ciface.poll(t, &mut cdev, &mut csockets);
            while let Some(f) = cdev.pop_egress() {
                nat.push_guest_frame(f);
            }
            // NAT advances; its egress → client stack.
            nat.poll(t_ms);
            while let Some(f) = nat.pop_egress() {
                cdev.push_guest_frame(f);
            }

            // Drive the "host": surface new conns, echo outbound → inbound.
            for (id, _, _) in qc.take_new() {
                open.push(id);
            }
            for &id in &open {
                let (bytes, _closed) = qc.drain_outbound(id);
                if !bytes.is_empty() {
                    assert!(qc.push_inbound(id, &bytes), "echo accepted");
                }
            }

            // Once connected, send a request; later, collect the echo.
            let csock = csockets.get_mut::<tcp::Socket>(chandle);
            if csock.can_send() && !sent_request {
                csock.send_slice(b"PING").expect("send");
                sent_request = true;
            }
            if csock.can_recv() {
                let _ = csock.recv(|buf| {
                    got.extend_from_slice(buf);
                    (buf.len(), ())
                });
            }
            if got == b"PING" {
                break;
            }
            t_ms += 10;
        }
        assert_eq!(
            got, b"PING",
            "guest received the host's echo through the NAT"
        );
        assert_eq!(qc.conn_count(), 1, "flow stayed open across the exchange");
    }
}
