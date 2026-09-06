//! The participant: sockets, the receive loop, and everything above wired
//! together.
//!
//! This is the only asynchronous module in the crate and the only one that
//! knows a clock exists. Everything it orchestrates — the writers, the
//! readers, the proxies, the discovery database — is synchronous and takes
//! `now` as an argument, so the concurrency lives in exactly one file and can
//! be reasoned about in one sitting.
//!
//! # The locking rule
//!
//! One [`Mutex`] covers all protocol state. Nothing is ever sent while it is
//! held: a call locks, computes [`Outbound`] values, unlocks, and *then*
//! awaits the sends. That is what
//! [`RtpsWriter::produce`](crate::behavior::writer::RtpsWriter::produce)'s
//! "return what to send" shape is for, and it means there is no lock-ordering
//! question to get wrong, because there is only one lock.
//!
//! # Writes are sent inline
//!
//! [`WriterHandle::write`] puts the sample on the wire before it returns. The
//! obvious alternative — queue it and let the background loop send — makes
//! every test that writes and then awaits a sample depend on a timer, and
//! makes latency a function of the cadence. The background loop owns only
//! what is genuinely periodic: SPDP re-announcement, heartbeats, lease
//! expiry, and draining readers' ACKNACKs.
//!
//! # Two sockets, three when multicast works
//!
//! - **metatraffic unicast** — discovery traffic, and where a peer's SPDP and
//!   SEDP samples arrive.
//! - **user unicast** — application samples.
//! - **metatraffic multicast** — the `239.255.0.1` group, when the kernel
//!   allows the join. Its absence is recorded in
//!   [`Participant::multicast`], never hidden: on a sandboxed host discovery
//!   runs over [`initial peers`](crate::discovery::SpdpConfig::initial_peers)
//!   instead, which is the deterministic path this crate's tests use.
//!
//! Every socket may be bound to port 0, and the announced locators are read
//! back from `local_addr()`. See [`BindPolicy`].

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

use tokio::sync::{Mutex, Notify, broadcast};

use crate::behavior::endpoint::{EntityIdAllocator, Outbound, Sample, TopicKey};
use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::behavior::handle::{ReaderHandle, SampleSink};
use crate::behavior::liveliness::{LivelinessTracker, ParticipantMessageKind};
use crate::behavior::reader::{ReaderConfig, RtpsReader};
use crate::behavior::state::State;
use crate::behavior::transport::{MulticastCapability, RtpsSocket, UdpTransport};
use crate::behavior::writer::RtpsWriter;
use crate::discovery::db::{DiscoveryDb, DiscoveryEvent};
use crate::discovery::matching::{ReaderQos, WriterQos};
use crate::discovery::spdp::{Spdp, SpdpConfig};
use crate::security::{EndpointSecurity, SecurityContext};
use crate::structure::{EntityId, Guid, GuidPrefix, Locator, SequenceNumber, Time, port};

/// How many discovery events the broadcast channel buffers.
pub const EVENT_CAPACITY: usize = 256;

/// How often the background loop wakes to run the cadence.
pub const DEFAULT_TICK_PERIOD: StdDuration = StdDuration::from_millis(20);

/// Octets the receive buffers hold.
pub const RECEIVE_BUFFER_LEN: usize = crate::messages::MAX_UDP_PAYLOAD;

/// Where a participant's sockets are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindPolicy {
    /// Bind port 0 on loopback and announce whatever the kernel chose.
    ///
    /// The default, and the only policy under which two participants can run
    /// in one process — or two tests in parallel — without colliding. Nothing
    /// leaves the host.
    #[default]
    EphemeralLoopback,
    /// Bind port 0 on every interface and announce the given address.
    EphemeralAny(Ipv4Addr),
    /// Bind the §9.6.1.1 ports for the configured domain and participant id.
    ///
    /// What a real deployment does, and what a peer that has never met this
    /// participant expects to find it on.
    Standard(Ipv4Addr),
}

impl BindPolicy {
    /// The address to announce in locators.
    #[must_use]
    pub const fn announced_address(self) -> Ipv4Addr {
        match self {
            Self::EphemeralLoopback => Ipv4Addr::LOCALHOST,
            Self::EphemeralAny(address) | Self::Standard(address) => address,
        }
    }

    /// True when the ports are chosen by the kernel.
    #[must_use]
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::EphemeralLoopback | Self::EphemeralAny(_))
    }
}

/// How a participant is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantConfig {
    /// Discovery: domain, identity, peers, cadence.
    pub spdp: SpdpConfig,
    /// Where the sockets are bound.
    pub bind: BindPolicy,
    /// How often a reliable writer heartbeats.
    pub heartbeat_period: StdDuration,
    /// How often the background loop runs the cadence.
    pub tick_period: StdDuration,
    /// Octets one datagram may occupy.
    ///
    /// Defaults to [`DEFAULT_DATAGRAM_BUDGET`](crate::messages::DEFAULT_DATAGRAM_BUDGET),
    /// which fits an Ethernet MTU. Raising it towards
    /// [`MAX_UDP_PAYLOAD`](crate::messages::MAX_UDP_PAYLOAD) is only safe
    /// where the operating system's socket send buffer is large enough —
    /// macOS defaults to a little over nine kilobytes and returns `EMSGSIZE`
    /// above it, even on loopback.
    pub datagram_budget: usize,
    /// Octets per fragment when fragmenting.
    pub fragment_size: u16,
    /// Samples above this size are fragmented.
    ///
    /// Clamped by the datagram budget: see
    /// [`WriterConfig::effective_threshold`](crate::behavior::writer::WriterConfig::effective_threshold).
    pub fragmentation_threshold: usize,
    /// Samples one subscription queues before dropping the oldest.
    pub sink_capacity: usize,
    /// The default security settings for every **user** endpoint this
    /// participant creates.
    ///
    /// Builtin discovery endpoints are never protected by it: SPDP has to be
    /// readable by a peer that has not been configured yet, and protecting
    /// SEDP with a per-topic pre-shared key would require the key before the
    /// topic is known. Whole-message protection is what DDS-Security uses for
    /// metatraffic and it is out of scope here — see [`crate::security`],
    /// which says so at the top.
    ///
    /// [`EndpointSecurity::none`] by default, and
    /// [`create_secure_writer`](Participant::create_secure_writer) and
    /// [`create_secure_reader`](Participant::create_secure_reader) override
    /// it per endpoint.
    pub security: EndpointSecurity,
}

