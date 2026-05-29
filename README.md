# wwwvm

Учебная виртуальная машина в браузере. Rust компилируется в WebAssembly,
управляется из JavaScript. Цель — обучающий проект по Linux:
страница загружает образ, стартует VM, JS отдаёт команды и получает вывод.

Полный учебный PC: 8086+80186 ISA, стандартный набор устройств (UART,
двойной PIC, PIT, клавиатура, CMOS, VGA-text), interrupt-driven I/O
через IDT, snapshot/restore, доступ из JS через wasm-bindgen, отдельный
Rust-прокси для сетевых соединений. Три встроенных гостя для первого
запуска, тутор по hand-assembly в [docs/HAND_ASSEMBLY.md](docs/HAND_ASSEMBLY.md).

**Linux 6.12 i386 загружается до userspace.** `WWWVM_INITRD_BUILTIN=1
cargo run --release --example linux_boot` собирает минимальный initramfs
(136-байтный ELF /init + /dev/console) inline, бутает tinycore vmlinuz из
`/tmp/wwwvm-linux/vmlinuz` и печатает `HELLO FROM USERSPACE` через полный
путь syscall: user `int 0x80` → kernel cross-ring → `sys_write` →
`tty_write` → `serial8250` THRE IRQ → наш UART → host stdout.
Архитектурный обзор покрытия — раздел "Что уже работает (i386-ядро)".

```
┌──────────────────────────┐         ┌──────────────────────────┐
│ index.html / xterm.js    │  HTTP   │      static server       │
│ main.js (runCommand API) ├────────►│  python -m http.server   │
└──────────┬───────────────┘         └──────────────────────────┘
           │ import init, { WwwVm }                                   
           ▼                                                          
┌──────────────────────────────────────────────────────────────────┐
│                     crates/wasm  (cdylib)                        │
│  WwwVm  ─►  load_image / boot / run / send_command / read_output │
└──────────┬───────────────────────────────────────────────────────┘
           │
┌──────────▼──────────┐
│  crates/vm          │  Vm = { Cpu, Memory, IoBus, autorun }
│  HELLO_GUEST (43 B) │  pumps cycles, queues autorun on boot()
└──┬─────┬─────┬──────┘
   ▼     ▼     ▼
 cpu   mem   devices (UART 16550 on COM1)

(сеть)
┌──────────────────────────┐  WS  ┌──────────────────────────────┐
│ Browser                  │◄────►│ crates/proxy (Rust, tokio)    │
│ (future virtio-net stub) │      │ WebSocket ↔ TCP gateway       │
└──────────────────────────┘      │ allowlist via env var         │
                                  └──────────────────────────────┘
```

## Что работает сейчас

### CPU (`crates/cpu`) — 8086 ISA полностью + 80186-добавки

Поддерживается весь основной набор инструкций реального режима:
полная ALU-семья (8/16 бит, все формы операндов), сдвиги и
повороты включая RCL/RCR через CF, MUL/IMUL/DIV/IDIV, INC/DEC,
TEST/CMP, MOV/MOVS/LODS/STOS/SCAS/CMPS с REP-префиксами, стек
SS:SP с PUSH/POP/PUSHA/POPA, near и far CALL/RET/RETF, condition jumps,
LOOP/LOOPE/LOOPNE/JCXZ, XCHG, LEA, XLAT, CBW/CWD/LAHF/SAHF, BCD-семейство
(DAA/DAS/AAA/AAS/AAM/AAD), ENTER/LEAVE (level 0), 3-операндный IMUL,
управление флагами (CLC/STC/CMC/CLD/STD/CLI/STI).

ModR/M полный — все 16-битные формы адресации с правильным выбором
сегмента по умолчанию (SS для `[BP*]`, иначе DS), seg-override
префиксы CS:/DS:/ES:/SS:, ModR/M-память во всех ALU-командах.

Прерывания в реал-моде — `INT imm8`/`INT3`/`INTO`/`IRET` через IVT
на линейном 0. Внешние IRQ от устройств доставляются автоматически:
в начале `step()` CPU проверяет `IoBus::pending_irq_vector()` и,
если `IF=1`, делает ack + `do_interrupt(vec)`.

