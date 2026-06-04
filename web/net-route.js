// Hybrid LAN + NAT frame routing — the per-frame decision the worker makes when
// a VM is on the in-page L2 switch (peers) AND behind the in-wasm NAT (the
// outside world) over one NIC. Kept as a pure function so it's node-testable
// (see net-route.test.mjs) and shared as the single source of truth with
// vm-worker.js's pumpHybrid.
//
// The guest's own routing table picks the destination MAC: an off-subnet packet
// goes to the gateway MAC (default route), a same-subnet packet to the peer's
// MAC, and discovery (ARP) is broadcast. So we route by destination MAC:
//   • gateway MAC      → the NAT only (the outside world)
//   • broadcast/mcast  → BOTH: the NAT (it answers gateway ARP/DHCP) AND the
//                        switch (peers must see the broadcast to reply)
//   • any other (peer) → the switch only
//
// Correctness rests on the NAT owning ONLY the gateway IP, so a peer ARP it sees
// via the broadcast path is ignored rather than hijacked — asserted natively in
// crates/net nat.rs::answers_gateway_arp_but_ignores_peer_arp.

// The MAC the in-wasm NAT answers to (matches net_enable_ip's GW_MAC in
// crates/wasm). Keep in sync with that constant.
export const GW_MAC = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];

// A group address (broadcast ff:ff:… or any multicast) has the I/G bit set —
// the least-significant bit of the first octet.
export const isGroupMac = (frame) => (frame[0] & 1) === 1;

export const isGatewayMac = (frame, gw = GW_MAC) =>
  frame[0] === gw[0] && frame[1] === gw[1] && frame[2] === gw[2] &&
  frame[3] === gw[3] && frame[4] === gw[4] && frame[5] === gw[5];

// Classify one transmitted Ethernet frame: "nat" | "both" | "switch".
// `frame` is a Uint8Array (or array) whose first 6 bytes are the destination MAC.
export function classifyFrame(frame, gw = GW_MAC) {
  if (isGatewayMac(frame, gw)) return "nat";
  if (isGroupMac(frame)) return "both";
  return "switch";
}
