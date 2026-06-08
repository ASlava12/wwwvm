# wwwvm

Учебная виртуальная машина в браузере. Rust компилируется в WebAssembly,
управляется из JavaScript. Цель — обучающий проект по Linux:
страница загружает образ, стартует VM, JS отдаёт команды и получает вывод.

Полный учебный PC: 8086+80186 ISA, стандартный набор устройств (UART,
двойной PIC, PIT, клавиатура, CMOS, VGA-text), interrupt-driven I/O
через IDT, snapshot/restore, доступ из JS через wasm-bindgen, отдельный
Rust-прокси для сетевых соединений. Три встроенных гостя для первого
запуска, тутор по hand-assembly в [docs/HAND_ASSEMBLY.md](docs/HAND_ASSEMBLY.md).

**Статус.** Минимальное Linux 6.12 i386-ядро грузится до userspace; пройдено
36+ syscall-майлстоунов (fork/execve/mmap/file-IO/signals/uname/…), каждый —
с регресс-тестом. Поверх этого работают Alpine-userspace, сеть из гостя,
графика (efifb→canvas), снапшоты и параллельные сетевые VM (см. ниже).
Полный дев-журнал бутстрапа — в [docs/MILESTONES.md](docs/MILESTONES.md).

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
┌──────────────────────────────┐  WS  ┌──────────────────────────────┐
│ guest RTL8139 NIC            │◄────►│ crates/proxy (Rust, tokio)    │
│ → in-wasm smoltcp NAT        │      │ WebSocket ↔ TCP gateway       │
│   (crates/net, slirp role)  │      │ allowlist + Origin lock (env) │
└──────────────────────────────┘      └──────────────────────────────┘
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
| Linear framebuffer (efifb) | reserved RAM, top of mem | — | screen_info VLFB/EFI |
| PCI host bridge + RTL8139 NIC | 0xCF8/0xCFC + BAR0 | 11 → slave bit 3 | level (ISR & IMR) |

`IoBus::refresh_irqs` на каждом шаге CPU перекладывает все pending
IRQ в IRR. Slave автоматически каскадит через master IRQ 2: если master
выставляет IRQ 2, `pending_irq_vector` спускается в slave и возвращает
его вектор, а `ack_irq` ack'ает оба чипа — двухтактный INTA на железе.

**Сеть (RTL8139, `crates/devices/src/{pci,rtl8139}.rs`).** PCI Mechanism #1
(0xCF8/0xCFC) с host-bridge (Intel 440FX) на 00:00.0 — без него ядро
печатает «PCI: Fatal» и отключает шину. NIC RealTek RTL8139 на 00:01.0
(vendor 0x10EC device 0x8139), MAC `52:54:00:12:34:56` читается стоковым
`8139too` из модели 93C46 EEPROM (Microwire bit-bang на Cmd9346). BAR0 —
256-байтное I/O-окно. **Bus-master DMA** идёт через шаг CPU (у устройства
нет доступа к гостевой RAM): на TX `Cpu::service_nic_tx` копирует кадры
из RAM по дескрипторам TSAD/TSD в `Vm::drain_tx_frames`; на RX
`Vm::inject_rx_frame` пишет кадр в кольцо RBSTART (заголовок status+len,
−16-quirk у CAPR) и поднимает ISR.ROK → IRQ 11. Драйверы NIC у Alpine —
модули (`mii.ko`+`8139too.ko` из modloop-lts), грузятся `insmod` как есть
(кастомное ядро не нужно). Хост-сторона — `crates/vm/src/lan.rs`
(`VirtualGateway`): отвечает на ARP и ICMP echo; **`ping 10.0.2.2` из
гостя проходит** (0% loss, ядро проверяет наши IP/ICMP-контрольные суммы).

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
  (формат **v15**: CPU+RAM+устройства+LAPIC/HPET **+ protected-mode
  seg_cache** — base/limit каждого сегмента; до v15 restore восстанавливал
  кэши как `sel<<4`, из-за чего PM-гость (Linux) падал при resume — см.
  «Снапшот-платформа» ниже)

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
Для графики — `enable_framebuffer(w,h)` (efifb) + `framebuffer_bytes()`
/ `framebuffer_width/height/stride()`: демо блитит пиксели на canvas.
Ошибки CPU surface как `last_error: Option<String>`.

