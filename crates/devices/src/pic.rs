//! 8259A Programmable Interrupt Controller (master) — minimal subset.
//!
//! What we model:
//!   * IMR (mask), IRR (requested), ISR (in-service) — one byte each
//!   * vector base (set by ICW2 or directly on construction)
//!   * Initialization Command Words 1/2 (ICW1 sets init mode + expected
//!     ICWs, ICW2 sets vector base). ICW3/ICW4 are accepted and dropped.
//!   * OCW2 EOI variants:
//!       * 0x20 (non-specific) — clear highest in-service ISR bit
//!       * 0x60 | irq (specific EOI) — clear ISR bit for that IRQ
//!   * OCW3 read-register select (IRR / ISR)
//!   * OCW3 / poll mode — *not* yet implemented
//!
//! What we do *not* model:
//!   * cascading to a slave PIC (no IRQ 8..15)
//!   * priority rotation, specific EOI by IRQ number
//!   * auto-EOI
//!
//! The CPU side asks `pending_vector()` for the highest-priority unmasked
//! pending IRQ; `ack()` moves that bit from IRR to ISR (the standard
//! INTA-cycle effect). Devices push a request with `raise_irq(n)`.

use crate::IoDevice;

pub struct Pic {
    base_port: u16,
    pub vector_base: u8,
    pub imr: u8,
    pub irr: u8,
    pub isr: u8,
    init_state: InitState,
    /// OCW3 read-register select. After the kernel writes OCW3
    /// with bits 1:0 = 10, reads from the command port return
    /// IRR (the default); with bits 1:0 = 11 they return ISR.
    /// Linux uses this to detect spurious IRQ 7 / IRQ 15 by
    /// checking whether the highest in-service bit is actually
    /// set when the handler fires.
    read_select: ReadSelect,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReadSelect {
    Irr,
    Isr,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum InitState {
    /// Normal operation; writes to command port are OCWs.
    Idle,
    /// Saw ICW1; next data-port write is ICW2 (vector base).
    ExpectIcw2,
    /// Got ICW2; next data-port write is ICW3 (cascade — ignored).
    ExpectIcw3,
    /// Got ICW3; next data-port write is ICW4 (mode — ignored).
    ExpectIcw4,
}

impl Pic {
    pub const MASTER_BASE: u16 = 0x20;

    pub fn new(base_port: u16) -> Self {
        Self {
            base_port,
            vector_base: 0x08,
            imr: 0xFF,
            irr: 0,
            isr: 0,
            init_state: InitState::Idle,
            read_select: ReadSelect::Irr,
        }
    }

    pub fn master() -> Self {
        Self::new(Self::MASTER_BASE)
    }

    /// Device-side: assert an IRQ line. n must be 0..=7.
    pub fn raise_irq(&mut self, n: u8) {
        if n < 8 {
            self.irr |= 1 << n;
        }
    }

    /// CPU-side: highest-priority unmasked IRQ that is in IRR but not
    /// in ISR. Returns the vector number (vector_base + irq).
    pub fn pending_vector(&self) -> Option<u8> {
        let candidates = self.irr & !self.imr & !self.isr;
        if candidates == 0 {
            return None;
        }
        let irq = candidates.trailing_zeros() as u8;
        Some(self.vector_base.wrapping_add(irq))
    }

    /// CPU-side: acknowledge the highest-priority pending IRQ (the one
    /// `pending_vector` last returned). Moves the bit from IRR to ISR.
    pub fn ack(&mut self) {
        let candidates = self.irr & !self.imr & !self.isr;
        if candidates == 0 {
            return;
        }
        let irq_bit = candidates & candidates.wrapping_neg();
        self.irr &= !irq_bit;
        self.isr |= irq_bit;
    }

    /// Snapshot: vector_base, imr, irr, isr, init_state (5 bytes).
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.push(self.vector_base);
        out.push(self.imr);
        out.push(self.irr);
        out.push(self.isr);
        out.push(match self.init_state {
            InitState::Idle => 0,
            InitState::ExpectIcw2 => 1,
            InitState::ExpectIcw3 => 2,
            InitState::ExpectIcw4 => 3,
        });
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 5 {
            return Err("pic: truncated");
        }
        self.vector_base = bytes[0];
        self.imr = bytes[1];
        self.irr = bytes[2];
        self.isr = bytes[3];
        self.init_state = match bytes[4] {
            0 => InitState::Idle,
            1 => InitState::ExpectIcw2,
            2 => InitState::ExpectIcw3,
            3 => InitState::ExpectIcw4,
            _ => return Err("pic: bad init_state tag"),
        };
        Ok(5)
    }

    /// Software EOI — clears the highest-priority bit in ISR.
    fn non_specific_eoi(&mut self) {
        if self.isr == 0 {
            return;
        }
        let bit = self.isr & self.isr.wrapping_neg();
        self.isr &= !bit;
    }

    /// Specific EOI — clears the named IRQ's bit in ISR regardless of
    /// priority order. Used when the handler is for a lower-priority
    /// IRQ that fired after a higher-priority one already EOIed.
    fn specific_eoi(&mut self, irq: u8) {
        if irq < 8 {
            self.isr &= !(1 << irq);
        }
    }
}

impl IoDevice for Pic {
    fn port_range(&self) -> (u16, u16) {
        (self.base_port, self.base_port + 1)
    }

