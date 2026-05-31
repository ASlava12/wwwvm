//! In-gateway DNS forwarder (frame level).
//!
//! Answers the guest's DNS queries to the gateway IP from a cache that the
//! host pre-resolves at startup (one `getaddrinfo` per allowlisted host,
//! *before* the VM runs — so we never block the single-threaded VM loop on a
//! slow resolver, and there is no DNS-rebinding window: we only ever vend IPs
//! we resolved ourselves for allowlisted names). The cache doubles as the
//! IP→name map the TCP NAT will use to apply the allowlist by hostname.
//!
//! The DNS *message* parsing/building lives in [`crate::dns`]; this module
//! adds the UDP/IPv4/Ethernet framing (reusing [`crate::util::internet_checksum`])
//! and the allowlist gate. It is pure frame-in → frame-out, so it unit-tests
//! against hand-built packets. (When the bridge moves onto smoltcp this
//! framing is replaced by a smoltcp UDP socket; the cache + codec stay.)

use std::collections::HashMap;
use std::net::Ipv4Addr;

use crate::dns::{self, DnsAnswer, QTYPE_A, QTYPE_AAAA};
use crate::util::internet_checksum;
use crate::Allowlist;

const ETH_IPV4: [u8; 2] = [0x08, 0x00];
const IP_PROTO_UDP: u8 = 17;
const DNS_PORT: u16 = 53;

/// The bits we pull out of a guest DNS-query frame to build the reply.
struct DnsQueryFrame<'a> {
    guest_mac: [u8; 6],
    guest_ip: [u8; 4],
    src_port: u16,
    payload: &'a [u8],
}

pub struct DnsForwarder {
    gw_ip: [u8; 4],
    gw_mac: [u8; 6],
    allow: Allowlist,
    /// name → resolved IPv4 addresses (pre-resolved at startup).
    by_name: HashMap<String, Vec<Ipv4Addr>>,
    /// resolved IP → name, for the TCP NAT's hostname allowlist check.
    by_ip: HashMap<Ipv4Addr, String>,
}

impl DnsForwarder {
    pub fn new(gw_ip: [u8; 4], gw_mac: [u8; 6], allow: Allowlist) -> Self {
        Self {
            gw_ip,
            gw_mac,
            allow,
            by_name: HashMap::new(),
            by_ip: HashMap::new(),
        }
    }

    /// Record a pre-resolved name→addrs mapping. Non-allowlisted names and
    /// non-global addresses (loopback/private/link-local/multicast/our own
    /// subnet) are dropped — defence against a name that resolves somewhere
    /// it shouldn't (SSRF). Returns how many addresses were accepted.
    pub fn cache_resolution(&mut self, name: &str, addrs: &[Ipv4Addr]) -> usize {
        if !self.allow.permits_host(name) {
            return 0;
        }
        let lc = name.to_ascii_lowercase();
        let kept: Vec<Ipv4Addr> = addrs
            .iter()
            .copied()
            .filter(|ip| self.is_routable(*ip))
            .collect();
        for ip in &kept {
            self.by_ip.insert(*ip, lc.clone());
        }
        if !kept.is_empty() {
            self.by_name.entry(lc).or_default().extend(kept.iter());
        }
        kept.len()
    }

    /// The hostname we resolved `ip` to, if any — the TCP NAT recovers the
    /// name here to apply the allowlist (a SYN to an unknown IP is denied).
    pub fn name_for_ip(&self, ip: Ipv4Addr) -> Option<&str> {
        self.by_ip.get(&ip).map(String::as_str)
    }

    /// Decide whether the guest may open a TCP connection to `ip:port`:
    /// recover the hostname we resolved that IP to, then apply the allowlist
    /// by name+port. Returns the host name on success (for logging), or None
    /// if the IP was never resolved by us or the host:port isn't allowed —
    /// closing both the rebinding/SSRF and the literal-IP escape paths.
    pub fn connection_permitted(&self, ip: Ipv4Addr, port: u16) -> Option<String> {
        let name = self.by_ip.get(&ip)?;
        if self.allow.permits(name, port) {
            Some(name.clone())
        } else {
            None
        }
    }

    /// Reject non-globally-routable destinations so an allowlisted name that
    /// (mis)resolves to an internal address can't be used to reach the host
    /// or its LAN. Delegates to the shared policy (also excludes CGNAT /
    /// reserved ranges) plus our own gateway IP.
    fn is_routable(&self, ip: Ipv4Addr) -> bool {
        crate::allow::is_globally_routable(ip) && ip.octets() != self.gw_ip
    }

