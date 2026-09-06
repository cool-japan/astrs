//! Request/response over ordinary edges (blueprint §9.4, `request_id`).
//!
//! ```text
//!   client                                     server
//!     │ service_request(req)  ──► request_id=R ──►│
//!     │                                           │ ServiceRequest::from_event
//!     │◄── request_id=R ◄──  service_response(…) ──┤
//!     │ ServiceResponse::matches(R)               │
//! ```
//!
//! A service is two edges and one metadata key. The client stamps a fresh
//! `request_id`; the server copies it onto the answer; the client matches it.
//! Nothing else is needed, and nothing else is provided — which is why a
//! service in AstRS records, replays and type-checks exactly like any other
//! edge.
//!
//! # What goes wrong without help
//!
//! Answering with *no* `request_id`, or with a fresh one, leaves a client
//! waiting forever. [`Node::service_response`] takes the request's metadata
//! rather than an id, so the id it copies is the one that arrived; and it
//! *refuses* — [`crate::NodeError::Pattern`] — when the metadata carries no
//! request id at all, rather than sending an uncorrelated answer.

use core::fmt;

use astrs_data::AstrsMessage;
use astrs_wire::{DataId, Metadata};

use crate::error::{NodeError, Result};
use crate::events::Event;
use crate::node::Node;
use crate::output::{Output, RawOutput};
use crate::payload::Payload;

/// One service exchange's correlation id (§9.4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// A fresh id.
    #[must_use]
    pub fn generate() -> Self {
        Self(super::fresh_id())
    }

    /// Wraps an existing id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the id.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for RequestId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl PartialEq<str> for RequestId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// An incoming service request, as the server sees it.
#[derive(Debug)]
pub struct ServiceRequest {
    /// The input it arrived on.
    pub input: DataId,
    /// Its correlation id.
    pub id: RequestId,
    /// The metadata to pass to [`Node::service_response`].
    pub metadata: Metadata,
    /// The request payload.
    pub payload: Payload,
}

impl ServiceRequest {
    /// Reads a request out of an event, if it is one.
    ///
    /// An input with no `request_id` is not a service request — it is an
    /// ordinary message, and a server that treated it as one would answer
    /// into the void.
    pub fn from_event(event: Event) -> core::result::Result<Self, Box<Event>> {
        let Event::Input { id, meta, data } = event else {
            return Err(Box::new(event));
        };
        let Some(request_id) = meta.request_id().map(RequestId::new) else {
            return Err(Box::new(Event::Input { id, meta, data }));
        };
        Ok(Self {
            input: id,
            id: request_id,
            metadata: meta,
            payload: data,
        })
    }

    /// Decodes the request body.
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the payload is not a `T`.
    pub fn view<T: crate::message::FromPayload>(&self) -> Result<T> {
        self.payload.view()
    }
}

/// An incoming service response, as the client sees it.
#[derive(Debug)]
pub struct ServiceResponse {
    /// The input it arrived on.
    pub input: DataId,
    /// The id of the request it answers.
    pub id: RequestId,
    /// Its metadata.
    pub metadata: Metadata,
    /// The response payload.
    pub payload: Payload,
}

impl ServiceResponse {
    /// Reads a response out of an event, if it is one.
    pub fn from_event(event: Event) -> core::result::Result<Self, Box<Event>> {
        let Event::Input { id, meta, data } = event else {
            return Err(Box::new(event));
        };
        let Some(request_id) = meta.request_id().map(RequestId::new) else {
            return Err(Box::new(Event::Input { id, meta, data }));
        };
        Ok(Self {
            input: id,
            id: request_id,
            metadata: meta,
            payload: data,
        })
    }

    /// Whether this answers `request`.
    #[must_use]
    pub fn matches(&self, request: &RequestId) -> bool {
        &self.id == request
    }

    /// Decodes the response body.
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the payload is not a `T`.
    pub fn view<T: crate::message::FromPayload>(&self) -> Result<T> {
        self.payload.view()
    }
}

impl Node {
    /// Publishes a typed service request, returning its correlation id
    /// (§9.4).
    ///
    /// # Errors
    ///
    /// As [`Output::send`].
    pub fn service_request<T: AstrsMessage>(
        &self,
        output: &mut Output<T>,
        value: impl Into<T>,
    ) -> Result<RequestId> {
        let id = RequestId::generate();
        let mut metadata = self.metadata();
        metadata.set_request_id(id.as_str());
        output.send(value, metadata)?;
        Ok(id)
    }

    /// Publishes an untyped service request, returning its correlation id.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn service_request_bytes(
        &self,
        output: &mut RawOutput,
        payload: impl AsRef<[u8]>,
    ) -> Result<RequestId> {
        let id = RequestId::generate();
        let mut metadata = self.metadata();
        metadata.set_request_id(id.as_str());
        output.send_bytes(payload, metadata)?;
        Ok(id)
    }

    /// Publishes a typed response to `request` (§9.4).
    ///
    /// `request` is the *request's* metadata; the id is copied from it, so a
    /// server cannot answer with the wrong one.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] when `request` carries no `request_id`, plus
    /// whatever [`Output::send`] reports.
    pub fn service_response<T: AstrsMessage>(
        &self,
        output: &mut Output<T>,
        request: &Metadata,
        value: impl Into<T>,
    ) -> Result<()> {
        let metadata = response_metadata(request)?;
        output.send(value, metadata)
    }

    /// Publishes an untyped response to `request`.
    ///
    /// # Errors
    ///
    /// As [`Node::service_response`].
    pub fn service_response_bytes(
        &self,
        output: &mut RawOutput,
        request: &Metadata,
        payload: impl AsRef<[u8]>,
    ) -> Result<()> {
        let metadata = response_metadata(request)?;
        output.send_bytes(payload, metadata)
    }

    /// The metadata a response to `request` must carry.
    ///
    /// Exposed for a node that builds its own payload but wants the
    /// correlation handled for it.
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] when `request` carries no `request_id`.
    pub fn response_metadata(&self, request: &Metadata) -> Result<Metadata> {
        response_metadata(request)
    }
}

