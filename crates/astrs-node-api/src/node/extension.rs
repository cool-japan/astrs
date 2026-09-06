//! The extension table client (blueprint §9.1's `ext_store/load/drop`, §7.3).
//!
//! A small key/value store the daemon keeps on a node's behalf, and the seam
//! through which pinned host buffers and accelerator IPC handles will reach a
//! node in 0.2 (§7.3 "extension & pinned-memory"). What makes it worth having
//! now is what it survives: an entry outlives the node that stored it, so a
//! restarted node can pick up a calibration, a warm cache or a device handle
//! that its previous incarnation left behind.
//!
//! # Namespacing
//!
//! [`ExtensionKey`] is `namespace/name`, and a node may only write the `user`
//! namespace ([`ExtensionKey::is_writable_by_node`]). The reserved ones are
//! the daemon's to fill: a node *reads* the pinned buffer it was granted, it
//! does not invent one. [`Node::ext_store`] refuses a reserved key locally
//! rather than making a round trip to be told no.
//!
//! # Time to live
//!
//! [`Node::ext_store_with_ttl`] sets one. Without it a crashed node's handles
//! accumulate, which is the whole reason the wire carries a `ttl` field.
//!
//! # Reads are a round trip
//!
//! `ExtLoad` is answered by an `ExtValue` event, which the session dispatcher
//! routes to the caller rather than to the node's stream. That makes
//! [`Node::ext_load`] a blocking call with a deadline — and gives it an async
//! twin for a node whose loop is already asynchronous.

use std::time::Duration;

use astrs_wire::{DurationMs, ExtensionKey, ExtensionNamespace, NodeRequest};

use crate::error::{NodeError, Result};
use crate::node::Node;

