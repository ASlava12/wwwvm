//! The smoltcp-based host stack that owns the gateway IP on the guest's
//! virtual LAN. It is the single ARP authority and:
//!   * answers ARP + ICMP echo (smoltcp, internally), so `ping 10.0.2.2` works;
//!   * serves DNS on UDP/53 via the [`DnsForwarder`] policy;
//!   * NATs guest TCP connections out to real host sockets (the "slirp" role).
//!
//! TCP NAT, concretely: smoltcp runs with `any_ip`, so it accepts the guest's
//! SYN to *any* destination. We sniff each initial SYN, recover the
//! destination's hostname from the DNS cache and apply the allowlist, then
//! lazily `listen` a TCP socket bound to that exact destination (before the
//! SYN is processed) and spawn a [`crate::relay`] thread that opens the real
//! host connection. From then on smoltcp terminates the guest's TCP and we
//! shuttle the payload byte stream to/from the host socket — so TLS, when the
//! guest uses it, stays end-to-end (we never decrypt).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::{Duration as SmolDuration, Instant};
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
};

use crate::device::{GuestDevice, MTU};
use crate::forwarder::DnsForwarder;

const ETH_IPV4: u16 = 0x0800;
const IP_PROTO_TCP: u8 = 6;
const TCP_SYN: u8 = 0x02;
const TCP_ACK: u8 = 0x10;
/// Per-socket TCP buffer (64 KiB each way) — generous enough for the guest's
/// window without over-producing against the small NIC RX ring.
const TCP_BUF: usize = 64 * 1024;
/// Max payload we move per direction per flow per poll, to bound work.
const PUMP_CHUNK: usize = 8 * 1024;
/// Max concurrent NATed flows. Each costs a smoltcp socket (2×64 KiB), a
/// relay thread, and a host FD, so cap it to bound resource use against a
/// guest that opens connections faster than they close.
const MAX_FLOWS: usize = 64;

/// Identifies one guest TCP flow (the guest IP is fixed, so it's omitted).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FlowKey {
    guest_port: u16,
    dst_ip: Ipv4Addr,
    dst_port: u16,
}

/// One NATed TCP connection: the smoltcp socket plus the host relay channels
/// and the single-chunk backpressure slots in each direction.
struct Flow {
    handle: SocketHandle,
    /// Guest → host. `None` once we've half-closed the host write side.
    to_host: Option<SyncSender<Vec<u8>>>,
    from_host: Receiver<Vec<u8>>,
    /// A chunk recv'd from the guest we couldn't hand to the channel yet.
    pending_to_host: Option<Vec<u8>>,
    /// A chunk from the host we couldn't fully push into the guest socket yet.
    pending_to_guest: Option<Vec<u8>>,
    /// The host reader signalled EOF/closed (channel disconnected).
    host_done: bool,
    /// We've seen the connection established (guards premature half-close).
    established: bool,
    /// Tripped on reap so the relay threads shut the host socket down.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How a permitted guest connection is opened to the outside world. The
/// default (native) uses [`crate::relay::spawn`] — a real host socket on a
/// background thread. A browser build would supply a WebSocket-to-proxy
/// connector here, and tests supply an in-memory fake; the NAT logic above
/// is identical for all three.
pub type Connect = Box<dyn FnMut(Ipv4Addr, u16) -> crate::relay::HostConn>;

/// Host stack for the guest's virtual LAN.
pub struct NatStack {
    iface: Interface,
    device: GuestDevice,
    sockets: SocketSet<'static>,
    dns: DnsForwarder,
    dns_handle: SocketHandle,
    guest_ip: Ipv4Addr,
    flows: HashMap<FlowKey, Flow>,
    connect: Connect,
}

impl NatStack {
    /// Build the stack on `gw_ip`/`gw_mac`, serving the guest at `guest_ip`.
    /// `dns` carries the (pre-resolved) name cache + allowlist. Permitted
    /// connections are opened with the native thread+socket relay.
    pub fn new(gw_ip: [u8; 4], gw_mac: [u8; 6], guest_ip: [u8; 4], dns: DnsForwarder) -> Self {
        Self::with_connect(gw_ip, gw_mac, guest_ip, dns, Box::new(crate::relay::spawn))
    }

