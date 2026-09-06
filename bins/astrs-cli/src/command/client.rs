//! The one connection every client verb shares: CLI → coordinator.
//!
//! ```text
//!   astrs list / stop / logs -f / param … ─┐
//!                                          ▼
//!            TcpStream ─► FramedDuplex ─► initiate(Hello{role=Cli, token})
//!                                          │
//!            ControlRequest ───────────────┤  "stream 0"
//!            ControlReply   ◄──────────────┤
//!            Log / Data frames ◄───────────┘  pushed for a subscription
//! ```
//!
//! Blueprint §16 in one place: the greeting carries [`Role::Cli`] and the
//! 64-hex cluster token, and a coordinator that refuses either says why in a
//! typed [`astrs_wire::Refused`] the transport turns into an error this
//! module reports with the address that was dialled attached — "connection
//! refused" without an address is the least useful error a CLI can print.
//!
//! # Why not `astrs_transport::backend::connect`
//!
//! That entry point returns a [`astrs_transport::Connection`] — a route
//! multiplexer with per-route streams, credit and datagrams, built for the
//! daemon↔daemon *data* plane. A CLI has exactly one logical stream (§6.4's
//! "stream 0") and needs the raw request/reply frames on it, which is what
//! [`astrs_transport::FramedDuplex`] plus [`astrs_transport::initiate`]
//! give directly. Using the mux here would add a header to every frame the
//! coordinator's CLI session does not speak.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DataflowId, DataflowSummary, FeatureFlags, Frame,
    FrameKind, FrameLimits, Role, SessionId, SubscriptionId,
};
use tokio::net::TcpStream;

use crate::error::CliError;

/// How long the greeting has before a dial is called dead.
///
/// Matches `astrs-coordinator`'s own `DEFAULT_HANDSHAKE_TIMEOUT`: a CLI that
/// waited longer than the server is willing to wait would report a timeout
/// the server had already given up on.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a single request waits for its reply before giving up.
///
/// Generous, because the requests behind it are not all cheap — `Start`
/// waits for a placement decision, `Destroy` for a cluster-wide stop — but
/// bounded, because a CLI that hangs forever on a wedged coordinator is
/// indistinguishable from a CLI that crashed.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything needed to reach one coordinator.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The resolved address.
    pub addr: SocketAddr,
    /// The token presented in the greeting.
    pub token: AuthToken,
    /// What the address was written as, for error messages.
    pub display: String,
}

impl Endpoint {
    /// An endpoint for `addr`, authenticating with `token`.
    #[must_use]
    pub fn new(addr: SocketAddr, token: AuthToken) -> Self {
        Self {
            addr,
            token,
            display: addr.to_string(),
        }
    }
}

/// Resolves a `--coordinator` argument (or its absence) into an address.
///
/// Accepts, in order of how often a user types them: nothing (loopback on
/// the default port, or `ASTRS_COORDINATOR_ADDR`), a bare port (`7407`), a
/// bare host (`10.0.0.4`, default port), and a full `host:port`. A name is
/// resolved through [`ToSocketAddrs`], preferring the first address the
/// resolver returns.
///
/// # Errors
///
/// [`CliError::BadAddress`] naming what was typed and why it could not be
/// used.
pub fn resolve_addr(argument: Option<&str>) -> Result<SocketAddr, CliError> {
    let text = match argument {
        Some(text) => text.to_owned(),
        None => match std::env::var(crate::runtime_dir::ENV_COORDINATOR_ADDR) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => format!("127.0.0.1:{}", default_port()),
        },
    };
    resolve_text(&text)
}

/// The coordinator port this machine defaults to (§24.2).
#[must_use]
pub fn default_port() -> u16 {
    std::env::var(astrs_coordinator::ENV_COORDINATOR_PORT)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(astrs_coordinator::DEFAULT_COORDINATOR_PORT)
}

/// The resolution rules [`resolve_addr`] documents, without the environment
/// lookup — the part worth testing directly.
///
/// # Errors
///
/// [`CliError::BadAddress`] when nothing resolves.
pub fn resolve_text(text: &str) -> Result<SocketAddr, CliError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CliError::BadAddress {
            input: text.to_owned(),
            reason: "the address is empty".to_owned(),
        });
    }
    // A bare port: the shortest thing a user types for "the local one, but
    // over there".
    if let Ok(port) = trimmed.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    // A literal or a name, with or without a port.
    let candidate = if trimmed.contains(':') && !trimmed.ends_with(':') {
        trimmed.to_owned()
    } else {
        format!("{}:{}", trimmed.trim_end_matches(':'), default_port())
    };
    let mut resolved = candidate
        .to_socket_addrs()
        .map_err(|error| CliError::BadAddress {
            input: text.to_owned(),
            reason: error.to_string(),
        })?;
    resolved.next().ok_or_else(|| CliError::BadAddress {
        input: text.to_owned(),
        reason: "the name resolved to no addresses".to_owned(),
    })
}

