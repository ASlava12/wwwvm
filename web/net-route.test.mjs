// Node tests for the browser's pure networking-logic modules (no DOM / wasm /
// workers): the hybrid frame router and the L2 switch. Run via
// `scripts/test-web-js.sh` (or `node --test web/net-route.test.mjs`).
//
// These mirror the Rust unit tests for the same logic (crates/net switch.rs +
// nat.rs ARP ownership) so browser and native stay in lockstep.
import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyFrame, GW_MAC, isGroupMac } from "./net-route.js";
import { L2Switch } from "./l2-switch.js";

// 14-byte minimal Ethernet header: dst, src, ethertype.
const frame = (dst, src) =>
  Uint8Array.from([...dst, ...src, 0x08, 0x00]);
const PEER = [0x52, 0x54, 0x00, 0x00, 0x00, 0x03];
const BCAST = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
const MCAST = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
const SRC = [0x52, 0x54, 0x00, 0x00, 0x00, 0x01];

test("classifyFrame: gateway MAC → nat", () => {
  assert.equal(classifyFrame(frame(GW_MAC, SRC)), "nat");
});

test("classifyFrame: broadcast + multicast → both", () => {
  assert.equal(classifyFrame(frame(BCAST, SRC)), "both");
  assert.equal(classifyFrame(frame(MCAST, SRC)), "both");
});

test("classifyFrame: peer unicast → switch", () => {
  assert.equal(classifyFrame(frame(PEER, SRC)), "switch");
});

test("classifyFrame: a custom gateway MAC is honoured", () => {
  const gw = [0x02, 0, 0, 0, 0, 0x99];
  assert.equal(classifyFrame(frame(gw, SRC), gw), "nat");
  assert.equal(classifyFrame(frame(GW_MAC, SRC), gw), "switch"); // not the gw now
});

test("VM NIC MACs never collide with the gateway (route as switch, not nat)", () => {
  // lan.js assigns VM NIC MACs as 52:54:00:00:01:(i+1); none may equal GW_MAC
  // (52:54:00:00:00:02) or peer unicast would be misrouted into the NAT.
  for (let i = 0; i < 8; i++) {
    const mac = [0x52, 0x54, 0x00, 0x00, 0x01, i + 1];
    assert.equal(classifyFrame(frame(mac, SRC)), "switch", `VM ${i + 1} MAC must not be the gateway`);
  }
});

test("isGroupMac: I/G bit of octet 0", () => {
  assert.equal(isGroupMac(BCAST), true);
  assert.equal(isGroupMac(MCAST), true);
  assert.equal(isGroupMac(PEER), false);
});

// --- L2 switch (mirrors crates/net switch.rs) ---

test("L2Switch: unknown unicast floods, learned unicast is direct", () => {
  const sw = new L2Switch();
  const A = [0x02, 0, 0, 0, 0, 0x0a];
  const B = [0x02, 0, 0, 0, 0, 0x0b];
  // Port 0 sends from A → switch learns A@0; B unknown → flood to all but 0.
  assert.deepEqual(sw.egress(0, frame(B, A), 3), [1, 2]);
  // Port 1 sends to A → now known at port 0 → unicast there.
  assert.deepEqual(sw.egress(1, frame(A, B), 3), [0]);
});

test("L2Switch: broadcast floods, dst-on-ingress drops", () => {
  const sw = new L2Switch();
  const A = [0x02, 0, 0, 0, 0, 0x0a];
  assert.deepEqual(sw.egress(0, frame(BCAST, A), 3), [1, 2]);
  // A is learned at port 0; a frame arriving on port 0 *for* A must drop (don't
  // echo back to the ingress port).
  assert.deepEqual(sw.egress(0, frame(A, [0x02, 0, 0, 0, 0, 0x0c]), 3), []);
});
