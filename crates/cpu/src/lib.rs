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

use thiserror::Error;
use wwwvm_devices::IoBus;
use wwwvm_mem::Memory;

#[derive(Debug, Error)]
pub enum CpuError {
    #[error("unimplemented opcode 0x{opcode:02X} at {cs:04X}:{ip:04X}")]
    Unimplemented { opcode: u8, cs: u16, ip: u16 },
    #[error("unimplemented ModR/M mode {mode} (opcode 0x{opcode:02X} at {cs:04X}:{ip:04X})")]
    UnimplementedModRm { opcode: u8, mode: u8, cs: u16, ip: u16 },
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

pub struct Cpu {
    pub regs: [u16; 8],
    pub sregs: [u16; 6],
    pub ip: u16,
    pub flags: u16,
    pub halted: bool,
}

/// Decoded 16-bit effective address: linear = (sregs[seg] << 4) + off.
#[derive(Copy, Clone, Debug)]
pub struct EffAddr {
    pub seg: usize,
    pub off: u16,
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
            sregs: [0; 6],
            ip: 0,
            flags: 0,
            halted: false,
        }
    }

    /// Reset to a sensible boot state: CS:IP = 0000:7C00 (where BIOS
    /// loads the first sector), stack at the bottom of conventional
    /// memory, all data segments = 0.
    pub fn reset_to_boot(&mut self) {
        self.regs = [0; 8];
        self.sregs = [0; 6];
        self.regs[r16::SP] = 0x7C00;
        self.ip = 0x7C00;
        self.flags = 0;
        self.halted = false;
    }

    pub fn read_r8(&self, i: u8) -> u8 {
        let idx = (i & 3) as usize;
        let high = i >= 4;
        let word = self.regs[idx];
        if high { (word >> 8) as u8 } else { word as u8 }
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

    fn linear(seg: u16, off: u16) -> u32 {
        ((seg as u32) << 4).wrapping_add(off as u32)
    }

    fn fetch_u8(&mut self, mem: &Memory) -> u8 {
        let addr = Self::linear(self.sregs[sreg::CS], self.ip);
        self.ip = self.ip.wrapping_add(1);
        mem.read_u8(addr)
    }

    fn fetch_u16(&mut self, mem: &Memory) -> u16 {
        let lo = self.fetch_u8(mem) as u16;
        let hi = self.fetch_u8(mem) as u16;
        lo | (hi << 8)
    }

    fn set_flag(&mut self, mask: u16, value: bool) {
        if value { self.flags |= mask; } else { self.flags &= !mask; }
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

    /// Decode a 16-bit ModR/M effective address. `mode` must be 0b00,
    /// 0b01 or 0b10 — the 0b11 case is "register, not memory" and the
    /// caller dispatches it separately. Advances IP past any disp.
    fn compute_ea(&mut self, mode: u8, rm: u8, mem: &Memory) -> EffAddr {
        // Special slot: mode=00, rm=110 means [disp16] — no register
        // operand at all. Default segment DS.
        if mode == 0b00 && rm == 0b110 {
            let off = self.fetch_u16(mem);
            return EffAddr { seg: sreg::DS, off };
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
        EffAddr {
            seg: if default_ss { sreg::SS } else { sreg::DS },
            off: base.wrapping_add(disp),
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
        } else {
            Rm::Mem(self.compute_ea(mode, rm_field, mem))
        };
        (mode, reg, rm)
    }

    fn read_rm8(&self, rm: Rm, mem: &Memory) -> u8 {
        match rm {
            Rm::Reg(i) => self.read_r8(i),
            Rm::Mem(ea) => mem.read_u8(Self::linear(self.sregs[ea.seg], ea.off)),
        }
    }
    fn write_rm8(&mut self, rm: Rm, mem: &mut Memory, value: u8) {
        match rm {
            Rm::Reg(i) => self.write_r8(i, value),
            Rm::Mem(ea) => mem.write_u8(Self::linear(self.sregs[ea.seg], ea.off), value),
        }
    }
    fn read_rm16(&self, rm: Rm, mem: &Memory) -> u16 {
        match rm {
            Rm::Reg(i) => self.read_r16(i),
            Rm::Mem(ea) => mem.read_u16(Self::linear(self.sregs[ea.seg], ea.off)),
        }
    }
    fn write_rm16(&mut self, rm: Rm, mem: &mut Memory, value: u16) {
        match rm {
            Rm::Reg(i) => self.write_r16(i, value),
            Rm::Mem(ea) => mem.write_u16(Self::linear(self.sregs[ea.seg], ea.off), value),
        }
    }

    /// Push a 16-bit value onto the SS:SP stack. SP decrements *before*
    /// the write — matching real x86 — so after a push SP points at the
    /// new top word.
    fn push16(&mut self, mem: &mut Memory, value: u16) {
        let sp = self.regs[r16::SP].wrapping_sub(2);
        self.regs[r16::SP] = sp;
        mem.write_u16(Self::linear(self.sregs[sreg::SS], sp), value);
    }

    /// Pop a 16-bit value from SS:SP. SP increments *after* the read.
    fn pop16(&mut self, mem: &Memory) -> u16 {
        let sp = self.regs[r16::SP];
        let v = mem.read_u16(Self::linear(self.sregs[sreg::SS], sp));
        self.regs[r16::SP] = sp.wrapping_add(2);
        v
    }

    /// Compute one of the 8 standard ALU ops on 8-bit operands and
    /// update flags. Returns (result, true) for ADD/OR/ADC/SBB/AND/SUB/
    /// XOR (writeback) or (result, false) for CMP. `op` is the same
    /// 0..7 encoding used by both the main ALU dispatch and Group 1.
    fn alu_apply8(&mut self, op: u8, a: u8, b: u8) -> (u8, bool) {
        let cin = if (op == 2 || op == 3) && self.has(flag::CF) { 1 } else { 0 };
        match op {
            0 => { let r = a.wrapping_add(b); self.flags_add8(a, b, 0, r); (r, true) }
            1 => { let r = a | b; self.flags_logic8(r); (r, true) }
            2 => { let r = a.wrapping_add(b).wrapping_add(cin); self.flags_add8(a, b, cin, r); (r, true) }
            3 => { let r = a.wrapping_sub(b).wrapping_sub(cin); self.flags_sub8(a, b, cin, r); (r, true) }
            4 => { let r = a & b; self.flags_logic8(r); (r, true) }
            5 => { let r = a.wrapping_sub(b); self.flags_sub8(a, b, 0, r); (r, true) }
            6 => { let r = a ^ b; self.flags_logic8(r); (r, true) }
            7 => { let r = a.wrapping_sub(b); self.flags_sub8(a, b, 0, r); (r, false) }
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
                let cf = if count <= 8 { ((value as u16) >> (8 - count)) & 1 != 0 } else { false };
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
                    if value & 0x80 != 0 { 0xFF } else { 0 }
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
            // RCL (2) / RCR (3): rotate through CF — deferred to a
            // follow-up. They're rare in compiler output but needed
            // for big-number arithmetic and we'll add them with tests.
            _ => Err(CpuError::Unimplemented { opcode: 0xD0, cs: 0, ip: 0 }),
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
                let cf = if count <= 16 { ((value as u32) >> (16 - count)) & 1 != 0 } else { false };
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
                    if value & 0x8000 != 0 { 0xFFFF } else { 0 }
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
            _ => Err(CpuError::Unimplemented { opcode: 0xD1, cs: 0, ip: 0 }),
        }
    }

    /// Common SI/DI delta for string ops, picked by DF (10 → backward).
    fn string_delta(&self, word: bool) -> u16 {
        let step = if word { 2 } else { 1 };
        if self.has(flag::DF) { 0u16.wrapping_sub(step) } else { step }
    }

    fn step_movsb(&mut self, mem: &mut Memory) {
        let src = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let dst = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let v = mem.read_u8(src);
        mem.write_u8(dst, v);
        let d = self.string_delta(false);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(d);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_movsw(&mut self, mem: &mut Memory) {
        let src = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let dst = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let v = mem.read_u16(src);
        mem.write_u16(dst, v);
        let d = self.string_delta(true);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(d);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_stosb(&mut self, mem: &mut Memory) {
        let dst = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let al = self.read_r8(0);
        mem.write_u8(dst, al);
        let d = self.string_delta(false);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_stosw(&mut self, mem: &mut Memory) {
        let dst = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let ax = self.regs[r16::AX];
        mem.write_u16(dst, ax);
        let d = self.string_delta(true);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_lodsb(&mut self, mem: &Memory) {
        let src = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let v = mem.read_u8(src);
        self.write_r8(0, v);
        let d = self.string_delta(false);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(d);
    }
    fn step_lodsw(&mut self, mem: &Memory) {
        let src = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let v = mem.read_u16(src);
        self.regs[r16::AX] = v;
        let d = self.string_delta(true);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(d);
    }
    fn step_scasb(&mut self, mem: &Memory) {
        let addr = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let v = mem.read_u8(addr);
        let al = self.read_r8(0);
        let r = al.wrapping_sub(v);
        self.flags_sub8(al, v, 0, r);
        let d = self.string_delta(false);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_scasw(&mut self, mem: &Memory) {
        let addr = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let v = mem.read_u16(addr);
        let ax = self.regs[r16::AX];
        let r = ax.wrapping_sub(v);
        self.flags_sub16(ax, v, 0, r);
        let d = self.string_delta(true);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(d);
    }
    fn step_cmpsb(&mut self, mem: &Memory) {
        let s = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let d_addr = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let a = mem.read_u8(s);
        let b = mem.read_u8(d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub8(a, b, 0, r);
        let delta = self.string_delta(false);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(delta);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(delta);
    }
    fn step_cmpsw(&mut self, mem: &Memory) {
        let s = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
        let d_addr = Self::linear(self.sregs[sreg::ES], self.regs[r16::DI]);
        let a = mem.read_u16(s);
        let b = mem.read_u16(d_addr);
        let r = a.wrapping_sub(b);
        self.flags_sub16(a, b, 0, r);
        let delta = self.string_delta(true);
        self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(delta);
        self.regs[r16::DI] = self.regs[r16::DI].wrapping_add(delta);
    }

    /// Dispatch a single string op by primary opcode. Returns true if
    /// the opcode is a recognized string op (callers like the REP
    /// prefix handler use this to know whether the prefix is valid).
    fn step_string(&mut self, inner: u8, mem: &mut Memory) -> bool {
        match inner {
            0xA4 => self.step_movsb(mem),
            0xA5 => self.step_movsw(mem),
            0xA6 => self.step_cmpsb(mem),
            0xA7 => self.step_cmpsw(mem),
            0xAA => self.step_stosb(mem),
            0xAB => self.step_stosw(mem),
            0xAC => self.step_lodsb(mem),
            0xAD => self.step_lodsw(mem),
            0xAE => self.step_scasb(mem),
            0xAF => self.step_scasw(mem),
            _ => return false,
        }
        true
    }

    fn alu_apply16(&mut self, op: u8, a: u16, b: u16) -> (u16, bool) {
        let cin: u16 = if (op == 2 || op == 3) && self.has(flag::CF) { 1 } else { 0 };
        match op {
            0 => { let r = a.wrapping_add(b); self.flags_add16(a, b, 0, r); (r, true) }
            1 => { let r = a | b; self.flags_logic16(r); (r, true) }
            2 => { let r = a.wrapping_add(b).wrapping_add(cin); self.flags_add16(a, b, cin, r); (r, true) }
            3 => { let r = a.wrapping_sub(b).wrapping_sub(cin); self.flags_sub16(a, b, cin, r); (r, true) }
            4 => { let r = a & b; self.flags_logic16(r); (r, true) }
            5 => { let r = a.wrapping_sub(b); self.flags_sub16(a, b, 0, r); (r, true) }
            6 => { let r = a ^ b; self.flags_logic16(r); (r, true) }
            7 => { let r = a.wrapping_sub(b); self.flags_sub16(a, b, 0, r); (r, false) }
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

        // Resolve operands per variant. After this block we have:
        //   a, b        the two values (a is the destination side)
        //   dest        where to write the result (None = imm form)
        //   is_word     8-bit vs 16-bit
        #[derive(Copy, Clone)]
        enum Dest {
            Rm(Rm),
            Reg8(u8),
            Reg16(u8),
        }
        let is_word: bool;
        let a: u32;
        let b: u32;
        let dest: Dest;
        match variant {
            0 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_rm8(rm, mem) as u32;
                b = self.read_r8(reg) as u32;
                dest = Dest::Rm(rm);
                is_word = false;
            }
            1 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_rm16(rm, mem) as u32;
                b = self.read_r16(reg) as u32;
                dest = Dest::Rm(rm);
                is_word = true;
            }
            2 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_r8(reg) as u32;
                b = self.read_rm8(rm, mem) as u32;
                dest = Dest::Reg8(reg);
                is_word = false;
            }
            3 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                a = self.read_r16(reg) as u32;
                b = self.read_rm16(rm, mem) as u32;
                dest = Dest::Reg16(reg);
                is_word = true;
            }
            4 => {
                let imm = self.fetch_u8(mem);
                a = self.read_r8(0) as u32;
                b = imm as u32;
                dest = Dest::Reg8(0);
                is_word = false;
            }
            5 => {
                let imm = self.fetch_u16(mem);
                a = self.read_r16(0) as u32;
                b = imm as u32;
                dest = Dest::Reg16(0);
                is_word = true;
            }
            _ => unreachable!("ALU dispatch only covers variants 0..5"),
        }

        let (result, writeback) = if !is_word {
            let (r, wb) = self.alu_apply8(op, a as u8, b as u8);
            (r as u32, wb)
        } else {
            let (r, wb) = self.alu_apply16(op, a as u16, b as u16);
            (r as u32, wb)
        };

        if writeback {
            match dest {
                Dest::Rm(rm) if is_word => self.write_rm16(rm, mem, result as u16),
                Dest::Rm(rm) => self.write_rm8(rm, mem, result as u8),
                Dest::Reg8(i) => self.write_r8(i, result as u8),
                Dest::Reg16(i) => self.write_r16(i, result as u16),
            }
        }
        Ok(())
    }

    /// Execute a single instruction. Returns Ok(()) on success, or an
    /// error if the opcode/ModR/M form is not implemented.
    pub fn step(&mut self, mem: &mut Memory, io: &mut IoBus) -> Result<(), CpuError> {
        if self.halted {
            return Ok(());
        }
        let op_cs = self.sregs[sreg::CS];
        let op_ip = self.ip;
        let opcode = self.fetch_u8(mem);

        match opcode {
            0x90 => { /* NOP */ }
            0xF4 => { self.halted = true; }
            0xFA => { self.set_flag(flag::IF, false); }
            0xFB => { self.set_flag(flag::IF, true); }
            0xFC => { self.set_flag(flag::DF, false); }
            0xFD => { self.set_flag(flag::DF, true); }

            0xB0..=0xB7 => {
                let imm = self.fetch_u8(mem);
                self.write_r8(opcode - 0xB0, imm);
            }
            0xB8..=0xBF => {
                let imm = self.fetch_u16(mem);
                self.write_r16(opcode - 0xB8, imm);
            }

            0xEB => {
                let rel = self.fetch_u8(mem) as i8;
                self.ip = self.ip.wrapping_add(rel as i16 as u16);
            }
            0xE9 => {
                let rel = self.fetch_u16(mem) as i16;
                self.ip = self.ip.wrapping_add(rel as u16);
            }

            // Jcc rel8 family — 0x70..0x7F
            0x70..=0x7F => {
                let rel = self.fetch_u8(mem) as i8;
                if self.eval_cond(opcode & 0x0F) {
                    self.ip = self.ip.wrapping_add(rel as i16 as u16);
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
                let is_word = matches!(opcode, 0xD1 | 0xD3 | 0xC1);
                let (_, sub, rm) = self.fetch_modrm(mem);
                let count = match opcode {
                    0xD0 | 0xD1 => 1,
                    0xD2 | 0xD3 => self.read_r8(1), // CL
                    0xC0 | 0xC1 => self.fetch_u8(mem),
                    _ => unreachable!(),
                };
                if !is_word {
                    let v = self.read_rm8(rm, mem);
                    let r = self.shift_apply8(sub, v, count)?;
                    self.write_rm8(rm, mem, r);
                } else {
                    let v = self.read_rm16(rm, mem);
                    let r = self.shift_apply16(sub, v, count)?;
                    self.write_rm16(rm, mem, r);
                }
            }

            // REP / REPE / REPZ (0xF3) and REPNE / REPNZ (0xF2) prefix.
            // For MOVS/STOS/LODS the prefix repeats CX times with no
            // ZF condition. For CMPS/SCAS the prefix repeats while
            // (REPE: ZF=1, REPNE: ZF=0). CX is decremented after each
            // string-op step.
            0xF2 | 0xF3 => {
                let rep_zero = opcode == 0xF3;
                let inner = self.fetch_u8(mem);
                let conditional = matches!(inner, 0xA6 | 0xA7 | 0xAE | 0xAF);
                while self.regs[r16::CX] != 0 {
                    if !self.step_string(inner, mem) {
                        return Err(CpuError::Unimplemented {
                            opcode: inner, cs: op_cs, ip: op_ip,
                        });
                    }
                    self.regs[r16::CX] = self.regs[r16::CX].wrapping_sub(1);
                    if conditional {
                        let zf = self.has(flag::ZF);
                        if rep_zero && !zf { break; }
                        if !rep_zero && zf { break; }
                    }
                }
            }

            // Standard ALU family (ADD/OR/ADC/SBB/AND/SUB/XOR/CMP) —
            // opcodes 0x00..0x3F where (opcode & 0x06) != 0x06 (those
            // slots are PUSH/POP sreg / prefixes, handled elsewhere).
            0x00..=0x05 | 0x08..=0x0D | 0x10..=0x15 | 0x18..=0x1D
            | 0x20..=0x25 | 0x28..=0x2D | 0x30..=0x35 | 0x38..=0x3D => {
                self.alu_dispatch(opcode, mem)?;
            }

            // MOV r/m8, r8 — direction = r/m
            0x88 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_r8(reg);
                self.write_rm8(rm, mem, v);
            }
            // MOV r/m16, r16
            0x89 => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_r16(reg);
                self.write_rm16(rm, mem, v);
            }
            // MOV r8, r/m8 — direction = reg
            0x8A => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_rm8(rm, mem);
                self.write_r8(reg, v);
            }
            // MOV r16, r/m16
            0x8B => {
                let (_, reg, rm) = self.fetch_modrm(mem);
                let v = self.read_rm16(rm, mem);
                self.write_r16(reg, v);
            }
            // Group 1: ALU r/m, imm.  reg field of ModR/M = op (0=ADD..7=CMP)
            //   0x80: r/m8, imm8
            //   0x81: r/m16, imm16
            //   0x83: r/m16, imm8 sign-extended to 16-bit
            0x80 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                let imm = self.fetch_u8(mem);
                let a = self.read_rm8(rm, mem);
                let (r, wb) = self.alu_apply8(op, a, imm);
                if wb { self.write_rm8(rm, mem, r); }
            }
            0x81 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                let imm = self.fetch_u16(mem);
                let a = self.read_rm16(rm, mem);
                let (r, wb) = self.alu_apply16(op, a, imm);
                if wb { self.write_rm16(rm, mem, r); }
            }
            0x83 => {
                let (_, op, rm) = self.fetch_modrm(mem);
                let imm = self.fetch_u8(mem) as i8 as i16 as u16;
                let a = self.read_rm16(rm, mem);
                let (r, wb) = self.alu_apply16(op, a, imm);
                if wb { self.write_rm16(rm, mem, r); }
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
                        // NEG sets CF = (operand != 0); flags_sub8 already
                        // computed CF as (0 < v) which is exactly that.
                        self.write_rm8(rm, mem, r);
                    }
                    _ => return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip }),
                }
            }
            0xF7 => {
                let (_, sub, rm) = self.fetch_modrm(mem);
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
                    _ => return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip }),
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
                    _ => return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip }),
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
                        let v = self.read_rm16(rm, mem);
                        let r = v.wrapping_add(1);
                        self.flags_add16(v, 1, 0, r);
                        self.set_flag(flag::CF, cf_before);
                        self.write_rm16(rm, mem, r);
                    }
                    1 => {
                        let cf_before = self.has(flag::CF);
                        let v = self.read_rm16(rm, mem);
                        let r = v.wrapping_sub(1);
                        self.flags_sub16(v, 1, 0, r);
                        self.set_flag(flag::CF, cf_before);
                        self.write_rm16(rm, mem, r);
                    }
                    2 => {
                        let target = self.read_rm16(rm, mem);
                        let ret_ip = self.ip;
                        self.push16(mem, ret_ip);
                        self.ip = target;
                    }
                    4 => {
                        let target = self.read_rm16(rm, mem);
                        self.ip = target;
                    }
                    6 => {
                        let v = self.read_rm16(rm, mem);
                        self.push16(mem, v);
                    }
                    _ => return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip }),
                }
            }

            // MOV r/m8, imm8  — Group 11 /0
            0xC6 => {
                let (_, reg_field, rm) = self.fetch_modrm(mem);
                if reg_field != 0 {
                    return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip });
                }
                let imm = self.fetch_u8(mem);
                self.write_rm8(rm, mem, imm);
            }
            // MOV r/m16, imm16
            0xC7 => {
                let (_, reg_field, rm) = self.fetch_modrm(mem);
                if reg_field != 0 {
                    return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip });
                }
                let imm = self.fetch_u16(mem);
                self.write_rm16(rm, mem, imm);
            }

            // PUSH r16 (0x50..0x57) — push GPR in standard r16 order.
            // PUSH SP on the 8086 pushes the value *after* the decrement
            // (an 80186 quirk fixed by Intel later). We push the original
            // SP — the 80286+ behaviour — because it is what every modern
            // toolchain assumes.
            0x50..=0x57 => {
                let i = opcode - 0x50;
                let v = self.read_r16(i);
                self.push16(mem, v);
            }
            // POP r16 (0x58..0x5F)
            0x58..=0x5F => {
                let i = opcode - 0x58;
                let v = self.pop16(mem);
                self.write_r16(i, v);
            }

            // PUSH imm16
            0x68 => {
                let imm = self.fetch_u16(mem);
                self.push16(mem, imm);
            }
            // PUSH imm8 (sign-extended to 16 bits)
            0x6A => {
                let imm = self.fetch_u8(mem) as i8 as i16 as u16;
                self.push16(mem, imm);
            }

            // CALL rel16 — push return IP, then jump.
            0xE8 => {
                let rel = self.fetch_u16(mem) as i16;
                let ret_ip = self.ip;
                self.push16(mem, ret_ip);
                self.ip = self.ip.wrapping_add(rel as u16);
            }
            // RET (near) — pop IP.
            0xC3 => {
                self.ip = self.pop16(mem);
            }
            // RET imm16 (near) — pop IP, then SP += imm16. Used by
            // callee-cleanup conventions.
            0xC2 => {
                let extra = self.fetch_u16(mem);
                self.ip = self.pop16(mem);
                self.regs[r16::SP] = self.regs[r16::SP].wrapping_add(extra);
            }

            // PUSHF — push the FLAGS register.
            0x9C => {
                let f = self.flags;
                self.push16(mem, f);
            }
            // POPF — pop FLAGS.
            0x9D => {
                self.flags = self.pop16(mem);
            }

            // INC r16 (0x40-0x47) / DEC r16 (0x48-0x4F). Per the 8086,
            // these preserve CF and update ZF/SF/PF/OF/AF.
            0x40..=0x47 => {
                let i = opcode - 0x40;
                let a = self.read_r16(i);
                let r = a.wrapping_add(1);
                let cf_before = self.has(flag::CF);
                self.flags_add16(a, 1, 0, r);
                self.set_flag(flag::CF, cf_before);
                self.write_r16(i, r);
            }
            0x48..=0x4F => {
                let i = opcode - 0x48;
                let a = self.read_r16(i);
                let r = a.wrapping_sub(1);
                let cf_before = self.has(flag::CF);
                self.flags_sub16(a, 1, 0, r);
                self.set_flag(flag::CF, cf_before);
                self.write_r16(i, r);
            }

            // TEST AL, imm8
            0xA8 => {
                let imm = self.fetch_u8(mem);
                let result = self.read_r8(0) & imm;
                self.flags_logic8(result);
            }
            // TEST AX, imm16
            0xA9 => {
                let imm = self.fetch_u16(mem);
                let result = self.read_r16(0) & imm;
                self.flags_logic16(result);
            }

            0xEC => {
                // IN AL, DX
                let port = self.regs[r16::DX];
                let v = io.read(port);
                self.write_r8(0, v);
            }
            0xEE => {
                // OUT DX, AL
                let port = self.regs[r16::DX];
                let v = self.read_r8(0);
                io.write(port, v);
            }
            0xE4 => {
                // IN AL, imm8
                let port = self.fetch_u8(mem) as u16;
                let v = io.read(port);
                self.write_r8(0, v);
            }
            0xE6 => {
                // OUT imm8, AL
                let port = self.fetch_u8(mem) as u16;
                let v = self.read_r8(0);
                io.write(port, v);
            }

            _ => {
                return Err(CpuError::Unimplemented {
                    opcode, cs: op_cs, ip: op_ip,
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
            0x0 => of,             // JO
            0x1 => !of,            // JNO
            0x2 => cf,             // JB / JC
            0x3 => !cf,            // JAE / JNC
            0x4 => zf,             // JE / JZ
            0x5 => !zf,            // JNE / JNZ
            0x6 => cf || zf,       // JBE
            0x7 => !cf && !zf,     // JA
            0x8 => sf,             // JS
            0x9 => !sf,            // JNS
            0xA => pf,             // JP
            0xB => !pf,            // JNP
            0xC => sf != of,       // JL
            0xD => sf == of,       // JGE
            0xE => zf || (sf != of), // JLE
            0xF => !zf && (sf == of), // JG
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wwwvm_devices::IoBus;

    fn run_payload(bytes: &[u8], steps: usize) -> (Cpu, Memory, IoBus) {
        run_with_data(bytes, 0, &[], steps)
    }

    fn run_with_data(bytes: &[u8], data_at: u32, data: &[u8], steps: usize) -> (Cpu, Memory, IoBus) {
        let mut mem = Memory::new(0x10_0000);
        mem.write_slice(0x7C00, bytes);
        if !data.is_empty() {
            mem.write_slice(data_at, data);
        }
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        for _ in 0..steps {
            if cpu.halted { break; }
            cpu.step(&mut mem, &mut io).expect("step");
        }
        (cpu, mem, io)
    }

    #[test]
    fn mov_imm_then_hlt() {
        let (cpu, _, _) = run_payload(&[0xB8, 0x34, 0x12, 0xF4], 8);
        assert_eq!(cpu.regs[r16::AX], 0x1234);
        assert!(cpu.halted);
    }

    #[test]
    fn or_al_al_sets_zf_when_zero() {
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x00,       // MOV AL, 0
            0x08, 0xC0,       // OR AL, AL
            0xF4,             // HLT
        ], 8);
        assert!(cpu.has(flag::ZF));
        assert!(cpu.halted);
    }

    #[test]
    fn out_writes_to_uart() {
        let (_, _, mut io) = run_payload(&[
            0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
            0xB0, b'X',       // MOV AL, 'X'
            0xEE,             // OUT DX, AL
            0xF4,             // HLT
        ], 8);
        assert_eq!(io.uart_mut().drain_tx(), b"X");
    }

    #[test]
    fn add_r16_imm16_to_ax_sets_flags() {
        // MOV AX, 0xFFF0 ; ADD AX, 0x0020 → 0x0010 with CF=1
        let (cpu, _, _) = run_payload(&[
            0xB8, 0xF0, 0xFF,
            0x05, 0x20, 0x00,
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0x0010);
        assert!(cpu.has(flag::CF));
        assert!(!cpu.has(flag::ZF));
    }

    #[test]
    fn add_r8_to_r8_register_form() {
        // MOV AL, 5 ; MOV BL, 7 ; ADD AL, BL ; HLT
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x05,
            0xB3, 0x07,
            0x00, 0xD8,         // ADD AL, BL (0x00 /r, modrm=11 011 000)
            0xF4,
        ], 8);
        assert_eq!(cpu.read_r8(0), 12);
        assert!(!cpu.has(flag::ZF));
        assert!(!cpu.has(flag::CF));
    }

    #[test]
    fn sub_sets_borrow() {
        // MOV AL, 1 ; SUB AL, 2 ; expect AL=0xFF, CF=1, SF=1
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x01,
            0x2C, 0x02,         // SUB AL, imm8
            0xF4,
        ], 8);
        assert_eq!(cpu.read_r8(0), 0xFF);
        assert!(cpu.has(flag::CF));
        assert!(cpu.has(flag::SF));
        assert!(!cpu.has(flag::ZF));
    }

    #[test]
    fn cmp_does_not_writeback_but_sets_flags() {
        // MOV AX, 7 ; CMP AX, 7 → ZF=1, AX unchanged
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x07, 0x00,
            0x3D, 0x07, 0x00,   // CMP AX, imm16
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 7);
        assert!(cpu.has(flag::ZF));
        assert!(!cpu.has(flag::CF));
    }

    #[test]
    fn xor_clears_register_and_sets_zf() {
        // MOV AX, 0x1234 ; XOR AX, AX
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x34, 0x12,
            0x31, 0xC0,          // XOR AX, AX (0x31 /r, modrm=11 000 000)
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0);
        assert!(cpu.has(flag::ZF));
        assert!(!cpu.has(flag::CF));
    }

    #[test]
    fn inc_dec_r16_preserve_cf() {
        // MOV AX, 0xFFFF ; STC equivalent via ADD overflow ; INC AX ; should wrap to 0, ZF=1, CF preserved
        let (cpu, _, _) = run_payload(&[
            0xB8, 0xFF, 0xFF,
            0x40,               // INC AX
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0);
        assert!(cpu.has(flag::ZF));
        // CF was 0 going in; INC must not touch it
        assert!(!cpu.has(flag::CF));

        // DEC 0 → 0xFFFF, ZF=0, SF=1
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x00, 0x00,
            0x48,               // DEC AX
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0xFFFF);
        assert!(!cpu.has(flag::ZF));
        assert!(cpu.has(flag::SF));
    }

    #[test]
    fn mov_byte_to_memory_and_back_via_bx() {
        // MOV BX, 0x500 ; MOV AL, 0x42 ; MOV [BX], AL
        // MOV CL, 0     ; MOV CL, [BX]
        // ModR/M for [BX]: mod=00 rm=111
        //   MOV [BX], AL : 0x88 modrm=00 000(AL) 111(BX) = 0x07
        //   MOV CL, [BX] : 0x8A modrm=00 001(CL) 111(BX) = 0x0F
        let (cpu, mem, _) = run_payload(&[
            0xBB, 0x00, 0x05,
            0xB0, 0x42,
            0x88, 0x07,
            0xB1, 0x00,
            0x8A, 0x0F,
            0xF4,
        ], 12);
        assert_eq!(mem.read_u8(0x500), 0x42);
        assert_eq!(cpu.read_r8(1), 0x42);
    }

    #[test]
    fn mov_word_imm_to_disp16_address() {
        // MOV WORD [0x600], 0xCAFE
        // 0xC7 modrm=00 000 110 = 0x06, then disp16=0x0600, then imm16=0xCAFE
        let (_, mem, _) = run_payload(&[
            0xC7, 0x06, 0x00, 0x06, 0xFE, 0xCA,
            0xF4,
        ], 4);
        assert_eq!(mem.read_u16(0x600), 0xCAFE);
    }

    #[test]
    fn add_reg_to_memory_via_bx() {
        // MOV WORD [0x700], 10
        // MOV BX, 0x700 ; MOV AX, 5 ; ADD [BX], AX
        //   ADD r/m16, r16 = 0x01 /r ; mod=00 reg=000(AX) rm=111(BX) = 0x07
        let (_, mem, _) = run_payload(&[
            0xC7, 0x06, 0x00, 0x07, 0x0A, 0x00,
            0xBB, 0x00, 0x07,
            0xB8, 0x05, 0x00,
            0x01, 0x07,
            0xF4,
        ], 10);
        assert_eq!(mem.read_u16(0x700), 15);
    }

    #[test]
    fn bp_addressing_defaults_to_ss_segment() {
        // SS is 0 in our reset_to_boot, so this is just a sanity check
        // that decoding picks SS (not DS) for [BP] form, and that the
        // address still resolves correctly when both are zero.
        // MOV BP, 0x900 ; MOV WORD [BP], 0x1357 (mod=10 rm=110 disp16=0)
        //   0xC7 modrm=10 000 110 = 0x86 ; disp16=0x0000 ; imm16=0x1357
        let (_, mem, _) = run_payload(&[
            0xBD, 0x00, 0x09,
            0xC7, 0x86, 0x00, 0x00, 0x57, 0x13,
            0xF4,
        ], 6);
        assert_eq!(mem.read_u16(0x900), 0x1357);
    }

    #[test]
    fn sum_array_in_memory_via_indirect_addressing() {
        // Array of u16 at 0x800: 1, 2, 3, 4, 5, 0 (terminator)
        //   MOV SI, 0x800
        //   MOV CX, 2          ; step
        //   XOR AX, AX
        // loop (offset 8):
        //   MOV BX, [SI]       ; 8B 1C  (mod=00 reg=011 BX rm=100 [SI])
        //   OR  BX, BX         ; 09 DB
        //   JZ  +6  -> done    ; 74 06
        //   ADD AX, BX         ; 01 D8
        //   ADD SI, CX         ; 01 CE  (SI += CX)
        //   JMP -12 -> loop    ; EB F4
        // done (offset 0x14):
        //   HLT                ; F4
        let array: &[u8] = &[1,0, 2,0, 3,0, 4,0, 5,0, 0,0];
        let bytes = [
            0xBE, 0x00, 0x08,
            0xB9, 0x02, 0x00,
            0x31, 0xC0,
            0x8B, 0x1C,
            0x09, 0xDB,
            0x74, 0x06,
            0x01, 0xD8,
            0x01, 0xCE,
            0xEB, 0xF4,
            0xF4,
        ];
        let (cpu, _, _) = run_with_data(&bytes, 0x800, array, 200);
        assert_eq!(cpu.regs[r16::AX], 15);
        assert!(cpu.halted);
    }

    #[test]
    fn loop_with_dec_and_jnz() {
        // Sum 1..=5 in BX using DEC + JNZ.
        //   MOV CX, 5
        //   XOR BX, BX
        // lp:
        //   ADD BX, CX
        //   DEC CX
        //   JNZ lp        (rel = -5)
        //   HLT
        let (cpu, _, _) = run_payload(&[
            0xB9, 0x05, 0x00,       // MOV CX, 5
            0x31, 0xDB,             // XOR BX, BX
            0x01, 0xCB,             // ADD BX, CX  (0x01 /r, modrm=11 001 011)
            0x49,                   // DEC CX
            0x75, 0xFB,             // JNZ -5
            0xF4,                   // HLT
        ], 50);
        assert_eq!(cpu.regs[r16::BX], 15);
        assert_eq!(cpu.regs[r16::CX], 0);
        assert!(cpu.halted);
    }

    #[test]
    fn push_pop_round_trip_through_other_reg() {
        // MOV AX, 0x1234 ; PUSH AX ; MOV AX, 0 ; POP BX ; HLT
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x34, 0x12,
            0x50,                // PUSH AX
            0xB8, 0x00, 0x00,
            0x5B,                // POP BX
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::BX], 0x1234);
        assert_eq!(cpu.regs[r16::AX], 0);
        // SP must be back to its boot value
        assert_eq!(cpu.regs[r16::SP], 0x7C00);
    }

    #[test]
    fn push_writes_below_sp_lifo() {
        // PUSH 0xAAAA ; PUSH 0xBBBB ; POP AX ; POP BX
        // After pushes, AX should be the most-recent (0xBBBB), BX older.
        let (cpu, _, _) = run_payload(&[
            0x68, 0xAA, 0xAA,
            0x68, 0xBB, 0xBB,
            0x58,                // POP AX
            0x5B,                // POP BX
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0xBBBB);
        assert_eq!(cpu.regs[r16::BX], 0xAAAA);
    }

    #[test]
    fn push_imm8_sign_extends_to_16_bits() {
        // PUSH 0xFF (imm8) → on the stack as 0xFFFF
        let (cpu, mem, _) = run_payload(&[
            0x6A, 0xFF,
            0xF4,
        ], 4);
        // Stack top is at SS:SP after the push
        let top = mem.read_u16(((cpu.sregs[sreg::SS] as u32) << 4) + cpu.regs[r16::SP] as u32);
        assert_eq!(top, 0xFFFF);
    }

    #[test]
    fn call_pushes_return_ip_and_ret_restores_it() {
        // 0: B8 00 00     MOV AX, 0
        // 3: E8 01 00     CALL +1  (target offset 7)
        // 6: F4           HLT
        // 7: B8 07 00     MOV AX, 7
        // A: C3           RET
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x00, 0x00,
            0xE8, 0x01, 0x00,
            0xF4,
            0xB8, 0x07, 0x00,
            0xC3,
        ], 16);
        assert_eq!(cpu.regs[r16::AX], 7);
        assert!(cpu.halted);
        // SP must be back to its boot value
        assert_eq!(cpu.regs[r16::SP], 0x7C00);
    }

    #[test]
    fn ret_imm16_pops_extra_bytes() {
        // 0: 68 99 00       PUSH 0x99           ; "argument"
        // 3: E8 02 00       CALL +2             ; -> 8
        // 6: F4             HLT
        // 7: 90             NOP (filler)
        // 8: C2 02 00       RET 2               ; pop IP, then SP+=2
        //
        // Inv: after RET 2, SP is back to its boot value because the
        // imm16 cleanup popped the argument. Plain RET would leave SP
        // 2 bytes lower.
        let (cpu, _, _) = run_payload(&[
            0x68, 0x99, 0x00,
            0xE8, 0x02, 0x00,
            0xF4,
            0x90,
            0xC2, 0x02, 0x00,
        ], 16);
        assert!(cpu.halted);
        assert_eq!(cpu.regs[r16::SP], 0x7C00);
    }

    #[test]
    fn pushf_popf_round_trips_flags() {
        // Set ZF via XOR AX, AX ; PUSHF ; clear ZF via MOV AX, 1 (no
        // flag changes…) — we need an op that touches ZF. Use INC AX
        // which clears ZF when AX!=0.
        //   XOR AX, AX        ; ZF=1
        //   PUSHF
        //   INC AX            ; ZF=0
        //   POPF              ; ZF=1 restored
        //   HLT
        let (cpu, _, _) = run_payload(&[
            0x31, 0xC0,
            0x9C,
            0x40,
            0x9D,
            0xF4,
        ], 8);
        assert!(cpu.has(flag::ZF));
    }

    #[test]
    fn group1_add_imm_to_r16() {
        // ADD AX, 7    via 0x83 /0 (sign-ext imm8) — ModR/M = 11 000 000 = 0xC0
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x05, 0x00,    // MOV AX, 5
            0x83, 0xC0, 0x07,    // ADD AX, 7
            0xF4,
        ], 6);
        assert_eq!(cpu.regs[r16::AX], 12);
    }

    #[test]
    fn group1_sub_r16_imm16() {
        // SUB AX, 0x1000 via 0x81 /5 — ModR/M = 11 101 000 = 0xE8
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x34, 0x12,
            0x81, 0xE8, 0x00, 0x10,
            0xF4,
        ], 6);
        assert_eq!(cpu.regs[r16::AX], 0x0234);
    }

    #[test]
    fn group1_cmp_imm_does_not_writeback() {
        // CMP AL, 0x42 via 0x80 /7 — ModR/M = 11 111 000 = 0xF8
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x42,
            0x80, 0xF8, 0x42,
            0xF4,
        ], 6);
        assert_eq!(cpu.read_r8(0), 0x42);
        assert!(cpu.has(flag::ZF));
    }

    #[test]
    fn group3_neg_and_not_r16() {
        // NEG AX where AX=5 -> 0xFFFB, CF=1
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x05, 0x00,
            0xF7, 0xD8,           // NEG AX (F7 /3, ModR/M = 11 011 000 = 0xD8)
            0xF4,
        ], 6);
        assert_eq!(cpu.regs[r16::AX], 0xFFFB);
        assert!(cpu.has(flag::CF));

        // NOT BX where BX=0xAAAA -> 0x5555, flags untouched
        let (cpu, _, _) = run_payload(&[
            0xBB, 0xAA, 0xAA,
            0xF7, 0xD3,           // NOT BX (F7 /2, ModR/M = 11 010 011 = 0xD3)
            0xF4,
        ], 6);
        assert_eq!(cpu.regs[r16::BX], 0x5555);
    }

    #[test]
    fn group3_test_rm_imm() {
        // TEST AL, 0x80 (F6 /0, modrm=11 000 000 = 0xC0); AL=0x80 → ZF=0, SF=1
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x80,
            0xF6, 0xC0, 0x80,
            0xF4,
        ], 6);
        assert!(!cpu.has(flag::ZF));
        assert!(cpu.has(flag::SF));
        assert_eq!(cpu.read_r8(0), 0x80);   // unchanged
    }

    #[test]
    fn group4_inc_memory_byte() {
        // INC byte [0x900] via FE /0 (modrm=00 000 110 = 0x06, then disp16)
        let (_, mem, _) = run_payload(&[
            0xC6, 0x06, 0x00, 0x09, 0x09,  // MOV byte [0x900], 9
            0xFE, 0x06, 0x00, 0x09,        // INC byte [0x900]
            0xF4,
        ], 6);
        assert_eq!(mem.read_u8(0x900), 10);
    }

    #[test]
    fn group5_indirect_call_via_register() {
        // Code is loaded at CS:IP = 0000:7C00, so absolute IPs are
        // 0x7C00 + offset.
        //
        // 0: B8 08 7C     MOV AX, 0x7C08    ; absolute target
        // 3: FF D0        CALL AX           (FF /2, modrm=11 010 000)
        // 5: B3 11        MOV BL, 0x11
        // 7: F4           HLT
        // 8: B3 22        MOV BL, 0x22      ; callee
        // A: C3           RET
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x08, 0x7C,
            0xFF, 0xD0,
            0xB3, 0x11,
            0xF4,
            0xB3, 0x22,
            0xC3,
        ], 24);
        // The callee ran (BL=0x22), then we returned and the next line
        // overwrote BL with 0x11. So after halt, BL == 0x11. If CALL had
        // gone elsewhere (or RET hadn't returned), this would fail.
        assert_eq!(cpu.read_r8(3), 0x11);
        assert!(cpu.halted);
        assert_eq!(cpu.regs[r16::SP], 0x7C00);
    }

    #[test]
    fn group5_jmp_indirect_via_register() {
        // JMP AX (FF /4) — jump without saving the return IP.
        // 0: B8 06 7C     MOV AX, 0x7C06    ; absolute target
        // 3: FF E0        JMP AX
        // 5: F4           HLT               ; skipped
        // 6: B3 77        MOV BL, 0x77
        // 8: F4           HLT
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x06, 0x7C,
            0xFF, 0xE0,
            0xF4,
            0xB3, 0x77,
            0xF4,
        ], 8);
        assert_eq!(cpu.read_r8(3), 0x77);
        assert!(cpu.halted);
    }

    #[test]
    fn group5_push_rm16() {
        // PUSH [0x900] via FF /6 (modrm=00 110 110 = 0x36, disp16)
        let (cpu, mem, _) = run_payload(&[
            0xC7, 0x06, 0x00, 0x09, 0xCD, 0xAB,  // MOV WORD [0x900], 0xABCD
            0xFF, 0x36, 0x00, 0x09,              // PUSH [0x900]
            0x58,                                 // POP AX
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0xABCD);
        let _ = mem; // mem is consulted via the POP
    }

    #[test]
    fn shl_by_one_sets_cf_from_top_bit() {
        // MOV AL, 0xC0 ; SHL AL, 1 → 0x80, CF=1, OF=0 (sign unchanged)
        // SHL r/m8, 1 = 0xD0 /4. ModR/M = 11 100 000 = 0xE0
        let (cpu, _, _) = run_payload(&[
            0xB0, 0xC0,
            0xD0, 0xE0,
            0xF4,
        ], 6);
        assert_eq!(cpu.read_r8(0), 0x80);
        assert!(cpu.has(flag::CF));
        assert!(!cpu.has(flag::OF));
    }

    #[test]
    fn shl_by_cl_count() {
        // MOV AX, 1 ; MOV CL, 4 ; SHL AX, CL → 0x10
        // SHL r/m16, CL = 0xD3 /4. ModR/M = 11 100 000 = 0xE0
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x01, 0x00,
            0xB1, 0x04,
            0xD3, 0xE0,
            0xF4,
        ], 8);
        assert_eq!(cpu.regs[r16::AX], 0x10);
    }

    #[test]
    fn shr_by_one_drops_lsb_into_cf() {
        // MOV AL, 0x03 ; SHR AL, 1 → 0x01, CF=1
        // SHR r/m8, 1 = 0xD0 /5. ModR/M = 11 101 000 = 0xE8
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x03,
            0xD0, 0xE8,
            0xF4,
        ], 4);
        assert_eq!(cpu.read_r8(0), 0x01);
        assert!(cpu.has(flag::CF));
    }

    #[test]
    fn sar_sign_extends_negative() {
        // MOV AL, 0x80 ; SAR AL, 1 → 0xC0 (sign-extended), CF=0
        // SAR r/m8, 1 = 0xD0 /7. ModR/M = 11 111 000 = 0xF8
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x80,
            0xD0, 0xF8,
            0xF4,
        ], 4);
        assert_eq!(cpu.read_r8(0), 0xC0);
        assert!(!cpu.has(flag::CF));
        assert!(cpu.has(flag::SF));
    }

    #[test]
    fn rol_by_one_wraps_msb_to_lsb() {
        // MOV AL, 0x81 ; ROL AL, 1 → 0x03, CF=1, OF=0 (no sign flip)
        // ROL r/m8, 1 = 0xD0 /0. ModR/M = 11 000 000 = 0xC0
        let (cpu, _, _) = run_payload(&[
            0xB0, 0x81,
            0xD0, 0xC0,
            0xF4,
        ], 4);
        assert_eq!(cpu.read_r8(0), 0x03);
        assert!(cpu.has(flag::CF));
    }

    #[test]
    fn ror_by_imm_count() {
        // MOV AX, 0x0001 ; ROR AX, 4 → 0x1000
        // ROR r/m16, imm8 = 0xC1 /1. ModR/M = 11 001 000 = 0xC8
        let (cpu, _, _) = run_payload(&[
            0xB8, 0x01, 0x00,
            0xC1, 0xC8, 0x04,
            0xF4,
        ], 6);
        assert_eq!(cpu.regs[r16::AX], 0x1000);
    }

    #[test]
    fn movsb_copies_one_byte_with_si_di_increment() {
        // src @ 0x800 = 0x77 ; ES already 0, SS=0
        // MOV SI, 0x800 ; MOV DI, 0x900 ; MOVSB
        let (cpu, mem, _) = run_with_data(&[
            0xBE, 0x00, 0x08,
            0xBF, 0x00, 0x09,
            0xA4,
            0xF4,
        ], 0x800, &[0x77], 8);
        assert_eq!(mem.read_u8(0x900), 0x77);
        assert_eq!(cpu.regs[r16::SI], 0x801);
        assert_eq!(cpu.regs[r16::DI], 0x901);
    }

    #[test]
    fn rep_movsb_copies_buffer() {
        // Copy 5 bytes from 0x800 to 0x900 with REP MOVSB.
        //   MOV SI, 0x800
        //   MOV DI, 0x900
        //   MOV CX, 5
        //   REP MOVSB   (F3 A4)
        //   HLT
        let src = b"hello";
        let (cpu, mem, _) = run_with_data(&[
            0xBE, 0x00, 0x08,
            0xBF, 0x00, 0x09,
            0xB9, 0x05, 0x00,
            0xF3, 0xA4,
            0xF4,
        ], 0x800, src, 12);
        let mut got = [0u8; 5];
        for i in 0..5 { got[i] = mem.read_u8(0x900 + i as u32); }
        assert_eq!(&got, src);
        assert_eq!(cpu.regs[r16::CX], 0);
    }

    #[test]
    fn rep_stosb_fills_buffer() {
        // Fill 4 bytes at 0x900 with 0xAA.
        //   MOV AL, 0xAA ; MOV DI, 0x900 ; MOV CX, 4 ; REP STOSB
        let (_, mem, _) = run_payload(&[
            0xB0, 0xAA,
            0xBF, 0x00, 0x09,
            0xB9, 0x04, 0x00,
            0xF3, 0xAA,
            0xF4,
        ], 10);
        for i in 0..4 {
            assert_eq!(mem.read_u8(0x900 + i), 0xAA);
        }
        // Should NOT overwrite the byte one past.
        assert_eq!(mem.read_u8(0x904), 0);
    }

    #[test]
    fn repne_scasb_finds_terminator() {
        // Search a NUL-terminated string for NUL using REPNE SCASB.
        //   AL=0 ; ES:DI = 0x800 ; CX = 0xFFFF ; REPNE SCASB
        // After: DI points one past the NUL; (0xFFFF - 1) - CX = bytes
        // scanned.
        let s = b"abc\0";
        let (cpu, _, _) = run_with_data(&[
            0xB0, 0x00,
            0xBF, 0x00, 0x08,
            0xB9, 0xFF, 0xFF,
            0xF2, 0xAE,
            0xF4,
        ], 0x800, s, 12);
        // Found at byte 3 ('\0'), so DI advanced 4 times.
        assert_eq!(cpu.regs[r16::DI], 0x804);
        assert!(cpu.has(flag::ZF));
    }

    #[test]
    fn repe_cmpsb_stops_on_mismatch() {
        // "abXd" at 0x800 vs "abYd" at 0x900. REPE CMPSB walks while
        // equal — should stop on the X/Y pair. We seed 0x800 via the
        // run_with_data data slot and write 0x900 inline via four
        // MOV byte [disp16], imm instructions.
        //
        // Expected: 3 compares done (eq, eq, ne), so CX goes 4→1, DI
        // advances 3 → 0x903, ZF=0 from the last failed compare.
        let bytes = [
            // Write "abYd" to 0x900
            0xC6, 0x06, 0x00, 0x09, b'a',
            0xC6, 0x06, 0x01, 0x09, b'b',
            0xC6, 0x06, 0x02, 0x09, b'Y',
            0xC6, 0x06, 0x03, 0x09, b'd',
            // REPE CMPSB setup + run
            0xBE, 0x00, 0x08,
            0xBF, 0x00, 0x09,
            0xB9, 0x04, 0x00,
            0xF3, 0xA6,
            0xF4,
        ];
        let (cpu, _, _) = run_with_data(&bytes, 0x800, b"abXd", 30);
        assert_eq!(cpu.regs[r16::CX], 1);
        assert_eq!(cpu.regs[r16::DI], 0x903);
        assert!(!cpu.has(flag::ZF));
    }

    #[test]
    fn unknown_opcode_reports_error() {
        let mut mem = Memory::new(0x10_0000);
        mem.write_slice(0x7C00, &[0x0F]); // 0x0F = 2-byte opcode prefix, not supported
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        let err = cpu.step(&mut mem, &mut io).unwrap_err();
        match err {
            CpuError::Unimplemented { opcode: 0x0F, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
