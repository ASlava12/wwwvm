//! VM orchestrator: owns CPU + memory + IO bus, drives the fetch loop,
//! and exposes a small high-level API used by the wasm bindings.
//!
//! The crate also ships a tiny hand-assembled real-mode guest payload
//! (`HELLO_GUEST`) — it prints a banner over the UART and echoes any
//! input back. That payload is the proof-of-pipeline used by the demo
//! while the CPU/devices grow towards running real OS images.

#![forbid(unsafe_code)]

pub mod bzimage;
pub mod elf;

pub use bzimage::{parse as parse_bzimage, BzImage, BzImageError};
pub use elf::{load_elf, ElfError};
use wwwvm_cpu::{flag, Cpu, CpuError};
use wwwvm_devices::IoBus;
use wwwvm_mem::Memory;

/// Standard boot-sector load address on x86.
pub const BOOT_LOAD_ADDR: u32 = 0x7C00;

/// CGA/VGA text-mode buffer base (linear). 80 columns × 25 rows of
/// 2-byte cells (character + attribute) lives here. Guests write
/// directly via MOV instructions; the host reads it back with
/// [`Vm::vga_text_snapshot`].
pub const VGA_TEXT_BASE: u32 = 0xB8000;
pub const VGA_TEXT_COLS: usize = 80;
pub const VGA_TEXT_ROWS: usize = 25;

/// `screen_info.orig_video_isVGA` values from the Linux boot protocol
/// (`<uapi/linux/screen_info.h>`). A linear framebuffer is advertised
/// with one of these; the matching driver (`efifb` / `vesafb`) then
/// binds off the `lfb_*` fields without any real firmware.
pub const VIDEO_TYPE_VLFB: u8 = 0x23; // VESA VGA in graphic mode → vesafb
pub const VIDEO_TYPE_EFI: u8 = 0x70; // EFI graphic mode → efifb

/// BIOS Data Area cursor location for active page 0:
/// linear `0x450` = column, `0x451` = row. Real BIOS keeps all 8
/// pages in `0x450..0x460` (2 bytes each); we just touch page 0
/// because the host-side BIOS shim never changes the active page.
pub const BDA_CURSOR_COL: u32 = 0x0450;
pub const BDA_CURSOR_ROW: u32 = 0x0451;

/// Host-side BIOS shim. Installed via [`Vm::install_bios`]. When the
/// guest issues `INT n`, the CPU calls this with the vector; we
/// handle the small subset of BIOS calls the bundled guests use and
/// return `true` to short-circuit the normal IVT dispatch. Anything
/// we don't claim returns `false` and falls through to whatever the
/// guest installed at the IVT entry.
///
/// Currently implemented:
///   * INT 0x10 AH=0x00 — set video mode. Clears the 80×25 text
///     buffer and resets the cursor to (0, 0). The mode number is
///     accepted but ignored (we only model mode 3).
///   * INT 0x10 AH=0x01 — set cursor shape (CH/CL scan lines). We
///     don't model cursor shape, so this is a silent-accept.
///   * INT 0x10 AH=0x02 — set cursor position (BH page, DH row, DL col)
///   * INT 0x10 AH=0x03 — read cursor position (returns row/col in
///     DH/DL plus a canned cursor shape in CH/CL)
///   * INT 0x10 AH=0x06 — scroll window up. CH/CL = upper-left,
///     DH/DL = lower-right, AL = lines (0 = clear), BH = fill attr.
///   * INT 0x10 AH=0x07 — scroll window down (mirror of 0x06).
///   * INT 0x10 AH=0x08 — read char + attribute at the current
///     cursor (BH = page; AL = char, AH = attribute on return).
///   * INT 0x10 AH=0x09 — write char + attribute at the current
///     cursor, CX times. Does NOT advance the cursor (matches BIOS).
///   * INT 0x10 AH=0x0E — TTY teletype output. Writes AL to the VGA
///     text buffer at the BDA cursor (page 0), advances the cursor,
///     wraps at column 80 and clamps at row 24. CR/LF/BS are honored;
///     BEL is silently dropped.
///   * INT 0x10 AH=0x0F — get video mode (returns mode 3 / 80 cols)
///   * INT 0x10 AH=0x13 — write string at ES:BP. AL bits select
///     cursor-advance and attribute-interleaved modes (the four
///     modes 0..3 from the original BIOS spec). Used by setup.bin
///     banners and kernel panic messages.
///   * INT 0x16 AH=0x02 — get keyboard shift-flags. We don't model
///     modifier-key state, so always report 0 (no modifiers held).
///   * INT 0x1A AH=0x00 — get system tick counter (CX:DX = BDA
///     0x046C/0x046E; AL = midnight-rollover flag, always 0).
///   * INT 0x1A AH=0x01 — set system tick counter from CX:DX.
///   * INT 0x1A AH=0x02/0x04 — read RTC time/date from CMOS; the
///     returned values are BCD-encoded (BIOS convention), regardless
///     of the CMOS binary/BCD mode.
pub fn bios_hook(cpu: &mut Cpu, mem: &mut Memory, io: &mut IoBus, vector: u8) -> bool {
    let handled = match vector {
        0x10 => bios_int10(cpu, mem),
        0x12 => bios_int12(cpu),
        0x13 => bios_int13(cpu, mem, io),
        0x15 => bios_int15(cpu, mem),
        0x16 => bios_int16(cpu, io),
        0x1A => bios_int1a(cpu, mem, io),
        _ => false,
    };
    if !handled && matches!(vector, 0x10 | 0x13 | 0x15) {
        // For the vectors a real BIOS always serves, surface an
        // "unsupported sub-function" reply (CF=1, AH=0x86) instead
        // of falling through to the IVT — where an uninitialized
        // entry would land the CPU at linear 0 and crash. Linux's
        // setup code checks CF and moves on. The 0x16 (keyboard) /
        // 0x1A (clock) / 0x12 (low-mem) sub-functions are simple
        // enough that we handle every realistic call.
        cpu.write_r8(4, 0x86);
        cpu.flags |= wwwvm_cpu::flag::CF;
        return true;
    }
    handled
}

/// INT 0x12 — Get Conventional Memory Size. Returns AX = number of
/// contiguous KiB of memory below 1 MiB. We always return 640 (the
/// historical "640 K below DOS" answer) regardless of actual VM
/// size, because real PCs always reserved 0xA0000-0xFFFFF for
/// VGA/ROM regardless of installed RAM.
fn bios_int12(cpu: &mut Cpu) -> bool {
    cpu.regs[wwwvm_cpu::r16::AX] = 640;
    cpu.flags &= !wwwvm_cpu::flag::CF;
    true
}

/// Fill a rectangular region of the VGA text buffer with `ch` +
/// `attr`. Coordinates are inclusive on both ends.
fn fill_rect(
    mem: &mut Memory,
    top: usize,
    left: usize,
    bot: usize,
    right: usize,
    ch: u8,
    attr: u8,
) {
    for r in top..=bot {
        for c in left..=right {
            let off = ((r * VGA_TEXT_COLS) + c) * 2;
            mem.write_u8(VGA_TEXT_BASE + off as u32, ch);
            mem.write_u8(VGA_TEXT_BASE + off as u32 + 1, attr);
        }
    }
}

/// Copy a single-row span `[left..=right]` from `src_row` to
/// `dst_row` in the VGA buffer. Caller arranges read/write ordering
/// when src and dst overlap (scroll handlers iterate forwards or
/// reverse depending on direction).
fn copy_row_range(mem: &mut Memory, src_row: usize, dst_row: usize, left: usize, right: usize) {
    for c in left..=right {
        let src_off = ((src_row * VGA_TEXT_COLS) + c) * 2;
        let dst_off = ((dst_row * VGA_TEXT_COLS) + c) * 2;
        let ch = mem.read_u8(VGA_TEXT_BASE + src_off as u32);
        let at = mem.read_u8(VGA_TEXT_BASE + src_off as u32 + 1);
        mem.write_u8(VGA_TEXT_BASE + dst_off as u32, ch);
        mem.write_u8(VGA_TEXT_BASE + dst_off as u32 + 1, at);
    }
}

fn bios_int10(cpu: &mut Cpu, mem: &mut Memory) -> bool {
    let ah = cpu.read_r8(4); // AH lives in the high half of AX
    match ah {
        // Set video mode. AL = mode number — we only model mode 3
        // (80×25 16-colour text) so the value is accepted but
        // ignored; the contract observers actually care about is
        // that the screen clears and the cursor resets to (0, 0).
        0x00 => {
            for i in 0..(VGA_TEXT_ROWS * VGA_TEXT_COLS) as u32 {
                let off = i * 2;
                mem.write_u8(VGA_TEXT_BASE + off, b' ');
                mem.write_u8(VGA_TEXT_BASE + off + 1, 0x07);
            }
            mem.write_u8(BDA_CURSOR_COL, 0);
            mem.write_u8(BDA_CURSOR_ROW, 0);
            true
        }
        // Set cursor position. BH = page (ignored — page 0 only),
        // DH = row, DL = column. Clamps each to the 80×25 grid.
        // Set cursor shape (CH = start scan line, CL = end). We
        // don't model the cursor shape, so accept silently — every
        // CGA/EGA/VGA-aware caller writes through this on startup.
        0x01 => true,
        0x02 => {
            let row = cpu.read_r8(6).min(VGA_TEXT_ROWS as u8 - 1);
            let col = cpu.read_r8(2).min(VGA_TEXT_COLS as u8 - 1);
            mem.write_u8(BDA_CURSOR_ROW, row);
            mem.write_u8(BDA_CURSOR_COL, col);
            true
        }
        // Read cursor position. Returns DH=row, DL=col, and a
        // plausible cursor shape (start/end scan line) in CH/CL —
        // we don't model the cursor shape so just report the
        // standard underline-style block (lines 14..15).
        0x03 => {
            let row = mem.read_u8(BDA_CURSOR_ROW);
            let col = mem.read_u8(BDA_CURSOR_COL);
            cpu.write_r8(6, row); // DH
            cpu.write_r8(2, col); // DL
            cpu.write_r8(5, 0x0E); // CH (start scan line)
            cpu.write_r8(1, 0x0F); // CL (end scan line)
            true
        }
        // Scroll a window up (AH=0x06) or down (AH=0x07). CH/CL =
        // upper-left row/col, DH/DL = lower-right row/col, AL =
        // number of lines to scroll, BH = attribute for the new
        // blank lines. AL=0 — or AL >= window height — clears the
        // whole window. Used for clearing panels and the bottom
        // status row in kernel banners.
        0x06 | 0x07 => {
            let scroll_up = ah == 0x06;
            let lines = cpu.read_r8(0) as usize; // AL
            let attr = cpu.read_r8(7); // BH
            let top = cpu.read_r8(5) as usize; // CH
            let left = cpu.read_r8(1) as usize; // CL
            let bot = cpu.read_r8(6) as usize; // DH
            let right = cpu.read_r8(2) as usize; // DL
                                                 // Reject obviously-bogus coordinates; real BIOS silently
                                                 // ignores out-of-range rectangles too.
            if top >= VGA_TEXT_ROWS
                || bot >= VGA_TEXT_ROWS
                || left >= VGA_TEXT_COLS
                || right >= VGA_TEXT_COLS
                || top > bot
                || left > right
            {
                return true;
            }
            let window_rows = bot - top + 1;
            if lines == 0 || lines >= window_rows {
                // Whole-window clear.
                fill_rect(mem, top, left, bot, right, b' ', attr);
            } else if scroll_up {
                // Copy rows [top+lines..=bot] up by `lines`, then
                // clear the bottom `lines` rows.
                for r in top..=(bot - lines) {
                    copy_row_range(mem, r + lines, r, left, right);
                }
                fill_rect(mem, bot + 1 - lines, left, bot, right, b' ', attr);
            } else {
                // Scroll down: walk top-down in reverse so we don't
                // clobber source rows before reading them.
                for r in ((top + lines)..=bot).rev() {
                    copy_row_range(mem, r - lines, r, left, right);
                }
                fill_rect(mem, top, left, top + lines - 1, right, b' ', attr);
            }
            true
        }
        // Write char + attribute at current cursor, CX times. AL =
        // char, BL = attribute, CX = repeat count. The cursor is
        // NOT advanced — that's the real BIOS contract, distinct
        // Read char + attribute at the current cursor (BH = page,
        // ignored — page 0 only). Returns AL = char, AH = attr.
        0x08 => {
            let col = mem.read_u8(BDA_CURSOR_COL) as usize;
            let row = mem.read_u8(BDA_CURSOR_ROW) as usize;
            let off = ((row * VGA_TEXT_COLS) + col) * 2;
            let ch = mem.read_u8(VGA_TEXT_BASE + off as u32);
            let attr = mem.read_u8(VGA_TEXT_BASE + off as u32 + 1);
            cpu.write_r8(0, ch); // AL
            cpu.write_r8(4, attr); // AH
            true
        }
        // from AH=0x0E's teletype behavior.
        0x09 => {
            let ch = cpu.read_r8(0); // AL
            let attr = cpu.read_r8(3); // BL
            let count = cpu.read_r16(1) as usize; // CX
            let col = mem.read_u8(BDA_CURSOR_COL) as usize;
            let row = mem.read_u8(BDA_CURSOR_ROW) as usize;
            for i in 0..count {
                if col + i >= VGA_TEXT_COLS {
                    break;
                }
                let off = ((row * VGA_TEXT_COLS) + col + i) * 2;
                mem.write_u8(VGA_TEXT_BASE + off as u32, ch);
                mem.write_u8(VGA_TEXT_BASE + off as u32 + 1, attr);
            }
            true
        }
        0x0E => {
            let al = cpu.read_r8(0);
            let mut col = mem.read_u8(BDA_CURSOR_COL) as usize;
            let mut row = mem.read_u8(BDA_CURSOR_ROW) as usize;
            match al {
                0x07 => { /* BEL — silently drop */ }
                0x08 => col = col.saturating_sub(1),
                b'\r' => col = 0,
                b'\n' => {
                    if row + 1 < VGA_TEXT_ROWS {
                        row += 1;
                    }
                }
                _ => {
                    let off = ((row * VGA_TEXT_COLS) + col) * 2;
                    mem.write_u8(VGA_TEXT_BASE + off as u32, al);
                    // Attribute byte: 0x07 = light-grey on black.
                    mem.write_u8(VGA_TEXT_BASE + off as u32 + 1, 0x07);
                    col += 1;
                    if col >= VGA_TEXT_COLS {
                        col = 0;
                        if row + 1 < VGA_TEXT_ROWS {
                            row += 1;
                        }
                    }
                }
            }
            mem.write_u8(BDA_CURSOR_COL, col as u8);
            mem.write_u8(BDA_CURSOR_ROW, row as u8);
            true
        }
        // Get video mode. AL = active mode, AH = columns, BH = page.
        // We don't model multiple modes — always report 80×25
        // text (mode 3) with the active page = 0.
        0x0F => {
            cpu.write_r8(0, 3); // AL = mode 3
            cpu.write_r8(4, VGA_TEXT_COLS as u8); // AH = columns
            cpu.write_r8(7, 0); // BH = page
            true
        }
        // Write string. AL = mode (bit 0 = update cursor, bit 1 =
        // string includes attribute bytes after each char). CX =
        // length in chars, BH = page, BL = attribute (used for
        // mode 0/1), DH/DL = starting row/col, ES:BP = string.
        // Setup.bin's banner uses this; kernel panic output too.
        0x13 => {
            let mode = cpu.read_r8(0); // AL
            let count = cpu.read_r16(1) as usize; // CX
            let attr_default = cpu.read_r8(3); // BL
            let row = cpu.read_r8(6) as usize; // DH
            let col_start = cpu.read_r8(2) as usize; // DL
            let bp = cpu.read_r16(5); // BP
            let src_base = cpu.linear_seg(wwwvm_cpu::sreg::ES, bp as u32);
            let has_attr_in_str = mode & 0x02 != 0;
            let advance_cursor = mode & 0x01 != 0;
            let mut col = col_start;
            let mut cur_row = row;
            // Stride between chars in the source: 2 if interleaved
            // attrs are present, 1 otherwise.
            let stride = if has_attr_in_str { 2u32 } else { 1u32 };
            for i in 0..count {
                if cur_row >= VGA_TEXT_ROWS {
                    break;
                }
                let off_src = src_base.wrapping_add((i as u32) * stride);
                let ch = mem.read_u8(off_src);
                let attr = if has_attr_in_str {
                    mem.read_u8(off_src.wrapping_add(1))
                } else {
                    attr_default
                };
                // CR/LF aren't part of the write-string contract,
                // but BIOSes typically still honor them for the
                // teletype-style modes (1, 3). Keep it simple: just
                // place the char at (cur_row, col), then wrap.
                let off = ((cur_row * VGA_TEXT_COLS) + col) * 2;
                mem.write_u8(VGA_TEXT_BASE + off as u32, ch);
                mem.write_u8(VGA_TEXT_BASE + off as u32 + 1, attr);
                col += 1;
                if col >= VGA_TEXT_COLS {
                    col = 0;
                    cur_row += 1;
                }
            }
            if advance_cursor {
                mem.write_u8(BDA_CURSOR_ROW, cur_row.min(VGA_TEXT_ROWS - 1) as u8);
                mem.write_u8(BDA_CURSOR_COL, col.min(VGA_TEXT_COLS - 1) as u8);
            }
            true
        }
        // AH=0x12 — video subsystem configuration. Linux's
        // arch/x86/boot/video-vga.c calls this with BL=0x10 to
        // discover the EGA/VGA adapter; if BL comes back unchanged
        // the function is treated as unsupported. We model the
        // BL=0x10 sub-function and return "VGA color, 256K of video
        // memory" — which is what's effectively true here (the VGA
        // text buffer at 0xB8000 covers more than enough). Other BL
        // values return CF=1 (unsupported).
        0x12 => {
            let bl = cpu.read_r8(3); // BL
            if bl == 0x10 {
                cpu.write_r8(6, 0); // BH = 0 (color)
                cpu.write_r8(3, 3); // BL = 3 (256K video memory)
                cpu.write_r8(5, 0); // CH = 0 (feature bits)
                cpu.write_r8(1, 0); // CL = 0 (switches)
                cpu.flags &= !wwwvm_cpu::flag::CF;
                true
            } else {
                cpu.flags |= wwwvm_cpu::flag::CF;
                true
            }
        }
        _ => false,
    }
}

