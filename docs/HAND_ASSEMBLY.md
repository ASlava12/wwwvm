# Hand-assembling your first wwwvm guest

This tutorial walks through writing a guest "from scratch": byte-by-byte
x86 machine code that runs inside wwwvm. By the end you'll have a
program that reads a number from the terminal, doubles it, and prints
the result.

Audience: a student who knows what bits, bytes, and registers are, but
has never hand-encoded an x86 instruction. We use no assembler — every
byte is explicit. That's the point: you'll *see* how an opcode maps to
behavior.

## What's running

The wwwvm VM boots a CPU in **16-bit real mode** at CS:IP = 0000:7C00 —
the same address a real PC's BIOS loads a boot sector into. Your
program is at physical address 0x7C00.

After reset:
- All general-purpose registers (AX, CX, DX, BX, SI, DI, BP) are zero.
- SP = 0x7C00 (stack grows downward from the boot sector).
- All segment registers are zero, so `[addr]` means linear address `addr`.
- Interrupts are disabled (IF=0). Use `STI` (0xFB) to enable.

You can poke at three channels:
- **UART** at ports 0x3F8..0x3FF. Writing to 0x3F8 prints a byte to the
  host terminal. Reading from 0x3F8 returns a byte the host pushed via
  `vm.send_input`. Status byte 0x3FD bit 0 = "data available".
- **PIT timer** at 0x40/0x43 → IRQ 0. See README for the control word
  format if you want periodic interrupts.
- **VGA text** at memory 0xB8000. Each cell is `char_byte + attr_byte`.
  Read back from JS with `vm.vga_text_snapshot()`.

## Anatomy of an instruction

A 16-bit-mode x86 instruction is one or more bytes. The first byte is
the **opcode** which determines what the instruction does. Many opcodes
take a second **ModR/M** byte that selects registers or a memory
operand, followed optionally by displacement and/or immediate bytes.

Example: `MOV AX, 0x1234`

| Byte | Meaning |
|------|---------|
| `B8` | Opcode "MOV reg16, imm16" with reg = AX (B8 + reg-index, AX=0) |
| `34` | Low byte of immediate |
| `12` | High byte of immediate |

Three bytes total. Little-endian: low byte first.

Another: `MOV AL, 'H'`

| Byte | Meaning |
|------|---------|
| `B0` | Opcode "MOV reg8, imm8" with reg = AL (B0 + reg-index, AL=0) |
| `48` | The character 'H' (ASCII 0x48) |

Reference for the registers wwwvm uses (Intel's standard r8/r16
ordering):

```
r16:  0=AX  1=CX  2=DX  3=BX  4=SP  5=BP  6=SI  7=DI
r8:   0=AL  1=CL  2=DL  3=BL  4=AH  5=CH  6=DH  7=BH
```

## Example 1 — print "HI" via UART

This guest prints two bytes and halts. Total: 10 bytes.

```
Offset  Bytes       Mnemonic
00      BA F8 03    MOV DX, 0x3F8      ; UART data port
03      B0 48       MOV AL, 'H'
05      EE          OUT DX, AL          ; print 'H'
06      B0 49       MOV AL, 'I'
08      EE          OUT DX, AL          ; print 'I'
09      F4          HLT
```

Putting it in JS:

```js
import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
await init();
const vm = new WwwVm();

// Hand-assembled bytes:
const program = new Uint8Array([
  0xBA, 0xF8, 0x03,
  0xB0, 0x48,
  0xEE,
  0xB0, 0x49,
  0xEE,
  0xF4,
]);

vm.load_image(0x7C00, program);
vm.boot();
vm.run(1000);
console.log(vm.read_output());  // "HI"
```

Run it in your browser DevTools console. You should see `HI` printed.

### What just happened

- `MOV DX, 0x3F8` sets DX to the UART data port. We need DX because the
  `OUT DX, AL` instruction takes its port number from DX.
