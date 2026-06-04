//! Realtek RTL8139 NIC — register file (the 256-byte PIO window behind
//! BAR0) plus the 93C46 serial EEPROM that holds the MAC.
//!
//! The in-guest `8139too` driver touches, to probe the chip and register
//! `eth0`:
//!   * the 93C46 EEPROM (via Cmd9346, reg 0x50) — it reads the MAC from
//!     EEPROM words 7-9, NOT from IDR0-5, so the EEPROM must be modeled or
//!     the MAC comes out 00:00:00:00:00:00.
//!   * IDR0-5 (0x00-0x05) — the MAC, also mirrored here (read-only).
//!   * ChipCmd (0x37) — bit 4 = software reset; auto-completed (cleared
//!     immediately) so the driver's reset poll terminates.
//!   * TxConfig (0x40-0x43) — the high byte carries the hardware-version
//!     ID; we report 0x74 (RTL-8139C) so the driver recognizes the chip.
//!
//! TX/RX descriptor rings, the interrupt, and link state are NOT modeled
//! yet (Phase A3b+). Unmodeled registers read/write as plain RAM, which is
//! enough to get the driver bound and `eth0` created with a real MAC. The
//! window is dispatched by `IoBus` at the kernel-assigned BAR0 base.

/// Default MAC — a locally-administered address (the `52:54:00` QEMU-style
/// prefix), so it's obviously a virtual NIC.
const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// TxConfig high byte (offset 0x43) hardware-version field: 0x74 selects
/// "RTL-8139C" in the driver's chip table.
const HW_VERSION_HI: u8 = 0x74;

// Cmd9346 (reg 0x50) bits driving the 93C46 serial EEPROM.
const EE_CS: u8 = 0x08; // chip select
const EE_SK: u8 = 0x04; // serial clock
const EE_DI: u8 = 0x02; // data in (host → eeprom)
const EE_DO: u8 = 0x01; // data out (eeprom → host)

pub struct Rtl8139 {
    regs: [u8; 256],
    /// 93C46 EEPROM: 64 × 16-bit words. Word 0 = 0x8129 (the RTL8139
    /// signature); words 7-9 hold the MAC (little-endian).
    eeprom: [u16; 64],
    // Microwire serial state for the EEPROM bit-bang on reg 0x50.
    ee_prev_sk: bool,
    ee_cs: bool,
    ee_waiting_start: bool,
    ee_reading: bool,
    ee_cmd: u16,
    ee_count: u8,
    ee_shift: u16,
    ee_dobit: u8,
    /// Frames the driver kicked for transmit, as (guest-physical addr,
    /// length). The device can't read guest RAM itself, so the VM loop
    /// drains this, DMAs each frame out, and the bytes go to the host.
    tx_pending: Vec<(u32, u16)>,
    /// RX read pointer (offset into the ring), the chip's internal
    /// `RxBufPtr`. Resets to 0 and is reloaded as `(CAPR + 16) mod len`
    /// only when the driver *writes* CAPR — the hardware's −16 quirk. We
    /// can't derive it from CAPR alone (reset 0 vs a written 0 differ), so
    /// it's tracked explicitly. CBR (reg 0x3A) is the matching write ptr.
    rx_rptr: u32,
}

// RTL8139 register offsets (within the BAR0 I/O window).
const TSD0: usize = 0x10; // TxStatus0-3 at 0x10/0x14/0x18/0x1C
const TSAD0: usize = 0x20; // TxAddr0-3 at 0x20/0x24/0x28/0x2C
const IMR: usize = 0x3C; // interrupt mask (16-bit)
const ISR: usize = 0x3E; // interrupt status (16-bit, write-1-to-clear)
const TSD_OWN: u32 = 1 << 13; // descriptor owned by NIC (driver clears to TX)
const TSD_TOK: u32 = 1 << 15; // transmit OK (NIC sets on completion)
const TSD_SIZE_MASK: u32 = 0x1FFF; // frame length, bits 0..12
const ISR_TOK: u16 = 1 << 2; // transmit-OK interrupt
const ISR_ROK: u16 = 1 << 0; // receive-OK interrupt

