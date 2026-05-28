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
const STEP_BUDGET: u64 = 4_000_000_000;

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
    //
    // lpj=1000000 skips calibrate_delay's busy-wait loop. That loop
    // requires timer IRQs to update jiffies, which requires IF=1,
    // which our kernel hasn't reached yet — so without lpj we hit
    // a soft hang in calibrate_delay_converge. Setting lpj declares
    // a pre-computed value and skips calibration entirely.
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 initcall_debug",
    );

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
    // WWWVM_STOP_AT_FIRST_PF=1: replace the coarse chunk loop with
    // a fine 100-step loop that halts on the first observable #PF
    // (CR2 transition from 0 to non-zero) so we can dump physical
    // memory at that exact step, not millions of steps later.
    // Note CR2 stays set after the handler runs — so once we detect
    // a transition we then switch to single-step until we *catch*
    // the next CR2 update, giving us the exact faulting EIP.
    let stop_at_first_pf = env::var("WWWVM_STOP_AT_FIRST_PF").is_ok();
    // WWWVM_TRACE_ESP_ALIGN=1: after CR0.PG=1 switch to single-step
    // mode and report every ESP alignment transition (4-byte align
    // bit flipping). Kernel stacks are 4-byte aligned; the first
    // sustained misalignment is the lead for the unaligned-ESP at
    // first #PF, which then misreads stack values and feeds a wild
    // pointer into a kernel write.
    let trace_esp_align = env::var("WWWVM_TRACE_ESP_ALIGN").is_ok();
    let mut stop = Stop::StepBudget;
    let mut last_esp = 0u32;
    let mut last_esp_align = true;
    let mut last_eip = 0u32;
    let mut transitions = 0u32;
    let mut last_if = false;
    let mut if_transitions = 0u32;
    // WWWVM_STOP_AT_FIRST_EXC=1: halt at the moment a handler runs.
    // Detect by EIP entering the IDT[14] (#PF) handler region —
    // which our trace already proved to be at 0xC0889E00..+0x100.
    // The trace bookends gave us TSC=435422282 for the NULL-call
    // first exception; we want the stack contents at that moment.
    let stop_at_first_exc = env::var("WWWVM_STOP_AT_FIRST_EXC").is_ok();
    // WWWVM_STOP_AT_PANIC: halt at the moment EIP first enters
    // panic() (vmlinux disasm puts it at 0xC08730E0..0xC08731FF).
    // The stack at that moment names the caller — i.e. what
    // function decided this was unrecoverable.
    let stop_at_panic = env::var("WWWVM_STOP_AT_PANIC").is_ok();
    let stop_at_eip = env::var("WWWVM_STOP_AT_EIP")
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    // WWWVM_STOP_AT_CR2=0xVAL — halt at the first step where CR2
    // equals the specified value. Use this to catch the precise
    // moment of a known bad-pointer fault before Linux's #PF
    // handler runs and rewrites the visible state.
    let stop_at_cr2 = env::var("WWWVM_STOP_AT_CR2")
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    while steps < STEP_BUDGET {
        let pg_on = vm.cpu().cr0 & (1 << 31) != 0;
        let chunk = if pg_on && (stop_at_panic || stop_at_eip.is_some() || stop_at_cr2.is_some()) {
            1
        } else if pg_on && (stop_at_first_pf || stop_at_first_exc) {
            100
        } else if trace_esp_align && pg_on {
            1
        } else {
            chunk
        };
        // Capture EIP/ESP BEFORE the step so we can name the
        // instruction that retired into the transition.
        let pre_eip = vm.cpu().ip;
        let _ = pre_eip;
        let (s, st) = vm.run_steps(chunk);
        steps += s as u64;
        if trace_esp_align && pg_on {
            let esp = vm.cpu().read_r32(4);
            let aligned = esp & 3 == 0;
            if aligned != last_esp_align && transitions < 80 {
                // The instruction that retired is at last_eip — we
                // snapshotted it before the step. Read the bytes
                // through the kernel page tables.
                let cr3 = vm.cpu().cr3 & !0xFFF;
                let pde = vm.mem().read_u32(cr3 + ((last_eip >> 22) & 0x3FF) * 4);
                let phys = if pde & 1 != 0 {
                    let pt = pde & !0xFFF;
                    let pte = vm.mem().read_u32(pt + ((last_eip >> 12) & 0x3FF) * 4);
                    if pte & 1 != 0 {
                        Some((pte & !0xFFF) | (last_eip & 0xFFF))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let mut bytes = [0u8; 8];
                if let Some(p) = phys {
                    for (i, b) in bytes.iter_mut().enumerate() {
                        *b = vm.mem().read_u8(p.wrapping_add(i as u32));
                    }
                }
                println!(
                    "[{:>10}] ESP {} ({:#X} -> {:#X})  retired EIP={:08X}  bytes: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
                    steps,
                    if aligned { "REALIGNED" } else { "MISALIGNED" },
                    last_esp,
                    esp,
                    last_eip,
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
                );
                transitions += 1;
            }
            last_esp = esp;
            last_esp_align = aligned;
            last_eip = vm.cpu().ip;
        }
        if let Some(target) = stop_at_eip {
            // Exact match — caller must pick the right boundary
            // (e.g. just after a MOV they care about).
            let eip = vm.cpu().ip;
            if pg_on && eip == target {
                println!("[{steps:>10}] HIT EIP target {target:08X} (sampled EIP={eip:08X})");
                stop = st;
                break;
            }
        }
        if stop_at_panic && pg_on && (0xC08730E0..0xC0873200).contains(&vm.cpu().ip) {
            println!(
                "[{:>10}] FIRST panic() entry: EIP={:08X} ESP={:08X}",
                steps,
                vm.cpu().ip,
                vm.cpu().read_r32(4)
            );
            stop = st;
            break;
        }
        // EIP in IDT[14] handler region = a fault was just dispatched
        // (this catches the NULL-call case where CR2 stays at 0).
        if stop_at_first_exc && pg_on && (0xC0889E00..0xC088A000).contains(&vm.cpu().ip) {
            println!(
                "[{:>10}] FIRST EXC dispatched: EIP={:08X} (handler entry) ESP={:08X}",
                steps,
                vm.cpu().ip,
                vm.cpu().read_r32(4)
            );
            stop = st;
            break;
        }
        if let Some(target) = stop_at_cr2 {
            if pg_on && vm.cpu().cr2 == target {
                println!(
                    "[{:>10}] HIT CR2 target {:08X} at EIP={:08X} ESP={:08X}",
                    steps,
                    target,
                    vm.cpu().ip,
                    vm.cpu().read_r32(4)
                );
                stop = st;
                break;
            }
        }
        if stop_at_first_pf && vm.cpu().cr2 != 0 {
            // CR2 became non-zero — a #PF was raised somewhere in
            // the last `chunk` steps. The current EIP is *after*
            // the dispatch (or after the handler returned), not the
            // faulting instruction itself — CR2 stays set so we
            // can't rewind. Report and stop; for finer-grained
            // probing, use WWWVM_STOP_AT_FIRST_EXC instead.
            println!(
                "[{:>10}] CR2 first set: CR2={:08X} (within last {} steps), EIP now {:08X}",
                steps,
                vm.cpu().cr2,
                chunk,
                vm.cpu().ip
            );
            stop = st;
            break;
        }
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
        // IF (EFLAGS bit 9) — if it never flips to 1 after PG=1,
        // the kernel ran with interrupts disabled for the whole
        // post-paging life, which deadlocks any "wait for jiffies"
        // loop. Log every transition so we see when (if ever)
        // the kernel STIs.
        let if_set = (vm.cpu().flags & wwwvm_cpu::flag::IF) != 0;
        if if_set != last_if {
            if_transitions += 1;
            if if_transitions <= 10 {
                println!(
                    "[{:>10}] IF -> {} at EIP={:08X}  (transition #{if_transitions})",
                    steps,
                    if_set,
                    vm.cpu().ip
                );
            }
            last_if = if_set;
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
    // EIP is virtual; the kernel image is at phys ~0x100000+, so a
    // bare phys read of `eip` returns zero. Walk CR3 first so the
    // bytes shown match what the CPU is actually decoding.
    let walk_byte = |va: u32| -> Option<u32> {
        let cr3 = cpu.cr3 & !0xFFF;
        let pde = mem.read_u32(cr3 + ((va >> 22) & 0x3FF) * 4);
        if pde & 1 == 0 {
            return None;
        }
        if pde & 0x80 != 0 {
            return Some((pde & 0xFFC0_0000) | (va & 0x003F_FFFF));
        }
        let pte = mem.read_u32((pde & !0xFFF) + ((va >> 12) & 0x3FF) * 4);
        if pte & 1 == 0 {
            return None;
        }
        Some((pte & !0xFFF) | (va & 0xFFF))
    };
    let base = eip.saturating_sub(4);
    let mut win = Vec::new();
    for i in 0..16 {
        let b = walk_byte(base + i).map(|p| mem.read_u8(p)).unwrap_or(0xFF);
        win.push(b);
    }
    print!("  bytes @ EIP-4 (paged): ");
    for (i, b) in win.iter().enumerate() {
        if base + i as u32 == eip {
            print!("[{:02X}] ", b);
        } else {
            print!("{:02X} ", b);
        }
    }
    println!();

    // Stack contents through paging — read the kernel stack as the
    // kernel sees it. mem.read_u32 reads physical, which is wrong
    // for any VA above our RAM size; here we walk CR3 first. This
    // surfaces the return-address chain so we can name the caller
    // that led to the stuck __delay/loop.
    let walk_va = |va: u32| -> Option<u32> {
        let cr3 = cpu.cr3 & !0xFFF;
        let pde = mem.read_u32(cr3 + ((va >> 22) & 0x3FF) * 4);
        if pde & 1 == 0 {
            return None;
        }
        if pde & 0x80 != 0 {
            return Some((pde & 0xFFC0_0000) | (va & 0x003F_FFFF));
        }
        let pt = pde & !0xFFF;
        let pte = mem.read_u32(pt + ((va >> 12) & 0x3FF) * 4);
        if pte & 1 == 0 {
            return None;
        }
        Some((pte & !0xFFF) | (va & 0xFFF))
    };
    print!("  stack @ ESP (paged):");
    for i in 0..16 {
        let va = esp.wrapping_add(i * 4);
        if let Some(phys) = walk_va(va) {
            let w = mem.read_u32(phys);
            print!(" {w:08X}");
        } else {
            print!(" --------");
        }
    }
    println!();
    // EBP-based frame chain — kernel functions push EBP early, so
    // [EBP+4] is the return address into the caller and [EBP] is
    // the previous frame. Walk up to 8 frames.
    println!("  EBP-chain (call stack):");
    let mut frame_ebp = ebp;
    for depth in 0..8 {
        let ret_addr_va = frame_ebp.wrapping_add(4);
        let prev_ebp_va = frame_ebp;
        let ret = walk_va(ret_addr_va).map(|p| mem.read_u32(p)).unwrap_or(0);
        let prev = walk_va(prev_ebp_va).map(|p| mem.read_u32(p)).unwrap_or(0);
        if ret == 0 && prev == 0 {
            break;
        }
        println!("    [{depth}] EBP={frame_ebp:08X}  ret={ret:08X}");
        if prev == frame_ebp || prev < 0x1000 {
            break;
        }
        frame_ebp = prev;
    }

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

    // Resolve EIP's physical page via the same paging walk we use
    // elsewhere — earlier diagnostics assumed PDE always points to a
    // PT, which is wrong when PDE.PS=1 (4 MiB super-pages cover most
    // of the kernel linear region). Scan that page for content
    // density and dump 32 bytes at the resolved phys address so we
    // can tell "page is wiped" from "diagnostic was reading wrong
    // phys area".
    if let Some(phys_eip) = walk(eip) {
        let phys_page = phys_eip & !0xFFF;
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
            "  resolved phys EIP={phys_eip:08X}, page {phys_page:08X} contents: {nonzero}/4096 non-zero, first @ {:?}",
            first_nz
        );
        print!("  phys @ {phys_eip:08X}:");
        for i in 0..32 {
            print!(" {:02X}", mem.read_u8(phys_eip + i));
        }
        println!();
    } else {
        println!("  EIP {eip:08X} has no valid page translation");
    }

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
