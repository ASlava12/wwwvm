//! A learning Ethernet (L2) switch for connecting several guest VMs into one
//! virtual LAN, so parallel VMs can talk to each other directly (ARP, DHCP,
//! ping, any protocol) — not just reach the outside world through the NAT.
//!
//! This is pure forwarding logic: each VM's NIC is a numbered *port*. Feed a
//! frame that arrived on a port to [`Switch::route`] (or [`Switch::egress`])
//! and it returns where to deliver it. The driver (in-browser hub, or a native
//! harness) does the actual moving of bytes between NIC seams. Keeping the
//! decision logic separate makes it unit-testable without any NIC/worker glue.
//!
//! Behaviour is a textbook learning bridge: the source MAC of every frame is
//! associated with its ingress port (so later unicasts to that MAC go straight
//! there); a frame to an unknown unicast, or to a broadcast/multicast address,
//! is flooded to every other port; a frame whose destination is known to live
//! on the ingress port is dropped (it's already there).

use std::collections::HashMap;

/// Where a frame should go after the switch inspects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forward {
    /// Deliver to exactly this port (a known unicast destination).
    Unicast(usize),
    /// Deliver to every port except the ingress one (unknown unicast, or a
    /// broadcast/multicast destination).
    Flood,
    /// Deliver nowhere: the frame was malformed (too short), or its destination
    /// is already on the ingress port.
    Drop,
}

/// A learning L2 switch: a MAC→port forwarding table built from observed
/// traffic. Cheap to clone-free reuse; one per virtual LAN.
#[derive(Debug, Default)]
pub struct Switch {
    table: HashMap<[u8; 6], usize>,
    /// Bound the table so a flood of spoofed source MACs can't grow it without
    /// limit; on overflow we stop learning (still forward correctly by flooding
    /// unknowns). Plenty for a training LAN of a few VMs.
    cap: usize,
}

/// True for a group address (broadcast `ff:ff:…` or any multicast): the I/G bit
/// is the least-significant bit of the first octet.
fn is_group(mac: &[u8; 6]) -> bool {
    mac[0] & 1 == 1
}

impl Switch {
    /// A new switch with a sensible table cap (1024 MACs).
    pub fn new() -> Self {
        Self {
            table: HashMap::new(),
            cap: 1024,
        }
    }

    /// Learn the source MAC of a frame seen on `in_port`, then decide where the
    /// frame should be delivered. Frames shorter than a 14-byte Ethernet header
    /// are dropped.
    pub fn route(&mut self, in_port: usize, frame: &[u8]) -> Forward {
        if frame.len() < 14 {
            return Forward::Drop;
        }
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();

        // Learn the source (unicast only — a group src is illegal and never an
        // actual host). Don't grow past the cap, but DO refresh a known MAC's
        // port so a VM that moved ports (restart) is relearned.
        if !is_group(&src) && (self.table.len() < self.cap || self.table.contains_key(&src)) {
            self.table.insert(src, in_port);
        }

        if is_group(&dst) {
            return Forward::Flood; // broadcast / multicast → everyone
        }
        match self.table.get(&dst) {
            Some(&p) if p == in_port => Forward::Drop, // already on this port
            Some(&p) => Forward::Unicast(p),
            None => Forward::Flood, // unknown unicast → flood
        }
    }

    /// Convenience wrapper over [`Switch::route`]: returns the concrete list of
    /// egress ports for a frame arriving on `in_port`, given `num_ports` total.
    /// A `Unicast` to an out-of-range port yields an empty list (dropped).
    pub fn egress(&mut self, in_port: usize, frame: &[u8], num_ports: usize) -> Vec<usize> {
        match self.route(in_port, frame) {
            Forward::Drop => Vec::new(),
            Forward::Unicast(p) if p < num_ports => vec![p],
            Forward::Unicast(_) => Vec::new(),
            Forward::Flood => (0..num_ports).filter(|&p| p != in_port).collect(),
        }
    }

    /// Forget a port's learned MACs — call when a VM detaches so stale entries
    /// don't misforward to a dead port.
    pub fn forget_port(&mut self, port: usize) {
        self.table.retain(|_, &mut p| p != port);
    }

    /// Number of learned MAC→port entries (for diagnostics/tests).
    pub fn learned(&self) -> usize {
        self.table.len()
    }
}

/// One attachment point on the [`Hub`] — a VM's NIC. `Vm` implements this via
/// its `drain_tx_frames`/`inject_rx_frame` seam; the in-browser hub mirrors the
/// same two calls per worker over postMessage.
pub trait L2Port {
    /// Take every Ethernet frame the guest has transmitted since the last call.
    fn drain_tx(&mut self) -> Vec<Vec<u8>>;
    /// Deliver one inbound frame to the guest's NIC. Returns false if dropped
    /// (RX disabled / ring full).
    fn inject_rx(&mut self, frame: &[u8]) -> bool;
}