/// INT 0x13 — disk services. We model:
///
///   * AH=0x00 — Reset disk system. We have no state to reset;
///     return success silently.
///   * AH=0x01 — Get status of last operation. We never report
///     errors, so AH=0 / AL=0 / CF=0 always.
///   * AH=0x02 — Read sectors. Inputs: AL = sector count, CH = cyl
///     bits 0..7, CL bits 6..7 = cyl bits 8..9, CL bits 0..5 = sector
///     (1-based!), DH = head, DL = drive (0x80 = boot drive). The
///     destination buffer is ES:BX.
///   * AH=0x03 — Write sectors. Mirror of AH=0x02 with the same
///     CHS layout; source buffer is ES:BX. The disk image grows on
///     demand (via `Disk::write_sectors`) so a guest can format a
///     fresh image.
///   * AH=0x08 — Get drive parameters. Returns the canonical 1.44 MB
///     floppy geometry (80 cyl × 2 heads × 18 sec/track) in CH/CL/
///     DH, drive count 1 in DL, and BL = 0x04 (1.44 MB floppy type).
///     Called by bzImage `setup.bin` while probing the boot drive.
///   * AH=0x41 — Check LBA extensions. We report "not supported"
///     (CF=1, AH=0x01) so callers fall back to CHS/AH=0x02 reads.
///
/// The geometry is the canonical 1.44 MB floppy: 80 cylinders, 2
/// heads, 18 sectors/track. We use it for hard disks too, which is
/// wrong in general but fine for boot stubs that read by LBA via
/// `MOV CH,0; MOV CL,2` (cyl 0, sector 2) etc.
///
/// On return: CF=0 + AH=0 on success; CF=1 + AH=error on failure.
/// The flags update goes through `set_flag(flag::CF, …)` so it
/// survives any later state inspection.
fn bios_int13(cpu: &mut Cpu, mem: &mut Memory, io: &mut IoBus) -> bool {
    let ah = cpu.read_r8(4);
    match ah {
        // Reset disk system. Nothing to reset in our model — the
        // controller has no transient state between commands. Boot
        // loaders call this once before their read loop; without
        // the handler the BIOS shim falls through to the host and
        // the loader stalls.
        0x00 => {
            cpu.write_r8(4, 0); // AH = 0 (success)
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        // Status of last operation. Since AH=0x02/0x03 never report
        // errors, the answer is permanently "no error".
        0x01 => {
            cpu.write_r8(4, 0); // AH = 0
            cpu.write_r8(0, 0); // AL = 0 (status byte)
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        0x02 => {
            let count = cpu.read_r8(0) as usize; // AL
            let ch = cpu.read_r8(5) as u32; // CH
            let cl = cpu.read_r8(1) as u32; // CL
            let dh = cpu.read_r8(6) as u32; // DH
            let _dl = cpu.read_r8(2); // DL (drive index — we have one disk)
            let cyl = ((cl & 0xC0) << 2) | ch;
            let sector = (cl & 0x3F).saturating_sub(1); // 1-based -> 0-based
            let head = dh;
            // CHS -> LBA: ((cyl * heads) + head) * sectors_per_track + sector.
            // Standard 1.44 MB floppy geometry.
            let lba = (cyl * 2 + head) * 18 + sector;
            // Destination linear = ES.base + BX. read_r16(3) returns BX.
            let bx = cpu.read_r16(3);
            let dest_linear = cpu.linear_seg(wwwvm_cpu::sreg::ES, bx as u32);

            let mut buf = vec![0u8; count * wwwvm_devices::DISK_SECTOR_SIZE];
            io.disk().read_sectors(lba, count as u8, &mut buf);
            for (i, &b) in buf.iter().enumerate() {
                cpu.mem_write_u8(mem, dest_linear.wrapping_add(i as u32), b);
            }
            // Success: CF=0, AH=0. AL keeps the requested sector count.
            cpu.flags &= !wwwvm_cpu::flag::CF;
            cpu.write_r8(4, 0);
            true
        }
        // Write sectors — mirror of AH=0x02 with data flowing
        // toward the disk. Same CHS layout in CH/CL/DH/DL/AL,
        // same ES:BX source buffer.
        0x03 => {
            let count = cpu.read_r8(0) as usize;
            let ch = cpu.read_r8(5) as u32;
            let cl = cpu.read_r8(1) as u32;
            let dh = cpu.read_r8(6) as u32;
            let _dl = cpu.read_r8(2);
            let cyl = ((cl & 0xC0) << 2) | ch;
            let sector = (cl & 0x3F).saturating_sub(1);
            let head = dh;
            let lba = (cyl * 2 + head) * 18 + sector;
            let bx = cpu.read_r16(3);
            let src_linear = cpu.linear_seg(wwwvm_cpu::sreg::ES, bx as u32);
            let total = count * wwwvm_devices::DISK_SECTOR_SIZE;
            let mut buf = Vec::with_capacity(total);
            for i in 0..total {
                buf.push(cpu.mem_read_u8(mem, src_linear.wrapping_add(i as u32)));
            }
            io.disk_mut().write_sectors(lba, &buf);
            cpu.flags &= !wwwvm_cpu::flag::CF;
            cpu.write_r8(4, 0);
            true
        }
        // Get drive parameters. Reports the 1.44 MB floppy geometry
        // we already use for AH=0x02 — max-cyl 79, max-head 1, 18
        // sectors per track. Drive count = 1 (we only have one).
        0x08 => {
            cpu.write_r8(4, 0); // AH = 0 (success)
            cpu.write_r8(0, 0); // AL = 0 (reserved)
            cpu.write_r8(3, 0x04); // BL = drive type: 1.44 MB floppy
            cpu.write_r8(5, 79); // CH = max cylinder bits 0..7
                                 // CL bits 6..7 = cyl bits 8..9 (zero for cyl <= 255);
                                 // CL bits 0..5 = max sectors per track = 18.
            cpu.write_r8(1, 18);
            cpu.write_r8(6, 1); // DH = max head
            cpu.write_r8(2, 1); // DL = drive count
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        // Check LBA extensions. Real INT 13h extensions are advertised
        // here; we report "not supported" so callers fall back to the
        // CHS path (AH=0x02). Per the spec, failure means CF=1 and
        // AH = 0x01 (invalid function).
        0x41 => {
            cpu.write_r8(4, 0x01);
            cpu.flags |= wwwvm_cpu::flag::CF;
            true
        }
        _ => false,
    }
}

/// INT 0x15 — system services. We currently implement only one
/// sub-function:
///
///   * AX=0xE820 — System Memory Map. Loops over E820 entries; on
///     each call the kernel passes a 20-byte buffer at ES:DI, a
///     continuation index in EBX (0 to start), the buffer size in
///     ECX, and the signature "SMAP" in EDX. The BIOS writes one
///     entry and updates EBX. EBX=0 on return means "this was the
///     last entry".
///
/// Our model returns a single usable entry covering the VM's RAM
/// — sufficient for kernels that just want to know how much memory
/// they have, and consistent with how a flat physical address
/// space looks.
fn bios_int15(cpu: &mut Cpu, mem: &mut Memory) -> bool {
    let ax = cpu.regs[wwwvm_cpu::r16::AX];
    // AH=0x88 — legacy "Get Extended Memory Size" fallback. Returns
    // AX = number of contiguous KiB above 1 MiB, capped at 0xFFFF
    // (≈ 64 MiB - 1 KiB). Linux setup falls back to this when E820
    // isn't supported. Real BIOSes returned 0 if there's no
    // extended memory; we compute (RAM_SIZE - 1 MiB) / 1024.
    if ax >> 8 == 0x88 {
        let ram = mem.size() as u64;
        let extended = if ram > 0x10_0000 {
            ((ram - 0x10_0000) / 1024).min(0xFFFF)
        } else {
            0
        };
        cpu.regs[wwwvm_cpu::r16::AX] = extended as u16;
        cpu.flags &= !wwwvm_cpu::flag::CF;
        return true;
    }
    // AH=0x86 — wait CX:DX microseconds. Real silicon busy-waits;
    // since our model has no notion of wall-clock time between
    // step()s, the wait completes instantly. CF=0 = success.
    if ax >> 8 == 0x86 {
        cpu.flags &= !wwwvm_cpu::flag::CF;
        return true;
    }
    // AH=0xC0 — Get System Config (returns ES:BX → a 16-byte
    // hardware descriptor table on real BIOSes). We don't supply
    // the table; reply "function not supported" per the legacy
    // contract (CF=1, AH=0x86) so setup.bin falls back to defaults.
    if ax >> 8 == 0xC0 {
        cpu.write_r8(4, 0x86);
        cpu.flags |= wwwvm_cpu::flag::CF;
        return true;
    }
    // AH=0x24 — A20 gate control. Linux's arch/x86/boot/a20.c
    // tries this first before falling back to KBC and port 0x92.
    //   AL=0x00 disable A20
    //   AL=0x01 enable A20
    //   AL=0x02 query current A20 state (returns AL = state)
    //   AL=0x03 query supported A20 methods
    //             (returns BX = bit0:KBC | bit1:fast-A20)
    // Real BIOSes return CF=1 / AH=0x86 if A20 isn't BIOS-controllable;
    // we always model it, so CF=0 on every sub-function.
    if ax >> 8 == 0x24 {
        let al = cpu.read_r8(0);
        match al {
            0x00 => {
                cpu.a20 = false;
                cpu.flags &= !wwwvm_cpu::flag::CF;
                cpu.write_r8(4, 0);
            }
            0x01 => {
                cpu.a20 = true;
                cpu.flags &= !wwwvm_cpu::flag::CF;
                cpu.write_r8(4, 0);
            }
            0x02 => {
                cpu.write_r8(0, cpu.a20 as u8);
                cpu.write_r8(4, 0);
                cpu.flags &= !wwwvm_cpu::flag::CF;
            }
            0x03 => {
                // We support both: KBC bit (1<<0) + fast A20 (1<<1).
                cpu.regs[wwwvm_cpu::r16::BX] = 0b11;
                cpu.write_r8(4, 0);
                cpu.flags &= !wwwvm_cpu::flag::CF;
            }
            _ => {
                cpu.write_r8(4, 0x86);
                cpu.flags |= wwwvm_cpu::flag::CF;
            }
        }
        return true;
    }
    // AX=0xE801 — Get Memory Size (older alternative to E820 some
    // loaders prefer). Returns memory 1MB..16MB in KiB (AX = CX,
    // capped at 0x3C00 = 15 MiB) and memory above 16MB in 64KB
    // blocks (BX = DX, capped at 0xFFFF).
    if ax == 0xE801 {
        let ram = mem.size() as u64;
        let lo_kb = if ram > 0x10_0000 {
            ((ram - 0x10_0000) / 1024).min(0x3C00)
        } else {
            0
        };
        let hi_blocks = if ram > 0x0100_0000 {
            ((ram - 0x0100_0000) / (64 * 1024)).min(0xFFFF)
        } else {
            0
        };
        cpu.regs[wwwvm_cpu::r16::AX] = lo_kb as u16;
        cpu.regs[wwwvm_cpu::r16::CX] = lo_kb as u16;
        cpu.regs[wwwvm_cpu::r16::BX] = hi_blocks as u16;
        cpu.regs[wwwvm_cpu::r16::DX] = hi_blocks as u16;
        cpu.flags &= !wwwvm_cpu::flag::CF;
        return true;
    }
    if ax != 0xE820 {
        return false;
    }
    // Validate "SMAP" signature in EDX. If the guest didn't set it
    // we still service the call (some loaders skip it), but it's
    // the canonical check Linux setup does.
    let ebx = cpu.read_r32(3); // EBX
    let ecx = cpu.read_r32(1); // ECX (buffer size)
    if ecx < 20 {
        // Buffer too small for a 20-byte entry. Set CF=1.
        cpu.flags |= wwwvm_cpu::flag::CF;
        return true;
    }
    if ebx != 0 {
        // We only model one entry. Any nonzero continuation index
        // means "we already returned the last one" — surface that
        // by setting CF=1 and EBX=0.
        cpu.flags |= wwwvm_cpu::flag::CF;
        cpu.write_r32(3, 0);
        return true;
    }

    // Linear destination = ES:DI.
    let di = cpu.regs[wwwvm_cpu::r16::DI];
    let dest = cpu.linear_seg(wwwvm_cpu::sreg::ES, di as u32);

    let base: u64 = 0;
    let length = mem.size() as u64;
    let entry_type: u32 = 1; // usable

    // base (u64), length (u64), type (u32) — little-endian.
    for i in 0..8 {
        cpu.mem_write_u8(mem, dest + i, (base >> (i * 8)) as u8);
    }
    for i in 0..8 {
        cpu.mem_write_u8(mem, dest + 8 + i, (length >> (i * 8)) as u8);
    }
    for i in 0..4 {
        cpu.mem_write_u8(mem, dest + 16 + i, (entry_type >> (i * 8)) as u8);
    }

    // Returns: EAX = "SMAP", ECX = 20, EBX = 0 (no more entries),
    // CF = 0.
    cpu.write_r32(0, 0x534D_4150); // "SMAP"
    cpu.write_r32(1, 20); // bytes returned
    cpu.write_r32(3, 0); // continuation = 0 → no more
    cpu.flags &= !wwwvm_cpu::flag::CF;
    true
}

/// INT 0x16 — keyboard services. We model:
///
///   * AH=0x00 — Read keystroke. Blocking on real BIOS; here we
///     simulate the block by rewinding IP by 2 (back over the
///     `CD 16` we just decoded) when the keyboard queue is empty,
///     so the same INT re-executes next step. Once a byte is
///     available, we pop it and return AH=AL=byte (we don't yet
///     translate scan codes to ASCII).
///   * AH=0x01 — Check for keystroke. Non-blocking: ZF=1 if queue
///     empty, ZF=0 + AH/AL set (without popping) if a key waits.
fn bios_int16(cpu: &mut Cpu, io: &mut IoBus) -> bool {
    let ah = cpu.read_r8(4);
    match ah {
        0x00 => {
            if io.kbd.rx_pending() == 0 {
                // No key yet — rewind past `CD 16` so the next step
                // retries. The IP is currently after the imm8.
                cpu.ip = cpu.ip.wrapping_sub(2);
            } else {
                let byte = io.kbd.pop_scancode().unwrap_or(0);
                cpu.write_r8(0, byte); // AL
                cpu.write_r8(4, byte); // AH (scan code; we don't differentiate)
            }
            true
        }
        0x01 => {
            if let Some(byte) = io.kbd.peek_scancode() {
                cpu.flags &= !wwwvm_cpu::flag::ZF;
                cpu.write_r8(0, byte);
                cpu.write_r8(4, byte);
            } else {
                cpu.flags |= wwwvm_cpu::flag::ZF;
            }
            true
        }
        // Get keyboard shift-flags. We don't track modifier-key
        // state (shift, ctrl, alt, capslock, etc.) so always report
        // "nothing held". setup.bin reads this to decide between
        // graphical and serial console; with AL=0 it falls back to
        // its default behaviour, which is what we want.
        0x02 => {
            cpu.write_r8(0, 0);
            true
        }
        _ => false,
    }
}

/// BDA offsets the tick-counter lives at — IBM BIOS convention.
const BDA_TICK_LOW: u32 = 0x046C; // u32 low word
const BDA_TICK_HIGH: u32 = 0x046E; // u32 high word

fn bios_int1a(cpu: &mut Cpu, mem: &mut Memory, io: &mut IoBus) -> bool {
    let ah = cpu.read_r8(4);
    match ah {
        // Get tick counter. CX = high word, DX = low word of the
        // 32-bit count at BDA 0x046C/0x046E. AL = midnight rollover
        // (we don't track it across reads, always 0).
        0x00 => {
            let lo = mem.read_u16(BDA_TICK_LOW);
            let hi = mem.read_u16(BDA_TICK_HIGH);
            cpu.regs[wwwvm_cpu::r16::CX] = hi;
            cpu.regs[wwwvm_cpu::r16::DX] = lo;
            cpu.write_r8(0, 0);
            true
        }
        // Set tick counter from CX:DX (CX = high, DX = low).
        0x01 => {
            let hi = cpu.regs[wwwvm_cpu::r16::CX];
            let lo = cpu.regs[wwwvm_cpu::r16::DX];
            mem.write_u16(BDA_TICK_LOW, lo);
            mem.write_u16(BDA_TICK_HIGH, hi);
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        // Read RTC time. CH=hours, CL=minutes, DH=seconds, DL=DST.
        // The CMOS stores BCD (the BIOS convention), so the register
        // bytes are returned as-is.
        0x02 => {
            let hours = io.cmos.storage_byte(wwwvm_devices::cmos_reg::HOURS);
            let mins = io.cmos.storage_byte(wwwvm_devices::cmos_reg::MINUTES);
            let secs = io.cmos.storage_byte(wwwvm_devices::cmos_reg::SECONDS);
            cpu.write_r8(5, hours); // CH (BCD)
            cpu.write_r8(1, mins); // CL (BCD)
            cpu.write_r8(6, secs); // DH (BCD)
            cpu.write_r8(2, 0); // DL = DST flag (none)
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        // Read RTC date. CH=century, CL=year, DH=month, DL=day —
        // all BCD (stored BCD, returned as-is). We don't store
        // century in CMOS, so hard-code 0x20 (the 21st century).
        0x04 => {
            let year = io.cmos.storage_byte(wwwvm_devices::cmos_reg::YEAR);
            let month = io.cmos.storage_byte(wwwvm_devices::cmos_reg::MONTH);
            let day = io.cmos.storage_byte(wwwvm_devices::cmos_reg::DAY_OF_MONTH);
            cpu.write_r8(5, 0x20); // CH = century
            cpu.write_r8(1, year); // CL = year (BCD)
            cpu.write_r8(6, month); // DH = month (BCD)
            cpu.write_r8(2, day); // DL = day (BCD)
            cpu.flags &= !wwwvm_cpu::flag::CF;
            true
        }
        _ => false,
    }
}

/// Convert a Unix timestamp (seconds since 1970-01-01 UTC) to a civil
/// UTC date/time `(year, month, day, hour, minute, second)`. Uses Howard
/// Hinnant's days-from-civil inverse, valid across the proleptic Gregorian
/// calendar — no leap-year special-casing bugs. Used to seed the RTC from
/// the host clock.
fn civil_from_unix_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    // days-from-civil inverse (era-based), epoch shifted to 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

/// Snapshot format constants and the error type used by `restore`.
/// Content-addressed **paged** snapshots, for storing many derived snapshots
/// (e.g. one per training task) cheaply against a shared base image.
///
/// A snapshot blob is `header + cpu + RAM + devices`; the RAM region dominates
/// the size (it's the full guest RAM, verbatim). This module splits a blob into
/// a small `meta` (everything but RAM) plus the RAM as PAGE-sized pages keyed by
/// blake3 hash. A snapshot derived from a base by running a recipe only dirties
/// a fraction of RAM, so it shares all unchanged pages with the base — storing
/// it costs just the pages whose hash the content store lacks. Restore is a
/// single pass (no diff-chain replay): take `meta`, splice each page back by
/// hash. Dedup is automatic across all snapshots sharing a base.
pub mod paged {
    use std::collections::HashSet;

    /// Page granularity for the RAM split (matches the guest page size).
    pub const PAGE: usize = 4096;

    /// A content-addressed page: its blake3 hash and the (≤ `PAGE`) bytes.
    pub type Page = ([u8; 32], Vec<u8>);

    /// A snapshot decomposed for content-addressed storage.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Paged {
        /// The snapshot blob with the RAM region removed (prefix ++ suffix).
        pub meta: Vec<u8>,
        /// Offset where the RAM region sat in the original blob (= where the
        /// suffix begins in `meta`); RAM is spliced back here on reassembly.
        pub ram_off: usize,
        /// Length of the RAM region in the original blob.
        pub ram_len: usize,
        /// blake3 hash of each RAM page, in order. The last page may be shorter
        /// than `PAGE` if `ram_len` isn't a multiple of it.
        pub page_hashes: Vec<[u8; 32]>,
    }

    /// Split a full snapshot `blob` (whose RAM occupies `[ram_off, ram_off +
    /// ram_len)`) into a [`Paged`] manifest plus the `(hash, bytes)` of every
    /// page so the caller can persist any the store lacks. Panics if the RAM
    /// region is out of bounds (a malformed blob/region pairing — a bug).
    pub fn split(blob: &[u8], ram_off: usize, ram_len: usize) -> (Paged, Vec<Page>) {
        assert!(ram_off + ram_len <= blob.len(), "RAM region out of bounds");
        let ram = &blob[ram_off..ram_off + ram_len];
        let n_pages = ram_len.div_ceil(PAGE);
        let mut page_hashes = Vec::with_capacity(n_pages);
        let mut pages = Vec::with_capacity(n_pages);
        for chunk in ram.chunks(PAGE) {
            let h = *blake3::hash(chunk).as_bytes();
            page_hashes.push(h);
            pages.push((h, chunk.to_vec()));
        }
        let mut meta = Vec::with_capacity(blob.len() - ram_len);
        meta.extend_from_slice(&blob[..ram_off]);
        meta.extend_from_slice(&blob[ram_off + ram_len..]);
        (
            Paged {
                meta,
                ram_off,
                ram_len,
                page_hashes,
            },
            pages,
        )
    }

    /// The distinct page hashes this snapshot needs that `have` (the hashes the
    /// store already holds — e.g. from the base image) lacks: exactly the pages
    /// to upload. Deduped, preserving first-seen order.
    pub fn missing(paged: &Paged, have: &HashSet<[u8; 32]>) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for h in &paged.page_hashes {
            if !have.contains(h) && seen.insert(*h) {
                out.push(*h);
            }
        }
        out
    }

    /// Reassemble the full snapshot blob from a [`Paged`] manifest and a page
    /// fetcher (hash → page bytes). Returns `None` if any page is missing from
    /// the store, or if a fetched page is the wrong size.
    pub fn reassemble(
        paged: &Paged,
        fetch: impl Fn(&[u8; 32]) -> Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        let mut blob = Vec::with_capacity(paged.meta.len() + paged.ram_len);
        blob.extend_from_slice(&paged.meta[..paged.ram_off]);
        let mut remaining = paged.ram_len;
        for h in &paged.page_hashes {
            let page = fetch(h)?;
            let take = remaining.min(PAGE);
            if page.len() < take {
                return None;
            }
            blob.extend_from_slice(&page[..take]);
            remaining -= take;
        }
        if remaining != 0 {
            return None; // manifest's pages didn't cover the whole RAM region
        }
        blob.extend_from_slice(&paged.meta[paged.ram_off..]);
        Some(blob)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        fn blob(prefix: &[u8], ram: &[u8], suffix: &[u8]) -> (Vec<u8>, usize, usize) {
            let mut b = Vec::new();
            b.extend_from_slice(prefix);
            let off = b.len();
            b.extend_from_slice(ram);
            b.extend_from_slice(suffix);
            (b, off, ram.len())
        }

        /// split → reassemble (via a hash→bytes store) reproduces the blob
        /// byte-for-byte, including a final partial page and the suffix.
        #[test]
        fn split_reassemble_round_trips() {
            let ram: Vec<u8> = (0..(PAGE as u32 * 3 + 100)).map(|i| i as u8).collect();
            let (b, off, len) = blob(b"PREFIX", &ram, b"SUFFIXBYTES");
            let (paged, pages) = split(&b, off, len);
            assert_eq!(paged.page_hashes.len(), 4, "3 full + 1 partial page");
            let store: HashMap<_, _> = pages.into_iter().collect();
            let out = reassemble(&paged, |h| store.get(h).cloned()).unwrap();
            assert_eq!(out, b);
        }

        /// A reassemble that can't find a page fails cleanly (None), rather than
        /// producing a corrupt blob.
        #[test]
        fn reassemble_missing_page_is_none() {
            let ram = vec![1u8; PAGE * 2];
            let (b, off, len) = blob(b"", &ram, b"");
            let (paged, _) = split(&b, off, len);
            assert!(reassemble(&paged, |_| None).is_none());
        }

        /// `missing` returns only the pages a base store lacks — the upload diff.
        #[test]
        fn missing_is_only_changed_pages() {
            let ram1 = vec![0xAA; PAGE * 4];
            let (b1, off, len) = blob(b"M", &ram1, b"");
            let (_p1, pages1) = split(&b1, off, len);
            let have: HashSet<_> = pages1.iter().map(|(h, _)| *h).collect();

            let mut ram2 = ram1.clone();
            ram2[PAGE + 5] = 0xBB; // dirty exactly one page
            let (b2, off2, len2) = blob(b"M", &ram2, b"");
            let (p2, _) = split(&b2, off2, len2);
            assert_eq!(missing(&p2, &have).len(), 1, "only the dirtied page is new");
            assert!(missing(&p2, &have)[0] == p2.page_hashes[1]);
        }

        /// Identical pages collapse to one hash (cross-page dedup), so an
        /// all-zero RAM region stores a single page.
        #[test]
        fn identical_pages_dedup() {
            let ram = vec![0u8; PAGE * 5];
            let (b, off, len) = blob(b"", &ram, b"");
            let (p, pages) = split(&b, off, len);
            assert_eq!(p.page_hashes.len(), 5);
            let uniq: HashSet<_> = pages.iter().map(|(h, _)| *h).collect();
            assert_eq!(uniq.len(), 1, "identical pages share one hash");
            assert_eq!(missing(&p, &HashSet::new()).len(), 1, "deduped upload");
        }

        /// End-to-end against a real VM snapshot: paging then reassembling the
        /// actual `Vm::snapshot()` blob reproduces it exactly.
        #[test]
        fn vm_snapshot_paged_round_trips() {
            let vm = crate::Vm::new();
            let blob = vm.snapshot();
            let (off, len) = vm.snapshot_ram_region();
            let (paged, pages) = split(&blob, off, len);
            let store: HashMap<_, _> = pages.into_iter().collect();
            let out = reassemble(&paged, |h| store.get(h).cloned()).unwrap();
            assert_eq!(out, blob, "paged round-trip equals the raw snapshot");
        }
    }
}

pub mod snapshot {
    /// 6-byte format magic. Suitable for identifying the file from a
    /// hex dump.
    pub const MAGIC: &[u8] = b"WWWVM\x00";
    /// Current snapshot format version.
    /// * v1 — CPU (16-bit regs) + RAM only.
    /// * v2 — adds device state (UART/PIC/PIT/KBD/CMOS) after RAM.
    /// * v3 — adds i386 fields to the CPU image: upper 16 bits of
    ///   each GPR (regs_high), CR0, GDTR, IDTR.
    /// * v4 — appends CR3 (4 bytes) past the v3 layout — needed once
    ///   paging is in use, since CR3 names the page directory.
    /// * v5 — appends CR2 (4 bytes) past v4. CR2 holds the linear
    ///   address of the last page fault; preserving it across save/
    ///   restore lets a #PF handler see the address it was about to
    ///   service.
    /// * v6 — extends the CPU image with the fields added by the
    ///   32-bit-PM work: high 16 of IP (so EIP fully survives), CR4,
    ///   TSC, LDTR, TR, A20, and stack_size_32.
    /// * v7 — appends FPU control/status words and the three
    ///   IA32_SYSENTER MSRs (CS/ESP/EIP).
    /// * v8 — appends the x87 register stack: 8 × f64 (`fpu_st`) +
    ///   the TOP index (`fpu_top`).
    /// * v9 — appends the SSE register file: 8 × u128 (`xmm`).
    /// * v10 — appends the LAPIC MMIO scratch buffer (4 KiB)
    ///   between the RAM section and the device blob. Lets a
    ///   guest's kernel writes to SIV/TPR/etc. survive snapshot.
    /// * v11 — appends the HPET MMIO scratch buffer (1 KiB) right
    ///   after the LAPIC section.
    /// * v12 — extends the CPU image with `code_size_32` (1 byte),
    ///   `misc_enable` (u64), `tsc_aux` (u32), and the eight DR
    ///   registers (`dr[8]`, 32 bytes). Total 45 new bytes. With
    ///   this every Cpu field that survives reset is covered by
    ///   the snapshot; pre-v12 snapshots leave the new fields at
    ///   `Cpu::new()` defaults.
    /// * v13 — appends 12 bytes of HPET per-timer period state
    ///   (`Memory::hpet_period_bytes`) right after the v11 HPET MMIO
    ///   buffer. Needed so a periodic-mode HPET timer set up before
    ///   a snapshot still auto-advances on restore; pre-v13
    ///   snapshots resume with periods=0 (one-shot until the kernel
    ///   re-arms the comparator).
    /// * v14 — extends the inner PIT record with channel-2 state
    ///   (reload + counter + flags + write_state + pending_lsb,
    ///   plus 4 reserved bytes) so the kernel's TSC-via-PIT
    ///   calibration setup at port 0x42 + the port-0x61 gate/
    ///   speaker bits round-trip. Pre-v14 snapshots leave ch2
    ///   idle on restore; the kernel re-arms it next calibration.
    pub const VERSION: u8 = 14;
    /// Bytes the v10 LAPIC section adds past the RAM region. Sized
    /// to match [`wwwvm_mem::LAPIC_SIZE`] but kept as a const here
    /// so the snapshot module is self-contained.
    pub const LAPIC_LEN: usize = 4096;
    /// Bytes the v11 HPET section adds past the LAPIC region.
    pub const HPET_LEN: usize = 1024;
    /// Bytes the v13 HPET-period extension adds past the v11 HPET
    /// buffer: 3 timers × u32 LE = 12 bytes.
    pub const HPET_PERIOD_LEN: usize = 12;
    /// Bytes consumed by header: magic + version + flags + reserved.
    pub const HEADER_LEN: usize = 16;
    /// Bytes consumed by the v1/v2 CPU image — 8 r16 + 6 sreg + ip +
    /// flags + halted + seg_override + 2 reserved.
    pub const CPU_LEN: usize = 36;
    /// Extra bytes the v3 CPU image carries past the v1/v2 layout:
    /// 16 (regs_high u16 × 8) + 4 (cr0 u32) + 6 (gdtr) + 6 (idtr) = 32.
    pub const CPU_V3_EXTRA: usize = 32;
    /// Total bytes a v3 CPU image takes.
    pub const CPU_V3_LEN: usize = CPU_LEN + CPU_V3_EXTRA;
    /// Extra bytes v4 adds past the v3 image: 4 (cr3 u32).
    pub const CPU_V4_EXTRA: usize = 4;
    /// Total bytes a v4 CPU image takes.
    pub const CPU_V4_LEN: usize = CPU_V3_LEN + CPU_V4_EXTRA;
    /// Extra bytes v5 adds past v4: 4 (cr2 u32).
    pub const CPU_V5_EXTRA: usize = 4;
    /// Total bytes a v5 CPU image takes.
    pub const CPU_V5_LEN: usize = CPU_V4_LEN + CPU_V5_EXTRA;
    /// Extra bytes v6 adds: high-16 of IP (2) + CR4 (4) + TSC (8) +
    /// LDTR (2) + TR (2) + A20 (1) + stack_size_32 (1) = 20 bytes.
    pub const CPU_V6_EXTRA: usize = 20;
    /// Total bytes a v6 CPU image takes.
    pub const CPU_V6_LEN: usize = CPU_V5_LEN + CPU_V6_EXTRA;
    /// Extra bytes v7 adds: fpu_sw (2) + fpu_cw (2) + sysenter_cs (4)
    /// + sysenter_esp (4) + sysenter_eip (4) = 16 bytes.
    pub const CPU_V7_EXTRA: usize = 16;
    /// Total bytes a v7 CPU image takes.
    pub const CPU_V7_LEN: usize = CPU_V6_LEN + CPU_V7_EXTRA;
    /// Extra bytes v8 adds: 8 × f64 fpu_st (64) + fpu_top (1) = 65.
    pub const CPU_V8_EXTRA: usize = 65;
    /// Total bytes a v8 CPU image takes.
    pub const CPU_V8_LEN: usize = CPU_V7_LEN + CPU_V8_EXTRA;
    /// Extra bytes v9 adds: 8 × u128 xmm (128) = 128.
    pub const CPU_V9_EXTRA: usize = 128;
    /// Total bytes a v9 CPU image takes.
    pub const CPU_V9_LEN: usize = CPU_V8_LEN + CPU_V9_EXTRA;
    /// Extra bytes v12 adds: code_size_32 (1) + misc_enable (8) +
    /// tsc_aux (4) + dr[8] (32) = 45 bytes. v10 and v11 left the
    /// CPU image alone — they appended MMIO scratch buffers past
    /// RAM — so v12's CPU extension comes right after v9's XMM.
    pub const CPU_V12_EXTRA: usize = 45;
    /// Total bytes a v12 CPU image takes.
    pub const CPU_V12_LEN: usize = CPU_V9_LEN + CPU_V12_EXTRA;

    #[derive(Debug)]
    pub enum SnapshotError {
        TooSmall { got: usize, need: usize },
        BadMagic,
        UnsupportedVersion(u8),
        MemorySizeMismatch { expected: usize, actual: usize },
        DeviceRestore(String),
    }

    impl std::fmt::Display for SnapshotError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::TooSmall { got, need } => {
                    write!(
                        f,
                        "snapshot too small: got {got} bytes, need at least {need}"
                    )
                }
                Self::BadMagic => write!(f, "snapshot magic mismatch"),
                Self::UnsupportedVersion(v) => {
                    write!(f, "unsupported snapshot version {v}")
                }
                Self::MemorySizeMismatch { expected, actual } => {
                    write!(f, "memory size mismatch: expected {expected}, got {actual}")
                }
                Self::DeviceRestore(msg) => write!(f, "device restore failed: {msg}"),
            }
        }
    }

    impl std::error::Error for SnapshotError {}
}

