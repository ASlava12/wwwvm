//! Doc-anchor tests for `docs/HAND_ASSEMBLY.md`.
//!
//! Each test embeds the *exact* byte sequence shown in the tutorial
//! and asserts the behaviour the prose claims. If a future refactor
//! changes opcode semantics or a writer fat-fingers a hex byte in the
//! docs, CI catches it.
//!
//! Example 3 (PIT-interrupt counter) is already covered by
//! `pit_timer_drives_irq0_handler_through_vm` in `src/tests.rs`
//! with the same byte sequence — no need to duplicate it here.

use wwwvm_vm::{Stop, Vm, BOOT_LOAD_ADDR};

/// Example 1 — `MOV DX, 0x3F8 ; MOV AL, 'H' ; OUT DX, AL ; MOV AL, 'I' ;
/// OUT DX, AL ; HLT`. Expects the UART to receive exactly `"HI"`.
#[test]
fn tutorial_example_1_prints_hi() {
    let program: &[u8] = &[
        0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
        0xB0, 0x48, // MOV AL, 'H'
        0xEE, // OUT DX, AL
        0xB0, 0x49, // MOV AL, 'I'
        0xEE, // OUT DX, AL
        0xF4, // HLT
    ];
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    let (_, stop) = vm.run_steps(100);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.drain_output(), b"HI");
}

/// Example 2 — poll-LSR loop, IN AL from RBR, SHL AL by 1, OUT back.
/// Send a single byte; expect twice its value back. Verifies the
/// LSR-poll JZ -5 displacement and the implicit DX=0x3F8 carry-over.
#[test]
fn tutorial_example_2_doubles_byte() {
    let program: &[u8] = &[
        0xBA, 0xFD, 0x03, // MOV DX, 0x3FD (UART LSR)
        0xEC, // IN AL, DX                 ; loop body
        0xA8, 0x01, // TEST AL, 1
        0x74, 0xFB, // JZ -5 → IN AL, DX
        0xBA, 0xF8, 0x03, // MOV DX, 0x3F8
        0xEC, // IN AL, DX (RBR)
        0xD0, 0xE0, // SHL AL, 1
        0xEE, // OUT DX, AL
        0xF4, // HLT
    ];
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    vm.send_input(&[10]);
    let (_, stop) = vm.run_steps(1_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.drain_output(), vec![20]);
}