/// Drives a [`Switch`] over a set of [`L2Port`]s: one `pump` drains every port's
/// transmitted frames, routes each through the switch, and injects it into the
/// destination port(s). The port's index in the slice is its switch port number.
#[derive(Debug, Default)]
pub struct Hub {
    switch: Switch,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            switch: Switch::new(),
        }
    }

    /// Move one round of frames between ports. Drains all TX first, then injects
    /// — so a frame transmitted this round is delivered this round, and the
    /// two-pass split keeps the borrow of `ports` non-aliasing.
    pub fn pump<P: L2Port>(&mut self, ports: &mut [P]) {
        let n = ports.len();
        // Pass 1: collect (egress_port, frame) deliveries.
        let mut deliveries: Vec<(usize, Vec<u8>)> = Vec::new();
        for (i, port) in ports.iter_mut().enumerate() {
            for f in port.drain_tx() {
                for eg in self.switch.egress(i, &f, n) {
                    deliveries.push((eg, f.clone())); // flood → one clone per egress
                }
            }
        }
        // Pass 2: deliver.
        for (port, frame) in deliveries {
            ports[port].inject_rx(&frame);
        }
    }

    /// The underlying switch (for diagnostics, or to `forget_port` on detach).
    pub fn switch_mut(&mut self) -> &mut Switch {
        &mut self.switch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x0a];
    const B: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x0b];
    const BCAST: [u8; 6] = [0xff; 6];

    /// Build a minimal Ethernet frame (dst, src, ethertype, 0 payload).
    fn frame(dst: [u8; 6], src: [u8; 6]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&[0x08, 0x00]); // IPv4 ethertype
        f
    }

    #[test]
    fn unknown_unicast_floods_then_learns() {
        let mut sw = Switch::new();
        // A (port 0) → B: B unknown, so flood; A is now learned on port 0.
        assert_eq!(sw.route(0, &frame(B, A)), Forward::Flood);
        assert_eq!(sw.learned(), 1);
        // B (port 1) → A: A is known on port 0 → unicast there.
        assert_eq!(sw.route(1, &frame(A, B)), Forward::Unicast(0));
        // Now A → B: B was learned on port 1 → unicast there.
        assert_eq!(sw.route(0, &frame(B, A)), Forward::Unicast(1));
    }

    #[test]
    fn broadcast_always_floods() {
        let mut sw = Switch::new();
        assert_eq!(sw.route(0, &frame(BCAST, A)), Forward::Flood);
        // multicast (I/G bit set) too
        let mcast = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
        assert_eq!(sw.route(0, &frame(mcast, A)), Forward::Flood);
    }

    #[test]
    fn dst_on_ingress_port_is_dropped() {
        let mut sw = Switch::new();
        sw.route(0, &frame(B, A)); // learn A on 0
        sw.route(0, &frame(B, B)); // learn B on 0 too (both on port 0)
                                   // A → B, both on port 0 → drop (already there)
        assert_eq!(sw.route(0, &frame(B, A)), Forward::Drop);
    }

    #[test]
    fn short_frame_dropped() {
        let mut sw = Switch::new();
        assert_eq!(sw.route(0, &[0u8; 10]), Forward::Drop);
    }

    #[test]
    fn egress_expands_flood_and_unicast() {
        let mut sw = Switch::new();
        // unknown → flood to all but ingress (port 1) across 3 ports
        assert_eq!(sw.egress(1, &frame(B, A), 3), vec![0, 2]);
        // now A learned on 1; B→A unicasts to 1
        assert_eq!(sw.egress(0, &frame(A, B), 3), vec![1]);
    }

    #[test]
    fn forget_port_drops_its_macs() {
        let mut sw = Switch::new();
        sw.route(0, &frame(B, A)); // A on 0
        sw.route(1, &frame(A, B)); // B on 1
        assert_eq!(sw.learned(), 2);
        sw.forget_port(0);
        assert_eq!(sw.learned(), 1);
        // A is forgotten → A as dst now floods again
        assert_eq!(sw.route(1, &frame(A, B)), Forward::Flood);
    }

    #[test]
    fn group_source_is_not_learned() {
        let mut sw = Switch::new();
        // A frame with a broadcast SOURCE (illegal) must not pollute the table.
        sw.route(0, &frame(B, BCAST));
        assert_eq!(sw.learned(), 0);
    }

    /// A test port: a queue of frames to "transmit" and a log of injected ones.
    #[derive(Default)]
    struct MockPort {
        tx: Vec<Vec<u8>>,
        rx: Vec<Vec<u8>>,
    }
    impl L2Port for MockPort {
        fn drain_tx(&mut self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.tx)
        }
        fn inject_rx(&mut self, f: &[u8]) -> bool {
            self.rx.push(f.to_vec());
            true
        }
    }

    /// Broadcast from one port reaches all others (not the sender); a follow-up
    /// unicast to the now-learned source goes only to that port.
    #[test]
    fn hub_floods_broadcast_then_unicasts_learned() {
        let mut hub = Hub::new();
        let mut ports = [
            MockPort::default(),
            MockPort::default(),
            MockPort::default(),
        ];
        // Port 0 broadcasts (announcing A).
        ports[0].tx.push(frame(BCAST, A));
        hub.pump(&mut ports);
        assert!(ports[0].rx.is_empty(), "sender doesn't get its own frame");
        assert_eq!(ports[1].rx.len(), 1, "flooded to 1");
        assert_eq!(ports[2].rx.len(), 1, "flooded to 2");

        // Port 1 replies to A — A was learned on port 0, so only port 0 hears it.
        ports[1].tx.push(frame(A, B));
        hub.pump(&mut ports);
        assert_eq!(ports[0].rx.len(), 1, "unicast reached port 0");
        assert_eq!(
            ports[2].rx.len(),
            1,
            "port 2 unchanged (still just the bcast)"
        );
    }
}
