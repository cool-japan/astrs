//! Log and topic fan-out: which CLI session gets which pushed frame
//! (blueprint §4.2, §7.3, §17 `logs -f` / `topic echo`).
//!
//! A [`astrs_wire::ControlRequest::TopicSubscribe`] reply is easy to route:
//! the daemon's [`astrs_wire::DaemonEvent::TopicTapData`] already carries the
//! exact [`SubscriptionId`] to deliver to. A
//! [`astrs_wire::ControlRequest::LogSubscribe`] is not: a daemon's
//! [`astrs_wire::DaemonEvent::Log`] push (`request: None`) is not
//! subscription-tagged at all — the daemon does not know who, if anyone, is
//! listening — so the coordinator must hold each log subscription's own
//! filter (dataflow/node/[`LogQuery`]) here and re-check every pushed batch
//! against every open one.

use std::collections::HashMap;

use astrs_wire::{DataflowId, LogQuery, NodeId, PortRef, SubscriptionId, TopicQuery};
use tokio::sync::mpsc;

use crate::session::CliOutbound;

/// A log subscription's filter (mirrors the fields of
/// [`astrs_wire::ControlRequest::LogSubscribe`] this subscriber asked for).
#[derive(Debug, Clone)]
pub struct LogSubscription {
    /// The dataflow to follow; `None` follows the whole cluster.
    pub dataflow: Option<DataflowId>,
    /// One node, or all of them.
    pub node: Option<NodeId>,
    /// The filter records must pass.
    pub query: LogQuery,
}

impl LogSubscription {
    /// Whether a record from `record_dataflow`/`record_node` matching
    /// `record` should be delivered to this subscription.
    #[must_use]
    pub fn matches(
        &self,
        record_dataflow: Option<DataflowId>,
        record_node: Option<&NodeId>,
        record: &astrs_wire::LogRecord,
    ) -> bool {
        if let Some(wanted) = self.dataflow
            && record_dataflow != Some(wanted)
        {
            return false;
        }
        if let Some(wanted) = &self.node
            && record_node != Some(wanted)
        {
            return false;
        }
        self.query.matches(record)
    }
}

/// A topic subscription's target (mirrors
/// [`astrs_wire::ControlRequest::TopicSubscribe`]).
#[derive(Debug, Clone)]
pub struct TopicSubscription {
    /// The dataflow tapped.
    pub dataflow: DataflowId,
    /// The producer port tapped.
    pub port: PortRef,
    /// How the tap was shaped when it was opened (kept for reference;
    /// shaping itself is enforced daemon-side once `TopicTapStart` is
    /// dispatched).
    pub query: TopicQuery,
}

/// What kind of thing a [`SubscriberHandle`] was opened for.
#[derive(Debug, Clone)]
pub enum SubscriptionKind {
    /// A log tail.
    Log(LogSubscription),
    /// A topic tap.
    Topic(TopicSubscription),
}

/// One open subscription: what it is, and where its frames go.
pub struct SubscriberHandle {
    /// The subscription id the CLI will see on every delivered frame.
    pub id: SubscriptionId,
    /// What was subscribed to.
    pub kind: SubscriptionKind,
    /// The CLI connection's outbound queue.
    pub sender: mpsc::Sender<CliOutbound>,
    /// How many frames have been dropped because the subscriber's queue
    /// was full (blueprint §17: `topic echo --hz` shaping tolerates loss;
    /// a coordinator-side subscriber queue is one more place it can
    /// happen).
    pub dropped: u64,
}

/// The live subscription registry: every open `LogSubscribe`/
/// `TopicSubscribe`, across every CLI connection.
#[derive(Default)]
pub struct SubscriptionRegistry {
    subscriptions: HashMap<SubscriptionId, SubscriberHandle>,
}

