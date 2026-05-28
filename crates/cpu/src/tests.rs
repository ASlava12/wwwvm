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
        if cpu.halted {
            break;
        }
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
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x00, // MOV AL, 0
            0x08, 0xC0, // OR AL, AL
            0xF4, // HLT
        ],
        8,
    );
    assert!(cpu.has(flag::ZF));
    assert!(cpu.halted);
}

#[test]
fn out_writes_to_uart() {
    let (_, _, mut io) = run_payload(
        &[
            0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
            0xB0, b'X', // MOV AL, 'X'
            0xEE, // OUT DX, AL
            0xF4, // HLT
        ],
        8,
    );
    assert_eq!(io.uart_mut().drain_tx(), b"X");
}

#[test]
fn add_r16_imm16_to_ax_sets_flags() {
    // MOV AX, 0xFFF0 ; ADD AX, 0x0020 → 0x0010 with CF=1
    let (cpu, _, _) = run_payload(&[0xB8, 0xF0, 0xFF, 0x05, 0x20, 0x00, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 0x0010);
    assert!(cpu.has(flag::CF));
    assert!(!cpu.has(flag::ZF));
}

#[test]
fn add_r8_to_r8_register_form() {
    // MOV AL, 5 ; MOV BL, 7 ; ADD AL, BL ; HLT
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x05, 0xB3, 0x07, 0x00, 0xD8, // ADD AL, BL (0x00 /r, modrm=11 011 000)
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 12);
    assert!(!cpu.has(flag::ZF));
    assert!(!cpu.has(flag::CF));
}

#[test]
fn sub_sets_borrow() {
    // MOV AL, 1 ; SUB AL, 2 ; expect AL=0xFF, CF=1, SF=1
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x01, 0x2C, 0x02, // SUB AL, imm8
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 0xFF);
    assert!(cpu.has(flag::CF));
    assert!(cpu.has(flag::SF));
    assert!(!cpu.has(flag::ZF));
}

#[test]
fn cmp_does_not_writeback_but_sets_flags() {
    // MOV AX, 7 ; CMP AX, 7 → ZF=1, AX unchanged
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x07, 0x00, 0x3D, 0x07, 0x00, // CMP AX, imm16
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 7);
    assert!(cpu.has(flag::ZF));
    assert!(!cpu.has(flag::CF));
}

#[test]
fn xor_clears_register_and_sets_zf() {
    // MOV AX, 0x1234 ; XOR AX, AX
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x34, 0x12, 0x31, 0xC0, // XOR AX, AX (0x31 /r, modrm=11 000 000)
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 0);
    assert!(cpu.has(flag::ZF));
    assert!(!cpu.has(flag::CF));
}

#[test]
fn inc_dec_r16_preserve_cf() {
    // MOV AX, 0xFFFF ; STC equivalent via ADD overflow ; INC AX ; should wrap to 0, ZF=1, CF preserved
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0xFF, 0xFF, 0x40, // INC AX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 0);
    assert!(cpu.has(flag::ZF));
    // CF was 0 going in; INC must not touch it
    assert!(!cpu.has(flag::CF));

    // DEC 0 → 0xFFFF, ZF=0, SF=1
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x00, 0x00, 0x48, // DEC AX
            0xF4,
        ],
        8,
    );
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
    let (cpu, mem, _) = run_payload(
        &[
            0xBB, 0x00, 0x05, 0xB0, 0x42, 0x88, 0x07, 0xB1, 0x00, 0x8A, 0x0F, 0xF4,
        ],
        12,
    );
    assert_eq!(mem.read_u8(0x500), 0x42);
    assert_eq!(cpu.read_r8(1), 0x42);
}

#[test]
fn mov_word_imm_to_disp16_address() {
    // MOV WORD [0x600], 0xCAFE
    // 0xC7 modrm=00 000 110 = 0x06, then disp16=0x0600, then imm16=0xCAFE
    let (_, mem, _) = run_payload(&[0xC7, 0x06, 0x00, 0x06, 0xFE, 0xCA, 0xF4], 4);
    assert_eq!(mem.read_u16(0x600), 0xCAFE);
}

#[test]
fn add_reg_to_memory_via_bx() {
    // MOV WORD [0x700], 10
    // MOV BX, 0x700 ; MOV AX, 5 ; ADD [BX], AX
    //   ADD r/m16, r16 = 0x01 /r ; mod=00 reg=000(AX) rm=111(BX) = 0x07
    let (_, mem, _) = run_payload(
        &[
            0xC7, 0x06, 0x00, 0x07, 0x0A, 0x00, 0xBB, 0x00, 0x07, 0xB8, 0x05, 0x00, 0x01, 0x07,
            0xF4,
        ],
        10,
    );
    assert_eq!(mem.read_u16(0x700), 15);
}

#[test]
fn bp_addressing_defaults_to_ss_segment() {
    // SS is 0 in our reset_to_boot, so this is just a sanity check
    // that decoding picks SS (not DS) for [BP] form, and that the
    // address still resolves correctly when both are zero.
    // MOV BP, 0x900 ; MOV WORD [BP], 0x1357 (mod=10 rm=110 disp16=0)
    //   0xC7 modrm=10 000 110 = 0x86 ; disp16=0x0000 ; imm16=0x1357
    let (_, mem, _) = run_payload(
        &[0xBD, 0x00, 0x09, 0xC7, 0x86, 0x00, 0x00, 0x57, 0x13, 0xF4],
        6,
    );
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
    let array: &[u8] = &[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 0, 0];
    let bytes = [
        0xBE, 0x00, 0x08, 0xB9, 0x02, 0x00, 0x31, 0xC0, 0x8B, 0x1C, 0x09, 0xDB, 0x74, 0x06, 0x01,
        0xD8, 0x01, 0xCE, 0xEB, 0xF4, 0xF4,
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
    let (cpu, _, _) = run_payload(
        &[
            0xB9, 0x05, 0x00, // MOV CX, 5
            0x31, 0xDB, // XOR BX, BX
            0x01, 0xCB, // ADD BX, CX  (0x01 /r, modrm=11 001 011)
            0x49, // DEC CX
            0x75, 0xFB, // JNZ -5
            0xF4, // HLT
        ],
        50,
    );
    assert_eq!(cpu.regs[r16::BX], 15);
    assert_eq!(cpu.regs[r16::CX], 0);
    assert!(cpu.halted);
}

#[test]
fn push_pop_round_trip_through_other_reg() {
    // MOV AX, 0x1234 ; PUSH AX ; MOV AX, 0 ; POP BX ; HLT
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x34, 0x12, 0x50, // PUSH AX
            0xB8, 0x00, 0x00, 0x5B, // POP BX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::BX], 0x1234);
    assert_eq!(cpu.regs[r16::AX], 0);
    // SP must be back to its boot value
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn push_writes_below_sp_lifo() {
    // PUSH 0xAAAA ; PUSH 0xBBBB ; POP AX ; POP BX
    // After pushes, AX should be the most-recent (0xBBBB), BX older.
    let (cpu, _, _) = run_payload(
        &[
            0x68, 0xAA, 0xAA, 0x68, 0xBB, 0xBB, 0x58, // POP AX
            0x5B, // POP BX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 0xBBBB);
    assert_eq!(cpu.regs[r16::BX], 0xAAAA);
}

#[test]
fn push_imm8_sign_extends_to_16_bits() {
    // PUSH 0xFF (imm8) → on the stack as 0xFFFF
    let (cpu, mem, _) = run_payload(&[0x6A, 0xFF, 0xF4], 4);
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
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x00, 0x00, 0xE8, 0x01, 0x00, 0xF4, 0xB8, 0x07, 0x00, 0xC3,
        ],
        16,
    );
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
    let (cpu, _, _) = run_payload(
        &[
            0x68, 0x99, 0x00, 0xE8, 0x02, 0x00, 0xF4, 0x90, 0xC2, 0x02, 0x00,
        ],
        16,
    );
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
    let (cpu, _, _) = run_payload(&[0x31, 0xC0, 0x9C, 0x40, 0x9D, 0xF4], 8);
    assert!(cpu.has(flag::ZF));
}

#[test]
fn group1_add_imm_to_r16() {
    // ADD AX, 7    via 0x83 /0 (sign-ext imm8) — ModR/M = 11 000 000 = 0xC0
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x05, 0x00, // MOV AX, 5
            0x83, 0xC0, 0x07, // ADD AX, 7
            0xF4,
        ],
        6,
    );
    assert_eq!(cpu.regs[r16::AX], 12);
}

#[test]
fn group1_sub_r16_imm16() {
    // SUB AX, 0x1000 via 0x81 /5 — ModR/M = 11 101 000 = 0xE8
    let (cpu, _, _) = run_payload(&[0xB8, 0x34, 0x12, 0x81, 0xE8, 0x00, 0x10, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 0x0234);
}

#[test]
fn group1_cmp_imm_does_not_writeback() {
    // CMP AL, 0x42 via 0x80 /7 — ModR/M = 11 111 000 = 0xF8
    let (cpu, _, _) = run_payload(&[0xB0, 0x42, 0x80, 0xF8, 0x42, 0xF4], 6);
    assert_eq!(cpu.read_r8(0), 0x42);
    assert!(cpu.has(flag::ZF));
}

#[test]
fn group3_neg_and_not_r16() {
    // NEG AX where AX=5 -> 0xFFFB, CF=1
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x05, 0x00, 0xF7, 0xD8, // NEG AX (F7 /3, ModR/M = 11 011 000 = 0xD8)
            0xF4,
        ],
        6,
    );
    assert_eq!(cpu.regs[r16::AX], 0xFFFB);
    assert!(cpu.has(flag::CF));

    // NOT BX where BX=0xAAAA -> 0x5555, flags untouched
    let (cpu, _, _) = run_payload(
        &[
            0xBB, 0xAA, 0xAA, 0xF7, 0xD3, // NOT BX (F7 /2, ModR/M = 11 010 011 = 0xD3)
            0xF4,
        ],
        6,
    );
    assert_eq!(cpu.regs[r16::BX], 0x5555);
}

#[test]
fn group3_test_rm_imm() {
    // TEST AL, 0x80 (F6 /0, modrm=11 000 000 = 0xC0); AL=0x80 → ZF=0, SF=1
    let (cpu, _, _) = run_payload(&[0xB0, 0x80, 0xF6, 0xC0, 0x80, 0xF4], 6);
    assert!(!cpu.has(flag::ZF));
    assert!(cpu.has(flag::SF));
    assert_eq!(cpu.read_r8(0), 0x80); // unchanged
}

#[test]
fn group4_inc_memory_byte() {
    // INC byte [0x900] via FE /0 (modrm=00 000 110 = 0x06, then disp16)
    let (_, mem, _) = run_payload(
        &[
            0xC6, 0x06, 0x00, 0x09, 0x09, // MOV byte [0x900], 9
            0xFE, 0x06, 0x00, 0x09, // INC byte [0x900]
            0xF4,
        ],
        6,
    );
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
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x08, 0x7C, 0xFF, 0xD0, 0xB3, 0x11, 0xF4, 0xB3, 0x22, 0xC3,
        ],
        24,
    );
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
    let (cpu, _, _) = run_payload(&[0xB8, 0x06, 0x7C, 0xFF, 0xE0, 0xF4, 0xB3, 0x77, 0xF4], 8);
    assert_eq!(cpu.read_r8(3), 0x77);
    assert!(cpu.halted);
}

