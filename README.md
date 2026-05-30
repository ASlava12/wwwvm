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
usec < 1M (sub-second clock + struct-write путь). Парный
`linux_userspace_clock_gettime_milestone` пинает `sys_clock_gettime`
(265) с CLOCK_MONOTONIC: два сэмпла подряд, тест ассертит
нормализованный tv_nsec (< 1e9), ненулевые часы (не -ENOSYS) и
ts2 ≥ ts1 (свойство монотонности). Шестой —
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
Девятнадцатый — `linux_userspace_mkdir_milestone` — /init
делает mkdir + создаёт файл в /dir/test; тест ассертит round-
trip `b"INDIR"` (kernel directory inode + multi-level path
resolution в do_filp_open). Двадцатый —
`linux_userspace_nanosleep_milestone` — /init sleep'ит 1 сек,
тест ассертит что guest wall-clock реально продвинулся
(t1 > t0); первый milestone с real wall-time, PIT IRQ + scheduler
wakeup проверены. Двадцать первый —
`linux_userspace_writev_milestone` — /init зовёт sys_writev с
двух-элементным iovec'ом; тест ищет склеенную строку в UART
(vectored I/O работает). Двадцать второй —
`linux_userspace_rename_milestone` — /init создаёт `/a`,
rename'ит в `/b`, читает `/b`; тест ассертит content
round-trip (dentry rename + inode survival). Двадцать третий —
`linux_userspace_truncate_milestone` — /init создаёт файл с
10 байтами, truncate'ит до 4, stat'ит; тест ассертит
`st_size == 4` (file-shrink path). Двадцать четвёртый —
`linux_userspace_chmod_milestone` — /init создаёт файл с
mode 0o644, chmod'ит в 0o600, stat'ит; тест ассертит
`st_mode == 0o100600` (mode-bit manipulation работает).
Двадцать пятый —
`linux_userspace_getppid_in_child_milestone` — /init форкается,
child зовёт getppid; тест ассертит `ppid == 1` (parent-child
link через real_parent в task struct). Двадцать шестой —
`linux_userspace_access_milestone` — /init access'ит файл до
и после unlink'а; тест ассертит `pre == 0` И `post == -ENOENT`
(success + provoked-failure в одном тесте). Двадцать седьмой —
`linux_userspace_statfs_milestone` — /init зовёт statfs64 на
"/"; тест ассертит `f_type == TMPFS_MAGIC` (rootfs is tmpfs).
Двадцать восьмой — `linux_userspace_fcntl_milestone` — /init
ставит FD_CLOEXEC через F_SETFD и читает обратно через
F_GETFD; тест ассертит `flags == 1` (per-fd flag storage в
kernel'ской fd table). Двадцать девятый —
`linux_userspace_sysinfo_milestone` — /init зовёт sysinfo,
читает uptime; тест ассертит uptime положительный (kernel
sysinfo struct-fill path). Тридцатый —
`linux_userspace_mprotect_milestone` — /init mmap'ит R+W,
пишет 0x42, mprotect'ит в R-only, читает обратно; тест
ассертит ret=0 и byte=0x42 (per-VMA vm_flags update).
Тридцать первый — `linux_userspace_signal_milestone` —
**первый signal milestone**: /init sigaction'ит handler на
SIGUSR1, kill'ит себя; handler пишет marker + exit'ит. Тест
ассертит marker (one-way signal delivery). Sigreturn round-
trip broken (handler `ret` → SIGSEGV) — задокументировано в
blocker table. Тридцать второй —
`linux_userspace_symlink_milestone` — /init `symlink` + `readlink`
round-trip; тест ассертит sym_ret=0, rl_ret=7, content="/target"
(S_IFLNK inode + body-read). Тридцать третий —
`linux_userspace_uname_milestone` — /init `sys_uname`, тест
ассертит sysname="Linux"; **закрывает {600,602}-сагу** —
uname (исходный hang-триггер) работает через safe-wrapper.
Тридцать четвёртый — `linux_userspace_hardlink_milestone` —
/init `sys_link("/a","/b")`, читает `/b`; тест ассертит
link_ret=0 и content="HARDLINK" (shared inode, vs symlink).
Тридцать пятый — `linux_userspace_getdents_milestone` — /init
`getdents64` на каталоге с созданным файлом; тест ассертит
n>0 и наличие имени "ZZMARKER" в буфере (directory enumeration
для ls/readdir). Тридцать шестой —
`linux_userspace_chdir_milestone` — /init mkdir+chdir+getcwd;
тест ассертит getcwd==5 и cwd=="/sub" (закрывает ложный
getcwd-блокер — это была ошибка теста, не ядра).
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

**643 теста** зелёные (mem 30 + devices 77 + cpu 391 + vm 128 +
tutorial-anchor 2 + wasm 7 + proxy 8). Снапшот v15.
CI gates: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace
--locked`. Throughput release ≈ 60–110 MIPS зависит от хоста
(x86_64 быстрее aarch64; пример печатает арку, чтобы цифры не
сравнивались случайно: `cargo run --example throughput -p wwwvm-vm
--release`). Tutorial-anchor тесты в
`crates/vm/tests/tutorial_examples.rs` пин-fиксируют hex-байты из
`docs/HAND_ASSEMBLY.md` — любое смещение между документацией и
поведением VM ловит CI.

**Аудит корректности CPU (30 мая 2026):** многоагентный adversarial-
аудит ISA-семантики против Intel SDM нашёл и исправил 5 багов (каждый
с teeth-confirmed unit-тестом): (1) SHLD/SHRD затирали CF через
`flags_logic` после вычисления → CF всегда 0; (2) LOOP/JCXZ/JECXZ
использовали 16-бит CX вместо полного ECX в 32-битном addr-mode; (3)
MOVZX/MOVSX r16,r/m16 (0x66 0F B7/BF) затирали верхнюю половину
регистра вместо preserve; (4) IDIV INT_MIN/-1 паниковал («divide with
overflow») в debug вместо #DE; (5) AF (auxiliary carry) не считался ни
одним ALU-помощником → `ADD; DAA` использовал stale AF. Отложено
(экзотика, низкий приоритет): POP [ESP]-base post-increment EA, и
не-rollback ESP при #GP на faulting POP-в-сегмент. Дубль BCD-находки
(AAA/AAS byte-carry в AH) НЕ исправлены — поведение эмулятора (раздельный
`AL±=6; AH±=1`) совпадает с реальным кремнием для valid-domain входов;
«AX±=106h» из новых SDM расходится только на out-of-domain входах.

**Второй проход — system/privileged lanes (30 мая 2026):** аудит
сегментов/дескрипторов/пейджинга/far-transfer/IRET/прерываний/system-инструкций.
Исправлено 3 реальных бага (teeth-confirmed): (1) far CALL 0x9A / RETF
0xCB/0xCA игнорировали operand-size — пушили/попали 16-бит CS:IP в
32-битном коде (тот же класс, что far-call-indirect FF/3); (2) LMSW мог
сбросить CR0.PE (тихий возврат в real mode); (3) ARPL (0x63) не
декодировался → Unimplemented/краш. ОТЛОЖЕНО как намеренные упрощения
protection-модели (ядро их не требует — flat-сегменты, валидные
дескрипторы, корректный код; добавление чеков = риск сломать boot без
выгоды): null/present/CPL-RPL-DPL чеки сегмент-загрузок, limit-enforcement,
U/S и WP пейджинг-чеки, IRET EFLAGS-маскирование/SS-reload, IDT
limit/present чеки, far-transfer privilege/gates. Не реализованы (редкие
опкоды, ядро/userspace не используют): LAR/LSL, BOUND, SMSW/SLDT/STR
32-bit zero-ext. Детали в memory `cpu-audit-deferred-findings`.

**Третий проход — атомики и bit-string (30 мая 2026):**
CMPXCHG/CMPXCHG8B/XADD/XCHG/LOCK + BT/BTS/BTR/BTC/BSF/BSR. Найдено всего
2 бага — и НИ ОДНОГО в CMPXCHG/CMPXCHG8B, bitmap-операциях
(BT/BTS/BTR/BTC с reg-индексом, адресующим дальние байты) или BSF/BSR:
атомарные + bitmap-примитивы ядра корректны. Исправлено: XADD писал dest
раньше src → вырожденный `XADD AX,AX` оставлял старое значение вместо
2×old (ядерный `LOCK XADD [mem],reg` всегда был корректен — разное
хранилище). Отложено: LOCK-префикс (0xF0) как отдельный no-op-шаг
теряет префикс, стоящий ПЕРЕД ним (`66 F0 …`, неканонический порядок —
ассемблеры так не генерят); канонический `F0 66 …` работает.

**Четвёртый проход — x87 FPU + SSE (30 мая 2026):** condition-codes,
стек, control-word, packed-arith. 12 находок, ВСЕ исправлены
(teeth-confirmed). Главная — **системная инверсия SSE**: каждый
0F-опкод с мандаторным `0x66` (COMISD/UCOMISD, ADD/MUL/SUB/DIV/MIN/MAX/SQRT
PD, MOVD/MOVDQA, весь packed-integer PADD/PSUB/PCMP/PAND/PXOR/PMUL/PSHUFD/
shifts/PMOVMSKB/…) выбирал PD-vs-PS форму по `op_size_32` (= 0x66 XOR
code_size_32), а не по литеральному префиксу. В 32-битном PM (где идёт
ВСЯ Linux-userspace SSE) это ИНВЕРТИРОВАЛО выбор: COMISD декодировался
как COMISS, MOVDQA не распознавался, packed-double читал single-полосы.
Работало только в real mode (где гоняются старые тесты). Фикс:
`Cpu::has_66()` = `op_size_32 != code_size_32` (восстанавливает «был ли
0x66» в обоих режимах), 37 SSE-сайтов переведены на него; genuine
operand-size 0F-опкоды (MOVZX/MOVSX/CMOVcc/BT/…) не тронуты. НЕ повлияло
на multi-lib busybox (та же ошибка) — значит SSE не был причиной
коррапта. Остальные x87-фиксы: DC-форма FSUB/FDIV (операнды
переставлены), FIST/FISTP (truncate→round-to-nearest по CW), FNINIT
сбрасывает TOP, FNSTSW кодирует TOP в биты 11-13, FCOM чистит C1.
Реализованы ранее НЕдекодированные (краш) compare/classify: FCOMI/FCOMIP/
FUCOMI/FUCOMIP (ставят EFLAGS — современные компиляторы их генерят для
float-сравнений), FXAM (libm-классификация), FUCOM/FUCOMP, FCOMPP,
FUCOMPP (новый 0xDA-handler).

**Пятый проход — префиксы/декод + CPUID/MSR + mode-transition
(30 мая 2026):** 6 находок, 5 исправлено (1 отложена). ГЛАВНАЯ —
**`rep ret` (F3 C3)**: дефолтный GCC function-return эпилог ~десятилетие
(обход AMD K8/K10 branch-predict), поэтому реальные i386-бинари содержат
ТЫСЯЧИ F3 C3 на горячем пути. F2/F3-handler спец-кейсил только
inner=0x0F (SSE) и 0x90 (PAUSE); всё прочее падало в string-loop, который
для не-string опкода либо ошибался (CX!=0 → Unimplemented), либо ПРОПУСКАЛ
инструкцию (CX==0 → corrupt control-flow). Фикс: string-loop только для
реальных string-опкодов (A4-A7/AA-AF/6C-6F), иначе префикс игнорируется
(rewind на байт после него). (НЕ исправил multi-lib busybox — тот же
0x6f622037-краш; значит rep ret не причина того коррапта.) Остальные:
near RET imm16 (0xC2) корректирует полный ESP (carry в верхнюю
половину); SS-load защёлкивает stack_size_32 из B-бита дескриптора;
SYSEXIT форсит RPL=3/CPL=3 на CS/SS (иначе post-sysenter userspace бежал
на CPL=0 → неверный U/S в #PF — а glibc через vDSO использует sysenter,
SEP в CPUID есть); real-mode CS/SS reload сбрасывает code/stack_size_32 в
16-бит. Отложено: LOCK-префикс (0xF0) как отдельный no-op-шаг теряет
предшествующий префикс (`66 F0 …`, неканонический порядок — ассемблеры
так не генерят).

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
  FILD/FIST/FISTP (m16/m32/m64 integer load-store), FADD/FMUL/FSUB(R)/FDIV(R) + ...P-формы, FCHS/FABS/
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
vmlinuz. Recipe для получения kernel'а — извлечь `boot/vmlinuz`
из Tinycore Core ISO (`http://tinycorelinux.net/15.x/x86/release/Core-current.iso`,
`7z e Core-current.iso boot/vmlinuz`) в `/tmp/wwwvm-linux/vmlinuz`.
Verified против Tinycore 15.x (5.18 MB сжатого vmlinuz; более
старый build был 5.85 MB — точные step-count'ы ниже зависят от
версии ядра). Запуск примера — одна команда:

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
смысл выставлять выше ~2.1 B (порог HELLO в integration test'е):
example прогоняет ядро с расширенным cmdline (`initcall_debug`),
который льёт UART-трассу на каждом initcall'е, так что USERSPACE
в example достигается заметно позже — план ~10 B. Чистый прогон без диагностики
(см. integration test ниже) занимает **~52 секунды** на той же
машине: до момента когда /init успевает напечатать HELLO,
проходит ≈2.1 миллиарда CPU-step'ов на Tinycore 15.x (на более
старом 5.85 MB build'е было ≈1.9 B — figure зависит от версии
ядра; это итерации `cpu.step()` включая idle-tick'и после HLT с
IF=1, не только retired-инструкции). В UART видна
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

