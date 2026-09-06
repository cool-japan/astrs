//! [`ServiceClient`]: the asking half of a service.

use core::marker::PhantomData;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration as StdDuration;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::ServiceType;
use crate::error::{Ros2Error, Ros2Result};
use crate::names::{FullName, TopicKind};
use crate::node::Ros2Context;
use crate::pubsub::{RawPublisher, RawSubscription};
use crate::qos::QosProfile;
use crate::service::identity::{
    RequestId, SampleIdentity, decode_with_identity, encode_with_identity, peek_identity,
};

/// A service client: publishes on `rq/…Request`, subscribes to
/// `rr/…Reply`.
///
/// # Why there is a background task
///
/// `rr/…Reply` is shared by every client of the service, and one client may
/// have several calls outstanding. A reply therefore has to be *routed*:
/// discarded if another client asked, and matched to the right pending call
/// if this one did. Doing that inside `call` would mean two concurrent
/// calls racing on one queue, with each able to take the other's reply and
/// then park without a way to hand it over.
///
/// So one task per client drains the reply subscription into an inbox keyed
/// by sequence number, and `call` waits on the inbox. The task is aborted
/// when the last clone of the client is dropped, so a client that goes out
/// of scope leaves nothing running.
#[derive(Debug)]
pub struct ServiceClient<S: ServiceType> {
    inner: Arc<ClientInner>,
    marker: PhantomData<fn() -> S>,
}

impl<S: ServiceType> Clone for ServiceClient<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            marker: PhantomData,
        }
    }
}

#[derive(Debug)]
struct ClientInner {
    context: Arc<Ros2Context>,
    request: RawPublisher,
    reply: RawSubscription,
    service: FullName,
    next_sequence: AtomicI64,
    inbox: Inbox,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        // `Mutex::try_lock` rather than a block: `Drop` cannot await, and the
        // dispatcher lock is only ever held for the length of a `take`, so
        // contention here means another clone is mid-construction — which
        // cannot happen, because this is the last one.
        if let Ok(mut guard) = self.dispatcher.try_lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

/// Replies that have arrived and not yet been claimed.
#[derive(Debug, Default)]
struct Inbox {
    replies: Mutex<BTreeMap<i64, Vec<u8>>>,
    arrived: Notify,
}

impl Inbox {
    /// Store a reply and wake everyone waiting.
    async fn deliver(&self, sequence: i64, payload: Vec<u8>) {
        self.replies.lock().await.insert(sequence, payload);
        self.arrived.notify_waiters();
    }

    /// Claim a reply, if it has arrived.
    async fn claim(&self, sequence: i64) -> Option<Vec<u8>> {
        self.replies.lock().await.remove(&sequence)
    }

    /// How many unclaimed replies are held.
    async fn len(&self) -> usize {
        self.replies.lock().await.len()
    }
}

impl<S: ServiceType> ServiceClient<S> {
    /// Create a client for `service`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when either endpoint cannot be created.
    pub async fn new(
        context: Arc<Ros2Context>,
        service: FullName,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let request = RawPublisher::for_kind(
            Arc::clone(&context),
            service.clone(),
            TopicKind::Request,
            S::REQUEST_DDS_NAME,
            qos,
            owner.clone(),
        )
        .await?;
        let reply = RawSubscription::for_kind(
            Arc::clone(&context),
            service.clone(),
            TopicKind::Reply,
            S::RESPONSE_DDS_NAME,
            qos,
            owner,
        )
        .await?;

        let inner = Arc::new(ClientInner {
            context,
            request,
            reply,
            service,
            next_sequence: AtomicI64::new(1),
            inbox: Inbox::default(),
            dispatcher: Mutex::new(None),
        });

        let dispatcher = Arc::clone(&inner);
        let handle = tokio::spawn(async move { dispatch(dispatcher).await });
        *inner.dispatcher.lock().await = Some(handle);

        Ok(Self {
            inner,
            marker: PhantomData,
        })
    }

    /// The fully-qualified service name.
    #[must_use]
    pub fn service(&self) -> &FullName {
        &self.inner.service
    }

    /// The DDS request topic this client publishes on.
    #[must_use]
    pub fn request_topic(&self) -> String {
        self.inner.service.dds_name(TopicKind::Request)
    }

    /// The DDS reply topic this client subscribes to.
    #[must_use]
    pub fn reply_topic(&self) -> String {
        self.inner.service.dds_name(TopicKind::Reply)
    }

    /// This client's identity on the wire: its request-writer GUID.
    #[must_use]
    pub fn guid(&self) -> astrs_rtps::structure::Guid {
        self.inner.request.guid()
    }

    /// True when at least one server has been discovered.
    pub async fn is_available(&self) -> bool {
        self.inner.request.subscription_count().await > 0
            && self.inner.reply.publisher_count().await > 0
    }