// Receive path.
const RBSTART: usize = 0x30; // RX ring base, guest-physical (32-bit)
const CAPR: usize = 0x38; // Current Address of Packet Read (driver's rptr − 16)
const CBR: usize = 0x3A; // Current Buffer Address (NIC's write offset)
const RCR: usize = 0x44; // RX Configuration (buffer-length + WRAP bits)
const CMD_RX_ENABLE: u8 = 1 << 3; // ChipCmd: RX engine enabled
const CMD_BUFE: u8 = 1 << 0; // ChipCmd: RX buffer empty (read-only status)
const RX_STATUS_ROK: u16 = 0x0001; // per-packet RX header status: receive OK
const ETH_MIN_FRAME: usize = 60; // minimum Ethernet frame (sans 4-byte CRC)
const RX_CRC_LEN: usize = 4; // trailing FCS the driver strips off

impl Rtl8139 {
    pub fn new() -> Self {
        Self::with_mac(DEFAULT_MAC)
    }

    /// Reassign the MAC in place — both IDR0-5 and the EEPROM words 7-9 the
    /// driver actually reads. Must be done BEFORE the guest's driver binds (it
    /// latches the MAC from EEPROM at probe), i.e. before boot. Used to give
    /// each VM on a virtual LAN a distinct address (parallel VMs from the same
    /// image would otherwise all share `DEFAULT_MAC` and collide).
    pub fn set_mac(&mut self, mac: [u8; 6]) {
        self.regs[0..6].copy_from_slice(&mac);
        self.eeprom[7] = mac[0] as u16 | ((mac[1] as u16) << 8);
        self.eeprom[8] = mac[2] as u16 | ((mac[3] as u16) << 8);
        self.eeprom[9] = mac[4] as u16 | ((mac[5] as u16) << 8);
    }

    pub fn with_mac(mac: [u8; 6]) -> Self {
        let mut regs = [0u8; 256];
        regs[0..6].copy_from_slice(&mac);
        let mut eeprom = [0u16; 64];
        eeprom[0] = 0x8129; // RTL8139 EEPROM signature
        eeprom[7] = mac[0] as u16 | ((mac[1] as u16) << 8);
        eeprom[8] = mac[2] as u16 | ((mac[3] as u16) << 8);
        eeprom[9] = mac[4] as u16 | ((mac[5] as u16) << 8);
        Self {
            regs,
            eeprom,
            ee_prev_sk: false,
            ee_cs: false,
            ee_waiting_start: true,
            ee_reading: false,
            ee_cmd: 0,
            ee_count: 0,
            ee_shift: 0,
            ee_dobit: 0,
            tx_pending: Vec::new(),
            rx_rptr: 0,
        }
    }

