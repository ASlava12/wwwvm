// Node tests for the PS/2 Set-1 scancode mapping (the canvas → guest 8042
// keyboard path). Pure functions, no DOM. Run via scripts/test-web-js.sh.
import { test } from "node:test";
import assert from "node:assert/strict";
import { makeBytes, breakBytes, comboBytes } from "./ps2-keymap.js";

test("makeBytes: single-byte, extended, and unknown", () => {
  assert.deepEqual(makeBytes("KeyA"), [0x1e]);
  assert.deepEqual(makeBytes("ArrowUp"), [0xe0, 0x48]); // extended (0xE0 prefix)
  assert.equal(makeBytes("NoSuchKey"), null);
});

test("breakBytes: 0x80 release bit on the final byte, prefix kept", () => {
  assert.deepEqual(breakBytes("KeyA"), [0x9e]); // 0x1e | 0x80
  assert.deepEqual(breakBytes("ArrowUp"), [0xe0, 0xc8]); // prefix + (0x48 | 0x80)
  assert.equal(breakBytes("NoSuchKey"), null);
});

test("comboBytes: press in order, release in reverse, skip unknowns", () => {
  // Ctrl+Alt+Del: Ctrl↓ Alt↓ Del↓  then  Del↑ Alt↑ Ctrl↑.
  assert.deepEqual(
    comboBytes(["ControlLeft", "AltLeft", "Delete"]),
    [0x1d, 0x38, 0xe0, 0x53, 0xe0, 0xd3, 0xb8, 0x9d],
  );
  // Unknown code contributes nothing to either phase.
  assert.deepEqual(comboBytes(["KeyA", "Bogus"]), [0x1e, 0x9e]);
});
