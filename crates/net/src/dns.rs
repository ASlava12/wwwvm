//! Minimal DNS message codec for the in-gateway forwarder.
//!
//! The guest's musl resolver sends standard DNS queries (typically an A and
//! an AAAA in parallel) to the gateway IP; we parse the question, resolve
//! the name on the host (allowlist-gated, done by the caller), and build a
//! response. This module is *pure* message-in → message-out over the DNS
//! payload (the UDP data); the UDP/IP framing and the actual host
//! resolution live in the bridge. Keeping it pure means the wire format —
//! the part that must satisfy a real resolver — is fully unit-tested.
//!
//! Defensive per the design review: bounded QNAME parsing with a
//! compression-pointer-loop guard, single-question only, and the answer
//! echoes the question verbatim (musl matches the response question).

use std::net::Ipv4Addr;

/// DNS record types we care about.
pub const QTYPE_A: u16 = 1;
pub const QTYPE_AAAA: u16 = 28;
const QCLASS_IN: u16 = 1;
const MAX_NAME_LEN: usize = 255;

/// A parsed single-question query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsQuery {
    pub id: u16,
    /// Recursion-Desired bit, echoed back in the response.
    pub recursion_desired: bool,
    /// The queried name, lowercased, dot-separated, no trailing dot.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// The exact QNAME+QTYPE+QCLASS bytes, echoed verbatim in the answer.
    question_wire: Vec<u8>,
}

/// What the caller decided the answer should be.
pub enum DnsAnswer {
    /// A-record answer (NOERROR). Empty vec ⇒ NOERROR with no answers
    /// (use this for AAAA on our v4-only LAN).
    Addrs(Vec<Ipv4Addr>),
    /// Name does not exist (NXDOMAIN) — also our "not allowlisted" reply.
    NxDomain,
}

/// Parse a DNS query payload. Returns `None` for anything that isn't a
/// well-formed single-question query (responses, QDCOUNT≠1, truncated
/// names, compression loops) — the caller simply drops those and the
/// resolver retries.
pub fn parse_query(payload: &[u8]) -> Option<DnsQuery> {
    if payload.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([payload[0], payload[1]]);
    let flags = u16::from_be_bytes([payload[2], payload[3]]);
    if flags & 0x8000 != 0 {
        return None; // QR=1 → a response, not a query
    }
    let opcode = (flags >> 11) & 0xF;
    if opcode != 0 {
        return None; // only standard QUERY
    }
    let qdcount = u16::from_be_bytes([payload[4], payload[5]]);
    if qdcount != 1 {
        return None; // exactly one question, or we don't touch it
    }

    let (name, after_name) = parse_name(payload, 12)?;
    // QTYPE + QCLASS follow the (uncompressed) question name.
    if after_name + 4 > payload.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([payload[after_name], payload[after_name + 1]]);
    let qclass = u16::from_be_bytes([payload[after_name + 2], payload[after_name + 3]]);
    let question_wire = payload[12..after_name + 4].to_vec();

    Some(DnsQuery {
        id,
        recursion_desired: flags & 0x0100 != 0,
        name,
        qtype,
        qclass,
        question_wire,
    })
}

/// Parse a QNAME starting at `start`, returning (name, offset just past the
/// in-line name). Questions never legally use compression, but we follow
/// pointers defensively with a jump cap so a hostile/looping packet can't
/// hang us. The returned offset is always the position right after the
/// in-line bytes of the FIRST name (so QTYPE/QCLASS read correctly).
fn parse_name(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut pos = start;
    let mut after: Option<usize> = None; // first offset past the inline name
    let mut jumps = 0;
    let mut total = 0;
    loop {
        let len = *buf.get(pos)? as usize;
        if len & 0xC0 == 0xC0 {
            // Compression pointer (2 bytes).
            let b2 = *buf.get(pos + 1)? as usize;
            if after.is_none() {
                after = Some(pos + 2);
            }
            jumps += 1;
            if jumps > 16 {
                return None; // pointer loop / abuse
            }
            pos = ((len & 0x3F) << 8) | b2;
            continue;
        }
        if len & 0xC0 != 0 {
            return None; // reserved label type
        }
        if len == 0 {
            if after.is_none() {
                after = Some(pos + 1);
            }
            break; // root label terminates the name
        }
        let s = pos + 1;
        let e = s + len;
        if e > buf.len() {
            return None;
        }
        total += len + 1;
        if total > MAX_NAME_LEN {
            return None;
        }
        let label = std::str::from_utf8(&buf[s..e]).ok()?.to_ascii_lowercase();
        labels.push(label);
        pos = e;
    }
    Some((labels.join("."), after?))
}

