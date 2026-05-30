//! PCI configuration space — Mechanism #1 (the standard since 1995).
//!
//! Two 32-bit ports:
//!
//!   * 0xCF8 — address register. Bit 31 = enable; bits 16..23 = bus;
//!     bits 11..15 = device; bits 8..10 = function; bits 2..7 =
//!     register offset (dword-aligned).
//!   * 0xCFC — data window. Reads/writes hit the dword at the latched
//!     bus/device/function/register location.
//!
//! We model one device: a host bridge at 00:00.0 (Intel 440FX, class
//! 0x0600). This is REQUIRED for PCI to work at all — Linux's Mechanism #1
//! detection passes the CF8 read-back check but then runs
//! `pci_sanity_check`, which scans bus 0 for at least one plausible device
//! (a host bridge, a VGA, or an Intel/Compaq vendor). An empty bus (all
//! 0xFFFFFFFF) fails that check, so the kernel prints "PCI: Fatal: No
//! config space access function found" and disables PCI entirely. With the
//! bridge present the bus is real and enumeration proceeds; other slots
//! still answer 0xFFFFFFFF (vendor 0xFFFF = "no device, skip").
//!
//! Writes from the CPU come in as byte writes (via the 0x66-prefixed
//! 32-bit OUT path's four consecutive byte-writes shim), so we
//! accumulate the address into [`Pci::addr`] byte-by-byte. Likewise
//! reads at 0xCFC..0xCFF return the four bytes of an `u32` value.

use crate::IoDevice;

const PORT_ADDR_LO: u16 = 0xCF8;
const PORT_LAST: u16 = 0xCFF;

/// Configuration-space dwords for the host bridge at 00:00.0 — an Intel
/// 440FX (vendor 0x8086, device 0x1237), class 0x060000 (bridge / host).
/// `reg` is the dword-aligned register offset. Unimplemented registers
/// read as 0 (a present device, not the 0xFFFFFFFF "absent" sentinel).
fn host_bridge_config(reg: u32) -> u32 {
    match reg {
        0x00 => 0x1237_8086,        // device 0x1237 << 16 | vendor 0x8086
        0x04 => 0x0000_0000,        // command / status
        0x08 => 0x0600_0000 | 0x02, // class 0x060000 (host bridge), rev 0x02
        0x0C => 0x0000_0000,        // BIST / header type 0 / latency / cacheline
        _ => 0x0000_0000,           // no BARs, no capabilities
    }
}

/// Configuration-space dwords for the NIC at 00:01.0 — a Realtek RTL8139
/// (vendor 0x10EC, device 0x8139), class 0x020000 (network / ethernet).
/// Read-only for now: the device is enumerable (so the kernel lists it and
/// the 8139 driver can match), but BAR0 reads 0 — the I/O register window
/// and bus-master DMA come in a follow-up (Phase A2b/A3).
fn rtl8139_config(reg: u32) -> u32 {
    match reg {
        0x00 => 0x8139_10EC,        // device 0x8139 << 16 | vendor 0x10EC
        0x04 => 0x0000_0000,        // command / status
        0x08 => 0x0200_0000 | 0x10, // class 0x020000 (ethernet), rev 0x10
        0x0C => 0x0000_0000,        // header type 0
        0x3C => 0x0000_010B,        // interrupt pin INTA (1), line 11
        _ => 0x0000_0000,           // BAR0 (0x10) reads 0 until A2b
    }
}

pub struct Pci {
    /// Latched address register (CF8..CFB). Bit 31 enable; bus/device/
    /// function/register in the lower bits.
    addr: u32,
}

impl Pci {
    pub fn new() -> Self {
        Self { addr: 0 }
    }

    /// Look up the dword at the currently-latched configuration address,
    /// routing by bus/device/function. Only 00:00.0 (the host bridge) is
    /// present; every other slot answers 0xFFFFFFFF ("no device").
    fn read_data(&self) -> u32 {
        if self.addr & 0x8000_0000 == 0 {
            return 0xFFFF_FFFF; // config cycle not enabled
        }
        let bus = (self.addr >> 16) & 0xFF;
        let dev = (self.addr >> 11) & 0x1F;
        let func = (self.addr >> 8) & 0x07;
        let reg = self.addr & 0xFC; // dword-aligned register offset
        match (bus, dev, func) {
            (0, 0, 0) => host_bridge_config(reg),
            (0, 1, 0) => rtl8139_config(reg),
            _ => 0xFFFF_FFFF,
        }
    }

    /// Serialize the latched address register (4 bytes). The bus
    /// has no devices, so there's nothing else to preserve.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.addr.to_le_bytes());
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 4 {
            return Err("pci: truncated");
        }
        self.addr = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(4)
    }
}

impl Default for Pci {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for Pci {
    fn port_range(&self) -> (u16, u16) {
        (PORT_ADDR_LO, PORT_LAST)
    }

