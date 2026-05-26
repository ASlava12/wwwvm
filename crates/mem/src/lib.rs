//! Physical memory for the guest. A flat byte buffer with little-endian
//! word accessors. Out-of-range accesses return zero on read and are
//! silently dropped on write — matches how unmapped DRAM behaves on real
//! hardware for our purposes and keeps the CPU loop branch-free.

#![forbid(unsafe_code)]

pub struct Memory {
    bytes: Vec<u8>,
}

impl Memory {
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0; size],
        }
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    pub fn read_u8(&self, addr: u32) -> u8 {
        let a = addr as usize;
        if a < self.bytes.len() {
            self.bytes[a]
        } else {
            0
        }
    }

    pub fn write_u8(&mut self, addr: u32, value: u8) {
        let a = addr as usize;
        if a < self.bytes.len() {
            self.bytes[a] = value;
        }
    }

    pub fn read_u16(&self, addr: u32) -> u16 {
        let lo = self.read_u8(addr) as u16;
        let hi = self.read_u8(addr.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }

    pub fn write_u16(&mut self, addr: u32, value: u16) {
        self.write_u8(addr, value as u8);
        self.write_u8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        let start = addr as usize;
        let end = start.saturating_add(data.len()).min(self.bytes.len());
        let n = end.saturating_sub(start);
        self.bytes[start..start + n].copy_from_slice(&data[..n]);
    }

    /// Borrow the entire backing buffer. Used by the VM's snapshot
    /// helper to write 1 MB out in a single pass.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Replace the entire backing buffer in one shot. Returns Err if
    /// `data.len()` does not match the configured RAM size — restoring
    /// a snapshot from a VM built with different memory sizing would
    /// otherwise silently truncate or leave stale tail bytes.
    pub fn restore_full(&mut self, data: &[u8]) -> Result<(), usize> {
        if data.len() != self.bytes.len() {
            return Err(self.bytes.len());
        }
        self.bytes.copy_from_slice(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_byte_round_trip() {
        let mut m = Memory::new(64);
        m.write_u8(10, 0xAB);
        assert_eq!(m.read_u8(10), 0xAB);
    }

    #[test]
    fn word_is_little_endian() {
        let mut m = Memory::new(64);
        m.write_u16(0, 0xBEEF);
        assert_eq!(m.read_u8(0), 0xEF);
        assert_eq!(m.read_u8(1), 0xBE);
        assert_eq!(m.read_u16(0), 0xBEEF);
    }

    #[test]
    fn out_of_range_read_returns_zero_write_is_noop() {
        let mut m = Memory::new(16);
        m.write_u8(100, 0xFF);
        assert_eq!(m.read_u8(100), 0);
    }

    #[test]
    fn write_slice_clips_at_boundary() {
        let mut m = Memory::new(8);
        m.write_slice(6, &[1, 2, 3, 4]);
        assert_eq!(m.read_u8(6), 1);
        assert_eq!(m.read_u8(7), 2);
    }

    #[test]
    fn restore_full_round_trips() {
        let mut m = Memory::new(16);
        m.write_u8(0, 0xAA);
        let snap = m.as_slice().to_vec();
        let mut m2 = Memory::new(16);
        m2.restore_full(&snap).unwrap();
        assert_eq!(m2.read_u8(0), 0xAA);
    }

    #[test]
    fn restore_full_rejects_size_mismatch() {
        let mut m = Memory::new(16);
        let err = m.restore_full(&[0u8; 8]).unwrap_err();
        assert_eq!(err, 16);
    }
}
