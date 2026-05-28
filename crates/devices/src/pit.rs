//! 8254 Programmable Interval Timer — channels 0 and 2 subset.
//!
//! What we model:
//!   * Channel 0 with reload register, current counter, and modes 0
//!     (one-shot) and 2/3 (periodic — both reload on terminal count
//!     and behave identically for IRQ generation purposes).
//!   * Channel 2 minimal: mode 0 (one-shot) only, gated by port 0x61
//!     bit 0. Linux uses this for TSC-via-PIT calibration: it
//!     programs ch2 with a known countdown and polls port 0x61 bit
//!     5 (TIMER_2_OUTPUT) until the OUT line goes high. Without ch2
//!     the calibration polls forever and start_kernel never
//!     advances past tsc_init.
//!   * Port 0x61 (NMI Status & Control Register) — bits 0 (timer 2
//!     gate enable) and 1 (speaker data) are writable; bit 5
//!     reflects channel 2's OUT line. Other bits read back as 0
//!     and writes to them are dropped.
//!   * Control-word writes to 0x43 with SC=0 or SC=2 and access
//!     pattern RW=3 (LSB then MSB). Counter Latch Command (CW with
//!     RW=0) snapshots channel 0 only.
//!   * `tick(n)` decrements channel 0 unconditionally and channel 2
//!     only while its gate is high.
//!
//! Channel 1 (DRAM refresh) accepts writes silently. Mode > 0 on
//! channel 2 isn't modeled — Linux only uses mode 0 for calibration.

use crate::IoDevice;

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
    /// Counter-latch hold register. `Some(value)` after a Counter
    /// Latch Command (CW with RW=0); two subsequent reads from the
    /// channel-0 data port return LSB then MSB of `value` and then
    /// `ch0_latch` clears. While latched, reads do not see live
    /// counter updates — that's the entire point of the latch.
    ch0_latch: Option<u16>,
    /// Next byte to return when `ch0_latch` is Some: 0 = LSB, 1 = MSB.
    /// Also tracks the LSB/MSB phase for unlatched RW=3 reads.
    read_state: u8,
    // ----- Channel 2 (Linux TSC-via-PIT calibration) -----
    pub ch2_reload: u16,
    pub ch2_counter: u32,
    pub ch2_running: bool,
    /// Channel 2 OUT line. In mode 0 (the only mode we model for
    /// ch2) OUT is 0 while counting and flips to 1 on terminal
    /// count, where it latches until the next control-word write or
    /// reload. Port 0x61 bit 5 reads from here.
    pub ch2_out: bool,
    ch2_write_state: u8,
    ch2_pending_lsb: u8,
    /// Port 0x61 bit 0 — gate for channel 2. When clear the counter
    /// is frozen (matches the 8254 behavior of holding the count on
    /// gate-low in mode 0). Linux sets this before programming ch2.
    pub gate2: bool,
    /// Port 0x61 bit 1 — speaker data. Stored for round-trip but
    /// otherwise unused; nothing in the model produces audio.
    pub speaker2: bool,
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
            ch0_latch: None,
            read_state: 0,
            ch2_reload: 0,
            ch2_counter: 0,
            ch2_running: false,
            ch2_out: false,
            ch2_write_state: 0,
            ch2_pending_lsb: 0,
            gate2: false,
            speaker2: false,
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
        if n == 0 {
            return;
        }
        // Channel 0 — IRQ source on terminal count.
        if self.ch0_running {
            let mut remaining = n;
            loop {
                if self.ch0_counter == 0 {
                    match self.ch0_mode {
                        0 => {
                            self.ch0_running = false;
                            break;
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
                    break;
                }
            }
        }
        // Channel 2 — mode 0 only; gated by port 0x61 bit 0. We
        // don't reload on terminal count (mode 0 holds OUT high
        // until the counter is reprogrammed), so a single saturating
        // sub is enough.
        if self.ch2_running && self.gate2 {
            let take = n.min(self.ch2_counter);
            self.ch2_counter -= take;
            if self.ch2_counter == 0 {
                self.ch2_out = true;
                self.ch2_running = false;
            }
        }
    }

    /// Read NMI Status & Control register (port 0x61). Bit 5
    /// reflects channel 2's OUT line, which Linux's TSC-via-PIT
    /// calibration polls to detect when its programmed countdown
    /// has elapsed. Other bits round-trip the last write.
    pub fn read_port_61(&self) -> u8 {
        let mut v = 0u8;
        if self.gate2 {
            v |= 0x01;
        }
        if self.speaker2 {
            v |= 0x02;
        }
        if self.ch2_out {
            v |= 0x20;
        }
        v
    }

    /// Write NMI Status & Control register (port 0x61). Only bits 0
    /// (channel 2 gate) and 1 (speaker data) are stored; everything
    /// else is dropped. Lowering the gate freezes channel 2 in
    /// place; raising it resumes counting from where we left off.
    pub fn write_port_61(&mut self, value: u8) {
        self.gate2 = value & 0x01 != 0;
        self.speaker2 = value & 0x02 != 0;
    }

    /// Consume the channel-0 edge, if any. IoBus calls this each
    /// `refresh_irqs` and turns a true result into a one-shot IRR
    /// set on the PIC.
    pub fn take_ch0_pending(&mut self) -> bool {
        let p = self.ch0_pending_edge;
        self.ch0_pending_edge = false;
        p
    }

    /// Snapshot layout, 12 bytes: reload (u16), counter (u32), mode
    /// (u8), flags (u8 — bit0=running, bit1=pending_edge,
    /// bit2=latched, bit3=read_state), write_state (u8), pending_lsb
    /// (u8), latch_value (u16). Older 10-byte snapshots (no latch
    /// fields) restore cleanly with latch=None and read_state=0.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ch0_reload.to_le_bytes());
        out.extend_from_slice(&self.ch0_counter.to_le_bytes());
        out.push(self.ch0_mode);
        let flags = (self.ch0_running as u8)
            | ((self.ch0_pending_edge as u8) << 1)
            | ((self.ch0_latch.is_some() as u8) << 2)
            | ((self.read_state & 1) << 3);
        out.push(flags);
        out.push(self.write_state);
        out.push(self.pending_lsb);
        out.extend_from_slice(&self.ch0_latch.unwrap_or(0).to_le_bytes());
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 10 {
            return Err("pit: truncated");
        }
        self.ch0_reload = u16::from_le_bytes([bytes[0], bytes[1]]);
        self.ch0_counter = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        self.ch0_mode = bytes[6];
        let flags = bytes[7];
        self.ch0_running = flags & 1 != 0;
        self.ch0_pending_edge = flags & 2 != 0;
        self.write_state = bytes[8];
        self.pending_lsb = bytes[9];
        // Latch fields are optional — old 10-byte snapshots predate
        // counter-latch support and restore with latch cleared.
        if bytes.len() >= 12 {
            let latched = flags & (1 << 2) != 0;
            self.read_state = (flags >> 3) & 1;
            let val = u16::from_le_bytes([bytes[10], bytes[11]]);
            self.ch0_latch = if latched { Some(val) } else { None };
            Ok(12)
        } else {
            self.ch0_latch = None;
            self.read_state = 0;
            Ok(10)
        }
    }
}

