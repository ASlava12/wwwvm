//! Per-connection host-socket relay.
//!
//! When the guest opens a TCP connection, the NAT spawns one of these: a real
//! host `TcpStream` to the destination, shuttled to/from the guest's smoltcp
//! socket over two bounded channels. Bounded channels give end-to-end flow
//! control for free — if the guest's window closes the NAT stops draining
//! `from_host`, the reader thread blocks on `send`, and the host TCP window
//! closes; symmetrically for the other direction.
//!
//! Each connection gets its OWN channels (never one shared channel across
//! connections), so a stalled flow can't head-of-line-block the others.
//! Blocking I/O lives entirely on these spawned threads; the single-threaded
//! VM loop only ever does non-blocking `try_send`/`try_recv`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

/// Channel depth (frames of ≤ a TCP segment) in each direction.
const CHAN_DEPTH: usize = 16;
/// How long to wait for the host connect before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The NAT's handle to one relayed connection.
pub struct HostConn {
    /// Guest → host payload bytes. `try_send`; full ⇒ apply backpressure
    /// (leave the bytes in the smoltcp socket). Dropping it half-closes the
    /// host write side.
    pub to_host: SyncSender<Vec<u8>>,
    /// Host → guest payload bytes. `try_recv`; `Disconnected` ⇒ the host
    /// closed (EOF) or never connected ⇒ close the guest socket.
    pub from_host: Receiver<Vec<u8>>,
}

/// Open a host connection to `dst:port` and start shuttling bytes. Returns
/// immediately; the actual connect happens on the spawned thread (a connect
/// failure simply drops `from_host`, which the NAT reads as "close the guest
/// side").
pub fn spawn(dst: Ipv4Addr, port: u16) -> HostConn {
    let (to_host_tx, to_host_rx) = mpsc::sync_channel::<Vec<u8>>(CHAN_DEPTH);
    let (from_host_tx, from_host_rx) = mpsc::sync_channel::<Vec<u8>>(CHAN_DEPTH);

    thread::spawn(move || {
        let addr = SocketAddr::from((dst, port));
        let stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => s,
            Err(_) => return, // drops from_host_tx → NAT closes the guest socket
        };
        let write_half = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };

        // Writer: guest → host. Ends (and half-closes the host write side)
        // when the NAT drops `to_host` (the guest sent FIN) or a write fails.
        let writer = thread::spawn(move || {
            let mut w = write_half;
            while let Ok(buf) = to_host_rx.recv() {
                if w.write_all(&buf).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(Shutdown::Write);
        });

        // Reader: host → guest. `send` blocks when the guest is behind
        // (backpressure); ends on host EOF/error or when the NAT drops
        // `from_host`.
        let mut r = stream;
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if from_host_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        // Host read side is done; make sure the writer also winds down.
        drop(from_host_tx);
        let _ = writer.join();
    });

    HostConn {
        to_host: to_host_tx,
        from_host: from_host_rx,
    }
}
