//! Small shared helpers for the networking bridge.

/// The internet checksum (RFC 1071): one's-complement sum of 16-bit
/// big-endian words, folded to 16 bits and inverted. Used for the IPv4
/// header (and, when not zeroed, UDP/TCP). A valid packet, summed with its
/// checksum field included, yields 0.
pub fn internet_checksum(data: &[u8]) -> u16 {
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

    #[test]
    fn rfc1071_known_vector() {
        let hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(internet_checksum(&hdr), 0xb861);
    }

    #[test]
    fn valid_packet_sums_to_zero() {
        let mut hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        let ck = internet_checksum(&hdr);
        hdr[10..12].copy_from_slice(&ck.to_be_bytes());
        assert_eq!(internet_checksum(&hdr), 0);
    }
}
