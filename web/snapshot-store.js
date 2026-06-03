// Browser side of the custom-snapshot platform: parse the `snapshot_export()`
// buffer the wasm VM produces, and talk to the content-addressed store
// (crates/snapstore). Pages are named by the blake3 hash Rust computed (Web
// Crypto has no blake3), so JS never hashes — it just slices and ships bytes.
//
// Export buffer format (must match wwwvm_vm::paged::encode_export; LE u32):
//   meta_len | meta[meta_len] | ram_off | ram_len | n_pages
//   | hashes[n_pages*32] | ram[ram_len]
// The bytes up to `ram` are the MANIFEST (stored verbatim); page i is
// ram[i*PAGE ..] (last may be short) named by hashes[i].

export const PAGE = 4096;

const toHex = (bytes) => {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
};

// Parse the header of an export (or a standalone manifest — it only reads up to
// the page region). Returns offsets + the per-page hash list.
export function parseExport(buf) {
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let p = 0;
  const metaLen = dv.getUint32(p, true); p += 4;
  const meta = buf.subarray(p, p + metaLen); p += metaLen;
  const ramOff = dv.getUint32(p, true); p += 4;
  const ramLen = dv.getUint32(p, true); p += 4;
  const nPages = dv.getUint32(p, true); p += 4;
  const hashes = [];
  for (let i = 0; i < nPages; i++) {
    hashes.push(toHex(buf.subarray(p, p + 32)));
    p += 32;
  }
  return { meta, ramOff, ramLen, nPages, hashes, pagesStart: p };
}

// The manifest bytes (header up to the RAM page region) — what gets stored.
export function manifestOf(buf, parsed = parseExport(buf)) {
  return buf.subarray(0, parsed.pagesStart);
}

// Bytes of page `i` (the last page may be shorter than PAGE).
export function pageAt(buf, parsed, i) {
  const start = parsed.pagesStart + i * PAGE;
  const end = Math.min(start + PAGE, parsed.pagesStart + parsed.ramLen);
  return buf.subarray(start, end);
}

// Rebuild a full export buffer from a stored manifest + its pages (in hash
// order) — feed to WwwVm.restore_export(). Pages are Uint8Arrays.
export function buildExport(manifestBytes, pages) {
  let total = manifestBytes.length;
  for (const pg of pages) total += pg.length;
  const out = new Uint8Array(total);
  out.set(manifestBytes, 0);
  let off = manifestBytes.length;
  for (const pg of pages) {
    out.set(pg, off);
    off += pg.length;
  }
  return out;
}

// Thin client for crates/snapstore. `base` is the store's URL (or "" for
// same-origin, e.g. behind Caddy at /snap). `token` is the admin token (only
// needed for uploads). Reads are open.
export class SnapStore {
  constructor(base = "", token = "") {
    this.base = base.replace(/\/$/, "");
    this.token = token;
  }
  _auth() {
    return this.token ? { Authorization: `Bearer ${this.token}` } : {};
  }
  async hasPage(hex) {
    const r = await fetch(`${this.base}/pages/${hex}`, { method: "HEAD" });
    return r.ok;
  }
  // Returns true if newly stored, false if it already existed.
  async putPage(hex, bytes) {
    const r = await fetch(`${this.base}/pages/${hex}`, {
      method: "PUT", headers: this._auth(), body: bytes,
    });
    if (!r.ok) throw new Error(`putPage ${hex.slice(0, 8)}…: HTTP ${r.status}`);
    return r.status === 201;
  }
  async putManifest(id, bytes) {
    const r = await fetch(`${this.base}/manifests/${encodeURIComponent(id)}`, {
      method: "PUT", headers: this._auth(), body: bytes,
    });
    if (!r.ok) throw new Error(`putManifest ${id}: HTTP ${r.status}`);
  }
  async getManifest(id) {
    const r = await fetch(`${this.base}/manifests/${encodeURIComponent(id)}`);
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`getManifest ${id}: HTTP ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  }
  async getPage(hex) {
    const r = await fetch(`${this.base}/pages/${hex}`);
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`getPage ${hex.slice(0, 8)}…: HTTP ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  }
  async listManifests() {
    const r = await fetch(`${this.base}/manifests`);
    if (!r.ok) throw new Error(`listManifests: HTTP ${r.status}`);
    return r.json();
  }
}

// Upload a snapshot export to the store as `id`: PUT only the pages the store
// lacks (the diff — most are shared with the base), then the manifest. Calls
// onProgress(uploaded, total) as it goes. Returns {pages, uploaded, bytes}.
export async function uploadSnapshot(store, id, exportBuf, onProgress) {
  const parsed = parseExport(exportBuf);
  let uploaded = 0, bytes = 0;
  // Dedup within this snapshot too (identical pages share a hash).
  const seen = new Set();
  for (let i = 0; i < parsed.nPages; i++) {
    const hex = parsed.hashes[i];
    if (seen.has(hex)) continue;
    seen.add(hex);
    if (!(await store.hasPage(hex))) {
      const pg = pageAt(exportBuf, parsed, i);
      await store.putPage(hex, pg);
      uploaded++;
      bytes += pg.length;
    }
    if (onProgress) onProgress(i + 1, parsed.nPages);
  }
  await store.putManifest(id, manifestOf(exportBuf, parsed));
  return { pages: parsed.nPages, uploaded, bytes };
}

// Fetch a stored snapshot's manifest + all its pages and rebuild the export
// buffer for WwwVm.restore_export(). Pages come from the content store by hash.
export async function downloadSnapshot(store, id) {
  const manifestBytes = await store.getManifest(id);
  if (!manifestBytes) throw new Error(`no snapshot "${id}"`);
  const parsed = parseExport(manifestBytes);
  const cache = new Map();
  const pages = [];
  for (const hex of parsed.hashes) {
    let pg = cache.get(hex);
    if (!pg) {
      pg = await store.getPage(hex);
      if (!pg) throw new Error(`snapshot "${id}" missing page ${hex.slice(0, 8)}…`);
      cache.set(hex, pg);
    }
    pages.push(pg);
  }
  return buildExport(manifestBytes, pages);
}