impl ParticipantConfig {
    /// A participant on `domain_id` with the given identity.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::DomainIdOutOfRange`] or
    /// [`BehaviorError::ParticipantIdOutOfRange`].
    pub fn new(
        domain_id: u32,
        participant_id: u32,
        guid_prefix: GuidPrefix,
    ) -> BehaviorResult<Self> {
        Ok(Self::from_spdp(SpdpConfig::new(
            domain_id,
            participant_id,
            guid_prefix,
        )?))
    }

    /// A participant from a prepared discovery configuration.
    #[must_use]
    pub fn from_spdp(spdp: SpdpConfig) -> Self {
        Self {
            spdp,
            bind: BindPolicy::default(),
            heartbeat_period: crate::behavior::writer::DEFAULT_HEARTBEAT_PERIOD,
            tick_period: DEFAULT_TICK_PERIOD,
            datagram_budget: crate::messages::DEFAULT_DATAGRAM_BUDGET,
            fragment_size: crate::behavior::fragment::DEFAULT_FRAGMENT_SIZE,
            fragmentation_threshold: crate::behavior::fragment::FRAGMENTATION_THRESHOLD,
            sink_capacity: crate::behavior::handle::DEFAULT_SINK_CAPACITY,
            security: EndpointSecurity::none(),
        }
    }

    /// Replace the default security settings for user endpoints.
    #[must_use]
    pub fn with_security(mut self, security: EndpointSecurity) -> Self {
        self.security = security;
        self
    }

    /// Replace the bind policy.
    #[must_use]
    pub const fn with_bind(mut self, bind: BindPolicy) -> Self {
        self.bind = bind;
        self
    }

    /// Replace the heartbeat cadence.
    #[must_use]
    pub const fn with_heartbeat_period(mut self, period: StdDuration) -> Self {
        self.heartbeat_period = period;
        self
    }

    /// Replace the background loop's cadence.
    #[must_use]
    pub const fn with_tick_period(mut self, period: StdDuration) -> Self {
        self.tick_period = period;
        self
    }

    /// Replace the fragmentation settings.
    #[must_use]
    pub const fn with_fragmentation(mut self, threshold: usize, fragment_size: u16) -> Self {
        self.fragmentation_threshold = threshold;
        self.fragment_size = fragment_size;
        self
    }

    /// Replace the per-datagram budget.
    #[must_use]
    pub const fn with_datagram_budget(mut self, budget: usize) -> Self {
        self.datagram_budget = budget;
        self
    }
}

/// Which socket a datagram goes out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    /// Discovery traffic.
    Metatraffic,
    /// Application traffic.
    UserData,
}

/// Everything one `handle_message` call decided.
#[derive(Debug, Default)]
pub(crate) struct Dispatch {
    pub(crate) outbound: Vec<(Channel, Outbound)>,
    pub(crate) samples: Vec<(EntityId, Vec<Sample>)>,
    pub(crate) events: Vec<DiscoveryEvent>,
}

/// A pure-Rust RTPS participant on tokio UDP.
///
/// Cloneable: every clone is the same participant. Dropping the last clone
/// leaves the sockets to be closed by the runtime; call
/// [`shutdown`](Participant::shutdown) to announce departure first.
#[derive(Debug, Clone)]
pub struct Participant {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    guid: Guid,
    domain_id: u32,
    metatraffic: RtpsSocket,
    user_data: RtpsSocket,
    multicast: Option<RtpsSocket>,
    multicast_capability: MulticastCapability,
    state: Mutex<State>,
    events: broadcast::Sender<DiscoveryEvent>,
    stop: Notify,
    sinks: Mutex<BTreeMap<EntityId, Arc<SampleSink>>>,
    sink_capacity: usize,
    /// Whether any endpoint is protected, so `send_all` can skip the second
    /// lock acquisition on every datagram of an unsecured participant.
    ///
    /// A cache of `State::security.is_active()`, refreshed under the lock
    /// whenever an endpoint is created or deleted. It is only ever read to
    /// decide whether to *take* the lock and ask the authority, so a stale
    /// `true` costs a lock and a stale `false` cannot happen: it is set
    /// before the endpoint that needs it can produce anything.
    security_active: AtomicBool,
}

impl Participant {
    /// Bind the sockets and build the participant.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Bind`] when a socket will not bind, and
    /// [`BehaviorError::ParticipantIdOutOfRange`] under
    /// [`BindPolicy::Standard`] with an unusable participant id.
    pub async fn new(config: ParticipantConfig) -> BehaviorResult<Self> {
        let domain_id = config.spdp.domain_id;
        let participant_id = config.spdp.participant_id;

        let (metatraffic, user_data) = match config.bind {
            BindPolicy::EphemeralLoopback => (
                UdpTransport::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?,
                UdpTransport::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?,
            ),
            BindPolicy::EphemeralAny(_) => (
                UdpTransport::bind_any(0).await?,
                UdpTransport::bind_any(0).await?,
            ),
            BindPolicy::Standard(_) => {
                let metatraffic_port = port::metatraffic_unicast(domain_id, participant_id).ok_or(
                    BehaviorError::ParticipantIdOutOfRange {
                        participant_id,
                        domain_id,
                    },
                )?;
                let user_port = port::user_unicast(domain_id, participant_id).ok_or(
                    BehaviorError::ParticipantIdOutOfRange {
                        participant_id,
                        domain_id,
                    },
                )?;
                (
                    UdpTransport::bind_any(metatraffic_port).await?,
                    UdpTransport::bind_any(user_port).await?,
                )
            }
        };

        let multicast = if config.spdp.multicast_enabled {
            match Self::bind_multicast(domain_id).await {
                Ok(socket) => Some(socket),
                Err(error) => {
                    tracing::debug!(%error, "multicast reception unavailable");
                    None
                }
            }
        } else {
            None
        };

        Self::with_transport(
            config,
            Arc::new(metatraffic),
            Arc::new(user_data),
            multicast.map(|socket| (socket.inner().clone(), socket.multicast().clone())),
        )
        .await
    }

