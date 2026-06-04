// In-page learning L2 switch — the browser twin of crates/net/src/switch.rs,
// for connecting several worker VMs into one virtual LAN. Each VM's NIC is a
// numbered port; the hub drains each VM's transmitted Ethernet frames
// (WwwVm.drain_tx_frame), routes them here, and injects them into the
// destination VM(s) (WwwVm.inject_rx_frame).
//
// Pure forwarding logic (no worker/postMessage glue), so it's node-testable and
// mirrors the Rust unit tests one-to-one.

const hex6 = (mac) => {
  let s = "";
  for (let i = 0; i < 6; i++) s += mac[i].toString(16).padStart(2, "0");
  return s;
};

// Group address (broadcast ff:ff:… or any multicast): I/G bit = LSB of octet 0.
const isGroup = (mac) => (mac[0] & 1) === 1;

export class L2Switch {
  constructor(cap = 1024) {
    this.table = new Map(); // mac-hex → port
    this.cap = cap;
  }

  // Learn the source, then decide delivery for a frame seen on `inPort`.
  // Returns { kind: "unicast", port } | { kind: "flood" } | { kind: "drop" }.
  route(inPort, frame) {
    if (frame.length < 14) return { kind: "drop" };
    const dst = frame.subarray(0, 6);
    const src = frame.subarray(6, 12);
    if (!isGroup(src)) {
      const k = hex6(src);
      if (this.table.size < this.cap || this.table.has(k)) this.table.set(k, inPort);
    }
    if (isGroup(dst)) return { kind: "flood" };
    const p = this.table.get(hex6(dst));
    if (p === undefined) return { kind: "flood" };
    if (p === inPort) return { kind: "drop" };
    return { kind: "unicast", port: p };
  }

  // Concrete egress port list for a frame arriving on `inPort`, given numPorts.
  egress(inPort, frame, numPorts) {
    const r = this.route(inPort, frame);
    if (r.kind === "drop") return [];
    if (r.kind === "unicast") return r.port < numPorts ? [r.port] : [];
    const out = [];
    for (let p = 0; p < numPorts; p++) if (p !== inPort) out.push(p);
    return out;
  }

  // Forget a detached port's learned MACs so they don't misforward.
  forgetPort(port) {
    for (const [k, p] of this.table) if (p === port) this.table.delete(k);
  }

  get learned() {
    return this.table.size;
  }
}

// Move one round of frames between ports. `ports[i]` must expose
// drainTx() → array of Uint8Array frames the VM sent, and injectRx(frame).
// Two passes (drain all, then deliver) so a frame sent this round is delivered
// this round; flood copies to each egress port.
export function pump(sw, ports) {
  const n = ports.length;
  const deliveries = []; // [port, frame]
  for (let i = 0; i < n; i++) {
    for (const f of ports[i].drainTx()) {
      for (const eg of sw.egress(i, f, n)) deliveries.push([eg, f]);
    }
  }
  for (const [port, frame] of deliveries) ports[port].injectRx(frame);
}
