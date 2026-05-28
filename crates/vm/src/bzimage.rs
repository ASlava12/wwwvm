//! Linux bzImage header parser. Validates the boot-sector signature
//! and the "HdrS" magic, then extracts the handful of setup-header
//! fields that load-time and kernel-launch logic actually need.
//!
//! This is *parser only* — it does not unpack, decompress, or place
//! the kernel in memory. The intent is to make later ticks
//! (bzImage loader, real-mode setup execution) trivial; the parsing
//! work doesn't need to be redone there.
//!
//! Reference: linux/Documentation/x86/boot.rst. Only the v2.00+
//! header layout is supported — earlier kernels are pre-1.3.73
//! relics not worth chasing.

/// Minimum bzImage size: 512-byte bootsector + at least 1 sector of
/// setup data. Anything shorter is definitely malformed.
const MIN_BZIMAGE_LEN: usize = 1024;

/// Offsets inside the setup header (Linux boot.rst names in comments).
const OFF_SETUP_SECTS: usize = 0x1F1; // setup_sects (u8)
const OFF_BOOT_FLAG: usize = 0x1FE; // boot_flag (u16) = 0xAA55
const OFF_HEADER: usize = 0x202; // "HdrS" magic (4 bytes)
const OFF_VERSION: usize = 0x206; // protocol version (u16)
const OFF_CODE32_START: usize = 0x214; // 32-bit kernel entry (u32)
const OFF_RAMDISK_IMAGE: usize = 0x218; // ramdisk address (u32)
const OFF_RAMDISK_SIZE: usize = 0x21C; // ramdisk size (u32)
const OFF_RELOCATABLE_KERNEL: usize = 0x234; // u8 — non-zero = can move
const OFF_CMDLINE_SIZE: usize = 0x238; // u32 — max command-line length
const OFF_PAYLOAD_OFFSET: usize = 0x248; // u32 — compressed payload offset
const OFF_PAYLOAD_LENGTH: usize = 0x24C; // u32 — compressed payload length
const OFF_INIT_SIZE: usize = 0x260; // u32 — total RAM needed (kernel + scratch)

/// Parsed view of the bzImage setup header. All fields are read
/// directly from the image with no further interpretation — for
/// example, `code32_start` is the linear address the kernel
/// expects to live at, *not* a file offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BzImage {
    /// Number of 512-byte sectors of setup data *not counting* the
    /// boot sector. The Linux convention: if `setup_sects` reads as
    /// 0 in the image, treat it as 4 (the kernel boot-sector code
    /// itself does this remapping).
    pub setup_sects: u8,
    /// Header version, high byte = major, low byte = minor. Used by
    /// callers to gate features that depend on later protocols.
    pub version: u16,
    /// Linear address the protected-mode kernel expects to be at.
    /// Standard bzImage value is `0x0010_0000` (1 MiB).
    pub code32_start: u32,
    /// Linear address of an initial ramdisk. 0 if no initrd.
    pub ramdisk_image: u32,
    /// Length in bytes of the initial ramdisk. 0 if no initrd.
    pub ramdisk_size: u32,
    /// File offset of the protected-mode kernel payload — i.e. the
    /// data that starts at sector `(setup_sects + 1)`.
    pub payload_offset: usize,
    /// Non-zero if the kernel can be loaded at any address that
    /// satisfies its alignment. When zero the loader must place
    /// the kernel at exactly `code32_start`. Modern Linux is
    /// always relocatable (=1). Field added in protocol 2.05.
    pub relocatable_kernel: u8,
    /// Maximum command-line length the kernel will accept. 2.06+.
    /// Loaders should truncate at this size to avoid kernel-side
    /// overflow. Zero in older images.
    pub cmdline_size: u32,
    /// Offset within the bzImage file of the compressed kernel
    /// payload. Distinct from `payload_offset` (which is the
    /// classic "first byte after setup sectors"); the v2.08+
    /// header field at 0x248 lets the loader find the compressed
    /// data without walking the setup sectors. Zero for pre-2.08.
    pub compressed_offset: u32,
    /// Length in bytes of the compressed payload. 2.08+. Lets the
    /// loader allocate exactly that much for the source buffer.
    pub compressed_length: u32,
    /// Total RAM the kernel needs while decompressing — the kernel
    /// itself plus scratch space for the decompressor. A loader
    /// that places the kernel elsewhere (relocatable) needs this
    /// to avoid overwriting the destination region. 2.10+.
    pub init_size: u32,
}