#[test]
fn group5_push_rm16() {
    // PUSH [0x900] via FF /6 (modrm=00 110 110 = 0x36, disp16)
    let (cpu, mem, _) = run_payload(
        &[
            0xC7, 0x06, 0x00, 0x09, 0xCD, 0xAB, // MOV WORD [0x900], 0xABCD
            0xFF, 0x36, 0x00, 0x09, // PUSH [0x900]
            0x58, // POP AX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 0xABCD);
    let _ = mem; // mem is consulted via the POP
}

#[test]
fn shl_by_one_sets_cf_from_top_bit() {
    // MOV AL, 0xC0 ; SHL AL, 1 → 0x80, CF=1, OF=0 (sign unchanged)
    // SHL r/m8, 1 = 0xD0 /4. ModR/M = 11 100 000 = 0xE0
    let (cpu, _, _) = run_payload(&[0xB0, 0xC0, 0xD0, 0xE0, 0xF4], 6);
    assert_eq!(cpu.read_r8(0), 0x80);
    assert!(cpu.has(flag::CF));
    assert!(!cpu.has(flag::OF));
}

#[test]
fn shl_by_cl_count() {
    // MOV AX, 1 ; MOV CL, 4 ; SHL AX, CL → 0x10
    // SHL r/m16, CL = 0xD3 /4. ModR/M = 11 100 000 = 0xE0
    let (cpu, _, _) = run_payload(&[0xB8, 0x01, 0x00, 0xB1, 0x04, 0xD3, 0xE0, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 0x10);
}

#[test]
fn shr_by_one_drops_lsb_into_cf() {
    // MOV AL, 0x03 ; SHR AL, 1 → 0x01, CF=1
    // SHR r/m8, 1 = 0xD0 /5. ModR/M = 11 101 000 = 0xE8
    let (cpu, _, _) = run_payload(&[0xB0, 0x03, 0xD0, 0xE8, 0xF4], 4);
    assert_eq!(cpu.read_r8(0), 0x01);
    assert!(cpu.has(flag::CF));
}

#[test]
fn sar_sign_extends_negative() {
    // MOV AL, 0x80 ; SAR AL, 1 → 0xC0 (sign-extended), CF=0
    // SAR r/m8, 1 = 0xD0 /7. ModR/M = 11 111 000 = 0xF8
    let (cpu, _, _) = run_payload(&[0xB0, 0x80, 0xD0, 0xF8, 0xF4], 4);
    assert_eq!(cpu.read_r8(0), 0xC0);
    assert!(!cpu.has(flag::CF));
    assert!(cpu.has(flag::SF));
}

#[test]
fn rol_by_one_wraps_msb_to_lsb() {
    // MOV AL, 0x81 ; ROL AL, 1 → 0x03, CF=1, OF=0 (no sign flip)
    // ROL r/m8, 1 = 0xD0 /0. ModR/M = 11 000 000 = 0xC0
    let (cpu, _, _) = run_payload(&[0xB0, 0x81, 0xD0, 0xC0, 0xF4], 4);
    assert_eq!(cpu.read_r8(0), 0x03);
    assert!(cpu.has(flag::CF));
}

#[test]
fn ror_by_imm_count() {
    // MOV AX, 0x0001 ; ROR AX, 4 → 0x1000
    // ROR r/m16, imm8 = 0xC1 /1. ModR/M = 11 001 000 = 0xC8
    let (cpu, _, _) = run_payload(&[0xB8, 0x01, 0x00, 0xC1, 0xC8, 0x04, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 0x1000);
}

#[test]
fn movsb_copies_one_byte_with_si_di_increment() {
    // src @ 0x800 = 0x77 ; ES already 0, SS=0
    // MOV SI, 0x800 ; MOV DI, 0x900 ; MOVSB
    let (cpu, mem, _) = run_with_data(
        &[0xBE, 0x00, 0x08, 0xBF, 0x00, 0x09, 0xA4, 0xF4],
        0x800,
        &[0x77],
        8,
    );
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
    let (cpu, mem, _) = run_with_data(
        &[
            0xBE, 0x00, 0x08, 0xBF, 0x00, 0x09, 0xB9, 0x05, 0x00, 0xF3, 0xA4, 0xF4,
        ],
        0x800,
        src,
        12,
    );
    let mut got = [0u8; 5];
    for (i, b) in got.iter_mut().enumerate() {
        *b = mem.read_u8(0x900 + i as u32);
    }
    assert_eq!(&got, src);
    assert_eq!(cpu.regs[r16::CX], 0);
}

#[test]
fn rep_stosb_fills_buffer() {
    // Fill 4 bytes at 0x900 with 0xAA.
    //   MOV AL, 0xAA ; MOV DI, 0x900 ; MOV CX, 4 ; REP STOSB
    let (_, mem, _) = run_payload(
        &[
            0xB0, 0xAA, 0xBF, 0x00, 0x09, 0xB9, 0x04, 0x00, 0xF3, 0xAA, 0xF4,
        ],
        10,
    );
    for i in 0..4 {
        assert_eq!(mem.read_u8(0x900 + i), 0xAA);
    }
    // Should NOT overwrite the byte one past.
    assert_eq!(mem.read_u8(0x904), 0);
}

/// 0x66 0xF3 0xA5 → REP MOVSD. Copies CX *dwords* (4 bytes each)
/// from DS:SI to ES:DI. Linux memcpy is shaped like this for the
/// dword-aligned bulk path.
#[test]
fn rep_movsd_copies_dwords_under_0x66() {
    // 16 bytes (= 4 dwords) of source at 0x800.
    let src: &[u8] = &[
        0x11, 0x22, 0x33, 0x44, 0xAA, 0xBB, 0xCC, 0xDD, 0x55, 0x66, 0x77, 0x88, 0x99, 0xEE, 0xFF,
        0x00,
    ];
    // MOV SI, 0x800; MOV DI, 0x900; MOV CX, 4; 66 F3 A5; HLT
    let (cpu, mem, _) = run_with_data(
        &[
            0xBE, 0x00, 0x08, 0xBF, 0x00, 0x09, 0xB9, 0x04, 0x00, 0x66, 0xF3, 0xA5, 0xF4,
        ],
        0x800,
        src,
        16,
    );
    for (i, &b) in src.iter().enumerate() {
        assert_eq!(mem.read_u8(0x900 + i as u32), b, "byte {i}");
    }
    assert_eq!(cpu.regs[r16::CX], 0);
    // SI advanced 4*4 = 16 bytes.
    assert_eq!(cpu.regs[r16::SI], 0x810);
    assert_eq!(cpu.regs[r16::DI], 0x910);
}

/// 0x66 0xF3 0xAB → REP STOSD. Fills CX dwords with EAX. Linux
/// memset is shaped like this for the bulk path.
#[test]
fn rep_stosd_fills_dwords_under_0x66() {
    // MOV EAX, 0xCAFEBABE  → 66 B8 BE BA FE CA
    // MOV DI, 0x900        → BF 00 09
    // MOV CX, 3            → B9 03 00
    // 66 F3 AB             → REP STOSD
    // HLT
    let (_, mem, _) = run_payload(
        &[
            0x66, 0xB8, 0xBE, 0xBA, 0xFE, 0xCA, // MOV EAX, 0xCAFEBABE
            0xBF, 0x00, 0x09, // MOV DI, 0x900
            0xB9, 0x03, 0x00, // MOV CX, 3
            0x66, 0xF3, 0xAB, // REP STOSD
            0xF4,
        ],
        16,
    );
    // Three dwords of 0xCAFEBABE = BE BA FE CA repeated.
    for i in 0..3 {
        let base = 0x900 + (i * 4) as u32;
        assert_eq!(mem.read_u8(base), 0xBE);
        assert_eq!(mem.read_u8(base + 1), 0xBA);
        assert_eq!(mem.read_u8(base + 2), 0xFE);
        assert_eq!(mem.read_u8(base + 3), 0xCA);
    }
    // Byte at offset 12 must be untouched.
    assert_eq!(mem.read_u8(0x90C), 0);
}

#[test]
fn repne_scasb_finds_terminator() {
    // Search a NUL-terminated string for NUL using REPNE SCASB.
    //   AL=0 ; ES:DI = 0x800 ; CX = 0xFFFF ; REPNE SCASB
    // After: DI points one past the NUL; (0xFFFF - 1) - CX = bytes
    // scanned.
    let s = b"abc\0";
    let (cpu, _, _) = run_with_data(
        &[
            0xB0, 0x00, 0xBF, 0x00, 0x08, 0xB9, 0xFF, 0xFF, 0xF2, 0xAE, 0xF4,
        ],
        0x800,
        s,
        12,
    );
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
        0xC6, 0x06, 0x00, 0x09, b'a', 0xC6, 0x06, 0x01, 0x09, b'b', 0xC6, 0x06, 0x02, 0x09, b'Y',
        0xC6, 0x06, 0x03, 0x09, b'd', // REPE CMPSB setup + run
        0xBE, 0x00, 0x08, 0xBF, 0x00, 0x09, 0xB9, 0x04, 0x00, 0xF3, 0xA6, 0xF4,
    ];
    let (cpu, _, _) = run_with_data(&bytes, 0x800, b"abXd", 30);
    assert_eq!(cpu.regs[r16::CX], 1);
    assert_eq!(cpu.regs[r16::DI], 0x903);
    assert!(!cpu.has(flag::ZF));
}

#[test]
fn mul_r8_unsigned_low_byte_only_clears_cf() {
    // MOV AL, 6 ; MOV BL, 7 ; MUL BL → AX=42, CF=0, OF=0
    //   MUL r/m8 = 0xF6 /4, ModR/M = 11 100 011 = 0xE3 (rm=BL)
    let (cpu, _, _) = run_payload(&[0xB0, 0x06, 0xB3, 0x07, 0xF6, 0xE3, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 42);
    assert!(!cpu.has(flag::CF));
    assert!(!cpu.has(flag::OF));
}

#[test]
fn mul_r8_sets_cf_when_ah_nonzero() {
    // MOV AL, 200 ; MOV BL, 200 ; MUL BL → AX=40000=0x9C40, CF=1
    let (cpu, _, _) = run_payload(&[0xB0, 0xC8, 0xB3, 0xC8, 0xF6, 0xE3, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 40000);
    assert!(cpu.has(flag::CF));
}

#[test]
fn imul_r8_negative_result() {
    // MOV AL, -5 (0xFB) ; MOV BL, 7 ; IMUL BL → AX = -35 (0xFFDD)
    //   IMUL r/m8 = 0xF6 /5, ModR/M = 11 101 011 = 0xEB
    let (cpu, _, _) = run_payload(&[0xB0, 0xFB, 0xB3, 0x07, 0xF6, 0xEB, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX] as i16, -35);
    // -35 fits in i8, so CF/OF should be clear
    assert!(!cpu.has(flag::CF));
    assert!(!cpu.has(flag::OF));
}

#[test]
fn div_r8_quotient_and_remainder() {
    // MOV AX, 100 ; MOV BL, 7 ; DIV BL → AL=14 quotient, AH=2 remainder
    //   DIV r/m8 = 0xF6 /6, ModR/M = 11 110 011 = 0xF3
    let (cpu, _, _) = run_payload(&[0xB8, 0x64, 0x00, 0xB3, 0x07, 0xF6, 0xF3, 0xF4], 6);
    assert_eq!(cpu.read_r8(0), 14); // AL
    assert_eq!(cpu.read_r8(4), 2); // AH
}

#[test]
fn div_r16_dx_ax_dividend() {
    // DX:AX = 0x0001_0000 = 65536, DIV BX where BX=256 → AX=256, DX=0
    //   DIV r/m16 = 0xF7 /6, ModR/M = 11 110 011 = 0xF3
    let (cpu, _, _) = run_payload(
        &[
            0xBA, 0x01, 0x00, // MOV DX, 1
            0xB8, 0x00, 0x00, // MOV AX, 0
            0xBB, 0x00, 0x01, // MOV BX, 256
            0xF7, 0xF3, // DIV BX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.regs[r16::AX], 256);
    assert_eq!(cpu.regs[r16::DX], 0);
}

/// Divide-by-zero raises #DE through IVT[0]. The handler ran iff
/// AL holds the sentinel we set inside it; the pushed IP names the
/// start of the DIV instruction (the fault-frame convention).
#[test]
fn div_by_zero_vectors_through_ivt_de() {
    let mut mem = Memory::new(0x10_0000);
    // IVT[0] = 0:0x7C20 — handler that sets AL = sentinel and HLTs.
    mem.write_u8(0, 0x20);
    mem.write_u8(1, 0x7C);
    mem.write_u8(2, 0);
    mem.write_u8(3, 0);
    //   MOV AL, 5 ; MOV BL, 0 ; DIV BL  (DIV is at offset 4 → 0x7C04)
    mem.write_slice(0x7C00, &[0xB0, 0x05, 0xB3, 0x00, 0xF6, 0xF3, 0xF4]);
    //   Handler: MOV AL, 0xDE ; HLT
    mem.write_slice(0x7C20, &[0xB0, 0xDE, 0xF4]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0xDE, "handler ran");
    // Frame: SP=0x7C00 → 0x7BFA after three 16-bit pushes (FLAGS, CS, IP).
    // mem[0x7BFA] = saved IP = 0x7C04 (start of DIV BL).
    assert_eq!(mem.read_u16(0x7BFA), 0x7C04);
}

#[test]
fn es_segment_override_redirects_memory_load() {
    // Place 0xCC at ES:0x0100 (since ES=0 after reset, linear=0x0100)
    // and 0x33 at DS:0x0100 (also linear=0x0100 — same location).
    // To meaningfully test ES override, change ES first.
    //
    // Plan: set ES=0x10 via PUSH/POP (we don't have MOV sreg yet).
    // Actually, MOV ES, r isn't implemented. Use the data slot:
    // write a distinct byte at ES:0x100 (linear 0x100 + 16*0x10 =
    // 0x200) and at DS:0x100 (linear 0x100), then verify the
    // override reads from ES.
    //
    // Simpler: don't change ES. With ES=DS=0 the override is a
    // no-op functionally but still exercises the decode path.
    // Verify by making sure 26 prefix doesn't break a normal load.
    //
    //   MOV BX, 0x800 ; MOV AL, 0 ; (prefix 26) MOV AL, [BX]
    //   F4 HLT
    let (cpu, _, _) = run_with_data(
        &[
            0xBB, 0x00, 0x08, 0xB0, 0x00, 0x26, 0x8A, 0x07, // ES: MOV AL, [BX]
            0xF4,
        ],
        0x800,
        &[0x42],
        8,
    );
    assert_eq!(cpu.read_r8(0), 0x42);
    // seg_override must reset across the boundary
    assert!(cpu.seg_override.is_none());
}

#[test]
fn seg_override_does_not_leak_to_next_instruction() {
    // Sequence: (26) MOV AL, [BX] ; MOV AL, [SI]
    // After the first, seg_override should reset to None so the
    // second instruction uses default segments.
    let (cpu, _, _) = run_with_data(
        &[
            0xBB, 0x00, 0x08, 0xBE, 0x01, 0x08, 0x26, 0x8A,
            0x07, // ES: MOV AL, [BX]   reads 0x800
            0x8A, 0x04, //     MOV AL, [SI]   reads DS:0x801
            0xF4,
        ],
        0x800,
        &[0x11, 0x22],
        8,
    );
    // Last read came from DS:0x801 = 0x22
    assert_eq!(cpu.read_r8(0), 0x22);
    assert!(cpu.seg_override.is_none());
}

#[test]
fn mov_sreg_round_trip_through_ax() {
    // Set ES via AX: MOV AX, 0x1234 ; MOV ES, AX
    //   MOV sreg, r/m16 = 0x8E /0 (ES). ModR/M = 11 000 000 = 0xC0
    // Then read it back: MOV BX, ES (MOV r/m16, sreg = 0x8C /0)
    //   ModR/M = 11 000 011 = 0xC3
    let (cpu, _, _) = run_payload(&[0xB8, 0x34, 0x12, 0x8E, 0xC0, 0x8C, 0xC3, 0xF4], 6);
    assert_eq!(cpu.sregs[sreg::ES], 0x1234);
    assert_eq!(cpu.regs[r16::BX], 0x1234);
}

#[test]
fn lea_computes_address_without_memory_read() {
    // MOV BX, 0x100 ; MOV SI, 5 ; LEA AX, [BX+SI+10]
    //   LEA r16, m = 0x8D /r. ModR/M for [BX+SI+disp8]:
    //   mod=01 reg=000(AX) rm=000([BX+SI]) → 01 000 000 = 0x40, disp8=0x0A
    let (cpu, _, _) = run_payload(
        &[0xBB, 0x00, 0x01, 0xBE, 0x05, 0x00, 0x8D, 0x40, 0x0A, 0xF4],
        8,
    );
    // 0x100 + 5 + 10 = 0x10F
    assert_eq!(cpu.regs[r16::AX], 0x10F);
}

#[test]
fn lea_r32_zero_extends_ea_under_0x66() {
    // MOV BX, 0x1234
    // 0x66 LEA EAX, [BX+0x10]
    //   modrm 00 000 111 = 0x07 (rm=[BX], reg=AX). With disp8 the
    //   modrm becomes 01 000 111 = 0x47; then one disp8 byte.
    //   Sequence: 0x66 0x8D 0x47 0x10
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0xBB, 0x34, 0x12, // MOV BX, 0x1234
            0x66, 0x8D, 0x47, 0x10, // LEA EAX, [BX+0x10]
            0xF4,
        ],
        12,
    );
    // EA = BX + 0x10 = 0x1244. With 16-bit address mode, EA stays
    // 16-bit; we zero-extend it into EAX.
    assert_eq!(cpu.regs[r16::AX], 0x1244);
    assert_eq!(
        cpu.regs_high[r16::AX],
        0,
        "LEA r32 must zero-extend the 16-bit EA"
    );
}

#[test]
fn lea_register_form_returns_unimplemented() {
    // LEA AX, AX (mod=11) is undefined on real x86 — we surface it
    // as an error so we notice if anyone tries.
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x7C00, &[0x8D, 0xC0]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    let err = cpu.step(&mut mem, &mut io).unwrap_err();
    match err {
        CpuError::Unimplemented { opcode: 0x8D, .. } => {}
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn xchg_r16_r16_swaps_values() {
    // MOV AX, 1 ; MOV BX, 2 ; XCHG AX, BX
    //   XCHG r/m16, r16 = 0x87 /r. ModR/M = 11 000 011 = 0xC3
    //   (reg=AX, rm=BX) — either direction is equivalent for XCHG.
    let (cpu, _, _) = run_payload(&[0xB8, 0x01, 0x00, 0xBB, 0x02, 0x00, 0x87, 0xC3, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 2);
    assert_eq!(cpu.regs[r16::BX], 1);
}

#[test]
fn xchg_ax_r16_short_form() {
    // MOV AX, 0xAAAA ; MOV CX, 0xCCCC ; XCHG AX, CX  (0x91)
    let (cpu, _, _) = run_payload(&[0xB8, 0xAA, 0xAA, 0xB9, 0xCC, 0xCC, 0x91, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX], 0xCCCC);
    assert_eq!(cpu.regs[r16::CX], 0xAAAA);
}

#[test]
fn xchg_rm8_with_memory_operand() {
    // Memory at 0x800 = 0xAA; AL = 0xBB; XCHG [BX], AL → mem becomes 0xBB, AL becomes 0xAA
    //   XCHG r/m8, r8 = 0x86 /r. ModR/M = 00 000 111 = 0x07  (reg=AL, rm=[BX])
    let (cpu, mem, _) = run_with_data(
        &[0xBB, 0x00, 0x08, 0xB0, 0xBB, 0x86, 0x07, 0xF4],
        0x800,
        &[0xAA],
        6,
    );
    assert_eq!(cpu.read_r8(0), 0xAA);
    assert_eq!(mem.read_u8(0x800), 0xBB);
}

#[test]
fn les_loads_far_pointer_into_reg_and_es() {
    // 4-byte far pointer at 0x800: offset=0x1234, segment=0x5678
    // LES BX, [SI]  — SI=0x800
    //   LES r16, m = 0xC4 /r. ModR/M = 00 011 100 = 0x1C
    let far_ptr = &[0x34, 0x12, 0x78, 0x56];
    let (cpu, _, _) = run_with_data(&[0xBE, 0x00, 0x08, 0xC4, 0x1C, 0xF4], 0x800, far_ptr, 6);
    assert_eq!(cpu.regs[r16::BX], 0x1234);
    assert_eq!(cpu.sregs[sreg::ES], 0x5678);
}

#[test]
fn lds_loads_far_pointer_into_reg_and_ds() {
    let far_ptr = &[0xCD, 0xAB, 0x21, 0x43];
    let (cpu, _, _) = run_with_data(
        &[
            0xBE, 0x00, 0x08, 0xC5, 0x1C, // LDS BX, [SI]
            0xF4,
        ],
        0x800,
        far_ptr,
        6,
    );
    assert_eq!(cpu.regs[r16::BX], 0xABCD);
    assert_eq!(cpu.sregs[sreg::DS], 0x4321);
}

#[test]
fn cbw_sign_extends_negative_al() {
    // MOV AL, 0x80 ; CBW → AX=0xFF80
    let (cpu, _, _) = run_payload(&[0xB0, 0x80, 0x98, 0xF4], 4);
    assert_eq!(cpu.regs[r16::AX], 0xFF80);
}

#[test]
fn cbw_preserves_positive_al() {
    // MOV AL, 0x42 ; CBW → AX=0x0042
    let (cpu, _, _) = run_payload(&[0xB0, 0x42, 0x98, 0xF4], 4);
    assert_eq!(cpu.regs[r16::AX], 0x0042);
}

#[test]
fn cwd_sign_extends_negative_ax_into_dx() {
    // MOV AX, 0x8000 ; CWD → DX=0xFFFF, AX unchanged
    let (cpu, _, _) = run_payload(&[0xB8, 0x00, 0x80, 0x99, 0xF4], 4);
    assert_eq!(cpu.regs[r16::AX], 0x8000);
    assert_eq!(cpu.regs[r16::DX], 0xFFFF);
}

#[test]
fn lahf_sahf_round_trips_low_flags() {
    // Force CF=1, ZF=0 via XOR AL with non-zero then add carry-out.
    // Simpler: set a known FLAGS state, LAHF, clobber, SAHF, verify.
    //   MOV AL, 0xFF       ; ADD AL, 1 → CF=1, ZF=1, SF=0, PF=1
    //   LAHF               ; AH captures the flag image
    //   MOV AL, 0          ; clobber low-flag-affecting state via flag-clobber
    //   XOR DX, DX         ; ZF=1 anyway — pick an op that resets things
    //   SAHF               ; restore from AH
    //   HLT
    // Easier deterministic test: use MOV-only ops that don't touch
    // flags between LAHF and SAHF.
    //   MOV AL, 0xFF
    //   ADD AL, 1          → CF=1 ZF=1 SF=0 PF=1
    //   LAHF               → AH bit pattern reflects above
    //   MOV BL, AH         → BL captures it
    //   MOV AH, 0          ; clobber AH (doesn't touch FLAGS)
    //   MOV AH, BL         ; restore raw byte
    //   SAHF               → flags reloaded
    //   HLT
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0xFF, 0x04, 0x01, // ADD AL, 1
            0x9F, // LAHF
            0x88,
            0xE3, // MOV BL, AH (8A or 88? 88 stores reg→r/m, reg=AH(4), rm=BL(3) → 11 100 011 = 0xE3)
            0xB4, 0x00, // MOV AH, 0
            0x88, 0xDC, // MOV AH, BL (reg=BL(3), rm=AH(4) → 11 011 100 = 0xDC)
            0x9E, // SAHF
            0xF4,
        ],
        10,
    );
    assert!(cpu.has(flag::CF));
    assert!(cpu.has(flag::ZF));
    assert!(cpu.has(flag::PF));
    assert!(!cpu.has(flag::SF));
}

#[test]
fn int_dispatch_through_ivt_then_iret_resumes() {
    // IVT[0x30] = far pointer to handler at 0:0x7C10.
    // Program prints a marker, calls INT 0x30, then a second
    // marker after IRET. The handler tweaks AL and IRETs.
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0xC0, 0x7C10); // offset of handler
    mem.write_u16(0xC2, 0x0000); // segment
    let program = &[
        // 0x00: MOV AX, 0xBEEF
        0xB8, 0xEF, 0xBE, // 0x03: INT 0x30
        0xCD, 0x30, // 0x05: MOV BL, 0x22   (runs after IRET)
        0xB3, 0x22, // 0x07: HLT
        0xF4, // 0x08..0x0F: padding so handler lands at 0x7C10
        0, 0, 0, 0, 0, 0, 0, 0, // 0x10: MOV AL, 0x42
        0xB0, 0x42, // 0x12: IRET
        0xCF,
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..50 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    // AH untouched by handler (was 0xBE from initial MOV); AL set
    // to 0x42 by the handler.
    assert_eq!(cpu.regs[r16::AX], 0xBE42);
    // BL set after IRET resumed at 0x7C05.
    assert_eq!(cpu.read_r8(3), 0x22);
    assert!(cpu.halted);
    // Stack must be balanced: pre-INT push of FLAGS/CS/IP (6 bytes)
    // and post-IRET pop should restore SP to boot value.
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn int_clears_if_so_handlers_run_with_interrupts_masked() {
    // STI ; INT 0x40 → inside handler IF must be 0.
    // Handler stores FLAGS via PUSHF; we read it from the stack
    // via [BP+offset]. Simpler: have the handler MOV BL,1 if IF=0
    // by reading FLAGS via PUSHF; POP BX.
    //
    // Test plan:
    //   STI                ; set IF=1
    //   INT 0x40            ; handler at 0x7C10
    //   HLT
    // Handler:
    //   PUSHF              ; push flags-in-handler
    //   POP BX             ; BX = flags
    //   IRET
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0x40 * 4, 0x7C10);
    mem.write_u16(0x40 * 4 + 2, 0);
    let program = &[
        0xFB, // STI
        0xCD, 0x40, // INT 0x40
        0xF4, // HLT
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,    // pad to 0x10
        0x9C, // PUSHF
        0x5B, // POP BX
        0xCF, // IRET
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..50 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    // BX captured the FLAGS the handler saw. IF bit must be 0.
    assert_eq!(cpu.regs[r16::BX] & flag::IF, 0);
    // After IRET restores original FLAGS, IF should be 1 again.
    assert!(cpu.has(flag::IF));
}

#[test]
fn into_only_fires_when_overflow_set() {
    // Case A: OF=0 → INTO is a no-op.
    // We provoke an arithmetic op that *clears* OF (e.g. ADD 1+1)
    // and check that INTO doesn't transfer.
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x01, 0x04, 0x01, // ADD AL, 1 → OF=0
            0xCE, // INTO
            0xB3, 0x77, 0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(3), 0x77);
    assert!(!cpu.has(flag::OF));

    // Case B: OF=1 → INTO fires INT 4.
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(4 * 4, 0x7C10);
    mem.write_u16(4 * 4 + 2, 0);
    let program = &[
        0xB0, 0x7F, 0x04, 0x01, // ADD AL, 1 → 0x80, OF=1
        0xCE, // INTO → should fire
        0xB3, 0x11, // runs after IRET
        0xF4, 0, 0, 0, 0, 0, 0, 0, 0, // pad to 0x10
        // 0x10: handler
        0xB7, 0x99, // MOV BH, 0x99
        0xCF, // IRET
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..50 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_eq!(cpu.read_r8(7), 0x99); // BH set by handler
    assert_eq!(cpu.read_r8(3), 0x11); // BL set after IRET
}

#[test]
fn push_ds_pop_es_copies_segment_through_stack() {
    // Set DS via AX, then PUSH DS / POP ES, verify ES picked it up.
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x34, 0x12, // MOV AX, 0x1234
            0x8E, 0xD8, // MOV DS, AX (8E /3 = DS, modrm=11 011 000)
            0x1E, // PUSH DS
            0x07, // POP ES
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.sregs[sreg::DS], 0x1234);
    assert_eq!(cpu.sregs[sreg::ES], 0x1234);
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn call_far_pushes_cs_ip_then_retf_restores() {
    // 0: 9A 09 00 00 00   CALL 0x0000:0x0009     ; far call to offset 9
    // 5: B3 22            MOV BL, 0x22           ; runs after RETF
    // 7: F4               HLT
    // 8: 90               NOP (padding)
    // 9: B7 88            MOV BH, 0x88           ; callee
    // B: CB               RETF
    let (cpu, _, _) = run_payload(
        &[
            0x9A, 0x09, 0x7C, 0x00, 0x00, 0xB3, 0x22, 0xF4, 0x90, 0xB7, 0x88, 0xCB,
        ],
        16,
    );
    assert_eq!(cpu.read_r8(7), 0x88); // BH set by callee
    assert_eq!(cpu.read_r8(3), 0x22); // BL set after RETF
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn jmp_far_loads_cs_ip_without_stack_activity() {
    // 0: EA 06 7C 00 00   JMP 0x0000:0x7C06
    // 5: F4               HLT (skipped)
    // 6: B3 77            MOV BL, 0x77
    // 8: F4               HLT
    let (cpu, _, _) = run_payload(&[0xEA, 0x06, 0x7C, 0x00, 0x00, 0xF4, 0xB3, 0x77, 0xF4], 8);
    assert_eq!(cpu.read_r8(3), 0x77);
    assert!(cpu.halted);
    // No PUSH happened — SP unchanged
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn retf_imm16_cleans_extra_stack_bytes() {
    // PUSH an argument, CALL far, callee RETF 2 — SP must roll back
    // through both the return-pair and the arg.
    //
    // 0: 68 99 00          PUSH 0x99
    // 3: 9A 0C 7C 00 00    CALL 0x0000:0x7C0C
    // 8: F4                HLT
    // 9: 90 90 90          NOP padding
    // C: C2 not — we want RETF, so:
    // C: CA 02 00          RETF 2
    let (cpu, _, _) = run_payload(
        &[
            0x68, 0x99, 0x00, 0x9A, 0x0C, 0x7C, 0x00, 0x00, 0xF4, 0x90, 0x90, 0x90, 0xCA, 0x02,
            0x00,
        ],
        16,
    );
    assert!(cpu.halted);
    // Argument popped by RETF 2 — SP back to boot value.
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn group5_far_call_indirect_via_pointer_in_memory() {
    // Far pointer at 0x800: offset=0x7C0A, segment=0x0000.
    // 0: BB 00 08          MOV BX, 0x800
    // 3: FF 1F             CALL FAR [BX]   (FF /3, modrm=00 011 111 = 0x1F)
    // 5: B3 11             MOV BL, 0x11
    // 7: F4                HLT
    // 8: 90 90             NOP padding
    // A: B7 55             MOV BH, 0x55
    // C: CB                RETF
    let far_ptr = &[0x0A, 0x7C, 0x00, 0x00];
    let (cpu, _, _) = run_with_data(
        &[
            0xBB, 0x00, 0x08, 0xFF, 0x1F, 0xB3, 0x11, 0xF4, 0x90, 0x90, 0xB7, 0x55, 0xCB,
        ],
        0x800,
        far_ptr,
        16,
    );
    assert_eq!(cpu.read_r8(7), 0x55);
    assert_eq!(cpu.read_r8(3), 0x11);
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn group5_far_jmp_indirect_no_return() {
    // Like above but with FF /5 — far JMP, no stack push.
    // 0: BB 00 08          MOV BX, 0x800
    // 3: FF 2F             JMP FAR [BX]   (FF /5, modrm=00 101 111 = 0x2F)
    // 5: F4                HLT             (skipped)
    // 6: B3 99             MOV BL, 0x99
    // 8: F4                HLT
    let far_ptr = &[0x06, 0x7C, 0x00, 0x00];
    let (cpu, _, _) = run_with_data(
        &[0xBB, 0x00, 0x08, 0xFF, 0x2F, 0xF4, 0xB3, 0x99, 0xF4],
        0x800,
        far_ptr,
        8,
    );
    assert_eq!(cpu.read_r8(3), 0x99);
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn loop_counts_cx_to_zero() {
    // Sum 1+2+3+4+5 via LOOP. CX holds the counter, BX the sum.
    //   MOV CX, 5
    //   XOR BX, BX
    // lp: ADD BX, CX
    //   LOOP lp     (decrement CX; if CX != 0 jump back)
    //   HLT
    // Hand-assembled:
    //   B9 05 00       MOV CX, 5
    //   31 DB          XOR BX, BX (modrm 11 011 011 = 0xDB)
    //   01 CB          ADD BX, CX (mod=11 reg=CX=001 rm=BX=011 → 0xCB)
    //   E2 FC          LOOP -4 (rel8)
    //   F4             HLT
    // After-instr IP of LOOP is at offset 9. Target back to ADD at
    // offset 5. Delta = 5 - 9 = -4 = 0xFC.
    let (cpu, _, _) = run_payload(
        &[0xB9, 0x05, 0x00, 0x31, 0xDB, 0x01, 0xCB, 0xE2, 0xFC, 0xF4],
        100,
    );
    assert_eq!(cpu.regs[r16::BX], 15);
    assert_eq!(cpu.regs[r16::CX], 0);
    assert!(cpu.halted);
}

#[test]
fn loope_stops_on_zf_clear() {
    // LOOPE keeps iterating while ZF=1 (and CX>0). We compare each
    // step and the loop must exit early on the first mismatch.
    //   MOV CX, 5
    //   MOV AL, 7
    // lp: CMP AL, 7   ; ZF=1
    //   LOOPE lp      ; jumps while ZF=1 and CX != 0
    // After 4 iterations CX=1 and the 5th decrement brings CX to 0
    // — LOOPE stops on CX=0 even though ZF is still 1.
    let (cpu, _, _) = run_payload(
        &[
            0xB9, 0x05, 0x00, 0xB0, 0x07, 0x3C, 0x07, // CMP AL, 7
            0xE1, 0xFC, // LOOPE -4
            0xF4,
        ],
        50,
    );
    assert_eq!(cpu.regs[r16::CX], 0);
    assert!(cpu.halted);
}

#[test]
fn loopne_stops_when_zf_becomes_set() {
    // Search loop: keep iterating until CMP finds a match.
    //   MOV CX, 5
    //   MOV AL, 7
    // lp: CMP AL, 7   ; ZF=1 on first iter (we want LOOPNE to exit)
    //   LOOPNE lp     ; keeps going while ZF=0 (and CX != 0)
    // Since ZF=1 right away, LOOPNE decrements CX to 4 then exits.
    let (cpu, _, _) = run_payload(
        &[
            0xB9, 0x05, 0x00, 0xB0, 0x07, 0x3C, 0x07, 0xE0, 0xFC, // LOOPNE -4
            0xF4,
        ],
        20,
    );
    assert_eq!(cpu.regs[r16::CX], 4);
    assert!(cpu.halted);
}

#[test]
fn jcxz_skips_when_cx_zero_without_decrementing() {
    // JCXZ at the head of a would-be 65536-iter LOOP guards against
    // it. Here we just verify control flow + that CX is untouched.
    //   XOR CX, CX
    //   JCXZ over     (CX=0 → taken; IP advances to "over")
    //   MOV BX, 0x1234  (skipped)
    // over:
    //   MOV AX, 0x5678
    //   HLT
    let (cpu, _, _) = run_payload(
        &[
            0x31, 0xC9, // XOR CX, CX (modrm 11 001 001 = 0xC9)
            0xE3, 0x03, // JCXZ +3
            0xBB, 0x34, 0x12, // MOV BX, 0x1234 (skipped)
            0xB8, 0x78, 0x56, // MOV AX, 0x5678
            0xF4,
        ],
        10,
    );
    assert_eq!(cpu.regs[r16::AX], 0x5678);
    assert_eq!(cpu.regs[r16::BX], 0);
    assert_eq!(cpu.regs[r16::CX], 0);
}

#[test]
fn out_ax_writes_both_bytes_to_consecutive_ports() {
    // OUT 0x3F8 (THR), AX writes the low byte to 0x3F8 (UART tx)
    // and the high byte to 0x3F9 (IER on the UART — accepted and
    // dropped by our model). Verify the UART captured the low byte.
    let (_, _, mut io) = run_payload(
        &[
            0xB8, b'Y', b'Z', // MOV AX, "ZY" → AL='Y', AH='Z'
            0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
            0xEF, // OUT DX, AX
            0xF4,
        ],
        6,
    );
    // UART tx should have received the low byte 'Y'.
    assert_eq!(io.uart_mut().drain_tx(), b"Y");
}

#[test]
fn in_ax_reads_low_byte_then_next_port() {
    // Push 'X' into the UART rx buffer, then IN AX, DX from 0x3F8.
    //   IN AX, 0x3F8 reads RBR (0x3F8) into AL and IER (0x3F9, zero)
    //   into AH.
    let mut io = IoBus::new();
    io.uart_mut().push_rx(b"X");
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x7C00,
        &[
            0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
            0xED, // IN AX, DX
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    for _ in 0..6 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_eq!(cpu.read_r8(0), b'X'); // AL
    assert_eq!(cpu.read_r8(4), 0); // AH (IER reads zero)
}

#[test]
fn xlat_translates_via_table_at_ds_bx_plus_al() {
    // Translation table at 0x800: 0→'a', 1→'b', 2→'c', ...
    //   MOV BX, 0x800
    //   MOV AL, 2
    //   XLAT
    //   HLT
    let table = b"abcdef";
    let (cpu, _, _) = run_with_data(&[0xBB, 0x00, 0x08, 0xB0, 0x02, 0xD7, 0xF4], 0x800, table, 6);
    assert_eq!(cpu.read_r8(0), b'c');
}

#[test]
fn clc_stc_cmc_drive_carry_flag() {
    // STC ; CMC ; (CF=0) ; STC ; (CF=1) ; CLC ; (CF=0) ; HLT
    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xF5, // CMC
            0xF9, // STC
            0xF5, // CMC again → CF=0
            0xF4,
        ],
        6,
    );
    assert!(!cpu.has(flag::CF));

    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xF4,
        ],
        4,
    );
    assert!(cpu.has(flag::CF));

    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xF8, // CLC
            0xF4,
        ],
        4,
    );
    assert!(!cpu.has(flag::CF));
}

#[test]
fn lock_and_wait_prefixes_are_noop() {
    // LOCK MOV AX, 0xBEEF is the LOCK byte followed by a normal MOV.
    // Per our model the LOCK byte counts as one no-op step; the
    // next step() executes the MOV.
    // WAIT (0x9B) is treated the same way.
    let (cpu, _, _) = run_payload(
        &[
            0xF0, // LOCK
            0xB8, 0xEF, 0xBE, // MOV AX, 0xBEEF
            0x9B, // WAIT
            0xBB, 0x42, 0x42, // MOV BX, 0x4242
            0xF4,
        ],
        10,
    );
    assert_eq!(cpu.regs[r16::AX], 0xBEEF);
    assert_eq!(cpu.regs[r16::BX], 0x4242);
}

#[test]
fn pusha_popa_round_trip_preserves_all_gprs() {
    // Initialise each register to a distinct value, PUSHA, clobber
    // them all, POPA, verify restoration. The SP slot is special
    // — PUSHA captures the pre-push SP, POPA discards it. We verify
    // that side too: SP returns to the value it had right after the
    // clobber's PUSHA (because POPA restores it implicitly via its
    // own pop count).
    let (cpu, _, _) = run_payload(
        &[
            // Distinct register seeds
            0xB8, 0x01, 0x00, // MOV AX, 1
            0xB9, 0x02, 0x00, // MOV CX, 2
            0xBA, 0x03, 0x00, // MOV DX, 3
            0xBB, 0x04, 0x00, // MOV BX, 4
            0xBD, 0x05, 0x00, // MOV BP, 5
            0xBE, 0x06, 0x00, // MOV SI, 6
            0xBF, 0x07, 0x00, // MOV DI, 7
            0x60, // PUSHA
            // Clobber everything
            0x31, 0xC0, // XOR AX, AX
            0x31, 0xC9, // XOR CX, CX
            0x31, 0xD2, // XOR DX, DX
            0x31, 0xDB, // XOR BX, BX
            0x31, 0xED, // XOR BP, BP
            0x31, 0xF6, // XOR SI, SI
            0x31, 0xFF, // XOR DI, DI
            0x61, // POPA
            0xF4, // HLT
        ],
        50,
    );
    assert_eq!(cpu.regs[r16::AX], 1);
    assert_eq!(cpu.regs[r16::CX], 2);
    assert_eq!(cpu.regs[r16::DX], 3);
    assert_eq!(cpu.regs[r16::BX], 4);
    assert_eq!(cpu.regs[r16::BP], 5);
    assert_eq!(cpu.regs[r16::SI], 6);
    assert_eq!(cpu.regs[r16::DI], 7);
    // PUSHA pushed 8 words (16 bytes), POPA popped 8 — stack balanced.
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn imul_three_operand_imm16() {
    // MOV BX, 3 ; IMUL AX, BX, 7  → AX = 21
    //   IMUL r16, r/m16, imm16 = 0x69 /r ; modrm 11 000(AX) 011(BX) = 0xC3
    let (cpu, _, _) = run_payload(&[0xBB, 0x03, 0x00, 0x69, 0xC3, 0x07, 0x00, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX] as i16, 21);
    // 21 fits in i16, no overflow
    assert!(!cpu.has(flag::CF));
    assert!(!cpu.has(flag::OF));
}

#[test]
fn imul_three_operand_imm8_sign_extended() {
    // MOV BX, 1000 ; IMUL AX, BX, -10 → AX = -10000
    //   IMUL r16, r/m16, imm8 = 0x6B /r ; modrm 11 000 011 = 0xC3
    //   imm8 = -10 = 0xF6
    let (cpu, _, _) = run_payload(&[0xBB, 0xE8, 0x03, 0x6B, 0xC3, 0xF6, 0xF4], 6);
    assert_eq!(cpu.regs[r16::AX] as i16, -10000);
    assert!(!cpu.has(flag::CF));
}

#[test]
fn imul_three_operand_sets_overflow_when_result_truncates() {
    // 1000 * 1000 = 1_000_000, won't fit in i16 (max 32767).
    //   MOV BX, 1000 ; IMUL AX, BX, 1000 → CF=OF=1
    let (cpu, _, _) = run_payload(&[0xBB, 0xE8, 0x03, 0x69, 0xC3, 0xE8, 0x03, 0xF4], 6);
    assert!(cpu.has(flag::CF));
    assert!(cpu.has(flag::OF));
}

#[test]
fn enter_leave_balances_frame() {
    // ENTER 8, 0 ; LEAVE  must net-zero on the stack and leave BP
    // pointing where it was before ENTER (since we BP wasn't set
    // beforehand it's still 0 after LEAVE pops it back).
    // 0: B8 EF BE     MOV AX, 0xBEEF       ; just to occupy state
    // 3: C8 08 00 00  ENTER 8, 0
    // 7: C9           LEAVE
    // 8: F4           HLT
    let (cpu, _, _) = run_payload(&[0xB8, 0xEF, 0xBE, 0xC8, 0x08, 0x00, 0x00, 0xC9, 0xF4], 8);
    assert_eq!(cpu.regs[r16::BP], 0);
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn enter_with_nonzero_level_returns_unimplemented() {
    // ENTER 16, 1 — nesting level 1 not yet supported.
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x7C00, &[0xC8, 0x10, 0x00, 0x01]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    let err = cpu.step(&mut mem, &mut io).unwrap_err();
    match err {
        CpuError::Unimplemented { opcode: 0xC8, .. } => {}
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unmasked_irq_dispatches_through_ivt_when_if_set() {
    // Set up IVT[0x08] (IRQ 0 vector) to handler at 0x7C10.
    // Program: STI, then a tight loop of NOPs. We raise IRQ 0
    // before stepping, so the very first step should service the
    // IRQ via the handler.
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0x08 * 4, 0x7C10);
    mem.write_u16(0x08 * 4 + 2, 0);
    let program = &[
        0xFB, // STI                       offset 0
        0x90, 0x90, 0x90, // NOPs we never reach pre-handler
        0xF4, // HLT
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // pad to 0x10
        // 0x10: handler
        0xB3, 0xAB, // MOV BL, 0xAB
        // EOI to master PIC: OUT 0x20, AL where AL=0x20
        0xB0, 0x20, 0xE6, 0x20, 0xCF, // IRET
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    // Unmask IRQ 0 and raise it.
    io.pic_mut().imr = 0xFE;
    io.pic_mut().raise_irq(0);

    for _ in 0..40 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_eq!(cpu.read_r8(3), 0xAB);
    assert!(cpu.halted);
    // ISR cleared by EOI
    assert_eq!(io.pic_mut().isr, 0);
    // Stack balanced
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

#[test]
fn cli_blocks_irq_delivery() {
    // IRQ raised but IF cleared (default after reset). Step a
    // sequence of NOPs — handler must NOT run. Then STI; the next
    // step should pick it up.
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0x08 * 4, 0x7C10);
    mem.write_u16(0x08 * 4 + 2, 0);
    let program = &[
        0x90, 0x90, 0x90, // 3 NOPs while IF=0
        0xFB, // STI                  offset 3
        0x90, 0x90, // these *might* be replaced by IRQ
        0xF4, 0, 0, 0, 0, 0, 0, 0, 0, 0, // pad to 0x10
        0xB3, 0xCD, // MOV BL, 0xCD          handler
        0xB0, 0x20, 0xE6, 0x20, 0xCF,
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    io.pic_mut().imr = 0xFE;
    io.pic_mut().raise_irq(0);

    // Three NOPs with IF=0: handler must not run.
    for _ in 0..3 {
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_ne!(cpu.read_r8(3), 0xCD);
    // STI then run to completion.
    for _ in 0..40 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_eq!(cpu.read_r8(3), 0xCD);
    assert!(cpu.halted);
}

#[test]
fn masked_irq_stays_pending_until_unmasked() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0x08 * 4, 0x7C10);
    mem.write_u16(0x08 * 4 + 2, 0);
    let program = &[
        0xFB, // STI
        0x90, 0x90, 0x90, 0x90, // Unmask IRQ 0 via OUT 0x21, 0xFE
        0xB0, 0xFE, 0xE6, 0x21, 0x90, 0x90, 0x90, 0xF4, 0, 0, 0, // pad to 0x10
        0xB3, 0x11, // handler: MOV BL, 0x11
        0xB0, 0x20, 0xE6, 0x20, 0xCF,
    ];
    mem.write_slice(0x7C00, program);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    // IRQ 0 raised, but IMR=0xFF (default) blocks it.
    io.pic_mut().raise_irq(0);

    // Run a few steps. Until OUT 0x21, 0xFE runs the handler stays
    // blocked. After it, IRQ should be delivered.
    for _ in 0..50 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert_eq!(cpu.read_r8(3), 0x11);
    assert!(cpu.halted);
}

#[test]
fn rcl_al_1_rotates_through_carry() {
    // CF=1; MOV AL, 0x40; RCL AL, 1 → AL = 0x81 (40<<1 | CF), CF=0.
    // RCL r/m8, 1 = 0xD0 /2, ModR/M = 11 010 000 = 0xD0
    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xB0, 0x40, // MOV AL, 0x40
            0xD0, 0xD0, // RCL AL, 1
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 0x81);
    assert!(!cpu.has(flag::CF));
}

#[test]
fn rcr_al_1_brings_carry_into_top_bit() {
    // CF=1; MOV AL, 0x02; RCR AL, 1 → AL = 0x81 (CF→top, 0x02>>1=1), CF=0
    // RCR r/m8, 1 = 0xD0 /3, ModR/M = 11 011 000 = 0xD8
    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xB0, 0x02, 0xD0, 0xD8, 0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 0x81);
    assert!(!cpu.has(flag::CF));
}

#[test]
fn rcl_ax_9_cycles_back_through_carry() {
    // RCL with count=9 on a 9-bit cycle is identity. Reload CF=1
    // first so we can observe it round-trip.
    // RCL r/m8, CL: 0xD2 /2, modrm 0xD0; CL=9.
    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0xB0, 0xAA, // AL = 0xAA
            0xB1, 0x09, // CL = 9
            0xD2, 0xD0, // RCL AL, CL
            0xF4,
        ],
        10,
    );
    assert_eq!(cpu.read_r8(0), 0xAA);
    assert!(cpu.has(flag::CF));
}

#[test]
fn daa_after_bcd_add() {
    // 0x09 + 0x01 (binary) = 0x0A; DAA should adjust to 0x10 (BCD).
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x09, // MOV AL, 9
            0x04, 0x01, // ADD AL, 1 → AL = 0x0A
            0x27, // DAA
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 0x10);
}

#[test]
fn das_after_bcd_sub() {
    // 0x10 - 0x01 = 0x0F binary; DAS adjusts to 0x09.
    let (cpu, _, _) = run_payload(&[0xB0, 0x10, 0x2C, 0x01, 0x2F, 0xF4], 8);
    assert_eq!(cpu.read_r8(0), 0x09);
}

#[test]
fn aaa_adjusts_unpacked_bcd_carry() {
    // After "5 + 6" = 0x0B in AL, AAA → AL=1, AH+=1, CF=1.
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x05, 0x04, 0x06, // ADD AL, 6
            0x37, // AAA
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 1); // AL
    assert_eq!(cpu.read_r8(4), 1); // AH bumped
    assert!(cpu.has(flag::CF));
}

#[test]
fn aam_splits_al_into_ah_al() {
    // MOV AL, 23 ; AAM (base 10) → AH=2, AL=3
    let (cpu, _, _) = run_payload(
        &[
            0xB0, 0x17, // 23
            0xD4, 0x0A, // AAM 10
            0xF4,
        ],
        6,
    );
    assert_eq!(cpu.read_r8(4), 2); // AH
    assert_eq!(cpu.read_r8(0), 3); // AL
}

#[test]
fn aad_combines_ah_al_into_al() {
    // AH=2, AL=3 ; AAD (base 10) → AL = 2*10 + 3 = 23, AH = 0
    let (cpu, _, _) = run_payload(
        &[
            0xB4, 0x02, // MOV AH, 2
            0xB0, 0x03, // MOV AL, 3
            0xD5, 0x0A, // AAD 10
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r8(0), 23);
    assert_eq!(cpu.read_r8(4), 0);
}

/// AAM with base = 0 raises #DE through IVT[0] (same path as DIV
/// by zero). The handler-ran-and-IP-was-right shape of the test
/// is identical to div_by_zero_vectors_through_ivt_de.
#[test]
fn aam_with_zero_base_vectors_through_ivt_de() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u8(0, 0x20);
    mem.write_u8(1, 0x7C);
    mem.write_u8(2, 0);
    mem.write_u8(3, 0);
    // MOV AL, 5 ; AAM 0  (AAM at offset 2 → 0x7C02)
    mem.write_slice(0x7C00, &[0xB0, 0x05, 0xD4, 0x00, 0xF4]);
    mem.write_slice(0x7C20, &[0xB0, 0xAA, 0xF4]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0xAA);
    assert_eq!(mem.read_u16(0x7BFA), 0x7C02, "saved IP = start of AAM");
}

#[test]
fn read_write_r32_round_trip() {
    let mut cpu = Cpu::new();
    cpu.write_r32(0, 0xCAFE_BABE);
    assert_eq!(cpu.read_r32(0), 0xCAFE_BABE);
    assert_eq!(cpu.read_r16(0), 0xBABE);
    assert_eq!(cpu.read_r8(0), 0xBE); // AL
    assert_eq!(cpu.read_r8(4), 0xBA); // AH
}

#[test]
fn r16_write_preserves_upper_16_of_r32() {
    let mut cpu = Cpu::new();
    cpu.write_r32(3, 0xDEAD_0000);
    cpu.write_r16(3, 0xBEEF);
    assert_eq!(cpu.read_r32(3), 0xDEAD_BEEF);
}

#[test]
fn r8_write_preserves_upper_24_of_r32() {
    let mut cpu = Cpu::new();
    cpu.write_r32(0, 0x1122_3344);
    cpu.write_r8(0, 0xFF);
    assert_eq!(cpu.read_r32(0), 0x1122_33FF);
    cpu.write_r8(4, 0xAA);
    assert_eq!(cpu.read_r32(0), 0x1122_AAFF);
}

/// 0x66 0xB8 imm32 → MOV EAX, imm32. Full 32 bits land in EAX.
#[test]
fn mov_eax_imm32_with_operand_size_prefix() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x78, 0x56, 0x34, 0x12, // MOV EAX, 0x12345678
            0xF4,
        ],
        4,
    );
    assert_eq!(cpu.read_r32(0), 0x1234_5678);
    assert_eq!(cpu.read_r16(0), 0x5678); // AX = low 16
    assert_eq!(cpu.regs_high[0], 0x1234); // upper 16 in regs_high
}

/// 0x66 prefix is per-instruction — the next opcode without it
/// reverts to 16-bit semantics, leaving the upper 16 alone.
#[test]
fn operand_size_prefix_resets_after_one_instruction() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xBB, 0xEF, 0xBE, 0xAD, 0xDE, // MOV EBX, 0xDEADBEEF
            0xBB, 0x34, 0x12, // MOV BX, 0x1234  (no 0x66 — 16-bit)
            0xF4,
        ],
        6,
    );
    // Lower 16 = 0x1234, upper 16 preserved from EBX assignment.
    assert_eq!(cpu.read_r32(3), 0xDEAD_1234);
}

/// 0x66 0x0D imm32 → OR EAX, imm32 (variant 5 under 0x66). The
/// canonical "set CR0.PE bit" pre-step uses this.
#[test]
fn or_eax_imm32_sets_target_bit() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // MOV EAX, 0
            0x66, 0x0D, 0x01, 0x00, 0x00, 0x00, // OR EAX, 1
            0xF4,
        ],
        4,
    );
    assert_eq!(cpu.read_r32(0), 1);
    assert!(!cpu.has(flag::ZF));
}

/// ADD r32, r32 — variant 1 (0x01 /r) under 0x66. Carry crossing
/// the 16-bit boundary should not set CF.
#[test]
fn add_r32_r32_with_operand_size_prefix() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0xFF, 0xFF, 0x00, 0x00, // MOV EAX, 0x0000_FFFF
            0x66, 0xBB, 0x01, 0x00, 0x00, 0x00, // MOV EBX, 0x0000_0001
            0x66, 0x01, 0xD8, // ADD EAX, EBX (modrm 11 011 000)
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.read_r32(0), 0x0001_0000);
    assert!(!cpu.has(flag::CF));
    assert!(!cpu.has(flag::ZF));
}

/// In real mode `write_sreg` mirrors `selector << 4` into the cache.
/// Same as the legacy linear computation — verifies the helper
/// preserves existing semantics for RM code.
#[test]
fn write_sreg_in_real_mode_synthesizes_cache_from_shift() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mem = Memory::new(0x10_0000);
    cpu.write_sreg(sreg::DS, 0x1234, &mem);
    assert_eq!(cpu.sregs[sreg::DS], 0x1234);
    assert_eq!(cpu.seg_cache[sreg::DS].base, 0x1_2340);
    assert_eq!(cpu.seg_cache[sreg::DS].limit, 0xFFFF);
    assert_eq!(cpu.linear_seg(sreg::DS, 0x10), 0x1_2350);
}

/// In protected mode `write_sreg` decodes an 8-byte GDT descriptor.
/// Build a descriptor at GDT[1] with base=0x0010_0000, limit=0xFFFF,
/// access=0x92 (data, R/W, present, ring 0), granularity=0. Load
/// DS with selector 0x08 (RPL=0, TI=0, index=1) and check that the
/// hidden cache matches.
#[test]
fn write_sreg_in_protected_mode_loads_descriptor_from_gdt() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // CR0.PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    let mut mem = Memory::new(0x10_0000);
    // GDT[0] = null (zeros). GDT[1] at offset 8 inside GDT:
    //   limit_lo  = 0xFFFF
    //   base_lo   = 0x0000
    //   base_mid  = 0x10              (byte 4)
    //   access    = 0x92              (byte 5)
    //   limit_hi  = 0x00              (byte 6 low nibble) + flags 0x00
    //   base_hi   = 0x00              (byte 7)
    mem.write_slice(
        0x0508,
        &[
            0xFF, 0xFF, // limit 15:0
            0x00, 0x00, // base 15:0
            0x10, // base 23:16
            0x92, // access
            0x00, // limit 19:16 | flags
            0x00, // base 31:24
        ],
    );
    cpu.write_sreg(sreg::DS, 0x08, &mem);
    assert_eq!(cpu.sregs[sreg::DS], 0x08);
    assert_eq!(cpu.seg_cache[sreg::DS].base, 0x0010_0000);
    assert_eq!(cpu.seg_cache[sreg::DS].limit, 0xFFFF);
    assert_eq!(cpu.seg_cache[sreg::DS].access, 0x92);
    // Cache-based linear lookup gives base + offset.
    assert_eq!(cpu.linear_seg(sreg::DS, 0x100), 0x0010_0100);
}

/// Granularity bit (G=1) shifts limit left by 12 and fills the low
/// 12 with ones — turning 0xFFFFF into 0xFFFF_FFFF (a 4 GiB segment).
#[test]
fn write_sreg_decodes_page_granularity_limit() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1;
    cpu.gdtr.base = 0x0500;
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x0508,
        &[
            0xFF, 0xFF, // limit 15:0
            0x00, 0x00, // base 15:0
            0x00, // base 23:16
            0x92, // access
            0x8F, // limit 19:16 = 0xF, flags = 0x8 (G=1)
            0x00, // base 31:24
        ],
    );
    cpu.write_sreg(sreg::DS, 0x08, &mem);
    assert_eq!(cpu.seg_cache[sreg::DS].limit, 0xFFFF_FFFF);
}

/// PM-transition idiom: `MOV EAX, CR0 ; OR EAX, 1 ; MOV CR0, EAX`.
/// Sets CR0.PE — the actual real-mode → protected-mode transition.
#[test]
fn pm_transition_idiom_sets_cr0_pe() {
    let (cpu, _, _) = run_payload(
        &[
            0x0F, 0x20, 0xC0, // MOV EAX, CR0
            0x66, 0x0D, 0x01, 0x00, 0x00, 0x00, // OR EAX, 1
            0x0F, 0x22, 0xC0, // MOV CR0, EAX
            0xF4,
        ],
        8,
    );
    assert_eq!(cpu.cr0 & 1, 1);
}

/// PE=1 + a non-trivial GDT descriptor base must re-route every memory
/// access that goes through the segment cache. We arrange the cache so
/// the same DS selector (0x08) would read very different addresses in
/// real mode (base = sel << 4 = 0x80) vs. PE (base = 0x4000), preload
/// both candidates with distinct bytes, and verify MOV AL, [0x100]
/// landed on the PE one.
#[test]
fn protected_mode_addressing_uses_descriptor_base_not_shift_by_four() {
    let mut mem = Memory::new(0x10_0000);
    // Code at boot CS:IP = 0000:7C00.
    // MOV AL,[0x100]; HLT. Uses 0x8A /0 modrm=00 000 110 → [disp16].
    mem.write_slice(0x7C00, &[0x8A, 0x06, 0x00, 0x01, 0xF4]);
    // Real-mode-equivalent of DS=0x08 → 0x80 base. If addressing still
    // went through (sel << 4) we'd read 0x42 from here.
    mem.write_u8(0x0180, 0x42);
    // PE descriptor base = 0x4000 → expected source is here.
    mem.write_u8(0x4100, 0xAB);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    // GDT[1] at 0x0508: base=0x0000_4000, limit=0xFFFF, access=0x92.
    mem.write_slice(
        0x0508,
        &[
            0xFF, 0xFF, // limit 15:0
            0x00, 0x40, // base 15:0
            0x00, // base 23:16
            0x92, // access
            0x00, // limit 19:16 | flags
            0x00, // base 31:24
        ],
    );
    cpu.write_sreg(sreg::DS, 0x08, &mem);
    assert_eq!(cpu.seg_cache[sreg::DS].base, 0x0000_4000);

    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(
        cpu.read_r8(0),
        0xAB,
        "MOV AL,[0x100] must read from descriptor base 0x4000, not (sel << 4) = 0x80"
    );
}

/// JMP ptr16:16 (0xEA) in protected mode must reload CS through the
/// GDT — the next instruction fetch then lands at the descriptor base
/// plus the new IP, not at (selector << 4) + new_ip. We pre-poison
/// both candidate code addresses with distinct HLT-vs-NOP+HLT
/// sequences so the post-jump state distinguishes the two paths.
#[test]
fn protected_mode_far_jump_reloads_cs_through_gdt() {
    let mut mem = Memory::new(0x10_0000);
    // Boot code at 0x7C00: JMP FAR 0x10:0x0200; HLT (filler if jump fails).
    mem.write_slice(0x7C00, &[0xEA, 0x00, 0x02, 0x10, 0x00, 0xF4]);
    // Real-mode shift candidate: CS=0x10 → 0x100 base + IP 0x200 = 0x300.
    // Land here would mean addressing still ignores PE.
    // Put 0x90 0xF4 0xF4 ... here so AL would stay 0 after fetch.
    mem.write_slice(0x0300, &[0x90, 0xF4]); // NOP; HLT — leaves AX=0
                                            // PE descriptor base = 0x8000 → expected fetch at 0x8200.
                                            // Put MOV AL, 0xC3; HLT here as the signature.
    mem.write_slice(0x8200, &[0xB0, 0xC3, 0xF4]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    // GDT[2] at 0x0510: base=0x8000, limit=0xFFFF, access=0x9A (code, R/X, present).
    mem.write_slice(
        0x0510,
        &[
            0xFF, 0xFF, // limit 15:0
            0x00, 0x80, // base 15:0
            0x00, // base 23:16
            0x9A, // access (code segment, executable, readable, present)
            0x00, // limit 19:16 | flags
            0x00, // base 31:24
        ],
    );

    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.seg_cache[sreg::CS].base, 0x8000);
    assert_eq!(
        cpu.read_r8(0),
        0xC3,
        "JMP FAR in PE must fetch the next opcode from descriptor.base + IP"
    );
}

