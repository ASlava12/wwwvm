//! Minimal IDE/ATA controller — what a Linux kernel pokes at when
//! it bypasses BIOS and talks to the disk directly. We own a
//! single `Disk` and present the legacy port interface at a
//! configurable base (0x1F0 for the primary channel, 0x170 for
//! the secondary).
//!
//! The two commands we actually service:
//!   * 0xEC — IDENTIFY DEVICE (drive metadata block)
//!   * 0x20 — READ SECTORS (LBA28, one or more 512-byte sectors)
//!
//! Everything else is acknowledged silently with status = READY.
//! That's enough for the kernel's PIO mode probe-and-read paths.
//!
//! ## A note on 16-bit transfers
//!
//! Real ATA's data register at 0x1F0 is a true 16-bit port. Our CPU
//! decomposes `IN AX, DX` into two byte reads — `inb 0x1F0` then
//! `inb 0x1F1`. To make `inw 0x1F0` consume exactly two buffer
//! bytes (one word) we advance the data cursor on *both* reads
//! while DRQ is set. When DRQ is clear, 0x1F1 reverts to its usual
//! Error-register role. This is a host-side accommodation; real
//! silicon does the transfer in a single 16-bit bus cycle.

use crate::disk::{Disk, SECTOR_SIZE};
use crate::IoDevice;

/// Standard primary-channel command-block base (also the BIOS
/// boot drive). The secondary channel lives at 0x170.
pub const PRIMARY_PORT_BASE: u16 = 0x1F0;
pub const SECONDARY_PORT_BASE: u16 = 0x170;

/// Status-register bits we drive. (STATUS_ERR is not used yet —
/// we never raise a command error.)
const STATUS_DRQ: u8 = 0x08;
const STATUS_DRDY: u8 = 0x40;
const STATUS_BSY: u8 = 0x80;

/// ATA commands we recognise.
const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_IDENTIFY: u8 = 0xEC;

pub struct Ata {
    pub disk: Disk,
    /// Sector-count register (base+2). 0 means 256 sectors.
    sector_count: u8,
    /// LBA byte registers (base+3..base+5) — low/mid/high.
    lba_low: u8,
    lba_mid: u8,
    lba_high: u8,
    /// Drive / head register (base+6). Bits 0..3 = LBA[27:24], bit
    /// 4 = drive number (we only emulate drive 0), bit 6 set marks
    /// LBA mode (vs CHS, which we don't model).
    drive_head: u8,
    /// Error register (read back at base+1).
    error: u8,
    /// Last status (read back at base+7).
    status: u8,
    /// Pending data-transfer buffer. Filled when DRQ goes high; the
    /// guest drains it via reads on the data port for READ commands,
    /// or fills it via writes for WRITE.
    buf: Vec<u8>,
    /// Byte cursor into [`Self::buf`].
    pos: usize,
    /// Direction of the current DRQ-active transfer. `true` means the
    /// guest is feeding bytes in (WRITE SECTORS); `false` means the
    /// guest is reading bytes out (IDENTIFY, READ SECTORS).
    writing: bool,
    /// LBA latched at WRITE SECTORS time, so it survives the guest
    /// scribbling on the LBA registers while still mid-transfer.
    write_lba: u32,
    /// Command-block port base — 0x1F0 (primary) or 0x170 (secondary).
    port_base: u16,
}

impl Ata {
    /// Build a controller listening on the primary command block
    /// (0x1F0..0x1F7). This is what the existing IoBus uses.
    pub fn new() -> Self {
        Self::with_port_base(PRIMARY_PORT_BASE)
    }

    /// Build a controller listening on a specific command-block
    /// base. Use [`SECONDARY_PORT_BASE`] for the second channel.
    pub fn with_port_base(port_base: u16) -> Self {
        Self {
            disk: Disk::new(),
            sector_count: 0,
            lba_low: 0,
            lba_mid: 0,
            lba_high: 0,
            drive_head: 0,
            error: 0,
            status: STATUS_DRDY,
            buf: Vec::new(),
            pos: 0,
            writing: false,
            write_lba: 0,
            port_base,
        }
    }

