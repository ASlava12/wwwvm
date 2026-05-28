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

/// Base of the HPET (High Precision Event Timer) MMIO window. The
/// ACPI HPET table normally tells the kernel this address; we pin
/// it at the canonical Intel default so the kernel's hard-coded
/// fallback probe finds the device.
pub const HPET_BASE: u32 = 0xFED0_0000;
/// HPET window size — 1 KiB is the spec minimum.
pub const HPET_SIZE: u32 = 0x0400;

pub struct Memory {
    bytes: Vec<u8>,
    /// Backing store for the LAPIC MMIO window. Most bytes are 0 by
    /// default; a few are pre-populated at construction with the
    /// canonical register values (Version, ID) the kernel checks
    /// during APIC presence probing.
    lapic: [u8; LAPIC_SIZE as usize],
    /// Backing store for the HPET MMIO window. Initialised with a
    /// realistic General Capabilities register at offset 0x000 so
    /// the kernel's presence probe sees 3 timers, a 64-bit counter,
    /// vendor ID 0x8086, and a 100 ns counter period.
    hpet: [u8; HPET_SIZE as usize],
    /// LAPIC timer interrupt request — set by `tick_lapic_timer`
    /// when Current Count crosses zero with a non-masked LVT_TIMER.
    /// The CPU's `step()` drains this and dispatches the vector;
    /// the kernel's handler acks via the LAPIC EOI register
    /// (0xFEE0_00B0, which is a plain scratch write here). Transient
    /// — not snapshotted, since restoring mid-IRQ is a niche case.
    pending_lapic_irq: Option<u8>,
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

        let mut hpet = [0u8; HPET_SIZE as usize];
        // General Capabilities register at offset 0x000 (64-bit).
        // Low dword (0x8086_A201):
        //   bits 0..7   REV_ID          = 0x01
        //   bits 8..12  NUM_TIM_CAP     = 2 (means 3 timers)
        //   bit 13      COUNT_SIZE_CAP  = 1 (64-bit counter)
        //   bit 15      LEG_RT_CAP      = 1 (legacy replacement OK)
        //   bits 16..31 VENDOR_ID       = 0x8086 (Intel)
        // High dword: COUNTER_CLK_PERIOD in femtoseconds. 0x05F5_E100
        // = 100_000_000 fs = 100 ns period (10 MHz tick rate).
        hpet[0x00] = 0x01;
        hpet[0x01] = 0xA2;
        hpet[0x02] = 0x86;
        hpet[0x03] = 0x80;
        hpet[0x04] = 0x00;
        hpet[0x05] = 0xE1;
        hpet[0x06] = 0xF5;
        hpet[0x07] = 0x05;
        Self {
            bytes: vec![0; size],
            lapic,
            hpet,
            pending_lapic_irq: None,
        }
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Grow the physical RAM region in place. Existing contents
    /// stay at their original addresses; new bytes are zero-filled.
    /// LAPIC and HPET MMIO windows are unaffected — they live in
    /// their own scratch buffers, not in `bytes`. Used when a VM
    /// host needs to expand RAM mid-life (e.g. loading a bzImage
    /// whose code32_start sits past the default 1 MiB).
    pub fn resize(&mut self, new_size: usize) {
        if new_size > self.bytes.len() {
            self.bytes.resize(new_size, 0);
        }
    }

    /// True if `addr` falls inside the LAPIC's MMIO window.
    #[inline]
    fn is_lapic(addr: u32) -> bool {
        addr.wrapping_sub(LAPIC_BASE) < LAPIC_SIZE
    }

