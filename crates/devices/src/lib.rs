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

/// Concrete IO dispatcher. Owns one instance of each PC device,
/// including the cascaded master+slave 8259A PIC pair. Routes
/// accesses by port. Unmapped ports read 0xFF (open bus on real
/// hardware) and accept writes silently.
pub struct IoBus {
    pub uart: Uart,
    /// Master PIC, 0x20/0x21, vector base 0x08 — IRQs 0..7.
    pub pic: Pic,
    /// Slave PIC, 0xA0/0xA1, vector base 0x70 — IRQs 8..15. Cascaded
    /// through master IRQ 2 (the standard PC wiring).
    pub slave_pic: Pic,
    pub pit: Pit,
    pub kbd: Keyboard,
    pub cmos: Cmos,
}

impl IoBus {
    pub fn new() -> Self {
        let mut slave_pic = Pic::new(0xA0);
        slave_pic.vector_base = 0x70;
        Self {
            uart: Uart::com1(),
            pic: Pic::master(),
            slave_pic,
            pit: Pit::standard(),
            kbd: Keyboard::new(),
            cmos: Cmos::new(),
        }
    }

    pub fn with_uart(uart: Uart) -> Self {
        let mut slave_pic = Pic::new(0xA0);
        slave_pic.vector_base = 0x70;
        Self {
            uart,
            pic: Pic::master(),
            slave_pic,
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
        // Cascade — slave-pending state controls master IRR bit 2.
        // Level-triggered so master deasserts as soon as the slave has
        // nothing left to deliver.
        let cascade_bit = 1u8 << 2;
        if self.slave_pic.pending_vector().is_some() {
            self.pic.irr |= cascade_bit;
        } else {
            self.pic.irr &= !cascade_bit;
        }
    }

    /// CPU-side accessor: highest-priority unmasked pending IRQ, or
    /// None if there's nothing to deliver right now. If the master
    /// reports IRQ 2 (the cascade), we descend into the slave and
    /// return *its* vector instead — the CPU never sees the cascade
    /// IRQ directly.
    pub fn pending_irq_vector(&self) -> Option<u8> {
        let vec = self.pic.pending_vector()?;
        let master_irq = vec.wrapping_sub(self.pic.vector_base);
        if master_irq == 2 {
            self.slave_pic.pending_vector()
        } else {
            Some(vec)
        }
    }

    /// CPU-side accessor: latch the IRQ as in-service. For a cascade
    /// IRQ we ack the slave first (its IRR→ISR move), then the master
    /// (the cascade IRQ 2 stays in master's ISR until the handler
    /// EOIs both PICs). That matches the two-INTA-cycle behavior real
    /// hardware uses.
    pub fn ack_irq(&mut self) {
        let vec = match self.pic.pending_vector() {
            Some(v) => v,
            None => return,
        };
        let master_irq = vec.wrapping_sub(self.pic.vector_base);
        if master_irq == 2 {
            self.slave_pic.ack();
        }
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
        let (lo, hi) = self.slave_pic.port_range();
        if port >= lo && port <= hi {
            return self.slave_pic.read(port);
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
            return;
        }
        let (lo, hi) = self.slave_pic.port_range();
        if port >= lo && port <= hi {
            self.slave_pic.write(port, value);
        }
    }
}

impl Default for IoBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot blob layout: a 1-byte device count followed by repeated
/// `[length: u32LE][device-id: u8][bytes...]` records. Length covers
/// the device-id byte plus the payload, so a parser can skip an
/// unknown device cleanly by jumping `length` bytes ahead.
///
/// Device IDs are stable: 1=UART, 2=PIC master, 3=PIC slave, 4=PIT,
/// 5=Keyboard, 6=CMOS. New devices append. Unknown IDs are silently
/// skipped — that's how we'll handle forward-compat snapshots later.
impl IoBus {
    const DEV_UART: u8 = 1;
    const DEV_PIC_MASTER: u8 = 2;
    const DEV_PIC_SLAVE: u8 = 3;
    const DEV_PIT: u8 = 4;
    const DEV_KBD: u8 = 5;
    const DEV_CMOS: u8 = 6;

    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        out.push(6u8); // device count
        let emit = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
            let len = 1 + payload.len() as u32;
            out.extend_from_slice(&len.to_le_bytes());
            out.push(id);
            out.extend_from_slice(payload);
        };
        let mut buf = Vec::new();
        buf.clear(); self.uart.snapshot_into(&mut buf); emit(&mut out, Self::DEV_UART, &buf);
        buf.clear(); self.pic.snapshot_into(&mut buf); emit(&mut out, Self::DEV_PIC_MASTER, &buf);
        buf.clear(); self.slave_pic.snapshot_into(&mut buf); emit(&mut out, Self::DEV_PIC_SLAVE, &buf);
        buf.clear(); self.pit.snapshot_into(&mut buf); emit(&mut out, Self::DEV_PIT, &buf);
        buf.clear(); self.kbd.snapshot_into(&mut buf); emit(&mut out, Self::DEV_KBD, &buf);
        buf.clear(); self.cmos.snapshot_into(&mut buf); emit(&mut out, Self::DEV_CMOS, &buf);
        out
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() { return Err("iobus: empty".into()); }
        let count = bytes[0];
        let mut p = 1;
        for _ in 0..count {
            if bytes.len() < p + 4 { return Err("iobus: truncated record header".into()); }
            let len = u32::from_le_bytes([
                bytes[p], bytes[p+1], bytes[p+2], bytes[p+3]
            ]) as usize;
            p += 4;
            if bytes.len() < p + len || len < 1 {
                return Err("iobus: truncated record body".into());
            }
            let id = bytes[p];
            let payload = &bytes[p+1..p+len];
            let consumed = match id {
                Self::DEV_UART => self.uart.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIC_MASTER => self.pic.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIC_SLAVE => self.slave_pic.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIT => self.pit.restore(payload).map_err(str::to_string)?,
                Self::DEV_KBD => self.kbd.restore(payload).map_err(str::to_string)?,
                Self::DEV_CMOS => self.cmos.restore(payload).map_err(str::to_string)?,
                // Unknown device — skip its payload.
                _ => payload.len(),
            };
            let _ = consumed; // device may consume fewer bytes than payload; trailing data ignored
            p += len;
        }
        Ok(())
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
    fn iobus_routes_to_slave_pic() {
        let mut bus = IoBus::new();
        bus.write(0xA1, 0xAA);
        assert_eq!(bus.read(0xA1), 0xAA);
    }

