//! Discovery between two participants' protocol states, with the link and the
//! clock in the test's hands.
//!
//! Split out of `state.rs` the way `writer/tests.rs` is: `use super::*` still
//! resolves to the state module, so these reach its private items.
//!
//! No socket and no runtime: two [`State`]s hand each other the datagrams
//! they produce, and each direction of the link can be cut on its own. The
//! subject is a lease that runs out on **one side only** — one participant's
//! announcements stop arriving while the other's still get through. Over real
//! UDP that takes waiting a lease out on a loaded host; here it is a flag and
//! a loop over the clock, so every test is exact, and fast.
//!
//! Both halves of that event are here. The side that gave up on its peer and
//! found it again has forgotten what the peer's SEDP writers delivered, and
//! the peer, which never gave up, still thinks it has it. And the peer, which
//! was given up on, still holds every endpoint the other side announced,
//! including any deleted while it was given up on — whose retirement nobody
//! was matched to be owed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use crate::behavior::cache::InstanceHandle;
use crate::discovery::spdp::SpdpConfig;
use crate::messages::{Data, DataPayload, Header, inline_qos_encoding};
use crate::structure::{Duration, VendorId};
use astrs_cdr::{Encoding, Endianness, ParameterId, ParameterList, pid};

/// The period both participants announce, heartbeat and tick at.
const PERIOD: StdDuration = StdDuration::from_millis(20);

/// A lease no test here outlives.
const LONG_LEASE: Duration = Duration::from_secs(100);

/// A lease a test can run out: fifty periods.
const SHORT_LEASE: Duration = Duration::from_secs(1);

/// How many of its own leases a participant owes a peer it gave up on the
/// retirements of what it deleted meanwhile.
///
/// `LAPSE_RETENTION_LEASES`, restated rather than imported so that these
/// tests build against a `State` that predates it — which is how they were
/// shown to fail without the fix. The retention test below pins the value.
const RETENTION_LEASES: u32 = 2;

/// Which of the two participants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// A distinct, vendor-scoped GUID prefix.
fn prefix(side: Side) -> GuidPrefix {
    let seed = match side {
        Side::Left => 71,
        Side::Right => 72,
    };
    GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
}

/// A participant's metatraffic locator. Nothing listens there: the link
/// carries every datagram to the other side whatever it is addressed to.
fn metatraffic(side: Side) -> Locator {
    match side {
        Side::Left => Locator::udpv4(Ipv4Addr::LOCALHOST, 21_710),
        Side::Right => Locator::udpv4(Ipv4Addr::LOCALHOST, 21_720),
    }
}

/// A participant's user-traffic locator.
fn user_traffic(side: Side) -> Locator {
    match side {
        Side::Left => Locator::udpv4(Ipv4Addr::LOCALHOST, 21_711),
        Side::Right => Locator::udpv4(Ipv4Addr::LOCALHOST, 21_721),
    }
}

/// The protocol state of the participant on `side`, asking its peer for
/// `lease`, with the other side as its initial peer — built the way
/// `Participant::with_transport` builds it.
fn participant(side: Side, lease: Duration, now: Instant) -> State {
    let peer = match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    };
    let config = SpdpConfig::new(0, 0, prefix(side))
        .expect("domain 0 is usable")
        .with_multicast(false)
        .with_initial_peer(metatraffic(peer))
        .with_announce_period(PERIOD)
        .with_lease(lease);
    let spdp =
        Spdp::new(config, metatraffic(side), user_traffic(side)).expect("the announcement encodes");
    let mut state = State {
        spdp,
        db: DiscoveryDb::new(),
        liveliness: LivelinessTracker::new(),
        writers: BTreeMap::new(),
        readers: BTreeMap::new(),
        entity_ids: EntityIdAllocator::new(),
        spdp_sample: None,
        user_locator: user_traffic(side),
        heartbeat_period: PERIOD,
        datagram_budget: crate::messages::DEFAULT_DATAGRAM_BUDGET,
        fragment_size: crate::behavior::fragment::DEFAULT_FRAGMENT_SIZE,
        fragmentation_threshold: crate::behavior::fragment::FRAGMENTATION_THRESHOLD,
        shutting_down: false,
        security: SecurityContext::new(crate::messages::DEFAULT_DATAGRAM_BUDGET),
        endpoint_security: EndpointSecurity::none(),
    };
    state
        .create_builtin_endpoints()
        .expect("the builtin endpoints are created");
    state
        .refresh_spdp_sample(now)
        .expect("the SPDP sample is written");
    state
}

