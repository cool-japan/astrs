//! The testing harness (blueprint §9.1's `init_testing`).
//!
//! ```
//! use astrs_node_api::prelude::*;
//! use astrs_wire::StopCause;
//!
//! # fn main() -> Result<(), NodeError> {
//! let mut harness = Node::init_testing()?;
//! let id = harness.node.id().clone();
//!
//! harness.daemon.stop(&id, StopCause::Requested)?;
//! let event = harness.events.recv().expect("the stop");
//! assert!(event.is_stop());
//! # Ok(())
//! # }
//! ```
//!
//! # What makes this worth having
//!
//! A node's logic is `event in → output out`, and everything between is the
//! session. A harness that stubbed the session would test the stub. This one
//! runs the *real* session — the real greeting, the real frame codec, the real
//! dispatcher, the real queues — against a [`MockDaemon`] that speaks the real
//! protocol on the other end of a `tokio::io::duplex` pair. The only thing
//! that is not real is the socket.
//!
//! That buys three things a stub cannot:
//!
//! * **Two-node tests.** [`TestHarness::pair`] connects a producer and a
//!   consumer wired to each other, so a service round trip or an action FSM is
//!   an ordinary `#[test]`.
//! * **Protocol regressions.** A change that breaks the wire breaks these
//!   tests, because they *are* on the wire.
//! * **Route-upgrade tests.** [`MockDaemon::upgrade_route`] drives the §6.3
//!   handshake, including the ack the node sends back.

pub mod daemon;

use std::time::Duration;

use astrs_wire::{
    DataId, InputSpec, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef, StopCause,
};

use crate::error::{NodeError, Result};
use crate::events::EventStream;
use crate::node::Node;

#[cfg(unix)]
pub use daemon::UnixEndpoint;
pub use daemon::{DUPLEX_BUFFER, HANDSHAKE_TIMEOUT, MockDaemon, RecordedRequest, RecordedSend};

/// How long the harness's convenience waits allow.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(5);

/// A node, its event stream, and the daemon it is talking to.
#[derive(Debug)]
pub struct TestHarness {
    /// The node under test.
    pub node: Node,
    /// Its event stream.
    pub events: EventStream,
    /// The daemon on the other end.
    pub daemon: MockDaemon,
}

impl TestHarness {
    /// The node id [`TestHarness::start`] uses.
    pub const DEFAULT_NODE: &'static str = "node-under-test";

    /// The input [`TestHarness::start`] declares.
    pub const DEFAULT_INPUT: &'static str = "in";

    /// The output [`TestHarness::start`] declares.
    pub const DEFAULT_OUTPUT: &'static str = "out";

    /// Starts a daemon and one node with a single input and output.
    ///
    /// # Errors
    ///
    /// [`NodeError::Testing`] when the harness cannot be started.
    pub fn start() -> Result<Self> {
        let daemon = MockDaemon::start()?;
        let spec = Self::default_spec(&daemon)?;
        let (node, events) = daemon.connect_node(spec)?;
        Ok(Self {
            node,
            events,
            daemon,
        })
    }

    /// Starts a daemon and one node with an explicit specification.
    ///
    /// # Errors
    ///
    /// As [`TestHarness::start`].
    pub fn with_spec(spec: NodeSpawnSpec) -> Result<Self> {
        let daemon = MockDaemon::start()?;
        let (node, events) = daemon.connect_node(spec)?;
        Ok(Self {
            node,
            events,
            daemon,
        })
    }

