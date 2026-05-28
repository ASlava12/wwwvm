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

use std::cell::Cell;
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
    /// Real x86 raises interrupt #0 (Divide Error) for div-by-zero or
    /// quotient overflow. We surface it as a CPU error so callers see
    /// what happened — future iterations may wire it to an IDT-based
    /// interrupt vector.
    #[error("divide error at {cs:04X}:{ip:08X}")]
    DivideError { cs: u16, ip: u32 },
}

/// Flags register bits we actually maintain.
pub mod flag {
    pub const CF: u16 = 1 << 0;
    pub const PF: u16 = 1 << 2;
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
    /// PSE, PAE, PGE, OSFXSR, etc.). We don't act on any of them yet;
    /// the value is just stored so a kernel can read/write it via
    /// `MOV CR4, r32` / `MOV r32, CR4` without faulting.
    pub cr4: u32,
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
    pub fpu_st: [f64; 8],
    /// x87 stack-top index (the architectural TOP field, 0..7).
    pub fpu_top: u8,
    /// SYSENTER MSRs (IA32_SYSENTER_CS/ESP/EIP = 0x174/0x175/0x176).
    /// SYSENTER loads CS:EIP and SS:ESP from these; SYSEXIT derives
    /// the return selectors from `sysenter_cs`. Written via WRMSR.
    pub sysenter_cs: u32,
    pub sysenter_esp: u32,
    pub sysenter_eip: u32,
    /// Set by `translate()` when a page walk hits a non-present
    /// entry. Read at the end of each `step()`; if set, the CPU
    /// dispatches INT 14 with the error code pushed on the stack,
    /// sets CR2 to the faulting address, and clears the slot.
    /// `Cell` so translate can flag a fault through `&self`.
    pending_fault: Cell<Option<PageFault>>,
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

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: [0; 8],
            regs_high: [0; 8],
            sregs: [0; 6],
            ip: 0,
            flags: 0,
            halted: false,
            seg_override: None,
            op_size_32: false,
            addr_size_32: false,
            stack_size_32: false,
            cr0: 0,
            gdtr: DescriptorTable::default(),
            idtr: DescriptorTable::default(),
            cr3: 0,
            cr2: 0,
            cr4: 0,
            tsc: 0,
            ldtr: 0,
            tr: 0,
            fpu_sw: 0,
            fpu_cw: 0x037F,
            fpu_st: [0.0; 8],
            fpu_top: 0,
            sysenter_cs: 0,
            sysenter_esp: 0,
            sysenter_eip: 0,
            pending_fault: Cell::new(None),
            a20: true,
            bios_hook: None,
            seg_cache: [SegmentCache::default(); 6],
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
        self.tsc = 0;
        self.ldtr = 0;
        self.tr = 0;
        self.fpu_sw = 0;
        self.fpu_cw = 0x037F;
        self.fpu_st = [0.0; 8];
        self.fpu_top = 0;
        self.sysenter_cs = 0;
        self.sysenter_esp = 0;
        self.sysenter_eip = 0;
        self.pending_fault.set(None);
        self.a20 = true;
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
    }

    /// PE-aware linear-address translation. In real mode the cache
    /// base is `sregs[idx] << 4` so this matches the legacy shift-by-4
    /// math. In PM the cache holds the descriptor's base directly, so
    /// CR0.PE=1 actually changes effective addresses for every memory
    /// access that routes through here.
    pub fn linear_seg(&self, seg_idx: usize, off: u32) -> u32 {
        self.seg_cache[seg_idx].base.wrapping_add(off)
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
    /// Not yet modelled: the R/W bit on the descriptor itself
    /// (protection violations vs. plain not-present), User/Supervisor
    /// (since we always run ring 0), and a TLB cache. 4 MiB pages
    /// (PSE) are also out of scope.
    ///
    /// Defaults to a read access; writes go through `translate_write`
    /// so the W bit in the #PF error code reflects the access type.
    pub fn translate(&self, mem: &Memory, linear: u32) -> u32 {
        self.translate_inner(mem, linear, false)
    }

    /// Same as `translate` but tags the resulting #PF (if any) with
    /// W=1 in the error code so the handler knows a write was the
    /// trigger. Used by `mem_write_u8/16/32`.
    pub fn translate_write(&self, mem: &Memory, linear: u32) -> u32 {
        self.translate_inner(mem, linear, true)
    }

    fn translate_inner(&self, mem: &Memory, linear: u32, write: bool) -> u32 {
        let phys = if self.cr0 & 0x8000_0000 == 0 {
            linear
        } else {
            let pd_index = (linear >> 22) & 0x3FF;
            let pt_index = (linear >> 12) & 0x3FF;
            let page_offset = linear & 0xFFF;
            let pd_base = self.cr3 & 0xFFFF_F000;
            let pde = mem.read_u32(pd_base.wrapping_add(pd_index * 4));
            let w_bit: u32 = if write { 0b10 } else { 0 };
            // Present bit (bit 0) clear -> #PF with P=0. W bit reflects
            // the access type. U/S stays zero (always supervisor for now).
            if pde & 1 == 0 {
                self.raise_fault(linear, w_bit);
                return 0;
            }
            let pt_base = pde & 0xFFFF_F000;
            let pte = mem.read_u32(pt_base.wrapping_add(pt_index * 4));
            if pte & 1 == 0 {
                self.raise_fault(linear, w_bit);
                return 0;
            }
            let frame = pte & 0xFFFF_F000;
            frame | page_offset
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
    pub fn mem_read_u8(&self, m: &Memory, linear: u32) -> u8 {
        m.read_u8(self.translate(m, linear))
    }

    pub fn mem_write_u8(&self, m: &mut Memory, linear: u32, value: u8) {
        let phys = self.translate_write(m, linear);
        m.write_u8(phys, value);
    }

    /// Read a 16-bit word at `linear`. We translate each byte
    /// independently so the rare case of a read that straddles a
    /// page boundary picks up the second byte from the right frame.
    pub fn mem_read_u16(&self, m: &Memory, linear: u32) -> u16 {
        let lo = self.mem_read_u8(m, linear) as u16;
        let hi = self.mem_read_u8(m, linear.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }

    pub fn mem_write_u16(&self, m: &mut Memory, linear: u32, value: u16) {
        self.mem_write_u8(m, linear, value as u8);
        self.mem_write_u8(m, linear.wrapping_add(1), (value >> 8) as u8);
    }

    pub fn mem_read_u32(&self, m: &Memory, linear: u32) -> u32 {
        let lo = self.mem_read_u16(m, linear) as u32;
        let hi = self.mem_read_u16(m, linear.wrapping_add(2)) as u32;
        lo | (hi << 16)
    }

    pub fn mem_write_u32(&self, m: &mut Memory, linear: u32, value: u32) {
        self.mem_write_u16(m, linear, value as u16);
        self.mem_write_u16(m, linear.wrapping_add(2), (value >> 16) as u16);
    }

    fn fetch_u8(&mut self, mem: &Memory) -> u8 {
        let addr = self.linear_seg(sreg::CS, self.ip);
        self.ip = self.ip.wrapping_add(1);
        self.mem_read_u8(mem, addr)
    }

    fn fetch_u16(&mut self, mem: &Memory) -> u16 {
        let lo = self.fetch_u8(mem) as u16;
        let hi = self.fetch_u8(mem) as u16;
        lo | (hi << 8)
    }

    /// Push a value onto the x87 stack: decrement TOP, write ST(0).
    fn fpu_push(&mut self, value: f64) {
        self.fpu_top = (self.fpu_top.wrapping_sub(1)) & 7;
        self.fpu_st[self.fpu_top as usize] = value;
    }

    /// Pop ST(0): read it, then increment TOP.
    fn fpu_pop(&mut self) -> f64 {
        let v = self.fpu_st[self.fpu_top as usize];
        self.fpu_top = (self.fpu_top.wrapping_add(1)) & 7;
        v
    }

    /// Read ST(i) without popping.
    fn fpu_st(&self, i: u8) -> f64 {
        self.fpu_st[((self.fpu_top + i) & 7) as usize]
    }

    /// Write ST(i).
    fn fpu_set_st(&mut self, i: u8, value: f64) {
        let idx = ((self.fpu_top + i) & 7) as usize;
        self.fpu_st[idx] = value;
    }

    /// Apply an x87 arithmetic sub-op (the 3-bit reg field of D8/DC/DE)
    /// to (a, b). FADD/FMUL are symmetric; FSUB/FDIV and their "reverse"
    /// variants differ in operand order. Returns None for the compare
    /// forms (FCOM/FCOMP), which don't produce a result.
    fn fpu_arith(op: u8, a: f64, b: f64) -> Option<f64> {
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

    /// Set the x87 condition-code bits C3/C2/C0 (status word bits
    /// 14/10/8) from a compare of `a` vs `b`, mirroring FCOM. Linux/
    /// glibc test these via FNSTSW + SAHF or `sahf; jcc`.
    fn fpu_compare(&mut self, a: f64, b: f64) {
        // Clear C3 (0x4000), C2 (0x0400), C0 (0x0100).
        self.fpu_sw &= !0x4500;
        if a.is_nan() || b.is_nan() {
            self.fpu_sw |= 0x4500; // unordered: C3=C2=C0=1
        } else if a > b {
            // all three already 0
        } else if a < b {
            self.fpu_sw |= 0x0100; // C0
        } else {
            self.fpu_sw |= 0x4000; // C3 (equal)
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
    }

    fn flags_add16(&mut self, a: u16, b: u16, cin: u16, result: u16) {
        let sum = a as u32 + b as u32 + cin as u32;
        self.set_flag(flag::CF, sum > 0xFFFF);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ result) & (b ^ result) & 0x8000) != 0);
    }

    fn flags_add32(&mut self, a: u32, b: u32, cin: u32, result: u32) {
        let sum = a as u64 + b as u64 + cin as u64;
        self.set_flag(flag::CF, sum > 0xFFFF_FFFF);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ result) & (b ^ result) & 0x8000_0000) != 0);
    }

    fn flags_sub8(&mut self, a: u8, b: u8, bin: u8, result: u8) {
        let borrow = (a as i16) - (b as i16) - (bin as i16);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x80 != 0);
        self.set_flag(flag::PF, (result.count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x80) != 0);
    }

    fn flags_sub16(&mut self, a: u16, b: u16, bin: u16, result: u16) {
        let borrow = (a as i32) - (b as i32) - (bin as i32);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x8000) != 0);
    }

    fn flags_sub32(&mut self, a: u32, b: u32, bin: u32, result: u32) {
        let borrow = (a as i64) - (b as i64) - (bin as i64);
        self.set_flag(flag::CF, borrow < 0);
        self.set_flag(flag::ZF, result == 0);
        self.set_flag(flag::SF, result & 0x8000_0000 != 0);
        self.set_flag(flag::PF, ((result as u8).count_ones() & 1) == 0);
        self.set_flag(flag::OF, ((a ^ b) & (a ^ result) & 0x8000_0000) != 0);
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

    /// Read 32-bit value through an `Rm`. Memory dword = two 16-bit
    /// reads at `off` and `off+2`.
    fn read_rm32(&self, rm: Rm, mem: &Memory) -> u32 {
        match rm {
            Rm::Reg(i) => self.read_r32(i),
            Rm::Mem(ea) => {
                let base = self.linear_seg(ea.seg, ea.off);
                let lo = self.mem_read_u16(mem, base) as u32;
                let hi = self.mem_read_u16(mem, base.wrapping_add(2)) as u32;
                lo | (hi << 16)
            }
        }
    }

    /// Write 32-bit value through an `Rm`. Memory dword = two 16-bit
    /// writes at `off` and `off+2`.
    fn write_rm32(&mut self, rm: Rm, mem: &mut Memory, value: u32) {
        match rm {
            Rm::Reg(i) => self.write_r32(i, value),
            Rm::Mem(ea) => {
                let base = self.linear_seg(ea.seg, ea.off);
                self.mem_write_u16(mem, base, value as u16);
                self.mem_write_u16(mem, base.wrapping_add(2), (value >> 16) as u16);
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
    /// IF is always cleared for the moment (we don't yet distinguish
    /// trap gates from interrupt gates — both end here). No privilege
    /// level change is modelled: a ring transition would require
    /// pushing the caller's SS/SP, which we'll add when ring 3 user
    /// code shows up.
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
        // In PE, the gate's type bits (low nibble of the access byte
        // at descriptor offset 5) pick the frame width:
        //   0x6 / 0x7 → 16-bit interrupt / trap gate
        //   0xE / 0xF → 32-bit interrupt / trap gate
        // Real-mode (PE=0) IVT is always the 16-bit 4-byte form.
        let (new_cs, new_ip, gate_is_32) = if self.cr0 & 1 == 0 {
            let ivt_addr = (n as u32) * 4;
            (
                self.mem_read_u16(mem, ivt_addr + 2),
                self.mem_read_u16(mem, ivt_addr) as u32,
                false,
            )
        } else {
            let gate_addr = self.idtr.base.wrapping_add((n as u32) * 8);
            let off_lo = self.mem_read_u16(mem, gate_addr) as u32;
            let selector = self.mem_read_u16(mem, gate_addr.wrapping_add(2));
            let access = self.mem_read_u8(mem, gate_addr.wrapping_add(5));
            let off_hi = self.mem_read_u16(mem, gate_addr.wrapping_add(6)) as u32;
            let is_32 = (access & 0x0F) >= 0x0E;
            let off = if is_32 {
                off_lo | (off_hi << 16)
            } else {
                off_lo
            };
            (selector, off, is_32)
        };
        let flags = self.flags;
        let cs = self.sregs[sreg::CS];
        if gate_is_32 {
            // 32-bit frame: push EFLAGS, CS (zero-extended to dword),
            // and the full 32-bit EIP. The error code (when present)
            // gets a dword push too.
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
        self.set_flag(flag::IF, false);
        // TF is not modeled yet — when it is, this is also where it
        // gets cleared.
        self.write_sreg(sreg::CS, new_cs, mem);
        self.ip = new_ip;
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
    /// decrements by 2 before the write. In 32-bit-stack mode the
    /// full ESP moves; in 16-bit-stack mode only the low SP moves.
    fn push16(&mut self, mem: &mut Memory, value: u16) {
        let sp = self.read_stack_ptr().wrapping_sub(2);
        self.write_stack_ptr(sp);
        self.mem_write_u16(mem, self.linear_seg(sreg::SS, sp), value);
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
    /// before the write.
    fn push32(&mut self, mem: &mut Memory, value: u32) {
        let sp = self.read_stack_ptr().wrapping_sub(4);
        self.write_stack_ptr(sp);
        self.mem_write_u32(mem, self.linear_seg(sreg::SS, sp), value);
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

    /// Execute a single instruction. Returns Ok(()) on success, or an
    /// error if the opcode/ModR/M form is not implemented.
    ///
    /// At the top we absorb any segment-override prefix bytes
    /// (0x26/0x2E/0x36/0x3E) into `self.seg_override`. They affect
    /// only the current instruction; a fresh `step()` always clears
    /// the override first.
    pub fn step(&mut self, mem: &mut Memory, io: &mut IoBus) -> Result<(), CpuError> {
        if self.halted {
            return Ok(());
        }
        self.tsc = self.tsc.wrapping_add(1);
        // A page fault flagged by the previous instruction's memory
        // accesses takes priority over fresh work. Latch the linear
        // address into CR2 and vector through INT 14, pushing the
        // architectural error code below the IP/CS/FLAGS frame.
        if let Some(pf) = self.take_pending_fault() {
            self.cr2 = pf.addr;
            self.do_interrupt_with_error(14, Some(pf.error_code), mem);
            return Ok(());
        }
        // External interrupt delivery — must come *before* fetch so an
        // unmasked IRQ runs its handler at the next instruction boundary
        // instead of one boundary late. Refresh first so devices that
        // assert their line (e.g. UART with rx data and IER set) get
        // latched into the PIC's IRR for this turn.
        io.refresh_irqs();
        if self.has(flag::IF) {
            if let Some(vec) = io.pending_irq_vector() {
                io.ack_irq();
                self.do_interrupt(vec, mem);
                return Ok(());
            }
        }
        self.seg_override = None;
        self.op_size_32 = false;
        self.addr_size_32 = false;
        let op_cs = self.sregs[sreg::CS];
        let op_ip = self.ip;
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
                0x66 => self.op_size_32 = true,
                // 0x67 — address-size override. Flips the ModR/M
                // address decode from 16-bit to 32-bit (and SIB).
                0x67 => self.addr_size_32 = true,
                _ => break b,
            }
        };

        match opcode {
            0x90 => { /* NOP */ }
            0xF4 => {
                self.halted = true;
            }
            0xFA => {
                self.set_flag(flag::IF, false);
            }
            0xFB => {
                self.set_flag(flag::IF, true);
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
                let cx = self.regs[r16::CX].wrapping_sub(1);
                self.regs[r16::CX] = cx;
                let cond = match opcode {
                    0xE2 => true,
                    0xE1 => self.has(flag::ZF),
                    0xE0 => !self.has(flag::ZF),
                    _ => unreachable!(),
                };
                if cx != 0 && cond {
                    self.ip = self.ip.wrapping_add(rel as i32 as u32);
                }
            }

            // JCXZ rel8 — branch if CX == 0. CX is NOT decremented;
            // this is the idiomatic guard before a LOOP that would
            // otherwise iterate 65536 times when CX starts at 0.
            0xE3 => {
                let rel = self.fetch_u8(mem) as i8;
                if self.regs[r16::CX] == 0 {
                    self.ip = self.ip.wrapping_add(rel as i32 as u32);
                }
            }

            // Single-shot string ops. REP-prefixed paths go through the
            // 0xF2/0xF3 handler below.
            0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE | 0xAF => {
                self.step_string(opcode, mem);
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
                        0x66 => self.op_size_32 = true,
                        0x67 => self.addr_size_32 = true,
                        _ => break b,
                    }
                };
                // PAUSE = F3 90. The 0xF3 prefix on a NOP is the
                // spin-loop hint, *not* a REP NOP — spinlocks emit it
                // constantly. Treat it as a no-op and stop here rather
                // than falling into the string-op loop (which would
                // reject 0x90).
                if inner == 0x90 {
                    // PAUSE / REP NOP — nothing to do.
                } else {
                    let conditional = matches!(inner, 0xA6 | 0xA7 | 0xAE | 0xAF);
                    loop {
                        let counter_done = if self.addr_size_32 {
                            self.read_r32(r16::CX as u8) == 0
                        } else {
                            self.regs[r16::CX] == 0
                        };
                        if counter_done {
                            break;
                        }
                        if !self.step_string(inner, mem) {
                            return Err(CpuError::Unimplemented {
                                opcode: inner,
                                cs: op_cs,
                                ip: op_ip,
                            });
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
            // a future iteration may reject it like real x86.
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
                self.write_sreg(sreg_idx as usize, v, mem);
            }

            // XCHG AX, r16 — short form. 0x90 (XCHG AX, AX) is NOP and
            // is handled by the dedicated NOP arm above.
            0x91..=0x97 => {
                let i = (opcode - 0x90) as usize;
                let ax = self.regs[r16::AX];
                let other = self.regs[i];
                self.regs[r16::AX] = other;
                self.regs[i] = ax;
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
                            return Err(CpuError::DivideError {
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        let ax = self.regs[r16::AX];
                        let q = ax / v as u16;
                        let r = ax % v as u16;
                        if q > 0xFF {
                            return Err(CpuError::DivideError {
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        self.write_r8(0, q as u8);
                        self.write_r8(4, r as u8); // AH
                    }
                    7 => {
                        // IDIV r/m8 — signed division of AX by r/m8
                        let v = self.read_rm8(rm, mem) as i8 as i16;
                        if v == 0 {
                            return Err(CpuError::DivideError {
                                cs: op_cs,
                                ip: op_ip,
                            });
                        }
                        let ax = self.regs[r16::AX] as i16;
                        let q = ax / v;
                        let r = ax % v;
                        if !(-128..=127).contains(&q) {
                            return Err(CpuError::DivideError {
                                cs: op_cs,
                                ip: op_ip,
                            });
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
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            let dividend =
                                ((self.read_r32(2) as u64) << 32) | self.read_r32(0) as u64;
                            let q = dividend / v;
                            let r = dividend % v;
                            if q > 0xFFFF_FFFF {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            self.write_r32(0, q as u32);
                            self.write_r32(2, r as u32);
                        }
                        7 => {
                            // IDIV r/m32 — signed division of EDX:EAX by r/m32
                            let v = self.read_rm32(rm, mem) as i32 as i64;
                            if v == 0 {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            let dividend = (((self.read_r32(2) as u64) << 32)
                                | self.read_r32(0) as u64)
                                as i64;
                            let q = dividend / v;
                            let r = dividend % v;
                            if !(i32::MIN as i64..=i32::MAX as i64).contains(&q) {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
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
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            let dividend =
                                ((self.regs[r16::DX] as u32) << 16) | self.regs[r16::AX] as u32;
                            let q = dividend / v;
                            let r = dividend % v;
                            if q > 0xFFFF {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            self.regs[r16::AX] = q as u16;
                            self.regs[r16::DX] = r as u16;
                        }
                        7 => {
                            // IDIV r/m16 — signed division of DX:AX by r/m16
                            let v = self.read_rm16(rm, mem) as i16 as i32;
                            if v == 0 {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
                            }
                            let dividend = (((self.regs[r16::DX] as u32) << 16)
                                | self.regs[r16::AX] as u32)
                                as i32;
                            let q = dividend / v;
                            let r = dividend % v;
                            if !(i16::MIN as i32..=i16::MAX as i32).contains(&q) {
                                return Err(CpuError::DivideError {
                                    cs: op_cs,
                                    ip: op_ip,
                                });
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
                    // CALL m16:16 — far indirect. The operand must be
                    // memory (a 4-byte pointer). We re-fetch the linear
                    // base from the resolved Rm::Mem so both words come
                    // from the same segment + base address.
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
                        let new_ip = self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off));
                        let new_cs =
                            self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                        let cs = self.sregs[sreg::CS];
                        self.push16(mem, cs);
                        let ip = self.ip as u16;
                        self.push16(mem, ip);
                        self.write_sreg(sreg::CS, new_cs, mem);
                        self.ip = new_ip as u32;
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
                    // JMP m16:16 — far indirect (no stack activity).
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
                        let new_ip = self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off));
                        let new_cs =
                            self.mem_read_u16(mem, self.linear_seg(ea.seg, ea.off.wrapping_add(2)));
                        self.write_sreg(sreg::CS, new_cs, mem);
                        self.ip = new_ip as u32;
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
            0x06 => {
                let v = self.sregs[sreg::ES];
                self.push16(mem, v);
            }
            0x0E => {
                let v = self.sregs[sreg::CS];
                self.push16(mem, v);
            }
            0x16 => {
                let v = self.sregs[sreg::SS];
                self.push16(mem, v);
            }
            0x1E => {
                let v = self.sregs[sreg::DS];
                self.push16(mem, v);
            }
            0x07 => {
                let v = self.pop16(mem);
                self.write_sreg(sreg::ES, v, mem);
            }
            0x17 => {
                let v = self.pop16(mem);
                self.write_sreg(sreg::SS, v, mem);
            }
            0x1F => {
                let v = self.pop16(mem);
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
                self.push16(mem, cs);
                // 16-bit gate convention: only low 16 of return IP go
                // on the stack. A 32-bit-gate path (when we add it)
                // pushes the full dword.
                let ip = self.ip as u16;
                self.push16(mem, ip);
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
                let extra = self.fetch_u16(mem);
                self.ip = if self.op_size_32 {
                    self.pop32(mem)
                } else {
                    self.pop16(mem) as u32
                };
                self.regs[r16::SP] = self.regs[r16::SP].wrapping_add(extra);
            }
            // RETF — pop IP then CS (far return).
            0xCB => {
                self.ip = self.pop16(mem) as u32;
                let cs = self.pop16(mem);
                self.write_sreg(sreg::CS, cs, mem);
            }
            // RETF imm16 — far return with callee-side stack cleanup.
            0xCA => {
                let extra = self.fetch_u16(mem);
                self.ip = self.pop16(mem) as u32;
                let cs = self.pop16(mem);
                self.write_sreg(sreg::CS, cs, mem);
                self.regs[r16::SP] = self.regs[r16::SP].wrapping_add(extra);
            }

            // PUSHF / PUSHFD — push FLAGS / EFLAGS. Under 0x66 the
            // pushed value widens to a dword; we only model the low
            // 16 bits of EFLAGS, so the high half pushes as zero.
            0x9C => {
                if self.op_size_32 {
                    self.push32(mem, self.flags as u32);
                } else {
                    self.push16(mem, self.flags);
                }
            }
            // POPF / POPFD — pop FLAGS / EFLAGS.
            0x9D => {
                if self.op_size_32 {
                    let eflags = self.pop32(mem);
                    self.flags = eflags as u16;
                } else {
                    self.flags = self.pop16(mem);
                }
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
            0xCC => {
                self.do_interrupt(3, mem);
            }
            // INT imm8 — software interrupt to the vector named by imm8.
            // The bios_hook gets first refusal: a Rust-side handler for
            // BIOS vectors (0x10 video, 0x13 disk, 0x16 keyboard, etc.)
            // returns true and the CPU treats the INT as "done" without
            // pushing a frame. Anything not claimed by the hook falls
            // through to the standard IVT/IDT dispatch.
            0xCD => {
                let n = self.fetch_u8(mem);
                if let Some(hook) = self.bios_hook {
                    if hook(self, mem, io, n) {
                        // Host handled it — no frame, no IRET needed.
                    } else {
                        self.do_interrupt(n, mem);
                    }
                } else {
                    self.do_interrupt(n, mem);
                }
            }
            // INTO — if OF=1, raise INT 4. Otherwise a no-op.
            0xCE => {
                if self.has(flag::OF) {
                    self.do_interrupt(4, mem);
                }
            }
            // IRET — pop EIP, CS, EFLAGS (in that order). Under the
            // 0x66 prefix (IRETD), pops a dword frame; otherwise a
            // word frame. The IF/TF state before the original INT is
            // restored as part of FLAGS.
            0xCF => {
                if self.op_size_32 {
                    self.ip = self.pop32(mem);
                    let cs = self.pop32(mem) as u16;
                    self.write_sreg(sreg::CS, cs, mem);
                    // EFLAGS pop — only the low 16 are modelled; the
                    // upper half (resume / V86 / AC bits) reads back
                    // but isn't acted upon yet.
                    let eflags = self.pop32(mem);
                    self.flags = eflags as u16;
                } else {
                    self.ip = self.pop16(mem) as u32;
                    let cs = self.pop16(mem);
                    self.write_sreg(sreg::CS, cs, mem);
                    self.flags = self.pop16(mem);
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

            0xEC => {
                // IN AL, DX
                let port = self.regs[r16::DX];
                let v = self.port_read(io, port);
                self.write_r8(0, v);
            }
            0xEE => {
                // OUT DX, AL
                let port = self.regs[r16::DX];
                let v = self.read_r8(0);
                self.port_write(io, port, v);
            }
            0xE4 => {
                // IN AL, imm8
                let port = self.fetch_u8(mem) as u16;
                let v = self.port_read(io, port);
                self.write_r8(0, v);
            }
            0xE5 => {
                // IN AX, imm8 — two byte reads from consecutive ports
                let port = self.fetch_u8(mem) as u16;
                let lo = self.port_read(io, port) as u16;
                let hi = self.port_read(io, port.wrapping_add(1)) as u16;
                self.regs[r16::AX] = lo | (hi << 8);
            }
            0xE6 => {
                // OUT imm8, AL
                let port = self.fetch_u8(mem) as u16;
                let v = self.read_r8(0);
                self.port_write(io, port, v);
            }
            0xE7 => {
                // OUT imm8, AX — two byte writes to consecutive ports
                let port = self.fetch_u8(mem) as u16;
                let ax = self.regs[r16::AX];
                self.port_write(io, port, ax as u8);
                self.port_write(io, port.wrapping_add(1), (ax >> 8) as u8);
            }
            0xED => {
                // IN AX, DX — 16-bit port read via DX
                let port = self.regs[r16::DX];
                let lo = self.port_read(io, port) as u16;
                let hi = self.port_read(io, port.wrapping_add(1)) as u16;
                self.regs[r16::AX] = lo | (hi << 8);
            }
            0xEF => {
                // OUT DX, AX — 16-bit port write via DX
                let port = self.regs[r16::DX];
                let ax = self.regs[r16::AX];
                self.port_write(io, port, ax as u8);
                self.port_write(io, port.wrapping_add(1), (ax >> 8) as u8);
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
                    return Err(CpuError::DivideError {
                        cs: op_cs,
                        ip: op_ip,
                    });
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
                    // Group 6 — SLDT (/0) STR (/1) LLDT (/2) LTR (/3).
                    // Each operates on a 16-bit selector in r/m16.
                    // VERR/VERW (/4/5) are not implemented yet.
                    0x00 => {
                        let (_, sub, rm) = self.fetch_modrm(mem);
                        match sub {
                            0 => {
                                let v = self.ldtr;
                                self.write_rm16(rm, mem, v);
                            }
                            1 => {
                                let v = self.tr;
                                self.write_rm16(rm, mem, v);
                            }
                            2 => self.ldtr = self.read_rm16(rm, mem),
                            3 => self.tr = self.read_rm16(rm, mem),
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
                                let v = self.cr0 as u16;
                                self.write_rm16(rm, mem, v);
                                return Ok(());
                            }
                            6 => {
                                let v = self.read_rm16(rm, mem);
                                self.cr0 = (self.cr0 & !0xF) | (v as u32 & 0xF);
                                return Ok(());
                            }
                            _ => {}
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
                            // LGDT / LIDT — load pseudo-descriptor.
                            2 | 3 => {
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
                            // INVLPG m — invalidate TLB entry.
                            7 => {
                                let _ = self.linear_seg(ea.seg, ea.off);
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
                    // MOV r32, CRn — 0x0F 0x20 /reg. CR0/CR2/CR3 routed
                    // through the full 32-bit GPR (write_r32) so the
                    // upper half of each control register survives.
                    // The #PF handler reads CR2 here to learn which
                    // linear address it must page in.
                    0x20 => {
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
                    // MOV CRn, r32 — 0x0F 0x22 /reg.
                    0x22 => {
                        let modrm = self.fetch_u8(mem);
                        let reg = (modrm >> 3) & 0x07;
                        let rm = modrm & 0x07;
                        let value = self.read_r32(rm);
                        match reg {
                            0 => self.cr0 = value,
                            2 => self.cr2 = value,
                            3 => self.cr3 = value,
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
                                    // FXSAVE m512 — write 512 bytes
                                    // of zeros. We don't model an FPU
                                    // or SSE register file, so an all-
                                    // zero save image is the truth.
                                    for off in 0..512u32 {
                                        self.mem_write_u8(mem, addr.wrapping_add(off), 0);
                                    }
                                }
                                1 => {
                                    // FXRSTOR m512 — read but discard
                                    // 512 bytes. Touch the memory so
                                    // an unmapped page faults the way
                                    // a real FXRSTOR would.
                                    for off in 0..512u32 {
                                        let _ = self.mem_read_u8(mem, addr.wrapping_add(off));
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
                    0x06 => self.cr0 &= !(1 << 3),
                    // INVD — 0x0F 0x08. Invalidate internal caches
                    // without write-back. We don't model caches at
                    // all, so it's a no-op.
                    0x08 => {}
                    // WBINVD — 0x0F 0x09. Same with write-back.
                    0x09 => {}
                    // RDTSC — 0x0F 0x31. Returns the time-stamp
                    // counter (low 32 bits in EAX, high 32 in EDX).
                    // We use the step counter — monotonically
                    // advancing, which is what calibration loops want.
                    0x31 => {
                        self.write_r32(0, self.tsc as u32);
                        self.write_r32(2, (self.tsc >> 32) as u32);
                    }
                    // RDMSR — 0x0F 0x32. Reads MSR named by ECX into
                    // EDX:EAX. We return 0 for everything; Linux setup
                    // treats this as "feature disabled" rather than
                    // probing further, which is what we want.
                    0x32 => {
                        let msr = self.read_r32(1); // ECX
                        let value: u64 = match msr {
                            0x10 => self.tsc,                  // IA32_TSC
                            0x1B => 0xFEE0_0000,               // IA32_APIC_BASE
                            0x174 => self.sysenter_cs as u64,  // IA32_SYSENTER_CS
                            0x175 => self.sysenter_esp as u64, // IA32_SYSENTER_ESP
                            0x176 => self.sysenter_eip as u64, // IA32_SYSENTER_EIP
                            _ => 0,
                        };
                        self.write_r32(0, value as u32);
                        self.write_r32(2, (value >> 32) as u32);
                    }
                    // WRMSR — 0x0F 0x30. Write MSR named by ECX from
                    // EDX:EAX. We persist the SYSENTER MSRs (the only
                    // ones with read-back semantics SYSENTER relies on);
                    // all others are dropped.
                    0x30 => {
                        let msr = self.read_r32(1); // ECX
                        let lo = self.read_r32(0); // EAX
                        match msr {
                            0x174 => self.sysenter_cs = lo,
                            0x175 => self.sysenter_esp = lo,
                            0x176 => self.sysenter_eip = lo,
                            _ => {}
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
                    0x35 => {
                        let cs_sel = ((self.sysenter_cs & 0xFFFC).wrapping_add(16)) as u16;
                        let ss_sel = cs_sel.wrapping_add(8);
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
                    // Simplified: we ignore the "address advances
                    // by bit/8 on memory operand" semantics — the
                    // bit index is taken modulo operand width, and
                    // we operate on the existing r/m word/dword.
                    // That matches every Linux set_bit / test_bit
                    // call where the bitmap word index is computed
                    // before the instruction.
                    0xA3 | 0xAB | 0xB3 | 0xBB => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let v = self.read_rm32(rm, mem);
                            let bit = self.read_r32(reg) & 31;
                            let mask = 1u32 << bit;
                            self.set_flag(flag::CF, v & mask != 0);
                            let new = match op2 {
                                0xA3 => v,         // BT — no write
                                0xAB => v | mask,  // BTS
                                0xB3 => v & !mask, // BTR
                                0xBB => v ^ mask,  // BTC
                                _ => unreachable!(),
                            };
                            if op2 != 0xA3 {
                                self.write_rm32(rm, mem, new);
                            }
                        } else {
                            let v = self.read_rm16(rm, mem);
                            let bit = self.read_r16(reg) & 15;
                            let mask = 1u16 << bit;
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
                    0xA0 => {
                        let v = self.sregs[sreg::FS];
                        self.push16(mem, v);
                    }
                    0xA1 => {
                        let v = self.pop16(mem);
                        self.write_sreg(sreg::FS, v, mem);
                    }
                    0xA8 => {
                        let v = self.sregs[sreg::GS];
                        self.push16(mem, v);
                    }
                    0xA9 => {
                        let v = self.pop16(mem);
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
                    // EAX/EBX/ECX/EDX. We respond to the two leaves
                    // Linux setup looks at first:
                    //   leaf 0: max-leaf in EAX, vendor string in EBX|EDX|ECX.
                    //           We report leaf 1 supported and pose as
                    //           "WWWVMxRust  " — close enough to satisfy
                    //           any "is this Intel/AMD" sniffing without
                    //           accidentally triggering vendor-specific
                    //           workarounds. (12 ASCII bytes.)
                    //   leaf 1: family/model/stepping in EAX, feature
                    //           flags in EDX/ECX. We report a bare i386
                    //           with FPU=0, PSE=0, PAE=0, no SSE — the
                    //           kernel may refuse if features look too
                    //           lean, but at least it'll know what it's
                    //           dealing with.
                    // Anything else returns zeros.
                    0xA2 => {
                        let leaf = self.read_r32(0); // EAX
                        match leaf {
                            0 => {
                                self.write_r32(0, 1); // max basic leaf = 1
                                                      // "WWWVMxRust  " = 12 bytes in EBX, EDX, ECX
                                self.write_r32(3, u32::from_le_bytes(*b"WWWV")); // EBX
                                self.write_r32(2, u32::from_le_bytes(*b"st  ")); // ECX
                                self.write_r32(1, u32::from_le_bytes(*b"MxRu"));
                                // EDX
                            }
                            1 => {
                                // Family 3 (i386), no model, stepping 0.
                                self.write_r32(0, 0x0000_0300);
                                self.write_r32(3, 0); // EBX
                                self.write_r32(2, 0); // ECX (SSE3+)
                                self.write_r32(1, 0); // EDX (FPU/PSE/etc all 0)
                            }
                            _ => {
                                self.write_r32(0, 0);
                                self.write_r32(3, 0);
                                self.write_r32(2, 0);
                                self.write_r32(1, 0);
                            }
                        }
                    }

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
                    // MOVZX r32, r/m16 — 0x0F 0xB7. Zero-extends a
                    // word into a dword. (16-bit dest with 0x66 would
                    // be a no-op MOV; we treat reg as r32 always.)
                    0xB7 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm16(rm, mem);
                        self.write_r32(reg, v as u32);
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
                    // MOVSX r32, r/m16 — 0x0F 0xBF.
                    0xBF => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let v = self.read_rm16(rm, mem) as i16;
                        self.write_r32(reg, v as i32 as u32);
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
                    // add: dest, src = dest + src, dest (in that order
                    // — the src register receives the old dest value).
                    // Used by Linux atomic_add_return and refcount_inc.
                    0xC0 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        let dest = self.read_rm8(rm, mem);
                        let src = self.read_r8(reg);
                        let sum = dest.wrapping_add(src);
                        self.flags_add8(dest, src, 0, sum);
                        self.write_rm8(rm, mem, sum);
                        self.write_r8(reg, dest);
                    }
                    // XADD r/m16/32, r16/32 — 0x0F 0xC1.
                    0xC1 => {
                        let (_, reg, rm) = self.fetch_modrm(mem);
                        if self.op_size_32 {
                            let dest = self.read_rm32(rm, mem);
                            let src = self.read_r32(reg);
                            let sum = dest.wrapping_add(src);
                            self.flags_add32(dest, src, 0, sum);
                            self.write_rm32(rm, mem, sum);
                            self.write_r32(reg, dest);
                        } else {
                            let dest = self.read_rm16(rm, mem);
                            let src = self.read_r16(reg);
                            let sum = dest.wrapping_add(src);
                            self.flags_add16(dest, src, 0, sum);
                            self.write_rm16(rm, mem, sum);
                            self.write_r16(reg, dest);
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
                            // FNINIT — reset SW to 0, CW to 0x037F.
                            self.fpu_sw = 0;
                            self.fpu_cw = 0x037F;
                        }
                        0xE2 => {
                            // FNCLEX — clear exception flags (SW bits 0..7).
                            self.fpu_sw &= !0x00FF;
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
                            self.fpu_push(v as f64);
                        }
                        // FISTP m32 — pop and store as truncated i32.
                        // The `(int)double` conversion.
                        3 => {
                            let v = self.fpu_pop();
                            self.mem_write_u32(mem, addr, (v as i32) as u32);
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
                if modrm == 0xE0 {
                    // FNSTSW AX — copy FPU status into AX.
                    self.regs[r16::AX] = self.fpu_sw;
                } else {
                    return Err(CpuError::Unimplemented {
                        opcode,
                        cs: op_cs,
                        ip: op_ip,
                    });
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
                        0xE8 => self.fpu_push(1.0),                       // FLD1
                        0xE9 => self.fpu_push(std::f64::consts::LOG2_10), // FLDL2T
                        0xEA => self.fpu_push(std::f64::consts::LOG2_E),  // FLDL2E
                        0xEB => self.fpu_push(std::f64::consts::PI),      // FLDPI
                        0xEC => self.fpu_push(std::f64::consts::LOG10_2), // FLDLG2
                        0xED => self.fpu_push(std::f64::consts::LN_2),    // FLDLN2
                        0xEE => self.fpu_push(0.0),                       // FLDZ
                        // FCHS — negate ST(0).
                        0xE0 => self.fpu_set_st(0, -self.fpu_st(0)),
                        // FABS — absolute value of ST(0).
                        0xE1 => self.fpu_set_st(0, self.fpu_st(0).abs()),
                        // FTST — compare ST(0) with 0.0.
                        0xE4 => {
                            let st0 = self.fpu_st(0);
                            self.fpu_compare(st0, 0.0);
                        }
                        // FSQRT — square root of ST(0).
                        0xFA => self.fpu_set_st(0, self.fpu_st(0).sqrt()),
                        // FRNDINT — round ST(0) to integer (nearest-even,
                        // the default rounding mode; we don't model the
                        // RC field yet).
                        0xFC => {
                            let v = self.fpu_st(0);
                            self.fpu_set_st(0, v.round_ties_even());
                        }
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
                            self.fpu_push(f32::from_bits(bits) as f64);
                        }
                        // FST m32 — store ST(0) as f32 (no pop).
                        2 => {
                            let v = self.fpu_st(0) as f32;
                            self.mem_write_u32(mem, addr, v.to_bits());
                        }
                        // FSTP m32 — store ST(0) as f32 and pop.
                        3 => {
                            let v = self.fpu_pop() as f32;
                            self.mem_write_u32(mem, addr, v.to_bits());
                        }
                        // FLDCW m16 — load control word.
                        5 => self.fpu_cw = self.mem_read_u16(mem, addr),
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
                    f32::from_bits(self.mem_read_u32(mem, addr)) as f64
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
                    // DC C0+i: ST(i) = ST(i) op ST(0).
                    let i = modrm & 0x07;
                    let dst = self.fpu_st(i);
                    let st0 = self.fpu_st(0);
                    if let Some(r) = Self::fpu_arith(op, dst, st0) {
                        self.fpu_set_st(i, r);
                    }
                } else {
                    let ea = if self.addr_size_32 {
                        self.compute_ea_32(mode, modrm & 0x07, mem)
                    } else {
                        self.compute_ea(mode, modrm & 0x07, mem)
                    };
                    let addr = self.linear_seg(ea.seg, ea.off);
                    let src = f64::from_bits(
                        self.mem_read_u32(mem, addr) as u64
                            | ((self.mem_read_u32(mem, addr.wrapping_add(4)) as u64) << 32),
                    );
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
                            self.fpu_push(f64::from_bits(lo | (hi << 32)));
                        }
                        2 | 3 => {
                            // FST/FSTP m64.
                            let v = self.fpu_st(0).to_bits();
                            self.mem_write_u32(mem, addr, v as u32);
                            self.mem_write_u32(mem, addr.wrapping_add(4), (v >> 32) as u32);
                            if sub == 3 {
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
    let cf = (dest >> (32 - count)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.flags_logic32(result);
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
    let cf = (dest >> (16 - count)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.flags_logic16(result);
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
    let cf = (dest >> (count - 1)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.flags_logic32(result);
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
    let cf = (dest >> (count - 1)) & 1 != 0;
    cpu.set_flag(flag::CF, cf);
    cpu.flags_logic16(result);
    cpu.write_rm16(rm, mem, result);
}

#[cfg(test)]
mod tests;
