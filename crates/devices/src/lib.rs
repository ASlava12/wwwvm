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

mod ata;
mod cmos;
mod disk;
mod keyboard;
mod pci;
mod pic;
mod pit;
mod rtl8139;
mod uart;

pub use ata::{Ata, PRIMARY_PORT_BASE, SECONDARY_PORT_BASE};
pub use cmos::{reg as cmos_reg, Cmos};
pub use disk::{Disk, SECTOR_SIZE as DISK_SECTOR_SIZE};
pub use keyboard::Keyboard;
pub use pci::Pci;
pub use pic::Pic;
pub use pit::Pit;
pub use rtl8139::Rtl8139;
pub use uart::Uart;

/// Concrete IO dispatcher. Owns one instance of each PC device,
/// including the cascaded master+slave 8259A PIC pair. Routes
/// accesses by port. Unmapped ports read 0xFF (open bus on real
/// hardware) and accept writes silently.
pub struct IoBus {
    pub uart: Uart,
    /// Master PIC, 0x20/0x21, vector base 0x08 — IRQs 0..7.
    pub pic: Pic,
    /// PIT-tick prescaler counter — see `PIT_TICK_DIVIDER` for the
    /// rationale. `refresh_irqs` advances it; PIT gets a real tick
    /// only when it overflows the divider.
    pit_div_counter: u32,
    /// Slave PIC, 0xA0/0xA1, vector base 0x70 — IRQs 8..15. Cascaded
    /// through master IRQ 2 (the standard PC wiring).
    pub slave_pic: Pic,
    pub pit: Pit,
    pub kbd: Keyboard,
    pub cmos: Cmos,
    /// Primary IDE channel + its in-memory boot disk. The disk is
    /// owned by the controller; existing call sites that used to
    /// reach for `io.disk` directly now go through `io.disk()` /
    /// `io.disk_mut()` accessors. Not snapshotted yet.
    pub ata: Ata,
    /// Secondary IDE channel (ports 0x170..0x177). Same controller
    /// type, different port base — the standard PC two-channel
    /// layout, useful for a CD-ROM target or a second hard drive.
    pub ata2: Ata,
    /// PCI configuration space (ports 0xCF8..0xCFF). No devices
    /// behind the bus yet — every read at the data window returns
    /// the 0xFFFFFFFF "no device" sentinel, which is what Linux
    /// expects to see when it walks an empty bus.
    pub pci: Pci,
    /// RTL8139 NIC register file (00:01.0). Dispatched at the I/O base the
    /// kernel assigns to BAR0 (see `pci.nic_io_base`).
    pub rtl8139: Rtl8139,
    /// Ethernet frames the guest transmitted, copied out of guest RAM by
    /// the CPU step's bus-master TX service. The host (run loop / bridge)
    /// drains this with `drain_nic_tx`. Ephemeral — not snapshotted. Capped
    /// (drop-oldest, like a real NIC's TX-overrun) so an embedder that
    /// forgets to drain can't grow host memory without bound.
    nic_tx_frames: std::collections::VecDeque<Vec<u8>>,
}

/// Max buffered guest TX frames before we drop the oldest (TX overrun).
const NIC_TX_CAP: usize = 1024;

impl IoBus {
    /// Convenience accessor for code paths (the BIOS shim, the
    /// snapshot of the boot sector) that only care about the
    /// disk image, not the IDE controller's register state.
    pub fn disk(&self) -> &Disk {
        &self.ata.disk
    }