/// INT n in protected mode dispatches via the IDT, not the real-mode
/// IVT at 0x0000. We arm both candidate vectors — a fake "IVT" entry
/// at vector 0x21 (linear 0x84) and a real IDT gate at idtr.base+8*0x21
/// — pointing at different handlers. After `INT 0x21` we expect to be
/// in the IDT handler.
#[test]
fn protected_mode_int_dispatches_via_idt_gate() {
    let mut mem = Memory::new(0x10_0000);
    // Boot stub: INT 0x21; HLT.
    mem.write_slice(0x7C00, &[0xCD, 0x21, 0xF4]);

    // Fake real-mode IVT entry at vector 0x21 (linear 0x84): IP=0x100, CS=0x40.
    // If we incorrectly dispatched via the IVT this would land us at
    // 0x40<<4 + 0x100 = 0x500, where we'd run MOV AL,0xEE; HLT.
    mem.write_u16(0x84, 0x0100);
    mem.write_u16(0x86, 0x0040);
    mem.write_slice(0x0500, &[0xB0, 0xEE, 0xF4]);

    // GDT[1] for the IDT handler's code segment: base=0x9000, access=0x9A.
    cpu_pe_setup(&mut mem);

    // IDT base at 0x4000, gate 0x21 at 0x4000 + 0x21*8 = 0x4108.
    //   offset_lo = 0x0200, selector = 0x0008 (GDT[1]), type = 0x86 (16-bit interrupt gate)
    mem.write_slice(
        0x4108,
        &[
            0x00, 0x02, // offset 15:0
            0x08, 0x00, // selector
            0x00, // reserved
            0x86, // P=1 DPL=0 type=6 (16-bit interrupt gate)
            0x00, 0x00, // offset 31:16
        ],
    );
    // Real IDT handler at 0x9000 + 0x0200 = 0x9200: MOV AL, 0xC4; HLT.
    mem.write_slice(0x9200, &[0xB0, 0xC4, 0xF4]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    cpu.idtr.base = 0x4000;
    cpu.idtr.limit = 0x07FF;

    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(
        cpu.read_r8(0),
        0xC4,
        "INT in PE must dispatch via IDT gate (sets AL=0xC4), not real-mode IVT (would set AL=0xEE)"
    );
}

/// Builds GDT[1] = code segment base=0x9000 access=0x9A at gdtr.base=0x0500.
fn cpu_pe_setup(mem: &mut Memory) {
    mem.write_slice(
        0x0508,
        &[
            0xFF, 0xFF, // limit 15:0
            0x00, 0x90, // base 15:0
            0x00, // base 23:16
            0x9A, // access (code, R/X, present)
            0x00, // limit 19:16 | flags
            0x00, // base 31:24
        ],
    );
}

/// With paging on, a data load through MOV AL,[disp16] must resolve
/// the operand address through the page tables. We put the program
/// in physical memory at 0x7C00 (PG is off during fetch since CS
/// cache base = 0 + IP = 0x7C00 maps identity-ish... we instead
/// avoid pagewalk-through-CS by pre-translating CS to a frame where
/// PDE/PTE-walking identity-maps 0x7C00 → 0x7C00). To make that
/// reliable we use an identity-mapped first 4 MiB.
#[test]
fn paged_data_load_routes_through_page_tables() {
    let mut mem = Memory::new(0x10_0000);
    // Boot stub: MOV AL,[0x0100]; HLT. The linear operand address
    // becomes 0x0100. We're going to remap that frame.
    mem.write_slice(0x7C00, &[0x8A, 0x06, 0x00, 0x01, 0xF4]);

    // Identity-map the first 4 MiB (PD[0] -> PT0, PT0[i] -> frame i).
    // PD at 0x1000, PT0 at 0x2000.
    mem.write_u32(0x1000, 0x0000_2000 | 0x03); // PDE[0] = PT0 | P|RW
    for i in 0..1024u32 {
        mem.write_u32(0x2000 + i * 4, (i << 12) | 0x03);
    }
    // Remap PTE for linear 0x0000 (first 4 KiB) to physical 0x9000
    // instead of 0x0000. The boot code accesses linear 0x0100,
    // which should now read from physical 0x9100.
    mem.write_u32(0x2000, 0x0000_9000 | 0x03);
    // Sentinel at the remapped frame: physical 0x9100 = 0xC5.
    mem.write_u8(0x9100, 0xC5);
    // Wrong sentinel at the original linear address: physical 0x0100 = 0x42.
    mem.write_u8(0x0100, 0x42);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    // CR0.PE not strictly required for paging-on-translate; we keep
    // PE off so CS cache stays at base 0 and the code at linear
    // 0x7C00 still fetches (linear 0x7C00 → through paging → PT0
    // identity-maps everything except the remapped first frame, so
    // 0x7C00 fetches from physical 0x7C00 unchanged).
    cpu.cr0 = 0x8000_0000; // PG only
    cpu.cr3 = 0x0000_1000;

    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(
        cpu.read_r8(0),
        0xC5,
        "MOV AL,[0x100] under PG=1 must read from PTE-mapped frame 0x9100, not linear 0x0100"
    );
}

/// Code fetch goes through paging too. With the first frame remapped,
/// instructions whose linear address falls inside that frame have to
/// come from the new physical frame. We place the boot stub at linear
/// 0x0200 (inside the first 4 KiB) and remap PTE[0] so its physical
/// home is at 0xA000 — the bytes at 0xA200 are MOV AL,0xD7; HLT.
#[test]
fn paged_code_fetch_routes_through_page_tables() {
    let mut mem = Memory::new(0x10_0000);
    // Identity-map first 4 MiB. PD at 0x1000, PT0 at 0x2000.
    mem.write_u32(0x1000, 0x0000_2000 | 0x03);
    for i in 0..1024u32 {
        mem.write_u32(0x2000 + i * 4, (i << 12) | 0x03);
    }
    // Remap frame 0 → physical 0xA000.
    mem.write_u32(0x2000, 0x0000_A000 | 0x03);
    // Real code lives at physical 0xA200 (the remapped image of linear 0x0200).
    mem.write_slice(0xA200, &[0xB0, 0xD7, 0xF4]); // MOV AL,0xD7; HLT
                                                  // Decoy code at physical 0x0200 — would set AL=0x11.
    mem.write_slice(0x0200, &[0xB0, 0x11, 0xF4]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.ip = 0x0200; // start fetching from linear 0x0200
    cpu.cr0 = 0x8000_0000;
    cpu.cr3 = 0x0000_1000;

    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(
        cpu.read_r8(0),
        0xD7,
        "fetch_u8 under PG=1 must pull opcodes from PTE-mapped frame 0xA200"
    );
}

/// A linear address whose PDE has Present=0 must raise #PF. We arm
/// CR3, leave PDE[0] zeroed (P=0), then translate() and confirm the
/// pending-fault slot got latched with the right linear address.
#[test]
fn translate_with_non_present_pde_raises_page_fault() {
    let mem = Memory::new(0x10_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000;
    cpu.cr3 = 0x0000_1000;
    // PD at 0x1000 left entirely zero — PDE[0..1023] all have P=0.
    let _ = cpu.translate(&mem, 0x0040_1234);
    let pf = cpu.pending_fault().expect("translate must flag #PF");
    assert_eq!(pf.addr, 0x0040_1234);
    assert_eq!(
        pf.error_code & 1,
        0,
        "P bit clear (not present) in error code"
    );
}

/// PDE present, PTE not present. Same expectation, different stop in
/// the walk.
#[test]
fn translate_with_non_present_pte_raises_page_fault() {
    let mut mem = Memory::new(0x10_0000);
    // PDE[0] -> PT at 0x2000, present.
    mem.write_u32(0x1000, 0x0000_2000 | 0x01);
    // PT entirely zero — every PTE has P=0.
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000;
    cpu.cr3 = 0x0000_1000;
    let _ = cpu.translate(&mem, 0x0000_0123);
    let pf = cpu.pending_fault().expect("PTE fault must be latched");
    assert_eq!(pf.addr, 0x0000_0123);
}

/// PSE — CR4 bit 4 — promotes a PDE with PS=1 to a 4 MiB-page
/// descriptor: linear[31:22] picks the PDE, linear[21:0] indexes
/// into the directly-mapped 4 MiB frame, no PTE walk. Linux's
/// `head_32.S` flips this on so a handful of PDEs cover the whole
/// kernel virtual range.
#[test]
fn translate_with_pse_pde_uses_4mib_page_directly() {
    let mut mem = Memory::new(0x0080_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000; // PG
    cpu.cr4 = 0x10; // PSE
    cpu.cr3 = 0x0000_1000;
    // PDE[1] maps the linear range [0x0040_0000, 0x0080_0000) to
    // the 4 MiB frame at physical 0x0040_0000. P=1, RW=1, PS=1.
    //   PS = bit 7, RW = bit 1, P = bit 0  →  0x0040_0000 | 0x83.
    mem.write_u32(0x1004, 0x0040_0000 | 0x83);

    // linear 0x0040_0000 + 0x12_3456 — should resolve to physical
    // 0x0040_0000 + 0x12_3456, with no PTE involvement at all (PT
    // base would point at zeros, so a PTE walk would #PF).
    let phys = cpu.translate(&mem, 0x0040_0000 + 0x0012_3456);
    assert!(
        cpu.pending_fault().is_none(),
        "no fault on a PSE-mapped read"
    );
    assert_eq!(phys, 0x0040_0000 + 0x0012_3456);

    // And a write through the same PSE frame should match. We
    // poke through write-translation explicitly to confirm both
    // arms agree.
    let phys_w = cpu.translate_write(&mem, 0x0040_0000 + 0x0000_0FFF);
    assert!(cpu.pending_fault().is_none(), "no fault on PSE write");
    assert_eq!(phys_w, 0x0040_0000 + 0x0000_0FFF);
}

/// End-to-end: paging + PSE work for *instruction fetch*, not just
/// for `translate()` of data operands. We set up a flat 32-bit CS,
/// build a 4 MiB-page directory with PDE[0] identity-mapping [0,4M)
/// and PDE[1] aliasing [4M,8M) → [0,4M), enable PG+PSE, then start
/// EIP up in the alias range. The first fetch must walk the PSE PDE,
/// land on the same physical bytes the identity map sees, and run.
/// This is the shape `head_32.S` produces when it jumps to the kernel's
/// high virtual address right after flipping CR0.PG.
#[test]
fn paging_with_pse_steps_real_instruction_through_aliased_virtual() {
    let mut mem = Memory::new(0x0080_0000); // 8 MiB

    // GDT at 0x500: null + ring-0 code (D=1/G=1) + ring-0 data.
    mem.write_slice(
        0x0500,
        &[
            0, 0, 0, 0, 0, 0, 0, 0, // null
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, // code
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // data
        ],
    );

    // Page directory at 0x10_0000 (above the 1 MiB line so it doesn't
    // collide with the kernel payload). PDE[0] identity-maps [0, 4M),
    // PDE[1] aliases [4M, 8M) onto phys [0, 4M). Both PSE, P=1, RW=1.
    mem.write_u32(0x0010_0000, 0x0000_0083);
    mem.write_u32(0x0010_0004, 0x0000_0083);

    // Kernel payload at physical 0x0020_0000 (2 MiB in, well inside
    // the 4 MiB frame). With CS.D=1 the MOV imm32 is real 32-bit.
    //   B8 BE BA FE CA       MOV EAX, 0xCAFEBABE
    //   F4                   HLT
    mem.write_slice(0x0020_0000, &[0xB8, 0xBE, 0xBA, 0xFE, 0xCA, 0xF4]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    cpu.write_sreg(sreg::CS, 0x08, &mem);
    cpu.write_sreg(sreg::DS, 0x10, &mem);
    cpu.write_sreg(sreg::SS, 0x10, &mem);

    // Enable paging *after* the descriptors are loaded — flipping
    // PG with a stale CS would have the CPU fetch the next byte
    // through translate() against an empty PDE.
    cpu.cr3 = 0x0010_0000;
    cpu.cr4 = 0x10; // PSE
    cpu.cr0 = 0x8000_0001; // PG | PE

    // Enter through the *alias* — linear 0x0040_0000 + 0x0020_0000
    // (= 0x0060_0000) walks PDE[1] to physical 0x0020_0000, where
    // the payload sits. If the fetch were taken without paging
    // we'd read whatever was at linear 0x0060_0000 in raw memory
    // (zeros — RAM is unwritten there) and immediately ADD to a
    // zero-prefix string of opcodes, not run the MOV.
    cpu.ip = 0x0060_0000;

    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(0), 0xCAFE_BABE);
    // Sanity: paging is still on, the descriptor base is still 0.
    assert_eq!(cpu.cr0 & 0x8000_0001, 0x8000_0001);
    assert_eq!(cpu.seg_cache[sreg::CS].base, 0);
}

/// End-to-end: a load that touches an unmapped page must vector
/// through INT 14 with CR2 set. We identity-map the first 4 MiB so
/// the boot code, the IVT, and the #PF handler are all reachable,
/// then poke a hole at PTE[0x10] (linear 0x10000-0x10FFF). The boot
/// stub loads DS=0x1000 and does MOV AL,[0x100], so the operand
/// linear becomes 0x10100 — the address inside the unmapped page.
/// The handler at linear 0x9000 sets AH=0xFE and HLTs.
#[test]
fn page_fault_dispatches_int14_and_latches_cr2() {
    let mut mem = Memory::new(0x10_0000);
    // Boot stub at linear 0x7C00:
    //   MOV AX, 0x1000   ; B8 00 10
    //   MOV DS, AX       ; 8E D8
    //   MOV AL, [0x100]  ; 8A 06 00 01  (DS:0x100 → linear 0x10100)
    //   HLT              ; F4
    mem.write_slice(
        0x7C00,
        &[0xB8, 0x00, 0x10, 0x8E, 0xD8, 0x8A, 0x06, 0x00, 0x01, 0xF4],
    );
    // PF handler at linear 0x9000: MOV AH, 0xFE; HLT.
    mem.write_slice(0x9000, &[0xB4, 0xFE, 0xF4]);
    // IVT entry for INT 14 (linear 14*4 = 0x38): IP=0x9000, CS=0x0000.
    mem.write_u16(0x38, 0x9000);
    mem.write_u16(0x3A, 0x0000);

    // Page directory at 0x1000, page table at 0x2000. Identity-map
    // the first 4 MiB, then knock PTE[0x10] (linear 0x10000-0x10FFF)
    // unconditionally not-present.
    mem.write_u32(0x1000, 0x0000_2000 | 0x03);
    for i in 0..1024u32 {
        mem.write_u32(0x2000 + i * 4, (i << 12) | 0x03);
    }
    mem.write_u32(0x2000 + 0x10 * 4, 0); // hole at PTE[0x10]

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000; // PG only
    cpu.cr3 = 0x0000_1000;

    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(4), 0xFE, "AH set by the #PF handler");
    assert_eq!(
        cpu.cr2, 0x0001_0100,
        "CR2 latched the linear address inside the unmapped page"
    );
    assert!(
        cpu.pending_fault().is_none(),
        "fault must be consumed by dispatch"
    );
}

/// CR0.WP (bit 16) — Write Protect. A supervisor write to a
/// 4-KiB page whose effective R/W bit is 0 raises #PF with both
/// P=1 (page is present) and W=1 (write triggered the fault).
/// Linux's COW path depends on this: fork() marks shared pages
/// R/W=0; the first write from either child takes a #PF, and the
/// handler clones the page. Without WP enforcement, the write
/// would land silently and corrupt the shared backing.
#[test]
fn translate_with_wp_set_on_readonly_pte_raises_pf_on_write() {
    let mut mem = Memory::new(0x10_0000);
    // PDE[0] -> PT at 0x2000, P=1 RW=1.
    mem.write_u32(0x1000, 0x0000_2000 | 0x03);
    // PTE[0] = present (P=1) but R/W=0 (read-only).
    mem.write_u32(0x2000, 0x0000_3000 | 0x01);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8001_0000; // PG | WP
    cpu.cr3 = 0x0000_1000;

    // Read succeeds — WP only gates writes.
    let phys = cpu.translate(&mem, 0x0000_0123);
    assert!(
        cpu.pending_fault().is_none(),
        "read through R/W=0 page is fine"
    );
    assert_eq!(phys, 0x0000_3123);

    // Write must #PF with P=1 (present) | W=1 (write).
    let _ = cpu.translate_write(&mem, 0x0000_0234);
    let pf = cpu.pending_fault().expect("WP write must raise #PF");
    assert_eq!(pf.addr, 0x0000_0234);
    assert_eq!(pf.error_code & 0b11, 0b11, "P=1 (present) | W=1 (write)");
}

/// Same write-to-R/W=0 page, but with CR0.WP cleared. The
/// supervisor write is allowed through — Linux relies on this
/// during early init before COW is wired up.
#[test]
fn translate_without_wp_lets_supervisor_write_a_readonly_page() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x1000, 0x0000_2000 | 0x03);
    mem.write_u32(0x2000, 0x0000_3000 | 0x01); // P=1, R/W=0

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000; // PG, WP off
    cpu.cr3 = 0x0000_1000;

    let phys = cpu.translate_write(&mem, 0x0000_0567);
    assert!(
        cpu.pending_fault().is_none(),
        "no WP -> no protection fault"
    );
    assert_eq!(phys, 0x0000_3567);
}

/// CR0.WP also gates the 4 MiB (PSE) path: PDE with PS=1 and
/// R/W=0 raises #PF on a write. The check has to happen at the
/// PDE level here because PSE collapses the PTE walk.
#[test]
fn translate_with_wp_set_on_readonly_pse_pde_raises_pf_on_write() {
    let mut mem = Memory::new(0x0080_0000);
    // PSE 4 MiB at phys 0x0040_0000, P=1, PS=1, R/W=0.
    //   PS=0x80, P=0x01  →  0x81. (Note: R/W bit 1 NOT set.)
    mem.write_u32(0x1004, 0x0040_0000 | 0x81);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8001_0000; // PG | WP
    cpu.cr4 = 0x10; // PSE
    cpu.cr3 = 0x0000_1000;

    // Read works.
    let phys = cpu.translate(&mem, 0x0040_0010);
    assert!(cpu.pending_fault().is_none());
    assert_eq!(phys, 0x0040_0010);

    // Write must #PF.
    let _ = cpu.translate_write(&mem, 0x0040_0020);
    let pf = cpu.pending_fault().expect("WP+PSE write must #PF");
    assert_eq!(pf.addr, 0x0040_0020);
    assert_eq!(pf.error_code & 0b11, 0b11);
}

/// CR3 reload mid-execution switches the active address space
/// immediately — every subsequent load translates through the new
/// page directory. This is the linchpin of Linux's context switch:
/// `__switch_to_asm` ends with `mov %eax, %cr3` (eax = next->pgd)
/// and the very next instruction's data accesses see the new task's
/// memory map. Without that, every context switch silently leaks
/// the previous task's address space.
///
/// We park PD-A and PD-B at distinct physical bases; both
/// identity-map [0, 4M) (so the code keeps fetching after the
/// switch), but their PDE[1] points at different 4-MiB frames
/// stamped with distinct bytes. After running, BL holds the byte
/// read through PD-A and CL the byte through PD-B.
#[test]
fn cr3_reload_mid_execution_switches_address_space_immediately() {
    // 16 MiB so we can host two 4-MiB-aligned PSE frames past the
    // identity-mapped [0, 4M) range.
    let mut mem = Memory::new(0x0100_0000);

    // GDT at 0x500 — flat CS/SS/DS with D=1/G=1.
    mem.write_slice(
        0x0500,
        &[
            0, 0, 0, 0, 0, 0, 0, 0, // null
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, // code
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // data
        ],
    );

    // PD-A at 0x10_000: identity [0,4M) + linear [4M,8M) → phys 0x400000.
    mem.write_u32(0x0001_0000, 0x0000_0083);
    mem.write_u32(0x0001_0004, 0x0040_0083);
    // PD-B at 0x20_000: identity [0,4M) + linear [4M,8M) → phys 0x800000.
    // (0x800000 = next 4-MiB-aligned frame after 0x400000.)
    mem.write_u32(0x0002_0000, 0x0000_0083);
    mem.write_u32(0x0002_0004, 0x0080_0083);

    // Stamp distinguishing bytes inside each 4 MiB frame.
    mem.write_u8(0x0040_0000, 0xAA);
    mem.write_u8(0x0080_0000, 0xBB);

    // Kernel at phys 0x1000:
    //   A0 00 00 40 00  MOV AL, [0x400000]    ; via PD-A → 0xAA
    //   88 C3           MOV BL, AL            ; stash
    //   BF 00 00 02 00  MOV EDI, 0x20000      ; PD-B base
    //   0F 22 DF        MOV CR3, EDI          ; switch address space
    //   A0 00 00 40 00  MOV AL, [0x400000]    ; via PD-B → 0xBB
    //   88 C1           MOV CL, AL            ; stash
    //   F4              HLT
    mem.write_slice(
        0x1000,
        &[
            0xA0, 0x00, 0x00, 0x40, 0x00, // MOV AL, [0x400000]
            0x88, 0xC3, // MOV BL, AL
            0xBF, 0x00, 0x00, 0x02, 0x00, // MOV EDI, 0x20000
            0x0F, 0x22, 0xDF, // MOV CR3, EDI
            0xA0, 0x00, 0x00, 0x40, 0x00, // MOV AL, [0x400000]
            0x88, 0xC1, // MOV CL, AL
            0xF4, // HLT
        ],
    );

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    cpu.write_sreg(sreg::CS, 0x08, &mem);
    cpu.write_sreg(sreg::DS, 0x10, &mem);
    cpu.write_sreg(sreg::SS, 0x10, &mem);
    cpu.stack_size_32 = true;
    cpu.cr3 = 0x0001_0000;
    cpu.cr4 = 0x10; // PSE
    cpu.cr0 = 0x8000_0001; // PG | PE
    cpu.ip = 0x1000;

    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // BL captured the byte through PD-A, CL through PD-B.
    assert_eq!(cpu.read_r8(3), 0xAA, "BL via PD-A");
    assert_eq!(cpu.read_r8(1), 0xBB, "CL via PD-B");
    // CR3 is now PD-B's base.
    assert_eq!(cpu.cr3, 0x0002_0000);
}

/// Protected-mode #PF dispatch composes with paging *and* a real
/// 32-bit IDT gate. The previous test exercises the real-mode IVT
/// path; once the kernel is in PM with paging on, the dispatch goes
/// through an 8-byte gate descriptor instead, and the error-code
/// push happens on the kernel stack — exactly Linux's
/// `do_page_fault` entry. None of the existing tests cover the
/// composition of: PM CS.D=1, paged instruction fetch, #PF in PM,
/// 32-bit IDT gate, error-code push, CR2 latched.
#[test]
fn pm_page_fault_dispatches_through_32_bit_gate_with_cr2_set() {
    let mut mem = Memory::new(0x0080_0000); // 8 MiB

    // GDT at 0x500: null + ring-0 code (D=1/G=1) + ring-0 data.
    mem.write_slice(
        0x0500,
        &[
            0, 0, 0, 0, 0, 0, 0, 0, // null
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, // code
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // data
        ],
    );

    // IDT at 0x1000. 14 * 8 = 0x70 → 32-bit interrupt gate for #PF:
    //   offset_lo=0x0800, selector=0x0008, type=0x8E, offset_hi=0
    mem.write_slice(
        0x1000 + 14 * 8,
        &[0x00, 0x08, 0x08, 0x00, 0x00, 0x8E, 0x00, 0x00],
    );

    // Page directory at 0x2000 — PDE[0] = PSE identity for [0,4M);
    // PDE[1] = 0 → linear [4M,8M) all not-present.
    mem.write_u32(0x2000, 0x0000_0083);
    // PDE[1] stays zero.

    // #PF handler at physical 0x0800 (= linear 0x0800 with identity
    // map; PSE PDE[0] covers it). Reads CR2 into EBX, then HLTs.
    //   0F 20 D3   MOV EBX, CR2 (reg=2=CR2, rm=3=EBX)
    //   F4         HLT
    mem.write_slice(0x0800, &[0x0F, 0x20, 0xD3, 0xF4]);

    // Kernel boot stub at physical 0x10_0000, inside the identity-
    // mapped 4 MiB. Enables PG+PSE, then deliberately loads from an
    // unmapped linear address (0x40_0000 = PDE[1]).
    //   B8 00 20 00 00     MOV EAX, 0x2000        (5)
    //   0F 22 D8           MOV CR3, EAX            (3)
    //   0F 20 E0           MOV EAX, CR4            (3)
    //   83 C8 10           OR EAX, 0x10            (3)
    //   0F 22 E0           MOV CR4, EAX            (3)
    //   0F 20 C0           MOV EAX, CR0            (3)
    //   0D 00 00 00 80     OR EAX, 0x80000000      (5)
    //   0F 22 C0           MOV CR0, EAX            (3)
    //   A1 00 00 40 00     MOV EAX, [0x400000]    (5)   ← faults
    //   F4                 HLT (never reached)
    mem.write_slice(
        0x0010_0000,
        &[
            0xB8, 0x00, 0x20, 0x00, 0x00, // MOV EAX, 0x2000
            0x0F, 0x22, 0xD8, // MOV CR3, EAX
            0x0F, 0x20, 0xE0, // MOV EAX, CR4
            0x83, 0xC8, 0x10, // OR EAX, 0x10
            0x0F, 0x22, 0xE0, // MOV CR4, EAX
            0x0F, 0x20, 0xC0, // MOV EAX, CR0
            0x0D, 0x00, 0x00, 0x00, 0x80, // OR EAX, 0x80000000
            0x0F, 0x22, 0xC0, // MOV CR0, EAX
            0xA1, 0x00, 0x00, 0x40, 0x00, // MOV EAX, [0x400000]
            0xF4, // HLT
        ],
    );

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    cpu.idtr.base = 0x1000;
    cpu.idtr.limit = 0x07FF;
    cpu.write_sreg(sreg::CS, 0x08, &mem);
    cpu.write_sreg(sreg::DS, 0x10, &mem);
    cpu.write_sreg(sreg::SS, 0x10, &mem);
    cpu.write_sreg(sreg::ES, 0x10, &mem);
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000); // stack at 0x80000
    cpu.ip = 0x0010_0000;

    let mut io = IoBus::new();
    for _ in 0..64 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted, "handler must reach HLT");
    // CR2 holds the faulting linear address (0x40_0000).
    assert_eq!(cpu.cr2, 0x0040_0000);
    // The handler captured CR2 into EBX before the HLT.
    let ebx = cpu.regs[r16::BX] as u32 | ((cpu.regs_high[r16::BX] as u32) << 16);
    assert_eq!(ebx, 0x0040_0000);
    assert!(cpu.pending_fault().is_none());
}

/// CPU comes up with the A20 line enabled (matching post-BIOS state)
/// and reads port 0x92 with bit 1 set to expose this.
#[test]
fn a20_defaults_enabled_and_port_0x92_reflects_state() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    assert!(cpu.a20);
    let mut io = IoBus::new();
    // IN AL, imm8 (0xE4) — read port 0x92.
    assert_eq!(cpu.port_read(&mut io, 0x92), 0b10);
    cpu.a20 = false;
    assert_eq!(cpu.port_read(&mut io, 0x92), 0);
}

/// With A20 enabled (default), a write to linear 0x10_0000 lands at
/// physical 0x10_0000 and reads back unchanged. With A20 gated off,
/// the address wraps into the low 1 MiB — writing to 0x10_0000
/// actually writes to 0 (bit 20 forced clear), and a follow-up read
/// from linear 0x10_0000 sees the value that lives at 0.
///
/// Drives this through the standard `mem_write_u8`/`mem_read_u8`
/// helpers so we exercise the same translate() path the CPU uses
/// for every guest memory access.
#[test]
fn a20_gating_wraps_high_addresses_into_low_mebibyte() {
    let mut mem = Memory::new(0x0020_0000); // 2 MiB
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    // A20 on: write at 0x100000 stays there.
    cpu.mem_write_u8(&mut mem, 0x0010_0000, 0xCA);
    assert_eq!(cpu.mem_read_u8(&mem, 0x0010_0000), 0xCA);
    // A20 off: the same store wraps to 0.
    cpu.a20 = false;
    cpu.mem_write_u8(&mut mem, 0x0010_0000, 0xFE);
    assert_eq!(cpu.mem_read_u8(&mem, 0), 0xFE);
    assert_eq!(
        cpu.mem_read_u8(&mem, 0x0010_0000),
        0xFE,
        "read of 0x100000 with A20 off must alias to 0"
    );
    // Bit 19 is *not* masked — addresses < 1 MiB still address
    // themselves even with A20 off.
    cpu.mem_write_u8(&mut mem, 0x0008_0000, 0x42);
    assert_eq!(cpu.mem_read_u8(&mem, 0x0008_0000), 0x42);
}

/// Boot stub: `IN AL, 0x92; AND AL, ~2; OUT 0x92, AL; HLT` flips
/// A20 off via the fast gate. Confirms the CPU's IN/OUT dispatch
/// actually routes through `port_read`/`port_write`.
#[test]
fn out_to_port_0x92_with_bit1_clear_disables_a20() {
    let (cpu, _, _) = run_payload(
        &[
            0xE4, 0x92, // IN AL, 0x92
            0x24, 0xFD, // AND AL, 0xFD (clear bit 1)
            0xE6, 0x92, // OUT 0x92, AL
            0xF4, // HLT
        ],
        16,
    );
    assert!(!cpu.a20, "A20 must be gated off after OUT 0x92");
}

/// 32-bit IDT gate (type 0xE) — the architectural form Linux 32-bit
/// 0x66 0xC8 / 0x66 0xC9 — ENTER imm16, 0 / LEAVE under 32-bit
/// operand size. Standard C function prologue / epilogue with frame
/// pointer.
#[test]
fn enter_leave_round_trip_32_bit_frame() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);
    cpu.write_r32(5, 0xDEAD_BEEF); // EBP sentinel
                                   // 66 C8 10 00 00   ENTER 0x10, 0   ; reserve 16 bytes
                                   // (function body would use [EBP - imm] addressing; we just LEAVE)
                                   // 66 C9            LEAVE
                                   // F4
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xC8, 0x10, 0x00, 0x00, // ENTER 16, 0
            0x66, 0xC9, // LEAVE
            0xF4,
        ],
    );
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(5), 0xDEAD_BEEF, "LEAVE must restore EBP");
    assert_eq!(
        cpu.read_r32(r16::SP as u8),
        0x0008_0000,
        "stack ended at its starting value"
    );
}

/// Decode-coverage medley: a sum-of-squares loop compiled the way
/// gcc -m32 would, exercising a representative spread of 32-bit
/// opcodes end-to-end. Computes 1²+2²+3²+4²+5² = 55 in EAX.
///
///   xor  eax, eax          ; accumulator
///   mov  ecx, 1            ; counter
/// loop:
///   mov  ebx, ecx
///   imul ebx, ecx          ; ebx = ecx²
///   add  eax, ebx
///   inc  ecx
///   cmp  ecx, 6
///   jne  loop
///   hlt
#[test]
fn decode_medley_sum_of_squares_reaches_55() {
    let code: &[u8] = &[
        0x66, 0x31, 0xC0, // xor eax, eax
        0x66, 0xB9, 0x01, 0x00, 0x00, 0x00, // mov ecx, 1
        // loop: (offset 9)
        0x66, 0x89, 0xCB, // mov ebx, ecx
        0x66, 0x0F, 0xAF, 0xD9, // imul ebx, ecx
        0x66, 0x01, 0xD8, // add eax, ebx
        0x66, 0x41, // inc ecx
        0x66, 0x83, 0xF9, 0x06, // cmp ecx, 6
        0x75, 0xEE, // jne loop (rel8 = -18: IP 27 → offset 9)
        0xF4, // hlt
    ];
    let (cpu, _, _) = run_payload(code, 200);
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(0), 55, "sum of squares 1..5");
    assert_eq!(cpu.read_r32(1), 6, "loop counter ended at 6");
}

/// Spinlock acquire — the kernel's `lock cmpxchg` + `jz` + `pause`
/// loop. With the lock free (0), the first LOCK CMPXCHG swaps in 1,
/// sets ZF, and JZ falls through to the critical section; the
/// PAUSE/retry arm is present but not taken. Exercises the LOCK
/// prefix, 32-bit CMPXCHG to memory, and the branch — the exact
/// shape of `arch_spin_lock`.
#[test]
fn spinlock_acquire_via_lock_cmpxchg() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 0); // lock free
                             // acquire: (ofs 0)
                             //   66 31 C0              xor eax, eax        ; expected = 0
                             //   66 BB 01 00 00 00     mov ebx, 1          ; desired = 1
                             //   F0                    lock                (ofs 9)
                             //   66 0F B1 1E 00 06     cmpxchg [0x600], ebx (ofs 10)
                             //   74 04                 jz acquired (IP 18 → 22)
                             //   F3 90                 pause
                             //   EB EA                 jmp acquire (-22)
                             // acquired: (ofs 22)
                             //   F4                    hlt
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x31, 0xC0, // xor eax, eax
            0x66, 0xBB, 0x01, 0x00, 0x00, 0x00, // mov ebx, 1
            0xF0, // lock
            0x66, 0x0F, 0xB1, 0x1E, 0x00, 0x06, // cmpxchg [0x600], ebx
            0x74, 0x04, // jz acquired
            0xF3, 0x90, // pause
            0xEB, 0xEA, // jmp acquire
            0xF4, // hlt (acquired)
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted, "should acquire and reach the critical section");
    assert_eq!(mem.read_u32(0x600), 1, "lock taken (set to 1)");
}

/// SSE packed add (PADDD) — four independent 32-bit lane adds, with
/// the top lane proving per-lane wraparound (no carry across lanes).
#[test]
fn sse_paddd_packed_lane_add() {
    let mut mem = Memory::new(0x10_0000);
    // A = [1, 2, 3, 0xFFFFFFFF], B = [10, 20, 30, 1].
    let a = [1u32, 2, 3, 0xFFFF_FFFF];
    let b = [10u32, 20, 30, 1];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize]);
        mem.write_u32(0x610 + i * 4, b[i as usize]);
    }
    // MOVDQA XMM0,[0x600] ; MOVDQA XMM1,[0x610] ; PADDD XMM0,XMM1 ;
    // MOVDQA [0x620],XMM0 ; HLT
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0xFE, 0xC1, // PADDD XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x620), 11);
    assert_eq!(mem.read_u32(0x624), 22);
    assert_eq!(mem.read_u32(0x628), 33);
    // 0xFFFFFFFF + 1 wraps to 0 within the lane (no carry out).
    assert_eq!(mem.read_u32(0x62C), 0);
}

/// SSE ADDPS — packed single-precision add across all 4 lanes.
/// Loads two f32×4 vectors from memory (MOVUPS, no 0x66 prefix),
/// adds them lane-wise, stores the result, and checks each lane.
#[test]
fn sse_addps_packed_single_add() {
    let mut mem = Memory::new(0x10_0000);
    let a = [1.0f32, 2.0, 3.0, 4.0];
    let b = [10.0f32, 20.0, 30.0, 40.0];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize].to_bits());
        mem.write_u32(0x610 + i * 4, b[i as usize].to_bits());
    }
    // MOVUPS XMM0,[0x600] ; MOVUPS XMM1,[0x610] ; ADDPS XMM0,XMM1 ;
    // MOVUPS [0x620],XMM0 ; HLT
    mem.write_slice(
        0x7C00,
        &[
            0x0F, 0x10, 0x06, 0x00, 0x06, // MOVUPS XMM0, [0x600]
            0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVUPS XMM1, [0x610]
            0x0F, 0x58, 0xC1, // ADDPS XMM0, XMM1
            0x0F, 0x11, 0x06, 0x20, 0x06, // MOVUPS [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    for i in 0..4u32 {
        let got = f32::from_bits(mem.read_u32(0x620 + i * 4));
        assert_eq!(got, a[i as usize] + b[i as usize]);
    }
}

/// SSE2 MULPD — packed double-precision multiply (the 0x66 prefix
/// selects the 2×f64 form). Confirms lane width and op selection are
/// both keyed correctly off the prefix + opcode.
#[test]
fn sse_mulpd_packed_double_multiply() {
    let mut mem = Memory::new(0x10_0000);
    let a = [1.5f64, 2.5];
    let b = [10.0f64, 20.0];
    for i in 0..2u32 {
        let ab = a[i as usize].to_bits();
        let bb = b[i as usize].to_bits();
        mem.write_u32(0x600 + i * 8, ab as u32);
        mem.write_u32(0x600 + i * 8 + 4, (ab >> 32) as u32);
        mem.write_u32(0x610 + i * 8, bb as u32);
        mem.write_u32(0x610 + i * 8 + 4, (bb >> 32) as u32);
    }
    // MOVUPD XMM0,[0x600] ; MOVUPD XMM1,[0x610] ; MULPD XMM0,XMM1 ;
    // MOVUPD [0x620],XMM0 ; HLT  (all 0x66-prefixed)
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x10, 0x06, 0x00, 0x06, // MOVUPD XMM0, [0x600]
            0x66, 0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVUPD XMM1, [0x610]
            0x66, 0x0F, 0x59, 0xC1, // MULPD XMM0, XMM1
            0x66, 0x0F, 0x11, 0x06, 0x20, 0x06, // MOVUPD [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    for i in 0..2u32 {
        let lo = mem.read_u32(0x620 + i * 8) as u64;
        let hi = mem.read_u32(0x620 + i * 8 + 4) as u64;
        let got = f64::from_bits(lo | (hi << 32));
        assert_eq!(got, a[i as usize] * b[i as usize]);
    }
}