/// Why the run loop stopped this turn.
#[derive(Debug)]
pub enum Stop {
    /// Hit a HLT and the CPU is parked. Further `run` calls are no-ops.
    Halted,
    /// Reached the cycle budget for this turn — call again to keep going.
    StepBudget,
    /// CPU could not decode the next instruction. Detail in the message.
    CpuError(CpuError),
}

/// A linear framebuffer advertised to the guest kernel via the Linux
/// boot-protocol `screen_info` (part of `boot_params`). The "device"
/// is just a reserved region of guest RAM: the kernel's framebuffer
/// driver (efifb / vesafb) maps `base` and draws RGB pixels there, and
/// the host reads those bytes back out ([`Vm::framebuffer_bytes`]) to
/// blit onto a canvas. No real VESA BIOS or EFI firmware is involved —
/// `screen_info` alone is enough for `efifb`/`vesafb` to bind.
#[derive(Debug, Clone, Copy)]
pub struct FramebufferConfig {
    /// Physical base address of the framebuffer in guest RAM (also the
    /// `screen_info.lfb_base` we hand the kernel). E820-reserved.
    pub base: u32,
    /// Reserved byte span at `base` (`>= stride * height`, page-rounded).
    pub size: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline (`width * 4` for our 32bpp layout).
    pub stride: u32,
    /// Bits per pixel (always 32 here: little-endian B,G,R,X bytes).
    pub bpp: u8,
    /// `screen_info.orig_video_isVGA` — [`VIDEO_TYPE_EFI`] (efifb) or
    /// [`VIDEO_TYPE_VLFB`] (vesafb). Alpine's `vmlinuz-lts` only has
    /// efifb built in, so EFI is the default.
    pub video_type: u8,
}