    /// The pure DNS policy: given a query *payload*, return the response
    /// *payload* (or None if it isn't a parseable query). A-records come from
    /// the cache (NXDOMAIN if the name isn't cached/allowlisted), AAAA gets an
    /// empty NOERROR (v4-only LAN), and ANY/TXT/etc. are refused (NXDOMAIN) so
    /// the forwarder can't be an exfil channel. Used both by the hand-rolled
    /// framing below and by the smoltcp UDP socket.
    pub fn respond_to_query(&self, payload: &[u8]) -> Option<Vec<u8>> {
        let query = dns::parse_query(payload)?;
        let answer = if query.qtype == QTYPE_AAAA {
            DnsAnswer::Addrs(Vec::new())
        } else if query.qtype == QTYPE_A {
            match self.by_name.get(&query.name) {
                Some(addrs) if !addrs.is_empty() => DnsAnswer::Addrs(addrs.clone()),
                _ => DnsAnswer::NxDomain,
            }
        } else {
            DnsAnswer::NxDomain
        };
        Some(dns::build_response(&query, answer))
    }

    /// If `frame` is a DNS query to the gateway, return the response frame to
    /// inject; otherwise None. (Hand-rolled UDP/IP/Ethernet framing — used
    /// before the bridge moves onto smoltcp, which does the framing itself.)
    pub fn handle_frame(&self, frame: &[u8]) -> Option<Vec<u8>> {
        let q = self.parse_dns_query_frame(frame)?;
        let dns_resp = self.respond_to_query(q.payload)?;
        Some(self.build_udp_frame(q.guest_mac, q.guest_ip, q.src_port, &dns_resp))
    }

