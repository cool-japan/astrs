//! [`ServiceServer`]: the answering half of a service.

use core::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::ServiceType;
use crate::error::{Ros2Error, Ros2Result};
use crate::names::{FullName, TopicKind};
use crate::node::Ros2Context;
use crate::pubsub::{RawPublisher, RawSubscription};
use crate::qos::QosProfile;
use crate::service::identity::{RequestId, decode_with_identity, encode_with_identity};

/// A service server: subscribes to `rq/…Request`, publishes on
/// `rr/…Reply`.
///
/// # Take, then answer — not a callback
///
/// The API is [`take_request`](ServiceServer::take_request) followed by
/// [`send_response`](ServiceServer::send_response), with a
/// [`RequestId`] in between, rather than a `fn(Request) -> Response`
/// callback. That is not a style preference: an action's `get_result`
/// endpoint is a service whose answer may be *minutes* away, because the
/// goal it asks about has not finished yet. A callback shape forces the
/// server to hold a task open for the whole goal, and every action server
/// built on it inherits that. Separating the two lets a server accept a
/// request, put its identity in a table, and answer when the work is done.
///
/// [`serve`](ServiceServer::serve) is the callback shape, built on the
/// general one, for the common case where the answer is immediate.
#[derive(Debug)]
pub struct ServiceServer<S: ServiceType> {
    inner: Arc<ServerInner>,
    marker: PhantomData<fn() -> S>,
}

impl<S: ServiceType> Clone for ServiceServer<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            marker: PhantomData,
        }
    }
}

#[derive(Debug)]
struct ServerInner {
    context: Arc<Ros2Context>,
    request: RawSubscription,
    reply: RawPublisher,
    service: FullName,
}

impl<S: ServiceType> ServiceServer<S> {
    /// Create a server for `service`.
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
        let request = RawSubscription::for_kind(
            Arc::clone(&context),
            service.clone(),
            TopicKind::Request,
            S::REQUEST_DDS_NAME,
            qos,
            owner.clone(),
        )
        .await?;
        let reply = RawPublisher::for_kind(
            Arc::clone(&context),
            service.clone(),
            TopicKind::Reply,
            S::RESPONSE_DDS_NAME,
            qos,
            owner,
        )
        .await?;

