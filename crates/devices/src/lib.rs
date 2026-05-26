//! IO-port-mapped devices visible to the CPU.
//!
//! Three concrete devices today: a 16550 UART on COM1 (the JS ↔ guest
//! byte stream), a master 8259A PIC (IRQ controller), and an 8254 PIT
//! (timer wired to IRQ 0). Each lives in its own module; this file is
//! just the trait, the dispatcher, and the IoBus glue.
//!
//! `IoBus` is concrete (not a trait-object container) on purpose: the
//! CPU needs typed access to the PIC for IRQ vector delivery and the
//! VM needs typed access to the UART for the JS-facing byte channel.
//! When we add another device kind we grow `IoBus` with another typed
//! field and another range in the dispatch match.

#![forbid(unsafe_code)]

/// Trait describing the shape every port-mapped device must satisfy.
/// Kept as documentation of the contract even though `IoBus` currently
/// dispatches to concrete types.
pub trait IoDevice {
    fn port_range(&self) -> (u16, u16);
    fn read(&mut self, port: u16) -> u8;
    fn write(&mut self, port: u16, value: u8);
}

mod cmos;
mod keyboard;
mod pic;
mod pit;
mod uart;

pub use cmos::{Cmos, reg as cmos_reg};
pub use keyboard::Keyboard;
pub use pic::Pic;
pub use pit::Pit;
pub use uart::Uart;

/// Concrete IO dispatcher. Owns one instance each of UART, PIC, PIT,
/// keyboard; routes accesses by port. Unmapped ports read 0xFF (open
/// bus on real hardware) and accept writes silently.
pub struct IoBus {
    pub uart: Uart,
    pub pic: Pic,
    pub pit: Pit,
    pub kbd: Keyboard,
    pub cmos: Cmos,
}

impl IoBus {
    pub fn new() -> Self {
        Self {
            uart: Uart::com1(),
            pic: Pic::master(),
            pit: Pit::standard(),
            kbd: Keyboard::new(),
            cmos: Cmos::new(),
        }
    }

    pub fn with_uart(uart: Uart) -> Self {
        Self {
            uart,
            pic: Pic::master(),
            pit: Pit::standard(),
            kbd: Keyboard::new(),
            cmos: Cmos::new(),
        }
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
        // Keyboard — level-triggered on IRQ 1.
        let irq1_bit = 1u8 << 1;
        if self.kbd.irq_pending() {
            self.pic.irr |= irq1_bit;
        } else {
            self.pic.irr &= !irq1_bit;
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
        let (lo, hi) = self.kbd.port_range();
        if port >= lo && port <= hi {
            return self.kbd.read(port);
        }
        let (lo, hi) = self.cmos.port_range();
        if port >= lo && port <= hi {
            return self.cmos.read(port);
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
            return;
        }
        let (lo, hi) = self.kbd.port_range();
        if port >= lo && port <= hi {
            self.kbd.write(port, value);
            return;
        }
        let (lo, hi) = self.cmos.port_range();
        if port >= lo && port <= hi {
            self.cmos.write(port, value);
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
    fn routes_to_uart() {
        let mut bus = IoBus::new();
        bus.write(0x3F8, b'q');
        assert_eq!(bus.read(0x3FD) >> 5 & 1, 1);
        assert_eq!(bus.uart.drain_tx(), b"q");
    }

    #[test]
    fn unmapped_port_reads_ff() {
        let mut bus = IoBus::new();
        assert_eq!(bus.read(0x1234), 0xFF);
    }

    #[test]
    fn refresh_irqs_latches_uart_into_pic() {
        let mut bus = IoBus::new();
        bus.write(Uart::COM1_BASE + 1, 0x01);
        bus.uart.push_rx(b"Q");
        bus.pic.imr = !(1 << 4);
        assert!(bus.pic.pending_vector().is_none());
        bus.refresh_irqs();
        assert_eq!(bus.pic.pending_vector(), Some(0x08 + 4));
    }

    #[test]
    fn refresh_translates_keyboard_assertion_into_pic_irr() {
        let mut bus = IoBus::new();
        bus.kbd.push_scancode(0x1E);
        bus.pic.imr = !(1 << 1); // unmask IRQ 1
        assert!(bus.pic.pending_vector().is_none());
        bus.refresh_irqs();
        assert_eq!(bus.pic.pending_vector(), Some(0x08 + 1));
    }

    #[test]
    fn iobus_routes_to_cmos() {
        let mut bus = IoBus::new();
        bus.cmos.set_time(26, 5, 27, 8, 0, 0);
        bus.write(0x70, cmos_reg::HOURS);
        assert_eq!(bus.read(0x71), 8);
    }

    #[test]
    fn iobus_routes_to_keyboard() {
        let mut bus = IoBus::new();
        bus.kbd.push_scancode(0x42);
        assert_eq!(bus.read(Keyboard::STATUS_PORT) & 1, 1);
        assert_eq!(bus.read(Keyboard::DATA_PORT), 0x42);
        assert_eq!(bus.read(Keyboard::STATUS_PORT) & 1, 0);
    }

    #[test]
    fn refresh_translates_pit_edge_into_pic_irr() {
        let mut bus = IoBus::new();
        bus.write(0x43, 0x34);
        bus.write(0x40, 0x01);
        bus.write(0x40, 0x00);
        bus.pic.imr = 0xFE;
        bus.refresh_irqs();
        assert_eq!(bus.pic.pending_vector(), Some(0x08));
    }
}