Неподдержанный опкод → `CpuError::Unimplemented { opcode, cs, ip }`,
деление на ноль → `CpuError::DivideError`. Тестов — 98.

### Устройства (`crates/devices`) — стандартный PC stack

| Чип | Порты | IRQ | Триггер |
|-----|-------|-----|---------|
| 16550 UART (COM1)        | 0x3F8..0x3FF | 4  | level (rx + IER bit 0) |
| 8259A PIC (master)       | 0x20/0x21    | —  | — |
| 8259A PIC (slave)        | 0xA0/0xA1    | 8..15 → каскад через master IRQ 2 |
| 8254 PIT (канал 0)       | 0x40..0x43   | 0  | edge (terminal count) |
| PS/2 keyboard            | 0x60/0x64    | 1  | level (rx queue non-empty) |
| MC146818 CMOS/RTC        | 0x70/0x71    | —  | (alarm IRQ 8 не реализован) |
| VGA text mode (RAM)      | mem 0xB8000  | —  | — |

`IoBus::refresh_irqs` на каждом шаге CPU перекладывает все pending
IRQ в IRR. Slave автоматически каскадит через master IRQ 2: если master
выставляет IRQ 2, `pending_irq_vector` спускается в slave и возвращает
его вектор, а `ack_irq` ack'ает оба чипа — двухтактный INTA на железе.

### Оркестрация (`crates/vm`)

API из JS:
- **Lifecycle:** `new()` → `load_default_guest()` / `load_interactive_demo()` /
  `load_calculator_demo()` / `load_image(addr, bytes)` → `set_autorun_commands(…)`
  → `boot()` → `run_steps(budget) -> (steps, Stop)`
- **I/O:** `send_input(bytes)`, `drain_output() -> Vec<u8>`,
  `push_scancode(code)`, `set_cmos_time(y,m,d,h,mi,s)`
- **Память/IDT:** `set_ivt(vec, seg, off)`, `read_mem_u8/u16(addr)`,
  `vga_text_snapshot() -> String`
- **Persistence:** `snapshot() -> Vec<u8>` / `restore(&[u8]) -> Result<…>`

Три встроенных гостя:
- `HELLO_GUEST` (43 байта) — polling LSR + echo;
- `interactive_demo` — banner через LODSB + IRQ-driven UART echo;
- `calculator_demo` — `byte²` через MUL, decimal-форматирование через
  divide-by-10 + push/pop.

Snapshot v2 ≈ 1 MiB + ~200 байт: header `WWWVM\x00` + 36-байтный CPU
image + 1 МБ memory dump + length-prefixed device-state секция.
`restore` принимает и v1 (без device state, backward compat), и v2.

### Bridge в JS (`crates/wasm`)

`WwwVm` через wasm-bindgen экспортирует весь lifecycle Vm плюс
`snapshot/restore` (как `Vec<u8>` / `&[u8]`) и `vga_text_snapshot()`.
Ошибки CPU surface как `last_error: Option<String>`.

### Сеть (`crates/proxy`)

Standalone Rust-бинарь на tokio + tokio-tungstenite. Принимает
WebSocket, первое сообщение JSON `{"host","port"}`, дальше байты в
обе стороны. Allow-list — env var `WWWVM_PROXY_ALLOWLIST`
(`*` / `host:port` / `host:*`, comma-separated).

### Веб-демо (`web/`)

- xterm.js terminal с двусторонним IO;
- селектор между 3 встроенными гостями + autorun-textarea;
- `window.runCommand(text) -> Promise<string>` для DevTools;
- **Save/Load** через IndexedDB (`storage.js`);
- **Download .bin / Upload .bin** — портативный экспорт-импорт;
- pane с VGA-snapshot 80×25.

### Качество