    fn read(&mut self, port: u16) -> u8 {
        match port {
            // Address register read-back. Real silicon mirrors what
            // was last written.
            0xCF8 => self.addr as u8,
            0xCF9 => (self.addr >> 8) as u8,
            0xCFA => (self.addr >> 16) as u8,
            0xCFB => (self.addr >> 24) as u8,
            // Data window — slice the result of read_data().
            0xCFC => self.read_data() as u8,
            0xCFD => (self.read_data() >> 8) as u8,
            0xCFE => (self.read_data() >> 16) as u8,
            0xCFF => (self.read_data() >> 24) as u8,
            _ => 0xFF,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port {
            // Accumulate the dword address by byte position.
            0xCF8 => self.addr = (self.addr & !0xFF) | (value as u32),
            0xCF9 => self.addr = (self.addr & !0xFF00) | ((value as u32) << 8),
            0xCFA => self.addr = (self.addr & !0xFF_0000) | ((value as u32) << 16),
            0xCFB => self.addr = (self.addr & !0xFF00_0000) | ((value as u32) << 24),
            // Writes to the data window land in non-existent
            // devices — silently discarded.
            0xCFC..=0xCFF => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_read(pci: &mut Pci, addr: u32) -> u32 {
        for i in 0..4u16 {
            pci.write(0xCF8 + i, (addr >> (i * 8)) as u8);
        }
        let mut got = 0u32;
        for i in 0..4u16 {
            got |= (pci.read(0xCFC + i) as u32) << (i * 8);
        }
        got
    }

    #[test]
    fn host_bridge_present_at_00_00_0() {
        // Linux's pci_sanity_check needs a real device on bus 0, or it
        // disables PCI. 00:00.0 must report the host-bridge ID + class.
        let mut pci = Pci::new();
        // enable=1, bus0, dev0, func0, reg0 → vendor/device.
        assert_eq!(cfg_read(&mut pci, 0x8000_0000), 0x1237_8086);
        // reg 0x08 → class 0x060000 in the high 24 bits; sanity_check
        // reads the 16-bit class at offset 0x0A and wants 0x0600.
        let classdw = cfg_read(&mut pci, 0x8000_0008);
        assert_eq!(classdw >> 16, 0x0600, "class code = host bridge");
    }

    #[test]
    fn rtl8139_nic_present_at_00_01_0() {
        let mut pci = Pci::new();
        // device 1 (bit 11), reg 0 → vendor 0x10EC / device 0x8139.
        assert_eq!(cfg_read(&mut pci, 0x8000_0800), 0x8139_10EC);
        // class 0x0200 (ethernet) in the high word of reg 0x08.
        assert_eq!(cfg_read(&mut pci, 0x8000_0808) >> 16, 0x0200);
    }

    #[test]
    fn absent_slot_reads_all_ones() {
        let mut pci = Pci::new();
        // device 2 (bit 12) is empty → vendor 0xFFFF sentinel.
        assert_eq!(cfg_read(&mut pci, 0x8000_1000), 0xFFFF_FFFF);
    }

    #[test]
    fn address_register_round_trips_through_byte_writes() {
        // Confirms the byte-accumulate logic for the 0x66-prefixed
        // 32-bit OUT path: write 4 bytes, read them back, compare.
        let mut pci = Pci::new();
        let want: u32 = 0x8000_4321; // enable + arbitrary address bits
        for i in 0..4u16 {
            pci.write(0xCF8 + i, (want >> (i * 8)) as u8);
        }
        let mut got = 0u32;
        for i in 0..4u16 {
            got |= (pci.read(0xCF8 + i) as u32) << (i * 8);
        }
        assert_eq!(got, want);
    }

    #[test]
    fn writes_to_data_window_are_silently_discarded() {
        let mut pci = Pci::new();
        for i in 0..4u16 {
            pci.write(0xCFC + i, 0xAB);
        }
        // Read still returns the "no device" sentinel; nothing
        // landed in any imaginary register.
        for i in 0..4u16 {
            assert_eq!(pci.read(0xCFC + i), 0xFF);
        }
    }

    /// snapshot/restore must round-trip the latched 32-bit address
    /// register so a snapshot taken mid-config-cycle resumes at
    /// the same probe address. Without it, the kernel issues a
    /// data-port read post-restore and we return 0xFFFFFFFF for
    /// what the kernel thought was a different register — silently
    /// wrong sub-system behavior. Companion to the CMOS/PIC/UART/
    /// keyboard round-trip tests in this series.
    #[test]
    fn snapshot_round_trip_preserves_latched_address() {
        let mut pci = Pci::new();
        let want: u32 = 0x8000_4321;
        for i in 0..4u16 {
            pci.write(0xCF8 + i, (want >> (i * 8)) as u8);
        }
        let mut buf = Vec::new();
        pci.snapshot_into(&mut buf);
        assert_eq!(buf.len(), 4);

        let mut pci2 = Pci::new();
        let consumed = pci2.restore(&buf).expect("restore");
        assert_eq!(consumed, 4);
        let mut got = 0u32;
        for i in 0..4u16 {
            got |= (pci2.read(0xCF8 + i) as u32) << (i * 8);
        }
        assert_eq!(got, want);
    }

    /// restore must reject a truncated blob rather than panic.
    /// 4-byte address takes exactly 4 bytes; any less is corruption.
    #[test]
    fn restore_rejects_truncated_blob() {
        let mut pci = Pci::new();
        assert!(pci.restore(&[0u8; 3]).is_err());
    }
}