impl SubscriptionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a subscription.
    pub fn insert(&mut self, handle: SubscriberHandle) {
        self.subscriptions.insert(handle.id, handle);
    }

    /// Closes a subscription, returning its handle if it was open.
    pub fn remove(&mut self, id: SubscriptionId) -> Option<SubscriberHandle> {
        self.subscriptions.remove(&id)
    }

    /// Looks up a subscription by id.
    #[must_use]
    pub fn get(&self, id: SubscriptionId) -> Option<&SubscriberHandle> {
        self.subscriptions.get(&id)
    }

    /// Whether a subscription is open.
    #[must_use]
    pub fn contains(&self, id: SubscriptionId) -> bool {
        self.subscriptions.contains_key(&id)
    }

    /// How many subscriptions are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.subscriptions.len()
    }

    /// Whether no subscription is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }

    /// Delivers `frame` to the subscription it names, if still open.
    ///
    /// Returns `true` if delivered. A full receiver queue counts as a drop
    /// (bumping [`SubscriberHandle::dropped`]) rather than blocking this
    /// call — a slow CLI must never stall the daemon-event loop that feeds
    /// every other subscriber and every other piece of coordinator state.
    pub fn deliver_data(&mut self, frame: astrs_wire::DataFrame) -> bool {
        let Some(handle) = self.subscriptions.get_mut(&frame.subscription) else {
            return false;
        };
        match handle.sender.try_send(CliOutbound::from(frame)) {
            Ok(()) => true,
            Err(_) => {
                handle.dropped += 1;
                false
            }
        }
    }

    /// Delivers `record` (tagged with `dataflow`/`node`, if known) to every
    /// open log subscription whose filter matches it.
    ///
    /// Returns how many subscribers actually received it.
    pub fn deliver_log(
        &mut self,
        dataflow: Option<DataflowId>,
        node: Option<&NodeId>,
        record: &astrs_wire::LogRecord,
    ) -> usize {
        let mut delivered = 0;
        for handle in self.subscriptions.values_mut() {
            let SubscriptionKind::Log(subscription) = &handle.kind else {
                continue;
            };
            if !subscription.matches(dataflow, node, record) {
                continue;
            }
            let frame = astrs_wire::LogFrame::new(handle.id, record.clone());
            match handle.sender.try_send(CliOutbound::from(frame)) {
                Ok(()) => delivered += 1,
                Err(_) => handle.dropped += 1,
            }
        }
        delivered
    }

    /// Every subscription id belonging to a CLI connection, identified by
    /// whether its sender is the given channel — used to close every
    /// subscription a connection opened when that connection ends.
    ///
    /// Comparing by `mpsc::Sender::same_channel` rather than tracking a
    /// separate connection id keeps the registry from needing to know
    /// anything about connection identity at all.
    #[must_use]
    pub fn ids_for_sender(&self, sender: &mpsc::Sender<CliOutbound>) -> Vec<SubscriptionId> {
        self.subscriptions
            .values()
            .filter(|handle| handle.sender.same_channel(sender))
            .map(|handle| handle.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{LogLevel, LogRecord};

    fn channel() -> (mpsc::Sender<CliOutbound>, mpsc::Receiver<CliOutbound>) {
        mpsc::channel(8)
    }

    #[test]
    fn topic_frames_route_by_the_frames_own_subscription_id() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, mut rx) = channel();
        let id = SubscriptionId::new(1);
        registry.insert(SubscriberHandle {
            id,
            kind: SubscriptionKind::Topic(TopicSubscription {
                dataflow: DataflowId::from_u128(1),
                port: "camera/image".parse().unwrap(),
                query: TopicQuery::new(),
            }),
            sender: tx,
            dropped: 0,
        });

        let frame = astrs_wire::DataFrame::new(
            id,
            DataflowId::from_u128(1),
            "camera/image".parse().unwrap(),
            astrs_wire::Metadata::new(HlcTimestamp::EPOCH),
            vec![1, 2, 3],
        );
        assert!(registry.deliver_data(frame));
        assert!(matches!(rx.try_recv().unwrap(), CliOutbound::Data(_)));
    }

    #[test]
    fn a_frame_for_a_closed_subscription_is_not_delivered() {
        let mut registry = SubscriptionRegistry::new();
        let frame = astrs_wire::DataFrame::new(
            SubscriptionId::new(99),
            DataflowId::from_u128(1),
            "camera/image".parse().unwrap(),
            astrs_wire::Metadata::new(HlcTimestamp::EPOCH),
            vec![],
        );
        assert!(!registry.deliver_data(frame));
    }

    #[test]
    fn a_full_receiver_counts_as_a_drop_not_a_block() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, rx) = mpsc::channel(1);
        let id = SubscriptionId::new(1);
        registry.insert(SubscriberHandle {
            id,
            kind: SubscriptionKind::Topic(TopicSubscription {
                dataflow: DataflowId::from_u128(1),
                port: "a/o".parse().unwrap(),
                query: TopicQuery::new(),
            }),
            sender: tx,
            dropped: 0,
        });
        let make = || {
            astrs_wire::DataFrame::new(
                id,
                DataflowId::from_u128(1),
                "a/o".parse().unwrap(),
                astrs_wire::Metadata::new(HlcTimestamp::EPOCH),
                vec![],
            )
        };
        assert!(registry.deliver_data(make()));
        assert!(
            !registry.deliver_data(make()),
            "the queue of depth 1 is now full"
        );
        assert_eq!(registry.get(id).unwrap().dropped, 1);
        drop(rx);
    }

    #[test]
    fn log_delivery_matches_dataflow_and_node_filters() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, mut rx) = channel();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(1),
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: Some(dataflow),
                node: Some(node.clone()),
                query: LogQuery::new(),
            }),
            sender: tx,
            dropped: 0,
        });

        let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "hello");
        assert_eq!(
            registry.deliver_log(Some(dataflow), Some(&node), &record),
            1
        );
        assert!(matches!(rx.try_recv().unwrap(), CliOutbound::Log(_)));

        let other_node = NodeId::new("lidar").unwrap();
        assert_eq!(
            registry.deliver_log(Some(dataflow), Some(&other_node), &record),
            0,
            "a different node must not match"
        );
    }

    #[test]
    fn a_cluster_wide_log_subscription_matches_every_dataflow() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, mut rx) = channel();
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(1),
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: None,
                node: None,
                query: LogQuery::new(),
            }),
            sender: tx,
            dropped: 0,
        });
        let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "hello");
        assert_eq!(
            registry.deliver_log(Some(DataflowId::generate()), None, &record),
            1
        );
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn the_query_level_filter_still_applies() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, mut rx) = channel();
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(1),
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: None,
                node: None,
                query: LogQuery::new().with_min_level(LogLevel::Error),
            }),
            sender: tx,
            dropped: 0,
        });
        let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "hello");
        assert_eq!(registry.deliver_log(None, None, &record), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ids_for_sender_finds_every_subscription_on_one_connection() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, _rx) = channel();
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(1),
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: None,
                node: None,
                query: LogQuery::new(),
            }),
            sender: tx.clone(),
            dropped: 0,
        });
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(2),
            kind: SubscriptionKind::Topic(TopicSubscription {
                dataflow: DataflowId::from_u128(1),
                port: "a/o".parse().unwrap(),
                query: TopicQuery::new(),
            }),
            sender: tx.clone(),
            dropped: 0,
        });
        let (other_tx, _rx2) = channel();
        registry.insert(SubscriberHandle {
            id: SubscriptionId::new(3),
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: None,
                node: None,
                query: LogQuery::new(),
            }),
            sender: other_tx,
            dropped: 0,
        });

        let mut ids = registry.ids_for_sender(&tx);
        ids.sort();
        assert_eq!(ids, vec![SubscriptionId::new(1), SubscriptionId::new(2)]);
    }

    #[test]
    fn remove_closes_exactly_one_subscription() {
        let mut registry = SubscriptionRegistry::new();
        let (tx, _rx) = channel();
        let id = SubscriptionId::new(1);
        registry.insert(SubscriberHandle {
            id,
            kind: SubscriptionKind::Log(LogSubscription {
                dataflow: None,
                node: None,
                query: LogQuery::new(),
            }),
            sender: tx,
            dropped: 0,
        });
        assert!(registry.contains(id));
        assert!(registry.remove(id).is_some());
        assert!(!registry.contains(id));
        assert!(registry.is_empty());
    }
}