    /// True if `addr` falls inside the HPET's MMIO window.
    #[inline]
    fn is_hpet(addr: u32) -> bool {
        addr.wrapping_sub(HPET_BASE) < HPET_SIZE
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        if Self::is_lapic(addr) {
            return self.lapic[(addr - LAPIC_BASE) as usize];
        }
        if Self::is_hpet(addr) {
            return self.hpet[(addr - HPET_BASE) as usize];
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
            let off = (addr - LAPIC_BASE) as usize;
            self.lapic[off] = value;
            // A write to LAPIC Initial Count (0x380..0x384) also
            // resets the matching byte of Current Count (0x390+).
            // Real silicon snaps current = initial atomically; we
            // mirror byte-wise, which converges to the same value
            // after the kernel finishes its u32 write (Linux writes
            // initial count as a single MOV).
            if (0x380..0x384).contains(&off) {
                self.lapic[0x390 + (off - 0x380)] = value;
            }
            return;
        }
        if Self::is_hpet(addr) {
            self.hpet[(addr - HPET_BASE) as usize] = value;
            return;
        }
        let a = addr as usize;
        if a < self.bytes.len() {
            self.bytes[a] = value;
        }
    }

    /// Tick the LAPIC timer once. Decrements the Current Count
    /// (LAPIC offset 0x390) by 1; on the zero crossing, fires the
    /// vector from LVT_TIMER (offset 0x320) unless masked, and
    /// either reloads from Initial Count (periodic mode) or stays
    /// at zero (one-shot). The CPU calls this once per step;
    /// Linux's tick path then sees both the calibration delta and
    /// the periodic interrupt source.
    ///
    /// LVT_TIMER layout:
    ///   bits  7:0  vector
    ///   bit  16    mask (1 = no interrupt fires)
    ///   bits 18:17 timer mode (00 = one-shot, 01 = periodic, 10 = TSC-deadline)
    /// We don't model TSC-deadline.
    pub fn tick_lapic_timer(&mut self) {
        let cur_off = 0x390;
        let cur = u32::from_le_bytes([
            self.lapic[cur_off],
            self.lapic[cur_off + 1],
            self.lapic[cur_off + 2],
            self.lapic[cur_off + 3],
        ]);
        if cur == 0 {
            return;
        }
        let next = cur - 1;
        self.lapic[cur_off..cur_off + 4].copy_from_slice(&next.to_le_bytes());
        if next != 0 {
            return;
        }
        // Zero crossing — check LVT_TIMER.
        let lvt = u32::from_le_bytes([
            self.lapic[0x320],
            self.lapic[0x321],
            self.lapic[0x322],
            self.lapic[0x323],
        ]);
        let masked = lvt & (1 << 16) != 0;
        let periodic = (lvt >> 17) & 0b11 == 0b01;
        if !masked {
            // Don't overwrite an already-pending IRQ — the kernel
            // hasn't drained it yet. Real silicon would coalesce
            // via the LAPIC IRR / ISR; our minimal model just drops
            // the new edge (matches "lost tick" behavior, which
            // Linux already tolerates).
            if self.pending_lapic_irq.is_none() {
                self.pending_lapic_irq = Some(lvt as u8);
            }
        }
        if periodic {
            let init = u32::from_le_bytes([
                self.lapic[0x380],
                self.lapic[0x381],
                self.lapic[0x382],
                self.lapic[0x383],
            ]);
            self.lapic[cur_off..cur_off + 4].copy_from_slice(&init.to_le_bytes());
        }
    }

    /// CPU-side: drain the pending LAPIC timer IRQ, if any. The
    /// `step()` loop calls this in the IF-enabled fast path,
    /// alongside the legacy-PIC vector check.
    pub fn take_pending_lapic_irq(&mut self) -> Option<u8> {
        self.pending_lapic_irq.take()
    }

