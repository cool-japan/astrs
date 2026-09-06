//! Crate-internal test seam: an in-memory [`crate::socket::DiscoverySocket`]
//! backed by channels, used by this crate's own `#[cfg(test)]` unit tests
//! (never compiled into a release build, and not part of the public API).
//!
//! A test gets a [`ChannelSocket`] (what the code under test — a
//! [`crate::watcher::BeaconWatcher`] or [`crate::sender::BeaconSender`] —
//! holds) paired with a [`ChannelSocketHandle`] (what the test itself
//! holds): push bytes into [`ChannelSocketHandle::inject`] to simulate an
//! inbound datagram with no real socket involved, and drain
//! [`ChannelSocketHandle::sent`] to observe what the code under test tried
//! to send and to whom.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;

use tokio::sync::{Mutex, mpsc};

use crate::socket::DiscoverySocket;

/// One end of an in-memory socket pair — held by the code under test.
pub(crate) struct ChannelSocket {
    local_addr: SocketAddr,
    inbound: Mutex<mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>>,
    outbound: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
}

/// The other end — held by the test itself.
pub(crate) struct ChannelSocketHandle {
    inbound: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    outbound: Mutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>,
}

impl ChannelSocketHandle {
    /// Injects a datagram as though it arrived from `from`, with no real
    /// socket, network stack, or serialization involved.
    pub(crate) fn inject(&self, bytes: impl Into<Vec<u8>>, from: SocketAddr) {
        // The paired `ChannelSocket` outliving the handle in every test
        // below is the only case exercised; a closed receiver here would
        // just mean the test dropped its socket before finishing, which is
        // a test bug, not something to propagate.
        let _ = self.inbound.send((bytes.into(), from));
    }

    /// Waits for and returns the next `(target, bytes)` pair the code under
    /// test sent.
    pub(crate) async fn next_sent(&self) -> Option<(SocketAddr, Vec<u8>)> {
        self.outbound.lock().await.recv().await
    }

    /// Drains every currently-queued sent datagram without waiting for
    /// more.
    pub(crate) fn drain_sent(&self) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut rx = self.outbound.try_lock().expect("uncontended in tests");
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            out.push(item);
        }
        out
    }
}

/// Serializes every test in this crate's test binary that mutates process
/// environment variables (`PeerBook::from_env`'s `ASTRS_COORDINATOR_ADDR`/
/// `ASTRS_DAEMON_ADDRS`, `defaults::multicast_endpoint_from_env`'s
/// `ASTRS_DISCOVERY_MULTICAST_GROUP`/`_PORT`), regardless of which specific
/// variable names they touch.
///
/// One shared lock for the whole crate rather than one per module matters
/// for a reason stronger than avoiding logical collisions between the
/// *names* two tests happen to set: `std::env::set_var`/`remove_var` mutate
/// a single process-global table, and the standard library documents that
/// mutating it concurrently with **any** other thread's read or write of
/// that table — even of an unrelated key — is a data race (exactly why both
/// functions are `unsafe` as of the 2024 edition). `cargo nextest` runs each
/// test in its own process and never needs this; `cargo test`'s default
/// multi-threaded-in-process runner does.
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `f` with `vars` set (`Some`) or removed (`None`) for its duration,
/// restoring every variable's prior value (or absence) afterward, serialized
/// against every other env-mutating test in this crate via [`ENV_LOCK`].
pub(crate) fn with_env_vars<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous: Vec<(&str, Option<String>)> = vars
        .iter()
        .map(|(k, _)| (*k, std::env::var(k).ok()))
        .collect();
    for (k, v) in vars {
        match v {
            // SAFETY: serialized by `ENV_LOCK` above; this crate's own test
            // suite is the only writer of these `ASTRS_*` variables.
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    f();
    for (k, v) in previous {
        match v {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
}

/// Builds a connected in-memory socket pair with a fixed fake local
/// address.
pub(crate) fn channel_socket(local_addr: SocketAddr) -> (ChannelSocket, ChannelSocketHandle) {
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    (
        ChannelSocket {
            local_addr,
            inbound: Mutex::new(inbound_rx),
            outbound: outbound_tx,
        },
        ChannelSocketHandle {
            inbound: inbound_tx,
            outbound: Mutex::new(outbound_rx),
        },
    )
}

impl DiscoverySocket for ChannelSocket {
    // See `TokioSocket`'s impl for why this is spelled out rather than
    // written as plain `async fn`.
    #[allow(clippy::manual_async_fn)]
    fn send_to<'a>(
        &'a self,
        buf: &'a [u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send + 'a {
        async move {
            let len = buf.len();
            self.outbound
                .send((target, buf.to_vec()))
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            Ok(len)
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn recv_from<'a>(
        &'a self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send + 'a {
        async move {
            let mut rx = self.inbound.lock().await;
            match rx.recv().await {
                Some((data, from)) => {
                    // Mirrors ordinary (non-`MSG_TRUNC`) UDP semantics: a
                    // datagram larger than `buf` is silently truncated, and
                    // the reported length is however much was copied.
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok((n, from))
                }
                None => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn injected_bytes_are_observed_by_recv_from() {
        let (socket, handle) = channel_socket(addr(1));
        handle.inject(b"hello".to_vec(), addr(2));

        let mut buf = [0u8; 16];
        let (n, from) = socket.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(from, addr(2));
    }

    #[tokio::test]
    async fn sent_bytes_are_observed_by_the_handle() {
        let (socket, handle) = channel_socket(addr(1));
        socket.send_to(b"ping", addr(3)).await.unwrap();

        let (target, bytes) = handle.next_sent().await.unwrap();
        assert_eq!(target, addr(3));
        assert_eq!(bytes, b"ping");
    }

    #[tokio::test]
    async fn oversized_inject_truncates_like_real_udp() {
        let (socket, handle) = channel_socket(addr(1));
        handle.inject(vec![7u8; 32], addr(2));

        let mut small = [0u8; 4];
        let (n, _from) = socket.recv_from(&mut small).await.unwrap();
        assert_eq!(n, 4);
        assert_eq!(small, [7u8; 4]);
    }

    #[tokio::test]
    async fn drain_sent_collects_everything_queued_so_far() {
        let (socket, handle) = channel_socket(addr(1));
        socket.send_to(b"a", addr(2)).await.unwrap();
        socket.send_to(b"b", addr(3)).await.unwrap();

        let sent = handle.drain_sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, addr(2));
        assert_eq!(sent[1].0, addr(3));
    }
}