- `MOV AL, 'H'` puts the byte 0x48 in AL.
- `OUT DX, AL` writes AL to the port whose number is in DX. The UART
  treats writes to 0x3F8 as bytes to transmit.
- Repeat for `'I'`.
- `HLT` parks the CPU.

## Example 2 — read a byte, double it, print

This guest reads one byte from the host (sent via `vm.send_input`),
multiplies it by 2 with a shift, and prints the low byte.

```
00  BA FD 03   MOV DX, 0x3FD       ; UART LSR (line status)
03  EC         IN  AL, DX          ; loop body — poll LSR
04  A8 01      TEST AL, 1          ; bit 0 = data available
06  74 FB      JZ  -5 -> 0x03      ; spin until ready
08  BA F8 03   MOV DX, 0x3F8       ; switch to UART RBR
0B  EC         IN  AL, DX          ; AL = the input byte
0C  D0 E0      SHL AL, 1           ; AL *= 2
0E  EE         OUT DX, AL          ; DX still 0x3F8 — print it
0F  F4         HLT
```

16 bytes. Note the **JZ** at offset 0x06: it reads `74 FB`, which means
"jump if zero, displacement -5". The displacement is added to the IP
*after* the JZ instruction is fetched, so the target is
`0x08 + (-5) = 0x03` — back to the `IN AL, DX`. DX stays at 0x3FD
through the poll, then 0x3F8 for the read+write — no need to reload
it across the doubling.

Drive it from JS:

```js
vm.load_image(0x7C00, new Uint8Array([
  0xBA, 0xFD, 0x03,
  0xEC,
  0xA8, 0x01,
  0x74, 0xFB,
  0xBA, 0xF8, 0x03,
  0xEC,
  0xD0, 0xE0,
  0xEE,
  0xF4,
]));
vm.boot();
vm.send_input(new Uint8Array([10]));  // push byte value 10
vm.run(2000);
console.log(vm.read_output());  // Uint8Array with byte 20
```

## Computing jump displacements

Every conditional and unconditional short jump in this tutorial uses an
**8-bit signed displacement**. The CPU's algorithm is:

```
1. Fetch the JZ/JMP opcode and the displacement byte.
2. IP now points to the *next* instruction.
3. If the condition is true (for JZ: ZF=1), set IP = IP + sign-extend(disp8).
```

