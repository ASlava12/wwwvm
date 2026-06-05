#!/usr/bin/env bash
# Deploy the static web/ front-end to the Beget shared host (https://vm.nas.su).
#
# STATIC ONLY: index.html / lan.html / *.js / wasm / images. The relay
# (wwwvm-proxy) and the snapstore are Rust services and CANNOT run on shared PHP
# hosting, so the in-browser features work (single VM, graphics, Fleet peer-LAN,
# local snapshots) but the Internet (NAT -> relay) mode and the snapshot store
# need a separate VPS — and, since the site is https, a wss:// (TLS) relay.
#
# Auth: SSH key (set one up once with `ssh-copy-id artsl_vm@artsl.beget.tech`).
# No --delete: the host's own dirs (cgi-bin, .cache) and the index.php backup are
# left untouched. .htaccess (wasm MIME + index.html first) is regenerated here.
#
# Usage: scripts/deploy-beget.sh        (override HOST/DEST/URL via env)
set -euo pipefail

HOST="${BEGET_HOST:-artsl_vm@artsl.beget.tech}"
DEST="${BEGET_DEST:-/home/a/artsl/vm.nas.su/public_html}"
URL="${BEGET_URL:-https://vm.nas.su/}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/web"

[ -f "$SRC/pkg/wwwvm_wasm_bg.wasm" ] || {
  echo "build wasm first: wasm-pack build crates/wasm --target web --out-dir ../../web/pkg --release" >&2
  exit 1
}
[ -f "$SRC/images/manifest.json" ] ||
  echo "warn: no web/images — run scripts/build-web-images.sh for the boot images" >&2

ht="$(mktemp)"
cat > "$ht" <<'HT'
# Serve index.html first; correct wasm MIME for streaming compile; gzip text.
DirectoryIndex index.html index.php
AddType application/wasm .wasm
<IfModule mod_deflate.c>
AddOutputFilterByType DEFLATE text/html text/css application/javascript text/javascript application/wasm application/json image/svg+xml
</IfModule>
HT

echo "-> syncing web/ to $HOST:$DEST"
# -rltz (recurse, symlinks, file mtimes for incremental, compress). NOT -a:
# --omit-dir-times + --no-perms avoid chmod/utime on the root-owned docroot "."
# (shared hosting denies that). New files inherit a sane default mode (644).
rsync -rltz --human-readable --omit-dir-times --no-perms \
  --exclude 'package.json' --exclude '*.test.mjs' --exclude '*.d.ts' \
  --exclude '.gitignore' --exclude '.DS_Store' --exclude '.htaccess' \
  "$SRC"/ "$HOST:$DEST"/
scp -q "$ht" "$HOST:$DEST/.htaccess"
rm -f "$ht"

echo "-> verifying $URL"
code=$(curl -s -m 25 -o /dev/null -w '%{http_code}' "$URL" || true)
mime=$(curl -s -m 25 -I "${URL}pkg/wwwvm_wasm_bg.wasm" | awk 'tolower($1)=="content-type:"{print $2}' | tr -d '\r')
echo "   index: HTTP $code   wasm MIME: ${mime:-?}"
if [ "$code" = "200" ] && [ "$mime" = "application/wasm" ]; then
  echo "OK deployed: $URL"
else
  echo "verification failed (index HTTP $code, wasm MIME ${mime:-none})" >&2
  exit 1
fi