Девятнадцатый — `linux_userspace_mkdir_milestone`. Directory
creation + multi-level path resolution. /init зовёт
`sys_mkdir("/dir", 0o755)` (syscall 39) — ядро создаёт directory
inode в tmpfs и attach'ит его к dentry root'а. Дальше
file-in-subdirectory: `sys_open("/dir/test", O_CREAT|O_WRONLY,
0o644)` — kernel должен пройти path "/" → найти "dir" → войти
в неё → создать новый "test". Каждый file test до этого
работал с flat-путями типа `/probe`, `/log` — этот первый
проверяет walk через два уровня. Дальше стандартный roundtrip
(write "INDIR", close, reopen RDONLY, read 5 байт, close,
write markers + 5 байт в UART). Первый зелёный прогон:
`file in /dir round-tripped: "INDIR"`. Пинит kernel
directory inode allocation, dentry attach к parent, и multi-
level path resolution в do_filp_open. Шаг к программам с
организованной FS (mkdir build/; gcc -o build/foo …). Ещё
~52 секунды wall-clock.

Двадцать первый — `linux_userspace_writev_milestone`. Vectored
I/O: /init собирает в data segment'е массив из двух `struct
iovec { void *base; size_t len }` на i386 (8 байт каждая,
итого 16 байт) — первый iovec указывает на строку
`[USERSPACE WRITEV=A` (19 байт), второй на `B][USERSPACE END]\n`
(18 байт). Потом зовёт `sys_writev(1, iov_arr, 2)` (syscall 146).
Ядро walk'ит iov массив и атомарно конкатенирует оба буфера в
UART: получается `[USERSPACE WRITEV=AB][USERSPACE END]\n`. Тест
ищет всю склеенную строку в cumulative и ассертит её
присутствие. Первый зелёный прогон: `writev milestone full
string seen after 1900000000 steps`. Пинит kernel'ский
iovec-walker write path — все предыдущие write milestone'ы
использовали один contiguous буфер. Важно для real libc'и:
glibc.printf'ы используют writev для buffered output, и shell'ы
типа bash тоже. Ещё ~52 секунды wall-clock.

Двадцать второй — `linux_userspace_rename_milestone`. File
rename в tmpfs. /init создаёт `/a` через open+write+close с
содержимым `"RENDATA"` (7 байт), потом зовёт
`sys_rename("/a", "/b")` (syscall 38) — ядро отвязывает dentry
от старого parent'а, переименовывает, привязывает к новому
parent'у (тот же родительский каталог — root). После rename
открывает `/b` для чтения, читает 7 байт, закрывает, и пишет
между маркерами `[USERSPACE RENAMED=…][USERSPACE END]`. Тест
ассертит что round-trip через `/b` равен `b"RENDATA"`. Первый
зелёный прогон: `rename round-trip via /b: "RENDATA"`. Пинит
kernel rename path: dentry-tree manipulation (detach +
reattach), inode survives (содержимое не копировалось — тот
же inode по новому пути), и старый путь больше не доступен
(если rename failed, open("/b") вернул бы -ENOENT, и read'у не
было бы что вернуть — buf остался нулевым). Шаг к atomic
file replace patterns (mv tmp/X X, configure scripts, package
managers). Ещё ~54 секунды wall-clock.

Двадцать третий — `linux_userspace_truncate_milestone`. File
shrink через `sys_truncate` (syscall 92). /init создаёт `/probe`
с 10 байтами "ABCDEFGHIJ", закрывает, потом зовёт
`sys_truncate("/probe", 4)` — ядро уменьшает inode->i_size с 10
до 4, освобождает лишние страницы page cache'а. Дальше
`sys_stat64` подтверждает: st_size = 4. /init пишет 4 байта
size'а между маркерами `[USERSPACE TRUNC_SIZE=…][USERSPACE END]`.
Тест ассертит ровно `st_size == 4`. Первый зелёный прогон:
`stat after truncate returned st_size = 4`. Пинит kernel'ский
file-shrink path (`vmtruncate → simple_setsize`), отличающийся
от write-extends-size которое мы уже проверили в stat milestone.
Шаг к log rotation, sparse files, tempfile reset. Ещё ~53 секунды
wall-clock.

Двадцать четвёртый — `linux_userspace_chmod_milestone`.
Mode-bit manipulation. /init создаёт `/probe` через open с
mode'ом 0o644, закрывает, потом зовёт `sys_chmod("/probe",
0o600)` (syscall 15) — ядро обновляет `inode->i_mode` на
новое значение через `notify_change` path. Дальше `sys_stat64`
читает `st_mode` (offset 16 в struct stat64, 4 байта). /init
пишет mode value между маркерами `[USERSPACE MODE=…]
[USERSPACE END]`. Тест ассертит ровно `st_mode == 0o100600 =
0x8180` (S_IFREG bit 0o100000 + perms 0o600). Первый зелёный
прогон: `stat after chmod returned st_mode = 0o100600 (0x8180)`.
Пинит kernel mode-update path (chmod → setattr →
notify_change → simple_setattr) AND то что stat сразу
отражает новое значение. Шаг к программам которые меняют
права (chmod CLI tool, install -m, setup scripts). Ещё
~53 секунды wall-clock.

Двадцать пятый — `linux_userspace_getppid_in_child_milestone`.
Parent-child link в task struct. /init форкается (sys_fork);
ветвление test+jnz: child идёт в одну ветку, parent — в другую.
Child зовёт `sys_getppid` (syscall 64) — kernel смотрит на
`current->real_parent->pid` — должен вернуть PID /init'а
(который всегда = 1). Child пишет 4 байта PID'а между маркерами
`[USERSPACE PARENT_PID=…][USERSPACE END]`, потом `exit(0)`.
Parent делает `sys_waitpid(-1, NULL, 0)` (чтобы дождаться
child'а и не панически выйти первым), потом `exit(0)`. Тест
декодирует 4 байта и ассертит ровно `ppid == 1`. Первый
зелёный прогон: `child's getppid returned ppid = 1`. Пинит
kernel'ский parent setup при fork'е: `copy_process` должен
установить `child->real_parent = current` (где current на
момент fork'а — /init). Если бы kernel оставил child'у
real_parent указывающий на swapper (PID 0) или kthreadd
(PID 2), тест бы поймал. Ещё ~55 секунд wall-clock.

Двадцать шестой — `linux_userspace_access_milestone`. Первый
milestone который проверяет **обе ветки** одного syscall'а в
одном /init'е — успех и provoked-failure: /init создаёт `/probe`
через open+close, потом зовёт `sys_access("/probe", F_OK=0)`
(syscall 33) — ядро должно вернуть 0 потому что файл существует.
Сохраняет eax в `pre_buf`. Потом `sys_unlink("/probe")`, и СНОВА
`sys_access("/probe", F_OK)` — теперь должен вернуть -ENOENT
(-2 sign-extended = 0xFFFFFFFE) потому что файл удалён.
Сохраняет в `post_buf`. /init пишет оба значения между маркерами
`[USERSPACE PRE=…POST=…][USERSPACE END]`. Тест декодирует оба
4-байтовых поля и ассертит ровно `pre == 0` и
`post == 0xFFFFFFFE`. Первый зелёный прогон:
`pre = 0x00000000 (0), post = 0xFFFFFFFE (-2)`. Пинит kernel
path-resolution для both contexts (success + ENOENT) в один
тест — раньше успех и failure были отдельными milestone'ами
(unlink-milestone был сам по себе failure-only, mkdir-milestone
сам по себе success-only). Полезно для shell-патернов
`if [ -e file ]; then ...` которые буквально через `access` и
проверяются. Ещё ~53 секунды wall-clock.

Двадцать седьмой — `linux_userspace_statfs_milestone`.
Filesystem metadata. /init зовёт `sys_statfs64("/", 84, &buf)`
(syscall 268) — ядро заполняет 84-байтовую `struct statfs64` с
полем `f_type` в первых 4 байтах. /init читает f_type и пишет
между маркерами `[USERSPACE FS_TYPE=…][USERSPACE END]`. Тест
ассертит ровно `f_type == 0x01021994` (TMPFS_MAGIC из
linux/magic.h) — подтверждает что rootfs действительно tmpfs.
Первый зелёный прогон: `statfs returned f_type = 0x01021994`.
**Хорошая находка**: первая попытка использовала legacy
`sys_statfs` (syscall 99) — вернула f_type = 0, скорее всего
ядро вернуло -errno и не заполнило буфер. Modern Linux
предпочитает `sys_statfs64` который явно передаёт размер
буфера. Хорошо что задокументировано — для будущих
filesystem-related milestone'ов берём statfs64. Ещё ~53 секунды
wall-clock.

Двадцать восьмой — `linux_userspace_fcntl_milestone`. Fd-flag
storage. /init открывает файл, потом зовёт
`sys_fcntl(fd, F_SETFD=2, FD_CLOEXEC=1)` (syscall 55) — ядро
ставит close-on-exec бит на эту fd-table запись. Потом
`sys_fcntl(fd, F_GETFD=1, 0)` — должен вернуть тот же бит. /init
пишет 4 байта возврата между маркерами
`[USERSPACE FD_FLAGS=…][USERSPACE END]`. Тест ассертит ровно
`flags == 1` (FD_CLOEXEC). Первый зелёный прогон:
`fcntl F_GETFD returned flags = 0x00000001`. Пинит kernel'ское
хранение fd-flag'ов в file-descriptor table entry — это
отличается от open-file flag'ов (O_CREAT, O_RDONLY и т.п.)
которые живут в самом `struct file`. fd-flag'и per-fd, open
flags per-file (shared между dup'ами). Шаг к shell'ам — они
используют FD_CLOEXEC чтобы fd'ы не утекали через execve в
child процессы. Ещё ~54 секунды wall-clock.

Двадцать девятый — `linux_userspace_sysinfo_milestone`. System
info. /init зовёт `sys_sysinfo(&buf)` (syscall 116) — ядро
заполняет 64-байтовую `struct sysinfo` с uptime, load average'ами,
total/free RAM, swap, и procs count'ом. /init читает первое
поле (`uptime` — long, 4 байта на i386) через `mov eax,
ds:[buf+0]`. Пишет 4 байта между маркерами
`[USERSPACE UPTIME=…][USERSPACE END]`. Тест ассертит uptime
положительный. Первый зелёный прогон:
`sysinfo returned uptime = 68 seconds`. Интересно: реальное
wall-clock test'а ~53 секунды, но ядерный uptime показывает
68 секунд — разница это boot time до того как /init
exec'нулся. Пинит kernel'ский sysinfo path: copy_to_user
заполняет struct, kernel timekeeping advance'ит internal
uptime counter через jiffies, и /init может прочитать любые
из 6 полей. Полезно для программ типа `uptime`, `top`, `free`
которые буквально используют sysinfo. Ещё ~54 секунды
wall-clock.

Тридцатый — `linux_userspace_mprotect_milestone`. Page
protection change. /init зовёт `sys_mmap2(NULL, 0x1000, R|W, ...)`
получает anon page, пишет байт-sentinel `0x42` в первый байт,
потом `sys_mprotect(addr, 4096, PROT_READ=1)` (syscall 125) —
ядро ловит VMA по адресу и обновляет `vm_flags`, очищая VM_WRITE.
Дальше /init читает байт обратно через `mov al, [esi]` — должен
получить 0x42 (данные не должны были потеряться через permission
change). Пишет mprotect's eax + прочитанный байт между
маркерами `[USERSPACE MPROT_RET=…BYTE=…][USERSPACE END]`. Тест
ассертит `mprotect_ret == 0 AND byte == 0x42`. Первый зелёный
прогон: `mprotect ret = 0x00000000, byte read back = 0x42`.
Пинит kernel'ский per-VMA protection state machine: mprotect
находит VMA, может splitнуть её если addr partially покрывает
существующие VMA'и, обновляет vm_flags для каждого
покрываемого VMA. Что *не* тестируется: write-after-mprotect
(должен SIGSEGV — потребует signal handling). Полезно для
JITов, garbage collector'ов, и shared library loader'ов
которые меняют permissions страниц. Ещё ~54 секунды wall-clock.

Тридцать первый — `linux_userspace_signal_milestone`. Первый
milestone с **signal handling**. /init зовёт
`sys_rt_sigaction(SIGUSR1=10, &sigact, NULL, 8)` (syscall 174)
с sigact'ом который указывает на embedded handler внутри /init
code segment'а. Потом `sys_getpid` + `sys_kill(pid, SIGUSR1)`
(syscall 37) — отправляет себе сигнал. Kernel queue'ит signal,
returnting to userspace ловит pending, set'ит up sigframe на
user stack, jumps to handler's address. Handler (embedded
функция) пишет `[USERSPACE HANDLER]\n` через write(2), потом
зовёт `sys_exit(0)` напрямую. Тест ассертит что HANDLER marker
появился. Первый зелёный прогон: `HANDLER marker after 1900000000
steps — signal delivery confirmed`.

**Sigreturn round-trip (РЕШЕНО 29 мая 2026)**: первая версия
handler'а вместо exit'а делала `ret` — pop'нуть kernel-stashed
restorer address со stack'а, перейти в restorer (sys_rt_sigreturn),
ucontext restored, main resume'ит после kill'а. Сначала это
segfault'ило (`exitcode=0x0b`, main не resume'ил), и было записано
как блокер. При расследовании сырой дамп signal frame'а
(`linux_userspace_sigframe_dump_diag`) показал, что **эмулятор
корректен**: kernel сохранил полный контекст (saved eip=0x0804808a,
esp, все GP-регистры), просто frame был **legacy `sigframe`**, а не
`rt_sigframe`, потому что handler регистрировался без `SA_SIGINFO`.
Restorer звал `rt_sigreturn`, который читает контекст с offset'а
rt-frame'а → нули → segfault. Фикс — выставить `SA_SIGINFO`, чтобы
frame совпал с restorer'ом. Теперь полный round-trip работает:
HANDLER → ret → restorer → rt_sigreturn → main resume'ит → DONE →
clean exit. Покрыто `linux_userspace_sigreturn_milestone` (round-trip)
И `linux_userspace_signal_milestone` (одностороннее delivery,
handler сразу exit'ит — минимальный пин для Ctrl-C-паттернов).
Reentrant handler'ы (shell job control) теперь работают. Ещё ~53
секунды wall-clock.

Тридцать второй — `linux_userspace_symlink_milestone`. Symlink
create + read. /init зовёт `sys_symlink("/target", "/link")`
(syscall 83; ebx=oldname=target, ecx=newname=link) — ядро
создаёт symlink inode (S_IFLNK) в tmpfs чьё тело хранит строку
target'а. Потом `sys_readlink("/link", buf, 32)` (syscall 85) —
читает тело symlink'а обратно в буфер БЕЗ follow'а ссылки.
/init пишет три значения между маркерами:
`[USERSPACE SYM=<sym_ret>RL=<rl_ret>LINK=<7 bytes>][USERSPACE END]`.
Тест ассертит `sym_ret == 0`, `rl_ret == 7` (длина "/target"),
и содержимое буфера == `b"/target"`. Первый зелёный прогон:
`sys_symlink ret = 0, sys_readlink ret = 7, link bytes = "/target"`.
**Process discovery**: первая версия builder'а (один marker
`[USERSPACE LINK=`, без захвата return-кодов) фейлилась под
10-thread re-verify'ем — readlink буфер выходил нулевым. После
реворка с захватом обоих syscall-return'ов milestone стабильно
проходит и alone, и под 10 threads — значит это был баг в старом
builder'е (точная причина zero-буфера в старом layout'е не
изолирована — static analysis старого кода выглядел корректным;
реворк и фиксит, и добавляет диагностическую ценность). Пинит
symlink inode creation + readlink body-read. Полезно для
программ работающих с симлинками (ls -l, package managers,
/etc/alternatives). Ещё ~55 секунд wall-clock.

