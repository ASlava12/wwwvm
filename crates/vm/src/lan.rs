//! A tiny host-side "virtual gateway" that answers the guest NIC's L2/L3
//! traffic so the guest believes it is on a real LAN — the foundation of
//! the user-mode networking bridge (the slirp role: the guest runs its own
//! TCP/IP, we terminate L2/L3 on the host and, later, NAT L4 out to real
//! sockets).
//!
//! Right now it handles the two protocols a freshly-configured interface
//! exercises before any TCP flows:
//!   * **ARP** — answers "who-has GW_IP" with the gateway MAC so the guest
//!     can resolve its default route (and so we can reach it).
//!   * **ICMP echo** — replies to `ping GW_IP`, which is the cleanest
//!     end-to-end check that inbound delivery (RX ring + IRQ) and the
//!     guest's IP stack are both healthy.
//!
//! Everything here is pure frame-in → frames-out logic (no sockets), so it
//! unit-tests against hand-built packets and the checksum maths is pinned
//! down before it ever has to satisfy a real kernel. The VM feeds guest
//! transmits in via [`VirtualGateway::handle_frame`] and injects whatever
//! frames come back into the RX ring.

/// EtherType for IPv4.
const ETH_IPV4: [u8; 2] = [0x08, 0x00];
/// EtherType for ARP.
const ETH_ARP: [u8; 2] = [0x08, 0x06];
const IP_PROTO_ICMP: u8 = 1;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// The host end of the guest's virtual LAN: a single gateway host at
/// `gw_ip` / `gw_mac` that the guest can ARP for and ping.
pub struct VirtualGateway {
    gw_ip: [u8; 4],
    gw_mac: [u8; 6],
}

impl VirtualGateway {
    pub fn new(gw_ip: [u8; 4], gw_mac: [u8; 6]) -> Self {
        Self { gw_ip, gw_mac }
    }

    /// Process one Ethernet frame the guest transmitted and return the
    /// frames to inject back (usually zero or one). Unknown / unrelated
    /// traffic yields nothing.
    pub fn handle_frame(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        if let Some(reply) = self.arp_reply(frame) {
            return vec![reply];
        }
        if let Some(reply) = self.icmp_echo_reply(frame) {
            return vec![reply];
        }
        Vec::new()
    }

