//! In-memory disk image with BIOS-style sector access. Not port-
//! mapped — the host-side BIOS shim (`vm::bios_hook`) calls into
//! this directly to service INT 0x13 reads. Real PCs use IDE/ATA
//! port reads through 0x1F0..0x1F7; we'll grow that path when a guest
//! disables BIOS and pokes the controller itself.

pub const SECTOR_SIZE: usize = 512;

/// A flat disk image addressed by 0-based LBA. Empty by default;
/// call [`Disk::load`] before issuing reads.
#[derive(Default)]
pub struct Disk {
    bytes: Vec<u8>,
}

impl Disk {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Replace the image with `bytes`. Length need not be a sector
    /// multiple — the last partial sector is treated as if padded
    /// with zeros.
    pub fn load(&mut self, bytes: &[u8]) {
        self.bytes.clear();
        self.bytes.extend_from_slice(bytes);
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Read `count` 512-byte sectors starting at `lba` into `dest`.
    /// Returns the number of bytes actually written. Bytes past the
    /// image end read as zero so a guest reading past the image
    /// doesn't crash — it just sees blank space.
    pub fn read_sectors(&self, lba: u32, count: u8, dest: &mut [u8]) -> usize {
        let want = (count as usize) * SECTOR_SIZE;
        let to_write = want.min(dest.len());
        let start = (lba as usize) * SECTOR_SIZE;
        for (i, slot) in dest.iter_mut().take(to_write).enumerate() {
            let off = start + i;
            *slot = if off < self.bytes.len() {
                self.bytes[off]
            } else {
                0
            };
        }
        to_write
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_sector_past_end_returns_zero_padding() {
        let mut d = Disk::new();
        d.load(&[0xAA; 256]); // half a sector
        let mut buf = [0xFFu8; SECTOR_SIZE];
        let n = d.read_sectors(0, 1, &mut buf);
        assert_eq!(n, SECTOR_SIZE);
        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[255], 0xAA);
        assert_eq!(buf[256], 0x00, "zero-padded past image end");
    }

    #[test]
    fn read_sector_one_grabs_second_512_bytes() {
        let mut d = Disk::new();
        let mut img = vec![0x11; SECTOR_SIZE];
        img.extend_from_slice(&[0x22; SECTOR_SIZE]);
        d.load(&img);
        let mut buf = [0u8; SECTOR_SIZE];
        d.read_sectors(1, 1, &mut buf);
        assert!(buf.iter().all(|&b| b == 0x22));
    }
}