    /// Read a little-endian u32 from the register file.
    fn reg_u32(&self, off: usize) -> u32 {
        u32::from_le_bytes([
            self.regs[off],
            self.regs[off + 1],
            self.regs[off + 2],
            self.regs[off + 3],
        ])
    }
    fn set_reg_u32(&mut self, off: usize, v: u32) {
        self.regs[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn reg_u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.regs[off], self.regs[off + 1]])
    }

    /// True when the NIC is asserting its interrupt line — any interrupt
    /// status bit that the mask (IMR) has enabled. The VM wires this into
    /// the PIC (IRQ 11).
    pub fn irq_pending(&self) -> bool {
        self.reg_u16(ISR) & self.reg_u16(IMR) != 0
    }

    /// True when the driver has kicked one or more transmits that the VM
    /// hasn't drained yet — a cheap guard so the step loop only does the
    /// bus-master copy when there's actually a frame waiting.
    pub fn has_pending_tx(&self) -> bool {
        !self.tx_pending.is_empty()
    }

    /// Drain the frames the driver queued for transmit, as (guest-physical
    /// addr, len). The VM reads each from RAM and sends it to the host. TX
    /// is reported complete synchronously (OWN+TOK already set), so the
    /// caller must copy the bytes out before the driver reuses the buffer.
    pub fn take_tx_frames(&mut self) -> Vec<(u32, u16)> {
        core::mem::take(&mut self.tx_pending)
    }

    /// Mark a receive-OK interrupt (the VM calls this after DMAing a frame
    /// into the RX ring). Sets ISR.ROK; the VM then re-checks irq_pending.
    pub fn signal_rx_ok(&mut self) {
        let isr = self.reg_u16(ISR) | ISR_ROK;
        self.regs[ISR..ISR + 2].copy_from_slice(&isr.to_le_bytes());
    }

    /// RX ring length in bytes, selected by RCR bits 11-12 (00=8K, 01=16K,
    /// 10=32K, 11=64K). This is the modulo length the driver uses; the
    /// driver allocates an extra ~1.5 KB of slack so a packet written near
    /// the end (WRAP/RxNoWrap mode) can spill past it without wrapping.
    fn rx_buf_len(&self) -> u32 {
        8192u32 << ((self.reg_u32(RCR) >> 11) & 0x3)
    }

    /// True when the RX ring holds no unread packets — the NIC's write
    /// offset (CBR) has caught up to the driver's read offset (`rx_rptr`).
    /// The driver polls this via ChipCmd bit 0 (BUFE) to drain its rx loop.
    fn rx_buffer_empty(&self) -> bool {
        let len = self.rx_buf_len();
        (self.reg_u16(CBR) as u32 % len) == self.rx_rptr % len
    }

    /// Accept one inbound Ethernet frame (L2, no CRC) into the RX ring.
    /// Returns the guest-physical destination and the exact bytes to write
    /// there — the device can't touch RAM, so the VM performs the DMA — or
    /// `None` if RX is disabled or the ring lacks room (frame dropped).
    ///
    /// Layout per the RTL8139 legacy receiver: a 4-byte header (u16 status,
    /// u16 length-including-CRC) then the frame, a 4-byte dummy CRC, padded
    /// to a dword. We assume RxNoWrap (8139too's mode): the packet is
    /// written contiguously from the current offset, spilling into the
    /// driver's slack rather than wrapping mid-packet; CBR wraps for the
    /// next one. ISR.ROK is raised so the VM's refresh asserts IRQ 11.
    pub fn accept_rx(&mut self, frame: &[u8]) -> Option<(u32, Vec<u8>)> {
        if self.regs[0x37] & CMD_RX_ENABLE == 0 {
            return None;
        }
        // Pad runt frames to the Ethernet minimum, as a real NIC does.
        let payload = frame.len().max(ETH_MIN_FRAME);
        let rx_size = payload + RX_CRC_LEN; // length field counts the CRC
        let total = (4 + rx_size).next_multiple_of(4); // header + data, dword-aligned

        let len = self.rx_buf_len();
        if total as u32 >= len {
            return None; // frame larger than the whole ring — drop
        }
        let wptr = self.reg_u16(CBR) as u32 % len;
        let rptr = self.rx_rptr % len;
        let unread = (wptr + len - rptr) % len;
        if unread + total as u32 >= len {
            return None; // would overrun unread data — drop
        }

        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&RX_STATUS_ROK.to_le_bytes());
        buf.extend_from_slice(&(rx_size as u16).to_le_bytes());
        buf.extend_from_slice(frame);
        buf.resize(total, 0); // pad payload to ETH_MIN, CRC, and dword align

        let dest = self.reg_u32(RBSTART).wrapping_add(wptr);
        let new_cbr = ((wptr + total as u32) % len) as u16;
        self.regs[CBR..CBR + 2].copy_from_slice(&new_cbr.to_le_bytes());
        self.signal_rx_ok();
        Some((dest, buf))
    }

    /// Read one byte from the register window (offset within BAR0).
    pub fn read_reg(&self, off: u16) -> u8 {
        let off = (off & 0xFF) as usize;
        match off {
            // Cmd9346: the EEPROM data-out bit lives in bit 0; the other
            // bits read back what was last written (mode/CS/SK/DI).
            0x50 => (self.regs[0x50] & !EE_DO) | self.ee_dobit,
            // TxConfig high byte: report the RTL-8139C hardware version.
            0x43 => HW_VERSION_HI,
            // BasicModeStatus (BMSR, the internal PHY's MII status, 0x64):
            // report link-up + autoneg-complete + 10/100 capable (0x782D)
            // so `mii` brings the carrier on and the interface can go UP.
            0x64 => 0x2D,
            0x65 => 0x78,
            // ChipCmd: bit 0 (BUFE) is a live status bit the driver polls to
            // tell whether the RX ring has an unread packet; the stored
            // RX/TX-enable bits ride alongside it.
            0x37 => {
                if self.rx_buffer_empty() {
                    self.regs[0x37] | CMD_BUFE
                } else {
                    self.regs[0x37] & !CMD_BUFE
                }
            }
            _ => self.regs[off],
        }
    }

    /// Write one byte to the register window.
    pub fn write_reg(&mut self, off: u16, value: u8) {
        let off = (off & 0xFF) as usize;
        match off {
            // Cmd9346 — drive the 93C46 serial state machine, then store.
            0x50 => {
                self.eeprom_clock(value);
                self.regs[0x50] = value;
            }
            // ChipCmd: the reset bit (0x10) auto-completes — clear it at
            // once so the driver's "wait for reset" poll terminates.
            0x37 => self.regs[0x37] = value & !0x10,
            // IDR0-5 hold the MAC; read-only.
            0x00..=0x05 => {}
            // ISR (interrupt status) is write-1-to-clear: the driver acks
            // an interrupt by writing 1s to the bits it handled.
            0x3E | 0x3F => self.regs[off] &= !value,
            // CAPR (RxBufPtr): the driver advances its read pointer here as
            // it drains the ring. The chip stores it −16 (a hardware quirk),
            // so the real read offset is CAPR + 16. Reload rx_rptr after the
            // 16-bit write completes (high byte at 0x39 lands last).
            0x38 | 0x39 => {
                self.regs[off] = value;
                self.rx_rptr = (self.reg_u16(CAPR) as u32).wrapping_add(16) % self.rx_buf_len();
            }
            // TSD0-3 high byte completes a 32-bit transmit-descriptor write.
            // The 8139too driver writes the full dword (size + OWN cleared)
            // to kick TX; we queue the frame and report TX done at once.
            0x13 | 0x17 | 0x1B | 0x1F => {
                self.regs[off] = value;
                let n = (off - 0x13) / 4; // descriptor index 0..3
                let tsd_off = TSD0 + n * 4;
                let tsd = self.reg_u32(tsd_off);
                if tsd & TSD_OWN == 0 {
                    let addr = self.reg_u32(TSAD0 + n * 4);
                    let size = (tsd & TSD_SIZE_MASK) as u16;
                    self.tx_pending.push((addr, size));
                    // Synchronous completion: NIC now owns the descriptor
                    // (OWN) and the transmit is OK (TOK); raise the TOK intr.
                    self.set_reg_u32(tsd_off, tsd | TSD_OWN | TSD_TOK);
                    let isr = self.reg_u16(ISR) | ISR_TOK;
                    self.regs[ISR..ISR + 2].copy_from_slice(&isr.to_le_bytes());
                }
            }
            _ => self.regs[off] = value,
        }
    }

    /// Advance the 93C46 Microwire read protocol on a Cmd9346 write.
    /// Command framing (a 93C46, 6-bit address): after CS rises, ignore
    /// leading zeros until the start bit (1), then 2 opcode bits + 6
    /// address bits; READ (opcode 10) then shifts the 16-bit word out MSB
    /// first on EEDO, one bit per SK rising edge.
    fn eeprom_clock(&mut self, value: u8) {
        let cs = value & EE_CS != 0;
        let sk = value & EE_SK != 0;
        let di = value & EE_DI != 0;
        if !cs {
            self.ee_cs = false;
            self.ee_waiting_start = true;
            self.ee_reading = false;
            self.ee_count = 0;
            self.ee_prev_sk = sk;
            return;
        }
        if !self.ee_cs {
            // CS rising — begin a fresh command.
            self.ee_waiting_start = true;
            self.ee_reading = false;
            self.ee_cmd = 0;
            self.ee_count = 0;
        }
        self.ee_cs = true;
        if sk && !self.ee_prev_sk {
            // SK rising edge.
            if self.ee_waiting_start {
                if di {
                    self.ee_waiting_start = false;
                    self.ee_cmd = 0;
                    self.ee_count = 0;
                }
            } else if !self.ee_reading {
                self.ee_cmd = (self.ee_cmd << 1) | u16::from(di);
                self.ee_count += 1;
                if self.ee_count == 8 {
                    let opcode = (self.ee_cmd >> 6) & 0x3;
                    let addr = (self.ee_cmd & 0x3F) as usize;
                    if opcode == 0b10 {
                        // READ — latch the word; bits clock out next edges.
                        self.ee_shift = self.eeprom[addr];
                        self.ee_reading = true;
                    }
                    // Other opcodes (WRITE/EWEN/…) are no-ops — read-only.
                }
            } else {
                // Shift out the next data bit (MSB first).
                self.ee_dobit = ((self.ee_shift >> 15) & 1) as u8;
                self.ee_shift <<= 1;
            }
        }
        self.ee_prev_sk = sk;
    }

    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.regs);
        for w in &self.eeprom {
            out.extend_from_slice(&w.to_le_bytes());
        }
        // RX read pointer — derived state that can't be reconstructed from
        // CAPR alone. Appended after the original 384-byte layout.
        out.extend_from_slice(&self.rx_rptr.to_le_bytes());
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        // 256 register bytes + 64 EEPROM words (128 bytes) = 384, then an
        // optional 4-byte rx_rptr (absent in pre-RX snapshots → 0).
        if bytes.len() < 384 {
            return Err("rtl8139: truncated");
        }
        self.regs.copy_from_slice(&bytes[..256]);
        for (i, w) in self.eeprom.iter_mut().enumerate() {
            let o = 256 + i * 2;
            *w = u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        }
        if bytes.len() >= 388 {
            self.rx_rptr = u32::from_le_bytes([bytes[384], bytes[385], bytes[386], bytes[387]]);
            Ok(388)
        } else {
            self.rx_rptr = 0;
            Ok(384)
        }
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

    /// Drive the 93C46 READ of `word` the way `8139too` does (a 93C46 with
    /// a 6-bit address): leading zeros, start bit, opcode 10, 6 addr bits,
    /// then clock out 16 bits — and return the assembled word.
    fn eeprom_read(nic: &mut Rtl8139, word: u8) -> u16 {
        let base = EE_CS; // programming mode bits omitted; CS is what matters
        nic.write_reg(0x50, 0); // deselect
        nic.write_reg(0x50, base); // CS high
                                   // command bits MSB-first: 0,0,1(start),1,0(op=READ),a5..a0
        let cmd_bits = [
            0u8,
            0,
            1,
            1,
            0,
            (word >> 5) & 1,
            (word >> 4) & 1,
            (word >> 3) & 1,
            (word >> 2) & 1,
            (word >> 1) & 1,
            word & 1,
        ];
        for b in cmd_bits {
            let di = if b != 0 { EE_DI } else { 0 };
            nic.write_reg(0x50, base | di); // SK low, DI set
            nic.write_reg(0x50, base | di | EE_SK); // SK high (rising edge)
        }
        nic.write_reg(0x50, base); // SK low between command and read (as the driver does)
        let mut val = 0u16;
        for _ in 0..16 {
            nic.write_reg(0x50, base | EE_SK); // SK high → presents next bit
            val = (val << 1) | (nic.read_reg(0x50) & EE_DO) as u16;
            nic.write_reg(0x50, base); // SK low
        }
        nic.write_reg(0x50, 0); // CS low
        val
    }

    #[test]
    fn eeprom_returns_mac_words() {
        let mut nic = Rtl8139::new();
        // Words 7-9 are the MAC (little-endian): 52:54:00:12:34:56.
        assert_eq!(eeprom_read(&mut nic, 7), 0x5452);
        assert_eq!(eeprom_read(&mut nic, 8), 0x1200);
        assert_eq!(eeprom_read(&mut nic, 9), 0x5634);
        // Word 0 is the signature.
        assert_eq!(eeprom_read(&mut nic, 0), 0x8129);
    }

    #[test]
    fn eeprom_custom_mac() {
        let mut nic = Rtl8139::with_mac([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);
        assert_eq!(eeprom_read(&mut nic, 7), 0xADDE);
        assert_eq!(eeprom_read(&mut nic, 8), 0xEFBE);
        assert_eq!(eeprom_read(&mut nic, 9), 0x0100);
    }

    #[test]
    fn set_mac_updates_eeprom_and_idr() {
        // A default NIC reassigned in place exposes the new MAC via both the
        // EEPROM words the driver reads and the IDR registers.
        let mut nic = Rtl8139::new();
        nic.set_mac([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x02]);
        assert_eq!(eeprom_read(&mut nic, 7), 0xADDE);
        assert_eq!(eeprom_read(&mut nic, 8), 0xEFBE);
        assert_eq!(eeprom_read(&mut nic, 9), 0x0200);
        assert_eq!(nic.read_reg(0), 0xDE);
        assert_eq!(nic.read_reg(5), 0x02);
    }

    #[test]
    fn mac_address_reads_back_from_idr() {
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

    // Write a 32-bit NIC register via 4 byte writes (as the guest's 32-bit
    // OUT decomposes), low byte first.
    fn wreg32(nic: &mut Rtl8139, off: u16, val: u32) {
        for i in 0..4u16 {
            nic.write_reg(off + i, (val >> (i * 8)) as u8);
        }
    }

    #[test]
    fn tx_kick_queues_frame_and_signals_tok() {
        let mut nic = Rtl8139::new();
        // Driver: set TX buffer address (TSAD0), then write TSD0 with the
        // size and OWN cleared to start the transmit.
        wreg32(&mut nic, 0x20, 0x0010_0000); // TSAD0 = guest phys 0x100000
        wreg32(&mut nic, 0x10, 64); // TSD0 = size 64, OWN=0
        let frames = nic.take_tx_frames();
        assert_eq!(frames, vec![(0x0010_0000u32, 64u16)]);
        // The descriptor is reported complete (OWN + TOK set).
        assert_ne!(nic.read_reg(0x11) & 0x20, 0, "OWN set");
        assert_ne!(nic.read_reg(0x11) & 0x80, 0, "TOK set");
        // ISR.TOK (bit 2) raised.
        assert_ne!(nic.read_reg(0x3E) & 0x04, 0, "ISR TOK");
        // A second drain is empty.
        assert!(nic.take_tx_frames().is_empty());
    }

    #[test]
    fn tx_uses_the_right_descriptor() {
        let mut nic = Rtl8139::new();
        wreg32(&mut nic, 0x28, 0xDEAD_0000); // TSAD2
        wreg32(&mut nic, 0x18, 128); // TSD2, size 128
        assert_eq!(nic.take_tx_frames(), vec![(0xDEAD_0000u32, 128u16)]);
    }

    #[test]
    fn irq_pending_tracks_isr_and_mask() {
        let mut nic = Rtl8139::new();
        wreg32(&mut nic, 0x20, 0x1000);
        wreg32(&mut nic, 0x10, 60); // TX → sets ISR.TOK
        assert!(!nic.irq_pending(), "masked: IMR=0");
        // Unmask TOK in IMR (0x3C).
        nic.write_reg(0x3C, 0x04);
        assert!(nic.irq_pending(), "TOK unmasked → IRQ");
        // Driver acks by writing 1 to ISR.TOK (write-1-to-clear).
        nic.write_reg(0x3E, 0x04);
        assert!(!nic.irq_pending(), "ISR cleared → no IRQ");
    }

    #[test]
    fn bmsr_reports_link_up() {
        let nic = Rtl8139::new();
        let bmsr = (nic.read_reg(0x64) as u16) | ((nic.read_reg(0x65) as u16) << 8);
        assert_ne!(bmsr & 0x0004, 0, "link-status bit set");
        assert_ne!(bmsr & 0x0020, 0, "autoneg-complete bit set");
    }

    #[test]
    fn signal_rx_ok_sets_isr() {
        let mut nic = Rtl8139::new();
        nic.signal_rx_ok();
        assert_ne!(nic.read_reg(0x3E) & 0x01, 0, "ISR ROK");
    }

    // Bring the RX engine online exactly as 8139too's open() does: program
    // the ring base (RBSTART), default RCR (8 KB buffer), initialise CAPR to
    // −16 (0xFFF0, the driver's quirk), and set ChipCmd RxEnable.
    fn rx_ready(rbstart: u32) -> Rtl8139 {
        let mut nic = Rtl8139::new();
        wreg32(&mut nic, 0x30, rbstart); // RBSTART
        nic.write_reg(0x38, 0xF0); // CAPR = 0xFFF0
        nic.write_reg(0x39, 0xFF);
        nic.write_reg(0x37, 0x08); // ChipCmd: RxEnable
        nic
    }

    #[test]
    fn rx_wraps_contiguously_past_ring_end() {
        // A long transfer eventually writes a packet near the ring's end.
        // With RxNoWrap the packet is laid down contiguously (spilling into
        // the driver's slack) from the current offset, and CBR wraps for the
        // next one — the behaviour 8139too relies on.
        let rbstart = 0x6_0000u32;
        let mut nic = rx_ready(rbstart);
        let len = 8192u32; // default RCR → 8 KB ring
                           // Put the write pointer 10 bytes from the end, read pointer alongside
                           // it so there's room (CBR = rptr → empty).
        nic.write_reg(0x3A, ((len - 10) & 0xFF) as u8);
        nic.write_reg(0x3B, (((len - 10) >> 8) & 0xFF) as u8);
        // CAPR = (len-10) - 16 so rx_rptr = (CAPR+16)%len = len-10 = CBR.
        let capr = len - 26;
        nic.write_reg(0x38, (capr & 0xFF) as u8);
        nic.write_reg(0x39, ((capr >> 8) & 0xFF) as u8);

        let frame = vec![0x7E; 64];
        let (dest, bytes) = nic.accept_rx(&frame).expect("accepted");
        // Written contiguously at the old offset (into the slack region).
        assert_eq!(dest, rbstart + (len - 10));
        assert_eq!(bytes.len(), 72);
        // CBR wrapped: (len-10 + 72) mod len = 62.
        let cbr = nic.read_reg(0x3A) as u32 | ((nic.read_reg(0x3B) as u32) << 8);
        assert_eq!(cbr, 62);
    }

    #[test]
    fn tx_multiple_descriptors_drain_in_order() {
        // A burst uses all four TX descriptors round-robin; the VM must see
        // each queued frame, in kick order.
        let mut nic = Rtl8139::new();
        for (i, (tsad, tsd)) in [(0x20, 0x10), (0x24, 0x14), (0x28, 0x18), (0x2C, 0x1C)]
            .into_iter()
            .enumerate()
        {
            wreg32(&mut nic, tsad as u16, 0x1000 * (i as u32 + 1));
            wreg32(&mut nic, tsd as u16, 100 + i as u32);
        }
        let frames = nic.take_tx_frames();
        assert_eq!(
            frames,
            vec![
                (0x1000, 100u16),
                (0x2000, 101),
                (0x3000, 102),
                (0x4000, 103),
            ]
        );
    }

    #[test]
    fn rx_accept_writes_header_and_marks_data_available() {
        let rbstart = 0x4_0000u32;
        let mut nic = rx_ready(rbstart);
        // Fresh ring: BUFE set (empty), no ROK yet.
        assert_ne!(nic.read_reg(0x37) & 0x01, 0, "BUFE set when empty");

        let frame = vec![0xAA; 64];
        let (dest, bytes) = nic.accept_rx(&frame).expect("frame accepted");
        assert_eq!(dest, rbstart, "first packet lands at the ring base");
        // Header: status ROK, length = frame + 4-byte CRC.
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0x0001);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 68);
        assert_eq!(&bytes[4..4 + 64], &frame[..], "frame copied after header");
        // total = align_up(4 header + 68, 4) = 72.
        assert_eq!(bytes.len(), 72);
        // CBR advanced; ROK raised; ring no longer empty.
        let cbr = nic.read_reg(0x3A) as u16 | ((nic.read_reg(0x3B) as u16) << 8);
        assert_eq!(cbr, 72);
        assert_ne!(nic.read_reg(0x3E) & 0x01, 0, "ISR.ROK raised");
        assert_eq!(nic.read_reg(0x37) & 0x01, 0, "BUFE clear — packet waiting");

        // Driver consumes the packet: cur_rx = 72 → CAPR = 72 − 16 = 56.
        nic.write_reg(0x38, 56);
        nic.write_reg(0x39, 0);
        assert_ne!(
            nic.read_reg(0x37) & 0x01,
            0,
            "BUFE set once read ptr catches up"
        );
    }

    #[test]
    fn rx_runt_frame_padded_to_min() {
        let mut nic = rx_ready(0x1000);
        let frame = vec![0x11; 20]; // a 20-byte runt
        let (_, bytes) = nic.accept_rx(&frame).expect("accepted");
        // Length field is the padded minimum (60) plus the 4-byte CRC.
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 64);
        assert_eq!(bytes.len(), 68); // align_up(4 + 64, 4)
        assert_eq!(&bytes[4..24], &frame[..], "real bytes preserved");
        assert!(bytes[24..].iter().all(|&b| b == 0), "padding is zero");
    }

    #[test]
    fn rx_dropped_when_engine_disabled() {
        let mut nic = Rtl8139::new(); // RxEnable never set
        assert!(nic.accept_rx(&[0u8; 64]).is_none());
    }

    #[test]
    fn rx_ring_full_drops_until_drained() {
        let mut nic = rx_ready(0x2000);
        let frame = vec![0x22; 60];
        let mut accepted = 0;
        // The driver never advances CAPR, so the 8 KB ring eventually fills.
        while nic.accept_rx(&frame).is_some() {
            accepted += 1;
            assert!(accepted < 1000, "must stop, not loop forever");
        }
        assert!(accepted > 0, "some frames fit");
        assert!(nic.accept_rx(&frame).is_none(), "still full");
    }

    #[test]
    fn snapshot_round_trips() {
        let mut nic = rx_ready(0x5000);
        nic.write_reg(0x3C, 0x05);
        // Receive a frame so CBR and rx_rptr hold non-trivial state.
        nic.accept_rx(&[0x55; 64]).expect("accepted");
        let mut buf = Vec::new();
        nic.snapshot_into(&mut buf);
        assert_eq!(buf.len(), 388); // 256 regs + 128 EEPROM + 4 rx_rptr
        let mut nic2 = Rtl8139::new();
        nic2.restore(&buf).expect("restore");
        assert_eq!(nic2.read_reg(0x3C), 0x05);
        assert_eq!(eeprom_read(&mut nic2, 7), 0x5452);
        // RX ring pointers (CBR + rx_rptr) survive, so BUFE is preserved.
        assert_eq!(nic2.read_reg(0x37) & 0x01, nic.read_reg(0x37) & 0x01);
    }

    #[test]
    fn snapshot_restores_old_384_byte_blob() {
        // A pre-RX snapshot (no trailing rx_rptr) must still load, with
        // rx_rptr defaulting to 0.
        let nic = Rtl8139::new();
        let mut buf = Vec::new();
        nic.snapshot_into(&mut buf);
        buf.truncate(384); // drop the rx_rptr tail → legacy layout
        let mut nic2 = Rtl8139::new();
        assert_eq!(nic2.restore(&buf).expect("restore"), 384);
    }
}
