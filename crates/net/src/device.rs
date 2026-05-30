//! A smoltcp `phy::Device` backed by the emulated NIC's frame queues.
//!
//! smoltcp drives this device: it pulls guest frames we've pushed in (the
//! frames the VM drained from the NIC's TX ring) and pushes the frames it
//! wants sent (which the VM injects into the NIC's RX ring). No FCS is added
//! or expected in either direction — the RTL8139 model is CRC-less both ways
//! — and frames may be shorter than 60 bytes (we never pad on egress).

use std::collections::VecDeque;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Ethernet MTU for the virtual link.
pub const MTU: usize = 1500;

/// Frame queues bridging smoltcp to the VM's drain/inject API.
pub struct GuestDevice {
    /// Frames from the guest (VM `drain_tx_frames` → here → smoltcp).
    rx: VecDeque<Vec<u8>>,
    /// Frames smoltcp wants to send (here → VM `inject_rx_frame`).
    tx: VecDeque<Vec<u8>>,
}

impl GuestDevice {
    pub fn new() -> Self {
        Self {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
        }
    }

    /// Hand a guest-transmitted frame to smoltcp.
    pub fn push_guest_frame(&mut self, frame: Vec<u8>) {
        self.rx.push_back(frame);
    }

    /// Take the next frame smoltcp produced for the guest, if any.
    pub fn pop_egress(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }

    /// Put a frame back at the front of the egress queue (the NIC RX ring was
    /// full); it must be re-tried before any later frame to preserve order.
    pub fn requeue_egress_front(&mut self, frame: Vec<u8>) {
        self.tx.push_front(frame);
    }

    /// Whether smoltcp has produced frames waiting to be injected.
    pub fn has_egress(&self) -> bool {
        !self.tx.is_empty()
    }
}

impl Default for GuestDevice {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns a guest frame; smoltcp reads it once to process.
pub struct RxToken(Vec<u8>);

/// Holds a mutable handle to the egress queue; smoltcp fills a buffer that we
/// push for the VM to inject.
pub struct TxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for GuestDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }
}