Тридцать третий — `linux_userspace_uname_milestone`. /init
зовёт `sys_uname(&buf)` (syscall 122) — ядро заполняет
`struct new_utsname` (6 полей по 65 байт: sysname, nodename,
release, version, machine, domainname), /init печатает первое
поле `sysname[0..65]` между маркерами `[USERSPACE UNAME=…]
[USERSPACE END]`. Тест ассертит что sysname начинается с
"Linux". Первый зелёный прогон: `sysname="Linux" confirmed`.
**Закрывает {600,602}-сагу**: `sys_uname` был самым первым
триггером того боль-стояния — оригинальный `build_initramfs_uname`
(который намеренно landed at exactly 600 bytes через raw
make_init_elf32) запускал kernel-hang в pata_legacy probe и
породил весь 17-проб bisection. Этот milestone использует
`build_initramfs_uname_safe` через `make_init_elf32_safe`
(binary выходит 595 байт — вне bad set'а), доказывая что
safety-wrapper нейтрализует исторический hang: uname теперь
работает как production milestone. Полезно для программ
читающих kernel version (uname -a, configure-скрипты,
package compatibility checks). Ещё ~56 секунд wall-clock.

Тридцать четвёртый — `linux_userspace_hardlink_milestone`. /init
создаёт `/a` с содержимым "HARDLINK", потом
`sys_link("/a", "/b")` (syscall 9; ebx=oldname, ecx=newname) —
ядро создаёт ВТОРОЙ dentry `/b`, указывающий на ТОТ ЖЕ inode
что у `/a` (i_nlink инкрементится), без копирования данных.
Дальше открывает `/b` на чтение и читает 8 байт. /init пишет
link_ret + content между маркерами `[USERSPACE LINK_RET=…DATA=…]
[USERSPACE END]`. Тест ассертит `link_ret == 0` и content ==
`b"HARDLINK"` — доказывая что `/b` шарит `/a`'s inode (те же
data-блоки видны через оба пути). Первый зелёный прогон:
`sys_link ret = 0, content via /b = "HARDLINK"`. Отличие от
symlink-milestone'а: hard link — это второй dentry к ОДНОМУ
inode (vs symlink = отдельный inode с path-телом). Полезно для
программ использующих hard link'и (tar/cpio extraction с
дедупликацией, `cp -l`, `ln`). Ещё ~55 секунд wall-clock.