**568 тестов** зелёные (mem 30 + devices 77 + cpu 320 + vm 124 +
tutorial-anchor 2 + wasm 7 + proxy 8). Снапшот v14.
CI gates: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace
--locked`. Throughput release ≈ 60–110 MIPS зависит от хоста
(x86_64 быстрее aarch64; пример печатает арку, чтобы цифры не
сравнивались случайно: `cargo run --example throughput -p wwwvm-vm
--release`). Tutorial-anchor тесты в
`crates/vm/tests/tutorial_examples.rs` пин-fиксируют hex-байты из
`docs/HAND_ASSEMBLY.md` — любое смещение между документацией и
поведением VM ловит CI.

## Что уже работает (i386-ядро)

Непривилегированный + системный i386/i486/Pentium набор по сути
готов и широко проверен realistic end-to-end медли (sum-of-squares
loop, SIB array + CALL, рекурсивный factorial по cdecl, 64-битная
ADC/SBB-арифметика, INT 0x80 syscall, strlen через REPNE SCASB,
spinlock через LOCK CMPXCHG + PAUSE):

- **Protected mode**: CR0.PE, GDT/LDT-дескрипторы, segment cache,
  16- и 32-битные IDT-gates с раздельным IF (interrupt vs trap),
  #PF с CR2 и полным error-code (P/W/U-S/I-D), IRET/IRETD,
  кросс-ring INT через TSS.SS0:ESP0, кросс-ring IRETD pops
  user SS:ESP. CS.D bit из дескриптора latched в `code_size_32`
  — 32-bit код работает без 0x66-stuffing каждого immediate.
- **Paging**: CR0.PG, CR3, CR0.WP (supervisor write на R/W=0
  стрелы #PF — нужно для COW), 2-уровневый walk PDE/PTE,
  4 MiB PSE-страницы (CR4.PSE + PDE.PS), A20-gate, demand
  paging с rewind IP к faulting instruction (IRETD ретраит
  тот же MOV), I/D bit на instruction-fetch faults, U/S bit
  на CPL=3 access.
- **32-бит**: полный EIP, операнд/адрес-префиксы 0x66/0x67, SIB,
  ESP-стек, все ALU/shift/rotate/mul/div формы, TEST/XCHG/IMUL
  (2- и 3-операндные), MOVZX/MOVSX, CMOVcc/SETcc, BT/BTS/BTR/BTC,
  BSF/BSR/BSWAP, XADD/CMPXCHG, CMPXCHG8B (64-битный CAS),
  SHLD/SHRD, far/near jumps + Jcc rel32, ENTER/LEAVE,
  PUSHAD/POPAD/PUSHFD/POPFD, FS/GS префиксы.
- **Системное**: CR2/CR3/CR4, MOV DR0..7 (stub-only),
  RDMSR/WRMSR (TSC read+write/APIC/SYSENTER + MISC_ENABLE
  (64-bit dword pair) /BIOS_SIGN_ID/TSC_AUX/PLATFORM_ID/
  MTRR_DEF_TYPE/FEATURE_CONTROL/MCG_CAP/EFER), RDTSC + RDTSCP
  (с TSC_AUX в ECX) + RDPMC (через CR4.PCE из CPL>0, stub
  возвращает 0). CPUID: leaf 0/1/2 + ext 0x80000000..0x80000008
  (включая address widths). EDX-флаги: FPU/PSE/TSC/MSR/CX8/SEP/
  PGE/CMOV/CLFLUSH/FXSR/SSE/SSE2. EBX содержит CLFLUSH line
  size = 64 байт.
  SYSENTER/SYSEXIT, LLDT/LTR/SGDT/SIDT/SMSW/LMSW,
  CLTS, INVLPG, WBINVD, PAUSE, LOCK, UD2 (#UD vector 6),
  RDMSR/WRMSR на неизвестных MSR раздают #GP(0) (как rdmsr_safe),
  MOV/POP sreg и LES/LDS на селекторе вне GDT.limit раздают
  #GP(selector) через общий raise_gp_if_bad_selector,
  DIV/IDIV/AAM на нуле или overflow раздают #DE (vector 0) через IVT.
- **x87 FPU**: 8×f64 register-stack с TOP, FLD/FST/FSTP (m32/m64),
  FILD/FISTP, FADD/FMUL/FSUB(R)/FDIV(R) + ...P-формы, FCHS/FABS/
  FSQRT/FRNDINT, константы (FLD1/FLDPI/...), FCOM/FTST + FNSTSW.
- **SSE/SSE2** (подмножество): XMM register file, MOVD/MOVDQA/
  MOVDQU/MOVAPS/MOVUPS, MOVSS/MOVSD, PAND/POR/PXOR, PADD/PSUB
  (B/W/D/Q), packed ADD/SUB/MUL/DIV (PS/PD) + скалярные (SS/SD),
  CVTSI2SS/SD + CVT(T)SS/SD2SI, [U]COMISS/[U]COMISD (флаги),
  MIN/MAX/SQRT (packed PS/PD + скалярные SS/SD), битовые
  ANDPS/ANDNPS/ORPS/XORPS, UNPCKL/UNPCKH PS/PD, SHUFPS/SHUFPD,
  PSHUFD, MOVHLPS/MOVLHPS + MOVLPS/MOVHPS m64-load+store,
  PCMPEQB/W/D + PCMPGTB/W/D (signed), Group 12/13/14 imm-shifts
  (PSLLW/D/Q + PSRLW/D/Q + PSRAW/D + PSLLDQ/PSRLDQ),
  переменные shift-по-XMM, PMULLW/PMULHW/PMADDWD, packed-конверты
  CVTPS2PD/CVTPD2PS, CVTDQ2PS/CVTPS2DQ/CVTTPS2DQ,
  CVTDQ2PD/CVTPD2DQ/CVTTPD2DQ + scalar CVTSS2SD/CVTSD2SS,
  saturating PADDUS/PSUBUS/PADDS/PSUBS, PACKSSWB/PACKUSWB/PACKSSDW,
  PSHUFHW/PSHUFLW, PMULUDQ/PMULHUW/PSADBW, PMINUB/PMAXUB/PMINSW/PMAXSW,
  PAVGB/PAVGW, MOVQ (load и store), PMOVMSKB, non-temporal stores
  MOVNTDQ/MOVNTPS/MOVNTI, MASKMOVDQU, фенсы LFENCE/SFENCE/MFENCE.
- **BIOS-shim**: INT 0x10 (AH=0x00 set mode / 0x01 set cursor
  shape / 0x02 set cursor / 0x03 get cursor / 0x06 scroll up /
  0x07 scroll down / 0x08 read char+attr / 0x09 char+attr / 0x0E
  TTY / 0x0F get mode / 0x12/BL=10 VGA info / 0x13 write string),
  0x12 low-mem, 0x13 (AH=0x02 read sectors / 0x03 write sectors /
  0x00 reset / 0x01 status / 0x08 get drive params / 0x41 LBA ext
  check), 0x15 (AH=0x86 wait µs / AH=0x88 ext-mem / AH=0xC0
  config-stub / AH=0x24 A20 control [0/1/2/3] / AX=0xE801 mem split /
  AX=0xE820), 0x16 (AH=0x00 read / 0x01 peek / 0x02 shift flags),
  0x1A (AH=0x00 get tick / 0x01 set tick / 0x02 RTC time / 0x04
  RTC date — BCD из CMOS). Неподдерживаемые подфункции для
  INT 0x10/0x13/0x15 возвращают CF=1 / AH=0x86 — не падают через
  null IVT entry.
- **IDE/ATA два канала** (primary 0x1F0..0x1F7 + control 0x3F6,
  secondary 0x170..0x177 + control 0x376): IDENTIFY DEVICE,
  READ SECTORS и WRITE SECTORS (LBA28). Alt-status и device-control
  через base+0x206. 16-битная передача данных приходит как пара
  байтовых обращений подряд — оба продвигают буфер, в обе стороны
  (read drain и write fill).
- **Загрузка**: cold-boot из disk-sector, ELF32-loader, bzImage
  header parser + loader, `set_kernel_cmdline` / `set_ramdisk`,
  `start_protected_mode_at` (skip real-mode trampoline; honors
  boot protocol §4.1: ESI=0x90000, EBP/EDI/EBX=0, IF clear,
  ESP=0x7C00), `load_pm_demo` (bundled synthetic bzImage что
  печатает "Hello from PM!" в UART). Снапшот v12 round-trip'ит
  всё состояние CPU (включая code_size_32, misc_enable, tsc_aux,
  DR0..7) + RAM + LAPIC + HPET scratch buffers + devices.
- **PCI** (порты 0xCF8/0xCFC, Mechanism #1): пустая шина — все
  чтения окна данных возвращают 0xFFFFFFFF (sentinel "нет устройства").
  Полноценные 32-битные IN/OUT через 0x66-префикс декомпозируются
  в четыре байтовых обращения подряд.
- **LAPIC MMIO** (0xFEE0_0000 + 4 KiB): Version reg на 0x030 =
  0x0006_0014, ID на 0x020 = 0. Таймер: write в Initial Count
  (0x380) защёлкивает Current Count (0x390); каждый cpu.step
  декрементирует Current Count, на zero-crossing — если
  LVT_TIMER (0x320) не masked — диспетчится вектор из LVT_TIMER.
  Поддержаны periodic (бит 17=01) и one-shot (00) режимы.
  Доставка идёт впереди legacy-PIC очереди в step(). EOI-запись
  в 0x0B0 — no-op (scratch). IA32_APIC_BASE MSR не выставляет
  enable-bit; Linux всё ещё видит и LAPIC, и legacy PIC.
- **HPET MMIO** (0xFED0_0000 + 1 KiB): General Caps на 0x000 =
  0x05F5_E100_8086_A201 (3 таймера, 64-битный counter, vendor
  0x8086, 100 ns период). Main Counter (0x0F0) тикает на cpu.step
  когда General Configuration's ENABLE_CNF (0x010 бит 0) выставлен.
  На каждом инкременте проверяются 3 таймера: если у таймера
  INT_ENB_CNF (бит 2) и FSB_EN_CNF (бит 14) выставлены, а Main
  Counter совпал с Comparator — диспетчится IRQ по вектору из
  FSB_INT_VAL (Linux'овский MSI-driver HPET). LAPIC и HPET делят
  слот pending_lapic_irq (обе цели — local APIC).

## Загрузка Linux 6.12 (tinycore vmlinuz)

Полный путь от ROM-handoff до userspace проверен на tinycore
vmlinuz (5.85 MB сжатого ядра, ~12 MB после распаковки). Recipe —
одна команда:

```
WWWVM_INITRD_BUILTIN=1 cargo run --release --example linux_boot
```

Минимальный initramfs (136-байтный ELF /init + /dev/console)
собирается inline в [crates/vm/examples/linux_boot.rs](crates/vm/examples/linux_boot.rs);
для своих init-ELF'ов передавайте `WWWVM_INITRD=path/to/cpio`.
Полный прогон через linux_boot example ~10 минут wall-clock —
там работает кучка per-step диагностики (EIP region tracking, IF
transitions, stuck detection) которая интересна для отладки но
жрёт ~5x времени. Чистый прогон без диагностики (см. integration
test ниже) занимает **~52 секунды** на той же машине: эффективные
≈150 MIPS, ≈8 миллиардов CPU-инструкций до момента когда /init
успевает напечатать HELLO. В UART видна вся последовательность
kernel boot → driver_init → do_initcalls → run_init_process →
пользовательский `int 0x80` write → THRE IRQ → host stdout →
пользовательский exit → kernel panic с `exitcode=0x00002a00`
(= exit(42) << 8). Подробности end-to-end syscall-цепочки — в
commit `milestone: Linux 6.12 boots to userspace`.

Регрессионный тест milestone'а зафиксирован в
[crates/vm/tests/linux_userspace.rs](crates/vm/tests/linux_userspace.rs)
и помечен `#[ignore]` (потому что зависит от vmlinuz файла). Тест
двухэтапный: сначала ждёт `HELLO FROM USERSPACE` (write-syscall +
THRE), потом — kernel panic с `exitcode=0x00002a00` (sys_exit +
panic-on-init-exit). Оба маркера обычно попадают в один drain,
поэтому overhead второго чека ≈ 0с — итого те же ~52 секунды.

