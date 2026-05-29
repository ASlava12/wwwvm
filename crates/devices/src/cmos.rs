//! CMOS / Real-Time Clock — minimal MC146818-style surface.
//!
//! What we model:
//!   * Port 0x70 — index latch. Bottom 7 bits select which CMOS byte
//!     to access; bit 7 is the NMI-mask flag on real hardware and is
//!     dropped here.
//!   * Port 0x71 — data port. Read returns `storage[index]`; write
//!     updates it.
//!   * 128-byte storage backing the standard register layout.
//!   * Sensible defaults so a guest that probes 0x0B (Status B) finds
//!     binary + 24-hour mode set, and 0x0D (Status D) reports a good
//!     battery.
//!
//! What we don't model:
//!   * Periodic / alarm / update-ended IRQs (no slave PIC anyway).
//!   * BCD vs binary mode switching (Status B bit 2 is always set).
//!   * The 24h / 12h flag (Status B bit 1 is always set).
//!   * UIP flag — Status A bit 7 stays clear, so reads are always
//!     valid.
//!
//! The host can seed time via [`Cmos::set_time`]; without it the clock
//! reads as the build-time default below.

use crate::IoDevice;

pub struct Cmos {
    index_port: u16,
    data_port: u16,
    storage: [u8; 128],
    index: u8,
}

/// Standard MC146818 register offsets used by the host-side API.
pub mod reg {
    pub const SECONDS: u8 = 0x00;
    pub const MINUTES: u8 = 0x02;
    pub const HOURS: u8 = 0x04;
    pub const DAY_OF_WEEK: u8 = 0x06;
    pub const DAY_OF_MONTH: u8 = 0x07;
    pub const MONTH: u8 = 0x08;
    pub const YEAR: u8 = 0x09;
    pub const STATUS_A: u8 = 0x0A;
    pub const STATUS_B: u8 = 0x0B;
    pub const STATUS_C: u8 = 0x0C;
    pub const STATUS_D: u8 = 0x0D;
}

impl Cmos {
    pub const INDEX_PORT: u16 = 0x70;
    pub const DATA_PORT: u16 = 0x71;

    pub fn new() -> Self {
        let mut storage = [0u8; 128];
        // Default clock: 2026-01-01 00:00:00, Thursday.
        storage[reg::SECONDS as usize] = 0;
        storage[reg::MINUTES as usize] = 0;
        storage[reg::HOURS as usize] = 0;
        storage[reg::DAY_OF_WEEK as usize] = 5; // Thursday (1=Sun … 7=Sat)
        storage[reg::DAY_OF_MONTH as usize] = 1;
        storage[reg::MONTH as usize] = 1;
        storage[reg::YEAR as usize] = 26;
        // Status A — divider chain on, 1024 Hz rate. UIP stays clear.
        storage[reg::STATUS_A as usize] = 0x26;
        // Status B — bit 2 = binary format (not BCD), bit 1 = 24-hour.
        storage[reg::STATUS_B as usize] = 0x06;
        // Status C — no pending IRQs.
        storage[reg::STATUS_C as usize] = 0;
        // Status D — bit 7 = valid CMOS RAM / battery good.
        storage[reg::STATUS_D as usize] = 0x80;
        Self {
            index_port: Self::INDEX_PORT,
            data_port: Self::DATA_PORT,
            storage,
            index: 0,
        }
    }

    /// Seed the date/time bytes in binary format. Year is 2-digit
    /// (00..99) per the MC146818 convention; everything else is the
    /// natural numeric value.
    pub fn set_time(&mut self, year: u8, month: u8, day: u8, hour: u8, minute: u8, second: u8) {
        self.storage[reg::SECONDS as usize] = second;
        self.storage[reg::MINUTES as usize] = minute;
        self.storage[reg::HOURS as usize] = hour;
        self.storage[reg::DAY_OF_MONTH as usize] = day;
        self.storage[reg::MONTH as usize] = month;
        self.storage[reg::YEAR as usize] = year;
    }

    /// Host-side read of any CMOS byte, bypassing the index latch.
    /// Useful for debugging and from JS for reading boot-config bytes
    /// the guest may have written.
    pub fn storage_byte(&self, idx: u8) -> u8 {
        self.storage[(idx & 0x7F) as usize]
    }

    /// Host-side write of any CMOS byte, bypassing the index latch.
    pub fn set_storage_byte(&mut self, idx: u8, value: u8) {
        self.storage[(idx & 0x7F) as usize] = value;
    }

    /// Snapshot: index (u8) + 128 storage bytes.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.push(self.index);
        out.extend_from_slice(&self.storage);
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 1 + 128 {
            return Err("cmos: truncated");
        }
        self.index = bytes[0];
        self.storage.copy_from_slice(&bytes[1..1 + 128]);
        Ok(1 + 128)
    }
}

