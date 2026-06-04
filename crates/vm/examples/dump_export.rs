//! Dump a real `snapshot_export()` buffer to a file, so the browser-side codec
//! (`web/snapshot-store.js`) can be checked against the ACTUAL Rust wire format
//! (`vm::paged::encode_export`) — not just a hand-built synthetic buffer. Catches
//! Rust↔JS format drift that would otherwise only surface in a live browser.
//!
//!   WWWVM_EXPORT_OUT=/tmp/wwwvm-export.bin cargo run --release \
//!     --example dump_export -p wwwvm-vm
//! then parse /tmp/wwwvm-export.bin with web/snapshot-store.js (parseExport →
//! manifestOf + pageAt → buildExport must reproduce the file byte-for-byte).

use std::fs;

use wwwvm_vm::Vm;

fn main() {
    let out = std::env::var("WWWVM_EXPORT_OUT").unwrap_or_else(|_| "/tmp/wwwvm-export.bin".into());
    // Small RAM (256 KiB → 64 pages) so the file is tiny; write a pattern so the
    // pages aren't all-identical (exercises distinct hashes + a partial tail).
    let mut vm = Vm::with_ram_size(256 * 1024);
    let pattern: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 1) as u8).collect();
    vm.load_image(0x1000, &pattern);
    let buf = vm.snapshot_export();
    fs::write(&out, &buf).unwrap_or_else(|e| panic!("write {out}: {e}"));
    eprintln!("[dump_export] wrote {} bytes to {out}", buf.len());
}
