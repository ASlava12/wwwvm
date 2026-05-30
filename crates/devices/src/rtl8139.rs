//! Realtek RTL8139 NIC — register file (the 256-byte PIO window behind
//! BAR0). This is the minimum the in-guest `8139too` driver touches to
//! probe the chip and register `eth0`:
//!
//!   * IDR0-5 (0x00-0x05) — the MAC address (read-only here).
//!   * ChipCmd (0x37) — bit 4 = software reset; we auto-complete it (clear
//!     the bit immediately) so the driver's reset poll terminates.
//!   * TxConfig (0x40-0x43) — the high byte carries the hardware-version
//!     ID; we report 0x74 (RTL-8139C) so the driver recognizes the chip.
//!
//! TX/RX descriptor rings, the interrupt, and link/MII state are NOT
//! modeled yet (Phase A3) — unmodeled registers read/write as plain RAM,
//! which is enough to get the driver bound and `eth0` created. The window
//! is dispatched by `IoBus` at the kernel-assigned BAR0 base.

/// Default MAC — a locally-administered address (the `52:54:00` QEMU-style
/// prefix), so it's obviously a virtual NIC.
const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// TxConfig high byte (offset 0x43) hardware-version field: 0x74 selects
/// "RTL-8139C" in the driver's chip table.
const HW_VERSION_HI: u8 = 0x74;

pub struct Rtl8139 {
    regs: [u8; 256],
}

impl Rtl8139 {
    pub fn new() -> Self {
        let mut regs = [0u8; 256];
        regs[0..6].copy_from_slice(&DEFAULT_MAC);
        Self { regs }
    }

    /// Read one byte from the register window (offset within BAR0).
    pub fn read_reg(&self, off: u16) -> u8 {
        let off = (off & 0xFF) as usize;
        match off {
            // TxConfig high byte: report the RTL-8139C hardware version so
            // the driver's chip-ID match succeeds.
            0x43 => HW_VERSION_HI,
            _ => self.regs[off],
        }
    }

    /// Write one byte to the register window.
    pub fn write_reg(&mut self, off: u16, value: u8) {
        let off = (off & 0xFF) as usize;
        match off {
            // ChipCmd: the reset bit (0x10) auto-completes — clear it at
            // once so the driver's "wait for reset" poll terminates.
            0x37 => self.regs[0x37] = value & !0x10,
            // IDR0-5 hold the MAC; read-only.
            0x00..=0x05 => {}
            _ => self.regs[off] = value,
        }
    }

    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.regs);
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 256 {
            return Err("rtl8139: truncated");
        }
        self.regs.copy_from_slice(&bytes[..256]);
        Ok(256)
    }
}

impl Default for Rtl8139 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_address_reads_back() {
        let nic = Rtl8139::new();
        for (i, &b) in DEFAULT_MAC.iter().enumerate() {
            assert_eq!(nic.read_reg(i as u16), b);
        }
    }

    #[test]
    fn idr_is_read_only() {
        let mut nic = Rtl8139::new();
        nic.write_reg(0x00, 0xAA);
        assert_eq!(nic.read_reg(0x00), DEFAULT_MAC[0], "MAC must not change");
    }

    #[test]
    fn chip_reset_bit_auto_clears() {
        let mut nic = Rtl8139::new();
        // Driver writes CmdReset (bit 4) then polls until it clears.
        nic.write_reg(0x37, 0x10);
        assert_eq!(nic.read_reg(0x37) & 0x10, 0, "reset must auto-complete");
    }

    #[test]
    fn txconfig_reports_rtl8139c_version() {
        let nic = Rtl8139::new();
        assert_eq!(nic.read_reg(0x43), 0x74, "HW version = RTL-8139C");
    }

    #[test]
    fn plain_registers_round_trip() {
        let mut nic = Rtl8139::new();
        nic.write_reg(0x3C, 0xBE); // IntrMask low byte
        assert_eq!(nic.read_reg(0x3C), 0xBE);
    }

    #[test]
    fn snapshot_round_trips() {
        let mut nic = Rtl8139::new();
        nic.write_reg(0x3C, 0x05);
        let mut buf = Vec::new();
        nic.snapshot_into(&mut buf);
        assert_eq!(buf.len(), 256);
        let mut nic2 = Rtl8139::new();
        nic2.restore(&buf).expect("restore");
        assert_eq!(nic2.read_reg(0x3C), 0x05);
        assert_eq!(nic2.read_reg(0x00), DEFAULT_MAC[0]);
    }
}