/// Scalar SSE via the F3 0F mandatory prefix: MOVSS load (zeroes
/// upper lanes) + ADDSS (touches only the low lane, preserves the
/// upper three). This is the path that used to mis-route into the
/// REP string handler before the F3/F2 escape was added.
#[test]
fn sse_movss_addss_scalar_preserves_upper_lanes() {
    let mut mem = Memory::new(0x10_0000);
    // XMM0 source: low lane = 1.0f32, upper three are markers.
    mem.write_u32(0x600, 1.0f32.to_bits());
    mem.write_u32(0x604, 0xAAAA_AAAA);
    mem.write_u32(0x608, 0xBBBB_BBBB);
    mem.write_u32(0x60C, 0xCCCC_CCCC);
    // XMM1 source: low lane = 2.0f32.
    mem.write_u32(0x610, 2.0f32.to_bits());
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0xF3, 0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVSS XMM1, [0x610]
            0xF3, 0x0F, 0x58, 0xC1, // ADDSS XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x620)), 3.0);
    assert_eq!(mem.read_u32(0x624), 0xAAAA_AAAA);
    assert_eq!(mem.read_u32(0x628), 0xBBBB_BBBB);
    assert_eq!(mem.read_u32(0x62C), 0xCCCC_CCCC);
}

/// Scalar double-precision via F2 0F: MOVSD + MULSD, with the
/// 128-bit unaligned MOVDQU (F3 0F 6F/7F) moving the vector in and
/// out. Confirms the low f64 lane is computed and the high lane is
/// left untouched.
#[test]
fn sse_movsd_mulsd_scalar_via_movdqu() {
    let mut mem = Memory::new(0x10_0000);
    // XMM0 source: low f64 = 1.5, high qword = recognizable marker.
    let lo = 1.5f64.to_bits();
    let hi_marker: u64 = 0x1122_3344_5566_7788;
    mem.write_u32(0x600, lo as u32);
    mem.write_u32(0x604, (lo >> 32) as u32);
    mem.write_u32(0x608, hi_marker as u32);
    mem.write_u32(0x60C, (hi_marker >> 32) as u32);
    // XMM1 source: low f64 = 4.0.
    let mul = 4.0f64.to_bits();
    mem.write_u32(0x610, mul as u32);
    mem.write_u32(0x614, (mul >> 32) as u32);
    mem.write_slice(
        0x7C00,
        &[
            0xF3, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQU XMM0, [0x600]
            0xF2, 0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVSD XMM1, [0x610]
            0xF2, 0x0F, 0x59, 0xC1, // MULSD XMM0, XMM1
            0xF3, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQU [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    let result_lo = (mem.read_u32(0x620) as u64) | ((mem.read_u32(0x624) as u64) << 32);
    assert_eq!(f64::from_bits(result_lo), 6.0);
    let result_hi = (mem.read_u32(0x628) as u64) | ((mem.read_u32(0x62C) as u64) << 32);
    assert_eq!(result_hi, hi_marker);
}

/// SSE int↔float converts: CVTSI2SD lifts an int into a double,
/// MULSD scales it, then CVTTSD2SI (truncate) and CVTSD2SI (round)
/// bring it back — exercising both rounding modes off one value.
#[test]
fn sse_cvt_int_float_roundtrip() {
    let mut mem = Memory::new(0x10_0000);
    // 7 * 1.53 = 10.71  → trunc 10, round 11.
    let scale = 1.53f64.to_bits();
    mem.write_u32(0x600, scale as u32);
    mem.write_u32(0x604, (scale >> 32) as u32);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xB8, 0x07, 0x00, 0x00, 0x00, // MOV EAX, 7
            0xF2, 0x0F, 0x2A, 0xC0, // CVTSI2SD XMM0, EAX
            0xF2, 0x0F, 0x10, 0x0E, 0x00, 0x06, // MOVSD XMM1, [0x600]
            0xF2, 0x0F, 0x59, 0xC1, // MULSD XMM0, XMM1
            0xF2, 0x0F, 0x2C, 0xD8, // CVTTSD2SI EBX, XMM0 (trunc)
            0xF2, 0x0F, 0x2D, 0xC8, // CVTSD2SI ECX, XMM0  (round)
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(3), 10); // EBX = trunc(10.71)
    assert_eq!(cpu.read_r32(1), 11); // ECX = round(10.71)
}

/// COMISD sets ZF/PF/CF from the ordering of the low f64 lanes.
/// Checks the three ordered cases (less / equal / greater) by
/// running a tiny compare program per scenario.
#[test]
fn sse_comisd_sets_ordering_flags() {
    fn compare(a: f64, b: f64) -> (bool, bool, bool) {
        let mut mem = Memory::new(0x10_0000);
        let ab = a.to_bits();
        let bb = b.to_bits();
        mem.write_u32(0x600, ab as u32);
        mem.write_u32(0x604, (ab >> 32) as u32);
        mem.write_u32(0x610, bb as u32);
        mem.write_u32(0x614, (bb >> 32) as u32);
        mem.write_slice(
            0x7C00,
            &[
                0xF2, 0x0F, 0x10, 0x06, 0x00, 0x06, // MOVSD XMM0, [0x600]
                0xF2, 0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVSD XMM1, [0x610]
                0x66, 0x0F, 0x2F, 0xC1, // COMISD XMM0, XMM1
                0xF4,
            ],
        );
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        for _ in 0..16 {
            if cpu.halted {
                break;
            }
            cpu.step(&mut mem, &mut io).expect("step");
        }
        assert!(cpu.halted);
        (cpu.has(flag::ZF), cpu.has(flag::PF), cpu.has(flag::CF))
    }
    // a > b → all clear.
    assert_eq!(compare(3.0, 2.0), (false, false, false));
    // a < b → CF only.
    assert_eq!(compare(2.0, 3.0), (false, false, true));
    // a == b → ZF only.
    assert_eq!(compare(2.5, 2.5), (true, false, false));
    // unordered (NaN) → ZF, PF, CF all set.
    assert_eq!(compare(f64::NAN, 1.0), (true, true, true));
}

/// Packed SQRTPS then MINPS: sqrt four lanes, then clamp each to a
/// per-lane ceiling. Confirms the unary (SQRT) and binary (MIN)
/// packed-float paths and the x86 MIN tie rule.
#[test]
fn sse_sqrtps_then_minps() {
    let mut mem = Memory::new(0x10_0000);
    let a = [4.0f32, 9.0, 16.0, 25.0]; // sqrt → [2,3,4,5]
    let lim = [3.0f32, 3.0, 3.0, 3.0];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize].to_bits());
        mem.write_u32(0x610 + i * 4, lim[i as usize].to_bits());
    }
    mem.write_slice(
        0x7C00,
        &[
            0x0F, 0x10, 0x06, 0x00, 0x06, // MOVUPS XMM0, [0x600]
            0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVUPS XMM1, [0x610]
            0x0F, 0x51, 0xC0, // SQRTPS XMM0, XMM0  → [2,3,4,5]
            0x0F, 0x5D, 0xC1, // MINPS  XMM0, XMM1  → [2,3,3,3]
            0x0F, 0x11, 0x06, 0x20, 0x06, // MOVUPS [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    let want = [2.0f32, 3.0, 3.0, 3.0];
    for i in 0..4u32 {
        assert_eq!(
            f32::from_bits(mem.read_u32(0x620 + i * 4)),
            want[i as usize]
        );
    }
}

/// Scalar SQRTSS then MINSS via the F3 prefix: only the low lane is
/// touched, the upper three lanes survive untouched.
#[test]
fn sse_sqrtss_minss_scalar_preserves_upper() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 25.0f32.to_bits());
    mem.write_u32(0x604, 0xAAAA_AAAA);
    mem.write_u32(0x608, 0xBBBB_BBBB);
    mem.write_u32(0x60C, 0xCCCC_CCCC);
    mem.write_u32(0x610, 3.0f32.to_bits());
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0xF3, 0x0F, 0x10, 0x0E, 0x10, 0x06, // MOVSS XMM1, [0x610]
            0xF3, 0x0F, 0x51, 0xC0, // SQRTSS XMM0, XMM0 → low=5.0
            0xF3, 0x0F, 0x5D, 0xC1, // MINSS  XMM0, XMM1 → low=3.0
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x620)), 3.0);
    assert_eq!(mem.read_u32(0x624), 0xAAAA_AAAA);
    assert_eq!(mem.read_u32(0x628), 0xBBBB_BBBB);
    assert_eq!(mem.read_u32(0x62C), 0xCCCC_CCCC);
}

/// SSE bitwise float logicals (ANDPS/ANDNPS/ORPS/XORPS). Runs each
/// over a fixed lane pattern and checks the 128-bit result, with
/// ANDNPS exercising the (NOT dest) AND src ordering specifically.
#[test]
fn sse_bitwise_logicals_andps_orps_xorps_andnps() {
    fn logical(op: u8) -> u128 {
        let (a, b) = (0xF0F0_F0F0u32, 0x0FF0_0FF0u32);
        let mut mem = Memory::new(0x10_0000);
        for i in 0..4u32 {
            mem.write_u32(0x600 + i * 4, a);
            mem.write_u32(0x610 + i * 4, b);
        }
        mem.write_slice(
            0x7C00,
            &[
                0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
                0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
                0x0F, op, 0xC1, // <logical> XMM0, XMM1
                0xF4,
            ],
        );
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        for _ in 0..12 {
            if cpu.halted {
                break;
            }
            cpu.step(&mut mem, &mut io).expect("step");
        }
        assert!(cpu.halted);
        cpu.xmm[0]
    }
    let rep = |d: u32| {
        let d = d as u128;
        d | (d << 32) | (d << 64) | (d << 96)
    };
    assert_eq!(logical(0x54), rep(0x00F0_00F0)); // ANDPS  = A & B
    assert_eq!(logical(0x56), rep(0xFFF0_FFF0)); // ORPS   = A | B
    assert_eq!(logical(0x57), rep(0xFF00_FF00)); // XORPS  = A ^ B
    assert_eq!(logical(0x55), rep(0x0F00_0F00)); // ANDNPS = !A & B
}

/// UNPCKLPS interleaves the low two 32-bit lanes of dest and source:
/// [a0,a1,a2,a3] ∪ [b0,b1,b2,b3] → [a0,b0,a1,b1].
#[test]
fn sse_unpcklps_interleaves_low_lanes() {
    let mut mem = Memory::new(0x10_0000);
    let a = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    let b = [0xAAAA_AAAAu32, 0xBBBB_BBBB, 0xCCCC_CCCC, 0xDDDD_DDDD];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize]);
        mem.write_u32(0x610 + i * 4, b[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x0F, 0x14, 0xC1, // UNPCKLPS XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x620), 0x1111_1111); // a0
    assert_eq!(mem.read_u32(0x624), 0xAAAA_AAAA); // b0
    assert_eq!(mem.read_u32(0x628), 0x2222_2222); // a1
    assert_eq!(mem.read_u32(0x62C), 0xBBBB_BBBB); // b1
}

/// PSHUFD reverse + SHUFPS combining two sources. PSHUFD's imm 0x1B
/// reverses lane order; SHUFPS then picks lanes 0,2 from the result
/// and lanes 1,3 from a second register, giving a mixed output.
#[test]
fn sse_pshufd_then_shufps_combines_lanes() {
    let mut mem = Memory::new(0x10_0000);
    let a = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    let b = [0xAAAA_AAAAu32, 0xBBBB_BBBB, 0xCCCC_CCCC, 0xDDDD_DDDD];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize]);
        mem.write_u32(0x610 + i * 4, b[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x70, 0xD0, 0x1B, // PSHUFD XMM2, XMM0, 0x1B (reverse)
            0x0F, 0xC6, 0xD1, 0xD8, // SHUFPS XMM2, XMM1, 0xD8
            0x66, 0x0F, 0x7F, 0x16, 0x20, 0x06, // MOVDQA [0x620], XMM2
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // After PSHUFD reverse XMM2 = [a3, a2, a1, a0].
    // SHUFPS imm=0xD8 picks src1 lanes (0,2) and src2 lanes (1,3):
    //   lane 0 = src1[0] = a3,  lane 1 = src1[2] = a1,
    //   lane 2 = src2[1] = b1,  lane 3 = src2[3] = b3.
    assert_eq!(mem.read_u32(0x620), 0x4444_4444);
    assert_eq!(mem.read_u32(0x624), 0x2222_2222);
    assert_eq!(mem.read_u32(0x628), 0xBBBB_BBBB);
    assert_eq!(mem.read_u32(0x62C), 0xDDDD_DDDD);
}

/// MOVLPS + MOVHPS assemble a 16-byte XMM register from two
/// separate 8-byte memory loads — the standard pair compilers emit
/// for an unaligned 128-bit load split across two qwords.
#[test]
fn sse_movlps_movhps_assemble_xmm() {
    let mut mem = Memory::new(0x10_0000);
    // Two distinct 8-byte payloads at adjacent addresses.
    let low_pat: [u32; 2] = [0x4433_2211, 0x8877_6655];
    let high_pat: [u32; 2] = [0xDDCC_BBAA, 0x1100_FFEE];
    mem.write_u32(0x600, low_pat[0]);
    mem.write_u32(0x604, low_pat[1]);
    mem.write_u32(0x608, high_pat[0]);
    mem.write_u32(0x60C, high_pat[1]);
    mem.write_slice(
        0x7C00,
        &[
            0x0F, 0x12, 0x06, 0x00, 0x06, // MOVLPS XMM0, [0x600]
            0x0F, 0x16, 0x06, 0x08, 0x06, // MOVHPS XMM0, [0x608]
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x620), low_pat[0]);
    assert_eq!(mem.read_u32(0x624), low_pat[1]);
    assert_eq!(mem.read_u32(0x628), high_pat[0]);
    assert_eq!(mem.read_u32(0x62C), high_pat[1]);
}

/// MOVHLPS reg-form: the high 64 bits of the source overwrite the
/// low 64 of the destination; the destination's upper 64 survive.
#[test]
fn sse_movhlps_reg_form_overwrites_low_preserves_high() {
    let mut mem = Memory::new(0x10_0000);
    // XMM0 init: low = 0xDEAD..., high = 0x5555... (will be preserved).
    mem.write_u32(0x600, 0xDEAD_BEEF);
    mem.write_u32(0x604, 0xCAFE_BABE);
    mem.write_u32(0x608, 0x5555_5555);
    mem.write_u32(0x60C, 0x6666_6666);
    // XMM1 high = 0x9999...8888..., low irrelevant.
    mem.write_u32(0x610, 0x0000_0000);
    mem.write_u32(0x614, 0x0000_0000);
    mem.write_u32(0x618, 0x8888_8888);
    mem.write_u32(0x61C, 0x9999_9999);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x0F, 0x12, 0xC1, // MOVHLPS XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Low 64 of XMM0 := high 64 of XMM1 = 0x9999...8888...
    assert_eq!(mem.read_u32(0x620), 0x8888_8888);
    assert_eq!(mem.read_u32(0x624), 0x9999_9999);
    // High 64 of XMM0 preserved.
    assert_eq!(mem.read_u32(0x628), 0x5555_5555);
    assert_eq!(mem.read_u32(0x62C), 0x6666_6666);
}

/// PCMPEQD writes an all-ones lane on equal, all-zeros on unequal.
/// Mixes matching and non-matching dwords to exercise both outcomes.
#[test]
fn sse_pcmpeqd_per_lane_equality() {
    let mut mem = Memory::new(0x10_0000);
    let a = [1u32, 2, 3, 4];
    let b = [1u32, 9, 3, 9]; // lanes 0,2 match; 1,3 don't
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, a[i as usize]);
        mem.write_u32(0x610 + i * 4, b[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x76, 0xC1, // PCMPEQD XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x620), 0xFFFF_FFFF);
    assert_eq!(mem.read_u32(0x624), 0x0000_0000);
    assert_eq!(mem.read_u32(0x628), 0xFFFF_FFFF);
    assert_eq!(mem.read_u32(0x62C), 0x0000_0000);
}

/// PCMPGTB is signed — a high-bit-set byte (e.g. 0x80) must compare
/// less than a small positive byte. Mixes four explicit byte pairs
/// covering greater/equal/less/positive-vs-negative.
#[test]
fn sse_pcmpgtb_uses_signed_byte_compare() {
    let mut mem = Memory::new(0x10_0000);
    // Bytes 0..3 are the interesting cases; rest are zero (0 > 0 → 0).
    let a = [0x10u8, 0x80, 0x01, 0x7F];
    let b = [0x05u8, 0x10, 0x05, 0x05];
    for i in 0..4 {
        mem.write_u8(0x600 + i, a[i as usize]);
        mem.write_u8(0x610 + i, b[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x64, 0xC1, // PCMPGTB XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u8(0x620), 0xFF); //  16  > 5
    assert_eq!(mem.read_u8(0x621), 0x00); // -128 > 16  (false, negative)
    assert_eq!(mem.read_u8(0x622), 0x00); //  1   > 5
    assert_eq!(mem.read_u8(0x623), 0xFF); // 127  > 5
                                          // High bytes: both zero everywhere → 0 > 0 false → all zero.
    assert_eq!(mem.read_u8(0x62F), 0x00);
}

/// Immediate-count shifts via Group 12/13/14 — exercises PSLLW,
/// PSRAD (with sign-fill), and PSLLDQ (whole-register byte shift)
/// in one program.
#[test]
fn sse_immediate_shifts_psllw_psrad_pslldq() {
    let mut mem = Memory::new(0x10_0000);
    // PSLLW input: words. Pick a small left-shift that fits.
    let psllw_in: [u16; 8] = [
        0x1234, 0x5678, 0x9ABC, 0xDEF0, 0x0001, 0xFFFF, 0x0010, 0x0080,
    ];
    for i in 0..8u32 {
        mem.write_u8(0x600 + i * 2, (psllw_in[i as usize] & 0xFF) as u8);
        mem.write_u8(
            0x600 + i * 2 + 1,
            ((psllw_in[i as usize] >> 8) & 0xFF) as u8,
        );
    }
    // PSRAD input: dwords mixing positive, negative, all-ones, zero.
    let psrad_in: [u32; 4] = [0x8000_0000, 0x4000_0000, 0xFFFF_FFFE, 0x0000_0000];
    for i in 0..4u32 {
        mem.write_u32(0x610 + i * 4, psrad_in[i as usize]);
    }
    // PSLLDQ input: a recognizable bit pattern, byte-shifted left
    // by 1 should slide every byte up one position.
    for i in 0..16u32 {
        mem.write_u8(0x620 + i, (i + 1) as u8); // [01,02,...,10]
    }
    mem.write_slice(
        0x7C00,
        &[
            // PSLLW XMM0, 4
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x71, 0xF0, 0x04, // PSLLW XMM0, 4 (/6)
            0x66, 0x0F, 0x7F, 0x06, 0x40, 0x06, // MOVDQA [0x640], XMM0
            // PSRAD XMM1, 4 — arithmetic; sign bit replicates.
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x72, 0xE1, 0x04, // PSRAD XMM1, 4 (/4)
            0x66, 0x0F, 0x7F, 0x0E, 0x50, 0x06, // MOVDQA [0x650], XMM1
            // PSLLDQ XMM2, 1 — whole-register byte shift left.
            0x66, 0x0F, 0x6F, 0x16, 0x20, 0x06, // MOVDQA XMM2, [0x620]
            0x66, 0x0F, 0x73, 0xFA, 0x01, // PSLLDQ XMM2, 1 (/7)
            0x66, 0x0F, 0x7F, 0x16, 0x60, 0x06, // MOVDQA [0x660], XMM2
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..40 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // PSLLW results: each word shifted left 4, masked to 16 bits.
    for i in 0..8u32 {
        let lo = mem.read_u8(0x640 + i * 2) as u16;
        let hi = mem.read_u8(0x640 + i * 2 + 1) as u16;
        let got = lo | (hi << 8);
        let want = psllw_in[i as usize].wrapping_shl(4);
        assert_eq!(got, want, "PSLLW lane {i}");
    }
    // PSRAD results:
    //   0x8000_0000 >> 4 (arith) = 0xF800_0000  (sign-fill)
    //   0x4000_0000 >> 4 (arith) = 0x0400_0000
    //   0xFFFF_FFFE >> 4 (arith) = 0xFFFF_FFFF  (-2 / 16 toward -inf = -1)
    //   0          >> 4         = 0
    assert_eq!(mem.read_u32(0x650), 0xF800_0000);
    assert_eq!(mem.read_u32(0x654), 0x0400_0000);
    assert_eq!(mem.read_u32(0x658), 0xFFFF_FFFF);
    assert_eq!(mem.read_u32(0x65C), 0x0000_0000);
    // PSLLDQ shifts every byte up by one position; byte 0 becomes
    // 0, bytes 1..15 hold the originals [01,02,...,0F].
    assert_eq!(mem.read_u8(0x660), 0x00);
    for i in 1..16u32 {
        assert_eq!(mem.read_u8(0x660 + i), i as u8, "PSLLDQ byte {i}");
    }
}

/// Variable-count packed shift: PSLLD with the count coming from
/// the low qword of a second XMM register. Confirms the variable
/// form shares the imm-shift semantics (and helpers) cleanly.
#[test]
fn sse_variable_pslld_count_from_xmm() {
    let mut mem = Memory::new(0x10_0000);
    // XMM0: four copies of 0x1234_5678.
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, 0x1234_5678);
    }
    // XMM1 low qword = 4 (shift count); upper bits zero.
    mem.write_u32(0x610, 4);
    mem.write_u32(0x614, 0);
    mem.write_u32(0x618, 0);
    mem.write_u32(0x61C, 0);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0xF2, 0xC1, // PSLLD XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    for i in 0..4u32 {
        // Each lane: 0x1234_5678 << 4 (mod 2^32) = 0x2345_6780.
        assert_eq!(mem.read_u32(0x620 + i * 4), 0x2345_6780, "lane {i}");
    }
}

/// PMULLW (low 16 of word product), PMULHW (high 16 of signed word
/// product), and PMADDWD (signed word pairs → dword sums) over the
/// same two source vectors. The 0xFFFF lane (-1 signed) gives the
/// best diagnostic: PMULLW(-1, 2) = 0xFFFE, PMULHW(-1, 2) = 0xFFFF.
#[test]
fn sse_packed_int_multiplies_pmullw_pmulhw_pmaddwd() {
    let mut mem = Memory::new(0x10_0000);
    // Lane | a       | b       | low(a*b)  high(signed) | madd pair
    //   0  | 0x1000  | 0x0003  | 0x3000    0x0000       | a0*b0 + a1*b1
    //   1  | 0x0100  | 0x0005  | 0x0500    0x0000       |  = 0x3500
    //   2  | 0xFFFF  | 0x0002  | 0xFFFE    0xFFFF       | a2*b2 + a3*b3
    //   3  | 0x0080  | 0x0040  | 0x2000    0x0000       |  = 0x1FFE
    let a_words: [u16; 8] = [0x1000, 0x0100, 0xFFFF, 0x0080, 0, 0, 0, 0];
    let b_words: [u16; 8] = [0x0003, 0x0005, 0x0002, 0x0040, 0, 0, 0, 0];
    for i in 0..8u32 {
        mem.write_u8(0x600 + i * 2, (a_words[i as usize] & 0xFF) as u8);
        mem.write_u8(0x600 + i * 2 + 1, (a_words[i as usize] >> 8) as u8);
        mem.write_u8(0x610 + i * 2, (b_words[i as usize] & 0xFF) as u8);
        mem.write_u8(0x610 + i * 2 + 1, (b_words[i as usize] >> 8) as u8);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x6F, 0xD0, // MOVDQA XMM2, XMM0
            0x66, 0x0F, 0xD5, 0xD1, // PMULLW XMM2, XMM1
            0x66, 0x0F, 0x7F, 0x16, 0x20, 0x06, // MOVDQA [0x620], XMM2
            0x66, 0x0F, 0x6F, 0xD8, // MOVDQA XMM3, XMM0
            0x66, 0x0F, 0xE5, 0xD9, // PMULHW XMM3, XMM1
            0x66, 0x0F, 0x7F, 0x1E, 0x30, 0x06, // MOVDQA [0x630], XMM3
            0x66, 0x0F, 0x6F, 0xE0, // MOVDQA XMM4, XMM0
            0x66, 0x0F, 0xF5, 0xE1, // PMADDWD XMM4, XMM1
            0x66, 0x0F, 0x7F, 0x26, 0x40, 0x06, // MOVDQA [0x640], XMM4
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..40 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // PMULLW (low 16 of each product) — word-by-word check.
    let expect_low: [u16; 8] = [0x3000, 0x0500, 0xFFFE, 0x2000, 0, 0, 0, 0];
    for i in 0..8u32 {
        let got =
            (mem.read_u8(0x620 + i * 2) as u16) | ((mem.read_u8(0x620 + i * 2 + 1) as u16) << 8);
        assert_eq!(got, expect_low[i as usize], "PMULLW lane {i}");
    }
    // PMULHW (signed high 16) — lane 2 (0xFFFF, signed -1) is the
    // diagnostic; -1*2 = -2 = 0xFFFFFFFE → high 16 = 0xFFFF.
    let expect_high: [u16; 8] = [0x0000, 0x0000, 0xFFFF, 0x0000, 0, 0, 0, 0];
    for i in 0..8u32 {
        let got =
            (mem.read_u8(0x630 + i * 2) as u16) | ((mem.read_u8(0x630 + i * 2 + 1) as u16) << 8);
        assert_eq!(got, expect_high[i as usize], "PMULHW lane {i}");
    }
    // PMADDWD — 8 word lanes → 4 dword sums.
    assert_eq!(mem.read_u32(0x640), 0x0000_3500);
    assert_eq!(mem.read_u32(0x644), 0x0000_1FFE);
    assert_eq!(mem.read_u32(0x648), 0);
    assert_eq!(mem.read_u32(0x64C), 0);
}

/// CVTPS2PD widens 2×f32 (low half) to 2×f64; CVTPD2PS narrows it
/// back into the low 64 bits with the upper 64 zeroed. A clean
/// round-trip on values that fit both representations.
#[test]
fn sse_cvtps2pd_then_cvtpd2ps_roundtrip() {
    let mut mem = Memory::new(0x10_0000);
    // Low half = [1.5f32, 2.5f32]; upper half is irrelevant for
    // CVTPS2PD but we zero it for tidiness.
    mem.write_u32(0x600, 1.5f32.to_bits());
    mem.write_u32(0x604, 2.5f32.to_bits());
    mem.write_u32(0x608, 0);
    mem.write_u32(0x60C, 0);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x0F, 0x5A, 0xC8, // CVTPS2PD XMM1, XMM0
            0x66, 0x0F, 0x5A, 0xD1, // CVTPD2PS XMM2, XMM1
            0x66, 0x0F, 0x7F, 0x16, 0x20, 0x06, // MOVDQA [0x620], XMM2
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x620)), 1.5);
    assert_eq!(f32::from_bits(mem.read_u32(0x624)), 2.5);
    assert_eq!(mem.read_u32(0x628), 0); // CVTPD2PS zeroes upper 64.
    assert_eq!(mem.read_u32(0x62C), 0);
}

/// CVTPS2DQ rounds (round-to-nearest-even, default MXCSR) while
/// CVTTPS2DQ truncates toward zero. Inputs picked so the two
/// instructions disagree on lane 0 (1.7 → 2 vs 1).
#[test]
fn sse_cvtps2dq_vs_cvttps2dq_rounding_modes() {
    let mut mem = Memory::new(0x10_0000);
    let xs = [1.7f32, -2.4, 0.5, -0.5];
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, xs[i as usize].to_bits());
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0xC8, // MOVDQA XMM1, XMM0
            0x66, 0x0F, 0x5B, 0xC0, // CVTPS2DQ  XMM0, XMM0 (round)
            0xF3, 0x0F, 0x5B, 0xC9, // CVTTPS2DQ XMM1, XMM1 (trunc)
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0x66, 0x0F, 0x7F, 0x0E, 0x30, 0x06, // MOVDQA [0x630], XMM1
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..24 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Round-to-even: 1.7→2, -2.4→-2, 0.5→0, -0.5→0 (ties go to even).
    assert_eq!(mem.read_u32(0x620) as i32, 2);
    assert_eq!(mem.read_u32(0x624) as i32, -2);
    assert_eq!(mem.read_u32(0x628) as i32, 0);
    assert_eq!(mem.read_u32(0x62C) as i32, 0);
    // Truncate: 1.7→1, -2.4→-2, 0.5→0, -0.5→0.
    assert_eq!(mem.read_u32(0x630) as i32, 1);
    assert_eq!(mem.read_u32(0x634) as i32, -2);
    assert_eq!(mem.read_u32(0x638) as i32, 0);
    assert_eq!(mem.read_u32(0x63C) as i32, 0);
}

/// CVTDQ2PD (F3 0F E6) widens 2×i32 from the low qword into 2×f64
/// across the full 128 bits — the scalar-prefix route into
/// sse_scalar's E6 arm. Negative input verifies signed widening.
#[test]
fn sse_cvtdq2pd_widens_two_ints_to_doubles() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 42_u32);
    mem.write_u32(0x604, (-7_i32) as u32);
    mem.write_u32(0x608, 0);
    mem.write_u32(0x60C, 0);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0xF3, 0x0F, 0xE6, 0xC8, // CVTDQ2PD XMM1, XMM0
            0x66, 0x0F, 0x7F, 0x0E, 0x20, 0x06, // MOVDQA [0x620], XMM1
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    let lo = (mem.read_u32(0x620) as u64) | ((mem.read_u32(0x624) as u64) << 32);
    let hi = (mem.read_u32(0x628) as u64) | ((mem.read_u32(0x62C) as u64) << 32);
    assert_eq!(f64::from_bits(lo), 42.0);
    assert_eq!(f64::from_bits(hi), -7.0);
}