pub struct Vm {
    cpu: Cpu,
    mem: Memory,
    io: IoBus,
    autorun: Vec<u8>,
    booted: bool,
    /// `Some` once [`Vm::enable_linear_framebuffer`] is called; drives
    /// the `screen_info` + e820 reservation in [`start_protected_mode_at`].
    fb: Option<FramebufferConfig>,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// Default 1 MiB of conventional + low memory. Real-mode guests
    /// and the bundled demos fit here. For loading ELF kernels with
    /// entries above 0x100000, construct via [`Vm::with_ram_size`].
    pub const RAM_SIZE: usize = 0x10_0000;

    pub fn new() -> Self {
        Self::with_ram_size(Self::RAM_SIZE)
    }

    /// Construct a VM with a non-default RAM size. Use this when
    /// loading kernel images whose entry point lives above 1 MiB
    /// (typical Linux/BSD/Hobby-OS ELF kernels are linked at
    /// 0x00100000 or higher). Snapshot/restore still works as long
    /// as both ends use the *same* RAM size.
    pub fn with_ram_size(size: usize) -> Self {
        Self {
            cpu: Cpu::new(),
            mem: Memory::new(size),
            io: IoBus::new(),
            autorun: Vec::new(),
            booted: false,
            fb: None,
        }
    }

    /// Copy bytes into physical RAM at `addr`. The same primitive
    /// powers `load_image` (a guest program), seeding data tables,
    /// and writing IVT entries.
    pub fn load_image(&mut self, addr: u32, bytes: &[u8]) {
        self.mem.write_slice(addr, bytes);
    }

    /// Enable the CPU's debug instruction-trace ring (see
    /// `Cpu::enable_pf_trace`). Diagnostics use this to dump the last
    /// `cap` instructions when a wild jump / null-deref #PF fires.
    pub fn enable_cpu_pf_trace(&self, cap: usize) {
        self.cpu.enable_pf_trace(cap);
    }

    /// Dump the CPU instruction-trace ring on demand (no-op if not
    /// enabled). Call at a known failure point (e.g. after the UART shows
    /// a kernel panic) to see the last instructions leading up to it.
    pub fn dump_cpu_pf_trace(&self, header: &str) {
        self.cpu.dump_pf_trace(header);
    }

    /// Write an IVT entry for the given vector. Vector `v` lives at
    /// linear address `v*4` as a 4-byte (offset, segment) record. JS
    /// callers use this to wire up handlers without emitting a string
    /// of `MOV WORD` instructions in the guest.
    pub fn set_ivt(&mut self, vector: u8, segment: u16, offset: u16) {
        let base = (vector as u32) * 4;
        self.mem.write_u16(base, offset);
        self.mem.write_u16(base + 2, segment);
    }

    /// Read a single byte from guest RAM. Useful for assertions in
    /// integration tests and for JS-side inspection of guest state.
    pub fn read_mem_u8(&self, addr: u32) -> u8 {
        self.mem.read_u8(addr)
    }

    /// Read a 16-bit little-endian word from guest RAM.
    pub fn read_mem_u16(&self, addr: u32) -> u16 {
        self.mem.read_u16(addr)
    }

    /// Read a 32-bit little-endian dword from guest RAM. Lets JS
    /// peek a sentinel (e.g. a `MOV [addr], 0xDEADBEEF` outcome)
    /// in one call instead of four byte reads. Used by the
    /// head_32-shaped boot-pipeline demos and by integration tests
    /// that need to assert on a full register width.
    pub fn read_mem_u32(&self, addr: u32) -> u32 {
        self.mem.read_u32(addr)
    }

    /// Capture the VM's state as a compact byte buffer for later
    /// `restore()`. Format v1: 16-byte header (magic + version +
    /// reserved) + 36-byte CPU image + 1 MB memory image. Total
    /// ≈ 1 MiB + 52 bytes.
    ///
    /// **Scope of v1**: CPU and RAM only. Device state (UART buffers,
    /// PIC IMR/IRR/ISR, PIT counter, keyboard queue, CMOS storage)
    /// is *not* preserved — restored snapshots come back with fresh
    /// devices. A snapshot taken mid-handler can therefore land in a
    /// surprising place after restore. Use snapshots when the guest
    /// is at a clean rest point (boot, JMP -2 idle, HLT).
    /// The `[offset, length)` of the RAM region inside a [`Vm::snapshot`] blob.
    /// RAM is written immediately after the fixed-size header + v12 CPU image,
    /// so it starts at `HEADER_LEN + CPU_V12_LEN` and runs for the guest RAM
    /// size. Used to split a blob into content-addressed pages without parsing
    /// the whole format. (The browser slices the blob at this offset to page it.)
    pub fn snapshot_ram_region(&self) -> (usize, usize) {
        (
            snapshot::HEADER_LEN + snapshot::CPU_V12_LEN,
            self.mem.size(),
        )
    }

    /// Take a snapshot and decompose it into a content-addressed [`paged::Paged`]
    /// manifest + the `(hash, bytes)` of each RAM page — the form used to store a
    /// derived snapshot as just the pages a base lacks. See [`mod@paged`].
    pub fn snapshot_paged(&self) -> (paged::Paged, Vec<paged::Page>) {
        let blob = self.snapshot();
        let (off, len) = self.snapshot_ram_region();
        paged::split(&blob, off, len)
    }