Тридцать пятый — `linux_userspace_getdents_milestone`. Directory
enumeration — primitive для `ls`/`readdir`. /init делает
`sys_mkdir("/d")`, создаёт `/d/ZZMARKER` (open+close), открывает
`/d` с `O_RDONLY|O_DIRECTORY` (0x10000), и зовёт
`sys_getdents64(dirfd, buf, 512)` (syscall 220). Ядро walk'ит
children dir-inode'а и сериализует `struct linux_dirent64`
записи (`{u64 d_ino, s64 d_off, u16 d_reclen, u8 d_type, char
d_name[]}`) — по одной на ".", "..", "ZZMARKER" — в буфер.
/init пишет n (byte count из eax) + сам буфер между маркерами
`[USERSPACE DENTS=<n>BUF=<buf>][USERSPACE END]`. Тест ассертит
`n > 0` и что в буфере присутствует строка `b"ZZMARKER"`
(NUL-terminated d_name нашего файла). Первый зелёный прогон:
`getdents64 returned n = 80 bytes; ZZMARKER present = true` —
80 байт это ровно 3 entry'я (., .., ZZMARKER) с их dirent
заголовками. Пинит directory enumeration end-to-end —
kernel'ский readdir путь, который нужен любому `ls`, shell
glob'у, `find`, `opendir/readdir`-based коду. Ещё ~55 секунд
wall-clock.

Тридцать шестой — `linux_userspace_chdir_milestone`. /init
делает `sys_mkdir("/sub")` (39) → `sys_chdir("/sub")` (12) →
`sys_getcwd(buf, 32)` (183), захватывая все три return-кода +
cwd-буфер между маркерами
`[USERSPACE MKDIR=…CHDIR=…GETCWD=…PWD=<8>][USERSPACE END]`.
Тест ассертит mkdir==0, chdir==0, getcwd==5 (длина "/sub\0"),
и cwd начинается с "/sub". Зелёный прогон:
`getcwd ret = 5, cwd buf = "/sub\0\0\0\0"`. **Закрывает
ложный getcwd-блокер**: раньше в blocker table значилось
"sys_getcwd возвращает 0 (не в контракте)" — но root-cause
2026-05-29 показал что это была ошибка тест-харнесса, а не
ядра. getcwd работает идеально (возвращает 5, заполняет
"/sub"). Первая ложная "0"-проба читала неправильный buffer
offset; вторая ложная проба искала маркер "CWD=", который
**подстрока** "GETCWD=" — `position()` находил его внутри
GETCWD= и читал getcwd-return (5) вместо самого буфера. С
distinct-маркером "PWD=" цепочка читается чисто. Урок:
sub-string-коллизии маркеров — коварный класс багов в этих
UART-scraping тестах. Пинит chdir (`fs->pwd` update) +
getcwd (dentry-tree walk → pathname build → copy_to_user).
Полезно для cd, bash $PWD, любого getcwd-based path
resolution. Ещё ~55 секунд wall-clock.

Двадцатый — `linux_userspace_nanosleep_milestone`. Первый
milestone с **реальным течением guest wall-clock time'а**: /init
зовёт `sys_time(NULL)` (сохраняет t0), потом
`sys_nanosleep(&ts={1, 0}, NULL)` (syscall 162; struct timespec
с tv_sec=1, tv_nsec=0), потом снова `sys_time` (сохраняет t1).
Пишет оба значения как 4-байт LE u32 между маркерами
`[USERSPACE T0=…T1=…][USERSPACE END]`. Тест ассертит `t1 > t0` —
то есть прошло как минимум 1 секунда guest wall-clock'а. Первый
зелёный прогон: `t0 = 1577836809, t1 = 1577836810, delta = 1
seconds`. Пинит PIT/LAPIC IRQ → jiffies counter → timer wheel
→ scheduler wakeup пути end-to-end. До этого все milestone'ы
работали "instant" guest time (не было реального sleep'а).
Шаг к программам которые sleep'ят (sh sleep N, watch N, cron,
любой polling loop).

**Скрытая находка**: тест **сначала упал** с подозрительными
значениями `t1 = 199297549, delta = 2916428036`. Диагноз:
ядерный TTY ONLCR-translate'ит каждый `\n` в `\r\n`, а в
t1's бинарной репрезентации (0x5E0BE10A = 1577836810) есть
байт 0x0A. Kernel вставил перед ним 0x0D, сдвинув 4-байтовое
окно decoder'а на 1 байт. Получился decoded u32 0x0BE10A0D
= 199297549. Фикс — strip'ить `\r` перед `\n` при extract'е
binary u32 из UART cumulative'а. Этот паттерн теперь baked в
test и реусабельный для будущих binary-data milestone'ов. Ещё
~52 секунды wall-clock.
Запуск: `WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz cargo test
--release --test linux_userspace -- --ignored`. Если файла нет —
тест silently skip'ается; на CI без vmlinuz просто пропустит.