Рядом — second milestone в том же файле:
`linux_userspace_proc_version_milestone`. /init монтирует procfs
через `sys_mount("proc", "/proc", "proc", 0, 0)` (5-аргументный
syscall через int 0x80!), открывает `/proc/version`, читает в
128-байтный буфер, печатает с уникальным префиксом `[USERSPACE
/proc/version]:` (чтобы не путать с кернел-printk'ом самого
boot-баннера), и `exit(0)`. Пять разных syscall'ов (mount, open,
read, write, exit) через cross-ring trampoline, и kernel
действительно отдаёт `Linux version 6.12...` из procfs в
user-space. Ещё ~52 секунды wall-clock, идёт параллельно с
основным milestone'ом.
Запуск: `WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz cargo test
--release --test linux_userspace -- --ignored`. Если файла нет —
тест silently skip'ается; на CI без vmlinuz просто пропустит.

Что нужно vmlinuz: положите его в `/tmp/wwwvm-linux/vmlinuz` или
укажите путь через `WWWVM_KERNEL=...`. Tinycore Core ISO извлекает
vmlinuz прямо из `boot/vmlinuz`.

**Двусторонний I/O через tty.** Та же команда с `WWWVM_INIT_INPUT=Q`
переключает встроенный /init в echo-режим:

