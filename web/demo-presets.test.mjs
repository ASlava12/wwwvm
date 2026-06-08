// Node tests for the demo-preset helpers (no DOM). Run via
// `scripts/test-web-js.sh` (or `node --test web/demo-presets.test.mjs`).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  DEMO_PRESETS,
  pickImageId,
  presetToConfig,
} from "./demo-presets.js";
import { buildHashFromConfig, parseConfigFromHash } from "./demo-link.js";

const MANIFEST = [
  { id: "alpine-3.21", name: "Alpine 3.21 (musl)" },
  { id: "alpine-gui", name: "Alpine GUI (X)" },
  { id: "tinycore", name: "Tiny Core" },
];

test("pickImageId: matches id substring, first hit wins", () => {
  assert.equal(pickImageId(MANIFEST, "alpine"), "alpine-3.21");
  assert.equal(pickImageId(MANIFEST, "gui"), "alpine-gui");
  assert.equal(pickImageId(MANIFEST, "tiny"), "tinycore");
});

test("pickImageId: matches display name too", () => {
  assert.equal(pickImageId(MANIFEST, "core"), "tinycore"); // "Tiny Core"
});

test("pickImageId: no match / empty inputs → ''", () => {
  assert.equal(pickImageId(MANIFEST, "windows"), "");
  assert.equal(pickImageId([], "alpine"), "");
  assert.equal(pickImageId(MANIFEST, ""), "");
  assert.equal(pickImageId(undefined, "alpine"), "");
});

test("presetToConfig: includes set fields, booleans stay boolean, no boot", () => {
  const p = {
    imageMatch: "alpine",
    net: true,
    fb: false,
    allow: "host\n*",
    autorun: "uname -a",
  };
  const cfg = presetToConfig(p, "alpine-3.21");
  assert.deepEqual(cfg, {
    img: "alpine-3.21",
    allow: "host\n*",
    autorun: "uname -a",
    net: true,
    fb: false,
  });
  assert.ok(!("boot" in cfg), "preset never auto-boots");
});

test("presetToConfig: omits image when none resolved; skips empty fields", () => {
  const cfg = presetToConfig({ net: true, cmdline: "" }, "");
  assert.deepEqual(cfg, { net: true });
});

test("presetToConfig: null/garbage preset → {}", () => {
  assert.deepEqual(presetToConfig(null, "x"), {});
  assert.deepEqual(presetToConfig(undefined, ""), {});
});

test("every built-in preset round-trips through demo-link (build → parse)", () => {
  for (const p of DEMO_PRESETS) {
    const img = pickImageId(MANIFEST, p.imageMatch);
    const cfg = presetToConfig(p, img);
    const parsed = parseConfigFromHash(buildHashFromConfig(cfg));
    // Every preset resolves an alpine image from our test manifest.
    assert.equal(parsed.img, "alpine-3.21", `${p.id} resolves an image`);
    // autorun survives encoding (presets all set one).
    assert.equal(parsed.autorun, p.autorun, `${p.id} autorun round-trips`);
    // net is reflected as "1" only when the preset asked for it.
    if (p.net) assert.equal(parsed.net, "1", `${p.id} net=1`);
    else assert.ok(!("net" in parsed), `${p.id} omits net when false`);
  }
});

test("built-in presets have unique ids + labels", () => {
  const ids = DEMO_PRESETS.map((p) => p.id);
  assert.equal(new Set(ids).size, ids.length, "unique ids");
  for (const p of DEMO_PRESETS) {
    assert.ok(p.label && p.note, `${p.id} has a label + note`);
  }
});