    #[test]
    fn slave_pending_cascades_through_master_irq2() {
        let mut bus = IoBus::new();
        bus.slave_pic.imr = 0; // unmask all on slave
        bus.pic.imr = !(1 << 2); // master only sees IRQ 2 (the cascade)
        // Slave IRQ 0 → vector 0x70
        bus.slave_pic.raise_irq(0);
        // Without refresh, master is unaware.
        assert!(bus.pic.pending_vector().is_none());
        bus.refresh_irqs();
        // refresh latched IRQ 2 into master IRR; pending_irq_vector
        // now follows the cascade and returns the slave's vector.
        assert_eq!(bus.pending_irq_vector(), Some(0x70));
    }

    #[test]
    fn cascade_ack_clears_both_pic_irr_bits() {
        let mut bus = IoBus::new();
        bus.slave_pic.imr = 0;
        bus.pic.imr = !(1 << 2);
        bus.slave_pic.raise_irq(3); // slave IRQ 11 → vector 0x73
        bus.refresh_irqs();
        assert_eq!(bus.pending_irq_vector(), Some(0x73));
        bus.ack_irq();
        // Both PICs moved their bits from IRR to ISR.
        assert_eq!(bus.pic.isr & (1 << 2), 1 << 2);
        assert_eq!(bus.slave_pic.isr & (1 << 3), 1 << 3);
        // No further pending — refresh_irqs deasserts master cascade
        // once slave has nothing left unmasked-and-unserviced.
        bus.refresh_irqs();
        assert!(bus.pending_irq_vector().is_none());
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