    fn read(&mut self, port: u16) -> u8 {
        match port - self.base_port {
            0 => match self.read_select {
                ReadSelect::Irr => self.irr,
                ReadSelect::Isr => self.isr,
            },
            1 => self.imr,
            _ => 0,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base_port {
            0 => {
                // Command port. ICW1 has bit 4 set; OCW2/OCW3 don't.
                if value & 0x10 != 0 {
                    self.init_state = InitState::ExpectIcw2;
                    self.imr = 0;
                    self.isr = 0;
                    self.irr = 0;
                } else if value & 0x18 == 0x08 {
                    // OCW3 (bit 3 set, bit 4 clear). Bits 1:0 select
                    // the read register on subsequent command-port
                    // reads: 10 = IRR (default), 11 = ISR. Linux
                    // writes OCW3 with bits 1:0 = 11 before reading
                    // ISR to detect spurious IRQ 7 / IRQ 15.
                    match value & 0b11 {
                        0b10 => self.read_select = ReadSelect::Irr,
                        0b11 => self.read_select = ReadSelect::Isr,
                        _ => {}
                    }
                } else if value & 0x18 == 0x00 {
                    // OCW2. Bits 7:5 select the operation:
                    //   001 (0x20) — non-specific EOI
                    //   011 (0x60) — specific EOI; bits 2:0 = IRQ
                    // Rotate variants (100/101/110/111) parse but
                    // don't take effect — we ignore the rotation
                    // hint since `pending_vector` always picks the
                    // lowest-numbered IRR bit anyway.
                    match value & 0xE0 {
                        0x20 => self.non_specific_eoi(),
                        0x60 => self.specific_eoi(value & 0x07),
                        0xA0 => self.non_specific_eoi(),
                        0xE0 => self.specific_eoi(value & 0x07),
                        _ => {}
                    }
                }
            }
            1 => match self.init_state {
                InitState::Idle => self.imr = value,
                InitState::ExpectIcw2 => {
                    self.vector_base = value & 0xF8;
                    self.init_state = InitState::ExpectIcw3;
                }
                InitState::ExpectIcw3 => {
                    self.init_state = InitState::ExpectIcw4;
                }
                InitState::ExpectIcw4 => {
                    self.init_state = InitState::Idle;
                }
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OCW3 with bits 1:0 = 11 switches the command-port read to
    /// return ISR. Linux uses this to detect spurious IRQ 7 / 15
    /// — if the handler fires but the matching ISR bit is clear,
    /// the IRQ was spurious and the handler should bail without
    /// sending EOI.
    #[test]
    fn ocw3_isr_select_switches_command_port_read() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(5);
        pic.ack();
        // ISR bit 5 set; IRR is now clear.
        assert_eq!(pic.isr, 1 << 5);
        assert_eq!(pic.irr, 0);

        // Default read returns IRR (= 0).
        assert_eq!(pic.read(0x20), 0);

        // Write OCW3 with bits 1:0 = 11 → switch to ISR.
        // OCW3 layout: 0b0_xx_01_xx_b → bit 3 set, bit 4 clear.
        pic.write(0x20, 0b0000_1011);
        assert_eq!(pic.read(0x20), 1 << 5, "ISR readable via OCW3");

        // Switch back to IRR with bits 1:0 = 10.
        pic.write(0x20, 0b0000_1010);
        assert_eq!(pic.read(0x20), 0, "IRR readable after switch back");
    }

    #[test]
    fn masked_irq_does_not_become_pending() {
        let mut pic = Pic::master();
        pic.raise_irq(3);
        assert!(pic.pending_vector().is_none());
        pic.write(0x21, 0xF7);
        assert_eq!(pic.pending_vector(), Some(0x08 + 3));
    }

    #[test]
    fn ack_moves_request_to_in_service() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(2);
        assert_eq!(pic.pending_vector(), Some(0x08 + 2));
        pic.ack();
        assert_eq!(pic.isr, 1 << 2);
        assert_eq!(pic.irr, 0);
        assert!(pic.pending_vector().is_none());
    }

    #[test]
    fn eoi_clears_isr_top_bit() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(1);
        pic.ack();
        assert!(pic.isr != 0);
        pic.write(0x20, 0x20);
        assert_eq!(pic.isr, 0);
    }

    /// Specific EOI (OCW2 = 0x60 | irq) clears the named bit even
    /// when it's not the highest-priority pending one. Non-specific
    /// EOI by contrast would clear bit 3 (the higher-priority IRQ)
    /// — leaving the IRQ-5 handler with no way to acknowledge.
    #[test]
    fn specific_eoi_clears_named_irq_bit_only() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(3);
        pic.ack();
        pic.raise_irq(5);
        pic.ack();
        // Both IRQ-3 and IRQ-5 are in-service. Non-specific would
        // clear IRQ-3 (lowest bit). Specific-EOI for IRQ-5 clears
        // exactly bit 5.
        assert_eq!(pic.isr, (1 << 3) | (1 << 5));
        pic.write(0x20, 0x60 | 5);
        assert_eq!(pic.isr, 1 << 3, "only IRQ-5 cleared");
        pic.write(0x20, 0x60 | 3);
        assert_eq!(pic.isr, 0);
    }

    /// Rotate-on-specific-EOI (OCW2 = 0xE0 | irq) still EOIs the
    /// named bit — we don't model the rotation hint, but the EOI
    /// half of the operation must take effect.
    #[test]
    fn rotate_specific_eoi_still_clears_named_bit() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(2);
        pic.ack();
        assert_eq!(pic.isr, 1 << 2);
        pic.write(0x20, 0xE0 | 2);
        assert_eq!(pic.isr, 0);
    }

    #[test]
    fn icw_sequence_sets_vector_base() {
        let mut pic = Pic::master();
        // Standard PC remap to vector 0x30..0x37
        pic.write(0x20, 0x11);
        pic.write(0x21, 0x30);
        pic.write(0x21, 0x04);
        pic.write(0x21, 0x01);
        assert_eq!(pic.vector_base, 0x30);
        pic.write(0x21, 0xFE);
        assert_eq!(pic.imr, 0xFE);
    }

    #[test]
    fn higher_priority_irq_wins() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(7);
        pic.raise_irq(0);
        assert_eq!(pic.pending_vector(), Some(0x08));
    }
}