```
WWWVM_INITRD_BUILTIN=1 WWWVM_INIT_INPUT=Q \
  cargo run --release --example linux_boot
```

/init делает `write(1, "echo ", 5); read(0, &buf, 1); write(1, &buf, 1);
write(1, "\n", 1); exit(42)`. Пример откладывает push байта в UART
rx queue до момента, когда `/init`'s "echo " префикс появляется в
выводе — это обходит 8250 autoconfig probe, который иначе съедает
наши rx-байты. В выводе появляется `echo Q\n` — RDA IRQ доставка,
serial8250 ISR, tty line discipline buffer, scheduler wake блокированного
/init, обратная запись через THRE IRQ — все звенья цепи работают.

## Что НЕ работает (дорожная карта к Alpine)

Между «грузит userspace из minimal initramfs» и «грузит Alpine» —
ещё дистанция. Крупные оставшиеся блокеры, по приоритету:

| Блокер | Объём | Зачем |
|--------|-------|-------|
| x87 расширения (трансцендентные FSIN/FCOS/FPTAN/F2XM1, 80-бит m80, FPU-исключения) | средний | База (стек + арифметика + сравнения) уже есть; glibc местами зовёт трансцендентные |
| MMX-стек (mm0..mm7, EMMS, packed-int MMX-only), помарки в SSE3+ (HADDPS/HADDPD, MOVDDUP, LDDQU) | средний | SSE2 готов в практическом смысле; Alpine ≥3.x линкуется именно с ним. MMX совершенно отдельный регистровый стек — линукс почти не пользуется в современном коде |
| Real-mode setup execution (~16 KiB Linux boot-ASM) | очень большой | bzImage сам делает PE-переход — нужно выполнить его setup-код |
| Kernel decompression (gzip/zstd) | средний | bzImage payload сжат; либо распаковывать, либо грузить vmlinux |
| Ring 3 + полноценный TSS + privilege transitions | малый | Cross-ring INT/IRET, syscall round-trip (IRETD→user→INT→handler), cross-ring #PF — всё работает. CPL=0 guards стоят на HLT/CLI/STI/IN/OUT (IOPL), LLDT/LTR/LGDT/LIDT/LMSW/INVLPG, INVD/WBINVD, MOV CR/DR, RDMSR/WRMSR/SYSEXIT, CLTS, RDPMC (через CR4.PCE). Остаётся: per-port IO permission bitmap в TSS |
| Полный #DF / #NP / #SS | средний | #DE, #UD, #PF и весь основной #GP набор уже доезжают; #DF/#NP/#SS — ещё нет |
| IDE/ATA DMA / virtio-blk | средний | Оба канала (primary + secondary) read+write через PIO уже работают; для модерн дистров нужно ещё DMA |
| HPET таймер-IRQ / реалистичный PIT-тайминг | малый | LAPIC периодический таймер уже доставляет; HPET — только probe-stub без доставки. Linux в большинстве конфигов берёт LAPIC, так что HPET-доставка — second-tier. |
| ne2k/virtio-net + slirp поверх `crates/proxy` | средний | Сеть из гостя |
| VGA graphics, framebuffer | средний | fbcon, графические гости |