    /// Like [`new`](Self::new) but with a custom connector — a WebSocket
    /// relay (browser) or an in-memory fake (tests).
    pub fn with_connect(
        gw_ip: [u8; 4],
        gw_mac: [u8; 6],
        guest_ip: [u8; 4],
        dns: DnsForwarder,
        connect: Connect,
    ) -> Self {
        let mut device = GuestDevice::new();
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(gw_mac)));
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(
                    IpAddress::v4(gw_ip[0], gw_ip[1], gw_ip[2], gw_ip[3]),
                    24,
                ))
                .unwrap();
        });
        // Accept (and answer for) destination IPs other than our own — this is
        // what lets the catch-all TCP listener terminate the guest's SYN to an
        // arbitrary mirror address. smoltcp's AnyIP only accepts a foreign-dst
        // packet if a route resolves that dst to one of OUR addresses, so a
        // default route via our own gateway IP makes every unicast destination
        // "locally routed" and the SYN reaches the listening socket.
        iface.set_any_ip(true);
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(gw_ip[0], gw_ip[1], gw_ip[2], gw_ip[3]))
            .expect("default route");

        let mut sockets = SocketSet::new(Vec::new());
        let udp_rx =
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8 * 1024]);
        let udp_tx =
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 8 * 1024]);
        let mut dns_sock = udp::Socket::new(udp_rx, udp_tx);
        dns_sock.bind(53).expect("bind DNS :53");
        let dns_handle = sockets.add(dns_sock);

        Self {
            iface,
            device,
            sockets,
            dns,
            dns_handle,
            guest_ip: Ipv4Addr::from(guest_ip),
            flows: HashMap::new(),
            connect,
        }
    }

    /// Feed one guest-transmitted Ethernet frame into the stack. An initial
    /// TCP SYN to a new destination is intercepted FIRST — allowlist-checked,
    /// then a listening socket is bound to the exact destination and a relay
    /// spawned — *before* the frame reaches smoltcp, so the handshake the
    /// next `poll` completes lands on a socket that's ready for it.
    pub fn push_guest_frame(&mut self, frame: Vec<u8>) {
        if let Some(key) = parse_initial_syn(&frame) {
            if !self.flows.contains_key(&key) {
                self.try_open_flow(key);
            }
        }
        self.device.push_guest_frame(frame);
    }

    fn try_open_flow(&mut self, key: FlowKey) {
        // Bound concurrent flows: each one holds a smoltcp socket (2×64 KiB)
        // + a relay thread + a host FD. A guest cycling source ports (the
        // FlowKey includes the guest port) would otherwise grow these without
        // limit. Past the cap, drop the SYN (the guest retries/backs off).
        if self.flows.len() >= MAX_FLOWS {
            eprintln!("[wwwvm net] flow cap ({MAX_FLOWS}) reached — dropping new SYN");
            return;
        }
        // Allowlist by recovered hostname; an unknown/denied destination is
        // simply not listened for (the guest's SYN goes unanswered → it gives
        // up). Deny-by-default.
        let host = match self.dns.connection_permitted(key.dst_ip, key.dst_port) {
            Some(h) => h,
            None => {
                eprintln!(
                    "[wwwvm net] denied TCP to {}:{} (not allowlisted)",
                    key.dst_ip, key.dst_port
                );
                return;
            }
        };
        eprintln!(
            "[wwwvm net] TCP open {host} ({}:{})",
            key.dst_ip, key.dst_port
        );

        let rx = tcp::SocketBuffer::new(vec![0u8; TCP_BUF]);
        let tx = tcp::SocketBuffer::new(vec![0u8; TCP_BUF]);
        let mut sock = tcp::Socket::new(rx, tx);
        // An abandoned half-open (guest SYNs then vanishes) would otherwise
        // sit in SynReceived forever; a timeout makes smoltcp abort it → it
        // reaches Closed → pump_flows reaps it. Also guards a stalled flow.
        sock.set_timeout(Some(SmolDuration::from_secs(30)));
        // Listen on the EXACT destination so smoltcp accepts only this SYN.
        let listen = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(key.dst_ip.into())),
            port: key.dst_port,
        };
        if sock.listen(listen).is_err() {
            return;
        }
        let handle = self.sockets.add(sock);
        let conn = (self.connect)(key.dst_ip, key.dst_port);
        self.flows.insert(
            key,
            Flow {
                handle,
                to_host: Some(conn.to_host),
                from_host: conn.from_host,
                pending_to_host: None,
                pending_to_guest: None,
                host_done: false,
                established: false,
                stop: conn.stop,
            },
        );
    }

    /// Advance the stack at host time `now_millis` (monotonic ms): process
    /// inbound frames, service DNS, pump every TCP flow, reap dead ones, then
    /// re-poll so replies egress this turn.
    pub fn poll(&mut self, now_millis: i64) {
        let ts = Instant::from_millis(now_millis);
        self.iface.poll(ts, &mut self.device, &mut self.sockets);
        self.service_dns();
        self.pump_flows();
        self.iface.poll(ts, &mut self.device, &mut self.sockets);
    }

    fn service_dns(&mut self) {
        let mut pending: Vec<(Vec<u8>, IpEndpoint)> = Vec::new();
        {
            let sock = self.sockets.get_mut::<udp::Socket>(self.dns_handle);
            while let Ok((data, meta)) = sock.recv() {
                pending.push((data.to_vec(), meta.endpoint));
            }
        }
        let sock = self.sockets.get_mut::<udp::Socket>(self.dns_handle);
        for (query, endpoint) in pending {
            if let Some(resp) = self.dns.respond_to_query(&query) {
                let _ = sock.send_slice(&resp, endpoint);
            }
        }
    }

    fn pump_flows(&mut self) {
        let Self { sockets, flows, .. } = self;
        let mut dead: Vec<FlowKey> = Vec::new();
        for (key, flow) in flows.iter_mut() {
            let sock = sockets.get_mut::<tcp::Socket>(flow.handle);
            pump_flow(sock, flow);
            // Reap on Closed regardless of `established`: a half-open that
            // never completed (abandoned SYN) reaches Closed via the socket
            // timeout set in try_open_flow, and must be freed too — otherwise
            // its smoltcp socket + relay thread leak forever.
            if sock.state() == tcp::State::Closed {
                dead.push(*key);
            }
        }
        for key in dead {
            if let Some(flow) = flows.remove(&key) {
                // Tell the relay threads to shut the host socket down (the
                // reader may be parked in read()); then free the smoltcp socket.
                flow.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                sockets.remove(flow.handle);
            }
        }
    }

    /// Feed one guest-transmitted frame (no SYN interception — used by tests).
    pub fn push_guest_frame_raw(&mut self, frame: Vec<u8>) {
        self.device.push_guest_frame(frame);
    }

    /// Next frame to inject into the guest's NIC RX ring, if any.
    pub fn pop_egress(&mut self) -> Option<Vec<u8>> {
        self.device.pop_egress()
    }

    /// Return a frame to the front of the egress queue (NIC RX ring was full).
    pub fn requeue_egress_front(&mut self, frame: Vec<u8>) {
        self.device.requeue_egress_front(frame);
    }

    /// Whether there are frames waiting to be injected.
    pub fn has_egress(&self) -> bool {
        self.device.has_egress()
    }

    /// Number of live NATed flows (diagnostics/tests).
    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }

    /// The guest IP this stack serves.
    pub fn guest_ip(&self) -> Ipv4Addr {
        self.guest_ip
    }

    /// Seed the DNS forwarder's name→IP cache (and allowlist gate) so the guest
    /// can resolve `name`. The browser build calls this — it resolves names
    /// host-side (the proxy / a DoH lookup) and pushes the answers in, exactly
    /// as the native `alpine_console` pre-resolves before the VM runs. Returns
    /// how many of `ips` were kept (non-routable ones are dropped).
    pub fn cache_dns(&mut self, name: &str, ips: &[Ipv4Addr]) -> usize {
        self.dns.cache_resolution(name, ips)
    }

    /// Drain the names the guest asked for that aren't cached yet but are
    /// allowlisted — the browser resolves these on the fly (DoH) and feeds the
    /// answers back via `cache_dns`. Enables a `*` (allow-all) allowlist, where
    /// nothing is pre-resolved.
    pub fn take_dns_requests(&mut self) -> Vec<String> {
        self.dns.take_pending()
    }

    /// The hostname the guest's destination `ip` resolved from, if any. The
    /// browser relay sends this (not the bare IP) to `crates/proxy`, which
    /// re-resolves + allowlists by name and pins the address — so the proxy's
    /// own deny-by-default gate keys on the same hostnames as ours.
    pub fn host_for_ip(&self, ip: Ipv4Addr) -> Option<&str> {
        self.dns.name_for_ip(ip)
    }
}