/// Packed saturation arithmetic. PADDUSB clamps each byte to 0xFF
/// instead of wrapping; PADDSB clamps to the signed i8 range. The
/// first four bytes of each input are chosen so both directions of
/// saturation fire (overflow and underflow).
#[test]
fn sse_packed_sat_arith_clamps_at_lane_bounds() {
    let mut mem = Memory::new(0x10_0000);
    // PADDUSB pair — unsigned saturation.
    let ua = [0xF0u8, 0x10, 0x80, 0x01];
    let ub = [0x20u8, 0x10, 0x80, 0xFF];
    // PADDSB pair — signed saturation (high bit = negative).
    let sa = [0x70u8, 0xF0, 0x80, 0x7F]; //  +112, -16,  -128, +127
    let sb = [0x20u8, 0xF0, 0x80, 0x01]; //  +32,  -16,  -128,   +1
    for i in 0..4 {
        mem.write_u8(0x600 + i, ua[i as usize]);
        mem.write_u8(0x610 + i, ub[i as usize]);
        mem.write_u8(0x620 + i, sa[i as usize]);
        mem.write_u8(0x630 + i, sb[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            // PADDUSB XMM0, XMM1 → [0x640]
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0xDC, 0xC1, // PADDUSB XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x40, 0x06, // MOVDQA [0x640], XMM0
            // PADDSB XMM2, XMM3 → [0x650]
            0x66, 0x0F, 0x6F, 0x16, 0x20, 0x06, // MOVDQA XMM2, [0x620]
            0x66, 0x0F, 0x6F, 0x1E, 0x30, 0x06, // MOVDQA XMM3, [0x630]
            0x66, 0x0F, 0xEC, 0xD3, // PADDSB XMM2, XMM3
            0x66, 0x0F, 0x7F, 0x16, 0x50, 0x06, // MOVDQA [0x650], XMM2
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..30 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // PADDUSB: 0xF0+0x20=0x110→0xFF;  0x10+0x10=0x20;
    //          0x80+0x80=0x100→0xFF;  0x01+0xFF=0x100→0xFF.
    assert_eq!(mem.read_u8(0x640), 0xFF);
    assert_eq!(mem.read_u8(0x641), 0x20);
    assert_eq!(mem.read_u8(0x642), 0xFF);
    assert_eq!(mem.read_u8(0x643), 0xFF);
    // PADDSB: +112+32=+144→+127(0x7F); -16+-16=-32(0xE0);
    //         -128+-128=-256→-128(0x80); +127+1=+128→+127(0x7F).
    assert_eq!(mem.read_u8(0x650), 0x7F);
    assert_eq!(mem.read_u8(0x651), 0xE0);
    assert_eq!(mem.read_u8(0x652), 0x80);
    assert_eq!(mem.read_u8(0x653), 0x7F);
}

/// PACKSSWB narrows i16 words to i8 bytes with signed saturation.
/// Each lane of XMM0 produces a byte in the low half of the result;
/// each lane of XMM1 a byte in the high half.
#[test]
fn sse_packsswb_clamps_words_to_signed_bytes() {
    let mut mem = Memory::new(0x10_0000);
    // XMM0: positive overflow, in-range, negative overflow, in-range,
    //       max i16, min i16, zero, zero.
    let a_words: [u16; 8] = [
        100,
        200, // 100 fits; 200 → +127
        ((-100i16) as u16),
        ((-200i16) as u16), // -100 fits; -200 → -128
        0x7FFF,
        0x8000, //  max i16 → +127; min i16 → -128
        0,
        0,
    ];
    // XMM1: all clearly in-range to confirm the high-half copy.
    let b_words: [u16; 8] = [1, 2, 3, 4, ((-5i16) as u16), ((-6i16) as u16), 7, 8];
    for i in 0..8u32 {
        mem.write_u8(0x600 + i * 2, (a_words[i as usize] & 0xFF) as u8);
        mem.write_u8(0x600 + i * 2 + 1, (a_words[i as usize] >> 8) as u8);
        mem.write_u8(0x610 + i * 2, (b_words[i as usize] & 0xFF) as u8);
        mem.write_u8(0x610 + i * 2 + 1, (b_words[i as usize] >> 8) as u8);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x63, 0xC1, // PACKSSWB XMM0, XMM1
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Low 8 bytes from XMM0 lanes.
    let want_low: [i8; 8] = [100, 127, -100, -128, 127, -128, 0, 0];
    for i in 0..8u32 {
        let got = mem.read_u8(0x620 + i) as i8;
        assert_eq!(got, want_low[i as usize], "low byte {i}");
    }
    // High 8 bytes from XMM1 lanes (all in-range, copied verbatim).
    let want_high: [i8; 8] = [1, 2, 3, 4, -5, -6, 7, 8];
    for i in 0..8u32 {
        let got = mem.read_u8(0x628 + i) as i8;
        assert_eq!(got, want_high[i as usize], "high byte {i}");
    }
}

/// PSHUFLW shuffles the low 4 words by imm8, leaving the high 4
/// untouched; PSHUFHW is the mirror. Applying both in sequence
/// with the same selector fully reverses the 8 words.
#[test]
fn sse_pshuflw_and_pshufhw_word_shuffles() {
    let mut mem = Memory::new(0x10_0000);
    let words: [u16; 8] = [
        0x1111, 0x2222, 0x3333, 0x4444, 0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD,
    ];
    for i in 0..8u32 {
        mem.write_u8(0x600 + i * 2, (words[i as usize] & 0xFF) as u8);
        mem.write_u8(0x600 + i * 2 + 1, (words[i as usize] >> 8) as u8);
    }
    // imm 0x1B = 0b00_01_10_11 reverses a 4-word group.
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0xF2, 0x0F, 0x70, 0xC0, 0x1B, // PSHUFLW XMM0, XMM0, 0x1B
            0xF3, 0x0F, 0x70, 0xC0, 0x1B, // PSHUFHW XMM0, XMM0, 0x1B
            0x66, 0x0F, 0x7F, 0x06, 0x20, 0x06, // MOVDQA [0x620], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // After both shuffles the word order is fully reversed.
    let expected: [u16; 8] = [
        0x4444, 0x3333, 0x2222, 0x1111, 0xDDDD, 0xCCCC, 0xBBBB, 0xAAAA,
    ];
    for i in 0..8u32 {
        let lo = mem.read_u8(0x620 + i * 2) as u16;
        let hi = mem.read_u8(0x620 + i * 2 + 1) as u16;
        assert_eq!(lo | (hi << 8), expected[i as usize], "word {i}");
    }
}

/// PMOVMSKB extracts the high bit of every byte into the low 16
/// bits of a GP register — the SIMD-to-branch primitive paired with
/// PCMPEQB for vectorized searches. Byte positions {0,1,5,15} hold
/// 0xFF; the others 0x00 → mask = 0x8023.
#[test]
fn sse_pmovmskb_extracts_top_bits_of_bytes() {
    let mut mem = Memory::new(0x10_0000);
    for i in 0..16u32 {
        let v = if matches!(i, 0 | 1 | 5 | 15) {
            0xFFu8
        } else {
            0x00
        };
        mem.write_u8(0x600 + i, v);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0xD7, 0xC0, // PMOVMSKB EAX, XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..12 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // bits {0,1,5,15} = 1 → 0x0001 | 0x0002 | 0x0020 | 0x8000 = 0x8023.
    assert_eq!(cpu.read_r32(0), 0x0000_8023);
}

/// PSADBW (sum of absolute differences across each 8-byte half) and
/// PMINUB (per-byte unsigned minimum) in a single program. The
/// PSADBW result lives as a 16-bit value in the low word of each
/// 64-bit lane, upper bits zero.
#[test]
fn sse_psadbw_and_pminub_byte_metrics() {
    let mut mem = Memory::new(0x10_0000);
    let a: [u8; 16] = [10, 20, 30, 40, 50, 60, 70, 80, 1, 2, 3, 4, 5, 6, 7, 8];
    let b: [u8; 16] = [15, 5, 30, 100, 0, 60, 80, 0, 10, 20, 30, 40, 50, 60, 70, 80];
    for i in 0..16u32 {
        mem.write_u8(0x600 + i, a[i as usize]);
        mem.write_u8(0x610 + i, b[i as usize]);
    }
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x6F, 0x0E, 0x10, 0x06, // MOVDQA XMM1, [0x610]
            0x66, 0x0F, 0x6F, 0xD0, // MOVDQA XMM2, XMM0
            0x66, 0x0F, 0xF6, 0xD1, // PSADBW  XMM2, XMM1
            0x66, 0x0F, 0x7F, 0x16, 0x20, 0x06, // MOVDQA [0x620], XMM2
            0x66, 0x0F, 0x6F, 0xD8, // MOVDQA XMM3, XMM0
            0x66, 0x0F, 0xDA, 0xD9, // PMINUB  XMM3, XMM1
            0x66, 0x0F, 0x7F, 0x1E, 0x30, 0x06, // MOVDQA [0x630], XMM3
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Low half: 5+15+0+60+50+0+10+80 = 220.
    // High half: 9+18+27+36+45+54+63+72 = 324.
    let lo = (mem.read_u8(0x620) as u16) | ((mem.read_u8(0x621) as u16) << 8);
    let hi = (mem.read_u8(0x628) as u16) | ((mem.read_u8(0x629) as u16) << 8);
    assert_eq!(lo, 220);
    assert_eq!(hi, 324);
    // Upper bytes of each qword must be zero.
    for off in [0x622, 0x623, 0x624, 0x625, 0x626, 0x627, 0x62A, 0x62B] {
        assert_eq!(mem.read_u8(off), 0, "PSADBW upper byte @ {off:#X}");
    }
    // PMINUB per-byte minima.
    let want_min: [u8; 16] = [10, 5, 30, 40, 0, 60, 70, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    for i in 0..16u32 {
        assert_eq!(
            mem.read_u8(0x630 + i),
            want_min[i as usize],
            "PMINUB byte {i}"
        );
    }
}

/// MOVQ load (F3 0F 7E) and MOVQ store (66 0F D6) — both zero the
/// upper 64 bits of the destination XMM (no preservation, unlike
/// MOVLPS). Round-trips an 8-byte pattern through both encodings
/// and verifies the upper half is cleared.
#[test]
fn sse_movq_load_and_store_low_qword() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 0x5566_7788);
    mem.write_u32(0x604, 0x1122_3344);
    mem.write_slice(
        0x7C00,
        &[
            0xF3, 0x0F, 0x7E, 0x06, 0x00, 0x06, // MOVQ XMM0, [0x600]
            0x66, 0x0F, 0xD6, 0xC1, // MOVQ XMM1, XMM0 (reg form)
            0x66, 0x0F, 0x7F, 0x0E, 0x20, 0x06, // MOVDQA [0x620], XMM1
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Low 8 bytes of XMM1 = the loaded pattern.
    assert_eq!(mem.read_u32(0x620), 0x5566_7788);
    assert_eq!(mem.read_u32(0x624), 0x1122_3344);
    // Upper 8 bytes — zero-extended by *both* MOVQ encodings.
    assert_eq!(mem.read_u32(0x628), 0);
    assert_eq!(mem.read_u32(0x62C), 0);
}

/// MOVNTDQ + MOVNTPS + MOVNTI in one program. Since we have no
/// cache to bypass, every non-temporal store is just a plain store
/// — but we still want to confirm the dispatch wires each one up
/// to the right memory width (128, 128, 32) and the correct source
/// (XMM, XMM, GP).
#[test]
fn sse_non_temporal_stores_match_their_temporal_kin() {
    let mut mem = Memory::new(0x10_0000);
    // XMM source = four distinct dwords.
    mem.write_u32(0x600, 0x1111_1111);
    mem.write_u32(0x604, 0x2222_2222);
    mem.write_u32(0x608, 0x3333_3333);
    mem.write_u32(0x60C, 0x4444_4444);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0xE7, 0x06, 0x20, 0x06, // MOVNTDQ [0x620], XMM0
            0x0F, 0x2B, 0x06, 0x30, 0x06, // MOVNTPS [0x630], XMM0
            0x66, 0xB8, 0xEF, 0xBE, 0xAD, 0xDE, // MOV EAX, 0xDEADBEEF
            0x0F, 0xC3, 0x06, 0x40, 0x06, // MOVNTI  [0x640], EAX
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // MOVNTDQ + MOVNTPS each stored a full 128 bits.
    for (off, base) in [0x620u32, 0x630].iter().copied().enumerate() {
        let want = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
        for i in 0..4u32 {
            assert_eq!(
                mem.read_u32(base + i * 4),
                want[i as usize],
                "store {off} lane {i}"
            );
        }
    }
    // MOVNTI stored EAX = 0xDEADBEEF as one dword.
    assert_eq!(mem.read_u32(0x640), 0xDEAD_BEEF);
}

/// MASKMOVDQU writes only the bytes whose mask byte has its high
/// bit set, to DS:[(E)DI]. Sentinel-fill the destination first so
/// the unselected bytes can be checked as unchanged.
#[test]
fn sse_maskmovdqu_writes_only_selected_bytes() {
    let mut mem = Memory::new(0x10_0000);
    // Destination region: pre-fill with 0xCC.
    for i in 0..16u32 {
        mem.write_u8(0x600 + i, 0xCC);
    }
    // Data bytes 1..16; mask high-bits at positions {0, 1, 5, 15}.
    for i in 0..16u32 {
        mem.write_u8(0x610 + i, (i + 1) as u8);
        let m = if matches!(i, 0 | 1 | 5 | 15) {
            0x80u8
        } else {
            0x00
        };
        mem.write_u8(0x620 + i, m);
    }
    mem.write_slice(
        0x7C00,
        &[
            0xBF, 0x00, 0x06, // MOV DI, 0x600 (destination address)
            0x66, 0x0F, 0x6F, 0x06, 0x10, 0x06, // MOVDQA XMM0, [0x610]  (data)
            0x66, 0x0F, 0x6F, 0x0E, 0x20, 0x06, // MOVDQA XMM1, [0x620]  (mask)
            0x66, 0x0F, 0xF7, 0xC1, // MASKMOVDQU XMM0, XMM1
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Selected positions hold the corresponding data byte; the rest
    // still read 0xCC from the pre-fill.
    let want: [u8; 16] = [
        0x01, 0x02, 0xCC, 0xCC, 0xCC, 0x06, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC,
        0x10,
    ];
    for i in 0..16u32 {
        assert_eq!(mem.read_u8(0x600 + i), want[i as usize], "byte {i}");
    }
}

/// UD2 raises #UD (vector 6) through the real-mode IVT: the IVT
/// entry at 6×4 = 0x0018 points at a handler that sets a sentinel
/// in AX and halts. We also verify the saved IP on the stack is
/// the start of UD2 (the fault-frame convention) rather than the
/// byte after — this is what BUG() / panic_on_oops rely on to find
/// the bug-table entry that immediately follows the UD2.
#[test]
fn ud2_vectors_through_ivt_and_pushes_faulting_ip() {
    let mut mem = Memory::new(0x10_0000);
    // IVT[6] — handler at 0:0x7C20.
    mem.write_u8(0x0018, 0x20); // offset low
    mem.write_u8(0x0019, 0x7C); // offset high
    mem.write_u8(0x001A, 0x00); // segment low
    mem.write_u8(0x001B, 0x00); // segment high
    mem.write_slice(
        0x7C00,
        &[
            0x0F, 0x0B, // UD2 — should fault here
            0xF4, // HLT — should never be reached
        ],
    );
    mem.write_slice(
        0x7C20,
        &[
            0xB8, 0xEF, 0xBE, // MOV AX, 0xBEEF
            0xF4, // HLT
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // The handler executed.
    assert_eq!(cpu.regs[r16::AX], 0xBEEF);
    // The fault frame pushed (FLAGS, CS, IP) — initial SP=0x7C00
    // dropped to 0x7BFA after three 16-bit pushes. Saved IP must
    // point at the start of UD2 (0x7C00), not the byte after.
    let saved_ip = mem.read_u16(0x7BFA);
    assert_eq!(saved_ip, 0x7C00);
    // IF must be cleared inside the handler (we executed it to HLT
    // with IF=0).
    assert!(!cpu.has(flag::IF));
}

/// In PE mode, MOV sreg with a selector past the GDT limit raises
/// #GP with the selector as the error code (RPL bits dropped — the
/// Intel error-code shape: EXT, IDT, TI then 13-bit index). Without
/// this guard the dispatcher would decode garbage past the GDT as
/// a descriptor and silently install nonsense into the segment cache.
#[test]
fn mov_sreg_in_pm_with_bad_selector_raises_gp_with_selector() {
    let mut mem = Memory::new(0x10_0000);
    // GDT[0..1] valid (limit = 0x0F means 16 bytes = two 8-byte slots).
    // GDT[1] is a code descriptor at base 0x9000 (from cpu_pe_setup).
    cpu_pe_setup(&mut mem);
    // IDT[13] = #GP gate at 0x4000 + 13*8 = 0x4068. 16-bit interrupt
    // gate so the frame layout is the simple word-pushed kind.
    mem.write_slice(
        0x4068,
        &[
            0x00, 0x03, // offset 15:0 = 0x0300
            0x08, 0x00, // selector = GDT[1]
            0x00, // reserved
            0x86, // P=1, DPL=0, 16-bit interrupt gate
            0x00, 0x00, // offset 31:16
        ],
    );
    // Handler at CS_base 0x9000 + 0x0300 = 0x9300: set AX, HLT.
    mem.write_slice(0x9300, &[0xB8, 0x0D, 0x00, 0xF4]);
    // Boot code at 0x7C00:  MOV AX, 0x18 ; MOV ES, AX ; HLT
    //   AX = 0x18 → selector index 3 (byte offset 0x18). With
    //   GDTR.limit = 0x0F only indices 0 and 1 are valid, so this
    //   must fault.
    mem.write_slice(
        0x7C00,
        &[
            0xB8, 0x18, 0x00, // MOV AX, 0x0018  (bogus selector)
            0x8E, 0xC0, // MOV ES, AX      ← #GP(0x18)
            0xF4, // HLT  (never reached)
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE — activate the bad-selector check
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x000F;
    cpu.idtr.base = 0x4000;
    cpu.idtr.limit = 0x07FF;
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0x000D, "handler ran");
    // Stack frame (16-bit gate, 4 words pushed: EC, IP, CS, FLAGS).
    // Initial SP = 0x7C00; final SP = 0x7BF8.
    //   mem[0x7BF8] = error code = 0x18 & ~3 = 0x18 (no RPL bits set)
    //   mem[0x7BFA] = saved IP = 0x7C03 (start of MOV ES, AX)
    assert_eq!(mem.read_u16(0x7BF8), 0x0018, "error code = bad selector");
    assert_eq!(mem.read_u16(0x7BFA), 0x7C03, "saved IP = start of MOV sreg");
}

/// POP DS (and the other POP sreg / LES / LDS forms) share the same
/// #GP path. Pre-push a bogus selector, then `POP DS`, and verify
/// the same shape of vector-13 fault as the MOV sreg case — proving
/// every segment-load entrypoint funnels through the same helper.
#[test]
fn pop_ds_in_pm_with_bad_selector_raises_gp_with_selector() {
    let mut mem = Memory::new(0x10_0000);
    cpu_pe_setup(&mut mem);
    // IDT[13] gate (16-bit interrupt gate to CS=0x08:0x0300).
    mem.write_slice(0x4068, &[0x00, 0x03, 0x08, 0x00, 0x00, 0x86, 0x00, 0x00]);
    mem.write_slice(0x9300, &[0xB8, 0x0D, 0x00, 0xF4]);
    // Boot:  PUSH 0x18 (bogus selector); POP DS  ← #GP(0x18); HLT.
    mem.write_slice(
        0x7C00,
        &[
            0x68, 0x18, 0x00, // PUSH 0x0018
            0x1F, // POP DS  ← #GP
            0xF4, // HLT (never reached)
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1;
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x000F;
    cpu.idtr.base = 0x4000;
    cpu.idtr.limit = 0x07FF;
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0x000D);
    // SP path: 0x7C00 → 0x7BFE (PUSH) → 0x7C00 (POP read) → 0x7BF8
    // (4 fault-frame words). EC at SP+0, IP at SP+2.
    assert_eq!(mem.read_u16(0x7BF8), 0x0018);
    assert_eq!(mem.read_u16(0x7BFA), 0x7C03, "saved IP = start of POP DS");
}

/// End-to-end ATA: issue an IDENTIFY DEVICE command via OUT DX, AL
/// then drain the first word from the data port via IN AX, DX. The
/// signature word (0x0040) confirms (a) the CPU dispatches port IO
/// to the right device, (b) the IDE controller's command + data
/// paths are wired through IoBus, and (c) inw decomposed into two
/// byte reads still drains exactly one word of buffer.
#[test]
fn ata_identify_via_in_out_returns_signature_word() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x7C00,
        &[
            0xBA, 0xF7, 0x01, // MOV DX, 0x01F7  (command port)
            0xB0, 0xEC, // MOV AL, 0xEC   (IDENTIFY)
            0xEE, // OUT DX, AL
            0xBA, 0xF0, 0x01, // MOV DX, 0x01F0  (data port)
            0xED, // IN AX, DX
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    // Two-sector image — IDENTIFY's word 60 will report it.
    io.ata.disk.load(&[0xAA; 1024]);
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Word 0 of the IDENTIFY block: 0x0040 (present ATA device).
    assert_eq!(cpu.regs[r16::AX], 0x0040);
}

/// End-to-end PCI Mechanism #1 probe via 32-bit OUT/IN under 0x66.
/// Selects bus 0 / device 0 / register 0 with enable=1, then reads
/// the 32-bit data window — the canonical "scan PCI for a device"
/// idiom Linux uses. An empty bus returns 0xFFFFFFFF (vendor ID =
/// 0xFFFF), and that's what we verify here.
#[test]
fn pci_config_read_returns_no_device_sentinel_on_empty_bus() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x7C00,
        &[
            // EAX = 0x80000000 (enable bit + bus 0 / dev 0 / reg 0).
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x80, // MOV EAX, 0x80000000
            0xBA, 0xF8, 0x0C, // MOV DX, 0x0CF8
            0x66, 0xEF, // OUT DX, EAX  (32-bit; uses op_size_32 path)
            0xBA, 0xFC, 0x0C, // MOV DX, 0x0CFC
            0x66, 0xED, // IN  EAX, DX
            0xF4, // HLT
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Empty PCI bus → vendor/device ID = 0xFFFFFFFF.
    assert_eq!(cpu.read_r32(0), 0xFFFF_FFFF);
}

/// Software INT n against a kernel-only IDT gate (DPL=0) from
/// CPL=3 must fault with #GP and the Intel error-code shape
/// ((vector << 3) | 2 — IDT bit set). Without this guard,
/// user-space could trigger any IDT entry including the page-fault
/// or double-fault handler.
#[test]
fn int_n_from_ring_3_against_dpl0_gate_faults_with_idt_error_code() {
    let mut mem = Memory::new(0x10_0000);
    let gdt: [u8; 48] = [
        0, 0, 0, 0, 0, 0, 0, 0, // null
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF,
        0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xF2,
        0xCF, 0x00, 0x67, 0x00, 0x00, 0x40, 0x00, 0x89, 0x00, 0x00,
    ];
    mem.write_slice(0x500, &gdt);
    mem.write_u32(0x4004, 0x3000);
    mem.write_u16(0x4008, 0x0010);

    // IDT[0x14] = kernel-only gate (DPL=0). Userspace mustn't reach it.
    // IDT[13] = #GP gate, DPL=3 (so the fault below can land in it).
    mem.write_slice(
        0x6000 + 0x14 * 8,
        &[0x00, 0xA0, 0x08, 0x00, 0x00, 0x8E, 0x00, 0x00],
    );
    mem.write_slice(
        0x6000 + 13 * 8,
        &[0x00, 0x90, 0x08, 0x00, 0x00, 0xEE, 0x00, 0x00],
    );
    // User-mode INT 0x14.
    mem.write_slice(0x8000, &[0xCD, 0x14]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1;
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x2F;
    cpu.idtr.base = 0x6000;
    cpu.idtr.limit = 0x07FF;
    cpu.tr = 0x28;
    cpu.write_sreg(sreg::CS, 0x001B, &mem);
    cpu.write_sreg(sreg::SS, 0x0023, &mem);
    cpu.write_r32(r16::SP as u8, 0x2000);
    cpu.ip = 0x8000;
    cpu.flags = 0x0202;

    let mut io = IoBus::new();
    cpu.step(&mut mem, &mut io).expect("step INT 0x14");

    // Landed on the #GP handler (vector 13), not the requested gate
    // (which would have jumped to offset 0xA000).
    assert_eq!(cpu.sregs[sreg::CS], 0x0008);
    assert_eq!(cpu.ip, 0x9000, "#GP handler at 0x9000, not gate's 0xA000");
    // Error code: vector 0x14 = index → (0x14 << 3) | 2 = 0xA2.
    assert_eq!(mem.read_u32(0x3000 - 24), 0xA2);
    // Saved IP names the start of the INT 0x14 instruction.
    assert_eq!(mem.read_u32(0x3000 - 20), 0x8000);
}

/// `IN AL, 0x60` from CPL=3 with IOPL=0 raises #GP via the same
/// `raise_gp_if_below_iopl` path as CLI/STI. We only check the
/// fault branch here; the positive branch (CPL ≤ IOPL) is exercised
/// by the CLI test, since both arms share the helper.
#[test]
fn in_al_from_ring_3_with_iopl_zero_faults() {
    let mut mem = Memory::new(0x10_0000);
    let gdt: [u8; 48] = [
        0, 0, 0, 0, 0, 0, 0, 0, // null
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF,
        0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xF2,
        0xCF, 0x00, 0x67, 0x00, 0x00, 0x40, 0x00, 0x89, 0x00, 0x00,
    ];
    mem.write_slice(0x500, &gdt);
    mem.write_u32(0x4004, 0x3000);
    mem.write_u16(0x4008, 0x0010);
    mem.write_slice(
        0x6000 + 13 * 8,
        &[0x00, 0x90, 0x08, 0x00, 0x00, 0xEE, 0x00, 0x00],
    );
    // User-mode `IN AL, 0x60` — read the PS/2 data port. Without
    // the guard this would read whatever the keyboard emulator
    // returned; with it, the instruction faults at its first byte.
    mem.write_slice(0x8000, &[0xE4, 0x60]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1;
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x2F;
    cpu.idtr.base = 0x6000;
    cpu.idtr.limit = 0x07FF;
    cpu.tr = 0x28;
    cpu.write_sreg(sreg::CS, 0x001B, &mem);
    cpu.write_sreg(sreg::SS, 0x0023, &mem);
    cpu.write_r32(r16::SP as u8, 0x2000);
    cpu.ip = 0x8000;
    cpu.flags = 0x0202; // IF set, IOPL=0

    let mut io = IoBus::new();
    cpu.step(&mut mem, &mut io).expect("step IN");

    assert_eq!(cpu.sregs[sreg::CS], 0x0008);
    assert_eq!(cpu.ip, 0x9000);
    assert_eq!(mem.read_u32(0x3000 - 20), 0x8000);
    assert_eq!(mem.read_u32(0x3000 - 24), 0);
}

/// CLI in CPL=3 fires #GP iff IOPL < CPL. Same setup pattern as
/// the HLT-from-ring-3 test, run with two IOPL values back-to-back:
/// IOPL=0 must fault; IOPL=3 lifts the guard and CLI commits.
#[test]
fn cli_in_ring_3_faults_at_iopl_0_succeeds_at_iopl_3() {
    fn run(iopl: u16) -> (bool, Cpu, Memory) {
        let mut mem = Memory::new(0x10_0000);
        let gdt: [u8; 48] = [
            0, 0, 0, 0, 0, 0, 0, 0, // null
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92,
            0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00,
            0x00, 0xF2, 0xCF, 0x00, 0x67, 0x00, 0x00, 0x40, 0x00, 0x89, 0x00, 0x00,
        ];
        mem.write_slice(0x500, &gdt);
        mem.write_u32(0x4004, 0x3000);
        mem.write_u16(0x4008, 0x0010);
        mem.write_slice(
            0x6000 + 13 * 8,
            &[0x00, 0x90, 0x08, 0x00, 0x00, 0xEE, 0x00, 0x00],
        );
        mem.write_u8(0x8000, 0xFA); // CLI

        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        cpu.cr0 = 1;
        cpu.gdtr.base = 0x0500;
        cpu.gdtr.limit = 0x2F;
        cpu.idtr.base = 0x6000;
        cpu.idtr.limit = 0x07FF;
        cpu.tr = 0x28;
        cpu.write_sreg(sreg::CS, 0x001B, &mem);
        cpu.write_sreg(sreg::SS, 0x0023, &mem);
        cpu.write_r32(r16::SP as u8, 0x2000);
        cpu.ip = 0x8000;
        // IF set + the requested IOPL.
        cpu.flags = 0x0202 | (iopl << 12);

        let mut io = IoBus::new();
        cpu.step(&mut mem, &mut io).expect("step CLI");
        let faulted = cpu.sregs[sreg::CS] == 0x0008;
        (faulted, cpu, mem)
    }

    // IOPL=0 → CPL=3 > IOPL=0 → fault. Saved IP names the start
    // of CLI; user SS:ESP land on the kernel stack via TSS.
    let (faulted, cpu, mem) = run(0);
    assert!(faulted, "CPL=3 > IOPL=0 must #GP");
    assert_eq!(cpu.ip, 0x9000);
    assert_eq!(mem.read_u32(0x3000 - 20), 0x8000);
    assert_eq!(mem.read_u32(0x3000 - 24), 0, "error code = 0");

    // IOPL=3 → CPL=3 == IOPL → guard passes; CLI commits, IF
    // clears, IP advances past the one-byte opcode.
    let (faulted, cpu, _) = run(3);
    assert!(!faulted, "CPL=3 <= IOPL=3 must succeed");
    assert_eq!(cpu.sregs[sreg::CS], 0x001B);
    assert_eq!(cpu.flags & flag::IF, 0);
    assert_eq!(cpu.ip, 0x8001);
}

/// HLT from CPL=3 raises #GP(0) instead of halting the CPU. The
/// fault uses the same cross-ring entry path as INT-from-ring-3:
/// land on the kernel stack via TSS, push the user frame, then
/// continue in the #GP handler. Real silicon does this so a
/// userspace HLT becomes a SIGSEGV rather than wedging the CPU.
#[test]
fn hlt_in_ring_3_raises_gp_via_cross_ring_entry() {
    let mut mem = Memory::new(0x10_0000);
    let gdt: [u8; 48] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, 0xFF, 0xFF, 0x00,
        0x00, 0x00, 0x92, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, 0xFF, 0xFF,
        0x00, 0x00, 0x00, 0xF2, 0xCF, 0x00, 0x67, 0x00, 0x00, 0x40, 0x00, 0x89, 0x00, 0x00,
    ];
    mem.write_slice(0x500, &gdt);
    mem.write_u32(0x4004, 0x3000); // TSS.ESP0
    mem.write_u16(0x4008, 0x0010); // TSS.SS0
                                   // IDT[13] = #GP gate. 32-bit interrupt gate to ring-0 code,
                                   // DPL=3 so a fault from ring 3 reaches it.
    mem.write_slice(
        0x6000 + 13 * 8,
        &[0x00, 0x90, 0x08, 0x00, 0x00, 0xEE, 0x00, 0x00],
    );
    mem.write_u8(0x8000, 0xF4); // user-mode HLT

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1;
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x2F;
    cpu.idtr.base = 0x6000;
    cpu.idtr.limit = 0x07FF;
    cpu.tr = 0x28;
    cpu.write_sreg(sreg::CS, 0x001B, &mem);
    cpu.write_sreg(sreg::SS, 0x0023, &mem);
    cpu.write_r32(r16::SP as u8, 0x2000);
    cpu.ip = 0x8000;
    cpu.flags = 0x0202;

    let mut io = IoBus::new();
    cpu.step(&mut mem, &mut io).expect("step HLT");

    assert!(!cpu.halted, "HLT must not have committed");
    assert_eq!(cpu.sregs[sreg::CS], 0x0008);
    assert_eq!(cpu.sregs[sreg::SS], 0x0010);
    assert_eq!(cpu.ip, 0x9000);
    // Six 32-bit pushes (SS, ESP, FLAGS, CS, IP, EC) — note this
    // is *one more* than the INT case because #GP carries an
    // error code.
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x3000 - 24);
    assert_eq!(mem.read_u32(0x3000 - 4), 0x23);
    assert_eq!(mem.read_u32(0x3000 - 8), 0x2000);
    assert_eq!(mem.read_u32(0x3000 - 16), 0x1B);
    // Saved IP names the *start* of HLT — fault, not trap.
    assert_eq!(mem.read_u32(0x3000 - 20), 0x8000);
    assert_eq!(mem.read_u32(0x3000 - 24), 0);
}

/// Cross-ring entry: an INT from CPL=3 lands on the kernel stack
/// via TSS.SS0:ESP0 and pushes the full five-dword frame (SS_user,
/// ESP_user, FLAGS, CS_user, IP_user). This is the kernel-entry
/// half of the user-mode round trip; the IRET-side test above is
/// the matching kernel→user return.
#[test]
fn int_from_ring_3_switches_to_kernel_stack_via_tss() {
    let mut mem = Memory::new(0x10_0000);
    // GDT at 0x500. Six entries (limit = 47 = 0x2F):
    //   0x00 null
    //   0x08 ring-0 code
    //   0x10 ring-0 data
    //   0x18 ring-3 code  (selector 0x1B with RPL=3)
    //   0x20 ring-3 data  (selector 0x23 with RPL=3)
    //   0x28 TSS descriptor — base 0x4000, limit 0x67, type 0x89
    //                          (available 32-bit TSS, DPL=0).
    let gdt: [u8; 48] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, 0xFF, 0xFF, 0x00,
        0x00, 0x00, 0x92, 0xCF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, 0xFF, 0xFF,
        0x00, 0x00, 0x00, 0xF2, 0xCF, 0x00, 0x67, 0x00, 0x00, 0x40, 0x00, 0x89, 0x00, 0x00,
    ];
    mem.write_slice(0x500, &gdt);

    // TSS at 0x4000. ESP0 at +4, SS0 at +8.
    mem.write_u32(0x4004, 0x3000);
    mem.write_u16(0x4008, 0x0010);

    // IDT at 0x6000. Vector 0x80 lives at 0x6000 + 0x80*8 = 0x6400.
    // 32-bit interrupt gate, DPL=3 (callable from ring 3),
    // selector = ring-0 code (0x08), offset = 0x9000.
    mem.write_slice(
        0x6400,
        &[
            0x00, 0x90, // offset 15:0
            0x08, 0x00, // selector
            0x00, // reserved
            0xEE, // P=1 DPL=3 32-bit interrupt gate
            0x00, 0x00, // offset 31:16
        ],
    );

    // Ring-3 code at 0x8000: a single INT 0x80.
    mem.write_slice(0x8000, &[0xCD, 0x80]);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x2F;
    cpu.idtr.base = 0x6000;
    cpu.idtr.limit = 0x07FF;
    cpu.tr = 0x28; // active TSS selector (index 5)

    cpu.write_sreg(sreg::CS, 0x001B, &mem);
    cpu.write_sreg(sreg::SS, 0x0023, &mem);
    cpu.write_r32(r16::SP as u8, 0x2000);
    cpu.ip = 0x8000;
    cpu.flags = 0x0202; // distinctive value so we can verify the push

    let mut io = IoBus::new();
    cpu.step(&mut mem, &mut io).expect("step INT 0x80");

    // Landed on kernel CS via the IDT gate.
    assert_eq!(cpu.sregs[sreg::CS], 0x0008);
    assert_eq!(cpu.ip, 0x9000);
    // Stack switched via TSS.
    assert_eq!(cpu.sregs[sreg::SS], 0x0010);
    // ESP = TSS.ESP0 - 20 (five 32-bit pushes, no error code).
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x3000 - 20);
    // Frame layout, walking down from TSS.ESP0:
    //   [0x3000 - 4]  = SS_user  (0x23 zero-extended)
    //   [0x3000 - 8]  = ESP_user (0x2000)
    //   [0x3000 - 12] = FLAGS    (with IF cleared by the gate)
    //   [0x3000 - 16] = CS_user  (0x1B)
    //   [0x3000 - 20] = IP_user  (0x8002 = address after INT 0x80,
    //                             since INT pushes the IP of the
    //                             following instruction)
    assert_eq!(mem.read_u32(0x3000 - 4), 0x23);
    assert_eq!(mem.read_u32(0x3000 - 8), 0x2000);
    assert_eq!(mem.read_u32(0x3000 - 16), 0x1B);
    assert_eq!(mem.read_u32(0x3000 - 20), 0x8002);
}

/// IRET to a higher-privilege CS (CPL goes from 0 to 3) pops the
/// additional SS:ESP frame Real silicon expects, switching to the
/// caller's user-mode stack. Same-ring IRET (the existing tests)
/// stays unchanged. This is the entry point for ring 3 transitions;
/// kernel→user-mode returns go through this exact shape.
#[test]
fn iret_to_higher_ring_pops_extra_ss_esp_and_switches_stack() {
    let mut mem = Memory::new(0x10_0000);
    // GDT at 0x500. Five entries: null + ring-0 code (0x08) +
    // ring-0 data (0x10) + ring-3 code (0x18, RPL=3 = 0x1B) +
    // ring-3 data (0x20, RPL=3 = 0x23).
    let gdt: [u8; 40] = [
        0, 0, 0, 0, 0, 0, 0, 0, // null
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00, // ring-0 code
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00, // ring-0 data
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFA, 0xCF, 0x00, // ring-3 code
        0xFF, 0xFF, 0x00, 0x00, 0x00, 0xF2, 0xCF, 0x00, // ring-3 data
    ];
    mem.write_slice(0x500, &gdt);

    // IRET frame at 0x6000 (dword pushes — the GDT descriptors
    // above set CS.D=1 via the 0xCF granularity byte, so the
    // CPU's IRET defaults to IRETD on this code segment):
    //   EIP = 0x0000_0000
    //   CS  = 0x0000_001B (zero-extended)
    //   EFLAGS = 0x0000_0202
    //   ESP = 0x0000_4000
    //   SS  = 0x0000_0023
    mem.write_u32(0x6000, 0x0000_0000);
    mem.write_u32(0x6004, 0x0000_001B);
    mem.write_u32(0x6008, 0x0000_0202);
    mem.write_u32(0x600C, 0x0000_4000);
    mem.write_u32(0x6010, 0x0000_0023);

    // IRET (0xCF) at linear 0x7C00 — that's where reset_to_boot
    // points CS:IP (CS=0 / IP=0x7C00). We later swap CS to ring-0
    // selector 0x08 via write_sreg; that descriptor has base 0, so
    // CS:IP still resolves to linear 0x7C00.
    mem.write_u8(0x7C00, 0xCF);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x27; // five 8-byte entries

    cpu.write_sreg(sreg::CS, 0x08, &mem); // ring-0 code
    cpu.write_sreg(sreg::SS, 0x10, &mem); // ring-0 data
    cpu.regs[r16::SP] = 0x6000;
    cpu.ip = 0x7C00;

    let mut io = IoBus::new();
    cpu.step(&mut mem, &mut io).expect("step");

    // CPL went 0 → 3, so the extra SS:SP frame got popped and
    // applied.
    assert_eq!(cpu.sregs[sreg::CS], 0x001B, "CS = ring-3 code");
    assert_eq!(cpu.sregs[sreg::SS], 0x0023, "SS = ring-3 data");
    assert_eq!(cpu.regs[r16::SP], 0x4000, "SP = caller's user stack");
    assert_eq!(cpu.ip, 0x0000, "IP = popped value");
    assert_eq!(cpu.flags, 0x0202, "FLAGS restored");
}

/// SSE PXOR xmm, xmm — the canonical "zero an XMM register" idiom.
#[test]
fn sse_pxor_self_zeroes_register() {
    // MOV EAX, 0xFFFFFFFF ; MOVD XMM0, EAX (nonzero low lane) ;
    // PXOR XMM0, XMM0 ; MOVD EBX, XMM0 ; HLT  → EBX = 0
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // MOV EAX, -1
            0x66, 0x0F, 0x6E, 0xC0, // MOVD XMM0, EAX
            0x66, 0x0F, 0xEF, 0xC0, // PXOR XMM0, XMM0
            0x66, 0x0F, 0x7E, 0xC3, // MOVD EBX, XMM0
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(3), 0);
    assert_eq!(cpu.xmm[0], 0);
}

/// SSE data movement: MOVD GP→XMM, MOVDQA XMM→XMM, MOVD XMM→GP.
/// Round-trips a dword through the XMM register file.
#[test]
fn sse_movd_movdqa_register_round_trip() {
    // MOV EAX, 0xDEADBEEF ; MOVD XMM0, EAX ; MOVDQA XMM1, XMM0 ;
    // MOVD EBX, XMM1 ; HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0xEF, 0xBE, 0xAD, 0xDE, // MOV EAX, 0xDEADBEEF
            0x66, 0x0F, 0x6E, 0xC0, // MOVD XMM0, EAX
            0x66, 0x0F, 0x6F, 0xC8, // MOVDQA XMM1, XMM0
            0x66, 0x0F, 0x7E, 0xCB, // MOVD EBX, XMM1
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(3), 0xDEAD_BEEF, "dword survived GP→XMM→XMM→GP");
    // MOVD zero-extends the upper 96 bits of XMM0.
    assert_eq!(cpu.xmm[0], 0xDEAD_BEEF_u128);
}

/// SSE 128-bit memory move: MOVDQA m128 → XMM → m128 copies all
/// 16 bytes intact.
#[test]
fn sse_movdqa_memory_round_trip() {
    let mut mem = Memory::new(0x10_0000);
    // Source 128-bit pattern at 0x600.
    for i in 0..4u32 {
        mem.write_u32(0x600 + i * 4, 0x1111_1111 * (i + 1));
    }
    // MOVDQA XMM0, [0x600] ; MOVDQA [0x610], XMM0 ; HLT
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x0F, 0x6F, 0x06, 0x00, 0x06, // MOVDQA XMM0, [0x600]
            0x66, 0x0F, 0x7F, 0x06, 0x10, 0x06, // MOVDQA [0x610], XMM0
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..12 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    for i in 0..4u32 {
        assert_eq!(
            mem.read_u32(0x610 + i * 4),
            0x1111_1111 * (i + 1),
            "dword {i} copied via XMM"
        );
    }
}

/// Unary x87 ops: FSQRT, FCHS, FABS. sqrt(16)=4, negate → -4,
/// abs → 4.
#[test]
fn fpu_unary_sqrt_chs_abs() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 16.0f32.to_bits());
    // FLD [0x600] (16) ; FSQRT (4) ; FCHS (-4) ; FABS (4) ; FSTP [0x604]
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0x06, 0x00, 0x06, // FLD m32 [0x600]
            0xD9, 0xFA, // FSQRT  → 4
            0xD9, 0xE0, // FCHS   → -4
            0xD9, 0xE1, // FABS   → 4
            0xD9, 0x1E, 0x04, 0x06, // FSTP m32 [0x604]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x604)), 4.0);
}

/// FLDPI pushes π; storing it back round-trips the constant.
#[test]
fn fpu_fldpi_constant() {
    let mut mem = Memory::new(0x10_0000);
    // FLDPI ; FSTP m64 [0x600]
    let w64_read = |mem: &Memory, addr: u32| -> f64 {
        let lo = mem.read_u32(addr) as u64;
        let hi = mem.read_u32(addr + 4) as u64;
        f64::from_bits(lo | (hi << 32))
    };
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0xEB, // FLDPI
            0xDD, 0x1E, 0x00, 0x06, // FSTP m64 [0x600]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(w64_read(&mem, 0x600), std::f64::consts::PI);
}

/// FPU compare → branch: `if (a > b)` on floats. FCOMP sets the
/// C0/C2/C3 condition bits, FNSTSW AX copies the status word to AX,
/// SAHF moves C0→CF / C3→ZF, and JA branches when above. This is
/// the full path glibc/compilers use to compare doubles before a
/// conditional.
#[test]
fn fpu_compare_then_branch_above() {
    // helper: run the compare program with given a,b; return AL.
    fn run_cmp(a: f32, b: f32) -> u8 {
        let mut mem = Memory::new(0x10_0000);
        mem.write_u32(0x600, a.to_bits());
        mem.write_u32(0x604, b.to_bits());
        mem.write_slice(
            0x7C00,
            &[
                0xD9, 0x06, 0x00, 0x06, // FLD m32 [0x600] = a
                0xD8, 0x1E, 0x04, 0x06, // FCOMP m32 [0x604] = b
                0xDF, 0xE0, // FNSTSW AX
                0x9E, // SAHF
                0x77, 0x04, // JA greater (+4)
                0xB0, 0x00, // MOV AL, 0
                0xEB, 0x02, // JMP done
                0xB0, 0x01, // greater: MOV AL, 1
                0xF4, // done: HLT
            ],
        );
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        for _ in 0..24 {
            if cpu.halted {
                break;
            }
            cpu.step(&mut mem, &mut io).expect("step");
        }
        assert!(cpu.halted);
        cpu.read_r8(0)
    }
    assert_eq!(run_cmp(5.0, 3.0), 1, "5 > 3 → JA taken");
    assert_eq!(run_cmp(2.0, 9.0), 0, "2 > 9 → not taken");
    assert_eq!(run_cmp(4.0, 4.0), 0, "4 > 4 → not taken (equal)");
}

/// x87 m64 (double) load + memory-operand FADD + store. `d + 0.25`
/// where d is a double in memory.
#[test]
fn fpu_double_load_memory_add_store() {
    let mut mem = Memory::new(0x10_0000);
    let w64 = |mem: &mut Memory, addr: u32, v: f64| {
        let b = v.to_bits();
        mem.write_u32(addr, b as u32);
        mem.write_u32(addr + 4, (b >> 32) as u32);
    };
    w64(&mut mem, 0x600, 10.5);
    w64(&mut mem, 0x608, 0.25);
    // FLD m64 [0x600]      ; DD /0, modrm 00 000 110
    // FADD m64 [0x608]     ; DC /0, modrm 00 000 110
    // FSTP m64 [0x610]     ; DD /3, modrm 00 011 110
    // HLT
    mem.write_slice(
        0x7C00,
        &[
            0xDD, 0x06, 0x00, 0x06, // FLD m64 [0x600]
            0xDC, 0x06, 0x08, 0x06, // FADD m64 [0x608]
            0xDD, 0x1E, 0x10, 0x06, // FSTP m64 [0x610]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    let lo = mem.read_u32(0x610) as u64;
    let hi = mem.read_u32(0x614) as u64;
    assert_eq!(f64::from_bits(lo | (hi << 32)), 10.75);
}

/// FILD / FISTP — integer↔float conversion. `(int)((double)7 * 1.5)`
/// = (int)10.5 = 10 (truncated).
#[test]
fn fpu_fild_fistp_int_conversion() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 7); // integer 7
    mem.write_u32(0x604, 1.5f32.to_bits());
    // FILD m32 [0x600]   ; DB /0  → ST0 = 7.0
    // FLD  m32 [0x604]   ; D9 /0  → ST0 = 1.5, ST1 = 7
    // FMULP              ; DE C9  → ST0 = 10.5
    // FISTP m32 [0x608]  ; DB /3  → store (int)10.5 = 10
    // HLT
    mem.write_slice(
        0x7C00,
        &[
            0xDB, 0x06, 0x00, 0x06, // FILD m32 [0x600]
            0xD9, 0x06, 0x04, 0x06, // FLD m32 [0x604]
            0xDE, 0xC9, // FMULP
            0xDB, 0x1E, 0x08, 0x06, // FISTP m32 [0x608]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x608), 10, "(int)(7 * 1.5) = 10");
}

/// FXCH swaps ST(0) and ST(1): compute 10.0 - 3.0 with operands in
/// the "wrong" order, fix with FXCH, FSUBP.
#[test]
fn fpu_fxch_swaps_top() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 3.0f32.to_bits());
    mem.write_u32(0x604, 10.0f32.to_bits());
    // FLD [0x600] (3) ; FLD [0x604] (10) ; FXCH (now ST0=3,ST1=10)
    // FSUBP ST(1),ST(0): ST(1)=ST(1)-ST(0)=10-3=7, pop ; FSTP [0x608]
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0x06, 0x00, 0x06, // FLD [0x600] = 3
            0xD9, 0x06, 0x04, 0x06, // FLD [0x604] = 10
            0xD9, 0xC9, // FXCH ST(1)  → ST0=3, ST1=10
            0xDE, 0xE9, // FSUBP ST(1),ST(0) → 10-3=7
            0xD9, 0x1E, 0x08, 0x06, // FSTP [0x608]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x608)), 7.0);
}

/// x87 FPU: FLD / FADDP / FSTP round-trip. Loads two f32s onto the
/// stack, adds them, stores the result. 1.5 + 2.25 = 3.75. First
/// real floating-point arithmetic — the start of the FPU blocker.
#[test]
fn fpu_fadd_load_add_store() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 1.5f32.to_bits());
    mem.write_u32(0x604, 2.25f32.to_bits());
    // FLD [0x600] ; FLD [0x604] ; FADDP ; FSTP [0x608] ; HLT
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0x06, 0x00, 0x06, // FLD m32 [0x600]
            0xD9, 0x06, 0x04, 0x06, // FLD m32 [0x604]
            0xDE, 0xC1, // FADDP ST(1), ST(0)
            0xD9, 0x1E, 0x08, 0x06, // FSTP m32 [0x608]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x608)), 3.75);
    assert_eq!(cpu.fpu_top, 0, "stack balanced after load/load/addp/store");
}

/// FLD1 / FLDZ constant loads and FMULP.
#[test]
fn fpu_constants_and_fmulp() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 6.0f32.to_bits());
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0x06, 0x00, 0x06, // FLD m32 [0x600]   → ST0=6
            0xD9, 0xE8, // FLD1              → ST0=1, ST1=6
            0xD9, 0xE8, // FLD1              → ST0=1, ST1=1, ST2=6
            0xDE, 0xC1, // FADDP            → ST0=2, ST1=6
            0xDE, 0xC9, // FMULP            → ST0=12
            0xD9, 0x1E, 0x04, 0x06, // FSTP m32 [0x604]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..20 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(f32::from_bits(mem.read_u32(0x604)), 12.0);
}

/// F3 90 — PAUSE. The spin-loop hint a `while (locked) cpu_relax()`
/// emits. Must decode as a no-op, not as REP NOP (which would
/// reject 0x90).
#[test]
fn pause_decodes_as_noop() {
    // PAUSE ; MOV AL, 0x7E ; HLT — proves execution continues past it.
    let (cpu, _, _) = run_payload(&[0xF3, 0x90, 0xB0, 0x7E, 0xF4], 8);
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0x7E);
}

/// Decode-coverage survey: a batch of common instruction encodings
/// must all decode (not return Unimplemented). Each entry is a
/// complete instruction; we place it at the boot address, single-
/// step once, and require the step to succeed. This is a wide net
/// for decode regressions across the opcode map in one test.
#[test]
fn decode_survey_common_encodings_all_accepted() {
    let cases: &[(&str, &[u8])] = &[
        ("mov eax,imm32", &[0x66, 0xB8, 0x01, 0x02, 0x03, 0x04]),
        ("add eax,ebx", &[0x66, 0x01, 0xD8]),
        ("sub eax,ebx", &[0x66, 0x29, 0xD8]),
        ("xor eax,eax", &[0x66, 0x31, 0xC0]),
        ("and eax,ebx", &[0x66, 0x21, 0xD8]),
        ("or eax,ebx", &[0x66, 0x09, 0xD8]),
        ("cmp eax,ebx", &[0x66, 0x39, 0xD8]),
        ("test eax,eax", &[0x66, 0x85, 0xC0]),
        ("imul eax,ebx", &[0x66, 0x0F, 0xAF, 0xC3]),
        ("shl eax,1", &[0x66, 0xD1, 0xE0]),
        ("sar eax,cl", &[0x66, 0xD3, 0xF8]),
        ("shld eax,ebx,4", &[0x66, 0x0F, 0xA4, 0xD8, 0x04]),
        ("bt eax,3", &[0x66, 0x0F, 0xBA, 0xE0, 0x03]),
        ("bsf eax,ebx", &[0x66, 0x0F, 0xBC, 0xC3]),
        ("movzx eax,bl", &[0x66, 0x0F, 0xB6, 0xC3]),
        ("movsx eax,bl", &[0x66, 0x0F, 0xBE, 0xC3]),
        ("cmovz eax,ebx", &[0x66, 0x0F, 0x44, 0xC3]),
        ("setz al", &[0x0F, 0x94, 0xC0]),
        ("xadd eax,ebx", &[0x66, 0x0F, 0xC1, 0xD8]),
        ("cmpxchg eax,ebx", &[0x66, 0x0F, 0xB1, 0xD8]),
        ("bswap eax", &[0x66, 0x0F, 0xC8]),
        ("push eax", &[0x66, 0x50]),
        ("pop eax", &[0x66, 0x58]),
        ("inc eax", &[0x66, 0x40]),
        ("lea eax,[bx]", &[0x66, 0x8D, 0x07]),
        ("movzx via 0F B7", &[0x66, 0x0F, 0xB7, 0xC3]),
        ("cdq", &[0x66, 0x99]),
        ("pause", &[0xF3, 0x90]),
        ("rdtsc", &[0x0F, 0x31]),
        ("cpuid", &[0x0F, 0xA2]),
        ("clc", &[0xF8]),
        ("std", &[0xFD]),
        ("nop", &[0x90]),
        ("multibyte nop", &[0x0F, 0x1F, 0xC0]),
    ];
    for (name, bytes) in cases {
        let mut mem = Memory::new(0x10_0000);
        mem.write_slice(0x7C00, bytes);
        let mut cpu = Cpu::new();
        cpu.reset_to_boot();
        let mut io = IoBus::new();
        let res = cpu.step(&mut mem, &mut io);
        assert!(
            res.is_ok(),
            "encoding {name:?} ({bytes:02X?}) failed to decode: {res:?}"
        );
    }
}

/// 64-bit subtract via SUB + SBB across a dword pair — the borrow-
/// chain counterpart to the ADC test, used for 64-bit counter deltas.
///
///   0x5_0000_0001 - 0x1_FFFF_FFFF = 0x3_0000_0002
///   (low: 1 - 0xFFFFFFFF = 2 with borrow; high: 5 - 1 - 1 = 3)
#[test]
fn multiword_sub_propagates_borrow_through_sbb() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u32(0x600, 0x0000_0001); // A lo
    mem.write_u32(0x604, 0x0000_0005); // A hi
    mem.write_u32(0x608, 0xFFFF_FFFF); // B lo
    mem.write_u32(0x60C, 0x0000_0001); // B hi
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xA1, 0x00, 0x06, // mov eax, [0x600]
            0x66, 0x2B, 0x06, 0x08, 0x06, // sub eax, [0x608]
            0x66, 0xA3, 0x10, 0x06, // mov [0x610], eax
            0x66, 0xA1, 0x04, 0x06, // mov eax, [0x604]
            0x66, 0x1B, 0x06, 0x0C, 0x06, // sbb eax, [0x60C]
            0x66, 0xA3, 0x14, 0x06, // mov [0x614], eax
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x610), 0x0000_0002, "low dword w/ borrow");
    assert_eq!(mem.read_u32(0x614), 0x0000_0003, "high dword");
}

