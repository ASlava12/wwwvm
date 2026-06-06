// Node tests for the snapshot-store export parser — it consumes bytes from a
// possibly-untrusted store, so a malformed manifest must throw a clean Error
// (not RangeError / a runaway loop / a giant allocation). Run via
// scripts/test-web-js.sh.
import { test } from "node:test";
import assert from "node:assert/strict";
import { parseExport, PAGE } from "./snapshot-store.js";

// Build a header (metaLen=0, ramOff=0) with the given ramLen/nPages in a buffer
// of `totalLen` bytes. Bytes past the header are left zero (stand-in hashes/ram).
function header(ramLen, nPages, totalLen) {
  const buf = new Uint8Array(totalLen);
  const dv = new DataView(buf.buffer);
  dv.setUint32(0, 0, true); // metaLen
  dv.setUint32(4, 0, true); // ramOff
  dv.setUint32(8, ramLen, true);
  dv.setUint32(12, nPages, true);
  return buf;
}

test("parseExport accepts consistent empty + 1-page manifests", () => {
  const empty = parseExport(header(0, 0, 16));
  assert.equal(empty.nPages, 0);
  const one = parseExport(header(PAGE, 1, 16 + 32)); // header + 1 hash
  assert.equal(one.nPages, 1);
  assert.equal(one.ramLen, PAGE);
});

test("parseExport rejects malformed/hostile manifests without RangeError/OOM", () => {
  assert.throws(() => parseExport(new Uint8Array(2)), /truncated/); // too short for header
  // Huge nPages field in a tiny buffer must be rejected, not allocated/looped.
  assert.throws(() => parseExport(header(PAGE, 1_000_000, 16)), /exceeds buffer/);
  // ramLen/nPages inconsistency (2 pages claimed for one page of RAM).
  assert.throws(() => parseExport(header(PAGE, 2, 16 + 64)), /inconsistent/);
  // metaLen past the buffer.
  const bad = header(0, 0, 16);
  new DataView(bad.buffer).setUint32(0, 0xffffffff, true); // metaLen = 4 GiB
  assert.throws(() => parseExport(bad), /meta length out of range/);
});