/// The octets of every datagram in a batch; the link ignores the addresses.
fn datagrams(outbound: Vec<(Channel, Outbound)>) -> Vec<Vec<u8>> {
    outbound
        .into_iter()
        .map(|(_, item)| item.datagram)
        .collect()
}

/// The remote writers and readers a participant has discovered.
fn known(state: &State) -> (BTreeSet<Guid>, BTreeSet<Guid>) {
    (
        state.db.writers().map(DiscoveredWriterData::guid).collect(),
        state.db.readers().map(DiscoveredReaderData::guid).collect(),
    )
}

/// What one of a participant's own writers holds: the kind and instance of
/// every change, oldest first.
fn history(state: &State, writer: EntityId) -> Vec<(ChangeKind, InstanceHandle)> {
    state
        .writers
        .get(&writer)
        .map(|writer| {
            writer
                .cache()
                .iter()
                .map(|change| (change.kind, change.instance))
                .collect()
        })
        .unwrap_or_default()
}

/// The history a SEDP writer holds when exactly `endpoints` exist.
fn announcements_of(endpoints: &[Guid]) -> Vec<(ChangeKind, InstanceHandle)> {
    endpoints
        .iter()
        .map(|guid| (ChangeKind::Alive, sedp::guid_instance(*guid)))
        .collect()
}

/// A set of GUIDs, for comparing against [`known`].
fn set(guids: &[Guid]) -> BTreeSet<Guid> {
    guids.iter().copied().collect()
}

/// Two participants on a link the test controls.
struct Link {
    left: State,
    right: State,
    now: Instant,
    /// Whether what the left sends reaches the right.
    left_to_right: bool,
    /// Whether what the right sends reaches the left.
    right_to_left: bool,
}

impl Link {
    /// Two participants asking each other for the given leases, the link
    /// open both ways.
    fn new(left_lease: Duration, right_lease: Duration) -> Self {
        let now = Instant::now();
        Self {
            left: participant(Side::Left, left_lease, now),
            right: participant(Side::Right, right_lease, now),
            now,
            left_to_right: true,
            right_to_left: true,
        }
    }