    /// Build a participant on sockets the caller supplies.
    ///
    /// The seam the reliability tests need: a
    /// [`DatagramSocket`](crate::behavior::transport::DatagramSocket) that
    /// drops datagrams is the honest way to test retransmission, and it is
    /// the only way that exercises the real state machine rather than a
    /// simulation of it. Everything above this call is identical to
    /// [`new`](Self::new).
    ///
    /// The locators announced are read back from each socket's
    /// `local_addr()`, so a wrapper must report the address of the socket it
    /// wraps.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub async fn with_transport(
        config: ParticipantConfig,
        metatraffic: Arc<dyn crate::behavior::transport::DatagramSocket>,
        user_data: Arc<dyn crate::behavior::transport::DatagramSocket>,
        multicast: Option<(
            Arc<dyn crate::behavior::transport::DatagramSocket>,
            MulticastCapability,
        )>,
    ) -> BehaviorResult<Self> {
        let domain_id = config.spdp.domain_id;
        let address = config.bind.announced_address();

        // Read the ports back: under every ephemeral policy the kernel chose
        // them, and announcing the computed ones instead is the silent
        // failure this whole design exists to prevent.
        let metatraffic = RtpsSocket::new(metatraffic)?;
        let user_data = RtpsSocket::new(user_data)?;
        let metatraffic_locator = metatraffic.locator_via(address);
        let user_locator = user_data.locator_via(address);

        let (multicast, multicast_capability) = match multicast {
            Some((socket, capability)) => (Some(RtpsSocket::new(socket)?), capability),
            None if config.spdp.multicast_enabled => (
                None,
                MulticastCapability::Refused {
                    group: port::DEFAULT_MULTICAST_GROUP,
                    reason: crate::behavior::error::IoFailure {
                        kind: std::io::ErrorKind::PermissionDenied,
                        message: "the multicast reception socket could not be bound".to_owned(),
                    },
                },
            ),
            None => (None, MulticastCapability::Disabled),
        };

        let spdp = Spdp::new(config.spdp.clone(), metatraffic_locator, user_locator)?;
        let guid = spdp.guid();

        let mut state = State {
            spdp,
            db: DiscoveryDb::new(),
            liveliness: LivelinessTracker::new(),
            writers: BTreeMap::new(),
            readers: BTreeMap::new(),
            entity_ids: EntityIdAllocator::new(),
            spdp_sample: None,
            user_locator,
            heartbeat_period: config.heartbeat_period,
            datagram_budget: config.datagram_budget,
            fragment_size: config.fragment_size,
            fragmentation_threshold: config.fragmentation_threshold,
            shutting_down: false,
            security: SecurityContext::new(config.datagram_budget),
            endpoint_security: config.security.clone(),
        };
        state.create_builtin_endpoints()?;
        state.refresh_spdp_sample(Instant::now())?;

        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            inner: Arc::new(Inner {
                guid,
                domain_id,
                metatraffic,
                user_data,
                multicast,
                multicast_capability,
                state: Mutex::new(state),
                events,
                stop: Notify::new(),
                sinks: Mutex::new(BTreeMap::new()),
                security_active: AtomicBool::new(false),
                sink_capacity: config.sink_capacity,
            }),
        })
    }

    /// Bind the multicast reception socket and join the SPDP group.
    async fn bind_multicast(domain_id: u32) -> BehaviorResult<RtpsSocket> {
        let group_port =
            port::metatraffic_multicast(domain_id).ok_or(BehaviorError::DomainIdOutOfRange {
                domain_id,
                maximum: port::MAX_DOMAIN_ID,
            })?;
        let transport = UdpTransport::bind_any(group_port).await?;
        // Loop sends back so that two participants in one process can see
        // each other's announcements.
        let _ = transport.set_multicast_loop(true);
        let socket = RtpsSocket::with_multicast(
            Arc::new(transport),
            port::DEFAULT_MULTICAST_GROUP,
            Ipv4Addr::UNSPECIFIED,
        )?;
        Ok(socket)
    }

    /// This participant's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.inner.guid
    }

    /// The domain it is on.
    #[must_use]
    pub fn domain_id(&self) -> u32 {
        self.inner.domain_id
    }

    /// The locator peers should send discovery traffic to.
    ///
    /// What another participant in the same process is given as an initial
    /// peer, and the reason the deterministic test path needs no multicast.
    #[must_use]
    pub fn metatraffic_locator(&self) -> Locator {
        self.inner.metatraffic.locator()
    }

    /// The locator peers should send user traffic to.
    #[must_use]
    pub fn user_locator(&self) -> Locator {
        self.inner.user_data.locator()
    }

    /// What the kernel said when the multicast join was attempted.
    ///
    /// Never a boolean and never hidden: a sandboxed host refuses the join,
    /// and a test asserts *that* rather than skipping.
    #[must_use]
    pub fn multicast(&self) -> &MulticastCapability {
        &self.inner.multicast_capability
    }

    /// Subscribe to discovery events.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.inner.events.subscribe()
    }

    /// How many remote participants are known.
    pub async fn known_participants(&self) -> usize {
        self.inner.state.lock().await.db.participant_count()
    }

    /// True when `participant` has been discovered.
    pub async fn knows(&self, participant: Guid) -> bool {
        self.inner.state.lock().await.db.knows(participant)
    }

    /// The GUIDs of every known remote participant.
    pub async fn participants(&self) -> Vec<Guid> {
        self.inner
            .state
            .lock()
            .await
            .db
            .participants()
            .map(|remote| remote.data.guid)
            .collect()
    }

    /// A snapshot of everything discovery knows: remote participants, remote
    /// writers, remote readers, with their announced names, types and QoS.
    ///
    /// The accessor an rcl-level layer needs and nothing above [`Guid`] can
    /// synthesize. `astrs-ros2` builds the ROS graph — `ros2 node list`,
    /// `ros2 topic list -t`, publisher and subscriber counts — from the SEDP
    /// samples in here, and a discovery *event* carries only a GUID, so
    /// there would otherwise be no way to answer "what topic is that?".
    ///
    /// A clone rather than a borrow, because the database lives under the
    /// participant's one lock and handing out a guard would let a caller
    /// hold it across an `await`. Cloning a few hundred discovery records is
    /// cheap next to that risk.
    pub async fn discovery_snapshot(&self) -> DiscoveryDb {
        self.inner.state.lock().await.db.clone()
    }

    /// How many remote writers a local reader has matched.
    pub async fn matched_writers(&self, reader: EntityId) -> usize {
        self.inner
            .state
            .lock()
            .await
            .readers
            .get(&reader)
            .map_or(0, RtpsReader::matched_writer_count)
    }

    /// The liveliness lease this participant is tracking for `participant`.
    ///
    /// `None` means no assertion of that kind has arrived, or the lease has
    /// already run out.
    pub async fn liveliness_state(
        &self,
        participant: Guid,
        kind: ParticipantMessageKind,
    ) -> Option<crate::behavior::liveliness::LivelinessState> {
        self.inner
            .state
            .lock()
            .await
            .liveliness
            .state(participant.participant_guid(), kind)
    }

    /// How many times the application has manually asserted liveliness.
    ///
    /// The value this participant announces in
    /// `PID_PARTICIPANT_MANUAL_LIVELINESS_COUNT`.
    pub async fn manual_liveliness_count(&self) -> i32 {
        self.inner
            .state
            .lock()
            .await
            .spdp
            .local()
            .manual_liveliness_count
    }

    /// How many remote readers a local writer has matched.
    pub async fn matched_readers(&self, writer: EntityId) -> usize {
        self.inner
            .state
            .lock()
            .await
            .writers
            .get(&writer)
            .map_or(0, RtpsWriter::matched_reader_count)
    }

    /// Create a publication.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::EntityKeysExhausted`], or
    /// [`BehaviorError::Wire`] when the SEDP announcement will not encode.
    pub async fn create_writer(
        &self,
        topic: TopicKey,
        qos: WriterQos,
    ) -> BehaviorResult<WriterHandle> {
        let security = self.inner.state.lock().await.endpoint_security.clone();
        self.create_secure_writer(topic, qos, security).await
    }

    /// Create a publication whose submessages are protected.
    ///
    /// The per-endpoint form of [`ParticipantConfig::security`]: the key and
    /// protection level given here override the participant's default for
    /// this writer alone. Passing [`EndpointSecurity::none`] is exactly
    /// [`create_writer`](Self::create_writer) on a participant with no
    /// default.
    ///
    /// The peer must configure the *same* pre-shared key on its reader for
    /// the same topic; there is no key exchange. See [`crate::security`].
    ///
    /// # Errors
    ///
    /// As [`create_writer`](Self::create_writer), plus
    /// [`BehaviorError::Security`] when the settings do not make sense — a
    /// protection level with no key — or key derivation fails.
    pub async fn create_secure_writer(
        &self,
        topic: TopicKey,
        qos: WriterQos,
        security: EndpointSecurity,
    ) -> BehaviorResult<WriterHandle> {
        let now = Instant::now();
        let (guid, outbound) = {
            let mut state = self.inner.state.lock().await;
            let entity_id = state.entity_ids.allocate_writer()?;
            let guid = Guid::new(self.inner.guid.prefix, entity_id);
            state.register_security(entity_id, &topic, &security)?;
            let config = state
                .writer_config(guid, topic.clone())
                .with_qos(qos)
                .with_security(security);
            state.writers.insert(entity_id, RtpsWriter::new(config));
            state.rematch_writer(entity_id);
            self.refresh_security(&state);
            let outbound = state.announce_endpoint(entity_id, true, now)?;
            (guid, outbound)
        };
        self.send_all(outbound).await?;
        Ok(WriterHandle {
            participant: self.clone(),
            guid,
            topic,
        })
    }

    /// Create a subscription.
    ///
    /// # Errors
    ///
    /// As [`create_writer`](Self::create_writer).
    pub async fn create_reader(
        &self,
        topic: TopicKey,
        qos: ReaderQos,
    ) -> BehaviorResult<ReaderHandle> {
        let security = self.inner.state.lock().await.endpoint_security.clone();
        self.create_secure_reader(topic, qos, security).await
    }

    /// Create a subscription that requires protection.
    ///
    /// The mirror of [`create_secure_writer`](Self::create_secure_writer),
    /// and the half that makes the protection binding rather than advisory: a
    /// plaintext `DATA` addressed to this reader is refused, so an attacker
    /// cannot downgrade by simply omitting the transform.
    ///
    /// # Errors
    ///
    /// As [`create_secure_writer`](Self::create_secure_writer).
    pub async fn create_secure_reader(
        &self,
        topic: TopicKey,
        qos: ReaderQos,
        security: EndpointSecurity,
    ) -> BehaviorResult<ReaderHandle> {
        let now = Instant::now();
        let sink = Arc::new(SampleSink::new(self.inner.sink_capacity));
        let (guid, outbound) = {
            let mut state = self.inner.state.lock().await;
            let entity_id = state.entity_ids.allocate_reader()?;
            let guid = Guid::new(self.inner.guid.prefix, entity_id);
            state.register_security(entity_id, &topic, &security)?;
            let config = ReaderConfig::new(guid, topic.clone())
                .with_qos(qos)
                .with_security(security);
            state.readers.insert(entity_id, RtpsReader::new(config));
            state.rematch_reader(entity_id);
            self.refresh_security(&state);
            let outbound = state.announce_endpoint(entity_id, false, now)?;
            (guid, outbound)
        };
        self.inner
            .sinks
            .lock()
            .await
            .insert(guid.entity_id, sink.clone());
        self.send_all(outbound).await?;
        Ok(ReaderHandle::new(guid, topic, sink))
    }

    /// Announce this participant now, whatever the cadence says.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Send`] or [`BehaviorError::Wire`].
    pub async fn announce(&self) -> BehaviorResult<usize> {
        let now = Instant::now();
        let outbound = {
            let mut state = self.inner.state.lock().await;
            state.spdp_announcement(now)?
        };
        let count = outbound.len();
        self.send_all(outbound).await?;
        Ok(count)
    }

    /// Run one round of the periodic work: announce if due, heartbeat,
    /// expire leases, drain readers' acknowledgements.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Send`] or [`BehaviorError::Wire`].
    pub async fn tick(&self) -> BehaviorResult<()> {
        let now = Instant::now();
        let (outbound, events) = {
            let mut state = self.inner.state.lock().await;
            let events = state.expire(now);
            let outbound = state.cadence(now)?;
            (outbound, events)
        };
        self.publish_events(&events);
        self.send_all(outbound).await
    }

    /// Assert this participant's liveliness on the WLP topic.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Send`] or [`BehaviorError::Wire`].
    pub async fn assert_liveliness(&self, kind: ParticipantMessageKind) -> BehaviorResult<()> {
        let now = Instant::now();
        let outbound = {
            let mut state = self.inner.state.lock().await;
            state.assert_liveliness(kind, now)?
        };
        self.send_all(outbound).await
    }

    /// Wait for one datagram on any socket and act on it.
    ///
    /// Returns `false` when the wait timed out. A datagram that will not
    /// decode, or that a peer got wrong, is logged and dropped — it returns
    /// `true`, because something did arrive.
    ///
    /// # Errors
    ///
    /// Only transport failures; peer faults are dropped, not raised.
    pub async fn poll_once(&self, timeout: StdDuration) -> BehaviorResult<bool> {
        let mut buffers = ReceiveBuffers::new();
        self.poll_into(&mut buffers, timeout).await
    }

    /// Wait for one datagram, reusing the caller's buffers.
    ///
    /// The receive loop's inner call. Three `RECEIVE_BUFFER_LEN` buffers is
    /// nearly two hundred kilobytes; allocating them per datagram turns a
    /// megabyte sample — eight hundred fragments — into a hundred and fifty
    /// megabytes of allocation, which is what this exists to avoid.
    ///
    /// # Errors
    ///
    /// Only transport failures; peer faults are dropped, not raised.
    pub async fn poll_into(
        &self,
        buffers: &mut ReceiveBuffers,
        timeout: StdDuration,
    ) -> BehaviorResult<bool> {
        let ReceiveBuffers {
            metatraffic,
            user_data,
            multicast,
        } = buffers;

        let received = tokio::time::timeout(timeout, async {
            match &self.inner.multicast {
                Some(group) => tokio::select! {
                    result = self.inner.metatraffic.recv(metatraffic) =>
                        result.map(|(len, from)| (Received::Metatraffic(len), from)),
                    result = self.inner.user_data.recv(user_data) =>
                        result.map(|(len, from)| (Received::UserData(len), from)),
                    result = group.recv(multicast) =>
                        result.map(|(len, from)| (Received::Multicast(len), from)),
                },
                None => tokio::select! {
                    result = self.inner.metatraffic.recv(metatraffic) =>
                        result.map(|(len, from)| (Received::Metatraffic(len), from)),
                    result = self.inner.user_data.recv(user_data) =>
                        result.map(|(len, from)| (Received::UserData(len), from)),
                },
            }
        })
        .await;

        let Ok(result) = received else {
            return Ok(false);
        };
        let (which, from) = result?;
        let datagram = match which {
            Received::Metatraffic(len) => metatraffic.get(..len).unwrap_or(&[]),
            Received::UserData(len) => user_data.get(..len).unwrap_or(&[]),
            Received::Multicast(len) => multicast.get(..len).unwrap_or(&[]),
        };
        self.handle_datagram(datagram, from).await?;
        Ok(true)
    }

    /// Decode and act on one datagram.
    ///
    /// # Errors
    ///
    /// Transport failures from the sends it provokes.
    pub async fn handle_datagram(&self, datagram: &[u8], from: SocketAddr) -> BehaviorResult<()> {
        let now = Instant::now();
        let dispatch = {
            let mut state = self.inner.state.lock().await;
            match state.handle_message(datagram, now) {
                Ok(dispatch) => dispatch,
                Err(error) if error.is_peer_fault() => {
                    tracing::debug!(%from, %error, "dropping a datagram");
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        };
        self.publish_events(&dispatch.events);
        self.deliver(dispatch.samples).await;
        self.send_all(dispatch.outbound).await
    }

    /// Run the participant until [`shutdown`](Self::shutdown) is called.
    ///
    /// # Errors
    ///
    /// The first transport failure; peer faults never stop the loop.
    pub async fn run(&self, tick_period: StdDuration) -> BehaviorResult<()> {
        let mut buffers = ReceiveBuffers::new();
        let mut ticker = tokio::time::interval(tick_period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The flag is checked as well as awaited. `Notify::notify_waiters`
            // wakes only the tasks parked at that instant, and this loop
            // spends most of its life parked in `poll_once` instead — so a
            // shutdown that lands between iterations would otherwise be lost,
            // and the loop would run until the next one. The flag closes that
            // window; the notify makes it prompt rather than one tick late.
            if self.is_shut_down().await {
                return Ok(());
            }
            tokio::select! {
                () = self.inner.stop.notified() => return Ok(()),
                _ = ticker.tick() => self.tick().await?,
                result = self.poll_into(&mut buffers, tick_period) => {
                    result?;
                }
            }
        }
    }

    /// Run the participant on a background task.
    ///
    /// The task ends when [`shutdown`](Self::shutdown) is called or the
    /// handle is aborted.
    #[must_use]
    pub fn spawn(&self, tick_period: StdDuration) -> tokio::task::JoinHandle<BehaviorResult<()>> {
        let participant = self.clone();
        tokio::spawn(async move { participant.run(tick_period).await })
    }

    /// Announce departure, stop the background loop, and mark the
    /// participant as shut down.
    ///
    /// The announcement is a disposal on the SPDP topic (§8.5.3.1). Without
    /// it a peer would only notice when the lease ran out — a hundred seconds
    /// by default — and would spend that time sending into a void. Any
    /// failure to send it is swallowed: a participant that is going away
    /// cannot usefully report that it failed to say so.
    ///
    /// Further writes report [`BehaviorError::Shutdown`].
    pub async fn shutdown(&self) {
        let now = Instant::now();
        let outbound = {
            let mut state = self.inner.state.lock().await;
            if state.shutting_down {
                Vec::new()
            } else {
                state.shutting_down = true;
                state.dispose_participant(now).unwrap_or_default()
            }
        };
        if let Err(error) = self.send_all(outbound).await {
            tracing::debug!(%error, "could not announce departure");
        }
        self.inner.stop.notify_waiters();
    }

    /// Delete a publication, announcing its disposal over SEDP.
    ///
    /// Returns whether the writer was there. Peers unwire it immediately
    /// rather than waiting for the participant's lease.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Send`] or [`BehaviorError::Wire`].
    pub async fn delete_writer(&self, writer: Guid) -> BehaviorResult<bool> {
        let now = Instant::now();
        let (existed, outbound) = {
            let mut state = self.inner.state.lock().await;
            let disposed = state.dispose_endpoint(writer.entity_id, true, now)?;
            self.refresh_security(&state);
            disposed
        };
        self.send_all(outbound).await?;
        Ok(existed)
    }

    /// Delete a subscription, announcing its disposal over SEDP.
    ///
    /// Returns whether the reader was there. The samples still queued in its
    /// [`ReaderHandle`] stay takeable; only new ones stop.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Send`] or [`BehaviorError::Wire`].
    pub async fn delete_reader(&self, reader: Guid) -> BehaviorResult<bool> {
        let now = Instant::now();
        let (existed, outbound) = {
            let mut state = self.inner.state.lock().await;
            let disposed = state.dispose_endpoint(reader.entity_id, false, now)?;
            self.refresh_security(&state);
            disposed
        };
        self.inner.sinks.lock().await.remove(&reader.entity_id);
        self.send_all(outbound).await?;
        Ok(existed)
    }

    /// Every matched writer that has missed its reader's `DEADLINE`.
    ///
    /// A condition rather than an event: it stops being true the moment a
    /// sample arrives. The background loop logs each one every cadence; this
    /// is how an application — or `astrs-ros2`, raising
    /// `REQUESTED_DEADLINE_MISSED` — observes it.
    pub async fn deadline_misses(&self) -> Vec<crate::behavior::reader::DeadlineMiss> {
        let now = Instant::now();
        self.inner.state.lock().await.missed_deadlines(now)
    }

    /// True once [`shutdown`](Self::shutdown) has been called.
    pub async fn is_shut_down(&self) -> bool {
        self.inner.state.lock().await.shutting_down
    }

    /// Write a sample through one of this participant's writers.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::UnknownWriter`], [`BehaviorError::Shutdown`],
    /// [`BehaviorError::HistoryFull`], or a transport failure.
    async fn write(
        &self,
        writer: EntityId,
        payload: Vec<u8>,
        timestamp: Option<Time>,
    ) -> BehaviorResult<SequenceNumber> {
        let now = Instant::now();
        let (number, outbound) = {
            let mut state = self.inner.state.lock().await;
            if state.shutting_down {
                return Err(BehaviorError::Shutdown);
            }
            let guid = Guid::new(self.inner.guid.prefix, writer);
            let entry = state
                .writers
                .get_mut(&writer)
                .ok_or(BehaviorError::UnknownWriter { guid })?;
            let number = entry.write(payload, timestamp, now)?;
            let outbound = entry.produce(now)?;
            let channel = channel_for(writer);
            (
                number,
                outbound
                    .into_iter()
                    .map(|item| (channel, item))
                    .collect::<Vec<_>>(),
            )
        };
        self.send_all(outbound).await?;
        Ok(number)
    }

    /// Cache whether any endpoint is protected, so `send_all` can skip the
    /// lock when none is.
    ///
    /// Called with the lock already held, which is what makes the cache
    /// coherent: the flag is set before the endpoint that needs it can be
    /// asked for a datagram.
    fn refresh_security(&self, state: &State) {
        self.inner
            .security_active
            .store(state.security.is_active(), Ordering::Relaxed);
    }

    /// Publish discovery events, ignoring the absence of subscribers.
    fn publish_events(&self, events: &[DiscoveryEvent]) {
        for event in events {
            let _ = self.inner.events.send(*event);
        }
    }

    /// Push samples into the sinks of the readers that produced them.
    async fn deliver(&self, samples: Vec<(EntityId, Vec<Sample>)>) {
        if samples.is_empty() {
            return;
        }
        let sinks = self.inner.sinks.lock().await;
        for (reader, batch) in samples {
            if let Some(sink) = sinks.get(&reader) {
                sink.extend(batch).await;
            }
        }
    }

    /// Send everything, choosing a socket per channel.
    ///
    /// **The one funnel every outbound datagram passes through**, and
    /// therefore the one place submessage protection is applied. Protecting
    /// closer to the producers looked equivalent and was not: `write`
    /// produces and sends without going through the cadence or the dispatch
    /// path, so a policy applied there left every application sample in the
    /// clear while every test still passed. See `tests/security.rs`, where a
    /// capturing socket reads the octets rather than trusting the reasoning.
    async fn send_all(&self, outbound: Vec<(Channel, Outbound)>) -> BehaviorResult<()> {
        // An unsecured participant pays one relaxed atomic load per batch and
        // never takes the lock a second time.
        let outbound = if self.inner.security_active.load(Ordering::Relaxed) {
            self.inner.state.lock().await.protect(outbound)
        } else {
            outbound
        };
        for (channel, item) in outbound {
            let socket = match channel {
                Channel::Metatraffic => &self.inner.metatraffic,
                Channel::UserData => &self.inner.user_data,
            };
            let sent = socket
                .send_datagram_to_locators(&item.datagram, &item.locators)
                .await?;
            if sent == 0 && !item.locators.is_empty() {
                tracing::debug!(
                    locators = item.locators.len(),
                    "no addressable locator for an outbound datagram"
                );
            }
        }
        Ok(())
    }
}

/// A publication.
#[derive(Debug, Clone)]
pub struct WriterHandle {
    participant: Participant,
    guid: Guid,
    topic: TopicKey,
}

impl WriterHandle {
    /// The writer's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.guid
    }

    /// The topic and type it publishes.
    #[must_use]
    pub const fn topic(&self) -> &TopicKey {
        &self.topic
    }

    /// Write a sample and put it on the wire before returning.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Shutdown`], [`BehaviorError::HistoryFull`], or a
    /// transport failure.
    pub async fn write(&self, payload: impl Into<Vec<u8>>) -> BehaviorResult<SequenceNumber> {
        self.participant
            .write(self.guid.entity_id, payload.into(), None)
            .await
    }

    /// Write a sample with an explicit source timestamp.
    ///
    /// # Errors
    ///
    /// As [`write`](Self::write).
    pub async fn write_at(
        &self,
        payload: impl Into<Vec<u8>>,
        timestamp: Time,
    ) -> BehaviorResult<SequenceNumber> {
        self.participant
            .write(self.guid.entity_id, payload.into(), Some(timestamp))
            .await
    }

    /// How many remote readers this writer has matched.
    pub async fn matched_readers(&self) -> usize {
        self.participant.matched_readers(self.guid.entity_id).await
    }

    /// True when every matched reliable reader has acknowledged everything.
    pub async fn is_acknowledged(&self) -> bool {
        self.participant
            .inner
            .state
            .lock()
            .await
            .writers
            .get(&self.guid.entity_id)
            .is_none_or(RtpsWriter::is_acknowledged)
    }
}

