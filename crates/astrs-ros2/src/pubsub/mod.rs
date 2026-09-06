//! Topics: publishers, subscriptions, and the two APIs each has.
//!
//! Every entity comes in two forms, and both are first-class:
//!
//! | Typed | Type-erased | Difference |
//! |---|---|---|
//! | [`Publisher<T>`] | [`RawPublisher`] | the DDS type name comes from [`MessageType`](crate::MessageType) rather than a string, and `publish` takes `&T` rather than octets |
//! | [`Subscription<T>`] | [`RawSubscription`] | `recv` decodes rather than handing back octets |
//!
//! The type-erased form is not a lesser one. A declarative bridge
//! (blueprint §10.5) names its message type in YAML — `message_type:
//! sensor_msgs/msg/LaserScan` — so it has a *string* at the moment it
//! creates the endpoint, and no amount of generics will turn that into a
//! type parameter. The typed form is a decoding wrapper over the erased
//! one, never a parallel implementation, so the two cannot drift.
//!
//! # Waiting without sleeping
//!
//! "Has anybody matched yet?" is answered by
//! [`Publisher::wait_for_subscriptions`] and
//! [`Subscription::wait_for_publishers`], both of which wait on the
//! participant's discovery-event stream and re-check the condition when an
//! event arrives. There is no polling interval to tune and no sleep to make
//! a test flaky: matching happens inside the same `handle_message` call that
//! emits the event, so an event is exactly the signal that the answer may
//! have changed.

pub mod publisher;
pub mod subscription;

use std::time::Duration as StdDuration;

use tokio::sync::broadcast;

use crate::error::{Ros2Error, Ros2Result};
use crate::node::Ros2Context;

pub use publisher::{Publisher, RawPublisher};
pub use subscription::{MessageInfo, RawSubscription, Subscription};

/// Wait until `condition` holds, waking on every discovery event.
///
/// The shared implementation behind every `wait_for_*` in this crate.
/// `condition` is checked once before waiting — the thing may already be
/// true — and again after every event and on every timeout tick.
///
/// # Errors
///
/// [`Ros2Error::Timeout`] when `timeout` elapses with the condition still
/// false, and [`Ros2Error::NodeShutDown`] when the event stream closes.
pub(crate) async fn wait_for<F, Fut>(
    context: &Ros2Context,
    timeout: StdDuration,
    operation: &'static str,
    mut condition: F,
) -> Ros2Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let mut events = context.discovery_events();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timed_out(operation, timeout));
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(_)) => {}
            // Lagged means events were missed, not that the condition is
            // false; the next loop re-checks the authoritative state.
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(Ros2Error::NodeShutDown);
            }
            Err(_) => {
                // One last check: the condition may have become true while
                // the timeout was expiring, and reporting a timeout for
                // something that already happened is the worst kind of
                // flake.
                if condition().await {
                    return Ok(());
                }
                return Err(timed_out(operation, timeout));
            }
        }
    }
}

/// The timeout error, with the elapsed budget rendered in milliseconds.
pub(crate) fn timed_out(operation: &'static str, timeout: StdDuration) -> Ros2Error {
    Ros2Error::Timeout {
        operation,
        elapsed_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::names::FullName;
    use crate::node::ContextOptions;
    use crate::qos::QosProfile;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn a_condition_that_already_holds_returns_at_once() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        wait_for(
            &context,
            StdDuration::from_millis(1),
            "an immediate truth",
            || async { true },
        )
        .await
        .expect("no waiting needed");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_condition_that_never_holds_times_out() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let error = wait_for(
            &context,
            StdDuration::from_millis(30),
            "an impossible truth",
            || async { false },
        )
        .await
        .expect_err("timed out");
        assert!(matches!(error, Ros2Error::Timeout { .. }));
        assert!(error.is_transient());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_condition_that_becomes_true_is_noticed_through_an_event() {
        let first = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let second = Ros2Context::loopback([first.metatraffic_locator()])
            .await
            .expect("bind");

        let seen = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&seen);
        let peer = first.guid();
        wait_for(&second, StdDuration::from_secs(5), "discovery", || {
            let probe = Arc::clone(&probe);
            let second = Arc::clone(&second);
            async move {
                let known = second.participant().knows(peer).await;
                probe.store(known, Ordering::Relaxed);
                known
            }
        })
        .await
        .expect("the peer is discovered");
        assert!(seen.load(Ordering::Relaxed));

        first.shutdown().await;
        second.shutdown().await;
    }

    #[tokio::test]
    async fn waiting_for_a_publisher_that_arrives_succeeds() {
        let publishing = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let subscribing = Ros2Context::loopback([publishing.metatraffic_locator()])
            .await
            .expect("bind");

        let subscription = Subscription::<crate::msg::std_msgs::String>::new(
            Arc::clone(&subscribing),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("subscription");

        let publisher = Publisher::<crate::msg::std_msgs::String>::new(
            Arc::clone(&publishing),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");

        subscription
            .wait_for_publishers(1, StdDuration::from_secs(10))
            .await
            .expect("the publisher is discovered");
        publisher
            .wait_for_subscriptions(1, StdDuration::from_secs(10))
            .await
            .expect("and so is the subscription");

        publishing.shutdown().await;
        subscribing.shutdown().await;
    }

    #[test]
    fn the_timeout_error_renders_the_budget() {
        let error = timed_out("a thing", StdDuration::from_millis(250));
        assert!(error.to_string().contains("250"), "{error}");
        assert!(error.to_string().contains("a thing"), "{error}");
    }
}