/// Move bytes both ways between the guest's smoltcp socket and the host relay,
/// respecting backpressure (one buffered chunk per direction) and propagating
/// half-closes.
fn pump_flow(sock: &mut tcp::Socket, flow: &mut Flow) {
    if sock.may_send() || sock.may_recv() {
        flow.established = true;
    }

    // Host → guest.
    loop {
        if flow.pending_to_guest.is_none() {
            match flow.from_host.try_recv() {
                Ok(buf) => flow.pending_to_guest = Some(buf),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    flow.host_done = true;
                    break;
                }
            }
        }
        let Some(buf) = flow.pending_to_guest.take() else {
            break;
        };
        if !sock.can_send() {
            flow.pending_to_guest = Some(buf);
            break;
        }
        match sock.send_slice(&buf) {
            Ok(n) if n < buf.len() => {
                flow.pending_to_guest = Some(buf[n..].to_vec());
                break;
            }
            Ok(_) => {} // fully sent — loop for more
            Err(_) => break,
        }
    }
    // Host closed and we've flushed everything → send FIN to the guest.
    if flow.host_done && flow.pending_to_guest.is_none() && sock.may_send() {
        sock.close();
    }

    // Guest → host.
    let mut moved = 0;
    loop {
        if let Some(buf) = flow.pending_to_host.take() {
            match flow.to_host.as_ref().map(|tx| tx.try_send(buf)) {
                Some(Ok(())) => {} // delivered — loop
                Some(Err(TrySendError::Full(b))) => {
                    flow.pending_to_host = Some(b); // channel full — backpressure
                    break;
                }
                Some(Err(TrySendError::Disconnected(_))) | None => break, // host gone
            }
        } else if sock.can_recv() && moved < PUMP_CHUNK {
            let mut tmp = vec![0u8; 4096];
            match sock.recv_slice(&mut tmp) {
                Ok(n) if n > 0 => {
                    tmp.truncate(n);
                    moved += n;
                    flow.pending_to_host = Some(tmp);
                }
                _ => break,
            }
        } else {
            break;
        }
    }
    // Guest closed its write side (we got its FIN) and we've fully drained
    // it — half-close the host write side by dropping the sender. The
    // `!can_recv()` guard matters: with >PUMP_CHUNK bytes still buffered the
    // loop above exits early (cap reached) with pending_to_host empty but the
    // socket still readable; half-closing then would TRUNCATE the upload.
    // Deferring until the buffer is drained lets the next poll finish it.
    if matches!(sock.state(), tcp::State::CloseWait)
        && flow.pending_to_host.is_none()
        && !sock.can_recv()
    {
        flow.to_host = None;
    }
}