    pub fn snapshot(&self) -> Vec<u8> {
        let total = snapshot::HEADER_LEN + snapshot::CPU_V12_LEN + self.mem.size();
        let mut buf = Vec::with_capacity(total);
        // Header
        buf.extend_from_slice(snapshot::MAGIC);
        buf.push(snapshot::VERSION);
        buf.push(0); // flags (reserved)
        buf.extend_from_slice(&[0u8; 8]); // reserved padding

        // CPU image v1/v2 prefix — preserved verbatim so v3 snapshots
        // remain readable by any tool that knows the v1 layout.
        for r in &self.cpu.regs {
            buf.extend_from_slice(&r.to_le_bytes());
        }
        for s in &self.cpu.sregs {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        // Snapshot still emits 16-bit IP for v1-v5 compatibility.
        // The Cpu field is now u32; high bits get truncated on save
        // (a future v6 layout widens this).
        buf.extend_from_slice(&(self.cpu.ip as u16).to_le_bytes());
        buf.extend_from_slice(&self.cpu.flags.to_le_bytes());
        buf.push(self.cpu.halted as u8);
        buf.push(match self.cpu.seg_override() {
            None => 0xFF,
            Some(i) => i as u8,
        });
        // 2 reserved bytes — the v1/v2 layout always ended with these.
        buf.extend_from_slice(&[0u8; 2]);

        // v3 CPU extension — upper-16 of each GPR + CR0 + GDTR + IDTR.
        for h in &self.cpu.regs_high {
            buf.extend_from_slice(&h.to_le_bytes());
        }
        buf.extend_from_slice(&self.cpu.cr0.to_le_bytes());
        buf.extend_from_slice(&self.cpu.gdtr.limit.to_le_bytes());
        buf.extend_from_slice(&self.cpu.gdtr.base.to_le_bytes());
        buf.extend_from_slice(&self.cpu.idtr.limit.to_le_bytes());
        buf.extend_from_slice(&self.cpu.idtr.base.to_le_bytes());

        // v4 CPU extension — CR3 (page directory physical base).
        buf.extend_from_slice(&self.cpu.cr3.to_le_bytes());

        // v5 CPU extension — CR2 (last page-fault linear address).
        buf.extend_from_slice(&self.cpu.cr2.to_le_bytes());

        // v6 CPU extension — pay off the tech-debt from the rest of
        // the 32-bit work: high-16 of IP, CR4, TSC, LDTR, TR, A20,
        // stack_size_32 (20 bytes total).
        buf.extend_from_slice(&((self.cpu.ip >> 16) as u16).to_le_bytes());
        buf.extend_from_slice(&self.cpu.cr4.to_le_bytes());
        buf.extend_from_slice(&self.cpu.tsc.to_le_bytes());
        buf.extend_from_slice(&self.cpu.ldtr.to_le_bytes());
        buf.extend_from_slice(&self.cpu.tr.to_le_bytes());
        buf.push(self.cpu.a20 as u8);
        buf.push(self.cpu.stack_size_32 as u8);

        // v7 CPU extension — FPU control/status + SYSENTER MSRs.
        buf.extend_from_slice(&self.cpu.fpu_sw.to_le_bytes());
        buf.extend_from_slice(&self.cpu.fpu_cw.to_le_bytes());
        buf.extend_from_slice(&self.cpu.sysenter_cs.to_le_bytes());
        buf.extend_from_slice(&self.cpu.sysenter_esp.to_le_bytes());
        buf.extend_from_slice(&self.cpu.sysenter_eip.to_le_bytes());

        // v8 CPU extension — the x87 register stack. Serialized as 8 × f64
        // (the historical wire format; snapshots were always f64-precision).
        // Live execution keeps full 80-bit F80 precision; only a
        // snapshot/restore round-trip demotes through f64, as it always did.
        for st in &self.cpu.fpu_st {
            buf.extend_from_slice(&st.to_f64().to_bits().to_le_bytes());
        }
        buf.push(self.cpu.fpu_top);

        // v9 CPU extension — the SSE register file (8 × XMM, 128-bit each).
        for x in &self.cpu.xmm {
            buf.extend_from_slice(&x.to_le_bytes());
        }

        // v12 CPU extension — fields added since v9 that survive
        // reset: code_size_32 (latched from CS.D), misc_enable
        // (MSR 0x1A0), tsc_aux (MSR 0xC0000103, the RDTSCP ECX
        // source), and the eight debug registers (DR0..7).
        buf.push(self.cpu.code_size_32 as u8);
        buf.extend_from_slice(&self.cpu.misc_enable.to_le_bytes());
        buf.extend_from_slice(&self.cpu.tsc_aux.to_le_bytes());
        for d in &self.cpu.dr {
            buf.extend_from_slice(&d.to_le_bytes());
        }

        // Memory
        buf.extend_from_slice(self.mem.as_slice());

        // v10 LAPIC scratch window (4 KiB). Lives between RAM and
        // the device blob; written verbatim with no length prefix
        // since the size is fixed by snapshot::LAPIC_LEN.
        buf.extend_from_slice(self.mem.lapic_bytes());

        // v11 HPET scratch window (1 KiB) follows the LAPIC.
        buf.extend_from_slice(self.mem.hpet_bytes());

        // v13 HPET per-timer period (12 bytes) — software state that
        // backs periodic-mode comparator auto-advance, kept outside
        // the MMIO buffer so the guest can't read or clobber it.
        buf.extend_from_slice(&self.mem.hpet_period_bytes());

        // Devices (v2-style length-prefixed records — see IoBus::snapshot).
        let dev = self.io.snapshot();
        let dev_len = dev.len() as u32;
        buf.extend_from_slice(&dev_len.to_le_bytes());
        buf.extend_from_slice(&dev);
        buf
    }

    /// Restore VM state from a buffer produced by `snapshot()`. On
    /// error the VM's state is unchanged (we validate first, mutate
    /// only on success). Devices are *not* restored — they keep
    /// whatever state they had before the call.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), snapshot::SnapshotError> {
        use snapshot::SnapshotError;
        let ram_size = self.mem.size();
        if bytes.len() < snapshot::HEADER_LEN + snapshot::CPU_LEN + ram_size {
            return Err(SnapshotError::TooSmall {
                got: bytes.len(),
                need: snapshot::HEADER_LEN + snapshot::CPU_LEN + ram_size,
            });
        }
        if &bytes[..snapshot::MAGIC.len()] != snapshot::MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = bytes[snapshot::MAGIC.len()];
        if !matches!(version, 1..=14) {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let cpu_len = match version {
            // v13 / v14 don't extend the CPU image — they extend
            // device-side blobs (HPET periods in v13, PIT ch2 in
            // v14).
            12..=14 => snapshot::CPU_V12_LEN,
            // v10/v11 don't extend the CPU image — they append MMIO
            // sections (LAPIC, then HPET) between RAM and the device
            // blob.
            9..=11 => snapshot::CPU_V9_LEN,
            8 => snapshot::CPU_V8_LEN,
            7 => snapshot::CPU_V7_LEN,
            6 => snapshot::CPU_V6_LEN,
            5 => snapshot::CPU_V5_LEN,
            4 => snapshot::CPU_V4_LEN,
            3 => snapshot::CPU_V3_LEN,
            _ => snapshot::CPU_LEN,
        };
        // Re-validate min size against the version-specific CPU image.
        if bytes.len() < snapshot::HEADER_LEN + cpu_len + ram_size {
            return Err(SnapshotError::TooSmall {
                got: bytes.len(),
                need: snapshot::HEADER_LEN + cpu_len + ram_size,
            });
        }
        let cpu_start = snapshot::HEADER_LEN;
        let mem_start = cpu_start + cpu_len;

        // Decode v1/v2 prefix (always present in any version).
        let mut regs = [0u16; 8];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = u16::from_le_bytes([bytes[cpu_start + i * 2], bytes[cpu_start + i * 2 + 1]]);
        }
        let sregs_off = cpu_start + 16;
        let mut sregs = [0u16; 6];
        for (i, s) in sregs.iter_mut().enumerate() {
            *s = u16::from_le_bytes([bytes[sregs_off + i * 2], bytes[sregs_off + i * 2 + 1]]);
        }
        let ip = u16::from_le_bytes([bytes[cpu_start + 28], bytes[cpu_start + 29]]);
        let flags = u16::from_le_bytes([bytes[cpu_start + 30], bytes[cpu_start + 31]]);
        let halted = bytes[cpu_start + 32] != 0;
        let seg_override = match bytes[cpu_start + 33] {
            0xFF => None,
            i if (i as usize) < 6 => Some(i as usize),
            _ => None,
        };

        // v3 extension (regs_high, cr0, gdtr, idtr). For v1/v2 these
        // come back at their defaults (zero / empty).
        let mut regs_high = [0u16; 8];
        let mut cr0: u32 = 0;
        let mut gdtr = wwwvm_cpu::DescriptorTable::default();
        let mut idtr = wwwvm_cpu::DescriptorTable::default();
        let mut cr3: u32 = 0;
        if version >= 3 {
            let ext = cpu_start + snapshot::CPU_LEN;
            for (i, h) in regs_high.iter_mut().enumerate() {
                *h = u16::from_le_bytes([bytes[ext + i * 2], bytes[ext + i * 2 + 1]]);
            }
            cr0 = u32::from_le_bytes([
                bytes[ext + 16],
                bytes[ext + 17],
                bytes[ext + 18],
                bytes[ext + 19],
            ]);
            gdtr.limit = u16::from_le_bytes([bytes[ext + 20], bytes[ext + 21]]);
            gdtr.base = u32::from_le_bytes([
                bytes[ext + 22],
                bytes[ext + 23],
                bytes[ext + 24],
                bytes[ext + 25],
            ]);
            idtr.limit = u16::from_le_bytes([bytes[ext + 26], bytes[ext + 27]]);
            idtr.base = u32::from_le_bytes([
                bytes[ext + 28],
                bytes[ext + 29],
                bytes[ext + 30],
                bytes[ext + 31],
            ]);
        }
        if version >= 4 {
            let ext = cpu_start + snapshot::CPU_V3_LEN;
            cr3 = u32::from_le_bytes([bytes[ext], bytes[ext + 1], bytes[ext + 2], bytes[ext + 3]]);
        }
        let mut cr2: u32 = 0;
        if version >= 5 {
            let ext = cpu_start + snapshot::CPU_V4_LEN;
            cr2 = u32::from_le_bytes([bytes[ext], bytes[ext + 1], bytes[ext + 2], bytes[ext + 3]]);
        }
        // v6 extras: high-16 of IP (so the full EIP survives) plus
        // the architectural state added since v5.
        let mut ip_high: u16 = 0;
        let mut cr4: u32 = 0;
        let mut tsc: u64 = 0;
        let mut ldtr: u16 = 0;
        let mut tr: u16 = 0;
        let mut a20: bool = true;
        let mut stack_size_32: bool = false;
        if version >= 6 {
            let ext = cpu_start + snapshot::CPU_V5_LEN;
            ip_high = u16::from_le_bytes([bytes[ext], bytes[ext + 1]]);
            cr4 = u32::from_le_bytes([
                bytes[ext + 2],
                bytes[ext + 3],
                bytes[ext + 4],
                bytes[ext + 5],
            ]);
            tsc = u64::from_le_bytes([
                bytes[ext + 6],
                bytes[ext + 7],
                bytes[ext + 8],
                bytes[ext + 9],
                bytes[ext + 10],
                bytes[ext + 11],
                bytes[ext + 12],
                bytes[ext + 13],
            ]);
            ldtr = u16::from_le_bytes([bytes[ext + 14], bytes[ext + 15]]);
            tr = u16::from_le_bytes([bytes[ext + 16], bytes[ext + 17]]);
            a20 = bytes[ext + 18] != 0;
            stack_size_32 = bytes[ext + 19] != 0;
        }
        let mut fpu_sw: u16 = 0;
        let mut fpu_cw: u16 = 0x037F;
        let mut sysenter_cs: u32 = 0;
        let mut sysenter_esp: u32 = 0;
        let mut sysenter_eip: u32 = 0;
        if version >= 7 {
            let ext = cpu_start + snapshot::CPU_V6_LEN;
            fpu_sw = u16::from_le_bytes([bytes[ext], bytes[ext + 1]]);
            fpu_cw = u16::from_le_bytes([bytes[ext + 2], bytes[ext + 3]]);
            sysenter_cs = u32::from_le_bytes([
                bytes[ext + 4],
                bytes[ext + 5],
                bytes[ext + 6],
                bytes[ext + 7],
            ]);
            sysenter_esp = u32::from_le_bytes([
                bytes[ext + 8],
                bytes[ext + 9],
                bytes[ext + 10],
                bytes[ext + 11],
            ]);
            sysenter_eip = u32::from_le_bytes([
                bytes[ext + 12],
                bytes[ext + 13],
                bytes[ext + 14],
                bytes[ext + 15],
            ]);
        }
        let mut fpu_st = [0.0f64; 8];
        let mut fpu_top: u8 = 0;
        if version >= 8 {
            let ext = cpu_start + snapshot::CPU_V7_LEN;
            for (i, slot) in fpu_st.iter_mut().enumerate() {
                let o = ext + i * 8;
                let bits = u64::from_le_bytes([
                    bytes[o],
                    bytes[o + 1],
                    bytes[o + 2],
                    bytes[o + 3],
                    bytes[o + 4],
                    bytes[o + 5],
                    bytes[o + 6],
                    bytes[o + 7],
                ]);
                *slot = f64::from_bits(bits);
            }
            fpu_top = bytes[ext + 64];
        }
        let mut xmm = [0u128; 8];
        if version >= 9 {
            let ext = cpu_start + snapshot::CPU_V8_LEN;
            for (i, slot) in xmm.iter_mut().enumerate() {
                let o = ext + i * 16;
                let mut b = [0u8; 16];
                b.copy_from_slice(&bytes[o..o + 16]);
                *slot = u128::from_le_bytes(b);
            }
        }
        // v12 extras: code_size_32 + misc_enable + tsc_aux + DR0..7.
        // Pre-v12 snapshots leave these at `Cpu::new()` defaults
        // (false / 0 / 0 / [0; 8]).
        let mut code_size_32 = false;
        let mut misc_enable: u64 = 0;
        let mut tsc_aux: u32 = 0;
        let mut dr = [0u32; 8];
        if version >= 12 {
            let ext = cpu_start + snapshot::CPU_V9_LEN;
            code_size_32 = bytes[ext] != 0;
            misc_enable = u64::from_le_bytes([
                bytes[ext + 1],
                bytes[ext + 2],
                bytes[ext + 3],
                bytes[ext + 4],
                bytes[ext + 5],
                bytes[ext + 6],
                bytes[ext + 7],
                bytes[ext + 8],
            ]);
            tsc_aux = u32::from_le_bytes([
                bytes[ext + 9],
                bytes[ext + 10],
                bytes[ext + 11],
                bytes[ext + 12],
            ]);
            for (i, slot) in dr.iter_mut().enumerate() {
                let o = ext + 13 + i * 4;
                *slot = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
            }
        }

        // Memory restore — `restore_full` validates size again as a
        // defense-in-depth check, but we already verified above.
        self.mem
            .restore_full(&bytes[mem_start..mem_start + ram_size])
            .map_err(|expected| SnapshotError::MemorySizeMismatch {
                expected,
                actual: bytes.len() - mem_start,
            })?;
        // v10 LAPIC scratch. v1..=9 snapshots predate this section,
        // so we leave the LAPIC at its construction defaults
        // (Version reg already populated by Memory::new). Newer
        // snapshots overwrite verbatim.
        let lapic_end = if version >= 10 {
            let lapic_off = mem_start + ram_size;
            if bytes.len() < lapic_off + snapshot::LAPIC_LEN {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: lapic_off + snapshot::LAPIC_LEN,
                });
            }
            self.mem
                .restore_lapic(&bytes[lapic_off..lapic_off + snapshot::LAPIC_LEN])
                .map_err(|expected| SnapshotError::MemorySizeMismatch {
                    expected,
                    actual: snapshot::LAPIC_LEN,
                })?;
            lapic_off + snapshot::LAPIC_LEN
        } else {
            mem_start + ram_size
        };
        // v11 HPET scratch sits right after the LAPIC. Pre-v11
        // snapshots leave the HPET at construction defaults (Caps
        // register pre-populated).
        let mmio_end = if version >= 11 {
            if bytes.len() < lapic_end + snapshot::HPET_LEN {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: lapic_end + snapshot::HPET_LEN,
                });
            }
            self.mem
                .restore_hpet(&bytes[lapic_end..lapic_end + snapshot::HPET_LEN])
                .map_err(|expected| SnapshotError::MemorySizeMismatch {
                    expected,
                    actual: snapshot::HPET_LEN,
                })?;
            let hpet_end = lapic_end + snapshot::HPET_LEN;
            // v13 HPET period extension. Pre-v13 snapshots leave the
            // periods at zero, which degrades a periodic-mode timer
            // to one-shot until the kernel re-writes the comparator.
            if version >= 13 {
                if bytes.len() < hpet_end + snapshot::HPET_PERIOD_LEN {
                    return Err(SnapshotError::TooSmall {
                        got: bytes.len(),
                        need: hpet_end + snapshot::HPET_PERIOD_LEN,
                    });
                }
                self.mem
                    .restore_hpet_period(&bytes[hpet_end..hpet_end + snapshot::HPET_PERIOD_LEN])
                    .map_err(|expected| SnapshotError::MemorySizeMismatch {
                        expected,
                        actual: snapshot::HPET_PERIOD_LEN,
                    })?;
                hpet_end + snapshot::HPET_PERIOD_LEN
            } else {
                hpet_end
            }
        } else {
            lapic_end
        };
        // Device section (v2 and v3). v1 snapshots have nothing here.
        if version >= 2 {
            let dev_off = mmio_end;
            if bytes.len() < dev_off + 4 {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: dev_off + 4,
                });
            }
            let dev_len = u32::from_le_bytes([
                bytes[dev_off],
                bytes[dev_off + 1],
                bytes[dev_off + 2],
                bytes[dev_off + 3],
            ]) as usize;
            if bytes.len() < dev_off + 4 + dev_len {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: dev_off + 4 + dev_len,
                });
            }
            self.io
                .restore(&bytes[dev_off + 4..dev_off + 4 + dev_len])
                .map_err(SnapshotError::DeviceRestore)?;
        }

        // Commit CPU state.
        self.cpu.regs = regs;
        self.cpu.regs_high = regs_high;
        self.cpu.sregs = sregs;
        self.cpu.ip = (ip as u32) | ((ip_high as u32) << 16);
        self.cpu.flags = flags;
        self.cpu.halted = halted;
        self.cpu.set_seg_override(seg_override);
        self.cpu.cr0 = cr0;
        self.cpu.gdtr = gdtr;
        self.cpu.idtr = idtr;
        self.cpu.cr3 = cr3;
        self.cpu.cr2 = cr2;
        self.cpu.cr4 = cr4;
        self.cpu.tsc = tsc;
        self.cpu.ldtr = ldtr;
        self.cpu.tr = tr;
        self.cpu.a20 = a20;
        self.cpu.stack_size_32 = stack_size_32;
        self.cpu.fpu_sw = fpu_sw;
        self.cpu.fpu_cw = fpu_cw;
        self.cpu.sysenter_cs = sysenter_cs;
        self.cpu.sysenter_esp = sysenter_esp;
        self.cpu.sysenter_eip = sysenter_eip;
        // Wire format is f64; promote each into the 80-bit F80 stack.
        self.cpu.fpu_st = fpu_st.map(wwwvm_cpu::f80::F80::from_f64);
        self.cpu.fpu_top = fpu_top;
        self.cpu.xmm = xmm;
        // v12 — fields added after v9. Pre-v12 restores leave these
        // at Cpu::new()'s defaults; the kernel re-derives most of
        // them on the next instruction anyway (code_size_32 from
        // CS.D on the next far-jump, MSRs on the next WRMSR).
        self.cpu.code_size_32 = code_size_32;
        self.cpu.misc_enable = misc_enable;
        self.cpu.tsc_aux = tsc_aux;
        self.cpu.dr = dr;
        // Re-derive seg_cache from the visible selectors. For real-
        // mode snapshots this is exact (cache = sel << 4). For a
        // future PM snapshot the cache values would diverge from
        // sel<<4 and need their own section in a v4 layout.
        for (slot, sel) in self.cpu.seg_cache.iter_mut().zip(sregs.iter()) {
            *slot = wwwvm_cpu::SegmentCache {
                base: (*sel as u32) << 4,
                limit: 0xFFFF,
                access: 0x93,
            };
        }
        self.booted = true;
        Ok(())
    }

    /// Snapshot the VGA text-mode buffer as a plain string: 25 rows
    /// of 80 chars, newline-separated. The attribute byte of each
    /// cell is dropped; control bytes (and NULs from un-initialized
    /// buffer) become spaces so the result is always readable. Use
    /// this to render the guest's text-mode display in the host UI
    /// or to assert on guest output in tests.
    pub fn vga_text_snapshot(&self) -> String {
        let mut out = String::with_capacity(VGA_TEXT_ROWS * (VGA_TEXT_COLS + 1));
        for row in 0..VGA_TEXT_ROWS {
            for col in 0..VGA_TEXT_COLS {
                let off = ((row * VGA_TEXT_COLS) + col) * 2;
                let ch = self.mem.read_u8(VGA_TEXT_BASE + off as u32);
                if (0x20..0x7F).contains(&ch) {
                    out.push(ch as char);
                } else {
                    out.push(' ');
                }
            }
            out.push('\n');
        }
        out
    }

    /// Load the bundled hello guest at the standard boot-sector address.
    pub fn load_default_guest(&mut self) {
        self.load_image(BOOT_LOAD_ADDR, HELLO_GUEST);
    }

    /// Load the bundled interactive demo: an interrupt-driven UART
    /// echo with a banner. Installs main + handler + greeting in
    /// memory and wires the IRQ-4 vector through the IVT — JS just
    /// has to call `boot()` afterwards.
    pub fn load_interactive_demo(&mut self) {
        use interactive_demo as d;
        self.load_image(d::MAIN_ADDR, d::MAIN);
        self.load_image(d::HANDLER_ADDR, d::HANDLER);
        self.load_image(d::GREETING_ADDR, d::GREETING);
        self.set_ivt(d::IRQ4_VECTOR, 0, d::HANDLER_ADDR as u16);
    }

    /// Load the bundled mini-calculator demo: poll a byte, square it
    /// via `MUL`, print the result as decimal followed by `\n`. Lives
    /// across two memory regions — main at 0x7C00, the print_dec
    /// subroutine at 0x7C30.
    pub fn load_calculator_demo(&mut self) {
        use calculator_demo as d;
        self.load_image(d::BASE_ADDR, d::MAIN);
        self.load_image(d::PRINT_DEC_ADDR, d::PRINT_DEC);
    }

    /// Load the bundled protected-mode kernel demo: a synthetic
    /// bzImage whose payload runs in 32-bit CS.D=1 mode, prints
    /// "Hello from PM!\n" via COM1 (port 0x3F8) and HLTs. Composes
    /// the full bzImage → PM handoff path for JS demos and
    /// integration tests that want a stock PM guest without
    /// hand-rolling the bzImage bytes.
    pub fn load_pm_demo(&mut self) -> Result<(), bzimage::BzImageError> {
        // Synthetic v2.10 bzImage: setup_sects=1, boot_flag=AA55,
        // HdrS magic, code32_start = 1 MiB. 1024-byte setup blob
        // followed by the 32-bit kernel payload.
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        // Kernel payload (CS.D=1 32-bit):
        //   BA F8 03 00 00      MOV EDX, 0x3F8       (COM1 THR)
        //   B0 <c> EE  …  per char of "Hello from PM!\n"
        //   F4                  HLT
        bz.extend_from_slice(&[0xBA, 0xF8, 0x03, 0x00, 0x00]);
        for ch in b"Hello from PM!\n" {
            bz.extend_from_slice(&[0xB0, *ch, 0xEE]);
        }
        bz.push(0xF4);

        // The default 1 MiB RAM can't fit code32_start (1 MiB) —
        // resize up to 2 MiB so the payload lands inside RAM. This
        // mirrors the JS-callable best practice for any real
        // bzImage (use new_with_ram_size for ≥2 MiB).
        if self.mem.size() < 0x0020_0000 {
            self.mem.resize(0x0020_0000);
        }

        let parsed = self.load_bzimage(&bz)?;
        self.start_protected_mode_at(parsed.code32_start);
        Ok(())
    }

    /// Queue commands to be delivered to the guest the moment it boots.
    /// Each command is terminated with `\n` and they are concatenated in
    /// order — `["ls", "cat /etc/os-release"]` becomes `"ls\ncat …\n"`.
    pub fn set_autorun_commands<I, S>(&mut self, commands: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.autorun.clear();
        for cmd in commands {
            self.autorun.extend_from_slice(cmd.as_ref().as_bytes());
            self.autorun.push(b'\n');
        }
    }

    /// Reset the CPU to the boot state and deliver autorun bytes (if any)
    /// to the UART input buffer so the guest reads them first.
    pub fn boot(&mut self) {
        self.cpu.reset_to_boot();
        self.io.uart_mut().push_rx(&self.autorun);
        self.autorun.clear();
        self.booted = true;
    }

    /// Skip the real-mode → PM transition dance and jump straight
    /// into 32-bit protected-mode execution at `entry`. The standard
    /// real-bootloader trampoline at 0x7C00 (LGDT + CR0|=1 + far
    /// JMP) becomes unnecessary because we mutate the CPU state
    /// directly: write a minimal flat-segments GDT at 0x500, set
    /// CR0.PE, load CS/SS/DS/ES/FS/GS from GDT, and point IP at
    /// `entry`.
    ///
    /// Pair with [`load_bzimage`]: pass `bz.code32_start` as the
    /// entry. The resulting flow is exactly what a real GRUB
    /// hand-off would have produced, minus the trampoline bytes.
    /// `autorun` bytes still arrive via the UART for guests that
    /// read input there.
    ///
    /// Per the Linux x86 boot protocol (§4.1), `%esi` must point at
    /// the real-mode kernel header — that's where the kernel reads
    /// boot_params from. The setup block lives at 0x90000 (where
    /// `load_bzimage` puts it), so that's what we load into ESI.
    /// The protocol additionally requires `%ebp = %edi = %ebx = 0`
    /// and interrupts disabled at entry; we honor both so a kernel
    /// that scribbles into these registers as scratch on entry
    /// doesn't pick up garbage left over from the bootloader.
    ///
    /// ESP is "don't care" per the protocol — Linux's `startup_32`
    /// sets its own stack via `lss BP_kernel_alignment(%esi), %esp`
    /// almost immediately — but a fresh-from-`new()` Cpu has ESP=0,
    /// and any fault that fires before the kernel sets its own
    /// stack would wrap the stack pointer through 0 into unmapped
    /// memory. We seed it to 0x7C00 (the historic bootloader
    /// scratch address, below the bzImage setup block at 0x90000
    /// and well above anything the kernel writes).
    /// Reserve a linear framebuffer in guest RAM and advertise it to
    /// the kernel via `screen_info`, so `efifb` (or `vesafb`) binds and
    /// fbcon renders the text console as real RGB pixels. Call before
    /// [`start_protected_mode_at`], which writes the `screen_info`
    /// fields and carves the region out of the e820 map as reserved
    /// (so neither the page allocator nor the fb driver's
    /// `request_mem_region` hands it out twice).
    ///
    /// The framebuffer is 32 bits-per-pixel, byte order B,G,R,X (the
    /// efifb default): pixel `u32 = red<<16 | green<<8 | blue`. It is
    /// placed at the top of RAM, page-aligned; give the VM enough RAM
    /// that the region sits well above the kernel + initramfs (256 MiB
    /// is plenty for any console resolution). `video_type` selects the
    /// driver — [`VIDEO_TYPE_EFI`] (efifb; the only built-in fb driver
    /// in Alpine's `vmlinuz-lts`) or [`VIDEO_TYPE_VLFB`] (vesafb).
    pub fn enable_linear_framebuffer(&mut self, width: u32, height: u32, video_type: u8) {
        let bpp = 32u8;
        let stride = width.saturating_mul(4);
        let raw = stride.saturating_mul(height);
        let size = (raw + 0xFFF) & !0xFFF; // page-round the reservation
        let ram = self.mem.size() as u32;
        let base = ram.saturating_sub(size) & !0xFFF; // top of RAM, page-aligned
        self.fb = Some(FramebufferConfig {
            base,
            size,
            width,
            height,
            stride,
            bpp,
            video_type,
        });
    }

    /// The linear framebuffer's geometry, once
    /// [`enable_linear_framebuffer`] has run (else `None`).
    pub fn framebuffer_config(&self) -> Option<FramebufferConfig> {
        self.fb
    }

    /// Copy the framebuffer's current pixels out of guest RAM —
    /// `stride * height` bytes in the 32bpp B,G,R,X layout — or `None`
    /// if no framebuffer is enabled. Callers (canvas blit) byte-swap
    /// B,G,R,X → R,G,B,A as needed.
    pub fn framebuffer_bytes(&self) -> Option<Vec<u8>> {
        let fb = self.fb?;
        let start = fb.base as usize;
        let len = (fb.stride as usize) * (fb.height as usize);
        let ram = self.mem.as_slice();
        let end = (start + len).min(ram.len());
        Some(ram[start..end].to_vec())
    }

    pub fn start_protected_mode_at(&mut self, entry: u32) {
        // Flat-segments GDT: null + ring-0 code + ring-0 data, all
        // base 0 / limit 4 GiB. Placed at 0x500 (between the BIOS
        // data area and the boot-sector load address, where it
        // doesn't collide with the bzImage setup blob at 0x90000).
        const GDT: [u8; 24] = [
            0, 0, 0, 0, 0, 0, 0, 0, // null
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, // code
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // data
        ];
        self.mem.write_slice(0x500, &GDT);
        self.cpu.cr0 |= 1;
        self.cpu.gdtr = wwwvm_cpu::DescriptorTable {
            base: 0x500,
            limit: 0x17,
        };
        // Load each segment register through write_sreg so the
        // descriptor cache reflects the flat-4-GiB layout.
        self.cpu.write_sreg(wwwvm_cpu::sreg::CS, 0x08, &self.mem);
        self.cpu.write_sreg(wwwvm_cpu::sreg::DS, 0x10, &self.mem);
        self.cpu.write_sreg(wwwvm_cpu::sreg::ES, 0x10, &self.mem);
        self.cpu.write_sreg(wwwvm_cpu::sreg::FS, 0x10, &self.mem);
        self.cpu.write_sreg(wwwvm_cpu::sreg::GS, 0x10, &self.mem);
        self.cpu.write_sreg(wwwvm_cpu::sreg::SS, 0x10, &self.mem);
        self.cpu.stack_size_32 = true;
        self.cpu.ip = entry;
        // ESI = setup block linear address (boot_params live here).
        // `load_bzimage` places the setup header at 0x90000 — the
        // historical SETUPSEG slot the boot protocol bakes in.
        self.cpu.write_r32(wwwvm_cpu::r16::SI as u8, 0x0009_0000);
        // Protocol §4.1: EBP / EDI / EBX must be zero at entry.
        self.cpu.write_r32(wwwvm_cpu::r16::BP as u8, 0);
        self.cpu.write_r32(wwwvm_cpu::r16::DI as u8, 0);
        self.cpu.write_r32(wwwvm_cpu::r16::BX as u8, 0);
        // ESP defaults — see doc on this fn for the rationale.
        self.cpu.write_r32(wwwvm_cpu::r16::SP as u8, 0x0000_7C00);
        // Protocol §4.1: interrupts must be disabled at entry.
        self.cpu.flags &= !wwwvm_cpu::flag::IF;
        let bp = 0x9_0000u32;
        const OFF_E820_ENTRIES: u32 = 0x1E8;
        const OFF_E820_TABLE: u32 = 0x2D0;

        // Build a clean zero page. `load_bzimage` copies the whole
        // setup blob (boot sector + setup) verbatim to 0x90000, so
        // boot_params[0..0x1F1] — the pre-header region holding
        // screen_info, the legacy hd*_info, and the sentinel — still
        // contains the bzImage's boot-sector stub bytes. A real
        // bootloader hands the kernel a *zeroed* page with only the
        // setup header (0x1F1+) populated. Match that: zero everything
        // before the setup header so screen_info starts clean (we fill
        // it below) and the sentinel reads 0 ("all fields initialized").
        self.mem.write_slice(bp, &[0u8; 0x1F1]);

        // Advertise a linear framebuffer in screen_info if one was
        // enabled (field offsets per struct screen_info,
        // <uapi/linux/screen_info.h>). The kernel's efifb/vesafb binds
        // off these and fbcon then renders the console as RGB pixels
        // into the reserved region carved out of e820 just below.
        if let Some(fb) = self.fb {
            self.mem.write_u8(bp + 0x0F, fb.video_type); // orig_video_isVGA
            self.mem.write_u16(bp + 0x12, fb.width as u16); // lfb_width
            self.mem.write_u16(bp + 0x14, fb.height as u16); // lfb_height
            self.mem.write_u16(bp + 0x16, fb.bpp as u16); // lfb_depth
            self.mem.write_u32(bp + 0x18, fb.base); // lfb_base
            self.mem.write_u32(bp + 0x1C, fb.size); // lfb_size (bytes)
            self.mem.write_u16(bp + 0x24, fb.stride as u16); // lfb_linelength
            self.mem.write_u8(bp + 0x26, 8); // red_size (32bpp B,G,R,X: pixel = R<<16|G<<8|B)
            self.mem.write_u8(bp + 0x27, 16); // red_pos
            self.mem.write_u8(bp + 0x28, 8); // green_size
            self.mem.write_u8(bp + 0x29, 8); // green_pos
            self.mem.write_u8(bp + 0x2A, 8); // blue_size
            self.mem.write_u8(bp + 0x2B, 0); // blue_pos
            self.mem.write_u8(bp + 0x2C, 8); // rsvd_size
            self.mem.write_u8(bp + 0x2D, 24); // rsvd_pos
        }

        // Populate boot_params.e820_table with a memory map that
        // covers our entire RAM. Without this Linux's early-init
        // memblock_alloc_node_data() sees zero usable RAM and panics
        // with "Failed to allocate %ld bytes for node %d memory map".
        // The BIOS INT 0x15 E820 shim only matters for real-mode
        // setup; a PM-direct entry never runs that.
        //
        // Layout: 0x0..0x9FC00 usable (DOS/conventional), 0x9FC00..
        // 0x100000 reserved (EBDA + video + BIOS ROM), 0x100000..
        // ram_size usable (extended memory holding the kernel +
        // everything past 1 MiB). When a framebuffer is enabled, the
        // reserved fb region splits that last usable span so neither
        // the page allocator nor the fb driver's request_mem_region
        // can hand it out twice. Each entry is 20 bytes packed; up to
        // 128 fit in boot_params.
        let ram_size = self.mem.size() as u64;
        let mut entries: Vec<(u64, u64, u32)> = vec![
            (0x0000_0000, 0x0009_FC00, 1), // usable conventional
            (0x0009_FC00, 0x0006_0400, 2), // reserved BIOS / video
        ];
        match self.fb {
            Some(fb) if (fb.base as u64) > 0x0010_0000 => {
                let fb_base = fb.base as u64;
                let fb_size = fb.size as u64;
                entries.push((0x0010_0000, fb_base - 0x0010_0000, 1)); // usable
                entries.push((fb_base, fb_size, 2)); // reserved framebuffer
                let tail = fb_base + fb_size;
                if tail < ram_size {
                    entries.push((tail, ram_size - tail, 1)); // usable tail
                }
            }
            _ => entries.push((0x0010_0000, ram_size.saturating_sub(0x10_0000), 1)),
        }
        self.mem
            .write_u8(bp + OFF_E820_ENTRIES, entries.len() as u8);
        for (i, (base, size, kind)) in entries.iter().enumerate() {
            let off = bp + OFF_E820_TABLE + (i as u32) * 20;
            self.mem.write_u32(off, *base as u32);
            self.mem.write_u32(off + 4, (*base >> 32) as u32);
            self.mem.write_u32(off + 8, *size as u32);
            self.mem.write_u32(off + 12, (*size >> 32) as u32);
            self.mem.write_u32(off + 16, *kind);
        }
        self.io.uart_mut().push_rx(&self.autorun);
        self.autorun.clear();
        self.booted = true;
    }

    /// Wire the host-side BIOS shim into the CPU. After this, `INT 0x10`
    /// (and future BIOS vectors) gets dispatched to the Rust functions
    /// in [`bios_hook`] instead of the IVT entry — so a freshly booted
    /// guest can print via teletype without first installing its own
    /// handler. A guest that *does* install its own IVT entry can
    /// override us by overwriting the `bios_hook` field with `None`.
    pub fn install_bios(&mut self) {
        self.cpu.bios_hook = Some(bios_hook);
    }

    /// Replace the boot disk image. The host-side `INT 0x13 AH=0x02`
    /// shim reads from here. `bytes.len()` need not be a sector
    /// multiple; bytes past the end read as zero.
    pub fn load_disk_image(&mut self, bytes: &[u8]) {
        self.io.disk_mut().load(bytes);
    }

    /// Replace the secondary-channel disk image (the one a guest
    /// reaches via ATA at 0x170..0x177 / 0x376). Useful for handing
    /// the kernel a second drive — a CD-ROM mock, a swap target,
    /// or a separate rootfs. Doesn't touch the BIOS boot drive.
    pub fn load_secondary_disk_image(&mut self, bytes: &[u8]) {
        self.io.ata2.disk.load(bytes);
    }

    /// Parse an ELF32 image and copy its PT_LOAD segments into guest
    /// memory. After this returns, the caller should call [`boot`]
    /// (which resets the CPU) and then position CS:IP at the entry
    /// point — typically by writing the IP register directly. The
    /// loader doesn't touch CS:IP itself because the caller might
    /// want to construct a different initial state (e.g. PM-up
    /// already, GDT pre-installed) before jumping to entry.
    pub fn load_elf_image(&mut self, bytes: &[u8]) -> Result<u32, ElfError> {
        elf::load_elf(&mut self.mem, bytes)
    }

    /// Parse a Linux bzImage and lay it out at the canonical addresses:
    /// the setup blob (boot-sector + setup sectors, `payload_offset`
    /// bytes total) lands at linear 0x90000 — the historical "SETUPSEG"
    /// = 0x9000:0000 — and the protected-mode kernel payload lands at
    /// `code32_start` from the header (typically 0x10_0000).
    ///
    /// If the bzImage advertises a v2.10 `init_size`, we validate that
    /// the VM has at least `code32_start + init_size` bytes of RAM —
    /// a relocatable kernel writes that much while decompressing, so
    /// loading into too-small a VM would corrupt host data structures
    /// or crash partway through. Older images (init_size = 0) skip
    /// the check, matching the pre-2.10 contract.
    ///
    /// Returns the parsed [`BzImage`] so the caller can read fields it
    /// also needs (entry, ramdisk pointers, etc.).
    ///
    /// Loader-only: this does *not* boot the kernel — see the bzImage
    /// integration test for the rest of the dance. A future tick will
    /// add an end-to-end `boot_bzimage` once we have the matching
    /// real-mode setup execution flow.
    pub fn load_bzimage(&mut self, bytes: &[u8]) -> Result<BzImage, BzImageError> {
        let bz = bzimage::parse(bytes)?;
        // Validate init_size against available RAM. The check uses
        // u64 arithmetic to avoid the corner where code32_start +
        // init_size overflows u32 on a maliciously-large header.
        if bz.init_size != 0 {
            let need = bz.code32_start as u64 + bz.init_size as u64;
            let have = self.mem.size() as u64;
            if need > have {
                return Err(BzImageError::NotEnoughRam { need, have });
            }
        }
        // Setup blob at linear 0x90000.
        self.mem.write_slice(0x9_0000, &bytes[..bz.payload_offset]);
        // 32-bit payload at code32_start.
        if bz.payload_offset < bytes.len() {
            self.mem
                .write_slice(bz.code32_start, &bytes[bz.payload_offset..]);
        }
        Ok(bz)
    }

    /// Place a kernel command-line string in memory and point the
    /// loaded bzImage's `cmd_line_ptr` (setup header offset 0x228)
    /// at it. The conventional bootloader layout puts the string
    /// at linear `0x90800` (2 KiB past the setup blob at 0x90000),
    /// where the kernel's setup.bin already expects to find it.
    ///
    /// `cmdline` is truncated to 2047 bytes — the practical cap our
    /// 2 KiB slot can hold with room for a null terminator. Most
    /// modern kernels advertise `cmdline_size = 4096` or more, but
    /// the 2 KiB convention works back to early protocol versions.
    /// Call *after* [`load_bzimage`] — this only updates two
    /// regions of memory and doesn't validate that a bzImage is
    /// actually loaded.
    pub fn set_kernel_cmdline(&mut self, cmdline: &str) {
        const CMD_LINE_ADDR: u32 = 0x9_0800;
        const MAX_LEN: usize = 2047;
        let bytes = cmdline.as_bytes();
        let len = bytes.len().min(MAX_LEN);
        self.mem.write_slice(CMD_LINE_ADDR, &bytes[..len]);
        self.mem.write_u8(CMD_LINE_ADDR + len as u32, 0); // null terminator
                                                          // cmd_line_ptr in the setup header lives at 0x90000 + 0x228.
        self.mem
            .write_u32(0x9_0000 + bzimage::OFF_CMD_LINE_PTR as u32, CMD_LINE_ADDR);
    }

    /// Place an initial RAM disk at the top of physical memory,
    /// page-aligned downward, and write its address/size into the
    /// bzImage setup header (ramdisk_image at 0x90218,
    /// ramdisk_size at 0x9021C). The kernel reads those fields
    /// during boot to find and unpack the initrd.
    ///
    /// "Top-aligned" placement is what real bootloaders do — keeps
    /// the kernel's expand-from-low-memory layout undisturbed.
    /// If the image is larger than available RAM (rounded up to a
    /// 4 KiB boundary) we return `BzImageError::NotEnoughRam`
    /// rather than silently truncating; a half-loaded initrd would
    /// corrupt the root filesystem the kernel tries to mount.
    ///
    /// Call after [`load_bzimage`] so the setup-header writes land
    /// in the bzImage's actual header copy.
    pub fn set_ramdisk(&mut self, bytes: &[u8]) -> Result<(), BzImageError> {
        let len = bytes.len();
        let aligned = (len + 0xFFF) & !0xFFF;
        let ram_size = self.mem.size();
        if aligned > ram_size {
            return Err(BzImageError::NotEnoughRam {
                need: aligned as u64,
                have: ram_size as u64,
            });
        }
        let start = (ram_size - aligned) as u32;
        self.mem.write_slice(start, bytes);
        self.mem
            .write_u32(0x9_0000 + bzimage::OFF_RAMDISK_IMAGE as u32, start);
        self.mem
            .write_u32(0x9_0000 + bzimage::OFF_RAMDISK_SIZE as u32, len as u32);
        // type_of_loader at 0x90210 — Linux checks this to enable
        // the "loader knows the boot protocol" code path. With
        // type_of_loader=0 Linux treats the boot block as legacy
        // and ignores ramdisk_image / cmd_line_ptr. We claim
        // 0xFF = "Unknown bootloader, with reserved value 0xF" —
        // the conventional sentinel a non-syslinux/non-grub loader
        // uses to opt into modern protocol fields without lying
        // about its identity.
        self.mem.write_u8(0x9_0000 + 0x210, 0xFF);
        Ok(())
    }

    /// Cold-boot from disk: reset the CPU, copy sector 0 of the loaded
    /// disk image to linear `0x7C00` (the standard boot-sector load
    /// address), then continue with the same autorun/UART setup as
    /// [`boot`]. Real BIOS does this before the IVT is even guest-
    /// visible — we model it directly on `Vm` rather than inside the
    /// CPU because nothing in CPU state needs to know.
    pub fn boot_from_disk(&mut self) {
        self.cpu.reset_to_boot();
        let mut buf = [0u8; wwwvm_devices::DISK_SECTOR_SIZE];
        self.io.disk().read_sectors(0, 1, &mut buf);
        self.mem.write_slice(BOOT_LOAD_ADDR, &buf);
        self.io.uart_mut().push_rx(&self.autorun);
        self.autorun.clear();
        self.booted = true;
    }

    pub fn is_booted(&self) -> bool {
        self.booted
    }

    pub fn is_halted(&self) -> bool {
        self.cpu.halted
    }

    /// Push bytes for the guest to read via the UART. JS uses this to
    /// forward keystrokes or `runCommand` payloads.
    pub fn send_input(&mut self, bytes: &[u8]) {
        self.io.uart_mut().push_rx(bytes);
        self.io.mark_irq_dirty();
    }

    /// Push a raw scan code byte into the PS/2 keyboard buffer.
    /// Raises IRQ 1 to a guest that has unmasked it. The translation
    /// from host keystrokes to scan codes (Set 1 / Set 2) is the
    /// host's job — this just queues bytes.
    pub fn push_scancode(&mut self, code: u8) {
        self.io.kbd.push_scancode(code);
        self.io.mark_irq_dirty();
    }

    /// Inject a PS/2 mouse movement/button packet (raises IRQ 12 to a
    /// guest that has enabled the aux port + reporting). `dx`/`dy` are
    /// signed deltas in PS/2 convention: +x = right, **+y = up**. The
    /// host feeds these from canvas pointer events, negating the screen
    /// y-delta (screen y grows downward). No-op until the guest enables
    /// mouse reporting.
    pub fn push_mouse_packet(&mut self, dx: i16, dy: i16, left: bool, right: bool, middle: bool) {
        self.io.kbd.push_mouse_packet(dx, dy, left, right, middle);
        self.io.mark_irq_dirty();
    }

    /// Type an ASCII string into the PS/2 keyboard as Set-1 scan codes
    /// (make+break, Shift-wrapped for uppercase/symbols; unmapped chars
    /// skipped). For scripting keyboard input to a graphical guest from the
    /// host — the native counterpart of the browser's per-key path.
    pub fn type_ascii(&mut self, text: &str) {
        for code in wwwvm_devices::string_to_scancodes(text) {
            self.io.kbd.push_scancode(code);
        }
    }

    /// Seed the CMOS clock with a date/time. Arguments are natural decimal
    /// values, year two-digit (00..99); the device stores them BCD-encoded
    /// (Status B is BCD + 24-hour), which is what a guest probing 0x70/0x71
    /// and the Linux `rtc-cmos` driver expect.
    pub fn set_cmos_time(
        &mut self,
        year: u8,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) {
        self.io
            .cmos
            .set_time(year, month, day, hour, minute, second);
    }

    /// Seed the CMOS clock from the HOST's current wall-clock time (UTC),
    /// so the guest's `date` reflects real time instead of the build-time
    /// default. Interactive front-ends (the console examples) call this;
    /// tests deliberately don't, keeping a deterministic 2026-01-01 default.
    /// A pre-epoch host clock leaves the default untouched.
    pub fn set_cmos_time_from_host(&mut self) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if secs == 0 {
            return;
        }
        let (y, mo, d, h, mi, s) = civil_from_unix_secs(secs);
        self.set_cmos_time(
            (y % 100) as u8,
            mo as u8,
            d as u8,
            h as u8,
            mi as u8,
            s as u8,
        );
    }

    /// Drain everything the guest has transmitted since the last call.
    pub fn drain_output(&mut self) -> Vec<u8> {
        self.io.uart_mut().drain_tx()
    }

    /// Drain the Ethernet frames the guest's NIC driver has transmitted
    /// since the last call. Each is a complete L2 frame (dst/src MAC,
    /// ethertype, payload) the CPU step copied out of guest RAM via the
    /// RTL8139 bus-master TX path. The host networking bridge feeds these
    /// onward; an empty vec means nothing was sent.
    pub fn drain_tx_frames(&mut self) -> Vec<Vec<u8>> {
        self.io.drain_nic_tx()
    }

    /// Pop a single transmitted frame (oldest first), or None when none are
    /// queued. For one-frame-at-a-time hosts (the wasm/browser bridge).
    pub fn drain_tx_frames_one(&mut self) -> Option<Vec<u8>> {
        self.io.pop_nic_tx()
    }

    /// Deliver one inbound Ethernet frame (L2, no CRC) to the guest's NIC.
    /// The device lays out the RX-ring header and tells us where to DMA it;
    /// we (holding the `Memory`) write the bytes into guest RAM and the
    /// device raises ISR.ROK, so the next CPU step asserts IRQ 11 and the
    /// driver's receive handler runs. Returns false if RX is disabled or
    /// the ring is full (frame dropped) — the inverse of `drain_tx_frames`.
    pub fn inject_rx_frame(&mut self, frame: &[u8]) -> bool {
        match self.io.rtl8139.accept_rx(frame) {
            Some((dest, bytes)) => {
                for (i, b) in bytes.iter().enumerate() {
                    self.mem.write_u8(dest.wrapping_add(i as u32), *b);
                }
                // An accepted RX frame raises the NIC's RX-OK interrupt.
                self.io.mark_irq_dirty();
                true
            }
            None => false,
        }
    }

    /// Step the CPU up to `max` times. Returns (steps_executed, reason).
    /// HLT is treated as a *terminal* stop here — for the guests in
    /// our test fleet (HELLO_GUEST, the calculator demo, etc.) the
    /// program is done once it halts and a tight loop downstream
    /// would just spin. For kernel-idle-aware stepping (where HLT
    /// with IF=1 is a wait-for-IRQ rather than a terminal stop), use
    /// `run_steps_idle_aware`.
    pub fn run_steps(&mut self, max: u32) -> (u32, Stop) {
        if !self.booted {
            self.boot();
        }
        let mut executed = 0;
        for _ in 0..max {
            if self.cpu.halted {
                return (executed, Stop::Halted);
            }
            if let Err(e) = self.cpu.step(&mut self.mem, &mut self.io) {
                return (executed, Stop::CpuError(e));
            }
            executed += 1;
        }
        (executed, Stop::StepBudget)
    }

    /// Same as `run_steps`, but a HLT with EFLAGS.IF=1 is treated as
    /// an idle wait — `cpu.step` itself keeps ticking the timer
    /// sources and will dispatch the next IRQ that fires, so the
    /// kernel's `STI; HLT` idle pattern eventually resumes. A HLT
    /// with IF=0 is still terminal: nothing can wake the CPU. Use
    /// this for the Linux boot probe; existing test fleets call
    /// `run_steps` directly so their fast Halted return path is
    /// preserved.
    pub fn run_steps_idle_aware(&mut self, max: u32) -> (u32, Stop) {
        if !self.booted {
            self.boot();
        }
        let mut executed = 0;
        for _ in 0..max {
            if self.cpu.halted && self.cpu.flags & flag::IF == 0 {
                return (executed, Stop::Halted);
            }
            if let Err(e) = self.cpu.step(&mut self.mem, &mut self.io) {
                return (executed, Stop::CpuError(e));
            }
            executed += 1;
        }
        (executed, Stop::StepBudget)
    }

    /// Like `run_steps_idle_aware`, but returns as soon as the guest is
    /// *genuinely idle* — halted with IF=1 and no internal source (timer,
    /// LAPIC) about to wake it. That's the moment the guest is blocked
    /// waiting for EXTERNAL input (a NIC RX frame, a console keystroke), so
    /// the host loop should stop spinning and go feed it. Without this, a
    /// guest blocked on network data spins the whole step budget on its idle
    /// HLT before the host gets a turn to inject, starving throughput.
    ///
    /// HLT with IF=0 is still terminal (`Halted`); a CPU error still returns
    /// `CpuError`; exhausting `max` returns `StepBudget`. The early idle
    /// return also reports `StepBudget` (the caller treats "ran < max" as a
    /// cue to inject + briefly sleep).
    pub fn run_steps_until_idle(&mut self, max: u32) -> (u32, Stop) {
        if !self.booted {
            self.boot();
        }
        let mut executed = 0;
        for _ in 0..max {
            if self.cpu.halted && self.cpu.flags & flag::IF == 0 {
                return (executed, Stop::Halted);
            }
            let was_halted = self.cpu.halted;
            if let Err(e) = self.cpu.step(&mut self.mem, &mut self.io) {
                return (executed, Stop::CpuError(e));
            }
            executed += 1;
            // Entered the step halted-with-IF and still halted afterwards: the
            // tick didn't surface an IRQ, so nothing internal will wake it.
            // Hand control back so the host can deliver external input.
            if was_halted && self.cpu.halted {
                return (executed, Stop::StepBudget);
            }
        }
        (executed, Stop::StepBudget)
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }
    pub fn mem(&self) -> &Memory {
        &self.mem
    }
}

