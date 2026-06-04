//! Repro/diagnostic for resume-after-restore of a full Linux guest.
//!
//! Boots Alpine, writes a marker into the (tmpfs) fs, takes a paged
//! `snapshot_export`, restores it into a fresh VM, and `cat`s the marker back.
//!
//! KNOWN LIMITATION (this currently FAILS, exit 1): the export/decode round-trip
//! is byte-exact (proven by `vm::paged` unit tests), and `restore_export`
//! reconstructs the identical snapshot blob — but a restored full Linux guest
//! does NOT faithfully resume: it busy-loops (first run_steps returns
//! StepBudget, not Halted) and never services console I/O (no output, no
//! response to input). Signature of device-timer/interrupt state not resuming
//! after restore (this kernel uses the LAPIC timer + IRQ routing; no tick → the
//! idle loop spins, no UART IRQ → input ignored). A pre-existing emulator-core
//! snapshot-FIDELITY gap, independent of the storage/transport layer — it also
//! affects the web Save/Load. Use this example to drive a fix; it PASSES when a
//! restored guest resumes and `cat /tmp/m` prints the marker.
//!
//! Prereqs:
//!   WWWVM_DUMP_INITRAMFS=/tmp/wwwvm-console.cpio WWWVM_ALPINE_MINIROOT=/tmp/alpine/root \
//!     cargo run --release --example alpine_console
//!   WWWVM_KERNEL=/tmp/wwwvm-alpine/vmlinuz-lts WWWVM_CONSOLE_CPIO=/tmp/wwwvm-console.cpio \
//!     cargo run --release --example snapshot_resume -p wwwvm-vm

use std::env;
use std::fs;

use wwwvm_vm::Vm;

const RAM: usize = 256 * 1024 * 1024;
const MARKER: &str = "SNAPMARK42";

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Step a VM until `marker` appears in its serial output, accumulating into
/// `out`. Returns true if seen within the round budget.
fn step_until(vm: &mut Vm, out: &mut Vec<u8>, marker: &[u8], rounds: u32) -> bool {
    for _ in 0..rounds {
        vm.run_steps_idle_aware(2_000_000);
        out.extend(vm.drain_output());
        if contains(out, marker) {
            return true;
        }
    }
    false
}

fn main() {
    let kernel_path =
        env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-alpine/vmlinuz-lts".into());
    let cpio_path =
        env::var("WWWVM_CONSOLE_CPIO").unwrap_or_else(|_| "/tmp/wwwvm-console.cpio".into());
    let kernel = fs::read(&kernel_path).unwrap_or_else(|e| panic!("read {kernel_path}: {e}"));
    let cpio = fs::read(&cpio_path).unwrap_or_else(|e| panic!("read {cpio_path}: {e}"));

    // --- boot, write a marker into the guest fs ---
    let mut vm = Vm::with_ram_size(RAM);
    let bz = vm.load_bzimage(&kernel).expect("load_bzimage");
    vm.set_kernel_cmdline("earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4");
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut out = Vec::new();
    if !step_until(&mut vm, &mut out, b"shell ready", 1500) {
        println!("=== RESULT: FAIL — guest never reached the shell ===");
        std::process::exit(1);
    }
    eprintln!("[snapshot_resume] shell ready — writing marker");
    vm.send_input(format!("echo {MARKER} > /tmp/m; sync; echo WROTE\n").as_bytes());
    if !step_until(&mut vm, &mut out, b"WROTE", 200) {
        println!("=== RESULT: FAIL — marker write didn't complete ===");
        std::process::exit(1);
    }
    // Let the shell settle back to an idle prompt before snapshotting.
    for _ in 0..10 {
        vm.run_steps_idle_aware(2_000_000);
        let _ = vm.drain_output();
    }

    // --- snapshot (paged export) → restore into a FRESH VM ---
    let export = vm.snapshot_export();
    eprintln!(
        "[snapshot_resume] exported {} bytes; restoring into a fresh VM",
        export.len()
    );
    drop(vm);
    let mut vm2 = Vm::with_ram_size(RAM);
    vm2.restore_export(&export).expect("restore_export");
    eprintln!(
        "[snapshot_resume] after restore: booted={} halted={}",
        vm2.is_booted(),
        vm2.is_halted()
    );

    // --- confirm the marker survived: cat it back ---
    let mut out2 = Vec::new();
    // First step tells us if the guest is wedged: a halted CPU with IF=0 returns
    // Stop::Halted immediately (executed=0) and never checks IRQs again.
    let (n, stop) = vm2.run_steps_idle_aware(2_000_000);
    eprintln!("[snapshot_resume] first step: executed={n} stop={stop:?}");
    out2.extend(vm2.drain_output());
    // Warm up: does the resumed guest emit anything on its own?
    for _ in 0..30 {
        vm2.run_steps_idle_aware(2_000_000);
        out2.extend(vm2.drain_output());
    }
    eprintln!("[snapshot_resume] warmup output: {} bytes", out2.len());
    // Kick with a newline — a live shell reprints its prompt.
    vm2.send_input(b"\n");
    for _ in 0..60 {
        vm2.run_steps_idle_aware(2_000_000);
        out2.extend(vm2.drain_output());
    }
    eprintln!(
        "[snapshot_resume] after newline: {} bytes total",
        out2.len()
    );
    // Now ask for the marker.
    vm2.send_input(b"cat /tmp/m\n");
    let seen = step_until(&mut vm2, &mut out2, MARKER.as_bytes(), 400);

    let text = String::from_utf8_lossy(&out2);
    println!("---- restored VM console (tail) ----");
    let start = text.len().saturating_sub(600);
    for line in text[start..].lines() {
        println!("R> {line}");
    }
    println!("------------------------------------");
    if seen {
        println!("\n=== RESULT: PASS — restored guest resumed and the marker survived ===");
    } else {
        println!(
            "\n=== RESULT: FAIL (known limitation) — the export round-trip is byte-exact \
             (see vm::paged tests), but the restored guest does not resume I/O: \
             timer/interrupt state isn't faithfully restored. ==="
        );
    }
    std::process::exit(if seen { 0 } else { 1 });
}
