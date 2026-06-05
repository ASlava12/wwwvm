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
//!     BCD + 24-hour mode set, and 0x0D (Status D) reports a good
//!     battery.
//!
//! Time is stored in **BCD** (packed binary-coded decimal), the PC/AT
//! convention that essentially every BIOS and the Linux `rtc-cmos` driver
//! expect — e.g. year 26 is the byte 0x26, not 0x1A. (We previously stored
//! binary and set Status B's DM bit, but the guest kernel read the
//! registers as BCD regardless, mangling the date — binary 26 = 0x1A,
//! and `bcd2bin(0x1A)` = 20, which is exactly why `date` showed 2020.)
//!
//! What we don't model:
//!   * Periodic / alarm / update-ended IRQs (no slave PIC anyway).
//!   * BCD vs binary mode switching (Status B DM bit is always clear = BCD).
//!   * The 24h / 12h flag (Status B bit 1 is always set = 24-hour).
//!   * UIP flag — Status A bit 7 stays clear, so reads are always
//!     valid.
//!
//! The host can seed time via [`Cmos::set_time`]; without it the clock
//! reads as the build-time default below.

use crate::IoDevice;

/// Pack a natural decimal value 0..99 into BCD (e.g. 26 -> 0x26).
#[inline]
fn bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

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
        // Default clock: 2026-01-01 00:00:00, Thursday. Stored BCD.
        storage[reg::SECONDS as usize] = bcd(0);
        storage[reg::MINUTES as usize] = bcd(0);
        storage[reg::HOURS as usize] = bcd(0);
        storage[reg::DAY_OF_WEEK as usize] = bcd(5); // Thursday (1=Sun … 7=Sat)
        storage[reg::DAY_OF_MONTH as usize] = bcd(1);
        storage[reg::MONTH as usize] = bcd(1);
        storage[reg::YEAR as usize] = bcd(26);
        // Status A — divider chain on, 1024 Hz rate. UIP stays clear.
        storage[reg::STATUS_A as usize] = 0x26;
        // Status B — bit 2 (DM) clear = BCD format, bit 1 = 24-hour.
        storage[reg::STATUS_B as usize] = 0x02;
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

    /// Seed the date/time registers. Arguments are natural decimal values
    /// (year 2-digit 00..99 per the MC146818 convention); they're stored
    /// BCD-encoded, matching the registers' default BCD mode (Status B).
    pub fn set_time(&mut self, year: u8, month: u8, day: u8, hour: u8, minute: u8, second: u8) {
        self.storage[reg::SECONDS as usize] = bcd(second);
        self.storage[reg::MINUTES as usize] = bcd(minute);
        self.storage[reg::HOURS as usize] = bcd(hour);
        self.storage[reg::DAY_OF_MONTH as usize] = bcd(day);
        self.storage[reg::MONTH as usize] = bcd(month);
        self.storage[reg::YEAR as usize] = bcd(year);
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
        // Mask to 0..127: `storage` is [u8; 128] and read/write index it
        // directly, so an unmasked index from a malicious snapshot (>= 0x80)
        // would panic on the next port-0x71 access. Live writes already mask
        // (write() at &0x7F); restore must too.
        self.index = bytes[0] & 0x7F;
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
    fn default_status_b_is_bcd_24h() {
        let cmos = Cmos::new();
        let sb = cmos.storage_byte(reg::STATUS_B);
        assert_eq!(sb & 0x04, 0x00, "DM bit clear = BCD format");
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
        // Registers are BCD: 12 -> 0x12, 34 -> 0x34, etc.
        cmos.write(0x70, reg::HOURS);
        assert_eq!(cmos.read(0x71), 0x12);
        cmos.write(0x70, reg::MINUTES);
        assert_eq!(cmos.read(0x71), 0x34);
        cmos.write(0x70, reg::SECONDS);
        assert_eq!(cmos.read(0x71), 0x56);
        cmos.write(0x70, reg::DAY_OF_MONTH);
        assert_eq!(cmos.read(0x71), 0x27);
        cmos.write(0x70, reg::MONTH);
        assert_eq!(cmos.read(0x71), 0x05);
        cmos.write(0x70, reg::YEAR);
        assert_eq!(cmos.read(0x71), 0x26);
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
        // Time bytes round-tripped through storage (BCD: 30 -> 0x30, …).
        assert_eq!(cmos2.storage_byte(reg::SECONDS), 0x30);
        assert_eq!(cmos2.storage_byte(reg::MINUTES), 0x15);
        assert_eq!(cmos2.storage_byte(reg::HOURS), 0x08);
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

    /// A malicious snapshot must not be able to set the index >= 128 and then
    /// OOB-panic the [u8; 128] storage on the next port-0x71 access.
    #[test]
    fn restore_masks_index_to_storage_bounds() {
        let mut cmos = Cmos::new();
        let mut blob = vec![0u8; 1 + 128];
        blob[0] = 0xFF; // crafted: index past storage
        cmos.restore(&blob).expect("restore");
        assert!(cmos.index < 128, "index must be masked into 0..127");
        // And a data-port read must not panic.
        let _ = cmos.read(cmos.data_port);
    }
}