/// Derives a response's metadata from its request's.
///
/// [`Metadata::follow`] keeps the correlation keys and re-stamps the clock, so
/// the answer is causally after the question and carries the same id.
fn response_metadata(request: &Metadata) -> Result<Metadata> {
    if request.request_id().is_none() {
        return Err(NodeError::Pattern(
            "a service response needs the request's `request_id`; this metadata has none"
                .to_owned(),
        ));
    }
    Ok(request.follow())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::{AstrsMessage, Scalar};
    use crate::testing::TestHarness;
    use astrs_time::HlcTimestamp;

    #[test]
    fn request_ids_are_fresh_and_comparable() {
        let first = RequestId::generate();
        let second = RequestId::generate();
        assert_ne!(first, second);
        assert_eq!(first.to_string(), first.as_str());
        assert_eq!(RequestId::new("abc"), RequestId::from("abc".to_owned()));
        assert!(RequestId::new("abc") == *"abc");
        assert_eq!(RequestId::new("abc").into_string(), "abc");
    }

    #[test]
    fn an_uncorrelated_input_is_not_a_service_message() {
        let event = Event::Input {
            id: DataId::new("in").unwrap(),
            meta: Metadata::new(HlcTimestamp::new(1, 0)),
            data: Payload::empty(),
        };
        let back = ServiceRequest::from_event(event).unwrap_err();
        assert!(back.is_input());
        let back = ServiceResponse::from_event(*back).unwrap_err();
        assert!(back.is_input());

        let control = ServiceRequest::from_event(Event::AllInputsClosed).unwrap_err();
        assert!(!control.is_input());
    }

    #[test]
    fn a_response_without_a_request_id_is_refused() {
        let mut harness = TestHarness::start().unwrap();
        let mut output = harness
            .node
            .raw_output(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        let error = harness
            .node
            .service_response_bytes(&mut output, &Metadata::default(), vec![1])
            .unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(
            harness
                .node
                .response_metadata(&Metadata::default())
                .is_err()
        );
    }

    #[test]
    fn a_full_round_trip_between_two_nodes() {
        // client --request--> server --response--> client
        let daemon = crate::testing::MockDaemon::start().unwrap();
        let client_spec = astrs_wire::NodeSpawnSpec::new(
            daemon.dataflow(),
            astrs_wire::NodeId::new("client").unwrap(),
            0,
            astrs_wire::NodeSource::Dynamic,
        )
        .with_output(astrs_wire::OutputSpec::new(DataId::new("request").unwrap()))
        .with_input(astrs_wire::InputSpec::new(
            DataId::new("response").unwrap(),
            astrs_wire::PortRef::from_parts("server", "response").unwrap(),
        ));
        let server_spec = astrs_wire::NodeSpawnSpec::new(
            daemon.dataflow(),
            astrs_wire::NodeId::new("server").unwrap(),
            0,
            astrs_wire::NodeSource::Dynamic,
        )
        .with_input(astrs_wire::InputSpec::new(
            DataId::new("request").unwrap(),
            astrs_wire::PortRef::from_parts("client", "request").unwrap(),
        ))
        .with_output(astrs_wire::OutputSpec::new(
            DataId::new("response").unwrap(),
        ));

        let (mut client, mut client_events) = daemon.connect_node(client_spec).unwrap();
        let (mut server, mut server_events) = daemon.connect_node(server_spec).unwrap();

        let mut request_out = client.output::<Scalar<f64>>("request").unwrap();
        let id = client.service_request(&mut request_out, 21.0_f64).unwrap();

        // Server side.
        let event = server_events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .expect("the request");
        let request = ServiceRequest::from_event(event).expect("a correlated request");
        assert_eq!(request.id, id);
        assert_eq!(request.input.as_str(), "request");
        let value = request.view::<Scalar<f64>>().unwrap().into_inner();

        let mut response_out = server.output::<Scalar<f64>>("response").unwrap();
        server
            .service_response(&mut response_out, &request.metadata, value * 2.0)
            .unwrap();

        // Client side.
        let event = client_events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .expect("the response");
        let response = ServiceResponse::from_event(event).expect("a correlated response");
        assert!(response.matches(&id));
        assert_eq!(response.view::<Scalar<f64>>().unwrap().into_inner(), 42.0);
        assert_eq!(response.input.as_str(), "response");
    }

    #[test]
    fn an_untyped_request_carries_a_fresh_id() {
        let mut harness = TestHarness::start().unwrap();
        let mut output = harness
            .node
            .raw_output(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        let id = harness
            .node
            .service_request_bytes(&mut output, vec![1, 2, 3])
            .unwrap();
        let sends = harness
            .daemon
            .wait_for_sends(
                harness.node.id(),
                &DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap(),
                1,
                std::time::Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(sends[0].metadata.request_id(), Some(id.as_str()));
    }

    #[test]
    fn the_urn_of_a_service_port_is_unchanged_by_correlation() {
        // Correlation is metadata; the payload type is the payload type.
        assert_eq!(<Scalar<f64> as AstrsMessage>::URN, "std/core/v1/Float64");
    }
}