    /// The 28-bit LBA assembled from the register file.
    fn lba28(&self) -> u32 {
        (self.lba_low as u32)
            | ((self.lba_mid as u32) << 8)
            | ((self.lba_high as u32) << 16)
            | (((self.drive_head & 0x0F) as u32) << 24)
    }

    /// True iff the host is waiting for the guest to drain the
    /// pending transfer buffer (i.e. STATUS.DRQ is set).
    fn drq(&self) -> bool {
        self.status & STATUS_DRQ != 0
    }

    fn execute(&mut self, cmd: u8) {
        match cmd {
            CMD_IDENTIFY => {
                self.buf = build_identify_block(&self.disk);
                self.pos = 0;
                self.writing = false;
                self.error = 0;
                self.status = STATUS_DRDY | STATUS_DRQ;
            }
            CMD_READ_SECTORS => {
                let count = if self.sector_count == 0 {
                    256u16
                } else {
                    self.sector_count as u16
                };
                let lba = self.lba28();
                self.buf = vec![0u8; SECTOR_SIZE * count as usize];
                self.disk.read_sectors(lba, count as u8, &mut self.buf);
                self.pos = 0;
                self.writing = false;
                self.error = 0;
                self.status = STATUS_DRDY | STATUS_DRQ;
            }
            CMD_WRITE_SECTORS => {
                let count = if self.sector_count == 0 {
                    256u16
                } else {
                    self.sector_count as u16
                };
                self.buf = vec![0u8; SECTOR_SIZE * count as usize];
                self.pos = 0;
                self.writing = true;
                self.write_lba = self.lba28();
                self.error = 0;
                self.status = STATUS_DRDY | STATUS_DRQ;
            }
            // Anything else: silently say "done, no data". Real
            // silicon would raise an error; the kernel typically
            // reads status afterwards and moves on.
            _ => {
                self.buf.clear();
                self.pos = 0;
                self.writing = false;
                self.error = 0;
                self.status = STATUS_DRDY;
            }
        }
    }

    /// Read the next byte from the pending transfer buffer; clears
    /// DRQ once drained.
    fn pop_buf_byte(&mut self) -> u8 {
        if self.pos >= self.buf.len() {
            self.status &= !STATUS_DRQ;
            return 0xFF;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        if self.pos >= self.buf.len() {
            self.status &= !STATUS_DRQ;
        }
        b
    }

    /// Accept the next byte of a WRITE-SECTORS transfer. When the
    /// buffer is full, flush it to the disk image, clear DRQ, and
    /// drop back into the idle state.
    fn push_buf_byte(&mut self, b: u8) {
        if self.pos >= self.buf.len() {
            return;
        }
        self.buf[self.pos] = b;
        self.pos += 1;
        if self.pos >= self.buf.len() {
            self.disk.write_sectors(self.write_lba, &self.buf);
            self.status &= !STATUS_DRQ;
            self.writing = false;
        }
    }
}

impl Default for Ata {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for Ata {
    fn port_range(&self) -> (u16, u16) {
        (self.port_base, self.port_base + 7)
    }