/// If `frame` is an initial TCP SYN (SYN set, ACK clear), return its flow key.
fn parse_initial_syn(frame: &[u8]) -> Option<FlowKey> {
    if frame.len() < 14 + 20 || u16::from_be_bytes([frame[12], frame[13]]) != ETH_IPV4 {
        return None;
    }
    let ip = 14;
    let ihl = (frame[ip] & 0x0F) as usize * 4;
    if ihl < 20 || frame.len() < ip + ihl + 20 || frame[ip + 9] != IP_PROTO_TCP {
        return None;
    }
    let tcp_off = ip + ihl;
    let flags = frame[tcp_off + 13];
    if flags & TCP_SYN == 0 || flags & TCP_ACK != 0 {
        return None; // not an initial SYN
    }
    let src_port = u16::from_be_bytes([frame[tcp_off], frame[tcp_off + 1]]);
    let dst_port = u16::from_be_bytes([frame[tcp_off + 2], frame[tcp_off + 3]]);
    let dst_ip = Ipv4Addr::new(
        frame[ip + 16],
        frame[ip + 17],
        frame[ip + 18],
        frame[ip + 19],
    );
    Some(FlowKey {
        guest_port: src_port,
        dst_ip,
        dst_port,
    })
}

/// The link MTU smoltcp negotiates for this device.
pub const LINK_MTU: usize = MTU;

#[cfg(test)]
mod tests {
    use super::*;