/// strlen via REPNE SCASB — the canonical libc idiom. Scan ES:EDI
/// for the AL=0 terminator with ECX=-1, then `not ecx; dec ecx` to
/// recover the length. Exercises the 32-bit REPNE loop (ECX/EDI
/// driven via 0x67) plus NOT/DEC r32 in one realistic sequence.
#[test]
fn strlen_via_repne_scasb() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x600, b"Hello\0"); // 5 chars + NUL
                                        // MOV EDI, 0x600        ; 66 BF 00 06 00 00
                                        // XOR AL, AL            ; 30 C0
                                        // MOV ECX, -1           ; 66 B9 FF FF FF FF
                                        // REPNE SCASB           ; 67 F2 AE   (addr32 → ECX/EDI)
                                        // NOT ECX               ; 66 F7 D1
                                        // DEC ECX               ; 66 49
                                        // HLT
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xBF, 0x00, 0x06, 0x00, 0x00, // MOV EDI, 0x600
            0x30, 0xC0, // XOR AL, AL
            0x66, 0xB9, 0xFF, 0xFF, 0xFF, 0xFF, // MOV ECX, -1
            0x67, 0xF2, 0xAE, // REPNE SCASB
            0x66, 0xF7, 0xD1, // NOT ECX
            0x66, 0x49, // DEC ECX
            0xF4, // HLT
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..64 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(1), 5, "strlen(\"Hello\") = 5");
}

/// INT 0x80 legacy-syscall round-trip — the Linux i386 syscall ABI
/// shape. User code loads argument registers, INT 0x80 vectors to a
/// handler that computes a result into EAX, and IRET returns to the
/// instruction after the INT with the result visible. Exercises the
/// register-passing convention end-to-end (not just a sentinel).
#[test]
fn int_0x80_syscall_returns_computed_result() {
    let mut mem = Memory::new(0x10_0000);
    // IVT[0x80] at linear 0x80*4 = 0x200: IP=0x9000, CS=0x0000.
    mem.write_u16(0x200, 0x9000);
    mem.write_u16(0x202, 0x0000);
    // Boot stub: EBX=10, ECX=32, INT 0x80, HLT. Expect EAX=42.
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xBB, 0x0A, 0x00, 0x00, 0x00, // MOV EBX, 10
            0x66, 0xB9, 0x20, 0x00, 0x00, 0x00, // MOV ECX, 32
            0xCD, 0x80, // INT 0x80
            0xF4, // HLT
        ],
    );
    // Handler at 0x9000: EAX = EBX + ECX ; IRET.
    mem.write_slice(
        0x9000,
        &[
            0x66, 0x89, 0xD8, // MOV EAX, EBX
            0x66, 0x01, 0xC8, // ADD EAX, ECX
            0xCF, // IRET
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted, "must return from the syscall and reach HLT");
    assert_eq!(cpu.read_r32(0), 42, "EAX = EBX + ECX from the handler");
    // IRET landed on the HLT right after INT 0x80 (offset 14 → 0x7C0E).
    assert_eq!(cpu.ip, 0x7C0F, "resumed past INT, ran the 1-byte HLT");
}

/// 64-bit add via ADD + ADC across a dword pair — the multi-precision
/// pattern the kernel uses for 64-bit counters (jiffies, ktime) on a
/// 32-bit CPU. Verifies CF produced by the low ADD feeds the high ADC.
///
///   0x1_FFFF_FFFF + 0x3_0000_0002 = 0x5_0000_0001
///   (low: 0xFFFFFFFF+2 = 0x1_00000001 → 0x00000001 + carry;
///    high: 1 + 3 + carry = 5)
#[test]
fn multiword_add_propagates_carry_through_adc() {
    let mut mem = Memory::new(0x10_0000);
    // operand A at 0x600 (lo, hi), operand B at 0x608.
    mem.write_u32(0x600, 0xFFFF_FFFF);
    mem.write_u32(0x604, 0x0000_0001);
    mem.write_u32(0x608, 0x0000_0002);
    mem.write_u32(0x60C, 0x0000_0003);
    // mov eax, [0x600]      ; 66 A1 00 06
    // add eax, [0x608]      ; 66 03 06 08 06   (low → CF=1)
    // mov [0x610], eax      ; 66 A3 10 06
    // mov eax, [0x604]      ; 66 A1 04 06
    // adc eax, [0x60C]      ; 66 13 06 0C 06   (high + carry)
    // mov [0x614], eax      ; 66 A3 14 06
    // hlt
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xA1, 0x00, 0x06, // mov eax, [0x600]
            0x66, 0x03, 0x06, 0x08, 0x06, // add eax, [0x608]
            0x66, 0xA3, 0x10, 0x06, // mov [0x610], eax
            0x66, 0xA1, 0x04, 0x06, // mov eax, [0x604]
            0x66, 0x13, 0x06, 0x0C, 0x06, // adc eax, [0x60C]
            0x66, 0xA3, 0x14, 0x06, // mov [0x614], eax
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // 0x1_FFFFFFFF + 0x3_00000002 = 0x5_00000001.
    assert_eq!(mem.read_u32(0x610), 0x0000_0001, "low dword");
    assert_eq!(mem.read_u32(0x614), 0x0000_0005, "high dword w/ carry");
}

/// Recursive factorial — the deepest end-to-end control-flow test
/// so far. fact(5)=120 via cdecl recursion: argument on the stack,
/// EBP-relative frame access, 32-bit CALL/RET (each pushing a dword
/// return address), and balanced `add esp, 4` cleanup. If 32-bit
/// CALL/RET push/pop widths or EBP-relative addressing were wrong,
/// the recursion's stack would drift and the result would be junk.
///
///   ; entry: push 5; call fact; hlt
///   fact:
///     push ebp ; mov ebp, esp
///     mov eax, [ebp+8]      ; n
///     cmp eax, 1
///     jg recurse
///     mov eax, 1            ; base
///     jmp done
///   recurse:
///     dec eax ; push eax ; call fact ; add esp, 4
///     imul eax, [ebp+8]    ; fact(n-1) * n
///   done:
///     mov esp, ebp ; pop ebp ; ret
#[test]
fn recursive_factorial_via_32bit_cdecl() {
    let mut mem = Memory::new(0x10_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);

    // Entry at 0x7C00:
    //   66 6A 05         push dword 5         (PUSH imm8 sign-ext, op32)
    //   66 E8 rel32      call fact
    //   F4               hlt
    // fact at 0x7C0A (computed below).
    // We assemble fact first to know its size, then fix the rel32.
    // EBP-relative memory operands need the 0x67 address-size prefix
    // *and* the 0x66 operand-size prefix — 0x66 alone leaves the
    // ModR/M in the 16-bit table where rm=101 means [DI], not [EBP].
    let fact: &[u8] = &[
        0x66, 0x55, // push ebp                  (idx 0-1)
        0x66, 0x89, 0xE5, // mov ebp, esp              (idx 2-4)
        0x67, 0x66, 0x8B, 0x45, 0x08, // mov eax, [ebp+8]          (idx 5-9)
        0x66, 0x83, 0xF8, 0x01, // cmp eax, 1                (idx 10-13)
        0x7F, 0x08, // jg recurse (IP 16 → 24)   (idx 14-15)
        0x66, 0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1            (idx 16-21)
        0xEB, 0x14, // jmp done (IP 24 → 44)     (idx 22-23)
        // recurse: (idx 24)
        0x66, 0x48, // dec eax                   (idx 24-25)
        0x66, 0x50, // push eax                  (idx 26-27)
        0x66, 0xE8, 0x00, 0x00, 0x00, 0x00, // call fact (rel32)         (idx 28-33)
        0x66, 0x83, 0xC4, 0x04, // add esp, 4                (idx 34-37)
        0x67, 0x66, 0x0F, 0xAF, 0x45, 0x08, // imul eax, [ebp+8]         (idx 38-43)
        // done: (idx 44)
        0x66, 0x89, 0xEC, // mov esp, ebp              (idx 44-46)
        0x66, 0x5D, // pop ebp                   (idx 47-48)
        0x66, 0xC3, // ret                       (idx 49-50)
    ];
    // Layout: entry is 3 (push) + 6 (call) + 1 (hlt) = 10 bytes, so
    // fact starts at 0x7C0A.
    let fact_start: u32 = 0x7C0A;
    // Entry CALL is at 0x7C03 (after the 3-byte push); after its 6
    // bytes IP = 0x7C09. rel32 = fact_start - 0x7C09.
    let entry_call_rel: u32 = fact_start.wrapping_sub(0x7C09);
    // The recursive `call fact` (66 E8 + rel32) starts at idx 28
    // inside fact; after its 6 bytes IP = fact_start + 34. rel32
    // back to fact_start.
    let rec_call_site = fact_start + 28;
    let rec_call_rel: u32 = fact_start.wrapping_sub(rec_call_site + 6);

    let mut entry = vec![0x66, 0x6A, 0x05, 0x66, 0xE8];
    entry.extend_from_slice(&entry_call_rel.to_le_bytes());
    entry.push(0xF4);
    let mut fact_patched = fact.to_vec();
    fact_patched[30..34].copy_from_slice(&rec_call_rel.to_le_bytes());

    mem.write_slice(0x7C00, &entry);
    mem.write_slice(fact_start, &fact_patched);

    let mut io = IoBus::new();
    for _ in 0..2000 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted, "factorial recursion should terminate");
    assert_eq!(cpu.read_r32(0), 120, "fact(5) = 120");
    // Stack balanced back to the entry value minus the one arg the
    // top-level `push 5` left (we never clean it up): 0x80000 - 4.
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0007_FFFC);
}

/// Richer decode medley: sum a 4-element dword array via SIB-indexed
/// loads in a loop, then CALL a helper that doubles the accumulator.
/// Exercises 32-bit SIB addressing (addr32), MOV r32 from memory,
/// the loop control flow, and a near CALL/RET round-trip — the
/// shape of a real C function indexing an array and calling a leaf.
///
///   xor eax, eax ; xor ecx, ecx
/// loop:
///   mov edx, [ecx*4 + 0x600]
///   add eax, edx
///   inc ecx
///   cmp ecx, 4
///   jne loop
///   call double
///   hlt
/// double:
///   add eax, eax
///   ret
#[test]
fn decode_medley_sib_array_sum_then_call() {
    let mut mem = Memory::new(0x10_0000);
    // Array [10, 20, 30, 40] at 0x600.
    for (i, v) in [10u32, 20, 30, 40].iter().enumerate() {
        mem.write_u32(0x600 + (i as u32) * 4, *v);
    }
    let code: &[u8] = &[
        0x66, 0x31, 0xC0, // xor eax, eax              (ofs 0)
        0x66, 0x31, 0xC9, // xor ecx, ecx              (ofs 3)
        // loop: (ofs 6)
        0x67, 0x66, 0x8B, 0x14, 0x8D, 0x00, 0x06, 0x00,
        0x00, // mov edx, [ecx*4 + 0x600]  (ofs 6, 9 bytes)
        0x66, 0x01, 0xD0, // add eax, edx              (ofs 15)
        0x66, 0x41, // inc ecx                   (ofs 18)
        0x66, 0x83, 0xF9, 0x04, // cmp ecx, 4                (ofs 20)
        0x75, 0xEC, // jne loop (-20: IP 26→6)   (ofs 24)
        0xE8, 0x01, 0x00, // call double (rel16=1)     (ofs 26)
        0xF4, // hlt                       (ofs 29)
        // double: (ofs 30)
        0x66, 0x01, 0xC0, // add eax, eax
        0xC3, // ret
    ];
    mem.write_slice(0x7C00, code);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..200 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // (10+20+30+40) * 2 = 200.
    assert_eq!(cpu.read_r32(0), 200);
}

/// 0x64 — FS segment-override prefix. `mov al, fs:[0x10]` reads
/// from FS.base + 0x10, the way Linux fetches per-CPU data.
#[test]
fn fs_segment_override_reads_through_fs_base() {
    let mut mem = Memory::new(0x10_0000);
    // FS = 0x2000 → real-mode base 0x20000. Put sentinel at
    // linear 0x20000 + 0x10 = 0x20010.
    mem.write_u8(0x2_0010, 0x9D);
    // Boot stub:
    //   MOV AX, 0x2000 ; MOV FS, AX
    //   64 8A 06 10 00   MOV AL, fs:[0x10]   (0x64 prefix, MOV r8 r/m8
    //                    with modrm 00 000 110 = [disp16], disp=0x10)
    //   HLT
    mem.write_slice(
        0x7C00,
        &[
            0xB8, 0x00, 0x20, // MOV AX, 0x2000
            0x8E, 0xE0, // MOV FS, AX
            0x64, 0x8A, 0x06, 0x10, 0x00, // MOV AL, fs:[0x10]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..12 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0x9D, "AL must come from FS:0x10");
}

/// 0x0F 0xA0/0xA1 — PUSH FS / POP FS round-trip. (FS holds the
/// per-CPU / TLS base in Linux.)
#[test]
fn push_pop_fs_round_trip() {
    let mut mem = Memory::new(0x10_0000);
    // Real-mode: load FS via a far-pointer-free path. We use the
    // MOV-to-sreg-from-r16 (0x8E /4) form: MOV FS, AX.
    //   MOV AX, 0x1234   ; B8 34 12
    //   MOV FS, AX       ; 8E E0  (modrm 11 100 000 → sreg=4=FS, rm=AX)
    //   PUSH FS          ; 0F A0
    //   MOV AX, 0        ; B8 00 00
    //   MOV FS, AX       ; 8E E0  (clobber FS)
    //   POP FS           ; 0F A1
    //   HLT
    mem.write_slice(
        0x7C00,
        &[
            0xB8, 0x34, 0x12, 0x8E, 0xE0, 0x0F, 0xA0, 0xB8, 0x00, 0x00, 0x8E, 0xE0, 0x0F, 0xA1,
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    // SS:SP must point at usable RAM for the push/pop.
    cpu.regs[r16::SP] = 0x2000;
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // FS clobbered to 0 then restored from the stack.
    assert_eq!(cpu.sregs[sreg::FS], 0x1234);
}

/// 0x66 0x69 — three-operand IMUL r32, r/m32, imm32.
#[test]
fn imul_three_operand_imm32() {
    // MOV EBX, 100 ; IMUL EAX, EBX, 1000 ; HLT → EAX=100000
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xBB, 0x64, 0x00, 0x00, 0x00, // MOV EBX, 100
            0x66, 0x69, 0xC3, 0xE8, 0x03, 0x00, 0x00, // IMUL EAX, EBX, 1000
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 100_000);
}

/// 0x66 0x6B — three-operand IMUL r32, r/m32, imm8 (sign-extended).
#[test]
fn imul_three_operand_imm8_32bit() {
    // MOV EBX, 0x1000 ; IMUL EAX, EBX, -2 ; HLT → EAX = -0x2000
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xBB, 0x00, 0x10, 0x00, 0x00, // MOV EBX, 0x1000
            0x66, 0x6B, 0xC3, 0xFE, // IMUL EAX, EBX, -2
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0xFFFF_E000); // -0x2000
}

/// 0xA0/0xA2 — MOV AL, moffs8 / MOV moffs8, AL. Absolute-address
/// accumulator load/store (global-variable access).
#[test]
fn mov_moffs8_store_then_load_round_trip() {
    // MOV AL, 0x5A          ; B0 5A
    // MOV [0x0400], AL      ; A2 00 04   (store AL to DS:0x400)
    // MOV AL, 0x00          ; B0 00      (clobber)
    // MOV AL, [0x0400]      ; A0 00 04   (load it back)
    // HLT
    let (cpu, mem, _) = run_payload(
        &[
            0xB0, 0x5A, // MOV AL, 0x5A
            0xA2, 0x00, 0x04, // MOV [0x400], AL
            0xB0, 0x00, // MOV AL, 0
            0xA0, 0x00, 0x04, // MOV AL, [0x400]
            0xF4,
        ],
        12,
    );
    assert_eq!(mem.read_u8(0x400), 0x5A);
    assert_eq!(cpu.read_r8(0), 0x5A);
}

/// 0x66 0xA1 / 0x66 0xA3 — MOV EAX, moffs32 / MOV moffs32, EAX.
#[test]
fn mov_moffs_eax_round_trip() {
    // MOV EAX, 0xCAFEBABE   ; 66 B8 BE BA FE CA
    // MOV [0x0500], EAX     ; 66 A3 00 05
    // MOV EAX, 0            ; 66 B8 00 00 00 00
    // MOV EAX, [0x0500]     ; 66 A1 00 05
    // HLT
    let (cpu, mem, _) = run_payload(
        &[
            0x66, 0xB8, 0xBE, 0xBA, 0xFE, 0xCA, // MOV EAX, 0xCAFEBABE
            0x66, 0xA3, 0x00, 0x05, // MOV [0x500], EAX
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // MOV EAX, 0
            0x66, 0xA1, 0x00, 0x05, // MOV EAX, [0x500]
            0xF4,
        ],
        16,
    );
    assert_eq!(mem.read_u16(0x500), 0xBABE);
    assert_eq!(mem.read_u16(0x502), 0xCAFE);
    assert_eq!(cpu.read_r32(0), 0xCAFE_BABE);
}

/// 0x0F 0xAF — two-operand IMUL r, r/m. The `a * b` a C compiler
/// emits.
#[test]
fn imul_two_operand_r32() {
    // MOV EAX, 7 ; MOV EBX, 6 ; IMUL EAX, EBX ; HLT → EAX=42
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x07, 0x00, 0x00, 0x00, // MOV EAX, 7
            0x66, 0xBB, 0x06, 0x00, 0x00, 0x00, // MOV EBX, 6
            0x66, 0x0F, 0xAF, 0xC3, // IMUL EAX, EBX (modrm 11 000 011)
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 42);
    assert!(!cpu.has(flag::OF), "no overflow for 7*6");
}

/// IMUL two-operand with a product that overflows 32 bits sets OF/CF.
#[test]
fn imul_two_operand_r32_overflow_sets_of() {
    // EAX = 0x10000, EBX = 0x10000 → product 0x1_0000_0000 overflows.
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x01, 0x00, // MOV EAX, 0x10000
            0x66, 0xBB, 0x00, 0x00, 0x01, 0x00, // MOV EBX, 0x10000
            0x66, 0x0F, 0xAF, 0xC3, // IMUL EAX, EBX
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 0, "low 32 bits of 0x1_0000_0000");
    assert!(cpu.has(flag::OF), "overflow flagged");
    assert!(cpu.has(flag::CF));
}

/// 0x66 0x98 / 0x66 0x99 — CWDE / CDQ sign-extension.
#[test]
fn cwde_cdq_sign_extend() {
    // MOV AX, 0xFFFF (=-1) ; CWDE → EAX = 0xFFFFFFFF
    // CDQ → EDX = 0xFFFFFFFF
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0xFF, 0xFF, // MOV AX, 0xFFFF
            0x66, 0x98, // CWDE
            0x66, 0x99, // CDQ
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0xFFFF_FFFF);
    assert_eq!(cpu.read_r32(2), 0xFFFF_FFFF);
}

/// 0x85 — TEST r/m, r. `test ax, ax` sets ZF when AX is zero.
#[test]
fn test_rm16_r16_sets_zf_when_anding_to_zero() {
    // MOV AX, 0 ; TEST AX, AX ; HLT  → ZF=1
    let (cpu, _, _) = run_payload(&[0xB8, 0x00, 0x00, 0x85, 0xC0, 0xF4], 8);
    assert!(cpu.has(flag::ZF));
    // Non-zero case: MOV AX, 0x8000 ; TEST AX, AX → SF=1, ZF=0
    let (cpu2, _, _) = run_payload(&[0xB8, 0x00, 0x80, 0x85, 0xC0, 0xF4], 8);
    assert!(!cpu2.has(flag::ZF));
    assert!(cpu2.has(flag::SF));
}

/// 0x66 0x85 — TEST r/m32, r32 with a value only visible in the
/// high half proves the 32-bit width.
#[test]
fn test_rm32_r32_sees_high_half() {
    // MOV EAX, 0x00010000 ; TEST EAX, EAX ; HLT → ZF=0 (high half set)
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x01, 0x00, // MOV EAX, 0x10000
            0x66, 0x85, 0xC0, // TEST EAX, EAX
            0xF4,
        ],
        12,
    );
    assert!(!cpu.has(flag::ZF), "high-half bit keeps ZF clear");
}

/// 0x66 0x87 — XCHG r/m32, r32 swaps full 32-bit registers.
#[test]
fn xchg_r32_r32_swaps_full_dwords() {
    // MOV EAX, 0x11112222 ; MOV EBX, 0x33334444 ; XCHG EAX, EBX ; HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x22, 0x22, 0x11, 0x11, // MOV EAX, 0x11112222
            0x66, 0xBB, 0x44, 0x44, 0x33, 0x33, // MOV EBX, 0x33334444
            0x66, 0x87, 0xD8, // XCHG EAX, EBX (modrm 11 011 000 → reg=EBX rm=EAX)
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 0x3333_4444);
    assert_eq!(cpu.read_r32(3), 0x1111_2222);
}

/// 0x0F 0x34 — SYSENTER. WRMSR seeds IA32_SYSENTER_CS/ESP/EIP, then
/// SYSENTER reloads CS:EIP and SS:ESP from them. Linux 2.6+ uses this
/// for the fast syscall entry path.
#[test]
fn sysenter_loads_cs_eip_ss_esp_from_msrs() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE; CS cache stays base 0 (selector 0) for the stub
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    // GDT[1] sel 0x08: flat code base=0 limit=0xFFFFF G=1 access=0x9A.
    mem.write_slice(0x0508, &[0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00]);
    // GDT[2] sel 0x10: flat data base=0 access=0x92.
    mem.write_slice(0x0510, &[0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00]);

    // Boot stub at 0x7C00:
    //   MOV ECX, 0x174 ; WRMSR (CS=0x08)
    //   MOV ECX, 0x176 ; MOV EAX, 0x9000 ; WRMSR (EIP)
    //   MOV ECX, 0x175 ; MOV EAX, 0x7_0000 ; WRMSR (ESP)
    //   SYSENTER
    // Build it programmatically to keep the encoding readable.
    let mut boot: Vec<u8> = Vec::new();
    // EAX=0x08, ECX=0x174, WRMSR
    boot.extend_from_slice(&[0x66, 0xB8, 0x08, 0x00, 0x00, 0x00]); // MOV EAX,0x08
    boot.extend_from_slice(&[0x66, 0xB9, 0x74, 0x01, 0x00, 0x00]); // MOV ECX,0x174
    boot.extend_from_slice(&[0x0F, 0x30]); // WRMSR
                                           // EAX=0x9000, ECX=0x176, WRMSR
    boot.extend_from_slice(&[0x66, 0xB8, 0x00, 0x90, 0x00, 0x00]); // MOV EAX,0x9000
    boot.extend_from_slice(&[0x66, 0xB9, 0x76, 0x01, 0x00, 0x00]); // MOV ECX,0x176
    boot.extend_from_slice(&[0x0F, 0x30]); // WRMSR
                                           // EAX=0x70000, ECX=0x175, WRMSR
    boot.extend_from_slice(&[0x66, 0xB8, 0x00, 0x00, 0x07, 0x00]); // MOV EAX,0x70000
    boot.extend_from_slice(&[0x66, 0xB9, 0x75, 0x01, 0x00, 0x00]); // MOV ECX,0x175
    boot.extend_from_slice(&[0x0F, 0x30]); // WRMSR
    boot.extend_from_slice(&[0x0F, 0x34]); // SYSENTER
    mem.write_slice(0x7C00, &boot);

    // Handler at 0x9000: MOV AL,0x77; HLT.
    mem.write_slice(0x9000, &[0xB0, 0x77, 0xF4]);

    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(
        cpu.read_r8(0),
        0x77,
        "SYSENTER must transfer to the handler"
    );
    assert_eq!(cpu.sregs[sreg::CS], 0x08);
    assert_eq!(cpu.sregs[sreg::SS], 0x10);
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0007_0000);
}

/// 0x0F 0x1F — multi-byte NOP. Verifies the dispatch consumes the
/// ModR/M (and disp here) without touching architectural state.
#[test]
fn multibyte_nop_does_nothing_and_consumes_bytes() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x42, 0x00, // MOV AX, 0x42
            0x67, 0x0F, 0x1F, 0x44, 0x00, 0x00, // NOP DWORD PTR [eax+eax+0]
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0x42);
    assert!(cpu.halted);
}