/// Builds the endpoint a client verb should dial from its flags.
///
/// `require_token` distinguishes the verbs that *mutate* a cluster (§16's
/// "mutating verbs") from a bare probe: without a token the former refuses
/// locally with [`CliError::NoToken`], naming every way one could be
/// supplied, rather than dialling and being refused by the far end with a
/// message about protocol details the user did not ask about.
///
/// # Errors
///
/// - [`CliError::BadAddress`] if the address cannot be resolved.
/// - [`CliError::NoToken`] when `require_token` and none was found.
/// - Whatever [`crate::runtime_dir::find_token`] returns.
pub fn endpoint(
    coordinator: Option<&str>,
    token: Option<&str>,
    token_file: Option<&Path>,
    working_dir: Option<&Path>,
    require_token: bool,
) -> Result<Endpoint, CliError> {
    let addr = resolve_addr(coordinator)?;
    let dir = crate::runtime_dir::working_dir(working_dir);
    let found = crate::runtime_dir::find_token(token, token_file, &dir)?;
    let token = match found {
        Some((token, _)) => token,
        None if require_token => {
            return Err(CliError::NoToken {
                env: crate::runtime_dir::ENV_TOKEN,
                file: crate::runtime_dir::TOKEN_FILE,
            });
        }
        // A development coordinator started without a token accepts the
        // all-zero one; refusing to try would make `astrs list` useless
        // against exactly the setup a first-time user has.
        None => AuthToken::ZERO,
    };
    Ok(Endpoint {
        display: coordinator.map_or_else(|| addr.to_string(), ToOwned::to_owned),
        addr,
        token,
    })
}

/// One connected CLI session.
///
/// Owns the socket and nothing else: every verb drives it with
/// [`Client::request`] and, for a subscription, [`Client::next_frame`].
#[derive(Debug)]
pub struct Client {
    /// The framed socket.
    stream: FramedStream<TcpStream>,
    /// The session the coordinator assigned.
    session: SessionId,
    /// How the endpoint was named, for errors.
    display: String,
}