impl Node {
    /// Stores `value` under `key` (§7.3 `ExtStore`).
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] for a key in a namespace a node may not write,
    /// and [`NodeError::DaemonGone`] once the session has ended.
    pub fn ext_store(&self, key: &ExtensionKey, value: Vec<u8>) -> Result<()> {
        self.ext_store_inner(key, value, None)
    }

    /// Stores `value` under `key`, to be reclaimed after `ttl`.
    ///
    /// # Errors
    ///
    /// As [`Node::ext_store`].
    pub fn ext_store_with_ttl(
        &self,
        key: &ExtensionKey,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<()> {
        self.ext_store_inner(key, value, Some(DurationMs::from_duration(ttl)))
    }

    /// Reads `key` back, waiting up to the session's default timeout.
    ///
    /// # Errors
    ///
    /// [`NodeError::Timeout`] when the daemon does not answer,
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn ext_load(&self, key: &ExtensionKey) -> Result<Option<Vec<u8>>> {
        self.ext_load_timeout(key, Self::reply_timeout())
    }

    /// Reads `key` back, waiting up to `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Node::ext_load`], plus [`NodeError::BlockingInAsync`] when the
    /// caller may not block.
    pub fn ext_load_timeout(
        &self,
        key: &ExtensionKey,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let session = self.session();
        let receiver = session.watch_extension(key);
        session.send_request(NodeRequest::ExtLoad { key: key.clone() })?;
        let runtime = session.runtime.clone();
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        // The timeout future is built *inside* the async block: constructing a
        // `Sleep` needs a reactor, and outside a runtime there is none until
        // `block_on` enters one.
        let answer = runtime.block_on("Node::ext_load", "Node::ext_load_async", async move {
            tokio::time::timeout(timeout, receiver).await
        })?;
        match answer {
            Ok(Ok(value)) => Ok(value),
            // The sender was dropped: the session closed under us.
            Ok(Err(_)) => Err(NodeError::DaemonGone),
            Err(_) => Err(NodeError::Timeout {
                operation: "Node::ext_load",
                millis,
            }),
        }
    }

    /// The async twin of [`Node::ext_load`].
    ///
    /// # Errors
    ///
    /// As [`Node::ext_load`].
    pub async fn ext_load_async(&self, key: &ExtensionKey) -> Result<Option<Vec<u8>>> {
        self.ext_load_async_timeout(key, Self::reply_timeout())
            .await
    }

    /// The async twin of [`Node::ext_load_timeout`].
    ///
    /// # Errors
    ///
    /// As [`Node::ext_load`].
    pub async fn ext_load_async_timeout(
        &self,
        key: &ExtensionKey,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let session = self.session();
        let receiver = session.watch_extension(key);
        session
            .send_request_async(NodeRequest::ExtLoad { key: key.clone() })
            .await?;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(NodeError::DaemonGone),
            Err(_) => Err(NodeError::Timeout {
                operation: "Node::ext_load_async",
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }

    /// Drops `key` from the table (§7.3 `ExtDrop`).
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn ext_drop(&self, key: &ExtensionKey) -> Result<()> {
        self.session()
            .send_request(NodeRequest::ExtDrop { key: key.clone() })
    }

    /// A validated key in the `user` namespace.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the name fails the identifier grammar.
    pub fn ext_key(name: impl Into<String>) -> Result<ExtensionKey> {
        Ok(ExtensionKey::user(name)?)
    }

    /// The shared implementation of the two store forms.
    fn ext_store_inner(
        &self,
        key: &ExtensionKey,
        value: Vec<u8>,
        ttl: Option<DurationMs>,
    ) -> Result<()> {
        if !key.is_writable_by_node() {
            return Err(NodeError::Pattern(format!(
                "the `{}` extension namespace is the daemon's to write, not a node's",
                key.namespace
            )));
        }
        self.session().send_request(NodeRequest::ExtStore {
            key: key.clone(),
            value,
            ttl,
        })
    }
}

/// Whether a namespace is one a node may write (§7.3).
///
/// A convenience for a caller building keys dynamically; the store methods
/// apply it themselves.
#[must_use]
pub const fn is_writable(namespace: ExtensionNamespace) -> bool {
    !namespace.is_reserved()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::testing::TestHarness;

    #[test]
    fn a_stored_value_reads_back() {
        let harness = TestHarness::start().unwrap();
        let key = Node::ext_key("calibration").unwrap();
        harness.node.ext_store(&key, vec![1, 2, 3]).unwrap();
        assert_eq!(harness.node.ext_load(&key).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(harness.daemon.extension(&key), Some(vec![1, 2, 3]));
    }

    #[test]
    fn an_unset_key_reads_back_as_none() {
        let harness = TestHarness::start().unwrap();
        let key = Node::ext_key("absent").unwrap();
        assert_eq!(harness.node.ext_load(&key).unwrap(), None);
    }

    #[test]
    fn dropping_a_key_removes_it() {
        let harness = TestHarness::start().unwrap();
        let key = Node::ext_key("temporary").unwrap();
        harness.node.ext_store(&key, vec![7]).unwrap();
        assert_eq!(harness.node.ext_load(&key).unwrap(), Some(vec![7]));
        harness.node.ext_drop(&key).unwrap();
        assert_eq!(harness.node.ext_load(&key).unwrap(), None);
    }

    #[test]
    fn a_time_to_live_is_carried_on_the_wire() {
        let harness = TestHarness::start().unwrap();
        let key = Node::ext_key("expiring").unwrap();
        harness
            .node
            .ext_store_with_ttl(&key, vec![1], Duration::from_secs(60))
            .unwrap();
        harness
            .daemon
            .wait_for(Duration::from_secs(5), |requests| {
                requests.iter().any(|entry| {
                    matches!(
                        &entry.request,
                        NodeRequest::ExtStore { ttl: Some(ttl), .. }
                            if ttl.as_millis() == 60_000
                    )
                })
            })
            .unwrap();
    }

    #[test]
    fn a_reserved_namespace_is_refused_locally() {
        let harness = TestHarness::start().unwrap();
        let key = ExtensionKey::new(ExtensionNamespace::PinnedMemory, "pool").unwrap();
        let error = harness.node.ext_store(&key, vec![1]).unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(!is_writable(ExtensionNamespace::PinnedMemory));
        assert!(is_writable(ExtensionNamespace::User));

        // No round trip was made.
        assert!(
            !harness
                .daemon
                .requests()
                .iter()
                .any(|entry| matches!(entry.request, NodeRequest::ExtStore { .. }))
        );
    }

    #[test]
    fn a_load_after_shutdown_reports_the_daemon_is_gone() {
        let mut harness = TestHarness::start().unwrap();
        let key = Node::ext_key("gone").unwrap();
        harness.node.shutdown().unwrap();
        assert!(matches!(
            harness.node.ext_load(&key),
            Err(NodeError::DaemonGone)
        ));
        assert!(matches!(
            harness.node.ext_store(&key, vec![1]),
            Err(NodeError::DaemonGone)
        ));
        assert!(matches!(
            harness.node.ext_drop(&key),
            Err(NodeError::DaemonGone)
        ));
    }
}