    fn state_mut(&mut self, side: Side) -> &mut State {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    fn guid(&self, side: Side) -> Guid {
        match side {
            Side::Left => self.left.spdp.guid(),
            Side::Right => self.right.spdp.guid(),
        }
    }

    /// Carry what `from` sent, then every answer it provokes, until neither
    /// side has anything left to say. A cut direction loses what it carries.
    fn carry(&mut self, from: Side, sent: Vec<Vec<u8>>) {
        let (mut to_right, mut to_left) = match from {
            Side::Left => (sent, Vec::new()),
            Side::Right => (Vec::new(), sent),
        };
        self.exchange(&mut to_right, &mut to_left);
    }

    fn exchange(&mut self, to_right: &mut Vec<Vec<u8>>, to_left: &mut Vec<Vec<u8>>) {
        let now = self.now;
        for _ in 0..1_000 {
            if to_right.is_empty() && to_left.is_empty() {
                return;
            }
            let mut answers_to_left = Vec::new();
            let mut answers_to_right = Vec::new();
            for datagram in to_right.drain(..) {
                if self.left_to_right {
                    let dispatch = self
                        .right
                        .handle_message(&datagram, now)
                        .expect("what the left sends is valid");
                    answers_to_left.extend(datagrams(dispatch.outbound));
                }
            }
            for datagram in to_left.drain(..) {
                if self.right_to_left {
                    let dispatch = self
                        .left
                        .handle_message(&datagram, now)
                        .expect("what the right sends is valid");
                    answers_to_right.extend(datagrams(dispatch.outbound));
                }
            }
            *to_right = answers_to_right;
            *to_left = answers_to_left;
        }
        panic!("the exchange never went quiet");
    }

    /// One period: each participant expires the leases that ran out and
    /// runs its cadence, and the link carries what that starts.
    fn tick(&mut self) {
        self.now += PERIOD;
        let now = self.now;
        self.left.expire(now);
        self.right.expire(now);
        let mut to_right = datagrams(self.left.cadence(now).expect("the cadence runs"));
        let mut to_left = datagrams(self.right.cadence(now).expect("the cadence runs"));
        self.exchange(&mut to_right, &mut to_left);
    }

    fn run(&mut self, periods: u32) {
        for _ in 0..periods {
            self.tick();
        }
    }

    /// Tick until `done` holds; returns how many periods that took. Fails
    /// after `limit`, saying what each side knew.
    fn tick_until(&mut self, what: &str, limit: u32, done: impl Fn(&Self) -> bool) -> u32 {
        for periods in 0..=limit {
            if done(self) {
                return periods;
            }
            self.tick();
        }
        panic!(
            "{what} did not happen within {limit} periods: the left knew {:?} and the right \
             knew {:?}",
            known(&self.left),
            known(&self.right)
        );
    }

    /// Run until the two participants know each other.
    fn meet(&mut self) {
        let left = self.guid(Side::Left);
        let right = self.guid(Side::Right);
        self.tick_until("mutual discovery", 20, |link| {
            link.left.db.knows(right) && link.right.db.knows(left)
        });
    }

    /// Create an endpoint on `side`, on a topic of its own so that nothing
    /// is ever matched to it, and carry its announcement.
    fn create(&mut self, side: Side, is_writer: bool, name: &str) -> Guid {
        let now = self.now;
        let state = self.state_mut(side);
        let topic =
            TopicKey::new(name, "std_msgs::msg::dds_::String_").expect("the names are valid");
        let entity_id = if is_writer {
            state.entity_ids.allocate_writer()
        } else {
            state.entity_ids.allocate_reader()
        }
        .expect("entity keys remain");
        let guid = Guid::new(state.spdp.guid().prefix, entity_id);
        if is_writer {
            let config = state
                .writer_config(guid, topic)
                .with_qos(WriterQos::services_default());
            state.writers.insert(entity_id, RtpsWriter::new(config));
            state.rematch_writer(entity_id);
        } else {
            let config = ReaderConfig::new(guid, topic).with_qos(ReaderQos::reliable(10));
            state.readers.insert(entity_id, RtpsReader::new(config));
            state.rematch_reader(entity_id);
        }
        let sent = state
            .announce_endpoint(entity_id, is_writer, now)
            .expect("the announcement encodes");
        self.carry(side, datagrams(sent));
        guid
    }

    /// Delete an endpoint of `side`'s, and carry its disposal.
    fn delete(&mut self, side: Side, endpoint: Guid) {
        let now = self.now;
        let (existed, sent) = self
            .state_mut(side)
            .dispose_endpoint(endpoint.entity_id, endpoint.entity_id.is_writer(), now)
            .expect("the disposal encodes");
        assert!(existed, "{endpoint} was there to delete");
        self.carry(side, datagrams(sent));
    }

    /// Cut what the right sends, and run until the left's lease on the right
    /// has run out — the right never losing the left, whose announcements
    /// still reach it.
    fn left_gives_up_on_right(&mut self) {
        let left = self.guid(Side::Left);
        let right = self.guid(Side::Right);
        self.right_to_left = false;
        self.tick_until("the left's lease on the right running out", 200, |link| {
            !link.left.db.knows(right)
        });
        assert!(
            self.right.db.knows(left),
            "one-sided: the right never lost the left"
        );
    }
}

// ---------------------------------------------------------------------------
// The side that gave up on its peer, and found it again
// ---------------------------------------------------------------------------

#[test]
fn a_peer_that_gave_up_on_us_and_found_us_again_relearns_every_endpoint() {
    // The right gives up on the left; the left never gives up on the right.
    // So the left's SEDP writers keep their proxies for the right's readers,
    // which say the right has every announcement — and the right, having
    // forgotten the left, wires the left's writers up again from scratch and
    // asks for all of it. Ignored, it would know the left and none of its
    // endpoints until the left restarted.
    let mut link = Link::new(SHORT_LEASE, LONG_LEASE);
    link.meet();
    // One period apart, so each SEDP reader of the right answers a heartbeat
    // per endpoint and its ACKNACK count climbs well past one.
    let mut writers = Vec::new();
    let mut readers = Vec::new();
    for index in 0..6 {
        writers.push(link.create(Side::Left, true, &format!("rt/left/publication_{index}")));
        readers.push(link.create(Side::Left, false, &format!("rt/left/subscription_{index}")));
        link.tick();
    }
    let everything = (set(&writers), set(&readers));
    link.tick_until(
        "the right learning every endpoint of the left",
        10,
        |link| known(&link.right) == everything,
    );

    let left = link.guid(Side::Left);
    let right = link.guid(Side::Right);
    link.left_to_right = false;
    link.tick_until("the right's lease on the left running out", 200, |link| {
        !link.right.db.knows(left)
    });
    assert!(
        link.left.db.knows(right),
        "one-sided: the left never lost the right"
    );
    assert_eq!(
        known(&link.right),
        (BTreeSet::new(), BTreeSet::new()),
        "the right forgot the left's endpoints with the left"
    );

    link.left_to_right = true;
    let periods = link.tick_until(
        "the right relearning every endpoint of the left",
        50,
        |link| link.right.db.knows(left) && known(&link.right) == everything,
    );
    assert!(
        periods <= 2,
        "the first exchange after the right found the left again must carry the replay, \
         not {periods} periods of it"
    );
    link.run(10);
    assert_eq!(
        known(&link.right),
        everything,
        "and the right goes on knowing them"
    );
}

// ---------------------------------------------------------------------------
// The side that was given up on
// ---------------------------------------------------------------------------

#[test]
fn a_peer_we_gave_up_on_learns_which_endpoints_went_while_it_was_away() {
    // The left gives up on the right; the right never gives up on the left,
    // whose announcements keep reaching it. The left deletes a publication
    // and a subscription while the right is unmatched there: the retirements
    // are owed to nobody matched, and without the right being remembered as
    // lapsed they are swept at once — and the right, which is rediscovered
    // the moment its announcements get through again, is replayed only the
    // survivors and keeps both deleted endpoints for as long as the left
    // lives.
    let mut link = Link::new(LONG_LEASE, SHORT_LEASE);
    link.meet();
    let doomed_writer = link.create(Side::Left, true, "rt/left/doomed_publication");
    let kept_writer = link.create(Side::Left, true, "rt/left/kept_publication");
    let doomed_reader = link.create(Side::Left, false, "rt/left/doomed_subscription");
    let kept_reader = link.create(Side::Left, false, "rt/left/kept_subscription");
    let everything = (
        set(&[doomed_writer, kept_writer]),
        set(&[doomed_reader, kept_reader]),
    );
    link.tick_until("the right learning all four endpoints", 10, |link| {
        known(&link.right) == everything
    });

    link.left_gives_up_on_right();
    link.delete(Side::Left, doomed_writer);
    link.delete(Side::Left, doomed_reader);
    link.run(10);
    assert_eq!(
        known(&link.right),
        everything,
        "while the left has the right unmatched, nothing tells the right about the deletions"
    );

    link.right_to_left = true;
    let survivors = (set(&[kept_writer]), set(&[kept_reader]));
    link.tick_until(
        "the right forgetting exactly the two deleted endpoints",
        10,
        |link| known(&link.right) == survivors,
    );
    link.run(10);
    assert_eq!(known(&link.right), survivors, "and it stays that way");
    assert_eq!(
        history(&link.left, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
        announcements_of(&[kept_writer]),
        "once the right has the retirements they leave the left's history"
    );
    assert_eq!(
        history(&link.left, ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER),
        announcements_of(&[kept_reader])
    );
}

#[test]
fn a_peer_that_never_comes_back_is_owed_nothing_once_the_retention_has_passed() {
    // What the lapse record holds, it holds for a bounded time: two of the
    // left's own leases. The outage `left_gives_up_on_right` sets up is
    // one-directional: the right's messages never reach the left, so the
    // left gives up on the right and lapses it, but the left's own messages
    // keep reaching the right the whole time, so the right never gives up
    // on the left (asserted in `left_gives_up_on_right` itself) — the right
    // can hear the left throughout this test, including after the retention
    // here runs out. What this pins is only that the *left's* bookkeeping
    // for a reader it can no longer reach is bounded, not that the right
    // has forgotten anything by then: a record held for ever would instead
    // grow the SEDP history for as long as endpoints are deleted, as a
    // history holding every retirement once did. A one-directional outage
    // that outlasts the retention, as here, is the accepted residual gap —
    // the right keeps whatever the left swept, for as long as the left
    // lives.
    let mut link = Link::new(SHORT_LEASE, SHORT_LEASE);
    link.meet();
    let doomed = link.create(Side::Left, true, "rt/left/doomed_publication");
    let kept = link.create(Side::Left, true, "rt/left/kept_publication");
    link.tick_until("the right learning both publications", 10, |link| {
        known(&link.right).0 == set(&[doomed, kept])
    });

    link.left_gives_up_on_right();
    let lapsed_at = link.now;
    let deadline = lapsed_at
        + SHORT_LEASE
            .to_std()
            .expect("finite")
            .saturating_mul(RETENTION_LEASES);
    link.delete(Side::Left, doomed);
    let held = vec![
        (ChangeKind::Alive, sedp::guid_instance(kept)),
        (ChangeKind::NotAliveDisposed, sedp::guid_instance(doomed)),
    ];
    while link.now + PERIOD < deadline {
        assert_eq!(
            history(&link.left, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
            held,
            "the retirement is held for the lapsed right, {:?} after it lapsed",
            link.now.saturating_duration_since(lapsed_at)
        );
        link.tick();
    }
    link.tick_until(
        "the retirement leaving once the retention passed",
        2,
        |link| {
            history(&link.left, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER)
                == announcements_of(&[kept])
        },
    );
    assert!(
        !link.left.db.knows(link.guid(Side::Right)),
        "the right never came back"
    );
}

#[test]
fn a_peer_that_announces_its_departure_is_owed_nothing_more() {
    // Its lease ran out here first, so there is nothing of it left to forget
    // — but it said it is leaving, and what was held for it goes at once.
    let mut link = Link::new(LONG_LEASE, SHORT_LEASE);
    link.meet();
    let doomed = link.create(Side::Left, true, "rt/left/doomed_publication");
    let kept = link.create(Side::Left, true, "rt/left/kept_publication");
    link.tick_until("the right learning both publications", 10, |link| {
        known(&link.right).0 == set(&[doomed, kept])
    });
    link.left_gives_up_on_right();
    link.delete(Side::Left, doomed);
    assert_eq!(
        history(&link.left, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
        vec![
            (ChangeKind::Alive, sedp::guid_instance(kept)),
            (ChangeKind::NotAliveDisposed, sedp::guid_instance(doomed)),
        ],
        "held for the lapsed right"
    );

    let now = link.now;
    let departure = datagrams(
        link.right
            .dispose_participant(now)
            .expect("the departure encodes"),
    );
    assert!(!departure.is_empty(), "the right announces its departure");
    link.right_to_left = true;
    link.carry(Side::Right, departure);
    assert_eq!(
        history(&link.left, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
        announcements_of(&[kept]),
        "a departed participant is owed no retirement"
    );
}

// ---------------------------------------------------------------------------
// Disposals in the forms other stacks send
// ---------------------------------------------------------------------------

/// How a disposal names the endpoint it retires.
#[derive(Debug, Clone, Copy)]
enum KeyForm {
    /// `PID_KEY_HASH` in the inline QoS and no payload at all.
    KeyHashOnly,
    /// A `PL_CDR` key payload holding `PID_ENDPOINT_GUID`.
    ParameterList,
}

/// A disposal of `endpoint`, as the right's SEDP writer `writer` would send
/// it at `number` in the given form.
fn foreign_disposal(
    from: GuidPrefix,
    writer: EntityId,
    reader: EntityId,
    number: SequenceNumber,
    endpoint: Guid,
    form: KeyForm,
) -> Vec<u8> {
    let mut inline_qos = ParameterList::new(inline_qos_encoding(Endianness::Little));
    inline_qos
        .push_octets(ParameterId::new(pid::STATUS_INFO), vec![0, 0, 0, 3])
        .expect("a status info fits");
    let payload = match form {
        KeyForm::KeyHashOnly => {
            inline_qos
                .push_octets(
                    ParameterId::new(pid::KEY_HASH),
                    endpoint.to_bytes().to_vec(),
                )
                .expect("a key hash fits");
            DataPayload::None
        }
        KeyForm::ParameterList => {
            let mut key = ParameterList::new(Encoding::DISCOVERY);
            key.push_value(ParameterId::new(pid::ENDPOINT_GUID), &endpoint)
                .expect("a GUID fits");
            DataPayload::Key(SerializedPayload::from_parameter_list(&key).expect("the key encodes"))
        }
    };
    let mut message = Message::new(Header::new(from));
    message.push(Data::new(reader, writer, number, payload).with_inline_qos(inline_qos));
    message.encode().expect("the disposal encodes")
}

/// Hand the left a disposal of the right's `endpoint` in `form`, from the
/// right's SEDP writer for its kind, and check that the left's SEDP reader
/// took it — so that what the test sees is the attribution, not a rejection.
fn dispose_in(link: &mut Link, endpoint: Guid, form: KeyForm) {
    let (writer, reader) = if endpoint.entity_id.is_writer() {
        (
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
        )
    } else {
        (
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
        )
    };
    let number = link
        .right
        .writers
        .get(&writer)
        .map(RtpsWriter::last_change)
        .expect("the right has its SEDP writers")
        .next();
    let right = link.guid(Side::Right);
    let datagram = foreign_disposal(right.prefix, writer, reader, number, endpoint, form);
    let now = link.now;
    link.left
        .handle_message(&datagram, now)
        .expect("the disposal is a valid datagram");
    let taken = link
        .left
        .readers
        .get(&reader)
        .and_then(|local| {
            local
                .matched_writers()
                .find(|proxy| proxy.guid() == Guid::new(right.prefix, writer))
                .map(crate::behavior::proxy::WriterProxy::acked_through)
        })
        .expect("the left's SEDP reader is matched to the right's writer");
    assert_eq!(
        taken, number,
        "the left's SEDP reader took the {form:?} disposal of {endpoint}"
    );
}

/// Two participants that know each other, the right with two publications
/// and two subscriptions the left has learned.
fn right_with_two_of_each() -> (Link, [Guid; 2], [Guid; 2]) {
    let mut link = Link::new(LONG_LEASE, LONG_LEASE);
    link.meet();
    let writers = [
        link.create(Side::Right, true, "rt/right/first_publication"),
        link.create(Side::Right, true, "rt/right/second_publication"),
    ];
    let readers = [
        link.create(Side::Right, false, "rt/right/first_subscription"),
        link.create(Side::Right, false, "rt/right/second_subscription"),
    ];
    let everything = (set(&writers), set(&readers));
    link.tick_until("the left learning the right's endpoints", 10, |link| {
        known(&link.left) == everything
    });
    (link, writers, readers)
}

#[test]
fn a_disposal_that_names_its_endpoint_only_by_key_hash_retires_it() {
    // A spec-valid disposal: no payload, the endpoint named by
    // `PID_KEY_HASH` alone. There is no key in the sample to read a GUID
    // from; the instance the reader filed it under is the GUID.
    let (mut link, writers, readers) = right_with_two_of_each();
    dispose_in(&mut link, writers[0], KeyForm::KeyHashOnly);
    assert_eq!(
        known(&link.left),
        (set(&writers[1..]), set(&readers)),
        "exactly the publication the key hash names is gone"
    );
    dispose_in(&mut link, readers[0], KeyForm::KeyHashOnly);
    assert_eq!(
        known(&link.left),
        (set(&writers[1..]), set(&readers[1..])),
        "and exactly the subscription"
    );
}

#[test]
fn a_disposal_keyed_by_a_parameter_list_retires_its_endpoint() {
    // The key as a `PL_CDR` list holding `PID_ENDPOINT_GUID`: read as a
    // plain-CDR key, its first sixteen octets after the encapsulation are a
    // parameter header and most of the GUID — a GUID of nobody.
    let (mut link, writers, readers) = right_with_two_of_each();
    dispose_in(&mut link, writers[1], KeyForm::ParameterList);
    assert_eq!(
        known(&link.left),
        (set(&writers[..1]), set(&readers)),
        "exactly the publication the key names is gone"
    );
    dispose_in(&mut link, readers[1], KeyForm::ParameterList);
    assert_eq!(
        known(&link.left),
        (set(&writers[..1]), set(&readers[..1])),
        "and exactly the subscription"
    );
}

// ---------------------------------------------------------------------------
// D2 attribution precedence: the instance first, the plain-CDR key as
// fallback, tried in that order
// ---------------------------------------------------------------------------
//
// The two tests above exercise `forget_disposed` only through shapes where
// one of its two candidates is unavailable (no payload to read a key from,
// or a `PL_CDR` payload that misreads as a GUID of nobody), so they cannot
// tell an implementation that tries the candidates in the right order from
// one that tries them in the wrong order, or that drops the fallback
// outright: whichever candidate is available is also the one that wins. The
// four tests below build the `Sample` `forget_disposed` sees directly,
// rather than through the wire, so that `instance` and the payload's
// plain-CDR key can be set independently and made to disagree.

/// The plain-CDR key payload `RtpsWriter::dispose` sends: a four-octet
/// encapsulation header, then the sixteen raw octets of `guid` — exactly
/// what `sedp::guid_from_key` reads back.
fn plain_cdr_key(guid: Guid) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(
        &astrs_cdr::EncapsulationHeader::new(astrs_cdr::EncapsulationKind::CdrLe).to_bytes(),
    );
    payload.extend_from_slice(&guid.to_bytes());
    payload
}

/// A GUID no test here ever creates, sharing `like`'s entity id so it still
/// looks like the same kind of endpoint.
fn unknown_guid(like: Guid) -> Guid {
    Guid::new(
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [200; 10]),
        like.entity_id,
    )
}

/// A disposal `Sample`, built directly rather than decoded off the wire, so
/// that `instance` and `payload` can be chosen independently of each other —
/// which is what pinning `forget_disposed`'s precedence needs.
fn disposal_sample(from: Guid, instance: InstanceHandle, payload: Vec<u8>) -> Sample {
    Sample {
        writer: from,
        sequence_number: SequenceNumber::new(1),
        payload,
        source_timestamp: None,
        received_at: Instant::now(),
        kind: ChangeKind::NotAliveDisposed,
        instance,
    }
}

#[test]
fn a_key_hash_only_disposal_is_attributed_through_the_instance() {
    // (a) `DataPayload::None` plus `PID_KEY_HASH`: there is no key in the
    // payload to fall back to. Only the instance the reader filed the
    // disposal under carries a GUID here, so this pins that the instance
    // path is tried at all — a `forget_disposed` that tried the plain-CDR
    // key first and the instance only when the key fails would still pass,
    // since the key is unavailable, but one that dropped the instance path
    // entirely and read the key alone (the pre-fix behaviour) would forget
    // nobody.
    let (mut link, writers, _readers) = right_with_two_of_each();
    let sample = disposal_sample(writers[0], sedp::guid_instance(writers[0]), Vec::new());
    let event = link
        .left
        .forget_disposed(&sample, |db, guid| db.forget_writer(guid));
    assert!(event.is_some(), "the key-hash-named endpoint is forgotten");
    assert!(!known(&link.left).0.contains(&writers[0]), "it left the DB");
    assert!(
        known(&link.left).0.contains(&writers[1]),
        "its sibling is untouched"
    );
}

#[test]
fn a_plain_cdr_key_disposal_with_no_instance_is_attributed_through_the_fallback() {
    // (b) The instance is NIL and only the payload's plain-CDR key names a
    // GUID. Dropping the fallback — attributing by the instance alone —
    // would find nothing here even though the key names an endpoint this
    // participant knows.
    let (mut link, writers, _readers) = right_with_two_of_each();
    let sample = disposal_sample(writers[0], InstanceHandle::NIL, plain_cdr_key(writers[0]));
    let event = link
        .left
        .forget_disposed(&sample, |db, guid| db.forget_writer(guid));
    assert!(event.is_some(), "the plain-CDR key's endpoint is forgotten");
    assert!(!known(&link.left).0.contains(&writers[0]), "it left the DB");
    assert!(
        known(&link.left).0.contains(&writers[1]),
        "its sibling is untouched"
    );
}

#[test]
fn a_disposal_whose_instance_names_a_stranger_falls_back_to_the_key() {
    // (c) The instance names a GUID this participant has never discovered,
    // and the payload's plain-CDR key names one it has. An instance is a
    // candidate to *try*, not one to stop at merely for being non-NIL:
    // stopping there, instead of falling through when it names nobody
    // known, would find nothing here.
    let (mut link, writers, _readers) = right_with_two_of_each();
    let stranger = unknown_guid(writers[0]);
    let sample = disposal_sample(
        writers[0],
        sedp::guid_instance(stranger),
        plain_cdr_key(writers[0]),
    );
    let event = link
        .left
        .forget_disposed(&sample, |db, guid| db.forget_writer(guid));
    assert!(
        event.is_some(),
        "the plain-CDR key's endpoint is forgotten by the fallback"
    );
    assert!(!known(&link.left).0.contains(&writers[0]), "it left the DB");
    assert!(
        known(&link.left).0.contains(&writers[1]),
        "its sibling is untouched"
    );
}

#[test]
fn a_disposal_whose_instance_and_key_disagree_is_attributed_through_the_instance() {
    // (d) Both the instance and the payload's plain-CDR key name a known
    // endpoint, and they disagree. The instance is authoritative — the
    // plain-CDR key is a fallback for when the instance names nothing this
    // participant knows, not a second vote — so trying the key first, or
    // letting it override an instance that did resolve, would forget the
    // wrong endpoint.
    let (mut link, writers, _readers) = right_with_two_of_each();
    let sample = disposal_sample(
        writers[0],
        sedp::guid_instance(writers[0]),
        plain_cdr_key(writers[1]),
    );
    let event = link
        .left
        .forget_disposed(&sample, |db, guid| db.forget_writer(guid));
    assert!(event.is_some(), "the instance-named endpoint is forgotten");
    assert!(
        !known(&link.left).0.contains(&writers[0]),
        "the instance's endpoint left the DB"
    );
    assert!(
        known(&link.left).0.contains(&writers[1]),
        "the key's endpoint, only a disagreeing fallback candidate, is untouched"
    );
}