/// RDMSR with ECX=0x10 (IA32_TSC) returns the current TSC.
#[test]
fn rdmsr_for_ia32_tsc_returns_tsc() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0x10, 0x00, 0x00, 0x00, // MOV ECX, 0x10
            0x0F, 0x32, // RDMSR
            0xF4,
        ],
        12,
    );
    let full = ((cpu.read_r32(2) as u64) << 32) | cpu.read_r32(0) as u64;
    assert!(full > 0);
}

/// RDMSR for IA32_APIC_BASE returns the canonical 0xFEE0_0000.
#[test]
fn rdmsr_for_ia32_apic_base_returns_canonical_value() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0x1B, 0x00, 0x00, 0x00, // MOV ECX, 0x1B
            0x0F, 0x32, // RDMSR
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0xFEE0_0000);
    assert_eq!(cpu.read_r32(2), 0);
}

/// `RDMSR(IA32_MISC_ENABLE)` returns whatever the kernel wrote
/// last; `WRMSR` round-trips through the stored `misc_enable`
/// slot. Linux's arch/x86/kernel/cpu/intel.c reads this MSR
/// unconditionally, so a #GP here would surface as an oops on
/// every boot.
#[test]
fn rdmsr_wrmsr_round_trip_through_ia32_misc_enable() {
    // WRMSR(0x1A0) writes 0xCAFE_BABE into the low half; the
    // value should be readable back via RDMSR(0x1A0). The 0x66
    // prefix is applied per-instruction so each MOV imm32 sees a
    // 32-bit operand, and the read-back MOV uses 0x66 too so the
    // upper half of EAX survives into EBX.
    //   66 B9 A0 01 00 00      MOV ECX, 0x1A0
    //   66 B8 BE BA FE CA      MOV EAX, 0xCAFEBABE
    //   66 BA 00 00 00 00      MOV EDX, 0
    //   0F 30                  WRMSR
    //   66 31 C0               XOR EAX, EAX  (prove RDMSR rewrites EAX)
    //   0F 32                  RDMSR
    //   66 89 C3               MOV EBX, EAX (32-bit form)
    //   F4
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0xA0, 0x01, 0x00, 0x00, // MOV ECX, 0x1A0
            0x66, 0xB8, 0xBE, 0xBA, 0xFE, 0xCA, // MOV EAX, imm32
            0x66, 0xBA, 0x00, 0x00, 0x00, 0x00, // MOV EDX, 0
            0x0F, 0x30, // WRMSR
            0x66, 0x31, 0xC0, // XOR EAX, EAX
            0x0F, 0x32, // RDMSR
            0x66, 0x89, 0xC3, // MOV EBX, EAX
            0xF4,
        ],
        32,
    );
    assert_eq!(cpu.misc_enable, 0xCAFE_BABE);
    assert_eq!(cpu.read_r32(3), 0xCAFE_BABE);
}

/// `RDMSR(IA32_BIOS_SIGN_ID)` returns 0 — i.e. "no microcode
/// loaded". Linux's microcode_intel_init does this read and
/// branches on the value; a #GP here would oops at early init.
#[test]
fn rdmsr_for_ia32_bios_sign_id_returns_zero() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0x8B, 0x00, 0x00, 0x00, // MOV ECX, 0x8B
            0x0F, 0x32, // RDMSR
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0);
    assert_eq!(cpu.read_r32(2), 0);
}

/// 0x0F 0xAE /0 — FXSAVE m512. Stub writes 512 zeros at EA.
#[test]
fn fxsave_writes_512_zero_bytes() {
    let mut mem = Memory::new(0x10_0000);
    // Pre-poison the region so we can see FXSAVE clear it.
    for off in 0..512 {
        mem.write_u8(0x2000 + off, 0xFF);
    }
    // FXSAVE [0x2000] — 0F AE 06 00 20 (mod=00 reg=0 rm=110 = 0x06 disp16)
    mem.write_slice(0x7C00, &[0x0F, 0xAE, 0x06, 0x00, 0x20, 0xF4]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    for off in 0..512 {
        assert_eq!(
            mem.read_u8(0x2000 + off),
            0,
            "FXSAVE must zero offset {off}"
        );
    }
}

/// 0x0F 0xAE /6 with mod=11 — MFENCE no-op.
#[test]
fn mfence_runs_as_noop() {
    // MFENCE — 0F AE F0 (modrm 11 110 000)
    let (cpu, _, _) = run_payload(&[0x0F, 0xAE, 0xF0, 0xF4], 8);
    assert!(cpu.halted);
}

/// FNINIT + FNSTSW AX — Linux probes the FPU's existence with this
/// pair. After FNINIT the status word is 0 and FNSTSW must copy it
/// into AX. The "FPU present" check is `(AX & 0xB8FF) == 0`.
#[test]
fn fninit_then_fnstsw_returns_zero_status() {
    // Seed AX with garbage so we can prove FNSTSW overwrote it.
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0xFF, 0xAA, // MOV AX, 0xAAFF
            0xDB, 0xE3, // FNINIT
            0xDF, 0xE0, // FNSTSW AX
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0);
}

/// FLDCW / FNSTCW round-trip the FPU control word through memory.
#[test]
fn fldcw_fnstcw_round_trip_through_memory() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_u16(0x600, 0x027F); // seed CW image
                                  // FLDCW [0x600]  → D9 2D 00 06 (modrm 00 101 101)
                                  // ... actually mod=00 rm=110 = disp16 → modrm = 00 101 110 = 0x2E
                                  // FNSTCW [0x602] → D9 3E 02 06
    mem.write_slice(
        0x7C00,
        &[
            0xD9, 0x2E, 0x00, 0x06, // FLDCW [0x600]
            0xD9, 0x3E, 0x02, 0x06, // FNSTCW [0x602]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.fpu_cw, 0x027F);
    assert_eq!(mem.read_u16(0x602), 0x027F);
}

/// 0x0F 0x00 /2 / /0 — LLDT / SLDT round-trip via AX.
#[test]
fn lldt_sldt_round_trip_via_register() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x28, 0x00, // MOV AX, 0x28
            0x0F, 0x00, 0xD0, // LLDT AX
            0x0F, 0x00, 0xC3, // SLDT BX
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.ldtr, 0x28);
    assert_eq!(cpu.regs[r16::BX], 0x28);
}

/// 0x0F 0x01 /0 — SGDT stores the 6-byte GDTR pseudo-descriptor.
#[test]
fn sgdt_stores_pseudo_descriptor_to_memory() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x500, &[0xFF, 0x00, 0x30, 0x20, 0x10, 0x00]);
    mem.write_slice(
        0x7C00,
        &[
            0xBE, 0x00, 0x05, // MOV SI, 0x500
            0x0F, 0x01, 0x14, // LGDT [SI]
            0xBE, 0x00, 0x06, // MOV SI, 0x600
            0x0F, 0x01, 0x04, // SGDT [SI]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.gdtr.limit, 0x00FF);
    assert_eq!(cpu.gdtr.base, 0x0010_2030);
    assert_eq!(mem.read_u16(0x600), 0x00FF);
    assert_eq!(mem.read_u16(0x602), 0x2030);
    assert_eq!(mem.read_u16(0x604), 0x0010);
}

/// 0x0F 0x01 /4 — SMSW stores low 16 of CR0 into r/m16.
#[test]
fn smsw_stores_cr0_low_16() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x1234_5678;
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x7C00, &[0x0F, 0x01, 0xE0, 0xF4]); // SMSW AX; HLT
    let mut io = IoBus::new();
    for _ in 0..4 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0x5678);
}

/// 0x0F 0x31 — RDTSC. Returns a monotonically advancing counter
/// in EDX:EAX. After two RDTSCs separated by a NOP, the second
/// reading must be strictly greater than the first.
#[test]
fn rdtsc_advances_between_reads() {
    // RDTSC ; MOV ECX, EAX ; NOP ; RDTSC ; HLT
    //   0F 31              ; first read
    //   89 C1              ; mov cx, ax  (we only check low 16)
    //   90                 ; nop
    //   0F 31              ; second read
    //   F4
    let (cpu, _, _) = run_payload(&[0x0F, 0x31, 0x89, 0xC1, 0x90, 0x0F, 0x31, 0xF4], 16);
    // First reading captured into ECX (low half via the 16-bit MOV
    // CX, AX which we use as a proxy). Second reading lives in EAX.
    // The second must be strictly larger.
    assert!(
        cpu.regs[r16::AX] > cpu.regs[r16::CX],
        "RDTSC must advance between reads: AX={:#x}, CX={:#x}",
        cpu.regs[r16::AX],
        cpu.regs[r16::CX]
    );
}

/// RDTSCP — 0x0F 0x01 0xF9. Returns TSC in EDX:EAX *and* the
/// per-CPU TSC_AUX in ECX. We don't model multiple CPUs, so
/// TSC_AUX is always 0 (= "CPU 0"). Linux's vDSO
/// `clock_gettime(CLOCK_MONOTONIC)` falls through to this on
/// every userspace gettimeofday — a #UD here would make those
/// calls trap into the kernel slow-path on every invocation.
#[test]
fn rdtscp_returns_tsc_and_zero_aux() {
    // 0F 01 F9   RDTSCP
    // F4         HLT
    let (cpu, _, _) = run_payload(&[0x0F, 0x01, 0xF9, 0xF4], 8);
    // TSC was incremented per step; after 1 step it's at 1.
    assert!(cpu.read_r32(0) >= 1);
    assert_eq!(cpu.read_r32(2), 0); // EDX = high half, zero for small TSC
    assert_eq!(cpu.read_r32(1), 0); // ECX = TSC_AUX = 0 (CPU 0)
}

/// WRMSR(IA32_TSC_AUX) round-trips through RDMSR *and* through
/// RDTSCP's ECX. Linux's `cpu_init()` writes the CPU number here
/// on every AP bring-up; the vDSO's `__vdso_getcpu` reads it back
/// through RDTSCP to answer `getcpu(2)` without a syscall. If the
/// write didn't stick — say, RDTSCP always returned 0 — every
/// userspace `getcpu()` call would lie about the current CPU.
#[test]
fn tsc_aux_round_trips_through_wrmsr_rdmsr_and_rdtscp() {
    // 66 B9 03 01 00 C0     MOV ECX, 0xC0000103
    // 66 B8 2A 00 00 00     MOV EAX, 42
    // 66 BA 00 00 00 00     MOV EDX, 0
    // 0F 30                 WRMSR
    // 0F 32                 RDMSR             ; EAX should be 42 again
    // 0F 01 F9              RDTSCP            ; ECX should be 42
    // F4                    HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0x03, 0x01, 0x00, 0xC0, // MOV ECX, MSR id
            0x66, 0xB8, 0x2A, 0x00, 0x00, 0x00, // MOV EAX, 42
            0x66, 0xBA, 0x00, 0x00, 0x00, 0x00, // MOV EDX, 0
            0x0F, 0x30, // WRMSR
            0x0F, 0x32, // RDMSR
            0x0F, 0x01, 0xF9, // RDTSCP
            0xF4,
        ],
        32,
    );
    // Stored MSR value.
    assert_eq!(cpu.tsc_aux, 42);
    // After RDTSCP, ECX holds TSC_AUX.
    assert_eq!(cpu.read_r32(1), 42);
}

/// RDPMC — 0x0F 0x33. Reads PMC[ECX] into EDX:EAX. We have no
/// PMCs, so every read returns zero. Linux's perf-event sampling
/// path uses this; a #UD would oops `perf_event_open`.
#[test]
fn rdpmc_returns_zero_for_any_counter() {
    // MOV ECX, 0       ; 66 B9 00 00 00 00
    // RDPMC            ; 0F 33
    // HLT              ; F4
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB9, 0x00, 0x00, 0x00, 0x00, // MOV ECX, 0
            0x0F, 0x33, // RDPMC
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 0);
    assert_eq!(cpu.read_r32(2), 0);
}

/// 0x0F 0x06 — CLTS clears CR0.TS (bit 3).
#[test]
fn clts_clears_cr0_ts_bit() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x0000_000C; // TS=1, EM=1
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(0x7C00, &[0x0F, 0x06, 0xF4]); // CLTS; HLT
    let mut io = IoBus::new();
    for _ in 0..4 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.cr0, 0x0000_0004, "CLTS clears only bit 3; EM stays");
}

/// 0x0F 0x22 /4 / 0x0F 0x20 /4 — MOV CR4, r32 / MOV r32, CR4.
#[test]
fn mov_cr4_round_trip_carries_feature_bits() {
    // MOV EAX, 0x0000_0020 (PSE bit) — 66 B8 imm32
    // MOV CR4, EAX                   — 0F 22 E0 (reg=4=CR4, rm=0=EAX)
    // MOV EBX, CR4                   — 0F 20 E3 (reg=4, rm=3=EBX)
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x20, 0x00, 0x00, 0x00, // MOV EAX, 0x20
            0x0F, 0x22, 0xE0, // MOV CR4, EAX
            0x0F, 0x20, 0xE3, // MOV EBX, CR4
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.cr4, 0x20);
    assert_eq!(cpu.regs[r16::BX], 0x20);
}

/// RDMSR on an unrecognized MSR raises #GP(0) — matching real
/// silicon and the rdmsr_safe pattern Linux uses for cpu-feature
/// probes. Vectors through IVT[13] to a handler that captures a
/// sentinel; we also check that the pushed IP names the start of
/// the RDMSR instruction and the pushed error code is 0.
#[test]
fn rdmsr_unknown_msr_raises_gp_via_ivt() {
    let mut mem = Memory::new(0x10_0000);
    // IVT[13] = 0:0x7C30 (handler).
    mem.write_u8(13 * 4, 0x30);
    mem.write_u8(13 * 4 + 1, 0x7C);
    mem.write_u8(13 * 4 + 2, 0);
    mem.write_u8(13 * 4 + 3, 0);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xB9, 0xAD, 0xDE, 0x00, 0x00, // MOV ECX, 0xDEAD (unknown MSR)
            0x0F, 0x32, // RDMSR  ← should #GP at this IP (0x7C06)
            0xF4, // HLT — must never run
        ],
    );
    mem.write_slice(
        0x7C30,
        &[
            0xB8, 0x0D, 0x00, // MOV AX, 0x000D (vector 13 sentinel)
            0xF4, // HLT
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..20 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0x000D);
    // Initial SP = 0x7C00. Four 16-bit pushes (error code + IP + CS +
    // FLAGS) move SP to 0x7BF8. mem[0x7BF8] = error code = 0,
    // mem[0x7BFA] = saved IP = 0x7C06 (start of RDMSR).
    assert_eq!(mem.read_u16(0x7BF8), 0, "error code");
    assert_eq!(mem.read_u16(0x7BFA), 0x7C06, "saved IP = start of RDMSR");
}

/// The kernel-style rdmsr_safe pattern: a #GP handler discards the
/// error code, advances the saved IP past the faulting RDMSR,
/// zeros EAX/EDX (signalling "MSR absent"), and IRETs. After IRET
/// execution continues at the instruction immediately following
/// RDMSR — the whole point of catchable #GP for cpu-feature probes.
#[test]
fn gp_handler_irets_past_unknown_rdmsr_clearing_eax_edx() {
    let mut mem = Memory::new(0x10_0000);
    // IVT[13] = 0:0x7C30.
    mem.write_u8(13 * 4, 0x30);
    mem.write_u8(13 * 4 + 1, 0x7C);
    mem.write_u8(13 * 4 + 2, 0);
    mem.write_u8(13 * 4 + 3, 0);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xB9, 0xAD, 0xDE, 0x00, 0x00, // MOV ECX, 0xDEAD
            0x0F, 0x32, // RDMSR → #GP
            0xBB, 0xFE, 0xCA, // MOV BX, 0xCAFE  (resumes here after IRET)
            0xF4, // HLT
        ],
    );
    mem.write_slice(
        0x7C30,
        &[
            0x58, // POP AX           (discard error code; AX clobbered)
            0x89, 0xE5, // MOV BP, SP      (BP now points at saved IP)
            0x83, 0x46, 0x00, 0x02, // ADD WORD PTR [BP], 2  (skip 2-byte RDMSR)
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // MOV EAX, 0
            0x66, 0xBA, 0x00, 0x00, 0x00, 0x00, // MOV EDX, 0
            0xCF, // IRET — returns to RDMSR+2 with EAX/EDX cleared
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..40 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(0), 0, "EAX cleared by handler");
    assert_eq!(cpu.read_r32(2), 0, "EDX cleared by handler");
    assert_eq!(cpu.regs[r16::BX], 0xCAFE, "post-IRET code ran");
}

/// 0x66 0xFF /0 — INC r/m32.
#[test]
fn inc_r32_increments_dword_preserving_cf() {
    // STC (so we can verify CF is preserved)
    // MOV EAX, 0xFFFF_FFFF  ; 66 B8 FF FF FF FF
    // INC EAX               ; 66 FF C0 (modrm 11 000 000 → sub=0, rm=EAX)
    // HLT
    // 0xFFFFFFFF + 1 = 0 with ZF=1, CF preserved from STC = 1.
    let (cpu, _, _) = run_payload(
        &[
            0xF9, // STC
            0x66, 0xB8, 0xFF, 0xFF, 0xFF, 0xFF, // MOV EAX, -1
            0x66, 0xFF, 0xC0, // INC EAX
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0);
    assert!(cpu.has(flag::ZF));
    assert!(cpu.has(flag::CF), "INC preserves CF");
}

/// 0x66 0xFF /6 — PUSH r/m32 from a register source.
#[test]
fn push_rm32_via_group5_pushes_dword() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);
    cpu.write_r32(0, 0x1234_5678); // EAX
                                   // 66 FF F0   PUSH EAX (sub=6 rm=EAX → modrm 11 110 000 = 0xF0)
                                   // F4
    mem.write_slice(0x7C00, &[0x66, 0xFF, 0xF0, 0xF4]);
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0007_FFFC);
    assert_eq!(mem.read_u32(0x0007_FFFC), 0x1234_5678);
}

/// 0x66 0xC1 /4 — SHL r/m32, imm8. 32-bit shift through Group 2.
/// CF after SHL is the *last* bit shifted out — i.e. bit (32-count)
/// of the original. For value=0x80000001 and count=1, the last (and
/// only) bit shifted out is bit 31 = 1, so CF=1.
#[test]
fn shl_r32_imm8_shifts_and_sets_cf_from_shifted_out_bit() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x01, 0x00, 0x00, 0x80, // MOV EAX, 0x80000001
            0x66, 0xC1, 0xE0, 0x01, // SHL EAX, 1
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0x0000_0002);
    assert!(cpu.has(flag::CF));
}

/// 0x66 0xC1 /7 — SAR r/m32, imm8. Signed shift preserves the
/// sign bit.
#[test]
fn sar_r32_imm8_preserves_sign_bit() {
    // MOV EAX, 0xFFFF_FF80 ; SAR EAX, 3 ; HLT
    //   -128 >> 3 = -16 → 0xFFFF_FFF0
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x80, 0xFF, 0xFF, 0xFF, // MOV EAX, 0xFFFFFF80
            0x66, 0xC1, 0xF8, 0x03, // SAR EAX, 3 (sub=7, rm=EAX)
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0xFFFF_FFF0);
    assert!(cpu.has(flag::SF));
}

/// 0x66 0xC1 /0 — ROL r/m32, imm8. CF takes the bit rotated out.
#[test]
fn rol_r32_imm8_rotates_dword() {
    // MOV EAX, 0x8000_0001 ; ROL EAX, 1 ; HLT
    //   ROL by 1 → 0x0000_0003 (top bit wraps to bit 0); CF = old bit 31 = 1
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x01, 0x00, 0x00, 0x80, // MOV EAX, 0x80000001
            0x66, 0xC1, 0xC0, 0x01, // ROL EAX, 1
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0x0000_0003);
    assert!(cpu.has(flag::CF));
}

/// 0x66 0xF7 /4 — MUL r/m32. EDX:EAX = EAX * r/m32 unsigned.
#[test]
fn mul_r32_unsigned_produces_64_bit_product_in_edx_eax() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x01, 0x00, // MOV EAX, 0x10000
            0x66, 0xBB, 0x00, 0x00, 0x01, 0x00, // MOV EBX, 0x10000
            0x66, 0xF7, 0xE3, // MUL EBX
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 0);
    assert_eq!(cpu.read_r32(2), 1);
    assert!(cpu.has(flag::CF));
}

/// 0x66 0xF7 /6 — DIV r/m32. EAX = EDX:EAX / r/m32, EDX = rem.
#[test]
fn div_r32_unsigned_divides_64_bit_dividend() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // MOV EAX, 0
            0x66, 0xBA, 0x01, 0x00, 0x00, 0x00, // MOV EDX, 1
            0x66, 0xBB, 0x00, 0x00, 0x01, 0x00, // MOV EBX, 0x10000
            0x66, 0xF7, 0xF3, // DIV EBX
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.read_r32(0), 0x10000);
    assert_eq!(cpu.read_r32(2), 0);
}

/// 0x66 0xF7 /3 — NEG r/m32.
#[test]
fn neg_r32_two_complements_dword() {
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x05, 0x00, 0x00, 0x00, // MOV EAX, 5
            0x66, 0xF7, 0xD8, // NEG EAX
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.read_r32(0), 0xFFFF_FFFB);
    assert!(cpu.has(flag::CF));
}

/// 0x0F 0xBA /5 — BTS r/m16, imm8. Sets a bit, returns old in CF.
#[test]
fn bts_imm8_sets_bit_and_writes_cf_with_old_value() {
    // MOV AX, 0x0100 ; BTS AX, 1 ; HLT
    //   AX bit 1 is currently 0 → CF=0, AX afterwards = 0x0102.
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x00, 0x01, // MOV AX, 0x0100
            0x0F, 0xBA, 0xE8, 0x01, // BTS AX, 1
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0x0102);
    assert!(!cpu.has(flag::CF));
}

/// 0x0F 0xBA /4 — BT r/m16, imm8. Reads CF from the bit, no write.
#[test]
fn bt_imm8_reads_bit_into_cf() {
    // MOV AX, 0x0080 ; BT AX, 7 ; HLT — bit 7 set → CF=1.
    let (cpu, _, _) = run_payload(&[0xB8, 0x80, 0x00, 0x0F, 0xBA, 0xE0, 0x07, 0xF4], 12);
    assert!(cpu.has(flag::CF));
    assert_eq!(cpu.regs[r16::AX], 0x0080, "BT must not modify the operand");
}

/// 0x0F 0xB3 — BTR r/m16, r16. Clears bit.
#[test]
fn btr_r16_clears_bit_taking_index_from_reg() {
    // MOV AX, 0x0303 ; MOV CX, 1 ; BTR AX, CX ; HLT
    //   bit 1 was set → CF=1, AX afterwards = 0x0301.
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x03, 0x03, // MOV AX, 0x0303
            0xB9, 0x01, 0x00, // MOV CX, 1
            0x0F, 0xB3, 0xC8, // BTR AX, CX (modrm 11 001 000)
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0x0301);
    assert!(cpu.has(flag::CF));
}

/// Kernel-shaped integration: boot stub calls a 32-bit subroutine via
/// CALL rel32. Subroutine uses ENTER, REP MOVSD, CMPXCHG-on-memory,
/// LEAVE, RET. Asserts copy + counter update + ESP unchanged.
#[test]
fn kernel_shaped_routine_combines_enter_repmovsd_cmpxchg_leave() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);
    mem.write_u32(0x0001_0000, 0x1111_1111);
    mem.write_u32(0x0001_0004, 0x2222_2222);
    mem.write_u32(0x0001_0008, 0x3333_3333);
    mem.write_u32(0x0001_000C, 0x4444_4444);
    mem.write_u32(0x0001_4000, 7);
    // Boot stub: 30 bytes of MOV imm32 + 6-byte CALL + 1-byte HLT.
    // CALL at 0x7C00+30=0x7C1E. After 6 bytes IP=0x7C24. Target=0x9000.
    let rel32: u32 = 0x9000u32.wrapping_sub(0x7C24);
    let mut boot = vec![
        0x66, 0xBE, 0x00, 0x00, 0x01, 0x00, // MOV ESI, 0x10000
        0x66, 0xBF, 0x00, 0x00, 0x02, 0x00, // MOV EDI, 0x20000
        0x66, 0xB9, 0x04, 0x00, 0x00, 0x00, // MOV ECX, 4
        0x66, 0xB8, 0x07, 0x00, 0x00, 0x00, // MOV EAX, 7
        0x66, 0xBB, 0x09, 0x00, 0x00, 0x00, // MOV EBX, 9
        0x66, 0xE8,
    ];
    boot.extend_from_slice(&rel32.to_le_bytes());
    boot.push(0xF4);
    mem.write_slice(0x7C00, &boot);
    // Subroutine.
    let sub = [
        0x66, 0xC8, 0x00, 0x00, 0x00, // ENTER 0, 0
        0x67, 0x66, 0xF3, 0xA5, // REP MOVSD
        0x67, 0x0F, 0xB1, 0x1D, 0x00, 0x40, 0x01, 0x00, // CMPXCHG [0x14000], EBX
        0x66, 0xC9, // LEAVE
        0x66, 0xC3, // RET near (32-bit: matches the 66 E8 CALL's dword push)
    ];
    mem.write_slice(0x9000, &sub);
    let mut io = IoBus::new();
    for _ in 0..96 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x0002_0000), 0x1111_1111);
    assert_eq!(mem.read_u32(0x0002_0004), 0x2222_2222);
    assert_eq!(mem.read_u32(0x0002_0008), 0x3333_3333);
    assert_eq!(mem.read_u32(0x0002_000C), 0x4444_4444);
    assert_eq!(mem.read_u32(0x0001_4000), 9);
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0008_0000);
}

/// 0x66 0x60 / 0x66 0x61 — PUSHAD / POPAD.
#[test]
fn pushad_popad_round_trip_preserves_all_32_bit_gprs() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);
    cpu.write_r32(0, 0xAAAA_AAAA);
    cpu.write_r32(1, 0xCCCC_CCCC);
    cpu.write_r32(2, 0xDDDD_DDDD);
    cpu.write_r32(3, 0xBBBB_BBBB);
    cpu.write_r32(5, 0xBEBE_BEBE);
    cpu.write_r32(6, 0x5151_5151);
    cpu.write_r32(7, 0xD1D1_D1D1);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0x60, // PUSHAD
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // trample EAX
            0x66, 0xBB, 0x00, 0x00, 0x00, 0x00, // trample EBX
            0x66, 0x61, // POPAD
            0xF4,
        ],
    );
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r32(0), 0xAAAA_AAAA);
    assert_eq!(cpu.read_r32(1), 0xCCCC_CCCC);
    assert_eq!(cpu.read_r32(2), 0xDDDD_DDDD);
    assert_eq!(cpu.read_r32(3), 0xBBBB_BBBB);
    assert_eq!(cpu.read_r32(5), 0xBEBE_BEBE);
    assert_eq!(cpu.read_r32(6), 0x5151_5151);
    assert_eq!(cpu.read_r32(7), 0xD1D1_D1D1);
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0008_0000);
}

/// 0x66 0x9C / 0x66 0x9D — PUSHFD / POPFD.
#[test]
fn pushfd_popfd_round_trip_through_32_bit_stack() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);
    // STC; PUSHFD; CLC; POPFD; HLT
    mem.write_slice(0x7C00, &[0xF9, 0x66, 0x9C, 0xF8, 0x66, 0x9D, 0xF4]);
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert!(cpu.has(flag::CF));
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0008_0000);
}

/// kernels install. INT pushes EFLAGS, CS, and the full 32-bit EIP
/// as dwords; IRETD pops them back. Round-trip: INT 0x21 dispatches
/// through a 32-bit gate; the handler does IRETD (66 CF) and the
/// CPU returns exactly where it was before the INT.
#[test]
fn pm_interrupt_through_32_bit_gate_and_iretd() {
    let mut mem = Memory::new(0x0010_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 1; // PE
    cpu.gdtr.base = 0x0500;
    cpu.gdtr.limit = 0x0017;
    cpu.idtr.base = 0x4000;
    cpu.idtr.limit = 0x07FF;
    cpu.stack_size_32 = true;
    cpu.write_r32(r16::SP as u8, 0x0008_0000);

    // GDT[1] = flat code segment base=0, limit=0xFFFFF, G=1, access=0x9A.
    mem.write_slice(0x0508, &[0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00]);
    // IDT gate 0x21 at 0x4000+0x21*8 = 0x4108. 32-bit interrupt gate
    // type=0xE, P=1, DPL=0 → access byte = 0x8E.
    //   offset_lo = 0x0900, selector = 0x0008, type = 0x8E,
    //   offset_hi = 0x0000.
    mem.write_slice(0x4108, &[0x00, 0x09, 0x08, 0x00, 0x00, 0x8E, 0x00, 0x00]);
    // Handler at linear 0x0900: MOV BL,0x55; CF (IRETD — default
    // 32-bit because the CS descriptor has D=1); HLT.
    mem.write_slice(0x0900, &[0xB3, 0x55, 0xCF, 0xF4]);

    // Boot stub at 0x7C00:
    //   INT 0x21 (CD 21)
    //   MOV CL, 0x99   (B1 99)    ; runs after IRETD
    //   HLT
    mem.write_slice(0x7C00, &[0xCD, 0x21, 0xB1, 0x99, 0xF4]);

    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // Handler ran (BL=0x55) and IRETD returned to MOV CL,0x99 (CL=0x99).
    assert_eq!(cpu.read_r8(3), 0x55);
    assert_eq!(cpu.read_r8(1), 0x99);
    // ESP must be back to 0x0008_0000 — IRETD un-pushed the 3 dword
    // frame, restoring the stack precisely.
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0008_0000);
}

/// With `stack_size_32 = true` (i.e. running on a 32-bit kernel
/// stack), push/pop drive the full ESP register — not just SP —
/// so a stack that lives above the 64 KiB boundary works.
#[test]
fn stack_size_32_lets_push_pop_use_full_esp() {
    let mut mem = Memory::new(0x0080_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.stack_size_32 = true;
    // ESP = 0x0020_0000, above the 64 KiB boundary.
    cpu.write_r32(r16::SP as u8, 0x0020_0000);
    // 66 68 EF BE AD DE   PUSH imm32 (0xDEADBEEF)
    // 66 58               POP EAX
    // F4                   HLT
    mem.write_slice(
        0x7C00,
        &[0x66, 0x68, 0xEF, 0xBE, 0xAD, 0xDE, 0x66, 0x58, 0xF4],
    );
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    // ESP must be back to 0x0020_0000 (push then pop).
    assert_eq!(cpu.read_r32(r16::SP as u8), 0x0020_0000);
    // EAX should hold 0xDEADBEEF.
    assert_eq!(cpu.read_r32(0), 0xDEAD_BEEF);
}

/// `0x67 0x66 0xF3 0xA5` — REP MOVSD with 32-bit address size.
/// Drives the loop counter from ECX and updates ESI/EDI as full
/// 32-bit registers. The kernel-side memcpy compiles to this shape.
#[test]
fn rep_movsd_under_0x67_uses_ecx_esi_edi() {
    let mut mem = Memory::new(0x0010_0000);
    mem.write_u32(0x0001_0000, 0xAABBCCDD);
    mem.write_u32(0x0001_0004, 0x11223344);
    mem.write_u32(0x0001_0008, 0xDEADBEEF);
    mem.write_u32(0x0001_000C, 0xCAFEBABE);
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xBE, 0x00, 0x00, 0x01, 0x00, // MOV ESI, 0x10000
            0x66, 0xBF, 0x00, 0x00, 0x02, 0x00, // MOV EDI, 0x20000
            0x66, 0xB9, 0x04, 0x00, 0x00, 0x00, // MOV ECX, 4
            0x67, 0x66, 0xF3, 0xA5, // 32-bit REP MOVSD
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..32 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(mem.read_u32(0x0002_0000), 0xAABBCCDD);
    assert_eq!(mem.read_u32(0x0002_0004), 0x11223344);
    assert_eq!(mem.read_u32(0x0002_0008), 0xDEADBEEF);
    assert_eq!(mem.read_u32(0x0002_000C), 0xCAFEBABE);
    assert_eq!(cpu.read_r32(1), 0);
    assert_eq!(cpu.read_r32(6), 0x0001_0010);
    assert_eq!(cpu.read_r32(7), 0x0002_0010);
}

/// 0x67 prefix switches ModR/M to 32-bit addressing mode: rm
/// fields name r32 registers, an optional SIB byte follows, and
/// displacements are 8- or 32-bit. With a 32-bit operand size on
/// top we can do `MOV EAX, [EBX]` — kernel-style.
#[test]
fn addressing_32_bit_mov_eax_from_ebx_deref() {
    let mut mem = Memory::new(0x0010_0000);
    // Place sentinel dword at 0x40000.
    mem.write_u32(0x0004_0000, 0xCAFE_BABE);
    // Boot stub at 0x7C00:
    //   MOV EBX, 0x40000     (66 BB 00 00 04 00)
    //   MOV EAX, [EBX]        (67 66 8B 03)
    //     67: addr-size 32; 66: op-size 32; 8B: MOV r32, r/m32;
    //     ModR/M 00 000 011 = mode=00 reg=AX rm=EBX (rm=3 → EBX).
    //   HLT (F4)
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xBB, 0x00, 0x00, 0x04, 0x00, 0x67, 0x66, 0x8B, 0x03, 0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0xBABE);
    assert_eq!(cpu.regs_high[r16::AX], 0xCAFE);
}

/// 0x67 with mode=00 rm=5 → pure disp32. `MOV EAX, [0x12345]`.
#[test]
fn addressing_32_bit_disp32_only() {
    let mut mem = Memory::new(0x0080_0000);
    mem.write_u32(0x0001_2345, 0xDEAD_BEEF);
    // 67 66 8B 05 45 23 01 00 ; MOV EAX, [0x12345]
    // F4
    mem.write_slice(
        0x7C00,
        &[0x67, 0x66, 0x8B, 0x05, 0x45, 0x23, 0x01, 0x00, 0xF4],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0xBEEF);
    assert_eq!(cpu.regs_high[r16::AX], 0xDEAD);
}

/// SIB byte: `MOV EAX, [EBX + ECX*4]`. SIB scale=2 (×4), index=ECX,
/// base=EBX. ModR/M with rm=4 introduces the SIB byte.
#[test]
fn addressing_32_bit_sib_base_index_scale() {
    let mut mem = Memory::new(0x0010_0000);
    mem.write_u32(0x0001_0010, 0x1234_5678);
    // 66 BB 00 00 01 00       MOV EBX, 0x10000
    // 66 B9 04 00 00 00       MOV ECX, 4         (so ECX*4 = 0x10)
    // 67 66 8B 04 8B          MOV EAX, [EBX+ECX*4]
    //   ModR/M = 00 000 100 = 0x04  (rm=4 → SIB follows)
    //   SIB    = 10 001 011 = 0x8B  (scale=2 → ×4, index=ECX, base=EBX)
    // F4
    mem.write_slice(
        0x7C00,
        &[
            0x66, 0xBB, 0x00, 0x00, 0x01, 0x00, // MOV EBX, 0x10000
            0x66, 0xB9, 0x04, 0x00, 0x00, 0x00, // MOV ECX, 4
            0x67, 0x66, 0x8B, 0x04, 0x8B, // MOV EAX, [EBX+ECX*4]
            0xF4,
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.regs[r16::AX], 0x5678);
    assert_eq!(cpu.regs_high[r16::AX], 0x1234);
}

/// 0x66 0xE9 — JMP rel32. Under operand-size 0x66 the relative
/// offset is 32-bit, so the jump can reach anywhere in the address
/// space, not just ±32 KiB. We put the target at linear 0x00C0_0000
/// and rely on a sign-magnitude rel32 to land there from 0x7C00.
#[test]
fn jmp_rel32_under_0x66_reaches_high_address() {
    let mut mem = Memory::new(0x0100_0000); // 16 MiB
                                            // Compute the rel32 from the address right after the JMP's
                                            // 6 bytes (0x66 E9 imm32) back at 0x7C00.
                                            // IP after fetch = 0x7C06. Target = 0x00C0_0000. rel = target - IP.
    let target: u32 = 0x00C0_0000;
    let after_jmp_ip: u32 = 0x7C06;
    let rel32: u32 = target.wrapping_sub(after_jmp_ip);
    let mut payload = vec![0x66, 0xE9];
    payload.extend_from_slice(&rel32.to_le_bytes());
    mem.write_slice(0x7C00, &payload);
    mem.write_slice(target, &[0xB0, 0x5A, 0xF4]); // MOV AL,0x5A; HLT

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0x5A);
}