    /// Tick the HPET main counter once. When the General
    /// Configuration register's ENABLE_CNF bit (offset 0x010 bit 0)
    /// is set, the 64-bit Main Counter at 0x0F0 increments by 1.
    /// When ENABLE_CNF is clear, the counter freezes — that's how
    /// Linux pauses HPET during recalibration / suspend.
    ///
    /// After the increment, each of the three timers is checked for
    /// a comparator match. A matching timer with INT_ENB_CNF
    /// (bit 2) and FSB_EN_CNF (bit 14) both set queues an IRQ at
    /// the vector from its FSB_INT_VAL register — Linux's HPET
    /// driver in MSI-direct delivery mode. We share the LAPIC IRQ
    /// slot (both targets are the local APIC anyway); the kernel
    /// can't tell whether the delivery came from LAPIC timer or
    /// HPET except by the vector it programmed.
    pub fn tick_hpet_counter(&mut self) {
        if self.hpet[0x10] & 1 == 0 {
            return;
        }
        let off = 0xF0;
        let cur = u64::from_le_bytes([
            self.hpet[off],
            self.hpet[off + 1],
            self.hpet[off + 2],
            self.hpet[off + 3],
            self.hpet[off + 4],
            self.hpet[off + 5],
            self.hpet[off + 6],
            self.hpet[off + 7],
        ]);
        let next = cur.wrapping_add(1);
        self.hpet[off..off + 8].copy_from_slice(&next.to_le_bytes());

        // Three timers at 0x100, 0x120, 0x140. Per timer:
        //   +0x00 Config / Cap   (we care about bits 2, 14)
        //   +0x08 Comparator     (64-bit, we use the low qword)
        //   +0x10 FSB Route       (low 32 = MSI data, high 32 = MSI addr)
        for tn in 0..3 {
            let base = 0x100 + tn * 0x20;
            let cfg_lo = u32::from_le_bytes([
                self.hpet[base],
                self.hpet[base + 1],
                self.hpet[base + 2],
                self.hpet[base + 3],
            ]);
            if cfg_lo & ((1 << 2) | (1 << 14)) != (1 << 2) | (1 << 14) {
                continue;
            }
            let cmp_off = base + 0x08;
            let cmp = u64::from_le_bytes([
                self.hpet[cmp_off],
                self.hpet[cmp_off + 1],
                self.hpet[cmp_off + 2],
                self.hpet[cmp_off + 3],
                self.hpet[cmp_off + 4],
                self.hpet[cmp_off + 5],
                self.hpet[cmp_off + 6],
                self.hpet[cmp_off + 7],
            ]);
            if next != cmp {
                continue;
            }
            let fsb_off = base + 0x10;
            let fsb_val = u32::from_le_bytes([
                self.hpet[fsb_off],
                self.hpet[fsb_off + 1],
                self.hpet[fsb_off + 2],
                self.hpet[fsb_off + 3],
            ]);
            if self.pending_lapic_irq.is_none() {
                self.pending_lapic_irq = Some(fsb_val as u8);
            }
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

    /// Borrow the 4 KiB LAPIC scratch window. Used by the VM
    /// snapshot path to round-trip MMIO state alongside RAM.
    pub fn lapic_bytes(&self) -> &[u8] {
        &self.lapic
    }

    /// Replace the LAPIC scratch window in one shot. Returns Err
    /// with the expected size on length mismatch — same shape as
    /// `restore_full` for RAM.
    pub fn restore_lapic(&mut self, data: &[u8]) -> Result<(), usize> {
        if data.len() != self.lapic.len() {
            return Err(self.lapic.len());
        }
        self.lapic.copy_from_slice(data);
        Ok(())
    }

    /// Borrow the 1 KiB HPET scratch window — same role as
    /// `lapic_bytes` for the timer's MMIO state.
    pub fn hpet_bytes(&self) -> &[u8] {
        &self.hpet
    }

    pub fn restore_hpet(&mut self, data: &[u8]) -> Result<(), usize> {
        if data.len() != self.hpet.len() {
            return Err(self.hpet.len());
        }
        self.hpet.copy_from_slice(data);
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
    fn hpet_general_caps_register_reads_as_canonical_value() {
        let m = Memory::new(64);
        // Low dword: 0x8086_A201 — REV_ID 1, 3 timers, 64-bit
        // counter, LEG_RT supported, vendor 0x8086.
        assert_eq!(m.read_u32(HPET_BASE), 0x8086_A201);
        // High dword: 100 ns counter period in femtoseconds.
        assert_eq!(m.read_u32(HPET_BASE + 0x04), 0x05F5_E100);
    }

    #[test]
    fn hpet_writes_round_trip_through_scratch() {
        let mut m = Memory::new(64);
        // Main counter at offset 0x0F0 (writable when timer is off).
        m.write_u32(HPET_BASE + 0xF0, 0xDEAD_BEEF);
        assert_eq!(m.read_u32(HPET_BASE + 0xF0), 0xDEAD_BEEF);
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
