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

/// Concrete IO dispatcher. Owns the UART and routes accesses by port.
/// Unmapped ports read 0xFF (open bus on real hardware) and accept
/// writes silently.
pub struct IoBus {
    pub uart: Uart,
}

impl IoBus {
    pub fn new() -> Self {
        Self { uart: Uart::com1() }
    }

    pub fn with_uart(uart: Uart) -> Self {
        Self { uart }
    }

    pub fn uart_mut(&mut self) -> &mut Uart {
        &mut self.uart
    }

    pub fn read(&mut self, port: u16) -> u8 {
        let (lo, hi) = self.uart.port_range();
        if port >= lo && port <= hi {
            return self.uart.read(port);
        }
        0xFF
    }

    pub fn write(&mut self, port: u16, value: u8) {
        let (lo, hi) = self.uart.port_range();
        if port >= lo && port <= hi {
            self.uart.write(port, value);
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
}
