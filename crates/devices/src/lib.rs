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
    /// Interrupt Enable Register. Only bit 0 (Received Data Available)
    /// affects our IRQ logic; other bits are stored but otherwise
    /// inert.
    ier: u8,
}

impl Uart {
    pub const COM1_BASE: u16 = 0x3F8;

    pub fn new(base: u16) -> Self {
        Self {
            base,
            tx_buffer: Vec::new(),
            rx_buffer: VecDeque::new(),
            ier: 0,
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

    /// True if the UART has a level-high interrupt line right now: rx
    /// data is available AND the guest has enabled the RDA interrupt
    /// in IER bit 0. `IoBus::refresh_irqs` polls this each step and
    /// latches it into the PIC's IRR.
    pub fn irq_pending(&self) -> bool {
        (self.ier & 0x01) != 0 && !self.rx_buffer.is_empty()
    }
}

impl IoDevice for Uart {
    fn port_range(&self) -> (u16, u16) {
        (self.base, self.base + 7)
    }

    fn read(&mut self, port: u16) -> u8 {
        match port - self.base {
            0 => self.rx_buffer.pop_front().unwrap_or(0),
            1 => self.ier,
            5 => {
                let dr = if self.rx_buffer.is_empty() { 0 } else { 1 };
                let thre = 1 << 5;
                dr | thre
            }
            _ => 0,
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base {
            0 => self.tx_buffer.push(value),
            1 => self.ier = value,
            _ => {}
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

/// 8254 Programmable Interval Timer — minimal channel-0 subset.
///
/// What we model:
///   * Channel 0 with reload register, current counter, and modes 0
///     (one-shot) and 2/3 (periodic — both reload on terminal count
///     and behave identically for IRQ generation purposes).
///   * Control-word writes to 0x43 with SC=0 and access pattern
///     RW=3 (LSB then MSB). Other RW patterns are accepted but the
///     reload-value writes will silently not latch.
///   * `tick(n)` decrements the counter by n; on terminal count it
///     latches a pending edge for IRQ 0 and (mode 2/3) reloads.
///
/// Channels 1 and 2 accept writes silently and don't generate IRQs.
pub struct Pit {
    base_port: u16,
    pub ch0_reload: u16,
    pub ch0_counter: u32,
    pub ch0_mode: u8,
    pub ch0_running: bool,
    /// Set true on terminal count; consumed by `take_ch0_pending`.
    ch0_pending_edge: bool,
    /// Next byte written to a channel-0 data port: 0 = LSB, 1 = MSB.
    write_state: u8,
    pending_lsb: u8,
}

impl Pit {
    pub const BASE: u16 = 0x40;

    pub fn new(base_port: u16) -> Self {
        Self {
            base_port,
            ch0_reload: 0,
            ch0_counter: 0,
            ch0_mode: 0,
            ch0_running: false,
            ch0_pending_edge: false,
            write_state: 0,
            pending_lsb: 0,
        }
    }

    pub fn standard() -> Self {
        Self::new(Self::BASE)
    }

    /// Advance the timer by `n` ticks. On reaching zero in mode 0 the
    /// channel halts; in modes 2/3 it reloads from `ch0_reload` (with
    /// 0 treated as 0x10000 — the 8254 convention). Each terminal
    /// count latches a single pending edge that the IoBus will
    /// translate into an IRQ.
    pub fn tick(&mut self, n: u32) {
        if !self.ch0_running || n == 0 {
            return;
        }
        let mut remaining = n;
        loop {
            if self.ch0_counter == 0 {
                match self.ch0_mode {
                    0 => {
                        self.ch0_running = false;
                        return;
                    }
                    _ => {
                        let reload = if self.ch0_reload == 0 {
                            0x10000
                        } else {
                            self.ch0_reload as u32
                        };
                        self.ch0_counter = reload;
                    }
                }
            }
            let take = remaining.min(self.ch0_counter);
            self.ch0_counter -= take;
            remaining -= take;
            if self.ch0_counter == 0 {
                self.ch0_pending_edge = true;
            }
            if remaining == 0 {
                return;
            }
        }
    }

    /// Consume the channel-0 edge, if any. IoBus calls this each
    /// `refresh_irqs` and turns a true result into a one-shot IRR
    /// set on the PIC.
    pub fn take_ch0_pending(&mut self) -> bool {
        let p = self.ch0_pending_edge;
        self.ch0_pending_edge = false;
        p
    }
}

impl IoDevice for Pit {
    fn port_range(&self) -> (u16, u16) {
        (self.base_port, self.base_port + 3)
    }

    fn read(&mut self, port: u16) -> u8 {
        // Reads are rarely used by guest software outside of latching
        // commands which we don't model. Return the current counter
        // low byte for channel 0 as a stub.
        if port - self.base_port == 0 {
            (self.ch0_counter & 0xFF) as u8
        } else {
            0
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base_port {
            3 => {
                // Control word: SC RW M BCD
                let sc = value >> 6;
                let rw = (value >> 4) & 3;
                let mode = (value >> 1) & 7;
                if sc == 0 && rw == 3 {
                    self.ch0_mode = mode;
                    self.write_state = 0;
                    self.ch0_pending_edge = false;
                    self.ch0_running = false;
                }
                // Other channels / access patterns ignored.
            }
            0 => {
                if self.write_state == 0 {
                    self.pending_lsb = value;
                    self.write_state = 1;
                } else {
                    let reload = (self.pending_lsb as u16) | ((value as u16) << 8);
                    self.ch0_reload = reload;
                    self.ch0_counter = if reload == 0 { 0x10000 } else { reload as u32 };
                    self.ch0_running = true;
                    self.write_state = 0;
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
    pub pit: Pit,
}

impl IoBus {
    pub fn new() -> Self {
        Self { uart: Uart::com1(), pic: Pic::master(), pit: Pit::standard() }
    }

    pub fn with_uart(uart: Uart) -> Self {
        Self { uart, pic: Pic::master(), pit: Pit::standard() }
    }

    pub fn uart_mut(&mut self) -> &mut Uart {
        &mut self.uart
    }

    pub fn pic_mut(&mut self) -> &mut Pic {
        &mut self.pic
    }

    /// Latch every device-asserted IRQ line into the PIC's IRR and
    /// drive time-based devices forward by one tick. CPUs call this
    /// once per step() before checking pending IRQs.
    ///
    /// Standard wiring on a PC: COM1 → IRQ 4 (level-triggered, mirrors
    /// the line); PIT channel 0 → IRQ 0 (edge-triggered, one IRR pulse
    /// per terminal count).
    pub fn refresh_irqs(&mut self) {
        // UART — level-triggered. IRR bit 4 mirrors the line.
        let irq4_bit = 1u8 << 4;
        if self.uart.irq_pending() {
            self.pic.irr |= irq4_bit;
        } else {
            self.pic.irr &= !irq4_bit;
        }
        // PIT — one tick per CPU step. Each terminal count gets
        // translated into a one-shot IRR set on IRQ 0. The PIC keeps
        // the bit until ack, so even if multiple ticks fire between
        // refresh calls (we only do one per step) the handler still
        // runs once per pulse — periodic timers work as expected.
        self.pit.tick(1);
        if self.pit.take_ch0_pending() {
            self.pic.irr |= 1u8 << 0;
        }
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
        let (lo, hi) = self.pit.port_range();
        if port >= lo && port <= hi {
            return self.pit.read(port);
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
            return;
        }
        let (lo, hi) = self.pit.port_range();
        if port >= lo && port <= hi {
            self.pit.write(port, value);
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
    fn uart_irq_pending_requires_ier_bit0_and_rx_data() {
        let mut u = Uart::com1();
        // No data, IER=0 → no IRQ
        assert!(!u.irq_pending());
        u.push_rx(b"x");
        // Data, but IER bit 0 still 0 → no IRQ
        assert!(!u.irq_pending());
        // Enable RDA interrupt
        u.write(Uart::COM1_BASE + 1, 0x01);
        assert!(u.irq_pending());
        // Read drains rx → no more pending
        assert_eq!(u.read(Uart::COM1_BASE), b'x');
        assert!(!u.irq_pending());
    }

    #[test]
    fn iobus_refresh_irqs_latches_uart_into_pic() {
        let mut bus = IoBus::new();
        // Enable IER bit 0 via the bus path
        bus.write(Uart::COM1_BASE + 1, 0x01);
        bus.uart.push_rx(b"Q");
        // Unmask IRQ 4 in PIC
        bus.pic.imr = !(1 << 4);
        // Initially PIC sees nothing — refresh latches the line
        assert!(bus.pic.pending_vector().is_none());
        bus.refresh_irqs();
        assert_eq!(bus.pic.pending_vector(), Some(0x08 + 4));
    }

    #[test]
    fn pit_mode2_fires_periodic_edge_every_reload_ticks() {
        let mut pit = Pit::standard();
        // Control: SC=0, RW=3 (LSB then MSB), mode=2, BCD=0  → 0b00110100 = 0x34
        pit.write(0x43, 0x34);
        // Reload value 3
        pit.write(0x40, 0x03);
        pit.write(0x40, 0x00);
        // Three ticks should bring counter to 0 → pending edge.
        pit.tick(3);
        assert!(pit.take_ch0_pending());
        assert!(!pit.take_ch0_pending()); // consumed once
        // Three more — periodic mode reloads automatically.
        pit.tick(3);
        assert!(pit.take_ch0_pending());
    }

    #[test]
    fn pit_mode0_oneshot_halts_after_first_terminal_count() {
        let mut pit = Pit::standard();
        // Mode 0 (one-shot): control byte 0b00110000 = 0x30
        pit.write(0x43, 0x30);
        pit.write(0x40, 0x02);
        pit.write(0x40, 0x00);
        pit.tick(2);
        assert!(pit.take_ch0_pending());
        // Further ticks must not fire — channel halts.
        pit.tick(100);
        assert!(!pit.take_ch0_pending());
    }

    #[test]
    fn iobus_refresh_translates_pit_edge_into_pic_irr() {
        let mut bus = IoBus::new();
        // Configure PIT mode 2, reload 1 — fires every tick.
        bus.write(0x43, 0x34);
        bus.write(0x40, 0x01);
        bus.write(0x40, 0x00);
        // Unmask IRQ 0
        bus.pic.imr = 0xFE;
        bus.refresh_irqs(); // ticks once, channel hits zero → edge
        assert_eq!(bus.pic.pending_vector(), Some(0x08));
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