**Графика — линейный framebuffer (efifb).** `Vm::enable_linear_framebuffer`
резервирует регион RAM наверху памяти и прописывает в boot-protocol
`screen_info` (`lfb_base/size/width/height/depth/linelength` + RGB-поля,
`orig_video_isVGA` = `VIDEO_TYPE_EFI` 0x70 для efifb или `VIDEO_TYPE_VLFB`
0x23 для vesafb). Ядро биндит efifb/vesafb прямо из `screen_info` (без
настоящего VESA BIOS / EFI firmware), fbcon рисует консоль пикселями в
этот регион, а хост читает байты обратно (`framebuffer_bytes()`) и блитит
на `<canvas>`. У Alpine `vmlinuz-lts` встроен только efifb (vesafb выпилен),
поэтому дефолт — EFI. `start_protected_mode_at` заодно строит чистую
zero-page (зануляет `boot_params[0..0x1F1]` — как настоящий загрузчик,
вместо verbatim-копии boot-сектора) и вырезает регион FB из e820 как
reserved. Teeth: `linux_efifb_framebuffer_renders_pixels_milestone`
(efifb биндится к нашему base + 230 KB ненулевых пикселей на Tinycore;
на Alpine — `WWWVM_FB=800x600 WWWVM_FB_PROBE=1` в `alpine_console`).

### Сеть (`crates/proxy`)

Standalone Rust-бинарь на tokio + tokio-tungstenite. Принимает
WebSocket, первое сообщение JSON `{"host","port"}`, дальше байты в
обе стороны. Allow-list — env var `WWWVM_PROXY_ALLOWLIST`
(`*` / `host:port` / `host:*`, comma- **или** newline-separated — веб-UI
шлёт многострочный textarea; `*` = OPEN RELAY, только loopback).
`WWWVM_PROXY_ORIGINS` запирает WebSocket-handshake на конкретные браузерные
Origin'ы (защита от Cross-Site WebSocket Hijacking). Хост резолвится на
стороне прокси и пинится globally-routable IP (SSRF-guard).

**Цепочка через публичный прокси (upstream chaining, `crates/proxy/src/upstream.rs`).**
Connect-кадр может попросить туннель не напрямую, а через сторонний публичный
прокси: `{"host","port","upstream":{"kind":"socks5|socks4|http","host","port"}}`
— через конкретный, либо `{"host","port","auto":true}` — сервер сам берёт и
round-robin-ротирует из пула `WWWVM_PROXY_UPSTREAMS_FILE` (тот JSON, что пишет
`scripts/fetch-proxies.py`), пробуя до 6 штук с коротким per-proxy таймаутом
(дохлые отваливаются быстро). Реализованы no-auth SOCKS5 (ATYP=domain),
SOCKS4a (по имени — таргет локально не резолвится) и HTTP CONNECT. Адрес
*самого* upstream'а резолвится и обязан быть globally-routable (manual/auto
нельзя навести на loopback/LAN); таргет по-прежнему гейтится общим `Allowlist`.
Публичные прокси НЕдоверенные — только не-чувствительный трафик.

`scripts/fetch-proxies.py` — stdlib-only (Python 3.6+, для cron) парсер: тянет
HTTP/SOCKS-списки из Proxifly, TheSpeedX, ProxyScrape, GeoNode → `web/proxies.json`
(gitignored, дедуп, atomic write, fault-tolerant по источникам). Веб-UI грузит
этот файл в выпадающий список upstream-прокси (плюс «Direct», «Auto-rotate» и
ручной ввод `kind://host:port`).