/// Hand-assembled real-mode guest. Layout:
///
/// ```text
/// 00: BE 1D 7C       mov si, 0x7C1D      ; -> "wwwvm: ready\n\0"
/// 03: AC             lodsb               ; AL = DS:[SI], SI++
/// 04: 08 C0          or al, al           ; ZF=1 if NUL
/// 06: 74 06          jz  +6  -> 0x0E     ; into read loop on NUL
/// 08: BA F8 03       mov dx, 0x3F8       ; UART THR
/// 0B: EE             out dx, al
/// 0C: EB F5          jmp -11 -> 0x03     ; next char
/// 0E: BA FD 03       mov dx, 0x3FD       ; UART LSR  (read loop)
/// 11: EC             in  al, dx
/// 12: A8 01          test al, 1          ; DR bit
/// 14: 74 F8          jz  -8  -> 0x0E
/// 16: BA F8 03       mov dx, 0x3F8       ; UART RBR
/// 19: EC             in  al, dx
/// 1A: EE             out dx, al          ; echo
/// 1B: EB F1          jmp -15 -> 0x0E
/// 1D: "wwwvm: ready\n\0"
/// ```
pub const HELLO_GUEST: &[u8] = &[
    0xBE, 0x1D, 0x7C, // mov si, 0x7C1D
    0xAC, // lodsb
    0x08, 0xC0, // or al, al
    0x74, 0x06, // jz +6
    0xBA, 0xF8, 0x03, // mov dx, 0x3F8
    0xEE, // out dx, al
    0xEB, 0xF5, // jmp -11
    0xBA, 0xFD, 0x03, // mov dx, 0x3FD
    0xEC, // in al, dx
    0xA8, 0x01, // test al, 1
    0x74, 0xF8, // jz -8
    0xBA, 0xF8, 0x03, // mov dx, 0x3F8
    0xEC, // in al, dx
    0xEE, // out dx, al
    0xEB, 0xF1, // jmp -15
    b'w', b'w', b'w', b'v', b'm', b':', b' ', b'r', b'e', b'a', b'd', b'y', 0x0A, 0x00,
];

