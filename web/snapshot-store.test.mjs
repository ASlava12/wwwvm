// Node tests for the snapshot-store export parser — it consumes bytes from a
// possibly-untrusted store, so a malformed manifest must throw a clean Error
// (not RangeError / a runaway loop / a giant allocation). Run via
// scripts/test-web-js.sh.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseExport, PAGE, manifestOf, pageAt, buildExport, uploadSnapshot, downloadSnapshot,
} from "./snapshot-store.js";

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

// A synthetic export matching encode_export's layout:
//   metaLen | meta | ramOff | ramLen | nPages | hashes(32*n) | ram
// meta = [1,2,3,4]; ram = one full 0xAA page + a short 0xBB page (4196 B, 2 pages).
function synthExport() {
  const meta = Uint8Array.of(1, 2, 3, 4);
  const p0 = new Uint8Array(PAGE).fill(0xaa);
  const p1 = new Uint8Array(100).fill(0xbb);
  const ramLen = p0.length + p1.length;
  const h0 = new Uint8Array(32).fill(0x11);
  const h1 = new Uint8Array(32).fill(0x22);
  const out = [];
  const u32 = (n) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, n, true); return b; };
  out.push(u32(meta.length), meta, u32(0), u32(ramLen), u32(2), h0, h1, p0, p1);
  const total = out.reduce((n, a) => n + a.length, 0);
  const buf = new Uint8Array(total);
  let off = 0;
  for (const a of out) { buf.set(a, off); off += a.length; }
  return buf;
}

class MockStore {
  constructor() { this.pages = new Map(); this.manifests = new Map(); }
  async hasPage(h) { return this.pages.has(h); }
  async putPage(h, b) { const had = this.pages.has(h); this.pages.set(h, b.slice()); return !had; }
  async putManifest(id, b) { this.manifests.set(id, b.slice()); }
  async getManifest(id) { return this.manifests.get(id) ?? null; }
  async getPage(h) { return this.pages.get(h) ?? null; }
}

test("export round-trips through parse → page → build", () => {
  const buf = synthExport();
  const parsed = parseExport(buf);
  assert.equal(parsed.nPages, 2);
  assert.equal(parsed.ramLen, PAGE + 100);
  assert.equal(pageAt(buf, parsed, 0).length, PAGE);
  assert.equal(pageAt(buf, parsed, 1).length, 100); // last page short
  const rebuilt = buildExport(manifestOf(buf, parsed), [pageAt(buf, parsed, 0), pageAt(buf, parsed, 1)]);
  assert.deepEqual([...rebuilt], [...buf]);
});

test("export round-trips through a content-addressed store (upload diff → download)", async () => {
  const buf = synthExport();
  const store = new MockStore();
  const up = await uploadSnapshot(store, "task1", buf);
  assert.equal(up.pages, 2);
  assert.equal(up.uploaded, 2); // both pages new
  // Re-upload: every page already present → 0 uploaded (the diff/dedup path).
  assert.equal((await uploadSnapshot(store, "task1", buf)).uploaded, 0);
  const down = await downloadSnapshot(store, "task1");
  assert.deepEqual([...down], [...buf]);
});
