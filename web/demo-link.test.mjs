// Node tests for the shareable-demo-link logic (no DOM). Run via
// `scripts/test-web-js.sh` (or `node --test web/demo-link.test.mjs`).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseConfigFromHash,
  buildHashFromConfig,
  DEMO_FIELDS,
} from "./demo-link.js";

test("parse: basic key=value pairs, leading # optional", () => {
  assert.deepEqual(parseConfigFromHash("#img=alpine&ram=512"), {
    img: "alpine",
    ram: "512",
  });
  assert.deepEqual(parseConfigFromHash("img=alpine&ram=512"), {
    img: "alpine",
    ram: "512",
  });
});

test("parse: empty / non-string → {}", () => {
  assert.deepEqual(parseConfigFromHash(""), {});
  assert.deepEqual(parseConfigFromHash("#"), {});
  assert.deepEqual(parseConfigFromHash(undefined), {});
  assert.deepEqual(parseConfigFromHash(null), {});
});

test("parse: unknown keys are dropped (no UI injection from stray params)", () => {
  assert.deepEqual(parseConfigFromHash("#img=x&access_token=secret&foo=bar"), {
    img: "x",
  });
});

test("parse: %-decoding, + → space, newlines in autorun", () => {
  assert.equal(parseConfigFromHash("#autorun=uname%20-a").autorun, "uname -a");
  assert.equal(parseConfigFromHash("#autorun=uname+-a").autorun, "uname -a");
  assert.equal(
    parseConfigFromHash("#autorun=echo%20hi%0Auname%20-a").autorun,
    "echo hi\nuname -a",
  );
});

test("parse: bare key → empty string; later duplicate wins", () => {
  assert.deepEqual(parseConfigFromHash("#net"), { net: "" });
  assert.equal(parseConfigFromHash("#ram=256&ram=512").ram, "512");
});

test("parse: malformed %-escape falls back to raw, no throw", () => {
  assert.equal(parseConfigFromHash("#cmdline=%E0%A4%A").cmdline, "%E0%A4%A");
});

test("build: only known non-empty fields, booleans as truthy-only '1'", () => {
  assert.equal(
    buildHashFromConfig({ img: "alpine", net: true, fb: false, cmdline: "" }),
    "#img=alpine&net=1",
  );
  assert.equal(buildHashFromConfig({}), "");
  assert.equal(buildHashFromConfig({ boot: false }), "");
});

test("build: field order follows DEMO_FIELDS (stable links)", () => {
  // Supply out of order; expect canonical order img < net < autorun.
  const h = buildHashFromConfig({ autorun: "x", net: true, img: "a" });
  assert.equal(h, "#img=a&net=1&autorun=x");
  // Sanity: the canonical order is exactly DEMO_FIELDS.
  assert.ok(DEMO_FIELDS.indexOf("img") < DEMO_FIELDS.indexOf("net"));
});

test("build: encodes spaces and newlines", () => {
  assert.equal(
    buildHashFromConfig({ autorun: "echo hi\nuname -a" }),
    "#autorun=echo%20hi%0Auname%20-a",
  );
});

test("round-trip: build → parse preserves set fields", () => {
  const cfg = {
    img: "alpine-3.21",
    cmdline: "console=ttyS0 quiet",
    ram: "512",
    fb: true,
    fbres: "1024x768",
    net: true,
    allow: "dl-cdn.alpinelinux.org\n*",
    autorun: "apk add python3\npython3 -V",
    boot: true,
  };
  const parsed = parseConfigFromHash(buildHashFromConfig(cfg));
  assert.deepEqual(parsed, {
    img: "alpine-3.21",
    cmdline: "console=ttyS0 quiet",
    ram: "512",
    fb: "1", // booleans come back as "1"
    fbres: "1024x768",
    net: "1",
    allow: "dl-cdn.alpinelinux.org\n*",
    autorun: "apk add python3\npython3 -V",
    boot: "1",
  });
});