        Ok(Self {
            inner: Arc::new(ServerInner {
                context,
                request,
                reply,
                service,
            }),
            marker: PhantomData,
        })
    }

    /// The fully-qualified service name.
    #[must_use]
    pub fn service(&self) -> &FullName {
        &self.inner.service
    }

    /// The DDS request topic this server subscribes to.
    #[must_use]
    pub fn request_topic(&self) -> String {
        self.inner.service.dds_name(TopicKind::Request)
    }

    /// The DDS reply topic this server publishes on.
    #[must_use]
    pub fn reply_topic(&self) -> String {
        self.inner.service.dds_name(TopicKind::Reply)
    }

    /// The reply publisher's GUID.
    #[must_use]
    pub fn guid(&self) -> astrs_rtps::structure::Guid {
        self.inner.reply.guid()
    }

    /// How many clients have been discovered.
    pub async fn client_count(&self) -> usize {
        self.inner.request.publisher_count().await
    }

    /// Wait until at least one client has been discovered.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_client(&self, timeout: StdDuration) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.inner.context,
            timeout,
            "waiting for a service client",
            || async { self.client_count().await > 0 },
        )
        .await
    }

    /// Take the next request, waiting for one.
    ///
    /// A request whose payload will not decode is *dropped and the wait
    /// continues*, rather than returned as an error. A malformed request is
    /// one peer's fault, and letting it break every subsequent call on the
    /// service would turn one bad client into a denial of service.
    pub async fn take_request(&self) -> (RequestId, S::Request) {
        loop {
            let (payload, _) = self.inner.request.recv().await;
            match decode_with_identity::<S::Request>(&payload) {
                Ok(pair) => return pair,
                Err(error) => tracing::debug!(
                    service = %self.inner.service,
                    %error,
                    "dropping a request that would not decode"
                ),
            }
        }
    }

    /// Take the next request, or give up after `timeout`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when nothing arrives in time.
    pub async fn take_request_within(
        &self,
        timeout: StdDuration,
    ) -> Ros2Result<(RequestId, S::Request)> {
        tokio::time::timeout(timeout, self.take_request())
            .await
            .map_err(|_| crate::pubsub::timed_out("taking a service request", timeout))
    }

    /// Take a request if one is already waiting.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::MalformedRequestHeader`] when what is waiting is not a
    /// well-formed request — reported here rather than dropped, because a
    /// non-blocking caller has asked to be told about everything that
    /// arrived.
    pub async fn try_take_request(&self) -> Ros2Result<Option<(RequestId, S::Request)>> {
        let Some((payload, _)) = self.inner.request.try_recv().await else {
            return Ok(None);
        };
        match decode_with_identity::<S::Request>(&payload) {
            Ok(pair) => Ok(Some(pair)),
            Err(_) if payload.len() < 4 + crate::service::SAMPLE_IDENTITY_LEN => {
                Err(Ros2Error::MalformedRequestHeader {
                    topic: self.request_topic(),
                    what: "sample identity",
                    len: payload.len(),
                    needed: 4 + crate::service::SAMPLE_IDENTITY_LEN,
                })
            }
            Err(error) => Err(Ros2Error::Cdr(error)),
        }
    }

    /// Answer a request taken earlier.
    ///
    /// `id` is echoed into the reply verbatim; that echo *is* the
    /// correlation.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] when the response will not encode, plus whatever
    /// the publisher reports.
    pub async fn send_response(&self, id: RequestId, response: &S::Response) -> Ros2Result<()> {
        let payload = encode_with_identity(id, response)?;
        self.inner.reply.publish_bytes(payload).await
    }

    /// Answer every request with `handler` until the context shuts down.
    ///
    /// The immediate-answer shape, for a service whose reply does not
    /// depend on anything else finishing.
    ///
    /// # Errors
    ///
    /// Whatever [`send_response`](Self::send_response) reports; a handler
    /// that itself fails is the caller's business, which is why the handler
    /// returns a plain `Response`.
    pub async fn serve<F>(&self, mut handler: F) -> Ros2Result<()>
    where
        F: FnMut(&S::Request) -> S::Response + Send,
    {
        loop {
            let (id, request) = self.take_request().await;
            let response = handler(&request);
            match self.send_response(id, &response).await {
                Ok(()) => {}
                Err(Ros2Error::NodeShutDown) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    /// Serve `count` requests and then return.
    ///
    /// What a test wants: a bounded version of [`serve`](Self::serve) that
    /// terminates without needing the context torn down under it.
    ///
    /// # Errors
    ///
    /// As [`serve`](Self::serve), plus [`Ros2Error::Timeout`] when a request
    /// does not arrive in time.
    pub async fn serve_n<F>(
        &self,
        count: usize,
        timeout: StdDuration,
        mut handler: F,
    ) -> Ros2Result<()>
    where
        F: FnMut(&S::Request) -> S::Response + Send,
    {
        for _ in 0..count {
            let (id, request) = self.take_request_within(timeout).await?;
            let response = handler(&request);
            self.send_response(id, &response).await?;
        }
        Ok(())
    }

    /// Delete both endpoints.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn destroy(&self) -> Ros2Result<()> {
        self.inner.request.destroy().await?;
        self.inner.reply.destroy().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::msg::example_interfaces::{AddTwoIntsRequest, AddTwoIntsResponse};
    use crate::node::{ContextOptions, Ros2Context};
    use crate::service::{AddTwoInts, ServiceClient};

    async fn context() -> Arc<Ros2Context> {
        Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind")
    }

    async fn linked() -> (Arc<Ros2Context>, Arc<Ros2Context>) {
        let server = context().await;
        let client = Ros2Context::loopback([server.metatraffic_locator()])
            .await
            .expect("bind");
        (server, client)
    }

    #[tokio::test]
    async fn a_server_announces_both_mangled_topics() {
        let context = context().await;
        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");

        assert_eq!(server.request_topic(), "rq/add_two_intsRequest");
        assert_eq!(server.reply_topic(), "rr/add_two_intsReply");
        assert_eq!(server.client_count().await, 0);
        server.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn taking_a_request_with_none_pending_times_out() {
        let context = context().await;
        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&context),
            FullName::service("/add_two_ints").expect("valid"),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");
        assert!(
            server
                .take_request_within(StdDuration::from_millis(20))
                .await
                .is_err()
        );
        assert!(server.try_take_request().await.expect("no error").is_none());
        server.destroy().await.expect("destroy");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_round_trip_carries_the_identity_back() {
        let (server_context, client_context) = linked().await;
        let name = FullName::service("/add_two_ints").expect("valid");

        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&server_context),
            name.clone(),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&client_context),
            name,
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        client
            .wait_for_service(StdDuration::from_secs(10))
            .await
            .expect("discovered");
        server
            .wait_for_client(StdDuration::from_secs(10))
            .await
            .expect("discovered");

        let serving = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .serve_n(1, StdDuration::from_secs(10), |request| {
                        AddTwoIntsResponse {
                            sum: request.a + request.b,
                        }
                    })
                    .await
            })
        };

        let response = client
            .call(
                &AddTwoIntsRequest { a: 2, b: 40 },
                StdDuration::from_secs(10),
            )
            .await
            .expect("a reply");
        assert_eq!(response.sum, 42);
        serving.await.expect("no panic").expect("served");

        client.destroy().await.expect("destroy");
        server.destroy().await.expect("destroy");
        server_context.shutdown().await;
        client_context.shutdown().await;
    }

    #[tokio::test]
    async fn a_deferred_answer_works_as_well_as_an_immediate_one() {
        let (server_context, client_context) = linked().await;
        let name = FullName::service("/add_two_ints").expect("valid");

        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&server_context),
            name.clone(),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&client_context),
            name,
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");
        client
            .wait_for_service(StdDuration::from_secs(10))
            .await
            .expect("discovered");

        // The shape an action's `get_result` needs: take the request, put it
        // aside, answer later — with other work in between.
        let id = client
            .send_request(&AddTwoIntsRequest { a: 1, b: 1 })
            .await
            .expect("send");
        let (taken, request) = server
            .take_request_within(StdDuration::from_secs(10))
            .await
            .expect("a request");
        assert_eq!(taken.sequence_number, id.sequence_number);
        assert_eq!(request.a, 1);

        // …later.
        server
            .send_response(taken, &AddTwoIntsResponse { sum: 2 })
            .await
            .expect("answer");
        let response = client
            .take_response(id, StdDuration::from_secs(10))
            .await
            .expect("a reply");
        assert_eq!(response.sum, 2);

        client.destroy().await.expect("destroy");
        server.destroy().await.expect("destroy");
        server_context.shutdown().await;
        client_context.shutdown().await;
    }

    #[tokio::test]
    async fn two_clients_on_one_service_do_not_cross_talk() {
        // Both clients subscribe to the same `rr/…Reply` topic and both may
        // use sequence number 1. Only the writer GUID separates them.
        let (server_context, first_context) = linked().await;
        let second_context = Ros2Context::loopback([server_context.metatraffic_locator()])
            .await
            .expect("bind");
        let name = FullName::service("/add_two_ints").expect("valid");

        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&server_context),
            name.clone(),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");
        let first = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&first_context),
            name.clone(),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");
        let second = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&second_context),
            name,
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");

        first
            .wait_for_service(StdDuration::from_secs(10))
            .await
            .expect("discovered");
        second
            .wait_for_service(StdDuration::from_secs(10))
            .await
            .expect("discovered");
        assert_ne!(first.guid(), second.guid());

        let serving = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .serve_n(2, StdDuration::from_secs(10), |request| {
                        AddTwoIntsResponse {
                            sum: request.a * 1_000 + request.b,
                        }
                    })
                    .await
            })
        };

        let (left, right) = tokio::join!(
            first.call(
                &AddTwoIntsRequest { a: 1, b: 1 },
                StdDuration::from_secs(10)
            ),
            second.call(
                &AddTwoIntsRequest { a: 2, b: 2 },
                StdDuration::from_secs(10)
            ),
        );
        assert_eq!(left.expect("first client's reply").sum, 1_001);
        assert_eq!(
            right.expect("second client's reply").sum,
            2_002,
            "each client kept its own reply even though both used sequence 1"
        );
        serving.await.expect("no panic").expect("served");

        first.destroy().await.expect("destroy");
        second.destroy().await.expect("destroy");
        server.destroy().await.expect("destroy");
        server_context.shutdown().await;
        first_context.shutdown().await;
        second_context.shutdown().await;
    }

    #[tokio::test]
    async fn one_client_with_two_calls_in_flight_matches_both() {
        let (server_context, client_context) = linked().await;
        let name = FullName::service("/add_two_ints").expect("valid");

        let server = ServiceServer::<AddTwoInts>::new(
            Arc::clone(&server_context),
            name.clone(),
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("server");
        let client = ServiceClient::<AddTwoInts>::new(
            Arc::clone(&client_context),
            name,
            QosProfile::services_default(),
            None,
        )
        .await
        .expect("client");
        client
            .wait_for_service(StdDuration::from_secs(10))
            .await
            .expect("discovered");

        // Answer in the reverse order the requests arrived, so a client that
        // matched on arrival order rather than on sequence number would swap
        // the two answers.
        let first = client
            .send_request(&AddTwoIntsRequest { a: 10, b: 0 })
            .await
            .expect("send");
        let second = client
            .send_request(&AddTwoIntsRequest { a: 20, b: 0 })
            .await
            .expect("send");

        let (first_id, first_request) = server
            .take_request_within(StdDuration::from_secs(10))
            .await
            .expect("a request");
        let (second_id, second_request) = server
            .take_request_within(StdDuration::from_secs(10))
            .await
            .expect("a request");

        server
            .send_response(
                second_id,
                &AddTwoIntsResponse {
                    sum: second_request.a,
                },
            )
            .await
            .expect("answer");
        server
            .send_response(
                first_id,
                &AddTwoIntsResponse {
                    sum: first_request.a,
                },
            )
            .await
            .expect("answer");

        assert_eq!(
            client
                .take_response(first, StdDuration::from_secs(10))
                .await
                .expect("a reply")
                .sum,
            10
        );
        assert_eq!(
            client
                .take_response(second, StdDuration::from_secs(10))
                .await
                .expect("a reply")
                .sum,
            20
        );

        client.destroy().await.expect("destroy");
        server.destroy().await.expect("destroy");
        server_context.shutdown().await;
        client_context.shutdown().await;
    }
}