    fn read(&mut self, port: u16) -> u8 {
        // Register layout is the same shape on both channels — the
        // controller just shifts by `port_base`. Negative offsets
        // (impossible here because IoBus filters by port_range)
        // would underflow, but `wrapping_sub` keeps the math safe.
        match port.wrapping_sub(self.port_base) {
            0 if self.drq() && !self.writing => self.pop_buf_byte(),
            0 => 0xFF,
            1 if self.drq() && !self.writing => self.pop_buf_byte(),
            1 => self.error,
            2 => self.sector_count,
            3 => self.lba_low,
            4 => self.lba_mid,
            5 => self.lba_high,
            6 => self.drive_head,
            7 => {
                // Reading status would normally clear the interrupt
                // pending state; we don't generate interrupts so it
                // is a plain read.
                self.status
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port.wrapping_sub(self.port_base) {
            // Data port (and its +1 mirror during the byte-pair
            // outw hack): accept the byte iff we're in a WRITE-data
            // phase. Otherwise base+1-write is Features, which we
            // currently ignore (DMA modes etc).
            0 if self.writing && self.drq() => self.push_buf_byte(value),
            0 => {}
            1 if self.writing && self.drq() => self.push_buf_byte(value),
            1 => {}
            2 => self.sector_count = value,
            3 => self.lba_low = value,
            4 => self.lba_mid = value,
            5 => self.lba_high = value,
            6 => self.drive_head = value,
            7 => {
                // Issuing a command latches BSY briefly on real
                // silicon; we complete synchronously, so BSY is
                // never observable.
                self.status = STATUS_BSY;
                self.execute(value);
            }
            _ => {}
        }
    }
}

/// Build the 512-byte IDENTIFY DEVICE block (256 little-endian
/// words). We populate only the fields a kernel actually consults
/// during a non-error probe; everything else stays zero.
fn build_identify_block(disk: &Disk) -> Vec<u8> {
    let mut words = [0u16; 256];
    // Word 0 — general configuration. 0x0040 = "ATA, removable=0,
    // not a CFA device". A non-zero word here distinguishes a
    // present ATA device from a floating bus.
    words[0] = 0x0040;
    // Capabilities: bit 9 = LBA supported.
    words[49] = 1 << 9;
    // Total sectors (LBA28) — 32-bit field at words 60/61.
    let sectors = (disk.size() / SECTOR_SIZE) as u32;
    words[60] = sectors as u16;
    words[61] = (sectors >> 16) as u16;
    // Major version: pretend to support ATA-6 (bit 6 set) so the
    // kernel doesn't reject us as an antique.
    words[80] = 1 << 6;
    let mut out = Vec::with_capacity(512);
    for w in &words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::SECTOR_SIZE;

    #[test]
    fn identify_returns_signature_word_and_total_sector_count() {
        let mut ata = Ata::new();
        // Two-sector image.
        ata.disk.load(&[0xAA; SECTOR_SIZE * 2]);
        ata.write(0x1F7, CMD_IDENTIFY);
        assert!(ata.status & STATUS_DRQ != 0, "DRQ set after IDENTIFY");
        // Drain 512 bytes.
        let mut got = Vec::with_capacity(512);
        for _ in 0..512 {
            got.push(ata.read(0x1F0));
        }
        assert!(ata.status & STATUS_DRQ == 0, "DRQ clears once drained");
        // Word 0 = 0x0040.
        assert_eq!(u16::from_le_bytes([got[0], got[1]]), 0x0040);
        // Word 49 = 0x0200 (LBA supported).
        assert_eq!(u16::from_le_bytes([got[98], got[99]]), 0x0200);
        // Words 60/61 = total sectors (2 here).
        let total = (u16::from_le_bytes([got[120], got[121]]) as u32)
            | ((u16::from_le_bytes([got[122], got[123]]) as u32) << 16);
        assert_eq!(total, 2);
    }

    #[test]
    fn read_sectors_returns_disk_contents_through_data_port() {
        let mut ata = Ata::new();
        // Three distinct sectors so we can check lane order.
        let mut img = Vec::with_capacity(SECTOR_SIZE * 3);
        img.extend_from_slice(&[0x11; SECTOR_SIZE]);
        img.extend_from_slice(&[0x22; SECTOR_SIZE]);
        img.extend_from_slice(&[0x33; SECTOR_SIZE]);
        ata.disk.load(&img);
        // Read sectors 1..3 (2 sectors).
        ata.write(0x1F2, 2); // sector count
        ata.write(0x1F3, 1); // LBA low = 1
        ata.write(0x1F4, 0);
        ata.write(0x1F5, 0);
        ata.write(0x1F6, 0x40); // LBA mode, drive 0
        ata.write(0x1F7, CMD_READ_SECTORS);
        assert!(ata.status & STATUS_DRQ != 0);
        // First half of buffer is all 0x22, second half all 0x33.
        for i in 0..SECTOR_SIZE {
            assert_eq!(ata.read(0x1F0), 0x22, "sector 1 byte {i}");
        }
        for i in 0..SECTOR_SIZE {
            assert_eq!(ata.read(0x1F0), 0x33, "sector 2 byte {i}");
        }
        assert!(ata.status & STATUS_DRQ == 0);
    }

    #[test]
    fn write_sectors_commits_buffer_to_disk_on_full_drain() {
        let mut ata = Ata::new();
        // Start with two sectors of 0x11 so we can confirm sector 0
        // gets replaced while sector 1 stays untouched.
        ata.disk.load(&[0x11; SECTOR_SIZE * 2]);
        ata.write(0x1F2, 1); // sector count
        ata.write(0x1F3, 0); // LBA = 0
        ata.write(0x1F4, 0);
        ata.write(0x1F5, 0);
        ata.write(0x1F6, 0x40);
        ata.write(0x1F7, CMD_WRITE_SECTORS);
        assert!(ata.status & STATUS_DRQ != 0, "DRQ set after WRITE issue");
        assert!(ata.writing);
        // Pour 512 bytes through the data port.
        for _ in 0..SECTOR_SIZE {
            ata.write(0x1F0, 0x77);
        }
        assert!(ata.status & STATUS_DRQ == 0, "DRQ clears at end of write");
        assert!(!ata.writing);
        // Sector 0 now holds the new pattern; sector 1 still 0x11.
        let mut buf = [0u8; SECTOR_SIZE * 2];
        ata.disk.read_sectors(0, 2, &mut buf);
        assert!(buf[..SECTOR_SIZE].iter().all(|&b| b == 0x77));
        assert!(buf[SECTOR_SIZE..].iter().all(|&b| b == 0x11));
    }

    #[test]
    fn outw_pattern_fills_two_bytes_per_pair_via_1f0_and_1f1() {
        // OUT DX, AX decomposes into write(0x1F0); write(0x1F1).
        // Both must accept buffer bytes while DRQ is up — mirror of
        // the inw drain test.
        let mut ata = Ata::new();
        ata.write(0x1F2, 1);
        ata.write(0x1F6, 0x40);
        ata.write(0x1F7, CMD_WRITE_SECTORS);
        for i in 0..256u32 {
            ata.write(0x1F0, i as u8); // low byte
            ata.write(0x1F1, (i >> 8) as u8); // high byte
        }
        assert!(ata.status & STATUS_DRQ == 0);
        // Read sector 0 back — it must hold the 256-byte pattern.
        let mut buf = [0u8; SECTOR_SIZE];
        ata.disk.read_sectors(0, 1, &mut buf);
        for i in 0..256u32 {
            assert_eq!(buf[(i * 2) as usize], i as u8, "lane {i} low");
            assert_eq!(buf[(i * 2 + 1) as usize], (i >> 8) as u8, "lane {i} high");
        }
    }

    #[test]
    fn inw_pattern_drains_two_bytes_per_pair_via_1f0_and_1f1() {
        // Mimic what our CPU's `IN AX, DX` decomposes into: two
        // consecutive byte reads at 0x1F0 then 0x1F1. With DRQ set,
        // both reads must come from the data buffer (one byte each)
        // so a 256-iteration inw loop drains exactly 512 bytes.
        let mut ata = Ata::new();
        ata.disk.load(&[0x55u8; SECTOR_SIZE]);
        ata.write(0x1F2, 1);
        ata.write(0x1F6, 0x40);
        ata.write(0x1F7, CMD_READ_SECTORS);
        for _ in 0..256 {
            assert_eq!(ata.read(0x1F0), 0x55);
            assert_eq!(ata.read(0x1F1), 0x55);
        }
        assert!(ata.status & STATUS_DRQ == 0);
        // After DRQ clears, 0x1F1 returns the Error register again.
        assert_eq!(ata.read(0x1F1), 0);
    }
}