#[derive(Debug)]
pub enum BzImageError {
    /// Image is shorter than a minimal bootsector + 1 setup sector.
    TooSmall(usize),
    /// `boot_flag` at offset 0x1FE wasn't `0xAA55`.
    BadBootFlag(u16),
    /// `HdrS` magic at offset 0x202 didn't match `b"HdrS"`.
    BadHeaderMagic([u8; 4]),
}

impl std::fmt::Display for BzImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall(n) => write!(f, "bzImage too small: {n} bytes (need ≥ 1024)"),
            Self::BadBootFlag(b) => {
                write!(f, "boot_flag at 0x1FE is {b:#06X}, expected 0xAA55")
            }
            Self::BadHeaderMagic(m) => write!(
                f,
                "HdrS magic at 0x202 mismatch: got {m:?}, expected [b'H', b'd', b'r', b'S']"
            ),
        }
    }
}

impl std::error::Error for BzImageError {}

/// Parse a bzImage. Validates the bootsector signature + setup-header
/// magic, then reads the fields that matter for loading.
pub fn parse(bytes: &[u8]) -> Result<BzImage, BzImageError> {
    if bytes.len() < MIN_BZIMAGE_LEN {
        return Err(BzImageError::TooSmall(bytes.len()));
    }
    let boot_flag = u16::from_le_bytes([bytes[OFF_BOOT_FLAG], bytes[OFF_BOOT_FLAG + 1]]);
    if boot_flag != 0xAA55 {
        return Err(BzImageError::BadBootFlag(boot_flag));
    }
    let magic = [
        bytes[OFF_HEADER],
        bytes[OFF_HEADER + 1],
        bytes[OFF_HEADER + 2],
        bytes[OFF_HEADER + 3],
    ];
    if &magic != b"HdrS" {
        return Err(BzImageError::BadHeaderMagic(magic));
    }
    // setup_sects = 0 means "4" per Linux convention (older kernels).
    let raw_setup_sects = bytes[OFF_SETUP_SECTS];
    let setup_sects = if raw_setup_sects == 0 {
        4
    } else {
        raw_setup_sects
    };
    let version = u16::from_le_bytes([bytes[OFF_VERSION], bytes[OFF_VERSION + 1]]);
    let code32_start = read_u32(bytes, OFF_CODE32_START);
    let ramdisk_image = read_u32(bytes, OFF_RAMDISK_IMAGE);
    let ramdisk_size = read_u32(bytes, OFF_RAMDISK_SIZE);
    let payload_offset = (setup_sects as usize + 1) * 512;
    // v2.05+ relocatable flag; older images had reserved zero here,
    // so a 0 read still means "not relocatable" — the correct
    // legacy semantics.
    let relocatable_kernel = bytes[OFF_RELOCATABLE_KERNEL];
    let cmdline_size = read_u32(bytes, OFF_CMDLINE_SIZE);
    let compressed_offset = read_u32(bytes, OFF_PAYLOAD_OFFSET);
    let compressed_length = read_u32(bytes, OFF_PAYLOAD_LENGTH);
    let init_size = read_u32(bytes, OFF_INIT_SIZE);
    Ok(BzImage {
        setup_sects,
        version,
        code32_start,
        ramdisk_image,
        ramdisk_size,
        payload_offset,
        relocatable_kernel,
        cmdline_size,
        compressed_offset,
        compressed_length,
        init_size,
    })
}

fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid bzImage header. `setup_sects` of 0 is
    /// allowed (treated as 4); pass 1 here to make payload land at
    /// offset 1024 unless otherwise noted.
    fn make_bz(setup_sects: u8, code32_start: u32, version: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; MIN_BZIMAGE_LEN];
        bytes[OFF_SETUP_SECTS] = setup_sects;
        bytes[OFF_BOOT_FLAG..OFF_BOOT_FLAG + 2].copy_from_slice(&0xAA55u16.to_le_bytes());
        bytes[OFF_HEADER..OFF_HEADER + 4].copy_from_slice(b"HdrS");
        bytes[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&version.to_le_bytes());
        bytes[OFF_CODE32_START..OFF_CODE32_START + 4].copy_from_slice(&code32_start.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_too_small_image() {
        let bytes = vec![0u8; 512];
        assert!(matches!(parse(&bytes), Err(BzImageError::TooSmall(512))));
    }

    #[test]
    fn rejects_bad_boot_flag() {
        let mut bytes = make_bz(1, 0x10_0000, 0x020D);
        bytes[OFF_BOOT_FLAG] = 0x00;
        bytes[OFF_BOOT_FLAG + 1] = 0x00;
        assert!(matches!(parse(&bytes), Err(BzImageError::BadBootFlag(0))));
    }

    #[test]
    fn rejects_bad_header_magic() {
        let mut bytes = make_bz(1, 0x10_0000, 0x020D);
        bytes[OFF_HEADER..OFF_HEADER + 4].copy_from_slice(b"XdrS");
        assert!(matches!(
            parse(&bytes),
            Err(BzImageError::BadHeaderMagic(_))
        ));
    }

    #[test]
    fn extracts_standard_fields() {
        let bytes = make_bz(4, 0x0010_0000, 0x020D);
        let bz = parse(&bytes).expect("parse");
        assert_eq!(bz.setup_sects, 4);
        assert_eq!(bz.version, 0x020D);
        assert_eq!(bz.code32_start, 0x0010_0000);
        // payload starts after (setup_sects + 1) sectors = 5 * 512 = 2560
        assert_eq!(bz.payload_offset, 2560);
    }

    #[test]
    fn setup_sects_zero_normalizes_to_four() {
        // image only needs to be 1024 bytes for our parser to read
        // the header; payload_offset calc still uses normalized 4.
        let bytes = make_bz(0, 0x10_0000, 0x020D);
        let bz = parse(&bytes).expect("parse");
        assert_eq!(bz.setup_sects, 4);
        assert_eq!(bz.payload_offset, 2560);
    }

    #[test]
    fn reads_v2_10_modern_fields() {
        // A 2.10-shape header with the v2.05+ fields modern
        // bootloaders consult: relocatable=1, cmdline_size=2048,
        // compressed payload at file offset 0x800 for 0x10_0000
        // bytes, init_size=0x0080_0000 (kernel needs 8 MiB while
        // decompressing).
        let mut bytes = make_bz(1, 0x0010_0000, 0x020A);
        bytes[OFF_RELOCATABLE_KERNEL] = 1;
        bytes[OFF_CMDLINE_SIZE..OFF_CMDLINE_SIZE + 4].copy_from_slice(&2048u32.to_le_bytes());
        bytes[OFF_PAYLOAD_OFFSET..OFF_PAYLOAD_OFFSET + 4]
            .copy_from_slice(&0x0000_0800u32.to_le_bytes());
        bytes[OFF_PAYLOAD_LENGTH..OFF_PAYLOAD_LENGTH + 4]
            .copy_from_slice(&0x0010_0000u32.to_le_bytes());
        bytes[OFF_INIT_SIZE..OFF_INIT_SIZE + 4].copy_from_slice(&0x0080_0000u32.to_le_bytes());
        let bz = parse(&bytes).expect("parse");
        assert_eq!(bz.relocatable_kernel, 1);
        assert_eq!(bz.cmdline_size, 2048);
        assert_eq!(bz.compressed_offset, 0x0000_0800);
        assert_eq!(bz.compressed_length, 0x0010_0000);
        assert_eq!(bz.init_size, 0x0080_0000);
    }

    #[test]
    fn pre_v2_05_image_reads_modern_fields_as_zero() {
        // An old image with these fields unset (zero) must still
        // parse — older Linux versions reserved this region as
        // zero, so reading it back as zero is the correct legacy
        // semantics (relocatable=false, no payload-offset shortcut,
        // unbounded cmdline).
        let bz = parse(&make_bz(1, 0x0010_0000, 0x0204)).expect("parse");
        assert_eq!(bz.relocatable_kernel, 0);
        assert_eq!(bz.cmdline_size, 0);
        assert_eq!(bz.compressed_offset, 0);
        assert_eq!(bz.init_size, 0);
    }

    #[test]
    fn reads_ramdisk_fields() {
        let mut bytes = make_bz(1, 0x10_0000, 0x020D);
        bytes[OFF_RAMDISK_IMAGE..OFF_RAMDISK_IMAGE + 4]
            .copy_from_slice(&0x0500_0000u32.to_le_bytes());
        bytes[OFF_RAMDISK_SIZE..OFF_RAMDISK_SIZE + 4]
            .copy_from_slice(&0x0010_0000u32.to_le_bytes());
        let bz = parse(&bytes).expect("parse");
        assert_eq!(bz.ramdisk_image, 0x0500_0000);
        assert_eq!(bz.ramdisk_size, 0x0010_0000);
    }
}
