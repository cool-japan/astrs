//! SEDP files every endpoint under an instance of its own.
//!
//! `DCPSPublication` and `DCPSSubscription` are *keyed* topics: the key of a
//! sample is the endpoint's GUID (OMG DDSI-RTPS 2.3 §8.5.4.2), and a key of
//! sixteen octets is its own key hash (§9.6.3.8). Their builtin writers keep
//! `KEEP_LAST 1` under `TRANSIENT_LOCAL`, and `KEEP_LAST` counts **per
//! instance** — so what a late joiner is replayed is the current state of
//! every endpoint: the announcement of each one that exists, and the disposal
//! of each one that went.
//!
//! File every announcement under one keyless instance instead and the same
//! policy keeps one announcement *per topic*. Nothing looks wrong to a peer
//! that was listening all along — it heard each announcement live — which is
//! why every test here is about a peer that was **not**: it is created after
//! the endpoints were, and learns them only from the replay.
//!
//! A deleted endpoint's instance is *retired*: disposed and unregistered
//! (`PID_STATUS_INFO` 0x3), and held only until every matched reader has the
//! retirement. Kept any longer it would be kept for ever — entity keys are
//! never reused, so nothing replaces it — and a participant that creates
//! and deletes endpoints for its whole life would grow its SEDP history, and
//! every late joiner's replay, without bound.
//!
//! What survives such a churn is a few announcements scattered across
//! everything the writer ever wrote, and a late joiner must be served them
//! at once however much lies between: every run of numbers the history no
//! longer holds is one `GAP`, and a replay the joiner lost is repaired in
//! round trips, not in heartbeat periods.
//!
//! Two halves, as in `durability.rs`. The synchronous one pins the writer's
//! history directly: one announcement per endpoint, a disposal that
//! replaces exactly the endpoint it names, a retirement that leaves the
//! history exactly when the last matched reader has it, and a replay across
//! fifty thousand deletions driven through the real reader state machine.
//! The asynchronous one runs the whole path over real UDP, which is the only
//! place the *reader* half can go wrong: a `KEEP_LAST 1` bound counted across
//! the topic keeps one sample of every datagram, and a replay packs several
//! announcements into one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astrs_cdr::pid;
use astrs_rtps::behavior::endpoint::TopicKey;
use astrs_rtps::behavior::transport::UdpTransport;
use astrs_rtps::behavior::{
    BehaviorResult, ChangeKind, InstanceHandle, Outbound, Participant, ReaderConfig, ReaderHandle,
    ReaderProxy, RtpsReader, RtpsWriter, WriterConfig, WriterHandle, WriterProxy,
};
use astrs_rtps::discovery::sedp::{participant_topic, publications_topic};
use astrs_rtps::discovery::{DiscoveredWriterData, ReaderQos, RosCompat, WriterQos};
use astrs_rtps::messages::{AckNack, Gap, Message, Submessage};
use astrs_rtps::structure::{
    ENTITYID_PARTICIPANT, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
    ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER, EntityId, EntityKind, Guid, GuidPrefix, Locator,
    SequenceNumber, SequenceNumberSet,
};
use tokio::task::JoinHandle;

use harness::{LossySocket, PATIENCE, TICK, config};

// ---------------------------------------------------------------------------
// The synchronous half: one SEDP publications writer, no runtime
// ---------------------------------------------------------------------------

/// The participant every synchronous fixture's endpoints belong to.
const OWNER: GuidPrefix = GuidPrefix::new([7; 12]);

/// One of [`OWNER`]'s user writers, by entity key.
fn endpoint(key: u32) -> Guid {
    Guid::new(
        OWNER,
        EntityId::user_defined(key, EntityKind::USER_WRITER_NO_KEY),
    )
}

/// The instance an endpoint's announcements are filed under: its GUID.
fn instance_of(guid: Guid) -> InstanceHandle {
    InstanceHandle::new(guid.to_bytes())
}