So if you want to jump back to offset 0x00 from a JZ at offset 0x06:
- After fetching the 2-byte JZ, IP = 0x08.
- Target = 0x00. Displacement = `0x00 - 0x08 = -8 = 0xF8` (two's complement).

The same rule for forward jumps. To skip 6 bytes from a JZ at offset
0x10:
- After fetch, IP = 0x12.
- Target = 0x18. Displacement = 0x18 - 0x12 = +6 = 0x06.

## A small opcode reference card

The set wwwvm supports is much larger than this — see
[crates/cpu/src/lib.rs](../crates/cpu/src/lib.rs) — but these cover
the basics for hand-assembled programs:

| Mnemonic         | Bytes        | Notes                               |
|------------------|--------------|-------------------------------------|
| `MOV r8, imm8`   | `B0+r imm8`  | r in 0..7 (AL/CL/DL/BL/AH/CH/DH/BH) |
| `MOV r16, imm16` | `B8+r imm16` | Little-endian imm                   |
| `OUT DX, AL`     | `EE`         | Port from DX                        |
| `IN AL, DX`      | `EC`         | Same                                |
| `OUT imm8, AL`   | `E6 imm8`    | For ports 0..255 (e.g. PIC 0x20)    |
| `JMP rel8`       | `EB disp8`   | Unconditional short jump            |
| `JZ rel8`        | `74 disp8`   | Jump if ZF=1                        |
| `JNZ rel8`       | `75 disp8`   | Jump if ZF=0                        |
| `TEST AL, imm8`  | `A8 imm8`    | AND, set flags, discard result      |
| `CMP AL, imm8`   | `3C imm8`    | Subtract, set flags, discard        |
| `ADD AL, imm8`   | `04 imm8`    |                                     |
| `SHL AL, 1`      | `D0 E0`      | Group 2 / ModR/M = 11 100 000       |
| `SHR AL, 1`      | `D0 E8`      | ModR/M = 11 101 000                 |
| `INC AL`         | `FE C0`      | Group 4 /0                          |
| `STI`            | `FB`         | Enable interrupts                   |
| `HLT`            | `F4`         | Park the CPU                        |
| `LODSB`          | `AC`         | AL = [DS:SI]; SI++ (or -- if DF=1)  |
| `OR AL, AL`      | `08 C0`      | Sets ZF=1 if AL is zero             |

## Sending whole strings

The conventional way to print a NUL-terminated string is the `LODSB`
loop. Put the string at some known address, point SI at it, and read
one byte per iteration.

```
        MOV SI, msg
.loop:  LODSB
        OR AL, AL       ; ZF=1 when we hit the NUL
        JZ .done
        OUT DX, AL
        JMP .loop
.done:  HLT

msg:    db "Hello\n", 0
```

This is exactly what the built-in `interactive_demo` guest does to
print its banner — see
[`crates/vm/src/lib.rs`](../crates/vm/src/lib.rs) for the byte-by-byte
expansion.

## Example 3 — interrupt-driven timer counter

So far our guests have *polled* — the read-byte example sat in a tight
loop on the UART line-status register, asking "is there data yet?"
forever. Real systems don't do that. They configure a device to raise
an interrupt when it has work, then let the CPU sleep or do something
useful in between.

This example sets up the 8254 PIT (the PC's interval timer) to fire
IRQ 0 every 50 CPU steps. A handler increments a counter in memory.
Main code spins until the counter hits 4 and then halts. The PIT
keeps firing — *we'd never know about it* from the main code if not
for the IDT.

### The machinery you need to set up

Three things have to happen at boot before the first interrupt can
land in a handler:

1. **Install the handler in the IVT** so the CPU knows where to jump
   when IRQ 0 arrives. IRQ 0 maps to vector 0x08 (master PIC default
   base = 0x08). The IVT lives at linear 0, each entry is 4 bytes
   `offset, segment`. Use `vm.set_ivt(8, 0, handler_addr)` from JS
   instead of emitting `MOV WORD [0x20]` bytes — same effect, much
   less noise in the guest.
2. **Program the PIT** — write a control word to port 0x43 picking
   the channel, mode, and access pattern, then write the reload value
   (LSB first, then MSB) to port 0x40.
3. **Unmask IRQ 0 in the PIC** — port 0x21 holds the master Interrupt
   Mask Register; bit 0 = IRQ 0. Write `0xFE` to clear bit 0 and leave
   the others masked.
4. **`STI`** so the CPU starts honoring IRQs.

### Main, 25 bytes

```
0x00 B0 34            MOV AL, 0x34        ; SC=0, RW=3 (LSB then MSB), mode=2
0x02 E6 43            OUT 0x43, AL        ; PIT control word
0x04 B0 32            MOV AL, 50          ; reload LSB = 50 ticks
0x06 E6 40            OUT 0x40, AL
0x08 30 C0            XOR AL, AL          ; reload MSB = 0
0x0A E6 40            OUT 0x40, AL
0x0C B0 FE            MOV AL, 0xFE        ; PIC IMR — unmask IRQ 0 only
0x0E E6 21            OUT 0x21, AL
0x10 FB               STI
0x11 80 3E 00 09 04   CMP byte [0x900], 4 ; counter location, compare to 4
0x16 75 F9            JNZ -7 -> 0x11      ; spin until counter == 4
0x18 F4               HLT
```

A few notes:

- The PIT runs in *mode 2* (rate generator): every time the counter
  reaches zero it raises an edge on IRQ 0 *and* reloads itself, so the
  IRQ fires periodically forever. Mode 0 (one-shot) halts after the
  first terminal count.
- The `CMP byte [0x900], 4` uses the `[disp16]` form of ModR/M
  (mod=00, rm=110), which is why the bytes start `80 3E` — `3E` is the
  ModR/M byte `00 111 110` (op = CMP via Group 1 reg=7, rm=110).
- The displacement comes next: `00 09` = 0x0900 little-endian.

### Handler, 11 bytes, lives at 0x7C50

```
0x50 50              PUSH AX               ; preserve caller's AX
0x51 FE 06 00 09     INC byte [0x900]      ; advance the counter
0x55 B0 20           MOV AL, 0x20
0x57 E6 20           OUT 0x20, AL          ; non-specific EOI to master PIC
0x59 58              POP AX                ; restore AX
0x5A CF              IRET
```

Everything the handler clobbers, it saves first and restores on the
way out — that's what `PUSH AX` / `POP AX` are for. If we forgot, the
main loop's `CMP` would see a stale value next iteration.

The `OUT 0x20, AL` with `AL=0x20` is the canonical end-of-interrupt
signal to the master PIC. Without it, the PIC would keep IRQ 0 marked
in-service and refuse to deliver a second one — your timer would
"work" exactly once.

`IRET` pops IP, CS, and FLAGS — that last one restores `IF=1`, so the
next interrupt can fire as soon as we're back in the main loop.

### Wire it up from JS

```js
import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
await init();
const vm = new WwwVm();

vm.load_image(0x7C00, new Uint8Array([
  0xB0, 0x34, 0xE6, 0x43,
  0xB0, 0x32, 0xE6, 0x40,
  0x30, 0xC0, 0xE6, 0x40,
  0xB0, 0xFE, 0xE6, 0x21,
  0xFB,
  0x80, 0x3E, 0x00, 0x09, 0x04,
  0x75, 0xF9,
  0xF4,
]));

vm.load_image(0x7C50, new Uint8Array([
  0x50,
  0xFE, 0x06, 0x00, 0x09,
  0xB0, 0x20, 0xE6, 0x20,
  0x58,
  0xCF,
]));

// IRQ 0 → vector 0x08 (master PIC vector base) → handler at 0:0x7C50
vm.set_ivt(0x08, 0x0000, 0x7C50);

vm.boot();
vm.run(5000);
console.log("counter:", vm.read_mem_u8(0x900));  // 4
console.log("halted:", vm.is_halted());           // true
```

The same pattern works for any device IRQ: install the handler in the
right vector slot (UART is IRQ 4 → vector 0x0C, keyboard is IRQ 1 →
vector 0x09), unmask the corresponding bit in the PIC IMR, and EOI at
the end of the handler. See the `pit_timer_drives_irq0_handler_through_vm`
and `uart_rx_drives_irq4_handler_through_vm` integration tests in
`crates/vm/src/tests.rs` for fully worked references.

## Interrupts in two more sentences

When IRQ n fires *and* `IF=1` in FLAGS, the CPU pushes FLAGS/CS/IP,
clears IF (so the handler runs with interrupts masked by default),
and jumps to whichever address you stored in IVT[n]. The handler
does its job, EOIs the relevant PIC(s), and `IRET`s — which pops
IP/CS/FLAGS in that order and resumes the interrupted instruction.

## Where to go from here

- **Reference**: the CPU's full opcode table is the `match opcode { … }`
  inside `Cpu::step` in `crates/cpu/src/lib.rs`.
- **More devices**: ports for the PIT, keyboard, PIC, and CMOS are
  documented in `crates/devices/src/*.rs`.
- **VM API**: see `crates/wasm/src/lib.rs` for everything reachable
  from JS.
- **Save your work**: `vm.snapshot()` returns ~1 MiB of bytes you can
  stash in IndexedDB and feed back to `vm.restore(...)` later.
