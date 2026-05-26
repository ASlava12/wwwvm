# wwwvm

Учебная виртуальная машина в браузере. Rust компилируется в WebAssembly,
управляется из JavaScript. Цель — обучающий проект по Linux:
страница загружает образ, стартует VM, JS отдаёт команды и получает вывод.

Проект пишется поэтапно. На текущем этапе доказан **end-to-end pipeline**
`JS → WASM → CPU → UART → JS`: встроенный 43-байтовый гость печатает
банер и эхом отвечает на ввод. CPU и набор устройств намеренно
минимальны — будут расти под требования реальных ОС.

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

* **mem** — линейная физическая память, little-endian аксессоры.
* **devices** — 16550 UART (COM1: 0x3F8), 8259A PIC (master, 0x20/0x21),
  8254 PIT (0x40-0x43), PS/2 keyboard (0x60/0x64) и CMOS/RTC
  (0x70/0x71). PIC — IMR/IRR/ISR, ICW1/ICW2 (ICW3/ICW4 отбрасываются),
  non-specific EOI через OCW2. UART — IER на offset+1, `irq_pending()`
  true при rx + IER bit 0. PIT — канал 0 в режимах 0/2/3 с control-word
  на 0x43. Keyboard — очередь scan-кодов, port 0x60 pops byte, 0x64
  status bit 0 = OBF; level-triggered IRQ 1. CMOS — 128-байтное
  хранилище за index-латчем (0x70), data port (0x71); по умолчанию
  Status B = binary + 24h, дата 2026-01-01 (host может перезаписать
  через `Vm::set_cmos_time`). `IoBus::refresh_irqs` смешивает уровневые
  (UART → IRQ 4, keyboard → IRQ 1) и фронтовые (PIT → IRQ 0) сигналы.