**Публичный хостинг — релей нельзя убрать (браузер не умеет в raw TCP), но его
можно сделать безопасным.** Цепочка всегда `браузер → твой WS-релей → [опц. публичный
прокси] → сайт`. Абузят не «релей вообще», а ОТКРЫТЫЙ релей (`*`). Рецепт для выкладки
наружу: (1) **узкий allowlist** — только хосты, нужные демке (напр. `dl-cdn.alpinelinux.org:80,:443`),
тогда произвольный туннелинг/SSRF невозможен; (2) `WWWVM_PROXY_ORIGINS` на свой домен;
(3) **ресурс-лимиты** (anti-abuse): `WWWVM_PROXY_MAX_CONNS` (глоб. одновременных, деф. 512),
`WWWVM_PROXY_MAX_CONNS_PER_IP` (на IP, деф. 32) — включены по умолчанию, мгновенный reject
сверх лимита; `WWWVM_PROXY_IDLE_TIMEOUT_SECS` (закрыть простаивающие туннели) и
`WWWVM_PROXY_MAX_BYTES` (потолок байт/соединение) — opt-in (`0` = выкл; не режут легитимные
длинные передачи). Гейт = глобальный `Semaphore` + per-IP счётчики (RAII-`ConnGuard`
освобождает оба на Drop); idle-watchdog через `select!` + общий `last_ms`. Спрятать свой
egress-IP можно, направив релей через auto-rotate публичных прокси (выход не с твоего IP),
но это флаки и не отменяет allowlist.

**TLS / `wss://` встроен** (для https-страницы `ws://` браузер не пустит — mixed content).
Задай `WWWVM_PROXY_TLS_CERT` + `WWWVM_PROXY_TLS_KEY` (PEM-файлы, оба вместе) — релей сам
терминирует TLS (rustls/ring, TLS 1.2+1.3), отдельный reverse-proxy не нужен; без них —
обычный `ws://`. Тогда в поле `proxy ws` пишешь `wss://твой-домен:порт`. Стрим после
TLS-handshake тот же, что и для plain ws (handler дженерик по типу сокета).

**Деплой одной командой:** `WWWVM_DOMAIN=example.com docker compose up -d --build` поднимает
сайт + релей за single-origin TLS (Caddy: авто-Let's Encrypt, отдаёт `web/`, проксирует
`wss://домен/ws` на релей). См. **[docs/DEPLOY.md](docs/DEPLOY.md)** (`Dockerfile`, `Caddyfile`,
`docker-compose.yml`) — там и про scoped allowlist, лимиты и опц. egress через публичные прокси.

**Модель безопасности и безопасный деплой:** **[SECURITY.md](SECURITY.md)** —
границы доверия (недоверенный гость/образ/снапшот), правила релея (никогда `*`
на публичном бинде, `WWWVM_PROXY_ORIGINS`, SSRF-пиннинг, `wss://` для https),
авторизация снапстора. Прочти перед публичным деплоем.

**Сеть в браузере — TCP NAT в wasm → WebSocket-relay.** Тот же smoltcp-NAT,
что в нативе, крутится в wasm; меняется только транспорт per-flow:
вместо `std::thread`+`TcpStream` (невозможно в wasm) — `QueueConnector`
(`crates/net/src/queue.rs`), который строит тот же `HostConn`, что
потребляет NAT, но отдаёт встраивателю (JS) очереди байт. JS туннелит
каждый flow по WebSocket в `crates/proxy`. wasm-API: `net_enable(allow)`,
`net_pump(now)` (мостит NIC-кадры VM ⇄ NAT), `net_cache_dns(name, ips)`
(JS резолвит имена через DoH), `net_take_new_connections()`,
`net_conn_outbound/send/closed()`. Half-close корректен: `drain_outbound`
закрывает flow по reap-флагу (`stop`), а НЕ по дисконнекту `out_rx` (тот
срабатывает и на write-half-close гостя — иначе хвост ответа терялся бы).
Teeth: `queue::tests::guest_tcp_through_queue_nat_echoes` — настоящий
гостевой smoltcp-клиент через NAT (handshake + данные в обе стороны) +
half-close/reap различение. Браузерный e2e (WebSocket+DoH) — только в
реальном браузере (см. `docs/BROWSER_NET.md`).