    /// Build an ARP reply for a guest "who-has gw_ip" request, or None if
    /// `frame` isn't an ARP request targeting us. ARP carries no checksum.
    fn arp_reply(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 42 || frame[12..14] != ETH_ARP {
            return None;
        }
        // ARP: htype(2) ptype(2) hlen(1) plen(1) op(2) sha(6) spa(4) tha(6) tpa(4)
        let op = &frame[20..22];
        let target_ip = &frame[38..42];
        if op != [0x00, 0x01] || target_ip != self.gw_ip {
            return None; // not a request, or not for our IP
        }
        let sender_mac = &frame[6..12]; // Ethernet source = the guest
        let sender_ip = &frame[28..32]; // ARP sender protocol address

        let mut r = Vec::with_capacity(42);
        r.extend_from_slice(sender_mac); // Ethernet dst = the guest
        r.extend_from_slice(&self.gw_mac); // Ethernet src = us
        r.extend_from_slice(&ETH_ARP);
        r.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x02]); // hw/proto, op=reply
        r.extend_from_slice(&self.gw_mac); // sender hw = us
        r.extend_from_slice(&self.gw_ip); // sender proto = gw_ip
        r.extend_from_slice(sender_mac); // target hw = the guest
        r.extend_from_slice(sender_ip); // target proto = the guest
        Some(r)
    }

    /// Build an ICMP echo reply for a `ping gw_ip`, or None if `frame`
    /// isn't an ICMP echo request addressed to us. Swaps the L2/L3
    /// endpoints, flips the ICMP type, and recomputes both checksums.
    fn icmp_echo_reply(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 14 + 20 || frame[12..14] != ETH_IPV4 {
            return None;
        }
        let ip = 14; // start of the IP header
        let ihl = (frame[ip] & 0x0F) as usize * 4;
        if ihl < 20 || frame.len() < ip + ihl {
            return None;
        }
        if frame[ip + 9] != IP_PROTO_ICMP || frame[ip + 16..ip + 20] != self.gw_ip {
            return None; // not ICMP, or not to our IP
        }
        let icmp = ip + ihl;
        if frame.len() < icmp + 8 || frame[icmp] != ICMP_ECHO_REQUEST {
            return None;
        }

        // Reply is the request with endpoints swapped and type flipped.
        let mut r = frame.to_vec();
        r[0..6].copy_from_slice(&frame[6..12]); // Ethernet dst = the guest
        r[6..12].copy_from_slice(&self.gw_mac); // Ethernet src = us

        // IP: swap src/dst, refresh TTL, recompute the header checksum.
        let src = frame[ip + 12..ip + 16].to_vec();
        let dst = frame[ip + 16..ip + 20].to_vec();
        r[ip + 12..ip + 16].copy_from_slice(&dst); // new src = old dst (us)
        r[ip + 16..ip + 20].copy_from_slice(&src); // new dst = old src (guest)
        r[ip + 8] = 64; // TTL
        r[ip + 10] = 0; // zero checksum field before recompute
        r[ip + 11] = 0;
        let ip_ck = ip_checksum(&r[ip..ip + ihl]);
        r[ip + 10..ip + 12].copy_from_slice(&ip_ck.to_be_bytes());
        // ICMP: echo reply, recompute checksum over the whole ICMP message.
        r[icmp] = ICMP_ECHO_REPLY;
        r[icmp + 2] = 0;
        r[icmp + 3] = 0;
        let icmp_ck = ip_checksum(&r[icmp..]);
        r[icmp + 2..icmp + 4].copy_from_slice(&icmp_ck.to_be_bytes());
        Some(r)
    }
}