Честная оценка: до первой попытки boot минимального Linux —
сотни инкрементов; до работающего Alpine userspace — кратно больше.
Текущий цикл закрывает их по одному с тестом на каждый шаг.

## Сборка и запуск

### Хост-тесты (всегда работает)

```bash
cargo test --workspace
```

Должно вывести 568 пройденных тестов на текущий момент. CI
(`.github/workflows/ci.yml`) дополнительно гоняет `cargo fmt --check`
и `cargo clippy --workspace --all-targets -- -D warnings`.

### Throughput-бенчмарк

```bash
cargo run --example throughput -p wwwvm-vm --release
```

Гоняет ALU-цикл (`ADD BX, CX` + `LOOP`, 65535 итераций) через
`Vm::run_steps` и измеряет инструкций в секунду. На современном
x86_64 host'е release-сборка даёт ~100 MIPS, debug — ~12 MIPS.
Это включает `refresh_irqs`, IRQ-check и fetch+decode+execute на
каждом шаге — то есть видимый из JS throughput.

### Прокси

```bash
WWWVM_PROXY_ALLOWLIST='*' cargo run -p wwwvm-proxy -- 127.0.0.1:9000
```

В реальном развёртывании `*` НЕ использовать — открытый прокси опасен.
Используйте конкретные хосты: `WWWVM_PROXY_ALLOWLIST='hub.docker.com:443,deb.debian.org:80'`.