/// Mini-calculator guest. Each byte pushed via `vm.send_input(&[n])`
/// is squared with `MUL r/m8` (AX = AL × BL) and printed as decimal
/// followed by a newline. The print_dec subroutine showcases the
/// canonical ASCII-formatting idiom — divide by ten, push the
/// remainder, repeat until the quotient is zero, then pop and emit.
///
/// `[7]` → `"49\n"`, `[16]` → `"256\n"`, `[255]` → `"65025\n"`.
pub mod calculator_demo {
    pub const BASE_ADDR: u32 = 0x7C00;
    pub const PRINT_DEC_ADDR: u32 = 0x7C30;

    /// Main poll-mul-print loop. See [docs/HAND_ASSEMBLY.md](../../docs/HAND_ASSEMBLY.md)
    /// for an annotated walkthrough of how the bytes encode the
    /// instructions.
    pub const MAIN: &[u8] = &[
        // 0x00 BA FD 03    MOV DX, 0x3FD       ; UART LSR
        0xBA, 0xFD, 0x03, // 0x03 EC          IN  AL, DX
        0xEC, // 0x04 A8 01       TEST AL, 1
        0xA8, 0x01, // 0x06 74 FB       JZ  -5  -> 0x03     ; spin until ready
        0x74, 0xFB, // 0x08 BA F8 03    MOV DX, 0x3F8       ; UART RBR
        0xBA, 0xF8, 0x03, // 0x0B EC          IN  AL, DX
        0xEC, // 0x0C 88 C3       MOV BL, AL
        0x88, 0xC3, // 0x0E F6 E3       MUL BL              ; AX = AL * BL
        0xF6, 0xE3, // 0x10 E8 1D 00    CALL +0x1D -> 0x30  ; print_dec
        0xE8, 0x1D, 0x00, // 0x13 B0 0A       MOV AL, '\n'
        0xB0, 0x0A, // 0x15 BA F8 03    MOV DX, 0x3F8
        0xBA, 0xF8, 0x03, // 0x18 EE          OUT DX, AL
        0xEE, // 0x19 EB E5       JMP -27 -> 0x00     ; next input
        0xEB, 0xE5,
    ];

