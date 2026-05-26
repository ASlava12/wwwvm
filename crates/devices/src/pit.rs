//! 8254 Programmable Interval Timer — minimal channel-0 subset.
//!
//! What we model:
//!   * Channel 0 with reload register, current counter, and modes 0
//!     (one-shot) and 2/3 (periodic — both reload on terminal count
//!     and behave identically for IRQ generation purposes).
//!   * Control-word writes to 0x43 with SC=0 and access pattern
//!     RW=3 (LSB then MSB). Other RW patterns are accepted but the
//!     reload-value writes will silently not latch.
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

    /// Snapshot: reload (u16) + counter (u32) + mode (u8) + flags (u8:
    /// bit0=running, bit1=pending_edge) + write_state (u8) +
    /// pending_lsb (u8). 10 bytes.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ch0_reload.to_le_bytes());
        out.extend_from_slice(&self.ch0_counter.to_le_bytes());
        out.push(self.ch0_mode);
        let flags = (self.ch0_running as u8) | ((self.ch0_pending_edge as u8) << 1);
        out.push(flags);
        out.push(self.write_state);
        out.push(self.pending_lsb);
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 10 { return Err("pit: truncated"); }
        self.ch0_reload = u16::from_le_bytes([bytes[0], bytes[1]]);
        self.ch0_counter = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        self.ch0_mode = bytes[6];
        let flags = bytes[7];
        self.ch0_running = flags & 1 != 0;
        self.ch0_pending_edge = flags & 2 != 0;
        self.write_state = bytes[8];
        self.pending_lsb = bytes[9];
        Ok(10)
    }
}

impl IoDevice for Pit {
    fn port_range(&self) -> (u16, u16) {
        (self.base_port, self.base_port + 3)
    }

    fn read(&mut self, port: u16) -> u8 {
        if port - self.base_port == 0 {
            (self.ch0_counter & 0xFF) as u8
        } else {
            0
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        match port - self.base_port {
            3 => {
                let sc = value >> 6;
                let rw = (value >> 4) & 3;
                let mode = (value >> 1) & 7;
                if sc == 0 && rw == 3 {
                    self.ch0_mode = mode;
                    self.write_state = 0;
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
}