/// The buffers one receive loop reuses.
///
/// One per socket, allocated once. See
/// [`Participant::poll_into`](Participant::poll_into).
#[derive(Debug)]
pub struct ReceiveBuffers {
    metatraffic: Vec<u8>,
    user_data: Vec<u8>,
    multicast: Vec<u8>,
}

impl ReceiveBuffers {
    /// Allocate a fresh set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metatraffic: vec![0_u8; RECEIVE_BUFFER_LEN],
            user_data: vec![0_u8; RECEIVE_BUFFER_LEN],
            multicast: vec![0_u8; RECEIVE_BUFFER_LEN],
        }
    }
}

impl Default for ReceiveBuffers {
    fn default() -> Self {
        Self::new()
    }
}

/// Which socket a datagram came in on, and how long it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Received {
    Metatraffic(usize),
    UserData(usize),
    Multicast(usize),
}

/// Which socket an endpoint's traffic belongs on.
pub(crate) const fn channel_for(entity_id: EntityId) -> Channel {
    if entity_id.is_builtin() {
        Channel::Metatraffic
    } else {
        Channel::UserData
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
        VendorId,
    };

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    fn config(seed: u8) -> ParticipantConfig {
        ParticipantConfig::from_spdp(
            SpdpConfig::new(0, 0, prefix(seed))
                .expect("domain 0")
                .with_multicast(false)
                .with_initial_peer(Locator::udpv4(Ipv4Addr::LOCALHOST, 7_410)),
        )
    }

    #[tokio::test]
    async fn a_participant_binds_ephemeral_ports_and_announces_them() {
        let participant = Participant::new(config(1)).await.expect("bind");
        let metatraffic = participant.metatraffic_locator();
        let user = participant.user_locator();
        assert_ne!(metatraffic.udp_port(), Some(0));
        assert_ne!(user.udp_port(), Some(0));
        assert_ne!(metatraffic.udp_port(), user.udp_port());
        assert_eq!(participant.domain_id(), 0);
        assert_eq!(participant.guid().prefix, prefix(1));
    }

    #[tokio::test]
    async fn multicast_off_is_recorded_as_disabled() {
        let participant = Participant::new(config(2)).await.expect("bind");
        assert_eq!(participant.multicast(), &MulticastCapability::Disabled);
    }

    #[tokio::test]
    async fn a_participant_starts_with_the_eight_builtin_endpoints() {
        let participant = Participant::new(config(3)).await.expect("bind");
        let state = participant.inner.state.lock().await;
        assert_eq!(state.writers.len(), 4);
        assert_eq!(state.readers.len(), 4);
        assert!(
            state
                .writers
                .contains_key(&ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER)
        );
        assert!(
            state
                .readers
                .contains_key(&ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER)
        );
        assert_eq!(state.spdp_sample, Some(SequenceNumber::FIRST));
    }

    #[tokio::test]
    async fn creating_endpoints_allocates_user_entity_ids() {
        let participant = Participant::new(config(4)).await.expect("bind");
        let topic = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").unwrap();
        let writer = participant
            .create_writer(topic.clone(), WriterQos::services_default())
            .await
            .expect("writer");
        let reader = participant
            .create_reader(topic, ReaderQos::reliable(10))
            .await
            .expect("reader");

        assert!(writer.guid().entity_id.is_writer());
        assert!(!writer.guid().entity_id.is_builtin());
        assert!(reader.guid().entity_id.is_reader());
        assert_eq!(writer.guid().prefix, participant.guid().prefix);
        assert_eq!(writer.matched_readers().await, 0);
        assert_eq!(reader.len().await, 0);
    }

    #[tokio::test]
    async fn a_participant_with_no_peers_is_passive_not_refused() {
        let quiet = ParticipantConfig::from_spdp(
            SpdpConfig::new(0, 0, prefix(5))
                .expect("domain 0")
                .with_multicast(false),
        );
        let participant = Participant::new(quiet).await.expect("passive is legal");
        assert_eq!(
            participant.announce().await.expect("announce"),
            0,
            "there is nobody to announce to until someone announces first"
        );
        assert!(participant.inner.state.lock().await.spdp.is_passive());
        participant.shutdown().await;
    }

    #[tokio::test]
    async fn announcing_with_no_peers_sends_nothing() {
        let alone = ParticipantConfig::from_spdp(
            SpdpConfig::new(0, 0, prefix(6))
                .expect("domain 0")
                .with_multicast(false)
                .with_initial_peer(Locator::INVALID),
        );
        let participant = Participant::new(alone).await.expect("bind");
        assert_eq!(participant.announce().await.expect("announce"), 0);
    }

    #[tokio::test]
    async fn announcing_to_a_peer_produces_one_datagram() {
        let participant = Participant::new(config(7)).await.expect("bind");
        assert_eq!(participant.announce().await.expect("announce"), 1);
    }

    #[tokio::test]
    async fn polling_with_nothing_to_receive_times_out() {
        let participant = Participant::new(config(8)).await.expect("bind");
        assert!(
            !participant
                .poll_once(StdDuration::from_millis(10))
                .await
                .expect("poll")
        );
    }

    #[tokio::test]
    async fn a_garbage_datagram_is_dropped_not_fatal() {
        let participant = Participant::new(config(9)).await.expect("bind");
        let from = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        participant
            .handle_datagram(b"not an RTPS message", from)
            .await
            .expect("a malformed datagram must not stop the participant");
        assert_eq!(participant.known_participants().await, 0);
    }

    #[tokio::test]
    async fn writing_after_shutdown_is_refused() {
        let participant = Participant::new(config(10)).await.expect("bind");
        let topic = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").unwrap();
        let writer = participant
            .create_writer(topic, WriterQos::services_default())
            .await
            .expect("writer");
        participant.shutdown().await;
        assert!(participant.is_shut_down().await);
        assert_eq!(
            writer.write(vec![1, 2, 3, 4]).await.expect_err("refused"),
            BehaviorError::Shutdown
        );
    }

    #[tokio::test]
    async fn a_tick_with_no_peers_is_harmless() {
        let participant = Participant::new(config(11)).await.expect("bind");
        participant.tick().await.expect("tick");
        participant.tick().await.expect("tick");
        assert_eq!(participant.known_participants().await, 0);
    }

    #[tokio::test]
    async fn asserting_liveliness_bumps_the_manual_count() {
        let participant = Participant::new(config(12)).await.expect("bind");
        participant
            .assert_liveliness(ParticipantMessageKind::Manual)
            .await
            .expect("assert");
        let state = participant.inner.state.lock().await;
        assert_eq!(state.spdp.local().manual_liveliness_count, 1);
    }

    #[tokio::test]
    async fn the_run_loop_stops_when_shut_down() {
        let participant = Participant::new(config(13)).await.expect("bind");
        let task = participant.spawn(StdDuration::from_millis(5));
        participant.shutdown().await;
        let outcome = tokio::time::timeout(StdDuration::from_secs(5), task)
            .await
            .expect("the loop must stop promptly")
            .expect("the task must not panic");
        outcome.expect("the loop must not fail");
    }

    #[tokio::test]
    async fn a_clone_is_the_same_participant() {
        let participant = Participant::new(config(14)).await.expect("bind");
        let clone = participant.clone();
        assert_eq!(clone.guid(), participant.guid());
        assert_eq!(
            clone.metatraffic_locator(),
            participant.metatraffic_locator()
        );
        clone.shutdown().await;
        assert!(participant.is_shut_down().await);
    }

    #[test]
    fn the_bind_policy_decides_the_announced_address() {
        assert_eq!(
            BindPolicy::EphemeralLoopback.announced_address(),
            Ipv4Addr::LOCALHOST
        );
        assert!(BindPolicy::EphemeralLoopback.is_ephemeral());
        assert!(BindPolicy::EphemeralAny(Ipv4Addr::new(10, 0, 0, 1)).is_ephemeral());
        assert!(!BindPolicy::Standard(Ipv4Addr::LOCALHOST).is_ephemeral());
        assert_eq!(
            BindPolicy::Standard(Ipv4Addr::new(192, 168, 1, 5)).announced_address(),
            Ipv4Addr::new(192, 168, 1, 5)
        );
    }

    #[test]
    fn builtin_endpoints_use_the_metatraffic_socket() {
        assert_eq!(
            channel_for(ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER),
            Channel::Metatraffic
        );
        assert_eq!(
            channel_for(EntityId::user_defined(
                1,
                crate::structure::EntityKind::USER_WRITER_NO_KEY
            )),
            Channel::UserData
        );
    }
}
