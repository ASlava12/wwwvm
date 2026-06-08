# Building the Alpine guest images — detailed manual stages

Quick path (what you normally use): `scripts/fetch-alpine-assets.sh
[--with-net|--with-gui]` then `scripts/build-web-images.sh` (→ web/images/ +
manifest.json). The stage-by-stage bring-up below (musl userspace → apk.static
→ openrc → in-guest apk) is kept for reference / debugging — you do not need it
for normal use.

### Alpine (на пути к загрузке Alpine) — стадия A: musl-userspace работает

Текущий userspace — Tinycore (busybox + **glibc**). Alpine использует
**musl** (другой libc, другой `ld-musl-i386.so.1`, и busybox — **PIE**
ET_DYN, а не ET_EXEC). Стадия A проверяет, что эмулятор гоняет musl-PIE-
busybox из Alpine на текущем ядре:

**Ассеты (ядро + minirootfs) живут в `/tmp` и вычищаются между сессиями** —
восстановить одной командой: `scripts/fetch-alpine-assets.sh` (добавьте
`--with-net`, чтобы доложить NIC-модули `mii.ko`/`8139too.ko` из `modloop-lts`
для apk-по-сети в госте). Ручные шаги ниже — что скрипт делает под капотом.

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

**Сетевой `apk` РАБОТАЕТ** ✅ — `apk update` из реального зеркала Alpine
проходит: **`OK: 5532 distinct packages available`** (HTTP-fetch через NAT +
проверка RSA-подписи APKINDEX). Строилось по частям, каждая teeth-confirmed
в реальном Alpine:
- **A (NIC) ✅** — эмулированный RTL8139 + PCI host-bridge; стоковые
  `mii.ko`+`8139too.ko` из `modloop-lts` грузятся `insmod` как есть
  (кастомное ядро НЕ нужно). bus-master TX/RX-DMA + IRQ 11 (секция «Сеть»).
- **B1 (L2/L3-шлюз) ✅** — `ping 10.0.2.2` из гостя (0% loss). Изначально
  hand-rolled `lan.rs::VirtualGateway`; теперь ARP/ICMP отдаёт smoltcp.
- **B2 (DNS) ✅** — `crates/net` форвардер на 10.0.2.2:53: имена
  пре-резолвятся на хосте при старте (никаких блокировок VM-петли + нет
  DNS-rebinding); `nslookup` из гостя резолвит, неразрешённое → NXDOMAIN.
- **B3a (TCP-NAT) ✅** — `crates/net` поверх **smoltcp**: ловим SYN, по
  allowlist'у открываем реальный host-сокет к destination, шлём байты
  (TLS остался бы end-to-end). `wget` тянет полный APKINDEX (485109 байт).
- **MMX** ✅ — `apk` (libcrypto) использует MMX (x86-32 baseline без SSE2):
  реализованы mm0-7 + MOVD/MOVQ/PXOR/PADD/PMULUDQ/PSHUFW/PUNPCK/PACK/
  PINSRW/сдвиги/… → и RSA-подпись, и TLS-хеши считаются.
- **`apk add tree` ✅** — ставится из сети и запускается (`tree v2.2.1`,
  «OK: 6 MiB in 16 packages»). Turnkey: при `WWWVM_NET_STUB=1` гость
  настраивается сам (модули/IP/маршрут/resolv.conf), достаточно
  `apk update`/`apk add`.
- **HTTPS (Phase D) ✅** — `apk update`/`apk add` поверх **https://** тоже
  работают: TLS терминируется В ГОСТЕ против настоящего сертификата
  Let's Encrypt, relay гоняет только шифртекст → **end-to-end, без MITM**.
  CA-бандл уже есть в minirootfs. Всё через allowlist
  (`WWWVM_PROXY_ALLOWLIST`, deny-by-default, `*` в проде нельзя).
- **Браузер** — нативный мост (`crates/net`) использует std-сокеты/потоки,
  которых в wasm нет, поэтому в браузере сеть пока НЕ работает. Заложен
  фундамент: `crates/wasm` экспонирует кадры NIC
  (`drain_tx_frame`/`inject_rx_frame`/`run_until_idle`); осталось собрать
  браузерный мост (smoltcp-NAT в wasm + WebSocket-relay к `crates/proxy`) —
  см. `docs/BROWSER_NET.md`.

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