    /// Parse an Ethernet/IPv4/UDP frame addressed to the gateway's DNS port.
    fn parse_dns_query_frame<'a>(&self, frame: &'a [u8]) -> Option<DnsQueryFrame<'a>> {
        if frame.len() < 14 + 20 + 8 || frame[12..14] != ETH_IPV4 {
            return None;
        }
        let ip = 14;
        let ihl = (frame[ip] & 0x0F) as usize * 4;
        if ihl < 20 || frame.len() < ip + ihl + 8 {
            return None;
        }
        if frame[ip + 9] != IP_PROTO_UDP || frame[ip + 16..ip + 20] != self.gw_ip {
            return None;
        }
        let udp = ip + ihl;
        let src_port = u16::from_be_bytes([frame[udp], frame[udp + 1]]);
        let dst_port = u16::from_be_bytes([frame[udp + 2], frame[udp + 3]]);
        if dst_port != DNS_PORT {
            return None;
        }
        let udp_len = u16::from_be_bytes([frame[udp + 4], frame[udp + 5]]) as usize;
        // UDP length covers the 8-byte header + payload; clamp to the frame.
        if udp_len < 8 || udp + udp_len > frame.len() {
            return None;
        }
        let payload = &frame[udp + 8..udp + udp_len];

        let mut guest_mac = [0u8; 6];
        guest_mac.copy_from_slice(&frame[6..12]);
        let mut guest_ip = [0u8; 4];
        guest_ip.copy_from_slice(&frame[ip + 12..ip + 16]);
        Some(DnsQueryFrame {
            guest_mac,
            guest_ip,
            src_port,
            payload,
        })
    }

    /// Wrap `dns_resp` in UDP/IPv4/Ethernet headers back to the guest. UDP
    /// checksum is 0 (legal per RFC 768; Linux accepts it); the IP header
    /// checksum is computed.
    fn build_udp_frame(
        &self,
        guest_mac: [u8; 6],
        guest_ip: [u8; 4],
        guest_port: u16,
        dns_resp: &[u8],
    ) -> Vec<u8> {
        let udp_len = 8 + dns_resp.len();
        let total_len = 20 + udp_len;
        let mut f = Vec::with_capacity(14 + total_len);

        // Ethernet.
        f.extend_from_slice(&guest_mac); // dst = the guest
        f.extend_from_slice(&self.gw_mac); // src = gateway
        f.extend_from_slice(&ETH_IPV4);

        // IPv4 header (20 bytes).
        let ip_start = f.len();
        f.extend_from_slice(&[0x45, 0x00]); // v4, IHL 5, DSCP 0
        f.extend_from_slice(&(total_len as u16).to_be_bytes());
        f.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // id, flags/frag
        f.extend_from_slice(&[64, IP_PROTO_UDP, 0, 0]); // TTL, proto, checksum (0)
        f.extend_from_slice(&self.gw_ip); // src = gateway
        f.extend_from_slice(&guest_ip); // dst = the guest
        let ck = internet_checksum(&f[ip_start..ip_start + 20]);
        f[ip_start + 10..ip_start + 12].copy_from_slice(&ck.to_be_bytes());

        // UDP header + payload (checksum 0).
        f.extend_from_slice(&DNS_PORT.to_be_bytes()); // src port 53
        f.extend_from_slice(&guest_port.to_be_bytes()); // dst = guest's source port
        f.extend_from_slice(&(udp_len as u16).to_be_bytes());
        f.extend_from_slice(&[0x00, 0x00]); // checksum 0
        f.extend_from_slice(dns_resp);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GW_IP: [u8; 4] = [10, 0, 2, 2];
    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

    fn fwd() -> DnsForwarder {
        DnsForwarder::new(
            GW_IP,
            GW_MAC,
            Allowlist::parse("dl-cdn.alpinelinux.org:80,dl-cdn.alpinelinux.org:443"),
        )
    }

    fn dns_query_payload(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&0x0100u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            p.push(label.len() as u8);
            p.extend_from_slice(label.as_bytes());
        }
        p.push(0);
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes()); // IN
        p
    }

    fn query_frame(dst_ip: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let total = 20 + udp_len;
        let mut f = Vec::new();
        f.extend_from_slice(&GW_MAC); // to the gateway
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ETH_IPV4);
        f.extend_from_slice(&[0x45, 0x00]);
        f.extend_from_slice(&(total as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0, 0, 0, 64, IP_PROTO_UDP, 0, 0]);
        f.extend_from_slice(&GUEST_IP);
        f.extend_from_slice(&dst_ip);
        f.extend_from_slice(&40000u16.to_be_bytes()); // guest src port
        f.extend_from_slice(&DNS_PORT.to_be_bytes());
        f.extend_from_slice(&(udp_len as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn cache_resolution_honours_allowlist_and_routability() {
        let mut f = fwd();
        let public = Ipv4Addr::new(151, 101, 2, 132);
        assert_eq!(f.cache_resolution("dl-cdn.alpinelinux.org", &[public]), 1);
        assert_eq!(f.name_for_ip(public), Some("dl-cdn.alpinelinux.org"));
        // Not allowlisted → rejected.
        assert_eq!(f.cache_resolution("evil.example.com", &[public]), 0);
        // Allowlisted but private/loopback → dropped (SSRF guard).
        assert_eq!(
            f.cache_resolution(
                "dl-cdn.alpinelinux.org",
                &[Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 5)]
            ),
            0
        );
    }

    #[test]
    fn a_query_for_cached_name_gets_the_address() {
        let mut f = fwd();
        let ip = Ipv4Addr::new(151, 101, 2, 132);
        f.cache_resolution("dl-cdn.alpinelinux.org", &[ip]);
        let frame = query_frame(
            GW_IP,
            &dns_query_payload(0x1111, "dl-cdn.alpinelinux.org", QTYPE_A),
        );
        let resp = f.handle_frame(&frame).expect("dns reply");
        // Ethernet + IP endpoints swapped back to the guest.
        assert_eq!(&resp[0..6], &GUEST_MAC);
        assert_eq!(&resp[6..12], &GW_MAC);
        // Walk to the DNS payload: eth(14) + ip(20) + udp(8).
        let dns = &resp[42..];
        assert_eq!(u16::from_be_bytes([dns[0], dns[1]]), 0x1111, "id echoed");
        assert_eq!(u16::from_be_bytes([dns[6], dns[7]]), 1, "one answer");
        // Answer RDATA = the cached IP (last 4 bytes).
        assert_eq!(&resp[resp.len() - 4..], &ip.octets());
        // IP header checksum valid.
        assert_eq!(internet_checksum(&resp[14..34]), 0);
        // UDP dst port = the guest's source port (40000).
        assert_eq!(u16::from_be_bytes([resp[36], resp[37]]), 40000);
    }

    #[test]
    fn uncached_name_gets_nxdomain() {
        let f = fwd();
        let frame = query_frame(GW_IP, &dns_query_payload(2, "not-cached.example", QTYPE_A));
        let resp = f.handle_frame(&frame).expect("reply");
        let dns = &resp[42..];
        assert_eq!(u16::from_be_bytes([dns[2], dns[3]]) & 0x000F, 3, "NXDOMAIN");
    }

    #[test]
    fn aaaa_query_gets_empty_noerror() {
        let mut f = fwd();
        f.cache_resolution("dl-cdn.alpinelinux.org", &[Ipv4Addr::new(151, 101, 2, 132)]);
        let frame = query_frame(
            GW_IP,
            &dns_query_payload(3, "dl-cdn.alpinelinux.org", QTYPE_AAAA),
        );
        let resp = f.handle_frame(&frame).expect("reply");
        let dns = &resp[42..];
        assert_eq!(u16::from_be_bytes([dns[2], dns[3]]) & 0x000F, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([dns[6], dns[7]]), 0, "no answers");
    }

    #[test]
    fn non_dns_and_wrong_dst_are_ignored() {
        let f = fwd();
        // UDP to a different IP → not ours.
        let frame = query_frame([8, 8, 8, 8], &dns_query_payload(1, "x.com", QTYPE_A));
        assert!(f.handle_frame(&frame).is_none());
        // An ARP-sized non-IP frame.
        assert!(f.handle_frame(&[0u8; 30]).is_none());
    }
}