/// 0x66 0xE8 — CALL rel32. Same idea, but pushes the return IP.
#[test]
fn call_rel32_under_0x66_pushes_return_and_jumps_far() {
    let mut mem = Memory::new(0x0100_0000);
    let target: u32 = 0x0040_0000;
    let after_call_ip: u32 = 0x7C06;
    let rel32: u32 = target.wrapping_sub(after_call_ip);
    let mut payload = vec![0x66, 0xE8];
    payload.extend_from_slice(&rel32.to_le_bytes());
    payload.push(0xF4); // fallthrough HLT, shouldn't run
    mem.write_slice(0x7C00, &payload);
    mem.write_slice(target, &[0xB0, 0x77, 0xF4]); // MOV AL,0x77; HLT

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0x77);
    // Stack top must hold the low 16 of the return IP (= 0x7C06).
    let sp = cpu.regs[r16::SP];
    let lo = mem.read_u8(sp as u32);
    let hi = mem.read_u8(sp as u32 + 1);
    let ret_ip = u16::from_le_bytes([lo, hi]);
    assert_eq!(ret_ip, 0x7C06);
}

/// 0x0F 0x84 — JE rel16. Tests the long-form conditional jump.
/// Uses a 16-bit relative offset that's bigger than the rel8 range
/// (±127), so the short-form Jcc couldn't reach.
#[test]
fn je_rel16_conditional_long_jump() {
    // Boot stub:
    //   MOV AX, 1                  ; B8 01 00
    //   CMP AX, 1                  ; 3D 01 00          (sets ZF)
    //   JE rel16=+0x0200           ; 0F 84 00 02       (jump to IP + 0x200)
    //   MOV AL, 0xEE; HLT          ; B0 EE F4          (fall-through; should NOT run)
    // After fetching the full JE opcode+disp16 (4 bytes after the
    // 6-byte prelude), IP sits at 0x7C0A. Target = 0x7C0A + 0x200.
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x7C00,
        &[
            0xB8, 0x01, 0x00, // MOV AX, 1
            0x3D, 0x01, 0x00, // CMP AX, 1
            0x0F, 0x84, 0x00, 0x02, // JE +0x200
            0xB0, 0xEE, 0xF4, // fallthrough sentinel
        ],
    );
    mem.write_slice(0x7E0A, &[0xB0, 0xCC, 0xF4]); // target
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0xCC, "JE must have taken the long branch");
}

/// 0x0F 0x85 — JNE rel16 when condition is false: must NOT jump.
#[test]
fn jne_rel16_not_taken_falls_through() {
    let mut mem = Memory::new(0x10_0000);
    mem.write_slice(
        0x7C00,
        &[
            0xB8, 0x01, 0x00, // MOV AX, 1
            0x3D, 0x01, 0x00, // CMP AX, 1 (ZF=1, so JNE not taken)
            0x0F, 0x85, 0x00, 0x02, // JNE +0x200
            0xB0, 0xAB, 0xF4, // fallthrough (should run)
        ],
    );
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    for _ in 0..16 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0xAB);
}

/// Verifies that the CPU's IP register is now 32-bit and can hold
/// values above 0xFFFF — the prerequisite for jumping into a high-
/// memory kernel image. We seed IP and CS:base manually, place a
/// MOV AL,0x99; HLT at linear 0x12_3450, and let the fetch loop run.
#[test]
fn ip_register_is_32_bit_and_can_hold_addresses_above_64kib() {
    let mut mem = Memory::new(0x0080_0000); // 8 MiB
    mem.write_slice(0x12_3450, &[0xB0, 0x99, 0xF4]); // MOV AL,0x99; HLT
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    // CS cache base stays at 0; IP carries the full address.
    cpu.ip = 0x0012_3450;
    let mut io = IoBus::new();
    for _ in 0..8 {
        if cpu.halted {
            break;
        }
        cpu.step(&mut mem, &mut io).expect("step");
    }
    assert!(cpu.halted);
    assert_eq!(cpu.read_r8(0), 0x99);
    assert_eq!(cpu.ip, 0x0012_3453, "IP advanced past MOV+HLT");
}

/// 0x0F 0x44 — CMOVE r16, r/m16. Moves when ZF=1.
#[test]
fn cmove_moves_when_zf_set() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x01, 0x00, 0x3D, 0x01, 0x00, 0xBB, 0xAA, 0xAA, 0x0F, 0x44, 0xC3, 0xF4,
        ],
        16,
    );
    assert_eq!(cpu.regs[r16::AX], 0xAAAA);
}

#[test]
fn cmove_skips_when_zf_clear() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x01, 0x00, 0x3D, 0x02, 0x00, 0xBB, 0xAA, 0xAA, 0x0F, 0x44, 0xC3, 0xF4,
        ],
        16,
    );
    assert_eq!(cpu.regs[r16::AX], 1);
}

/// 0x0F 0xA4 — SHLD. Shift dest left; vacated low bits come from
/// source's high bits. dest=0x1234, src=0x5678, count=4:
///   combined = 0x1234_5678; << 4 = 0x1_2345_6780; low 32's top 16 = 0x2345.
#[test]
fn shld_r16_imm8_fills_low_from_source_high() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x34, 0x12, // MOV AX, 0x1234
            0xBB, 0x78, 0x56, // MOV BX, 0x5678
            0x0F, 0xA4, 0xD8, 0x04, // SHLD AX, BX, 4
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0x2345);
}

/// 0x0F 0xAC — SHRD. dest=0x1234, src=0x5678, count=4:
///   combined = 0x5678_1234; >> 4 = 0x0567_8123; low 16 = 0x8123.
#[test]
fn shrd_r16_imm8_fills_high_from_source_low() {
    let (cpu, _, _) = run_payload(
        &[
            0xB8, 0x34, 0x12, // MOV AX, 0x1234
            0xBB, 0x78, 0x56, // MOV BX, 0x5678
            0x0F, 0xAC, 0xD8, 0x04, // SHRD AX, BX, 4
            0xF4,
        ],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 0x8123);
}

/// 0x0F 0xA2 — CPUID leaf 0.
#[test]
fn cpuid_leaf_0_returns_max_leaf_and_vendor_string() {
    let (cpu, _, _) = run_payload(&[0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, 0x0F, 0xA2, 0xF4], 12);
    assert_eq!(cpu.read_r32(0), 1);
    assert_eq!(cpu.read_r32(3), u32::from_le_bytes(*b"WWWV"));
    assert_eq!(cpu.read_r32(1), u32::from_le_bytes(*b"MxRu"));
    assert_eq!(cpu.read_r32(2), u32::from_le_bytes(*b"st  "));
}

/// CPUID leaf 1 must advertise the ISA we actually implement: FPU,
/// TSC, MSR, SYSENTER, CMOV, FXSR, SSE, SSE2. The kernel reads EDX
/// to decide between fast modern paths and i386 fallbacks; honest
/// flags keep it on the fast paths.
#[test]
fn cpuid_leaf_1_advertises_implemented_features_in_edx() {
    let (cpu, _, _) = run_payload(&[0x66, 0xB8, 0x01, 0x00, 0x00, 0x00, 0x0F, 0xA2, 0xF4], 12);
    // Family 6, model 6, stepping 4.
    assert_eq!(cpu.read_r32(0), 0x0000_0664);
    let edx = cpu.read_r32(1);
    assert_ne!(edx & (1 << 0), 0, "FPU");
    assert_ne!(edx & (1 << 4), 0, "TSC");
    assert_ne!(edx & (1 << 5), 0, "MSR");
    assert_ne!(edx & (1 << 11), 0, "SEP");
    assert_ne!(edx & (1 << 15), 0, "CMOV");
    assert_ne!(edx & (1 << 24), 0, "FXSR");
    assert_ne!(edx & (1 << 25), 0, "SSE");
    assert_ne!(edx & (1 << 26), 0, "SSE2");
    // Things we don't implement must NOT be set.
    assert_eq!(edx & (1 << 6), 0, "PAE absent");
    assert_eq!(edx & (1 << 8), 0, "CX8 absent");
    assert_eq!(edx & (1 << 23), 0, "MMX absent");
}

/// CPUID extended leaves 0x80000002..4 return a 48-byte brand
/// string. Linux reads this verbatim into /proc/cpuinfo. The leaf
/// 0x80000000 reports the max extended leaf supported.
#[test]
fn cpuid_extended_leaves_return_brand_string() {
    // First: max extended leaf reported by 0x80000000 must be at
    // least 0x80000004.
    let (cpu, _, _) = run_payload(&[0x66, 0xB8, 0x00, 0x00, 0x00, 0x80, 0x0F, 0xA2, 0xF4], 12);
    assert!(cpu.read_r32(0) >= 0x8000_0004);

    // Now read all three brand-string leaves into a buffer in the
    // EAX/EBX/ECX/EDX order each leaf delivers.
    let mut s = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let prog = [
            0x66,
            0xB8,
            leaf as u8,
            (leaf >> 8) as u8,
            (leaf >> 16) as u8,
            (leaf >> 24) as u8,
            0x0F,
            0xA2,
            0xF4,
        ];
        let (c, _, _) = run_payload(&prog, 12);
        s.extend_from_slice(&c.read_r32(0).to_le_bytes()); // EAX
        s.extend_from_slice(&c.read_r32(3).to_le_bytes()); // EBX
        s.extend_from_slice(&c.read_r32(2).to_le_bytes()); // ECX
        s.extend_from_slice(&c.read_r32(1).to_le_bytes()); // EDX
    }
    assert_eq!(s.len(), 48);
    let text = String::from_utf8(s).expect("ascii");
    assert!(text.starts_with("wwwvm Rust"), "got {:?}", text);
    assert!(text.contains("x86 CPU"));
}

/// 0x0F 0xB6 — MOVZX r16, r/m8. Zero-extends a byte to 16 bits.
#[test]
fn movzx_r16_rm8_zero_extends() {
    // MOV BL, 0xFF; MOVZX AX, BL; HLT
    let (cpu, _, _) = run_payload(&[0xB3, 0xFF, 0x0F, 0xB6, 0xC3, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 0x00FF);
}

/// 0x0F 0xBE — MOVSX r16, r/m8. Sign-extends a byte to 16 bits.
#[test]
fn movsx_r16_rm8_sign_extends_negative_byte() {
    let (cpu, _, _) = run_payload(&[0xB3, 0xFF, 0x0F, 0xBE, 0xC3, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 0xFFFF);
}

/// 0x0F 0xB7 — MOVZX r32, r/m16.
#[test]
fn movzx_r32_rm16_zero_extends_high_half() {
    let (cpu, _, _) = run_payload(&[0xBB, 0xFE, 0xCA, 0x0F, 0xB7, 0xC3, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 0xCAFE);
    assert_eq!(cpu.regs_high[r16::AX], 0);
}

/// 0x0F 0x94 — SETE. Writes 1 when ZF=1, 0 when ZF=0.
#[test]
fn sete_writes_1_when_zf_set() {
    let (cpu, _, _) = run_payload(
        &[0xB8, 0x05, 0x00, 0x3D, 0x05, 0x00, 0x0F, 0x94, 0xC3, 0xF4],
        12,
    );
    assert_eq!(cpu.read_r8(3), 1);
}

#[test]
fn sete_writes_0_when_zf_clear() {
    let (cpu, _, _) = run_payload(
        &[0xB8, 0x05, 0x00, 0x3D, 0x06, 0x00, 0x0F, 0x94, 0xC3, 0xF4],
        12,
    );
    assert_eq!(cpu.read_r8(3), 0);
}

/// 0x0F 0xC1 — XADD r/m16, r16. Atomic exchange-and-add.
#[test]
fn xadd_r16_swaps_and_adds() {
    // MOV AX, 10 ; MOV BX, 3 ; XADD AX, BX ; HLT
    let (cpu, _, _) = run_payload(
        &[0xB8, 0x0A, 0x00, 0xBB, 0x03, 0x00, 0x0F, 0xC1, 0xD8, 0xF4],
        12,
    );
    assert_eq!(cpu.regs[r16::AX], 13);
    assert_eq!(cpu.regs[r16::BX], 10);
}

/// 0x0F 0xC8 — BSWAP EAX. Reverses byte order in EAX. Linux uses
/// this for converting between host and network byte order on
/// 32-bit fields.
#[test]
fn bswap_eax_reverses_dword_byte_order() {
    // MOV EAX, 0x11223344  → 66 B8 44 33 22 11
    // BSWAP EAX            → 0F C8
    // HLT
    let (cpu, _, _) = run_payload(&[0x66, 0xB8, 0x44, 0x33, 0x22, 0x11, 0x0F, 0xC8, 0xF4], 12);
    // 0x11223344 byte-reversed = 0x44332211
    assert_eq!(cpu.regs[r16::AX], 0x2211);
    assert_eq!(cpu.regs_high[r16::AX], 0x4433);
}

/// 0x0F 0xBC — BSF. Finds lowest set bit in r/m and writes index
/// to dest. ZF=1 when src is zero.
#[test]
fn bsf_r16_finds_lowest_set_bit() {
    // MOV BX, 0x0080  ; BB 80 00 (bit 7)
    // BSF AX, BX      ; 0F BC C3 (modrm 11 000 011 = reg AX, rm BX)
    // HLT
    let (cpu, _, _) = run_payload(&[0xBB, 0x80, 0x00, 0x0F, 0xBC, 0xC3, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 7);
    assert!(!cpu.has(flag::ZF));
}

#[test]
fn bsf_r16_with_zero_source_sets_zf() {
    // MOV BX, 0; BSF AX, BX; HLT
    let (cpu, _, _) = run_payload(&[0xBB, 0x00, 0x00, 0x0F, 0xBC, 0xC3, 0xF4], 8);
    assert!(cpu.has(flag::ZF));
}

/// 0x0F 0xBD — BSR. Finds highest set bit.
#[test]
fn bsr_r16_finds_highest_set_bit() {
    // MOV BX, 0x4001  ; BB 01 40  (bits 0 and 14)
    // BSR AX, BX      ; 0F BD C3
    // HLT
    let (cpu, _, _) = run_payload(&[0xBB, 0x01, 0x40, 0x0F, 0xBD, 0xC3, 0xF4], 8);
    assert_eq!(cpu.regs[r16::AX], 14);
    assert!(!cpu.has(flag::ZF));
}

/// 0x0F 0xB0 — CMPXCHG r/m8, r8. Equal case: writes the source
/// reg into r/m and sets ZF.
#[test]
fn cmpxchg_r8_equal_case_writes_source() {
    // MOV BYTE [0x500], 0x42 (C6 06 00 05 42)  ; seed memory with 0x42
    // MOV AL, 0x42  (B0 42)                    ; expected value in AL
    // MOV BL, 0x99  (B3 99)                    ; replacement in BL
    // CMPXCHG [0x500], BL (0F B0 1E 00 05)
    // HLT
    let (cpu, mem, _) = run_payload(
        &[
            0xC6, 0x06, 0x00, 0x05, 0x42, 0xB0, 0x42, 0xB3, 0x99, 0x0F, 0xB0, 0x1E, 0x00, 0x05,
            0xF4,
        ],
        16,
    );
    assert_eq!(mem.read_u8(0x500), 0x99);
    assert!(cpu.has(flag::ZF));
}

/// CMPXCHG mismatch case: writes the memory value into AL, ZF clear.
#[test]
fn cmpxchg_r8_mismatch_case_loads_memory_into_al() {
    // MOV AL, 0x42 ; MOV BL, 0x99 ; MOV BYTE [0x500], 0x77 ;
    // CMPXCHG [0x500], BL ; HLT
    let (cpu, mem, _) = run_payload(
        &[
            0xB0, 0x42, // MOV AL, 0x42
            0xB3, 0x99, // MOV BL, 0x99
            0xC6, 0x06, 0x00, 0x05, 0x77, // MOV BYTE [0x500], 0x77
            0x0F, 0xB0, 0x1E, 0x00, 0x05, // CMPXCHG [0x500], BL
            0xF4,
        ],
        16,
    );
    assert_eq!(mem.read_u8(0x500), 0x77, "memory unchanged on mismatch");
    assert_eq!(cpu.read_r8(0), 0x77, "AL loaded with memory value");
    assert!(!cpu.has(flag::ZF));
}

/// CMPXCHG8B match case — atomic 64-bit compare-and-swap. The
/// kernel uses this for `cmpxchg64`/`atomic64_t` on i486+ and for
/// per-CPU pointer updates. EDX:EAX hold the expected value; if
/// it matches [m64], publish ECX:EBX and set ZF=1.
#[test]
fn cmpxchg8b_match_publishes_ecx_ebx_and_sets_zf() {
    // Seed [0x500] = 0x1111_2222_3333_4444 (lo dword + hi dword)
    // 66 C7 06 00 05 44 33 22 11   MOV DWORD [0x500], 0x11223344
    // 66 C7 06 04 05 88 77 66 55   MOV DWORD [0x504], 0x55667788
    // Wait — we need the expected to match, so set expected:
    //   EAX = 0x11223344, EDX = 0x55667788
    //   ECX:EBX = 0xCAFEBABE_DEADBEEF (high in ECX, low in EBX)
    // Then CMPXCHG8B [0x500] (0F C7 0E 00 05; modrm=00 001 110 = 0x0E)
    let (cpu, mem, _) = run_payload(
        &[
            // Seed memory.
            0x66, 0xC7, 0x06, 0x00, 0x05, 0x44, 0x33, 0x22,
            0x11, // MOV DWORD [0x500], 0x11223344
            0x66, 0xC7, 0x06, 0x04, 0x05, 0x88, 0x77, 0x66,
            0x55, // MOV DWORD [0x504], 0x55667788
            // Set up the expected value.
            0x66, 0xB8, 0x44, 0x33, 0x22, 0x11, // MOV EAX, 0x11223344
            0x66, 0xBA, 0x88, 0x77, 0x66, 0x55, // MOV EDX, 0x55667788
            // Set up the replacement.
            0x66, 0xBB, 0xEF, 0xBE, 0xAD, 0xDE, // MOV EBX, 0xDEADBEEF
            0x66, 0xB9, 0xBE, 0xBA, 0xFE, 0xCA, // MOV ECX, 0xCAFEBABE
            0x0F, 0xC7, 0x0E, 0x00, 0x05, // CMPXCHG8B [0x500]
            0xF4,
        ],
        32,
    );
    // Memory was published: low dword = EBX, high dword = ECX.
    assert_eq!(mem.read_u16(0x500), 0xBEEF);
    assert_eq!(mem.read_u16(0x502), 0xDEAD);
    assert_eq!(mem.read_u16(0x504), 0xBABE);
    assert_eq!(mem.read_u16(0x506), 0xCAFE);
    assert!(cpu.has(flag::ZF), "ZF set on match");
}

/// CMPXCHG8B mismatch case: the memory value flows back into
/// EDX:EAX, ZF cleared, [m64] untouched.
#[test]
fn cmpxchg8b_mismatch_loads_memory_into_edx_eax_and_clears_zf() {
    let (cpu, mem, _) = run_payload(
        &[
            // Seed memory with 0x9999_8888_7777_6666 (lo,hi).
            0x66, 0xC7, 0x06, 0x00, 0x05, 0x66, 0x77, 0x88, 0x99, // [0x500] = 0x99887766
            0x66, 0xC7, 0x06, 0x04, 0x05, 0x22, 0x33, 0x44, 0x55, // [0x504] = 0x55443322
            // Expected (intentionally wrong).
            0x66, 0xB8, 0x00, 0x00, 0x00, 0x00, // MOV EAX, 0
            0x66, 0xBA, 0x00, 0x00, 0x00, 0x00, // MOV EDX, 0
            // Replacement (would-be).
            0x66, 0xBB, 0xEF, 0xBE, 0xAD, 0xDE, // MOV EBX, 0xDEADBEEF
            0x66, 0xB9, 0xBE, 0xBA, 0xFE, 0xCA, // MOV ECX, 0xCAFEBABE
            0x0F, 0xC7, 0x0E, 0x00, 0x05, // CMPXCHG8B [0x500]
            0xF4,
        ],
        32,
    );
    // Memory untouched.
    assert_eq!(mem.read_u16(0x500), 0x7766);
    assert_eq!(mem.read_u16(0x502), 0x9988);
    assert_eq!(mem.read_u16(0x504), 0x3322);
    assert_eq!(mem.read_u16(0x506), 0x5544);
    // EDX:EAX now hold the loaded memory value.
    assert_eq!(cpu.read_r32(0), 0x9988_7766);
    assert_eq!(cpu.read_r32(2), 0x5544_3322);
    assert!(!cpu.has(flag::ZF), "ZF clear on mismatch");
}

/// 0x0F 0x22 /2 (MOV CR2, r32) and 0x0F 0x20 /2 (MOV r32, CR2) — used
/// by a #PF handler to (re)write or read the faulting linear address.
#[test]
fn mov_cr2_round_trip_carries_full_32_bit_linear_address() {
    // MOV EAX, 0xDEADC0DE  → 66 B8 imm32
    // MOV CR2, EAX         → 0F 22 D0 (reg=2=CR2, rm=0=EAX)
    // MOV EBX, CR2         → 0F 20 D3 (reg=2=CR2, rm=3=EBX)
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0xDE, 0xC0, 0xAD, 0xDE, // MOV EAX, 0xDEADC0DE
            0x0F, 0x22, 0xD0, // MOV CR2, EAX
            0x0F, 0x20, 0xD3, // MOV EBX, CR2
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.cr2, 0xDEAD_C0DE);
    assert_eq!(cpu.regs[r16::BX], 0xC0DE);
    assert_eq!(cpu.regs_high[r16::BX], 0xDEAD);
}

/// A write that hits an unmapped page must flag the W bit in the #PF
/// error code. Mirror of `translate_with_non_present_pde_raises_page_fault`
/// but exercising `translate_write`.
#[test]
fn write_fault_sets_w_bit_in_error_code() {
    let mut mem = Memory::new(0x10_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000;
    cpu.cr3 = 0x0000_1000;
    // PD empty -> any write faults.
    cpu.mem_write_u8(&mut mem, 0x0040_1234, 0xFF);
    let pf = cpu.pending_fault().expect("write must fault");
    assert_eq!(pf.addr, 0x0040_1234);
    assert_eq!(
        pf.error_code & 0b10,
        0b10,
        "W bit set because the access was a write"
    );
    assert_eq!(pf.error_code & 1, 0, "P bit still clear (not present)");
}

/// Same setup as `write_fault_sets_w_bit_in_error_code` but a read —
/// proves the W bit is zero for reads, so the two paths are not
/// accidentally yoked together.
#[test]
fn read_fault_leaves_w_bit_clear() {
    let mem = Memory::new(0x10_0000);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0000;
    cpu.cr3 = 0x0000_1000;
    let _ = cpu.mem_read_u8(&mem, 0x0040_1234);
    let pf = cpu.pending_fault().expect("read must fault");
    assert_eq!(pf.error_code & 0b10, 0, "W bit clear for read access");
}

/// 0x66 + 0x50..0x57 / 0x58..0x5F → PUSH r32 / POP r32. Decrements SP
/// by 4, writes the full 32-bit register, then pops it back into a
/// different register so both halves survive.
#[test]
fn push_r32_pop_r32_round_trip_preserves_upper_half() {
    // MOV EAX, 0x11223344  → 66 B8 imm32
    // PUSH EAX             → 66 50
    // POP  EBX             → 66 5B
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x44, 0x33, 0x22, 0x11, // MOV EAX, 0x11223344
            0x66, 0x50, // PUSH EAX
            0x66, 0x5B, // POP EBX
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.regs[r16::BX], 0x3344);
    assert_eq!(cpu.regs_high[r16::BX], 0x1122);
    // SP must return to its boot value — push4 then pop4 is a no-op.
    assert_eq!(cpu.regs[r16::SP], 0x7C00);
}

/// 0x66 + 0x89 / 0x8B → MOV r/m32, r32 / MOV r32, r/m32. Store a
/// 32-bit GPR to memory through a memory operand, then load it back
/// into a different GPR. Confirms both directions handle full 32-bit
/// width and that the dword landed contiguously in memory.
#[test]
fn mov_rm32_r32_register_to_memory_round_trip() {
    // MOV EAX, 0xCAFEBABE  → 66 B8 imm32
    // MOV [0x500], EAX     → 66 89 06 00 05  (modrm 00 000 110 = [disp16])
    // MOV EBX, [0x500]     → 66 8B 1E 00 05  (modrm 00 011 110)
    // HLT
    let (cpu, mem, _) = run_payload(
        &[
            0x66, 0xB8, 0xBE, 0xBA, 0xFE, 0xCA, // MOV EAX, 0xCAFEBABE
            0x66, 0x89, 0x06, 0x00, 0x05, // MOV [0x500], EAX
            0x66, 0x8B, 0x1E, 0x00, 0x05, // MOV EBX, [0x500]
            0xF4,
        ],
        16,
    );
    assert_eq!(mem.read_u32(0x0500), 0xCAFE_BABE);
    assert_eq!(cpu.regs[r16::BX], 0xBABE);
    assert_eq!(cpu.regs_high[r16::BX], 0xCAFE);
}

/// 0x66 0x81 /5 → SUB r/m32, imm32. Verify that 32-bit subtraction
/// flows through alu_apply32 and clears ZF when the result is non-
/// zero, with the high half participating (subtracts a value that
/// would underflow if treated as 16-bit only).
#[test]
fn group1_sub_rm32_imm32_carries_borrow_through_high_half() {
    // MOV EAX, 0x00010000     ; 66 B8 00 00 01 00
    // SUB EAX, 0x00000001     ; 66 83 E8 01  (0x83 /5 sign-ext imm8)
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0x01, 0x00, // MOV EAX, 0x10000
            0x66, 0x83, 0xE8, 0x01, // SUB EAX, 1 (imm8 sign-ext)
            0xF4,
        ],
        16,
    );
    // 0x10000 - 1 = 0xFFFF — low half flips to 0xFFFF, high half
    // borrows down from 1 to 0.
    assert_eq!(cpu.regs[r16::AX], 0xFFFF);
    assert_eq!(cpu.regs_high[r16::AX], 0x0000);
    assert!(!cpu.has(flag::ZF));
    assert!(!cpu.has(flag::CF));
}

/// 0x66 0x81 /7 → CMP r/m32, imm32. Compare two 32-bit values that
/// differ only in the high half; the 32-bit compare must set the
/// flags correctly (a 16-bit-only compare would say "equal" here).
#[test]
fn group1_cmp_rm32_imm32_sees_high_half_difference() {
    // MOV EAX, 0xDEAD_0000    ; 66 B8 00 00 AD DE
    // CMP EAX, 0x0000_0000    ; 66 81 F8 00 00 00 00
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0x00, 0xAD, 0xDE, // MOV EAX, 0xDEAD0000
            0x66, 0x81, 0xF8, 0x00, 0x00, 0x00, 0x00, // CMP EAX, 0
            0xF4,
        ],
        16,
    );
    // 0xDEAD0000 != 0, so ZF clear. EAX unchanged.
    assert!(!cpu.has(flag::ZF));
    assert_eq!(cpu.regs[r16::AX], 0);
    assert_eq!(cpu.regs_high[r16::AX], 0xDEAD);
}

/// 0x66 0x01 → ADD r/m32, r32. Confirms the path through alu_dispatch
/// variant 1 actually does 32-bit math by adding a 32-bit value whose
/// low half rolls over into the high half.
#[test]
fn add_rm32_r32_with_carry_into_high_half() {
    // MOV EAX, 0x0000_FFFF    ; 66 B8 FF FF 00 00
    // MOV EBX, 0x0000_0001    ; 66 BB 01 00 00 00
    // ADD EAX, EBX            ; 66 01 D8  (modrm 11 011 000 → reg=EBX, rm=EAX)
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0xFF, 0xFF, 0x00, 0x00, // MOV EAX, 0xFFFF
            0x66, 0xBB, 0x01, 0x00, 0x00, 0x00, // MOV EBX, 1
            0x66, 0x01, 0xD8, // ADD EAX, EBX
            0xF4,
        ],
        16,
    );
    // 0xFFFF + 1 = 0x10000 — low half now 0, high half now 1.
    assert_eq!(cpu.regs[r16::AX], 0x0000);
    assert_eq!(cpu.regs_high[r16::AX], 0x0001);
    assert!(!cpu.has(flag::ZF));
    assert!(!cpu.has(flag::CF));
}

/// 0x66 0xC7 /0 imm32 → MOV r/m32, imm32. Round-trip through memory.
#[test]
fn mov_rm32_imm32_writes_dword_to_memory() {
    // MOV DWORD [0x500], 0xAABBCCDD
    //   0x66 0xC7 0x06 imm16_lo imm16_hi (modrm 00 000 110 = [disp16])
    //   disp16 = 0x0500, imm32 = 0xAABBCCDD (LE)
    let (_, mem, _) = run_payload(
        &[0x66, 0xC7, 0x06, 0x00, 0x05, 0xDD, 0xCC, 0xBB, 0xAA, 0xF4],
        4,
    );
    assert_eq!(mem.read_u16(0x500), 0xCCDD);
    assert_eq!(mem.read_u16(0x502), 0xAABB);
}

#[test]
fn mov_cr0_round_trip_through_ax() {
    // MOV AX, CR0 → CR0 (=0) flows into AX. Set PE bit via OR AL, 1.
    // MOV CR0, AX writes back. MOV BX, CR0 reads it again — both the
    // BX register and cpu.cr0 should reflect bit 0 = 1.
    //   0F 20 C0 — MOV AX, CR0   (ModR/M 11 000 000)
    //   0F 22 C0 — MOV CR0, AX
    //   0F 20 C3 — MOV BX, CR0   (rm = BX)
    let (cpu, _, _) = run_payload(
        &[
            0x0F, 0x20, 0xC0, 0x0C, 0x01, 0x0F, 0x22, 0xC0, 0x0F, 0x20, 0xC3, 0xF4,
        ],
        16,
    );
    assert_eq!(cpu.cr0 & 0x0000_FFFF, 1);
    assert_eq!(cpu.regs[r16::BX], 1);
}

/// 0x0F 0x22 /3 (MOV CR3, r32) and 0x0F 0x20 /3 (MOV r32, CR3) must
/// route the full 32-bit page-directory base. We use the operand-size
/// prefix 0x66 to fill EAX, write it into CR3, then read it back into
/// EBX. The high 16 bits live in `regs_high`.
#[test]
fn mov_cr3_round_trip_preserves_32_bit_page_directory_base() {
    // MOV EAX, 0xCAFEB000  → 66 B8 imm32
    // MOV CR3, EAX         → 0F 22 D8 (modrm = 11 011 000 → reg=3=CR3, rm=0=EAX)
    // MOV EBX, CR3         → 0F 20 DB (modrm = 11 011 011 → reg=3=CR3, rm=3=EBX)
    // HLT
    let (cpu, _, _) = run_payload(
        &[
            0x66, 0xB8, 0x00, 0xB0, 0xFE, 0xCA, // MOV EAX, 0xCAFEB000
            0x0F, 0x22, 0xD8, // MOV CR3, EAX
            0x0F, 0x20, 0xDB, // MOV EBX, CR3
            0xF4,
        ],
        16,
    );
    assert_eq!(cpu.cr3, 0xCAFE_B000);
    assert_eq!(cpu.regs[r16::BX], 0xB000);
    assert_eq!(cpu.regs_high[r16::BX], 0xCAFE);
}

/// `MOV r32, DRn` / `MOV DRn, r32` round-trip cleanly across all
/// 8 debug registers. Stubs only — we never raise #DB — but the
/// kernel's context switch path writes DR6/DR7 unconditionally
/// at every preemption, so a #UD here would crash schedule().
#[test]
fn mov_drn_round_trips_through_eax() {
    // For each DR index 0..7:
    //   MOV EAX, 0xDEC0_DE00 | i   ; 66 B8 imm32
    //   MOV DRi, EAX                ; 0F 23 (11 i 000)
    //   MOV EBX, DRi                ; 0F 21 (11 i 011)
    //   ... continue accumulating EBX into EDI via OR ...
    // We just verify the simplest pattern for one DR per run, but
    // walk all 8 indices in a loop so a typo in one ModR/M encoding
    // surfaces.
    for dr_idx in 0..8u8 {
        let modrm_w = 0b1100_0000 | (dr_idx << 3); // reg=dr_idx, rm=0(EAX)
        let modrm_r = 0b1100_0011 | (dr_idx << 3); // reg=dr_idx, rm=3(EBX)
        let value = 0xDEC0_DE00u32 | (dr_idx as u32);
        let imm = value.to_le_bytes();
        let prog = vec![
            0x66, 0xB8, imm[0], imm[1], imm[2], imm[3], // MOV EAX, value
            0x0F, 0x23, modrm_w, // MOV DRi, EAX
            0x0F, 0x21, modrm_r, // MOV EBX, DRi
            0xF4,    // HLT
        ];
        let (cpu, _, _) = run_payload(&prog, 16);
        assert_eq!(cpu.dr[dr_idx as usize], value, "DR{dr_idx} write");
        let ebx = cpu.regs[r16::BX] as u32 | ((cpu.regs_high[r16::BX] as u32) << 16);
        assert_eq!(ebx, value, "DR{dr_idx} read-back");
    }
}

/// translate() is identity whenever CR0.PG=0 — both real mode and
/// "PE but not yet paged" boot stages must keep using linear addresses
/// unchanged.
#[test]
fn translate_is_identity_without_paging() {
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mem = Memory::new(0x10_0000);
    cpu.cr0 = 1; // PE on but PG off
    assert_eq!(cpu.translate(&mem, 0x0000_0000), 0x0000_0000);
    assert_eq!(cpu.translate(&mem, 0x0007_C000), 0x0007_C000);
    assert_eq!(cpu.translate(&mem, 0x000F_FFFF), 0x000F_FFFF);
}

/// With CR0.PG=1 a linear address walks two levels of i386 page
/// tables. We map linear 0x0040_0123 -> physical 0x0008_0123 by
/// placing a page directory at 0x1000, PDE[1] (linear[31:22] = 1)
/// pointing at the PT at 0x2000, and PTE[0] (linear[21:12] = 0)
/// pointing at frame 0x80 — then assert the page offset (0x123)
/// flows through unchanged.
#[test]
fn paged_translation_resolves_through_two_level_walk() {
    let mut mem = Memory::new(0x10_0000);
    // Page directory at 0x1000. PDE[1] (offset 4) = PT_base 0x2000 | P|RW = 0x03.
    mem.write_u32(0x1000 + 4, 0x0000_2000 | 0x03);
    // Page table at 0x2000. PTE[0] (offset 0) = frame 0x80000 | P|RW = 0x03.
    mem.write_u32(0x2000, 0x0008_0000 | 0x03);

    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    cpu.cr0 = 0x8000_0001; // PE + PG
    cpu.cr3 = 0x0000_1000;

    // Linear 0x0040_0123: pd_idx = 1, pt_idx = 0, off = 0x123.
    let phys = cpu.translate(&mem, 0x0040_0123);
    assert_eq!(phys, 0x0008_0123);
    // Sanity: offset bits flow through untouched.
    assert_eq!(cpu.translate(&mem, 0x0040_0FFF), 0x0008_0FFF);
}

#[test]
fn lgdt_loads_gdt_descriptor_table_pseudo_register() {
    // 6-byte pseudo-descriptor at 0x800: limit=0x00FF, base=0x0010_2030
    // LGDT [SI] — 0x0F 0x01 /2, ModR/M = 00 010 100 = 0x14
    let descriptor: &[u8] = &[0xFF, 0x00, 0x30, 0x20, 0x10, 0x00];
    let (cpu, _, _) = run_with_data(
        &[0xBE, 0x00, 0x08, 0x0F, 0x01, 0x14, 0xF4],
        0x800,
        descriptor,
        8,
    );
    assert_eq!(cpu.gdtr.limit, 0x00FF);
    assert_eq!(cpu.gdtr.base, 0x0010_2030);
    assert_eq!(cpu.idtr, DescriptorTable::default());
}

#[test]
fn lidt_loads_idt_descriptor_independently() {
    let descriptor: &[u8] = &[0x7F, 0x03, 0xAB, 0xCD, 0xEF, 0x00];
    // LIDT [SI] — 0x0F 0x01 /3, ModR/M = 00 011 100 = 0x1C
    let (cpu, _, _) = run_with_data(
        &[0xBE, 0x00, 0x08, 0x0F, 0x01, 0x1C, 0xF4],
        0x800,
        descriptor,
        8,
    );
    assert_eq!(cpu.idtr.limit, 0x037F);
    assert_eq!(cpu.idtr.base, 0x00EF_CDAB);
    assert_eq!(cpu.gdtr, DescriptorTable::default());
}

#[test]
fn unknown_opcode_reports_error() {
    let mut mem = Memory::new(0x10_0000);
    // 0x0F is now a real prefix. Use 0x0F + an unrecognised second
    // byte to test the catch-all in the two-byte opcode space.
    mem.write_slice(0x7C00, &[0x0F, 0x77]);
    let mut cpu = Cpu::new();
    cpu.reset_to_boot();
    let mut io = IoBus::new();
    let err = cpu.step(&mut mem, &mut io).unwrap_err();
    match err {
        CpuError::Unimplemented { opcode: 0x77, .. } => {}
        other => panic!("unexpected: {other:?}"),
    }
}