Что нужно vmlinuz: положите его в `/tmp/wwwvm-linux/vmlinuz` или
укажите путь через `WWWVM_KERNEL=...`. Tinycore Core ISO извлекает
vmlinuz прямо из `boot/vmlinuz`.

**Запуск настоящего скомпилированного бинаря.**
`linux_userspace_real_static_binary_milestone` грузит как /init
реальный статически слинкованный glibc-бинарь (не hand-assembled
syscall-стаб). Путь берётся из `WWWVM_STATIC_INIT` (по умолчанию
`/tmp/wwwvm-linux/static-ldconfig`); нет файла — тест skip'ается.
Извлечь статический `ldconfig` из того же Tinycore ISO:
`7z e Core-current.iso boot/core.gz && zcat core.gz | cpio -id
sbin/ldconfig && cp sbin/ldconfig /tmp/wwwvm-linux/static-ldconfig`.
Это интеграционный тест всего стека сразу: ELF-лоадер ядра на
настоящем ET_EXEC, glibc static CRT (`__libc_start_main`), TLS через
`set_thread_area`, brk/mmap под malloc-арену, реальные fs-syscall'ы —
ldconfig сканирует (отсутствующие) lib-директории, печатает свои
настоящие `skipping /lib …` и `exit(0)`'ит. Доказывает, что эмулятор
исполняет настоящий compiled+linked Linux-софт.

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

## Linux userspace (busybox через ld.so) — проверенные возможности

**Multi-library динамическая линковка работает end-to-end.** Реальный
glibc-бинарь (busybox), тянущий ТРИ shared object'а (libc + libm +
libcrypt) через `/lib/ld-linux.so.2`, грузится, релоцируется и
запускается; поверх него работает настоящий usable userspace. Все
milestone'ы ниже — **asserting** (не diag), `#[ignore]`, требуют
`WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz` +
`WWWVM_DYN_ROOTFS=/tmp/wwwvm-linux/rootfs` (busybox + ld.so + 3 либы),
~57s каждый. Каждый печатает уникальный маркер, который появляется ТОЛЬКО
если соответствующий путь отработал корректно:

| Milestone | Маркер | Что доказывает |
|-----------|--------|----------------|
| `dynamic_multilib_milestone` | `DYNLINK_OK` | execve busybox; ядро + ld.so mmap'ят libc+libm+libcrypt, relocation, чистый `exit(0)` |
| `busybox_sh_milestone` | `SHELL_OK` | `sh -c`: шелл парсит командную строку, гоняет builtin |
| `busybox_sh_fork_exec_milestone` | `FORK_OK` | `fork()+execve()` второй динамической копии busybox (свежий ld.so-цикл) |
| `busybox_pipeline_milestone` | `PIPE_OK` | `echo … \| busybox cat`: `pipe()+fork()+dup2()` обеих стадий |
| `busybox_awk_milestone` | `AWK_OK_42` | awk-интерпретатор + x87 double-арифметика (6*7=42) + number→string. **Вскрыл и починил баг DF m64 x87** (см. ниже) |
| `busybox_file_io_milestone` | `FILE_OK` | `open(O_CREAT)`/write/read/close на реальном пути в tmpfs |
| `busybox_sed_milestone` | `SED_OK_123` | sed компилирует BRE `[a-z][a-z]*` + подстановка (regex-движок) |
| `busybox_ls_milestone` | `LSMARK_FILE` | `ls -l`: `getdents64` + `lstat` + форматирование mode/size/mtime |
| `busybox_shell_arith_milestone` | `SHELL_SUM_10` | shell-скриптинг: `for`-цикл + `$(())` арифметика + `if [ -eq ]` (валидирует и корректность суммы) |
| `busybox_gzip_milestone` | `GZIP_RT_OK` | `gzip\|gunzip` round-trip: DEFLATE + CRC32-верификация (bit-manipulation) |
| `busybox_signal_milestone` | `TRAP_OK` | доставка сигналов end-to-end: `rt_sigaction` → frame на user-стеке → handler → `rt_sigreturn` |
| `busybox_md5sum_milestone` | `dacfc4…526b9` | bit-exact MD5 против хоста (ROL/add ALU-путь корректен до бита) |
| `busybox_jobcontrol_milestone` | `ALL_REAPED` | конкурентность: 3 фоновых fork/exec + один `wait` reap'ит всех через wait4 |
| `busybox_sort_stress_milestone` | `SORTED_MAX_20000` | память под нагрузкой: `sort` буферит 20k строк (~120 KB) + полная сортировка |
| `busybox_interactive_milestone` | `INTERACTIVE_OK` | интерактивный ввод с tty: `send_input` → UART RX → IRQ → line discipline → shell `read()` |
| `busybox_interactive_session_milestone` | `PROD_42` | интерактивная СЕССИЯ: 3 команды через отдельные `read()`, состояние переменных persist'ит (`n=7;m=6;echo PROD_$((n*m))`) |
| `busybox_libm_milestone` | `LIBM_699` | awk libm-математика: sin/cos/exp/log/atan2/sqrt (софтверный x87-полином) численно корректны (×100, см. оговорки ниже) |
| `busybox_tar_milestone` | `TAR_RT_OK` | tar create→extract round-trip: ustar-заголовок (octal-поля + checksum) + 512-байт блоки; оригинал удалён, маркер только из архива |
| `busybox_stats_milestone` | `STAT_833` | sustained FP: awk считает дисперсию 100 чисел из pipe (`s+=x; q+=x*x; q/N - (s/N)^2`) = 833.25 → 833 — accumulate/mul/div/sub в double |
| `busybox_grep_milestone` | `GREP_HIT_42` | grep compile BRE `HIT_[0-9]*` + line-filter из pipe (matching строка проходит, skipme отфильтрованы) |
| `busybox_fifo_milestone` | `FIFO_OK` | named pipe: `mkfifo /f`, фоновый writer + foreground `cat` рандеву'ятся через FIFO (mknod S_IFIFO + open-blocking + cross-process transfer) |

**Корневая причина (была):** **SYSEXIT CPL=3 баг**. CPUID рекламирует
SEP → glibc идёт через vDSO `sysenter`; SYSEXIT возвращался с RPL=0, так
что после КАЖДОГО syscall'а userspace бежал на CPL=0 → U/S-бит в #PF был
0 (supervisor) → `do_page_fault` неверно обрабатывал demand-paging
busybox'а (3 либы demand-page'ят НАМНОГО больше, чем крошечный single-lib
rotdash — поэтому single-lib работал, а multi-lib нет) → corrupt
control-flow / saved-EIP (симптом: saved EIP становился ASCII-строкой
`0x6f622037`/`0x6f622061`). Форсинг RPL=3 на SYSEXIT всё починил. Долгий
fault-trace/watchpoint трейл расследования (wild-jumps в строки) был
ложным следом — он был downstream от U/S-коррапта; см. git history +
memory `multilib-dynamic-linking-state.md`.