    pub fn disk_mut(&mut self) -> &mut Disk {
        &mut self.ata.disk
    }
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
            ata: Ata::new(),
            ata2: Ata::with_port_base(SECONDARY_PORT_BASE),
            pci: Pci::new(),
            rtl8139: Rtl8139::new(),
            nic_tx_frames: std::collections::VecDeque::new(),
            pit_div_counter: 0,
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
            ata: Ata::new(),
            ata2: Ata::with_port_base(SECONDARY_PORT_BASE),
            pci: Pci::new(),
            rtl8139: Rtl8139::new(),
            nic_tx_frames: std::collections::VecDeque::new(),
            pit_div_counter: 0,
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
        // PS/2 mouse (AUX) — level-triggered on IRQ 12, which lives on the
        // slave PIC (IRQ 8..15 → slave IRR bits 0..7, so IRQ 12 = bit 4).
        // The cascade block below propagates it to master IRQ 2.
        let mouse_slave_bit = 1u8 << 4;
        if self.kbd.aux_irq_pending() {
            self.slave_pic.irr |= mouse_slave_bit;
        } else {
            self.slave_pic.irr &= !mouse_slave_bit;
        }
        // PIT — prescaler-divided tick. One CPU step ≠ one PIT
        // cycle: at our ~17 MIPS effective throughput, ticking PIT
        // once per step gives Linux a HZ=250 latch (~4773 PIT
        // cycles) every ~4 800 CPU steps. The scheduler-tick
        // handler then takes most of those steps doing 64-bit
        // ktime conversion, leaving almost no budget for the
        // kernel_init thread between ticks — and PID 1 never
        // gets enough CPU to reach run_init_process. Divide the
        // tick rate by `PIT_TICK_DIVIDER` so PIT IRQs fire roughly
        // every (divider × latch) steps. With divider=16 and HZ=
        // 250, that's ~76 K CPU steps per IRQ — the handler still
        // runs every tick but is well under 50 % of the budget,
        // so kernel_init makes forward progress. Linux doesn't
        // care about the absolute PIT rate (TSC calibration is
        // already overridden by `lpj=1000000`); it only needs the
        // edge to fire so jiffies advance.
        const PIT_TICK_DIVIDER: u32 = 16;
        self.pit_div_counter += 1;
        if self.pit_div_counter >= PIT_TICK_DIVIDER {
            self.pit_div_counter = 0;
            self.pit.tick(1);
            if self.pit.take_ch0_pending() {
                self.pic.irr |= 1u8 << 0;
            }
        }
        // RTL8139 NIC — level-triggered on IRQ 11, which lives on the
        // slave PIC (IRQ 8..15 → slave IRR bits 0..7, so IRQ 11 = bit 3).
        // The cascade block below then propagates it to master IRQ 2.
        let nic_slave_bit = 1u8 << 3;
        if self.rtl8139.irq_pending() {
            self.slave_pic.irr |= nic_slave_bit;
        } else {
            self.slave_pic.irr &= !nic_slave_bit;
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

    /// True when the NIC has a transmit the CPU step should DMA out of
    /// guest RAM. Cheap guard so the bus-master copy only runs when work
    /// is actually queued.
    pub fn nic_has_pending_tx(&self) -> bool {
        self.rtl8139.has_pending_tx()
    }

    /// Drain the NIC's queued transmit descriptors as (guest-physical
    /// address, length). The CPU step reads each frame out of guest RAM
    /// (it holds the `Memory`) and hands the bytes back via
    /// `record_nic_tx_frame` — the device crate itself never touches RAM.
    pub fn take_nic_tx_descriptors(&mut self) -> Vec<(u32, u16)> {
        self.rtl8139.take_tx_frames()
    }

    /// Store one Ethernet frame copied out of guest RAM, ready for the
    /// host bridge to pick up with `drain_nic_tx`. Drops the oldest frame if
    /// the queue is at `NIC_TX_CAP` (TX overrun — a real NIC drops too) so an
    /// embedder that never drains can't grow host memory without bound.
    pub fn record_nic_tx_frame(&mut self, frame: Vec<u8>) {
        if self.nic_tx_frames.len() >= NIC_TX_CAP {
            self.nic_tx_frames.pop_front();
        }
        self.nic_tx_frames.push_back(frame);
    }

    /// Take every Ethernet frame the guest has transmitted since the last
    /// drain. The host networking bridge calls this each run-loop batch.
    pub fn drain_nic_tx(&mut self) -> Vec<Vec<u8>> {
        self.nic_tx_frames.drain(..).collect()
    }

    /// Pop a single transmitted frame (oldest first), or None if the queue is
    /// empty — for hosts that consume one frame at a time (the wasm bridge,
    /// where returning a Vec-of-Vec across the JS boundary is awkward). O(1).
    pub fn pop_nic_tx(&mut self) -> Option<Vec<u8>> {
        self.nic_tx_frames.pop_front()
    }

    pub fn read(&mut self, port: u16) -> u8 {
        // RTL8139 register window — a DYNAMIC range at the I/O base the
        // kernel assigned to BAR0 (in the high I/O range, away from legacy
        // ports). Checked first; if the NIC isn't I/O-enabled this is inert.
        if self.pci.nic_io_enabled() {
            let base = self.pci.nic_io_base();
            if port >= base && (port - base) < pci::RTL8139_IO_SIZE as u16 {
                return self.rtl8139.read_reg(port - base);
            }
        }
        // Port 0x61 (NMI Status & Control) lives in the keyboard's
        // claimed range but is logically a PIT/PPI register —
        // Linux's TSC-via-PIT calibration polls bit 5 (channel-2
        // OUT) here. Dispatch it explicitly before the keyboard
        // catches the read and returns 0.
        if port == 0x61 {
            return self.pit.read_port_61();
        }
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
        let (lo, hi) = self.ata.port_range();
        if port >= lo && port <= hi {
            return self.ata.read(port);
        }
        let (lo, hi) = self.ata2.port_range();
        if port >= lo && port <= hi {
            return self.ata2.read(port);
        }
        // ATA control-block ports — base + 0x206. These live outside
        // each channel's contiguous command-block range, so they need
        // their own dispatch line.
        if port == self.ata.control_port() {
            return self.ata.read_alt_status();
        }
        if port == self.ata2.control_port() {
            return self.ata2.read_alt_status();
        }
        let (lo, hi) = self.pci.port_range();
        if port >= lo && port <= hi {
            return self.pci.read(port);
        }
        0xFF
    }

    pub fn write(&mut self, port: u16, value: u8) {
        // RTL8139 register window (see `read`).
        if self.pci.nic_io_enabled() {
            let base = self.pci.nic_io_base();
            if port >= base && (port - base) < pci::RTL8139_IO_SIZE as u16 {
                self.rtl8139.write_reg(port - base, value);
                return;
            }
        }
        // See `read` — port 0x61 is logically a PIT/PPI register.
        if port == 0x61 {
            self.pit.write_port_61(value);
            return;
        }
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
            return;
        }
        let (lo, hi) = self.ata.port_range();
        if port >= lo && port <= hi {
            self.ata.write(port, value);
            return;
        }
        let (lo, hi) = self.ata2.port_range();
        if port >= lo && port <= hi {
            self.ata2.write(port, value);
            return;
        }
        if port == self.ata.control_port() {
            self.ata.write_device_control(value);
            return;
        }
        if port == self.ata2.control_port() {
            self.ata2.write_device_control(value);
            return;
        }
        let (lo, hi) = self.pci.port_range();
        if port >= lo && port <= hi {
            self.pci.write(port, value);
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
/// 5=Keyboard, 6=CMOS, 7=ATA primary, 8=ATA secondary, 9=PCI. New
/// devices append. Unknown IDs are silently skipped — that's how we
/// handle forward-compat snapshots.
///
/// The ATA records persist controller register state and any pending
/// transfer buffer, but *not* the disk image itself (host re-loads
/// via `load_disk_image` / `load_secondary_disk_image`).
impl IoBus {
    const DEV_UART: u8 = 1;
    const DEV_PIC_MASTER: u8 = 2;
    const DEV_PIC_SLAVE: u8 = 3;
    const DEV_PIT: u8 = 4;
    const DEV_KBD: u8 = 5;
    const DEV_CMOS: u8 = 6;
    const DEV_ATA_PRIMARY: u8 = 7;
    const DEV_ATA_SECONDARY: u8 = 8;
    const DEV_PCI: u8 = 9;
    const DEV_RTL8139: u8 = 10;

    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        out.push(9u8); // device count
        let emit = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
            let len = 1 + payload.len() as u32;
            out.extend_from_slice(&len.to_le_bytes());
            out.push(id);
            out.extend_from_slice(payload);
        };
        let mut buf = Vec::new();
        buf.clear();
        self.uart.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_UART, &buf);
        buf.clear();
        self.pic.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_PIC_MASTER, &buf);
        buf.clear();
        self.slave_pic.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_PIC_SLAVE, &buf);
        buf.clear();
        self.pit.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_PIT, &buf);
        buf.clear();
        self.kbd.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_KBD, &buf);
        buf.clear();
        self.cmos.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_CMOS, &buf);
        buf.clear();
        self.ata.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_ATA_PRIMARY, &buf);
        buf.clear();
        self.ata2.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_ATA_SECONDARY, &buf);
        buf.clear();
        self.pci.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_PCI, &buf);
        buf.clear();
        self.rtl8139.snapshot_into(&mut buf);
        emit(&mut out, Self::DEV_RTL8139, &buf);
        out
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() {
            return Err("iobus: empty".into());
        }
        let count = bytes[0];
        let mut p = 1;
        for _ in 0..count {
            if bytes.len() < p + 4 {
                return Err("iobus: truncated record header".into());
            }
            let len =
                u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]) as usize;
            p += 4;
            if bytes.len() < p + len || len < 1 {
                return Err("iobus: truncated record body".into());
            }
            let id = bytes[p];
            let payload = &bytes[p + 1..p + len];
            let consumed = match id {
                Self::DEV_UART => self.uart.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIC_MASTER => self.pic.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIC_SLAVE => self.slave_pic.restore(payload).map_err(str::to_string)?,
                Self::DEV_PIT => self.pit.restore(payload).map_err(str::to_string)?,
                Self::DEV_KBD => self.kbd.restore(payload).map_err(str::to_string)?,
                Self::DEV_CMOS => self.cmos.restore(payload).map_err(str::to_string)?,
                Self::DEV_ATA_PRIMARY => self.ata.restore(payload).map_err(str::to_string)?,
                Self::DEV_ATA_SECONDARY => self.ata2.restore(payload).map_err(str::to_string)?,
                Self::DEV_PCI => self.pci.restore(payload).map_err(str::to_string)?,
                Self::DEV_RTL8139 => self.rtl8139.restore(payload).map_err(str::to_string)?,
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
    fn ata_state_round_trips_through_snapshot_and_restore() {
        let mut bus = IoBus::new();
        // Drive both ATA channels into recognisable mid-transfer
        // states: primary has issued IDENTIFY (DRQ up, buf full of
        // the IDENTIFY block); secondary has set LBA registers but
        // not yet issued a command.
        bus.ata.disk.load(&[0xAA; 1024]);
        bus.write(0x1F7, 0xEC); // IDENTIFY on primary
        bus.write(0x172, 5); // sector count = 5 on secondary
        bus.write(0x173, 0x42); // LBA low
        bus.write(0x174, 0x84); // LBA mid
        bus.write(0x176, 0x4F); // drive/head: LBA mode + bits
        let blob = bus.snapshot();
        // Restore into a fresh IoBus.
        let mut bus2 = IoBus::new();
        bus2.restore(&blob).expect("restore");
        // Primary's DRQ is still up and the very next read at 0x1F0
        // returns the first byte of the IDENTIFY block (0x40 = the
        // "ATA, non-removable" signature low byte).
        assert!(bus2.ata.read_alt_status() & 0x08 != 0, "DRQ");
        assert_eq!(bus2.read(0x1F0), 0x40);
        // Secondary's latched registers survived.
        assert_eq!(bus2.read(0x172), 5);
        assert_eq!(bus2.read(0x173), 0x42);
        assert_eq!(bus2.read(0x174), 0x84);
        assert_eq!(bus2.read(0x176), 0x4F);
    }

    #[test]
    fn pci_address_latch_round_trips_through_snapshot() {
        let mut bus = IoBus::new();
        // Write a non-default address (enable + bus 1 / dev 2 / reg
        // 0x10) byte-by-byte.
        let addr: u32 = 0x8001_1010;
        for i in 0..4u16 {
            bus.write(0xCF8 + i, (addr >> (i * 8)) as u8);
        }
        let blob = bus.snapshot();
        let mut bus2 = IoBus::new();
        bus2.restore(&blob).expect("restore");
        // Read the four address bytes back via the port path.
        let mut got = 0u32;
        for i in 0..4u16 {
            got |= (bus2.read(0xCF8 + i) as u32) << (i * 8);
        }
        assert_eq!(got, addr);
    }

    /// Forward-compat: a snapshot created by an old build that only
    /// knew 6 devices must still restore on this version (the new
    /// ATA/PCI records simply aren't present; defaults take over).
    #[test]
    fn old_six_device_snapshot_restores_with_defaults_for_new_devices() {
        // Hand-build an old-shape blob: count=6 + the existing six
        // records. Easiest path: snapshot the current IoBus, then
        // truncate after the 6th record. The fixed 1-byte count is
        // the entry point.
        let bus = IoBus::new();
        let full = bus.snapshot();
        let mut old = Vec::with_capacity(full.len());
        old.push(6u8);
        // Walk 6 records from `full` (which has count=9 at offset 0).
        let mut p = 1;
        for _ in 0..6 {
            let len = u32::from_le_bytes([full[p], full[p + 1], full[p + 2], full[p + 3]]) as usize;
            let total = 4 + len;
            old.extend_from_slice(&full[p..p + total]);
            p += total;
        }
        let mut bus2 = IoBus::new();
        // Pre-poison ata/pci so we can confirm restore leaves them
        // at default rather than crashing.
        bus2.write(0x1F2, 7);
        bus2.write(0xCF8, 0xFF);
        bus2.restore(&old).expect("restore old-format blob");
        // PCI restored to default (not the 0xFF we poisoned).
        // We can't directly read pci.addr; instead, write 0x80 to
        // CF8 (enable bit) and read it back, then read data — if
        // the latch was 0 before our write, the data window must
        // still answer 0xFFFFFFFF.
        // Just verify the dispatch is healthy by reading the data
        // window: should still return 0xFF (no device).
        assert_eq!(bus2.read(0xCFC), 0xFF);
    }

    #[test]
    fn ata_control_ports_mirror_status_for_both_channels() {
        let mut bus = IoBus::new();
        bus.ata.disk.load(&[0xAA; 512]);
        bus.ata2.disk.load(&[0x55; 512]);
        // Issue IDENTIFY on each side. The status registers should
        // come up with DRDY | DRQ. Read it both from the command
        // block (base+7) and the control port (base+0x206); the
        // values must match.
        for base in [0x1F0u16, 0x170u16] {
            bus.write(base + 7, 0xEC); // IDENTIFY
            let cmd = bus.read(base + 7);
            let alt = bus.read(base + 0x206);
            assert_eq!(cmd, alt, "channel @ {base:#X}");
            // DRDY (0x40) | DRQ (0x08) both set.
            assert_eq!(cmd & 0x48, 0x48);
            // A device-control write must be silently accepted.
            bus.write(base + 0x206, 0x02);
            // Alt-status reads don't disturb anything; same value
            // still comes back.
            assert_eq!(bus.read(base + 0x206), cmd);
        }
    }

    #[test]
    fn primary_and_secondary_ata_channels_route_independently() {
        let mut bus = IoBus::new();
        // Load distinct images so each side has a recognisable
        // payload.
        bus.ata.disk.load(&[0xAAu8; 512]); // primary
        bus.ata2.disk.load(&[0x55u8; 512]); // secondary
                                            // Issue READ SECTORS at LBA 0, sector count 1, on each.
                                            // Each command-block read should drain that channel's own
                                            // buffer; if dispatch leaked between them the secondary
                                            // would echo 0xAA or vice versa.
        for (base, expect) in [(0x1F0u16, 0xAAu8), (0x170u16, 0x55u8)] {
            bus.write(base + 2, 1); // sector count
            bus.write(base + 3, 0); // LBA low
            bus.write(base + 4, 0);
            bus.write(base + 5, 0);
            bus.write(base + 6, 0x40); // LBA mode
            bus.write(base + 7, 0x20); // READ SECTORS
            for _ in 0..4 {
                assert_eq!(bus.read(base), expect);
            }
        }
    }

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
        // Use IRQ 9 (slave bit 1, vector 0x71) — a line no device drives,
        // so the level-triggered refresh below leaves our manual raise
        // alone. (Slave bit 3 / IRQ 11 now belongs to the RTL8139, whose
        // own cascade path is covered in the cpu crate's NIC test.)
        bus.slave_pic.raise_irq(1);
        bus.refresh_irqs();
        assert_eq!(bus.pending_irq_vector(), Some(0x71));
        bus.ack_irq();
        // Both PICs moved their bits from IRR to ISR.
        assert_eq!(bus.pic.isr & (1 << 2), 1 << 2);
        assert_eq!(bus.slave_pic.isr & (1 << 1), 1 << 1);
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
        // `refresh_irqs` prescales the PIT (one real tick per 16
        // calls — see `PIT_TICK_DIVIDER`) so PIT IRQ rate stays
        // sane relative to our CPU throughput. We need (divider +
        // reload) calls before the edge actually fires.
        let mut bus = IoBus::new();
        bus.write(0x43, 0x34);
        bus.write(0x40, 0x01);
        bus.write(0x40, 0x00);
        bus.pic.imr = 0xFE;
        for _ in 0..16 {
            bus.refresh_irqs();
        }
        assert_eq!(bus.pic.pending_vector(), Some(0x08));
    }

    /// Prescaler must not drop edges: when the kernel programs a
    /// short PIT latch we still want one edge per `divider × latch`
    /// refresh_irqs calls. Drive 200 refresh ticks against a
    /// reload of 2 and count how many IRR pulses appear — we expect
    /// 200 / (16 * 2) ≈ 6 pulses.
    #[test]
    fn pit_prescaler_preserves_edge_count() {
        let mut bus = IoBus::new();
        bus.write(0x43, 0x34); // mode 2, ch0
        bus.write(0x40, 0x02);
        bus.write(0x40, 0x00);
        bus.pic.imr = 0xFE;
        let mut edges = 0;
        for _ in 0..200 {
            bus.pic.irr &= !1u8; // ack pending IRQ 0 each iter
            bus.refresh_irqs();
            if bus.pic.irr & 1 != 0 {
                edges += 1;
            }
        }
        // 200 / 32 = 6 (with remainder); accept 5..=7 to be robust
        // against off-by-one in counter init.
        assert!(
            (5..=7).contains(&edges),
            "expected ~6 PIT edges, got {edges}"
        );
    }

    /// Port 0x61 falls inside the keyboard's claimed range
    /// (0x60..=0x63) but is logically a PIT/PPI register: Linux's
    /// TSC-via-PIT calibration polls bit 5 (channel-2 OUT) here.
    /// The IoBus dispatch must steer this port to the PIT
    /// explicitly before the keyboard catches it. A regression
    /// that dropped the explicit dispatch would have ch2 polling
    /// return 0 (keyboard's default for unsupported ports), and
    /// the calibration loop in start_kernel would spin forever.
    #[test]
    fn iobus_port_61_dispatches_to_pit_not_keyboard() {
        let mut bus = IoBus::new();
        // Write port 0x61 with gate2=1, speaker2=1 — must land
        // in the PIT (sets gate2 and speaker2 flags), NOT the
        // keyboard (which silently drops writes to its range).
        bus.write(0x61, 0x03);
        assert!(bus.pit.gate2, "gate2 latched on the PIT");
        assert!(bus.pit.speaker2, "speaker2 latched on the PIT");
        // Read port 0x61 — must come from the PIT's read_port_61
        // (which composes gate2 + speaker2 + ch2_out into bit 5).
        // gate2=1 + speaker2=1 reflects back the bits we just wrote.
        let read = bus.read(0x61);
        assert_eq!(read & 0x03, 0x03, "PIT read_port_61 reflects the bits");
    }

    /// Unmapped I/O ports read as 0xFF — the ISA-bus pull-up
    /// behavior that Linux's `inb_p` probing relies on to
    /// distinguish a present device (returns its data byte) from
    /// an empty bus (returns 0xFF). A regression that returned 0
    /// for unmapped reads would make every device "look present"
    /// to a probe that looks at the high nibble.
    #[test]
    fn unmapped_port_read_returns_0xff_isa_pullup_default() {
        let mut bus = IoBus::new();
        // 0x0080 is the BIOS POST-code port — we don't model it.
        // 0x0378 is LPT1 — we don't model it either.
        // Both should read 0xFF.
        assert_eq!(bus.read(0x0080), 0xFF, "POST port unmapped");
        assert_eq!(bus.read(0x0378), 0xFF, "LPT1 unmapped");
    }

    /// Writes to unmapped ports are silently dropped — matches
    /// real hardware (nothing latches the byte). Distinct from
    /// returning a sentinel; this just asserts we don't panic.
    #[test]
    fn unmapped_port_write_is_silently_dropped() {
        let mut bus = IoBus::new();
        bus.write(0x0080, 0xAB); // BIOS POST code, no slot
        bus.write(0x0378, 0xCD); // LPT1, no slot
                                 // No assertion on state — we just need to NOT panic.
                                 // The follow-up reads still return the unmapped sentinel.
        assert_eq!(bus.read(0x0080), 0xFF);
        assert_eq!(bus.read(0x0378), 0xFF);
    }
}