    /// Wait until a server has been discovered on both halves.
    ///
    /// Both halves matter: a server whose request subscription has been
    /// discovered but whose reply publisher has not is one whose answer this
    /// client would not hear.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_service(&self, timeout: StdDuration) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.inner.context,
            timeout,
            "waiting for a service server",
            || async { self.is_available().await },
        )
        .await
        .map_err(|error| match error {
            Ros2Error::Timeout { .. } => Ros2Error::ServiceUnavailable {
                name: self.inner.service.as_str().to_owned(),
            },
            other => other,
        })
    }

    /// Send a request and return its identity, without waiting for a reply.
    ///
    /// The asynchronous half of the API: an action client sends a
    /// `get_result` request that may not be answered for minutes, and has no
    /// business holding a future open that long.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] when the request will not encode, plus whatever
    /// the publisher reports.
    pub async fn send_request(&self, request: &S::Request) -> Ros2Result<RequestId> {
        let sequence = self.inner.next_sequence.fetch_add(1, Ordering::Relaxed);
        let identity = SampleIdentity::from_guid(self.guid(), sequence);
        let payload = encode_with_identity(identity, request)?;
        self.inner.request.publish_bytes(payload).await?;
        Ok(identity)
    }

    /// Wait for the reply to a request already sent.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when nothing arrives in time, and
    /// [`Ros2Error::Cdr`] when the reply will not decode.
    pub async fn take_response(
        &self,
        id: RequestId,
        timeout: StdDuration,
    ) -> Ros2Result<S::Response> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(payload) = self.inner.inbox.claim(id.sequence_number).await {
                let (_, response) = decode_with_identity::<S::Response>(&payload)?;
                return Ok(response);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(crate::pubsub::timed_out("a service call", timeout));
            }
            // Registering the notification *before* the last claim attempt
            // would be the usual lost-wakeup fix; here the claim above runs
            // first and this wait is bounded, so a wakeup that lands in
            // between costs one extra loop rather than a hang.
            if tokio::time::timeout(remaining, self.inner.inbox.arrived.notified())
                .await
                .is_err()
                && self.inner.inbox.claim(id.sequence_number).await.is_none()
            {
                return Err(crate::pubsub::timed_out("a service call", timeout));
            }
        }
    }

    /// Send a request and wait for its reply.
    ///
    /// # Errors
    ///
    /// As [`send_request`](Self::send_request) and
    /// [`take_response`](Self::take_response).
    pub async fn call(
        &self,
        request: &S::Request,
        timeout: StdDuration,
    ) -> Ros2Result<S::Response> {
        let id = self.send_request(request).await?;
        self.take_response(id, timeout).await
    }

    /// Wait for a server, then call.
    ///
    /// The convenience a first call wants: a client created a microsecond
    /// ago has discovered nothing, and a bare [`call`](Self::call) would
    /// publish into a void and then time out with a misleading message.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::ServiceUnavailable`] when no server appears, plus
    /// whatever [`call`](Self::call) reports.
    pub async fn call_when_ready(
        &self,
        request: &S::Request,
        timeout: StdDuration,
    ) -> Ros2Result<S::Response> {
        self.wait_for_service(timeout).await?;
        self.call(request, timeout).await
    }

    /// How many replies have arrived and not been claimed.
    ///
    /// Nonzero after a call times out and its answer arrives late; a
    /// diagnostic, not a queue a caller should drain.
    pub async fn pending_replies(&self) -> usize {
        self.inner.inbox.len().await
    }

    /// Delete both endpoints.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn destroy(&self) -> Ros2Result<()> {
        if let Some(handle) = self.inner.dispatcher.lock().await.take() {
            handle.abort();
        }
        self.inner.request.destroy().await?;
        self.inner.reply.destroy().await?;
        Ok(())
    }
}

/// Drain the reply subscription forever, routing what belongs to this
/// client and discarding what does not.
async fn dispatch(inner: Arc<ClientInner>) {
    let own = inner.request.guid();
    loop {
        let (payload, _) = inner.reply.recv().await;
        match peek_identity(&payload) {
            Ok(identity) if identity.was_written_by(own) => {
                inner.inbox.deliver(identity.sequence_number, payload).await;
            }
            // Another client's reply on the shared topic: the normal case in
            // any system with two clients, and not worth a log line.
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(
                    service = %inner.service,
                    %error,
                    "a reply arrived without a well-formed sample identity"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::node::{ContextOptions, Ros2Context};
    use crate::service::AddTwoInts;

    async fn context() -> Arc<Ros2Context> {
        Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind")
    }

    #[tokio::test]
    async fn a_client_announces_both_mangled_topics() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        assert_eq!(client.request_topic(), "rq/add_two_intsRequest");
        assert_eq!(client.reply_topic(), "rr/add_two_intsReply");
        assert!(!client.is_available().await);
        client.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn waiting_for_an_absent_server_reports_it_as_unavailable() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        let error = client
            .wait_for_service(StdDuration::from_millis(30))
            .await
            .expect_err("nobody serves it");
        assert!(matches!(error, Ros2Error::ServiceUnavailable { .. }));
        assert!(error.is_transient());
        client.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn sequence_numbers_start_at_one_and_increase() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        let request = crate::msg::example_interfaces::AddTwoIntsRequest { a: 1, b: 2 };
        let first = client.send_request(&request).await.expect("send");
        let second = client.send_request(&request).await.expect("send");
        assert_eq!(first.sequence_number, 1);
        assert_eq!(second.sequence_number, 2);
        assert!(first.was_written_by(client.guid()));
        assert_eq!(first.writer_guid, second.writer_guid);

        client.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_call_with_no_server_times_out_rather_than_hanging() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        let error = client
            .call(
                &crate::msg::example_interfaces::AddTwoIntsRequest { a: 1, b: 2 },
                StdDuration::from_millis(30),
            )
            .await
            .expect_err("no server");
        assert!(matches!(error, Ros2Error::Timeout { .. }));
        assert_eq!(client.pending_replies().await, 0);

        client.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_destroyed_client_stops_its_dispatcher() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");
        client.destroy().await.expect("destroy");
        client
            .destroy()
            .await
            .expect("destroying twice is harmless");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_clone_shares_the_sequence_counter() {
        let context = context().await;
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");
        let clone = client.clone();
        let request = crate::msg::example_interfaces::AddTwoIntsRequest { a: 0, b: 0 };
        let first = client.send_request(&request).await.expect("send");
        let second = clone.send_request(&request).await.expect("send");
        assert_eq!(second.sequence_number, first.sequence_number + 1);
        assert_eq!(clone.guid(), client.guid());

        client.destroy().await.expect("destroy");
        context.shutdown().await;
    }
}
