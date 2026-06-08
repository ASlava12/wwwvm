// Shareable boot config in the URL hash. A link like
//   index.html#img=alpine&autorun=uname%20-a&net=1&boot=1
// pre-fills the controls (and, with boot=1, auto-boots) so a whole demo
// scenario is one shareable URL — the point of the showcase page.
//
// Kept as pure functions (no DOM) so they're node-testable the same way as
// net-route.js / snapshot-store.js (see demo-link.test.mjs). main.js owns the
// DOM glue: read the hash on load → apply to controls; "Share" → build a hash
// from the current controls.

// The config keys we round-trip. Order is fixed so generated links are stable
// and diffable. Anything not in this list is ignored on parse + skipped on
// build (so a stray `#access_token=…` from an OAuth redirect can't inject UI
// state, and future keys are opt-in).
export const DEMO_FIELDS = [
  "img", // server image id (the <select> value)
  "cmdline", // kernel command line
  "ram", // guest RAM (MiB)
  "ramdisk", // tmpfs RAM disk (MiB)
  "fb", // framebuffer on/off (boolean)
  "fbres", // framebuffer resolution ("800x600")
  "net", // networking on/off (boolean)
  "allow", // net allowlist (one host[:port] per line)
  "autorun", // shell commands to run once booted (one per line)
  "boot", // auto-boot once applied (boolean)
];

// Parse "#k=v&k2=v2" (leading # optional) into an object of decoded string
// values. Unknown keys are dropped; a bare "k" (no "=") yields "". `+` decodes
// to a space (form convention) so a space in autorun survives either encoding.
// A malformed %-escape falls back to the raw text rather than throwing.
export function parseConfigFromHash(hash) {
  const out = {};
  if (typeof hash !== "string") return out;
  const s = hash.replace(/^#/, "");
  if (!s) return out;
  for (const part of s.split("&")) {
    if (!part) continue;
    const eq = part.indexOf("=");
    const rawK = eq === -1 ? part : part.slice(0, eq);
    const rawV = eq === -1 ? "" : part.slice(eq + 1);
    let k, v;
    try {
      k = decodeURIComponent(rawK);
    } catch {
      k = rawK;
    }
    try {
      v = decodeURIComponent(rawV.replace(/\+/g, " "));
    } catch {
      v = rawV;
    }
    if (DEMO_FIELDS.includes(k)) out[k] = v;
  }
  return out;
}

// Build "#k=v&…" from a config object, including only known, non-empty fields.
// Booleans render as "1" and are emitted only when true (false is the default,
// omitted to keep links short). Returns "" when nothing is set (so the caller
// can avoid leaving a bare "#" on the URL).
export function buildHashFromConfig(config) {
  if (!config || typeof config !== "object") return "";
  const parts = [];
  for (const k of DEMO_FIELDS) {
    if (!(k in config)) continue;
    let v = config[k];
    if (v === undefined || v === null) continue;
    if (typeof v === "boolean") {
      if (!v) continue;
      v = "1";
    }
    v = String(v);
    if (v === "") continue;
    parts.push(encodeURIComponent(k) + "=" + encodeURIComponent(v));
  }
  return parts.length ? "#" + parts.join("&") : "";
}