**CPU-баги, вскрытые этими workload'ами:** awk вызвал `unimplemented
opcode 0xDF` — x87 **DF memory-формы** (`FILD/FIST/FISTP m16/m64`,
int↔double-конверсия), которых не было в декодере (echo/cat/sh/pipeline
их не трогают). Реализованы DF /0 /2 /3 /5 /7 + 2 teeth-confirmed
unit-теста. Урок: реальные workload'ы ходят по путям, которых нет у
простых applet'ов — стресс-тестирование настоящими программами находит
дыры, недостижимые синтетикой.

**FIFO-milestone → /dev/null → FIST m32 (цепочка находок):** named-pipe
milestone завис; причина — в minimal initramfs не было `/dev/null`, а ash
редиректит stdin фонового job'а туда, и subshell `(echo>/f)&` падал →
writer не открывал FIFO → cat блокировался навечно. Добавили `/dev/null` +
`/dev/zero` (CHR 1:3 / 1:5) в cpio. Это вскрыло латентный баг: с рабочим
backgrounding'ом busybox `sleep`'s float→int парсинг (strtod) дошёл до
`FIST m32` (DB /2 — store-без-pop), которого не было в декодере (были /0 /3
/5 /7) → `unimplemented opcode 0xDB`. Реализованы DB /2 (FIST) и /1 (FISTTP,
SSE3) + teeth-confirmed `fpu_fist_m32_stores_without_pop`. NB: jobcontrol-
milestone раньше проходил по НЕВЕРНОЙ причине (сломанный backgrounding без
/dev/null обходил FP-путь); теперь — по верной.

**Проактивный аудит декодера (workflow):** дизассемблировали РЕАЛЬНЫЕ
busybox/libc/libm/ld.so/libcrypt через `llvm-objdump`, извлекли все
редкие инструкции, которые они используют, и сверили с декодером — чтобы
найти латентные unimplemented-краши ДО того, как на них наткнётся
workload. Нашли и реализовали: x87 **DA memory integer-arith**
(`FIADD/FIMUL/FICOM/FICOMP/FISUB/FISUBR/FIDIV/FIDIVR m32int` — есть прямо
в busybox), **FLDENV/FNSTENV** (D9 /4,/6 — glibc `feholdexcept`/`fesetenv`
вокруг libm-математики), и **FCMOVcc/FCMOVNcc** (DA/DB регистровые формы —
libm `fcmove`). 5 teeth-confirmed unit-тестов. Низкорисковые находки
(TSX `xbegin`/`xend`, PKU `rdpkru`, `xgetbv`) оставлены unimplemented —
они CPUID-gated cold (leaf 7/OSXSAVE отключены, glibc идёт по fallback).

**FXSAVE/FXRSTOR (0F AE /0,/1) были заглушками** (FXSAVE писал нули,
FXRSTOR молча отбрасывал) — теперь реально сохраняют/восстанавливают x87
(ST/TOP/CW/SW, 80-бит) + XMM0-7. teeth-confirmed round-trip unit-тест.

**✅ РЕШЕНО — FP-rollback-on-fault (баг «первого libm-вызова»).** Симптом
был: ПЕРВЫЙ вызов трансцендентной libm-функции тихо возвращал ~0. Полный
x87-трейс (env `WWWVM_X87_TRACE`) на минимальном репро `awk 'BEGIN{x=sin(1);
y=sin(1)}'` (→ первый sin=0, второй=0.8414) показал: sin грузит полиномиальные
константы через `FLD m64` (DD /0) из `.rodata`, которая **demand-paged** —
на ПЕРВОМ доступе страница ещё не отображена → `FLD m64` берёт #PF. Модель
continue-on-fault чекпойнтила GP-регистры+флаги для отката, но **НЕ x87-стек
/ XMM** → faulting FLD пушил мусор (`3.39e-305`), который не откатывался, и
EIP-rewind retry пушил ВТОРОЕ значение на сбитый стек → дальше всё считалось
из мусора. **Фикс:** чекпойнт/откат `fpu_top/fpu_st/fpu_sw/fpu_cw + xmm` для
FP-опкодов (D8-DF, 0F, **и F2/F3-префиксный scalar SSE** — проактивно
доаудитил: `MULSD/DIVSD xmm,[mem]` декодируются с opcode F2, не покрывались
0F-гейтом; faulting `a*0=0` затирал оригинал → retry считал `0*correct`;
teeth-confirmed `sse_xmm_rolls_back_on_faulting_mulsd`). teeth-confirmed unit-тест
`fp_stack_rolls_back_on_faulting_fld_m64` (без фикса fpu_top=6 вместо 7) +
workload (RES_0→RES_8414). Затрагивает ЛЮБОЙ FP-код, грузящий константу с
ещё-не-отображённой страницы. `busybox_libm_milestone` теперь БЕЗ warmup
проверяет все 6 трансцендентных (×100 — эмулированный x87 хранит регистры
как f64, а не 80-бит, ~5-я значащая цифра отличается). FXSAVE/fstpt-fldt/MMX
были исключены трейсами по пути. Историю расследования см. memory
`sin-silent-miscompute-bug.md`.

## Что НЕ работает (дорожная карта к Alpine)

Между «грузит userspace из minimal initramfs» и «грузит Alpine» —
ещё дистанция. Крупные оставшиеся блокеры, по приоритету:

| Блокер | Объём | Зачем |
|--------|-------|-------|
| ✅ **РЕШЕНО** — Динамическая линковка (`ld.so`): multi-lib glibc работает | — | **✅ Работает end-to-end.** См. раздел «**Linux userspace (busybox через ld.so) — проверенные возможности**» выше: 15 asserting-milestone'ов от `DYNLINK_OK` до интерактивного шелла. Корневая причина была SYSEXIT CPL=3; исторический трейл расследования — в git history + memory `multilib-dynamic-linking-state.md`. |
| x87 расширения — настоящая 80-битная точность ✅ + FPU-исключения (#MF) ⏳ | малый | **80-битный x87 РЕАЛИЗОВАН (30 мая 2026):** x87-стек теперь хранит настоящие 80-битные значения (`crates/cpu/src/f80.rs` — soft-float `F80` с 64-битной мантиссой; арифметика на u128, round-nearest-even), а не f64. Это починило `busybox printf '%.17g' 0.1` (давал `0.099999999999994315`, теперь корректное `0.10000000000000001`; π → `3.1415926535897931`) — musl форматирует float'ы через `long double`, и на f64-стеке (53 бита) его dtoa терял ~11 бит. Корень был именно в точности модели, НЕ в опкодах (каждая x87-операция была бит-точна на f64 — пинит тест `fpu_dtoa_path_ops_are_individually_correct`). Milestone `linux_userspace_alpine_printf_dtoa_milestone` ассертит точный вывод. FXTRACT/FSCALE теперь точные (через поле экспоненты F80). Трансцендентные (FSIN/FCOS/FYL2X/…) и FSQRT пока считаются в f64 → промоутятся в F80 (second-tier, не на dtoa-пути). Осталось: FPU-исключения (#MF, маски в CW) и 80-битные точные FSQRT/трансцендентные. |
| MMX-стек (mm0..mm7, EMMS, packed-int MMX-only), помарки в SSE3+ (HADDPS/HADDPD, MOVDDUP, LDDQU) | средний | SSE2 готов в практическом смысле; Alpine ≥3.x линкуется именно с ним. MMX совершенно отдельный регистровый стек — линукс почти не пользуется в современном коде |
| Real-mode setup execution (~16 KiB Linux boot-ASM) | очень большой | bzImage сам делает PE-переход — нужно выполнить его setup-код |
| Kernel decompression (gzip/zstd) | средний | bzImage payload сжат; либо распаковывать, либо грузить vmlinux |
| Ring 3 + полноценный TSS + privilege transitions | малый | Cross-ring INT/IRET, syscall round-trip (IRETD→user→INT→handler), cross-ring #PF — всё работает. CPL=0 guards стоят на HLT/CLI/STI/IN/OUT (IOPL), LLDT/LTR/LGDT/LIDT/LMSW/INVLPG, INVD/WBINVD, MOV CR/DR, RDMSR/WRMSR/SYSEXIT, CLTS, RDPMC (через CR4.PCE). Остаётся: per-port IO permission bitmap в TSS |
| Полный #DF / #NP / #SS | средний | #DE, #UD, #PF и весь основной #GP набор уже доезжают; #DF/#NP/#SS — ещё нет |
| IDE/ATA DMA / virtio-blk | средний | Оба канала (primary + secondary) read+write через PIO уже работают; для модерн дистров нужно ещё DMA |
| HPET таймер-IRQ / реалистичный PIT-тайминг | малый | LAPIC периодический таймер уже доставляет; HPET — только probe-stub без доставки. Linux в большинстве конфигов берёт LAPIC, так что HPET-доставка — second-tier. |
| ne2k/virtio-net + slirp поверх `crates/proxy` | средний | Сеть из гостя |
| VGA graphics, framebuffer | средний | fbcon, графические гости |
| ✅ **РЕШЕНО (30 мая 2026)** — boot stall при /init size {213,600,602} | — | Старое: на прежнем ядре /init размером ровно 213/600/602 байт хэнгался в pata_legacy probe loop, не доходя до `Run /init as init process`. **Re-тест 30 мая (`linux_userspace_bad_init_sizes_diag`): все три размера ГРУЗЯТСЯ чисто** (HELLO на 2.1 B шагов каждый) на Tinycore 15.x kernel'е — с недавними CPU-фиксами (reg-rollback-on-fault, far-call-width). Stall больше не воспроизводится. `KNOWN_BAD_INIT_SIZES` опустошён (dodge в `make_init_elf32_safe` стал no-op passthrough, готов к ре-армингу если размер когда-нибудь регрессирует); `bad_init_sizes_diag` оставлен как regression-guard. Вероятно тот же класс багов (faulting-instruction reg corruption / far-call), что чинились для ld.so, проявлялся в pata_legacy probe при определённых раскладках памяти. |
| ✅ **ИСПРАВЛЕНО (29 мая 2026)** — `sys_rt_sigreturn` «segfault'ил» после handler return (это был **баг теста**, не эмулятора) | — | Записывалось как «sigreturn segfault'ит, exitcode=0x0b». Расследование 29 мая 2026 (через сырой дамп signal frame'а — `linux_userspace_sigframe_dump_diag`) показало: **эмулятор всё делает правильно**, баг был в тесте. Дамп frame'а доказал, что kernel'ский `setup_rt_frame` корректно сохранил ВЕСЬ контекст: pretcode=restorer, sig=10, saved eip=0x0804808a, esp=0xbf9cd6f0, ebx=1 (pid init), ecx=10, eax=0 (kill ret), cs=0x73, eflags=0x244, ss=0x7b — всё на месте. НО layout оказался **legacy `sigframe`** (sigcontext сразу после `sig`, на offset +8), а НЕ `rt_sigframe`. Причина: handler регистрировался с `sa_flags = SA_RESTORER` БЕЗ `SA_SIGINFO`, поэтому kernel строит legacy frame; а наш restorer звал `rt_sigreturn(173)`, который читает `uc.uc_mcontext` с offset'а rt-frame'а (+164) — в legacy frame'е там нули → restore'ится eip=0/esp=0 → `segfault at 0 ip 0 sp 0` → SIGSEGV. **Фикс:** выставить `SA_SIGINFO` (`sa_flags = 0x04000004`), чтобы kernel построил rt_sigframe, совпадающий с rt_sigreturn-restorer'ом (так и делает glibc: `__restore_rt` для SA_SIGINFO, `__restore` для legacy). После фикса полный round-trip работает: HANDLER → ret → restorer → rt_sigreturn → main resume'ит → `[USERSPACE DONE]` → clean exit (exitcode=0). **Регрессионный guard:** `linux_userspace_sigreturn_milestone` (asserts handler ran + НЕ segv + DONE). Reentrant signal handlers (shell job control, Ctrl-C resume) теперь работают. |
| ✅ **ИСПРАВЛЕНО (29 мая 2026)** — `copy_to_user` в нетронутую user-страницу терял данные (был: «`sys_pipe2` populate fd-пары layout-зависим») | — | **Root cause (CPU-баг, не kernel и не layout):** прошлая гипотеза «layout/address-зависимая аномалия» оказалась НЕВЕРНОЙ. Реальная причина в эмуляторе CPU. `copy_to_user` ядра делает `rep movsl`; когда destination — свежая COW / demand-zero user-страница, **первая** запись брала #PF. Модель page-fault'а в `step()` дописывает faulting-доступ как запись в физический 0, ставит `pending_fault`, и на следующем шаге диспатчит #PF, перематывая EIP на `last_op_ip` (чтобы инструкция переисполнилась после IRETD). **Но REP-цикл не откатывал частичную итерацию**: он гнал ECX→0, записывая всё в phys 0, и только потом ловил #PF. Перемотанный retry находил ECX==0 → копировал НОЛЬ байт. Итог: `pipe2` возвращал 0, но `fds` оставались `[0,0]`; round-trip молча падал (fd 0 = bidirectional /dev/console → read блокировался). **Подтверждение:** pre-touch эксперимент (записать байт в fds-страницу ДО pipe2, сделав её present) чинил populate → fds=[3,4], BUF="PIPE". **Фикс** (`crates/cpu/src/lib.rs`): и в REP-цикле, и в single-shot string-op пути снимаем снапшот ESI/EDI перед итерацией; если после `step_string` выставлен `pending_fault` — восстанавливаем ESI/EDI и выходим из цикла **до** декремента ECX. После того как #PF-handler (`do_wp_page`) маппит страницу, IRETD возвращается в тот же REP с тем же ECX и докопирует данные. Затрагивает ВСЕ пути через `copy_to_user`/`copy_from_user`/`memcpy`/`memset` в faulting-страницы. **Регрессионный guard:** `linux_userspace_pipe_milestone` (полный round-trip pipe2→write→read, asserts fds=[3,4], write=4, read=4, BUF="PIPE"). Диагностики оставлены: `linux_userspace_pipe_diag` + `linux_userspace_pipe_rt_diag`. |
| ✅ **ИСПРАВЛЕНО (29 мая 2026)** — `sys_read(stdin)` не доставлял байт после `send_input` | — | **Это была ТА ЖЕ ошибка, что и pipe-блокер выше** (REP-string #PF rollback), просто на стороне *доставки*. Симптом: `vm.send_input(b"K\n")` после того, как /init блокируется в `read(0, buf, 1)` — ядро ЭХОИЛО K обратно в UART (UART rx → ISR → ldisc input работал), но `buf` оставался нулевым; /init печатал `\0`. Это сбивало с толку («echo есть, а доставки нет»), но root cause тот же: `read`'s `copy_to_user` пишет принятый байт в `buf`, лежащий на свежей demand-zero странице → первая запись брала #PF → старый REP-цикл гнал ECX→0 в phys 0 → перемотанный retry находил ECX==0 и не копировал ничего. Echo работало, потому что оно идёт через UART tx (`copy_from_user` из *present* marker-страниц), а не через faulting destination. **Тот же фикс в `crates/cpu/src/lib.rs` чинит и это.** Проверено: `read(0, buf, 4)` после `send_input(b"KICK\n")` теперь возвращает 4 и `buf == "KICK"`. **Регрессионный guard:** `linux_userspace_read_stdin_milestone` (блокируется в read, инжектит строку, asserts ret=4 + buf="KICK"). |

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
+ `linux_userspace_mkdir_milestone` + `linux_userspace_nanosleep_milestone`
+ `linux_userspace_writev_milestone` + `linux_userspace_rename_milestone`
+ `linux_userspace_truncate_milestone` + `linux_userspace_chmod_milestone`
+ `linux_userspace_getppid_in_child_milestone` +
`linux_userspace_access_milestone` + `linux_userspace_statfs_milestone`
+ `linux_userspace_fcntl_milestone` + `linux_userspace_sysinfo_milestone`
+ `linux_userspace_mprotect_milestone` + `linux_userspace_signal_milestone`
+ `linux_userspace_symlink_milestone` + `linux_userspace_uname_milestone`
+ `linux_userspace_hardlink_milestone` + `linux_userspace_getdents_milestone`
+ `linux_userspace_chdir_milestone` +
`linux_userspace_pipe_milestone` +
`linux_userspace_read_stdin_milestone` +
`linux_userspace_sigreturn_milestone` +
`linux_userspace_wait_status_milestone` +
`linux_userspace_poll_pipe_milestone` +
`linux_userspace_pipeline_milestone` +
`linux_userspace_file_mmap_milestone` +
`linux_userspace_exec_mmap_milestone` +
`linux_userspace_set_thread_area_milestone` +
`linux_userspace_shared_mmap_milestone` +
`linux_userspace_futex_milestone` +
`linux_userspace_clone_thread_milestone` +
`linux_userspace_socketpair_milestone` +
`linux_userspace_epoll_milestone` +
`linux_userspace_eventfd_milestone` +
`linux_userspace_file_mmap_offset_milestone` +
`linux_userspace_real_static_binary_milestone` (★ настоящий
скомпилированный glibc-бинарь — статический `ldconfig` из Tinycore
rootfs — грузится ELF-лоадером, проходит glibc CRT + TLS, печатает
свой реальный вывод и чисто `exit(0)`'ит) — см. выше; все production
milestone'ы re-verified зелёными на Tinycore 15.x kernel'е при
10-thread параллелизме), но до работающего Alpine userspace ещё
дистанция по таблице выше. Текущий цикл закрывает блокеры по
одному с тестом на каждый шаг.

## Сборка и запуск

### Хост-тесты (всегда работает)

```bash
cargo test --workspace
```

Должно вывести 643 пройденных теста на текущий момент. CI
(`.github/workflows/ci.yml`) дополнительно гоняет `cargo fmt --check`
и `cargo clippy --workspace --all-targets -- -D warnings`.

### Живой интерактивный busybox-шелл (печатать команды вживую)

```bash
# нужны ассеты: /tmp/wwwvm-linux/vmlinuz + /tmp/wwwvm-linux/rootfs
cargo run -p wwwvm-vm --release --example busybox_console
```

Грузит настоящее ядро Linux i386 + динамически слинкованный glibc
`busybox sh` и соединяет твой терминал (stdin/stdout) с UART виртуалки.
~30–60 с boot-лога → промпт `/ #` → печатаешь команды и видишь реакцию.
PATH не задан, поэтому апплеты — как `busybox ls`, `busybox awk '...'`;
builtin'ы (echo, cd, for/while/if, `$((...))`) работают напрямую.
Ctrl-C выходит. Это «живой» аналог скриптованных milestone'ов из
`tests/linux_userspace.rs` (там ввод подаётся фиксированный).

### Alpine (на пути к загрузке Alpine) — стадия A: musl-userspace работает

Текущий userspace — Tinycore (busybox + **glibc**). Alpine использует
**musl** (другой libc, другой `ld-musl-i386.so.1`, и busybox — **PIE**
ET_DYN, а не ET_EXEC). Стадия A проверяет, что эмулятор гоняет musl-PIE-
busybox из Alpine на текущем ядре:

```bash
# подготовить Alpine musl-rootfs (один раз):
mkdir -p /tmp/alpine && cd /tmp/alpine
curl -sO https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86/alpine-minirootfs-3.21.7-x86.tar.gz
mkdir -p root && tar -xzf alpine-minirootfs-3.21.7-x86.tar.gz -C root
mkdir -p /tmp/wwwvm-alpine/rootfs/{bin,lib}
cp root/bin/busybox            /tmp/wwwvm-alpine/rootfs/bin/
cp root/lib/ld-musl-i386.so.1  /tmp/wwwvm-alpine/rootfs/lib/
cp root/lib/ld-musl-i386.so.1  /tmp/wwwvm-alpine/rootfs/lib/libc.musl-x86.so.1   # busybox DT_NEEDED's это имя

# запустить milestone:
cd /home/slava/projects/wwwvm
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_musl_milestone -- --ignored --nocapture
```

→ `ALPINE_MUSL_OK`: musl-libc + musl-ld.so + PIE-релокация работают.

**Стадия B — родное ядро Alpine + musl-userspace (= Alpine, без OpenRC/apk):**

```bash
# взять ядро Alpine (vmlinuz-lts, ~8 МБ) из netboot:
cd /tmp/alpine
curl -sO https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86/alpine-netboot-3.21.7-x86.tar.gz
tar -xzf alpine-netboot-3.21.7-x86.tar.gz boot/vmlinuz-lts
cp boot/vmlinuz-lts /tmp/wwwvm-alpine/vmlinuz-lts

cd /home/slava/projects/wwwvm
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_kernel_milestone -- --ignored --nocapture
```

→ `ALPINE_MUSL_OK`: эмулятор **грузит ядро Alpine 6.12 LTS И гоняет
Alpine-musl-busybox** — то есть Alpine (ядро + userspace), пока без полного
init.

**Стадия C — ВЕСЬ Alpine minirootfs → рабочий шелл:**

```bash
# minirootfs уже распакован в /tmp/alpine/root из стадии A
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_rootfs_milestone -- --ignored --nocapture
```

→ `ALPINE_ROOTFS_OK`: упаковывает **весь** дерево minirootfs (busybox + ~335
applet-симлинков, musl, настоящие `/etc`, `/sbin`, apk) в cpio (с
симлинками + /dev-нодами + `/init`-скриптом), грузит на ядре Alpine, и
`/init` (`#!/bin/sh` → busybox) печатает маркер + `/etc/alpine-release` +
`uname -a`. То есть **настоящий Alpine-userspace грузится до шелла** (ядро
Alpine + полный rootfs Alpine).

**Стадия D — НАСТОЯЩАЯ init-система Alpine (OpenRC) → login-промпт:**

В эмуляторе НЕТ сетевой карты, поэтому apk внутри гостя не работает; openrc
ставим **на хосте** через `apk.static` (кросс-арч, т.к. хост aarch64):

```bash
cd /tmp/alpine
# aarch64 apk.static (хост) ставит x86-пакеты в rootfs:
A=$(curl -s https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/ | grep -oE 'apk-tools-static-[0-9][^"]*\.apk' | head -1)
curl -sO "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/$A"; tar -xzf "$A"
mkdir -p oroot
./sbin/apk.static --arch x86 --root oroot \
  --repository https://dl-cdn.alpinelinux.org/alpine/v3.21/main \
  --update-cache --allow-untrusted --initdb --no-scripts add alpine-base openrc
# смержить openrc поверх рабочего minirootfs (он держит busybox-симлинки):
cp -a root aroot && cp -an oroot/. aroot/
sed -i 's|^#ttyS0::respawn:.*|ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100|' aroot/etc/inittab
ln -sf /bin/busybox aroot/init
for s in mdev hwdrivers; do ln -sf /etc/init.d/$s aroot/etc/runlevels/sysinit/$s; done

cd /home/slava/projects/wwwvm
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_openrc_milestone -- --ignored --nocapture
```

→ **настоящий init-flow Alpine**: busybox `init` (PID 1) → `/etc/inittab` →
`/sbin/openrc sysinit/boot/default` → getty → **`login:` промпт** (~3.5 млрд
шагов, ~120 c). То есть Alpine грузится до логина через свою родную
init-систему OpenRC.

**Живая Alpine-консоль (печатать команды в Alpine-шелл вживую):**

```bash
# нужны ассеты: /tmp/wwwvm-alpine/vmlinuz-lts + /tmp/alpine/root (см. стадию A/C)
cargo run -p wwwvm-vm --release --example alpine_console
```

Грузит ядро Alpine `vmlinuz-lts` + полный minirootfs (musl + PIE-busybox с
~335 апплет-симлинками), `/init` печатает строку готовности и `exec`'ает
интерактивный `busybox sh` на консоли, после чего твой терминал соединён с
UART гостя — печатаешь команды в **musl**-шелл Alpine и видишь реакцию (это
Alpine-аналог `busybox_console`, который грузит glibc/Tinycore). PATH не
задан → апплеты как `busybox ls`; builtin'ы работают напрямую. Ctrl-C
выходит. Хост-терминал переводится в raw-режим на время сессии (без
двойного эха и без утечки `^[[…R` от запросов позиции курсора), а часы
гостя засеваются реальным временем хоста (`set_cmos_time_from_host`), так
что `date` показывает «сейчас». Тестируемый (скриптованный) аналог — milestone
`linux_userspace_alpine_interactive_milestone`: он печатает `echo
$((6*7))` в живой musl-шелл по UART и ассертит, что обратно пришло
`ALPINE_LIVE_42` — то есть весь tty-input-путь (send_input → 16550 RX →
RX-IRQ → musl ash read() → арифметика → write) работает на Alpine/musl.

**apk внутри гостя (offline-установка пакета):**

Настоящий пакетный менеджер Alpine `apk` РАБОТАЕТ внутри гостя — ставит
пакет из локального `.apk` без сети:

```bash
cd /tmp/alpine
# точное имя/версию берём из APKINDEX (прямой scrape листинга врёт):
curl -sL -o APKINDEX.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86/APKINDEX.tar.gz
# tree — крошечный пакет, единственная зависимость so:libc.musl-x86.so.1 (musl уже стоит):
curl -sL -o tree.apk https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86/tree-2.2.1-r0.apk
cp -a root aproot && cp tree.apk aproot/tree.apk   # rootfs с локальным .apk внутри

cd /home/slava/projects/wwwvm
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_apk_milestone -- --ignored --nocapture
```

→ `/init` делает `apk add --allow-untrusted --no-network /tree.apk`, затем
`tree --version`; маркер `APK_TREE_INSTALLED_OK` печатается только если apk
реально распаковал пакет в `/usr/bin/tree`, зарегистрировал его, разрешил
зависимость `so:libc.musl-x86.so.1` из уже установленного musl, И новый
бинарь затем запустился. То есть apk (musl + его zlib/crypto/db) **работает
на эмуляторе** — установка пакета внутри гостя выполнена (offline).

Осталось: **сетевой `apk add` (fetch из remote-репозитория)** — это
вторая половина apk, которой нужна сетевая карта в эмуляторе + мост к
хосту через proxy с безопасным allowlist'ом (`*` нельзя). Подтверждено
исследованием: NIC-драйверы (rtl8139/virtio-net/e1000) в Alpine
`vmlinuz-lts` НЕ встроены — они модули в `modloop-lts` (squashfs `.ko`).
Поэтому сетевой apk требует: (1) устройство-NIC в эмуляторе, плюс (2) либо
загрузку модуля в госте (mount squashfs + `finit_module` + релокация
модуля — ничего из этого пока нет), либо своё ядро с драйвером,
встроенным (`CONFIG_8139TOO=y`), плюс (3) host-side TCP/IP-мост через
allowlist'нутый proxy. Это большая многосессионная фича.

**Бит-точность 64-битного ALU (sha512):**

```bash
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_sha512_milestone -- --ignored --nocapture
```

→ `echo SHA512_INPUT | busybox sha512sum`, ассерт **точного** 128-hex
дайджеста. SHA-512 (в отличие от md5/sha1 — 32-битных) работает на
64-битных словах, которые 32-битный CPU считает парами регистров: каждый
64-битный поворот = `shld`/`shrd`, каждое 64-битное сложение = `add`+`adc`.
Точное совпадение 512-битного дайджеста доказывает, что `SHLD`/`SHRD` и
цепочки add-with-carry эмулятора бит-корректны на всю 64-битную ширину —
путь, который 32-битные хеши не задевают. Парный тест на 64-битное
деление — `linux_userspace_alpine_factor_milestone` (`busybox factor
600851475143` → точное `71 839 1471 6857`): factor = trial division, тысячи
64-битных `n % d` / `n / d`, на i386 это софтовые `__umoddi3`/`__udivdi3`
поверх 32-битного `DIV` — точная факторизация пинит частное И остаток.

**LZMA range-decoder (xzcat):**

```bash
cd /tmp/alpine && cp -a root xroot
printf 'LZMA_RANGEDECODE_OK\n' | xz -9 > xroot/payload.xz   # asset: .xz внутри rootfs
cd /home/slava/projects/wwwvm
cargo test -p wwwvm-vm --release --test linux_userspace \
  linux_userspace_alpine_xz_milestone -- --ignored --nocapture
```

→ `busybox xzcat /payload.xz` распаковывает host-сделанный `.xz` и печатает
вшитый маркер. XZ/LZMA2-декодер — это путь, не похожий на другие апплеты:
бинарный RANGE-декодер с адаптивной моделью вероятностей (`bound =
(range>>11)*prob`, ренормализация сдвигами, подстройка `prob`) — плотный
цикл умножений/сдвигов/carry-сравнений, совсем не как table-lookup'ы
DEFLATE (gzip) или повороты хеша. xz ещё проверяет CRC64 над выходом
(второй 64-битный путь), так что верный маркер = range-декодер +
match-copy + CRC64 все бит-точны.

**bzip2 (BWT) декодер (bzcat):** `printf 'BZIP2_BWT_OK\n' | bzip2 -9 >
xroot/payload.bz2`, затем `linux_userspace_alpine_bzip2_milestone`
(`busybox bzcat /payload.bz2`). Третий, отдельный декомпрессор: Huffman →
inverse-MTF → **inverse Burrows-Wheeler** (обход sort-индексов по блоку,
тяжёлый на data-dependent indexed load/store) → CRC32. Верный маркер = весь
конвейер + инверсия BWT бит-точны — путь, которого не задевают ни DEFLATE,
ни LZMA.

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