### Wasm-сборка (для демо)

Нужны:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

Затем:

```bash
wasm-pack build crates/wasm --target web --out-dir ../../web/pkg
```

И поднять статический сервер из корня:

```bash
python3 -m http.server -d web 8080
```

Открыть `http://localhost:8080/`.

В UI:
* выбрать гостя (default polling, interactive IRQ-driven, или
  calculator с MUL и decimal-форматированием);
* вписать команды в Autorun (по одной в строке) → **Boot VM**;
* ввод/команды летят в гостя, вывод появляется в терминале;
* **Save / Load** сохраняет/восстанавливает состояние через IndexedDB;
* **Download .bin / Upload .bin** — портативный экспорт-импорт snapshot'а
  в файл (≈ 1 МБ, формат с магиком `WWWVM\x00`);
* VGA-snapshot pane отражает 0xB8000;
* `runCommand("hello")` доступен в DevTools-консоли.

## API из JavaScript

```js
import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
await init();
const vm = new WwwVm();

// 1. Загрузить образ (встроенный hello-гость или произвольные байты)
vm.load_default_guest();
// или: vm.load_image(0x7C00, new Uint8Array(await fetch("...").then(r => r.arrayBuffer())));

// 2. Заранее задать команды на автозапуск
vm.set_autorun(["echo hi", "ls /"]);

// 3. (Опционально) Установить IVT-обработчик из JS, без MOV WORD в госте
//    vm.set_ivt(vector, segment, offset);
// vm.set_ivt(0x0C, 0x0000, 0x7C40);     // IRQ 4 (UART)

// 4. Загрузиться (CS:IP -> 0000:7C00, autorun-байты доставляются в UART rx)
vm.boot();

// 4. Прокачивать CPU из rAF-цикла
function tick() {
  vm.run(50_000);
  const out = vm.read_output();
  if (out) console.log("guest:", out);
  if (vm.last_error) return console.error(vm.last_error);
  requestAnimationFrame(tick);
}
tick();

// 5. Отправить команду на лету
vm.send_command("uptime");

// 6. Или асинхронно с возвратом результата (см. web/main.js)
const result = await window.runCommand("date");

// 7. Прочитать содержимое памяти гостя (для дебага/ассертов из JS)
const status_byte = vm.read_mem_u8(0x900);
const counter = vm.read_mem_u16(0x902);
```

## Структура

```
crates/
  mem/        # физическая память
  devices/    # 16550 UART + 8259A PIC ×2 + 8254 PIT + PS/2 KBD + CMOS
  cpu/        # x86 real-mode подмножество (8086 + 80186)
  vm/         # оркестратор + встроенные гости + snapshot/restore
  wasm/       # cdylib для браузера (wasm-bindgen)
  proxy/      # отдельный бинарь: WebSocket ↔ TCP
web/
  index.html
  main.js
  storage.js  # IndexedDB-обёртка для snapshot/restore
  style.css
  pkg/        # сюда wasm-pack кладёт wasm + .js шим (gitignored)
docs/
  HAND_ASSEMBLY.md  # tutorial для студентов: писать гостей побайтово
```

## Лицензия

MIT OR Apache-2.0.
