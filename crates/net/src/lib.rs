//! Host-side networking for wwwvm: the guest↔internet bridge that lets the
//! emulated Alpine guest reach a real (allowlisted) mirror so `apk` works
//! over the network.
//!
//! The guest runs its own full TCP/IP stack and the emulator hands us raw
//! Ethernet frames (`Vm::drain_tx_frames` / `Vm::inject_rx_frame`). This
//! crate terminates the guest's L2/L3/L4 on the host (the "slirp" role) and
//! NATs TCP flows out to real host sockets — see `docs/NET_BRIDGE_DESIGN.md`.
//!
//! It is kept in its own crate so the heavier deps (smoltcp, tokio) stay out
//! of `vm`/`cpu`/`mem`, which remain `#![forbid(unsafe_code)]` and lean.
//!
//! Implemented so far:
//!   * [`Allowlist`] — the shared, deny-by-default connection policy (also
//!     used by `wwwvm-proxy`).
//!
//! Coming next: a DNS forwarder and a smoltcp-based TCP NAT.

#![forbid(unsafe_code)]

pub mod allow;
pub mod dns;
pub mod forwarder;
pub mod util;

pub use allow::Allowlist;
pub use forwarder::DnsForwarder;
