//! 16550-style UART. The channel between guest and host: the guest
//! writes bytes that JS reads as terminal output, and JS pushes bytes
//! that the guest reads as keystrokes / pre-canned commands.
//!
//! Minimal register subset:
//!   * THR (offset 0) — write transmits a byte to host output buffer
//!   * RBR (offset 0) — read pops a byte from host input buffer
//!   * IER (offset 1) — bit 0 enables Received Data Available IRQ
//!   * LSR (offset 5) — bit 0 (DR) = input available, bit 5 (THRE) = always 1
//!
//! Other registers (IIR, MCR, scratch, DLAB) return 0 and accept writes
//! silently. Enough for a guest that polls LSR or that uses IRQ 4.

use std::collections::VecDeque;

use crate::IoDevice;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_collects_bytes_in_order() {
        let mut u = Uart::com1();
        u.write(Uart::COM1_BASE, b'a');
        u.write(Uart::COM1_BASE, b'b');
        assert_eq!(u.drain_tx(), b"ab");
        assert!(u.drain_tx().is_empty());
    }

    #[test]
    fn lsr_reflects_rx_state() {
        let mut u = Uart::com1();
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 0);
        u.push_rx(b"X");
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 1);
        assert_eq!(u.read(Uart::COM1_BASE), b'X');
        assert_eq!(u.read(Uart::COM1_BASE + 5) & 1, 0);
    }

    #[test]
    fn thre_is_always_set() {
        let mut u = Uart::com1();
        assert_eq!(u.read(Uart::COM1_BASE + 5) >> 5 & 1, 1);
    }

    #[test]
    fn irq_pending_requires_ier_bit0_and_rx_data() {
        let mut u = Uart::com1();
        assert!(!u.irq_pending());
        u.push_rx(b"x");
        assert!(!u.irq_pending());
        u.write(Uart::COM1_BASE + 1, 0x01);
        assert!(u.irq_pending());
        assert_eq!(u.read(Uart::COM1_BASE), b'x');
        assert!(!u.irq_pending());
    }
}
