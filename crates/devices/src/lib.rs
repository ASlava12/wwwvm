//! IO-port-mapped devices visible to the CPU.
//!
//! Right now this is a 16550-shaped UART on COM1 (0x3F8..0x3FF). The
//! UART is the channel between guest and host: the guest writes bytes
//! that JS reads as terminal output, and JS pushes bytes that the guest
//! reads as keystrokes / pre-canned commands.
//!
//! `IoBus` is concrete (not a trait-object container) on purpose: the
//! VM needs typed access to the UART, and we only have a handful of
//! devices. When we add a second device kind we will grow `IoBus` with
//! another typed field and another range in the dispatch match.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// Trait describing the shape every port-mapped device must satisfy.
/// Kept as documentation of the contract even though `IoBus` currently
/// dispatches to concrete types.
pub trait IoDevice {
    fn port_range(&self) -> (u16, u16);
    fn read(&mut self, port: u16) -> u8;
    fn write(&mut self, port: u16, value: u8);
}

/// 16550-style UART, COM1 by default.
///
/// Minimal subset:
///   * THR (offset 0) — write transmits a byte to host output buffer
///   * RBR (offset 0) — read pops a byte from host input buffer
///   * LSR (offset 5) — bit 0 (DR) = input available, bit 5 (THRE) = always 1
///
/// Other registers (IER, IIR, MCR, scratch, DLAB) return 0 and accept
/// writes silently. Enough for a guest that polls LSR.
pub struct Uart {
    base: u16,
    tx_buffer: Vec<u8>,
    rx_buffer: VecDeque<u8>,
}

impl Uart {
    pub const COM1_BASE: u16 = 0x3F8;

    pub fn new(base: u16) -> Self {
        Self {
            base,
            tx_buffer: Vec::new(),
            rx_buffer: VecDeque::new(),
        }
    }

    pub fn com1() -> Self {
        Self::new(Self::COM1_BASE)
    }

    pub fn base(&self) -> u16 {
        self.base
    }

    /// Drain everything the guest has transmitted since the last call.
    pub fn drain_tx(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.tx_buffer)
    }

    /// Queue bytes for the guest to read via RBR.
    pub fn push_rx(&mut self, bytes: &[u8]) {
        self.rx_buffer.extend(bytes.iter().copied());
    }

    pub fn rx_pending(&self) -> usize {
        self.rx_buffer.len()
    }
}

impl IoDevice for Uart {
    fn port_range(&self) -> (u16, u16) {
        (self.base, self.base + 7)
    }

    fn read(&mut self, port: u16) -> u8 {
        match port - self.base {
            0 => self.rx_buffer.pop_front().unwrap_or(0),
            5 => {
                let dr = if self.rx_buffer.is_empty() { 0 } else { 1 };
                let thre = 1 << 5;
                dr | thre
            }
            _ => 0,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        if port - self.base == 0 {
            self.tx_buffer.push(value);
        }
    }
}

/// 8259A Programmable Interrupt Controller (master) — minimal subset.
///
/// What we model:
///   * IMR (mask), IRR (requested), ISR (in-service) — one byte each
///   * vector base (set by ICW2 or directly on construction)
///   * Initialization Command Words 1/2 (ICW1 sets init mode + expected
///     ICWs, ICW2 sets vector base). ICW3/ICW4 are accepted and dropped.
///   * OCW2 = EOI (any value with the bottom bits looking like 0x20)
///   * OCW3 / read-back / poll mode — *not* yet implemented
///
/// What we do *not* model:
///   * cascading to a slave PIC (no IRQ 8..15)
///   * priority rotation, specific EOI by IRQ number
///   * auto-EOI
///
/// The CPU side asks `pending_vector()` for the highest-priority unmasked
/// pending IRQ; `ack()` moves that bit from IRR to ISR (the standard
/// INTA-cycle effect). Devices push a request with `raise_irq(n)`.
pub struct Pic {
    base_port: u16,
    pub vector_base: u8,
    pub imr: u8,
    pub irr: u8,
    pub isr: u8,
    init_state: InitState,
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
            vector_base: 0x08, // PC/BIOS default for the master PIC
            imr: 0xFF,         // start fully masked
            irr: 0,
            isr: 0,
            init_state: InitState::Idle,
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
        let irq_bit = candidates & candidates.wrapping_neg(); // lowest set bit
        self.irr &= !irq_bit;
        self.isr |= irq_bit;
    }

