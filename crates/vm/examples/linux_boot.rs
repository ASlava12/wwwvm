//! Linux-boot probe — load a real bzImage and run until we hit a
//! wall, then dump enough state to know where we stopped. Not a
//! test; the kernel almost certainly will not complete its first
//! million instructions on our CPU. The point is to find out *which*
//! instruction / MMIO access / page-walk our model can't handle so
//! the next concrete tick has a concrete target.
//!
//! Usage:
//!   WWWVM_KERNEL=/path/to/vmlinuz \
//!     cargo run -p wwwvm-vm --release --example linux_boot
//!
//! Prints:
//!   * the bzImage header fields (code32_start, init_size, version)
//!   * step budget used and the Stop reason
//!   * EIP / EAX / EBX / EFLAGS / CR0 / CR3 at stop
//!   * any UART bytes the kernel pushed (earlyprintk output)
//!
//! Cargo budget: 256 MiB RAM, 5M steps. Both are tunable below.

use std::env;
use std::fs;
use std::io::Write;
use std::time::Instant;
use wwwvm_vm::{Stop, Vm};

const RAM_SIZE: usize = 256 * 1024 * 1024;
const STEP_BUDGET: u64 = 500_000_000;

fn main() {
    let path = env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
    };
    println!("loaded {} bytes from {}", bytes.len(), path);

    let mut vm = Vm::with_ram_size(RAM_SIZE);
    let bz = match vm.load_bzimage(&bytes) {
        Ok(bz) => bz,
        Err(e) => {
            eprintln!("load_bzimage: {e:?}");
            std::process::exit(1);
        }
    };
    println!(
        "  code32_start = 0x{:08X}  init_size = 0x{:08X}  payload_offset = 0x{:X}",
        bz.code32_start, bz.init_size, bz.payload_offset
    );
    println!(
        "  setup_sects  = {}  version = {}.{:02}  relocatable = {}",
        bz.setup_sects,
        bz.version >> 8,
        bz.version & 0xFF,
        bz.relocatable_kernel
    );

    // Minimal earlyprintk cmdline — without console=ttyS0 the kernel
    // queues output instead of pushing to UART. With it we should see
    // log lines as soon as the kernel's console driver initializes.
    vm.set_kernel_cmdline("earlyprintk=ttyS0,115200 console=ttyS0 panic=10");

    vm.start_protected_mode_at(bz.code32_start);
    println!("entered PM at 0x{:08X}", bz.code32_start);

    // Step in chunks so we can watch EIP / CR0.PG / CS transitions.
    let t0 = Instant::now();
    let mut steps = 0u64;
    let chunk = 10_000_000u32;
    let mut last_cr0_pg = false;
    let mut last_cs_hi = false;
    let mut last_eip_hi = false;
    let mut last_cr4 = 0u32;
    let mut last_idtr_base = 0u32;
    let mut last_cr3 = 0u32;
    let mut last_cr2 = 0u32;
    let mut last_eip_region: Option<u32> = None;
    let mut last_eip_sample = 0u32;
    let mut stuck_chunks = 0u32;
    let mut stop = Stop::StepBudget;
    while steps < STEP_BUDGET {
        let (s, st) = vm.run_steps(chunk);
        steps += s as u64;
        if matches!(st, Stop::CpuError(_) | Stop::Halted) {
            stop = st;
            break;
        }
        let pg = vm.cpu().cr0 & (1 << 31) != 0;
        let cs_hi = vm.cpu().sregs[wwwvm_cpu::sreg::CS] != 0x08;
        let eip_hi = vm.cpu().ip >= 0xC000_0000;
        if pg && !last_cr0_pg {
            println!("[{:>10}] CR0.PG=1 at EIP={:08X}", steps, vm.cpu().ip);
        }
        if cs_hi && !last_cs_hi {
            println!(
                "[{:>10}] kernel reloaded GDT (CS={:#06X}) at EIP={:08X}",
                steps,
                vm.cpu().sregs[wwwvm_cpu::sreg::CS],
                vm.cpu().ip
            );
        }
        if eip_hi && !last_eip_hi {
            println!(
                "[{:>10}] entered high-memory virtual space (EIP={:08X})",
                steps,
                vm.cpu().ip
            );
            // Snapshot physical memory just after the decompressor
            // hands off to the kernel — this is what the kernel
            // sees in low physical RAM. Compare against the bytes
            // gunzip extracts from the bzImage to find where (and
            // how) decompression deviates.
            if let Ok(path) = env::var("WWWVM_DUMP_AT_PG") {
                let mut f = match std::fs::File::create(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("dump create {path}: {e}");
                        continue;
                    }
                };
                let mut buf = vec![0u8; 0x0100_0000]; // 16 MiB
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = vm.mem().read_u8(0x0010_0000 + i as u32);
                }
                if let Err(e) = f.write_all(&buf) {
                    eprintln!("dump write: {e}");
                } else {
                    println!("  dumped 16 MiB phys 0x100000..0x1100000 -> {path}");
                }
            }
        }
        last_cr0_pg = pg;
        last_cs_hi = cs_hi;
        last_eip_hi = eip_hi;
        let cr4 = vm.cpu().cr4;
        if cr4 != last_cr4 {
            println!(
                "[{:>10}] CR4 changed: {:08X} -> {:08X} at EIP={:08X}",
                steps,
                last_cr4,
                cr4,
                vm.cpu().ip
            );
            last_cr4 = cr4;
        }
        let cr3 = vm.cpu().cr3;
        if cr3 != last_cr3 {
            println!(
                "[{:>10}] CR3 changed: {:08X} -> {:08X} at EIP={:08X}",
                steps,
                last_cr3,
                cr3,
                vm.cpu().ip
            );
            last_cr3 = cr3;
        }
        let idtr_base = vm.cpu().idtr.base;
        if idtr_base != last_idtr_base {
            println!(
                "[{:>10}] IDTR base: {:08X} -> {:08X} at EIP={:08X}",
                steps,
                last_idtr_base,
                idtr_base,
                vm.cpu().ip
            );
            last_idtr_base = idtr_base;
        }
        // Track which 1-MiB EIP region we're in. Switches expose
        // big control-flow changes (kernel function tables, etc.)
        let region = vm.cpu().ip & 0xFFF0_0000;
        if Some(region) != last_eip_region {
            println!(
                "[{:>10}] EIP region {:08X}.. (was {:08X}..)",
                steps,
                region,
                last_eip_region.unwrap_or(0)
            );
            last_eip_region = Some(region);
        }
        // CR2 only updates on a #PF — every transition here is a
        // new page-fault address. Spammy if the handler keeps
        // page-faulting in a loop, which is itself useful info.
        let cr2 = vm.cpu().cr2;
        if cr2 != last_cr2 {
            println!(
                "[{:>10}] #PF CR2: {:08X} -> {:08X} (EIP={:08X})",
                steps,
                last_cr2,
                cr2,
                vm.cpu().ip
            );
            last_cr2 = cr2;
        }
        // Coarse stuck-detection: if EIP didn't move out of a
        // 256-byte window across two consecutive chunks, the
        // kernel is in a tight loop — log so we know when it
        // started, even if no other transitions fire.
        let eip = vm.cpu().ip;
        if eip & !0xFF == last_eip_sample & !0xFF {
            stuck_chunks += 1;
            if stuck_chunks == 1 {
                println!("[{:>10}] EIP stuck in {:08X}..+0x100", steps, eip & !0xFF);
            }
        } else {
            stuck_chunks = 0;
        }
        last_eip_sample = eip;
        if (steps % 100_000_000) < (chunk as u64) {
            let out = vm.drain_output();
            if !out.is_empty() {
                println!(
                    "[{:>10}] UART pushed {} bytes: {:?}",
                    steps,
                    out.len(),
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }
    let elapsed = t0.elapsed();

    let cpu = vm.cpu();
    let mem = vm.mem();
    println!(
        "\nstopped after {steps} steps in {:.2}s — reason: {:?}",
        elapsed.as_secs_f64(),
        stop
    );

    let cs = cpu.sregs[wwwvm_cpu::sreg::CS];
    let eip = cpu.ip;
    let eax = cpu.read_r32(0);
    let ebx = cpu.read_r32(3);
    let ecx = cpu.read_r32(1);
    let edx = cpu.read_r32(2);
    let esp = cpu.read_r32(4);
    let ebp = cpu.read_r32(5);
    let esi = cpu.read_r32(6);
    let edi = cpu.read_r32(7);

    println!("  CS:EIP = {cs:04X}:{eip:08X}");
    println!("  EAX={eax:08X}  EBX={ebx:08X}  ECX={ecx:08X}  EDX={edx:08X}");
    println!("  ESI={esi:08X}  EDI={edi:08X}  ESP={esp:08X}  EBP={ebp:08X}");
    println!(
        "  EFLAGS={:04X}  CR0={:08X}  CR2={:08X}  CR3={:08X}  CR4={:08X}",
        cpu.flags, cpu.cr0, cpu.cr2, cpu.cr3, cpu.cr4
    );

    // Dump 16 bytes around EIP — what the next opcode looks like.
    let base = eip.saturating_sub(4);
    let mut win = Vec::new();
    for i in 0..16 {
        win.push(mem.read_u8(base + i));
    }
    print!("  bytes @ EIP-4: ");
    for (i, b) in win.iter().enumerate() {
        if base + i as u32 == eip {
            print!("[{:02X}] ", b);
        } else {
            print!("{:02X} ", b);
        }
    }
    println!();

    // Stack contents — top 32 bytes (likely contain saved return
    // addresses / register spills from the path that got us here).
    print!("  stack @ ESP: ");
    for i in 0..8 {
        let w = mem.read_u32(esp.wrapping_add(i * 4));
        print!("{w:08X} ");
    }
    println!();

    // IDT[14] = #PF gate. IDTR base is a *virtual* address — we
    // must walk through CR3 to read the gate. If our CPU's #PF
    // dispatch doesn't do this walk, that's the bug.
    let walk = |va: u32| -> Option<u32> {
        let pde_idx = (va >> 22) as usize;
        let pte_idx = ((va >> 12) & 0x3FF) as usize;
        let pde = mem.read_u32((cpu.cr3 & !0xFFF) + pde_idx as u32 * 4);
        if pde & 1 == 0 {
            return None;
        }
        if pde & 0x80 != 0 {
            return Some((pde & 0xFFC0_0000) | (va & 0x003F_FFFF));
        }
        let pte = mem.read_u32((pde & !0xFFF) + pte_idx as u32 * 4);
        if pte & 1 == 0 {
            return None;
        }
        Some((pte & !0xFFF) | (va & 0xFFF))
    };

    let idt_base = cpu.idtr.base;
    let idt14_va = idt_base.wrapping_add(14 * 8);
    let (g_lo, g_hi) = match walk(idt14_va) {
        Some(p_lo) => (mem.read_u32(p_lo), mem.read_u32(p_lo + 4)),
        None => (0, 0),
    };
    let handler_off = (g_lo & 0xFFFF) | (g_hi & 0xFFFF_0000);
    let handler_sel = (g_lo >> 16) & 0xFFFF;
    let handler_type = (g_hi >> 8) & 0xFF;
    println!(
        "  IDT[14] (VA {idt14_va:08X}) = offset {handler_off:08X}  sel {handler_sel:04X}  type {handler_type:02X}"
    );
    if handler_off == eip {
        println!("  -> EIP == IDT[14] offset: dispatch correctly entered do_page_fault");
    } else {
        println!(
            "  -> EIP differs from IDT[14]: either dispatch is broken, or handler tail-jumped here"
        );
    }

    // Dump 256 bytes at the physical location the handler maps to —
    // if it's all zeros we have a decompressor-output bug; if it's
    // non-zero our paging walks to the wrong physical address.
    let phys = (mem.read_u32(0x00C5CC08) & !0xFFF) // PDE[770] of CR3
        .wrapping_add(0); // first PT
    let pt_pa = mem.read_u32(0x00C5CC08) & !0xFFF;
    let pte_pa = pt_pa + 137 * 4;
    let pte_val = mem.read_u32(pte_pa);
    let phys_page = pte_val & !0xFFF;
    let phys_eip = phys_page | (eip & 0xFFF);
    println!("  physical EIP page: {phys_page:08X}  (PTE @ {pte_pa:08X} = {pte_val:08X})");
    // Scan 4096 bytes (whole page) to see how much is zero vs non-zero.
    let mut nonzero = 0;
    let mut first_nz = None;
    for i in 0..4096 {
        let b = mem.read_u8(phys_page + i);
        if b != 0 {
            nonzero += 1;
            if first_nz.is_none() {
                first_nz = Some((i, b));
            }
        }
    }
    println!(
        "  page {phys_page:08X} contents: {nonzero}/4096 non-zero bytes, first @ offset {:?}",
        first_nz
    );
    // 64-byte snapshot of the actual physical bytes at EIP.
    print!("  phys @ {phys_eip:08X}:");
    for i in 0..32 {
        print!(" {:02X}", mem.read_u8(phys_eip + i));
    }
    println!();
    let _ = phys; // silence unused

    // Walk the page directory to see how EIP's page is mapped.
    // VA bits 31:22 = PDE index, 21:12 = PTE index, 11:0 = offset.
    let pde_idx = (eip >> 22) as usize;
    let pte_idx = ((eip >> 12) & 0x3FF) as usize;
    let pde_addr = cpu.cr3 & !0xFFF | (pde_idx as u32 * 4);
    let pde = mem.read_u32(pde_addr);
    print!("  page walk EIP={eip:08X}: PDE[{pde_idx}] @ {pde_addr:08X} = {pde:08X}");
    if pde & 1 == 0 {
        println!("  (not present)");
    } else if pde & 0x80 != 0 {
        let phys = (pde & 0xFFC0_0000) | (eip & 0x003F_FFFF);
        println!("  (PS=1, 4 MiB page) -> phys {phys:08X}");
    } else {
        let pt_addr = (pde & !0xFFF) + pte_idx as u32 * 4;
        let pte = mem.read_u32(pt_addr);
        let phys = if pte & 1 != 0 {
            (pte & !0xFFF) | (eip & 0xFFF)
        } else {
            0
        };
        println!("  PTE[{pte_idx}] @ {pt_addr:08X} = {pte:08X}  -> phys {phys:08X}");
    }

    // What did the UART receive? Drain everything pending.
    let out = vm.drain_output();
    if out.is_empty() {
        println!("\nUART: (no output)");
    } else {
        println!(
            "\nUART ({} bytes):\n----- begin -----\n{}\n----- end -----",
            out.len(),
            String::from_utf8_lossy(&out)
        );
    }

    if let Stop::CpuError(e) = stop {
        println!("\nCPU error detail: {e}");
        std::process::exit(2);
    }

    // Optional second dump at the stopping point. Compare against
    // the WWWVM_DUMP_AT_PG dump to see what memory regions changed
    // between the decompressor handoff and the wall we hit.
    if let Ok(path) = env::var("WWWVM_DUMP_AT_STOP") {
        let mut f = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("dump create {path}: {e}");
                return;
            }
        };
        let mut buf = vec![0u8; 0x0100_0000];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = vm.mem().read_u8(0x0010_0000 + i as u32);
        }
        if let Err(e) = f.write_all(&buf) {
            eprintln!("dump write: {e}");
        } else {
            println!("dumped 16 MiB phys 0x100000..0x1100000 at stop -> {path}");
        }
    }
}