* **cpu** — реальный режим x86: `MOV r8/r16, imm`; `MOV r/m, r`,
  `MOV r, r/m`, `MOV r/m, imm` (опкоды 0x88–0x8B, 0xC6/0xC7); `LODSB`;
  полная ALU-семья (`ADD`/`OR`/`ADC`/`SBB`/`AND`/`SUB`/`XOR`/`CMP`,
  8 и 16 бит, формы `r/m,r`, `r,r/m`, `AL,imm8`, `AX,imm16`).
  **Полное 16-битное ModR/M-адресование памяти** — все 8 r/m-форм
  (`[BX+SI]`, `[BX+DI]`, `[BP+SI]`, `[BP+DI]`, `[SI]`, `[DI]`, `[BP]`,
  `[BX]`), включая исключение `mod=00,rm=110 → [disp16]`, с правильным
  выбором сегмента по умолчанию (SS для `[BP*]`, иначе DS), и disp8/disp16.
  `INC`/`DEC r16`; `TEST AL/AX, imm`; `IN/OUT` через DX и imm8;
  **Group 1** (`ADD/OR/ADC/SBB/AND/SUB/XOR/CMP r/m, imm` — 0x80/0x81/0x83
  с sign-extension);
  **Group 3** (`NOT`, `NEG`, `TEST r/m, imm`, `MUL`/`IMUL`/`DIV`/`IDIV` —
  0xF6/0xF7, 8 и 16 бит, с правильной обработкой DX:AX для 16-бит и
  возвратом `CpuError::DivideError` на деление на ноль или переполнение
  частного);
  **Group 4** (`INC`/`DEC r/m8` — 0xFE);
  **Group 5** (`INC`/`DEC r/m16`, `CALL r/m16` near indirect, `JMP r/m16`
  near indirect, `PUSH r/m16` — 0xFF);
  **Group 2** сдвиги/повороты (`SHL`/`SHR`/`SAR`/`ROL`/`ROR` r/m,1 / CL / imm —
  0xC0/0xC1/0xD0..0xD3; RCL/RCR пока не реализованы);
  **строковые операции** `MOVS`/`STOS`/`LODS`/`SCAS`/`CMPS` (B/W) с
  учётом DF и сегментов DS/ES; префиксы `REP`/`REPE`/`REPNE`
  (0xF2/0xF3) для повторения с CX-счётчиком, для CMPS/SCAS — с
  условием ZF;
  **сегментные префиксы** `CS:`/`DS:`/`ES:`/`SS:` (0x26/0x2E/0x36/0x3E)
  для любой инструкции с памятью; работают и до, и после REP-префикса;
  автоматически сбрасываются после каждой инструкции (state — в
  `Cpu::seg_override`);
  **сегментные регистры из гостя** — `MOV sreg, r/m16` / `MOV r/m16, sreg`
  (0x8E/0x8C), `LES`/`LDS` (0xC4/0xC5) для загрузки 32-битного far-указателя
  в регистр и ES/DS одной инструкцией;
  **LEA** `r16, m` (0x8D) — вычисляет EA, не читает память;
  **XCHG** — полная семья: `r/m, r` 8/16-бит (0x86/0x87), short-form
  `XCHG AX, r16` (0x91..0x97), плюс 0x90 (NOP = XCHG AX,AX);
  **прерывания в реал-моде** — `INT3` (0xCC), `INT imm8` (0xCD),
  `INTO` (0xCE, срабатывает только при OF=1), `IRET` (0xCF). IVT
  читается с линейного 0 как 256 записей offset:segment по 4 байта;
  INT толкает FLAGS, CS, IP, очищает IF и загружает CS:IP из вектора;
  IRET откатывает всё назад (см. `Cpu::do_interrupt`);
  **sign-extension** `CBW` (0x98) AL→AX, `CWD` (0x99) AX→DX:AX —
  обязательны перед `IDIV` для корректного знакового деления;
  **flag transfer** `LAHF` (0x9F) FLAGS-low→AH, `SAHF` (0x9E) AH→FLAGS-low;
  **far-инструкции**: `CALL ptr16:16` (0x9A, push CS+IP, load CS:IP из imm),
  `JMP ptr16:16` (0xEA, без стека), `RETF`/`RETF imm16` (0xCB/0xCA),
  `CALL m16:16`/`JMP m16:16` (Group 5 FF /3, /5 — far indirect через
  4-байтный указатель в памяти);
  **PUSH/POP сегментных регистров** — `PUSH ES/CS/SS/DS` (0x06/0x0E/0x16/0x1E),
  `POP ES/SS/DS` (0x07/0x17/0x1F). `POP CS` (0x0F) намеренно не реализован
  — на 80286+ это префикс 2-байтных опкодов;
  **счётные циклы**: `LOOP` (0xE2), `LOOPE`/`LOOPZ` (0xE1), `LOOPNE`/`LOOPNZ`
  (0xE0) с CX-pre-decrement, `JCXZ` (0xE3) без декремента — стандартная
  пара для защищённой счётной итерации;
  **16-битные порты**: `IN AX, DX`/`OUT DX, AX` (0xED/0xEF) и формы с imm8
  (0xE5/0xE7); реализованы как пара 8-битных доступов к port и port+1;
  **XLAT** (0xD7) — `AL = mem[DS:BX+AL]`, идиома таблицы перевода;
  **управление CF**: `CLC`/`STC`/`CMC` (0xF8/0xF9/0xF5);
  **префиксы-noop**: `LOCK` (0xF0), `WAIT` (0x9B) принимаются и
  засчитываются за one-step (на одиночном CPU без FPU они ни на что
  не влияют);
  **80186 additions**: `PUSHA`/`POPA` (0x60/0x61, 8 GPR одним блоком —
  идиома пролога handler'ов), `IMUL r16, r/m16, imm8/imm16`
  (0x69/0x6B — трёхоперандное знаковое умножение, основа для умножения
  на константу из компилятора), `ENTER imm16, 0` (0xC8 — стандартный
  function prologue), `LEAVE` (0xC9). ENTER с уровнем вложенности > 0
  не поддерживается (компиляторы C/Rust его не эмитят);
  **доставка внешних IRQ**. В начале `step()` CPU проверяет
  `IoBus::pending_irq_vector()`; при `IF=1` и наличии незамаскированного
  pending IRQ — `ack` PIC + `do_interrupt(vec)`. Интегрирует UART и
  будущие устройства в стандартный INT-цикл реал-моды.
  **стек SS:SP** — `PUSH`/`POP r16` (0x50–0x5F), `PUSH imm8/imm16`
  (0x68/0x6A), `PUSHF`/`POPF` (0x9C/0x9D), `CALL rel16` (0xE8),
  `RET`/`RET imm16` (0xC3/0xC2);
  `JMP rel8/rel16`; весь набор `Jcc rel8` (использует CF/ZF/SF/OF/PF);
  флаги корректно обновляются для арифметики и логики;
  `CLI/STI/CLD/STD/NOP/HLT`. Неподдержанные опкоды возвращают
  `CpuError::Unimplemented { opcode, cs, ip }`.
* **vm** — `load_default_guest`, `load_interactive_demo`, `set_autorun_commands`,
  `boot`, `run_steps(budget) -> (executed, Stop)`, `send_input`, `drain_output`,
  `set_ivt(vec, seg, off)`, `read_mem_u8/u16`. Два встроенных гостя:
  `HELLO_GUEST` (~43 байта, polling LSR + echo) и `interactive_demo`
  (~60 байт суммарно, banner через LODSB + interrupt-driven UART echo
  через IRQ 4 — настраивает IVT, IER, IMR, STI, спит в `JMP -2`).
* **wasm** — `WwwVm` для JS: `load_default_guest`, `load_image`,
  `set_autorun([…])`, `boot`, `run(cycles)`, `send_command`,
  `send_input`, `read_output`, `is_halted`, `is_booted`, `last_error`.
* **proxy** — отдельный Rust-бинарь. Принимает WebSocket, первое
  сообщение JSON `{"host","port"}`, дальше байты в обе стороны.
  Allow-list — `WWWVM_PROXY_ALLOWLIST` (`*` / `host:port` / `host:*`).
* **web** — демо-страница с xterm.js и `window.runCommand(text)`,
  возвращающим `Promise<string>`.
* Тестов — **135 зелёных** (mem 4 + devices 26 + cpu 89 + vm 10 + wasm 1
  + proxy 5). VM-уровень включает E2E-тесты `LOOP+OUT` (печать "ABCDE"),
  `MUL` (квадрат байта от UART), `DIV`-by-zero → `Stop::CpuError`,
  **interrupt-driven serial** (UART rx → IRQ 4 → handler читает RBR → EOI)
  и **periodic timer** (PIT mode 2 → IRQ 0 → handler инкрементит счётчик
  в RAM до 4 → HLT) — полная цепочка device → PIC IRR → CPU → IDT →
  handler → device drain работает для двух разных типов источников IRQ.

## Что НЕ работает (намеренно, дорожная карта)

Это учебный проект; полноценная поддержка Alpine/Linux в одном сеансе
нереалистична. Ниже — что осталось добавить.

| Этап | Объём | Зачем |
|------|-------|-------|
| `PUSH/POP sreg`, `CALL ptr16:16` (far), `RETF` | малый | Переходы через сегменты, далёкий ret |
| Префиксы сегмента (`CS:`, `DS:`, `ES:`, `SS:`) | малый | `MOV ES:[DI], …` и т.п. |
| `RCL`/`RCR` (Group 2 /2,/3), BCD (`AAA`/`AAS`/`AAM`/`AAD`/`DAA`/`DAS`) | малый | Big-number арифметика, DOS-era BCD-код |
| BIOS-хендлеры по векторам (0x10 — VGA, 0x13 — диск, 0x16 — клавиатура, 0x19 — boot) | средний | Гость, ожидающий стандартного PC BIOS API |
| 8042 controller commands (self-test, port disable), keyboard scan-code translation на host-стороне | малый | Совместимость со стандартными KB-драйверами |
| Реальные устройства: slave PIC (IRQ 8..15), IDE/ATA, VGA, RTC alarm IRQ | средний | Загрузочные тракты любого реального дистрибутива |
| Protected mode (CR0.PE, GDT, дескрипторы, прерывания через IDT-gates) | большой | Любое современное ядро |
| 32-бит (i386): операнд/адрес-префиксы 0x66/0x67, long-mode позже | большой | Любое 32+ ядро |
| Прерывания: `INT`, `IRET`, IDT, BIOS-вектора 0x10/0x13/0x16 | средний | Гости, использующие BIOS-калбэки |
| Protected mode + paging | большой | Любое современное ядро |
| Long mode (x86_64) | большой | 64-битные ядра |
| PIC/PIT/RTC/PS2/CMOS | средний | Загрузка дистрибутивов |
| Эмуляция IDE/ATA или virtio-blk | средний | Чтение rootfs |
| ne2k или virtio-net + slirp-подобный TCP/IP | средний | Сеть из гостя через прокси |
| VGA-текст / fbcon | малый | Видеть `init` без UART-консоли |
| Снимки/persist состояния в IndexedDB | малый | Сохранение сессий студентов |
| 9P / passthrough FS поверх postMessage | малый | Передача файлов между host и гостем |

## Сборка и запуск

### Хост-тесты (всегда работает)

```bash
cargo test --workspace
```

Должно вывести 17 + 5 пройденных тестов.

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

В UI: вписать команды в Autorun (по одной в строке) → **Boot VM** →
ввод/команды летят в гостя, вывод появляется в терминале.
`runCommand("hello")` доступен в DevTools-консоли.

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
  devices/    # 16550 UART + IoBus
  cpu/        # x86 real-mode подмножество
  vm/         # оркестратор + встроенный гость
  wasm/       # cdylib для браузера (wasm-bindgen)
  proxy/      # отдельный бинарь: WebSocket ↔ TCP
web/
  index.html
  main.js
  style.css
  pkg/        # сюда wasm-pack кладёт wasm + .js шим (gitignored)
```

## Лицензия

MIT OR Apache-2.0.