    fn syn_frame(dst_ip: [u8; 4], dst_port: u16, src_port: u16, ack: bool) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&[0x52, 0x54, 0x00, 0x00, 0x00, 0x02]); // dst MAC (gw)
        f.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // src MAC (guest)
        f.extend_from_slice(&ETH_IPV4.to_be_bytes());
        // IPv4 header (20 bytes).
        f.extend_from_slice(&[0x45, 0x00, 0x00, 0x28, 0, 0, 0, 0, 64, IP_PROTO_TCP, 0, 0]);
        f.extend_from_slice(&[10, 0, 2, 15]); // src IP (guest)
        f.extend_from_slice(&dst_ip);
        // TCP header (20 bytes).
        f.extend_from_slice(&src_port.to_be_bytes());
        f.extend_from_slice(&dst_port.to_be_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]); // seq
        f.extend_from_slice(&[0, 0, 0, 0]); // ack
        let flags = if ack { TCP_SYN | TCP_ACK } else { TCP_SYN };
        f.extend_from_slice(&[0x50, flags, 0xFF, 0xFF, 0, 0, 0, 0]); // dataoff/flags/win/cksum/urg
        f
    }

    #[test]
    fn parses_initial_syn() {
        let f = syn_frame([93, 184, 216, 34], 80, 50000, false);
        let key = parse_initial_syn(&f).expect("a SYN");
        assert_eq!(key.dst_ip, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(key.dst_port, 80);
        assert_eq!(key.guest_port, 50000);
    }

    #[test]
    fn ignores_syn_ack_and_non_tcp() {
        // SYN+ACK is not an *initial* SYN (that's the reply, never from guest).
        assert!(parse_initial_syn(&syn_frame([1, 2, 3, 4], 80, 1, true)).is_none());
        // A short/ARP-ish frame.
        assert!(parse_initial_syn(&[0u8; 30]).is_none());
    }

    // --- TCP connect gate: deny-by-default at SYN, headless ---

    use crate::relay::HostConn;
    use crate::Allowlist;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::sync_channel;

    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const PUBLIC_IP: [u8; 4] = [93, 184, 216, 34]; // example.com, globally routable

    /// A connector that records each (ip, port) it's asked to open and hands
    /// back dummy (undriven) channels — so a test can see whether the NAT
    /// decided to connect, without any real socket.
    fn recording_connect(log: Rc<RefCell<Vec<(Ipv4Addr, u16)>>>) -> Connect {
        Box::new(move |ip, port| {
            log.borrow_mut().push((ip, port));
            let (to_host, _rx) = sync_channel(1);
            let (_tx, from_host) = sync_channel(1);
            HostConn {
                to_host,
                from_host,
                stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        })
    }

    fn dns_with(allow: &str, name: &str, ip: [u8; 4]) -> DnsForwarder {
        let mut d = DnsForwarder::new([10, 0, 2, 2], GW_MAC, Allowlist::parse(allow));
        d.cache_resolution(name, &[Ipv4Addr::from(ip)]);
        d
    }

    fn nat_with(dns: DnsForwarder, log: Rc<RefCell<Vec<(Ipv4Addr, u16)>>>) -> NatStack {
        NatStack::with_connect(
            [10, 0, 2, 2],
            GW_MAC,
            [10, 0, 2, 15],
            dns,
            recording_connect(log),
        )
    }

    #[test]
    fn syn_to_allowlisted_host_opens_a_flow() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let dns = dns_with(
            "dl-cdn.alpinelinux.org:80",
            "dl-cdn.alpinelinux.org",
            PUBLIC_IP,
        );
        let mut nat = nat_with(dns, log.clone());
        nat.push_guest_frame(syn_frame(PUBLIC_IP, 80, 50000, false));
        assert_eq!(nat.flow_count(), 1, "flow opened");
        assert_eq!(
            *log.borrow(),
            vec![(Ipv4Addr::from(PUBLIC_IP), 80)],
            "connected once"
        );
    }

    #[test]
    fn syn_to_non_allowlisted_port_is_denied() {
        // Name allowlisted on :80; a SYN to :443 must be refused (no flow, no
        // connect) — the per-port check at connect time.
        let log = Rc::new(RefCell::new(Vec::new()));
        let dns = dns_with(
            "dl-cdn.alpinelinux.org:80",
            "dl-cdn.alpinelinux.org",
            PUBLIC_IP,
        );
        let mut nat = nat_with(dns, log.clone());
        nat.push_guest_frame(syn_frame(PUBLIC_IP, 443, 50000, false));
        assert_eq!(nat.flow_count(), 0, "denied → no flow");
        assert!(log.borrow().is_empty(), "connector never invoked");
    }

    #[test]
    fn syn_to_unresolved_ip_is_denied() {
        // A SYN straight to an IP our DNS never vended (literal-IP escape) is
        // denied even though the allowlist names a host — closes the bypass.
        let log = Rc::new(RefCell::new(Vec::new()));
        let dns = dns_with(
            "dl-cdn.alpinelinux.org:80",
            "dl-cdn.alpinelinux.org",
            PUBLIC_IP,
        );
        let mut nat = nat_with(dns, log.clone());
        nat.push_guest_frame(syn_frame([8, 8, 8, 8], 80, 50000, false));
        assert_eq!(nat.flow_count(), 0, "unknown IP → no flow");
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn empty_allowlist_denies_all_tcp() {
        let log = Rc::new(RefCell::new(Vec::new()));
        // Empty allowlist → cache_resolution keeps nothing → every SYN denied.
        let dns = dns_with("", "dl-cdn.alpinelinux.org", PUBLIC_IP);
        let mut nat = nat_with(dns, log.clone());
        nat.push_guest_frame(syn_frame(PUBLIC_IP, 80, 50000, false));
        assert_eq!(nat.flow_count(), 0);
        assert!(log.borrow().is_empty());
    }

    // --- Hybrid LAN + NAT: the NAT shares an L2 segment with peer VMs (the web
    // "lan+nat" mode). The worker routes peer-bound frames to the switch and
    // gateway/broadcast frames to the NAT. The safety property the routing
    // relies on: the NAT answers ARP for ITS gateway IP only — it must ignore
    // ARP for a peer IP, or it would hijack that peer's traffic on the segment.

    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

    fn arp_request(target_ip: [u8; 4]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&[0xff; 6]); // dst = broadcast
        f.extend_from_slice(&GUEST_MAC); // src = guest
        f.extend_from_slice(&[0x08, 0x06]); // ethertype = ARP
        f.extend_from_slice(&[0x00, 0x01]); // htype = Ethernet
        f.extend_from_slice(&ETH_IPV4.to_be_bytes()); // ptype = IPv4
        f.extend_from_slice(&[6, 4]); // hlen, plen
        f.extend_from_slice(&[0x00, 0x01]); // opcode = request
        f.extend_from_slice(&GUEST_MAC); // sender HW
        f.extend_from_slice(&[10, 0, 2, 15]); // sender proto (guest)
        f.extend_from_slice(&[0; 6]); // target HW (unknown)
        f.extend_from_slice(&target_ip); // target proto
        f
    }

    fn drain_egress(nat: &mut NatStack) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        while let Some(f) = nat.pop_egress() {
            v.push(f);
        }
        v
    }

    // ARP reply (op 2) whose sender protocol address is `sender_ip`.
    fn is_arp_reply_for(f: &[u8], sender_ip: [u8; 4]) -> bool {
        f.len() >= 42
            && f[12] == 0x08
            && f[13] == 0x06
            && f[20] == 0
            && f[21] == 2
            && f[28..32] == sender_ip
    }

    #[test]
    fn answers_gateway_arp_but_ignores_peer_arp() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let dns = DnsForwarder::new([10, 0, 2, 2], GW_MAC, Allowlist::parse(""));
        let mut nat = nat_with(dns, log);

        // ARP "who has 10.0.2.2" (the gateway) → the NAT owns it → reply with
        // the gateway MAC, so the guest can route off-subnet through it.
        nat.push_guest_frame(arp_request([10, 0, 2, 2]));
        nat.poll(0);
        let egress = drain_egress(&mut nat);
        let reply = egress
            .iter()
            .find(|f| is_arp_reply_for(f, [10, 0, 2, 2]))
            .expect("NAT must answer ARP for its gateway IP");
        assert_eq!(
            &reply[22..28],
            &GW_MAC,
            "ARP reply sender MAC = gateway MAC"
        );

        // ARP "who has 10.0.2.16" (a peer VM on the same switch) → NOT the
        // gateway → the NAT must stay silent; the switch delivers it to the real
        // peer. Answering would blackhole that peer's traffic.
        nat.push_guest_frame(arp_request([10, 0, 2, 16]));
        nat.poll(10);
        let egress = drain_egress(&mut nat);
        assert!(
            !egress.iter().any(|f| is_arp_reply_for(f, [10, 0, 2, 16])),
            "NAT must ignore ARP for peer IPs (no hijack on the shared LAN)"
        );
    }
}
