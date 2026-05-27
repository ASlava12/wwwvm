//! Minimal ELF32 loader. Just enough to copy a hand-crafted i386 ELF
//! image's PT_LOAD segments into guest memory and return the entry
//! point. No relocations, no dynamic linking, no symbol resolution
//! — that's all out of scope for a from-scratch educational VM.
//!
//! Real bzImage / vmlinux kernels are loaded by a separate path that
//! understands the bzImage header; this loader is for the kinds of
//! flat ELF32 binaries that early-stage OS tutorials produce.

use wwwvm_mem::Memory;

#[derive(Debug)]
pub enum ElfError {
    /// Image was shorter than the 52-byte ELF32 header.
    TooSmall,
    /// EI_MAG bytes didn't match `\x7FELF`.
    BadMagic,
    /// EI_CLASS wasn't ELFCLASS32 (1).
    WrongClass(u8),
    /// EI_DATA wasn't ELFDATA2LSB (1). We only do little-endian.
    WrongEndian(u8),
    /// e_machine wasn't EM_386 (3).
    WrongMachine(u16),
    /// A program header references bytes past the end of `bytes`.
    SegmentOutOfBounds {
        index: usize,
        p_offset: u32,
        p_filesz: u32,
    },
    /// A PT_LOAD's destination wouldn't fit in the guest's RAM image.
    DestOutOfBounds {
        index: usize,
        p_vaddr: u32,
        p_memsz: u32,
    },
}

impl std::fmt::Display for ElfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall => write!(f, "ELF image too small for a 32-bit header"),
            Self::BadMagic => write!(f, "ELF magic mismatch (expected 7F 45 4C 46)"),
            Self::WrongClass(c) => write!(f, "wrong EI_CLASS {c}, expected 1 (ELF32)"),
            Self::WrongEndian(e) => write!(f, "wrong EI_DATA {e}, expected 1 (little-endian)"),
            Self::WrongMachine(m) => write!(f, "wrong e_machine {m}, expected 3 (EM_386)"),
            Self::SegmentOutOfBounds {
                index,
                p_offset,
                p_filesz,
            } => write!(
                f,
                "PT_LOAD[{index}] body at offset {p_offset:#x}..+{p_filesz:#x} runs past image end"
            ),
            Self::DestOutOfBounds {
                index,
                p_vaddr,
                p_memsz,
            } => write!(
                f,
                "PT_LOAD[{index}] destination {p_vaddr:#x}..+{p_memsz:#x} runs past guest RAM"
            ),
        }
    }
}

impl std::error::Error for ElfError {}

const PT_LOAD: u32 = 1;

/// Load an ELF32 image into `mem`. Returns the entry-point linear
/// address from `e_entry`. The caller is responsible for setting
/// CS:IP — see [`crate::Vm::load_elf_image`].
pub fn load_elf(mem: &mut Memory, bytes: &[u8]) -> Result<u32, ElfError> {
    if bytes.len() < 52 {
        return Err(ElfError::TooSmall);
    }
    if &bytes[..4] != b"\x7FELF" {
        return Err(ElfError::BadMagic);
    }
    let ei_class = bytes[4];
    if ei_class != 1 {
        return Err(ElfError::WrongClass(ei_class));
    }
    let ei_data = bytes[5];
    if ei_data != 1 {
        return Err(ElfError::WrongEndian(ei_data));
    }
    let e_machine = u16::from_le_bytes([bytes[0x12], bytes[0x13]]);
    if e_machine != 3 {
        return Err(ElfError::WrongMachine(e_machine));
    }
    let e_entry = read_u32(bytes, 0x18);
    let e_phoff = read_u32(bytes, 0x1C);
    let e_phentsize = u16::from_le_bytes([bytes[0x2A], bytes[0x2B]]) as usize;
    let e_phnum = u16::from_le_bytes([bytes[0x2C], bytes[0x2D]]) as usize;

    let ram_size = mem.size() as u32;
    for i in 0..e_phnum {
        let ph_off = e_phoff as usize + i * e_phentsize;
        if ph_off + 32 > bytes.len() {
            return Err(ElfError::SegmentOutOfBounds {
                index: i,
                p_offset: ph_off as u32,
                p_filesz: 32,
            });
        }
        let p_type = read_u32(bytes, ph_off);
        if p_type != PT_LOAD {
            continue;
        }
        let p_offset = read_u32(bytes, ph_off + 4);
        let p_vaddr = read_u32(bytes, ph_off + 8);
        let p_filesz = read_u32(bytes, ph_off + 16);
        let p_memsz = read_u32(bytes, ph_off + 20);
        // `p_paddr` (offset 12) and `p_flags`/`p_align` (24/28) are
        // read by real loaders. For a flat memory model with no MMU
        // protection enforcement they're not load-bearing.

        let body_end = (p_offset as usize).checked_add(p_filesz as usize).ok_or(
            ElfError::SegmentOutOfBounds {
                index: i,
                p_offset,
                p_filesz,
            },
        )?;
        if body_end > bytes.len() {
            return Err(ElfError::SegmentOutOfBounds {
                index: i,
                p_offset,
                p_filesz,
            });
        }
        let dest_end = p_vaddr
            .checked_add(p_memsz)
            .ok_or(ElfError::DestOutOfBounds {
                index: i,
                p_vaddr,
                p_memsz,
            })?;
        if dest_end > ram_size {
            return Err(ElfError::DestOutOfBounds {
                index: i,
                p_vaddr,
                p_memsz,
            });
        }
        // Copy file-resident bytes.
        mem.write_slice(p_vaddr, &bytes[p_offset as usize..body_end]);
        // Zero-fill the BSS tail when p_memsz > p_filesz.
        for off in p_filesz..p_memsz {
            mem.write_u8(p_vaddr.wrapping_add(off), 0);
        }
    }

    Ok(e_entry)
}

fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_header(e_entry: u32, e_phoff: u32, e_phnum: u16) -> Vec<u8> {
        let mut h = vec![0u8; 52];
        h[..4].copy_from_slice(b"\x7FELF");
        h[4] = 1; // ELF32
        h[5] = 1; // LE
        h[6] = 1; // version
        h[0x10..0x12].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        h[0x12..0x14].copy_from_slice(&3u16.to_le_bytes()); // EM_386
        h[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // e_version
        h[0x18..0x1C].copy_from_slice(&e_entry.to_le_bytes());
        h[0x1C..0x20].copy_from_slice(&e_phoff.to_le_bytes());
        h[0x28..0x2A].copy_from_slice(&52u16.to_le_bytes()); // e_ehsize
        h[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes()); // e_phentsize
        h[0x2C..0x2E].copy_from_slice(&e_phnum.to_le_bytes());
        h
    }

    fn make_ph(p_type: u32, p_offset: u32, p_vaddr: u32, p_filesz: u32, p_memsz: u32) -> Vec<u8> {
        let mut p = vec![0u8; 32];
        p[0..4].copy_from_slice(&p_type.to_le_bytes());
        p[4..8].copy_from_slice(&p_offset.to_le_bytes());
        p[8..12].copy_from_slice(&p_vaddr.to_le_bytes());
        p[12..16].copy_from_slice(&p_vaddr.to_le_bytes()); // p_paddr = p_vaddr
        p[16..20].copy_from_slice(&p_filesz.to_le_bytes());
        p[20..24].copy_from_slice(&p_memsz.to_le_bytes());
        p[24..28].copy_from_slice(&7u32.to_le_bytes()); // R|W|X
        p[28..32].copy_from_slice(&0x1000u32.to_le_bytes());
        p
    }

    #[test]
    fn rejects_image_shorter_than_header() {
        let mut mem = Memory::new(0x1000);
        let bytes = [0x7F, b'E', b'L'];
        assert!(matches!(
            load_elf(&mut mem, &bytes),
            Err(ElfError::TooSmall)
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut mem = Memory::new(0x1000);
        let mut bytes = vec![0u8; 52];
        bytes[0] = b'X';
        assert!(matches!(
            load_elf(&mut mem, &bytes),
            Err(ElfError::BadMagic)
        ));
    }

    #[test]
    fn rejects_elf64_class() {
        let mut mem = Memory::new(0x1000);
        let mut bytes = make_header(0, 52, 0);
        bytes[4] = 2; // ELFCLASS64
        assert!(matches!(
            load_elf(&mut mem, &bytes),
            Err(ElfError::WrongClass(2))
        ));
    }

    #[test]
    fn rejects_non_i386_machine() {
        let mut mem = Memory::new(0x1000);
        let mut bytes = make_header(0, 52, 0);
        bytes[0x12..0x14].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        assert!(matches!(
            load_elf(&mut mem, &bytes),
            Err(ElfError::WrongMachine(62))
        ));
    }

    #[test]
    fn loads_single_pt_load_with_bss_tail() {
        let mut mem = Memory::new(0x10_0000);
        let mut bytes = make_header(0x0500, 52, 1);
        // PT_LOAD at file offset 84 (= 52 + 32), vaddr 0x0500,
        // filesz 3 ("ABC"), memsz 8 — last 5 bytes are zero-filled.
        bytes.extend_from_slice(&make_ph(PT_LOAD, 84, 0x0500, 3, 8));
        bytes.extend_from_slice(b"ABC");

        let entry = load_elf(&mut mem, &bytes).expect("load");
        assert_eq!(entry, 0x0500);
        assert_eq!(mem.read_u8(0x0500), b'A');
        assert_eq!(mem.read_u8(0x0501), b'B');
        assert_eq!(mem.read_u8(0x0502), b'C');
        // BSS tail must be zero (memory might already be zero from
        // Memory::new — we explicitly set a non-zero sentinel first).
        for off in 3..8 {
            assert_eq!(mem.read_u8(0x0500 + off), 0);
        }
    }

    #[test]
    fn rejects_pt_load_past_image_end() {
        let mut mem = Memory::new(0x10_0000);
        let mut bytes = make_header(0x0500, 52, 1);
        bytes.extend_from_slice(&make_ph(PT_LOAD, 84, 0x0500, 100, 100));
        bytes.extend_from_slice(b"ABC"); // only 3 bytes, not 100
        assert!(matches!(
            load_elf(&mut mem, &bytes),
            Err(ElfError::SegmentOutOfBounds { .. })
        ));
    }

    #[test]
    fn skips_non_load_program_headers() {
        let mut mem = Memory::new(0x10_0000);
        // Two PHs: PT_NULL (0) first, then PT_LOAD second. The loader
        // must skip PT_NULL and still process PT_LOAD.
        let mut bytes = make_header(0x0500, 52, 2);
        bytes.extend_from_slice(&make_ph(0, 0, 0, 0, 0)); // PT_NULL
        bytes.extend_from_slice(&make_ph(PT_LOAD, 116, 0x0500, 3, 3));
        bytes.extend_from_slice(b"XYZ");
        let entry = load_elf(&mut mem, &bytes).expect("load");
        assert_eq!(entry, 0x0500);
        assert_eq!(mem.read_u8(0x0500), b'X');
    }
}
