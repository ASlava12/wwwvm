#!/bin/sh
# Run the browser's pure-logic JS tests (no DOM / wasm / workers needed) and
# syntax-check the web modules. These cover the logic that the Rust gate can't —
# the hybrid frame router (net-route.js) and the L2 switch (l2-switch.js), kept
# in lockstep with their Rust twins in crates/net.
#
# Usage: scripts/test-web-js.sh   (needs Node 18+ for `node --test`)
set -eu

cd "$(dirname "$0")/.."

echo "== syntax-checking web/*.js =="
for f in web/*.js; do
  # ES modules — copy to .mjs so `node --check` parses `import`/`export`.
  tmp="$(mktemp --suffix=.mjs)"
  cp "$f" "$tmp"
  if node --check "$tmp"; then
    echo "  ok  $f"
  else
    echo "  FAIL $f"
    rm -f "$tmp"
    exit 1
  fi
  rm -f "$tmp"
done

echo "== node --test web/*.test.mjs =="
node --test web/*.test.mjs
