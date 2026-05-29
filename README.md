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
(139-байтный ELF /init + /dev/console) inline, бутает tinycore vmlinuz из
`/tmp/wwwvm-linux/vmlinuz` и печатает `HELLO FROM USERSPACE` через полный
путь syscall: user `int 0x80` → kernel cross-ring → `sys_write` →
`tty_write` → `serial8250` THRE IRQ → наш UART → host stdout. Второй
милстоун — `linux_userspace_proc_version_milestone` — пинает уже более
широкую поверхность: /init дополнительно делает 5-аргументный sys_mount
("proc"), sys_open("/proc/version"), sys_read и читает обратно
`Linux version 6.12...` из procfs — пять разных syscall'ов через
cross-ring trampoline в одном /init. Третий — `linux_userspace_time_milestone`
— /init зовёт `sys_time(NULL)`, и тест декодирует 4 байта time_t из
UART-стрима: kernel выставляет system clock через CMOS RTC в 1577836800
(2020-01-01 UTC), и jiffies накручивает ещё пару секунд к моменту
exec'а — пинит полный wall-clock путь до userspace. Четвёртый —
`linux_userspace_getpid_milestone` — /init зовёт `sys_getpid`, тест
ассертит ровно `pid == 1` (task struct, scheduler). Пятый —
`linux_userspace_gettimeofday_milestone` — /init зовёт
`sys_gettimeofday(&tv, NULL)`, ядро `copy_to_user`-ит struct timeval
(sec+usec) в /init's буфер; тест проверяет sec в нужном окне и
usec < 1M (sub-second clock + struct-write путь). Шестой —
`linux_userspace_fork_milestone` — /init зовёт `sys_fork`, обa
процесса пишут свой `eax` в UART, тест проверяет что один сequence
содержит 0 (child contract) а другой — реальный child PID
(`[0, 76]` на первом прогоне; process creation + scheduler).
Седьмой — `linux_userspace_execve_milestone` — initramfs содержит
ДВА бинарника, child execve'ит /helper, /helper пишет
`[USERSPACE EXECVE_OK]` (kernel path-resolving execve работает —
шаг к busybox). Восьмой — `linux_userspace_execve_chain_milestone`
— уже **три** бинарника, /init→/h1→/h2, доказывает что execve
работает из уже-exec'нутого процесса, не только из forked child'а.
Девятый — `linux_userspace_brk_milestone` — /init зовёт
`sys_brk(0)`, тест проверяет что возвращённый program break
page-aligned и в правильном диапазоне (mm->brk инициализирован).
Десятый — `linux_userspace_brk_extend_milestone` — /init query'ит
break, потом запрашивает +0x1000; тест ассертит ровно one-page
delta (kernel реально выделяет страницу on-demand, шаг к malloc).
Одиннадцатый — `linux_userspace_argv_milestone` — execve с
настоящим argv `["/helper", "ARG1"]`, /helper читает argv[1] со
стека и печатает; тест ассертит ровно `b"ARG1"` (i386 SysV
process-startup ABI работает). Двенадцатый —
`linux_userspace_envp_milestone` — то же самое для envp:
execve с `envp = ["KEY=VAL"]`, /helper читает envp[0] и пишет;
тест ассертит `b"KEY=VAL"` (теперь main(argc, argv, envp)
полностью работает). Тринадцатый —
`linux_userspace_mmap_milestone` — /init зовёт `sys_mmap2`,
пишет sentinel в новую страницу, читает обратно; тест
проверяет адрес и round-trip байта (отдельный VMA, не brk).
Четырнадцатый — `linux_userspace_file_io_milestone` — первая
**запись в FS**: /init создаёт `/test_file`, пишет, закрывает,
читает через другой fd; тест ассертит round-trip
`b"TESTDATA"` (writable tmpfs + sys_close + cross-fd
persistence). Пятнадцатый —
`linux_userspace_stat_milestone` — /init создаёт файл,
зовёт `sys_stat64`, читает st_size; тест ассертит
`st_size == 8` (inode metadata path + kernel обновляет
i_size после write'а). Шестнадцатый —
`linux_userspace_lseek_milestone` — /init seek'ит fd на
offset 4 и читает 4 байта; тест ассертит `b"DATA"`
(random-access I/O работает). Семнадцатый —
`linux_userspace_dup2_milestone` — /init дублирует fd на
fd 1, пишет через переадресованный fd 1 в файл, читает
обратно и печатает через fd 2 (stderr); тест ассертит
`b"REDIRECTED"` (foundation для shell redirection).
Восемнадцатый — `linux_userspace_unlink_milestone` — /init
создаёт файл, unlink'ает, потом stat'ит; тест ассертит
`stat == -ENOENT = 0xFFFFFFFE` (первый тест на отказ syscall'а).
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

Snapshot v14 для 1 MiB Vm ≈ 1 MiB + ~5 KiB: 16-байтный header
`WWWVM\x00` + ~300-байтный CPU image (включая FPU/SSE/SYSENTER/DR0..7) +
RAM dump + 4 KiB LAPIC scratch + 1 KiB HPET scratch + 12 байт HPET
per-timer period + length-prefixed device-state блок. `restore`
принимает все версии v1..v14 (v1 — без device state; промежуточные —
с дефолтами для полей, добавленных позже).

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

**572 теста** зелёные (mem 30 + devices 77 + cpu 320 + vm 128 +
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
  на CPL=3 access. Mini-TLB на 1 запись для fetch/read/write
  путей: кэширует `(linear_page, phys_frame, a20)` после
  успешного walk; инвалидируется на CR3 reload, INVLPG,
  CR0.WP toggle и CS reload. Боот Linux'а до userspace
  стал ~44% быстрее (95s → 53s) после добавления этих
  кэшей.
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
  печатает "Hello from PM!" в UART). Снапшот v14 round-trip'ит
  всё состояние CPU (включая code_size_32, misc_enable, tsc_aux,
  DR0..7) + RAM + LAPIC + HPET scratch buffers + HPET per-timer
  periods (v13) + PIT channel-2 reload/counter/gate (v14) +
  devices.
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

Минимальный initramfs (139-байтный ELF /init + /dev/console)
собирается inline в [crates/vm/examples/linux_boot.rs](crates/vm/examples/linux_boot.rs);
для своих init-ELF'ов передавайте `WWWVM_INITRD=path/to/cpio`.
Полный прогон через linux_boot example ~10 минут wall-clock —
там работает кучка per-step диагностики (EIP region tracking, IF
transitions, stuck detection) которая интересна для отладки но
жрёт ~5x времени. Если нужно ограничить прогон узким окном (чтобы диагностический
дамп — особенно гистограмма из `WWWVM_DUMP_REGIONS=1` — не
размывался post-panic шагами после выхода /init), задайте
`WWWVM_STEP_BUDGET=N` (decimal, без underscore'ов). Cap имеет
смысл выставлять выше 1.9 B (порог HELLO в integration test'е):
example прогоняет ядро с расширенным cmdline (`initcall_debug`),
который льёт UART-трассу на каждом initcall'е, так что USERSPACE
в example достигается заметно позже — план ~10 B. Чистый прогон без диагностики
(см. integration test ниже) занимает **~52 секунды** на той же
машине: до момента когда /init успевает напечатать HELLO,
проходит ≈1.9 миллиарда CPU-step'ов (т.е. итераций `cpu.step()` —
включая idle-tick'и после HLT с IF=1, не только retired-
инструкции). В UART видна
вся последовательность
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

Третий milestone — `linux_userspace_time_milestone`. /init зовёт
`sys_time(NULL)` (syscall 13, на i386 это `sys_time32`), пишет
4-байтный little-endian time_t в writable data segment, и
печатает его между маркерами `[USERSPACE TIME=…]\n[USERSPACE
END]`. Тест извлекает 4 байта из cumulative UART-стрима и
проверяет, что они попадают в диапазон [2020-01-01, 2038-01-19) —
наша CMOS-имплементация при boot'е выставляет system clock в
`2020-01-01T00:00:00 UTC = 1577836800`, а jiffies накручивает
ещё несколько секунд к моменту exec /init'а (в первом
зелёном прогоне — `time_t = 1577836809`, +9 секунд). Пинит
весь wall-clock путь: CMOS RTC → kernel timekeeping → sys_time
syscall → cross-ring trampoline → userspace decode. Ещё
~52 секунды wall-clock, параллельно с остальными milestone'ами.

Четвёртый — `linux_userspace_getpid_milestone`. /init зовёт
`sys_getpid` (syscall 20), который у ядра возвращает PID
текущего процесса из его task struct. /init печатает 4 байта
pid между `[USERSPACE PID=…]\n[USERSPACE END]`. Тест ассертит
ровно `pid == 1` — любое другое значение означало бы, что
ядро exec'нуло /init под другой task struct (или syscall ABI
mis-route'ит возвращаемое значение). Совсем мелкий тест по
поверхности (по форме почти clone time milestone'а), но пинит
отдельную ядерную подсистему — task-struct/scheduler, не
clock. Ещё ~52 секунды wall-clock.

Пятый — `linux_userspace_gettimeofday_milestone`. /init зовёт
`sys_gettimeofday(&tv, NULL)` (syscall 78); вместо возврата
значения через eax (как в time/getpid) ядро `copy_to_user`-ит
8-байтовый `struct timeval { sec, usec }` в буфер /init'а.
/init пишет эти 8 байт между маркерами `[USERSPACE TV=…]`. Тест
декодирует пару `(tv_sec, tv_usec)` little-endian и ассертит:
`tv_sec ∈ [2020-01-01, Y2038)` (то же окно что у time) и
`tv_usec < 1_000_000`. Тут пинятся две новые штуки относительно
прошлых milestone'ов: struct-write-to-userspace path (kernel
заполняет buffer пользователя — раньше только /proc/version
тестировал тот же механизм для статичной строки) и sub-second
clock resolution (sys_time округляет до секунды, gettimeofday
показывает что ядро действительно тикает на микросекундной
гранулярности). Ещё ~52 секунды wall-clock.

Шестой — `linux_userspace_fork_milestone`. /init зовёт `sys_fork`
(syscall 2), потом `sys_waitpid(-1, NULL, 0)` для синхронизации
(parent блокируется до завершения child'а; child получает
-ECHILD моментально), и **оба** процесса пишут
`[USERSPACE FORK ret=<eax>][USERSPACE END]`. Тест ищет в UART
две таких последовательности и ассертит что значения — `(0,
child_PID_который_родитель_увидел)`. На первом зелёном прогоне:
`fork returns observed: [0, 76]` — child получил 0 (контракт
fork(2)), parent — PID 76 (это первый свободный PID после
кучки kthread'ов которые ядро уже запустило за boot). Без
`waitpid`-синхронизации parent выходил первым и kernel panic
по "Attempted to kill init" убивала child'а посреди записи —
дамп показывал ровно один комплектный sequence + одно "лишнее"
marker_fork от обрезанного child'а. Пинит kernel process
creation (`copy_process` + page-table CoW + dup_task_struct +
wake_up_new_task), scheduler (оба процесса реально получают
CPU), return-to-user-from-fork в child'е (kernel должен поставить
ему регистры так, чтобы он re-enter'ил userspace на той же EIP
с eax=0 но в своём собственном address space'е). Ещё
~52 секунды wall-clock.

Седьмой — `linux_userspace_execve_milestone`. Initramfs теперь
содержит **два** ELF'а — /init и /helper — собирается через
новый `build_cpio_archive_with_helper`. /init форкается, child
зовёт `sys_execve("/helper", NULL, NULL)` (syscall 11), /helper
печатает `[USERSPACE EXECVE_OK]\n` и выходит. Parent waitpid'ит.
Тест ищет OK маркер в UART и параллельно ассертит что fallback
`[USERSPACE EXECVE_FAILED]` маркер НЕ появился (этот маркер
пишет /init если execve вернётся в caller'а — что случается
только при ошибке, потому что успех заменяет process image
целиком). Пинит ядерный path-resolving execve(2): VFS lookup в
initramfs'е, парсинг второго ELF'а, teardown старого mm, setup
нового, jump на entry point /helper'а. До этого момента kernel
exec'ивал только /init напрямую из initramfs unpacker'а
(internal kernel call), а тут уже честный user-issued execve из
forked child'а. Шаг к busybox/shell — теперь можно запускать
произвольные бинарники из userspace, не только /init. Ещё
~52 секунды wall-clock.

Восьмой — `linux_userspace_execve_chain_milestone`. Initramfs
содержит **три** бинарника (/init + /h1 + /h2) через новую
функцию `build_cpio_archive_with_two_helpers`. Поток:
/init форкается → child execve'ит /h1 → /h1 (уже execve'нутый
процесс) execve'ит /h2 → /h2 печатает `[USERSPACE H2_OK]\n` и
выходит. Parent waitpid'ит. У каждой стадии свой distinct
FAILED маркер (`H1_EXEC_FAILED` если init's child не смог
exec /h1, `H2_EXEC_FAILED` если /h1 не смог exec /h2), так что
если тест падает, мы знаем который hop сломался. Пинит execve
из *уже* execve'нутого процесса (не post-fork shortcut), и
второй подряд mm-swap. Первый зелёный прогон: `H2_OK marker
seen after 1900000000 steps`. Ещё ~55 секунд wall-clock.

Девятый — `linux_userspace_brk_milestone`. /init зовёт `sys_brk(0)`
(syscall 45, ebx=0 = query текущего break'а), пишет 4 байта
возврата между маркерами `[USERSPACE BRK=…]`. Тест ассертит что
brk лежит в `[INIT_LOAD_ADDR, 0xC0000000)` и page-aligned (low
12 бит = 0). Первый зелёный прогон: `brk = 0x08502000` — это
≈4.7 MiB выше /init's start address, page-aligned, далеко от
kernel base. Пинит `mm->brk` инициализацию у нового процесса —
ядро ставит heap pointer на конец data segment'а, округлённый
вверх по странице. Ещё ~52 секунды wall-clock.

Десятый — `linux_userspace_brk_extend_milestone`. Продолжение
brk-milestone'а: /init сначала query'ит текущий break через
`sys_brk(0)`, потом запрашивает `current + 0x1000` через
`sys_brk(new_addr)`. /init пишет оба значения между маркерами
`[USERSPACE BRK_OLD=…BRK_NEW=…END]`. Тест декодирует обе
4-байтовые величины и ассертит **точно** `new == old + 0x1000`.
brk(2) по контракту возвращает либо новый break (успех), либо
старый неизменённый (отказ) — отказ ловится тестом как
`new == old`. Первый зелёный прогон: `old_brk = 0x08727000,
new_brk = 0x08728000, delta = 4096 bytes` — ядро выделило
ровно одну новую страницу. Пинит on-demand page allocation
(не просто bump pointer'а, а реальный page table fill для
новой страницы). Шаг к работающему malloc'у — glibc.malloc
использует brk для маленьких аллокаций. Ещё ~53 секунды
wall-clock.

Одиннадцатый — `linux_userspace_argv_milestone`. Process-startup
ABI наконец проверен с настоящим argv (а не NULL как в execve
milestone'ах). /init форкается; child зовёт
`sys_execve("/helper", argv, NULL)` где `argv = ["/helper",
"ARG1"]` — массив из 3 указателей (3-й = NULL terminator),
сами строки лежат в data segment'е /init'а, адреса в argv
computed at build time. /helper читает `argv[1]` со своего
стека (`mov esi, [esp+0x08]` — на entry у /helper'а
`[esp+0]=argc, [esp+4]=argv[0], [esp+8]=argv[1]` по i386 SysV
process-startup contract'у), сохраняет в esi через syscall
round-trip'ы (ядро GP-регистры сохраняет), и пишет
`[USERSPACE ARGV1=ARG1][USERSPACE END]`. Тест декодирует
4 байта между маркерами и ассертит ровно `b"ARG1"`. Первый
зелёный прогон: `argv[1] returned: "ARG1" (raw: 41 52 47 31)`.
Пинит kernel `copy_strings` путь execve(2) (был skipped в
прошлых milestone'ах с `argv=NULL`) и i386 stack layout у
нового процесса. Шаг к запускa C-программ — main(argc, argv)
теперь имеет реальные значения. Ещё ~53 секунды wall-clock.

Двенадцатый — `linux_userspace_envp_milestone`. Завершает
process-startup ABI на envp-стороне. /init форкается; child
зовёт `sys_execve("/helper", argv, envp)` где
`argv = ["/helper"]` (один элемент + NULL = argc=1) и
`envp = ["KEY=VAL"]`. /helper читает `envp[0]` со стека —
с argc=1 он находится по фиксированному offset'у `[esp+0x0C]`
(layout: `argc, argv[0], NULL_terminator_argv, envp[0]`),
сохраняет в esi, и пишет 7 байт между маркерами
`[USERSPACE ENV=KEY=VAL][USERSPACE END]`. Тест декодирует и
ассертит ровно `b"KEY=VAL"`. Первый зелёный прогон:
`envp[0] returned: "KEY=VAL"`. Пинит envp половину
`copy_strings` пути execve (argv-milestone её не покрывал,
там envp=NULL) — теперь main(argc, argv, envp) полностью
доезжает с настоящими значениями всех трёх. Шаг к
работающему `getenv("PATH")` и friends — а это уже большой
кусок реального userspace'а (libc init, configure-style
программы). Ещё ~53 секунды wall-clock.

Тринадцатый — `linux_userspace_mmap_milestone`. Альтернативный
к brk механизм аллокации памяти: /init зовёт
`sys_mmap2(NULL, 0x1000, R|W, ANONYMOUS|PRIVATE, -1, 0)`
(syscall 192), записывает sentinel-байт 0x42 в первый байт
полученной страницы, потом читает его обратно, и пишет
`[USERSPACE MMAP=<addr>VAL=<byte>][USERSPACE END]`. Тест
ассертит что addr page-aligned, в userspace range
`[INIT_LOAD_ADDR, 0xC0000000)`, и прочитанный байт = 0x42.
Первый зелёный прогон: `addr = 0xB7F34000, byte = 0x42` —
адрес в типичной i386 mmap-зоне (0xB7xxxxxx, чуть ниже kernel
split'а), byte round-trip'ит ровно. Отличие от brk: mmap
выделяет отдельную VMA в произвольном месте virtual address
space'а (кернель сам выбирает адрес); brk расширяет
непрерывный с data segment'ом heap region. glibc.malloc
использует обе техники — brk для маленьких аллокаций,
mmap для больших. Третий milestone на тему процесс-памяти
после brk + brk-extend. Ещё ~54 секунды wall-clock.

Четырнадцатый — `linux_userspace_file_io_milestone`. Первый
тест с **записью в файловую систему** (а не в UART): /init
открывает `/test_file` с флагами `O_CREAT|O_WRONLY = 0x41` и
mode `0o644` через `sys_open` (syscall 5) — kernel создаёт
новый inode в tmpfs-backed rootfs. Потом `sys_write(fd1,
"TESTDATA", 8)`, `sys_close(fd1)` (syscall 6, ни разу не
тестировался раньше), `sys_open("/test_file", O_RDONLY = 0)`
возвращает новый fd2, `sys_read(fd2, buf, 8)` читает данные
обратно через ДРУГОЙ fd, `sys_close(fd2)`. /init пишет
содержимое между маркерами `[USERSPACE FILE=TESTDATA][USERSPACE
END]`. Тест декодирует 8 байт и ассертит ровно
`b"TESTDATA"`. Первый зелёный прогон:
`file content round-tripped: "TESTDATA"`. Пинит сразу 4 вещи:
writable initramfs (rootfs — это tmpfs у Linux), inode
creation через `do_filp_open` с O_CREAT (раньше открывались
только существующие файлы — /proc/version, /helper),
`sys_close` (нигде раньше не вызывался), и cross-fd
persistence через tmpfs page cache. Шаг к real программам —
любая команда вроде "запиши лог-файл" теперь работает. Ещё
~53 секунды wall-clock.

Пятнадцатый — `linux_userspace_stat_milestone`. /init сначала
создаёт `/probe` с 8 байтами "TESTDATA" (тот же путь что в
file-io milestone'е — open+write+close), потом зовёт
`sys_stat64("/probe", &statbuf)` (syscall 195). Ядро
заполняет ~96-байтовый `struct stat64` в буфере /init'а; /init
читает 4 младших байта `st_size` (offset 44 в i386 layout'е)
через `mov eax, ds:[statbuf+44]`, и пишет это значение между
маркерами `[USERSPACE STAT_SIZE=<4 bytes>][USERSPACE END]`. Тест
декодирует и ассертит ровно `st_size == 8` (мы записали 8 байт).
Первый зелёный прогон: `stat returned st_size = 8`. Пинит
inode metadata path: kernel `cp_new_stat64 → copy_to_user`
заполняет structure, layout i386 stat64 правильно
интерпретируется userspace'ом (offset 44 для st_size), и —
ключевое — kernel обновляет `inode->i_size` после write'а так
что stat показывает новый размер, а не stale 0. Шаг к
программам которые проверяют размер файла перед чтением
(cat, wc, configure-стайл утилиты). Ещё ~53 секунды wall-clock.

Шестнадцатый — `linux_userspace_lseek_milestone`. Random-access
I/O: /init создаёт `/probe` с 8 байтами "TESTDATA" (open+write
с флагами O_CREAT|O_RDWR=0x42 чтобы открыть тот же fd для
последующих read'ов), потом зовёт `sys_lseek(fd, 4, SEEK_SET=0)`
(syscall 19), читает 4 байта `sys_read(fd, buf, 4)`. По
контракту seek+read должны вернуть подстроку с offset 4 —
то есть `b"DATA"` (4-й..8-й байты "TESTDATA"). /init пишет
прочитанные 4 байта между маркерами `[USERSPACE LSEEK=DATA][USERSPACE
END]`. Тест ассертит ровно `b"DATA"` — если бы lseek не
сработал, read вернул бы первые 4 байта = `b"TEST"`. Первый
зелёный прогон: `lseek+read returned: "DATA"`. Пинит kernel'ный
`struct file` position bookkeeping — каждый fd хранит current
offset (`file->f_pos`), lseek модифицирует его. Раньше все
наши read'ы были на offset 0 (sequential), теперь random-access
работает. Шаг к программам которые seek'ают по файлу
(tar, ar, базы данных). Ещё ~53 секунды wall-clock.

Семнадцатый — `linux_userspace_dup2_milestone`. Foundation для
shell-style I/O redirection (`cmd > file`): /init открывает
`/log` через `sys_open` с O_CREAT|O_WRONLY, получает fd_log,
потом зовёт `sys_dup2(fd_log, 1)` (syscall 63) — ядро вкладывает
тот же `struct file *` что у fd_log в slot fd 1 текущего fd
table'а. Дальше `write(1, "REDIRECTED", 10)` идёт **не в
UART** (как во всех предыдущих milestone'ах), а в `/log`
через ту dup2'нутую запись. /init закрывает оба fd (fd_log и
1), потом открывает `/log` снова с O_RDONLY (получает fd 1,
lowest free), читает 10 байт, закрывает. И финал — пишет
содержимое в UART через **fd 2** (stderr → /dev/console;
впервые в milestone'ах используется fd 2): `[USERSPACE DUP2=…]
[USERSPACE END]`. Тест ассертит ровно
`b"REDIRECTED"` — если бы dup2 silently сфейлился,
write(1, ...) ушёл бы в UART (исходный fd 1), `/log` остался
бы пустым, и тест увидел бы `[USERSPACE DUP2=\0\0\0\0\0\0\0\0\0\0]
[USERSPACE END]`. Первый зелёный прогон:
`dup2 round-trip via /log: "REDIRECTED"`. Пинит kernel'ные fd
table операции, shared `struct file *` через два разных fd,
и то что fd 2 правильно set up'нут к /dev/console. Шаг к
shell-у — `cmd > file` это буквально `open + dup2 + close +
exec`. Ещё ~55 секунд wall-clock.

Восемнадцатый — `linux_userspace_unlink_milestone`. Первый
milestone, который проверяет **отказ** syscall'а как фичу
(а не успех): /init создаёт `/probe` через open+close, потом
зовёт `sys_unlink("/probe")` (syscall 10), и сразу пробует
`sys_stat64("/probe", …)`. По контракту stat **обязан**
вернуть `-ENOENT = -2` потому что файл только что удалили.
/init пишет 4 байта eax от stat'а между маркерами
`[USERSPACE UNLINKED_STAT=…][USERSPACE END]`. Тест декодирует
как little-endian u32 и ассертит ровно `0xFFFFFFFE` (= -2
sign-extended). Первый зелёный прогон:
`stat after unlink returned 0xFFFFFFFE = -2 (signed)`. Пинит
ядерный unlink path: dentry удаляется из родительского
каталога, inode'у уменьшается link count, и (так как никто его
открытым не держит) inode освобождается. И отдельно — пинит
`-errno` return convention: до этого milestone'а все syscall'ы
возвращали 0/success, неявно предполагая что kernel всегда
возвращает 0. Этот тест явно проверяет negative-result путь
через ABI eax. Шаг к программам которые проверяют существование
файлов (rm, mv, configure-style утилиты с access/stat). Ещё
~54 секунды wall-clock.
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
| Boot stall при /init binary size в "bad set" (известны 213, 600, 602, …) | неизвестный | Изначальный 17-проб-bisection 2026 года вокруг 600 нашёл sparse pair {600, 602} (вокруг: 588 ✓, 600 ✗, 601 ✓, 602 ✗, 604 ✓, 605 ✓, 608 ✓) и записал это как полный bad set. **Это была неполная диагностика.** 29 мая 2026 при добавлении `gettimeofday_milestone` оказалось, что /init размером ровно 213 байт хэнгается теми же симптомами: full kernel boot до "Trying to unpack rootfs image as initramfs...", потом ничего — не доходит до `Run /init as init process`, застревает в pata_legacy probe loop'е, budget исчерпывается за ~8.7 минут. Bad set, следовательно, шире и не локален к окрестности 600. Production /init'ы (hello=139, proc_version=417, time=211, getpid=208, gettimeofday=225 после tail-pad'а) специально подобраны вне известных bad sizes; `build_initramfs_gettimeofday` явно паддит 12 байт нулей в хвост чтобы обойти 213. Каноничные репродьюсеры: `build_initramfs_hello_padded_to_600` (бывший минимум), и теперь — `build_initramfs_gettimeofday` с обнулённым tail_pad (даёт 213). Root cause требует kernel-side трассировки (system map'а pata_legacy_init и сравнения step-counts с рабочим прогоном); инструментация для этого готова (`WWWVM_DUMP_REGIONS`, `WWWVM_STEP_BUDGET`). |
| `sys_pipe2` succeeds в isolation, но pipe round-trip ломается | средний | **Update 2026-05-29**: `linux_userspace_pipe_diag` (минимальный /init: sys_pipe2 → write eax + fds) даёт **зелёный** результат: `ret = 0x00000000 (signed: 0), fds[0] = 0, fds[1] = 4`. То есть **sys_pipe2 работает** и возвращает 0 + два валидных fd (нижний 0 для read end, fd 4 для write end — низкое значение fd 0 интересно само по себе: kernel не set up'ил stdin как /dev/console; ожидаемое поведение Linux — три fd на /dev/console, но fd 0 свободен). Тем не менее повторный milestone-attempt c полным round-trip'ом (sys_pipe2 → write(fds[1], "PIPE", 4) → read(fds[0], buf, 4) → write(2, marker+buf+marker, …)) **снова hung**: UART показывает только "PIPE" и зависание на 7.5 минут до budget exhaustion. Симптом тот же что в первой попытке — write пошёл на UART, а read заблокировался. **Гипотеза 2026-05-29**: в milestone /init'е sys_pipe2 возвращает -errno (в отличие от bare-diag /init'а где работает), скорее всего из-за разных адресов/layout у двух разных /init binary'ев. Diff'ить fds_addr и code_len между ними — TODO. Каноничный диагностический test — `linux_userspace_pipe_diag` (ignored, ~52s); рабочий round-trip milestone TODO. Не блокирует production milestones; блокирует shell pipelines (`cmd1 \| cmd2`). |
| `sys_read(stdin)` не доставляет байт после `send_input` | средний | Симметричная половина к рабочему write-пути: `vm.send_input(b"K\n")` после того, как /init блокируется в read(0, buf, 1) — ядро ЭХОИТ K обратно в UART (значит UART rx → kernel ISR → ldisc input работает), но buf остаётся нулевым; /init печатает `\0` вместо `K`. Дамп после первого probe-теста: `00003fe0: ... 4541 443f 5d4b 0d0a` (READ?]K\r\n — это echo), затем `00003ff0: ... 474f 543d 00` (GOT=\0 — это write(1, buf, 1) с пустым buf). Гипотеза: ldisc input буфер vs echo path в этом сценарии расходятся — либо ICANON-режим/dev/console не вкладывает символ в read buffer когда нет ожидающего reader'а в момент IRQ, либо есть race между sys_read и нашей ISR. Воспроизводится через test-локальный builder `build_initramfs_echo` (был добавлен в ходе investigate-tick'а 29 мая 2026, revert'нут до коммита). `WWWVM_INIT_INPUT=Q` в example linux_boot теоретически делает то же самое — но example с `initcall_debug` cmdline тянется ≥10 B шагов до /init exec (тестировано на 5 B step budget — kernel всё ещё крутится в `calling 0xc0bbf... initcall returned 0`, /init ни разу не виден). То есть быстрая cross-validation через example невозможна; integration-тест с тонким cmdline остаётся единственным разумным репродьюсером. |

Честная оценка: минимальный Linux уже грузится до userspace
(`linux_userspace_milestone` + `linux_userspace_proc_version_milestone`
+ `linux_userspace_time_milestone` + `linux_userspace_getpid_milestone`
+ `linux_userspace_gettimeofday_milestone` +
`linux_userspace_fork_milestone` + `linux_userspace_execve_milestone`
+ `linux_userspace_execve_chain_milestone` +
`linux_userspace_brk_milestone` + `linux_userspace_brk_extend_milestone`
+ `linux_userspace_argv_milestone` + `linux_userspace_envp_milestone`
+ `linux_userspace_mmap_milestone` + `linux_userspace_file_io_milestone`
+ `linux_userspace_stat_milestone` + `linux_userspace_lseek_milestone`
+ `linux_userspace_dup2_milestone` + `linux_userspace_unlink_milestone`
— см. выше), но до работающего Alpine userspace ещё дистанция
по таблице выше. Текущий цикл закрывает блокеры по одному с
тестом на каждый шаг.

## Сборка и запуск

### Хост-тесты (всегда работает)

```bash
cargo test --workspace
```

Должно вывести 572 пройденных теста на текущий момент. CI
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