### Снапшот-платформа + виртуальный LAN (`crates/snapstore`, `crates/net::switch`, `web/lan.html`)

Две фичи для обучающих сценариев (всё в браузере; сервер — только хранилище):

- **Кастомные снапшоты (base + recipe → content-addressed page-diff).** Снапшот
  бьётся на страницы 4 КиБ, каждая адресуется blake3-хешем (`crates/vm::paged`
  `encode_export`/`decode_export`; `Vm::snapshot_export`/`restore_export`,
  проброшены в wasm). Производный снапшот делит с базой неизменённые страницы →
  хранится только diff (изменённые рецептом), дедуп между снапшотами. Сервер
  `crates/snapstore` (`snapstore-server`) — filesystem content-addressed store:
  `put_page` сверяет `blake3(body)==hash` (нельзя подделать/испортить страницу),
  идемпотентный дедуп; PUT под admin-токеном (`WWWVM_SNAPSTORE_TOKEN`), GET
  открыт (immutable → кешируемо), CORS. Веб-UI «Custom snapshots»: загрузить
  базу → рецепт (команды в гостя) → `snapshot_export` → залить только
  недостающие страницы + манифест; обратная загрузка восстанавливает. Teeth:
  `examples/snapshot_resume` — маркер в гостевой ФС переживает export→restore.
- **Параллельные VM в одном L2-LAN.** `crates/net::switch` — learning Ethernet
  switch + `Hub` (drain TX → route → inject RX); каждая VM в своём Web Worker
  (`web/lan.html`), у каждой свой MAC (`set_nic_mac`) и IP (cmdline
  `wwwvm.ip=10.0.0.N/24`, режим `WWWVM_NET_LAN` в `/init`). Teeth:
  `examples/two_vm_lan` — два настоящих Alpine-гостя пингуют друг друга через
  свитч (2/2 received). Деплой снапстора — в `docs/DEPLOY.md` (Caddy `/snap`).
- **Гибрид: LAN + интернет на одном NIC** (галка «Internet» в `web/lan.html`).
  Все VM на `10.0.2.0/24` с NAT-шлюзом `10.0.2.2` у каждой (`net_enable_ip`,
  свой guest IP). Worker (`vm-worker.js`, режим `lan+nat`) маршрутизирует кадры
  по dst-MAC: шлюз → NAT (`net_push_frame`/`net_poll`/`net_pop_egress`), peer →
  свитч, broadcast → оба (шлюз отвечает на ARP, peers видят рассылку). smoltcp в
  NAT владеет только `10.0.2.2`, поэтому peer-ARP он игнорирует — без перехвата.
  В `/init` при `wwwvm.gw=` добавляются resolv.conf + apk→http → `apk update/add`
  в госте. Плюс: RAM/RAM-диск по-VM, «+ Add VM» на лету, живой RX/TX+uptime в
  списке. (TCP через relay; ICMP наружу не релеится — `ping` бьёт только шлюз.)
- **Панель Fleet в основной странице.** `web/index.html` встраивает лаб
  (`lan.html?embed=1`, режим без своей «шапки») как **правую панель Fleet,
  видимую по умолчанию** — изолированный `<iframe>`, чтобы мульти-worker хаб
  лаба не конфликтовал с одиночным движком главной страницы. Кнопка «🖥 Fleet»
  в шапке сворачивает/разворачивает (лаб продолжает работать скрытым).

### Веб-демо (`web/`)

- **Встраивание в свой проект:** [`docs/EMBED.md`](docs/EMBED.md) — тонкая
  обёртка `web/wwwvm.js` (`ready()` + класс `Vm`) и минимальный пример
  `web/embed-example.html` (поднять VM из чистой страницы в ~20 строк).
