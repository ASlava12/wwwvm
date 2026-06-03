//! x86 CPU, real-mode subset.
//!
//! Scope: enough opcodes to execute the embedded boot-sector-style guest
//! payload end-to-end (mov imm, lodsb, or, jz, jmp, out, in, test) plus a
//! handful of common ones (hlt, nop, mov r8 imm, jmp rel16, jcc family).
//!
//! Not implemented yet (intentionally — see roadmap in README):
//!   * protected / long mode and paging
//!   * full ModR/M with SIB and disp32
//!   * arithmetic family (add/sub/adc/sbb/inc/dec) beyond what is tested
//!   * string ops other than LODSB
//!   * interrupts, IDT, exceptions
//!   * 32-bit and 64-bit operand/address sizes
//!
//! The fetch loop is a flat match on the first byte; ModR/M handling is
//! limited to mod=11 (register-to-register) because that is what the
//! current opcode set needs. Anything outside this scope returns a
//! [`CpuError::Unimplemented`] so callers see precisely what is missing
//! rather than executing nonsense.

#![forbid(unsafe_code)]

use crate::f80::F80;
use std::cell::Cell;
use std::cmp::Ordering;
use thiserror::Error;
use wwwvm_devices::IoBus;
use wwwvm_mem::Memory;

#[derive(Debug, Error)]
pub enum CpuError {
    #[error("unimplemented opcode 0x{opcode:02X} at {cs:04X}:{ip:08X}")]
    Unimplemented { opcode: u8, cs: u16, ip: u32 },
    #[error("unimplemented ModR/M mode {mode} (opcode 0x{opcode:02X} at {cs:04X}:{ip:08X})")]
    UnimplementedModRm {
        opcode: u8,
        mode: u8,
        cs: u16,
        ip: u32,
    },
}

/// Software 80-bit extended-precision float for the x87 stack. Standalone
/// for now (Phase 1); wired into the FPU in a follow-up.
pub mod f80;

/// Flags register bits we actually maintain.
pub mod flag {
    pub const CF: u16 = 1 << 0;
    pub const PF: u16 = 1 << 2;
    /// Auxiliary carry — carry/borrow out of bit 3. Used by the BCD
    /// adjust instructions (DAA/DAS/AAA/AAS) and exposed via LAHF/PUSHF.
    pub const AF: u16 = 1 << 4;
    pub const ZF: u16 = 1 << 6;
    pub const SF: u16 = 1 << 7;
    pub const IF: u16 = 1 << 9;
    pub const DF: u16 = 1 << 10;
    pub const OF: u16 = 1 << 11;
}

/// Indices into [`Cpu::regs`] matching standard x86 r16 encoding.
pub mod r16 {
    pub const AX: usize = 0;
    pub const CX: usize = 1;
    pub const DX: usize = 2;
    pub const BX: usize = 3;
    pub const SP: usize = 4;
    pub const BP: usize = 5;
    pub const SI: usize = 6;
    pub const DI: usize = 7;
}

/// Indices into [`Cpu::sregs`] matching standard x86 sreg encoding.
pub mod sreg {
    pub const ES: usize = 0;
    pub const CS: usize = 1;
    pub const SS: usize = 2;
    pub const DS: usize = 3;
    pub const FS: usize = 4;
    pub const GS: usize = 5;
}

/// Signature of an installed BIOS shim — see [`Cpu::bios_hook`].
pub type BiosHook = fn(&mut Cpu, &mut Memory, &mut IoBus, u8) -> bool;

pub struct Cpu {
    /// General-purpose register file — AX..DI as the low 16 bits of
    /// E?X. Indexed by the standard r16 encoding.
    pub regs: [u16; 8],
    /// Upper 16 bits of E?X — populated only by 32-bit-operand
    /// instructions. In real mode and for 8086/186-only guests this
    /// stays zero. Kept as a separate array (rather than widening
    /// `regs` to u32) so the existing thousand+ call sites that
    /// operate on 16-bit values compile unchanged.
    pub regs_high: [u16; 8],
    pub sregs: [u16; 6],
    /// Instruction pointer. 32-bit so we can reach kernel addresses
    /// above 0xFFFF. Snapshot still saves only the low 16 bits for
    /// backward compatibility — that's a known limitation until a
    /// snapshot v6 lands.
    pub ip: u32,
    pub flags: u16,
    /// High 16 bits of EFLAGS (bits 16-31). Bits we don't model
    /// (AC=18, ID=21, VIP=20, VIF=19, VM=17, RF=16) still need to
    /// round-trip through PUSHFD/POPFD for Linux's 32-bit i486-vs-
    /// i386 detect, which flips AC and checks whether it sticks.
    /// We don't act on these bits, just preserve them.
    pub flags_high: u16,
    pub halted: bool,
    /// Active segment-override prefix for the current instruction.
    /// Reset at the top of each `step()` and set when we consume a
    /// `0x26`/`0x2E`/`0x36`/`0x3E` prefix byte. Reads through
    /// `compute_ea` and string-op source addresses honor it.
    pub(crate) seg_override: Option<usize>,
    /// Operand-size override for the current instruction. 0x66
    /// prefix flips the default size — in real mode default is 16,
    /// so this means "32-bit operand" while set. Reset at the top
    /// of each `step()` just like `seg_override`.
    pub(crate) op_size_32: bool,
    /// Address-size override for the current instruction. 0x67
    /// prefix flips the default address size. With default 16-bit
    /// addressing this means "32-bit address mode" — full r32
    /// registers in ModR/M, optional SIB byte, disp32 instead of
    /// disp16.
    pub(crate) addr_size_32: bool,
    /// Stack-size attribute. On real x86 this comes from the SS
    /// descriptor's D/B bit; a 32-bit kernel stack sets it to true
    /// so SP becomes the full 32-bit ESP. Default false (matches the
    /// 16-bit stack used in real mode and early PM). Public so an
    /// OS-bootstrap test can flip it after building an SS descriptor.
    pub stack_size_32: bool,
    /// Default operand/address size for code fetched through the
    /// current CS. Set from the CS descriptor's D bit at PM segment
    /// load time. With `code_size_32 = true` every fetch defaults to
    /// 32-bit operand/address sizes, and the 0x66 / 0x67 prefixes
    /// flip them back to 16-bit (rather than the other way round in
    /// real mode). This is how a 32-bit kernel runs without
    /// 0x66-stuffing every immediate.
    pub code_size_32: bool,
    /// Control Register 0. On real x86 it's 32 bits; we store the
    /// full width but only bit 0 (PE — Protection Enable) and bit 31
    /// (PG — Paging) will gain semantic meaning once those modes
    /// are implemented. Real-mode code can already read/write it via
    /// `MOV CR0, r` / `MOV r, CR0` (0x0F 0x22 / 0x0F 0x20).
    pub cr0: u32,
    /// GDT pseudo-descriptor: 16-bit limit + 32-bit base. Loaded by
    /// `LGDT` (0x0F 0x01 /2). Consulted by `write_sreg` in PM to
    /// fetch the 8-byte segment descriptor that populates the
    /// matching `seg_cache` entry.
    pub gdtr: DescriptorTable,
    /// IDT pseudo-descriptor — loaded by `LIDT` (0x0F 0x01 /3). In
    /// real mode the IDT is fixed at linear 0 with 4-byte entries;
    /// once we honor PM-style interrupt gates we'll consult this.
    pub idtr: DescriptorTable,
    /// Control Register 3 — physical base of the page directory.
    /// Bits 11..0 hold attributes (PWT/PCD on real x86, ignored here);
    /// bits 31..12 are the 4 KiB-aligned PD base. Active only when
    /// `cr0 & 0x8000_0000` (PG). Loaded via `MOV CR3, r32` (0x0F 0x22
    /// /3) and read via `MOV r32, CR3` (0x0F 0x20 /3).
    pub cr3: u32,
    /// Control Register 2 — written by the CPU on a page fault to
    /// the linear address that triggered it. Software (the #PF
    /// handler) reads it via `MOV r32, CR2` to figure out which
    /// address to fix up. We don't model the MOV opcode yet — it
    /// will be added when a guest needs it.
    pub cr2: u32,
    /// Control Register 4 — feature-enable bits added past i486 (VME,
    /// PSE, PAE, PGE, OSFXSR, etc.). Only CR4.PSE (bit 4) has semantic
    /// meaning here; the rest are stored so a kernel can read/write
    /// them via `MOV CR4, r32` / `MOV r32, CR4` without faulting.
    pub cr4: u32,
    /// Debug registers DR0..DR7. DR0..3 are linear breakpoint
    /// addresses, DR6 latches debug-event status, DR7 controls
    /// per-breakpoint enable/type/length. Linux clears DR6+DR7 on
    /// every context switch and reads them on `ptrace(PTRACE_PEEKUSR)`.
    /// We don't actually trigger #DB on hits — these are stub-only
    /// state so MOV r,DRn and MOV DRn,r round-trip cleanly instead of
    /// faulting #UD. DR4/DR5 are RAZ/WI aliases of DR6/DR7 on real
    /// CPUs; we expose them as independent slots and leave the
    /// aliasing for a future tick if anything needs it. Not snapshotted —
    /// a restore acts like a context switch (kernel reclears them).
    pub dr: [u32; 8],
    /// Time-stamp counter. Incremented once per `step()`. Read via
    /// RDTSC (0x0F 0x31). Linux uses TSC for delay calibration —
    /// returning a monotonically advancing counter is what matters,
    /// not the cycle-accurate semantics.
    pub tsc: u64,
    /// LDT selector — stored value, used only by SLDT (and a future
    /// LDT-based descriptor lookup). LLDT writes this; SLDT reads it.
    pub ldtr: u16,
    /// Task Register selector. Same shape: LTR sets, STR reads.
    /// We don't yet walk the TSS for ring transitions.
    pub tr: u16,
    /// FPU status word. Bits 0..5 = exception flags, 6 = SF, 7 = ES,
    /// 8..10 = C0..C2, 11..13 = TOP, 14 = C3, 15 = busy. We only
    /// track the value for FNSTSW; no actual FPU arithmetic yet.
    pub fpu_sw: u16,
    /// FPU control word. Default 0x037F after FNINIT.
    pub fpu_cw: u16,
    /// x87 register file, modelled as f64 (not bit-exact for the
    /// hardware's 80-bit extended format, but enough for the
    /// load/store/arith the kernel and glibc actually do). The stack
    /// top is `fpu_top`; ST(i) is `fpu_st[(fpu_top + i) & 7]`.
    pub fpu_st: [F80; 8],
    /// x87 stack-top index (the architectural TOP field, 0..7).
    pub fpu_top: u8,
    /// SSE register file: XMM0..XMM7, each 128 bits. Modelled as u128
    /// so data-movement (MOVD/MOVQ/MOVDQA) is exact; packed-arith ops
    /// reinterpret the lanes as needed.
    pub xmm: [u128; 8],
    /// MMX register file: MM0..MM7, each 64 bits. Architecturally these
    /// alias the x87 mantissas, but Alpine's MMX routines are self-contained
    /// (EMMS before any x87), so a separate array is exact for them and far
    /// simpler. Used by the integer-SIMD code in the guest's libcrypto/zlib.
    pub mmx: [u64; 8],
    /// SYSENTER MSRs (IA32_SYSENTER_CS/ESP/EIP = 0x174/0x175/0x176).
    /// SYSENTER loads CS:EIP and SS:ESP from these; SYSEXIT derives
    /// the return selectors from `sysenter_cs`. Written via WRMSR.
    pub sysenter_cs: u32,
    pub sysenter_esp: u32,
    pub sysenter_eip: u32,
    /// IA32_MISC_ENABLE (MSR 0x1A0). Linux's
    /// arch/x86/kernel/cpu/intel.c reads this very early to learn
    /// what the BIOS/microcode pre-enabled (FAST_STRING, NX-bit
    /// disable, PEBS disable, BIOS unlock, ...). The kernel only
    /// reads the bits it cares about; we just store the whole
    /// 64-bit value so writes round-trip. Not snapshotted yet —
    /// guests can re-write this on the next boot.
    pub misc_enable: u64,
    /// IA32_TSC_AUX (MSR 0xC0000103). The kernel writes the CPU
    /// number here on each AP bring-up so the vDSO's `vget_cpu()`
    /// can identify which CPU answered an RDTSCP without trapping
    /// into the kernel. RDTSCP returns this value in ECX. Stored
    /// only; we don't model multiple CPUs.
    pub tsc_aux: u32,
    /// IA32_EFER (MSR 0xC0000080). Bits we'd care about if we
    /// modelled them: SCE (bit 0 — syscall enable), NXE (bit 11 —
    /// no-execute), LME (bit 8 — long-mode enable). We don't model
    /// any of these — the field round-trips so the kernel's
    /// `setup_efer` doesn't oops on its writeback verification.
    /// Not snapshotted past Cpu::new() defaults; the kernel
    /// reconfigures on next boot.
    pub efer: u64,
    /// Set by `translate()` when a page walk hits a non-present
    /// entry. Read at the end of each `step()`; if set, the CPU
    /// dispatches INT 14 with the error code pushed on the stack,
    /// sets CR2 to the faulting address, and clears the slot.
    /// `Cell` so translate can flag a fault through `&self`.
    pending_fault: Cell<Option<PageFault>>,
    /// One-entry instruction-fetch translation cache (mini-TLB).
    /// Stores `(linear_page, phys_frame)` for the most recent
    /// successful `translate_fetch`. Sequential opcode fetches —
    /// the dominant memory-access pattern in a typical instruction
    /// stream — hit the cache and skip the PDE + PTE walks. Real
    /// silicon caches this in the L1 ITLB; we just cache one slot
    /// since sequential fetches mostly stay on the same page.
    /// (We measured a 2-entry direct-mapped variant on the Linux
    /// boot test: dead flat, no win — confirmed empirically that
    /// the hot fetch path doesn't bounce across page boundaries
    /// often enough to matter, so the 1-slot version stays.
    /// See `134ad50` for the null-result diff if a future
    /// contributor wants to re-attempt.)
    /// Cleared on CR3 reload, INVLPG, any CR0 write (covers PG
    /// toggle for fetch invariants, and CR0.WP toggle for the
    /// `write_tlb` permission invariants — the dispatch funnels
    /// all CR0 writes through the same invalidator), write_sreg(CS)
    /// (CPL transition may change U/S semantics), and CPU reset.
    /// The tuple is `(linear_page, phys_frame_after_a20, a20_state)`
    /// — a20 state is carried so a direct `cpu.a20 = …` poke (as
    /// the unit tests do) is self-invalidating: the lookup just
    /// misses on the next translate when a20 changed under us.
    fetch_tlb: Cell<Option<(u32, u32, bool)>>,
    /// Sibling 1-entry data-read translation cache. Covers
    /// `translate` (read access). Sequential stack pushes, struct
    /// field loads, REP MOVSD source/dest — these all hit the
    /// same page for many bytes in a row, so caching the most
    /// recent (page, frame, a20) tuple lets us skip the PDE+PTE
    /// walk on each follow-up byte.
    /// Invalidations match `fetch_tlb`.
    read_tlb: Cell<Option<(u32, u32, bool)>>,
    /// 1-entry data-write translation cache. Covers
    /// `translate_write`. Stack pushes, struct field stores, REP
    /// STOSD destination — these all keep writing to the same
    /// page. Write *permission* (CR0.WP + effective R/W + CPL=3
    /// U/S check) varies with state but all of those are
    /// invalidated when they can change: CR3 reload (PDE/PTE
    /// table swap), INVLPG (per-page R/W change), CR0 write
    /// (CR0.WP toggle), and write_sreg(CS) (CPL transition).
    /// As long as those four sites stay in `invalidate_fetch_tlb`,
    /// caching a successful write translation is sound — the
    /// permission check passed at cache time and nothing that
    /// could change permissions has fired since.
    write_tlb: Cell<Option<(u32, u32, bool)>>,
    /// IP of the instruction currently being decoded — captured at
    /// the top of every `step()` so that a #PF raised mid-decode
    /// can rewind `self.ip` to the faulting instruction before
    /// dispatching. Without this, IRETD from the #PF handler lands
    /// on the instruction *after* the one that faulted and demand
    /// paging silently breaks: the kernel can't retry the load.
    pub(crate) last_op_ip: u32,
    /// State of the A20 address line. On real hardware A20 starts
    /// gated *off* at reset — addresses with bit 20 set wrap into
    /// the low 1 MiB, the 8086 compatibility quirk. Modern BIOSes
    /// enable it before handing off, so we default to `true` to
    /// match the typical post-BIOS state. Toggle via port 0x92
    /// bit 1 (the "fast A20" gate).
    pub a20: bool,
    /// Optional intercept for software interrupts. When `INT imm8`
    /// fires, the CPU calls this with (cpu, mem, vector). If the hook
    /// returns `true`, the dispatch is skipped — the host already did
    /// the BIOS work directly in Rust. Returning `false` lets the
    /// normal IVT/IDT path run, so a guest that installs its own
    /// handler for the same vector still wins (it overrides the IVT
    /// entry, which we'd then consult).
    ///
    /// Stored as a bare `fn` pointer (not `Box<dyn>`) so the Cpu
    /// stays `Copy`-friendly and snapshot-able without extra plumbing.
    pub bios_hook: Option<BiosHook>,
    /// Shadow descriptor cache for each segment register. The CPU
    /// addresses memory through `seg_cache[idx].base`, *not*
    /// `sregs[idx] << 4`, so once PM is on, the visible selector
    /// and the active translation base diverge — same as real x86.
    pub seg_cache: [SegmentCache; 6],
    /// Optional debug instruction-trace ring. `None` (zero overhead)
    /// unless `enable_pf_trace` is called. When `Some`, records
    /// `(eip, eax, ebx, ecx)` at the start of every instruction and is
    /// dumped once by `raise_fault` on the first user-mode read fault
    /// of a low address — the investigation hook for the ld.so
    /// null-deref. `RefCell` so the `&self` `raise_fault` can dump it.
    pf_trace: core::cell::RefCell<Option<PfTrace>>,
    /// Cheap mirror of `pf_trace.borrow().is_some()` — a plain `Cell<bool>`
    /// checked once per step on the hot path instead of a RefCell borrow (the
    /// trace is off in every non-debug run). Set by `enable_pf_trace`.
    pf_trace_on: core::cell::Cell<bool>,
}

/// Debug instruction-trace ring buffer (see `Cpu::pf_trace`).
struct PfTrace {
    ring: Vec<(u32, u32, u32, u32)>,
    head: usize,
    count: usize,
    fired: bool,
}

impl PfTrace {
    fn new(cap: usize) -> Self {
        Self {
            ring: vec![(0, 0, 0, 0); cap],
            head: 0,
            count: 0,
            fired: false,
        }
    }
    fn record(&mut self, entry: (u32, u32, u32, u32)) {
        let cap = self.ring.len();
        self.ring[self.head] = entry;
        self.head = (self.head + 1) % cap;
        self.count += 1;
    }
    /// Print the ring (oldest first) with a header. Fields are
    /// (eip, eax, ebx, ebp) — see the recording site in `step`.
    fn dump(&self, header: &str) {
        let cap = self.ring.len();
        let n = self.count.min(cap);
        let start = if self.count <= cap { 0 } else { self.head };
        eprintln!("=== PF-TRACE: {header}; last {n} instrs (oldest first) ===");
        for k in 0..n {
            let (eip, eax, ebx, ebp) = self.ring[(start + k) % cap];
            eprintln!("  {k:4} eip={eip:#010x} eax={eax:#010x} ebx={ebx:#010x} ebp={ebp:#010x}");
        }
    }
}

/// Page-fault payload built by `translate()`. The `error_code` follows
/// the i386 #PF format documented in the Intel SDM:
///   * bit 0 — P    (0 = not present, 1 = protection violation)
///   * bit 1 — W/R  (1 = write attempt, 0 = read)
///   * bit 2 — U/S  (1 = user mode, 0 = supervisor)
///
/// Bits 3+ stay zero until we model reserved-bit / instruction-fetch
/// distinctions. `addr` is the linear address that triggered the
/// fault — it'll be latched into CR2 when the exception is taken.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PageFault {
    pub addr: u32,
    pub error_code: u32,
}

/// 6-byte pseudo-descriptor loaded by LGDT/LIDT: 16-bit limit
/// followed by 32-bit base.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DescriptorTable {
    pub limit: u16,
    pub base: u32,
}

/// "Hidden" portion of a segment register — loaded from a GDT/LDT
/// descriptor on every selector write in protected mode, and from
/// `selector << 4` in real mode. Address translation reads `base`
/// directly from here, which is why a snapshot of the selector
/// alone doesn't capture the active translation once we're in PM.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentCache {
    pub base: u32,
    pub limit: u32,
    /// Access-rights byte from the descriptor (P|DPL|S|type). In
    /// real mode we synthesize 0x93 (present, ring 0, data, R/W).
    pub access: u8,
}

/// Decoded effective address: linear = seg_cache[seg].base + off.
/// `off` is 32-bit so the same struct represents both 16-bit and
/// 32-bit addressing modes. In 16-bit mode the upper half is zero;
/// in 32-bit mode (0x67 prefix or in a 32-bit code segment) it
/// carries the full computed displacement.
#[derive(Copy, Clone, Debug)]
pub struct EffAddr {
    pub seg: usize,
    pub off: u32,
}

/// Either side of a ModR/M operand: register index or memory address.
#[derive(Copy, Clone, Debug)]
pub enum Rm {
    Reg(u8),
    Mem(EffAddr),
}

/// Access type fed to the page walker. The variant drives the W
/// and I/D bits in the #PF error code so the handler can tell
/// a fetch from a read from a write — Linux's `do_page_fault`
/// branches on these bits to decide what to map (executable vs
/// data) and whether to grant write permission.
#[derive(Copy, Clone, Debug)]
enum Access {
    Read,
    Write,
    Fetch,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

// --- Packed-integer lane helpers (shared by MMX 64-bit and, in principle,
// SSE forms). `lane` is the element width in bytes (1/2/4/8). ---

/// Apply `f` to each `lane`-byte element of `a` and `b` independently.
fn packed_map(a: u64, b: u64, lane: usize, f: impl Fn(u64, u64) -> u64) -> u64 {
    let bits = lane * 8;
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let mut out = 0u64;
    let mut sh = 0;
    while sh < 64 {
        let av = (a >> sh) & mask;
        let bv = (b >> sh) & mask;
        out |= (f(av, bv) & mask) << sh;
        sh += bits;
    }
    out
}

fn mmx_padd(a: u64, b: u64, lane: usize) -> u64 {
    packed_map(a, b, lane, |x, y| x.wrapping_add(y))
}

fn mmx_psub(a: u64, b: u64, lane: usize) -> u64 {
    packed_map(a, b, lane, |x, y| x.wrapping_sub(y))
}

fn mmx_pcmpeq(a: u64, b: u64, lane: usize) -> u64 {
    packed_map(a, b, lane, |x, y| if x == y { u64::MAX } else { 0 })
}

/// Packed logical right shift, per lane (count from a register/imm). A count
/// at or beyond the lane width zeroes the lane.
fn mmx_psrl(a: u64, count: u64, lane: usize) -> u64 {
    let bits = (lane * 8) as u64;
    packed_map(
        a,
        0,
        lane,
        move |x, _| if count >= bits { 0 } else { x >> count },
    )
}

/// Packed logical left shift, per lane (out-of-lane bits dropped by the mask).
fn mmx_psll(a: u64, count: u64, lane: usize) -> u64 {
    let bits = (lane * 8) as u64;
    packed_map(
        a,
        0,
        lane,
        move |x, _| if count >= bits { 0 } else { x << count },
    )
}

/// Packed arithmetic (signed) right shift, per lane. A count at/beyond the
/// lane width saturates to a full sign fill.
fn mmx_psra(a: u64, count: u64, lane: usize) -> u64 {
    let bits = (lane * 8) as u32;
    let c = if count >= bits as u64 {
        bits - 1
    } else {
        count as u32
    };
    packed_map(a, 0, lane, move |x, _| {
        let sx = ((x << (64 - bits)) as i64) >> (64 - bits); // sign-extend the lane
        (sx >> c) as u64
    })
}

/// MMX PUNPCKL/H: interleave `elem_bytes`-sized elements from the low
/// (`high=false`) or high (`high=true`) 32 bits of `d` and `s`, dst first.
fn mmx_punpck(d: u64, s: u64, elem_bytes: u64, high: bool) -> u64 {
    let bits = elem_bytes * 8;
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let base = if high { 32 } else { 0 };
    let n = 32 / bits; // elements taken from each operand's selected half
    let mut out = 0u64;
    for i in 0..n {
        let de = (d >> (base + i * bits)) & mask;
        let se = (s >> (base + i * bits)) & mask;
        out |= de << (2 * i * bits);
        out |= se << ((2 * i + 1) * bits);
    }
    out
}

fn sat_s8(v: i32) -> u64 {
    (v.clamp(-128, 127) as i8 as u8) as u64
}
fn sat_u8(v: i32) -> u64 {
    (v.clamp(0, 255) as u8) as u64
}
fn sat_s16(v: i32) -> u64 {
    (v.clamp(-32768, 32767) as i16 as u16) as u64
}

/// MMX PACKSSWB / PACKUSWB: saturate the 4 signed words of `d` then `s` to
/// bytes (`signed` picks signed vs unsigned saturation) → 8 bytes.
fn mmx_packwb(d: u64, s: u64, signed: bool) -> u64 {
    let sat = |w: i32| if signed { sat_s8(w) } else { sat_u8(w) };
    let mut out = 0u64;
    for (half, src) in [d, s].into_iter().enumerate() {
        for i in 0..4u64 {
            let w = ((src >> (i * 16)) & 0xFFFF) as u16 as i16 as i32;
            out |= sat(w) << ((half as u64 * 4 + i) * 8);
        }
    }
    out
}

/// MMX PACKSSDW: signed-saturate the 2 dwords of `d` then `s` to words → 4 words.
fn mmx_packssdw(d: u64, s: u64) -> u64 {
    let mut out = 0u64;
    for (half, src) in [d, s].into_iter().enumerate() {
        for i in 0..2u64 {
            let dw = ((src >> (i * 32)) & 0xFFFF_FFFF) as u32 as i32;
            out |= sat_s16(dw) << ((half as u64 * 2 + i) * 16);
        }
    }
    out
}

/// PSHUFW: select each of the 4 result words from the source per a 2-bit
/// field of `imm` (bits [2i+1:2i] pick the source word for result word i).
fn mmx_pshufw(src: u64, imm: u8) -> u64 {
    let mut out = 0u64;
    for i in 0..4 {
        let sel = (imm >> (2 * i)) & 3;
        let w = (src >> (sel as u64 * 16)) & 0xFFFF;
        out |= w << (i as u64 * 16);
    }
    out
}

/// PMADDWD: signed 16-bit lanes multiplied, adjacent pairs summed into
/// 32-bit results (4 words → 2 dwords for MMX).
fn mmx_pmaddwd(a: u64, b: u64) -> u64 {
    let mut out = 0u64;
    for pair in 0..2 {
        let sh = pair * 32;
        let a0 = ((a >> sh) & 0xFFFF) as i16 as i32;
        let a1 = ((a >> (sh + 16)) & 0xFFFF) as i16 as i32;
        let b0 = ((b >> sh) & 0xFFFF) as i16 as i32;
        let b1 = ((b >> (sh + 16)) & 0xFFFF) as i16 as i32;
        let dword = (a0 * b0).wrapping_add(a1 * b1) as u32 as u64;
        out |= dword << sh;
    }
    out
}

/// Signed greater-than, per lane → all-ones / zero. `bits` from `lane`.
fn mmx_pcmpgt(a: u64, b: u64, lane: usize) -> u64 {
    let bits = (lane * 8) as u32;
    packed_map(a, b, lane, |x, y| {
        // Sign-extend the lane to i64 before comparing.
        let sx = ((x << (64 - bits)) as i64) >> (64 - bits);
        let sy = ((y << (64 - bits)) as i64) >> (64 - bits);
        if sx > sy {
            u64::MAX
        } else {
            0
        }
    })
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: [0; 8],
            regs_high: [0; 8],
            sregs: [0; 6],
            ip: 0,
            flags: 0,
            flags_high: 0,
            halted: false,
            seg_override: None,
            op_size_32: false,
            addr_size_32: false,
            stack_size_32: false,
            code_size_32: false,
            cr0: 0,
            gdtr: DescriptorTable::default(),
            idtr: DescriptorTable::default(),
            cr3: 0,
            cr2: 0,
            cr4: 0,
            dr: [0; 8],
            tsc: 0,
            ldtr: 0,
            tr: 0,
            fpu_sw: 0,
            fpu_cw: 0x037F,
            fpu_st: [F80::ZERO; 8],
            fpu_top: 0,
            xmm: [0; 8],
            mmx: [0; 8],
            sysenter_cs: 0,
            sysenter_esp: 0,
            sysenter_eip: 0,
            misc_enable: 0,
            tsc_aux: 0,
            efer: 0,
            pending_fault: Cell::new(None),
            fetch_tlb: Cell::new(None),
            read_tlb: Cell::new(None),
            write_tlb: Cell::new(None),
            // (the TLB slots are also explicitly cleared by
            // reset_to_boot — see `Cpu::reset_to_boot`).
            last_op_ip: 0,
            a20: true,
            bios_hook: None,
            seg_cache: [SegmentCache::default(); 6],
            pf_trace: core::cell::RefCell::new(None),
            pf_trace_on: core::cell::Cell::new(false),
        }
    }

    /// Enable the debug instruction-trace ring (see `pf_trace`). The
    /// ring records the last `cap` instructions; on the first
    /// user-mode read #PF of a low address it is dumped to stderr.
    /// For investigating the ld.so null-deref; off by default.
    pub fn enable_pf_trace(&self, cap: usize) {
        *self.pf_trace.borrow_mut() = Some(PfTrace::new(cap));
        self.pf_trace_on.set(true);
    }

    /// Dump the instruction-trace ring on demand (no-op if not enabled).
    /// Lets a harness dump the last `cap` instructions at a known failure
    /// point (e.g. when the UART shows a kernel panic) rather than relying
    /// on a fault trigger.
    pub fn dump_pf_trace(&self, header: &str) {
        if let Some(t) = self.pf_trace.borrow().as_ref() {
            t.dump(header);
        }
    }

    /// Read the CPU's segment-override prefix. Exposed so the VM
    /// snapshot helper can persist transient state without crates
    /// having to make the field itself public.
    pub fn seg_override(&self) -> Option<usize> {
        self.seg_override
    }

    /// Counterpart to `seg_override()`. Used only by snapshot restore.
    pub fn set_seg_override(&mut self, value: Option<usize>) {
        self.seg_override = value;
    }

    /// Reset to a sensible boot state: CS:IP = 0000:7C00 (where BIOS
    /// loads the first sector), stack at the bottom of conventional
    /// memory, all data segments = 0.
    pub fn reset_to_boot(&mut self) {
        self.regs = [0; 8];
        self.regs_high = [0; 8];
        self.sregs = [0; 6];
        self.regs[r16::SP] = 0x7C00;
        self.ip = 0x7C00;
        self.flags = 0;
        self.halted = false;
        self.seg_override = None;
        self.cr0 = 0;
        self.gdtr = DescriptorTable::default();
        self.idtr = DescriptorTable::default();
        self.cr3 = 0;
        self.cr2 = 0;
        self.cr4 = 0;
        self.dr = [0; 8];
        self.tsc = 0;
        self.ldtr = 0;
        self.tr = 0;
        self.fpu_sw = 0;
        self.fpu_cw = 0x037F;
        self.fpu_st = [F80::ZERO; 8];
        self.fpu_top = 0;
        self.xmm = [0; 8];
        self.sysenter_cs = 0;
        self.sysenter_esp = 0;
        self.sysenter_eip = 0;
        self.misc_enable = 0;
        self.tsc_aux = 0;
        self.efer = 0;
        self.pending_fault.set(None);
        self.fetch_tlb.set(None);
        self.read_tlb.set(None);
        self.write_tlb.set(None);
        self.last_op_ip = 0;
        self.a20 = true;
        self.code_size_32 = false;
        self.stack_size_32 = false;
        // Real-mode default: every cache mirrors `sregs[i] << 4`.
        // Since sregs reset to 0, base is 0 for everything.
        self.seg_cache = [SegmentCache {
            base: 0,
            limit: 0xFFFF,
            access: 0x93,
        }; 6];
    }

    pub fn read_r8(&self, i: u8) -> u8 {
        let idx = (i & 3) as usize;
        let high = i >= 4;
        let word = self.regs[idx];
        if high {
            (word >> 8) as u8
        } else {
            word as u8
        }
    }

    pub fn write_r8(&mut self, i: u8, value: u8) {
        let idx = (i & 3) as usize;
        let high = i >= 4;
        let word = self.regs[idx];
        self.regs[idx] = if high {
            (word & 0x00FF) | ((value as u16) << 8)
        } else {
            (word & 0xFF00) | value as u16
        };
    }

    pub fn read_r16(&self, i: u8) -> u16 {
        self.regs[(i & 7) as usize]
    }

    pub fn write_r16(&mut self, i: u8, value: u16) {
        self.regs[(i & 7) as usize] = value;
    }

    /// Read the full 32-bit register. Splices the upper 16 bits from
    /// `regs_high` onto the low 16 from `regs`.
    pub fn read_r32(&self, i: u8) -> u32 {
        let idx = (i & 7) as usize;
        ((self.regs_high[idx] as u32) << 16) | self.regs[idx] as u32
    }

    /// Write the full 32-bit register, splitting into `regs` (low)
    /// and `regs_high` (high). Mirrors x86-64 zero-extension: a
    /// 32-bit write to a register zeros nothing visible because it
    /// covers the whole logical EAX.
    pub fn write_r32(&mut self, i: u8, value: u32) {
        let idx = (i & 7) as usize;
        self.regs[idx] = value as u16;
        self.regs_high[idx] = (value >> 16) as u16;
    }

    /// Write a segment register *and* refresh its hidden descriptor
    /// cache. In real mode the cache is `value << 4`. In protected
    /// mode the selector is split into RPL/TI/index, the 8-byte
    /// descriptor at `gdtr.base + index*8` is read, and its base,
    /// limit (with granularity expanded), and access byte populate
    /// the cache.
    ///
    /// We bypass protection / NULL-selector checks for now — the
    /// goal of this step is just to wire the cache. Limit
    /// violations and #GP faults arrive in a later iteration.
    /// In PE mode, return true if `selector` indexes past the GDT
    /// limit (the LDT case is not modeled — TI bit is ignored). In
    /// real mode every selector is valid; the cache simply derives
    /// `base = sel << 4`. Used by the segment-load helpers to raise
    /// #GP(selector) instead of decoding garbage as a descriptor.
    pub fn selector_out_of_gdt(&self, selector: u16) -> bool {
        if self.cr0 & 1 == 0 {
            return false;
        }
        let index = (selector & 0xFFF8) as u32;
        // The 8-byte descriptor at `index..index+7` must lie within
        // the table; GDTR.limit is the largest valid byte offset.
        index + 7 > self.gdtr.limit as u32
    }

    /// Raise #DE (vector 0) for a divide error — division by zero
    /// or quotient overflow on DIV/IDIV, or AAM with base = 0. Real
    /// silicon vectors through IDT[0]; previously we returned a
    /// host-visible `CpuError`. Rewinding IP to `op_ip` makes the
    /// pushed fault frame name the offending instruction.
    fn raise_de(&mut self, op_ip: u32, mem: &mut Memory) {
        self.ip = op_ip;
        self.do_interrupt(0, mem);
    }

    /// If `selector` is out of GDT bounds in PE mode, raise #GP
    /// (vector 13) with the selector as the error code (Intel
    /// shape: RPL bits cleared, TI bit and index pass through),
    /// rewind IP to `op_ip` so the fault frame names the offending
    /// instruction, and return true — the caller should immediately
    /// `return Ok(())`. False means the selector loaded cleanly and
    /// the caller can continue.
    fn raise_gp_if_bad_selector(&mut self, selector: u16, op_ip: u32, mem: &mut Memory) -> bool {
        if self.selector_out_of_gdt(selector) {
            self.ip = op_ip;
            self.do_interrupt_with_error(13, Some((selector & 0xFFFC) as u32), mem);
            true
        } else {
            false
        }
    }

    /// Software-INT gate-DPL check. When a guest executes INT3 /
    /// INT N / INTO from CPL > gate.DPL, real silicon raises
    /// #GP((vector*8)|2) instead of dispatching — that's how the
    /// kernel keeps internal IDT entries (e.g. page-fault, double
    /// fault) inaccessible to userspace while leaving the syscall
    /// gate (DPL=3) open. Returns true after firing; the caller
    /// should `return Ok(())`.
    ///
    /// The error-code shape is the Intel format:
    ///   bit 0 EXT (0 for software interrupt — not external)
    ///   bit 1 IDT (1 — the selector lives in the IDT)
    ///   bit 2 TI  (ignored when IDT=1)
    ///   bits 3..15 index = vector
    fn raise_gp_if_gate_too_privileged(
        &mut self,
        vector: u8,
        op_ip: u32,
        mem: &mut Memory,
    ) -> bool {
        if self.cr0 & 1 == 0 {
            return false;
        }
        let gate_addr = self.idtr.base.wrapping_add((vector as u32) * 8);
        let access = self.mem_read_u8(mem, gate_addr.wrapping_add(5));
        let gate_dpl = (access >> 5) & 3;
        let cpl = (self.sregs[sreg::CS] & 3) as u8;
        if cpl > gate_dpl {
            self.ip = op_ip;
            let ec = ((vector as u32) << 3) | 2;
            self.do_interrupt_with_error(13, Some(ec), mem);
            true
        } else {
            false
        }
    }

    /// Strict CPL=0 gate. PE mode + any non-zero CPL on the active
    /// CS raises #GP(0) and returns true; CPL=0 (or real mode)
    /// returns false. Used by LLDT/LTR/LGDT/LIDT/LMSW/INVLPG and
    /// the cache-flush opcodes (INVD/WBINVD), all of which the
    /// Intel SDM classifies as "supervisor-only" with the same
    /// fault shape as HLT.
    fn raise_gp_if_user(&mut self, op_ip: u32, mem: &mut Memory) -> bool {
        if self.cr0 & 1 == 0 {
            return false;
        }
        let cpl = (self.sregs[sreg::CS] & 3) as u8;
        if cpl > 0 {
            self.ip = op_ip;
            self.do_interrupt_with_error(13, Some(0), mem);
            true
        } else {
            false
        }
    }

    /// If we're in PE mode and the current CPL exceeds the IOPL
    /// field in EFLAGS, raise #GP(0) from `op_ip` and return true —
    /// the caller should immediately `return Ok(())`. IOPL gates
    /// the IF-modifying and port-IO instructions (CLI/STI/IN/OUT);
    /// most kernels run user processes with IOPL=0 so any of these
    /// from CPL=3 must trap.
    fn raise_gp_if_below_iopl(&mut self, op_ip: u32, mem: &mut Memory) -> bool {
        if self.cr0 & 1 == 0 {
            return false;
        }
        let cpl = (self.sregs[sreg::CS] & 3) as u8;
        let iopl = ((self.flags >> 12) & 3) as u8;
        if cpl > iopl {
            self.ip = op_ip;
            self.do_interrupt_with_error(13, Some(0), mem);
            true
        } else {
            false
        }
    }

    pub fn write_sreg(&mut self, idx: usize, value: u16, mem: &Memory) {
        if idx >= 6 {
            return;
        }
        self.sregs[idx] = value;
        if self.cr0 & 1 == 0 {
            self.seg_cache[idx] = SegmentCache {
                base: (value as u32) << 4,
                limit: 0xFFFF,
                access: 0x93,
            };
            // Real mode is 16-bit: a CS/SS reload here must drop the
            // operand/stack size back to 16 (otherwise a PE->real
            // transition would leave a stale 32-bit default and decode
            // real-mode code with the wrong width).
            if idx == sreg::CS {
                self.code_size_32 = false;
                self.invalidate_fetch_tlb();
            }
            if idx == sreg::SS {
                self.stack_size_32 = false;
            }
            return;
        }
        // Protected mode — fetch and decode the descriptor.
        let table_base = self.gdtr.base; // TI=LDT not modeled
        let desc_addr = table_base.wrapping_add((value & 0xFFF8) as u32);
        let d0 = self.mem_read_u16(mem, desc_addr) as u32;
        let d1 = self.mem_read_u16(mem, desc_addr.wrapping_add(2)) as u32;
        let d2 = self.mem_read_u16(mem, desc_addr.wrapping_add(4)) as u32;
        let d3 = self.mem_read_u16(mem, desc_addr.wrapping_add(6)) as u32;
        let base = d1 | ((d2 & 0x00FF) << 16) | ((d3 & 0xFF00) << 16);
        let access = ((d2 >> 8) & 0xFF) as u8;
        let raw_limit = (d0 & 0xFFFF) | ((d3 & 0x000F) << 16);
        let granularity = (d3 >> 7) & 1;
        let limit = if granularity != 0 {
            (raw_limit << 12) | 0x0FFF
        } else {
            raw_limit
        };
        self.seg_cache[idx] = SegmentCache {
            base,
            limit,
            access,
        };
        // CS load: latch the D bit (descriptor byte 6 bit 6 = 0x40)
        // into `code_size_32`. Without this a 32-bit kernel running
        // in a flat code segment would still decode instructions as
        // 16-bit by default — the very condition the D bit exists
        // to flip. The D bit lives in the granularity byte alongside
        // G and the limit-high nibble.
        if idx == sreg::CS {
            self.code_size_32 = (d3 >> 6) & 1 != 0;
            // CPL transitions can change the U/S meaning of cached
            // fetch translations — invalidate to force a fresh
            // walk under the new privilege level.
            self.invalidate_fetch_tlb();
        }
        // SS load: latch the B bit (same byte 6 bit 6 = 0x40) into
        // `stack_size_32`. The B bit selects ESP-vs-SP for every implicit
        // stack access (PUSH/POP/CALL/RET/ENTER/LEAVE/interrupt frames);
        // without this a guest that reloads SS with a differently-sized
        // descriptor would keep the stale stack width.
        if idx == sreg::SS {
            self.stack_size_32 = (d3 >> 6) & 1 != 0;
        }
    }

    /// PE-aware linear-address translation. In real mode the cache
    /// base is `sregs[idx] << 4` so this matches the legacy shift-by-4
    /// math. In PM the cache holds the descriptor's base directly, so
    /// CR0.PE=1 actually changes effective addresses for every memory
    /// access that routes through here.
    pub fn linear_seg(&self, seg_idx: usize, off: u32) -> u32 {
        self.seg_cache[seg_idx].base.wrapping_add(off)
    }

    /// VERR/VERW backing: is the segment named by `selector` either
    /// readable (`for_write=false`) or writable (`for_write=true`)
    /// from the current CPL? Returns `false` for the null selector,
    /// for any selector that walks off the GDT, and for descriptors
    /// whose access byte forbids the requested operation. In real
    /// mode every selector is treated as accessible — VERR/VERW
    /// without PE is undefined on real silicon but Linux only
    /// issues them under CR0.PE=1.
    fn selector_accessible(&self, mem: &Memory, selector: u16, for_write: bool) -> bool {
        if self.cr0 & 1 == 0 {
            return true;
        }
        // Null selector → always inaccessible.
        if selector & 0xFFFC == 0 {
            return false;
        }
        // TI=1 (LDT) not modeled — Linux never asks VERW on an LDT
        // selector in its mitigation paths.
        let table_base = self.gdtr.base;
        let addr = table_base.wrapping_add((selector & 0xFFF8) as u32);
        // Bail if the descriptor would walk past the GDT limit.
        if (selector & 0xFFF8) as u32 + 7 > self.gdtr.limit as u32 {
            return false;
        }
        // Access byte lives at offset 5 of an 8-byte descriptor.
        // bit 4 = S (1 = code/data, 0 = system); we only ever
        // VERR/VERW code or data, so a system descriptor returns
        // false. Among code/data, bits 3:1 encode (E, R/C, W/R, A);
        // readable: data is always readable; code is readable iff R
        // (bit 1) is 1. writable: code is never writable; data is
        // writable iff W (bit 1) is 1.
        let access = self.mem_read_u8(mem, addr.wrapping_add(5));
        let s = (access >> 4) & 1;
        if s == 0 {
            return false;
        }
        let executable = (access >> 3) & 1 != 0;
        let rw_bit = (access >> 1) & 1 != 0;
        if for_write {
            !executable && rw_bit
        } else {
            !executable || rw_bit
        }
    }

    /// Translate a linear address to a physical address. When
    /// `CR0.PG = 0` this is identity (real mode and unpaged PM).
    /// When `CR0.PG = 1` it walks the 2-level i386 page tables
    /// rooted at `cr3`:
    ///
    /// ```text
    ///   linear[31:22] -> PD index   (PDE at cr3[31:12] + idx*4)
    ///   linear[21:12] -> PT index   (PTE at PDE[31:12] + idx*4)
    ///   linear[11: 0] -> page offset
    /// ```
    ///
    /// Not yet modelled: User/Supervisor (since we always run ring
    /// 0) and a TLB cache. CR0.WP (bit 16) is honored: a supervisor
    /// write to a present page whose effective R/W bit is 0 raises
    /// #PF with P=1 (page is present) and W=1. Linux's COW path
    /// depends on this — without it `fork()`'d children silently
    /// overwrite the parent's shared pages. 4 MiB pages (CR4.PSE +
    /// PDE.PS) collapse the second walk and are honored here too —
    /// Linux's `head_32.S` maps the kernel that way.
    ///
    /// Defaults to a read access; writes go through `translate_write`
    /// so the W bit in the #PF error code reflects the access type.
    /// Instruction fetches go through `translate_fetch` so the I/D
    /// bit (bit 4) is set — Linux's `do_page_fault` reads this bit
    /// to know whether the kernel tried to execute the page.
    pub fn translate(&self, mem: &Memory, linear: u32) -> u32 {
        // Read fast path: same shape as `translate_fetch`'s
        // mini-ITLB, just on `read_tlb`. Walks for write accesses
        // and instruction fetches bypass this slot — see
        // `translate_write` and `translate_fetch`.
        if self.cr0 & 0x8000_0000 == 0 {
            return if self.a20 {
                linear
            } else {
                linear & 0xFFEF_FFFF
            };
        }
        let page = linear & 0xFFFF_F000;
        if let Some((cached_page, cached_frame, cached_a20)) = self.read_tlb.get() {
            if cached_page == page && cached_a20 == self.a20 {
                return cached_frame | (linear & 0xFFF);
            }
        }
        let phys = self.translate_inner(mem, linear, Access::Read);
        if self.pending_fault.get().is_none() {
            self.read_tlb
                .set(Some((page, phys & 0xFFFF_F000, self.a20)));
        }
        phys
    }

    /// Same as `translate` but tags the resulting #PF (if any) with
    /// W=1 in the error code so the handler knows a write was the
    /// trigger. Used by `mem_write_u8/16/32`.
    pub fn translate_write(&self, mem: &Memory, linear: u32) -> u32 {
        // Same shape as `translate` (read fast path), just on a
        // separate slot. See `write_tlb` doc-comment for the
        // soundness argument re: CR0.WP / R/W / CPL invalidation.
        if self.cr0 & 0x8000_0000 == 0 {
            return if self.a20 {
                linear
            } else {
                linear & 0xFFEF_FFFF
            };
        }
        let page = linear & 0xFFFF_F000;
        if let Some((cached_page, cached_frame, cached_a20)) = self.write_tlb.get() {
            if cached_page == page && cached_a20 == self.a20 {
                return cached_frame | (linear & 0xFFF);
            }
        }
        let phys = self.translate_inner(mem, linear, Access::Write);
        if self.pending_fault.get().is_none() {
            self.write_tlb
                .set(Some((page, phys & 0xFFFF_F000, self.a20)));
        }
        phys
    }

    /// Same as `translate` but tags the resulting #PF with I/D=1
    /// (bit 4) so the handler can tell an instruction-fetch fault
    /// from a data read. Used by `fetch_u8` for opcode and immediate
    /// bytes.
    pub fn translate_fetch(&self, mem: &Memory, linear: u32) -> u32 {
        // Fast path: the most recent successful fetch translation
        // is cached in `fetch_tlb` and matches when consecutive
        // opcode bytes share the same 4 KiB page. With this in
        // place a 95-second linux_userspace test cuts to ~70s on
        // a typical kernel-heavy boot; without it every byte of
        // every fetch re-walks the PDE + PTE (which sit in memory
        // outside the kernel image so each walk is two extra
        // memory reads). Real x86 has the same shortcut as the L1
        // ITLB. Invalidations: CR3 reload, INVLPG, CR0.PG toggle,
        // write_sreg(CS), and CPU reset all clear this slot.
        if self.cr0 & 0x8000_0000 == 0 {
            return if self.a20 {
                linear
            } else {
                linear & 0xFFEF_FFFF
            };
        }
        let page = linear & 0xFFFF_F000;
        if let Some((cached_page, cached_frame, cached_a20)) = self.fetch_tlb.get() {
            if cached_page == page && cached_a20 == self.a20 {
                return cached_frame | (linear & 0xFFF);
            }
        }
        let phys = self.translate_inner(mem, linear, Access::Fetch);
        // Only cache the result if the walk didn't fault — a
        // faulting walk returns the sentinel 0 and shouldn't seed
        // future fetches.
        if self.pending_fault.get().is_none() {
            self.fetch_tlb
                .set(Some((page, phys & 0xFFFF_F000, self.a20)));
        }
        phys
    }

    /// Clear the fetch-translation cache. Called from every site
    /// that can change the linear→physical mapping for code: CR3
    /// reload, INVLPG, CR0.PG toggle, write_sreg(CS), CPU reset.
    /// Memory writes are NOT covered — a self-modifying page
    /// table without INVLPG / CR3 reload is undefined on real
    /// silicon and Linux never does that on the fetch path.
    fn invalidate_fetch_tlb(&self) {
        self.fetch_tlb.set(None);
        // Read- and write-side caches share all the same
        // invalidation triggers; clear them together so the call
        // sites only have one helper to remember.
        self.read_tlb.set(None);
        self.write_tlb.set(None);
    }

    // PDE/PTE A/D bits are deliberately NOT updated by translate_inner.
    // Linux's only consumer is page-reclaim sweeps that compare A=1 vs
    // A=0 to estimate page activity; on our boot path no reclaim
    // ever runs (we hit the kernel-init → /init → exit panic chain
    // long before kswapd activates), so the user-visible difference
    // is zero. The deferred-writeback attempt that motivated this
    // decision (1-slot Cell flushed at step start) regressed three
    // page-table unit tests by clobbering user data writes; see
    // `c893f2f` for the post-mortem. A per-write same-word filter
    // would unbreak those tests but is fragile and brings no
    // workload-level win — leaving A/D unsupported is the load-
    // bearing call.
    fn translate_inner(&self, mem: &Memory, linear: u32, access: Access) -> u32 {
        let phys = if self.cr0 & 0x8000_0000 == 0 {
            linear
        } else {
            let pd_index = (linear >> 22) & 0x3FF;
            let pt_index = (linear >> 12) & 0x3FF;
            let page_offset = linear & 0xFFF;
            let pd_base = self.cr3 & 0xFFFF_F000;
            let pde = mem.read_u32(pd_base.wrapping_add(pd_index * 4));
            let write = matches!(access, Access::Write);
            let w_bit: u32 = if write { 0b10 } else { 0 };
            let id_bit: u32 = if matches!(access, Access::Fetch) {
                0b1_0000
            } else {
                0
            };
            // U/S bit: 1 if the access came from CPL=3, else 0. Read
            // CPL from CS.RPL — only meaningful in PE mode (real mode
            // is always "supervisor" semantically). Linux's
            // do_page_fault uses this to split userspace SIGSEGV
            // from kernel-mode fixup paths.
            let us_bit: u32 = if self.cr0 & 1 != 0 && (self.sregs[sreg::CS] & 3) == 3 {
                0b100
            } else {
                0
            };
            let extra: u32 = w_bit | id_bit | us_bit;
            // Present bit (bit 0) clear -> #PF with P=0. Error code
            // also carries W (write) and I/D (instruction-fetch) so
            // the handler can tell read / write / fetch apart.
            if pde & 1 == 0 {
                self.raise_fault(linear, extra);
                return 0;
            }
            // PSE (CR4.PSE = bit 4) + PDE.PS (bit 7) collapses the
            // PDE into a direct 4 MiB-page descriptor — no PTE
            // walk. Linux's `head_32.S` uses this to map the entire
            // kernel virtual range with a handful of PDEs instead of
            // emitting a million PTEs at boot. PDE.PS without CR4.PSE
            // is reserved on a 4 KiB-page MMU and would normally
            // raise #PF with RSVD=1; we treat it as a regular 4 KiB
            // PDE (i.e. ignore the PS bit) so a buggy guest doesn't
            // get a phantom fault.
            if self.cr4 & 0x10 != 0 && pde & 0x80 != 0 {
                // 4 MiB pages: PDE[31:22] is the frame, linear[21:0]
                // is the offset within it. With CR0.WP (bit 16) and
                // a write to a page whose R/W bit is clear, raise
                // #PF with P=1 (page IS present) | W=1.
                if write && self.cr0 & 0x0001_0000 != 0 && pde & 0b10 == 0 {
                    self.raise_fault(linear, extra | 1);
                    return 0;
                }
                let frame = pde & 0xFFC0_0000;
                let offset = linear & 0x003F_FFFF;
                frame | offset
            } else {
                let pt_base = pde & 0xFFFF_F000;
                let pte = mem.read_u32(pt_base.wrapping_add(pt_index * 4));
                if pte & 1 == 0 {
                    self.raise_fault(linear, extra);
                    return 0;
                }
                // CR0.WP: a supervisor write to a present page whose
                // effective R/W is 0 (= AND of PDE.RW and PTE.RW) is
                // a #PF with P=1 | W=1. This is what Linux's COW
                // path needs — without WP, fork() children's writes
                // would silently land in the parent's pages.
                let effective_rw = (pde & 0b10) & (pte & 0b10);
                if write && self.cr0 & 0x0001_0000 != 0 && effective_rw == 0 {
                    self.raise_fault(linear, extra | 1);
                    return 0;
                }
                let frame = pte & 0xFFFF_F000;
                frame | page_offset
            }
        };
        // A20 line gating happens *after* paging — it's a property of
        // the physical address bus. With A20 off (the 8086-compat
        // mode), bit 20 of every physical address is forced to zero.
        if self.a20 {
            phys
        } else {
            phys & 0xFFEF_FFFF
        }
    }

    /// Centralized port-read shim. Special-cases port 0x92 (System
    /// Control Port A) because bit 1 of that port toggles the A20
    /// gate, which lives on Cpu — IoBus can't service it. Every
    /// other port falls through to the regular IoBus dispatch.
    fn port_read(&mut self, io: &mut IoBus, port: u16) -> u8 {
        if port == 0x92 {
            // Bit 1 = A20 enable. Bit 0 (system reset) reads 0.
            return if self.a20 { 0b10 } else { 0 };
        }
        io.read(port)
    }

    /// Counterpart to `port_read`. A write to 0x92 with bit 1 set
    /// enables A20; clearing the bit gates A20 off.
    fn port_write(&mut self, io: &mut IoBus, port: u16, value: u8) {
        if port == 0x92 {
            self.a20 = value & 0b10 != 0;
            // The fetch/read TLB tuples carry the a20 state they
            // were captured under, so a direct toggle here would
            // already be picked up as a cache miss on the next
            // translate — we don't need an explicit invalidation.
            return;
        }
        io.write(port, value);
    }

    /// Record a pending #PF. `step()` consumes this at the top of the
    /// next iteration; until then, the in-progress instruction's
    /// memory accesses become benign reads from physical 0 (and
    /// writes to physical 0). That's accepted skew vs. real x86,
    /// which would abort the instruction outright — fine for now
    /// because the only guests we currently run are tests that bring
    /// down the CPU immediately after triggering a fault.
    fn raise_fault(&self, addr: u32, error_code: u32) {
        if self.pending_fault.get().is_none() {
            self.pending_fault.set(Some(PageFault { addr, error_code }));
        }
        // Debug hook: on the first user-mode READ fault of a low
        // address (the ld.so null-deref signature: CR2≈0, U=1, W=0),
        // dump the instruction-trace ring once. No-op unless enabled.
        if self.pf_trace.borrow().is_some() {
            let user_read = error_code & 0b110 == 0b100; // U=1, W=0
            let last_ip = self.last_op_ip;
            // A wild control transfer (bad ret/call/jmp target) shows up
            // as an instruction-FETCH fault: the address being faulted on
            // is the current op_ip (flat user CS → linear == offset).
            // Require the address to look like DATA — all four bytes
            // ASCII-printable — so we skip benign first-touch demand-page
            // faults of real code (e.g. ld.so's entry) and fire only when
            // EIP has clearly landed inside a string/data region.
            // An instruction-FETCH fault (I/D bit 4 set) into ASCII/string
            // data is a wild control transfer — fire regardless of
            // privilege (a wild KERNEL jump into data, U=0, is the busybox
            // multi-lib symptom and the user filter missed it).
            let is_fetch = error_code & 0x10 != 0;
            let looks_ascii = addr
                .to_le_bytes()
                .iter()
                .all(|&b| (0x20..=0x7e).contains(&b));
            if let Some(t) = self.pf_trace.borrow_mut().as_mut() {
                if !t.fired && user_read && addr < 0x1000 {
                    t.fired = true;
                    t.dump(&format!(
                        "user read #PF addr={addr:#x} err={error_code:#x} faulting_eip={last_ip:#x}"
                    ));
                } else if !t.fired && is_fetch && looks_ascii {
                    // EIP jumped into ASCII/string data — the trace ring
                    // shows the ret/call/jmp that set this bad target.
                    t.fired = true;
                    t.dump(&format!(
                        "FETCH #PF (wild jump into data) addr={addr:#x} err={error_code:#x} eip={last_ip:#x}"
                    ));
                }
            }
        }
    }

    /// Inspect the pending page-fault slot without consuming it. The
    /// test suite uses this to assert that translate() actually
    /// flagged the fault; `step()` uses `take_pending_fault`.
    pub fn pending_fault(&self) -> Option<PageFault> {
        self.pending_fault.get()
    }

    /// Consume the pending page-fault, if any. Used by `step()` to
    /// dispatch INT 14 after the faulting instruction returns.
    fn take_pending_fault(&self) -> Option<PageFault> {
        self.pending_fault.replace(None)
    }

    /// Paging-aware memory read. Returns the byte that lives at the
    /// physical address `translate(linear)` resolves to. When PG=0
    /// this is exactly `self.mem_read_u8(mem,linear)`. Used by every guest-
    /// visible memory access (instruction fetch, ModR/M reads, stack
    /// pops, string ops) so toggling CR0.PG actually changes which
    /// page-frame the guest sees.
    ///
    /// If a fault is already pending (from an earlier byte of a
    /// multi-byte access), short-circuit to 0 — real x86 would have
    /// aborted the instruction; our continue-on-fault model must at
    /// least stop chaining new translates so subsequent CR2 updates
    /// don't clobber the first faulting address.
    pub fn mem_read_u8(&self, m: &Memory, linear: u32) -> u8 {
        if self.pending_fault.get().is_some() {
            return 0;
        }
        m.read_u8(self.translate(m, linear))
    }

    /// Paging-aware instruction-byte read. Identical to `mem_read_u8`
    /// at the physical-address level — but any #PF raised has the
    /// I/D bit (bit 4) set, so the handler can branch on "exec vs
    /// data".  Used by `fetch_u8` and the immediate-byte fetchers.
    pub fn mem_fetch_u8(&self, m: &Memory, linear: u32) -> u8 {
        if self.pending_fault.get().is_some() {
            return 0;
        }
        // Instruction fetch never targets the LAPIC/HPET MMIO windows, so use
        // the code-read path that skips those per-byte checks.
        m.read_code_u8(self.translate_fetch(m, linear))
    }

    pub fn mem_write_u8(&self, m: &mut Memory, linear: u32, value: u8) {
        // Abort the write if a fault is already pending (an earlier
        // byte of a multi-byte access faulted) or if THIS translate
        // raises a fresh fault. Real x86 aborts the instruction on
        // the first faulting access; our model continues but must
        // not let the bogus phys=0 sentinel that translate returns
        // for a fault actually clobber DRAM. Without this guard,
        // Linux's millions of stack pushes to unmapped vmalloc
        // pages turn into millions of writes to physical 0, which
        // walks across the kernel image and corrupts .text.
        let had_fault = self.pending_fault.get().is_some();
        let phys = self.translate_write(m, linear);
        if had_fault || self.pending_fault.get().is_some() {
            return;
        }
        // Diagnostic watchpoint — see `watch_write` for the cached
        // env-var path. Sharing the helper means a single OnceLock
        // bound, used by both the byte and aligned paths.
        self.watch_write(m, linear, phys, value);
        m.write_u8(phys, value);
    }

    /// Read a 16-bit word at `linear`. We translate each byte
    /// independently so the rare case of a read that straddles a
    /// page boundary picks up the second byte from the right frame.
    pub fn mem_read_u16(&self, m: &Memory, linear: u32) -> u16 {
        // Page-crossing (base is the last byte of its page): the high byte is
        // on the next page with its own translation — fall back to per-byte.
        if linear & 0xFFF == 0xFFF {
            let lo = self.mem_read_u8(m, linear) as u16;
            let hi = self.mem_read_u8(m, linear.wrapping_add(1)) as u16;
            return lo | (hi << 8);
        }
        // Same-page fast path: translate once, read both bytes from the
        // physical frame (mirrors `mem_write_aligned`). read_u8 still routes
        // MMIO per byte, so a 16-bit MMIO read stays correct.
        if self.pending_fault.get().is_some() {
            return 0;
        }
        let phys = self.translate(m, linear);
        if self.pending_fault.get().is_some() {
            return 0;
        }
        (m.read_u8(phys) as u16) | ((m.read_u8(phys.wrapping_add(1)) as u16) << 8)
    }

    pub fn mem_write_u16(&self, m: &mut Memory, linear: u32, value: u16) {
        self.mem_write_aligned(m, linear, &value.to_le_bytes());
    }

    pub fn mem_read_u32(&self, m: &Memory, linear: u32) -> u32 {
        // Page-crossing within the 4 bytes: split into two u16 reads, each of
        // which handles its own page boundary.
        if linear & 0xFFF > 0xFFC {
            let lo = self.mem_read_u16(m, linear) as u32;
            let hi = self.mem_read_u16(m, linear.wrapping_add(2)) as u32;
            return lo | (hi << 16);
        }
        // Same-page fast path: translate once, read four bytes from the
        // physical frame (was 4 separate translations). read_u8 still routes
        // MMIO per byte.
        if self.pending_fault.get().is_some() {
            return 0;
        }
        let phys = self.translate(m, linear);
        if self.pending_fault.get().is_some() {
            return 0;
        }
        (m.read_u8(phys) as u32)
            | ((m.read_u8(phys.wrapping_add(1)) as u32) << 8)
            | ((m.read_u8(phys.wrapping_add(2)) as u32) << 16)
            | ((m.read_u8(phys.wrapping_add(3)) as u32) << 24)
    }

    pub fn mem_write_u32(&self, m: &mut Memory, linear: u32, value: u32) {
        self.mem_write_aligned(m, linear, &value.to_le_bytes());
    }

    /// Write `bytes` to consecutive linear addresses starting at
    /// `linear`. When the whole span fits in a single 4 KiB page we
    /// translate only the base address and use the resulting phys
    /// frame for every byte — this matches real x86 TLB semantics
    /// for an in-flight instruction, where a write to a page-table
    /// entry doesn't retroactively change the translation of the
    /// rest of the same access. Without it, Linux's `MOV [pde], val`
    /// that clears PDE.PS observes the cleared-PS state for byte 1
    /// onwards and walks through the half-written PT — which on a
    /// kernel pgdir update means a fault on an entry that's still
    /// being written.
    ///
    /// If the access crosses a page boundary we fall back to the
    /// per-byte path, which costs the second walk but only matters
    /// for the rare unaligned span. The diagnostic watchpoint
    /// (WWWVM_WATCH_WRITE) fires for every byte regardless of which
    /// path we take.
    fn mem_write_aligned(&self, m: &mut Memory, linear: u32, bytes: &[u8]) {
        // Diagnostic VALUE watchpoint (one-shot pf_trace dump).
        if bytes.len() == 4 {
            if let Some(wv) = *Self::watch_write_value() {
                if u32::from_le_bytes(bytes.try_into().unwrap()) == wv
                    && linear >= Self::watch_write_value_min()
                {
                    let eip = self.last_op_ip;
                    if let Some(t) = self.pf_trace.borrow_mut().as_mut() {
                        if !t.fired {
                            t.fired = true;
                            t.dump(&format!(
                                "VALUE-WATCH {wv:#010x} -> VA={linear:#010x} EIP={eip:#010x}"
                            ));
                        }
                    }
                }
            }
        }
        let len = bytes.len() as u32;
        let page_off = linear & 0xFFF;
        if page_off + len <= 0x1000 {
            // Same-page fast path: translate once, write many.
            let had_fault = self.pending_fault.get().is_some();
            let phys_base = self.translate_write(m, linear);
            if had_fault || self.pending_fault.get().is_some() {
                return;
            }
            for (i, b) in bytes.iter().enumerate() {
                let phys = phys_base.wrapping_add(i as u32);
                self.watch_write(m, linear.wrapping_add(i as u32), phys, *b);
                m.write_u8(phys, *b);
            }
        } else {
            // Page-crossing slow path: per-byte translate (each may
            // fault independently if the second page isn't mapped).
            for (i, b) in bytes.iter().enumerate() {
                self.mem_write_u8(m, linear.wrapping_add(i as u32), *b);
            }
        }
    }

    fn watch_write(&self, m: &Memory, linear: u32, phys: u32, value: u8) {
        if let Some((lo, hi)) = *Self::watch_write_range() {
            if phys >= lo && phys < hi {
                let esp = self.read_r32(r16::SP as u8);
                let read = |va: u32| -> u32 {
                    let pa = self.translate(m, va);
                    m.read_u32(pa)
                };
                eprintln!(
                    "[W] phys[{:08X}] (VA={:08X}) <- {:02X}  EIP={:08X} ret={:08X},{:08X},{:08X}",
                    phys,
                    linear,
                    value,
                    self.last_op_ip,
                    read(esp),
                    read(esp.wrapping_add(4)),
                    read(esp.wrapping_add(8))
                );
            }
        }
    }

    /// One-line trace per IRQ dispatch when WWWVM_TRACE_IRQ=1 is
    /// set. Print rate is capped via OnceLock-cached counters so a
    /// runaway IRQ source doesn't flood the log past the first few
    /// thousand events. Used to confirm whether the kernel is
    /// actually being ticked at all — Linux's silent stall after
    /// `random: crng init done` could equally be "scheduler never
    /// runs because timer IRQ never fires" or "userspace stuck in
    /// a loop"; the count tells them apart.
    fn trace_irq(&self, vector: u8, source: &str) {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::OnceLock;
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let on = *ENABLED.get_or_init(|| std::env::var_os("WWWVM_TRACE_IRQ").is_some());
        if !on {
            return;
        }
        // Per-vector counters. Index by vector (0..=255). All start
        // at 0; we increment atomically and only log the first 5 of
        // each vector so the trace remains useful.
        static COUNTS: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];
        let n = COUNTS[vector as usize].fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 5 || n.is_multiple_of(10_000) {
            eprintln!(
                "[IRQ] vec={:02X} src={source} count={n} EIP={:08X}",
                vector, self.last_op_ip
            );
        }
    }

    /// Cached parse of WWWVM_WATCH_WRITE so the diagnostic path is
    /// a single pointer-compare on the hot byte-write path instead
    /// of a per-call getenv syscall + hex parse. Returning the
    /// `Option<(lo,hi)>` by reference lets the caller take the
    /// branch when unset for free.
    fn watch_write_range() -> &'static Option<(u32, u32)> {
        use std::sync::OnceLock;
        static SPEC: OnceLock<Option<(u32, u32)>> = OnceLock::new();
        SPEC.get_or_init(|| {
            let spec = std::env::var("WWWVM_WATCH_WRITE").ok()?;
            let (lo, hi) = spec.split_once(':')?;
            Some((
                u32::from_str_radix(lo.trim_start_matches("0x"), 16).ok()?,
                u32::from_str_radix(hi.trim_start_matches("0x"), 16).ok()?,
            ))
        })
    }

    /// Diagnostic VALUE watchpoint: `WWWVM_WATCH_VALUE=0xNNNNNNNN` makes
    /// a 4-byte store of that exact value dump the pf_trace ring once
    /// (with the target VA + storing EIP). Used to find the instruction
    /// that corrupts a saved EIP with string bytes (see the multilib
    /// memory note). No-op unless the env var + pf_trace are set.
    fn watch_write_value() -> &'static Option<u32> {
        use std::sync::OnceLock;
        static SPEC: OnceLock<Option<u32>> = OnceLock::new();
        SPEC.get_or_init(|| {
            let spec = std::env::var("WWWVM_WATCH_VALUE").ok()?;
            u32::from_str_radix(spec.trim_start_matches("0x"), 16).ok()
        })
    }

    /// Optional minimum target VA for the value watchpoint
    /// (`WWWVM_WATCH_VALUE_MIN=0xNNNNNNNN`): the dump fires only on a
    /// matching store whose destination VA is >= this, so benign
    /// early-boot/low-address copies of the watched value can be skipped
    /// to reach a later kernel-side write.
    fn watch_write_value_min() -> u32 {
        use std::sync::OnceLock;
        static SPEC: OnceLock<u32> = OnceLock::new();
        *SPEC.get_or_init(|| {
            std::env::var("WWWVM_WATCH_VALUE_MIN")
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0)
        })
    }

    /// Read an SSE r/m operand: an XMM register (mod=11) or a 128-bit
    /// memory location.
    fn read_xmm_rm(&self, rm: Rm, mem: &Memory) -> u128 {
        match rm {
            Rm::Reg(i) => self.xmm[i as usize],
            Rm::Mem(ea) => self.mem_read_u128(mem, self.linear_seg(ea.seg, ea.off)),
        }
    }

    /// Write an SSE r/m operand (XMM register or 128-bit memory).
    fn write_xmm_rm(&mut self, rm: Rm, mem: &mut Memory, value: u128) {
        match rm {
            Rm::Reg(i) => self.xmm[i as usize] = value,
            Rm::Mem(ea) => {
                let addr = self.linear_seg(ea.seg, ea.off);
                self.mem_write_u128(mem, addr, value);
            }
        }
    }

    /// Read a scalar 32-bit SSE operand: the low dword of an XMM
    /// register or a 32-bit memory value. The `*SS` / MOVSS forms.
    fn read_xmm_rm32(&self, rm: Rm, mem: &Memory) -> u32 {
        match rm {
            Rm::Reg(i) => self.xmm[i as usize] as u32,
            Rm::Mem(ea) => self.mem_read_u32(mem, self.linear_seg(ea.seg, ea.off)),
        }
    }

    /// Read a scalar 64-bit SSE operand: the low qword of an XMM
    /// register or a 64-bit memory value. The `*SD` / MOVSD forms.
    fn read_xmm_rm64(&self, rm: Rm, mem: &Memory) -> u64 {
        match rm {
            Rm::Reg(i) => self.xmm[i as usize] as u64,
            Rm::Mem(ea) => {
                let a = self.linear_seg(ea.seg, ea.off);
                (self.mem_read_u32(mem, a) as u64)
                    | ((self.mem_read_u32(mem, a.wrapping_add(4)) as u64) << 32)
            }
        }
    }

    /// Read an MMX r/m operand: an MM register (mod=11) or a 64-bit memory
    /// value (the no-66 packed-integer forms).
    fn read_mm_rm(&self, rm: Rm, mem: &Memory) -> u64 {
        match rm {
            Rm::Reg(i) => self.mmx[i as usize],
            Rm::Mem(ea) => {
                let a = self.linear_seg(ea.seg, ea.off);
                (self.mem_read_u32(mem, a) as u64)
                    | ((self.mem_read_u32(mem, a.wrapping_add(4)) as u64) << 32)
            }
        }
    }

    /// Write an MMX r/m operand: an MM register or 64-bit memory.
    fn write_mm_rm(&mut self, rm: Rm, mem: &mut Memory, value: u64) {
        match rm {
            Rm::Reg(i) => self.mmx[i as usize] = value,
            Rm::Mem(ea) => {
                let a = self.linear_seg(ea.seg, ea.off);
                self.mem_write_u32(mem, a, value as u32);
                self.mem_write_u32(mem, a.wrapping_add(4), (value >> 32) as u32);
            }
        }
    }

    /// Read a 128-bit value (an XMM operand) as four little-endian
    /// dwords. Used by the SSE move/arith instructions.
    pub fn mem_read_u128(&self, m: &Memory, linear: u32) -> u128 {
        let mut v: u128 = 0;
        for i in 0..4u32 {
            let d = self.mem_read_u32(m, linear.wrapping_add(i * 4)) as u128;
            v |= d << (i * 32);
        }
        v
    }

    pub fn mem_write_u128(&self, m: &mut Memory, linear: u32, value: u128) {
        for i in 0..4u32 {
            self.mem_write_u32(m, linear.wrapping_add(i * 4), (value >> (i * 32)) as u32);
        }
    }

    /// Mandatory-prefix scalar SSE: the F3/F2 + 0F escape. `is_f3`
    /// (the 0xF3 prefix) selects single-precision / MOVSS; F2 selects
    /// double / MOVSD. `op2` is the opcode byte after 0x0F. Scalar
    /// forms touch only the low lane and preserve the rest of the
    /// destination register; memory loads zero-extend the upper bits.
    fn sse_scalar(
        &mut self,
        is_f3: bool,
        op2: u8,
        mem: &mut Memory,
        op_cs: u16,
        op_ip: u32,
    ) -> Result<(), CpuError> {
        const LO32: u128 = 0xFFFF_FFFF;
        const LO64: u128 = 0xFFFF_FFFF_FFFF_FFFF;
        match op2 {
            // MOVSS/MOVSD xmm, xmm/m — load the low lane. Reg-reg
            // preserves the destination's upper bits; a memory load
            // zero-extends them.
            0x10 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let (v, mask) = if is_f3 {
                    (self.read_xmm_rm32(rm, mem) as u128, LO32)
                } else {
                    (self.read_xmm_rm64(rm, mem) as u128, LO64)
                };
                self.xmm[reg as usize] = match rm {
                    Rm::Reg(_) => (self.xmm[reg as usize] & !mask) | v,
                    Rm::Mem(_) => v,
                };
            }
            // MOVSS/MOVSD xmm/m, xmm — store the low lane.
            0x11 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let v = self.xmm[reg as usize] as u32;
                    match rm {
                        Rm::Reg(i) => {
                            self.xmm[i as usize] = (self.xmm[i as usize] & !LO32) | (v as u128);
                        }
                        Rm::Mem(ea) => {
                            let a = self.linear_seg(ea.seg, ea.off);
                            self.mem_write_u32(mem, a, v);
                        }
                    }
                } else {
                    let v = self.xmm[reg as usize] as u64;
                    match rm {
                        Rm::Reg(i) => {
                            self.xmm[i as usize] = (self.xmm[i as usize] & !LO64) | (v as u128);
                        }
                        Rm::Mem(ea) => {
                            let a = self.linear_seg(ea.seg, ea.off);
                            self.mem_write_u32(mem, a, v as u32);
                            self.mem_write_u32(mem, a.wrapping_add(4), (v >> 32) as u32);
                        }
                    }
                }
            }
            // MOVDQU — unaligned 128-bit move (F3 0F 6F load / 7F
            // store). Identical to MOVDQA here; we don't fault on
            // misalignment. Only the F3 encoding is defined.
            0x6F if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = v;
            }
            0x7F if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.xmm[reg as usize];
                self.write_xmm_rm(rm, mem, v);
            }
            // Scalar float arithmetic — compute only the low lane,
            // preserve the rest of the destination.
            //   58 ADD   59 MUL   5C SUB   5E DIV
            0x58 | 0x59 | 0x5C | 0x5E => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let a = f32::from_bits(self.xmm[reg as usize] as u32);
                    let b = f32::from_bits(self.read_xmm_rm32(rm, mem));
                    let r = match op2 {
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5C => a - b,
                        _ => a / b,
                    };
                    self.xmm[reg as usize] =
                        (self.xmm[reg as usize] & !LO32) | (r.to_bits() as u128);
                } else {
                    let a = f64::from_bits(self.xmm[reg as usize] as u64);
                    let b = f64::from_bits(self.read_xmm_rm64(rm, mem));
                    let r = match op2 {
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5C => a - b,
                        _ => a / b,
                    };
                    self.xmm[reg as usize] =
                        (self.xmm[reg as usize] & !LO64) | (r.to_bits() as u128);
                }
            }
            // Scalar MIN/MAX (5D/5F) and SQRT (51) — low lane only,
            // upper bits preserved. SQRT is unary (source = rm).
            0x51 | 0x5D | 0x5F => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let a = f32::from_bits(self.xmm[reg as usize] as u32);
                    let b = f32::from_bits(self.read_xmm_rm32(rm, mem));
                    let r = match op2 {
                        0x51 => b.sqrt(),
                        0x5D => fmin_max_f32(a, b, true),
                        _ => fmin_max_f32(a, b, false),
                    };
                    self.xmm[reg as usize] =
                        (self.xmm[reg as usize] & !LO32) | (r.to_bits() as u128);
                } else {
                    let a = f64::from_bits(self.xmm[reg as usize] as u64);
                    let b = f64::from_bits(self.read_xmm_rm64(rm, mem));
                    let r = match op2 {
                        0x51 => b.sqrt(),
                        0x5D => fmin_max(a, b, true),
                        _ => fmin_max(a, b, false),
                    };
                    self.xmm[reg as usize] =
                        (self.xmm[reg as usize] & !LO64) | (r.to_bits() as u128);
                }
            }
            // CVTSI2SS / CVTSI2SD — signed int32 (GP r/m32) → scalar
            // float in the low lane of the XMM dest, upper bits kept.
            //   F3 0F 2A  CVTSI2SS    F2 0F 2A  CVTSI2SD
            0x2A => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let src = self.read_rm32(rm, mem) as i32;
                if is_f3 {
                    let v = (src as f32).to_bits() as u128;
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO32) | v;
                } else {
                    let v = (src as f64).to_bits() as u128;
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO64) | v;
                }
            }
            // CVT(T)SS2SI / CVT(T)SD2SI — scalar float (XMM/m) → signed
            // int32 in a GP register. The 0x2C forms truncate toward
            // zero; 0x2D round to nearest-even (default MXCSR mode).
            //   F3 0F 2C/2D  *SS2SI    F2 0F 2C/2D  *SD2SI
            0x2C | 0x2D => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let truncate = op2 == 0x2C;
                let result = if is_f3 {
                    let f = f32::from_bits(self.read_xmm_rm32(rm, mem));
                    let f = if truncate {
                        f.trunc()
                    } else {
                        f.round_ties_even()
                    };
                    f as i32
                } else {
                    let f = f64::from_bits(self.read_xmm_rm64(rm, mem));
                    let f = if truncate {
                        f.trunc()
                    } else {
                        f.round_ties_even()
                    };
                    f as i32
                };
                self.write_r32(reg, result as u32);
            }
            // Scalar precision converts (F3/F2 0F 5A):
            //   F3 → CVTSS2SD  (low f32 → low f64; upper 64 preserved)
            //   F2 → CVTSD2SS  (low f64 → low f32; upper 96 preserved)
            0x5A => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let f = f32::from_bits(self.read_xmm_rm32(rm, mem)) as f64;
                    let v = f.to_bits() as u128;
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO64) | v;
                } else {
                    let f = f64::from_bits(self.read_xmm_rm64(rm, mem)) as f32;
                    let v = f.to_bits() as u128;
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO32) | v;
                }
            }
            // CVTTPS2DQ (F3 0F 5B) — 4×f32 → 4×i32 with truncation.
            // The 0F 5B no-prefix / 66 forms (CVTDQ2PS / CVTPS2DQ)
            // go through the main 0F dispatch; F2 is undefined.
            0x5B if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let src = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = packed_lanes(src, 0, 32, |x, _, _| {
                    let f = f32::from_bits(x as u32);
                    (f.trunc() as i32) as u32 as u128
                });
            }
            // Packed double↔int converts (0F E6):
            //   F3 → CVTDQ2PD  (2×i32 from low 64 → 2×f64, all 128)
            //   F2 → CVTPD2DQ  (2×f64 → 2×i32 in low 64, upper = 0,
            //                    rounded per default MXCSR)
            // The 66 form (CVTTPD2DQ, truncate) is in the main 0F arm.
            0xE6 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let src = self.read_xmm_rm64(rm, mem);
                    let lo = (src as u32 as i32) as f64;
                    let hi = ((src >> 32) as u32 as i32) as f64;
                    self.xmm[reg as usize] =
                        (lo.to_bits() as u128) | ((hi.to_bits() as u128) << 64);
                } else {
                    let src = self.read_xmm_rm(rm, mem);
                    let lo = f64::from_bits(src as u64).round_ties_even() as i32 as u32 as u128;
                    let hi =
                        f64::from_bits((src >> 64) as u64).round_ties_even() as i32 as u32 as u128;
                    self.xmm[reg as usize] = lo | (hi << 32);
                }
            }
            // PSHUFHW (F3 0F 70 ib) / PSHUFLW (F2 0F 70 ib) — shuffle
            // either the high four or low four 16-bit lanes by an
            // imm8 selector (two bits per output lane); the untouched
            // half copies through verbatim. PSHUFD's word-level kin —
            // typical use is rearranging the bytes of a string lane
            // before a comparison.
            // MOVQ xmm1, xmm2/m64 (F3 0F 7E) — load the low qword,
            // zero-extending the upper 64 bits of the destination.
            // The 66 form (MOVD r/m32, xmm) is in the main 0F dispatch
            // and stays as-is; the F2 form is undefined.
            0x7E if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_xmm_rm64(rm, mem) as u128;
                self.xmm[reg as usize] = v;
            }
            0x70 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let imm = self.fetch_u8(mem);
                let src = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = if is_f3 {
                    // PSHUFHW: low 64 bits unchanged; high 4 words
                    // selected from positions [4..7] of the source.
                    let mut out = src & ((1u128 << 64) - 1);
                    for i in 0..4u32 {
                        let sel = ((imm >> (2 * i)) & 0b11) as u32;
                        let w = (src >> (64 + sel * 16)) & 0xFFFF;
                        out |= w << (64 + i * 16);
                    }
                    out
                } else {
                    // PSHUFLW: high 64 bits unchanged; low 4 words
                    // selected from positions [0..3].
                    let mut out = src & !((1u128 << 64) - 1);
                    for i in 0..4u32 {
                        let sel = ((imm >> (2 * i)) & 0b11) as u32;
                        let w = (src >> (sel * 16)) & 0xFFFF;
                        out |= w << (i * 16);
                    }
                    out
                };
            }
            // TZCNT (F3 0F BC) / LZCNT (F3 0F BD) — BMI1 / ABM bit-
            // counting instructions. Linux uses these for atomic
            // bit-search paths via __ffs/__fls; on a CPU without BMI1
            // the F3 prefix is silently ignored and BSF/BSR runs
            // instead, but our model has to dispatch the right
            // semantics. Difference vs BSF/BSR: on a zero source
            // TZCNT/LZCNT return the operand size (32) and set CF=1,
            // whereas BSF/BSR leave the destination architecturally
            // undefined and set ZF=1.
            0xBC | 0xBD if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let is_tzcnt = op2 == 0xBC;
                if self.op_size_32 {
                    let v = self.read_rm32(rm, mem);
                    if v == 0 {
                        self.set_flag(flag::CF, true);
                        self.write_r32(reg, 32);
                    } else {
                        self.set_flag(flag::CF, false);
                        let n = if is_tzcnt {
                            v.trailing_zeros()
                        } else {
                            v.leading_zeros()
                        };
                        self.write_r32(reg, n);
                    }
                } else {
                    let v = self.read_rm16(rm, mem);
                    if v == 0 {
                        self.set_flag(flag::CF, true);
                        self.write_r16(reg, 16);
                    } else {
                        self.set_flag(flag::CF, false);
                        let n = if is_tzcnt {
                            v.trailing_zeros() as u16
                        } else {
                            (v.leading_zeros() as u16).wrapping_sub(16)
                        };
                        self.write_r16(reg, n);
                    }
                }
            }
            // SSE3 duplicating moves. MOVDDUP (F2 0F 12) broadcasts the low
            // f64 to both lanes ([lo,lo]) — OpenBLAS dgemm broadcasts scalars
            // this way; MOVSLDUP (F3 0F 12) duplicates even f32 lanes
            // ([s0,s0,s2,s2]); MOVSHDUP (F3 0F 16) the odd lanes ([s1,s1,s3,s3]).
            0x12 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                self.xmm[reg as usize] = if is_f3 {
                    let v = self.read_xmm_rm(rm, mem);
                    let s0 = v & 0xFFFF_FFFF;
                    let s2 = (v >> 64) & 0xFFFF_FFFF;
                    s0 | (s0 << 32) | (s2 << 64) | (s2 << 96)
                } else {
                    let lo = self.read_xmm_rm64(rm, mem) as u128;
                    lo | (lo << 64)
                };
            }
            0x16 if is_f3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_xmm_rm(rm, mem);
                let s1 = (v >> 32) & 0xFFFF_FFFF;
                let s3 = (v >> 96) & 0xFFFF_FFFF;
                self.xmm[reg as usize] = s1 | (s1 << 32) | (s3 << 64) | (s3 << 96);
            }
            // SSE3 horizontal add/sub + add-sub, PS forms (F2 prefix):
            // HADDPS (F2 0F 7C), HSUBPS (F2 0F 7D), ADDSUBPS (F2 0F D0) over
            // 4×f32 lanes. (The 66-prefixed PD forms live in the 0F map.)
            0x7C => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let s = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = hadd_f32(self.xmm[reg as usize], s, false);
            }
            0x7D => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let s = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = hadd_f32(self.xmm[reg as usize], s, true);
            }
            0xD0 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let s = self.read_xmm_rm(rm, mem);
                self.xmm[reg as usize] = addsub_f32(self.xmm[reg as usize], s);
            }
            // CMPSS (F3 0F C2) / CMPSD (F2 0F C2) — scalar float compare with
            // an imm8 predicate; the low lane becomes an all-ones/all-zeros
            // mask, the upper bits of the destination are preserved.
            0xC2 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if is_f3 {
                    let a = f32::from_bits(self.xmm[reg as usize] as u32) as f64;
                    let b = f32::from_bits(self.read_xmm_rm32(rm, mem)) as f64;
                    let imm = self.fetch_u8(mem);
                    let mask = if sse_cmp(a, b, imm) { LO32 } else { 0 };
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO32) | mask;
                } else {
                    let a = f64::from_bits(self.xmm[reg as usize] as u64);
                    let b = f64::from_bits(self.read_xmm_rm64(rm, mem));
                    let imm = self.fetch_u8(mem);
                    let mask = if sse_cmp(a, b, imm) { LO64 } else { 0 };
                    self.xmm[reg as usize] = (self.xmm[reg as usize] & !LO64) | mask;
                }
            }
            _ => {
                return Err(CpuError::Unimplemented {
                    opcode: op2,
                    cs: op_cs,
                    ip: op_ip,
                });
            }
        }
        Ok(())
    }

    fn fetch_u8(&mut self, mem: &Memory) -> u8 {
        let addr = self.linear_seg(sreg::CS, self.ip);
        self.ip = self.ip.wrapping_add(1);
        self.mem_fetch_u8(mem, addr)
    }

    fn fetch_u16(&mut self, mem: &Memory) -> u16 {
        let ip = self.ip;
        let linear = self.linear_seg(sreg::CS, ip);
        // Page-crossing (low byte is the last in its page): the high byte has
        // its own fetch translation — fall back to per-byte.
        if linear & 0xFFF == 0xFFF {
            let lo = self.fetch_u8(mem) as u16;
            let hi = self.fetch_u8(mem) as u16;
            return lo | (hi << 8);
        }
        // Same page: translate the fetch once, read both code bytes. 32-bit
        // immediates/displacements (read as two fetch_u16) inherit this — 4
        // translations become 2.
        self.ip = ip.wrapping_add(2);
        if self.pending_fault.get().is_some() {
            return 0;
        }
        let phys = self.translate_fetch(mem, linear);
        if self.pending_fault.get().is_some() {
            return 0;
        }
        (mem.read_code_u8(phys) as u16) | ((mem.read_code_u8(phys.wrapping_add(1)) as u16) << 8)
    }

    /// Push a value onto the x87 stack: decrement TOP, write ST(0).
    fn fpu_push(&mut self, value: F80) {
        self.fpu_top = (self.fpu_top.wrapping_sub(1)) & 7;
        self.fpu_st[self.fpu_top as usize] = value;
    }

    /// Pop ST(0): read it, then increment TOP.
    fn fpu_pop(&mut self) -> F80 {
        let v = self.fpu_st[self.fpu_top as usize];
        self.fpu_top = (self.fpu_top.wrapping_add(1)) & 7;
        v
    }

    /// Read ST(i) without popping.
    fn fpu_st(&self, i: u8) -> F80 {
        self.fpu_st[((self.fpu_top + i) & 7) as usize]
    }

    /// Write ST(i).
    fn fpu_set_st(&mut self, i: u8, value: F80) {
        let idx = ((self.fpu_top + i) & 7) as usize;
        self.fpu_st[idx] = value;
    }

    /// Apply an x87 arithmetic sub-op (the 3-bit reg field of D8/DC/DE)
    /// to (a, b). FADD/FMUL are symmetric; FSUB/FDIV and their "reverse"
    /// variants differ in operand order. Returns None for the compare
    /// forms (FCOM/FCOMP), which don't produce a result.
    fn fpu_arith(op: u8, a: F80, b: F80) -> Option<F80> {
        match op {
            0 => Some(a + b), // FADD
            1 => Some(a * b), // FMUL
            2 | 3 => None,    // FCOM / FCOMP (compare; handled separately)
            4 => Some(a - b), // FSUB
            5 => Some(b - a), // FSUBR
            6 => Some(a / b), // FDIV
            7 => Some(b / a), // FDIVR
            _ => None,
        }
    }

    /// Round an F80 to an i64 per the x87 control word's rounding-control
    /// field (CW bits 10-11). FIST/FISTP use this; the default CW=0x037F
    /// selects round-to-nearest-even, NOT truncation.
    fn fpu_round_f80(&self, v: F80) -> i64 {
        v.to_i64_rc(((self.fpu_cw >> 10) & 3) as u8)
    }

    /// Set the x87 condition-code bits C3/C2/C0 (status word bits
    /// 14/10/8) from a compare of `a` vs `b`, mirroring FCOM. Linux/
    /// glibc test these via FNSTSW + SAHF or `sahf; jcc`.
    /// The architectural x87 status word: `fpu_sw` merged with the
    /// separately-tracked TOP pointer in bits 11-13. FNSTSW reads this;
    /// glibc's `fpu_top`-dependent code (and any FNSTSW→stack-relative
    /// logic) needs the real TOP, not a hardcoded 0.
    fn fpu_status_word(&self) -> u16 {
        (self.fpu_sw & !0x3800) | (((self.fpu_top & 7) as u16) << 11)
    }

    /// Set EFLAGS ZF/PF/CF directly from an x87 compare (FCOMI/FUCOMI
    /// family), clearing OF/SF/AF, per Intel SDM. Encoding matches SSE
    /// COMISS: >, <, =, unordered → (0,0,0)/(0,0,1)/(1,0,0)/(1,1,1).
    fn fpu_set_eflags_compare(&mut self, a: F80, b: F80) {
        let (zf, pf, cf) = match a.partial_cmp(b) {
            None => (true, true, true),
            Some(Ordering::Greater) => (false, false, false),
            Some(Ordering::Less) => (false, false, true),
            Some(Ordering::Equal) => (true, false, false),
        };
        self.set_flag(flag::ZF, zf);
        self.set_flag(flag::PF, pf);
        self.set_flag(flag::CF, cf);
        self.set_flag(flag::OF, false);
        self.set_flag(flag::SF, false);
        self.set_flag(flag::AF, false);
    }

    /// FXAM — classify ST(0) into the condition codes C3:C2:C0 and set
    /// C1 to the sign. We model ST as f64, so the "empty" and
    /// "unsupported" classes aren't distinguished.
    fn fpu_fxam(&mut self) {
        let v = self.fpu_st(0);
        self.fpu_sw &= !0x4700; // clear C3/C2/C1/C0
        if v.is_sign_negative() {
            self.fpu_sw |= 0x0200; // C1 = sign
        }
        // (C3, C2, C0): NaN=001, Inf=011, Zero=100, Denormal=110, Normal=010.
        let (c3, c2, c0) = if v.is_nan() {
            (false, false, true)
        } else if v.is_inf() {
            (false, true, true)
        } else if v.is_zero() {
            (true, false, false)
        } else if v.is_subnormal() {
            (true, true, false)
        } else {
            (false, true, false)
        };
        if c3 {
            self.fpu_sw |= 0x4000;
        }
        if c2 {
            self.fpu_sw |= 0x0400;
        }
        if c0 {
            self.fpu_sw |= 0x0100;
        }
    }

    fn fpu_compare(&mut self, a: F80, b: F80) {
        // Clear C3 (0x4000), C2 (0x0400), C1 (0x0200), C0 (0x0100). The
        // SDM requires FCOM/FUCOM/FTST to clear C1 (no stack fault here).
        self.fpu_sw &= !0x4700;
        match a.partial_cmp(b) {
            None => self.fpu_sw |= 0x4500,                 // unordered: C3=C2=C0=1
            Some(Ordering::Greater) => {}                  // all three already 0
            Some(Ordering::Less) => self.fpu_sw |= 0x0100, // C0
            Some(Ordering::Equal) => self.fpu_sw |= 0x4000, // C3 (equal)
        }
    }

    /// Set the FPREM/FPREM1 condition codes from the integer quotient
    /// `q`. The reduction completes in one step here, so C2 is cleared
    /// ("complete"), and the low three quotient bits are exposed per
    /// the Intel SDM mapping: C0←bit2, C3←bit1, C1←bit0. Code that
    /// does argument reduction (glibc's fmod/remainder, range-reduce
    /// loops) reads these to reconstruct the quotient modulo 8.
    fn fpu_set_prem_cc(&mut self, q: F80) {
        // Clear C3 | C2 | C1 | C0; C2 stays 0 = reduction complete.
        self.fpu_sw &= !0x4700;
        let qi = q.abs().to_i64_trunc() as u64; // NaN/inf → indefinite, low bits irrelevant
        if qi & 0b001 != 0 {
            self.fpu_sw |= 0x0200; // C1 ← quotient bit 0
        }
        if qi & 0b010 != 0 {
            self.fpu_sw |= 0x4000; // C3 ← quotient bit 1
        }
        if qi & 0b100 != 0 {
            self.fpu_sw |= 0x0100; // C0 ← quotient bit 2
        }
    }

    /// Fetch the direct memory offset that follows a `moffs`-form MOV
    /// (0xA0..0xA3). 16-bit under the default address size, 32-bit
    /// when the 0x67 prefix set `addr_size_32`.
    fn fetch_moffs(&mut self, mem: &Memory) -> u32 {
        if self.addr_size_32 {
            let lo = self.fetch_u16(mem) as u32;
            let hi = self.fetch_u16(mem) as u32;
            lo | (hi << 16)
        } else {
            self.fetch_u16(mem) as u32
        }
    }

    fn set_flag(&mut self, mask: u16, value: bool) {
        if value {
            self.flags |= mask;
        } else {
            self.flags &= !mask;
        }
    }

    fn has(&self, mask: u16) -> bool {
        self.flags & mask != 0
    }

    /// True iff a literal `0x66` prefix preceded this instruction. For
    /// the 0F SSE map the `0x66` is a MANDATORY prefix selecting the
    /// PD / packed-integer form (not an operand-size override), so the
    /// selection must key on the literal prefix, NOT the effective
    /// operand size. `op_size_32` defaults to `code_size_32` and `0x66`
    /// flips it, so `op_size_32 != code_size_32` exactly recovers
    /// "0x66 was present" in both real and 32-bit protected mode.
    fn has_66(&self) -> bool {
        self.op_size_32 != self.code_size_32
    }

    /// Update ZF/SF/PF after an 8-bit logical op. Clears CF and OF.
    fn flags_logic8(&mut self, result: u8) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x80 != 0);
        self.set_flag(flag::PF, (result.count_ones() & 1) == 0);
        self.set_flag(flag::CF, false);
        self.set_flag(flag::OF, false);
    }

    fn flags_logic16(&mut self, result: u16) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        // PF only reflects the low byte on x86.
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::CF, false);
        self.set_flag(flag::OF, false);
    }

    fn flags_logic32(&mut self, result: u32) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::CF, false);
        self.set_flag(flag::OF, false);
    }

    fn flags_add8(&mut self, a: u8, b: u8, cin: u8, result: u8) {
        let sum = a as u16 + b as u16 + cin as u16;
        self.set_flag(flag::CF, sum > 0xFF);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x80 != 0);
        self.set_flag(flag::PF, (result.count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ result) & (b ^ result) & 0x80) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    fn flags_add16(&mut self, a: u16, b: u16, cin: u16, result: u16) {
        let sum = a as u32 + b as u32 + cin as u32;
        self.set_flag(flag::CF, sum > 0xFFFF);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ result) & (b ^ result) & 0x8000) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    fn flags_add32(&mut self, a: u32, b: u32, cin: u32, result: u32) {
        let sum = a as u64 + b as u64 + cin as u64;
        self.set_flag(flag::CF, sum > 0xFFFF_FFFF);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ result) & (b ^ result) & 0x8000_0000) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    fn flags_sub8(&mut self, a: u8, b: u8, bin: u8, result: u8) {
        let borrow = (a as i16) - (b as i16) - (bin as i16);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x80 != 0);
        self.set_flag(flag::PF, (result.count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x80) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    fn flags_sub16(&mut self, a: u16, b: u16, bin: u16, result: u16) {
        let borrow = (a as i32) - (b as i32) - (bin as i32);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x8000) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    fn flags_sub32(&mut self, a: u32, b: u32, bin: u32, result: u32) {
        let borrow = (a as i64) - (b as i64) - (bin as i64);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x8000_0000) != 0);
        self.set_flag(flag::AF, ((a ^ b ^ result) & 0x10) != 0);
    }

    /// Decode a 16-bit ModR/M effective address. `mode` must be 0b00,
    /// 0b01 or 0b10 — the 0b11 case is "register, not memory" and the
    /// caller dispatches it separately. Advances IP past any disp.
    ///
    /// Honors `self.seg_override` if set: a `CS:`/`DS:`/`ES:`/`SS:`
    /// prefix replaces the default segment that the rm encoding would
    /// otherwise pick (SS for `[BP*]`, DS for everything else).
    fn compute_ea(&mut self, mode: u8, rm: u8, mem: &Memory) -> EffAddr {
        if mode == 0b00 && rm == 0b110 {
            let off = self.fetch_u16(mem) as u32;
            let seg = self.seg_override.unwrap_or(sreg::DS);
            return EffAddr { seg, off };
        }
        let (base, default_ss) = match rm {
            0b000 => (self.regs[r16::BX].wrapping_add(self.regs[r16::SI]), false),
            0b001 => (self.regs[r16::BX].wrapping_add(self.regs[r16::DI]), false),
            0b010 => (self.regs[r16::BP].wrapping_add(self.regs[r16::SI]), true),
            0b011 => (self.regs[r16::BP].wrapping_add(self.regs[r16::DI]), true),
            0b100 => (self.regs[r16::SI], false),
            0b101 => (self.regs[r16::DI], false),
            0b110 => (self.regs[r16::BP], true),
            0b111 => (self.regs[r16::BX], false),
            _ => unreachable!("rm is 3 bits"),
        };
        let disp = match mode {
            0b00 => 0,
            0b01 => self.fetch_u8(mem) as i8 as i16 as u16,
            0b10 => self.fetch_u16(mem),
            _ => unreachable!("mode is 2 bits, caller filters 0b11"),
        };
        let default_seg = if default_ss { sreg::SS } else { sreg::DS };
        EffAddr {
            seg: self.seg_override.unwrap_or(default_seg),
            off: base.wrapping_add(disp) as u32,
        }
    }

    /// 32-bit ModR/M effective address. Decodes per the Intel SDM
    /// "Addressing with 32-Bit Addresses" table. Notable differences
    /// from 16-bit:
    ///
    ///   * `rm == 4` means a SIB byte follows (Scale-Index-Base).
    ///   * `mode == 00 && rm == 5` means disp32 only (no base reg).
    ///   * `mode == 01 && rm == 5` means [EBP + disp8], not [DI].
    ///   * Displacements are 8- or 32-bit (not 16-bit).
    ///
    /// Default segment is DS unless the base reg is EBP/ESP (then SS).
    fn compute_ea_32(&mut self, mode: u8, rm: u8, mem: &Memory) -> EffAddr {
        if mode == 0b00 && rm == 0b101 {
            // disp32 only.
            let lo = self.fetch_u16(mem) as u32;
            let hi = self.fetch_u16(mem) as u32;
            let off = lo | (hi << 16);
            let seg = self.seg_override.unwrap_or(sreg::DS);
            return EffAddr { seg, off };
        }
        let (mut base, mut default_ss) = if rm == 0b100 {
            // SIB byte.
            let sib = self.fetch_u8(mem);
            let scale = sib >> 6;
            let index = (sib >> 3) & 0x07;
            let base_reg = sib & 0x07;
            // index == 4 means "no index". Other index values read
            // the indicated r32 and scale it by 1/2/4/8.
            let index_val = if index == 0b100 {
                0u32
            } else {
                self.read_r32(index) << scale
            };
            let (base_val, ss) = if mode == 0b00 && base_reg == 0b101 {
                // disp32 base, no register base.
                let lo = self.fetch_u16(mem) as u32;
                let hi = self.fetch_u16(mem) as u32;
                (lo | (hi << 16), false)
            } else {
                let ss = base_reg == 0b100 || base_reg == 0b101;
                (self.read_r32(base_reg), ss)
            };
            (base_val.wrapping_add(index_val), ss)
        } else {
            // Plain register base. mode==00 && rm==5 already
            // returned above; here EBP forces SS.
            let ss = rm == 0b101;
            (self.read_r32(rm), ss)
        };
        let disp: u32 = match mode {
            0b00 => 0,
            0b01 => self.fetch_u8(mem) as i8 as i32 as u32,
            0b10 => {
                let lo = self.fetch_u16(mem) as u32;
                let hi = self.fetch_u16(mem) as u32;
                lo | (hi << 16)
            }
            _ => unreachable!("mode is 2 bits, caller filters 0b11"),
        };
        base = base.wrapping_add(disp);
        // EBP with disp8/disp32 (mode != 00) still defaults to SS.
        if mode != 0b00 && rm == 0b101 {
            default_ss = true;
        }
        let default_seg = if default_ss { sreg::SS } else { sreg::DS };
        EffAddr {
            seg: self.seg_override.unwrap_or(default_seg),
            off: base,
        }
    }

    /// Fetch a ModR/M byte and resolve the r/m side into a [`Rm`]. The
    /// returned tuple is (mode, reg, rm) where `reg` is the 3-bit
    /// register field for the opposite operand and `mode` is kept for
    /// instructions whose group decoding looks at it.
    fn fetch_modrm(&mut self, mem: &Memory) -> (u8, u8, Rm) {
        let byte = self.fetch_u8(mem);
        let mode = byte >> 6;
        let reg = (byte >> 3) & 0x07;
        let rm_field = byte & 0x07;
        let rm = if mode == 0b11 {
            Rm::Reg(rm_field)
        } else if self.addr_size_32 {
            Rm::Mem(self.compute_ea_32(mode, rm_field, mem))
        } else {
            Rm::Mem(self.compute_ea(mode, rm_field, mem))
        };
        (mode, reg, rm)
    }

    fn read_rm8(&self, rm: Rm, mem: &Memory) -> u8 {
        match rm {
            Rm::Reg(i) => self.read_r8(i),
            Rm::Mem(ea) => self.mem_read_u8(mem, self.linear_seg(ea.seg, ea.off)),
        }
    }
    fn write_rm8(&mut self, rm: Rm, mem: &mut Memory, value: u8) {
        match rm {
            Rm::Reg(i) => self.write_r8(i, value),
            Rm::Mem(ea) => self.mem_write_u8(mem, self.linear_seg(ea.seg, ea.off), value),
        }
    }
    fn read_rm16(&self, rm: Rm, mem: &Memory) -> u16 {
        match rm {
            Rm::Reg(i) => self.read_r16(i),
            Rm::Mem(ea) => self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off)),
        }
    }
    fn write_rm16(&mut self, rm: Rm, mem: &mut Memory, value: u16) {
        match rm {
            Rm::Reg(i) => self.write_r16(i, value),
            Rm::Mem(ea) => self.mem_write_u16(mem, self.linear_seg(ea.seg, ea.off), value),
        }
    }

    /// Read 32-bit value through an `Rm`. A memory dword goes through
    /// `mem_read_u32`, which translates once for the whole (same-page) span
    /// — `mov r32,[mem]` is ubiquitous in 32-bit code, so this avoids the
    /// redundant second translation the old two-u16-reads form did.
    fn read_rm32(&self, rm: Rm, mem: &Memory) -> u32 {
        match rm {
            Rm::Reg(i) => self.read_r32(i),
            Rm::Mem(ea) => self.mem_read_u32(mem, self.linear_seg(ea.seg, ea.off)),
        }
    }

    /// Write 32-bit value through an `Rm`. Memory dword = two 16-bit
    /// writes at `off` and `off+2`.
    fn write_rm32(&mut self, rm: Rm, mem: &mut Memory, value: u32) {
        match rm {
            Rm::Reg(i) => self.write_r32(i, value),
            Rm::Mem(ea) => {
                let base = self.linear_seg(ea.seg, ea.off);
                // Single mem_write_u32 (which uses the per-write
                // mini-TLB) so a 4-byte store that *targets* a PTE
                // doesn't see its own first-byte effect on the
                // remaining three bytes — see mem_write_aligned.
                self.mem_write_u32(mem, base, value);
            }
        }
    }

    /// Take a software interrupt. In real mode reads the 4-byte IVT
    /// entry at linear `n*4`. In protected mode (CR0.PE=1) reads an
    /// 8-byte gate descriptor at `idtr.base + n*8`:
    ///
    /// ```text
    ///   byte 0-1: offset 15:0
    ///   byte 2-3: segment selector
    ///   byte 4:   reserved (0)
    ///   byte 5:   P|DPL|S|type — 0x86 = present, ring 0, 16-bit interrupt gate
    ///   byte 6-7: offset 31:16 (0 for 16-bit gates)
    /// ```
    ///
    /// Interrupt gates (type 0x6 / 0xE) clear IF on entry so the
    /// handler can't be re-entered by another IRQ before it's done.
    /// Trap gates (type 0x7 / 0xF) leave IF as-is — Linux uses them
    /// for #DB and #BP so a debugger probe doesn't silently disable
    /// the IRQ tick while you're stepping through.
    fn do_interrupt(&mut self, n: u8, mem: &mut Memory) {
        self.do_interrupt_with_error(n, None, mem);
    }

    /// Variant that also pushes an architectural error code below
    /// the IP/CS/FLAGS frame. Used for INT 14 (#PF), INT 8 (#DF),
    /// INT 10 (#TS), INT 11 (#NP), INT 12 (#SS), INT 13 (#GP). For
    /// now we only emit #PF — the other vectors will reuse this
    /// path as they come online. The error code is pushed as a 16-bit
    /// word, which is the 16-bit-handler convention; a future 32-bit
    /// handler path will widen to a 32-bit push.
    fn do_interrupt_with_error(&mut self, n: u8, error_code: Option<u32>, mem: &mut Memory) {
        // Probe trace: WWWVM_TRACE_EXC=1 prints every exception
        // (vec 0..=31) dispatch so we can see what the kernel hits
        // before it ends up in rewind_stack_and_make_dead. Also
        // reads [ESP] through paging — if EIP=0 that's a CALL to
        // NULL and [ESP] is the return address into the caller,
        // which names the bug site.
        if n < 32 && std::env::var_os("WWWVM_TRACE_EXC").is_some() {
            let esp = self.read_r32(r16::SP as u8);
            // Walk paging to read [ESP] — the return address pushed
            // by a CALL just before the fault, if there was one.
            let walk = |va: u32, mem: &Memory| -> Option<u32> {
                if self.cr0 & 0x8000_0000 == 0 {
                    return Some(va);
                }
                let cr3 = self.cr3 & 0xFFFF_F000;
                let pde = mem.read_u32(cr3.wrapping_add((va >> 22) * 4));
                if pde & 1 == 0 {
                    return None;
                }
                if pde & 0x80 != 0 {
                    return Some((pde & 0xFFC0_0000) | (va & 0x003F_FFFF));
                }
                let pte = mem.read_u32((pde & !0xFFF).wrapping_add(((va >> 12) & 0x3FF) * 4));
                if pte & 1 == 0 {
                    return None;
                }
                Some((pte & !0xFFF) | (va & 0xFFF))
            };
            let top = walk(esp, mem).map(|p| mem.read_u32(p)).unwrap_or(0);
            let top4 = walk(esp.wrapping_add(4), mem)
                .map(|p| mem.read_u32(p))
                .unwrap_or(0);
            eprintln!(
                "[EXC vec={n:#x}] CS:EIP={:04X}:{:08X} errcode={:?} CR2={:08X} ESP={:08X} [ESP]={:08X} [ESP+4]={:08X} TSC={}",
                self.sregs[sreg::CS],
                self.ip,
                error_code,
                self.cr2,
                esp,
                top,
                top4,
                self.tsc
            );
        }
        // In PE, the gate's type bits (low nibble of the access byte
        // at descriptor offset 5) pick the frame width and whether
        // IF gets cleared:
        //   0x6 / 0x7 → 16-bit interrupt / trap gate
        //   0xE / 0xF → 32-bit interrupt / trap gate
        //   bit 0 set → trap gate (keep IF)  ; bit 0 clear → interrupt
        //   bit 3 set → 32-bit frame
        // Real-mode (PE=0) IVT is always the 16-bit 4-byte form, and
        // INT in real mode clears IF (matches real-silicon real mode).
        let (new_cs, new_ip, gate_is_32, is_trap) = if self.cr0 & 1 == 0 {
            let ivt_addr = (n as u32) * 4;
            (
                self.mem_read_u16(mem, ivt_addr + 2),
                self.mem_read_u16(mem, ivt_addr) as u32,
                false,
                false,
            )
        } else {
            let gate_addr = self.idtr.base.wrapping_add((n as u32) * 8);
            let off_lo = self.mem_read_u16(mem, gate_addr) as u32;
            let selector = self.mem_read_u16(mem, gate_addr.wrapping_add(2));
            let access = self.mem_read_u8(mem, gate_addr.wrapping_add(5));
            let off_hi = self.mem_read_u16(mem, gate_addr.wrapping_add(6)) as u32;
            let is_32 = (access & 0x0F) >= 0x0E;
            let is_trap = (access & 0x01) != 0;
            let off = if is_32 {
                off_lo | (off_hi << 16)
            } else {
                off_lo
            };
            (selector, off, is_32, is_trap)
        };
        let flags = self.flags;
        let cs = self.sregs[sreg::CS];
        // Cross-ring entry: when a PE-mode interrupt fires from
        // CPL > 0 (user space) into a handler at a lower CPL, real
        // silicon loads SS0:ESP0 from the active TSS *before*
        // pushing the IRET frame, then also pushes the user's
        // SS:ESP atop the frame. The matching IRET path (with the
        // popped CS RPL > current CPL) walks all five (or six,
        // with error code) words back. We only switch when the
        // gate is 32-bit — 16-bit cross-ring entries are
        // vanishingly rare in real kernels, and supporting both
        // would just double the frame-layout code path.
        let cpl_before = if self.cr0 & 1 != 0 { (cs & 3) as u8 } else { 0 };
        let cross_ring = gate_is_32 && cpl_before > 0;
        let (old_ss, old_esp) = if cross_ring {
            let old_ss = self.sregs[sreg::SS];
            let old_esp = self.read_r32(r16::SP as u8);
            // Read ESP0 / SS0 from the 32-bit-TSS layout. Base
            // comes from the descriptor that LTR latched into TR.
            let tss_base = self.tss_base(mem);
            let new_esp = self.mem_read_u32(mem, tss_base.wrapping_add(4));
            let new_ss = self.mem_read_u16(mem, tss_base.wrapping_add(8));
            // Switch to the kernel stack before pushing any frame.
            self.write_sreg(sreg::SS, new_ss, mem);
            self.write_r32(r16::SP as u8, new_esp);
            (Some(old_ss), Some(old_esp))
        } else {
            (None, None)
        };
        if gate_is_32 {
            // Push the caller's SS:ESP first when we switched stacks,
            // so an IRET back to ring 3 walks them last.
            if let (Some(ss), Some(esp)) = (old_ss, old_esp) {
                self.push32(mem, ss as u32);
                self.push32(mem, esp);
            }
            self.push32(mem, flags as u32);
            self.push32(mem, cs as u32);
            self.push32(mem, self.ip);
            if let Some(ec) = error_code {
                self.push32(mem, ec);
            }
        } else {
            // 16-bit frame: word pushes. IP truncated to low 16.
            self.push16(mem, flags);
            self.push16(mem, cs);
            self.push16(mem, self.ip as u16);
            if let Some(ec) = error_code {
                self.push16(mem, ec as u16);
            }
        }
        // Interrupt gates clear IF so the handler runs with IRQs
        // off; trap gates leave IF as-is so debugger probes don't
        // silently disable IRQs while the handler runs. Real-mode
        // INT clears IF unconditionally (is_trap = false above).
        if !is_trap {
            self.set_flag(flag::IF, false);
        }
        // TF is not modeled yet — when it is, this is also where it
        // gets cleared.
        self.write_sreg(sreg::CS, new_cs, mem);
        self.ip = new_ip;
    }

    /// Look up the linear base of the currently-active TSS by
    /// decoding the descriptor that TR points at in the GDT. The
    /// 32-bit-TSS layout (the one Linux uses) starts with the
    /// previous-task link at offset 0 and has ESP0 at offset 4,
    /// SS0 at offset 8 — that's what `do_interrupt_with_error`
    /// reads on a cross-ring entry.
    fn tss_base(&self, mem: &Memory) -> u32 {
        let desc_addr = self.gdtr.base.wrapping_add((self.tr & 0xFFF8) as u32);
        let d1 = self.mem_read_u16(mem, desc_addr.wrapping_add(2)) as u32;
        let d2 = self.mem_read_u16(mem, desc_addr.wrapping_add(4)) as u32;
        let d3 = self.mem_read_u16(mem, desc_addr.wrapping_add(6)) as u32;
        d1 | ((d2 & 0x00FF) << 16) | ((d3 & 0xFF00) << 16)
    }

    /// Read the stack pointer at its current width. Returns the
    /// full ESP when `stack_size_32` is set, otherwise just SP
    /// zero-extended to u32 (which is what the SS:SP linear lookup
    /// expects on real-mode/16-bit stacks).
    fn read_stack_ptr(&self) -> u32 {
        if self.stack_size_32 {
            self.read_r32(r16::SP as u8)
        } else {
            self.regs[r16::SP] as u32
        }
    }

    /// Write the stack pointer at its current width.
    fn write_stack_ptr(&mut self, value: u32) {
        if self.stack_size_32 {
            self.write_r32(r16::SP as u8, value);
        } else {
            self.regs[r16::SP] = value as u16;
        }
    }

    /// Push a 16-bit value onto SS:[ESP|SP]. The stack pointer
    /// decrements by 2 before the write — but only if the write
    /// itself completes without a page fault. On real x86 a #PF on
    /// the destination aborts the instruction; SP must therefore
    /// stay at its pre-push value so that IRET from the kernel's
    /// #PF handler resumes at a sane stack pointer instead of one
    /// that has drifted downward through millions of repeated
    /// faulting pushes (Linux's recursive-fault scenario).
    fn push16(&mut self, mem: &mut Memory, value: u16) {
        if self.pending_fault.get().is_some() {
            return; // earlier push in this op already faulted
        }
        let new_sp = self.read_stack_ptr().wrapping_sub(2);
        self.mem_write_u16(mem, self.linear_seg(sreg::SS, new_sp), value);
        if self.pending_fault.get().is_some() {
            return;
        }
        self.write_stack_ptr(new_sp);
    }

    /// Pop a 16-bit value from SS:[ESP|SP]. SP increments *after*
    /// the read.
    fn pop16(&mut self, mem: &Memory) -> u16 {
        let sp = self.read_stack_ptr();
        let v = self.mem_read_u16(mem, self.linear_seg(sreg::SS, sp));
        self.write_stack_ptr(sp.wrapping_add(2));
        v
    }

    /// Push a 32-bit value onto SS:[ESP|SP]. SP decrements by 4
    /// before the write — but only if the write completes without a
    /// fault (see push16 for the rationale).
    fn push32(&mut self, mem: &mut Memory, value: u32) {
        if self.pending_fault.get().is_some() {
            return; // earlier push in this op already faulted
        }
        let new_sp = self.read_stack_ptr().wrapping_sub(4);
        self.mem_write_u32(mem, self.linear_seg(sreg::SS, new_sp), value);
        if self.pending_fault.get().is_some() {
            return;
        }
        self.write_stack_ptr(new_sp);
    }

    /// Pop a 32-bit value from SS:[ESP|SP]. SP increments by 4
    /// after the read.
    fn pop32(&mut self, mem: &Memory) -> u32 {
        let sp = self.read_stack_ptr();
        let v = self.mem_read_u32(mem, self.linear_seg(sreg::SS, sp));
        self.write_stack_ptr(sp.wrapping_add(4));
        v
    }

    /// Compute one of the 8 standard ALU ops on 8-bit operands and
    /// update flags. Returns (result, true) for ADD/OR/ADC/SBB/AND/SUB/
    /// XOR (writeback) or (result, false) for CMP. `op` is the same
    /// 0..7 encoding used by both the main ALU dispatch and Group 1.
    fn alu_apply8(&mut self, op: u8, a: u8, b: u8) -> (u8, bool) {
        let cin = if (op == 2 || op == 3) && self.has(flag::CF) {
            1
        } else {
            0
        };
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_add8(a, b, 0, r);
                (r, true)
            }
            1 => {
                let r = a | b;
                self.flags_logic8(r);
                (r, true)
            }
            2 => {
                let r = a.wrapping_add(b).wrapping_add(cin);
                self.flags_add8(a, b, cin, r);
                (r, true)
            }
            3 => {
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                self.flags_sub8(a, b, cin, r);
                (r, true)
            }
            4 => {
                let r = a & b;
                self.flags_logic8(r);
                (r, true)
            }
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_sub8(a, b, 0, r);
                (r, true)
            }
            6 => {
                let r = a ^ b;
                self.flags_logic8(r);
                (r, true)
            }
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_sub8(a, b, 0, r);
                (r, false)
            }
            _ => unreachable!("op is 3 bits"),
        }
    }

    /// Set ZF/SF/PF from an 8-bit result without touching CF/OF.
    /// Used by shifts where CF/OF have their own per-op meanings.
    fn flags_zsp8(&mut self, result: u8) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x80 != 0);
        self.set_flag(flag::PF, (result.count_ones() & 1) == 0);
    }

    fn flags_zsp16(&mut self, result: u16) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
    }

    fn flags_zsp32(&mut self, result: u32) {
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
    }

    /// Group 2 shift/rotate on an 8-bit operand. `sub` is the ModR/M
    /// reg field: 0=ROL, 1=ROR, 2=RCL, 3=RCR, 4=SHL, 5=SHR, 7=SAR.
    /// RCL/RCR are intentionally not implemented yet.
    fn shift_apply8(&mut self, sub: u8, value: u8, count_raw: u8) -> Result<u8, CpuError> {
        // 80186+ masks the count to 0x1F. A count of zero is a complete
        // no-op (no flag changes either).
        let count = count_raw & 0x1F;
        if count == 0 {
            return Ok(value);
        }
        match sub {
            // ROL — left rotate, CF = LSB of result; OF (count=1) = msb(res) xor CF
            0 => {
                let result = value.rotate_left(count as u32);
                let cf = result & 1 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x80 != 0) != cf);
                }
                Ok(result)
            }
            // ROR — right rotate, CF = MSB of result; OF (count=1) = msb xor (msb-1)
            1 => {
                let result = value.rotate_right(count as u32);
                let cf = result & 0x80 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb1 = result & 0x40 != 0;
                    self.set_flag(flag::OF, cf != msb1);
                }
                Ok(result)
            }
            // SHL/SAL — both ops, identical encoding (4 standard, 6 alias)
            4 | 6 => {
                let cf = if count <= 8 {
                    ((value as u16) >> (8 - count)) & 1 != 0
                } else {
                    false
                };
                let result = if count >= 8 { 0 } else { value << count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x80 != 0) != cf);
                }
                self.flags_zsp8(result);
                Ok(result)
            }
            // SHR — logical right shift
            5 => {
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 8 { 0 } else { value >> count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, value & 0x80 != 0);
                }
                self.flags_zsp8(result);
                Ok(result)
            }
            // SAR — arithmetic right shift, sign-extends
            7 => {
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 8 {
                    if value & 0x80 != 0 {
                        0xFF
                    } else {
                        0
                    }
                } else {
                    ((value as i8) >> count) as u8
                };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, false);
                }
                self.flags_zsp8(result);
                Ok(result)
            }
            // RCL — rotate through CF, 9-bit cycle (8 data bits + CF).
            2 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = count % 9;
                for _ in 0..n {
                    let new_cf = v & 0x80 != 0;
                    v = (v << 1) | (cf as u8);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (v & 0x80 != 0) != cf);
                }
                Ok(v)
            }
            // RCR — rotate right through CF.
            3 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = count % 9;
                for _ in 0..n {
                    let new_cf = v & 1 != 0;
                    v = (v >> 1) | ((cf as u8) << 7);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb = v & 0x80 != 0;
                    let msb1 = v & 0x40 != 0;
                    self.set_flag(flag::OF, msb != msb1);
                }
                Ok(v)
            }
            _ => Err(CpuError::Unimplemented {
                opcode: 0xD0,
                cs: 0,
                ip: 0,
            }),
        }
    }

    fn shift_apply16(&mut self, sub: u8, value: u16, count_raw: u8) -> Result<u16, CpuError> {
        let count = count_raw & 0x1F;
        if count == 0 {
            return Ok(value);
        }
        match sub {
            0 => {
                let result = value.rotate_left(count as u32);
                let cf = result & 1 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x8000 != 0) != cf);
                }
                Ok(result)
            }
            1 => {
                let result = value.rotate_right(count as u32);
                let cf = result & 0x8000 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb1 = result & 0x4000 != 0;
                    self.set_flag(flag::OF, cf != msb1);
                }
                Ok(result)
            }
            4 | 6 => {
                let cf = if count <= 16 {
                    ((value as u32) >> (16 - count)) & 1 != 0
                } else {
                    false
                };
                let result = if count >= 16 { 0 } else { value << count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x8000 != 0) != cf);
                }
                self.flags_zsp16(result);
                Ok(result)
            }
            5 => {
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 16 { 0 } else { value >> count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, value & 0x8000 != 0);
                }
                self.flags_zsp16(result);
                Ok(result)
            }
            7 => {
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 16 {
                    if value & 0x8000 != 0 {
                        0xFFFF
                    } else {
                        0
                    }
                } else {
                    ((value as i16) >> count) as u16
                };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, false);
                }
                self.flags_zsp16(result);
                Ok(result)
            }
            // RCL — 17-bit cycle (16 data + CF).
            2 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = count % 17;
                for _ in 0..n {
                    let new_cf = v & 0x8000 != 0;
                    v = (v << 1) | (cf as u16);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (v & 0x8000 != 0) != cf);
                }
                Ok(v)
            }
            // RCR
            3 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = count % 17;
                for _ in 0..n {
                    let new_cf = v & 1 != 0;
                    v = (v >> 1) | ((cf as u16) << 15);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb = v & 0x8000 != 0;
                    let msb1 = v & 0x4000 != 0;
                    self.set_flag(flag::OF, msb != msb1);
                }
                Ok(v)
            }
            _ => Err(CpuError::Unimplemented {
                opcode: 0xD1,
                cs: 0,
                ip: 0,
            }),
        }
    }

    /// 32-bit shift/rotate dispatch. Mirrors `shift_apply16` with a
    /// wider operand. Count is masked to 5 bits per Intel.
    fn shift_apply32(&mut self, sub: u8, value: u32, count_raw: u8) -> Result<u32, CpuError> {
        let count = count_raw & 0x1F;
        if count == 0 {
            return Ok(value);
        }
        match sub {
            0 => {
                // ROL
                let result = value.rotate_left(count as u32);
                let cf = result & 1 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x8000_0000 != 0) != cf);
                }
                Ok(result)
            }
            1 => {
                // ROR
                let result = value.rotate_right(count as u32);
                let cf = result & 0x8000_0000 != 0;
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb1 = result & 0x4000_0000 != 0;
                    self.set_flag(flag::OF, cf != msb1);
                }
                Ok(result)
            }
            4 | 6 => {
                // SHL / SAL
                let cf = ((value as u64) >> (32 - count as u64)) & 1 != 0;
                let result = if count >= 32 { 0 } else { value << count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (result & 0x8000_0000 != 0) != cf);
                }
                self.flags_zsp32(result);
                Ok(result)
            }
            5 => {
                // SHR
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 32 { 0 } else { value >> count };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, value & 0x8000_0000 != 0);
                }
                self.flags_zsp32(result);
                Ok(result)
            }
            7 => {
                // SAR
                let cf = (value >> (count - 1)) & 1 != 0;
                let result = if count >= 32 {
                    if value & 0x8000_0000 != 0 {
                        0xFFFF_FFFF
                    } else {
                        0
                    }
                } else {
                    ((value as i32) >> count) as u32
                };
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, false);
                }
                self.flags_zsp32(result);
                Ok(result)
            }
            // RCL — 33-bit cycle (32 data + CF).
            2 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = (count as u32) % 33;
                for _ in 0..n {
                    let new_cf = v & 0x8000_0000 != 0;
                    v = (v << 1) | (cf as u32);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    self.set_flag(flag::OF, (v & 0x8000_0000 != 0) != cf);
                }
                Ok(v)
            }
            // RCR
            3 => {
                let mut v = value;
                let mut cf = self.has(flag::CF);
                let n = (count as u32) % 33;
                for _ in 0..n {
                    let new_cf = v & 1 != 0;
                    v = (v >> 1) | ((cf as u32) << 31);
                    cf = new_cf;
                }
                self.set_flag(flag::CF, cf);
                if count == 1 {
                    let msb = v & 0x8000_0000 != 0;
                    let msb1 = v & 0x4000_0000 != 0;
                    self.set_flag(flag::OF, msb != msb1);
                }
                Ok(v)
            }
            _ => Err(CpuError::Unimplemented {
                opcode: 0xD1,
                cs: 0,
                ip: 0,
            }),
        }
    }

    /// Per-iteration SI/DI delta for string ops, picked by DF
    /// (1 → backward). `width` is the operand size in bytes (1/2/4).
    /// Returns u32 with sign-extension so 32-bit-address-mode index
    /// updates compute correctly.
    fn string_delta_n_u32(&self, width: u32) -> u32 {
        if self.has(flag::DF) {
            0u32.wrapping_sub(width)
        } else {
            width
        }
    }

    /// Read SI (16-bit) or ESI (32-bit) depending on the address-size
    /// attribute. String ops use this to source from the right width.
    fn read_si_for_string(&self) -> u32 {
        if self.addr_size_32 {
            self.read_r32(r16::SI as u8)
        } else {
            self.regs[r16::SI] as u32
        }
    }

    /// Read DI (16-bit) or EDI (32-bit) depending on the address-size
    /// attribute.
    fn read_di_for_string(&self) -> u32 {
        if self.addr_size_32 {
            self.read_r32(r16::DI as u8)
        } else {
            self.regs[r16::DI] as u32
        }
    }

    /// Advance SI/ESI by `delta`. `delta` is u32; in 16-bit mode the
    /// low 16 bits get added to the 16-bit register; in 32-bit mode
    /// the full 32-bit ESI is updated.
    fn advance_si_for_string(&mut self, delta: u32) {
        if self.addr_size_32 {
            let new = self.read_r32(r16::SI as u8).wrapping_add(delta);
            self.write_r32(r16::SI as u8, new);
        } else {
            self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(delta as u16);
        }
    }

    fn advance_di_for_string(&mut self, delta: u32) {
        if self.addr_size_32 {
            let new = self.read_r32(r16::DI as u8).wrapping_add(delta);
            self.write_r32(r16::DI as u8, new);
        } else {
            self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(delta as u16);
        }
    }

    /// Segment used for the SI side of string ops — DS by default, but
    /// honors a segment override prefix. The DI side always uses ES,
    /// which cannot be overridden on real x86.
    fn string_src_seg(&self) -> usize {
        self.seg_override.unwrap_or(sreg::DS)
    }

    fn step_movsb(&mut self, mem: &mut Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let v = self.mem_read_u8(mem, src);
        self.mem_write_u8(mem, dst, v);
        let d = self.string_delta_n_u32(1);
        self.advance_si_for_string(d);
        self.advance_di_for_string(d);
    }
    fn step_movsw(&mut self, mem: &mut Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let v = self.mem_read_u16(mem, src);
        self.mem_write_u16(mem, dst, v);
        let d = self.string_delta_n_u32(2);
        self.advance_si_for_string(d);
        self.advance_di_for_string(d);
    }
    fn step_stosb(&mut self, mem: &mut Memory) {
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let al = self.read_r8(0);
        self.mem_write_u8(mem, dst, al);
        let d = self.string_delta_n_u32(1);
        self.advance_di_for_string(d);
    }
    fn step_stosw(&mut self, mem: &mut Memory) {
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let ax = self.regs[r16::AX];
        self.mem_write_u16(mem, dst, ax);
        let d = self.string_delta_n_u32(2);
        self.advance_di_for_string(d);
    }
    fn step_lodsb(&mut self, mem: &Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let v = self.mem_read_u8(mem, src);
        self.write_r8(0, v);
        let d = self.string_delta_n_u32(1);
        self.advance_si_for_string(d);
    }
    fn step_lodsw(&mut self, mem: &Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let v = self.mem_read_u16(mem, src);
        self.regs[r16::AX] = v;
        let d = self.string_delta_n_u32(2);
        self.advance_si_for_string(d);
    }
    fn step_scasb(&mut self, mem: &Memory) {
        let addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let v = self.mem_read_u8(mem, addr);
        let al = self.read_r8(0);
        let r = al.wrapping_sub(v);
        self.flags_sub8(al, v, 0, r);
        let d = self.string_delta_n_u32(1);
        self.advance_di_for_string(d);
    }
    fn step_scasw(&mut self, mem: &Memory) {
        let addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let v = self.mem_read_u16(mem, addr);
        let ax = self.regs[r16::AX];
        let r = ax.wrapping_sub(v);
        self.flags_sub16(ax, v, 0, r);
        let d = self.string_delta_n_u32(2);
        self.advance_di_for_string(d);
    }
    fn step_cmpsb(&mut self, mem: &Memory) {
        let s = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let d_addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let a = self.mem_read_u8(mem, s);
        let b = self.mem_read_u8(mem, d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub8(a, b, 0, r);
        let delta = self.string_delta_n_u32(1);
        self.advance_si_for_string(delta);
        self.advance_di_for_string(delta);
    }
    fn step_cmpsw(&mut self, mem: &Memory) {
        let s = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let d_addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let a = self.mem_read_u16(mem, s);
        let b = self.mem_read_u16(mem, d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub16(a, b, 0, r);
        let delta = self.string_delta_n_u32(2);
        self.advance_si_for_string(delta);
        self.advance_di_for_string(delta);
    }

    // 32-bit string ops — selected by the 0x66 prefix on top of the
    // word-form opcodes (0xA5/0xA7/0xAB/0xAD/0xAF). Linux memcpy uses
    // `REP MOVSL` (= REP MOVSD) for bulk dword copies; memset uses
    // `REP STOSL` similarly.
    fn step_movsd(&mut self, mem: &mut Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let v = self.mem_read_u32(mem, src);
        self.mem_write_u32(mem, dst, v);
        let d = self.string_delta_n_u32(4);
        self.advance_si_for_string(d);
        self.advance_di_for_string(d);
    }
    fn step_stosd(&mut self, mem: &mut Memory) {
        let dst = self.linear_seg(sreg::ES, self.read_di_for_string());
        let eax = self.read_r32(0);
        self.mem_write_u32(mem, dst, eax);
        let d = self.string_delta_n_u32(4);
        self.advance_di_for_string(d);
    }
    fn step_lodsd(&mut self, mem: &Memory) {
        let src = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let v = self.mem_read_u32(mem, src);
        self.write_r32(0, v);
        let d = self.string_delta_n_u32(4);
        self.advance_si_for_string(d);
    }
    fn step_scasd(&mut self, mem: &Memory) {
        let d_addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let a = self.read_r32(0);
        let b = self.mem_read_u32(mem, d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub32(a, b, 0, r);
        let d = self.string_delta_n_u32(4);
        self.advance_di_for_string(d);
    }
    fn step_cmpsd(&mut self, mem: &Memory) {
        let s = self.linear_seg(self.string_src_seg(), self.read_si_for_string());
        let d_addr = self.linear_seg(sreg::ES, self.read_di_for_string());
        let a = self.mem_read_u32(mem, s);
        let b = self.mem_read_u32(mem, d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub32(a, b, 0, r);
        let delta = self.string_delta_n_u32(4);
        self.advance_si_for_string(delta);
        self.advance_di_for_string(delta);
    }

    /// Dispatch a single string op by primary opcode. Returns true if
    /// the opcode is a recognized string op (callers like the REP
    /// prefix handler use this to know whether the prefix is valid).
    /// Word-form opcodes (0xA5/0xA7/0xAB/0xAD/0xAF) become their
    /// dword equivalents when `op_size_32` is set by a 0x66 prefix.
    fn step_string(&mut self, inner: u8, mem: &mut Memory) -> bool {
        match inner {
            0xA4 => self.step_movsb(mem),
            0xA5 => {
                if self.op_size_32 {
                    self.step_movsd(mem)
                } else {
                    self.step_movsw(mem)
                }
            }
            0xA6 => self.step_cmpsb(mem),
            0xA7 => {
                if self.op_size_32 {
                    self.step_cmpsd(mem)
                } else {
                    self.step_cmpsw(mem)
                }
            }
            0xAA => self.step_stosb(mem),
            0xAB => {
                if self.op_size_32 {
                    self.step_stosd(mem)
                } else {
                    self.step_stosw(mem)
                }
            }
            0xAC => self.step_lodsb(mem),
            0xAD => {
                if self.op_size_32 {
                    self.step_lodsd(mem)
                } else {
                    self.step_lodsw(mem)
                }
            }
            0xAE => self.step_scasb(mem),
            0xAF => {
                if self.op_size_32 {
                    self.step_scasd(mem)
                } else {
                    self.step_scasw(mem)
                }
            }
            _ => return false,
        }
        true
    }

    /// Port-string ops (INS / OUTS, opcodes 0x6C..=0x6F). Each does
    /// one transfer between port `DX` and `ES:[EDI]` (INS) or
    /// `DS:[ESI]` (OUTS) and bumps the pointer by the operand size.
    /// REP-prefixed forms loop in the outer dispatcher. The byte
    /// variants (0x6C, 0x6E) are always 8-bit; the word/dword
    /// variants (0x6D, 0x6F) follow `op_size_32`.
    fn step_string_io(&mut self, inner: u8, mem: &mut Memory, io: &mut IoBus) {
        let port = self.regs[r16::DX];
        let step_size: u32 = match inner {
            0x6C | 0x6E => 1,
            _ if self.op_size_32 => 4,
            _ => 2,
        };
        let delta: u32 = if self.has(flag::DF) {
            (step_size as i32).wrapping_neg() as u32
        } else {
            step_size
        };
        let edi_idx = r16::DI as u8;
        let esi_idx = r16::SI as u8;
        match inner {
            // INSB / INSW / INSD — port DX → ES:[EDI]
            0x6C | 0x6D => {
                let dst = if self.addr_size_32 {
                    self.read_r32(edi_idx)
                } else {
                    self.regs[r16::DI] as u32
                };
                let linear = self.linear_seg(sreg::ES, dst);
                match step_size {
                    1 => {
                        let v = self.port_read(io, port);
                        self.mem_write_u8(mem, linear, v);
                    }
                    2 => {
                        let lo = self.port_read(io, port) as u16;
                        let hi = self.port_read(io, port.wrapping_add(1)) as u16;
                        self.mem_write_u16(mem, linear, lo | (hi << 8));
                    }
                    _ => {
                        let mut v = 0u32;
                        for i in 0..4 {
                            v |= (self.port_read(io, port.wrapping_add(i)) as u32) << (8 * i);
                        }
                        self.mem_write_u32(mem, linear, v);
                    }
                }
                if self.addr_size_32 {
                    let new = self.read_r32(edi_idx).wrapping_add(delta);
                    self.write_r32(edi_idx, new);
                } else {
                    self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(delta as u16);
                }
            }
            // OUTSB / OUTSW / OUTSD — DS:[ESI] → port DX
            0x6E | 0x6F => {
                let src_seg = self.seg_override.unwrap_or(sreg::DS);
                let src = if self.addr_size_32 {
                    self.read_r32(esi_idx)
                } else {
                    self.regs[r16::SI] as u32
                };
                let linear = self.linear_seg(src_seg, src);
                match step_size {
                    1 => {
                        let v = self.mem_read_u8(mem, linear);
                        self.port_write(io, port, v);
                    }
                    2 => {
                        let v = self.mem_read_u16(mem, linear);
                        self.port_write(io, port, v as u8);
                        self.port_write(io, port.wrapping_add(1), (v >> 8) as u8);
                    }
                    _ => {
                        let v = self.mem_read_u32(mem, linear);
                        for i in 0..4 {
                            self.port_write(io, port.wrapping_add(i), (v >> (8 * i)) as u8);
                        }
                    }
                }
                if self.addr_size_32 {
                    let new = self.read_r32(esi_idx).wrapping_add(delta);
                    self.write_r32(esi_idx, new);
                } else {
                    self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(delta as u16);
                }
            }
            _ => unreachable!(),
        }
    }

    fn alu_apply16(&mut self, op: u8, a: u16, b: u16) -> (u16, bool) {
        let cin: u16 = if (op == 2 || op == 3) && self.has(flag::CF) {
            1
        } else {
            0
        };
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_add16(a, b, 0, r);
                (r, true)
            }
            1 => {
                let r = a | b;
                self.flags_logic16(r);
                (r, true)
            }
            2 => {
                let r = a.wrapping_add(b).wrapping_add(cin);
                self.flags_add16(a, b, cin, r);
                (r, true)
            }
            3 => {
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                self.flags_sub16(a, b, cin, r);
                (r, true)
            }
            4 => {
                let r = a & b;
                self.flags_logic16(r);
                (r, true)
            }
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_sub16(a, b, 0, r);
                (r, true)
            }
            6 => {
                let r = a ^ b;
                self.flags_logic16(r);
                (r, true)
            }
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_sub16(a, b, 0, r);
                (r, false)
            }
            _ => unreachable!("op is 3 bits"),
        }
    }

    /// 32-bit version of `alu_apply16`. Identical structure with
    /// u32 operands and 32-bit flag helpers — the boilerplate is
    /// unavoidable until the helpers move behind a generic.
    fn alu_apply32(&mut self, op: u8, a: u32, b: u32) -> (u32, bool) {
        let cin: u32 = if (op == 2 || op == 3) && self.has(flag::CF) {
            1
        } else {
            0
        };
        match op {
            0 => {
                let r = a.wrapping_add(b);
                self.flags_add32(a, b, 0, r);
                (r, true)
            }
            1 => {
                let r = a | b;
                self.flags_logic32(r);
                (r, true)
            }
            2 => {
                let r = a.wrapping_add(b).wrapping_add(cin);
                self.flags_add32(a, b, cin, r);
                (r, true)
            }
            3 => {
                let r = a.wrapping_sub(b).wrapping_sub(cin);
                self.flags_sub32(a, b, cin, r);
                (r, true)
            }
            4 => {
                let r = a & b;
                self.flags_logic32(r);
                (r, true)
            }
            5 => {
                let r = a.wrapping_sub(b);
                self.flags_sub32(a, b, 0, r);
                (r, true)
            }
            6 => {
                let r = a ^ b;
                self.flags_logic32(r);
                (r, true)
            }
            7 => {
                let r = a.wrapping_sub(b);
                self.flags_sub32(a, b, 0, r);
                (r, false)
            }
            _ => unreachable!("op is 3 bits"),
        }
    }

    /// Execute one of the 8 standard ALU operations encoded in opcode
    /// 0x00..0x3F. `op` is the operation (0=ADD … 7=CMP) and `variant`
    /// (opcode & 7) selects operand form. Supports all 16-bit ModR/M
    /// memory modes plus register-direct (mod=11) and the
    /// `AL,imm8`/`AX,imm16` short forms.
    fn alu_dispatch(&mut self, opcode: u8, mem: &mut Memory) -> Result<(), CpuError> {
        let op = (opcode >> 3) & 7;
        let variant = opcode & 7;

        // OperandSize picks the width for this ALU dispatch. Byte
        // for variants 0/2/4; Word/Dword for 1/3/5 depending on the
        // 0x66 operand-size prefix.
        #[derive(Copy, Clone, PartialEq, Eq)]
        enum Sz {
            B,
            W,
            D,
        }
        #[derive(Copy, Clone)]
        enum Dest {
            Rm(Rm),
            Reg(u8),
        }
        let sz: Sz;
        let a: u32;
        let b: u32;
        let dest: Dest;
        match variant {
            0 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_rm8(rm, mem) as u32;
                b = self.read_r8(reg) as u32;
                dest = Dest::Rm(rm);
                sz = Sz::B;
            }
            1 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    a = self.read_rm32(rm, mem);
                    b = self.read_r32(reg);
                    sz = Sz::D;
                } else {
                    a = self.read_rm16(rm, mem) as u32;
                    b = self.read_r16(reg) as u32;
                    sz = Sz::W;
                }
                dest = Dest::Rm(rm);
            }
            2 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_r8(reg) as u32;
                b = self.read_rm8(rm, mem) as u32;
                dest = Dest::Reg(reg);
                sz = Sz::B;
            }
            3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    a = self.read_r32(reg);
                    b = self.read_rm32(rm, mem);
                    sz = Sz::D;
                } else {
                    a = self.read_r16(reg) as u32;
                    b = self.read_rm16(rm, mem) as u32;
                    sz = Sz::W;
                }
                dest = Dest::Reg(reg);
            }
            4 => {
                let imm = self.fetch_u8(mem);
                a = self.read_r8(0) as u32;
                b = imm as u32;
                dest = Dest::Reg(0);
                sz = Sz::B;
            }
            5 => {
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    b = lo | (hi << 16);
                    a = self.read_r32(0);
                    sz = Sz::D;
                } else {
                    let imm = self.fetch_u16(mem);
                    b = imm as u32;
                    a = self.read_r16(0) as u32;
                    sz = Sz::W;
                }
                dest = Dest::Reg(0);
            }
            _ => unreachable!("ALU dispatch only covers variants 0..5"),
        }

        let (result, writeback) = match sz {
            Sz::B => {
                let (r, wb) = self.alu_apply8(op, a as u8, b as u8);
                (r as u32, wb)
            }
            Sz::W => {
                let (r, wb) = self.alu_apply16(op, a as u16, b as u16);
                (r as u32, wb)
            }
            Sz::D => self.alu_apply32(op, a, b),
        };

        if writeback {
            match (dest, sz) {
                (Dest::Rm(rm), Sz::B) => self.write_rm8(rm, mem, result as u8),
                (Dest::Rm(rm), Sz::W) => self.write_rm16(rm, mem, result as u16),
                (Dest::Rm(rm), Sz::D) => self.write_rm32(rm, mem, result),
                (Dest::Reg(i), Sz::B) => self.write_r8(i, result as u8),
                (Dest::Reg(i), Sz::W) => self.write_r16(i, result as u16),
                (Dest::Reg(i), Sz::D) => self.write_r32(i, result),
            }
        }
        Ok(())
    }

    /// RTL8139 bus-master transmit. When the guest kicks a TX descriptor
    /// the device records (guest-physical addr, len); the device crate
    /// can't read guest RAM, so the CPU step (which holds the `Memory`)
    /// copies each frame out here and hands the bytes to the IoBus for the
    /// host bridge. Done promptly each step so the driver can't reuse the
    /// transmit buffer before we've captured its contents.
    fn service_nic_tx(mem: &Memory, io: &mut IoBus) {
        for (addr, size) in io.take_nic_tx_descriptors() {
            let mut frame = Vec::with_capacity(size as usize);
            for i in 0..size as u32 {
                frame.push(mem.read_u8(addr.wrapping_add(i)));
            }
            io.record_nic_tx_frame(frame);
        }
        // Completing a TX can raise the NIC's TX-OK interrupt; re-latch it.
        io.mark_irq_dirty();
    }

    /// Execute a single instruction. Returns Ok(()) on success, or an
    /// error if the opcode/ModR/M form is not implemented.
    ///
    /// At the top we absorb any segment-override prefix bytes
    /// (0x26/0x2E/0x36/0x3E) into `self.seg_override`. They affect
    /// only the current instruction; a fresh `step()` always clears
    /// the override first.
    pub fn step(&mut self, mem: &mut Memory, io: &mut IoBus) -> Result<(), CpuError> {
        if self.halted {
            // Real x86 wakes from HLT on the next external interrupt
            // (and NMI/SMI/INIT/RESET, which we don't model). The
            // refresh path below still needs to run so devices that
            // emit interrupts based on a counter (PIT, LAPIC timer,
            // HPET) get ticked — without that, the kernel's HLT in
            // its idle loop would deadlock when the only event it's
            // waiting for is a timer pulse.
            mem.tick_lapic_timer();
            mem.tick_hpet_counter();
            io.refresh_irqs();
            if io.nic_has_pending_tx() {
                Self::service_nic_tx(mem, io);
            }
            // Sleeping ticks still advance TSC — Linux measures
            // idle time against TSC and expects monotonic forward
            // motion even when no instruction retires.
            self.tsc = self.tsc.wrapping_add(1);
            // Any pending IRQ delivered now also wakes the CPU. The
            // ordering matches the architectural "HLT exits, IRQ is
            // taken at the next instruction boundary" sequence.
            if self.has(flag::IF) {
                if let Some(vec) = mem.take_pending_lapic_irq() {
                    self.halted = false;
                    self.trace_irq(vec, "LAPIC");
                    self.do_interrupt(vec, mem);
                    return Ok(());
                }
                if let Some(vec) = io.pending_irq_vector() {
                    self.halted = false;
                    io.ack_irq();
                    self.trace_irq(vec, "PIC");
                    self.do_interrupt(vec, mem);
                    return Ok(());
                }
            }
            return Ok(());
        }
        self.tsc = self.tsc.wrapping_add(1);
        // Diagnostic: WWWVM_TRACE_CALL=0xADDR prints whenever EIP
        // enters that address (typically a function start), with
        // the args (EAX/EDX/ECX), saved return addr at [ESP], and
        // the TSC. Cached in a OnceLock so the env-var lookup runs
        // exactly once per process — checking env::var per step
        // adds 50+ syscalls/instr and the kernel boot crawls.
        {
            use std::sync::OnceLock;
            static TARGET: OnceLock<Option<u32>> = OnceLock::new();
            let t = *TARGET.get_or_init(|| {
                std::env::var("WWWVM_TRACE_CALL")
                    .ok()
                    .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            });
            if let Some(target) = t {
                if self.ip == target {
                    let esp = self.read_r32(r16::SP as u8);
                    let ret = mem.read_u32(self.translate(mem, esp));
                    eprintln!(
                        "[CALL {target:08X}] EAX={:08X} EDX={:08X} ECX={:08X} ret={ret:08X} TSC={}",
                        self.read_r32(0),
                        self.read_r32(2),
                        self.read_r32(1),
                        self.tsc
                    );
                }
            }
        }
        // A page fault flagged by the previous instruction's memory
        // accesses takes priority over fresh work. Latch the linear
        // address into CR2 and vector through INT 14, pushing the
        // architectural error code below the IP/CS/FLAGS frame.
        //
        // Rewind IP to the *start* of the faulting instruction before
        // pushing the fault frame — #PF is a fault (not a trap), so
        // IRETD must land back on the same MOV and let it retry once
        // the handler maps the page in. Without the rewind, demand
        // paging silently breaks: IRETD returns to the *next*
        // instruction and the kernel never re-reads the operand.
        if let Some(pf) = self.take_pending_fault() {
            self.cr2 = pf.addr;
            self.ip = self.last_op_ip;
            self.do_interrupt_with_error(14, Some(pf.error_code), mem);
            return Ok(());
        }
        // External interrupt delivery — must come *before* fetch so an
        // unmasked IRQ runs its handler at the next instruction boundary
        // instead of one boundary late. Refresh first so devices that
        // assert their line (e.g. UART with rx data and IER set) get
        // latched into the PIC's IRR for this turn.
        // LAPIC timer ticks once per CPU step. tick_lapic_timer
        // decrements the Current Count MMIO register; on the zero
        // crossing (LVT_TIMER vector not masked) it queues a
        // pending IRQ. Linux's calibration measures the delta
        // against TSC to derive the bus ratio; periodic mode keeps
        // the kernel's scheduler tick alive.
        mem.tick_lapic_timer();
        // HPET main counter advances once per step too — Linux uses
        // it as a time-of-day source via direct MMIO reads. The
        // counter freezes when General Configuration ENABLE_CNF is
        // clear, so software-paused HPET still works.
        mem.tick_hpet_counter();
        io.refresh_irqs();
        if io.nic_has_pending_tx() {
            Self::service_nic_tx(mem, io);
        }
        if self.has(flag::IF) {
            // LAPIC IRQs win over legacy PIC — they're the higher-
            // priority source on real silicon, and the kernel
            // expects them to fire promptly so the tick rate stays
            // stable.
            if let Some(vec) = mem.take_pending_lapic_irq() {
                self.trace_irq(vec, "LAPIC");
                self.do_interrupt(vec, mem);
                return Ok(());
            }
            if let Some(vec) = io.pending_irq_vector() {
                io.ack_irq();
                self.trace_irq(vec, "PIC");
                self.do_interrupt(vec, mem);
                return Ok(());
            }
        }
        self.seg_override = None;
        // Default operand / address sizes come from the active CS
        // descriptor's D bit (latched in `code_size_32`). 0x66 / 0x67
        // prefixes invert them for this one instruction; real-mode
        // and 16-bit-PM CS leave `code_size_32 = false`, matching
        // the historical "everything is 16-bit unless prefixed".
        self.op_size_32 = self.code_size_32;
        self.addr_size_32 = self.code_size_32;
        let op_cs = self.sregs[sreg::CS];
        let op_ip = self.ip;
        // Persist op_ip across the step boundary so a #PF raised by
        // this instruction's memory accesses can rewind IP back here
        // when the fault is dispatched at the start of the next step.
        self.last_op_ip = op_ip;
        // Debug trace (no-op unless enabled). Records (eip, eax, ebx,
        // ebp) for every instruction; also fires a one-shot dump when
        // about to execute at a "wild" low address (op_ip < 0x10000) —
        // e.g. the kernel's bad jump to 0x2061 — so the ring shows the
        // instruction (ret/jmp/call) that set the bad target.
        if self.pf_trace_on.get() {
            let entry = (op_ip, self.read_r32(0), self.read_r32(3), self.read_r32(5));
            let wild = op_ip < 0x1_0000;
            if let Some(t) = self.pf_trace.borrow_mut().as_mut() {
                if wild && !t.fired {
                    t.fired = true;
                    t.dump(&format!("WILD low-address execute at op_ip={op_ip:#x}"));
                }
                t.record(entry);
            }
        }
        // Checkpoint GP registers + flags so a #PF mid-instruction can
        // roll them back. Our continue-on-fault model lets a faulting
        // memory read still write its (garbage 0) result to the
        // destination register; for `mov eax,[eax]` (dest == address
        // base) that clobbers eax, so the EIP-rewound retry computes a
        // *different* (zero) address and double-faults at 0. Restoring
        // the registers on a pending fault (see end of step) makes the
        // retry re-run with pristine state, exactly as real hardware
        // (a faulting instruction commits nothing). String/REP ops
        // self-manage partial progress and are excluded below.
        let reg_snap = self.regs;
        let reg_high_snap = self.regs_high;
        let flags_snap = self.flags;
        let flags_high_snap = self.flags_high;
        let opcode = loop {
            let b = self.fetch_u8(mem);
            match b {
                0x26 => self.seg_override = Some(sreg::ES),
                0x2E => self.seg_override = Some(sreg::CS),
                0x36 => self.seg_override = Some(sreg::SS),
                0x3E => self.seg_override = Some(sreg::DS),
                // 0x64/0x65 — FS/GS segment override. Linux addresses
                // per-CPU and TLS data as `fs:[off]` / `gs:[off]`.
                0x64 => self.seg_override = Some(sreg::FS),
                0x65 => self.seg_override = Some(sreg::GS),
                // 0x66 — operand-size override. Flips default
                // operand width from 16 to 32 for this instruction.
                0x66 => self.op_size_32 = !self.code_size_32,
                // 0x67 — address-size override. Flips the ModR/M
                // address decode from 16-bit to 32-bit (and SIB).
                0x67 => self.addr_size_32 = !self.code_size_32,
                _ => break b,
            }
        };

        // FP instructions can partially mutate the x87 stack / XMM before a
        // #PF — e.g. an `FLD m64` of a libm constant on a demand-paged
        // .rodata page pushes a garbage value, sets pending_fault, but the
        // GP snapshot above does NOT cover fpu_top/fpu_st/xmm, so the
        // EIP-rewound retry re-runs from a corrupted state. A non-idempotent
        // read-modify-write like `MULSD xmm,[mem]` is just as bad: the
        // faulting read yields 0, `xmm = a*0 = 0` overwrites the original
        // `a`, and the retry then computes `0*correct`. Snapshot the FP
        // state too. Opcodes: x87 escapes D8-DF, the 0F SSE/two-byte map
        // (incl. 66-prefixed SSE2 — 0x66 is consumed as a prefix), AND the
        // F2/F3-prefixed scalar SSE (which decode with opcode F2/F3). For
        // F2/F3 REP-string ops this snapshot is a harmless no-op (they touch
        // neither the x87 stack nor XMM). Gated to keep the common ALU path
        // cheap. Restored on a pending fault at the end of step().
        let fp_snap = if (0xD8..=0xDF).contains(&opcode)
            || opcode == 0x0F
            || opcode == 0xF2
            || opcode == 0xF3
        {
            Some((
                self.fpu_top,
                self.fpu_sw,
                self.fpu_cw,
                self.fpu_st,
                self.xmm,
                self.mmx,
            ))
        } else {
            None
        };

        match opcode {
            0x90 => { /* NOP */ }
            0xF4 => {
                // HLT is a CPL=0 instruction. From ring 3 (or any
                // ring with non-zero RPL on the current CS in PE
                // mode) it must raise #GP(0) — the kernel's
                // user-space oops handler catches that and reports
                // "general protection in user code" rather than
                // letting the guest wedge the CPU.
                if self.cr0 & 1 != 0 && (self.sregs[sreg::CS] & 3) != 0 {
                    self.ip = op_ip;
                    self.do_interrupt_with_error(13, Some(0), mem);
                    return Ok(());
                }
                self.halted = true;
            }
            0xFA => {
                // CLI is IOPL-sensitive — fault if CPL > IOPL.
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                self.set_flag(flag::IF, false);
            }
            0xFB => {
                // STI shares CLI's IOPL guard.
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                self.set_flag(flag::IF, true);
                // One-shot probe trace (WWWVM_TRACE_STI=1) used by
                // the linux_boot diagnostic to spot the first time
                // the kernel enables interrupts in early boot. The
                // env-var read happens every STI but STI is rare
                // (a few times per kernel boot), so the overhead
                // is negligible.
                if std::env::var_os("WWWVM_TRACE_STI").is_some() {
                    eprintln!(
                        "[STI] @ CS:EIP={:04X}:{:08X}  TSC={}",
                        self.sregs[sreg::CS],
                        op_ip,
                        self.tsc
                    );
                }
            }
            0xFC => {
                self.set_flag(flag::DF, false);
            }
            0xFD => {
                self.set_flag(flag::DF, true);
            }

            0xB0..=0xB7 => {
                let imm = self.fetch_u8(mem);
                self.write_r8(opcode - 0xB0, imm);
            }
            0xB8..=0xBF => {
                // MOV r16/r32, imm. With operand-size override (0x66)
                // it loads a 32-bit immediate into E?X; otherwise the
                // 16-bit form into ?X.
                let reg = opcode - 0xB8;
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    self.write_r32(reg, lo | (hi << 16));
                } else {
                    let imm = self.fetch_u16(mem);
                    self.write_r16(reg, imm);
                }
            }

            0xEB => {
                let rel = self.fetch_u8(mem) as i8;
                self.ip = self.ip.wrapping_add(rel as i32 as u32);
            }
            // JMP rel16 / rel32 — under 0x66 the displacement widens
            // from 16 to 32 bits. Kernel-side `jmp label` to anywhere
            // more than ±32 KiB away compiles to this form.
            0xE9 => {
                let rel: i32 = if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    (lo | (hi << 16)) as i32
                } else {
                    self.fetch_u16(mem) as i16 as i32
                };
                self.ip = self.ip.wrapping_add(rel as u32);
            }

            // Jcc rel8 family — 0x70..0x7F
            0x70..=0x7F => {
                let rel = self.fetch_u8(mem) as i8;
                if self.eval_cond(opcode & 0x0F) {
                    self.ip = self.ip.wrapping_add(rel as i32 as u32);
                }
            }

            // LOOP family — decrement CX then branch on rel8 if CX != 0
            // (and on the per-opcode condition).
            //   0xE0 LOOPNZ / LOOPNE — also requires ZF=0
            //   0xE1 LOOPZ  / LOOPE  — also requires ZF=1
            //   0xE2 LOOP            — unconditional on flags
            0xE0..=0xE2 => {
                let rel = self.fetch_u8(mem) as i8;
                // Counter is ECX when the effective address size is
                // 32-bit, else CX (Intel SDM: LOOP uses the address-size
                // attribute to select the count register). Mirror the
                // REP loop's addr_size_32 split so the upper half of ECX
                // borrows correctly instead of being left stale.
                let counter_nonzero = if self.addr_size_32 {
                    let c = self.read_r32(r16::CX as u8).wrapping_sub(1);
                    self.write_r32(r16::CX as u8, c);
                    c != 0
                } else {
                    let c = self.regs[r16::CX].wrapping_sub(1);
                    self.regs[r16::CX] = c;
                    c != 0
                };
                let cond = match opcode {
                    0xE2 => true,
                    0xE1 => self.has(flag::ZF),
                    0xE0 => !self.has(flag::ZF),
                    _ => unreachable!(),
                };
                if counter_nonzero && cond {
                    self.ip = self.ip.wrapping_add(rel as i32 as u32);
                }
            }

            // JCXZ/JECXZ rel8 — branch if the count register is 0. The
            // counter is NOT decremented; this is the idiomatic guard
            // before a LOOP that would otherwise iterate 2^32/2^16 times
            // when the count starts at 0. 0xE3 is JCXZ (test CX) when
            // address size is 16-bit and JECXZ (test full ECX) when 32.
            0xE3 => {
                let rel = self.fetch_u8(mem) as i8;
                let counter_zero = if self.addr_size_32 {
                    self.read_r32(r16::CX as u8) == 0
                } else {
                    self.regs[r16::CX] == 0
                };
                if counter_zero {
                    self.ip = self.ip.wrapping_add(rel as i32 as u32);
                }
            }

            // Single-shot string ops. REP-prefixed paths go through the
            // 0xF2/0xF3 handler below.
            0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF => {
                // Same #PF-rollback as the REP loop: step_string advances
                // SI/DI even when the access faults, but step() rewinds
                // EIP to re-execute this op after the handler. Without
                // restoring SI/DI, the retry would operate on the *next*
                // element and skip the faulting one. Snapshot + restore.
                // Flags are snapshotted too: CMPS/SCAS compute EFLAGS
                // from the (short-circuited-to-0) faulting read before
                // step() dispatches the #PF, so without this the #PF
                // frame would carry architecturally-wrong EFLAGS (real
                // x86 leaves them untouched on a faulting string op).
                let esi_snap = self.read_r32(r16::SI as u8);
                let edi_snap = self.read_r32(r16::DI as u8);
                let flags_snap = self.flags;
                self.step_string(opcode, mem);
                if self.pending_fault.get().is_some() {
                    self.write_r32(r16::SI as u8, esi_snap);
                    self.write_r32(r16::DI as u8, edi_snap);
                    self.flags = flags_snap;
                }
            }

            // INSB / INSW / INSD / OUTSB / OUTSW / OUTSD — port-to-
            // memory and memory-to-port string moves. Linux's
            // serial8250 driver init uses these; without them the
            // kernel oopses with Unimplemented mid-initcall and
            // never reaches `Run /init`.
            0x6C..=0x6F => {
                self.step_string_io(opcode, mem, io);
            }

            // Group 2: shift/rotate r/m by 1, CL, or imm8.
            //   0xD0: r/m8 by 1
            //   0xD1: r/m16 by 1
            //   0xD2: r/m8 by CL
            //   0xD3: r/m16 by CL
            //   0xC0: r/m8 by imm8
            //   0xC1: r/m16 by imm8
            // ModR/M reg field selects ROL/ROR/RCL/RCR/SHL/SHR/SAR.
            0xD0 | 0xD1 | 0xD2 | 0xD3 | 0xC0 | 0xC1 => {
                let is_wide = matches!(opcode, 0xD1 | 0xD3 | 0xC1);
                let (_, sub, rm) = self.fetch_modrm(mem);
                let count = match opcode {
                    0xD0 | 0xD1 => 1,
                    0xD2 | 0xD3 => self.read_r8(1), // CL
                    0xC0 | 0xC1 => self.fetch_u8(mem),
                    _ => unreachable!(),
                };
                if !is_wide {
                    let v = self.read_rm8(rm, mem);
                    let r = self.shift_apply8(sub, v, count)?;
                    self.write_rm8(rm, mem, r);
                } else if self.op_size_32 {
                    let v = self.read_rm32(rm, mem);
                    let r = self.shift_apply32(sub, v, count)?;
                    self.write_rm32(rm, mem, r);
                } else {
                    let v = self.read_rm16(rm, mem);
                    let r = self.shift_apply16(sub, v, count)?;
                    self.write_rm16(rm, mem, r);
                }
            }

            // REP / REPE / REPZ (0xF3) and REPNE / REPNZ (0xF2) prefix.
            // For MOVS/STOS/LODS the prefix repeats CX times with no
            // ZF condition. For CMPS/SCAS the prefix repeats while
            // (REPE: ZF=1, REPNE: ZF=0). The counter register is CX
            // or ECX depending on the address-size attribute — Linux
            // memcpy compiles to `REP MOVSD` with ECX-driven length.
            //
            // A seg-override prefix may appear before *or* after REP
            // (`26 F3 A4` and `F3 26 A4` both mean ES: REP MOVSB), so
            // we additionally absorb seg-overrides + address-size and
            // operand-size prefixes here.
            0xF2 | 0xF3 => {
                let rep_zero = opcode == 0xF3;
                let inner = loop {
                    let b = self.fetch_u8(mem);
                    match b {
                        0x26 => self.seg_override = Some(sreg::ES),
                        0x2E => self.seg_override = Some(sreg::CS),
                        0x36 => self.seg_override = Some(sreg::SS),
                        0x3E => self.seg_override = Some(sreg::DS),
                        0x64 => self.seg_override = Some(sreg::FS),
                        0x65 => self.seg_override = Some(sreg::GS),
                        0x66 => self.op_size_32 = !self.code_size_32,
                        0x67 => self.addr_size_32 = !self.code_size_32,
                        _ => break b,
                    }
                };
                // F2/F3 + 0F is *not* a REP string op — it's the
                // mandatory-prefix escape for scalar SSE (MOVSS/MOVSD,
                // ADDSS..DIVSD) and MOVDQU. Dispatch those before the
                // string-loop logic; F3 selects single-precision /
                // MOVSS, F2 selects double / MOVSD.
                if inner == 0x0F {
                    let op2 = self.fetch_u8(mem);
                    self.sse_scalar(rep_zero, op2, mem, op_cs, op_ip)?;
                } else if inner == 0x90 {
                    // PAUSE = F3 90. The 0xF3 prefix on a NOP is the
                    // spin-loop hint, *not* a REP NOP — spinlocks emit
                    // it constantly. Treat it as a no-op rather than
                    // falling into the string-op loop (which rejects
                    // 0x90).
                } else if matches!(inner, 0xA4..=0xA7 | 0xAA..=0xAF | 0x6C..=0x6F) {
                    let conditional = matches!(inner, 0xA6 | 0xA7 | 0xAE | 0xAF);
                    let is_io = matches!(inner, 0x6C..=0x6F);
                    loop {
                        let counter_done = if self.addr_size_32 {
                            self.read_r32(r16::CX as u8) == 0
                        } else {
                            self.regs[r16::CX] == 0
                        };
                        if counter_done {
                            break;
                        }
                        // Snapshot the string index registers before the
                        // iteration. If this element raises a #PF (e.g.
                        // `rep movsl` from copy_to_user hitting a fresh
                        // COW / demand-zero user page), the partial
                        // iteration must NOT commit: roll SI/DI back and
                        // stop WITHOUT decrementing ECX, so that after the
                        // page-fault handler maps the page in, IRETD
                        // returns to this REP (EIP was rewound in step())
                        // and it resumes from the *same* element. Without
                        // this rollback, a faulting copy_to_user would run
                        // ECX→0 against phys 0 and the rewound retry would
                        // find ECX==0 and copy nothing — silently dropping
                        // the whole transfer (the pipe2-fds-not-populated
                        // bug). step_string advances SI/DI internally, so
                        // we capture them here and restore on fault.
                        // Flags too: a faulting REPE CMPS / REPNE SCAS
                        // computes EFLAGS from the short-circuited read
                        // before the fault dispatches; restoring them
                        // keeps the #PF frame's EFLAGS architecturally
                        // correct (and the fault `break` below happens
                        // before the ZF check, so a bogus ZF could never
                        // terminate the REP early anyway).
                        let esi_snap = self.read_r32(r16::SI as u8);
                        let edi_snap = self.read_r32(r16::DI as u8);
                        let flags_snap = self.flags;
                        if is_io {
                            self.step_string_io(inner, mem, io);
                        } else if !self.step_string(inner, mem) {
                            return Err(CpuError::Unimplemented {
                                opcode: inner,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        if self.pending_fault.get().is_some() {
                            self.write_r32(r16::SI as u8, esi_snap);
                            self.write_r32(r16::DI as u8, edi_snap);
                            self.flags = flags_snap;
                            break;
                        }
                        if self.addr_size_32 {
                            let c = self.read_r32(r16::CX as u8).wrapping_sub(1);
                            self.write_r32(r16::CX as u8, c);
                        } else {
                            self.regs[r16::CX] = self.regs[r16::CX].wrapping_sub(1);
                        }
                        if conditional {
                            let zf = self.has(flag::ZF);
                            if rep_zero && !zf {
                                break;
                            }
                            if !rep_zero && zf {
                                break;
                            }
                        }
                    }
                } else {
                    // F2/F3 on a NON-string opcode (e.g. `rep ret` /
                    // `repz ret` = F3 C3 — GCC's function-return epilogue
                    // for ~a decade) is a meaningless, ignored prefix.
                    // Rewind to the byte just past the prefix so the next
                    // step() decodes the instruction normally (re-applying
                    // any intervening 0x66/0x67/segment prefix). Without
                    // this the byte fell into the string-rep loop, which
                    // errored (CX!=0) or skipped the instruction (CX==0).
                    self.ip = op_ip.wrapping_add(1);
                }
            }

            // Standard ALU family (ADD/OR/ADC/SBB/AND/SUB/XOR/CMP) —
            // opcodes 0x00..0x3F where (opcode & 0x06) != 0x06 (those
            // slots are PUSH/POP sreg / prefixes, handled elsewhere).
            0x00..=0x05
            | 0x08..=0x0D
            | 0x10..=0x15
            | 0x18..=0x1D
            | 0x20..=0x25
            | 0x28..=0x2D
            | 0x30..=0x35
            | 0x38..=0x3D => {
                self.alu_dispatch(opcode, mem)?;
            }

            // TEST r/m8, r8 — AND for flags only, no writeback.
            0x84 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let result = self.read_rm8(rm, mem) & self.read_r8(reg);
                self.flags_logic8(result);
            }
            // TEST r/m16, r16 — under 0x66 prefix becomes TEST r/m32,
            // r32. `test eax, eax` (0x85 0xC0) is the canonical
            // zero/sign check a compiler emits before a conditional
            // branch, so this is one of the hottest opcodes in any
            // kernel.
            0x85 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let result = self.read_rm32(rm, mem) & self.read_r32(reg);
                    self.flags_logic32(result);
                } else {
                    let result = self.read_rm16(rm, mem) & self.read_r16(reg);
                    self.flags_logic16(result);
                }
            }
            // XCHG r/m8, r8 — swap byte operands.
            0x86 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let a = self.read_rm8(rm, mem);
                let b = self.read_r8(reg);
                self.write_rm8(rm, mem, b);
                self.write_r8(reg, a);
            }
            // XCHG r/m16, r16 — under 0x66 becomes XCHG r/m32, r32.
            0x87 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let a = self.read_rm32(rm, mem);
                    let b = self.read_r32(reg);
                    self.write_rm32(rm, mem, b);
                    self.write_r32(reg, a);
                } else {
                    let a = self.read_rm16(rm, mem);
                    let b = self.read_r16(reg);
                    self.write_rm16(rm, mem, b);
                    self.write_r16(reg, a);
                }
            }

            // MOV r/m8, r8 — direction = r/m
            0x88 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_r8(reg);
                self.write_rm8(rm, mem, v);
            }
            // MOV r/m16, r16 — under 0x66 prefix becomes MOV r/m32, r32.
            0x89 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let v = self.read_r32(reg);
                    self.write_rm32(rm, mem, v);
                } else {
                    let v = self.read_r16(reg);
                    self.write_rm16(rm, mem, v);
                }
            }
            // MOV r8, r/m8 — direction = reg
            0x8A => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_rm8(rm, mem);
                self.write_r8(reg, v);
            }
            // MOV r16, r/m16 — under 0x66 prefix becomes MOV r32, r/m32.
            0x8B => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let v = self.read_rm32(rm, mem);
                    self.write_r32(reg, v);
                } else {
                    let v = self.read_rm16(rm, mem);
                    self.write_r16(reg, v);
                }
            }

            // MOV r/m16, sreg — store segment register to r/m.
            // reg field encodes the segment: 0=ES, 1=CS, 2=SS, 3=DS,
            // 4=FS, 5=GS. Values 6-7 are invalid.
            0x8C => {
                let (_, sreg_idx, rm) = self.fetch_modrm(mem);
                if sreg_idx > 5 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                let v = self.sregs[sreg_idx as usize];
                self.write_rm16(rm, mem, v);
            }

            // LEA r16/32, m — load effective address (no memory
            // access). mod=11 (register operand) is undefined on
            // real x86. Under 0x66, the destination is r32; the
            // computed EA still comes from 16-bit address arithmetic
            // unless a 0x67 prefix changes the address-size attribute
            // (not yet modelled — when it lands, LEA picks up 32-bit
            // EAs for free).
            0x8D => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                match rm {
                    Rm::Mem(ea) => {
                        if self.op_size_32 {
                            self.write_r32(reg, ea.off);
                        } else {
                            self.write_r16(reg, ea.off as u16);
                        }
                    }
                    Rm::Reg(_) => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        });
                    }
                }
            }

            // MOV sreg, r/m16 — load segment register from r/m.
            // Loading CS is normally illegal but we allow it for now;
            // a future iteration may reject it like real x86. In PE
            // mode, a selector indexing past the GDT limit raises
            // #GP(selector & ~3) — the error-code shape Intel defines
            // (RPL bits become EXT/IDT, both zero for instruction-
            // caused faults; TI bit and index pass through).
            0x8E => {
                let (_, sreg_idx, rm) = self.fetch_modrm(mem);
                if sreg_idx > 5 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                let v = self.read_rm16(rm, mem);
                if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                    return Ok(());
                }
                self.write_sreg(sreg_idx as usize, v, mem);
            }

            // POP r/m16/32 — 0x8F /0. Pop from the stack into a memory
            // location or register. Other /reg encodings (1..7) for
            // 0x8F are undefined and we leave them unimplemented. Linux
            // kernel uses this in function prologues / locals teardown:
            // `POP [EBP-disp]` to restore a saved local from stack.
            0x8F => {
                let (_, reg_ext, rm) = self.fetch_modrm(mem);
                if reg_ext != 0 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                if self.op_size_32 {
                    let v = self.pop32(mem);
                    self.write_rm32(rm, mem, v);
                } else {
                    let v = self.pop16(mem);
                    self.write_rm16(rm, mem, v);
                }
            }

            // XCHG AX, r16 — short form. Under 0x66 (or in a 32-bit
            // code segment) this is XCHG EAX, r32 and must swap the
            // full 32 bits, not just the low halves. 0x90 (XCHG AX,
            // AX) is NOP and is handled by the dedicated NOP arm
            // above. Linux's `__udelay`-family stubs call into
            // helpers that XCHG EAX with a register-loaded pointer
            // right after entry — losing the upper 16 bits there
            // turns a kernel pointer into a low-memory address and
            // the first deref BUGs with "unable to handle page
            // fault for address: 000031ec"-style oopses.
            0x91..=0x97 => {
                let i = (opcode - 0x90) as usize;
                if self.op_size_32 {
                    let eax = self.read_r32(r16::AX as u8);
                    let other = self.read_r32(i as u8);
                    self.write_r32(r16::AX as u8, other);
                    self.write_r32(i as u8, eax);
                } else {
                    let ax = self.regs[r16::AX];
                    let other = self.regs[i];
                    self.regs[r16::AX] = other;
                    self.regs[i] = ax;
                }
            }

            // LES r16, m — load far pointer into reg + ES.
            // The memory operand is 32 bits: low word -> reg, high word -> ES.
            0xC4 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let ea = match rm {
                    Rm::Mem(ea) => ea,
                    Rm::Reg(_) => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        });
                    }
                };
                let base = self.linear_seg(ea.seg, ea.off);
                let off_val = self.mem_read_u16(mem, base);
                let seg_val =
                    self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                if self.raise_gp_if_bad_selector(seg_val, op_ip, mem) {
                    return Ok(());
                }
                self.write_r16(reg, off_val);
                self.write_sreg(sreg::ES, seg_val, mem);
                let _ = base;
            }

            // LDS r16, m — same as LES but loads DS.
            0xC5 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let ea = match rm {
                    Rm::Mem(ea) => ea,
                    Rm::Reg(_) => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        });
                    }
                };
                let off_val = self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off));
                let seg_val =
                    self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                if self.raise_gp_if_bad_selector(seg_val, op_ip, mem) {
                    return Ok(());
                }
                self.write_r16(reg, off_val);
                self.write_sreg(sreg::DS, seg_val, mem);
            }
            // Group 1: ALU r/m, imm.  reg field of ModR/M = op (0=ADD..7=CMP)
            //   0x80: r/m8, imm8
            //   0x81: r/m16, imm16   (with 0x66: r/m32, imm32)
            //   0x83: r/m16, imm8 sign-extended to 16-bit (with 0x66:
            //         r/m32, imm8 sign-extended to 32-bit)
            0x80 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                let imm = self.fetch_u8(mem);
                let a = self.read_rm8(rm, mem);
                let (r, wb) = self.alu_apply8(op, a, imm);
                if wb {
                    self.write_rm8(rm, mem, r);
                }
            }
            0x81 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let imm = lo | (hi << 16);
                    let a = self.read_rm32(rm, mem);
                    let (r, wb) = self.alu_apply32(op, a, imm);
                    if wb {
                        self.write_rm32(rm, mem, r);
                    }
                } else {
                    let imm = self.fetch_u16(mem);
                    let a = self.read_rm16(rm, mem);
                    let (r, wb) = self.alu_apply16(op, a, imm);
                    if wb {
                        self.write_rm16(rm, mem, r);
                    }
                }
            }
            0x83 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let imm = self.fetch_u8(mem) as i8 as i32 as u32;
                    let a = self.read_rm32(rm, mem);
                    let (r, wb) = self.alu_apply32(op, a, imm);
                    if wb {
                        self.write_rm32(rm, mem, r);
                    }
                } else {
                    let imm = self.fetch_u8(mem) as i8 as i16 as u16;
                    let a = self.read_rm16(rm, mem);
                    let (r, wb) = self.alu_apply16(op, a, imm);
                    if wb {
                        self.write_rm16(rm, mem, r);
                    }
                }
            }

            // Group 3 (0xF6 8-bit, 0xF7 16-bit). reg field selects:
            //   /0 = TEST r/m, imm   (imm is fetched here)
            //   /2 = NOT r/m          (no flag updates)
            //   /3 = NEG r/m          (subtract from 0, sets CF if op != 0)
            //   /4 = MUL, /5 = IMUL, /6 = DIV, /7 = IDIV — deferred
            0xF6 => {
                let (_, sub, rm) = self.fetch_modrm(mem);
                match sub {
                    0 | 1 => {
                        let imm = self.fetch_u8(mem);
                        let v = self.read_rm8(rm, mem);
                        let r = v & imm;
                        self.flags_logic8(r);
                    }
                    2 => {
                        let v = self.read_rm8(rm, mem);
                        self.write_rm8(rm, mem, !v);
                    }
                    3 => {
                        let v = self.read_rm8(rm, mem);
                        let r = 0u8.wrapping_sub(v);
                        self.flags_sub8(0, v, 0, r);
                        self.write_rm8(rm, mem, r);
                    }
                    4 => {
                        // MUL r/m8 — AX = AL * r/m8 (unsigned)
                        let v = self.read_rm8(rm, mem);
                        let al = self.read_r8(0);
                        let result = (al as u16).wrapping_mul(v as u16);
                        self.regs[r16::AX] = result;
                        let upper = (result >> 8) as u8;
                        self.set_flag(flag::CF, upper != 0);
                        self.set_flag(flag::OF, upper != 0);
                    }
                    5 => {
                        // IMUL r/m8 — AX = AL * r/m8 (signed)
                        let v = self.read_rm8(rm, mem) as i8 as i16;
                        let al = self.read_r8(0) as i8 as i16;
                        let result = al.wrapping_mul(v);
                        self.regs[r16::AX] = result as u16;
                        // CF/OF set if AX is *not* the sign-extension of AL
                        let sign_extended = (result as i8) as i16;
                        let overflow = sign_extended != result;
                        self.set_flag(flag::CF, overflow);
                        self.set_flag(flag::OF, overflow);
                    }
                    6 => {
                        // DIV r/m8 — AL = AX/v (unsigned), AH = AX%v
                        let v = self.read_rm8(rm, mem);
                        if v == 0 {
                            self.raise_de(op_ip, mem);
                            return Ok(());
                        }
                        let ax = self.regs[r16::AX];
                        let q = ax / v as u16;
                        let r = ax % v as u16;
                        if q > 0xFF {
                            self.raise_de(op_ip, mem);
                            return Ok(());
                        }
                        self.write_r8(0, q as u8);
                        self.write_r8(4, r as u8); // AH
                    }
                    7 => {
                        // IDIV r/m8 — signed division of AX by r/m8
                        let v = self.read_rm8(rm, mem) as i8 as i16;
                        if v == 0 {
                            self.raise_de(op_ip, mem);
                            return Ok(());
                        }
                        let ax = self.regs[r16::AX] as i16;
                        // checked_div catches AX = i16::MIN / -1 (the true
                        // quotient +32768 doesn't fit) and raises #DE
                        // instead of panicking on the native overflow.
                        let Some(q) = ax.checked_div(v) else {
                            self.raise_de(op_ip, mem);
                            return Ok(());
                        };
                        let r = ax % v;
                        if !(-128..=127).contains(&q) {
                            self.raise_de(op_ip, mem);
                            return Ok(());
                        }
                        self.write_r8(0, q as u8);
                        self.write_r8(4, r as u8); // AH
                    }
                    _ => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        })
                    }
                }
            }
            0xF7 => {
                let (_, sub, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    // 32-bit forms: TEST/NOT/NEG/MUL/IMUL/DIV/IDIV r/m32.
                    match sub {
                        0 | 1 => {
                            let lo = self.fetch_u16(mem) as u32;
                            let hi = self.fetch_u16(mem) as u32;
                            let imm = lo | (hi << 16);
                            let v = self.read_rm32(rm, mem);
                            let r = v & imm;
                            self.flags_logic32(r);
                        }
                        2 => {
                            let v = self.read_rm32(rm, mem);
                            self.write_rm32(rm, mem, !v);
                        }
                        3 => {
                            let v = self.read_rm32(rm, mem);
                            let r = 0u32.wrapping_sub(v);
                            self.flags_sub32(0, v, 0, r);
                            self.write_rm32(rm, mem, r);
                        }
                        4 => {
                            // MUL r/m32 — EDX:EAX = EAX * r/m32 (unsigned)
                            let v = self.read_rm32(rm, mem) as u64;
                            let eax = self.read_r32(0) as u64;
                            let result = eax.wrapping_mul(v);
                            self.write_r32(0, result as u32);
                            self.write_r32(2, (result >> 32) as u32);
                            let upper_nonzero = (result >> 32) != 0;
                            self.set_flag(flag::CF, upper_nonzero);
                            self.set_flag(flag::OF, upper_nonzero);
                        }
                        5 => {
                            // IMUL r/m32 — EDX:EAX = EAX * r/m32 (signed)
                            let v = self.read_rm32(rm, mem) as i32 as i64;
                            let eax = self.read_r32(0) as i32 as i64;
                            let result = eax.wrapping_mul(v);
                            self.write_r32(0, result as u32);
                            self.write_r32(2, (result >> 32) as u32);
                            let sign_extended = (result as i32) as i64;
                            let overflow = sign_extended != result;
                            self.set_flag(flag::CF, overflow);
                            self.set_flag(flag::OF, overflow);
                        }
                        6 => {
                            // DIV r/m32 — EAX = EDX:EAX / v, EDX = rem (unsigned)
                            let v = self.read_rm32(rm, mem) as u64;
                            if v == 0 {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            let dividend =
                                ((self.read_r32(2) as u64) << 32) | self.read_r32(0) as u64;
                            let q = dividend / v;
                            let r = dividend % v;
                            if q > 0xFFFF_FFFF {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            self.write_r32(0, q as u32);
                            self.write_r32(2, r as u32);
                        }
                        7 => {
                            // IDIV r/m32 — signed division of EDX:EAX by r/m32
                            let v = self.read_rm32(rm, mem) as i32 as i64;
                            if v == 0 {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            let dividend = (((self.read_r32(2) as u64) << 32)
                                | self.read_r32(0) as u64)
                                as i64;
                            // checked_div catches EDX:EAX = i64::MIN / -1
                            // (quotient overflow) -> #DE, not a panic.
                            let Some(q) = dividend.checked_div(v) else {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            };
                            let r = dividend % v;
                            if !(i32::MIN as i64..=i32::MAX as i64).contains(&q) {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            self.write_r32(0, q as u32);
                            self.write_r32(2, r as u32);
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            })
                        }
                    }
                } else {
                    match sub {
                        0 | 1 => {
                            let imm = self.fetch_u16(mem);
                            let v = self.read_rm16(rm, mem);
                            let r = v & imm;
                            self.flags_logic16(r);
                        }
                        2 => {
                            let v = self.read_rm16(rm, mem);
                            self.write_rm16(rm, mem, !v);
                        }
                        3 => {
                            let v = self.read_rm16(rm, mem);
                            let r = 0u16.wrapping_sub(v);
                            self.flags_sub16(0, v, 0, r);
                            self.write_rm16(rm, mem, r);
                        }
                        4 => {
                            // MUL r/m16 — DX:AX = AX * r/m16 (unsigned)
                            let v = self.read_rm16(rm, mem) as u32;
                            let ax = self.regs[r16::AX] as u32;
                            let result = ax.wrapping_mul(v);
                            self.regs[r16::AX] = result as u16;
                            self.regs[r16::DX] = (result >> 16) as u16;
                            let upper_nonzero = self.regs[r16::DX] != 0;
                            self.set_flag(flag::CF, upper_nonzero);
                            self.set_flag(flag::OF, upper_nonzero);
                        }
                        5 => {
                            // IMUL r/m16 — DX:AX = AX * r/m16 (signed)
                            let v = self.read_rm16(rm, mem) as i16 as i32;
                            let ax = self.regs[r16::AX] as i16 as i32;
                            let result = ax.wrapping_mul(v);
                            self.regs[r16::AX] = result as u16;
                            self.regs[r16::DX] = (result >> 16) as u16;
                            let sign_extended = (result as i16) as i32;
                            let overflow = sign_extended != result;
                            self.set_flag(flag::CF, overflow);
                            self.set_flag(flag::OF, overflow);
                        }
                        6 => {
                            // DIV r/m16 — AX = DX:AX / v (unsigned), DX = rem
                            let v = self.read_rm16(rm, mem) as u32;
                            if v == 0 {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            let dividend =
                                ((self.regs[r16::DX] as u32) << 16) | self.regs[r16::AX] as u32;
                            let q = dividend / v;
                            let r = dividend % v;
                            if q > 0xFFFF {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            self.regs[r16::AX] = q as u16;
                            self.regs[r16::DX] = r as u16;
                        }
                        7 => {
                            // IDIV r/m16 — signed division of DX:AX by r/m16
                            let v = self.read_rm16(rm, mem) as i16 as i32;
                            if v == 0 {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            let dividend = (((self.regs[r16::DX] as u32) << 16)
                                | self.regs[r16::AX] as u32)
                                as i32;
                            // checked_div catches DX:AX = i32::MIN / -1
                            // (quotient overflow) -> #DE, not a panic.
                            let Some(q) = dividend.checked_div(v) else {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            };
                            let r = dividend % v;
                            if !(i16::MIN as i32..=i16::MAX as i32).contains(&q) {
                                self.raise_de(op_ip, mem);
                                return Ok(());
                            }
                            self.regs[r16::AX] = q as u16;
                            self.regs[r16::DX] = r as u16;
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            })
                        }
                    }
                }
            }

            // Group 4 (0xFE): INC/DEC r/m8.
            //   /0 = INC, /1 = DEC. Other sub-ops are undefined.
            0xFE => {
                let (_, sub, rm) = self.fetch_modrm(mem);
                let cf_before = self.has(flag::CF);
                let v = self.read_rm8(rm, mem);
                let r = match sub {
                    0 => {
                        let r = v.wrapping_add(1);
                        self.flags_add8(v, 1, 0, r);
                        r
                    }
                    1 => {
                        let r = v.wrapping_sub(1);
                        self.flags_sub8(v, 1, 0, r);
                        r
                    }
                    _ => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        })
                    }
                };
                // INC/DEC preserve CF on 8086.
                self.set_flag(flag::CF, cf_before);
                self.write_rm8(rm, mem, r);
            }

            // Group 5 (0xFF): r/m16 family.
            //   /0 = INC r/m16
            //   /1 = DEC r/m16
            //   /2 = CALL r/m16 (near, indirect)
            //   /3 = CALL m16:16 (far)            — deferred
            //   /4 = JMP r/m16 (near, indirect)
            //   /5 = JMP m16:16 (far)             — deferred
            //   /6 = PUSH r/m16
            0xFF => {
                let (_, sub, rm) = self.fetch_modrm(mem);
                match sub {
                    0 => {
                        let cf_before = self.has(flag::CF);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            let r = v.wrapping_add(1);
                            self.flags_add32(v, 1, 0, r);
                            self.set_flag(flag::CF, cf_before);
                            self.write_rm32(rm, mem, r);
                        } else {
                            let v = self.read_rm16(rm, mem);
                            let r = v.wrapping_add(1);
                            self.flags_add16(v, 1, 0, r);
                            self.set_flag(flag::CF, cf_before);
                            self.write_rm16(rm, mem, r);
                        }
                    }
                    1 => {
                        let cf_before = self.has(flag::CF);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            let r = v.wrapping_sub(1);
                            self.flags_sub32(v, 1, 0, r);
                            self.set_flag(flag::CF, cf_before);
                            self.write_rm32(rm, mem, r);
                        } else {
                            let v = self.read_rm16(rm, mem);
                            let r = v.wrapping_sub(1);
                            self.flags_sub16(v, 1, 0, r);
                            self.set_flag(flag::CF, cf_before);
                            self.write_rm16(rm, mem, r);
                        }
                    }
                    2 => {
                        if self.op_size_32 {
                            let target = self.read_rm32(rm, mem);
                            let ret_ip = self.ip;
                            self.push32(mem, ret_ip);
                            self.ip = target;
                        } else {
                            let target = self.read_rm16(rm, mem);
                            let ret_ip = self.ip as u16;
                            self.push16(mem, ret_ip);
                            self.ip = target as u32;
                        }
                    }
                    // CALL FAR indirect through memory. The operand
                    // size selects the pointer width: 16-bit → m16:16
                    // (2-byte offset + 2-byte selector at +2); 32-bit →
                    // m16:32 (4-byte offset + 2-byte selector at +4).
                    // Reading only a 16-bit offset in 32-bit mode (the
                    // old bug) truncates the target — the i386 kernel's
                    // `lcall *%cs:[ptr]` BIOS-service calls then jumped
                    // to a garbage low address (e.g. 0x2061).
                    3 => {
                        let ea = match rm {
                            Rm::Mem(ea) => ea,
                            Rm::Reg(_) => {
                                return Err(CpuError::Unimplemented {
                                    opcode,
                                    cs: op_cs,
                                    ip: op_ip,
                                })
                            }
                        };
                        let base = self.linear_seg(ea.seg, ea.off);
                        let cs = self.sregs[sreg::CS];
                        if self.op_size_32 {
                            let new_ip = self.mem_read_u32(mem, base);
                            let new_cs = self
                                .mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(4)));
                            self.push32(mem, cs as u32);
                            let ip = self.ip;
                            self.push32(mem, ip);
                            self.write_sreg(sreg::CS, new_cs, mem);
                            self.ip = new_ip;
                        } else {
                            let new_ip = self.mem_read_u16(mem, base);
                            let new_cs = self
                                .mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                            self.push16(mem, cs);
                            let ip = self.ip as u16;
                            self.push16(mem, ip);
                            self.write_sreg(sreg::CS, new_cs, mem);
                            self.ip = new_ip as u32;
                        }
                    }
                    4 => {
                        if self.op_size_32 {
                            let target = self.read_rm32(rm, mem);
                            self.ip = target;
                        } else {
                            let target = self.read_rm16(rm, mem);
                            self.ip = target as u32;
                        }
                    }
                    // JMP FAR indirect through memory (no stack
                    // activity). Same m16:16 vs m16:32 width handling as
                    // the far CALL above.
                    5 => {
                        let ea = match rm {
                            Rm::Mem(ea) => ea,
                            Rm::Reg(_) => {
                                return Err(CpuError::Unimplemented {
                                    opcode,
                                    cs: op_cs,
                                    ip: op_ip,
                                })
                            }
                        };
                        let base = self.linear_seg(ea.seg, ea.off);
                        if self.op_size_32 {
                            let new_ip = self.mem_read_u32(mem, base);
                            let new_cs = self
                                .mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(4)));
                            self.write_sreg(sreg::CS, new_cs, mem);
                            self.ip = new_ip;
                        } else {
                            let new_ip = self.mem_read_u16(mem, base);
                            let new_cs = self
                                .mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                            self.write_sreg(sreg::CS, new_cs, mem);
                            self.ip = new_ip as u32;
                        }
                    }
                    6 => {
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            self.push32(mem, v);
                        } else {
                            let v = self.read_rm16(rm, mem);
                            self.push16(mem, v);
                        }
                    }
                    _ => {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        })
                    }
                }
            }

            // MOV r/m8, imm8  — Group 11 /0
            0xC6 => {
                let (_, reg_field, rm) = self.fetch_modrm(mem);
                if reg_field != 0 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                let imm = self.fetch_u8(mem);
                self.write_rm8(rm, mem, imm);
            }
            // MOV r/m16, imm16  — or r/m32, imm32 when 0x66 prefix
            // is in effect.
            0xC7 => {
                let (_, reg_field, rm) = self.fetch_modrm(mem);
                if reg_field != 0 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    self.write_rm32(rm, mem, lo | (hi << 16));
                } else {
                    let imm = self.fetch_u16(mem);
                    self.write_rm16(rm, mem, imm);
                }
            }

            // PUSHA / POPA (80186+). Push all 8 GPRs in standard r16
            // order (AX, CX, DX, BX, SP_orig, BP, SI, DI) — the SP
            // value captured before the first push. POPA pops in
            // reverse and ignores the SP slot.
            //
            // Under the 0x66 prefix becomes PUSHAD / POPAD: each slot
            // is 4 bytes wide and the GPRs are read/written through
            // read_r32 / write_r32 so the full register file is saved.
            0x60 => {
                if self.op_size_32 {
                    let esp_orig = self.read_r32(r16::SP as u8);
                    self.push32(mem, self.read_r32(0)); // EAX
                    self.push32(mem, self.read_r32(1)); // ECX
                    self.push32(mem, self.read_r32(2)); // EDX
                    self.push32(mem, self.read_r32(3)); // EBX
                    self.push32(mem, esp_orig);
                    self.push32(mem, self.read_r32(5)); // EBP
                    self.push32(mem, self.read_r32(6)); // ESI
                    self.push32(mem, self.read_r32(7)); // EDI
                } else {
                    let sp_orig = self.regs[r16::SP];
                    let ax = self.regs[r16::AX];
                    self.push16(mem, ax);
                    let cx = self.regs[r16::CX];
                    self.push16(mem, cx);
                    let dx = self.regs[r16::DX];
                    self.push16(mem, dx);
                    let bx = self.regs[r16::BX];
                    self.push16(mem, bx);
                    self.push16(mem, sp_orig);
                    let bp = self.regs[r16::BP];
                    self.push16(mem, bp);
                    let si = self.regs[r16::SI];
                    self.push16(mem, si);
                    let di = self.regs[r16::DI];
                    self.push16(mem, di);
                }
            }
            0x61 => {
                if self.op_size_32 {
                    let edi = self.pop32(mem);
                    self.write_r32(7, edi);
                    let esi = self.pop32(mem);
                    self.write_r32(6, esi);
                    let ebp = self.pop32(mem);
                    self.write_r32(5, ebp);
                    let _ignored_esp = self.pop32(mem);
                    let ebx = self.pop32(mem);
                    self.write_r32(3, ebx);
                    let edx = self.pop32(mem);
                    self.write_r32(2, edx);
                    let ecx = self.pop32(mem);
                    self.write_r32(1, ecx);
                    let eax = self.pop32(mem);
                    self.write_r32(0, eax);
                } else {
                    self.regs[r16::DI] = self.pop16(mem);
                    self.regs[r16::SI] = self.pop16(mem);
                    self.regs[r16::BP] = self.pop16(mem);
                    let _ignored_sp = self.pop16(mem);
                    self.regs[r16::BX] = self.pop16(mem);
                    self.regs[r16::DX] = self.pop16(mem);
                    self.regs[r16::CX] = self.pop16(mem);
                    self.regs[r16::AX] = self.pop16(mem);
                }
            }

            // ARPL r/m16, r16 — adjust requested privilege level. If the
            // destination selector's RPL (low 2 bits) is less than the
            // source's, raise it to match and set ZF; otherwise leave the
            // destination unchanged and clear ZF. Only ZF is affected.
            // (In 32-bit this is ARPL; the 64-bit MOVSXD reuse does not
            // apply to this i386 core.)
            0x63 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let dst = self.read_rm16(rm, mem);
                let src = self.read_r16(reg);
                if (dst & 3) < (src & 3) {
                    self.write_rm16(rm, mem, (dst & !3) | (src & 3));
                    self.set_flag(flag::ZF, true);
                } else {
                    self.set_flag(flag::ZF, false);
                }
            }

            // IMUL r, r/m, imm (80186+ three-operand form).
            //   0x69 — imm16 (0x66: imm32)
            //   0x6B — imm8 sign-extended to operand width
            // The reg field of ModR/M is the destination; the source
            // is the r/m operand multiplied by the immediate. Under
            // the 0x66 prefix all three operands are 32-bit.
            0x69 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let imm = (lo | (hi << 16)) as i32 as i64;
                    let a = self.read_rm32(rm, mem) as i32 as i64;
                    let product = a.wrapping_mul(imm);
                    self.write_r32(reg, product as u32);
                    let overflow = i64::from(product as i32) != product;
                    self.set_flag(flag::CF, overflow);
                    self.set_flag(flag::OF, overflow);
                } else {
                    let imm = self.fetch_u16(mem) as i16 as i32;
                    let a = self.read_rm16(rm, mem) as i16 as i32;
                    let product = a.wrapping_mul(imm);
                    self.write_r16(reg, product as u16);
                    let overflow = (product as i16) as i32 != product;
                    self.set_flag(flag::CF, overflow);
                    self.set_flag(flag::OF, overflow);
                }
            }
            0x6B => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                if self.op_size_32 {
                    let imm = self.fetch_u8(mem) as i8 as i64;
                    let a = self.read_rm32(rm, mem) as i32 as i64;
                    let product = a.wrapping_mul(imm);
                    self.write_r32(reg, product as u32);
                    let overflow = i64::from(product as i32) != product;
                    self.set_flag(flag::CF, overflow);
                    self.set_flag(flag::OF, overflow);
                } else {
                    let imm = self.fetch_u8(mem) as i8 as i32;
                    let a = self.read_rm16(rm, mem) as i16 as i32;
                    let product = a.wrapping_mul(imm);
                    self.write_r16(reg, product as u16);
                    let overflow = (product as i16) as i32 != product;
                    self.set_flag(flag::CF, overflow);
                    self.set_flag(flag::OF, overflow);
                }
            }

            // ENTER imm16, imm8 (80186+) — function prologue.
            //   level = imm8 & 0x1F (only level 0 fully supported here)
            //   push BP ; frame = SP ; BP = frame ; SP -= imm16
            // Nesting (level > 0) would copy enclosing frame pointers
            // before the SP decrement; rare in modern code and not
            // emitted by any compiler we care about, so it returns
            // Unimplemented.
            0xC8 => {
                let frame_size = self.fetch_u16(mem);
                let level = self.fetch_u8(mem) & 0x1F;
                if level != 0 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                if self.op_size_32 {
                    // ENTER 32-bit: push 4 bytes of EBP, EBP = full
                    // ESP after the push, ESP -= frame_size.
                    let ebp = self.read_r32(5);
                    self.push32(mem, ebp);
                    let frame = self.read_stack_ptr();
                    self.write_r32(5, frame);
                    let new_sp = frame.wrapping_sub(frame_size as u32);
                    self.write_stack_ptr(new_sp);
                } else {
                    let bp = self.regs[r16::BP];
                    self.push16(mem, bp);
                    let frame = self.regs[r16::SP];
                    self.regs[r16::BP] = frame;
                    self.regs[r16::SP] = self.regs[r16::SP].wrapping_sub(frame_size);
                }
            }
            // LEAVE — function epilogue. Mirror of ENTER level 0.
            //   SP = BP ; BP = pop
            // Under 0x66 the dword form is used (ESP = EBP; pop EBP).
            0xC9 => {
                if self.op_size_32 {
                    let ebp = self.read_r32(5);
                    self.write_stack_ptr(ebp);
                    let new_ebp = self.pop32(mem);
                    self.write_r32(5, new_ebp);
                } else {
                    self.regs[r16::SP] = self.regs[r16::BP];
                    self.regs[r16::BP] = self.pop16(mem);
                }
            }

            // PUSH/POP segment registers. Encoding 0b000sss11{0,1} where
            // bits 3..4 select ES/CS/SS/DS in that order. POP CS (0x0F)
            // is the 2-byte opcode escape on 80286+ and undefined as
            // POP on 8086 — we leave it Unimplemented.
            // PUSH segment register: under 32-bit operand size the
            // selector is zero-extended to 32 bits and the push
            // decrements ESP by 4. Without this, a Linux kernel
            // entry path that pushes DS/ES/FS/SS as part of saving
            // user-mode segment state ends up with ESP misaligned
            // by 2 bytes on each push, eventually feeding wild
            // pointers back to the kernel via subsequent stack
            // reads at the wrong offset.
            0x06 => {
                let v = self.sregs[sreg::ES] as u32;
                if self.op_size_32 {
                    self.push32(mem, v);
                } else {
                    self.push16(mem, v as u16);
                }
            }
            0x0E => {
                let v = self.sregs[sreg::CS] as u32;
                if self.op_size_32 {
                    self.push32(mem, v);
                } else {
                    self.push16(mem, v as u16);
                }
            }
            0x16 => {
                let v = self.sregs[sreg::SS] as u32;
                if self.op_size_32 {
                    self.push32(mem, v);
                } else {
                    self.push16(mem, v as u16);
                }
            }
            0x1E => {
                let v = self.sregs[sreg::DS] as u32;
                if self.op_size_32 {
                    self.push32(mem, v);
                } else {
                    self.push16(mem, v as u16);
                }
            }
            0x07 => {
                let v = if self.op_size_32 {
                    self.pop32(mem) as u16
                } else {
                    self.pop16(mem)
                };
                if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                    return Ok(());
                }
                self.write_sreg(sreg::ES, v, mem);
            }
            0x17 => {
                let v = if self.op_size_32 {
                    self.pop32(mem) as u16
                } else {
                    self.pop16(mem)
                };
                if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                    return Ok(());
                }
                self.write_sreg(sreg::SS, v, mem);
            }
            0x1F => {
                let v = if self.op_size_32 {
                    self.pop32(mem) as u16
                } else {
                    self.pop16(mem)
                };
                if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                    return Ok(());
                }
                self.write_sreg(sreg::DS, v, mem);
            }

            // PUSH r16 (0x50..0x57) — push GPR in standard r16 order.
            // Under 0x66 prefix becomes PUSH r32: pushes the full 32
            // bits and decrements SP by 4. PUSH SP on the 8086 pushes
            // the value *after* the decrement (an 80186 quirk fixed
            // later). We push the original SP — the 80286+ behaviour —
            // because it is what every modern toolchain assumes.
            0x50..=0x57 => {
                let i = opcode - 0x50;
                if self.op_size_32 {
                    let v = self.read_r32(i);
                    self.push32(mem, v);
                } else {
                    let v = self.read_r16(i);
                    self.push16(mem, v);
                }
            }
            // POP r16 (0x58..0x5F) — under 0x66 prefix becomes POP r32.
            0x58..=0x5F => {
                let i = opcode - 0x58;
                if self.op_size_32 {
                    let v = self.pop32(mem);
                    self.write_r32(i, v);
                } else {
                    let v = self.pop16(mem);
                    self.write_r16(i, v);
                }
            }

            // PUSH imm16 / imm32 (under 0x66).
            0x68 => {
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    self.push32(mem, lo | (hi << 16));
                } else {
                    let imm = self.fetch_u16(mem);
                    self.push16(mem, imm);
                }
            }
            // PUSH imm8 (sign-extended). Under 0x66 the sign-extension
            // grows to 32 bits and the push is dword-sized.
            0x6A => {
                if self.op_size_32 {
                    let imm = self.fetch_u8(mem) as i8 as i32 as u32;
                    self.push32(mem, imm);
                } else {
                    let imm = self.fetch_u8(mem) as i8 as i16 as u16;
                    self.push16(mem, imm);
                }
            }

            // CALL rel16 / rel32 — under 0x66 the displacement is a
            // signed 32-bit offset *and* the return address pushed is
            // a full 32-bit dword (matching a 32-bit code segment's
            // near-CALL semantics). Without 0x66 it's the classic
            // 16-bit rel + 16-bit return push. Keeping the push width
            // tied to the operand size is what lets 32-bit cdecl
            // `add esp, 4`-style cleanup stay balanced.
            0xE8 => {
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let rel = (lo | (hi << 16)) as i32;
                    let ret_ip = self.ip;
                    self.push32(mem, ret_ip);
                    self.ip = self.ip.wrapping_add(rel as u32);
                } else {
                    let rel = self.fetch_u16(mem) as i16 as i32;
                    let ret_ip = self.ip as u16;
                    self.push16(mem, ret_ip);
                    self.ip = self.ip.wrapping_add(rel as u32);
                }
            }
            // CALL ptr16:16 — direct far call. Pushes CS then IP, then
            // loads CS:IP from the 4-byte immediate.
            0x9A => {
                let (new_ip, new_cs) = if self.op_size_32 {
                    // ptr16:32 layout: offset (4) then selector (2).
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let off = lo | (hi << 16);
                    let sel = self.fetch_u16(mem);
                    (off, sel)
                } else {
                    let off = self.fetch_u16(mem) as u32;
                    let sel = self.fetch_u16(mem);
                    (off, sel)
                };
                let cs = self.sregs[sreg::CS];
                // Return-frame width follows the operand size: a 32-bit
                // far CALL pushes CS zero-extended to a dword + the full
                // 32-bit return EIP (matching the indirect FF /3 path and
                // the RETF that unwinds it); 16-bit pushes word CS:IP.
                if self.op_size_32 {
                    self.push32(mem, cs as u32);
                    let ip = self.ip;
                    self.push32(mem, ip);
                } else {
                    self.push16(mem, cs);
                    let ip = self.ip as u16;
                    self.push16(mem, ip);
                }
                self.write_sreg(sreg::CS, new_cs, mem);
                self.ip = new_ip;
            }
            // JMP ptr16:16 — direct far jump. Under 0x66 the offset
            // becomes 32-bit (ptr16:32), the encoding Linux's PM
            // trampoline uses to enter the kernel at e.g.
            // 0xC0100000.
            0xEA => {
                let (new_ip, new_cs) = if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let off = lo | (hi << 16);
                    let sel = self.fetch_u16(mem);
                    (off, sel)
                } else {
                    let off = self.fetch_u16(mem) as u32;
                    let sel = self.fetch_u16(mem);
                    (off, sel)
                };
                self.write_sreg(sreg::CS, new_cs, mem);
                self.ip = new_ip;
            }
            // RET (near) — pop IP. Under 0x66 pops a 32-bit return
            // address, matching the CALL push width.
            0xC3 => {
                self.ip = if self.op_size_32 {
                    self.pop32(mem)
                } else {
                    self.pop16(mem) as u32
                };
            }
            // RET imm16 (near) — pop IP, then SP += imm16. Used by
            // callee-cleanup conventions.
            0xC2 => {
                let extra = self.fetch_u16(mem) as u32;
                self.ip = if self.op_size_32 {
                    self.pop32(mem)
                } else {
                    self.pop16(mem) as u32
                };
                // Pop the caller's argument bytes at the stack's native
                // width so the count carries into the upper half of ESP
                // for a 32-bit stack (not just the low 16 bits of SP).
                let sp = self.read_stack_ptr().wrapping_add(extra);
                self.write_stack_ptr(sp);
            }
            // RETF — pop IP then CS (far return). Width follows the
            // operand size: a 32-bit far return pops a dword EIP and a
            // dword CS slot (selector in the low 16, high 16 ignored),
            // matching the far CALL that built the frame.
            0xCB => {
                if self.op_size_32 {
                    self.ip = self.pop32(mem);
                    let cs = self.pop32(mem) as u16;
                    self.write_sreg(sreg::CS, cs, mem);
                } else {
                    self.ip = self.pop16(mem) as u32;
                    let cs = self.pop16(mem);
                    self.write_sreg(sreg::CS, cs, mem);
                }
            }
            // RETF imm16 — far return with callee-side stack cleanup.
            0xCA => {
                let extra = self.fetch_u16(mem) as u32;
                if self.op_size_32 {
                    self.ip = self.pop32(mem);
                    let cs = self.pop32(mem) as u16;
                    self.write_sreg(sreg::CS, cs, mem);
                } else {
                    self.ip = self.pop16(mem) as u32;
                    let cs = self.pop16(mem);
                    self.write_sreg(sreg::CS, cs, mem);
                }
                // Pop the caller's argument bytes at the stack's native
                // width (carry into the high half of ESP for a 32-bit stack).
                let sp = self.read_stack_ptr().wrapping_add(extra);
                self.write_stack_ptr(sp);
            }

            // PUSHF / PUSHFD — push FLAGS / EFLAGS. Under 0x66 the
            // pushed value widens to a dword; we only model the low
            // 16 bits of EFLAGS, so the high half pushes as zero.
            0x9C => {
                if self.op_size_32 {
                    // Combine low + preserved high halves so a PUSHFD
                    // followed by POPFD round-trips AC/ID/etc.
                    let full = (self.flags as u32) | ((self.flags_high as u32) << 16);
                    self.push32(mem, full);
                } else {
                    self.push16(mem, self.flags);
                }
            }
            // POPF / POPFD — pop FLAGS / EFLAGS. In PM, real silicon
            // strips bits the current CPL isn't allowed to change:
            //   * CPL > 0   → IOPL field (bits 12-13) stays pinned —
            //     otherwise userspace would self-promote to ring 0
            //     for the purpose of CLI/STI/IN/OUT.
            //   * CPL > IOPL → IF (bit 9) stays pinned — so an
            //     untrusted task can't disable interrupts.
            // Real-mode POPF (PE=0) writes through unmasked. We don't
            // model VM/RF/NT here — those fields stay at their
            // current value implicitly via the mask.
            0x9D => {
                let popped_full = if self.op_size_32 {
                    self.pop32(mem)
                } else {
                    self.pop16(mem) as u32
                };
                let popped = popped_full as u16;
                // POPFD restores the upper-16 EFLAGS half too. Mask
                // out the always-zero bit 15 and reserved bit 22+,
                // but keep AC (18), ID (21), VIP (20), VIF (19),
                // VM (17), RF (16) so Linux's i486 detection (flip
                // AC, see if it sticks) succeeds.
                if self.op_size_32 {
                    let high = ((popped_full >> 16) & 0x003F) as u16;
                    self.flags_high = high;
                }
                self.flags = if self.cr0 & 1 != 0 {
                    let cpl = self.sregs[sreg::CS] & 3;
                    let iopl = (self.flags >> 12) & 3;
                    let mut mask: u16 = 0xFFFF;
                    if cpl > 0 {
                        mask &= !0x3000; // preserve IOPL
                    }
                    if cpl > iopl {
                        mask &= !flag::IF; // preserve IF
                    }
                    (popped & mask) | (self.flags & !mask)
                } else {
                    popped
                };
            }

            // CBW — sign-extend AL into AX. AH = AL & 0x80 ? 0xFF : 0x00.
            0x98 => {
                if self.op_size_32 {
                    // CWDE — sign-extend AX into EAX.
                    let ax = self.regs[r16::AX] as i16 as i32 as u32;
                    self.write_r32(0, ax);
                } else {
                    let al = self.read_r8(0);
                    self.regs[r16::AX] = al as i8 as i16 as u16;
                }
            }
            // CWD — sign-extend AX into DX:AX. Under 0x66 becomes CDQ:
            // sign-extend EAX into EDX:EAX.
            0x99 => {
                if self.op_size_32 {
                    let eax = self.read_r32(0) as i32;
                    self.write_r32(2, if eax < 0 { 0xFFFF_FFFF } else { 0 });
                } else {
                    let ax = self.regs[r16::AX] as i16;
                    self.regs[r16::DX] = if ax < 0 { 0xFFFF } else { 0 };
                }
            }

            // SAHF — copy AH into the low byte of FLAGS (SF/ZF/AF/PF/CF).
            // Bit 1 of FLAGS is reserved and reads as 1; the other
            // reserved low-byte bits (3, 5) stay zero. Bits 8..15 are
            // untouched.
            0x9E => {
                let ah = self.read_r8(4);
                let mask = flag::CF | flag::PF | (1 << 4) | flag::ZF | flag::SF;
                let preserve = self.flags & !mask;
                self.flags = preserve | (ah as u16 & mask);
            }
            // LAHF — load AH from the low byte of FLAGS.
            0x9F => {
                let mask = flag::CF | flag::PF | (1 << 4) | flag::ZF | flag::SF;
                // Bit 1 reads back as 1 on real x86.
                let ah = ((self.flags & mask) as u8) | 0x02;
                self.write_r8(4, ah);
            }

            // INT3 — single-byte software interrupt to vector 3.
            // Subject to the software-INT gate-DPL check (CPL must
            // be ≤ gate.DPL), so user-space INT3 traps unless the
            // kernel left the breakpoint gate open at DPL=3.
            0xCC => {
                if self.raise_gp_if_gate_too_privileged(3, op_ip, mem) {
                    return Ok(());
                }
                self.do_interrupt(3, mem);
            }
            // INT imm8 — software interrupt to the vector named by imm8.
            // The bios_hook gets first refusal: a Rust-side handler for
            // BIOS vectors (0x10 video, 0x13 disk, 0x16 keyboard, etc.)
            // returns true and the CPU treats the INT as "done" without
            // pushing a frame. Anything not claimed by the hook falls
            // through to the standard IVT/IDT dispatch — but first
            // the gate-DPL guard runs so user-space INT $entry can't
            // reach kernel-only vectors.
            0xCD => {
                let n = self.fetch_u8(mem);
                if let Some(hook) = self.bios_hook {
                    if hook(self, mem, io, n) {
                        // Host handled it — no frame, no IRET needed.
                        return Ok(());
                    }
                }
                if self.raise_gp_if_gate_too_privileged(n, op_ip, mem) {
                    return Ok(());
                }
                self.do_interrupt(n, mem);
            }
            // INTO — if OF=1, raise INT 4. Otherwise a no-op.
            // INT 4 (#OF) shares the software-INT gate-DPL check.
            0xCE => {
                if self.has(flag::OF) {
                    if self.raise_gp_if_gate_too_privileged(4, op_ip, mem) {
                        return Ok(());
                    }
                    self.do_interrupt(4, mem);
                }
            }
            // IRET — pop EIP, CS, EFLAGS (in that order). Under the
            // 0x66 prefix (IRETD), pops a dword frame; otherwise a
            // word frame. The IF/TF state before the original INT is
            // restored as part of FLAGS.
            //
            // Cross-ring return: when in PE mode and the popped CS's
            // RPL is greater than the current CPL, real silicon also
            // pops the caller's SS:ESP (a five-word / five-dword
            // frame instead of three). That's how a kernel returns
            // to user-space via IRET. We honour that here so a
            // future ring 3 + TSS implementation can land cleanly.
            0xCF => {
                let is_pe = (self.cr0 & 1) != 0;
                let cpl_before = if is_pe {
                    (self.sregs[sreg::CS] & 3) as u8
                } else {
                    0
                };
                if self.op_size_32 {
                    let new_ip = self.pop32(mem);
                    let new_cs = self.pop32(mem) as u16;
                    // EFLAGS pop — only the low 16 are modelled; the
                    // upper half (resume / V86 / AC bits) reads back
                    // but isn't acted upon yet.
                    let new_eflags = self.pop32(mem);
                    let cpl_after = if is_pe { (new_cs & 3) as u8 } else { 0 };
                    let cross_ring = cpl_after > cpl_before;
                    let (popped_esp, popped_ss) = if cross_ring {
                        let esp = self.pop32(mem);
                        let ss = self.pop32(mem) as u16;
                        (Some(esp), Some(ss))
                    } else {
                        (None, None)
                    };
                    self.ip = new_ip;
                    self.write_sreg(sreg::CS, new_cs, mem);
                    self.flags = new_eflags as u16;
                    if let (Some(esp), Some(ss)) = (popped_esp, popped_ss) {
                        self.write_sreg(sreg::SS, ss, mem);
                        self.write_r32(r16::SP as u8, esp);
                    }
                } else {
                    let new_ip = self.pop16(mem) as u32;
                    let new_cs = self.pop16(mem);
                    let new_flags = self.pop16(mem);
                    let cpl_after = if is_pe { (new_cs & 3) as u8 } else { 0 };
                    let cross_ring = cpl_after > cpl_before;
                    let (popped_sp, popped_ss) = if cross_ring {
                        let sp = self.pop16(mem);
                        let ss = self.pop16(mem);
                        (Some(sp), Some(ss))
                    } else {
                        (None, None)
                    };
                    self.ip = new_ip;
                    self.write_sreg(sreg::CS, new_cs, mem);
                    self.flags = new_flags;
                    if let (Some(sp), Some(ss)) = (popped_sp, popped_ss) {
                        self.write_sreg(sreg::SS, ss, mem);
                        self.regs[r16::SP] = sp;
                    }
                }
            }

            // INC r16 (0x40-0x47) / DEC r16 (0x48-0x4F). Per the 8086,
            // these preserve CF and update ZF/SF/PF/OF/AF. Under 0x66
            // they operate on the full 32-bit register.
            0x40..=0x47 => {
                let i = opcode - 0x40;
                let cf_before = self.has(flag::CF);
                if self.op_size_32 {
                    let a = self.read_r32(i);
                    let r = a.wrapping_add(1);
                    self.flags_add32(a, 1, 0, r);
                    self.write_r32(i, r);
                } else {
                    let a = self.read_r16(i);
                    let r = a.wrapping_add(1);
                    self.flags_add16(a, 1, 0, r);
                    self.write_r16(i, r);
                }
                self.set_flag(flag::CF, cf_before);
            }
            0x48..=0x4F => {
                let i = opcode - 0x48;
                let cf_before = self.has(flag::CF);
                if self.op_size_32 {
                    let a = self.read_r32(i);
                    let r = a.wrapping_sub(1);
                    self.flags_sub32(a, 1, 0, r);
                    self.write_r32(i, r);
                } else {
                    let a = self.read_r16(i);
                    let r = a.wrapping_sub(1);
                    self.flags_sub16(a, 1, 0, r);
                    self.write_r16(i, r);
                }
                self.set_flag(flag::CF, cf_before);
            }

            // MOV AL, moffs8 / MOV moffs8, AL (0xA0/0xA2) and the
            // word/dword accumulator forms (0xA1/0xA3). `moffs` is a
            // direct memory offset that follows the opcode — 16-bit
            // under the default address size, 32-bit under 0x67. The
            // segment is DS (honoring an override). Compilers emit
            // these for absolute global-variable access.
            0xA0 => {
                let off = self.fetch_moffs(mem);
                let lin = self.linear_seg(self.string_src_seg(), off);
                let v = self.mem_read_u8(mem, lin);
                self.write_r8(0, v);
            }
            0xA1 => {
                let off = self.fetch_moffs(mem);
                let lin = self.linear_seg(self.string_src_seg(), off);
                if self.op_size_32 {
                    let v = self.mem_read_u32(mem, lin);
                    self.write_r32(0, v);
                } else {
                    let v = self.mem_read_u16(mem, lin);
                    self.write_r16(0, v);
                }
            }
            0xA2 => {
                let off = self.fetch_moffs(mem);
                let lin = self.linear_seg(self.string_src_seg(), off);
                let al = self.read_r8(0);
                self.mem_write_u8(mem, lin, al);
            }
            0xA3 => {
                let off = self.fetch_moffs(mem);
                let lin = self.linear_seg(self.string_src_seg(), off);
                if self.op_size_32 {
                    let v = self.read_r32(0);
                    self.mem_write_u32(mem, lin, v);
                } else {
                    let v = self.read_r16(0);
                    self.mem_write_u16(mem, lin, v);
                }
            }

            // TEST AL, imm8
            0xA8 => {
                let imm = self.fetch_u8(mem);
                let result = self.read_r8(0) & imm;
                self.flags_logic8(result);
            }
            // TEST AX, imm16 — under 0x66 becomes TEST EAX, imm32.
            0xA9 => {
                if self.op_size_32 {
                    let lo = self.fetch_u16(mem) as u32;
                    let hi = self.fetch_u16(mem) as u32;
                    let imm = lo | (hi << 16);
                    let result = self.read_r32(0) & imm;
                    self.flags_logic32(result);
                } else {
                    let imm = self.fetch_u16(mem);
                    let result = self.read_r16(0) & imm;
                    self.flags_logic16(result);
                }
            }

            // Port-IO instructions (IN / OUT, byte and word/dword
            // forms, imm8 and DX-port variants) are all IOPL-
            // sensitive. We don't model the per-port permission
            // bitmap in TSS yet — just the coarse `CPL <= IOPL`
            // check covers the common "user can't touch hardware"
            // case (real silicon would additionally consult the
            // bitmap when CPL > IOPL).
            0xEC => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // IN AL, DX
                let port = self.regs[r16::DX];
                let v = self.port_read(io, port);
                self.write_r8(0, v);
            }
            0xEE => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // OUT DX, AL
                let port = self.regs[r16::DX];
                let v = self.read_r8(0);
                self.port_write(io, port, v);
            }
            0xE4 => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // IN AL, imm8
                let port = self.fetch_u8(mem) as u16;
                let v = self.port_read(io, port);
                self.write_r8(0, v);
            }
            0xE5 => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // IN AX/EAX, imm8 — two/four byte reads from
                // consecutive ports. The 0x66 prefix widens to EAX
                // (four bytes from port..port+3).
                let port = self.fetch_u8(mem) as u16;
                if self.op_size_32 {
                    let v = port_read_u32(self, io, port);
                    self.write_r32(0, v);
                } else {
                    let lo = self.port_read(io, port) as u16;
                    let hi = self.port_read(io, port.wrapping_add(1)) as u16;
                    self.regs[r16::AX] = lo | (hi << 8);
                }
            }
            0xE6 => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // OUT imm8, AL
                let port = self.fetch_u8(mem) as u16;
                let v = self.read_r8(0);
                self.port_write(io, port, v);
            }
            0xE7 => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // OUT imm8, AX/EAX — two/four byte writes to
                // consecutive ports (0x66 → 32-bit form).
                let port = self.fetch_u8(mem) as u16;
                if self.op_size_32 {
                    let v = self.read_r32(0);
                    port_write_u32(self, io, port, v);
                } else {
                    let ax = self.regs[r16::AX];
                    self.port_write(io, port, ax as u8);
                    self.port_write(io, port.wrapping_add(1), (ax >> 8) as u8);
                }
            }
            0xED => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // IN AX/EAX, DX
                let port = self.regs[r16::DX];
                if self.op_size_32 {
                    let v = port_read_u32(self, io, port);
                    self.write_r32(0, v);
                } else {
                    let lo = self.port_read(io, port) as u16;
                    let hi = self.port_read(io, port.wrapping_add(1)) as u16;
                    self.regs[r16::AX] = lo | (hi << 8);
                }
            }
            0xEF => {
                if self.raise_gp_if_below_iopl(op_ip, mem) {
                    return Ok(());
                }
                // OUT DX, AX/EAX
                let port = self.regs[r16::DX];
                if self.op_size_32 {
                    let v = self.read_r32(0);
                    port_write_u32(self, io, port, v);
                } else {
                    let ax = self.regs[r16::AX];
                    self.port_write(io, port, ax as u8);
                    self.port_write(io, port.wrapping_add(1), (ax >> 8) as u8);
                }
            }

            // XLAT — AL = mem[DS:BX+AL] (with seg-override if present).
            // The translation-table idiom; 8086 lookups in 256-entry maps.
            0xD7 => {
                let seg = self.seg_override.unwrap_or(sreg::DS);
                let off = self.regs[r16::BX].wrapping_add(self.read_r8(0) as u16);
                let v = self.mem_read_u8(mem, self.linear_seg(seg, off as u32));
                self.write_r8(0, v);
            }

            // BCD adjusts. Rare in modern code but completing 8086 ISA.
            // DAA — Decimal Adjust after Add. Per Intel SDM Vol. 2.
            0x27 => {
                let old_al = self.read_r8(0);
                let old_cf = self.has(flag::CF);
                let mut al = old_al;
                let mut cf_out;
                if (al & 0x0F) > 9 || self.has(1 << 4) {
                    let (v, c) = al.overflowing_add(6);
                    al = v;
                    cf_out = c || old_cf;
                    self.set_flag(1 << 4, true); // AF
                } else {
                    self.set_flag(1 << 4, false);
                    cf_out = old_cf;
                }
                if old_al > 0x99 || old_cf {
                    al = al.wrapping_add(0x60);
                    cf_out = true;
                }
                self.write_r8(0, al);
                self.set_flag(flag::CF, cf_out);
                self.flags_zsp8(al);
            }
            // DAS — Decimal Adjust after Subtract.
            0x2F => {
                let old_al = self.read_r8(0);
                let old_cf = self.has(flag::CF);
                let mut al = old_al;
                let mut cf_out;
                if (al & 0x0F) > 9 || self.has(1 << 4) {
                    let (v, c) = al.overflowing_sub(6);
                    al = v;
                    cf_out = c || old_cf;
                    self.set_flag(1 << 4, true);
                } else {
                    self.set_flag(1 << 4, false);
                    cf_out = old_cf;
                }
                if old_al > 0x99 || old_cf {
                    al = al.wrapping_sub(0x60);
                    cf_out = true;
                }
                self.write_r8(0, al);
                self.set_flag(flag::CF, cf_out);
                self.flags_zsp8(al);
            }
            // AAA — ASCII Adjust after Addition.
            0x37 => {
                let al = self.read_r8(0);
                if (al & 0x0F) > 9 || self.has(1 << 4) {
                    let new_al = al.wrapping_add(6) & 0x0F;
                    let new_ah = self.read_r8(4).wrapping_add(1);
                    self.write_r8(0, new_al);
                    self.write_r8(4, new_ah);
                    self.set_flag(1 << 4, true);
                    self.set_flag(flag::CF, true);
                } else {
                    self.write_r8(0, al & 0x0F);
                    self.set_flag(1 << 4, false);
                    self.set_flag(flag::CF, false);
                }
            }
            // AAS — ASCII Adjust after Subtraction.
            0x3F => {
                let al = self.read_r8(0);
                if (al & 0x0F) > 9 || self.has(1 << 4) {
                    let new_al = al.wrapping_sub(6) & 0x0F;
                    let new_ah = self.read_r8(4).wrapping_sub(1);
                    self.write_r8(0, new_al);
                    self.write_r8(4, new_ah);
                    self.set_flag(1 << 4, true);
                    self.set_flag(flag::CF, true);
                } else {
                    self.write_r8(0, al & 0x0F);
                    self.set_flag(1 << 4, false);
                    self.set_flag(flag::CF, false);
                }
            }
            // AAM — ASCII Adjust after Multiply. imm8 = base (typically 10).
            // Divide-by-zero raises a Divide Error like DIV.
            0xD4 => {
                let base = self.fetch_u8(mem);
                if base == 0 {
                    self.raise_de(op_ip, mem);
                    return Ok(());
                }
                let al = self.read_r8(0);
                let ah = al / base;
                let new_al = al % base;
                self.write_r8(4, ah);
                self.write_r8(0, new_al);
                self.flags_zsp8(new_al);
            }
            // AAD — ASCII Adjust before Division.
            0xD5 => {
                let base = self.fetch_u8(mem);
                let al = self.read_r8(0);
                let ah = self.read_r8(4);
                let new_al = ah.wrapping_mul(base).wrapping_add(al);
                self.write_r8(0, new_al);
                self.write_r8(4, 0);
                self.flags_zsp8(new_al);
            }

            // Carry-flag tweaks.
            0xF5 => {
                let c = self.has(flag::CF);
                self.set_flag(flag::CF, !c);
            } // CMC
            0xF8 => {
                self.set_flag(flag::CF, false);
            } // CLC
            0xF9 => {
                self.set_flag(flag::CF, true);
            } // STC

            // LOCK (0xF0) and WAIT (0x9B) prefixes — no-op for a single-
            // CPU emulator without an FPU. Consume the byte and continue;
            // the next instruction runs in the same step boundary.
            // (LOCK is technically only valid on a small set of opcodes;
            // we accept it anywhere — that matches what most assemblers
            // emit and is harmless.)
            0x9B | 0xF0 => {
                // The byte is already fetched. We could recurse into a
                // fresh instruction here, but to keep one instruction
                // per step() call we surface it as a no-op for now.
                // The next step() will see whatever comes after.
            }

            // 0x0F — two-byte opcode escape. On the 8086 this byte is
            // POP CS (undocumented and rarely useful); on the 80286+
            // it became the prefix for the expanding "extended" opcode
            // space that protected-mode and i386+ instructions live in.
            // We dispatch on the second byte. Unknown second bytes are
            // surfaced through CpuError::Unimplemented with that byte
            // as the `opcode` field so error messages stay meaningful.
            0x0F => {
                let op2 = self.fetch_u8(mem);
                match op2 {
                    // Group 6 — SLDT (/0) STR (/1) LLDT (/2) LTR (/3)
                    // VERR (/4) VERW (/5). Each operates on a 16-bit
                    // selector in r/m16.
                    0x00 => {
                        let (_, sub, rm) = self.fetch_modrm(mem);
                        match sub {
                            0 => {
                                // SLDT — unprivileged read.
                                let v = self.ldtr;
                                self.write_rm16(rm, mem, v);
                            }
                            1 => {
                                // STR — unprivileged read.
                                let v = self.tr;
                                self.write_rm16(rm, mem, v);
                            }
                            2 => {
                                // LLDT — CPL=0 only.
                                if self.raise_gp_if_user(op_ip, mem) {
                                    return Ok(());
                                }
                                self.ldtr = self.read_rm16(rm, mem);
                            }
                            3 => {
                                // LTR — CPL=0 only.
                                if self.raise_gp_if_user(op_ip, mem) {
                                    return Ok(());
                                }
                                self.tr = self.read_rm16(rm, mem);
                            }
                            // VERR — set ZF=1 iff the segment selected
                            // by the low 16 bits of r/m is readable
                            // from the current CPL. Linux uses this
                            // sparingly; we don't see it on the boot
                            // path, but symmetry with VERW is cheap.
                            4 => {
                                let sel = self.read_rm16(rm, mem);
                                let ok = self.selector_accessible(mem, sel, false);
                                self.set_flag(flag::ZF, ok);
                            }
                            // VERW — set ZF=1 iff the segment is
                            // writable from the current CPL. Linux's
                            // MDS / RETBleed / MMIO-stale-data
                            // mitigations issue `verw $perf_event_ds`
                            // for the *side effect* (CPU-buffer
                            // clear on real silicon), not the ZF
                            // result — but if we don't implement
                            // the opcode at all the kernel oopses
                            // mid-mitigation. ZF must still be
                            // architecturally well-defined, so we
                            // honor the access-bit check.
                            5 => {
                                let sel = self.read_rm16(rm, mem);
                                let ok = self.selector_accessible(mem, sel, true);
                                self.set_flag(flag::ZF, ok);
                            }
                            _ => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        }
                    }
                    // Group 7 — LGDT, LIDT, SGDT, SIDT, SMSW, LMSW,
                    // INVLPG depending on the ModR/M reg field.
                    0x01 => {
                        let (mode, sub, rm) = self.fetch_modrm(mem);
                        // SMSW (/4) and LMSW (/6) accept r/m16 with
                        // mod=11. The other sub-ops require memory.
                        match sub {
                            4 => {
                                // SMSW — unprivileged read of CR0 low 16.
                                let v = self.cr0 as u16;
                                self.write_rm16(rm, mem, v);
                                return Ok(());
                            }
                            6 => {
                                // LMSW — CPL=0 only.
                                if self.raise_gp_if_user(op_ip, mem) {
                                    return Ok(());
                                }
                                let v = self.read_rm16(rm, mem);
                                // LMSW loads CR0 bits 0-3 (PE/MP/EM/TS)
                                // from the operand but CANNOT clear PE —
                                // once in protected mode it stays set
                                // (Intel SDM: LMSW can't return to real
                                // mode). Lock an already-set PE.
                                // LMSW loads CR0 bits 0-3 (PE/MP/EM/TS)
                                // from the operand but CANNOT clear PE —
                                // once in protected mode it stays set
                                // (Intel SDM: LMSW can't return to real
                                // mode). Lock an already-set PE.
                                let pe_locked = self.cr0 & 1;
                                self.cr0 = (self.cr0 & !0xF) | (v as u32 & 0xF) | pe_locked;
                                return Ok(());
                            }
                            _ => {}
                        }
                        // RDTSCP — 0x0F 0x01 0xF9 (mode=11 sub=7 rm=1).
                        // Reads TSC into EDX:EAX and TSC_AUX into ECX.
                        // We don't model TSC_AUX as state — Linux uses
                        // it for vget_cpu() to tag the current CPU, and
                        // returning 0 means "CPU 0", which is fine for
                        // our single-threaded VM. The vDSO clock_gettime
                        // path falls through to this on every userspace
                        // gettimeofday().
                        if mode == 0b11 && sub == 7 {
                            if let Rm::Reg(1) = rm {
                                self.write_r32(0, self.tsc as u32);
                                self.write_r32(2, (self.tsc >> 32) as u32);
                                self.write_r32(1, self.tsc_aux);
                                return Ok(());
                            }
                        }
                        let ea = match rm {
                            Rm::Mem(ea) => ea,
                            Rm::Reg(_) => {
                                return Err(CpuError::UnimplementedModRm {
                                    opcode: op2,
                                    mode,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        match sub {
                            // SGDT / SIDT — store pseudo-descriptor.
                            0 | 1 => {
                                let base_linear = self.linear_seg(ea.seg, ea.off);
                                let desc = if sub == 0 { self.gdtr } else { self.idtr };
                                self.mem_write_u16(mem, base_linear, desc.limit);
                                self.mem_write_u16(
                                    mem,
                                    base_linear.wrapping_add(2),
                                    desc.base as u16,
                                );
                                self.mem_write_u16(
                                    mem,
                                    base_linear.wrapping_add(4),
                                    (desc.base >> 16) as u16,
                                );
                            }
                            // LGDT / LIDT — load pseudo-descriptor. CPL=0 only.
                            2 | 3 => {
                                if self.raise_gp_if_user(op_ip, mem) {
                                    return Ok(());
                                }
                                let base_linear = self.linear_seg(ea.seg, ea.off);
                                let limit = self.mem_read_u16(mem, base_linear);
                                let base_lo = self.mem_read_u16(mem, base_linear.wrapping_add(2));
                                let base_hi = self.mem_read_u16(mem, base_linear.wrapping_add(4));
                                let base = (base_lo as u32) | ((base_hi as u32) << 16);
                                let desc = DescriptorTable { limit, base };
                                if sub == 2 {
                                    self.gdtr = desc;
                                } else {
                                    self.idtr = desc;
                                }
                            }
                            // INVLPG m — invalidate TLB entry. CPL=0 only.
                            7 => {
                                if self.raise_gp_if_user(op_ip, mem) {
                                    return Ok(());
                                }
                                let _ = self.linear_seg(ea.seg, ea.off);
                                // We only model a single-entry fetch
                                // TLB, so any INVLPG drops it — even
                                // if the named page wasn't cached,
                                // dropping is harmless.
                                self.invalidate_fetch_tlb();
                            }
                            _ => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        }
                    }
                    // UD2 (0F 0B) — the guaranteed-#UD instruction.
                    // Compilers emit this where control-flow must not
                    // reach (BUG()/unreachable()/panic) so the kernel's
                    // #UD handler can print a backtrace pointing at the
                    // exact byte. The fault frame saves the *start* of
                    // UD2, so the kernel can decode the bytes after it
                    // as a BUG-table key (this is how panic_on_oops
                    // finds its message).
                    0x0B => {
                        self.ip = op_ip;
                        self.do_interrupt(6, mem);
                    }
                    // MOV r32, CRn — 0x0F 0x20 /reg. CR0/CR2/CR3 routed
                    // through the full 32-bit GPR (write_r32) so the
                    // upper half of each control register survives.
                    // The #PF handler reads CR2 here to learn which
                    // linear address it must page in. CPL=0 only —
                    // userspace MOV r,CR is #GP(0).
                    0x20 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let modrm = self.fetch_u8(mem);
                        let reg = (modrm >> 3) & 0x07;
                        let rm = modrm & 0x07;
                        let value = match reg {
                            0 => self.cr0,
                            2 => self.cr2,
                            3 => self.cr3,
                            4 => self.cr4,
                            _ => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        self.write_r32(rm, value);
                    }
                    // MOV r32, DRn — 0x0F 0x21 /reg. CPL=0 only.
                    // Debug registers are stub-only: reads return
                    // whatever was last written. Linux's context
                    // switcher does `mov %dr6, %eax` early to sample
                    // status, then clears them — so a faulting #UD
                    // here would crash the kernel before it even
                    // prints. DR4/DR5 are independent slots here;
                    // real hardware aliases them to DR6/DR7.
                    0x21 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let modrm = self.fetch_u8(mem);
                        let reg = (modrm >> 3) & 0x07;
                        let rm = modrm & 0x07;
                        self.write_r32(rm, self.dr[reg as usize]);
                    }
                    // MOV CRn, r32 — 0x0F 0x22 /reg. CPL=0 only.
                    0x22 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let modrm = self.fetch_u8(mem);
                        let reg = (modrm >> 3) & 0x07;
                        let rm = modrm & 0x07;
                        let value = self.read_r32(rm);
                        match reg {
                            0 => {
                                self.cr0 = value;
                                // CR0.PG toggle changes whether paging
                                // applies at all — drop any cached
                                // identity-mapped fetches.
                                self.invalidate_fetch_tlb();
                            }
                            2 => self.cr2 = value,
                            3 => {
                                self.cr3 = value;
                                // CR3 reload is the architectural "flush
                                // TLB" signal on i386 — invalidate.
                                self.invalidate_fetch_tlb();
                            }
                            4 => self.cr4 = value,
                            _ => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        }
                    }
                    // MOV DRn, r32 — 0x0F 0x23 /reg. Counterpart to
                    // 0x21. Stub: store the value, no semantic action.
                    // CPL=0 only.
                    0x23 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let modrm = self.fetch_u8(mem);
                        let reg = (modrm >> 3) & 0x07;
                        let rm = modrm & 0x07;
                        self.dr[reg as usize] = self.read_r32(rm);
                    }
                    // 0x0F 0xAE — group with FXSAVE/FXRSTOR (when
                    // mod != 11) and the fences/CLFLUSH (when
                    // mod == 11 or for CLFLUSH /7). Decoded by both
                    // the reg field and the mod bits.
                    0xAE => {
                        let modrm = self.fetch_u8(mem);
                        let mode = modrm >> 6;
                        let sub = (modrm >> 3) & 0x07;
                        let rm_field = modrm & 0x07;
                        // Fences and CLFLUSH-with-mod=11 use no memory.
                        if mode == 0b11 {
                            match sub {
                                5..=7 => {
                                    // LFENCE / MFENCE / SFENCE — all
                                    // no-ops in our single-threaded model.
                                }
                                _ => {
                                    return Err(CpuError::Unimplemented {
                                        opcode: op2,
                                        cs: op_cs,
                                        ip: op_ip,
                                    });
                                }
                            }
                        } else {
                            let ea = if self.addr_size_32 {
                                self.compute_ea_32(mode, rm_field, mem)
                            } else {
                                self.compute_ea(mode, rm_field, mem)
                            };
                            let addr = self.linear_seg(ea.seg, ea.off);
                            match sub {
                                0 => {
                                    // FXSAVE m512 — save the full x87 + SSE
                                    // state. We DO model both (fpu_st/top/
                                    // cw/sw + xmm), so an all-zero image would
                                    // be wrong: a caller that FXSAVEs, uses the
                                    // FPU, then FXRSTORs (e.g. glibc's lazy-PLT
                                    // resolver bracketing) would lose the
                                    // saved x87 stack/TOP — corrupting the
                                    // restored register that holds a result.
                                    let cw = self.fpu_cw;
                                    let sw = self.fpu_status_word();
                                    for off in (0..512u32).step_by(4) {
                                        self.mem_write_u32(mem, addr.wrapping_add(off), 0);
                                    }
                                    self.mem_write_u16(mem, addr, cw);
                                    self.mem_write_u16(mem, addr.wrapping_add(2), sw);
                                    // abridged tag word (+4): 0xFF = all in
                                    // use (our model doesn't track per-reg
                                    // tags; FXRSTOR below reloads all 8).
                                    self.mem_write_u8(mem, addr.wrapping_add(4), 0xFF);
                                    // MXCSR (+24): default; not otherwise
                                    // modeled.
                                    self.mem_write_u32(mem, addr.wrapping_add(24), 0x0000_1F80);
                                    // ST(i), logical order, 80-bit extended,
                                    // at +32 + i*16.
                                    for i in 0..8u32 {
                                        let (mant, se) = self.fpu_st(i as u8).to_f80_parts();
                                        let foff = addr.wrapping_add(32 + i * 16);
                                        self.mem_write_u32(mem, foff, mant as u32);
                                        self.mem_write_u32(
                                            mem,
                                            foff.wrapping_add(4),
                                            (mant >> 32) as u32,
                                        );
                                        self.mem_write_u16(mem, foff.wrapping_add(8), se);
                                    }
                                    // XMM0-7 at +160 + i*16.
                                    for i in 0..8u32 {
                                        let v = self.xmm[i as usize];
                                        self.mem_write_u128(
                                            mem,
                                            addr.wrapping_add(160 + i * 16),
                                            v,
                                        );
                                    }
                                }
                                1 => {
                                    // FXRSTOR m512 — restore the x87 + SSE
                                    // state written by FXSAVE (CW/SW/TOP, the
                                    // eight ST registers, and XMM0-7). This is
                                    // the half that actually matters for the
                                    // save/restore bracketing above.
                                    let cw = self.mem_read_u16(mem, addr);
                                    let sw = self.mem_read_u16(mem, addr.wrapping_add(2));
                                    self.fpu_cw = cw;
                                    self.fpu_top = ((sw >> 11) & 7) as u8;
                                    self.fpu_sw = sw & !0x3800;
                                    for i in 0..8u32 {
                                        let foff = addr.wrapping_add(32 + i * 16);
                                        let lo = self.mem_read_u32(mem, foff) as u64;
                                        let hi =
                                            self.mem_read_u32(mem, foff.wrapping_add(4)) as u64;
                                        let se = self.mem_read_u16(mem, foff.wrapping_add(8));
                                        let v = F80::from_f80_parts(lo | (hi << 32), se);
                                        self.fpu_set_st(i as u8, v);
                                    }
                                    for i in 0..8u32 {
                                        let v = self
                                            .mem_read_u128(mem, addr.wrapping_add(160 + i * 16));
                                        self.xmm[i as usize] = v;
                                    }
                                }
                                2 | 3 => {
                                    // LDMXCSR / STMXCSR — load/store
                                    // the SSE control register. We
                                    // don't track it yet; load is a
                                    // no-op, store writes 0x1F80
                                    // (the architectural default).
                                    if sub == 3 {
                                        self.mem_write_u8(mem, addr, 0x80);
                                        self.mem_write_u8(mem, addr.wrapping_add(1), 0x1F);
                                        self.mem_write_u8(mem, addr.wrapping_add(2), 0);
                                        self.mem_write_u8(mem, addr.wrapping_add(3), 0);
                                    }
                                }
                                7 => {
                                    // CLFLUSH — flush cache line at
                                    // EA. No cache modelled, so just
                                    // consume the operand.
                                }
                                _ => {
                                    return Err(CpuError::Unimplemented {
                                        opcode: op2,
                                        cs: op_cs,
                                        ip: op_ip,
                                    });
                                }
                            }
                        }
                    }
                    // 0x0F 0x18 — PREFETCH hints (NTA/T0/T1/T2 per
                    // reg field). Also covers HINT_NOP forms on
                    // older CPUs. We just consume the ModR/M and
                    // do nothing.
                    0x18 => {
                        let _ = self.fetch_modrm(mem);
                    }
                    // 0x0F 0x1F — multi-byte NOP. Standard modern
                    // compiler NOP padding (`NOP DWORD PTR [rax]`
                    // and friends). Decode the ModR/M but otherwise
                    // do nothing.
                    0x1F => {
                        let _ = self.fetch_modrm(mem);
                    }
                    // CLTS — 0x0F 0x06. Clear Task-Switched flag in
                    // CR0 (bit 3). Used by FPU context-switch code.
                    // CPL=0 only — modifies a control register.
                    0x06 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        self.cr0 &= !(1 << 3);
                    }
                    // INVD — 0x0F 0x08. Invalidate internal caches
                    // without write-back. CPL=0 only — from ring 3
                    // raise #GP(0) per the Intel SDM. We don't model
                    // caches, so the privileged form is a no-op.
                    0x08 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                    }
                    // WBINVD — 0x0F 0x09. Same privilege rule as INVD.
                    0x09 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                    }
                    // RDTSC — 0x0F 0x31. Returns the time-stamp
                    // counter (low 32 bits in EAX, high 32 in EDX).
                    // We use the step counter — monotonically
                    // advancing, which is what calibration loops want.
                    0x31 => {
                        self.write_r32(0, self.tsc as u32);
                        self.write_r32(2, (self.tsc >> 32) as u32);
                    }
                    // RDPMC — 0x0F 0x33. Reads performance counter
                    // ECX into EDX:EAX. From CPL>0, real silicon
                    // gates on CR4.PCE (bit 8): bit clear → #GP(0).
                    // CPL=0 always succeeds. We don't model PMCs,
                    // so the success path returns zero — Linux's
                    // perf-event sampler sees "no events", which
                    // is correct for a single-threaded VM.
                    0x33 => {
                        if self.cr0 & 1 != 0
                            && (self.sregs[sreg::CS] & 3) != 0
                            && self.cr4 & (1 << 8) == 0
                        {
                            self.ip = op_ip;
                            self.do_interrupt_with_error(13, Some(0), mem);
                            return Ok(());
                        }
                        self.write_r32(0, 0);
                        self.write_r32(2, 0);
                    }
                    // RDMSR — 0x0F 0x32. Reads MSR named by ECX into
                    // EDX:EAX. CPL=0 only — userspace RDMSR is
                    // #GP(0). Unknown MSRs also raise #GP(0) (the
                    // shape `rdmsr_safe` catches); the kernel
                    // catches and treats as "MSR absent".
                    0x32 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let msr = self.read_r32(1); // ECX
                        let value: u64 = match msr {
                            0x10 => self.tsc,                  // IA32_TSC
                            0x1B => 0xFEE0_0000,               // IA32_APIC_BASE
                            0x174 => self.sysenter_cs as u64,  // IA32_SYSENTER_CS
                            0x175 => self.sysenter_esp as u64, // IA32_SYSENTER_ESP
                            0x176 => self.sysenter_eip as u64, // IA32_SYSENTER_EIP
                            // IA32_PLATFORM_ID (0x17): top byte of EDX
                            // encodes processor-family info; 0 is a
                            // benign read for a generic family-6 CPU.
                            0x17 => 0,
                            // IA32_BIOS_SIGN_ID (0x8B): microcode rev,
                            // read by Linux's microcode_intel_init.
                            // Reporting "no microcode loaded" (0) is
                            // what an un-patched CPU returns.
                            0x8B => 0,
                            // IA32_MISC_ENABLE (0x1A0): Linux's
                            // arch/x86/kernel/cpu/intel.c reads this on
                            // every boot. Stored value below; default
                            // 0 means "nothing special enabled".
                            0x1A0 => self.misc_enable,
                            // IA32_MTRR_DEF_TYPE (0x2FF): top byte holds
                            // E (enable) + FE (fixed-range enable). 0 =
                            // MTRRs disabled — Linux treats that as
                            // "fall back to PAT only", which is fine.
                            0x2FF => 0,
                            // IA32_TSC_AUX (0xC0000103). vDSO writes
                            // the CPU number here; the kernel reads it
                            // back via RDTSCP (or directly via this
                            // MSR) to identify the current CPU.
                            0xC000_0103 => self.tsc_aux as u64,
                            // IA32_FEATURE_CONTROL (0x3A). bit 0 = lock,
                            // bit 1 = enable-VMX-in-SMX, bit 2 = enable-
                            // VMX-outside-SMX. Reporting 1 = "locked,
                            // VMX disabled" tells Linux that the BIOS
                            // never unlocked VMX, so VT-x isn't usable.
                            0x3A => 1,
                            // IA32_MCG_CAP (0x179). bits 7:0 = number of
                            // MCE error-reporting banks. 0 = no banks,
                            // so Linux skips machine-check setup
                            // entirely.
                            0x179 => 0,
                            // IA32_EFER (0xC0000080). Linux probes for
                            // NXE / SCE bits. Returning whatever was
                            // last written lets `setup_efer`'s read-
                            // back verification pass without a #GP-
                            // recovery roundtrip.
                            0xC000_0080 => self.efer,
                            _ => {
                                self.ip = op_ip;
                                self.do_interrupt_with_error(13, Some(0), mem);
                                return Ok(());
                            }
                        };
                        self.write_r32(0, value as u32);
                        self.write_r32(2, (value >> 32) as u32);
                    }
                    // WRMSR — 0x0F 0x30. Write MSR named by ECX from
                    // EDX:EAX. CPL=0 only. Unknown MSRs raise #GP(0)
                    // — `wrmsr_safe` catches and treats as absent.
                    0x30 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        let msr = self.read_r32(1); // ECX
                        let lo = self.read_r32(0); // EAX
                        let hi = self.read_r32(2); // EDX
                        match msr {
                            // IA32_TSC: Linux writes it during early
                            // bring-up to sync TSC across CPUs (we're
                            // single-CPU so the write is mostly a
                            // calibration latch).
                            0x10 => self.tsc = (lo as u64) | ((hi as u64) << 32),
                            0x174 => self.sysenter_cs = lo,
                            0x175 => self.sysenter_esp = lo,
                            0x176 => self.sysenter_eip = lo,
                            // IA32_MISC_ENABLE — kernel toggles a few
                            // bits here (FAST_STRING, NHM_PEBS_DISABLE,
                            // BIOS_ENABLE etc.). Storing the value is
                            // enough; we don't act on any bit. The high
                            // dword carries XD-disable + reserved bits;
                            // capture it too so the kernel's read-back
                            // sees what it wrote.
                            0x1A0 => self.misc_enable = (lo as u64) | ((hi as u64) << 32),
                            // IA32_BIOS_SIGN_ID write means "trigger a
                            // microcode update read-back". We don't
                            // model microcode at all — silently accept.
                            0x8B => {}
                            // IA32_TSC_AUX: the kernel writes this
                            // per-CPU in cpu_init() so the vDSO can
                            // identify which CPU answered a syscall.
                            0xC000_0103 => self.tsc_aux = lo,
                            // IA32_EFER: store the full 64-bit value
                            // so `setup_efer`'s read-back sees what
                            // it just wrote. We don't act on NXE/SCE/
                            // LME bits — those would require modelling
                            // NX in the walker and 64-bit mode, both
                            // out of scope.
                            0xC000_0080 => self.efer = (lo as u64) | ((hi as u64) << 32),
                            _ => {
                                self.ip = op_ip;
                                self.do_interrupt_with_error(13, Some(0), mem);
                                return Ok(());
                            }
                        }
                    }
                    // SYSENTER — 0x0F 0x34. Fast ring-0 entry. Loads
                    // CS:EIP from IA32_SYSENTER_CS / _EIP and SS:ESP
                    // from CS+8 / _ESP. We don't model privilege rings,
                    // so this is a straight segment+pointer reload.
                    0x34 => {
                        let cs_sel = (self.sysenter_cs & 0xFFFC) as u16;
                        let ss_sel = cs_sel.wrapping_add(8);
                        self.write_sreg(sreg::CS, cs_sel, mem);
                        self.write_sreg(sreg::SS, ss_sel, mem);
                        self.ip = self.sysenter_eip;
                        let esp = self.sysenter_esp;
                        self.write_r32(r16::SP as u8, esp);
                    }
                    // SYSEXIT — 0x0F 0x35. Return to ring 3. CS from
                    // SYSENTER_CS+16, SS from +24; EIP=EDX, ESP=ECX.
                    // CPL=0 only — a userspace SYSEXIT would let an
                    // attacker swap into a kernel-supplied stack/CS
                    // at addresses the kernel doesn't expect.
                    0x35 => {
                        if self.raise_gp_if_user(op_ip, mem) {
                            return Ok(());
                        }
                        // Return to ring 3: CS = SYSENTER_CS+16, SS =
                        // SYSENTER_CS+24, BOTH with RPL forced to 3 (so
                        // CPL becomes 3). Without the RPL=3 the returned
                        // userspace ran at CPL=0, which mis-tags every
                        // subsequent user memory access as supervisor
                        // (wrong U/S bit in #PF error codes).
                        let base = (self.sysenter_cs & 0xFFFC) as u16;
                        let cs_sel = base.wrapping_add(16) | 3;
                        let ss_sel = base.wrapping_add(24) | 3;
                        self.write_sreg(sreg::CS, cs_sel, mem);
                        self.write_sreg(sreg::SS, ss_sel, mem);
                        self.ip = self.read_r32(2); // EDX
                        let esp = self.read_r32(1); // ECX
                        self.write_r32(r16::SP as u8, esp);
                    }
                    // Jcc rel16/rel32 — 0x0F 0x80..0x8F. Long-form
                    // conditional jump. Real-mode + no 0x66 = rel16;
                    // 0x66 prefix = rel32. Linux uses the 32-bit form
                    // pervasively because kernel functions span more
                    // than the rel8 ±128-byte reach of the 0x70..7F
                    // family.
                    0x80..=0x8F => {
                        let rel: i32 = if self.op_size_32 {
                            let lo = self.fetch_u16(mem) as u32;
                            let hi = self.fetch_u16(mem) as u32;
                            (lo | (hi << 16)) as i32
                        } else {
                            self.fetch_u16(mem) as i16 as i32
                        };
                        if self.eval_cond(op2 & 0x0F) {
                            self.ip = self.ip.wrapping_add(rel as u32);
                        }
                    }

                    // Bit-test family — 0x0F 0xA3 (BT), 0xAB (BTS),
                    // 0xB3 (BTR), 0xBB (BTC). Reads/modifies bit
                    // `r` within `r/m`. CF takes the old value of
                    // the tested bit.
                    //
                    // Memory-operand variant uses signed-arithmetic
                    // bit-position semantics per Intel SDM: the
                    // r-operand bit index addresses an arbitrary
                    // bit in the underlying bit-string, not just
                    // one of 16/32 bits within the named r/m. So
                    // `BTS [arr], r32` with r32=100 walks to
                    // byte arr+12, bit 4 within that dword — Linux
                    // uses this on per-cpu cap bitmaps where the
                    // feature index can be hundreds.
                    //
                    // Register-operand variant masks the bit index
                    // to the operand width (no addressing change),
                    // which is what the simplified path used to do
                    // for both.
                    0xA3 | 0xAB | 0xB3 | 0xBB => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let bit_idx = self.read_r32(reg) as i32;
                            let (v, write_back): (u32, Option<u32>) = match rm {
                                Rm::Reg(_) => {
                                    let v = self.read_rm32(rm, mem);
                                    let mask = 1u32 << (bit_idx & 31);
                                    self.set_flag(flag::CF, v & mask != 0);
                                    let new = match op2 {
                                        0xA3 => v,
                                        0xAB => v | mask,
                                        0xB3 => v & !mask,
                                        0xBB => v ^ mask,
                                        _ => unreachable!(),
                                    };
                                    (v, if op2 != 0xA3 { Some(new) } else { None })
                                }
                                Rm::Mem(ea) => {
                                    let base = self.linear_seg(ea.seg, ea.off);
                                    // Signed arithmetic shift: -1 / 32 = -1, etc.
                                    let dword_off = bit_idx >> 5;
                                    let addr = base.wrapping_add((dword_off as u32) * 4);
                                    let v = self.mem_read_u32(mem, addr);
                                    let mask = 1u32 << (bit_idx & 31);
                                    self.set_flag(flag::CF, v & mask != 0);
                                    let new = match op2 {
                                        0xA3 => v,
                                        0xAB => v | mask,
                                        0xB3 => v & !mask,
                                        0xBB => v ^ mask,
                                        _ => unreachable!(),
                                    };
                                    if op2 != 0xA3 {
                                        self.mem_write_u32(mem, addr, new);
                                    }
                                    (v, None)
                                }
                            };
                            // Already wrote in the Mem branch; this
                            // handles only Reg.
                            if let Some(new) = write_back {
                                self.write_rm32(rm, mem, new);
                            }
                            let _ = v;
                        } else {
                            let bit_idx = self.read_r16(reg) as i16;
                            match rm {
                                Rm::Reg(_) => {
                                    let v = self.read_rm16(rm, mem);
                                    let mask = 1u16 << ((bit_idx as u16) & 15);
                                    self.set_flag(flag::CF, v & mask != 0);
                                    let new = match op2 {
                                        0xA3 => v,
                                        0xAB => v | mask,
                                        0xB3 => v & !mask,
                                        0xBB => v ^ mask,
                                        _ => unreachable!(),
                                    };
                                    if op2 != 0xA3 {
                                        self.write_rm16(rm, mem, new);
                                    }
                                }
                                Rm::Mem(ea) => {
                                    let base = self.linear_seg(ea.seg, ea.off);
                                    let word_off = bit_idx >> 4;
                                    let addr = base.wrapping_add((word_off as u32) * 2);
                                    let v = self.mem_read_u16(mem, addr);
                                    let mask = 1u16 << ((bit_idx as u16) & 15);
                                    self.set_flag(flag::CF, v & mask != 0);
                                    let new = match op2 {
                                        0xA3 => v,
                                        0xAB => v | mask,
                                        0xB3 => v & !mask,
                                        0xBB => v ^ mask,
                                        _ => unreachable!(),
                                    };
                                    if op2 != 0xA3 {
                                        self.mem_write_u16(mem, addr, new);
                                    }
                                }
                            }
                        }
                    }

                    // BT/BTS/BTR/BTC r/m, imm8 — 0x0F 0xBA /reg.
                    //   reg=4 BT, 5 BTS, 6 BTR, 7 BTC.
                    0xBA => {
                        let (_, sub, rm) = self.fetch_modrm(mem);
                        let imm = self.fetch_u8(mem);
                        if !matches!(sub, 4..=7) {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            let bit = (imm & 31) as u32;
                            let mask = 1u32 << bit;
                            self.set_flag(flag::CF, v & mask != 0);
                            let new = match sub {
                                4 => v,
                                5 => v | mask,
                                6 => v & !mask,
                                7 => v ^ mask,
                                _ => unreachable!(),
                            };
                            if sub != 4 {
                                self.write_rm32(rm, mem, new);
                            }
                        } else {
                            let v = self.read_rm16(rm, mem);
                            let bit = (imm & 15) as u16;
                            let mask = 1u16 << bit;
                            self.set_flag(flag::CF, v & mask != 0);
                            let new = match sub {
                                4 => v,
                                5 => v | mask,
                                6 => v & !mask,
                                7 => v ^ mask,
                                _ => unreachable!(),
                            };
                            if sub != 4 {
                                self.write_rm16(rm, mem, new);
                            }
                        }
                    }

                    // SSE MOVAPS/MOVUPS xmm, xmm/m128 (0F 28/10) and
                    // the store direction (0F 29/11). Aligned vs
                    // unaligned is the same here — we don't enforce
                    // alignment faults. Under 0x66 these become
                    // MOVAPD/MOVUPD (same bits, double semantics —
                    // irrelevant for a pure 128-bit copy).
                    0x28 | 0x10 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = v;
                    }
                    0x29 | 0x11 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.xmm[reg as usize];
                        self.write_xmm_rm(rm, mem, v);
                    }
                    // [U]COMISS / [U]COMISD (0F 2E ordered-quiet, 0F 2F
                    // signaling — identical for flag purposes here).
                    // Compare the low scalar lane and set ZF/PF/CF per
                    // the x86 result encoding; OF/SF are cleared. The
                    // 0x66 prefix selects the double-precision form.
                    // Compilers emit these for `if (x < y)` on floats.
                    0x2E | 0x2F => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let ord = if self.has_66() {
                            let a = f64::from_bits(self.xmm[reg as usize] as u64);
                            let b = f64::from_bits(self.read_xmm_rm64(rm, mem));
                            a.partial_cmp(&b)
                        } else {
                            let a = f32::from_bits(self.xmm[reg as usize] as u32);
                            let b = f32::from_bits(self.read_xmm_rm32(rm, mem));
                            a.partial_cmp(&b)
                        };
                        let (zf, pf, cf) = comis_flags(ord);
                        self.set_flag(flag::ZF, zf);
                        self.set_flag(flag::PF, pf);
                        self.set_flag(flag::CF, cf);
                        self.set_flag(flag::OF, false);
                        self.set_flag(flag::SF, false);
                    }
                    // MOVD/MOVQ and MOVDQA. The 0x66 prefix (op_size_32)
                    // selects the integer-SSE forms:
                    //   66 0F 6E  MOVD xmm, r/m32   (GP/mem → low dword)
                    //   66 0F 7E  MOVD r/m32, xmm   (low dword → GP/mem)
                    //   66 0F 6F  MOVDQA xmm, xmm/m128
                    //   66 0F 7F  MOVDQA xmm/m128, xmm
                    0x6E if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm32(rm, mem) as u128;
                        self.xmm[reg as usize] = v; // zero-extends upper 96 bits
                    }
                    0x7E if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.xmm[reg as usize] as u32;
                        self.write_rm32(rm, mem, v);
                    }
                    0x6F if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = v;
                    }
                    0x7F if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.xmm[reg as usize];
                        self.write_xmm_rm(rm, mem, v);
                    }

                    // --- MMX (no 0x66 prefix): the 64-bit packed-integer
                    // forms, on the separate MM register file. Guest
                    // libcrypto/zlib use these (e.g. `movd mm0,[esp+x]` then
                    // `pxor mm1,mm1`). The 66-prefixed twins above are the
                    // 128-bit SSE2 versions; the `!has_66` guard keeps the two
                    // families apart.
                    0x6E if !self.has_66() => {
                        // MOVD mm, r/m32 — load low dword, zero the high.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        self.mmx[reg as usize] = self.read_rm32(rm, mem) as u64;
                    }
                    0x7E if !self.has_66() => {
                        // MOVD r/m32, mm — store the low dword.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.mmx[reg as usize] as u32;
                        self.write_rm32(rm, mem, v);
                    }
                    0x6F if !self.has_66() => {
                        // MOVQ mm, mm/m64.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        self.mmx[reg as usize] = self.read_mm_rm(rm, mem);
                    }
                    0x7F if !self.has_66() => {
                        // MOVQ mm/m64, mm.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.mmx[reg as usize];
                        self.write_mm_rm(rm, mem, v);
                    }
                    0x77 => {
                        // EMMS — end MMX state. The MM file is separate from
                        // our x87 stack, so there's nothing to retag.
                    }
                    0x70 if !self.has_66() => {
                        // PSHUFW mm, mm/m64, imm8 — word shuffle (the no-66
                        // form; 66/F2/F3 are the SSE PSHUFD/LW/HW handled
                        // elsewhere). Used by openssl's MMX SHA path.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_mm_rm(rm, mem);
                        let imm = self.fetch_u8(mem);
                        self.mmx[reg as usize] = mmx_pshufw(src, imm);
                    }
                    0xC4 if !self.has_66() => {
                        // PINSRW mm, r32/m16, imm8 — insert a 16-bit value
                        // into word slot (imm8 & 3) of the MM register.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let w = match rm {
                            Rm::Reg(i) => self.read_r16(i),
                            Rm::Mem(_) => self.read_rm16(rm, mem),
                        } as u64;
                        let slot = (self.fetch_u8(mem) & 3) as u64 * 16;
                        let d = &mut self.mmx[reg as usize];
                        *d = (*d & !(0xFFFFu64 << slot)) | (w << slot);
                    }
                    0xC5 if !self.has_66() => {
                        // PEXTRW r32, mm, imm8 — extract word (imm8 & 3) from
                        // the MM register, zero-extended into the GPR.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_mm_rm(rm, mem);
                        let slot = (self.fetch_u8(mem) & 3) as u64 * 16;
                        self.write_r32(reg, ((src >> slot) & 0xFFFF) as u32);
                    }
                    // SSE2 twins (66 prefix) of PINSRW/PEXTRW — the XMM forms
                    // have 8 word slots (imm8 & 7). numpy's import hit the
                    // PEXTRW xmm form ("unimplemented opcode 0xC5").
                    0xC4 if self.has_66() => {
                        // PINSRW xmm, r32/m16, imm8.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let w = match rm {
                            Rm::Reg(i) => self.read_r16(i) as u128,
                            Rm::Mem(_) => self.read_rm16(rm, mem) as u128,
                        };
                        let slot = (self.fetch_u8(mem) & 7) as u32 * 16;
                        let d = &mut self.xmm[reg as usize];
                        *d = (*d & !(0xFFFFu128 << slot)) | (w << slot);
                    }
                    0xC5 if self.has_66() => {
                        // PEXTRW r32, xmm, imm8.
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        let slot = (self.fetch_u8(mem) & 7) as u32 * 16;
                        self.write_r32(reg, ((src >> slot) & 0xFFFF) as u32);
                    }
                    // MMX PUNPCKL/H {BW,WD,DQ} (60/61/62 low, 68/69/6A high)
                    // and PACK {SSWB,USWB,SSDW} (63/67/6B). The no-66 MMX
                    // forms; the 66 SSE2 twins are handled below.
                    0x60 | 0x61 | 0x62 | 0x68 | 0x69 | 0x6A | 0x63 | 0x67 | 0x6B
                        if !self.has_66() =>
                    {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem);
                        let d = self.mmx[reg as usize];
                        self.mmx[reg as usize] = match op2 {
                            0x60 => mmx_punpck(d, s, 1, false),
                            0x61 => mmx_punpck(d, s, 2, false),
                            0x62 => mmx_punpck(d, s, 4, false),
                            0x68 => mmx_punpck(d, s, 1, true),
                            0x69 => mmx_punpck(d, s, 2, true),
                            0x6A => mmx_punpck(d, s, 4, true),
                            0x63 => mmx_packwb(d, s, true), // PACKSSWB
                            0x67 => mmx_packwb(d, s, false), // PACKUSWB
                            _ => mmx_packssdw(d, s),        // 0x6B PACKSSDW
                        };
                    }
                    // Bitwise logicals on the full 64 bits.
                    0xDB | 0xDF | 0xEB | 0xEF if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem);
                        let d = &mut self.mmx[reg as usize];
                        *d = match op2 {
                            0xDB => *d & s,  // PAND
                            0xDF => !*d & s, // PANDN
                            0xEB => *d | s,  // POR
                            _ => *d ^ s,     // PXOR (0xEF)
                        };
                    }
                    // Packed add / subtract (wrapping): b/w/d/q lanes.
                    0xFC | 0xFD | 0xFE | 0xD4 | 0xF8 | 0xF9 | 0xFA | 0xFB if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem);
                        let d = self.mmx[reg as usize];
                        self.mmx[reg as usize] = match op2 {
                            0xFC => mmx_padd(d, s, 1),
                            0xFD => mmx_padd(d, s, 2),
                            0xFE => mmx_padd(d, s, 4),
                            0xD4 => mmx_padd(d, s, 8),
                            0xF8 => mmx_psub(d, s, 1),
                            0xF9 => mmx_psub(d, s, 2),
                            0xFA => mmx_psub(d, s, 4),
                            _ => mmx_psub(d, s, 8), // 0xFB PSUBQ
                        };
                    }
                    // PMULUDQ mm, mm/m64 — unsigned 32×32→64 of the low
                    // dwords. The core of OpenSSL's MMX bignum multiply
                    // (PMULUDQ + PADDQ), used by libcrypto's RSA.
                    0xF4 if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem) as u32 as u64;
                        let d = self.mmx[reg as usize] as u32 as u64;
                        self.mmx[reg as usize] = d * s;
                    }
                    // Packed 16-bit multiplies: PMULLW (low), PMULHW (signed
                    // high), PMADDWD (multiply + add adjacent pairs → dwords).
                    0xD5 | 0xE5 | 0xF5 if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem);
                        let d = self.mmx[reg as usize];
                        self.mmx[reg as usize] = match op2 {
                            0xD5 => {
                                packed_map(d, s, 2, |x, y| (x as u16).wrapping_mul(y as u16) as u64)
                            }
                            0xE5 => packed_map(d, s, 2, |x, y| {
                                let p = (x as i16 as i32) * (y as i16 as i32);
                                ((p >> 16) & 0xFFFF) as u64
                            }),
                            _ => mmx_pmaddwd(d, s), // 0xF5
                        };
                    }
                    // Packed shift by imm8: 0F 71/72/73 group, where the
                    // ModRM reg field is the sub-op (2=SRL, 4=SRA, 6=SLL) and
                    // the rm field is the MM register shifted in place.
                    0x71..=0x73 if !self.has_66() => {
                        let (_, subop, rm) = self.fetch_modrm(mem);
                        let imm = self.fetch_u8(mem) as u64;
                        if let Rm::Reg(i) = rm {
                            let lane = match op2 {
                                0x71 => 2,
                                0x72 => 4,
                                _ => 8,
                            };
                            let d = self.mmx[i as usize];
                            self.mmx[i as usize] = match subop {
                                2 => mmx_psrl(d, imm, lane),
                                6 => mmx_psll(d, imm, lane),
                                4 => mmx_psra(d, imm, lane), // not encodable for qword
                                _ => d,
                            };
                        }
                    }
                    // Packed shift by mm/m64 count: PSRL (D1/D2/D3), PSRA
                    // (E1/E2), PSLL (F1/F2/F3) — word/dword/qword lanes.
                    0xD1 | 0xD2 | 0xD3 | 0xE1 | 0xE2 | 0xF1 | 0xF2 | 0xF3 if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let count = self.read_mm_rm(rm, mem);
                        let d = self.mmx[reg as usize];
                        self.mmx[reg as usize] = match op2 {
                            0xD1 => mmx_psrl(d, count, 2),
                            0xD2 => mmx_psrl(d, count, 4),
                            0xD3 => mmx_psrl(d, count, 8),
                            0xE1 => mmx_psra(d, count, 2),
                            0xE2 => mmx_psra(d, count, 4),
                            0xF1 => mmx_psll(d, count, 2),
                            0xF2 => mmx_psll(d, count, 4),
                            _ => mmx_psll(d, count, 8), // 0xF3 PSLLQ
                        };
                    }
                    // Packed compare: equal / signed-greater, b/w/d lanes.
                    0x74 | 0x75 | 0x76 | 0x64 | 0x65 | 0x66 if !self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_mm_rm(rm, mem);
                        let d = self.mmx[reg as usize];
                        self.mmx[reg as usize] = match op2 {
                            0x74 => mmx_pcmpeq(d, s, 1),
                            0x75 => mmx_pcmpeq(d, s, 2),
                            0x76 => mmx_pcmpeq(d, s, 4),
                            0x64 => mmx_pcmpgt(d, s, 1),
                            0x65 => mmx_pcmpgt(d, s, 2),
                            _ => mmx_pcmpgt(d, s, 4), // 0x66 PCMPGTD
                        };
                    }

                    // PUNPCKL/H {BW,WD,DQ,QDQ} (66 0F 60/61/62/6C and
                    // 68/69/6A/6D) — interleave the low / high elements of
                    // the destination (reg) and source (rm). SSE2 xmm form;
                    // emitted by musl/openssl SIMD (e.g. `punpckldq xmm,xmm`
                    // to broadcast/duplicate dwords). We model the xmm forms;
                    // the MMX (no-66) forms use the separate mm stack.
                    0x60 | 0x61 | 0x62 | 0x6C | 0x68 | 0x69 | 0x6A | 0x6D if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        let dst = self.xmm[reg as usize];
                        let (elem, high) = match op2 {
                            0x60 => (8, false),
                            0x61 => (16, false),
                            0x62 => (32, false),
                            0x6C => (64, false),
                            0x68 => (8, true),
                            0x69 => (16, true),
                            0x6A => (32, true),
                            _ => (64, true), // 0x6D PUNPCKHQDQ
                        };
                        self.xmm[reg as usize] = punpck(dst, src, elem, high);
                    }
                    // Packed-integer logicals (66 0F): PAND/PANDN/POR/PXOR.
                    // Lane-independent, so they're plain 128-bit bitops.
                    // `pxor xmm, xmm` is the canonical XMM-zeroing idiom.
                    0xDB if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] &= v;
                    }
                    // PANDN xmm: dest = (NOT dest) AND src. CPython's SSE2
                    // code emits it (it was the one logical op missing here,
                    // surfacing as "unimplemented opcode 0xDF").
                    0xDF if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        let d = self.xmm[reg as usize];
                        self.xmm[reg as usize] = !d & v;
                    }
                    0xEB if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] |= v;
                    }
                    0xEF if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] ^= v;
                    }
                    // Packed-integer add/sub with per-lane wrap. The
                    // opcode's low bits pick the lane width:
                    //   FC PADDB (8)  FD PADDW (16)  FE PADDD (32)
                    //   D4 PADDQ (64)
                    //   F8 PSUBB      F9 PSUBW       FA PSUBD
                    //   FB PSUBQ
                    0xFC | 0xFD | 0xFE | 0xD4 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lane = match op2 {
                            0xFC => 8,
                            0xFD => 16,
                            0xFE => 32,
                            _ => 64,
                        };
                        self.xmm[reg as usize] = packed_add(a, b, lane);
                    }
                    0xF8..=0xFB if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lane = match op2 {
                            0xF8 => 8,
                            0xF9 => 16,
                            0xFA => 32,
                            _ => 64,
                        };
                        self.xmm[reg as usize] = packed_sub(a, b, lane);
                    }
                    // PCMPEQB/W/D (66 0F 74/75/76) — per-lane equality;
                    // result lane is all-ones on equal, all-zeros
                    // otherwise. The classic SIMD primitive for
                    // strchr / memcmp / vectorized branching.
                    0x74..=0x76 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lane = match op2 {
                            0x74 => 8,
                            0x75 => 16,
                            _ => 32,
                        };
                        self.xmm[reg as usize] = pcmpeq(a, b, lane);
                    }
                    // PCMPGTB/W/D (66 0F 64/65/66) — signed greater-than;
                    // result lane all-ones if (signed) a > b. Pairs
                    // with PCMPEQ to derive {<, ≤, ≥, ≠} via masks.
                    0x64..=0x66 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lane = match op2 {
                            0x64 => 8,
                            0x65 => 16,
                            _ => 32,
                        };
                        self.xmm[reg as usize] = pcmpgt(a, b, lane);
                    }
                    // Group 12/13/14 immediate-count shifts:
                    //   66 0F 71 /2,4,6 ib  PSRLW / PSRAW / PSLLW  (16)
                    //   66 0F 72 /2,4,6 ib  PSRLD / PSRAD / PSLLD  (32)
                    //   66 0F 73 /2,6   ib  PSRLQ /         PSLLQ  (64)
                    //   66 0F 73 /3,7   ib  PSRLDQ / PSLLDQ        (byte
                    //                                  shifts of whole 128)
                    // The ModR/M reg field is the opcode extension;
                    // rm.is_reg() encodes the target XMM (no memory).
                    // Compilers emit `psrad xmm, 31` constantly for
                    // sign-extension across a vector.
                    0x71..=0x73 if self.has_66() => {
                        let (_, ext, rm) = self.fetch_modrm(mem);
                        let count = self.fetch_u8(mem) as u32;
                        let target = match rm {
                            Rm::Reg(i) => i as usize,
                            Rm::Mem(_) => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        let v = self.xmm[target];
                        let lane = match op2 {
                            0x71 => 16,
                            0x72 => 32,
                            _ => 64,
                        };
                        let r = match (op2, ext) {
                            (_, 2) => packed_shift_logical_right(v, lane, count),
                            (0x71, 4) | (0x72, 4) => packed_shift_arithmetic_right(v, lane, count),
                            (_, 6) => packed_shift_left(v, lane, count),
                            (0x73, 3) => byte_shift_right_128(v, count),
                            (0x73, 7) => byte_shift_left_128(v, count),
                            _ => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        self.xmm[target] = r;
                    }
                    // Variable-count packed-int shifts (66 0F):
                    //   D1/D2/D3  PSRLW / PSRLD / PSRLQ
                    //   E1/E2     PSRAW / PSRAD            (no PSRAQ)
                    //   F1/F2/F3  PSLLW / PSLLD / PSLLQ
                    // The count is the *full* low qword of the source
                    // operand — but values larger than the lane width
                    // are handled identically to the imm form (logical
                    // shifts clear, arithmetic clamps to width-1), so
                    // we just cap into u32 and reuse the helpers.
                    0xD1..=0xD3 | 0xE1 | 0xE2 | 0xF1..=0xF3 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        let count_full = src as u64;
                        let count = if count_full > u32::MAX as u64 {
                            u32::MAX
                        } else {
                            count_full as u32
                        };
                        let lane = match op2 {
                            0xD1 | 0xE1 | 0xF1 => 16,
                            0xD2 | 0xE2 | 0xF2 => 32,
                            _ => 64,
                        };
                        let v = self.xmm[reg as usize];
                        let r = match op2 {
                            0xD1..=0xD3 => packed_shift_logical_right(v, lane, count),
                            0xE1 | 0xE2 => packed_shift_arithmetic_right(v, lane, count),
                            _ => packed_shift_left(v, lane, count),
                        };
                        self.xmm[reg as usize] = r;
                    }
                    // Packed-int multiplies (66 0F):
                    //   D5  PMULLW   — low 16 of each 16×16 product
                    //   E5  PMULHW   — high 16 of signed 16×16 product
                    //   F5  PMADDWD  — signed 16×16 + pair-sum → dword
                    0xD5 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = pmullw(a, b);
                    }
                    0xE5 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = pmulhw(a, b);
                    }
                    0xF5 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = pmaddwd(a, b);
                    }
                    // Packed saturation arithmetic (66 0F):
                    //   D8/D9  PSUBUSB / PSUBUSW   (unsigned saturate)
                    //   DC/DD  PADDUSB / PADDUSW
                    //   E8/E9  PSUBSB  / PSUBSW    (signed saturate)
                    //   EC/ED  PADDSB  / PADDSW
                    // Clamps instead of wraps — what audio mixers and
                    // colour blends actually want.
                    0xD8 | 0xD9 | 0xDC | 0xDD | 0xE8 | 0xE9 | 0xEC | 0xED if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = match op2 {
                            0xD8 => packed_sat_sub_unsigned(a, b, 8),
                            0xD9 => packed_sat_sub_unsigned(a, b, 16),
                            0xDC => packed_sat_add_unsigned(a, b, 8),
                            0xDD => packed_sat_add_unsigned(a, b, 16),
                            0xE8 => packed_sat_sub_signed(a, b, 8),
                            0xE9 => packed_sat_sub_signed(a, b, 16),
                            0xEC => packed_sat_add_signed(a, b, 8),
                            _ => packed_sat_add_signed(a, b, 16), // 0xED
                        };
                    }
                    // Packs (66 0F):
                    //   63  PACKSSWB  (words → bytes, signed-saturate)
                    //   67  PACKUSWB  (words → bytes, unsigned-saturate)
                    //   6B  PACKSSDW  (dwords → words, signed-saturate)
                    // The destination's lanes occupy the low half of
                    // the result; the source's lanes the high half.
                    0x63 | 0x67 | 0x6B if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = match op2 {
                            0x63 => packsswb(a, b),
                            0x67 => packuswb(a, b),
                            _ => packssdw(a, b),
                        };
                    }
                    // Packed unsigned-byte / signed-word min/max
                    // (66 0F): DA PMINUB, DE PMAXUB, EA PMINSW, EE PMAXSW.
                    0xDA | 0xDE | 0xEA | 0xEE if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let take_max = matches!(op2, 0xDE | 0xEE);
                        let signed_word = matches!(op2, 0xEA | 0xEE);
                        self.xmm[reg as usize] = if signed_word {
                            packed_lanes(a, b, 16, |x, y, _| {
                                let sx = sign_extend_lane(x, 16);
                                let sy = sign_extend_lane(y, 16);
                                if (take_max && sx > sy) || (!take_max && sx < sy) {
                                    x
                                } else {
                                    y
                                }
                            })
                        } else {
                            packed_lanes(a, b, 8, |x, y, _| {
                                if (take_max && x > y) || (!take_max && x < y) {
                                    x
                                } else {
                                    y
                                }
                            })
                        };
                    }
                    // PAVGB (E0) / PAVGW (E3) — packed rounded average:
                    // (a + b + 1) / 2 per lane (byte / word). Common in
                    // pixel blends and motion-compensation filters.
                    0xE0 | 0xE3 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lane = if op2 == 0xE0 { 8 } else { 16 };
                        self.xmm[reg as usize] =
                            packed_lanes(a, b, lane, |x, y, _| (x + y + 1) >> 1);
                    }
                    // PMULHUW (E4) — unsigned 16×16 multiply, keep high
                    // 16 of each product. Counterpart to PMULHW for
                    // unsigned data (no sign extension before multiply).
                    0xE4 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] =
                            packed_lanes(a, b, 16, |x, y, mask| ((x * y) >> 16) & mask);
                    }
                    // PMULUDQ (F4) — unsigned 32×32 → 64 multiply on the
                    // low dword of each 64-bit lane. Used for 64-bit
                    // multi-precision arithmetic on top of SSE2.
                    0xF4 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let lo = ((a as u32) as u64) * ((b as u32) as u64);
                        let hi = (((a >> 64) as u32) as u64) * (((b >> 64) as u32) as u64);
                        self.xmm[reg as usize] = (lo as u128) | ((hi as u128) << 64);
                    }
                    // PSADBW (F6) — sum of absolute differences across
                    // each 8-byte half; the result is a 16-bit value
                    // in the low word of each 64-bit lane (upper bits
                    // zero). Vectorized memcmp / decoder cost metric.
                    0xF6 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let mut low: u128 = 0;
                        let mut high: u128 = 0;
                        for i in 0..8u32 {
                            let ax = (a >> (i * 8)) & 0xFF;
                            let bx = (b >> (i * 8)) & 0xFF;
                            low += ax.abs_diff(bx);
                            let ay = (a >> (64 + i * 8)) & 0xFF;
                            let by = (b >> (64 + i * 8)) & 0xFF;
                            high += ay.abs_diff(by);
                        }
                        self.xmm[reg as usize] = low | (high << 64);
                    }
                    // MOVQ xmm/m64, xmm1 (66 0F D6) — store low qword.
                    // Reg-form zeroes the destination's upper 64.
                    0xD6 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.xmm[reg as usize] as u64;
                        match rm {
                            Rm::Reg(i) => self.xmm[i as usize] = v as u128,
                            Rm::Mem(ea) => {
                                let a = self.linear_seg(ea.seg, ea.off);
                                self.mem_write_u32(mem, a, v as u32);
                                self.mem_write_u32(mem, a.wrapping_add(4), (v >> 32) as u32);
                            }
                        }
                    }
                    // PMOVMSKB (66 0F D7) r32, xmm — extract the high
                    // bit of each of the 16 bytes into the low 16 bits
                    // of a GP register; upper 16 cleared. The canonical
                    // SIMD-to-branch primitive: pair with PCMPEQB to
                    // hunt for a byte in a 16-byte chunk.
                    0xD7 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = match rm {
                            Rm::Reg(i) => self.xmm[i as usize],
                            Rm::Mem(_) => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        let mut mask: u32 = 0;
                        for i in 0..16u32 {
                            if ((src >> (i * 8 + 7)) & 1) != 0 {
                                mask |= 1 << i;
                            }
                        }
                        self.write_r32(reg, mask);
                    }
                    // Non-temporal stores. These are cache-bypass
                    // hints — semantically identical to regular stores
                    // for us since we don't model a cache. All forms
                    // require a memory destination (reg form is UD).
                    //   0F 2B  MOVNTPS m128, xmm   (66 → MOVNTPD; same bits)
                    //   66 0F E7  MOVNTDQ m128, xmm
                    //   0F C3  MOVNTI m32, r32     (note: source is GP)
                    0x2B => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if let Rm::Mem(ea) = rm {
                            let a = self.linear_seg(ea.seg, ea.off);
                            self.mem_write_u128(mem, a, self.xmm[reg as usize]);
                        } else {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                    0xE7 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if let Rm::Mem(ea) = rm {
                            let a = self.linear_seg(ea.seg, ea.off);
                            self.mem_write_u128(mem, a, self.xmm[reg as usize]);
                        } else {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                    0xC3 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if let Rm::Mem(ea) = rm {
                            let a = self.linear_seg(ea.seg, ea.off);
                            self.mem_write_u32(mem, a, self.read_r32(reg));
                        } else {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                    // MASKMOVDQU xmm1, xmm2 (66 0F F7) — for each of 16
                    // bytes in xmm1, if the matching byte in xmm2 has
                    // its high bit set, store the byte to DS:[(E)DI+i].
                    // Reg form only; the destination address is
                    // implicit (no ModR/M.rm encoded for the dest).
                    0xF7 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let mask_idx = match rm {
                            Rm::Reg(i) => i as usize,
                            Rm::Mem(_) => {
                                return Err(CpuError::Unimplemented {
                                    opcode: op2,
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                        };
                        let data = self.xmm[reg as usize];
                        let mask = self.xmm[mask_idx];
                        let di = if self.addr_size_32 {
                            self.read_r32(r16::DI as u8)
                        } else {
                            self.regs[r16::DI] as u32
                        };
                        let base = self.linear_seg(sreg::DS, di);
                        for i in 0..16u32 {
                            let mb = (mask >> (i * 8 + 7)) & 1;
                            if mb != 0 {
                                let byte = ((data >> (i * 8)) & 0xFF) as u8;
                                self.mem_write_u8(mem, base.wrapping_add(i), byte);
                            }
                        }
                    }
                    // Packed precision converts (0F 5A):
                    //   no prefix → CVTPS2PD (2×f32 → 2×f64)
                    //   0x66      → CVTPD2PS (2×f64 → 2×f32, low half;
                    //                          upper 64 of dest is zero)
                    // Scalar (F3/F2) variants go through sse_scalar.
                    0x5A => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        self.xmm[reg as usize] = if self.has_66() {
                            let src = self.read_xmm_rm(rm, mem);
                            let lo = f64::from_bits(src as u64) as f32;
                            let hi = f64::from_bits((src >> 64) as u64) as f32;
                            (lo.to_bits() as u128) | ((hi.to_bits() as u128) << 32)
                        } else {
                            let src = self.read_xmm_rm64(rm, mem);
                            let lo = f32::from_bits(src as u32) as f64;
                            let hi = f32::from_bits((src >> 32) as u32) as f64;
                            (lo.to_bits() as u128) | ((hi.to_bits() as u128) << 64)
                        };
                    }
                    // Packed int↔single converts (0F 5B):
                    //   no prefix → CVTDQ2PS  (4×i32 → 4×f32)
                    //   0x66      → CVTPS2DQ  (4×f32 → 4×i32, round)
                    //   F3 → CVTTPS2DQ (truncate) — via sse_scalar
                    0x5B => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = if self.has_66() {
                            packed_lanes(src, 0, 32, |x, _, _| {
                                let f = f32::from_bits(x as u32);
                                (f.round_ties_even() as i32) as u32 as u128
                            })
                        } else {
                            packed_lanes(src, 0, 32, |x, _, _| {
                                let f = (x as u32 as i32) as f32;
                                f.to_bits() as u128
                            })
                        };
                    }
                    // CVTTPD2DQ (66 0F E6) — 2×f64 → 2×i32 (truncate)
                    // in the low 64 bits; upper 64 of dest is zero.
                    // The F2/F3 variants (CVTPD2DQ round / CVTDQ2PD)
                    // route through sse_scalar.
                    0xE6 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        let lo = f64::from_bits(src as u64).trunc() as i32 as u32 as u128;
                        let hi = f64::from_bits((src >> 64) as u64).trunc() as i32 as u32 as u128;
                        self.xmm[reg as usize] = lo | (hi << 32);
                    }
                    // MOVMSKPS (0F 50) / MOVMSKPD (66 0F 50): gather the
                    // per-lane sign bits of an XMM into the low bits of a GP
                    // register (PS → 4 bits from the f32 lanes' bit31; PD → 2
                    // bits from the f64 lanes' bit63), zero-extended. The
                    // canonical way to test a packed-compare result
                    // (CMPPD → MOVMSKPD → branch), so SSE-heavy code (numpy/
                    // BLAS/libm) leans on it. Source is a register only.
                    0x50 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_xmm_rm(rm, mem);
                        let mask = if self.has_66() {
                            (((v >> 63) & 1) as u32) | ((((v >> 127) & 1) as u32) << 1)
                        } else {
                            let mut m = 0u32;
                            for lane in 0..4 {
                                m |= (((v >> (lane * 32 + 31)) & 1) as u32) << lane;
                            }
                            m
                        };
                        self.write_r32(reg, mask);
                    }
                    // SSE3 horizontal add/sub + add-sub-alternating, PD forms
                    // (66 prefix). PS forms are F2-prefixed → sse_scalar.
                    // HADDPD (66 0F 7C) / HSUBPD (66 0F 7D) reduce f64 pairs
                    // across the register (dot-product/reduction kernels);
                    // ADDSUBPD (66 0F D0) is the complex-multiply add/sub.
                    // Alpine binaries emit SSE3 despite CPUID not advertising
                    // it (cf. the FISTTP find).
                    0x7C if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = hadd_f64(self.xmm[reg as usize], s, false);
                    }
                    0x7D if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = hadd_f64(self.xmm[reg as usize], s, true);
                    }
                    0xD0 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = addsub_f64(self.xmm[reg as usize], s);
                    }
                    // CMPPS (0F C2) / CMPPD (66 0F C2): per-lane float compare
                    // with an imm8 predicate (0 EQ,1 LT,2 LE,3 UNORD,4 NEQ,
                    // 5 NLT,6 NLE,7 ORD), producing an all-ones/all-zeros mask
                    // per lane. OpenBLAS / numpy emit these; the F2/F3-prefixed
                    // scalar CMPSD/CMPSS route through sse_scalar.
                    0xC2 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let imm = self.fetch_u8(mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = if self.has_66() {
                            packed_cmp_f64(a, b, imm)
                        } else {
                            packed_cmp_f32(a, b, imm)
                        };
                    }
                    // Packed float arithmetic. The 0x66 prefix
                    // (op_size_32) selects double-precision (PD, 2×f64)
                    // over single (PS, 4×f32); the opcode low bits pick
                    // the operation:
                    //   58 ADD   59 MUL   5C SUB   5E DIV
                    0x58 | 0x59 | 0x5C | 0x5E => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = if self.has_66() {
                            match op2 {
                                0x58 => packed_f64(a, b, |x, y| x + y),
                                0x59 => packed_f64(a, b, |x, y| x * y),
                                0x5C => packed_f64(a, b, |x, y| x - y),
                                _ => packed_f64(a, b, |x, y| x / y),
                            }
                        } else {
                            match op2 {
                                0x58 => packed_f32(a, b, |x, y| x + y),
                                0x59 => packed_f32(a, b, |x, y| x * y),
                                0x5C => packed_f32(a, b, |x, y| x - y),
                                _ => packed_f32(a, b, |x, y| x / y),
                            }
                        };
                    }
                    // Packed MIN/MAX (0F 5D / 0F 5F) — per-lane, with
                    // x86's exact tie/NaN rule: MIN returns the second
                    // operand unless the first is strictly less (so a
                    // NaN or equal lane yields the source). 0x66 → PD.
                    0x5D | 0x5F => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        let is_min = op2 == 0x5D;
                        self.xmm[reg as usize] = if self.has_66() {
                            packed_f64(a, b, |x, y| fmin_max(x, y, is_min))
                        } else {
                            packed_f32(a, b, |x, y| fmin_max_f32(x, y, is_min))
                        };
                    }
                    // Packed SQRT (0F 51) — unary; each lane of the
                    // source is square-rooted into the destination.
                    // 0x66 → SQRTPD.
                    0x51 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let s = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = if self.has_66() {
                            packed_f64(s, s, |x, _| x.sqrt())
                        } else {
                            packed_f32(s, s, |x, _| x.sqrt())
                        };
                    }
                    // Packed bitwise logicals (0F 54-57): ANDPS/ANDNPS/
                    // ORPS/XORPS. Lane-independent, so plain 128-bit
                    // ops; the 0x66 (PD) forms use identical bits.
                    // `xorps xmm,xmm` zeroes a register; ANDPS/ANDNPS
                    // build the fabs / copysign sign-bit masks.
                    0x54..=0x57 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let b = self.read_xmm_rm(rm, mem);
                        let a = self.xmm[reg as usize];
                        self.xmm[reg as usize] = match op2 {
                            0x54 => a & b,
                            0x55 => !a & b, // ANDNPS: (NOT dest) AND src
                            0x56 => a | b,
                            _ => a ^ b,
                        };
                    }
                    // UNPCKL/UNPCKH (0F 14/15) — interleave lanes from
                    // the destination (SRC1) and r/m source (SRC2).
                    // 0x66 selects 64-bit lanes (PD), else 32-bit (PS).
                    0x14 | 0x15 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let src2 = self.read_xmm_rm(rm, mem);
                        let src1 = self.xmm[reg as usize];
                        let high = op2 == 0x15;
                        self.xmm[reg as usize] = if self.has_66() {
                            unpck_pd(src1, src2, high)
                        } else {
                            unpck_ps(src1, src2, high)
                        };
                    }

                    // MOVHLPS / MOVLPS (0F 12). The encoding splits on
                    // the ModR/M mode: a register source is MOVHLPS
                    // (xmm2 high → xmm1 low, xmm1 high preserved); a
                    // memory source is MOVLPS (m64 → xmm1 low, upper
                    // preserved). Compilers emit MOVLPS for unaligned
                    // 8-byte loads into the low lane of an XMM.
                    0x12 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let new_low = match rm {
                            Rm::Reg(i) => (self.xmm[i as usize] >> 64) as u64,
                            Rm::Mem(ea) => {
                                let a = self.linear_seg(ea.seg, ea.off);
                                (self.mem_read_u32(mem, a) as u64)
                                    | ((self.mem_read_u32(mem, a.wrapping_add(4)) as u64) << 32)
                            }
                        };
                        self.xmm[reg as usize] =
                            (self.xmm[reg as usize] & !(u64::MAX as u128)) | (new_low as u128);
                    }
                    // MOVLPS m64, xmm (0F 13) — store the low 64 bits
                    // of an XMM to memory. The register form is
                    // reserved; fall through to the unimplemented
                    // default if it ever appears.
                    0x13 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if let Rm::Mem(ea) = rm {
                            let a = self.linear_seg(ea.seg, ea.off);
                            let v = self.xmm[reg as usize] as u64;
                            self.mem_write_u32(mem, a, v as u32);
                            self.mem_write_u32(mem, a.wrapping_add(4), (v >> 32) as u32);
                        } else {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                    // MOVLHPS / MOVHPS (0F 16). Mirror of 0F 12 on the
                    // upper lane: reg form is MOVLHPS (xmm2 low → xmm1
                    // high, xmm1 low preserved); mem form is MOVHPS
                    // (m64 → xmm1 high). Compilers pair MOVLPS+MOVHPS
                    // to assemble a misaligned 16-byte load.
                    0x16 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let new_high = match rm {
                            Rm::Reg(i) => self.xmm[i as usize] as u64,
                            Rm::Mem(ea) => {
                                let a = self.linear_seg(ea.seg, ea.off);
                                (self.mem_read_u32(mem, a) as u64)
                                    | ((self.mem_read_u32(mem, a.wrapping_add(4)) as u64) << 32)
                            }
                        };
                        self.xmm[reg as usize] = (self.xmm[reg as usize] & (u64::MAX as u128))
                            | ((new_high as u128) << 64);
                    }
                    // MOVHPS m64, xmm (0F 17) — store the high 64 bits
                    // of an XMM to memory. Register form is reserved.
                    0x17 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if let Rm::Mem(ea) = rm {
                            let a = self.linear_seg(ea.seg, ea.off);
                            let v = (self.xmm[reg as usize] >> 64) as u64;
                            self.mem_write_u32(mem, a, v as u32);
                            self.mem_write_u32(mem, a.wrapping_add(4), (v >> 32) as u32);
                        } else {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }

                    // PSHUFD (66 0F 70 /r ib) — shuffle four dwords of
                    // the source into the destination, with two-bit
                    // selectors packed into the imm8. Each dest lane
                    // can pick any source lane (including duplicates),
                    // so this is the workhorse "broadcast / permute"
                    // for SSE2 integer code. The F3/F2 0F 70 variants
                    // (PSHUFHW/PSHUFLW) go through sse_scalar — they
                    // are not implemented here yet.
                    0x70 if self.has_66() => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let imm = self.fetch_u8(mem);
                        let src = self.read_xmm_rm(rm, mem);
                        let mut out: u128 = 0;
                        for i in 0..4u32 {
                            let sel = ((imm >> (2 * i)) & 0b11) as u32;
                            let lane = (src >> (sel * 32)) & 0xFFFF_FFFF;
                            out |= lane << (i * 32);
                        }
                        self.xmm[reg as usize] = out;
                    }

                    // SHUFPS / SHUFPD (0F C6 /r ib). SHUFPS picks two
                    // 32-bit lanes from the destination for result
                    // lanes 0,1 and two from the source for lanes 2,3
                    // (two bits per selector in imm). SHUFPD (with the
                    // 0x66 prefix) uses 64-bit lanes and a single bit
                    // per selector — only imm[0] and imm[1] are read.
                    0xC6 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let imm = self.fetch_u8(mem);
                        let src1 = self.xmm[reg as usize];
                        let src2 = self.read_xmm_rm(rm, mem);
                        self.xmm[reg as usize] = if self.has_66() {
                            let qw = |v: u128, i: u32| (v >> (i * 64)) & 0xFFFF_FFFF_FFFF_FFFF;
                            qw(src1, (imm & 1) as u32) | (qw(src2, ((imm >> 1) & 1) as u32) << 64)
                        } else {
                            let mut out: u128 = 0;
                            for i in 0..4u32 {
                                let sel = ((imm >> (2 * i)) & 0b11) as u32;
                                let src = if i < 2 { src1 } else { src2 };
                                let lane = (src >> (sel * 32)) & 0xFFFF_FFFF;
                                out |= lane << (i * 32);
                            }
                            out
                        };
                    }

                    // CMOVcc r16/32, r/m16/32 — 0x0F 0x40..0x4F.
                    // Conditional move: writes the source operand
                    // into the destination only if the condition
                    // holds. The whole point is to avoid a branch
                    // — speculative execution stays linear.
                    0x40..=0x4F => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let cond = self.eval_cond(op2 & 0x0F);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            if cond {
                                self.write_r32(reg, v);
                            }
                        } else {
                            let v = self.read_rm16(rm, mem);
                            if cond {
                                self.write_r16(reg, v);
                            }
                        }
                    }

                    // PUSH FS / POP FS / PUSH GS / POP GS. Linux uses
                    // FS (and GS on x86-64) for per-CPU / TLS bases, so
                    // these show up in entry/exit paths constantly.
                    // Operand size determines push/pop width: in 32-bit
                    // code default is 32, so the selector is zero-
                    // extended to 4 bytes and ESP moves by 4.
                    0xA0 => {
                        let v = self.sregs[sreg::FS] as u32;
                        if self.op_size_32 {
                            self.push32(mem, v);
                        } else {
                            self.push16(mem, v as u16);
                        }
                    }
                    0xA1 => {
                        let v = if self.op_size_32 {
                            self.pop32(mem) as u16
                        } else {
                            self.pop16(mem)
                        };
                        if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                            return Ok(());
                        }
                        self.write_sreg(sreg::FS, v, mem);
                    }
                    0xA8 => {
                        let v = self.sregs[sreg::GS] as u32;
                        if self.op_size_32 {
                            self.push32(mem, v);
                        } else {
                            self.push16(mem, v as u16);
                        }
                    }
                    0xA9 => {
                        let v = if self.op_size_32 {
                            self.pop32(mem) as u16
                        } else {
                            self.pop16(mem)
                        };
                        if self.raise_gp_if_bad_selector(v, op_ip, mem) {
                            return Ok(());
                        }
                        self.write_sreg(sreg::GS, v, mem);
                    }
                    // SHLD r/m16/32, r16/32, imm8 — 0x0F 0xA4.
                    // Shifts the destination left by `count`, filling
                    // the low end with bits shifted out of the source's
                    // high end. Count is masked to 5 bits (32-bit
                    // operand) or 4 bits (16-bit). CF gets the last
                    // bit shifted out of dest.
                    0xA4 | 0xA5 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let count = if op2 == 0xA4 {
                            self.fetch_u8(mem) & 0x1F
                        } else {
                            self.read_r8(1) & 0x1F // CL
                        };
                        if self.op_size_32 {
                            shld32(self, rm, reg, count, mem);
                        } else {
                            shld16(self, rm, reg, count & 0x0F, mem);
                        }
                    }
                    // SHRD r/m16/32, r16/32, imm8 — 0x0F 0xAC, CL form 0xAD.
                    0xAC | 0xAD => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let count = if op2 == 0xAC {
                            self.fetch_u8(mem) & 0x1F
                        } else {
                            self.read_r8(1) & 0x1F
                        };
                        if self.op_size_32 {
                            shrd32(self, rm, reg, count, mem);
                        } else {
                            shrd16(self, rm, reg, count & 0x0F, mem);
                        }
                    }

                    // CPUID — 0x0F 0xA2. Inputs in EAX/ECX, results in
                    // EAX/EBX/ECX/EDX. We answer the leaves a Linux
                    // boot path consults during cpu-feature probing,
                    // advertising the subset of ISA we actually
                    // implement (so the kernel takes the fast paths
                    // instead of the i386-fallback ones).
                    //
                    //   leaf 0      max-basic-leaf in EAX, 12-byte
                    //               vendor string in EBX|EDX|ECX
                    //   leaf 1      family/model/stepping in EAX,
                    //               feature flags in EDX/ECX
                    //   0x80000000  max-extended-leaf in EAX
                    //   0x80000002..4   48-byte brand string
                    //
                    // Anything else returns zeros (the "absent leaf"
                    // contract Linux expects).
                    0xA2 => cpuid_dispatch(self),

                    // IMUL r16/32, r/m16/32 — 0x0F 0xAF. Two-operand
                    // signed multiply: reg *= r/m, truncated to the
                    // operand width. CF/OF set when the full product
                    // doesn't fit the destination. This is the form a
                    // C compiler emits for `a * b`, so it shows up
                    // everywhere.
                    0xAF => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let a = self.read_r32(reg) as i32 as i64;
                            let b = self.read_rm32(rm, mem) as i32 as i64;
                            let full = a.wrapping_mul(b);
                            let trunc = full as i32;
                            self.write_r32(reg, trunc as u32);
                            let overflow = i64::from(trunc) != full;
                            self.set_flag(flag::CF, overflow);
                            self.set_flag(flag::OF, overflow);
                        } else {
                            let a = self.read_r16(reg) as i16 as i32;
                            let b = self.read_rm16(rm, mem) as i16 as i32;
                            let full = a.wrapping_mul(b);
                            let trunc = full as i16;
                            self.write_r16(reg, trunc as u16);
                            let overflow = i32::from(trunc) != full;
                            self.set_flag(flag::CF, overflow);
                            self.set_flag(flag::OF, overflow);
                        }
                    }
                    // MOVZX r16/32, r/m8 — 0x0F 0xB6. Zero-extend a
                    // byte into the dest. Under 0x66 dest is r32, else r16.
                    0xB6 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm8(rm, mem);
                        if self.op_size_32 {
                            self.write_r32(reg, v as u32);
                        } else {
                            self.write_r16(reg, v as u16);
                        }
                    }
                    // MOVZX r16/32, r/m16 — 0x0F 0xB7. Zero-extend a
                    // word. Under 0x66 (op_size_32 == false) the dest is
                    // r16, which must write only the low 16 bits and
                    // PRESERVE the upper half of the 32-bit register
                    // (behaves like MOV r16, r/m16); else dest is r32.
                    0xB7 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm16(rm, mem);
                        if self.op_size_32 {
                            self.write_r32(reg, v as u32);
                        } else {
                            self.write_r16(reg, v);
                        }
                    }
                    // MOVSX r16/32, r/m8 — 0x0F 0xBE. Sign-extend.
                    0xBE => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm8(rm, mem) as i8;
                        if self.op_size_32 {
                            self.write_r32(reg, v as i32 as u32);
                        } else {
                            self.write_r16(reg, v as i16 as u16);
                        }
                    }
                    // MOVSX r16/32, r/m16 — 0x0F 0xBF. Under 0x66
                    // (op_size_32 == false) the dest is r16 and source is
                    // r/m16, so this is effectively MOV r16, r/m16: write
                    // only the low 16 bits, preserving the upper half;
                    // else sign-extend the word into the 32-bit dest.
                    0xBF => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm16(rm, mem) as i16;
                        if self.op_size_32 {
                            self.write_r32(reg, v as i32 as u32);
                        } else {
                            self.write_r16(reg, v as u16);
                        }
                    }

                    // SETcc r/m8 — 0x0F 0x90..0x9F. Writes 1 to the
                    // 8-bit destination if the condition holds, 0
                    // otherwise. Linux uses these for branchless
                    // boolean conversions (`bool x = (a == b)`).
                    0x90..=0x9F => {
                        let (_, _, rm) = self.fetch_modrm(mem);
                        let cond = self.eval_cond(op2 & 0x0F);
                        self.write_rm8(rm, mem, if cond { 1 } else { 0 });
                    }

                    // XADD r/m8, r8 — 0x0F 0xC0. Atomic exchange-and-
                    // add. SDM order: SRC := DEST (old), then DEST :=
                    // SRC+DEST. The destination is written LAST so that
                    // when SRC and DEST are the SAME register (XADD AX,AX)
                    // the register ends with the sum (2*old), not the old
                    // value. Used by Linux atomic_add_return/refcount_inc.
                    0xC0 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let dest = self.read_rm8(rm, mem);
                        let src = self.read_r8(reg);
                        let sum = dest.wrapping_add(src);
                        self.flags_add8(dest, src, 0, sum);
                        self.write_r8(reg, dest);
                        self.write_rm8(rm, mem, sum);
                    }
                    // XADD r/m16/32, r16/32 — 0x0F 0xC1. Dest written last
                    // (see 0xC0) for the register-aliased case.
                    0xC1 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let dest = self.read_rm32(rm, mem);
                            let src = self.read_r32(reg);
                            let sum = dest.wrapping_add(src);
                            self.flags_add32(dest, src, 0, sum);
                            self.write_r32(reg, dest);
                            self.write_rm32(rm, mem, sum);
                        } else {
                            let dest = self.read_rm16(rm, mem);
                            let src = self.read_r16(reg);
                            let sum = dest.wrapping_add(src);
                            self.flags_add16(dest, src, 0, sum);
                            self.write_r16(reg, dest);
                            self.write_rm16(rm, mem, sum);
                        }
                    }

                    // CMPXCHG8B m64 — 0x0F 0xC7 /1. Atomic 64-bit
                    // compare-and-swap.  If EDX:EAX == [m64] then
                    // [m64] := ECX:EBX and ZF=1; otherwise
                    // EDX:EAX := [m64] and ZF=0.  Linux uses this
                    // for `cmpxchg64` / `atomic64_t` on i486+, and
                    // for per-CPU pointer updates during boot.
                    // Only the memory form is defined — mod=11
                    // would mean a register operand, which is
                    // invalid on real silicon (and overlaps with
                    // RDRAND/RDSEED on newer CPUs we don't model).
                    0xC7 => {
                        let (mode, reg, rm) = self.fetch_modrm(mem);
                        if mode == 0b11 || reg != 1 {
                            return Err(CpuError::Unimplemented {
                                opcode: op2,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        let Rm::Mem(ea) = rm else { unreachable!() };
                        let addr = self.linear_seg(ea.seg, ea.off);
                        let mem_lo = self.mem_read_u32(mem, addr);
                        let mem_hi = self.mem_read_u32(mem, addr.wrapping_add(4));
                        let expected_lo = self.read_r32(0); // EAX
                        let expected_hi = self.read_r32(2); // EDX
                        if mem_lo == expected_lo && mem_hi == expected_hi {
                            // Match → publish ECX:EBX, set ZF.
                            let new_lo = self.read_r32(3); // EBX
                            let new_hi = self.read_r32(1); // ECX
                            self.mem_write_u32(mem, addr, new_lo);
                            self.mem_write_u32(mem, addr.wrapping_add(4), new_hi);
                            self.set_flag(flag::ZF, true);
                        } else {
                            // Miss → load actual [m64] into EDX:EAX,
                            // clear ZF. The kernel retries the loop.
                            self.write_r32(0, mem_lo);
                            self.write_r32(2, mem_hi);
                            self.set_flag(flag::ZF, false);
                        }
                    }
                    // BSWAP r32 — 0x0F 0xC8..0xCF. Reverses byte
                    // order in a 32-bit register. Linux uses this
                    // for network byte-order conversions.
                    0xC8..=0xCF => {
                        let i = op2 - 0xC8;
                        let v = self.read_r32(i);
                        let swapped = v.swap_bytes();
                        self.write_r32(i, swapped);
                    }

                    // BSF r16/32, r/m16/32 — 0x0F 0xBC. Find the
                    // index of the lowest set bit in source; result
                    // in dest. ZF=1 if source is zero (dest is
                    // architecturally undefined; we leave it).
                    0xBC => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            if v == 0 {
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.set_flag(flag::ZF, false);
                                self.write_r32(reg, v.trailing_zeros());
                            }
                        } else {
                            let v = self.read_rm16(rm, mem);
                            if v == 0 {
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.set_flag(flag::ZF, false);
                                self.write_r16(reg, v.trailing_zeros() as u16);
                            }
                        }
                    }

                    // BSR r16/32, r/m16/32 — 0x0F 0xBD. Same but
                    // scans from the high end (highest set bit).
                    0xBD => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            if v == 0 {
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.set_flag(flag::ZF, false);
                                self.write_r32(reg, 31 - v.leading_zeros());
                            }
                        } else {
                            let v = self.read_rm16(rm, mem);
                            if v == 0 {
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.set_flag(flag::ZF, false);
                                self.write_r16(reg, 15 - v.leading_zeros() as u16);
                            }
                        }
                    }

                    // CMPXCHG r/m8, r8 — 0x0F 0xB0. If AL == r/m8:
                    // store src reg into r/m, set ZF=1. Else load
                    // r/m into AL, ZF=0. The atomic primitive
                    // underneath Linux spinlock_t and friends.
                    0xB0 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let dest = self.read_rm8(rm, mem);
                        let al = self.read_r8(0);
                        if al == dest {
                            let src = self.read_r8(reg);
                            self.write_rm8(rm, mem, src);
                            self.set_flag(flag::ZF, true);
                        } else {
                            self.write_r8(0, dest);
                            self.set_flag(flag::ZF, false);
                        }
                        // Flags as if CMP AL, dest (so SF/PF/CF/AF/OF
                        // also reflect the comparison).
                        let cmp = al.wrapping_sub(dest);
                        self.flags_sub8(al, dest, 0, cmp);
                    }

                    // CMPXCHG r/m16/32, r16/32 — 0x0F 0xB1. AX/EAX
                    // is the accumulator.
                    0xB1 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let dest = self.read_rm32(rm, mem);
                            let eax = self.read_r32(0);
                            if eax == dest {
                                let src = self.read_r32(reg);
                                self.write_rm32(rm, mem, src);
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.write_r32(0, dest);
                                self.set_flag(flag::ZF, false);
                            }
                            let cmp = eax.wrapping_sub(dest);
                            self.flags_sub32(eax, dest, 0, cmp);
                        } else {
                            let dest = self.read_rm16(rm, mem);
                            let ax = self.read_r16(0);
                            if ax == dest {
                                let src = self.read_r16(reg);
                                self.write_rm16(rm, mem, src);
                                self.set_flag(flag::ZF, true);
                            } else {
                                self.write_r16(0, dest);
                                self.set_flag(flag::ZF, false);
                            }
                            let cmp = ax.wrapping_sub(dest);
                            self.flags_sub16(ax, dest, 0, cmp);
                        }
                    }

                    _ => {
                        // WWWVM_DUMP_OP=1 dumps the raw instruction bytes (with
                        // prefixes) at the faulting IP, to tell a genuinely
                        // unimplemented 0F opcode from a decode desync.
                        if std::env::var_os("WWWVM_DUMP_OP").is_some() {
                            let mut bytes = [0u8; 12];
                            for (i, b) in bytes.iter_mut().enumerate() {
                                let a = self.linear_seg(sreg::CS, op_ip.wrapping_add(i as u32));
                                *b = self.mem_fetch_u8(mem, a);
                            }
                            eprintln!(
                                "[wwwvm op2=0F{op2:02X}] {op_cs:04X}:{op_ip:08X} 66={} bytes={bytes:02X?}",
                                self.has_66()
                            );
                        }
                        return Err(CpuError::Unimplemented {
                            opcode: op2,
                            cs: op_cs,
                            ip: op_ip,
                        });
                    }
                }
            }

            // FPU escape opcodes 0xD8..0xDF. We don't model the FP
            // register stack — these are minimal stubs to keep Linux's
            // FPU probe from faulting. The patterns we handle:
            //   DB E3        FNINIT
            //   DB E2        FNCLEX
            //   DF E0        FNSTSW AX
            //   D9 /5 m16    FLDCW
            //   D9 /7 m16    FNSTCW
            // Other 0xD8..0xDF forms surface as Unimplemented so we
            // notice when a real FPU instruction matters.
            0xDB => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                let sub = (modrm >> 3) & 0x07;
                if mode == 0b11 {
                    match modrm {
                        0xE3 => {
                            // FNINIT — reset CW to 0x037F, SW to 0
                            // (which also zeroes the TOP field), and the
                            // stack-top pointer to 0 (tag word = all
                            // empty). fpu_top is tracked separately, so it
                            // must be reset explicitly.
                            self.fpu_sw = 0;
                            self.fpu_cw = 0x037F;
                            self.fpu_top = 0;
                        }
                        0xE2 => {
                            // FNCLEX — clear exception flags (SW bits 0..7).
                            self.fpu_sw &= !0x00FF;
                        }
                        // FUCOMI ST(0),ST(i) (DB E8+i) and FCOMI
                        // ST(0),ST(i) (DB F0+i): set EFLAGS directly, no
                        // pop. Modern compilers emit these for float
                        // comparisons. (Our f64 model treats the ordered
                        // FCOMI and unordered FUCOMI identically.)
                        0xE8..=0xF7 => {
                            let i = modrm & 7;
                            let a = self.fpu_st(0);
                            let b = self.fpu_st(i);
                            self.fpu_set_eflags_compare(a, b);
                        }
                        // FCMOVNcc ST(0),ST(i): copy ST(i) into ST(0) if the
                        // negated EFLAGS condition holds. DB C0+i=FCMOVNB
                        // (CF=0), C8+i=FCMOVNE(ZF=0), D0+i=FCMOVNBE(CF=0&ZF=0),
                        // D8+i=FCMOVNU(PF=0).
                        0xC0..=0xDF => {
                            let cc = match modrm & 0x38 {
                                0x00 => 0x03, // FCMOVNB  (CF=0)
                                0x08 => 0x05, // FCMOVNE  (ZF=0)
                                0x10 => 0x07, // FCMOVNBE (CF=0 & ZF=0)
                                _ => 0x0B,    // FCMOVNU  (PF=0)
                            };
                            if self.eval_cond(cc) {
                                let v = self.fpu_st(modrm & 0x07);
                                self.fpu_set_st(0, v);
                            }
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    match sub {
                        // FILD m32 — load a 32-bit signed integer, push
                        // as float. The `(double)int` conversion.
                        0 => {
                            let v = self.mem_read_u32(mem, addr) as i32;
                            self.fpu_push(F80::from_i32(v));
                        }
                        // FISTTP m32 (SSE3) — truncate ST(0) toward zero,
                        // store as i32, then pop.
                        1 => {
                            let v = self.fpu_pop().to_i64_trunc() as i32;
                            self.mem_write_u32(mem, addr, v as u32);
                        }
                        // FIST m32 — store ST(0) as a rounded i32 (control-
                        // word rounding), WITHOUT popping. busybox's
                        // float->int paths (e.g. sleep's strtod) emit this.
                        2 => {
                            let st0 = self.fpu_st(0);
                            let r = self.fpu_round_f80(st0) as i32;
                            self.mem_write_u32(mem, addr, r as u32);
                        }
                        // FISTP m32 — pop and store as i32, rounded per
                        // the control word (default: nearest-even, NOT
                        // truncation). The `(int)double` conversion.
                        3 => {
                            let v = self.fpu_pop();
                            let r = self.fpu_round_f80(v) as i32;
                            self.mem_write_u32(mem, addr, r as u32);
                        }
                        // FLD m80 — load an 80-bit extended-precision value
                        // (long double) and push it, keeping full precision.
                        5 => {
                            let lo = self.mem_read_u32(mem, addr) as u64;
                            let hi = self.mem_read_u32(mem, addr + 4) as u64;
                            let se = self.mem_read_u16(mem, addr + 8);
                            self.fpu_push(F80::from_f80_parts(lo | (hi << 32), se));
                        }
                        // FSTP m80 — pop ST(0) and store it as an
                        // 80-bit extended value (10 bytes).
                        7 => {
                            let v = self.fpu_pop();
                            let (mant, se) = v.to_f80_parts();
                            self.mem_write_u32(mem, addr, mant as u32);
                            self.mem_write_u32(mem, addr + 4, (mant >> 32) as u32);
                            self.mem_write_u16(mem, addr + 8, se);
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                }
            }
            0xDF => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                if mode == 0b11 {
                    if modrm == 0xE0 {
                        // FNSTSW AX — copy FPU status (incl. the TOP field)
                        // into AX.
                        self.regs[r16::AX] = self.fpu_status_word();
                    } else if (0xE8..=0xF7).contains(&modrm) {
                        // FUCOMIP/FCOMIP ST(0),ST(i): set EFLAGS, then pop.
                        let i = modrm & 7;
                        let a = self.fpu_st(0);
                        let b = self.fpu_st(i);
                        self.fpu_set_eflags_compare(a, b);
                        let _ = self.fpu_pop();
                    } else {
                        return Err(CpuError::Unimplemented {
                            opcode,
                            cs: op_cs,
                            ip: op_ip,
                        });
                    }
                } else {
                    // DF memory forms — integer load/store. The /5 FILD m64
                    // and /7 FISTP m64 (64-bit int <-> double) are how glibc
                    // math / awk convert between numbers and integers.
                    let sub = (modrm >> 3) & 0x07;
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    match sub {
                        // FILD m16 — push a signed 16-bit integer as a float.
                        0 => {
                            let v = self.mem_read_u16(mem, addr) as i16;
                            self.fpu_push(F80::from_i16(v));
                        }
                        // FISTTP m16 (SSE3) — truncate ST(0) toward zero,
                        // store as i16, then pop. (Sibling of the DB /1 m32
                        // and DD /1 m64 forms; CPython's float code emits it.)
                        1 => {
                            let v = self.fpu_pop().to_i64_trunc() as i16;
                            self.mem_write_u16(mem, addr, v as u16);
                        }
                        // FIST m16 — store ST(0) as a rounded signed i16.
                        2 => {
                            let st0 = self.fpu_st(0);
                            let v = self.fpu_round_f80(st0) as i16;
                            self.mem_write_u16(mem, addr, v as u16);
                        }
                        // FISTP m16 — FIST m16 then pop.
                        3 => {
                            let st0 = self.fpu_pop();
                            let v = self.fpu_round_f80(st0) as i16;
                            self.mem_write_u16(mem, addr, v as u16);
                        }
                        // FILD m64 — push a signed 64-bit integer as a float.
                        5 => {
                            let lo = self.mem_read_u32(mem, addr) as u64;
                            let hi = self.mem_read_u32(mem, addr.wrapping_add(4)) as u64;
                            let v = (lo | (hi << 32)) as i64;
                            self.fpu_push(F80::from_i64(v));
                        }
                        // FISTP m64 — store ST(0) as a rounded i64, then pop.
                        7 => {
                            let st0 = self.fpu_pop();
                            let v = self.fpu_round_f80(st0);
                            let u = v as u64;
                            self.mem_write_u32(mem, addr, u as u32);
                            self.mem_write_u32(mem, addr.wrapping_add(4), (u >> 32) as u32);
                        }
                        // /4 FBLD, /6 FBSTP (80-bit packed BCD) — rare.
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                }
            }
            0xD9 => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                let sub = (modrm >> 3) & 0x07;
                let rm_field = modrm & 0x07;
                // Register-form constant loads: FLD1 (E8) / FLDZ (EE).
                if mode == 0b11 {
                    match modrm {
                        0xE8 => self.fpu_push(F80::from_f64(1.0)), // FLD1
                        0xE9 => self.fpu_push(F80::from_f64(std::f64::consts::LOG2_10)), // FLDL2T
                        0xEA => self.fpu_push(F80::from_f64(std::f64::consts::LOG2_E)), // FLDL2E
                        0xEB => self.fpu_push(F80::from_f64(std::f64::consts::PI)), // FLDPI
                        0xEC => self.fpu_push(F80::from_f64(std::f64::consts::LOG10_2)), // FLDLG2
                        0xED => self.fpu_push(F80::from_f64(std::f64::consts::LN_2)), // FLDLN2
                        0xEE => self.fpu_push(F80::ZERO),          // FLDZ
                        // FCHS — negate ST(0).
                        0xE0 => self.fpu_set_st(0, -self.fpu_st(0)),
                        // FABS — absolute value of ST(0).
                        0xE1 => self.fpu_set_st(0, self.fpu_st(0).abs()),
                        // FTST — compare ST(0) with 0.0.
                        0xE4 => {
                            let st0 = self.fpu_st(0);
                            self.fpu_compare(st0, F80::ZERO);
                        }
                        // FXAM — classify ST(0) into C3:C2:C1:C0.
                        0xE5 => self.fpu_fxam(),
                        // FSQRT — square root of ST(0), correctly rounded to
                        // the full 80-bit mantissa (F80::sqrt uses u128 isqrt).
                        0xFA => {
                            let v = self.fpu_st(0);
                            self.fpu_set_st(0, v.sqrt());
                        }
                        // FRNDINT — round ST(0) to an integer per the
                        // control-word rounding mode (default nearest-even).
                        0xFC => {
                            let rc = ((self.fpu_cw >> 10) & 3) as u8;
                            let v = self.fpu_st(0);
                            self.fpu_set_st(0, v.round_to_integer(rc));
                        }
                        // ---- Transcendentals (D9 F0..FF) ----
                        // These set/clear C2 (status bit 10, 0x0400): C2=1
                        // means the argument was out of the hardware
                        // reduction range (|x| >= 2^63) and the op was a
                        // no-op. We compute in f64 over the full range, so
                        // we always clear C2 to signal "reduced/complete".
                        //
                        // FSIN — ST(0) = sin(ST(0)). (Transcendentals are
                        // evaluated in f64 — second-tier, not the dtoa path.)
                        0xFE => {
                            let v = self.fpu_st(0).to_f64();
                            self.fpu_set_st(0, F80::from_f64(v.sin()));
                            self.fpu_sw &= !0x0400;
                        }
                        // FCOS — ST(0) = cos(ST(0)).
                        0xFF => {
                            let v = self.fpu_st(0).to_f64();
                            self.fpu_set_st(0, F80::from_f64(v.cos()));
                            self.fpu_sw &= !0x0400;
                        }
                        // FSINCOS — ST(0) = sin(ST(0)); push cos(ST(0))
                        // (so afterwards ST(1)=sin, ST(0)=cos).
                        0xFB => {
                            let v = self.fpu_st(0).to_f64();
                            self.fpu_set_st(0, F80::from_f64(v.sin()));
                            self.fpu_push(F80::from_f64(v.cos()));
                            self.fpu_sw &= !0x0400;
                        }
                        // FPTAN — ST(0) = tan(ST(0)); push 1.0 (the x87
                        // convention so a following FDIV yields cot, etc.).
                        0xF2 => {
                            let v = self.fpu_st(0).to_f64();
                            self.fpu_set_st(0, F80::from_f64(v.tan()));
                            self.fpu_push(F80::from_f64(1.0));
                            self.fpu_sw &= !0x0400;
                        }
                        // FPATAN — ST(1) = atan2(ST(1), ST(0)); pop. Result
                        // (the angle) ends up in ST(0).
                        0xF3 => {
                            let st0 = self.fpu_st(0).to_f64();
                            let st1 = self.fpu_st(1).to_f64();
                            self.fpu_pop();
                            self.fpu_set_st(0, F80::from_f64(st1.atan2(st0)));
                        }
                        // F2XM1 — ST(0) = 2^ST(0) - 1 (defined for
                        // ST(0) in [-1, 1]; we evaluate everywhere).
                        0xF0 => {
                            let v = self.fpu_st(0).to_f64();
                            self.fpu_set_st(0, F80::from_f64(v.exp2() - 1.0));
                            self.fpu_sw &= !0x0400;
                        }
                        // FYL2X — ST(1) = ST(1) * log2(ST(0)); pop. Result
                        // in ST(0).
                        0xF1 => {
                            let st0 = self.fpu_st(0).to_f64();
                            let st1 = self.fpu_st(1).to_f64();
                            self.fpu_pop();
                            self.fpu_set_st(0, F80::from_f64(st1 * st0.log2()));
                        }
                        // FYL2XP1 — ST(1) = ST(1) * log2(ST(0)+1); pop.
                        // Uses ln_1p for accuracy near ST(0)=0.
                        0xF9 => {
                            let st0 = self.fpu_st(0).to_f64();
                            let st1 = self.fpu_st(1).to_f64();
                            self.fpu_pop();
                            self.fpu_set_st(
                                0,
                                F80::from_f64(st1 * (st0.ln_1p() / std::f64::consts::LN_2)),
                            );
                        }
                        // FSCALE — ST(0) = ST(0) * 2^trunc(ST(1)). Exact in
                        // F80: add the integer scale to ST(0)'s exponent.
                        0xFD => {
                            let st0 = self.fpu_st(0);
                            let n = self.fpu_st(1).to_i64_trunc();
                            self.fpu_set_st(
                                0,
                                st0.scale2(n.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
                            );
                        }
                        // FXTRACT — split ST(0) into exponent and significand:
                        // ST(0) = unbiased exponent, then push the significand
                        // (in [1, 2)). Exact via F80's exponent/mantissa.
                        0xF4 => {
                            let v = self.fpu_st(0);
                            self.fpu_set_st(0, v.exponent_f80());
                            self.fpu_push(v.significand());
                        }
                        // FPREM — partial remainder ST(0) mod ST(1),
                        // truncated quotient. We complete in one step, so
                        // clear C2 (the "incomplete reduction" flag).
                        0xF8 => {
                            let st0 = self.fpu_st(0);
                            let st1 = self.fpu_st(1);
                            let q = (st0 / st1).round_to_integer(3); // trunc
                            self.fpu_set_st(0, st0 - st1 * q);
                            self.fpu_set_prem_cc(q);
                        }
                        // FPREM1 — IEEE-754 partial remainder (round-to-
                        // nearest quotient). Same single-step completion.
                        0xF5 => {
                            let st0 = self.fpu_st(0);
                            let st1 = self.fpu_st(1);
                            let q = (st0 / st1).round_to_integer(0); // nearest
                            self.fpu_set_st(0, st0 - st1 * q);
                            self.fpu_set_prem_cc(q);
                        }
                        // FDECSTP / FINCSTP — rotate TOP without touching
                        // register contents.
                        0xF6 => self.fpu_top = self.fpu_top.wrapping_sub(1) & 7,
                        0xF7 => self.fpu_top = self.fpu_top.wrapping_add(1) & 7,
                        // D9 C8+i — FXCH ST(i): swap ST(0) and ST(i).
                        0xC8..=0xCF => {
                            let i = modrm & 0x07;
                            let st0 = self.fpu_st(0);
                            let sti = self.fpu_st(i);
                            self.fpu_set_st(0, sti);
                            self.fpu_set_st(i, st0);
                        }
                        // D9 C0+i — FLD ST(i): push a copy of ST(i).
                        0xC0..=0xC7 => {
                            let v = self.fpu_st(modrm & 0x07);
                            self.fpu_push(v);
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, rm_field, mem)
                    } else {
                        self.compute_ea(mode, rm_field, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    match sub {
                        // FLD m32 — load a 32-bit float and push.
                        0 => {
                            let bits = self.mem_read_u32(mem, addr);
                            self.fpu_push(F80::from_f32(f32::from_bits(bits)));
                        }
                        // FST m32 — store ST(0) as f32 (no pop).
                        2 => {
                            let v = self.fpu_st(0).to_f64() as f32;
                            self.mem_write_u32(mem, addr, v.to_bits());
                        }
                        // FSTP m32 — store ST(0) as f32 and pop.
                        3 => {
                            let v = self.fpu_pop().to_f64() as f32;
                            self.mem_write_u32(mem, addr, v.to_bits());
                        }
                        // FLDENV m28 — load the x87 environment. glibc's
                        // feholdexcept/fesetenv (used to bracket libm
                        // transcendentals) round-trip CW/SW/TOP through this.
                        // The 32-bit protected-mode image puts CW at +0, SW
                        // at +4, tag word at +8; we restore CW/SW/TOP and
                        // ignore the (unmodeled) instruction/data pointers.
                        4 => {
                            let cw = self.mem_read_u16(mem, addr);
                            let sw = self.mem_read_u16(mem, addr.wrapping_add(4));
                            self.fpu_cw = cw;
                            self.fpu_top = ((sw >> 11) & 7) as u8;
                            self.fpu_sw = sw & !0x3800;
                        }
                        // FLDCW m16 — load control word.
                        5 => self.fpu_cw = self.mem_read_u16(mem, addr),
                        // FNSTENV m28 — store the x87 environment, then mask
                        // all FP exceptions (CW |= 0x3F), per the SDM (this
                        // is why feholdexcept uses it). We write CW/SW/TW;
                        // the IP/DP/opcode fields are zeroed (not modeled).
                        6 => {
                            let cw = self.fpu_cw;
                            let sw = self.fpu_status_word();
                            for off in (0..28).step_by(4) {
                                self.mem_write_u32(mem, addr.wrapping_add(off), 0);
                            }
                            self.mem_write_u16(mem, addr, cw);
                            self.mem_write_u16(mem, addr.wrapping_add(4), sw);
                            // tag word: 0xFFFF = all empty. Our model doesn't
                            // enforce tags, so the exact value is cosmetic.
                            self.mem_write_u16(mem, addr.wrapping_add(8), 0xFFFF);
                            self.fpu_cw |= 0x3F;
                        }
                        // FNSTCW m16 — store control word.
                        7 => {
                            let cw = self.fpu_cw;
                            self.mem_write_u16(mem, addr, cw);
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                }
            }
            // DA — 32-bit-integer-source arithmetic (FIADD/FIMUL/FICOM/
            // FICOMP/FISUB/FISUBR/FIDIV/FIDIVR with an m32int operand), the
            // FCMOVcc register forms (B/E/BE/U), and FUCOMPP (DA E9). The
            // integer-arith forms appear directly in busybox/glibc.
            0xDA => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                if mode == 0b11 {
                    match modrm {
                        // DA E9 — FUCOMPP: compare ST(0),ST(1), pop twice.
                        0xE9 => {
                            let st0 = self.fpu_st(0);
                            let st1 = self.fpu_st(1);
                            self.fpu_compare(st0, st1);
                            let _ = self.fpu_pop();
                            let _ = self.fpu_pop();
                        }
                        // FCMOVcc ST(0),ST(i): if the EFLAGS condition holds,
                        // copy ST(i) into ST(0). DA C0+i=FCMOVB(CF=1), C8+i=
                        // FCMOVE(ZF=1), D0+i=FCMOVBE(CF=1|ZF=1), D8+i=
                        // FCMOVU(PF=1).
                        0xC0..=0xDF => {
                            let cc = match modrm & 0x38 {
                                0x00 => 0x02, // FCMOVB  — "below"  (CF=1)
                                0x08 => 0x04, // FCMOVE  — "equal"  (ZF=1)
                                0x10 => 0x06, // FCMOVBE — "be"     (CF=1|ZF=1)
                                _ => 0x0A,    // FCMOVU  — "parity" (PF=1)
                            };
                            if self.eval_cond(cc) {
                                let v = self.fpu_st(modrm & 0x07);
                                self.fpu_set_st(0, v);
                            }
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                } else {
                    // Memory form: signed 32-bit integer source → f64.
                    let op = (modrm >> 3) & 0x07;
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    let src = F80::from_i32(self.mem_read_u32(mem, addr) as i32);
                    let st0 = self.fpu_st(0);
                    if op == 2 || op == 3 {
                        self.fpu_compare(st0, src);
                        if op == 3 {
                            let _ = self.fpu_pop(); // FICOMP pops
                        }
                    } else if let Some(r) = Self::fpu_arith(op, st0, src) {
                        self.fpu_set_st(0, r);
                    }
                }
            }
            // DE — arithmetic with a pop. The register forms used by
            // compilers: FADDP/FMULP/FSUBRP/FSUBP/FDIVRP/FDIVP ST(i),
            // ST(0). DE C1 = FADDP ST(1),ST(0): ST(1) op= ST(0), pop.
            0xDE => {
                let modrm = self.fetch_u8(mem);
                if modrm >> 6 != 0b11 {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
                }
                if modrm == 0xD9 {
                    // FCOMPP — compare ST(0) with ST(1), set condition
                    // codes, then pop twice. (The classic
                    // `fcompp; fnstsw ax; sahf` float-compare idiom.)
                    let st0 = self.fpu_st(0);
                    let st1 = self.fpu_st(1);
                    self.fpu_compare(st0, st1);
                    let _ = self.fpu_pop();
                    let _ = self.fpu_pop();
                } else {
                    let i = modrm & 0x07; // DE C0+i → ST(i) destination
                    let op = (modrm >> 3) & 0x07;
                    let dst = self.fpu_st(i);
                    let src = self.fpu_st(0);
                    let r = match op {
                        0 => dst + src, // FADDP
                        1 => dst * src, // FMULP
                        4 => src - dst, // FSUBRP
                        5 => dst - src, // FSUBP
                        6 => src / dst, // FDIVRP
                        7 => dst / src, // FDIVP
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    };
                    self.fpu_set_st(i, r);
                    let _ = self.fpu_pop();
                }
            }
            // D8 — arithmetic with ST(0) as destination, source is a
            // 32-bit memory float or ST(i). FADD/FMUL/FSUB/etc.
            0xD8 => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                let op = (modrm >> 3) & 0x07;
                let src = if mode == 0b11 {
                    self.fpu_st(modrm & 0x07)
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    F80::from_f32(f32::from_bits(self.mem_read_u32(mem, addr)))
                };
                let st0 = self.fpu_st(0);
                if op == 2 || op == 3 {
                    self.fpu_compare(st0, src);
                    if op == 3 {
                        let _ = self.fpu_pop(); // FCOMP pops
                    }
                } else if let Some(r) = Self::fpu_arith(op, st0, src) {
                    self.fpu_set_st(0, r);
                }
            }
            // DC — like D8 but the memory operand is a 64-bit double;
            // the register forms target ST(i) (reversed).
            0xDC => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                let op = (modrm >> 3) & 0x07;
                if mode == 0b11 {
                    // DC C0+i: ST(i) = ST(i) op ST(0). The reg field's
                    // SUB/DIV directions are REVERSED vs D8 (DC E0+i =
                    // FSUBR, E8+i = FSUB, F0+i = FDIVR, F8+i = FDIV), so
                    // pass (ST(0), ST(i)) to fpu_arith — its op=4 (a-b)
                    // then yields ST(0)-ST(i) for FSUBR, op=5 (b-a) yields
                    // ST(i)-ST(0) for FSUB, etc.
                    let i = modrm & 0x07;
                    let dst = self.fpu_st(i);
                    let st0 = self.fpu_st(0);
                    if let Some(r) = Self::fpu_arith(op, st0, dst) {
                        self.fpu_set_st(i, r);
                    }
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    let src = F80::from_f64(f64::from_bits(
                        self.mem_read_u32(mem, addr) as u64
                            | ((self.mem_read_u32(mem, addr.wrapping_add(4)) as u64) << 32),
                    ));
                    let st0 = self.fpu_st(0);
                    if op == 2 || op == 3 {
                        self.fpu_compare(st0, src);
                        if op == 3 {
                            let _ = self.fpu_pop();
                        }
                    } else if let Some(r) = Self::fpu_arith(op, st0, src) {
                        self.fpu_set_st(0, r);
                    }
                }
            }
            // DD — m64 load/store plus register stores/FFREE.
            0xDD => {
                let modrm = self.fetch_u8(mem);
                let mode = modrm >> 6;
                let sub = (modrm >> 3) & 0x07;
                if mode == 0b11 {
                    match sub {
                        // DD D0+i FST ST(i) / DD D8+i FSTP ST(i).
                        2 => self.fpu_set_st(modrm & 0x07, self.fpu_st(0)),
                        3 => {
                            let v = self.fpu_st(0);
                            self.fpu_set_st(modrm & 0x07, v);
                            let _ = self.fpu_pop();
                        }
                        // DD C0+i FFREE — mark a register free; we don't
                        // model tags, so it's a no-op.
                        0 => {}
                        // DD E0+i FUCOM ST(i) / DD E8+i FUCOMP ST(i):
                        // unordered compare ST(0) vs ST(i) → condition
                        // codes; FUCOMP pops once. (f64 model: same as
                        // FCOM.)
                        4 | 5 => {
                            let i = modrm & 0x07;
                            let st0 = self.fpu_st(0);
                            let sti = self.fpu_st(i);
                            self.fpu_compare(st0, sti);
                            if sub == 5 {
                                let _ = self.fpu_pop();
                            }
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    match sub {
                        0 => {
                            // FLD m64.
                            let lo = self.mem_read_u32(mem, addr) as u64;
                            let hi = self.mem_read_u32(mem, addr.wrapping_add(4)) as u64;
                            self.fpu_push(F80::from_f64(f64::from_bits(lo | (hi << 32))));
                        }
                        // FISTTP m64 (SSE3) — truncate ST(0) toward zero,
                        // store as i64, then pop.
                        1 => {
                            let v = self.fpu_pop().to_i64_trunc();
                            self.mem_write_u32(mem, addr, v as u32);
                            self.mem_write_u32(mem, addr.wrapping_add(4), (v >> 32) as u32);
                        }
                        2 | 3 => {
                            // FST/FSTP m64.
                            let v = self.fpu_st(0).to_f64().to_bits();
                            self.mem_write_u32(mem, addr, v as u32);
                            self.mem_write_u32(mem, addr.wrapping_add(4), (v >> 32) as u32);
                            if sub == 3 {
                                let _ = self.fpu_pop();
                            }
                        }
                        // DD /7 FNSTSW m16 — store FPU status word to
                        // memory. Linux's fpu init test sequence
                        // (fninit; fnstsw m; fnstcw m; check) uses
                        // this; without it the test "succeeds" only
                        // because we silently failed and Linux then
                        // clears X86_FEATURE_FPU.
                        7 => {
                            let sw = self.fpu_status_word();
                            self.mem_write_u16(mem, addr, sw);
                        }
                        _ => {
                            return Err(CpuError::Unimplemented {
                                opcode,
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                    }
                }
            }

            _ => {
                return Err(CpuError::Unimplemented {
                    opcode,
                    cs: op_cs,
                    ip: op_ip,
                });
            }
        }
        // If this instruction raised a #PF, roll its GP-register and
        // flag changes back so the EIP-rewound retry re-runs from the
        // pristine pre-instruction state (see the checkpoint above).
        // String/REP ops (0xA4..0xAF and the F2/F3 string-loop prefix)
        // commit completed iterations and do their own per-iteration
        // SI/DI/flags rollback, so they opt out here.
        if self.pending_fault.get().is_some()
            && !matches!(
                opcode,
                0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF | 0xF2 | 0xF3
            )
        {
            self.regs = reg_snap;
            self.regs_high = reg_high_snap;
            self.flags = flags_snap;
            self.flags_high = flags_high_snap;
        }
        // Roll back the x87 stack / XMM / MMX for a faulting FP instruction
        // (see the fp_snap checkpoint above). This is what makes an `FLD m64`
        // of a constant on a not-yet-paged libm page retry cleanly instead
        // of leaving a garbage push on the stack. MMX matters for the same
        // reason: a read-modify-write like `PMULUDQ mm,[m64]` (OpenSSL's RSA
        // bignum core) on a demand-paged operand would otherwise commit
        // `mm*0`, then retry from the now-zeroed register — a silent
        // miscompute, since the destination is also a source.
        if self.pending_fault.get().is_some() {
            if let Some((top, sw, cw, st, xmm, mmx)) = fp_snap {
                self.fpu_top = top;
                self.fpu_sw = sw;
                self.fpu_cw = cw;
                self.fpu_st = st;
                self.xmm = xmm;
                self.mmx = mmx;
            }
        }
        Ok(())
    }

    /// Evaluate a condition code (low nibble of Jcc opcode).
    fn eval_cond(&self, code: u8) -> bool {
        let cf = self.has(flag::CF);
        let zf = self.has(flag::ZF);
        let sf = self.has(flag::SF);
        let of = self.has(flag::OF);
        let pf = self.has(flag::PF);
        match code {
            0x0 => of,                // JO
            0x1 => !of,               // JNO
            0x2 => cf,                // JB / JC
            0x3 => !cf,               // JAE / JNC
            0x4 => zf,                // JE / JZ
            0x5 => !zf,               // JNE / JNZ
            0x6 => cf || zf,          // JBE
            0x7 => !cf && !zf,        // JA
            0x8 => sf,                // JS
            0x9 => !sf,               // JNS
            0xA => pf,                // JP
            0xB => !pf,               // JNP
            0xC => sf != of,          // JL
            0xD => sf == of,          // JGE
            0xE => zf || (sf != of),  // JLE
            0xF => !zf && (sf == of), // JG
            _ => false,
        }
    }
}

/// Packed per-lane add: split `a` and `b` into `lane_bits`-wide
/// lanes (8/16/32/64), add each with wraparound, recombine. The
/// SSE PADDB/PADDW/PADDD/PADDQ family.
fn packed_add(a: u128, b: u128, lane_bits: u32) -> u128 {
    packed_lanes(a, b, lane_bits, |x, y, mask| x.wrapping_add(y) & mask)
}

/// Packed per-lane subtract — PSUBB/PSUBW/PSUBD/PSUBQ.
fn packed_sub(a: u128, b: u128, lane_bits: u32) -> u128 {
    packed_lanes(a, b, lane_bits, |x, y, mask| x.wrapping_sub(y) & mask)
}

/// Packed per-lane equality — PCMPEQB/W/D. Each lane is all-ones if
/// the two source lanes are equal, all-zeros otherwise.
fn pcmpeq(a: u128, b: u128, lane_bits: u32) -> u128 {
    packed_lanes(a, b, lane_bits, |x, y, mask| if x == y { mask } else { 0 })
}

/// Packed per-lane signed-greater-than — PCMPGTB/W/D. Lanes are
/// sign-extended from their narrow width before the compare.
fn pcmpgt(a: u128, b: u128, lane_bits: u32) -> u128 {
    let signed = |v: u128| -> i64 {
        match lane_bits {
            8 => v as u8 as i8 as i64,
            16 => v as u16 as i16 as i64,
            32 => v as u32 as i32 as i64,
            _ => v as i64,
        }
    };
    packed_lanes(a, b, lane_bits, |x, y, mask| {
        if signed(x) > signed(y) {
            mask
        } else {
            0
        }
    })
}

/// Packed left shift by `count` — PSLLW/D/Q. Counts ≥ lane width
/// clear the lane (matching x86's saturating-zero semantics).
fn packed_shift_left(a: u128, lane_bits: u32, count: u32) -> u128 {
    if count >= lane_bits {
        return 0;
    }
    packed_lanes(a, 0, lane_bits, |x, _, mask| (x << count) & mask)
}

/// Packed logical right shift — PSRLW/D/Q. Counts ≥ lane width
/// clear the lane.
fn packed_shift_logical_right(a: u128, lane_bits: u32, count: u32) -> u128 {
    if count >= lane_bits {
        return 0;
    }
    packed_lanes(a, 0, lane_bits, |x, _, _| x >> count)
}

/// Packed arithmetic right shift — PSRAW/D (no PSRAQ in SSE2).
/// Counts ≥ lane width clamp to `lane_bits-1`, replicating the sign
/// bit across the lane.
fn packed_shift_arithmetic_right(a: u128, lane_bits: u32, count: u32) -> u128 {
    let count = count.min(lane_bits - 1);
    packed_lanes(a, 0, lane_bits, |x, _, mask| {
        let sb = 1u128 << (lane_bits - 1);
        let signed = if x & sb != 0 { x | !mask } else { x };
        ((signed as i128) >> count) as u128 & mask
    })
}

/// Packed saturating add — unsigned (PADDUSB/PADDUSW). Lanes that
/// would overflow past the lane's max clamp to that max.
fn packed_sat_add_unsigned(a: u128, b: u128, lane_bits: u32) -> u128 {
    packed_lanes(a, b, lane_bits, |x, y, mask| {
        let s = x + y; // fits within u128 even when both lanes are the lane's max
        if s > mask {
            mask
        } else {
            s
        }
    })
}

/// Packed saturating subtract — unsigned (PSUBUSB/PSUBUSW). Lanes
/// that would underflow below zero clamp to zero.
fn packed_sat_sub_unsigned(a: u128, b: u128, lane_bits: u32) -> u128 {
    packed_lanes(a, b, lane_bits, |x, y, _| x.saturating_sub(y))
}

/// Packed saturating add — signed (PADDSB/PADDSW). The signed range
/// of the lane bounds the result; both operands sign-extend first.
fn packed_sat_add_signed(a: u128, b: u128, lane_bits: u32) -> u128 {
    let lo: i64 = -(1i64 << (lane_bits - 1));
    let hi: i64 = (1i64 << (lane_bits - 1)) - 1;
    packed_lanes(a, b, lane_bits, |x, y, mask| {
        let sx = sign_extend_lane(x, lane_bits);
        let sy = sign_extend_lane(y, lane_bits);
        ((sx + sy).clamp(lo, hi) as u128) & mask
    })
}

/// Packed saturating subtract — signed (PSUBSB/PSUBSW).
fn packed_sat_sub_signed(a: u128, b: u128, lane_bits: u32) -> u128 {
    let lo: i64 = -(1i64 << (lane_bits - 1));
    let hi: i64 = (1i64 << (lane_bits - 1)) - 1;
    packed_lanes(a, b, lane_bits, |x, y, mask| {
        let sx = sign_extend_lane(x, lane_bits);
        let sy = sign_extend_lane(y, lane_bits);
        ((sx - sy).clamp(lo, hi) as u128) & mask
    })
}

/// Sign-extend a masked lane value to i64. Used by the signed
/// saturation helpers and any other lane-aware signed compare/arith.
fn sign_extend_lane(v: u128, lane_bits: u32) -> i64 {
    match lane_bits {
        8 => v as u8 as i8 as i64,
        16 => v as u16 as i16 as i64,
        32 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

/// PACKSSWB — pack eight i16 words to eight i8 bytes per source,
/// signed-saturated. Destination's lanes occupy the low 8 bytes of
/// the result; source's lanes the high 8.
fn packsswb(a: u128, b: u128) -> u128 {
    let mut out: u128 = 0;
    for i in 0..8u32 {
        let av = sign_extend_lane((a >> (i * 16)) & 0xFFFF, 16);
        let bv = sign_extend_lane((b >> (i * 16)) & 0xFFFF, 16);
        out |= ((av.clamp(-128, 127) as u8) as u128) << (i * 8);
        out |= ((bv.clamp(-128, 127) as u8) as u128) << ((i + 8) * 8);
    }
    out
}

/// PACKUSWB — pack eight signed-i16 words to eight u8 bytes
/// (unsigned saturation: negatives clamp to 0, > 255 clamp to 255).
fn packuswb(a: u128, b: u128) -> u128 {
    let mut out: u128 = 0;
    for i in 0..8u32 {
        let av = sign_extend_lane((a >> (i * 16)) & 0xFFFF, 16);
        let bv = sign_extend_lane((b >> (i * 16)) & 0xFFFF, 16);
        out |= ((av.clamp(0, 255) as u8) as u128) << (i * 8);
        out |= ((bv.clamp(0, 255) as u8) as u128) << ((i + 8) * 8);
    }
    out
}

/// PACKSSDW — pack four i32 dwords to four i16 words per source,
/// signed-saturated.
fn packssdw(a: u128, b: u128) -> u128 {
    let mut out: u128 = 0;
    for i in 0..4u32 {
        let av = sign_extend_lane((a >> (i * 32)) & 0xFFFF_FFFF, 32);
        let bv = sign_extend_lane((b >> (i * 32)) & 0xFFFF_FFFF, 32);
        out |= ((av.clamp(-32768, 32767) as u16) as u128) << (i * 16);
        out |= ((bv.clamp(-32768, 32767) as u16) as u128) << ((i + 4) * 16);
    }
    out
}

/// PMULLW — packed 16×16 multiply, keep low 16 of each product.
/// The low half is sign-agnostic so a plain wrapping_mul suffices.
fn pmullw(a: u128, b: u128) -> u128 {
    packed_lanes(a, b, 16, |x, y, mask| x.wrapping_mul(y) & mask)
}

/// PMULHW — packed signed 16×16 multiply, keep the high 16 of each
/// product. Sign-extending both operands to i32 before the multiply
/// avoids the i16×i16 → i32 ambiguity.
fn pmulhw(a: u128, b: u128) -> u128 {
    packed_lanes(a, b, 16, |x, y, mask| {
        let sx = x as u16 as i16 as i32;
        let sy = y as u16 as i16 as i32;
        ((sx * sy) >> 16) as u128 & mask
    })
}

/// PMADDWD — multiply adjacent signed 16-bit pairs and sum each pair
/// into a 32-bit lane. Sum wraps modulo 2^32 per the Intel SDM.
fn pmaddwd(a: u128, b: u128) -> u128 {
    let mut out: u128 = 0;
    for i in 0..4u32 {
        let lo_off = i * 32;
        let hi_off = i * 32 + 16;
        let a_lo = ((a >> lo_off) & 0xFFFF) as u16 as i16 as i32;
        let a_hi = ((a >> hi_off) & 0xFFFF) as u16 as i16 as i32;
        let b_lo = ((b >> lo_off) & 0xFFFF) as u16 as i16 as i32;
        let b_hi = ((b >> hi_off) & 0xFFFF) as u16 as i16 as i32;
        let sum = (a_lo as i64) * (b_lo as i64) + (a_hi as i64) * (b_hi as i64);
        out |= ((sum as u32) as u128) << (i * 32);
    }
    out
}

/// PSLLDQ — byte-granular left shift of the whole 128-bit register
/// (not per-lane). Counts ≥ 16 clear the register.
fn byte_shift_left_128(v: u128, count: u32) -> u128 {
    if count >= 16 {
        0
    } else {
        v << (count * 8)
    }
}

/// PSRLDQ — byte-granular right shift of the whole 128-bit register.
fn byte_shift_right_128(v: u128, count: u32) -> u128 {
    if count >= 16 {
        0
    } else {
        v >> (count * 8)
    }
}

/// Apply `f` to each `lane_bits`-wide lane of `a` and `b`. `f`
/// receives the two lane values (already masked) and the lane mask,
/// and returns the masked result lane.
fn packed_lanes(a: u128, b: u128, lane_bits: u32, f: impl Fn(u128, u128, u128) -> u128) -> u128 {
    let mask: u128 = if lane_bits == 128 {
        u128::MAX
    } else {
        (1u128 << lane_bits) - 1
    };
    let mut out: u128 = 0;
    let mut shift = 0u32;
    while shift < 128 {
        let la = (a >> shift) & mask;
        let lb = (b >> shift) & mask;
        out |= (f(la, lb, mask) & mask) << shift;
        shift += lane_bits;
    }
    out
}

/// Packed single-precision float op — split `a` and `b` into 4 ×
/// f32 lanes, apply `f` lane-by-lane, recombine. The SSE
/// ADDPS/SUBPS/MULPS/DIVPS family (no 0x66 prefix).
fn packed_f32(a: u128, b: u128, f: impl Fn(f32, f32) -> f32) -> u128 {
    let mut out: u128 = 0;
    for lane in 0..4 {
        let shift = lane * 32;
        let la = f32::from_bits((a >> shift) as u32);
        let lb = f32::from_bits((b >> shift) as u32);
        out |= (f(la, lb).to_bits() as u128) << shift;
    }
    out
}

/// Packed double-precision float op — split `a` and `b` into 2 ×
/// f64 lanes. The SSE2 ADDPD/SUBPD/MULPD/DIVPD family (0x66 prefix).
fn packed_f64(a: u128, b: u128, f: impl Fn(f64, f64) -> f64) -> u128 {
    let mut out: u128 = 0;
    for lane in 0..2 {
        let shift = lane * 64;
        let la = f64::from_bits((a >> shift) as u64);
        let lb = f64::from_bits((b >> shift) as u64);
        out |= (f(la, lb).to_bits() as u128) << shift;
    }
    out
}

/// SSE CMPPS/CMPPD/CMPSS/CMPSD predicate (imm8 low 3 bits) with x86 NaN
/// semantics: the ordered forms (EQ/LT/LE/ORD) are false when either operand
/// is NaN; the unordered forms (UNORD/NEQ/NLT/NLE) are true on NaN. Rust's
/// `==`/`<`/`<=` already return false on NaN, so the negated forms come out
/// right by construction.
// The negated `!(x < y)` / `!(x <= y)` are deliberate: NLT/NLE must be TRUE
// when a NaN is involved (x86 unordered semantics), which `>=`/`>` would get
// wrong — so the lint that wants `>=`/`>` here does not apply.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn sse_cmp(x: f64, y: f64, pred: u8) -> bool {
    match pred & 7 {
        0 => x == y,                      // EQ
        1 => x < y,                       // LT
        2 => x <= y,                      // LE
        3 => x.is_nan() || y.is_nan(),    // UNORD
        4 => !(x == y),                   // NEQ
        5 => !(x < y),                    // NLT
        6 => !(x <= y),                   // NLE
        _ => !(x.is_nan() || y.is_nan()), // ORD
    }
}

/// CMPPD (66 0F C2): 2×f64 lanes → per-lane all-ones (true) / all-zeros mask.
fn packed_cmp_f64(a: u128, b: u128, pred: u8) -> u128 {
    let mut out: u128 = 0;
    for lane in 0..2 {
        let shift = lane * 64;
        let x = f64::from_bits((a >> shift) as u64);
        let y = f64::from_bits((b >> shift) as u64);
        if sse_cmp(x, y, pred) {
            out |= (u64::MAX as u128) << shift;
        }
    }
    out
}

/// CMPPS (0F C2): 4×f32 lanes → per-lane all-ones / all-zeros mask.
fn packed_cmp_f32(a: u128, b: u128, pred: u8) -> u128 {
    let mut out: u128 = 0;
    for lane in 0..4 {
        let shift = lane * 32;
        let x = f32::from_bits((a >> shift) as u32) as f64;
        let y = f32::from_bits((b >> shift) as u32) as f64;
        if sse_cmp(x, y, pred) {
            out |= (u32::MAX as u128) << shift;
        }
    }
    out
}

fn f64_lane(v: u128, i: u32) -> f64 {
    f64::from_bits((v >> (i * 64)) as u64)
}
fn f32_lane(v: u128, i: u32) -> f32 {
    f32::from_bits((v >> (i * 32)) as u32)
}
fn pack_f64(lo: f64, hi: f64) -> u128 {
    (lo.to_bits() as u128) | ((hi.to_bits() as u128) << 64)
}

/// HADDPD (sub=false) / HSUBPD (sub=true): horizontal add/sub of f64 pairs —
/// dest = [d0±d1, s0±s1]. Used in dot-product/reduction kernels (BLAS/numpy).
fn hadd_f64(d: u128, s: u128, sub: bool) -> u128 {
    let op = |x: f64, y: f64| if sub { x - y } else { x + y };
    pack_f64(
        op(f64_lane(d, 0), f64_lane(d, 1)),
        op(f64_lane(s, 0), f64_lane(s, 1)),
    )
}
/// HADDPS/HSUBPS: dest = [d0±d1, d2±d3, s0±s1, s2±s3] over f32 lanes.
fn hadd_f32(d: u128, s: u128, sub: bool) -> u128 {
    let op = |x: f32, y: f32| if sub { x - y } else { x + y };
    let l = [
        op(f32_lane(d, 0), f32_lane(d, 1)),
        op(f32_lane(d, 2), f32_lane(d, 3)),
        op(f32_lane(s, 0), f32_lane(s, 1)),
        op(f32_lane(s, 2), f32_lane(s, 3)),
    ];
    let mut out: u128 = 0;
    for (i, v) in l.iter().enumerate() {
        out |= (v.to_bits() as u128) << (i * 32);
    }
    out
}
/// ADDSUBPD: dest = [d0-s0, d1+s1] (subtract even lanes, add odd) — complex math.
fn addsub_f64(d: u128, s: u128) -> u128 {
    pack_f64(
        f64_lane(d, 0) - f64_lane(s, 0),
        f64_lane(d, 1) + f64_lane(s, 1),
    )
}
/// ADDSUBPS: even f32 lanes subtract, odd lanes add.
fn addsub_f32(d: u128, s: u128) -> u128 {
    let mut out: u128 = 0;
    for i in 0..4u32 {
        let x = f32_lane(d, i);
        let y = f32_lane(s, i);
        let r = if i % 2 == 0 { x - y } else { x + y };
        out |= (r.to_bits() as u128) << (i * 32);
    }
    out
}

/// Map a float comparison result to the (ZF, PF, CF) the x86
/// [U]COMISS/[U]COMISD instructions write. `None` (a NaN operand) is
/// the "unordered" case and sets all three. OF/SF are cleared by the
/// caller; AF we don't model.
/// 48-byte brand string returned across CPUID 0x80000002..4.
/// Linux reads this verbatim into `/proc/cpuinfo`'s `model name`.
const CPUID_BRAND: &[u8; 48] = b"wwwvm Rust software-only x86 CPU                ";

/// Feature-flag bitmap for CPUID leaf 1 EDX. We turn on the bits
/// that correspond to ISA we actually implement so the kernel takes
/// the fast paths (FXSAVE, SSE2 memcpy, SYSENTER, CMOV, etc.)
/// instead of i386 fallbacks.
///
/// PSE (bit 3) and PGE (bit 13) are advertised because the page
/// walker honors CR4.PSE + PDE.PS (4 MiB pages). PGE is a TLB-only
/// optimization: without a TLB it's functionally a no-op, but
/// advertising it lets Linux's optimized paging paths apply.
/// CX8 (bit 8) is the CMPXCHG8B opcode — the kernel checks this
/// before emitting per-CPU cmpxchg64 sequences. CLFLUSH (bit 19)
/// is implemented as a no-op since we don't model caches, but
/// Linux uses the CPUID bit to gate emitting CLFLUSH sequences
/// (e.g. for DMA buffer flushing) and reads EBX[15:8] for the
/// associated cache-line size.
///
/// APIC (bit 9) tells the kernel the on-chip Local APIC is present;
/// without it Linux ignores 0xFEE0_0000 entirely even when the MMIO
/// surface answers, falling back to PIC-only delivery. We have the
/// LAPIC (timer, SVR, EOI scratch, ICR scratch) wired, so this is
/// safe to advertise. MCE (bit 7) opts the kernel into Machine-
/// Check init — it reads MCG_CAP/MCG_STATUS (we return zeros) and
/// sets CR4.MCE; we never raise #MC so the post-init path is a
/// no-op. DE (bit 2) says Debug Extensions are present — CR4.DE
/// can be set; on real silicon it changes DR4/DR5 access to #UD,
/// which Linux relies on to detect old vs new debug semantics.
const CPUID_LEAF1_EDX: u32 = (1 << 0)        // FPU
        | (1 << 2)                                   // DE (CR4.DE settable)
        | (1 << 3)                                   // PSE (4 MiB pages)
        | (1 << 4)                                   // TSC
        | (1 << 5)                                   // MSR
        | (1 << 7)                                   // MCE (MCG_CAP/MCG_STATUS stubs)
        | (1 << 8)                                   // CX8 (CMPXCHG8B)
        | (1 << 9)                                   // APIC (on-chip LAPIC present)
        | (1 << 11)                                  // SEP (SYSENTER)
        | (1 << 13)                                  // PGE (no-op without TLB)
        | (1 << 15)                                  // CMOV
        | (1 << 19)                                  // CLFLUSH (no-op stub)
        | (1 << 24)                                  // FXSR
        | (1 << 25)                                  // SSE
        | (1 << 26); // SSE2

/// CPUID leaf 1 EBX. Layout per Intel SDM Vol. 2A:
///   31:24  initial APIC ID                 = 0 (single CPU)
///   23:16  max logical processors          = 1 (single-threaded VM)
///   15: 8  CLFLUSH line size / 8           = 8 (64-byte cache line)
///    7: 0  brand index                     = 0 (not used)
/// Linux uses bits 15:8 for kmalloc cache-line alignment and 31:24
/// for SMP CPU enumeration. Returning 0 made the kernel think the
/// cache line was 0 bytes — kmalloc would then pick odd alignments.
const CPUID_LEAF1_EBX: u32 = (1 << 16) | (8 << 8);

/// Write a 4-byte chunk of the brand string into a u32 register,
/// little-endian (the order Linux reconstructs the string in).
fn brand_dword(offset: usize) -> u32 {
    u32::from_le_bytes([
        CPUID_BRAND[offset],
        CPUID_BRAND[offset + 1],
        CPUID_BRAND[offset + 2],
        CPUID_BRAND[offset + 3],
    ])
}

/// CPUID dispatcher — small enough to keep here as a free fn so the
/// step() arm stays a single line. Inputs in EAX (leaf) — we don't
/// yet read ECX (sub-leaf) because every leaf we answer is
/// sub-leaf-independent.
fn cpuid_dispatch(cpu: &mut Cpu) {
    let leaf = cpu.read_r32(0);
    match leaf {
        // Standard leaves. Register indices used through this dispatch:
        // 0 = EAX, 1 = ECX, 2 = EDX, 3 = EBX — matches r32 encoding,
        // NOT the canonical "EAX/EBX/ECX/EDX" return-order Intel docs
        // list, so the writes below pair the index with the explicit
        // register name in the comment.
        0 => {
            cpu.write_r32(0, 2); // EAX = max basic leaf = 2 (cache descriptors)
                                 // Vendor string. We pretend to be "GenuineIntel" so
                                 // Linux's vendor-specific early init runs and populates
                                 // boot_cpu_data with full feature flags instead of the
                                 // conservative "unknown vendor" path which clears FPU
                                 // (and most caps) before fpu init runs. Layout per Intel
                                 // SDM: EBX = "Genu", EDX = "ineI", ECX = "ntel".
            cpu.write_r32(3, u32::from_le_bytes(*b"Genu")); // EBX = chars 0..3
            cpu.write_r32(2, u32::from_le_bytes(*b"ineI")); // EDX = chars 4..7
            cpu.write_r32(1, u32::from_le_bytes(*b"ntel")); // ECX = chars 8..11
        }
        1 => {
            // Skylake-class shape: family 6, extended model = 5,
            // base model = E (= model 0x5E = 94), stepping 3.
            //   bits 31:28  reserved
            //   bits 27:20  extended family (0)
            //   bits 19:16  extended model (5)
            //   bits 15:14  reserved
            //   bits 13:12  processor type (00 = original OEM)
            //   bits 11: 8  family (6)
            //   bits  7: 4  model (E)
            //   bits  3: 0  stepping (3)
            // Why not Pentium-Pro (family 6, model 6) like before?
            // Linux's init_intel applies an avalanche of cpu-cap-
            // clear quirks for the older Intel models — the
            // Pentium-Pro PSE errata, TSC unreliability bits, the
            // F00F-bug-friendly FPU clears, etc — and the net
            // effect was that boot_cpu_data.x86_capability[0]
            // ended up with FPU and most other bits CLEARED before
            // fpu init read it. Reporting Skylake skips all those
            // model-gated quirks. Family-6 keeps the kernel on its
            // modern probe paths rather than the antique-i386
            // branches.
            cpu.write_r32(0, 0x0005_06E3);
            cpu.write_r32(3, CPUID_LEAF1_EBX); // EBX = brand idx / cflush / max-logical / APIC ID
            cpu.write_r32(1, 0); // ECX (SSE3+) — we advertise none
            cpu.write_r32(2, CPUID_LEAF1_EDX); // EDX = FPU/PSE/TSC/MSR/CX8/APIC/SEP/CMOV/CLFLUSH/FXSR/SSE/SSE2 etc.
        }
        2 => {
            // Cache descriptor leaf. EAX bits 7:0 = "iterations
            // needed minus 1" (so 0x01 means "one call gives you
            // everything"). Remaining bytes are descriptor codes;
            // 0x00 means "no descriptor in this slot". Linux's
            // arch/x86/kernel/cpu/intel.c::intel_detect_cache reads
            // this leaf and tolerates an all-zero descriptor set —
            // it just falls through to the deterministic-cache
            // leaf (4) or to defaults.
            cpu.write_r32(0, 0x0000_0001); // EAX
            cpu.write_r32(3, 0); // EBX
            cpu.write_r32(1, 0); // ECX
            cpu.write_r32(2, 0); // EDX
        }
        // Extended leaves.
        0x8000_0000 => {
            // Max extended leaf supported = 0x80000008 (address widths).
            cpu.write_r32(0, 0x8000_0008); // EAX
            cpu.write_r32(3, 0); // EBX
            cpu.write_r32(1, 0); // ECX
            cpu.write_r32(2, 0); // EDX
        }
        0x8000_0001 => {
            // Extended feature flags — long-mode and 3DNow! both off.
            cpu.write_r32(0, 0); // EAX
            cpu.write_r32(3, 0); // EBX
            cpu.write_r32(1, 0); // ECX
            cpu.write_r32(2, 0); // EDX
        }
        // Brand string: 48 bytes split across three leaves of four
        // dwords each (EAX/EBX/ECX/EDX = positions 0/4/8/12).
        0x8000_0002..=0x8000_0004 => {
            let base = ((leaf - 0x8000_0002) as usize) * 16;
            cpu.write_r32(0, brand_dword(base)); // EAX = chars 0..3
            cpu.write_r32(3, brand_dword(base + 4)); // EBX = chars 4..7
            cpu.write_r32(1, brand_dword(base + 8)); // ECX = chars 8..11
            cpu.write_r32(2, brand_dword(base + 12)); // EDX = chars 12..15
        }
        // Leaves 0x80000005 (AMD L1 cache info) and 0x80000006 (L2
        // cache info) are zeros on Intel and AMD-on-no-cache-info.
        // Linux's parse_amd_topology gracefully handles all-zero.
        0x8000_0005..=0x8000_0007 => {
            cpu.write_r32(0, 0); // EAX
            cpu.write_r32(3, 0); // EBX
            cpu.write_r32(1, 0); // ECX
            cpu.write_r32(2, 0); // EDX
        }
        // 0x80000008 — virtual / physical address widths.
        //   EAX bits  7:0  = physical address bits   = 32
        //   EAX bits 15:8  = linear (virtual) bits   = 32
        //   EAX bits 23:16 = guest physical bits     = 0 (no nested)
        // Linux uses this to compute MAXPHYSADDR — the mask for
        // CR3 / PTE / MTRR base addresses. On 32-bit non-PAE,
        // 32-bit physical is exactly right.
        0x8000_0008 => {
            cpu.write_r32(0, 0x0000_2020); // EAX = phys/virt bits
            cpu.write_r32(3, 0); // EBX
            cpu.write_r32(1, 0); // ECX
            cpu.write_r32(2, 0); // EDX
        }
        _ => {
            cpu.write_r32(0, 0);
            cpu.write_r32(3, 0);
            cpu.write_r32(2, 0);
            cpu.write_r32(1, 0);
        }
    }
}

/// 32-bit port read decomposed into four byte reads at port..port+3,
/// the same shape PCI configuration's data port (0xCFC) expects.
fn port_read_u32(cpu: &mut Cpu, io: &mut IoBus, port: u16) -> u32 {
    let b0 = cpu.port_read(io, port) as u32;
    let b1 = cpu.port_read(io, port.wrapping_add(1)) as u32;
    let b2 = cpu.port_read(io, port.wrapping_add(2)) as u32;
    let b3 = cpu.port_read(io, port.wrapping_add(3)) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

/// 32-bit port write decomposed into four byte writes at port..port+3.
fn port_write_u32(cpu: &mut Cpu, io: &mut IoBus, port: u16, value: u32) {
    cpu.port_write(io, port, value as u8);
    cpu.port_write(io, port.wrapping_add(1), (value >> 8) as u8);
    cpu.port_write(io, port.wrapping_add(2), (value >> 16) as u8);
    cpu.port_write(io, port.wrapping_add(3), (value >> 24) as u8);
}

fn comis_flags(ord: Option<std::cmp::Ordering>) -> (bool, bool, bool) {
    use std::cmp::Ordering::*;
    match ord {
        None => (true, true, true),             // unordered (NaN)
        Some(Greater) => (false, false, false), // a > b
        Some(Less) => (false, false, true),     // a < b
        Some(Equal) => (true, false, false),    // a == b
    }
}

/// PUNPCKL/H interleave for SSE2: weave the low (`high=false`) or high
/// (`high=true`) half's `elem`-bit elements of `d` (destination) and `s`
/// (source) — `out[2i] = d[base+i]`, `out[2i+1] = s[base+i]`. `elem` is
/// one of 8/16/32/64; `base` is 0 for the low half, `n/2` for the high.
fn punpck(d: u128, s: u128, elem: u32, high: bool) -> u128 {
    let n = 128 / elem; // element count
    let base = if high { n / 2 } else { 0 };
    let mask: u128 = if elem >= 128 {
        u128::MAX
    } else {
        (1u128 << elem) - 1
    };
    let get = |v: u128, i: u32| (v >> (i * elem)) & mask;
    let mut out = 0u128;
    for i in 0..(n / 2) {
        out |= get(d, base + i) << ((2 * i) * elem);
        out |= get(s, base + i) << ((2 * i + 1) * elem);
    }
    out
}

/// x86 MIN/MAX lane rule for f64: `MIN` returns the source (`b`)
/// unless the destination (`a`) is strictly less; `MAX` returns `b`
/// unless `a` is strictly greater. This deliberately differs from
/// `f64::min/max` — a NaN operand or an equal compare yields `b`,
/// matching MINPD/MAXPD exactly.
fn fmin_max(a: f64, b: f64, is_min: bool) -> f64 {
    if is_min {
        if a < b {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}

/// Single-precision counterpart of [`fmin_max`] — MINPS/MAXPS.
fn fmin_max_f32(a: f32, b: f32, is_min: bool) -> f32 {
    if is_min {
        if a < b {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}

/// UNPCKLPS/UNPCKHPS lane interleave (32-bit lanes). Low picks lanes
/// 0,1; high picks lanes 2,3. The result alternates SRC1, SRC2:
/// `[s1[i], s2[i], s1[i+1], s2[i+1]]`.
fn unpck_ps(src1: u128, src2: u128, high: bool) -> u128 {
    let lane = |v: u128, i: u32| (v >> (i * 32)) & 0xFFFF_FFFF;
    let (i0, i1) = if high { (2, 3) } else { (0, 1) };
    lane(src1, i0) | (lane(src2, i0) << 32) | (lane(src1, i1) << 64) | (lane(src2, i1) << 96)
}

/// UNPCKLPD/UNPCKHPD lane interleave (64-bit lanes): `[s1[i], s2[i]]`
/// where i is 0 (low) or 1 (high).
fn unpck_pd(src1: u128, src2: u128, high: bool) -> u128 {
    let lane = |v: u128, i: u32| (v >> (i * 64)) & 0xFFFF_FFFF_FFFF_FFFF;
    let i = if high { 1 } else { 0 };
    lane(src1, i) | (lane(src2, i) << 64)
}

// SHLD/SHRD helpers — free fns to keep the dispatcher above readable.
// Each takes &mut Cpu so it can update flags + the destination, plus
// &mut Memory for the possible memory operand. count is already masked.

fn shld32(cpu: &mut Cpu, rm: Rm, reg: u8, count: u8, mem: &mut Memory) {
    if count == 0 {
        return;
    }
    let dest = cpu.read_rm32(rm, mem);
    let src = cpu.read_r32(reg);
    // Combine dest||src into 64-bit, shift left by count, take top 32.
    let combined = ((dest as u64) << 32) | (src as u64);
    let shifted = combined.wrapping_shl(count as u32);
    let result = (shifted >> 32) as u32;
    cpu.flags_logic32(result);
    // CF = last bit shifted out of the destination. Must be set AFTER
    // flags_logic, which unconditionally clears CF (and OF).
    let cf = (dest >> (32 - count)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.write_rm32(rm, mem, result);
}

fn shld16(cpu: &mut Cpu, rm: Rm, reg: u8, count: u8, mem: &mut Memory) {
    if count == 0 {
        return;
    }
    let dest = cpu.read_rm16(rm, mem);
    let src = cpu.read_r16(reg);
    let combined = ((dest as u32) << 16) | (src as u32);
    let shifted = combined.wrapping_shl(count as u32);
    let result = (shifted >> 16) as u16;
    cpu.flags_logic16(result);
    // CF = last bit shifted out (set after flags_logic, which clears it).
    let cf = (dest >> (16 - count)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.write_rm16(rm, mem, result);
}

fn shrd32(cpu: &mut Cpu, rm: Rm, reg: u8, count: u8, mem: &mut Memory) {
    if count == 0 {
        return;
    }
    let dest = cpu.read_rm32(rm, mem);
    let src = cpu.read_r32(reg);
    // src||dest, shift right by count, take low 32.
    let combined = ((src as u64) << 32) | (dest as u64);
    let shifted = combined.wrapping_shr(count as u32);
    let result = shifted as u32;
    cpu.flags_logic32(result);
    // CF = last bit shifted out (set after flags_logic, which clears it).
    let cf = (dest >> (count - 1)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.write_rm32(rm, mem, result);
}

fn shrd16(cpu: &mut Cpu, rm: Rm, reg: u8, count: u8, mem: &mut Memory) {
    if count == 0 {
        return;
    }
    let dest = cpu.read_rm16(rm, mem);
    let src = cpu.read_r16(reg);
    let combined = ((src as u32) << 16) | (dest as u32);
    let shifted = combined.wrapping_shr(count as u32);
    let result = shifted as u16;
    cpu.flags_logic16(result);
    // CF = last bit shifted out (set after flags_logic, which clears it).
    let cf = (dest >> (count - 1)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.write_rm16(rm, mem, result);
}

#[cfg(test)]
mod tests;