/// The internet checksum (RFC 1071): one's-complement sum of 16-bit
/// big-endian words, folded and inverted. Used for both the IPv4 header
/// and the ICMP message. A correct packet's checksum, recomputed with the
/// field included, sums to zero.
fn ip_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8; // odd trailing byte is the high half
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GW_IP: [u8; 4] = [10, 0, 2, 2];
    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

    fn arp_request(target_ip: [u8; 4]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&[0xFF; 6]); // dst broadcast
        f.extend_from_slice(&GUEST_MAC); // src guest
        f.extend_from_slice(&ETH_ARP);
        f.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01]); // op request
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&GUEST_IP);
        f.extend_from_slice(&[0x00; 6]);
        f.extend_from_slice(&target_ip);
        f
    }

    #[test]
    fn arp_request_for_gateway_gets_reply() {
        let mut gw = VirtualGateway::new(GW_IP, GW_MAC);
        let replies = gw.handle_frame(&arp_request(GW_IP));
        assert_eq!(replies.len(), 1);
        let r = &replies[0];
        assert_eq!(&r[0..6], &GUEST_MAC, "Ethernet dst = the asker");
        assert_eq!(&r[6..12], &GW_MAC, "Ethernet src = us");
        assert_eq!(&r[12..14], &ETH_ARP);
        assert_eq!(&r[20..22], &[0x00, 0x02], "ARP op = reply");
        assert_eq!(&r[22..28], &GW_MAC, "sender hw = gateway MAC");
        assert_eq!(&r[28..32], &GW_IP, "sender proto = gateway IP");
        assert_eq!(&r[32..38], &GUEST_MAC, "target hw = the guest");
        assert_eq!(&r[38..42], &GUEST_IP, "target proto = the guest");
    }

    #[test]
    fn arp_for_other_ip_is_ignored() {
        let mut gw = VirtualGateway::new(GW_IP, GW_MAC);
        assert!(gw.handle_frame(&arp_request([10, 0, 2, 99])).is_empty());
    }

    /// Build an ICMP echo request from the guest to `dst` with a small
    /// payload, with valid IP + ICMP checksums.
    fn icmp_echo_request(dst: [u8; 4], ident: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut icmp = vec![ICMP_ECHO_REQUEST, 0, 0, 0];
        icmp.extend_from_slice(&ident.to_be_bytes());
        icmp.extend_from_slice(&seq.to_be_bytes());
        icmp.extend_from_slice(payload);
        let ck = ip_checksum(&icmp);
        icmp[2..4].copy_from_slice(&ck.to_be_bytes());

        let total = 20 + icmp.len();
        let mut iph = vec![
            0x45,
            0x00, // version 4, IHL 5, DSCP 0
            (total >> 8) as u8,
            total as u8, // total length
            0x00,
            0x00,
            0x00,
            0x00, // id, flags/frag
            64,
            IP_PROTO_ICMP,
            0,
            0, // TTL, proto, checksum (filled below)
        ];
        iph.extend_from_slice(&GUEST_IP);
        iph.extend_from_slice(&dst);
        let ipck = ip_checksum(&iph);
        iph[10..12].copy_from_slice(&ipck.to_be_bytes());

        let mut f = Vec::new();
        f.extend_from_slice(&GW_MAC); // dst = gateway (resolved via ARP)
        f.extend_from_slice(&GUEST_MAC); // src = guest
        f.extend_from_slice(&ETH_IPV4);
        f.extend_from_slice(&iph);
        f.extend_from_slice(&icmp);
        f
    }

    #[test]
    fn icmp_echo_to_gateway_is_answered_with_valid_checksums() {
        let mut gw = VirtualGateway::new(GW_IP, GW_MAC);
        let payload = b"wwwvm-ping-payload";
        let req = icmp_echo_request(GW_IP, 0xABCD, 7, payload);
        let replies = gw.handle_frame(&req);
        assert_eq!(replies.len(), 1);
        let r = &replies[0];

        // L2/L3 endpoints swapped.
        assert_eq!(&r[0..6], &GUEST_MAC);
        assert_eq!(&r[6..12], &GW_MAC);
        assert_eq!(&r[26..30], &GW_IP, "IP src = gateway");
        assert_eq!(&r[30..34], &GUEST_IP, "IP dst = guest");

        // ICMP became an echo reply, identifier/seq/payload preserved.
        assert_eq!(r[34], ICMP_ECHO_REPLY);
        assert_eq!(&r[38..40], &0xABCDu16.to_be_bytes(), "identifier kept");
        assert_eq!(&r[40..42], &7u16.to_be_bytes(), "sequence kept");
        assert_eq!(&r[42..42 + payload.len()], payload, "payload echoed");

        // Both checksums verify (a valid packet sums to zero).
        assert_eq!(ip_checksum(&r[14..34]), 0, "IP header checksum valid");
        assert_eq!(ip_checksum(&r[34..]), 0, "ICMP checksum valid");
    }

    #[test]
    fn icmp_echo_to_other_ip_is_ignored() {
        let mut gw = VirtualGateway::new(GW_IP, GW_MAC);
        let req = icmp_echo_request([10, 0, 2, 99], 1, 1, b"x");
        assert!(gw.handle_frame(&req).is_empty());
    }

    #[test]
    fn checksum_known_vector() {
        // Classic RFC 1071 worked example header.
        let hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(ip_checksum(&hdr), 0xb861);
    }

    #[test]
    fn unrelated_frame_yields_nothing() {
        let mut gw = VirtualGateway::new(GW_IP, GW_MAC);
        assert!(gw.handle_frame(&[0u8; 14]).is_empty());
        assert!(gw.handle_frame(b"not a frame").is_empty());
    }
}