/// Build a response for `query` carrying `answer`. The header sets QR/AA/RA,
/// echoes the RD bit and the question verbatim, and (for A answers) appends
/// one RR per address using a compression pointer back to the question name.
pub fn build_response(query: &DnsQuery, answer: DnsAnswer) -> Vec<u8> {
    let (rcode, addrs): (u16, &[Ipv4Addr]) = match &answer {
        DnsAnswer::Addrs(a) => (0, a),
        DnsAnswer::NxDomain => (3, &[]),
    };
    let ancount = addrs.len() as u16;

    let mut flags: u16 = 0x8000 | 0x0400 | 0x0080; // QR=1, AA=1, RA=1
    if query.recursion_desired {
        flags |= 0x0100; // echo RD
    }
    flags |= rcode & 0x000F;

    let mut out = Vec::with_capacity(12 + query.question_wire.len() + addrs.len() * 16);
    out.extend_from_slice(&query.id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&ancount.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&query.question_wire); // echo question verbatim

    for ip in addrs {
        out.extend_from_slice(&[0xC0, 0x0C]); // name = pointer to offset 12
        out.extend_from_slice(&QTYPE_A.to_be_bytes());
        out.extend_from_slice(&QCLASS_IN.to_be_bytes());
        out.extend_from_slice(&60u32.to_be_bytes()); // TTL 60s
        out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        out.extend_from_slice(&ip.octets());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a QNAME (labels) into wire form.
    fn qname(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for label in name.split('.') {
            v.push(label.len() as u8);
            v.extend_from_slice(label.as_bytes());
        }
        v.push(0);
        v
    }

    fn query_packet(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // RD set, standard query
        p.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR = 0
        p.extend_from_slice(&qname(name));
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&QCLASS_IN.to_be_bytes());
        p
    }

    #[test]
    fn parses_a_multi_label_a_query() {
        let q = parse_query(&query_packet(0xABCD, "dl-cdn.alpinelinux.org", QTYPE_A))
            .expect("valid query");
        assert_eq!(q.id, 0xABCD);
        assert_eq!(q.name, "dl-cdn.alpinelinux.org");
        assert_eq!(q.qtype, QTYPE_A);
        assert_eq!(q.qclass, QCLASS_IN);
        assert!(q.recursion_desired);
    }

    #[test]
    fn qname_is_lowercased() {
        let q = parse_query(&query_packet(1, "DL-CDN.Alpinelinux.ORG", QTYPE_A)).unwrap();
        assert_eq!(q.name, "dl-cdn.alpinelinux.org");
    }

    #[test]
    fn build_a_response_carries_address_and_echoes_question() {
        let q = parse_query(&query_packet(0x1234, "example.com", QTYPE_A)).unwrap();
        let ip = Ipv4Addr::new(93, 184, 216, 34);
        let r = build_response(&q, DnsAnswer::Addrs(vec![ip]));
        // Header: same ID, QR+AA+RA set, NOERROR, QD=1, AN=1.
        assert_eq!(u16::from_be_bytes([r[0], r[1]]), 0x1234);
        let flags = u16::from_be_bytes([r[2], r[3]]);
        assert_ne!(flags & 0x8000, 0, "QR");
        assert_eq!(flags & 0x000F, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([r[4], r[5]]), 1, "QDCOUNT");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1, "ANCOUNT");
        // Question echoed verbatim right after the 12-byte header.
        assert_eq!(&r[12..12 + q.question_wire.len()], &q.question_wire[..]);
        // Answer RR: compression pointer to 0x000C, type A, the IP in RDATA.
        let ans = 12 + q.question_wire.len();
        assert_eq!(&r[ans..ans + 2], &[0xC0, 0x0C]);
        assert_eq!(u16::from_be_bytes([r[ans + 2], r[ans + 3]]), QTYPE_A);
        assert_eq!(&r[ans + 12..ans + 16], &ip.octets());
    }

    #[test]
    fn aaaa_gets_empty_noerror() {
        // Caller maps AAAA → Addrs(vec![]) (NOERROR, no answers) on our v4 LAN.
        let q = parse_query(&query_packet(7, "example.com", QTYPE_AAAA)).unwrap();
        assert_eq!(q.qtype, QTYPE_AAAA);
        let r = build_response(&q, DnsAnswer::Addrs(vec![]));
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0, "ANCOUNT 0");
        assert_eq!(u16::from_be_bytes([r[2], r[3]]) & 0x000F, 0, "NOERROR");
    }

    #[test]
    fn nxdomain_sets_rcode_3() {
        let q = parse_query(&query_packet(9, "nope.example", QTYPE_A)).unwrap();
        let r = build_response(&q, DnsAnswer::NxDomain);
        assert_eq!(u16::from_be_bytes([r[2], r[3]]) & 0x000F, 3, "NXDOMAIN");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0, "no answers");
    }

    #[test]
    fn rejects_truncated_and_responses_and_multi_question() {
        assert!(parse_query(&[0u8; 5]).is_none(), "too short");
        // QR=1 (a response, not a query).
        let mut resp = query_packet(1, "x.com", QTYPE_A);
        resp[2] |= 0x80;
        assert!(parse_query(&resp).is_none(), "response rejected");
        // QDCOUNT = 2.
        let mut two = query_packet(1, "x.com", QTYPE_A);
        two[5] = 2;
        assert!(parse_query(&two).is_none(), "multi-question rejected");
    }

    #[test]
    fn compression_pointer_loop_does_not_hang() {
        // A question name that is a pointer to itself — must terminate, not loop.
        let mut p = Vec::new();
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&0x0100u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        p.extend_from_slice(&[0xC0, 0x0C]); // pointer at offset 12 → points to 12 (self)
        assert!(parse_query(&p).is_none());
    }
}
