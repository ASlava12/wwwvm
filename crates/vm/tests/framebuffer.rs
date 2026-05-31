//! Linear-framebuffer plumbing: `screen_info` + e820 construction.
//!
//! These are fast (no kernel) — they build a minimal bzImage header,
//! enable a framebuffer, run the protected-mode hand-off, and read the
//! boot_params bytes back out of guest RAM to confirm the kernel would
//! see a correctly-advertised efifb/vesafb framebuffer and a reserved
//! e820 region for it. The end-to-end "does efifb actually bind and
//! render" teeth-test lives in `linux_userspace.rs` (it needs a real
//! kernel and is `#[ignore]`).

use wwwvm_vm::{Vm, VIDEO_TYPE_EFI, VIDEO_TYPE_VLFB};

/// Minimal bzImage the loader accepts: setup_sects=1 (payload at
/// 1024), the 0xAA55 boot flag, the "HdrS" magic, a modern version,
/// and code32_start = 0x100000. init_size stays 0 so the RAM check is
/// skipped. `boot_sector_marker` lets a test stash a byte in the
/// boot-sector region (boot_params[0..0x1F1]) to prove the zero-page
/// wipe clears it.
fn minimal_bzimage(boot_sector_marker: Option<(usize, u8)>) -> Vec<u8> {
    let mut bz = vec![0u8; 1024];
    bz[0x1F1] = 1; // setup_sects
    bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes()); // boot_flag
    bz[0x202..0x206].copy_from_slice(b"HdrS");
    bz[0x206..0x208].copy_from_slice(&0x020Cu16.to_le_bytes()); // version 2.12
    bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes()); // code32_start
    if let Some((off, val)) = boot_sector_marker {
        bz[off] = val;
    }
    bz
}

const BP: u32 = 0x9_0000; // boot_params / zero page

#[test]
fn efifb_screen_info_and_reserved_e820_are_written() {
    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    // Stash a marker in the boot-sector region (apm_bios_info @ 0x40)
    // so we can prove the zero-page wipe clears it.
    let bz = vm
        .load_bzimage(&minimal_bzimage(Some((0x40, 0xAB))))
        .expect("load_bzimage");
    assert_eq!(vm.read_mem_u8(BP + 0x40), 0xAB, "marker should load first");

    vm.enable_linear_framebuffer(800, 600, VIDEO_TYPE_EFI);
    let cfg = vm.framebuffer_config().expect("fb enabled");
    assert_eq!((cfg.width, cfg.height, cfg.bpp), (800, 600, 32));
    assert_eq!(cfg.stride, 800 * 4);
    assert_eq!(cfg.video_type, VIDEO_TYPE_EFI);
    // Reserved region is page-aligned and sits at the top of RAM.
    assert_eq!(cfg.base & 0xFFF, 0, "base page-aligned");
    assert_eq!(cfg.size & 0xFFF, 0, "size page-rounded");
    assert!(cfg.size >= cfg.stride * cfg.height, "size covers the fb");
    assert!(
        cfg.base as u64 + cfg.size as u64 <= 256 * 1024 * 1024,
        "fb fits in RAM"
    );

    vm.start_protected_mode_at(bz.code32_start);

    // Zero-page wipe cleared the boot-sector marker.
    assert_eq!(vm.read_mem_u8(BP + 0x40), 0, "zero-page wipe clears 0x40");

    // screen_info advertises the framebuffer (offsets per struct
    // screen_info, <uapi/linux/screen_info.h>).
    assert_eq!(
        vm.read_mem_u8(BP + 0x0F),
        VIDEO_TYPE_EFI,
        "orig_video_isVGA"
    );
    assert_eq!(vm.read_mem_u16(BP + 0x12), 800, "lfb_width");
    assert_eq!(vm.read_mem_u16(BP + 0x14), 600, "lfb_height");
    assert_eq!(vm.read_mem_u16(BP + 0x16), 32, "lfb_depth");
    assert_eq!(vm.read_mem_u32(BP + 0x18), cfg.base, "lfb_base");
    assert_eq!(vm.read_mem_u32(BP + 0x1C), cfg.size, "lfb_size (bytes)");
    assert_eq!(vm.read_mem_u16(BP + 0x24), 800 * 4, "lfb_linelength");
    // 32bpp B,G,R,X channel layout.
    assert_eq!(vm.read_mem_u8(BP + 0x26), 8, "red_size");
    assert_eq!(vm.read_mem_u8(BP + 0x27), 16, "red_pos");
    assert_eq!(vm.read_mem_u8(BP + 0x29), 8, "green_pos");
    assert_eq!(vm.read_mem_u8(BP + 0x2B), 0, "blue_pos");
    assert_eq!(vm.read_mem_u8(BP + 0x2D), 24, "rsvd_pos");

    // e820: conventional, BIOS/video reserved, usable-up-to-fb,
    // reserved fb. With the fb at the very top of RAM there is no
    // usable tail, so exactly 4 entries.
    let count = vm.read_mem_u8(BP + 0x1E8);
    assert_eq!(count, 4, "conv + bios + usable + reserved-fb");
    // Entry 3 (index 3) is the reserved framebuffer.
    let e3 = BP + 0x2D0 + 3 * 20;
    assert_eq!(vm.read_mem_u32(e3), cfg.base, "e820[3].base == fb base");
    assert_eq!(vm.read_mem_u32(e3 + 8), cfg.size, "e820[3].size == fb size");
    assert_eq!(vm.read_mem_u32(e3 + 16), 2, "e820[3].type == reserved");
    // Entry 2 (usable) ends exactly where the fb begins.
    let e2 = BP + 0x2D0 + 2 * 20;
    assert_eq!(vm.read_mem_u32(e2), 0x0010_0000, "e820[2].base == 1 MiB");
    assert_eq!(
        vm.read_mem_u32(e2 + 8),
        cfg.base - 0x0010_0000,
        "usable span ends at fb base"
    );
}

