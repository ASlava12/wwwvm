//! The smoltcp-based host stack that owns the gateway IP on the guest's
//! virtual LAN. It answers ARP and ICMP (smoltcp handles those internally
//! for its own address) and serves DNS on UDP/53 via the [`DnsForwarder`]
//! policy. TCP NAT (catch-all SYN → real host socket) lands on top of this
//! in the next step.
//!
//! Being the single owner of the gateway IP is deliberate: smoltcp must be
//! the sole ARP authority, so this replaces the hand-rolled `VirtualGateway`
//! responders once wired in (ping/nslookup are re-confirmed as regressions).

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use crate::device::{GuestDevice, MTU};
use crate::forwarder::DnsForwarder;

/// Host stack for the guest's virtual LAN.
pub struct NatStack {
    iface: Interface,
    device: GuestDevice,
    sockets: SocketSet<'static>,
    dns: DnsForwarder,
    dns_handle: smoltcp::iface::SocketHandle,
}

impl NatStack {
    /// Build the stack on `gw_ip`/`gw_mac`, serving the guest at `guest_ip`.
    /// `dns` carries the (pre-resolved) name cache + allowlist.
    pub fn new(gw_ip: [u8; 4], gw_mac: [u8; 6], guest_ip: [u8; 4], dns: DnsForwarder) -> Self {
        let _ = guest_ip; // reserved for the neighbour-cache / route wiring in the TCP step
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
        // Accept packets addressed to IPs other than our own — required for
        // the upcoming TCP catch-all NAT; harmless for ARP/ICMP/DNS.
        iface.set_any_ip(true);

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
        }
    }

    /// Feed one guest-transmitted Ethernet frame into the stack.
    pub fn push_guest_frame(&mut self, frame: Vec<u8>) {
        self.device.push_guest_frame(frame);
    }

    /// Advance the stack at host time `now_millis` (monotonic ms). Processes
    /// inbound frames (ARP/ICMP/DNS), services the DNS socket, and lets
    /// smoltcp emit any replies into the egress queue.
    pub fn poll(&mut self, now_millis: i64) {
        let ts = Instant::from_millis(now_millis);
        self.iface.poll(ts, &mut self.device, &mut self.sockets);
        self.service_dns();
        // Re-poll so freshly-queued DNS replies egress in this turn.
        self.iface.poll(ts, &mut self.device, &mut self.sockets);
    }

    fn service_dns(&mut self) {
        // Collect pending queries first — copying them out releases the
        // socket's read borrow before we send replies on the same socket.
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
}

/// The link MTU smoltcp negotiates for this device.
pub const LINK_MTU: usize = MTU;