- xterm.js terminal с двусторонним IO;
- селектор между 3 встроенными гостями + autorun-textarea;
- `window.runCommand(text) -> Promise<string>` для DevTools;
- **Save/Load** через IndexedDB (`storage.js`);
- **Download .bin / Upload .bin** — портативный экспорт-импорт;
- **Boot Linux / Alpine** — **пикер готовых образов с сервера**
  (`images/manifest.json`): выбираешь образ (console / GUI) и жмёшь
  «Load selected image» — kernel + initramfs тянутся по HTTP и грузятся;
  выбор образа подставляет cmdline / framebuffer (можно поправить перед
  бутом). Сами образы собирает `scripts/build-web-images.sh` (см. ниже).
  Плюс fallback «…or load your own kernel files» (ручные пикеры bzImage +
  initramfs), чекбокс «Graphics framebuffer (efifb → canvas)» + разрешение,
  чекбокс «Networking» + proxy-URL + allowlist (TCP NAT → WebSocket-relay);
- pane с VGA-snapshot 80×25 + **canvas с efifb-пикселями** (fbcon).

### Качество

**788 тестов** зелёные (mem 30 + devices 120 + cpu 424 + vm 143
[вкл. tutorial-anchor 2] + net 42 + snapstore 11 + wasm 7 + proxy 11).
Снапшот v16. CI gates: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace
--locked` (+ `scripts/test-web-js.sh` для браузерной логики). Throughput release ≈ 60–110 MIPS зависит от хоста
(x86_64 быстрее aarch64; пример печатает арку, чтобы цифры не
сравнивались случайно: `cargo run --example throughput -p wwwvm-vm
--release`). Tutorial-anchor тесты в
`crates/vm/tests/tutorial_examples.rs` пин-fиксируют hex-байты из
`docs/HAND_ASSEMBLY.md` — любое смещение между документацией и
поведением VM ловит CI.

**Тесты браузерной логики:** `scripts/test-web-js.sh` (нужен Node 18+)
синтакс-проверяет все `web/*.js` и гоняет `node --test` на чистых
модулях без DOM/wasm/worker — гибридный роутер кадров
(`web/net-route.js`: шлюз→NAT, broadcast→оба, peer→свитч) и L2-свитч
(`web/l2-switch.js`). Они зеркалят свои Rust-двойники в `crates/net`
(`switch.rs` + `nat.rs` ARP-ownership), чтобы браузер и натив не
разъехались. `web/package.json` (`"type":"module"`) — только чтобы Node
видел `web/*.js` как ESM.

**Аудит корректности CPU (30 мая 2026)** — пять adversarial-проходов по ISA
против Intel SDM (ALU/shift/flags · сегменты/пейджинг/привилегии · атомики/
bit-string · x87/SSE · префиксы/CPUID/MSR/mode-transition) нашли и починили
реальные баги (главные: SSE PD/PS-инверсия в 32-битном PM; `rep ret` эпилог;
SYSEXIT CPL=3), каждый с teeth-confirmed тестом. Поразбор каждого прохода —
в [docs/MILESTONES.md](docs/MILESTONES.md); отложенные/намеренные упрощения —
memory `cpu-audit-deferred-findings`.

## Прогресс и майлстоуны

Детальная история бутстрапа (i386-ядро, Linux 6.12, busybox/ld.so userspace) с
датами и именами регресс-тестов вынесена в
**[docs/MILESTONES.md](docs/MILESTONES.md)**. Дорожная карта того, что ещё НЕ
работает — ниже.

## Что НЕ работает (дорожная карта к Alpine)

Между «грузит userspace из minimal initramfs» и «грузит Alpine» —
ещё дистанция. Крупные оставшиеся блокеры, по приоритету:

| Блокер | Объём | Зачем |
|--------|-------|-------|
| ✅ **РЕШЕНО** — Динамическая линковка (`ld.so`): multi-lib glibc работает | — | **✅ Работает end-to-end.** См. «Linux userspace (busybox через ld.so)» в [docs/MILESTONES.md](docs/MILESTONES.md): 15 asserting-milestone'ов от `DYNLINK_OK` до интерактивного шелла. Корневая причина была SYSEXIT CPL=3; исторический трейл расследования — в git history + memory `multilib-dynamic-linking-state.md`. |
| x87 расширения — настоящая 80-битная точность ✅ + FPU-исключения (#MF) ⏳ | малый | **80-битный x87 РЕАЛИЗОВАН (30 мая 2026):** x87-стек теперь хранит настоящие 80-битные значения (`crates/cpu/src/f80.rs` — soft-float `F80` с 64-битной мантиссой; арифметика на u128, round-nearest-even), а не f64. Это починило `busybox printf '%.17g' 0.1` (давал `0.099999999999994315`, теперь корректное `0.10000000000000001`; π → `3.1415926535897931`) — musl форматирует float'ы через `long double`, и на f64-стеке (53 бита) его dtoa терял ~11 бит. Корень был именно в точности модели, НЕ в опкодах (каждая x87-операция была бит-точна на f64 — пинит тест `fpu_dtoa_path_ops_are_individually_correct`). Milestone `linux_userspace_alpine_printf_dtoa_milestone` ассертит точный вывод. FXTRACT/FSCALE теперь точные (через поле экспоненты F80). Трансцендентные (FSIN/FCOS/FYL2X/…) и FSQRT пока считаются в f64 → промоутятся в F80 (second-tier, не на dtoa-пути). Осталось: FPU-исключения (#MF, маски в CW) и 80-битные точные FSQRT/трансцендентные. |
| ✅ **РЕШЕНО (2 июня 2026)** — SSE/SSE2/SSE3 + x87 для научного Python | — | Стресс реальным софтом (CPython, numpy) выловил и починил 7 пробелов в инструкциях: x87 FISTTP m16/m64, SSE2 PANDN, CMPPS/CMPPD/CMPSS/CMPSD, MOVMSKPS/PD, PINSRW/PEXTRW(xmm), SSE3 HADDPD/PS+HSUBPD/PS+ADDSUBPD/PS, MOVDDUP/MOVSLDUP/MOVSHDUP. **Подтверждено ворклоадом: `numpy 2.1.3` гоняет sum/dot/matmul бит-точно в госте** (OpenBLAS ddot/dgemm задействуют эти ops) + CPython end-to-end. Паттерн: MMX-форма опкода была, а 66/F2/F3-SSE-форма — нет. MMX-стек (mm0..mm7) — отдельный регистровый файл, ядро им почти не пользуется. |
| Real-mode setup execution (~16 KiB Linux boot-ASM) | очень большой | bzImage сам делает PE-переход — нужно выполнить его setup-код |
| Kernel decompression (gzip/zstd) | средний | bzImage payload сжат; либо распаковывать, либо грузить vmlinux |
| ✅ **РЕШЕНО** — Ring 3 + TSS + privilege transitions | — | Cross-ring INT/IRET, syscall round-trip (IRETD→user→INT→handler), cross-ring #PF — всё работает. CPL=0 guards на HLT/CLI/STI/IN/OUT (IOPL), LLDT/LTR/LGDT/LIDT/LMSW/INVLPG, INVD/WBINVD, MOV CR/DR, RDMSR/WRMSR/SYSEXIT, CLTS, RDPMC. **Per-port I/O-permission bitmap в TSS реализован** (`Cpu::io_access_permitted`: при CPL>IOPL читает битмап по I/O-map base в TSS, проверяет лимит TSS; тесты `io_bitmap_grants_specific_port_at_cpl3` + `in_al_from_ring_3_with_iopl_zero_faults`). |
| Полный #DF / #NP / #SS | средний | #DE, #UD, #PF и весь основной #GP набор уже доезжают; #DF/#NP/#SS — ещё нет |
| IDE/ATA DMA / virtio-blk | средний | Оба канала (primary + secondary) read+write через PIO уже работают; для модерн дистров нужно ещё DMA |
| HPET таймер-IRQ / реалистичный PIT-тайминг | малый | LAPIC периодический таймер уже доставляет; HPET — только probe-stub без доставки. Linux в большинстве конфигов берёт LAPIC, так что HPET-доставка — second-tier. |
| ✅ **РЕШЕНО** — Сеть из гостя (RTL8139 + NAT + relay) | — | Реализовано иначе, чем планировалось (RTL8139 вместо ne2k/virtio): NIC `crates/devices/src/rtl8139.rs` + in-wasm smoltcp-NAT (`crates/net`) с slirp-ролью → relay `crates/proxy` (WebSocket↔TCP). В госте: `apk update/add`, `wget`, TCP наружу; плюс виртуальный L2-LAN между несколькими VM (`crates/net::switch`) и гибрид LAN+интернет. См. разделы «Снапшот-платформа + виртуальный LAN» и `docs/EMBED.md`. Остаётся (second-tier): ICMP-релей наружу, DMA/offload. |
| ✅ **РЕШЕНО (31 мая 2026)** — VGA graphics, framebuffer | — | `Vm::enable_linear_framebuffer` прописывает boot-protocol `screen_info` (efifb/vesafb) + резервирует регион RAM в e820; ядро биндит efifb прямо из `screen_info` (без VESA BIOS/EFI firmware), fbcon рисует консоль пикселями, хост читает байты обратно и блитит на `<canvas>`. Teeth: `linux_efifb_framebuffer_renders_pixels_milestone` (efifb клеймит наш base, 230 KB ненулевых пикселей). У Alpine `vmlinuz-lts` встроен только efifb (vesafb выпилен) → дефолт EFI; для true-GUI (X/Wayland) ещё нужны 2D/DRM-устройства. |
| 🚧 **GUI (X/DRM) — дисплей разблокирован (2 июн 2026)** | в работе | Цель: графический десктоп в госте. **Recon:** netboot `vmlinuz-lts` имеет только минимум встроенного — fbcon рисует консоль, но юзерспейс-устройства (`/dev/fb0`, `/dev/dri`, `/dev/input/*`) отсутствуют (драйверы — модули в `modloop-lts`, как было с сетью). **✅ Дисплей решён без пересборки ядра:** `fetch-alpine-assets.sh --with-gui` достаёт из modloop замыкание `i2c-core→drm→drm_kms_helper→drm_shmem_helper→simpledrm` (+`evdev`/`mousedev`/`psmouse`); `alpine_console` /init грузит их при `WWWVM_FB`. `simpledrm` биндит `simple-framebuffer.0` (созданный `sysfb` из EFI `screen_info`) → **`/dev/dri/card0`** (устройство для X `modesetting`). `evdev` даёт `/dev/input/event*`. **Осталось:** ввод — 8042 сейчас заглушка (`i8042: Can't read CTR` → built-in `atkbd` не биндится), нужен полный контроллер 8042 + PS/2-мышь (AUX, IRQ12); затем `apk add xorg-server` + `modesetting` + WM. |
| ✅ **GUI ввод — клавиатура + мышь (2 июн 2026)** | — | Переписан `crates/devices/src/keyboard.rs` из заглушки в полноценный контроллер 8042: config-byte (`0x20`/`0x60` — чинит «Can't read CTR»), enable/disable портов (+ CCB clock-биты), протокол клавиатуры (reset→`FA AA`, identify→`FA AB 83`, set-LEDs/typematic), AUX-канал мыши (`0xA8`/`0xD4`, reset→`FA AA 00`, sample-rate knock, reporting, 3-байтные пакеты → **IRQ12**), AUX_LOOP `0xD3`/`0xD2` для детекта порта мыши. Ответы устройств поднимают IRQ (atkbd ждёт их по прерыванию). Teeth (in-guest, lts): `serio i8042 KBD irq 1` + `AUX irq 12`, `input: AT Raw Set 2 keyboard` + `PS/2 Generic Mouse`, **`/dev/input/event0`+`event1`** — то, что нужно X. 11 новых юнит-тестов (110 в devices). |
| 🚧 **GUI — Xorg запускается в госте (3 июн 2026)** | в работе | **X.Org работает в госте на 1280×800**, рисует в `/dev/fb0` (= наш линейный framebuffer = canvas): `xrandr` показывает `1280x800 connected`, `xsetroot -solid` красит root-окно. Рецепт (`docs/GUI_X.md`): `apk add xorg-server xf86-video-fbdev xf86-input-libinput eudev`, `udevd`+`udevadm trigger`, конфиг `Driver "fbdev"` на `/dev/fb0`, `X :0 vt1`. **Драйвер `fbdev`, НЕ `modesetting`**: на Alpine x86 `modesetting_drv.so`/`libglx.so` тянут `libgallium-*.so`, которого нет ни в одном пакете x86-репо → "no screens"; `fbdev` просто `mmap`-ит `/dev/fb0`, без gbm/gallium/GL. Code-фикс: `/init` монтирует `/proc`+`/sys`+`/dev/pts` (без sysfs udev/X не видят устройства → "no screens found"; без devpts `xterm` не может открыть pty). **Полноценный мини-десктоп: `twm` (WM) + `xterm` — оба в дереве окон `xwininfo`, рисуют в `/dev/fb0`.** Осталось: проброс ввода клавиатуры/мыши в браузере (G4). |
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

Должно вывести 788 пройденных тестов на текущий момент. CI
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

### Alpine-образы (сеть, графика)

Быстрый путь — скрипты: `scripts/fetch-alpine-assets.sh [--with-net|--with-gui]`
(ядро + minirootfs + модули NIC/DRM/af_packet), затем `scripts/build-web-images.sh`
(пакует `web/images/` + `manifest.json`, которые выбирает веб-UI). Детальные
ручные стадии бутстрапа Alpine (musl-userspace → apk.static → openrc → apk
в госте) — в [docs/BUILD.md](docs/BUILD.md).

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

(Опционально, но рекомендуется) собрать **готовые образы Alpine** для пикера
в UI — kernel + initramfs'ы + `manifest.json` в `web/images/`:

```bash
scripts/build-web-images.sh            # console + GUI образы (быстро)
scripts/build-web-images.sh --with-x   # + тяжёлый образ с предустановленным X
```

`--with-x` кросс-собирает x86-rootfs с `xorg-server`+`twm`+`xterm` в docker-
контейнере amd64 Alpine (хост не запускает x86 `apk`), выкидывает GL-стек
mesa/llvm (fbdev-драйвер его не использует) и пакует ~130-МиБ образ, который
грузится сразу в десктоп (X на `/dev/fb0` + twm + xterm) — `apk` в госте не
нужен. Нужны docker + сеть; образу нужно ~1 ГиБ гостевой RAM.

`web/images/` в `.gitignore` (большие бинарники). Без этого шага пикер покажет
«no server images», но ручной fallback (свои файлы) и встроенные демки работают.

И поднять статический сервер из корня:

```bash
python3 -m http.server -d web 8080
```

Открыть `http://localhost:8080/`. В панели «Boot Linux / Alpine» выбрать образ
и нажать «Load selected image»:
* **console** — musl-шелл по serial;
* **GUI** — framebuffer + DRM/input-модули (поверх ставится Xorg при сети);
* **X desktop** — Xorg+twm+xterm предустановлены, грузится сразу в десктоп
  (тяжёлый: ~130 МиБ, ~1 ГиБ RAM, медленный старт в wasm).

Сеть в госте (`apk`): поставить галку Networking до буста и запустить
`crates/proxy` (allowlist `dl-cdn.alpinelinux.org:80`) — образы поднимают
`eth0` (10.0.2.15) через in-wasm NAT → WebSocket-relay.

**Раскладка:** слева — скрываемые (кнопка ☰) настройки + отладка; в центре
сверху — **Canvas** (появляется, когда у гостя есть фреймбуфер), под ним —
**UART**-консоль (занимает всё поле, если Canvas нет), снизу — Autorun + отправка
команд. **Клик по Canvas** захватывает мышь и клавиатуру (pointer lock →
PS/2/scancodes), **правый Alt** отпускает; кнопка **Fullscreen** разворачивает
Canvas. У X-десктопа `/init` раз в 2 с перерисовывает весь фреймбуфер (simpledrm
шлёт в видимый scanout только damaged-области — иначе статичные окна застывают).

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