    /// Software EOI — clears the highest-priority bit in ISR.
    fn non_specific_eoi(&mut self) {
        if self.isr == 0 {
            return;
        }
        // Clear the lowest set bit (highest priority on a non-rotated PIC)
        let bit = self.isr & self.isr.wrapping_neg();
        self.isr &= !bit;
    }
}

impl IoDevice for Pic {
    fn port_range(&self) -> (u16, u16) {
        (self.base_port, self.base_port + 1)
    }

    fn read(&mut self, port: u16) -> u8 {
        match port - self.base_port {
            0 => self.irr,
            1 => self.imr,
            _ => 0,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base_port {
            0 => {
                // Command port. ICW1 has bit 4 set; OCW2/OCW3 don't.
                if value & 0x10 != 0 {
                    // ICW1 — start initialization
                    self.init_state = InitState::ExpectIcw2;
                    self.imr = 0;
                    self.isr = 0;
                    self.irr = 0;
                } else if value & 0x18 == 0x00 {
                    // OCW2 — bottom bits: 0x20 = non-specific EOI
                    if value & 0xE0 == 0x20 {
                        self.non_specific_eoi();
                    }
                    // Other OCW2 variants (specific EOI, rotate) are
                    // dropped for now.
                }
                // OCW3 (0b0xx01xxx) not implemented yet.
            }
            1 => {
                // Data port. During init it's ICW2/3/4; outside init it
                // is the IMR.
                match self.init_state {
                    InitState::Idle => self.imr = value,
                    InitState::ExpectIcw2 => {
                        self.vector_base = value & 0xF8;
                        self.init_state = InitState::ExpectIcw3;
                    }
                    InitState::ExpectIcw3 => {
                        // Cascade wiring — irrelevant for a single PIC
                        self.init_state = InitState::ExpectIcw4;
                    }
                    InitState::ExpectIcw4 => {
                        // Mode bits — dropped
                        self.init_state = InitState::Idle;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Concrete IO dispatcher. Owns the UART and the master PIC, routes
/// accesses by port. Unmapped ports read 0xFF (open bus on real
/// hardware) and accept writes silently.
pub struct IoBus {
    pub uart: Uart,
    pub pic: Pic,
}

impl IoBus {
    pub fn new() -> Self {
        Self { uart: Uart::com1(), pic: Pic::master() }
    }

    pub fn with_uart(uart: Uart) -> Self {
        Self { uart, pic: Pic::master() }
    }

    pub fn uart_mut(&mut self) -> &mut Uart {
        &mut self.uart
    }

    pub fn pic_mut(&mut self) -> &mut Pic {
        &mut self.pic
    }

    /// CPU-side accessor: highest-priority unmasked pending IRQ, or
    /// None if there's nothing to deliver right now.
    pub fn pending_irq_vector(&self) -> Option<u8> {
        self.pic.pending_vector()
    }

    /// CPU-side accessor: latch the IRQ as in-service. Caller should
    /// already have read `pending_irq_vector` and decided to dispatch.
    pub fn ack_irq(&mut self) {
        self.pic.ack();
    }

    pub fn read(&mut self, port: u16) -> u8 {
        let (lo, hi) = self.uart.port_range();
        if port >= lo && port <= hi {
            return self.uart.read(port);
        }
        let (lo, hi) = self.pic.port_range();
        if port >= lo && port <= hi {
            return self.pic.read(port);
        }
        0xFF
    }

    pub fn write(&mut self, port: u16, value: u8) {
        let (lo, hi) = self.uart.port_range();
        if port >= lo && port <= hi {
            self.uart.write(port, value);
            return;
        }
        let (lo, hi) = self.pic.port_range();
        if port >= lo && port <= hi {
            self.pic.write(port, value);
        }
    }
}

impl Default for IoBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uart_tx_collects_bytes_in_order() {
        let mut u = Uart::com1();
        u.write(Uart::COM1_BASE, b'a');
        u.write(Uart::COM1_BASE, b'b');
        assert_eq!(u.drain_tx(), b"ab");
        assert!(u.drain_tx().is_empty());
    }

    #[test]
    fn uart_lsr_reflects_rx_state() {
        let mut u = Uart::com1();
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 0);
        u.push_rx(b"X");
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 1);
        assert_eq!(u.read(Uart::COM1_BASE), b'X');
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 0);
    }

    #[test]
    fn uart_thre_is_always_set() {
        let mut u = Uart::com1();
        assert_eq!(u.read(Uart::COM1_BASE + 5) >> 5 & 1, 1);
    }

    #[test]
    fn iobus_routes_to_uart() {
        let mut bus = IoBus::new();
        bus.write(0x3F8, b'q');
        assert_eq!(bus.read(0x3FD) >> 5 & 1, 1);
        assert_eq!(bus.uart.drain_tx(), b"q");
    }

    #[test]
    fn iobus_unmapped_port_reads_ff() {
        let mut bus = IoBus::new();
        assert_eq!(bus.read(0x1234), 0xFF);
    }

    #[test]
    fn pic_masked_irq_does_not_become_pending() {
        let mut pic = Pic::master();
        // Default IMR is 0xFF (everything masked)
        pic.raise_irq(3);
        assert!(pic.pending_vector().is_none());
        // Unmask IRQ 3 by writing 0xF7 to data port
        pic.write(0x21, 0xF7);
        assert_eq!(pic.pending_vector(), Some(0x08 + 3));
    }

    #[test]
    fn pic_ack_moves_request_to_in_service() {
        let mut pic = Pic::master();
        pic.imr = 0;             // unmask all
        pic.raise_irq(2);
        assert_eq!(pic.pending_vector(), Some(0x08 + 2));
        pic.ack();
        // ISR holds IRQ 2; IRR cleared; no longer pending
        assert_eq!(pic.isr, 1 << 2);
        assert_eq!(pic.irr, 0);
        assert!(pic.pending_vector().is_none());
    }

    #[test]
    fn pic_eoi_clears_isr_top_bit() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(1);
        pic.ack();
        assert!(pic.isr != 0);
        // Non-specific EOI via OCW2
        pic.write(0x20, 0x20);
        assert_eq!(pic.isr, 0);
    }

    #[test]
    fn pic_icw_sequence_sets_vector_base() {
        let mut pic = Pic::master();
        // Standard PC remap to vector 0x30..0x37:
        //   ICW1 = 0x11 (init, ICW4 needed, edge-triggered)
        //   ICW2 = 0x30 (vector base)
        //   ICW3 = 0x04 (slave at IRQ2 — irrelevant for stub)
        //   ICW4 = 0x01 (8086 mode)
        pic.write(0x20, 0x11);
        pic.write(0x21, 0x30);
        pic.write(0x21, 0x04);
        pic.write(0x21, 0x01);
        assert_eq!(pic.vector_base, 0x30);
        // After init, writes to 0x21 go back to IMR
        pic.write(0x21, 0xFE);
        assert_eq!(pic.imr, 0xFE);
    }

    #[test]
    fn pic_higher_priority_irq_wins() {
        let mut pic = Pic::master();
        pic.imr = 0;
        pic.raise_irq(7);
        pic.raise_irq(0);
        // IRQ 0 is highest priority (bit 0 first)
        assert_eq!(pic.pending_vector(), Some(0x08));
    }
}