    /// `print_dec` — emits AX as a variable-length decimal string.
    /// Loaded at 0x7C30 so the main routine's `CALL +0x1D` lands on
    /// the first instruction.
    pub const PRINT_DEC: &[u8] = &[
        // 0x30 BB 0A 00    MOV BX, 10
        0xBB, 0x0A, 0x00, // 0x33 31 C9       XOR CX, CX
        0x31, 0xC9, // 0x35 31 D2       XOR DX, DX
        0x31, 0xD2, // 0x37 F7 F3       DIV BX
        0xF7, 0xF3, // 0x39 52          PUSH DX
        0x52, // 0x3A 41          INC CX
        0x41, // 0x3B 09 C0       OR  AX, AX
        0x09, 0xC0, // 0x3D 75 F6       JNZ -10 -> 0x35
        0x75, 0xF6, // 0x3F 58          POP AX
        0x58, // 0x40 04 30       ADD AL, '0'
        0x04, 0x30, // 0x42 BA F8 03    MOV DX, 0x3F8
        0xBA, 0xF8, 0x03, // 0x45 EE          OUT DX, AL
        0xEE, // 0x46 E2 F7       LOOP -9 -> 0x3F
        0xE2, 0xF7, // 0x48 C3          RET
        0xC3,
    ];
}

/// Interrupt-driven interactive demo. Unlike [`HELLO_GUEST`], which
/// polls the UART LSR in a tight loop, this one wires the UART to
/// IRQ 4 and lets the CPU spin in `JMP -2` — characters typed by the
/// host are delivered to the guest via an interrupt and echoed back
/// from the handler. Demonstrates the IDT + PIC + UART pipeline on
/// the same `Vm` API used by JS.
pub mod interactive_demo {
    pub const MAIN_ADDR: u32 = 0x7C00;
    pub const HANDLER_ADDR: u32 = 0x7C30;
    pub const GREETING_ADDR: u32 = 0x7C50;
    /// COM1 → IRQ 4 → vector 0x0C with the default PIC vector base 8.
    pub const IRQ4_VECTOR: u8 = 0x0C;

    /// Main entry. Prints the greeting via LODSB+OUT, configures
    /// UART IER, unmasks IRQ 4, STIs, then sits in an infinite
    /// `JMP -2` so refresh_irqs keeps polling between interrupts.
    ///
    /// ```text
    /// 0x00 BE 50 7C    MOV SI, 0x7C50              ; greeting string
    /// 0x03 AC          LODSB
    /// 0x04 08 C0       OR  AL, AL
    /// 0x06 74 06       JZ  +6  -> 0x0E (banner done)
    /// 0x08 BA F8 03    MOV DX, 0x3F8
    /// 0x0B EE          OUT DX, AL
    /// 0x0C EB F5       JMP -11 -> 0x03 (next char)
    /// 0x0E BA F9 03    MOV DX, 0x3F9               ; UART IER
    /// 0x11 B0 01       MOV AL, 1
    /// 0x13 EE          OUT DX, AL
    /// 0x14 B0 EF       MOV AL, 0xEF                ; unmask IRQ 4 only
    /// 0x16 E6 21       OUT 0x21, AL                ; PIC IMR
    /// 0x18 FB          STI
    /// 0x19 EB FE       JMP -2                       ; spin
    /// ```
    pub const MAIN: &[u8] = &[
        0xBE, 0x50, 0x7C, 0xAC, 0x08, 0xC0, 0x74, 0x06, 0xBA, 0xF8, 0x03, 0xEE, 0xEB, 0xF5, 0xBA,
        0xF9, 0x03, 0xB0, 0x01, 0xEE, 0xB0, 0xEF, 0xE6, 0x21, 0xFB, 0xEB, 0xFE,
    ];

    /// UART IRQ 4 handler. Reads RBR into AL, writes it straight back
    /// to THR (echo), EOIs the PIC, IRETs.
    ///
    /// ```text
    /// 0x00 50          PUSH AX
    /// 0x01 52          PUSH DX
    /// 0x02 BA F8 03    MOV DX, 0x3F8
    /// 0x05 EC          IN  AL, DX
    /// 0x06 EE          OUT DX, AL                  ; echo
    /// 0x07 B0 20       MOV AL, 0x20
    /// 0x09 E6 20       OUT 0x20, AL                ; non-specific EOI
    /// 0x0B 5A          POP DX
    /// 0x0C 58          POP AX
    /// 0x0D CF          IRET
    /// ```
    pub const HANDLER: &[u8] = &[
        0x50, 0x52, 0xBA, 0xF8, 0x03, 0xEC, 0xEE, 0xB0, 0x20, 0xE6, 0x20, 0x5A, 0x58, 0xCF,
    ];

    /// NUL-terminated banner printed once on boot. The trailing newline
    /// matters: terminals only flush a line when they see `\n`.
    pub const GREETING: &[u8] = b"wwwvm interactive\n\0";
}

#[cfg(test)]
mod tests;