impl IoDevice for Pit {
    fn port_range(&self) -> (u16, u16) {
        (self.base_port, self.base_port + 3)
    }

    fn read(&mut self, port: u16) -> u8 {
        if port - self.base_port != 0 {
            return 0;
        }
        // Latched reads (Counter Latch Command was issued): return
        // LSB then MSB of the snapshot; clear the latch after the
        // MSB read so the next pair sees the live counter again.
        if let Some(latched) = self.ch0_latch {
            if self.read_state == 0 {
                self.read_state = 1;
                return (latched & 0xFF) as u8;
            } else {
                self.ch0_latch = None;
                self.read_state = 0;
                return (latched >> 8) as u8;
            }
        }
        // Unlatched: read the live counter using the same LSB/MSB
        // sequencing the kernel programmed. We only model RW=3
        // (the only access pattern Linux uses for channel 0); the
        // counter is sampled fresh each pair, which matches what
        // happens on real silicon when the user skips the latch step
        // (the two bytes can race, but for our deterministic counter
        // they don't).
        let cur = self.ch0_counter as u16;
        if self.read_state == 0 {
            self.read_state = 1;
            (cur & 0xFF) as u8
        } else {
            self.read_state = 0;
            (cur >> 8) as u8
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base_port {
            3 => {
                let sc = value >> 6;
                let rw = (value >> 4) & 3;
                let mode = (value >> 1) & 7;
                if sc == 0 && rw == 0 {
                    // Counter Latch Command: snapshot the live counter
                    // into the hold register. A second latch issued
                    // while one is already pending is ignored on real
                    // silicon — we match by leaving `ch0_latch` alone.
                    if self.ch0_latch.is_none() {
                        self.ch0_latch = Some(self.ch0_counter as u16);
                        self.read_state = 0;
                    }
                } else if sc == 0 && rw == 3 {
                    self.ch0_mode = mode;
                    self.write_state = 0;
                    self.read_state = 0;
                    self.ch0_latch = None;
                    self.ch0_pending_edge = false;
                    self.ch0_running = false;
                } else if sc == 2 && rw == 3 {
                    // Channel 2 init: reset write phase, drop OUT
                    // back to 0 so the kernel's poll sees a clean
                    // starting edge, and arm for the LSB+MSB pair
                    // that follows. Mode bits are not stored because
                    // we only model mode 0 — Linux's calibration
                    // never asks for anything else.
                    let _ = mode;
                    self.ch2_write_state = 0;
                    self.ch2_out = false;
                    self.ch2_running = false;
                }
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
            2 => {
                // Channel 2 data port — LSB then MSB. The second
                // byte arms the counter; the gate (port 0x61 bit 0)
                // then controls whether it actually decrements.
                if self.ch2_write_state == 0 {
                    self.ch2_pending_lsb = value;
                    self.ch2_write_state = 1;
                } else {
                    let reload = (self.ch2_pending_lsb as u16) | ((value as u16) << 8);
                    self.ch2_reload = reload;
                    self.ch2_counter = if reload == 0 { 0x10000 } else { reload as u32 };
                    self.ch2_running = true;
                    self.ch2_out = false;
                    self.ch2_write_state = 0;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel 2 + port 0x61 model the loop Linux uses to derive the
    /// TSC frequency from the PIT's known crystal rate:
    ///   1. write port 0x61 to gate timer 2 on, speaker off
    ///   2. write 0x43 with SC=2 RW=3 mode 0 (= 0xB0)
    ///   3. write 0x42 LSB and 0x42 MSB to load the countdown
    ///   4. poll port 0x61 bit 5 — clear while counting, set on
    ///      terminal count
    ///
    /// Without this, the calibration loop in start_kernel spins
    /// forever and the kernel never advances past tsc_init.
    #[test]
    fn pit_ch2_terminal_count_flips_port61_bit5() {
        let mut pit = Pit::standard();
        // Enable gate, speaker off.
        pit.write_port_61(0x01);
        // Control word: SC=2 (channel 2), RW=3 (LSB then MSB),
        // mode=0 (interrupt on terminal count), BCD=0 → 0xB0.
        pit.write(0x43, 0xB0);
        // Counter = 5 ticks.
        pit.write(0x42, 5);
        pit.write(0x42, 0);
        // Pre-terminal: OUT=0, port 0x61 bit 5 reads back as 0.
        assert_eq!(pit.read_port_61() & 0x20, 0);
        // 4 ticks — still counting.
        pit.tick(4);
        assert_eq!(pit.read_port_61() & 0x20, 0);
        // 5th tick reaches zero → OUT high, bit 5 latches set.
        pit.tick(1);
        assert_eq!(pit.read_port_61() & 0x20, 0x20);
        // OUT stays high after additional ticks (mode 0 holds).
        pit.tick(100);
        assert_eq!(pit.read_port_61() & 0x20, 0x20);
    }

    /// Gating ch2 off via port 0x61 bit 0 freezes the counter — the
    /// 8254 spec for mode 0 says the count holds while GATE is low.
    /// Linux relies on this to set up the channel before letting it
    /// run.
    #[test]
    fn pit_ch2_gate_low_freezes_counter() {
        let mut pit = Pit::standard();
        pit.write(0x43, 0xB0);
        pit.write(0x42, 0x10);
        pit.write(0x42, 0x00);
        // Gate is still 0 (default) — ticks must not decrement.
        pit.tick(100);
        assert_eq!(pit.read_port_61() & 0x20, 0, "gate=0 must keep OUT low");
        // Raise gate; counter now decrements.
        pit.write_port_61(0x01);
        pit.tick(0x10);
        assert_eq!(pit.read_port_61() & 0x20, 0x20);
    }

    #[test]
    fn mode2_fires_periodic_edge_every_reload_ticks() {
        let mut pit = Pit::standard();
        // SC=0, RW=3, mode=2  → 0x34
        pit.write(0x43, 0x34);
        pit.write(0x40, 0x03);
        pit.write(0x40, 0x00);
        pit.tick(3);
        assert!(pit.take_ch0_pending());
        assert!(!pit.take_ch0_pending());
        pit.tick(3);
        assert!(pit.take_ch0_pending());
    }

    #[test]
    fn mode0_oneshot_halts_after_first_terminal_count() {
        let mut pit = Pit::standard();
        // Mode 0 — 0x30
        pit.write(0x43, 0x30);
        pit.write(0x40, 0x02);
        pit.write(0x40, 0x00);
        pit.tick(2);
        assert!(pit.take_ch0_pending());
        pit.tick(100);
        assert!(!pit.take_ch0_pending());
    }

    /// Counter Latch Command (CW=0x00): the live counter is
    /// snapshotted into the hold register so two-byte reads see
    /// a consistent value even if `tick()` runs in between. This
    /// is how Linux's PIT clocksource reads the counter.
    #[test]
    fn counter_latch_command_freezes_value_across_reads() {
        let mut pit = Pit::standard();
        // Mode 2, RW=3 — 0x34. Reload = 0xABCD.
        pit.write(0x43, 0x34);
        pit.write(0x40, 0xCD);
        pit.write(0x40, 0xAB);
        // Issue Counter Latch (SC=0, RW=0) — value to latch is 0xABCD.
        pit.write(0x43, 0x00);
        // Tick between the two reads — the latched value must NOT
        // change. (Pick a small N so we don't trip the terminal-
        // count edge case.)
        let lsb = pit.read(0x40);
        pit.tick(0x10);
        let msb = pit.read(0x40);
        assert_eq!(lsb, 0xCD, "latched LSB");
        assert_eq!(msb, 0xAB, "latched MSB (frozen despite tick)");
        // After the LSB+MSB pair the latch clears — subsequent
        // reads see the live counter (post-tick value, 0xABCD - 0x10
        // = 0xABBD).
        let live_lsb = pit.read(0x40);
        let live_msb = pit.read(0x40);
        assert_eq!(live_lsb, 0xBD);
        assert_eq!(live_msb, 0xAB);
    }

    /// A second Counter Latch issued while one is still pending is
    /// ignored on real 8254s. We match that — otherwise a fast
    /// re-latch loop could lose track of pending bytes.
    #[test]
    fn second_latch_during_pending_read_is_ignored() {
        let mut pit = Pit::standard();
        pit.write(0x43, 0x34);
        pit.write(0x40, 0xCD);
        pit.write(0x40, 0xAB);
        pit.write(0x43, 0x00); // first latch — value 0xABCD
        let _ = pit.read(0x40); // consume LSB
                                // Mutate the counter, then try to re-latch — should be ignored.
        pit.tick(0x100);
        pit.write(0x43, 0x00);
        let msb = pit.read(0x40);
        assert_eq!(msb, 0xAB, "second latch must not clobber first");
    }

    /// Latch state survives snapshot/restore — required so that a
    /// VM snapshot taken mid-read sequence resumes cleanly.
    #[test]
    fn snapshot_roundtrip_preserves_latch_state() {
        let mut pit = Pit::standard();
        pit.write(0x43, 0x34);
        pit.write(0x40, 0x34);
        pit.write(0x40, 0x12);
        pit.write(0x43, 0x00); // latch 0x1234
        let _ = pit.read(0x40); // consume LSB, read_state=1
        let mut buf = Vec::new();
        pit.snapshot_into(&mut buf);
        assert_eq!(buf.len(), 12);

        let mut pit2 = Pit::standard();
        let consumed = pit2.restore(&buf).unwrap();
        assert_eq!(consumed, 12);
        // The remaining read should still return the latched MSB.
        assert_eq!(pit2.read(0x40), 0x12);
        // And after that the latch is cleared.
        assert!(pit2.ch0_latch.is_none());
    }

    /// Old 10-byte snapshots (pre-latch) restore as latch=None and
    /// read_state=0. Keeps existing snapshots loadable.
    #[test]
    fn restore_legacy_10_byte_snapshot() {
        let mut pit = Pit::standard();
        // Synthesize the 10-byte payload directly.
        let legacy = [
            0x00, 0x01, // reload = 0x0100
            0x42, 0x00, 0x00, 0x00, // counter = 0x42
            0x02, // mode = 2
            0x01, // flags: running, no pending edge
            0x00, // write_state = 0
            0x00, // pending_lsb = 0
        ];
        let consumed = pit.restore(&legacy).unwrap();
        assert_eq!(consumed, 10);
        assert!(pit.ch0_latch.is_none());
        assert_eq!(pit.read_state, 0);
        assert_eq!(pit.ch0_reload, 0x0100);
        assert_eq!(pit.ch0_counter, 0x42);
    }
}