#[test]
fn framebuffer_bytes_reads_back_guest_pixels() {
    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm
        .load_bzimage(&minimal_bzimage(None))
        .expect("load_bzimage");
    vm.enable_linear_framebuffer(640, 480, VIDEO_TYPE_EFI);
    vm.start_protected_mode_at(bz.code32_start);

    let cfg = vm.framebuffer_config().unwrap();
    // A guest pixel write lands in the framebuffer we read back.
    vm.load_image(cfg.base, &[0x11, 0x22, 0x33, 0xFF]);
    let px = vm.framebuffer_bytes().expect("fb bytes");
    assert_eq!(px.len(), (640 * 4 * 480) as usize, "len == stride * height");
    assert_eq!(&px[0..4], &[0x11, 0x22, 0x33, 0xFF], "B,G,R,X round-trips");
}

#[test]
fn vlfb_video_type_selects_vesafb() {
    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm
        .load_bzimage(&minimal_bzimage(None))
        .expect("load_bzimage");
    vm.enable_linear_framebuffer(1024, 768, VIDEO_TYPE_VLFB);
    vm.start_protected_mode_at(bz.code32_start);
    assert_eq!(vm.read_mem_u8(BP + 0x0F), VIDEO_TYPE_VLFB, "VLFB → vesafb");
    assert_eq!(vm.read_mem_u16(BP + 0x12), 1024);
    assert_eq!(vm.read_mem_u16(BP + 0x14), 768);
}

#[test]
fn no_framebuffer_keeps_three_entry_e820_and_blank_screen_info() {
    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    // Marker in the boot-sector region to confirm the zero-page wipe
    // still runs on the plain (no-fb) path.
    let bz = vm
        .load_bzimage(&minimal_bzimage(Some((0x0F, 0x99))))
        .expect("load_bzimage");
    vm.start_protected_mode_at(bz.code32_start);
    assert!(vm.framebuffer_config().is_none(), "no fb enabled");
    // screen_info stays blank → kernel skips a video console (serial only).
    assert_eq!(vm.read_mem_u8(BP + 0x0F), 0, "orig_video_isVGA cleared");
    assert_eq!(vm.read_mem_u16(BP + 0x12), 0, "lfb_width cleared");
    // The classic 3-entry map (conventional, BIOS/video, extended).
    assert_eq!(vm.read_mem_u8(BP + 0x1E8), 3, "no fb → 3 e820 entries");
}
