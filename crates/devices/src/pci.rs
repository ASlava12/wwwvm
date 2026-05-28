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
//! With no PCI devices behind the bus, every read returns 0xFFFFFFFF
//! (the "no device present" sentinel — vendor ID = 0xFFFF). That's
//! what the Linux kernel scans for during PCI enumeration: a vendor
//! ID of 0xFFFF means "skip this slot". Without the dispatch this
//! port pair was unmapped, so reads got 0xFF byte-by-byte — by accident
//! the same answer, but writes to 0xCF8 went nowhere and any read
//! that should have hit a real device returned the same sentinel.
//!
//! Writes from the CPU come in as byte writes (via the 0x66-prefixed
//! 32-bit OUT path's four consecutive byte-writes shim), so we
//! accumulate the address into [`Pci::addr`] byte-by-byte. Likewise
//! reads at 0xCFC..0xCFF return the four bytes of an `u32` value.

use crate::IoDevice;

const PORT_ADDR_LO: u16 = 0xCF8;
const PORT_LAST: u16 = 0xCFF;

pub struct Pci {
    /// Latched address register (CF8..CFB). Bit 31 enable; bus/device/
    /// function/register in the lower bits.
    addr: u32,
}

impl Pci {
    pub fn new() -> Self {
        Self { addr: 0 }
    }

    /// Look up the dword at the currently-latched configuration
    /// address. Always 0xFFFFFFFF in this skeleton — we have no
    /// devices behind the bus yet.
    fn read_data(&self) -> u32 {
        // Real bus: route by bus/device/function. We don't model any
        // devices, so every slot answers "no device".
        let _ = self.addr;
        0xFFFF_FFFF
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

    #[test]
    fn empty_bus_reads_all_ones_at_data_port() {
        let mut pci = Pci::new();
        // Set address: enable=1, bus=0, device=0, function=0, reg=0.
        // (Linux's first probe: bus 0 device 0 vendor/device ID at
        // offset 0.)
        for (i, b) in [0x00u8, 0x00, 0x00, 0x80].iter().enumerate() {
            pci.write(0xCF8 + i as u16, *b);
        }
        // Read four bytes from the data window — should give us
        // 0xFFFFFFFF, the "no device" sentinel.
        let mut got = 0u32;
        for i in 0..4u16 {
            got |= (pci.read(0xCFC + i) as u32) << (i * 8);
        }
        assert_eq!(got, 0xFFFF_FFFF);
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
}