impl Default for Cmos {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for Cmos {
    fn port_range(&self) -> (u16, u16) {
        (self.index_port, self.data_port)
    }

    fn read(&mut self, port: u16) -> u8 {
        if port == self.data_port {
            self.storage[self.index as usize]
        } else {
            // 0x70 is write-only on real hardware; reads are undefined.
            0
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        if port == self.index_port {
            // Bit 7 is NMI mask on real hardware; we ignore it.
            self.index = value & 0x7F;
        } else if port == self.data_port {
            self.storage[self.index as usize] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_b_is_binary_24h() {
        let cmos = Cmos::new();
        let sb = cmos.storage_byte(reg::STATUS_B);
        assert_eq!(sb & 0x04, 0x04, "binary bit");
        assert_eq!(sb & 0x02, 0x02, "24h bit");
    }

    #[test]
    fn default_status_d_signals_battery_good() {
        let cmos = Cmos::new();
        assert_eq!(cmos.storage_byte(reg::STATUS_D), 0x80);
    }

    #[test]
    fn index_then_data_round_trip() {
        let mut cmos = Cmos::new();
        cmos.write(0x70, reg::SECONDS);
        assert_eq!(cmos.read(0x71), 0);
        cmos.write(0x71, 42);
        assert_eq!(cmos.read(0x71), 42);
        // Reading again still returns the latched value — there's no
        // auto-increment on real RTCs.
        assert_eq!(cmos.read(0x71), 42);
    }

    #[test]
    fn set_time_updates_register_bytes() {
        let mut cmos = Cmos::new();
        cmos.set_time(26, 5, 27, 12, 34, 56);
        cmos.write(0x70, reg::HOURS);
        assert_eq!(cmos.read(0x71), 12);
        cmos.write(0x70, reg::MINUTES);
        assert_eq!(cmos.read(0x71), 34);
        cmos.write(0x70, reg::SECONDS);
        assert_eq!(cmos.read(0x71), 56);
        cmos.write(0x70, reg::DAY_OF_MONTH);
        assert_eq!(cmos.read(0x71), 27);
        cmos.write(0x70, reg::MONTH);
        assert_eq!(cmos.read(0x71), 5);
        cmos.write(0x70, reg::YEAR);
        assert_eq!(cmos.read(0x71), 26);
    }

    #[test]
    fn nmi_mask_bit_is_stripped_from_index() {
        let mut cmos = Cmos::new();
        // Write index 0x80 | SECONDS — bit 7 should be ignored.
        cmos.write(0x70, 0x80 | reg::SECONDS);
        cmos.set_storage_byte(reg::SECONDS, 7);
        assert_eq!(cmos.read(0x71), 7);
    }

    /// snapshot/restore must round-trip both the latched index
    /// (so a guest mid-INDEX-then-DATA sequence resumes at the
    /// right register) and the full 128-byte storage backing
    /// store. Without index round-trip, a snapshot taken between
    /// `out 0x70, X` and `in al, 0x71` would resume reading the
    /// wrong register on the other side.
    #[test]
    fn snapshot_round_trip_preserves_index_and_storage() {
        let mut cmos = Cmos::new();
        cmos.set_time(26, 5, 27, 8, 15, 30);
        // Park the index latch at a non-default register so we
        // know the round-trip actually carries it.
        cmos.write(0x70, reg::MINUTES);
        // Sentinel write into a non-time scratch register too.
        cmos.write(0x70, 0x40);
        cmos.write(0x71, 0xAB);
        // And finally re-park the index at SECONDS so we can
        // check both halves: index latch AND storage contents.
        cmos.write(0x70, reg::SECONDS);

        let mut buf = Vec::new();
        cmos.snapshot_into(&mut buf);
        let mut cmos2 = Cmos::new();
        let consumed = cmos2.restore(&buf).expect("restore");
        assert_eq!(consumed, 1 + 128);

        // Index round-tripped.
        assert_eq!(cmos2.read(0x71), cmos.read(0x71), "index latch");
        // Time bytes round-tripped through storage (binary, not BCD).
        assert_eq!(cmos2.storage_byte(reg::SECONDS), 30);
        assert_eq!(cmos2.storage_byte(reg::MINUTES), 15);
        assert_eq!(cmos2.storage_byte(reg::HOURS), 8);
        // Scratch register round-tripped too.
        assert_eq!(cmos2.storage_byte(0x40), 0xAB);
    }

    /// restore must reject a truncated blob rather than panic on
    /// the slice access. Boundary: 128 bytes alone is too short
    /// (index byte missing).
    #[test]
    fn restore_rejects_truncated_blob() {
        let mut cmos = Cmos::new();
        let err = cmos.restore(&[0u8; 128]).unwrap_err();
        assert!(err.contains("truncated"));
    }
}
