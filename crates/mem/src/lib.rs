//! Physical memory for the guest. A flat byte buffer with little-endian
//! word accessors. Out-of-range accesses return zero on read and are
//! silently dropped on write — matches how unmapped DRAM behaves on real
//! hardware for our purposes and keeps the CPU loop branch-free.
//!
//! A single 4 KiB MMIO region at 0xFEE0_0000 mirrors the legacy Local
//! APIC base. Reads and writes in that window land in a separate
//! `lapic` scratch buffer rather than the main DRAM; the kernel uses
//! this to learn that a LAPIC is present (the Version register at
//! offset 0x030 holds a non-zero value) and to round-trip its own
//! writes (SIV, TPR, etc.). This is a minimum-viable stub — there's
//! no IRQ delivery and no timer; the kernel sees enough state to
//! complete its detection probe and then fall back to the legacy PIC
//! for actual interrupts.

#![forbid(unsafe_code)]

/// Base of the Local APIC's MMIO window — the canonical x86 address
/// `RDMSR IA32_APIC_BASE` reports.
pub const LAPIC_BASE: u32 = 0xFEE0_0000;
/// Size of the LAPIC window. Real silicon exposes a full 4 KiB page.
pub const LAPIC_SIZE: u32 = 0x1000;

pub struct Memory {
    bytes: Vec<u8>,
    /// Backing store for the LAPIC MMIO window. Most bytes are 0 by
    /// default; a few are pre-populated at construction with the
    /// canonical register values (Version, ID) the kernel checks
    /// during APIC presence probing.
    lapic: [u8; LAPIC_SIZE as usize],
}

impl Memory {
    pub fn new(size: usize) -> Self {
        let mut lapic = [0u8; LAPIC_SIZE as usize];
        // Version register at offset 0x030: low 8 bits = APIC
        // version (0x14 = "Local Xeon"); bits 16..23 = max LVT
        // entries (we report 6, matching typical Pentium-class
        // chips). Stored little-endian: [0x14, 0x00, 0x06, 0x00].
        lapic[0x30] = 0x14;
        lapic[0x32] = 0x06;
        // ID register at 0x020 already reads zero — that's the
        // canonical BSP APIC ID for our single-CPU model.
        Self {
            bytes: vec![0; size],
            lapic,
        }
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// True if `addr` falls inside the LAPIC's MMIO window.
    #[inline]
    fn is_lapic(addr: u32) -> bool {
        addr.wrapping_sub(LAPIC_BASE) < LAPIC_SIZE
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        if Self::is_lapic(addr) {
            return self.lapic[(addr - LAPIC_BASE) as usize];
        }
        let a = addr as usize;
        if a < self.bytes.len() {
            self.bytes[a]
        } else {
            0
        }
    }

    pub fn write_u8(&mut self, addr: u32, value: u8) {
        if Self::is_lapic(addr) {
            self.lapic[(addr - LAPIC_BASE) as usize] = value;
            return;
        }
        let a = addr as usize;
        if a < self.bytes.len() {
            self.bytes[a] = value;
        }
    }

    pub fn read_u16(&self, addr: u32) -> u16 {
        let lo = self.read_u8(addr) as u16;
        let hi = self.read_u8(addr.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }

    pub fn write_u16(&mut self, addr: u32, value: u16) {
        self.write_u8(addr, value as u8);
        self.write_u8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    pub fn read_u32(&self, addr: u32) -> u32 {
        let lo = self.read_u16(addr) as u32;
        let hi = self.read_u16(addr.wrapping_add(2)) as u32;
        lo | (hi << 16)
    }

    pub fn write_u32(&mut self, addr: u32, value: u32) {
        self.write_u16(addr, value as u16);
        self.write_u16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        let start = addr as usize;
        let end = start.saturating_add(data.len()).min(self.bytes.len());
        let n = end.saturating_sub(start);
        self.bytes[start..start + n].copy_from_slice(&data[..n]);
    }

    /// Borrow the entire backing buffer. Used by the VM's snapshot
    /// helper to write 1 MB out in a single pass.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Replace the entire backing buffer in one shot. Returns Err if
    /// `data.len()` does not match the configured RAM size — restoring
    /// a snapshot from a VM built with different memory sizing would
    /// otherwise silently truncate or leave stale tail bytes.
    pub fn restore_full(&mut self, data: &[u8]) -> Result<(), usize> {
        if data.len() != self.bytes.len() {
            return Err(self.bytes.len());
        }
        self.bytes.copy_from_slice(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_byte_round_trip() {
        let mut m = Memory::new(64);
        m.write_u8(10, 0xAB);
        assert_eq!(m.read_u8(10), 0xAB);
    }

    #[test]
    fn word_is_little_endian() {
        let mut m = Memory::new(64);
        m.write_u16(0, 0xBEEF);
        assert_eq!(m.read_u8(0), 0xEF);
        assert_eq!(m.read_u8(1), 0xBE);
        assert_eq!(m.read_u16(0), 0xBEEF);
    }

    #[test]
    fn out_of_range_read_returns_zero_write_is_noop() {
        let mut m = Memory::new(16);
        m.write_u8(100, 0xFF);
        assert_eq!(m.read_u8(100), 0);
    }

    #[test]
    fn write_slice_clips_at_boundary() {
        let mut m = Memory::new(8);
        m.write_slice(6, &[1, 2, 3, 4]);
        assert_eq!(m.read_u8(6), 1);
        assert_eq!(m.read_u8(7), 2);
    }

    #[test]
    fn restore_full_round_trips() {
        let mut m = Memory::new(16);
        m.write_u8(0, 0xAA);
        let snap = m.as_slice().to_vec();
        let mut m2 = Memory::new(16);
        m2.restore_full(&snap).unwrap();
        assert_eq!(m2.read_u8(0), 0xAA);
    }

    #[test]
    fn lapic_version_register_reads_as_canonical_value() {
        // Offset 0x030 — low byte version 0x14, bits 16..23 max LVT
        // count = 6. As a little-endian u32: 0x0006_0014.
        let m = Memory::new(64);
        assert_eq!(m.read_u32(LAPIC_BASE + 0x30), 0x0006_0014);
        // ID register at 0x020 = 0 (the BSP).
        assert_eq!(m.read_u32(LAPIC_BASE + 0x20), 0);
    }

    #[test]
    fn lapic_writes_round_trip_through_the_scratch_window() {
        // The Spurious Interrupt Vector at offset 0x0F0 is the
        // canonical read-after-write register Linux probes — if
        // it doesn't return the written value, the kernel decides
        // the LAPIC is broken and falls back.
        let mut m = Memory::new(64);
        m.write_u32(LAPIC_BASE + 0xF0, 0x0000_010F); // enable + vector 0x0F
        assert_eq!(m.read_u32(LAPIC_BASE + 0xF0), 0x0000_010F);
    }

    #[test]
    fn lapic_window_does_not_steal_from_low_dram() {
        // Confirm a normal RAM write at 0x1000 still goes to DRAM
        // (the LAPIC check uses wrapping_sub so an address far below
        // LAPIC_BASE doesn't accidentally land in the LAPIC array).
        let mut m = Memory::new(0x4000);
        m.write_u32(0x1000, 0xDEAD_BEEF);
        assert_eq!(m.read_u32(0x1000), 0xDEAD_BEEF);
    }

    #[test]
    fn restore_full_rejects_size_mismatch() {
        let mut m = Memory::new(16);
        let err = m.restore_full(&[0u8; 8]).unwrap_err();
        assert_eq!(err, 16);
    }
}