impl Client {
    /// Dials `endpoint` and greets it as [`Role::Cli`].
    ///
    /// # Errors
    ///
    /// - [`CliError::NoCluster`] if nothing is listening — the one failure a
    ///   user hits constantly, reported as "start one with `astrs up`"
    ///   rather than as a raw `ECONNREFUSED`.
    /// - [`CliError::Transport`] for a refusal or a protocol failure.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, CliError> {
        let raw = TcpStream::connect(endpoint.addr).await.map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) {
                CliError::NoCluster {
                    detail: format!("nothing answered at {}: {error}", endpoint.display),
                }
            } else {
                CliError::io(&endpoint.display, error)
            }
        })?;
        // Nagle off: every frame this connection sends is a whole request or
        // a whole reply, and waiting to coalesce them adds latency to every
        // single CLI invocation for no bandwidth gain.
        let _ = raw.set_nodelay(true);

        let mut stream =
            FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Cli), endpoint.token.clone())
            .with_features(FeatureFlags::EMPTY);
        let handshake = initiate(&mut stream, &params, HANDSHAKE_TIMEOUT).await?;
        Ok(Self {
            stream,
            session: handshake.session.session_id,
            display: endpoint.display.clone(),
        })
    }

    /// The session the coordinator assigned this connection.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// How the endpoint was named, for a caller's own messages.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Sends one request and waits for its [`ControlReply`].
    ///
    /// `label` names the verb in any error, so a failure reads
    /// "the coordinator refused `list`" rather than naming a wire variant
    /// the user never typed.
    ///
    /// # Errors
    ///
    /// - [`CliError::Transport`] if the socket fails or the reply does not
    ///   arrive within [`REQUEST_TIMEOUT`].
    /// - [`CliError::Refused`] if the coordinator answered
    ///   [`ControlReply::Error`].
    pub async fn request(
        &mut self,
        label: &'static str,
        request: &ControlRequest,
    ) -> Result<ControlReply, CliError> {
        self.stream.send_message(request).await?;
        let reply = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.stream
                .expect_message::<ControlReply>(FrameKind::ControlReply),
        )
        .await
        .map_err(|_| astrs_transport::TransportError::Timeout {
            operation: "control reply",
            timeout: REQUEST_TIMEOUT,
        })??;
        match reply {
            ControlReply::Error {
                message, context, ..
            } => Err(CliError::Refused {
                request: label,
                message: if context.is_empty() {
                    message
                } else {
                    format!("{message} ({})", context.join("; "))
                },
            }),
            other => Ok(other),
        }
    }

    /// Sends one request and requires a plain [`ControlReply::Ok`].
    ///
    /// # Errors
    ///
    /// As [`Client::request`], plus [`CliError::UnexpectedReply`] when the
    /// answer is neither `Ok` nor an error.
    pub async fn request_ok(
        &mut self,
        label: &'static str,
        request: &ControlRequest,
    ) -> Result<(), CliError> {
        match self.request(label, request).await? {
            ControlReply::Ok => Ok(()),
            other => Err(CliError::UnexpectedReply {
                request: label,
                reply: reply_name(&other),
            }),
        }
    }

    /// Waits for the next frame the coordinator *pushes* — a `Log` for an
    /// open `LogSubscribe`, a `Data` for a topic tap.
    ///
    /// Returns `Ok(None)` when the coordinator closed the connection, which
    /// is how a `logs -f` ends when the cluster goes down rather than an
    /// error a user should see a backtrace-shaped message for.
    ///
    /// # Errors
    ///
    /// [`CliError::Transport`] if the socket fails.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>, CliError> {
        Ok(self.stream.recv_frame().await?)
    }

    /// Closes the connection politely.
    ///
    /// # Errors
    ///
    /// [`CliError::Transport`] if the shutdown fails — reported rather than
    /// swallowed so a caller that cares (a script asserting a clean
    /// teardown) can, while every ordinary caller ignores it.
    pub async fn close(self) -> Result<(), CliError> {
        let (_, mut writer) = self.stream.into_halves();
        writer.shutdown().await?;
        Ok(())
    }

    /// Every dataflow the coordinator knows about.
    ///
    /// # Errors
    ///
    /// As [`Client::request`], plus [`CliError::UnexpectedReply`] when the
    /// answer is not a `DataflowList`.
    pub async fn list_dataflows(&mut self, all: bool) -> Result<Vec<DataflowSummary>, CliError> {
        match self.request("list", &ControlRequest::List { all }).await? {
            ControlReply::DataflowList { dataflows, .. } => Ok(dataflows),
            other => Err(CliError::UnexpectedReply {
                request: "list",
                reply: reply_name(&other),
            }),
        }
    }

    /// Turns a user-typed reference into the id every request but
    /// `StopByName`/`RestartByName` needs.
    ///
    /// An id is taken as written — no round trip, so `astrs logs <uuid>`
    /// works against a coordinator whose `List` is momentarily large. A name
    /// costs one `List { all: true }`: names are not unique by construction
    /// (nothing stops two runs sharing one), so an ambiguous name is refused
    /// with the ids to choose between rather than resolved arbitrarily.
    ///
    /// # Errors
    ///
    /// - As [`Client::request`].
    /// - [`CliError::UnknownDataflow`] when nothing matches, or when more
    ///   than one dataflow answers to the same name.
    pub async fn resolve(&mut self, reference: &DataflowRef) -> Result<DataflowId, CliError> {
        let name = match reference {
            DataflowRef::Id(id) => return Ok(*id),
            DataflowRef::Name(name) => name.clone(),
        };
        let dataflows = self.list_dataflows(true).await?;
        let matched: Vec<&DataflowSummary> = dataflows
            .iter()
            .filter(|summary| summary.name.as_deref() == Some(name.as_str()))
            .collect();
        match matched.as_slice() {
            [one] => Ok(one.id),
            [] => Err(CliError::UnknownDataflow {
                reference: name,
                reason: describe_known(&dataflows),
            }),
            many => Err(CliError::UnknownDataflow {
                reference: name,
                reason: format!(
                    "{} dataflows share that name; name one by id: {}",
                    many.len(),
                    many.iter()
                        .map(|summary| summary.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }

    /// The one dataflow a verb should act on when the user named none.
    ///
    /// Convenience with a hard edge: exactly one candidate is resolved, and
    /// anything else is refused. Guessing between two running graphs is how
    /// a CLI stops a robot's perception stack when the user meant to tail
    /// its planner.
    ///
    /// # Errors
    ///
    /// - As [`Client::request`].
    /// - [`CliError::UnknownDataflow`] when zero, or more than one, dataflow
    ///   is known.
    pub async fn sole_dataflow(&mut self, all: bool) -> Result<DataflowId, CliError> {
        let dataflows = self.list_dataflows(all).await?;
        match dataflows.as_slice() {
            [one] => Ok(one.id),
            [] => Err(CliError::UnknownDataflow {
                reference: "(none named)".to_owned(),
                reason: "this cluster has no dataflow to act on".to_owned(),
            }),
            many => Err(CliError::UnknownDataflow {
                reference: "(none named)".to_owned(),
                reason: format!(
                    "{} dataflows are known, so one must be named: {}",
                    many.len(),
                    describe_known(&dataflows)
                ),
            }),
        }
    }
}

/// A short "here is what does exist" list for an unresolved reference.
fn describe_known(dataflows: &[DataflowSummary]) -> String {
    if dataflows.is_empty() {
        return "this cluster knows no dataflows at all".to_owned();
    }
    let known: Vec<String> = dataflows
        .iter()
        .take(8)
        .map(DataflowSummary::display_name)
        .collect();
    format!("known dataflows: {}", known.join(", "))
}

/// Mints a subscription id no other client is likely to be using.
///
/// The coordinator's subscription registry is keyed by id across *every*
/// connected session and an insert replaces whatever was there, so two CLIs
/// that both picked [`SubscriptionId::FIRST`] would silently steal each
/// other's stream. There is no server-side allocator to ask (the protocol
/// has the client choose, §7.3), so the id is minted here from three things
/// that cannot all coincide between two live invocations: this process's
/// id, the nanoseconds since the epoch, and a per-process counter for the
/// several subscriptions one invocation opens.
///
/// Never [`SubscriptionId::NONE`], which the protocol reserves for "no
/// subscription".
#[must_use]
pub fn new_subscription_id() -> SubscriptionId {
    /// The per-process sequence, so two subscriptions opened in the same
    /// nanosecond still differ.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        });
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [
        u64::from(std::process::id()),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed),
    ] {
        for byte in part.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    SubscriptionId::new(hash.max(1))
}

/// The variant name of a reply, for [`CliError::UnexpectedReply`].
///
/// Spelled out rather than derived from `Debug` so an error message never
/// prints a whole payload — a `Logs` reply's `Debug` form is thousands of
/// lines of records.
#[must_use]
pub fn reply_name(reply: &ControlReply) -> &'static str {
    match reply {
        ControlReply::Ok => "Ok",
        ControlReply::Error { .. } => "Error",
        ControlReply::DataflowList { .. } => "DataflowList",
        ControlReply::DataflowResult { .. } => "DataflowResult",
        ControlReply::NodeInfo { .. } => "NodeInfo",
        ControlReply::DaemonList { .. } => "DaemonList",
        ControlReply::Logs { .. } => "Logs",
        ControlReply::ParamValue { .. } => "ParamValue",
        ControlReply::ParamList { .. } => "ParamList",
        ControlReply::TraceData { .. } => "TraceData",
        ControlReply::Refused(_) => "Refused",
        ControlReply::Welcome(_) => "Welcome",
        ControlReply::BuildStarted { .. } => "BuildStarted",
        ControlReply::Started { .. } => "Started",
        // `ControlReply` is `#[non_exhaustive]`: a variant added at the tail
        // (§3, append-only evolution) must still be nameable here.
        _ => "an unknown reply",
    }
}

/// Resolves a user-typed dataflow reference — an id or a name.
///
/// A [`DataflowId`] is a UUID; anything that does not parse as one is taken
/// as the `--name` a run was started under, which is what a user actually
/// types. The two are kept apart here rather than at each call site because
/// the wire protocol has *separate requests* for them
/// ([`ControlRequest::Stop`] vs [`ControlRequest::StopByName`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataflowRef {
    /// An explicit dataflow id.
    Id(DataflowId),
    /// A name given at `Start`.
    Name(String),
}

impl DataflowRef {
    /// Classifies one user-typed reference.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        text.trim()
            .parse::<DataflowId>()
            .map_or_else(|_| Self::Name(text.trim().to_owned()), Self::Id)
    }

    /// The id, when this reference is one.
    #[must_use]
    pub const fn id(&self) -> Option<DataflowId> {
        match self {
            Self::Id(id) => Some(*id),
            Self::Name(_) => None,
        }
    }
}

impl std::fmt::Display for DataflowRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(id) => write!(f, "{id}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

/// Builds a current-thread tokio runtime for one client verb.
///
/// Current-thread on purpose: a client verb is one socket and one
/// request/reply at a time, and spinning up a thread pool to wait on a
/// single `recv` is pure startup latency on a command a user runs
/// interactively.
///
/// # Errors
///
/// [`CliError::Io`] if the runtime cannot be built.
pub fn runtime() -> Result<tokio::runtime::Runtime, CliError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::io("tokio runtime", source))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-client-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bare_port_means_loopback() {
        let addr = resolve_text("7407").unwrap();
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 7407)));
    }

    #[test]
    fn a_host_and_port_is_taken_as_written() {
        let addr = resolve_text("127.0.0.1:9001").unwrap();
        assert_eq!(addr.port(), 9001);
    }

    #[test]
    fn a_bare_host_gets_the_default_port() {
        let addr = resolve_text("127.0.0.1").unwrap();
        assert_eq!(addr.port(), default_port());
    }

    #[test]
    fn an_empty_address_is_refused_with_what_was_typed() {
        let error = resolve_text("   ").unwrap_err();
        match error {
            CliError::BadAddress { input, .. } => assert_eq!(input, "   "),
            other => panic!("expected BadAddress, got {other}"),
        }
    }

    #[test]
    fn a_name_that_resolves_to_nothing_is_refused() {
        let error = resolve_text("no-such-host.invalid:1").unwrap_err();
        assert!(matches!(error, CliError::BadAddress { .. }), "{error}");
    }

    #[test]
    fn a_mutating_verb_without_a_token_refuses_locally() {
        let dir = scratch("no-token");
        let error = endpoint(Some("127.0.0.1:1"), None, None, Some(&dir), true).unwrap_err();
        match error {
            CliError::NoToken { file, .. } => {
                assert_eq!(file, crate::runtime_dir::TOKEN_FILE);
            }
            other => panic!("expected NoToken, got {other}"),
        }
    }

    #[test]
    fn a_probe_without_a_token_falls_back_to_the_zero_token() {
        let dir = scratch("zero-token");
        let endpoint = endpoint(Some("127.0.0.1:1"), None, None, Some(&dir), false).unwrap();
        assert!(endpoint.token.is_zero());
    }

    #[test]
    fn a_token_file_is_found_and_used() {
        let dir = scratch("with-token");
        let (token, _) = crate::runtime_dir::ensure_token_file(&dir).unwrap();
        let endpoint = endpoint(Some("127.0.0.1:1"), None, None, Some(&dir), true).unwrap();
        assert_eq!(endpoint.token, token);
    }

    #[test]
    fn the_endpoint_remembers_how_the_address_was_typed() {
        let dir = scratch("display");
        let endpoint = endpoint(Some("7407"), None, None, Some(&dir), false).unwrap();
        assert_eq!(endpoint.display, "7407");
        assert_eq!(endpoint.addr.port(), 7407);
    }

    #[test]
    fn a_dataflow_reference_is_an_id_or_a_name() {
        let id = DataflowId::generate();
        assert_eq!(DataflowRef::parse(&id.to_string()), DataflowRef::Id(id));
        assert_eq!(
            DataflowRef::parse(" my-flow "),
            DataflowRef::Name("my-flow".to_owned())
        );
        assert_eq!(DataflowRef::parse(&id.to_string()).id(), Some(id));
        assert!(DataflowRef::parse("my-flow").id().is_none());
    }

    #[test]
    fn every_reply_variant_has_a_name() {
        assert_eq!(reply_name(&ControlReply::Ok), "Ok");
        assert_eq!(
            reply_name(&ControlReply::Error {
                code: astrs_wire::ErrorCode::Internal,
                message: String::new(),
                context: Vec::new(),
            }),
            "Error"
        );
    }

    #[test]
    fn connecting_to_nothing_says_to_run_astrs_up() {
        let dir = scratch("no-cluster");
        // Port 1 on loopback: privileged, never bound by this test suite,
        // and refused immediately rather than timing out.
        let endpoint = endpoint(Some("127.0.0.1:1"), None, None, Some(&dir), false).unwrap();
        let runtime = runtime().unwrap();
        let error = runtime.block_on(Client::connect(&endpoint)).unwrap_err();
        let rendered = error.to_string();
        match error {
            CliError::NoCluster { detail } => assert!(detail.contains("127.0.0.1:1"), "{detail}"),
            other => panic!("expected NoCluster, got {other}"),
        }
        assert!(rendered.contains("astrs up"), "{rendered}");
    }
}