    /// Starts a daemon with a producer and a consumer wired to each other.
    ///
    /// `producer` publishes on `output`; `consumer` reads it as `input`. The
    /// returned pair is `(producer, consumer)`.
    ///
    /// # Errors
    ///
    /// As [`TestHarness::start`].
    pub fn pair(producer: &str, output: &str, consumer: &str, input: &str) -> Result<(Self, Self)> {
        let daemon = MockDaemon::start()?;
        let producer_spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new(producer)?,
            0,
            NodeSource::Dynamic,
        )
        .with_output(OutputSpec::new(DataId::new(output)?));
        let consumer_spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new(consumer)?,
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new(input)?,
            PortRef::from_parts(producer, output)?,
        ));

        let (producer_node, producer_events) = daemon.connect_node(producer_spec)?;
        let (consumer_node, consumer_events) = daemon.connect_node(consumer_spec)?;
        Ok((
            Self {
                node: producer_node,
                events: producer_events,
                daemon: daemon.clone(),
            },
            Self {
                node: consumer_node,
                events: consumer_events,
                daemon,
            },
        ))
    }

    /// The default specification: one input, one output, both wired to a
    /// notional peer.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] when the built-in names fail the grammar, which they
    /// cannot.
    pub fn default_spec(daemon: &MockDaemon) -> Result<NodeSpawnSpec> {
        Ok(NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new(Self::DEFAULT_NODE)?,
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new(Self::DEFAULT_INPUT)?,
            PortRef::from_parts("peer", "out")?,
        ))
        .with_output(OutputSpec::new(DataId::new(Self::DEFAULT_OUTPUT)?)))
    }

    /// The node's id.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.node.id().clone()
    }

    /// Delivers one message to the node's default input.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_input`].
    pub fn feed(&self, payload: Vec<u8>) -> Result<()> {
        self.feed_input(Self::DEFAULT_INPUT, payload)
    }

    /// Delivers one message to a named input.
    ///
    /// # Errors
    ///
    /// [`NodeError::Id`] for a malformed name, plus whatever
    /// [`MockDaemon::send_input`] reports.
    pub fn feed_input(&self, input: &str, payload: Vec<u8>) -> Result<()> {
        let metadata = self.node.metadata();
        self.daemon
            .send_input(self.node.id(), &DataId::new(input)?, metadata, payload)
    }

    /// Reads the next event, failing rather than hanging.
    ///
    /// # Errors
    ///
    /// [`NodeError::Timeout`] when nothing arrives in [`DEFAULT_WAIT`], and
    /// [`NodeError::Stopped`] when the stream has ended.
    pub fn next_event(&mut self) -> Result<crate::Event> {
        match self.events.recv_timeout(DEFAULT_WAIT)? {
            Some(event) => Ok(event),
            None if self.events.is_fused() || self.events.session_ended() => {
                Err(NodeError::Stopped)
            }
            None => Err(NodeError::Timeout {
                operation: "TestHarness::next_event",
                millis: u64::try_from(DEFAULT_WAIT.as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }

    /// Stops the node and drains the stop event.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::stop`].
    pub fn stop(&mut self, cause: StopCause) -> Result<()> {
        self.daemon.stop(self.node.id(), cause)?;
        while let Ok(event) = self.next_event() {
            if event.is_stop() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Shuts the harness down.
    pub fn shutdown(&mut self) {
        let _ = self.node.shutdown();
        self.daemon.shutdown();
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::Event;

    #[test]
    fn the_default_harness_has_one_input_and_one_output() {
        let mut harness = TestHarness::start().unwrap();
        assert_eq!(harness.node_id().as_str(), TestHarness::DEFAULT_NODE);
        assert_eq!(harness.node.descriptor().inputs.len(), 1);
        assert_eq!(harness.node.descriptor().outputs.len(), 1);

        harness.feed(vec![1, 2, 3]).unwrap();
        let event = harness.next_event().unwrap();
        let Some((id, _, data)) = event.into_input() else {
            panic!("expected an input");
        };
        assert_eq!(id.as_str(), TestHarness::DEFAULT_INPUT);
        assert_eq!(data.to_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn a_paired_harness_routes_between_two_nodes() {
        let (mut producer, mut consumer) =
            TestHarness::pair("camera", "image", "detect", "frames").unwrap();
        let mut image = producer.node.raw_output("image").unwrap();
        image
            .send_bytes(vec![4, 5, 6], producer.node.metadata())
            .unwrap();

        let event = consumer.next_event().unwrap();
        let Some((id, _, data)) = event.into_input() else {
            panic!("expected an input");
        };
        assert_eq!(id.as_str(), "frames");
        assert_eq!(data.to_vec(), vec![4, 5, 6]);
    }

    #[test]
    fn stopping_the_harness_fuses_the_stream() {
        let mut harness = TestHarness::start().unwrap();
        harness.stop(StopCause::Requested).unwrap();
        assert!(harness.events.is_fused());
        assert!(matches!(harness.next_event(), Err(NodeError::Stopped)));
    }

    #[test]
    fn an_empty_stream_times_out_rather_than_hanging() {
        let mut harness = TestHarness::start().unwrap();
        let error = harness
            .events
            .recv_timeout(Duration::from_millis(20))
            .unwrap();
        assert!(error.is_none());
    }

    #[test]
    fn a_custom_specification_is_honoured() {
        let daemon = MockDaemon::start().unwrap();
        let spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("custom").unwrap(),
            7,
            NodeSource::Dynamic,
        );
        let harness = TestHarness::with_spec(spec).unwrap();
        assert_eq!(harness.node.id().as_str(), "custom");
        assert_eq!(harness.node.restart_count(), 7);
        assert!(harness.node.is_restart());
    }

    #[test]
    fn a_message_for_an_undeclared_input_is_reported() {
        let mut harness = TestHarness::start().unwrap();
        harness.feed_input("nope", vec![1]).unwrap();
        let event = harness.next_event().unwrap();
        assert!(matches!(event, Event::Error(_)), "{event}");
    }

    #[test]
    fn the_documented_names_are_stable() {
        assert_eq!(TestHarness::DEFAULT_NODE, "node-under-test");
        assert_eq!(TestHarness::DEFAULT_INPUT, "in");
        assert_eq!(TestHarness::DEFAULT_OUTPUT, "out");
        assert_eq!(DEFAULT_WAIT, Duration::from_secs(5));
    }
}
