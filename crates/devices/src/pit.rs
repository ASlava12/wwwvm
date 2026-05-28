//! 8254 Programmable Interval Timer — minimal channel-0 subset.
//!
//! What we model:
//!   * Channel 0 with reload register, current counter, and modes 0
//!     (one-shot) and 2/3 (periodic — both reload on terminal count
//!     and behave identically for IRQ generation purposes).
//!   * Control-word writes to 0x43 with SC=0 and access pattern
//!     RW=3 (LSB then MSB). Other RW patterns are accepted but the
//!     reload-value writes will silently not latch.
//!   * Counter Latch Command (CW = 0x00..0x3F with RW=0): snapshots
//!     the live counter into a hold register so two-byte reads see
//!     a consistent value even if the counter ticks between them.
//!     Linux's PIT clocksource readback uses this every tick.
//!   * `tick(n)` decrements the counter by n; on terminal count it
//!     latches a pending edge for IRQ 0 and (mode 2/3) reloads.
//!
//! Channels 1 and 2 accept writes silently and don't generate IRQs.

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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