/// [`OWNER`]'s SEDP publications writer, with the builtin SEDP QoS.
fn publications_writer() -> RtpsWriter {
    RtpsWriter::new(
        WriterConfig::new(
            Guid::new(OWNER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
            publications_topic().expect("the builtin names are constants"),
        )
        .with_qos(WriterQos::builtin_sedp()),
    )
}

/// Announce one endpoint the way a participant does: alive, under the
/// endpoint's own instance.
fn announce(writer: &mut RtpsWriter, guid: Guid, now: Instant) -> SequenceNumber {
    writer
        .write_change(
            // Stand-in octets: the writer never looks inside a sample.
            vec![0x00, 0x03, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00],
            None,
            ChangeKind::Alive,
            instance_of(guid),
            now,
        )
        .expect("an announcement fits")
}

/// Everything the writer holds, oldest first.
fn held(writer: &RtpsWriter) -> Vec<(i64, ChangeKind, InstanceHandle)> {
    writer
        .cache()
        .iter()
        .map(|change| (change.sequence_number.value(), change.kind, change.instance))
        .collect()
}

/// Every sequence number a batch of datagrams carries a `DATA` for.
fn data_numbers(outbound: &[Outbound]) -> Vec<i64> {
    let mut numbers = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        for submessage in message.iter() {
            if let Some(data) = submessage.as_data() {
                numbers.push(data.writer_sn.value());
            }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// Every sequence number a batch of datagrams declares irrelevant.
fn gap_numbers(outbound: &[Outbound]) -> Vec<i64> {
    let mut numbers = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        for submessage in message.iter() {
            if let Some(gap) = submessage.as_gap() {
                numbers.extend(gap.irrelevant().map(SequenceNumber::value));
            }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// The raw `PID_STATUS_INFO` octets of every `DATA` in a batch that carries
/// one, by sequence number.
///
/// Raw, not read back through `ChangeKind::from_status_info`: that reads a
/// retirement (0x3) and a plain disposal (0x1) as the same kind, and the
/// difference between them is what these tests are about.
fn status_infos(outbound: &[Outbound]) -> Vec<(i64, [u8; 4])> {
    let mut found = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        for submessage in message.iter() {
            let Some(data) = submessage.as_data() else {
                continue;
            };
            let Some(status) = data
                .inline_qos
                .as_ref()
                .and_then(|qos| qos.get_by_base(pid::STATUS_INFO))
            else {
                continue;
            };
            let octets = <[u8; 4]>::try_from(status.value.as_ref()).expect("four octets");
            found.push((data.writer_sn.value(), octets));
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The GUID prefix of a peer participant, by seed.
fn peer(seed: u8) -> GuidPrefix {
    GuidPrefix::new([seed; 12])
}

/// The SEDP publications reader of the peer `seed`, as [`OWNER`]'s writer
/// sees it: `TRANSIENT_LOCAL`, and reliable unless `reliable` says not.
fn peer_reader(seed: u8, reliable: bool) -> ReaderProxy {
    ReaderProxy::new(
        Guid::new(peer(seed), ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 7_411)],
        reliable,
    )
    .wanting_history(true)
}

/// The `ACKNACK` in which the peer `seed`'s reader acknowledges everything
/// through `through` and asks for nothing, applied to `writer`.
fn acknowledge(writer: &mut RtpsWriter, seed: u8, through: SequenceNumber, count: i32) {
    let reader = Guid::new(peer(seed), ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER);
    let acknack = AckNack::new(
        reader.entity_id,
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
        SequenceNumberSet::new(through.next()),
        count,
    );
    assert!(
        writer.on_acknack(reader, &acknack),
        "a fresh ACKNACK from a matched reader applies"
    );
}

/// Serve the peer `seed`'s newly matched reader its whole replay, playing the
/// reader's part: after each round, acknowledge everything the round covered
/// with a `DATA` or a `GAP`. Returns the sequence numbers it was sent a
/// `DATA` for.
///
/// One round is not enough on a long history: a `produce` serves at most
/// `MAX_SAMPLES_PER_PRODUCE` samples per reader — the runs of holes between
/// them cost a `GAP` each, whatever their length — and the next round serves
/// the rest.
fn replay_to(writer: &mut RtpsWriter, seed: u8, now: Instant) -> Vec<i64> {
    let mut delivered = Vec::new();
    let mut count = 0;
    loop {
        let outbound = writer.produce(now).expect("produce");
        let data = data_numbers(&outbound);
        let covered = data
            .iter()
            .chain(gap_numbers(&outbound).iter())
            .copied()
            .max();
        let Some(covered) = covered else {
            break;
        };
        delivered.extend(data);
        count += 1;
        acknowledge(writer, seed, SequenceNumber::new(covered), count);
        if covered >= writer.last_change().value() {
            break;
        }
    }
    delivered.sort_unstable();
    delivered
}

/// The key octets of a disposal of `guid`, as the generic path would carry
/// them: a `CDR_LE` encapsulation header, then the GUID.
fn key_of(guid: Guid) -> Vec<u8> {
    let mut key = vec![0x00, 0x01, 0x00, 0x00];
    key.extend_from_slice(&guid.to_bytes());
    key
}

#[test]
fn a_disposal_replaces_exactly_the_endpoint_it_names() {
    let now = Instant::now();
    let mut writer = publications_writer();
    let first = announce(&mut writer, endpoint(1), now);
    let second = announce(&mut writer, endpoint(2), now);
    assert_eq!(
        writer.cache().len(),
        2,
        "KEEP_LAST 1 keeps one announcement per endpoint"
    );

    let first_gone = writer.dispose(endpoint(1), now).expect("dispose");
    assert_eq!(
        held(&writer),
        vec![
            (second.value(), ChangeKind::Alive, instance_of(endpoint(2))),
            (
                first_gone.value(),
                ChangeKind::NotAliveDisposed,
                instance_of(endpoint(1)),
            ),
        ],
        "the disposal replaces the announcement of the endpoint it names, and no other"
    );
    assert!(!writer.cache().contains(first));

    // The second disposal on one writer is where a disposal filed under a
    // shared instance does its damage: it evicts the *first* disposal, and
    // the endpoint that one retired is announced alive to every late joiner.
    // Filed per endpoint, it replaces endpoint 2's announcement and nothing
    // else. The first retirement goes too, for a different reason: no reader
    // is matched to be owed it, and the writer sweeps before it writes.
    let second_gone = writer.dispose(endpoint(2), now).expect("dispose");
    assert_eq!(
        held(&writer),
        vec![(
            second_gone.value(),
            ChangeKind::NotAliveDisposed,
            instance_of(endpoint(2)),
        )],
        "with both endpoints gone, nothing alive is left for a replay to resurrect"
    );
    assert!(!writer.cache().contains(second));
}

#[test]
fn a_late_reader_is_replayed_the_current_state_of_every_endpoint() {
    let now = Instant::now();
    let mut writer = publications_writer();
    announce(&mut writer, endpoint(1), now); // 1
    announce(&mut writer, endpoint(2), now); // 2
    announce(&mut writer, endpoint(3), now); // 3
    writer.dispose(endpoint(2), now).expect("dispose"); // 4, replaces 2

    // A peer's SEDP publications reader that only arrives now.
    writer.match_reader(peer_reader(8, true));
    let outbound = writer.produce(now).expect("produce");

    assert_eq!(
        data_numbers(&outbound),
        vec![1, 3],
        "endpoints 1 and 3 announced, and nothing about endpoint 2"
    );
    assert_eq!(
        gap_numbers(&outbound),
        vec![2, 4],
        "endpoint 2's announcement and its retirement are both GAPped: the \
         retirement was owed to nobody when this reader arrived, so it was \
         swept before the reader counted"
    );
}

#[test]
fn a_retirement_goes_out_disposed_and_unregistered() {
    let now = Instant::now();
    let mut writer = publications_writer();
    writer.match_reader(peer_reader(8, true));
    announce(&mut writer, endpoint(1), now); // 1
    announce(&mut writer, endpoint(2), now); // 2
    let retired = writer.dispose(endpoint(1), now).expect("dispose"); // 3
    // A plain disposal through the generic path, for contrast: disposed, but
    // the instance is still registered to this writer.
    let disposed = writer
        .write_change(
            key_of(endpoint(2)),
            None,
            ChangeKind::NotAliveDisposed,
            instance_of(endpoint(2)),
            now,
        )
        .expect("a disposal fits"); // 4

    let outbound = writer.produce(now).expect("produce");
    assert_eq!(
        status_infos(&outbound),
        vec![
            (retired.value(), [0, 0, 0, 3]),
            (disposed.value(), [0, 0, 0, 1])
        ],
        "a deleted entity's instance is disposed *and* unregistered (§9.6.3.9); \
         a plain disposal only disposed"
    );
    assert_eq!(
        ChangeKind::from_status_info([0, 0, 0, 3]),
        ChangeKind::NotAliveDisposed,
        "a receiver, this crate's included, still reads the retirement as a disposal"
    );

    // Once the reader has both, only the retirement leaves: a disposal alone
    // is state a TRANSIENT_LOCAL writer still owes the next reader.
    acknowledge(&mut writer, 8, disposed, 1);
    assert_eq!(
        held(&writer),
        vec![(
            disposed.value(),
            ChangeKind::NotAliveDisposed,
            instance_of(endpoint(2)),
        )]
    );
}

#[test]
fn a_retirement_is_held_until_every_matched_reader_has_it() {
    let now = Instant::now();
    let mut writer = publications_writer();
    writer.match_reader(peer_reader(8, true));
    announce(&mut writer, endpoint(1), now); // 1
    let kept = announce(&mut writer, endpoint(2), now); // 2
    writer.produce(now).expect("produce");
    acknowledge(&mut writer, 8, kept, 1);

    let retired = writer.dispose(endpoint(1), now).expect("dispose"); // 3
    let sent = writer.produce(now).expect("produce");
    assert_eq!(data_numbers(&sent), vec![retired.value()]);
    assert!(
        writer.cache().contains(retired),
        "sent is not had: until the reader acknowledges, it may be missing the \
         retirement, and dropping it would GAP the reader past the only change \
         that says endpoint 1 is gone"
    );

    // A reader that arrives while the retirement is still owed is replayed
    // it too — it names an endpoint the newcomer never heard of, which it
    // ignores — and is owed it from then on.
    writer.match_reader(peer_reader(9, true));
    let replay = writer.produce(now).expect("produce");
    assert_eq!(data_numbers(&replay), vec![kept.value(), retired.value()]);
    assert_eq!(gap_numbers(&replay), vec![1]);

    acknowledge(&mut writer, 8, retired, 2);
    assert!(
        writer.cache().contains(retired),
        "one reader acknowledging is not every reader"
    );
    acknowledge(&mut writer, 9, retired, 1);
    assert_eq!(
        held(&writer),
        vec![(kept.value(), ChangeKind::Alive, instance_of(endpoint(2)))],
        "the last acknowledgement releases the retirement, and only it"
    );

    // A reader that arrives now hears about endpoint 2 and nothing about 1.
    writer.match_reader(peer_reader(10, true));
    let late = writer.produce(now).expect("produce");
    assert_eq!(data_numbers(&late), vec![kept.value()]);
    assert_eq!(gap_numbers(&late), vec![1, retired.value()]);
}

#[test]
fn endless_create_and_delete_cycles_leave_the_sedp_history_bounded() {
    // A long-running ROS 2 node creates and destroys service, parameter and
    // action clients for its whole life, and the participant allocates every
    // one a fresh entity key. Were a deleted endpoint's retirement kept, this
    // history would hold one per cycle — five thousand here — and every late
    // joiner would be replayed all of them.
    const CYCLES: u32 = 5_000;
    const KEEP_EVERY: u32 = 500;
    const ACK_EVERY: u32 = 64;
    let now = Instant::now();
    let mut writer = publications_writer();
    writer.match_reader(peer_reader(8, true));

    let mut survivors = Vec::new();
    let mut largest = 0;
    let mut count = 0;
    for key in 1..=CYCLES {
        let announced = announce(&mut writer, endpoint(key), now);
        if key % KEEP_EVERY == 0 {
            survivors.push(announced.value());
        } else {
            let retired = writer.dispose(endpoint(key), now).expect("dispose");
            writer.produce(now).expect("produce");
            assert!(
                writer.cache().contains(retired),
                "a retirement the reader has not acknowledged is never swept"
            );
        }
        writer.produce(now).expect("produce");
        if key % ACK_EVERY == 0 {
            count += 1;
            let through = writer.last_change();
            acknowledge(&mut writer, 8, through, count);
        }
        largest = largest.max(writer.cache().len());
    }
    count += 1;
    let through = writer.last_change();
    acknowledge(&mut writer, 8, through, count);

    let bound = survivors.len() + ACK_EVERY as usize;
    assert!(
        largest <= bound,
        "the history peaked at {largest} changes; the survivors and one \
         acknowledgement interval's retirements are {bound}"
    );
    assert_eq!(
        held(&writer)
            .iter()
            .map(|(number, _, _)| *number)
            .collect::<Vec<_>>(),
        survivors,
        "once the reader has everything, the history is the survivors' announcements"
    );
    assert!(
        held(&writer)
            .iter()
            .all(|(_, kind, _)| *kind == ChangeKind::Alive),
        "and nothing else"
    );
    assert_eq!(writer.cache().instance_count(), survivors.len());

    // The late joiner: every survivor, and no DATA for a deleted endpoint.
    writer.match_reader(peer_reader(9, true));
    assert_eq!(
        replay_to(&mut writer, 9, now),
        survivors,
        "a late joiner is replayed the survivors' announcements and nothing \
         about the endpoints that were deleted"
    );
}

#[test]
fn a_best_effort_reader_is_owed_one_copy_of_a_retirement() {
    let now = Instant::now();
    let mut writer = publications_writer();
    writer.match_reader(peer_reader(8, false));
    announce(&mut writer, endpoint(1), now); // 1
    writer.produce(now).expect("produce");

    let retired = writer.dispose(endpoint(1), now).expect("dispose"); // 2
    assert!(
        writer.cache().contains(retired),
        "never swept before it is sent"
    );
    let outbound = writer.produce(now).expect("produce");
    assert_eq!(
        status_infos(&outbound),
        vec![(retired.value(), [0, 0, 0, 3])],
        "the one copy"
    );
    assert!(
        writer.cache().is_empty(),
        "and nothing after it: a best-effort reader never acknowledges, so \
         sent is all it will ever be"
    );
}

#[test]
fn a_departure_is_announced_before_it_is_swept() {
    // The SPDP half of the same rule. The participant's departure is a
    // retirement too, and `dispose_participant` announces it straight out of
    // the history — to the group and every peer, whether or not the writer
    // has matched a reader. So the retirement written a moment ago must still
    // be there to announce, and, with no reader matched as here, gone at the
    // next `produce`, when nobody can be owed it.
    let now = Instant::now();
    let participant = Guid::new(OWNER, ENTITYID_PARTICIPANT);
    let mut writer = RtpsWriter::new(
        WriterConfig::new(
            Guid::new(OWNER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER),
            participant_topic().expect("the builtin names are constants"),
        )
        .with_qos(WriterQos::builtin_spdp()),
    );
    let alive = writer
        .write_change(
            vec![0x00, 0x03, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00],
            None,
            ChangeKind::Alive,
            instance_of(participant),
            now,
        )
        .expect("an announcement fits");
    let gone = writer.dispose(participant, now).expect("dispose");
    assert_eq!(
        held(&writer),
        vec![(
            gone.value(),
            ChangeKind::NotAliveDisposed,
            instance_of(participant),
        )],
        "the departure replaces the participant's announcement"
    );

    let targets = vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 7_400)];
    let announcement = writer
        .announce(
            gone,
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
            targets.clone(),
        )
        .expect("the announcement encodes")
        .expect("the departure is still held when it is announced");
    assert_eq!(
        status_infos(&[announcement]),
        vec![(gone.value(), [0, 0, 0, 3])]
    );

    assert!(writer.produce(now).expect("produce").is_empty());
    assert!(
        writer.cache().is_empty(),
        "with no reader matched, nobody is owed the departure once it is out"
    );
    for number in [alive, gone] {
        assert!(
            writer
                .announce(
                    number,
                    ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
                    targets.clone()
                )
                .expect("encodes")
                .is_none(),
            "nothing left to announce, alive or gone"
        );
    }
}

// ---------------------------------------------------------------------------
// A late joiner behind a long churn: served in round trips, not heartbeats
// ---------------------------------------------------------------------------

/// The real announcement of `guid`: the parameter list a participant sends,
/// whose `PID_ENDPOINT_GUID` is what a discovery reader files it under.
fn announcement_of(guid: Guid) -> Vec<u8> {
    DiscoveredWriterData::new(guid, "rt/churn", harness::TYPE_NAME)
        .expect("the fixture's names are valid")
        .to_payload()
        .expect("an announcement encodes")
        .into_cow()
        .into_owned()
}

/// [`OWNER`]'s publications writer after a long churn: endpoint 1 announced
/// at startup, `cycles` endpoints created and deleted with no reader
/// matched, then endpoint 5 announced. The history holds exactly the two
/// survivors, at 1 and at `2 * cycles + 2`; everything between is a hole.
fn after_a_long_churn(cycles: u32) -> RtpsWriter {
    let now = Instant::now();
    let mut writer = publications_writer();
    let announce_real = |writer: &mut RtpsWriter, guid: Guid| {
        writer
            .write_change(
                announcement_of(guid),
                None,
                ChangeKind::Alive,
                instance_of(guid),
                now,
            )
            .expect("an announcement fits")
    };
    announce_real(&mut writer, endpoint(1));
    for key in 0..cycles {
        announce(&mut writer, endpoint(10 + key), now);
        writer.dispose(endpoint(10 + key), now).expect("dispose");
    }
    announce_real(&mut writer, endpoint(5));
    writer
}

/// The SEDP publications reader of the peer `seed` — the real state
/// machine, not a stand-in that acknowledges whatever it is shown — matched
/// to [`OWNER`]'s writer the way discovery wires it.
fn publications_reader(seed: u8) -> RtpsReader {
    let mut reader = RtpsReader::new(
        ReaderConfig::new(
            Guid::new(peer(seed), ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER),
            publications_topic().expect("the builtin names are constants"),
        )
        .with_qos(ReaderQos::builtin_sedp()),
    );
    reader.match_writer(WriterProxy::new(
        Guid::new(OWNER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 7_410)],
        true,
    ));
    reader
}

/// Hand `datagrams` to `reader` the way a participant's receive path does:
/// every submessage, in order, from [`OWNER`].
fn deliver(reader: &mut RtpsReader, datagrams: &[Outbound], now: Instant) {
    for item in datagrams {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        let mut timestamp = None;
        for submessage in message.iter() {
            match submessage {
                Submessage::InfoTimestamp(info) => timestamp = info.timestamp,
                Submessage::Data(data) => {
                    reader
                        .on_data(OWNER, data, timestamp, now)
                        .expect("the writer's own DATA is valid");
                }
                Submessage::Gap(gap) => {
                    reader.on_gap(OWNER, gap);
                }
                Submessage::Heartbeat(heartbeat) => {
                    reader.on_heartbeat(OWNER, heartbeat);
                }
                _ => {}
            }
        }
    }
}

/// Hand whatever `ACKNACK`s the reader has to send to `writer`. Returns how
/// many it sent, and how many of those asked for a repair.
fn answer(writer: &mut RtpsWriter, reader: &mut RtpsReader, now: Instant) -> (usize, usize) {
    let (mut sent, mut requests) = (0, 0);
    for item in reader.produce(now).expect("the reader's ACKNACK encodes") {
        let message = Message::decode(&item.datagram).expect("the reader's own datagram decodes");
        for submessage in message.iter() {
            let Some(acknack) = submessage.as_acknack() else {
                continue;
            };
            sent += 1;
            if !acknack.reader_sn_state.is_empty() {
                requests += 1;
            }
            let from = Guid::new(reader.guid().prefix, acknack.reader_id);
            assert!(writer.on_acknack(from, acknack), "a fresh ACKNACK applies");
        }
    }
    (sent, requests)
}

/// Run the exchange at the one instant `now` until neither side has
/// anything left to say. Returns how many `ACKNACK`s asked for a repair.
///
/// One instant, so the writer's cadence heartbeats at most once: every
/// round after that has to be one the protocol itself prompts.
fn settle(writer: &mut RtpsWriter, reader: &mut RtpsReader, now: Instant) -> usize {
    let mut requests = 0;
    for _ in 0..1_000 {
        let outbound = writer.produce(now).expect("produce");
        deliver(reader, &outbound, now);
        let (sent, asked) = answer(writer, reader, now);
        requests += asked;
        if outbound.is_empty() && sent == 0 {
            return requests;
        }
    }
    panic!("the exchange never went quiet");
}

/// The GAPs in a batch of datagrams.
fn gaps_in(outbound: &[Outbound]) -> Vec<Gap> {
    let mut found = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        found.extend(
            message
                .iter()
                .filter_map(|submessage| submessage.as_gap().cloned()),
        );
    }
    found
}

/// True when a datagram carries a submessage `kind` picks out.
fn carries(item: &Outbound, kind: fn(&Submessage<'_>) -> bool) -> bool {
    Message::decode(&item.datagram)
        .expect("the writer's own datagram decodes")
        .iter()
        .any(kind)
}

/// What a late joiner behind fifty thousand deletions went through.
#[derive(Debug, PartialEq, Eq)]
struct LateJoin {
    /// The sequence numbers the reader was delivered.
    delivered: Vec<i64>,
    /// How many of its `ACKNACK`s asked for a repair.
    repair_requests: usize,
    /// How many cadence heartbeat periods it had to wait.
    periods_waited: u32,
}

/// Replay [`after_a_long_churn`] to a late joiner, losing the datagrams of
/// the first `produce` that `lose` picks, and run the exchange to the end.
fn late_join(cycles: u32, lose: impl Fn(&Outbound) -> bool) -> LateJoin {
    let mut writer = after_a_long_churn(cycles);
    let last = writer.last_change();
    assert_eq!(
        held(&writer)
            .iter()
            .map(|(number, _, _)| *number)
            .collect::<Vec<_>>(),
        vec![1, last.value()],
        "the two survivors, and nothing of the churn between them"
    );
    let mut reader = publications_reader(9);
    writer.match_reader(peer_reader(9, true));

    let now = Instant::now();
    let first = writer.produce(now).expect("produce");
    let found = gaps_in(&first);
    assert_eq!(found.len(), 1, "one GAP for the whole churn: {found:?}");
    assert_eq!(
        (found[0].gap_start, found[0].contiguous_end()),
        (SequenceNumber::new(2), last),
        "its run names every number between the survivors"
    );
    let kept: Vec<Outbound> = first.into_iter().filter(|item| !lose(item)).collect();
    deliver(&mut reader, &kept, now);
    let mut repair_requests = settle(&mut writer, &mut reader, now);

    let mut periods_waited = 0;
    let caught_up = |reader: &RtpsReader| {
        reader
            .matched_writers()
            .all(|proxy| proxy.acked_through() == last)
    };
    if !caught_up(&reader) {
        // Nothing the reader got asked it anything: only the cadence can.
        periods_waited += 1;
        let later = now + writer.config().heartbeat_period;
        repair_requests += settle(&mut writer, &mut reader, later);
    }
    assert!(
        caught_up(&reader),
        "the reader must end with everything through {last}: it acknowledged {:?}",
        reader
            .matched_writers()
            .map(WriterProxy::acked_through)
            .collect::<Vec<_>>()
    );
    let mut delivered: Vec<i64> = reader
        .take_all()
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    delivered.sort_unstable();
    LateJoin {
        delivered,
        repair_requests,
        periods_waited,
    }
}

#[test]
fn a_late_joiner_behind_fifty_thousand_deletions_is_served_in_round_trips() {
    // A node that creates and deletes a client fifty thousand times leaves
    // its SEDP writer holding two announcements a hundred thousand numbers
    // apart. Served 256 numbers per heartbeat period, as it once was, a
    // reader that arrived afterwards waited 390 periods for the second one.
    // Now the whole churn is one GAP, and every loss the replay can suffer
    // costs round trips — the repair names the whole run it lands in, and a
    // repair that stops short prompts the reader for the rest — never
    // another heartbeat period.
    const CYCLES: u32 = 50_000;
    let last = i64::from(CYCLES) * 2 + 2;
    let both = vec![1, last];
    let is_data: fn(&Submessage<'_>) -> bool = |submessage| submessage.as_data().is_some();
    let is_gap: fn(&Submessage<'_>) -> bool = |submessage| submessage.as_gap().is_some();

    assert_eq!(
        late_join(CYCLES, |_| false),
        LateJoin {
            delivered: both.clone(),
            repair_requests: 0,
            periods_waited: 0,
        },
        "nothing lost: the first call serves it all"
    );
    assert_eq!(
        late_join(CYCLES, |item| carries(item, is_gap)),
        LateJoin {
            delivered: both.clone(),
            repair_requests: 1,
            periods_waited: 0,
        },
        "the GAP lost: one request, answered with the whole run"
    );
    assert_eq!(
        late_join(CYCLES, |item| carries(item, is_data)),
        LateJoin {
            delivered: both.clone(),
            repair_requests: 2,
            periods_waited: 0,
        },
        "both announcements lost: the run waits above the hole whole, the \
         first request repairs the hole and the prompt asks for the second"
    );
    assert_eq!(
        late_join(CYCLES, |_| true),
        LateJoin {
            delivered: both,
            repair_requests: 2,
            periods_waited: 1,
        },
        "the whole replay lost — what a late joiner that has not yet heard \
         the writer's participant does with it: one period for the cadence \
         to ask, then round trips"
    );
}

// ---------------------------------------------------------------------------
// A reader that forgot: its participant gave up on the writer's and found it
// again, while the writer's never gave up on it
// ---------------------------------------------------------------------------

/// Run the exchange at the one instant `now` until neither side has anything
/// left to say, whether or not the writer takes what the reader answers.
///
/// [`settle`] insists that every `ACKNACK` is fresh; a reader that forgot is
/// exactly the case where one may not be, and what matters then is what the
/// reader ends up with.
fn exchange(writer: &mut RtpsWriter, reader: &mut RtpsReader, now: Instant) {
    for _ in 0..1_000 {
        let outbound = writer.produce(now).expect("produce");
        deliver(reader, &outbound, now);
        let mut sent = 0;
        for item in reader.produce(now).expect("the reader's ACKNACK encodes") {
            let message =
                Message::decode(&item.datagram).expect("the reader's own datagram decodes");
            for acknack in message.iter().filter_map(Submessage::as_acknack) {
                sent += 1;
                let from = Guid::new(reader.guid().prefix, acknack.reader_id);
                writer.on_acknack(from, acknack);
            }
        }
        if outbound.is_empty() && sent == 0 {
            return;
        }
    }
    panic!("the exchange never went quiet");
}

/// The sequence numbers a reader has been delivered and not yet taken.
fn taken(reader: &mut RtpsReader) -> Vec<i64> {
    let mut numbers: Vec<i64> = reader
        .take_all()
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers
}

/// [`OWNER`]'s publications writer after forty endpoints were announced one
/// heartbeat period apart and one of them deleted, all of it acknowledged by
/// the peer 9's reader — which answered a heartbeat for each, so its
/// `ACKNACK` count is past forty. Returns the writer, the reader, the instant
/// reached and the numbers the history holds.
///
/// Real announcements, so that the reader files each under its endpoint's
/// instance and a replay of several in one datagram keeps them all.
fn forty_endpoints_acknowledged() -> (RtpsWriter, RtpsReader, Instant, Vec<i64>) {
    let mut writer = publications_writer();
    let mut reader = publications_reader(9);
    writer.match_reader(peer_reader(9, true));
    let period = writer.config().heartbeat_period;
    let mut now = Instant::now();
    let mut heard = Vec::new();
    for key in 1..=40 {
        let guid = endpoint(key);
        writer
            .write_change(
                announcement_of(guid),
                None,
                ChangeKind::Alive,
                instance_of(guid),
                now,
            )
            .expect("an announcement fits");
        exchange(&mut writer, &mut reader, now);
        heard.extend(taken(&mut reader));
        now += period;
    }
    writer.dispose(endpoint(20), now).expect("dispose"); // 41
    exchange(&mut writer, &mut reader, now);
    heard.extend(taken(&mut reader));
    heard.sort_unstable();
    assert_eq!(
        heard,
        (1..=41).collect::<Vec<_>>(),
        "the reader heard everything live"
    );
    let held: Vec<i64> = writer
        .cache()
        .iter()
        .map(|change| change.sequence_number.value())
        .collect();
    assert_eq!(
        held,
        (1..=40).filter(|number| *number != 20).collect::<Vec<_>>(),
        "the thirty-nine live announcements; the retirement left once the reader had it"
    );
    (writer, reader, now, held)
}

#[test]
fn a_reader_that_matched_the_writer_again_is_served_everything_it_forgot() {
    // The peer's lease on OWNER ran out, OWNER's on the peer did not. The
    // peer forgot OWNER's endpoints and, rediscovering OWNER, wired its
    // SEDP writer up again with nothing received — while OWNER's writer kept
    // its proxy for the peer's reader, which says the reader has everything.
    // Its ACKNACKs asked for numbers below that watermark, with a count
    // restarted at one, and both were ignored: the peer knew OWNER and none
    // of its endpoints until OWNER restarted.
    let (mut writer, mut reader, now, held) = forty_endpoints_acknowledged();
    let owner = Guid::new(OWNER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER);
    assert!(reader.unmatch_writer(owner));
    reader.match_writer(WriterProxy::new(
        owner,
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 7_410)],
        true,
    ));

    // One heartbeat period: the writer heartbeats, the reader answers.
    let period = writer.config().heartbeat_period;
    let later = now + period;
    exchange(&mut writer, &mut reader, later);
    assert_eq!(
        taken(&mut reader),
        held,
        "the first exchange after the reader matched the writer again replays every \
         announcement the writer holds"
    );
    // The next heartbeat is answered with an acknowledgement of all of it.
    exchange(&mut writer, &mut reader, later + period);
    assert!(writer.is_acknowledged(), "and the reader has it all again");
    assert!(taken(&mut reader).is_empty(), "delivered once");
}

#[test]
fn a_reader_that_restarted_its_count_is_heard_after_a_short_silence() {
    // The same, from a reader that does not count on from the old proxy —
    // another stack's, or this one's under a participant restarted with the
    // same GUID prefix: its ACKNACK count starts at one, below the forty the
    // writer last took. Counting up to that would take forty heartbeat
    // periods; it is heard once it has been silent, as far as the writer can
    // tell, for a handful.
    let (mut writer, _, mut now, held) = forty_endpoints_acknowledged();
    let mut reader = publications_reader(9);
    let period = writer.config().heartbeat_period;
    let mut periods = 0;
    while reader.is_empty() {
        assert!(periods < 40, "the restarted reader was never heard");
        now += period;
        periods += 1;
        exchange(&mut writer, &mut reader, now);
    }
    assert!(
        periods > 1,
        "a stale count is not taken at once: it is what a reordered ACKNACK looks like"
    );
    assert!(
        periods <= 12,
        "heard after {periods} periods: the silence counts, not the forty the count lags by"
    );
    assert_eq!(taken(&mut reader), held);
}

// ---------------------------------------------------------------------------
// The asynchronous half: a peer that arrives after the endpoints did
// ---------------------------------------------------------------------------

/// A topic of the endpoint's own, so that no two endpoints here ever match.
fn topic_named(name: &str) -> TopicKey {
    TopicKey::new(name, harness::TYPE_NAME).expect("the fixture's names are valid")
}

/// A participant whose only peer is `peer`, running on a background task.
async fn start(seed: u8, peer: Option<Locator>) -> (Participant, JoinHandle<BehaviorResult<()>>) {
    let participant = Participant::new(config(seed, peer, RosCompat::Jazzy))
        .await
        .expect("the participant must bind");
    let task = participant.spawn(TICK);
    (participant, task)
}

/// Stop a participant and join its loop.
async fn stop(participant: &Participant, task: JoinHandle<BehaviorResult<()>>) {
    participant.shutdown().await;
    let _ = tokio::time::timeout(PATIENCE, task).await;
}

/// Two publications and two subscriptions, each on a topic of its own, with
/// four different QoS so no announcement can pass for another.
async fn two_of_each(participant: &Participant) -> (Vec<WriterHandle>, Vec<ReaderHandle>) {
    let mut writers = Vec::new();
    for (name, qos) in [
        ("rt/sedp/first_publication", WriterQos::services_default()),
        ("rt/sedp/second_publication", WriterQos::latched(1)),
    ] {
        writers.push(
            participant
                .create_writer(topic_named(name), qos)
                .await
                .expect("the writer must be created"),
        );
    }
    let mut readers = Vec::new();
    for (name, qos) in [
        ("rt/sedp/first_subscription", ReaderQos::reliable(10)),
        ("rt/sedp/second_subscription", ReaderQos::sensor_data()),
    ] {
        readers.push(
            participant
                .create_reader(topic_named(name), qos)
                .await
                .expect("the reader must be created"),
        );
    }
    (writers, readers)
}

/// The remote writers and readers `participant` has discovered.
async fn known_endpoints(participant: &Participant) -> (BTreeSet<Guid>, BTreeSet<Guid>) {
    let db = participant.discovery_snapshot().await;
    (
        db.writers().map(|writer| writer.guid()).collect(),
        db.readers().map(|reader| reader.guid()).collect(),
    )
}

/// Wait until `participant` knows exactly `writers` and `readers`, then check
/// that it goes on knowing exactly that.
///
/// [`harness::await_condition`] plus the one thing it cannot do: say what
/// *was* known when it gave up. That is the whole diagnosis of a failure
/// here — "learned only the newest endpoint" and "was told about a deleted
/// one" both show up in it and nowhere else.
///
/// The second half is a negative, and like the harness's other negatives it
/// can only be tested by giving the wrong thing time to happen: five ticks is
/// five heartbeat periods, every repair a replay could still be waiting on.
async fn await_exactly(participant: &Participant, what: &str, writers: &[Guid], readers: &[Guid]) {
    let writers: BTreeSet<Guid> = writers.iter().copied().collect();
    let readers: BTreeSet<Guid> = readers.iter().copied().collect();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let (known_writers, known_readers) = known_endpoints(participant).await;
        if known_writers == writers && known_readers == readers {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "{what} did not happen within {PATIENCE:?}: expected writers {writers:?} and \
                 readers {readers:?}, but the participant knew writers {known_writers:?} and \
                 readers {known_readers:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    tokio::time::sleep(TICK * 5).await;
    let (known_writers, known_readers) = known_endpoints(participant).await;
    assert_eq!(
        known_writers, writers,
        "{what}: the writers changed after settling"
    );
    assert_eq!(
        known_readers, readers,
        "{what}: the readers changed after settling"
    );
}

fn writer_guids(writers: &[WriterHandle]) -> Vec<Guid> {
    writers.iter().map(WriterHandle::guid).collect()
}

fn reader_guids(readers: &[ReaderHandle]) -> Vec<Guid> {
    readers.iter().map(ReaderHandle::guid).collect()
}

#[tokio::test]
async fn a_late_joiner_learns_every_endpoint_that_existed_before_it() {
    let (left, left_task) = start(181, None).await;
    let (writers, readers) = two_of_each(&left).await;

    // Only now does the peer exist: everything it learns, it learns from the
    // TRANSIENT_LOCAL replay of the two SEDP writers.
    let (right, right_task) = start(182, Some(left.metatraffic_locator())).await;
    await_exactly(
        &right,
        "the late joiner learning both publications and both subscriptions",
        &writer_guids(&writers),
        &reader_guids(&readers),
    )
    .await;

    // Each endpoint is known by its own announcement, not a neighbour's.
    let db = right.discovery_snapshot().await;
    for writer in &writers {
        let known = db.writer(writer.guid()).expect("announced");
        assert_eq!(known.identity.topic_name, writer.topic().topic_name);
    }
    for reader in &readers {
        let known = db.reader(reader.guid()).expect("announced");
        assert_eq!(known.identity.topic_name, reader.topic().topic_name);
    }

    stop(&right, right_task).await;
    stop(&left, left_task).await;
}

#[tokio::test]
async fn deleting_an_endpoint_removes_exactly_that_endpoint_at_the_peer() {
    let (left, left_task) = start(183, None).await;
    let (writers, readers) = two_of_each(&left).await;
    let (right, right_task) = start(184, Some(left.metatraffic_locator())).await;
    await_exactly(
        &right,
        "the late joiner learning all four endpoints",
        &writer_guids(&writers),
        &reader_guids(&readers),
    )
    .await;

    assert!(
        left.delete_writer(writers[0].guid()).await.expect("delete"),
        "the writer was there"
    );
    assert!(
        left.delete_reader(readers[0].guid()).await.expect("delete"),
        "the reader was there"
    );
    await_exactly(
        &right,
        "the peer forgetting exactly the deleted publication and subscription",
        &[writers[1].guid()],
        &[readers[1].guid()],
    )
    .await;

    stop(&right, right_task).await;
    stop(&left, left_task).await;
}

#[tokio::test]
async fn a_late_joiner_is_told_nothing_about_endpoints_that_are_gone() {
    let (left, left_task) = start(185, None).await;
    let (writers, readers) = two_of_each(&left).await;

    // Both publications go — two disposals on one SEDP writer, the case a
    // disposal filed under a shared instance gets wrong — and one
    // subscription does.
    for writer in &writers {
        assert!(left.delete_writer(writer.guid()).await.expect("delete"));
    }
    assert!(left.delete_reader(readers[0].guid()).await.expect("delete"));
    // A publication created after the deletions. It is the newest change the
    // publications writer holds, so knowing it proves the replay was heard.
    let survivor = left
        .create_writer(
            topic_named("rt/sedp/third_publication"),
            WriterQos::services_default(),
        )
        .await
        .expect("the writer must be created");

    let (late, late_task) = start(186, Some(left.metatraffic_locator())).await;
    await_exactly(
        &late,
        "the late joiner learning exactly the endpoints that still exist",
        &[survivor.guid()],
        &[readers[1].guid()],
    )
    .await;

    stop(&late, late_task).await;
    stop(&left, left_task).await;
}

/// A participant with no initial peer, at the harness cadence except for its
/// heartbeat, which comes every `heartbeat`.
async fn start_heartbeating_every(
    seed: u8,
    heartbeat: Duration,
) -> (Participant, JoinHandle<BehaviorResult<()>>) {
    let participant =
        Participant::new(config(seed, None, RosCompat::Jazzy).with_heartbeat_period(heartbeat))
            .await
            .expect("the participant must bind");
    let task = participant.spawn(TICK);
    (participant, task)
}

#[tokio::test]
async fn a_late_joiner_behind_thousands_of_deletions_waits_one_heartbeat_period() {
    // Two thousand endpoints of each kind created and deleted before the
    // peer exists, one in 250 kept: each SEDP writer holds eight
    // announcements spread over four thousand numbers of holes. Served 256
    // numbers per heartbeat period, that replay took sixteen periods; walked
    // hole by hole but repaired one survivor per period, eight. Here it must
    // take one: the period in which the cadence asks a late joiner that
    // dropped the first replay what it lacks — it drops it because the
    // replay reaches it before the churner's SPDP announcement does — and
    // then round trips.
    //
    // The churner heartbeats every second, so PATIENCE holds five periods:
    // enough for one, and for any load this host is under, and too few for
    // eight or sixteen.
    const CYCLES: u32 = 2_000;
    const KEEP_EVERY: u32 = 250;
    const HEARTBEAT: Duration = Duration::from_secs(1);
    let (churner, churner_task) = start_heartbeating_every(187, HEARTBEAT).await;
    let mut writers = Vec::new();
    let mut readers = Vec::new();
    for cycle in 0..CYCLES {
        let topic = topic_named(&format!("rt/sedp/churn_{cycle}"));
        let writer = churner
            .create_writer(topic.clone(), WriterQos::services_default())
            .await
            .expect("the writer must be created");
        let reader = churner
            .create_reader(topic, ReaderQos::reliable(10))
            .await
            .expect("the reader must be created");
        if cycle % KEEP_EVERY == 0 {
            writers.push(writer.guid());
            readers.push(reader.guid());
        } else {
            assert!(churner.delete_writer(writer.guid()).await.expect("delete"));
            assert!(churner.delete_reader(reader.guid()).await.expect("delete"));
        }
    }

    let (late, late_task) = start(188, Some(churner.metatraffic_locator())).await;
    await_exactly(
        &late,
        "the late joiner learning exactly the survivors of the churn",
        &writers,
        &readers,
    )
    .await;

    stop(&late, late_task).await;
    stop(&churner, churner_task).await;
}

// ---------------------------------------------------------------------------
// A lease that runs out on one side only
// ---------------------------------------------------------------------------

/// A participant whose only peer is `peer`, asking its peers for `lease`,
/// whose metatraffic — its announcements and everything SEDP sends — can be
/// cut with the returned socket.
async fn start_cuttable(
    seed: u8,
    peer: Option<Locator>,
    lease: astrs_rtps::structure::Duration,
) -> (
    Participant,
    JoinHandle<BehaviorResult<()>>,
    Arc<LossySocket>,
) {
    let mut settings = config(seed, peer, RosCompat::Jazzy);
    settings.spdp = settings.spdp.with_lease(lease);
    let metatraffic = LossySocket::bind_loopback(0).await;
    let user_data = UdpTransport::bind_loopback()
        .await
        .expect("the user-traffic socket must bind");
    let participant =
        Participant::with_transport(settings, metatraffic.clone(), Arc::new(user_data), None)
            .await
            .expect("the participant must build on the supplied sockets");
    let task = participant.spawn(TICK);
    (participant, task, metatraffic)
}

/// Wait, up to `patience`, until `participant` no longer knows `peer`.
async fn await_forgotten(participant: &Participant, peer: Guid, patience: Duration) {
    let deadline = tokio::time::Instant::now() + patience;
    while participant.knows(peer).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{peer} was still known after {patience:?}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn a_lease_that_runs_out_on_one_side_hides_no_endpoint_and_leaves_no_ghost() {
    // The fickle participant asks its peers for a lease of two seconds, the
    // steady one for the default hundred, and the steady one holds the
    // fickle one as its initial peer, so its announcements reach it
    // whatever the steady one knows. Then the fickle one's metatraffic is
    // cut: its announcements stop reaching the steady one, which gives up on
    // it, while the steady one's keep reaching it and it never gives up.
    // Then the link heals. Each side then holds the half the other lost:
    //
    // - The steady one wired the fickle one's SEDP writers up again from
    //   scratch; the fickle one's writers still think it has every
    //   announcement. It must learn the fickle one's endpoints again.
    // - The steady one deleted a publication while it had the fickle one
    //   unmatched; the fickle one never heard. It must forget it.
    const LEASE: astrs_rtps::structure::Duration = astrs_rtps::structure::Duration::from_secs(2);
    let (fickle, fickle_task, cut) = start_cuttable(190, None, LEASE).await;
    let (steady, steady_task) = start(189, Some(fickle.metatraffic_locator())).await;
    let (writers, readers) = two_of_each(&fickle).await;
    let doomed = steady
        .create_writer(
            topic_named("rt/sedp/doomed_publication"),
            WriterQos::services_default(),
        )
        .await
        .expect("the writer must be created");
    let kept = steady
        .create_writer(
            topic_named("rt/sedp/kept_publication"),
            WriterQos::services_default(),
        )
        .await
        .expect("the writer must be created");
    await_exactly(
        &steady,
        "the steady participant learning the fickle one's endpoints",
        &writer_guids(&writers),
        &reader_guids(&readers),
    )
    .await;
    await_exactly(
        &fickle,
        "the fickle participant learning the steady one's publications",
        &[doomed.guid(), kept.guid()],
        &[],
    )
    .await;

    cut.set_blackhole(true);
    await_forgotten(&steady, fickle.guid(), PATIENCE + Duration::from_secs(5)).await;
    assert!(
        fickle.knows(steady.guid()).await,
        "one-sided: the fickle participant never gave up on the steady one"
    );
    assert!(steady.delete_writer(doomed.guid()).await.expect("delete"));
    tokio::time::sleep(TICK * 5).await;
    assert!(
        known_endpoints(&fickle).await.0.contains(&doomed.guid()),
        "while the fickle participant is given up on, nothing can tell it"
    );

    cut.set_blackhole(false);
    await_exactly(
        &steady,
        "the steady participant relearning every endpoint of the fickle one",
        &writer_guids(&writers),
        &reader_guids(&readers),
    )
    .await;
    await_exactly(
        &fickle,
        "the fickle participant forgetting the publication deleted while it was given up on",
        &[kept.guid()],
        &[],
    )
    .await;

    stop(&fickle, fickle_task).await;
    stop(&steady, steady_task).await;
}
