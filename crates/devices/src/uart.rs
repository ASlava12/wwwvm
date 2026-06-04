//! 16550-style UART. The channel between guest and host: the guest
//! writes bytes that JS reads as terminal output, and JS pushes bytes
//! that the guest reads as keystrokes / pre-canned commands.
//!
//! Minimal register subset:
//!   * THR (offset 0) — write transmits a byte to host output buffer
//!   * RBR (offset 0) — read pops a byte from host input buffer
//!   * IER (offset 1) — bit 0 = RDA IRQ enable, bit 1 = THRE IRQ enable
//!   * IIR (offset 2) — read returns 0x04 (RDA) | 0x02 (THRE) | 0x01
//!     (no IRQ pending), so Linux's 8250 driver can dispatch
//!   * LSR (offset 5) — bit 0 (DR) = input available, bit 5 (THRE) = always 1
//!
//! Other registers (MCR, scratch, DLAB) return 0 and accept writes
//! silently. THRE IRQ (transmit-ready) is needed for Linux's
//! user-mode tty write path — kernel printk uses polled writes
//! (spins on LSR.THRE), but user-mode writes go through `uart_write`
//! → `start_tx` → "enable THRE IRQ, send on each IRQ" and stall
//! indefinitely if the IRQ never fires.

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

    /// True if the UART has a level-high interrupt line right now.
    /// Two sources can drive it:
    ///   * RDA (IER bit 0) — guest has enabled the receive IRQ and
    ///     `rx_buffer` is non-empty.
    ///   * THRE (IER bit 1) — guest has enabled the transmit-ready
    ///     IRQ. Our model has zero TX latency: every write to THR
    ///     completes immediately, so the transmitter is *always*
    ///     ready and the IRQ stays asserted while IER bit 1 is set.
    ///     Linux's serial driver drains the circ_buf one byte per
    ///     IRQ delivery and clears IER bit 1 when there's nothing
    ///     left to send.
    pub fn irq_pending(&self) -> bool {
        let rx = (self.ier & 0x01) != 0 && !self.rx_buffer.is_empty();
        let tx = (self.ier & 0x02) != 0;
        rx || tx
    }

    /// Interrupt-enable register (debug/diagnostics).
    pub fn ier(&self) -> u8 {
        self.ier
    }

    /// Bytes currently queued in the RX FIFO (debug/diagnostics).
    pub fn rx_len(&self) -> usize {
        self.rx_buffer.len()
    }

    /// Serialize UART state into `out`. Format: IER (u8), tx_len
    /// (u32LE) + tx_bytes, rx_len (u32LE) + rx_bytes. Base port is
    /// not stored — it's an architectural constant set at
    /// construction.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.push(self.ier);
        let tx_len = self.tx_buffer.len() as u32;
        out.extend_from_slice(&tx_len.to_le_bytes());
        out.extend_from_slice(&self.tx_buffer);
        let rx_len = self.rx_buffer.len() as u32;
        out.extend_from_slice(&rx_len.to_le_bytes());
        for b in &self.rx_buffer {
            out.push(*b);
        }
    }

    /// Restore from a buffer produced by `snapshot_into`. Returns the
    /// number of bytes consumed.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        let mut p = 0;
        if bytes.len() < 1 + 4 {
            return Err("uart: truncated");
        }
        self.ier = bytes[p];
        p += 1;
        let tx_len =
            u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]) as usize;
        p += 4;
        if bytes.len() < p + tx_len + 4 {
            return Err("uart: truncated tx");
        }
        self.tx_buffer = bytes[p..p + tx_len].to_vec();
        p += tx_len;
        let rx_len =
            u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]) as usize;
        p += 4;
        if bytes.len() < p + rx_len {
            return Err("uart: truncated rx");
        }
        self.rx_buffer = bytes[p..p + rx_len].iter().copied().collect();
        p += rx_len;
        Ok(p)
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
            // IIR — Interrupt Identification Register. Linux's 8250
            // handler reads this immediately on IRQ entry to decide
            // which path to take. Encode the same priority order as
            // a real 16450: receive line status > RDA > THRE >
            // modem status. We don't model line-status or modem
            // events, so it's just RDA vs THRE vs "nothing pending"
            // (bit 0 = 1 means no IRQ; bits 3:1 = cause code).
            2 => {
                if (self.ier & 0x01) != 0 && !self.rx_buffer.is_empty() {
                    0x04 // RDA
                } else if (self.ier & 0x02) != 0 {
                    0x02 // THRE
                } else {
                    0x01 // no IRQ pending
                }
            }
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

    /// THRE IRQ stays asserted while IER bit 1 is set — our model
    /// has zero TX latency so the transmitter is always ready. This
    /// is the path Linux's user-mode serial write relies on: enable
    /// THRE IRQ, drain one byte per delivery, disable when done.
    #[test]
    fn thre_irq_fires_while_ier_bit1_set() {
        let mut u = Uart::com1();
        assert!(!u.irq_pending());
        u.write(Uart::COM1_BASE + 1, 0x02); // enable THRE
        assert!(u.irq_pending());
        // IIR reads as 0x02 (THRE) so the kernel handler knows to
        // drain `circ_buf` into THR rather than handle an RDA.
        assert_eq!(u.read(Uart::COM1_BASE + 2), 0x02);
        // Disable THRE — IRQ stops.
        u.write(Uart::COM1_BASE + 1, 0x00);
        assert!(!u.irq_pending());
        assert_eq!(u.read(Uart::COM1_BASE + 2), 0x01);
    }

    /// RDA wins priority over THRE in IIR, matching real-silicon
    /// dispatch (RX line status is the highest, then RDA, then
    /// THRE, then modem status).
    #[test]
    fn iir_prioritizes_rda_over_thre() {
        let mut u = Uart::com1();
        u.write(Uart::COM1_BASE + 1, 0x03); // RDA + THRE enabled
        u.push_rx(b"a");
        assert_eq!(u.read(Uart::COM1_BASE + 2), 0x04, "RDA wins");
        // After draining RX, IIR reports THRE.
        assert_eq!(u.read(Uart::COM1_BASE), b'a');
        assert_eq!(u.read(Uart::COM1_BASE + 2), 0x02);
    }

    /// snapshot/restore must round-trip IER (which IRQs the kernel
    /// expects) and both buffers (TX bytes the host hasn't drained
    /// yet, plus RX bytes the guest hasn't read). A regression in
    /// any of the three would silently corrupt boot-resume:
    ///   - IER: kernel handlers fire at wrong times
    ///   - TX: host loses un-drained guest output
    ///   - RX: guest sees a quiet UART after wake-up
    #[test]
    fn snapshot_round_trip_preserves_ier_and_both_buffers() {
        let mut u = Uart::com1();
        u.write(Uart::COM1_BASE + 1, 0x03); // IER = RDA + THRE
        u.write(Uart::COM1_BASE, b'H'); // TX collects bytes
        u.write(Uart::COM1_BASE, b'i');
        u.push_rx(b"yo!"); // RX from the host

        let mut buf = Vec::new();
        u.snapshot_into(&mut buf);
        // Format: IER(1) + tx_len(4) + tx_bytes(2) + rx_len(4) + rx(3) = 14
        assert_eq!(buf.len(), 14);

        let mut u2 = Uart::com1();
        let consumed = u2.restore(&buf).expect("restore");
        assert_eq!(consumed, 14);

        // IER survived — IRQ still latches RDA/THRE.
        assert!(u2.irq_pending());
        // TX buffer survived; drain_tx returns exactly what was
        // collected pre-snapshot.
        assert_eq!(u2.drain_tx(), b"Hi");
        // RX buffer survived in FIFO order — three pops drain it.
        assert_eq!(u2.read(Uart::COM1_BASE), b'y');
        assert_eq!(u2.read(Uart::COM1_BASE), b'o');
        assert_eq!(u2.read(Uart::COM1_BASE), b'!');
    }

    /// restore must reject malformed inputs rather than panic.
    /// Three failure modes: <5 bytes header (IER + tx_len missing);
    /// tx_len says more bytes follow than payload contains;
    /// rx_len same kind of overflow.
    #[test]
    fn restore_rejects_truncated_blob() {
        let mut u = Uart::com1();
        assert!(u.restore(&[0u8; 4]).is_err(), "header truncated");
        // IER=0, tx_len=10, only 2 bytes follow — overflow.
        let bad_tx = [0u8, 10, 0, 0, 0, 1, 2];
        assert!(u.restore(&bad_tx).is_err(), "tx truncated");
        // Valid header + 0-byte tx + rx_len=5 + only 2 rx bytes — overflow.
        let bad_rx = [0u8, 0, 0, 0, 0, 5, 0, 0, 0, 1, 2];
        assert!(u.restore(&bad_rx).is_err(), "rx truncated");
    }
}
