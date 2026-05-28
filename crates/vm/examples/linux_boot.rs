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
    let mut last_eip_region: Option<u32> = None;
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
}
