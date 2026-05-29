//! Throughput benchmark for the VM step loop.
//!
//! Runs a deterministic ALU-heavy guest (ADD BX, CX + LOOP, 65535
//! iterations) and reports instructions executed per second. The
//! number reflects everything the user-visible `run_steps` path does:
//! refresh_irqs, IRQ check, fetch+decode+execute, all per step.
//!
//! Run:  cargo run --example throughput -p wwwvm-vm --release
//!
//! Compare release vs debug builds — debug is typically 10-20× slower.

use std::time::Instant;
use wwwvm_vm::{Stop, Vm, BOOT_LOAD_ADDR};

fn main() {
    // Guest:
    //   MOV CX, 0xFFFF      ; B9 FF FF
    //   XOR BX, BX          ; 31 DB
    // lp: ADD BX, CX        ; 01 CB
    //   LOOP lp             ; E2 FC
    //   HLT                 ; F4
    let program: &[u8] = &[0xB9, 0xFF, 0xFF, 0x31, 0xDB, 0x01, 0xCB, 0xE2, 0xFC, 0xF4];

    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();

    // Drive to completion in a single budgeted call so we measure pure
    // CPU work without the run_steps return-and-recall overhead.
    let budget = 1_000_000u32; // > 3 * 65535 + setup
    let t0 = Instant::now();
    let (steps, stop) = vm.run_steps(budget);
    let elapsed = t0.elapsed();

    let secs = elapsed.as_secs_f64();
    let ips = (steps as f64 / secs) as u64;
    let mips = ips as f64 / 1_000_000.0;

    let mode = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    // Throughput varies a lot by host arch — Rust codegen does
    // very different things with our dispatch chains on x86_64 vs
    // aarch64 — so print the target so a comparison across hosts
    // doesn't accidentally compare different builds.
    println!("build:           {mode} ({})", std::env::consts::ARCH);
    println!("instructions:    {steps}");
    println!("wall time:       {elapsed:.3?}");
    println!("throughput:      {ips} inst/sec ({mips:.2} MIPS)");
    println!("stop reason:     {stop:?}");

    // Sanity: a real run hit HLT and produced BX = sum_{n=1..=65535} n
    // = 65535 * 65536 / 2 = 0x80008000 — but BX is only 16 bits, so the
    // low word is 0x8000. Verify so a buggy CPU change can't game the
    // benchmark by, say, skipping every ADD.
    match stop {
        Stop::Halted => {
            let bx = vm.cpu().regs[wwwvm_cpu::r16::BX];
            assert_eq!(bx, 0x8000, "ALU loop sanity check failed");
            println!("sanity:          BX = 0x{bx:04X} (expected 0x8000) ✓");
        }
        other => panic!("expected Halted, got {other:?}"),
    }
}
