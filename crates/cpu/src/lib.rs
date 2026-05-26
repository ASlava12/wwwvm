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

    /// Execute one of the 8 standard ALU operations encoded in opcode
    /// 0x00..0x3F. `op` is the operation (0=ADD … 7=CMP) and `variant`
    /// selects operand form (0..5). Currently mod=11 only for the
    /// register-mode variants — memory ModR/M arrives in a follow-up.
    fn alu_dispatch(
        &mut self,
        opcode: u8,
        op_cs: u16,
        op_ip: u16,
        mem: &mut Memory,
    ) -> Result<(), CpuError> {
        let op = (opcode >> 3) & 7;
        let variant = opcode & 7;
        // Resolve operands & destination per variant.
        enum Dest { R8(u8), R16(u8) }
        let is_word: bool;
        let a: u32;
        let b: u32;
        let dest: Dest;
        match variant {
            0 => {
                // r/m8, r8 — write back to r/m
                let modrm = self.fetch_u8(mem);
                let (mode, reg, rm) = parse_modrm(modrm);
                if mode != 0b11 {
                    return Err(CpuError::UnimplementedModRm { opcode, mode, cs: op_cs, ip: op_ip });
                }
                a = self.read_r8(rm) as u32;
                b = self.read_r8(reg) as u32;
                dest = Dest::R8(rm);
                is_word = false;
            }
            1 => {
                // r/m16, r16
                let modrm = self.fetch_u8(mem);
                let (mode, reg, rm) = parse_modrm(modrm);
                if mode != 0b11 {
                    return Err(CpuError::UnimplementedModRm { opcode, mode, cs: op_cs, ip: op_ip });
                }
                a = self.read_r16(rm) as u32;
                b = self.read_r16(reg) as u32;
                dest = Dest::R16(rm);
                is_word = true;
            }
            2 => {
                // r8, r/m8 — write back to reg
                let modrm = self.fetch_u8(mem);
                let (mode, reg, rm) = parse_modrm(modrm);
                if mode != 0b11 {
                    return Err(CpuError::UnimplementedModRm { opcode, mode, cs: op_cs, ip: op_ip });
                }
                a = self.read_r8(reg) as u32;
                b = self.read_r8(rm) as u32;
                dest = Dest::R8(reg);
                is_word = false;
            }
            3 => {
                // r16, r/m16
                let modrm = self.fetch_u8(mem);
                let (mode, reg, rm) = parse_modrm(modrm);
                if mode != 0b11 {
                    return Err(CpuError::UnimplementedModRm { opcode, mode, cs: op_cs, ip: op_ip });
                }
                a = self.read_r16(reg) as u32;
                b = self.read_r16(rm) as u32;
                dest = Dest::R16(reg);
                is_word = true;
            }
            4 => {
                // AL, imm8
                let imm = self.fetch_u8(mem);
                a = self.read_r8(0) as u32;
                b = imm as u32;
                dest = Dest::R8(0);
                is_word = false;
            }
            5 => {
                // AX, imm16
                let imm = self.fetch_u16(mem);
                a = self.read_r16(0) as u32;
                b = imm as u32;
                dest = Dest::R16(0);
                is_word = true;
            }
            _ => return Err(CpuError::Unimplemented { opcode, cs: op_cs, ip: op_ip }),
        }

        // Apply the op and update flags. CMP discards the result.
        let cin = if (op == 2 || op == 3) && self.has(flag::CF) { 1 } else { 0 };
        let (result, writeback) = if !is_word {
            let (a8, b8) = (a as u8, b as u8);
            match op {
                0 => { let r = a8.wrapping_add(b8); self.flags_add8(a8, b8, 0, r); (r as u32, true) }
                1 => { let r = a8 | b8; self.flags_logic8(r); (r as u32, true) }
                2 => { let r = a8.wrapping_add(b8).wrapping_add(cin as u8); self.flags_add8(a8, b8, cin as u8, r); (r as u32, true) }
                3 => { let r = a8.wrapping_sub(b8).wrapping_sub(cin as u8); self.flags_sub8(a8, b8, cin as u8, r); (r as u32, true) }
                4 => { let r = a8 & b8; self.flags_logic8(r); (r as u32, true) }
                5 => { let r = a8.wrapping_sub(b8); self.flags_sub8(a8, b8, 0, r); (r as u32, true) }
                6 => { let r = a8 ^ b8; self.flags_logic8(r); (r as u32, true) }
                7 => { let r = a8.wrapping_sub(b8); self.flags_sub8(a8, b8, 0, r); (r as u32, false) }
                _ => unreachable!(),
            }
        } else {
            let (a16, b16) = (a as u16, b as u16);
            match op {
                0 => { let r = a16.wrapping_add(b16); self.flags_add16(a16, b16, 0, r); (r as u32, true) }
                1 => { let r = a16 | b16; self.flags_logic16(r); (r as u32, true) }
                2 => { let r = a16.wrapping_add(b16).wrapping_add(cin); self.flags_add16(a16, b16, cin, r); (r as u32, true) }
                3 => { let r = a16.wrapping_sub(b16).wrapping_sub(cin); self.flags_sub16(a16, b16, cin, r); (r as u32, true) }
                4 => { let r = a16 & b16; self.flags_logic16(r); (r as u32, true) }
                5 => { let r = a16.wrapping_sub(b16); self.flags_sub16(a16, b16, 0, r); (r as u32, true) }
                6 => { let r = a16 ^ b16; self.flags_logic16(r); (r as u32, true) }
                7 => { let r = a16.wrapping_sub(b16); self.flags_sub16(a16, b16, 0, r); (r as u32, false) }
                _ => unreachable!(),
            }
        };

        if writeback {
            match dest {
                Dest::R8(i) => self.write_r8(i, result as u8),
                Dest::R16(i) => self.write_r16(i, result as u16),
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

            0xAC => {
                // LODSB: AL = DS:[SI]; SI += DF ? -1 : +1
                let addr = Self::linear(self.sregs[sreg::DS], self.regs[r16::SI]);
                let v = mem.read_u8(addr);
                self.write_r8(0, v); // AL
                let delta: u16 = if self.has(flag::DF) { 0xFFFF } else { 1 };
                self.regs[r16::SI] = self.regs[r16::SI].wrapping_add(delta);
            }

            // Standard ALU family (ADD/OR/ADC/SBB/AND/SUB/XOR/CMP) —
            // opcodes 0x00..0x3F where (opcode & 0x06) != 0x06 (those
            // slots are PUSH/POP sreg / prefixes, handled elsewhere).
            0x00..=0x05 | 0x08..=0x0D | 0x10..=0x15 | 0x18..=0x1D
            | 0x20..=0x25 | 0x28..=0x2D | 0x30..=0x35 | 0x38..=0x3D => {
                self.alu_dispatch(opcode, op_cs, op_ip, mem)?;
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

fn parse_modrm(byte: u8) -> (u8, u8, u8) {
    (byte >> 6, (byte >> 3) & 0x07, byte & 0x07)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wwwvm_devices::IoBus;

    fn run_payload(bytes: &[u8], steps: usize) -> (Cpu, Memory, IoBus) {
        let mut mem = Memory::new(0x10_0000);
        mem.write_slice(0x7C00, bytes);
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
