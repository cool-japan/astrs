//! Unit tests for the RTPS writer.
//!
//! Split out of `writer.rs` to keep every file under the 2000-line limit;
//! `use super::*` still resolves to the writer module, so these are the
//! same tests against the same private items.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::behavior::proxy::RESTART_AFTER_HEARTBEATS;
use crate::discovery::qos::{HistoryQos, LifespanQos, ReliabilityQos};
use crate::structure::{EntityKind, GuidPrefix, VendorId};
use std::net::Ipv4Addr;

fn writer_guid() -> Guid {
    Guid::new(
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]),
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
    )
}

fn reader_guid() -> Guid {
    Guid::new(
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [2; 10]),
        EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
    )
}

fn topic() -> TopicKey {
    TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").expect("valid names")
}

fn reliable_writer() -> RtpsWriter {
    RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic())
            .with_qos(WriterQos::services_default())
            .with_datagram_budget(1_400),
    )
}

fn best_effort_writer() -> RtpsWriter {
    RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::sensor_data()))
}

fn proxy(reliable: bool) -> ReaderProxy {
    ReaderProxy::new(
        reader_guid(),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_001)],
        reliable,
    )
}

fn submessages(outbound: &[Outbound]) -> Vec<Submessage<'_>> {
    outbound
        .iter()
        .flat_map(|item| decode_outbound(item).expect("decode"))
        .collect()
}

fn count_data(outbound: &[Outbound]) -> usize {
    submessages(outbound)
        .iter()
        .filter(|submessage| submessage.as_data().is_some())
        .count()
}

#[test]
fn a_writer_with_no_readers_emits_nothing() {
    let mut writer = reliable_writer();
    let now = Instant::now();
    writer.write(vec![1, 2, 3, 4], None, now).unwrap();
    assert!(writer.produce(now).unwrap().is_empty());
    assert_eq!(writer.last_change(), SequenceNumber::FIRST);
}

#[test]
fn sequence_numbers_start_at_one_and_increase() {
    let mut writer = reliable_writer();
    let now = Instant::now();
    assert_eq!(
        writer.write(vec![1], None, now).unwrap(),
        SequenceNumber::new(1)
    );
    assert_eq!(
        writer.write(vec![2], None, now).unwrap(),
        SequenceNumber::new(2)
    );
    assert_eq!(writer.last_change(), SequenceNumber::new(2));
}

#[test]
fn an_empty_history_reports_a_first_sn_above_its_last_sn() {
    let writer = reliable_writer();
    assert_eq!(writer.first_available(), SequenceNumber::FIRST);
    assert_eq!(writer.last_change(), SequenceNumber::ZERO);
    assert!(
        writer.first_available() > writer.last_change(),
        "the standard \"I hold nothing\" heartbeat"
    );
}

#[test]
fn a_matched_reader_receives_every_sample_once() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=3_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    let outbound = writer.produce(now).unwrap();
    assert_eq!(count_data(&outbound), 3);

    // A second produce sends nothing new.
    let again = writer.produce(now).unwrap();
    assert_eq!(count_data(&again), 0);
}

#[test]
fn every_datagram_names_its_destination() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![7], None, now).unwrap();
    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    assert!(
        found
            .iter()
            .any(|submessage| matches!(submessage, Submessage::InfoDestination(_))),
        "a unicast datagram must carry INFO_DST"
    );
}

#[test]
fn a_source_timestamp_becomes_an_info_ts() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer
        .write(vec![1], Some(Time::new(1_700_000_000, 0)), now)
        .unwrap();
    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    assert!(
        found
            .iter()
            .any(|submessage| matches!(submessage, Submessage::InfoTimestamp(_)))
    );
}

#[test]
fn a_reliable_writer_heartbeats_and_a_best_effort_one_does_not() {
    let now = Instant::now();

    let mut reliable = reliable_writer();
    reliable.match_reader(proxy(true));
    reliable.write(vec![1], None, now).unwrap();
    let outbound = reliable.produce(now).unwrap();
    assert!(
        submessages(&outbound)
            .iter()
            .any(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
    );

    let mut best_effort = best_effort_writer();
    best_effort.match_reader(proxy(false));
    best_effort.write(vec![1], None, now).unwrap();
    let outbound = best_effort.produce(now).unwrap();
    assert!(
        !submessages(&outbound)
            .iter()
            .any(|submessage| matches!(submessage, Submessage::Heartbeat(_))),
        "a best-effort writer has no reliability protocol to run"
    );
    assert_eq!(count_data(&outbound), 1, "but it does send the sample");
}

#[test]
fn the_heartbeat_cadence_is_respected() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let start = Instant::now();
    writer.write(vec![1], None, start).unwrap();

    let first = writer.produce(start).unwrap();
    let heartbeats = submessages(&first)
        .iter()
        .filter(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
        .count();
    assert_eq!(heartbeats, 1);

    let soon = start + StdDuration::from_millis(10);
    let second = writer.produce(soon).unwrap();
    assert!(
        !submessages(&second)
            .iter()
            .any(|submessage| matches!(submessage, Submessage::Heartbeat(_))),
        "the period has not elapsed"
    );

    let later = start + DEFAULT_HEARTBEAT_PERIOD + StdDuration::from_millis(1);
    let third = writer.produce(later).unwrap();
    assert!(
        submessages(&third)
            .iter()
            .any(|submessage| matches!(submessage, Submessage::Heartbeat(_)))
    );
}

#[test]
fn the_heartbeat_announces_the_real_window() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=4_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    let outbound = writer.force_heartbeat(now).unwrap();
    let found = submessages(&outbound);
    let heartbeat = found
        .iter()
        .find_map(|submessage| match submessage {
            Submessage::Heartbeat(heartbeat) => Some(heartbeat),
            _ => None,
        })
        .expect("a heartbeat");
    assert_eq!(heartbeat.first_sn, SequenceNumber::new(1));
    assert_eq!(heartbeat.last_sn, SequenceNumber::new(4));
    assert!(!heartbeat.is_final, "the reader has acknowledged nothing");
}

#[test]
fn a_caught_up_reader_gets_a_final_heartbeat() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    writer.produce(now).unwrap();

    let acknack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::new(2)),
        1,
    );
    assert!(writer.on_acknack(reader_guid(), &acknack));
    assert!(writer.is_acknowledged());

    let outbound = writer.force_heartbeat(now).unwrap();
    let found = submessages(&outbound);
    let heartbeat = found
        .iter()
        .find_map(|submessage| match submessage {
            Submessage::Heartbeat(heartbeat) => Some(heartbeat),
            _ => None,
        })
        .expect("a heartbeat");
    assert!(heartbeat.is_final);
}

#[test]
fn a_nacked_sample_is_retransmitted() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=3_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    writer.produce(now).unwrap();
    assert_eq!(count_data(&writer.produce(now).unwrap()), 0);

    // "I have 1; 2 and 3 are missing."
    let set = SequenceNumberSet::from_numbers(
        SequenceNumber::new(2),
        [SequenceNumber::new(2), SequenceNumber::new(3)],
    )
    .unwrap();
    let acknack =
        crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
    assert!(writer.on_acknack(reader_guid(), &acknack));

    let repairs = writer.produce(now).unwrap();
    assert_eq!(count_data(&repairs), 2, "both missing samples come back");
}

#[test]
fn a_stale_acknack_is_ignored() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    writer.produce(now).unwrap();

    let ack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::new(2)),
        5,
    );
    assert!(writer.on_acknack(reader_guid(), &ack));
    let stale = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::from_numbers(SequenceNumber::FIRST, [SequenceNumber::FIRST]).unwrap(),
        5,
    );
    assert!(!writer.on_acknack(reader_guid(), &stale));
    assert_eq!(count_data(&writer.produce(now).unwrap()), 0);
}

#[test]
fn an_acknack_from_an_unmatched_reader_is_ignored() {
    let mut writer = reliable_writer();
    let acknack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::FIRST),
        1,
    );
    assert!(!writer.on_acknack(reader_guid(), &acknack));
}

#[test]
fn an_evicted_sample_becomes_a_gap() {
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            reliability: ReliabilityQos::reliable(),
            history: HistoryQos::keep_last(2),
            ..WriterQos::default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=4_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    // The reader asks for sample 1, which KEEP_LAST 2 has evicted.
    let set =
        SequenceNumberSet::from_numbers(SequenceNumber::FIRST, [SequenceNumber::FIRST]).unwrap();
    let acknack =
        crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
    writer.on_acknack(reader_guid(), &acknack);

    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    let gap = found
        .iter()
        .find_map(|submessage| match submessage {
            Submessage::Gap(gap) => Some(gap),
            _ => None,
        })
        .expect("a GAP for the evicted sample");
    assert!(gap.covers(SequenceNumber::FIRST));
}

#[test]
fn an_expired_sample_is_dropped_and_reported() {
    let origin = Instant::now();
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_all(),
            lifespan: LifespanQos::from_millis(50),
            ..WriterQos::default()
        },
    ));
    writer.write(vec![1], None, origin).unwrap();
    writer
        .write(vec![2], None, origin + StdDuration::from_millis(40))
        .unwrap();

    let expired = writer.expire(origin + StdDuration::from_millis(60));
    assert_eq!(expired, vec![SequenceNumber::FIRST]);
    assert_eq!(writer.cache().len(), 1);
}

#[test]
fn keep_all_reclaims_what_every_reader_acknowledged() {
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_all(),
            ..WriterQos::default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=5_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    assert_eq!(writer.reclaim(), 0, "nothing acknowledged yet");

    let acknack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::new(4)),
        1,
    );
    writer.on_acknack(reader_guid(), &acknack);
    assert_eq!(writer.acked_by_all(), Some(SequenceNumber::new(3)));
    assert_eq!(writer.reclaim(), 3);
    assert_eq!(
        writer.cache().min_sequence_number(),
        Some(SequenceNumber::new(4))
    );
}

#[test]
fn keep_last_never_reclaims() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    let acknack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::new(2)),
        1,
    );
    writer.on_acknack(reader_guid(), &acknack);
    assert_eq!(
        writer.reclaim(),
        0,
        "a KEEP_LAST cache must keep its history for the next late joiner"
    );
}

#[test]
fn a_large_sample_is_fragmented() {
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic())
            .with_qos(WriterQos::services_default())
            .with_fragmentation(1_000, 500)
            .with_datagram_budget(1_400),
    );
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![0xab_u8; 4_000], None, now).unwrap();

    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    let fragments: Vec<_> = found
        .iter()
        .filter_map(|submessage| submessage.as_data_frag())
        .collect();
    let carried: u32 = fragments
        .iter()
        .map(|fragment| u32::from(fragment.fragments_in_submessage))
        .sum();
    assert_eq!(carried, 8, "4000 octets at 500 per fragment");
    assert_eq!(
        fragments.len(),
        4,
        "a 1400-octet datagram holds two 500-octet fragments"
    );
    assert_eq!(fragments[0].sample_size, 4_000);
    assert_eq!(fragments[0].fragment_size, 500);
    assert!(
        found
            .iter()
            .all(|submessage| submessage.as_data().is_none()),
        "a fragmented sample never also goes as a plain DATA"
    );
}

#[test]
fn a_small_sample_is_not_fragmented() {
    let writer = reliable_writer();
    assert!(writer.fragment_plan(100).unwrap().is_none());
    let plan = writer
        .fragment_plan(FRAGMENTATION_THRESHOLD + 1)
        .unwrap()
        .expect("above the threshold");
    assert!(plan.is_needed());
}

#[test]
fn the_datagram_budget_lowers_the_threshold_below_the_policy() {
    // The policy says 64 KiB; a 1400-octet datagram says otherwise, and
    // the physical limit wins.
    let tight = WriterConfig::new(writer_guid(), topic()).with_datagram_budget(1_400);
    assert_eq!(tight.fragmentation_threshold, FRAGMENTATION_THRESHOLD);
    assert_eq!(tight.effective_threshold(), 1_400 - DATAGRAM_OVERHEAD);

    let writer = RtpsWriter::new(tight);
    assert!(writer.fragment_plan(1_000).unwrap().is_none());
    assert!(writer.fragment_plan(2_000).unwrap().is_some());

    // With room to spare the policy is what binds.
    let roomy = WriterConfig::new(writer_guid(), topic())
        .with_datagram_budget(crate::messages::MAX_UDP_PAYLOAD)
        .with_fragmentation(1_000, 500);
    assert_eq!(roomy.effective_threshold(), 1_000);
}

#[test]
fn matching_the_same_reader_twice_keeps_its_state() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    writer.produce(now).unwrap();

    writer.match_reader(proxy(true));
    assert_eq!(writer.matched_reader_count(), 1);
    assert_eq!(
        count_data(&writer.produce(now).unwrap()),
        0,
        "re-matching must not resend everything"
    );
}

#[test]
fn unmatching_stops_delivery() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    assert!(writer.is_matched(reader_guid()));
    assert!(writer.unmatch_reader(reader_guid()));
    assert!(!writer.unmatch_reader(reader_guid()));

    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    assert!(writer.produce(now).unwrap().is_empty());
}

#[test]
fn unmatching_a_participant_removes_all_of_its_readers() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    writer.match_reader(ReaderProxy::new(
        Guid::new(
            reader_guid().prefix,
            EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY),
        ),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_002)],
        true,
    ));
    assert_eq!(writer.matched_reader_count(), 2);
    assert_eq!(writer.unmatch_participant(reader_guid()), 2);
    assert_eq!(writer.matched_reader_count(), 0);
}

#[test]
fn a_reader_with_no_locators_is_skipped_rather_than_failing() {
    let mut writer = reliable_writer();
    writer.match_reader(ReaderProxy::new(reader_guid(), Vec::new(), true));
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    assert!(writer.produce(now).unwrap().is_empty());
}

#[test]
fn a_deactivated_reader_receives_nothing() {
    let mut writer = reliable_writer();
    let mut idle = proxy(true);
    idle.deactivate();
    writer.match_reader(idle);
    let now = Instant::now();
    writer.write(vec![1], None, now).unwrap();
    // `match_reader` on a fresh GUID stores the proxy as given.
    assert_eq!(count_data(&writer.produce(now).unwrap()), 0);
}

#[test]
fn a_best_effort_writer_releases_its_keep_all_history() {
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            reliability: ReliabilityQos::best_effort(),
            history: HistoryQos::keep_all(),
            ..WriterQos::default()
        },
    ));
    writer.match_reader(proxy(false));
    let now = Instant::now();
    for value in 1..=3_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    writer.produce(now).unwrap();
    assert_eq!(
        writer.acked_by_all(),
        None,
        "there is no reliable reader to wait for"
    );
}

#[test]
fn an_announcement_addresses_arbitrary_locators() {
    let mut writer = reliable_writer();
    let now = Instant::now();
    let number = writer.write(vec![1, 2, 3, 4], None, now).unwrap();
    let outbound = writer
        .announce(
            number,
            crate::structure::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
            vec![Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7_400)],
        )
        .unwrap()
        .expect("the sample is held");
    assert!(outbound.is_deliverable());
    let found = decode_outbound(&outbound).unwrap();
    let data = found.iter().find_map(Submessage::as_data).expect("a DATA");
    assert_eq!(
        data.reader_id,
        crate::structure::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER
    );
    assert_eq!(data.writer_sn, number);
}

#[test]
fn announcing_a_sample_that_is_gone_yields_nothing() {
    let writer = reliable_writer();
    assert!(
        writer
            .announce(
                SequenceNumber::new(99),
                EntityId::UNKNOWN,
                vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 1)]
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn announcing_to_nowhere_yields_nothing() {
    let mut writer = reliable_writer();
    let now = Instant::now();
    let number = writer.write(vec![1], None, now).unwrap();
    assert!(
        writer
            .announce(number, EntityId::UNKNOWN, Vec::new())
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_liveliness_assertion_is_a_final_heartbeat_with_the_l_flag() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    let outbound = writer.assert_liveliness(now).unwrap();
    let found = submessages(&outbound);
    let heartbeat = found
        .iter()
        .find_map(|submessage| match submessage {
            Submessage::Heartbeat(heartbeat) => Some(heartbeat),
            _ => None,
        })
        .expect("a heartbeat");
    assert!(heartbeat.liveliness);
    assert!(heartbeat.is_final);
}

#[test]
fn a_nack_frag_requests_the_whole_sample_again() {
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic())
            .with_qos(WriterQos::services_default())
            .with_fragmentation(1_000, 500)
            .with_datagram_budget(1_400),
    );
    writer.match_reader(proxy(true));
    let now = Instant::now();
    let number = writer.write(vec![0xcd_u8; 4_000], None, now).unwrap();
    writer.produce(now).unwrap();
    assert!(writer.produce(now).unwrap().is_empty());

    let missing = crate::structure::FragmentNumberSet::from_numbers(
        crate::structure::FragmentNumber::new(3),
        [crate::structure::FragmentNumber::new(3)],
    )
    .unwrap();
    let nack = NackFrag::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        number,
        missing,
        1,
    );
    assert!(writer.on_nack_frag(reader_guid(), &nack));

    let repairs = writer.produce(now).unwrap();
    let carried: u32 = submessages(&repairs)
        .iter()
        .filter_map(|submessage| submessage.as_data_frag())
        .map(|fragment| u32::from(fragment.fragments_in_submessage))
        .sum();
    assert_eq!(carried, 8, "the whole sample is resent");
}

#[test]
fn a_nack_frag_for_a_sample_that_is_gone_is_ignored() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let missing = crate::structure::FragmentNumberSet::new(crate::structure::FragmentNumber::FIRST);
    let nack = NackFrag::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumber::new(42),
        missing,
        1,
    );
    assert!(!writer.on_nack_frag(reader_guid(), &nack));
}

#[test]
fn every_datagram_stays_within_the_budget() {
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic())
            .with_qos(WriterQos {
                history: HistoryQos::keep_all(),
                ..WriterQos::services_default()
            })
            .with_datagram_budget(600),
    );
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for _ in 0..20 {
        writer.write(vec![0x5a_u8; 100], None, now).unwrap();
    }
    let outbound = writer.produce(now).unwrap();
    assert!(
        outbound.len() > 1,
        "twenty samples cannot fit in one datagram"
    );
    for item in &outbound {
        assert!(
            item.len() <= 600,
            "a datagram of {} octets exceeds the budget",
            item.len()
        );
    }
    assert_eq!(count_data(&outbound), 20, "and every sample still went");
}

#[test]
fn a_volatile_writer_gives_a_late_joiner_nothing() {
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            reliability: ReliabilityQos::reliable(),
            durability: crate::discovery::qos::DurabilityQos::volatile(),
            history: HistoryQos::keep_last(10),
            ..WriterQos::default()
        },
    ));
    let now = Instant::now();
    for value in 1..=3_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    // The reader arrives after the fact.
    writer.match_reader(proxy(true));
    assert_eq!(
        count_data(&writer.produce(now).unwrap()),
        0,
        "VOLATILE means the late joiner missed them"
    );
    // …but everything written from now on does arrive.
    writer.write(vec![4; 4], None, now).unwrap();
    assert_eq!(count_data(&writer.produce(now).unwrap()), 1);
}

#[test]
fn a_transient_local_writer_replays_its_history_to_a_late_joiner() {
    let mut writer =
        RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::latched(10)));
    let now = Instant::now();
    for value in 1..=3_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    writer.match_reader(proxy(true));
    assert_eq!(
        count_data(&writer.produce(now).unwrap()),
        3,
        "TRANSIENT_LOCAL replays everything the history still holds"
    );
}

#[test]
fn a_scattered_gap_names_only_the_numbers_that_are_gone() {
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            reliability: ReliabilityQos::reliable(),
            history: HistoryQos::keep_all(),
            ..WriterQos::default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for value in 1..=9_u8 {
        writer.write(vec![value; 4], None, now).unwrap();
    }
    // Drop 3 and 9 from the history, keeping 4..=8.
    assert!(writer.forget(SequenceNumber::new(3)));
    assert!(writer.forget(SequenceNumber::new(9)));

    // The reader has 1 and 2 and asks for everything from 3.
    let set =
        SequenceNumberSet::from_numbers(SequenceNumber::new(3), (3..=9).map(SequenceNumber::new))
            .unwrap();
    let acknack =
        crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, 1);
    writer.on_acknack(reader_guid(), &acknack);

    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    let gap = found
        .iter()
        .find_map(|submessage| match submessage {
            Submessage::Gap(gap) => Some(gap),
            _ => None,
        })
        .expect("a GAP");
    let irrelevant: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
    assert_eq!(
        irrelevant,
        vec![3, 9],
        "the samples between the holes are still held and must not be gapped"
    );
    assert!(!gap.covers(SequenceNumber::new(5)));
    assert_eq!(count_data(&outbound), 5, "4..=8 are still there and go out");
}

#[test]
fn a_disposal_goes_out_as_a_key_with_a_status_info() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    let gone = Guid::new(
        reader_guid().prefix,
        EntityId::user_defined(9, EntityKind::USER_READER_NO_KEY),
    );
    writer.dispose(gone, now).expect("dispose");

    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    let data = found
        .iter()
        .find_map(Submessage::as_data)
        .expect("a DATA carrying the disposal");

    assert!(
        matches!(data.payload, DataPayload::Key(_)),
        "a disposal names the instance, it does not carry a value"
    );
    assert!(data.flags().has(crate::messages::flags::KEY));
    let qos = data.inline_qos.as_ref().expect("PID_STATUS_INFO");
    let status = qos
        .get_by_base(astrs_cdr::pid::STATUS_INFO)
        .expect("the status parameter");
    assert_eq!(
        ChangeKind::from_status_info([
            status.value[0],
            status.value[1],
            status.value[2],
            status.value[3],
        ]),
        ChangeKind::NotAliveDisposed
    );
    assert_eq!(
        status.value.as_ref(),
        &[0, 0, 0, 3],
        "a deleted entity is disposed and unregistered, both flags set (§9.6.3.9)"
    );

    // …and the key octets are the GUID, at the offset a receiver reads.
    let key = data.payload.payload().expect("key octets").as_slice();
    assert_eq!(
        &key[astrs_cdr::ENCAPSULATION_HEADER_LEN..],
        &gone.to_bytes()
    );
}

#[test]
fn a_live_sample_carries_no_status_info() {
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.write(vec![1, 2, 3, 4], None, now).unwrap();
    let outbound = writer.produce(now).unwrap();
    let found = submessages(&outbound);
    let data = found.iter().find_map(Submessage::as_data).expect("a DATA");
    assert!(data.inline_qos.is_none());
}

#[test]
fn the_writer_reports_its_own_configuration() {
    let writer = reliable_writer();
    assert_eq!(writer.guid(), writer_guid());
    assert_eq!(writer.topic().topic_name, "rt/chatter");
    assert!(writer.qos().is_reliable());
    assert!(writer.is_reliable());
    assert_eq!(writer.config().datagram_budget, 1_400);
    assert_eq!(writer.matched_readers().count(), 0);
}

// ---------------------------------------------------------------------------
// Retirements: held until every matched reader has them, then swept
// ---------------------------------------------------------------------------

/// An instance of a keyed topic, by seed.
fn instance(seed: u8) -> InstanceHandle {
    InstanceHandle::new([seed; crate::behavior::cache::INSTANCE_HANDLE_LEN])
}

/// A GUID whose octets are [`instance`]`(seed)`'s, so that `dispose` files
/// its retirement under that instance.
fn keyed(seed: u8) -> Guid {
    Guid::from_slice(instance(seed).as_bytes()).expect("sixteen octets are a GUID")
}

/// Acknowledge everything through `through` on behalf of [`reader_guid`].
fn acknowledge_through(writer: &mut RtpsWriter, through: i64, count: i32) {
    let acknack = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::new(through + 1)),
        count,
    );
    assert!(writer.on_acknack(reader_guid(), &acknack));
}

/// Every change held, as (sequence number, instance), oldest first.
fn held(writer: &RtpsWriter) -> Vec<(i64, InstanceHandle)> {
    writer
        .cache()
        .iter()
        .map(|change| (change.sequence_number.value(), change.instance))
        .collect()
}

#[test]
fn a_retirement_takes_the_older_changes_of_its_instance_with_it() {
    // KEEP_LAST 3: a retirement does not evict its instance's announcements
    // the way it does under the builtin writers' depth of one, so the sweep
    // has to take them itself — they are owed to nobody either, and left
    // behind they would be the same unbounded leak one layer down.
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_last(3),
            ..WriterQos::services_default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for seed in [1, 1, 2] {
        writer
            .write_change(vec![seed; 4], None, ChangeKind::Alive, instance(seed), now)
            .unwrap(); // 1, 2 of instance 1; 3 of instance 2
    }
    let retired = writer.dispose(keyed(1), now).unwrap(); // 4
    assert_eq!(retired, SequenceNumber::new(4));
    writer.produce(now).unwrap();
    assert_eq!(
        writer.cache().len(),
        4,
        "nothing goes before it is acknowledged"
    );

    acknowledge_through(&mut writer, 4, 1);
    assert_eq!(
        held(&writer),
        vec![(3, instance(2))],
        "instance 1's announcements went with its retirement; instance 2 stays"
    );
    assert!(writer.retired.is_empty());
}

#[test]
fn a_change_written_after_a_retirement_outlives_it() {
    // An instance written again after its retirement is alive again: the
    // sweep takes the retirement and what came before it, never what came
    // after.
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_last(3),
            ..WriterQos::services_default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer
        .write_change(vec![1; 4], None, ChangeKind::Alive, instance(1), now)
        .unwrap(); // 1
    writer.dispose(keyed(1), now).unwrap(); // 2
    writer
        .write_change(vec![1; 4], None, ChangeKind::Alive, instance(1), now)
        .unwrap(); // 3
    writer.produce(now).unwrap();
    acknowledge_through(&mut writer, 3, 1);
    assert_eq!(held(&writer), vec![(3, instance(1))]);
}

#[test]
fn a_retirement_that_leaves_by_another_door_is_forgotten() {
    // Evicted, forgotten or expired, a retirement is no longer held — and
    // the writer's note of it goes too, so that the set it keeps is bounded
    // by the history itself.
    let origin = Instant::now();
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_last(1),
            lifespan: LifespanQos::from_millis(50),
            ..WriterQos::services_default()
        },
    ));
    writer.match_reader(proxy(true));

    let first = writer.dispose(keyed(1), origin).unwrap();
    let second = writer.dispose(keyed(1), origin).unwrap();
    assert!(!writer.cache().contains(first), "KEEP_LAST 1 evicted it");
    assert_eq!(
        writer.retired.iter().copied().collect::<Vec<_>>(),
        vec![second]
    );

    assert!(writer.forget(second));
    assert!(writer.retired.is_empty());

    let third = writer.dispose(keyed(2), origin).unwrap();
    assert_eq!(
        writer.retired.iter().copied().collect::<Vec<_>>(),
        vec![third]
    );
    assert_eq!(
        writer.expire(origin + StdDuration::from_millis(60)),
        vec![third]
    );
    assert!(writer.retired.is_empty());
    assert!(writer.cache().is_empty());
}

#[test]
fn a_plain_disposal_is_never_swept() {
    // Only `dispose` retires. A disposal written through the generic path
    // leaves its instance registered, and a TRANSIENT_LOCAL writer owes it to
    // every late joiner for as long as the history policy keeps it.
    let mut writer = reliable_writer();
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer
        .write_change(
            vec![0x00, 0x01, 0x00, 0x00, 7, 7, 7, 7],
            None,
            ChangeKind::NotAliveDisposed,
            instance(7),
            now,
        )
        .unwrap();
    writer.produce(now).unwrap();
    acknowledge_through(&mut writer, 1, 1);
    writer.produce(now).unwrap();
    assert_eq!(held(&writer), vec![(1, instance(7))]);
    assert!(writer.retired.is_empty());
}

#[test]
fn a_best_effort_late_joiner_is_served_past_a_run_of_holes() {
    // The SPDP writer's shape: best-effort, TRANSIENT_LOCAL, KEEP_LAST 1.
    // Three hundred rewrites leave one sample and 299 numbers that are gone,
    // more than one `produce` window. A best-effort reader is never sent a
    // GAP, so unless its watermark moves past the holes it was served, every
    // call serves it the same window of nothing and the one sample it is
    // owed never goes out.
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::builtin_spdp()),
    );
    let now = Instant::now();
    for value in 1..=300_u16 {
        writer
            .write(value.to_le_bytes().repeat(2), None, now)
            .unwrap();
    }
    writer.match_reader(proxy(false));
    let mut sent = Vec::new();
    for _ in 0..3 {
        let outbound = writer.produce(now).unwrap();
        sent.extend(
            submessages(&outbound)
                .iter()
                .filter_map(Submessage::as_data)
                .map(|data| data.writer_sn.value()),
        );
    }
    assert_eq!(sent, vec![300], "the newest sample goes out, once");
    assert_eq!(
        writer
            .matched_readers()
            .next()
            .map(ReaderProxy::acked_through),
        Some(SequenceNumber::new(300))
    );
}

/// One create/delete cycle on a SEDP-shaped writer, acknowledged by
/// [`reader_guid`] as soon as it has been produced.
fn churn(writer: &mut RtpsWriter, key: u32, count: &mut i32, now: Instant) {
    let guid = Guid::new(
        GuidPrefix::new([9; 12]),
        EntityId::user_defined(key, EntityKind::USER_WRITER_NO_KEY),
    );
    writer
        .write_change(
            vec![0x00, 0x03, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00],
            None,
            ChangeKind::Alive,
            InstanceHandle::new(guid.to_bytes()),
            now,
        )
        .unwrap();
    writer.dispose(guid, now).unwrap();
    writer.produce(now).unwrap();
    *count += 1;
    let through = writer.last_change().value();
    acknowledge_through(writer, through, *count);
}

#[test]
fn a_best_effort_late_joiner_does_not_hold_retirements_back() {
    // The sweep waits for the lowest watermark of every matched reader,
    // best-effort ones included. Two hundred acknowledged create/delete
    // cycles leave four hundred numbers and nothing held, so a best-effort
    // reader matched then is served nothing but holes; were it stuck there,
    // no retirement would ever be swept again, for anyone.
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::builtin_sedp()),
    );
    writer.match_reader(proxy(true));
    let now = Instant::now();
    let mut count = 0;
    for key in 1..=200 {
        churn(&mut writer, key, &mut count, now);
    }
    assert!(writer.cache().is_empty(), "every retirement was swept");

    let late = ReaderProxy::new(
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [3; 10]),
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
        ),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_003)],
        false,
    );
    let late_guid = late.guid();
    writer.match_reader(late);
    for _ in 0..3 {
        writer.produce(now).unwrap();
    }
    assert_eq!(
        writer
            .matched_readers()
            .find(|proxy| proxy.guid() == late_guid)
            .map(ReaderProxy::acked_through),
        Some(writer.last_change()),
        "served past every hole"
    );

    for key in 201..=400 {
        churn(&mut writer, key, &mut count, now);
    }
    assert!(
        writer.cache().is_empty(),
        "and the sweep goes on: {} changes held",
        writer.cache().len()
    );
}

// ---------------------------------------------------------------------------
// Holes: every run is one GAP, pushed once, and repaired whole
// ---------------------------------------------------------------------------

/// Every `GAP` a batch carries, in order.
fn gaps(outbound: &[Outbound]) -> Vec<Gap> {
    submessages(outbound)
        .iter()
        .filter_map(|submessage| submessage.as_gap().cloned())
        .collect()
}

/// Every `DATA` sequence number a batch carries, in order.
fn data_numbers(outbound: &[Outbound]) -> Vec<i64> {
    submessages(outbound)
        .iter()
        .filter_map(Submessage::as_data)
        .map(|data| data.writer_sn.value())
        .collect()
}

/// Every `HEARTBEAT` a batch carries, in order.
fn heartbeats(outbound: &[Outbound]) -> Vec<Heartbeat> {
    submessages(outbound)
        .iter()
        .filter_map(|submessage| submessage.as_heartbeat().cloned())
        .collect()
}

/// The `ACKNACK` in which [`reader_guid`] says it has everything below
/// `base` and lacks `missing`.
fn nack(writer: &mut RtpsWriter, base: i64, missing: impl IntoIterator<Item = i64>, count: i32) {
    let set = SequenceNumberSet::from_numbers(
        SequenceNumber::new(base),
        missing.into_iter().map(SequenceNumber::new),
    )
    .unwrap();
    let acknack =
        crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, count);
    assert!(writer.on_acknack(reader_guid(), &acknack));
}

/// A SEDP-shaped writer — reliable, `TRANSIENT_LOCAL`, `KEEP_LAST 1` — that
/// holds exactly two changes, 1 and `last`: instance 1 written once, then
/// instance 2 written until its number reaches `last`, each write evicting
/// the one before. Every number between is a hole.
fn two_changes_far_apart(last: i64) -> RtpsWriter {
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::builtin_sedp()),
    );
    let now = Instant::now();
    writer
        .write_change(
            vec![0, 1, 0, 0, 1, 1, 1, 1],
            None,
            ChangeKind::Alive,
            instance(1),
            now,
        )
        .unwrap();
    for _ in 2..=last {
        writer
            .write_change(
                vec![0, 1, 0, 0, 2, 2, 2, 2],
                None,
                ChangeKind::Alive,
                instance(2),
                now,
            )
            .unwrap();
    }
    assert_eq!(
        held(&writer)
            .iter()
            .map(|(number, _)| *number)
            .collect::<Vec<_>>(),
        vec![1, last]
    );
    writer
}

#[test]
fn a_run_of_holes_is_one_gap_however_long() {
    // A hundred thousand numbers between the two changes the history holds:
    // what a SEDP writer is left with after a day of endpoint churn. A late
    // joiner is served both and one GAP for everything between, in one call.
    // Walked number by number — 256 to a call, and nothing more until an
    // ACKNACK moved the reader on — the same replay took 390 heartbeat
    // periods.
    const LAST: i64 = 100_001;
    let mut writer = two_changes_far_apart(LAST);
    writer.match_reader(proxy(true));
    let now = Instant::now();

    let outbound = writer.produce(now).unwrap();
    assert_eq!(data_numbers(&outbound), vec![1, LAST]);
    let found = gaps(&outbound);
    assert_eq!(found.len(), 1, "one GAP for the whole run: {found:?}");
    assert_eq!(found[0].gap_start, SequenceNumber::new(2));
    assert_eq!(
        found[0].contiguous_end(),
        SequenceNumber::new(LAST),
        "the run ends where the next held change begins"
    );
    assert!(found[0].gap_list.is_empty());
    assert_eq!(
        writer
            .matched_readers()
            .next()
            .map(ReaderProxy::highest_sent),
        Some(SequenceNumber::new(LAST)),
        "served through the end of the history"
    );

    let again = writer.produce(now).unwrap();
    assert!(data_numbers(&again).is_empty());
    assert!(
        gaps(&again).is_empty(),
        "a GAP is pushed once, like a sample; what the reader lost it asks for"
    );
}

#[test]
fn a_nack_inside_a_run_is_answered_with_the_whole_run_and_a_prompt() {
    // The reader lost the replay — a late joiner that had not yet heard the
    // writer's participant drops it whole — and can only ask for what its
    // bitmap reaches: 1..=256.
    const LAST: i64 = 100_001;
    let mut writer = two_changes_far_apart(LAST);
    writer.match_reader(proxy(true));
    let now = Instant::now();
    writer.produce(now).unwrap();

    nack(&mut writer, 1, 1..=256, 1);
    let repair = writer.produce(now).unwrap();
    assert_eq!(data_numbers(&repair), vec![1]);
    let found = gaps(&repair);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].gap_start, found[0].contiguous_end()),
        (SequenceNumber::new(2), SequenceNumber::new(LAST)),
        "the whole run through the next held change, not the 255 numbers of it the bitmap named"
    );
    let prompts = heartbeats(&repair);
    assert_eq!(
        prompts.len(),
        1,
        "the repair stopped short of the frontier, so the reader is asked what else it lacks"
    );
    assert!(!prompts[0].is_final, "and it must answer");
    assert_eq!(
        (prompts[0].first_sn, prompts[0].last_sn),
        (SequenceNumber::FIRST, SequenceNumber::new(LAST))
    );

    // Its answer names the one change left; that repair reaches the frontier.
    nack(&mut writer, LAST, [LAST], 2);
    let last = writer.produce(now).unwrap();
    assert_eq!(data_numbers(&last), vec![LAST]);
    assert!(gaps(&last).is_empty());
    assert!(
        heartbeats(&last).is_empty(),
        "nothing is left to ask about, and the cadence is not due"
    );
}

#[test]
fn a_prompt_never_moves_the_cadence() {
    // The prompt spends a Count_t, so the reader takes it as fresh, and it
    // leaves the cadence alone: the next periodic heartbeat is still due one
    // period after the last one, and counts above the prompt.
    const LAST: i64 = 1_000;
    let mut writer = two_changes_far_apart(LAST);
    writer.match_reader(proxy(true));
    let origin = Instant::now();
    let first = heartbeats(&writer.produce(origin).unwrap());
    assert_eq!(first.len(), 1, "the first cadence heartbeat");

    nack(&mut writer, 1, 1..=256, 1);
    let prompt = heartbeats(&writer.produce(origin).unwrap());
    assert_eq!(prompt.len(), 1);
    assert!(prompt[0].count > first[0].count);

    let period = writer.config().heartbeat_period;
    assert!(
        heartbeats(&writer.produce(origin + period / 2).unwrap()).is_empty(),
        "the prompt is not a cadence heartbeat, and did not make one due"
    );
    let cadence = heartbeats(&writer.produce(origin + period).unwrap());
    assert_eq!(cadence.len(), 1);
    assert!(cadence[0].count > prompt[0].count);
}

#[test]
fn a_repair_above_the_frontier_skips_nothing_below_it() {
    // A repair can name a number far above what the reader has been served:
    // a NACK_FRAG may. It goes out, but the frontier stays where the push
    // reached. Moved to the repair, it would jump every sample between, and
    // the reader would get those only by asking for them.
    let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
        WriterQos {
            history: HistoryQos::keep_all(),
            ..WriterQos::services_default()
        },
    ));
    writer.match_reader(proxy(true));
    let now = Instant::now();
    for _ in 0..1_000 {
        writer.write(vec![0x5a_u8; 8], None, now).unwrap();
    }
    let mut sent = data_numbers(&writer.produce(now).unwrap());
    assert_eq!(sent.len(), MAX_SAMPLES_PER_PRODUCE);

    let missing = crate::structure::FragmentNumberSet::from_numbers(
        crate::structure::FragmentNumber::FIRST,
        [crate::structure::FragmentNumber::FIRST],
    )
    .unwrap();
    let nack_frag = NackFrag::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumber::new(1_000),
        missing,
        1,
    );
    assert!(writer.on_nack_frag(reader_guid(), &nack_frag));
    for _ in 0..8 {
        sent.extend(data_numbers(&writer.produce(now).unwrap()));
    }
    let distinct: BTreeSet<i64> = sent.iter().copied().collect();
    assert_eq!(
        distinct,
        (1..=1_000).collect::<BTreeSet<i64>>(),
        "every sample went out"
    );
}

/// Every number a batch of `GAP`s declares irrelevant, with repeats.
fn named_by(found: &[Gap]) -> Vec<i64> {
    found
        .iter()
        .flat_map(|gap| {
            gap.irrelevant()
                .map(SequenceNumber::value)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The numbers of `holes`, ascending.
fn numbers_of(holes: &[(i64, i64)]) -> Vec<i64> {
    holes
        .iter()
        .flat_map(|(first, last)| *first..=*last)
        .collect()
}

/// [`gaps_naming`] over holes given as plain numbers.
fn gaps_for_holes(holes: &[(i64, i64)]) -> Vec<Gap> {
    let runs: Vec<(SequenceNumber, SequenceNumber)> = holes
        .iter()
        .map(|(first, last)| (SequenceNumber::new(*first), SequenceNumber::new(*last)))
        .collect();
    gaps_naming(&runs, reader_guid().entity_id, writer_guid().entity_id).unwrap()
}

#[test]
fn a_gap_spends_its_run_on_one_hole_and_its_bitmap_on_the_next() {
    // Scattered holes ride in one bitmap.
    let found = gaps_for_holes(&[(5, 9), (11, 11), (13, 20)]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].gap_start, SequenceNumber::new(5));
    assert_eq!(found[0].contiguous_end(), SequenceNumber::new(10));
    assert_eq!(named_by(&found), numbers_of(&[(5, 9), (11, 11), (13, 20)]));

    // A hole the bitmap cannot finish is where the next GAP starts.
    let found = gaps_for_holes(&[(2, 2), (4, 1_000)]);
    assert_eq!(found.len(), 2);
    assert_eq!(
        (found[0].gap_start, found[0].contiguous_end()),
        (SequenceNumber::new(2), SequenceNumber::new(3))
    );
    assert_eq!(found[0].gap_list.num_bits(), MAX_SET_BITS);
    assert_eq!(
        (found[1].gap_start, found[1].contiguous_end()),
        (SequenceNumber::new(259), SequenceNumber::new(1_001))
    );
    assert_eq!(named_by(&found), numbers_of(&[(2, 2), (4, 1_000)]));

    // A hole out of the bitmap's reach gets a GAP of its own.
    let found = gaps_for_holes(&[(2, 2), (300, 300)]);
    assert_eq!(found.len(), 2);
    assert_eq!(named_by(&found), vec![2, 300]);
}

proptest::proptest! {
    #[test]
    fn gaps_name_exactly_the_holes_they_are_given(
        shape in proptest::collection::vec((0_i64..400, 1_i64..600), 1..20)
    ) {
        let mut holes = Vec::new();
        let mut next = 1_i64;
        for (skip, len) in shape {
            let first = next + skip;
            let last = first + len - 1;
            holes.push((first, last));
            next = last + 2;
        }
        let found = gaps_for_holes(&holes);
        for gap in &found {
            proptest::prop_assert!(gap.validate().is_ok());
        }
        proptest::prop_assert!(found.len() <= holes.len() + numbers_of(&holes).len() / 256);
        proptest::prop_assert_eq!(named_by(&found), numbers_of(&holes));
    }

    #[test]
    fn every_number_is_served_exactly_once(
        kept in proptest::collection::vec(proptest::bool::weighted(0.3), 1..1_500),
        reliable in proptest::bool::ANY,
    ) {
        // Any history — held changes scattered among holes, a run at the
        // start, the end, or both — is served once: each held change as one
        // DATA, each hole inside one GAP (none to a best-effort reader), and
        // in as few calls as the sample budget allows.
        let mut writer = RtpsWriter::new(WriterConfig::new(writer_guid(), topic()).with_qos(
            WriterQos {
                reliability: if reliable {
                    ReliabilityQos::reliable()
                } else {
                    ReliabilityQos::best_effort()
                },
                history: HistoryQos::keep_all(),
                ..WriterQos::default()
            },
        ));
        writer.match_reader(proxy(reliable));
        let now = Instant::now();
        for _ in &kept {
            writer.write(vec![0x11_u8; 4], None, now).unwrap();
        }
        let mut held_numbers = Vec::new();
        for (index, keep) in kept.iter().enumerate() {
            let number = i64::try_from(index).unwrap() + 1;
            if *keep {
                held_numbers.push(number);
            } else {
                writer.forget(SequenceNumber::new(number));
            }
        }
        let last = i64::try_from(kept.len()).unwrap();

        let mut sent = Vec::new();
        let mut gapped = Vec::new();
        let mut calls = 0;
        for _ in 0..64 {
            let outbound = writer.produce(now).unwrap();
            let data = data_numbers(&outbound);
            let found = gaps(&outbound);
            if data.is_empty() && found.is_empty() {
                break;
            }
            calls += 1;
            sent.extend(data);
            gapped.extend(named_by(&found));
        }
        sent.sort_unstable();
        gapped.sort_unstable();
        let holes: Vec<i64> = (1..=last).filter(|number| !held_numbers.contains(number)).collect();
        proptest::prop_assert_eq!(sent, held_numbers.clone());
        if reliable {
            proptest::prop_assert_eq!(gapped, holes);
        } else {
            proptest::prop_assert!(gapped.is_empty());
        }
        proptest::prop_assert!(calls <= held_numbers.len().div_ceil(MAX_SAMPLES_PER_PRODUCE) + 1);
        let proxy = writer.matched_readers().next().unwrap();
        proptest::prop_assert_eq!(proxy.highest_sent(), SequenceNumber::new(last));
        if !reliable {
            proptest::prop_assert_eq!(proxy.acked_through(), SequenceNumber::new(last));
        }
    }
}

// ---------------------------------------------------------------------------
// Readers that forgot, and readers that lapsed
// ---------------------------------------------------------------------------

/// A SEDP-shaped writer — reliable, `TRANSIENT_LOCAL`, `KEEP_LAST 1` — with
/// [`reader_guid`] matched, holding the announcements of instances 1 and 2,
/// both acknowledged with count 1.
fn sedp_writer_with_two_acknowledged(now: Instant) -> RtpsWriter {
    let mut writer = RtpsWriter::new(
        WriterConfig::new(writer_guid(), topic()).with_qos(WriterQos::builtin_sedp()),
    );
    writer.match_reader(proxy(true));
    for seed in [1, 2] {
        writer
            .write_change(vec![seed; 4], None, ChangeKind::Alive, instance(seed), now)
            .unwrap();
    }
    writer.produce(now).unwrap();
    acknowledge_through(&mut writer, 2, 1);
    writer
}

/// An `ACKNACK` from [`reader_guid`], applied or not: whether it was.
fn offer_acknack(
    writer: &mut RtpsWriter,
    base: i64,
    missing: impl IntoIterator<Item = i64>,
    count: i32,
) -> bool {
    let set = SequenceNumberSet::from_numbers(
        SequenceNumber::new(base),
        missing.into_iter().map(SequenceNumber::new),
    )
    .unwrap();
    let acknack =
        crate::messages::AckNack::new(reader_guid().entity_id, writer_guid().entity_id, set, count);
    writer.on_acknack(reader_guid(), &acknack)
}

#[test]
fn a_reader_that_forgot_is_served_the_whole_history_again() {
    // The reader's participant gave up on this writer's and wired it up
    // again: its fresh proxy counts on from the old one (see
    // `RtpsReader::match_writer`) and has received nothing.
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    assert!(writer.produce(now).unwrap().is_empty(), "nothing is owed");

    assert!(offer_acknack(&mut writer, 1, [1, 2], 2));
    let replay = writer.produce(now).unwrap();
    assert_eq!(
        data_numbers(&replay),
        vec![1, 2],
        "everything the history holds, as to a late joiner"
    );
    assert!(!writer.is_acknowledged(), "until the reader says it has it");
    acknowledge_through(&mut writer, 2, 3);
    assert!(writer.is_acknowledged());
    assert!(writer.produce(now).unwrap().is_empty());
}

#[test]
fn only_the_periodic_heartbeat_counts_towards_hearing_a_restarted_count() {
    // A reader counting from one again — another stack rejoining — is heard
    // once it has been silent for RESTART_AFTER_HEARTBEATS cadence
    // heartbeats. A liveliness assertion comes at whatever rate the
    // application asserts, and says nothing about how long that is.
    let origin = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(origin);
    for _ in 0..RESTART_AFTER_HEARTBEATS * 3 {
        writer.assert_liveliness(origin).unwrap();
    }
    assert!(
        !offer_acknack(&mut writer, 1, [1, 2], 1),
        "stale, as far as the writer can tell"
    );

    let period = writer.config().heartbeat_period;
    let mut now = origin;
    for beat in 1..=RESTART_AFTER_HEARTBEATS {
        now += period;
        let outbound = writer.produce(now).unwrap();
        assert_eq!(heartbeats(&outbound).len(), 1, "one cadence heartbeat");
        let heard = offer_acknack(&mut writer, 1, [1, 2], 1);
        assert_eq!(
            heard,
            beat == RESTART_AFTER_HEARTBEATS,
            "after {beat} unanswered heartbeats"
        );
    }
    assert_eq!(data_numbers(&writer.produce(now).unwrap()), vec![1, 2]);
    assert!(
        offer_acknack(&mut writer, 3, [], 2),
        "and it counts on from where it restarted"
    );
}

#[test]
fn a_lapsed_reader_holds_the_retirements_it_is_owed_until_it_is_matched_again() {
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    assert_eq!(
        writer.lapse_participant(reader_guid(), now + StdDuration::from_secs(60)),
        1
    );
    assert_eq!(writer.matched_reader_count(), 0, "unmatched all the same");

    // Instance 1 retired while the reader is unmatched: nobody matched is
    // owed it, but the reader that may still hold instance 1 is.
    let retired = writer.dispose(keyed(1), now).unwrap(); // 3
    assert!(writer.produce(now).unwrap().is_empty());
    assert_eq!(
        held(&writer),
        vec![(2, instance(2)), (retired.value(), instance(1))]
    );

    // Rediscovered: a new proxy, replayed the history with the retirement.
    writer.match_reader(proxy(true));
    let replay = writer.produce(now).unwrap();
    assert_eq!(data_numbers(&replay), vec![2, retired.value()]);
    acknowledge_through(&mut writer, retired.value(), 1);
    assert_eq!(
        held(&writer),
        vec![(2, instance(2))],
        "once the reader has it, the retirement leaves"
    );
}

#[test]
fn a_lapsed_reader_is_owed_only_what_it_had_not_acknowledged() {
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    writer.dispose(keyed(1), now).unwrap(); // 3
    writer.produce(now).unwrap();
    acknowledge_through(&mut writer, 3, 2);
    assert_eq!(held(&writer), vec![(2, instance(2))], "the reader has it");

    writer.lapse_participant(reader_guid(), now + StdDuration::from_secs(60));
    writer
        .write_change(vec![3; 4], None, ChangeKind::Alive, instance(3), now)
        .unwrap(); // 4
    let retired = writer.dispose(keyed(3), now).unwrap(); // 5
    writer.produce(now).unwrap();
    assert_eq!(
        held(&writer),
        vec![(2, instance(2)), (retired.value(), instance(3))],
        "the retirement written since the reader lapsed is held for it; \
         instance 3's announcement went with it, replaced"
    );
}

#[test]
fn a_lapse_holds_only_until_its_deadline() {
    let now = Instant::now();
    let deadline = now + StdDuration::from_secs(10);
    let mut writer = sedp_writer_with_two_acknowledged(now);
    writer.lapse_participant(reader_guid(), deadline);
    let retired = writer.dispose(keyed(1), now).unwrap();
    writer.produce(now).unwrap();

    assert_eq!(
        writer.forget_lapses_due(deadline - StdDuration::from_millis(1)),
        0
    );
    assert!(writer.cache().contains(retired), "still owed");
    assert_eq!(writer.forget_lapses_due(deadline), 1);
    assert_eq!(
        held(&writer),
        vec![(2, instance(2))],
        "the deadline passed: owed to nobody, and swept at once"
    );
    assert!(writer.retired.is_empty());
}

#[test]
fn a_lapsed_reader_that_departs_or_is_unmatched_is_owed_nothing() {
    for departs in [true, false] {
        let now = Instant::now();
        let mut writer = sedp_writer_with_two_acknowledged(now);
        writer.lapse_participant(reader_guid(), now + StdDuration::from_secs(60));
        let retired = writer.dispose(keyed(1), now).unwrap();
        writer.produce(now).unwrap();
        assert!(writer.cache().contains(retired));
        if departs {
            assert_eq!(writer.forget_lapsed_participant(reader_guid()), 1);
        } else {
            assert!(
                !writer.unmatch_reader(reader_guid()),
                "it was not matched, only remembered"
            );
        }
        assert_eq!(held(&writer), vec![(2, instance(2))], "departs: {departs}");
    }
}

#[test]
fn unmatching_a_participant_leaves_its_lapsed_readers_alone() {
    // The lease path lapses the SEDP writers' readers and then unwires the
    // participant as a whole; that second step must not undo the first.
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    writer.lapse_participant(reader_guid(), now + StdDuration::from_secs(60));
    assert_eq!(writer.unmatch_participant(reader_guid()), 0);
    let retired = writer.dispose(keyed(1), now).unwrap();
    writer.produce(now).unwrap();
    assert!(writer.cache().contains(retired));
}

#[test]
fn a_full_lapse_table_forgets_the_record_that_expires_first() {
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    // One reader per participant, each lapsing later than the one before.
    let reader_of = |index: usize| {
        let [high, low] = u16::try_from(index).unwrap().to_be_bytes();
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [high, low, 9, 9, 9, 9, 9, 9, 9, 9]),
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
        )
    };
    for index in 0..MAX_LAPSED_READERS {
        let reader = reader_of(index);
        writer.match_reader(ReaderProxy::new(reader, Vec::new(), true));
        let until = now + StdDuration::from_secs(10_000 + u64::try_from(index).unwrap());
        assert_eq!(writer.lapse_participant(reader, until), 1);
    }
    assert_eq!(writer.lapsed.len(), MAX_LAPSED_READERS);

    writer.lapse_participant(reader_guid(), now + StdDuration::from_secs(1));
    assert_eq!(writer.lapsed.len(), MAX_LAPSED_READERS, "bounded");
    assert!(writer.lapsed.contains_key(&reader_guid()));
    assert!(
        !writer.lapsed.contains_key(&reader_of(0)),
        "the record that would have expired first made room"
    );
    assert!(writer.lapsed.contains_key(&reader_of(1)));
}

#[test]
fn an_acknack_with_a_base_of_zero_moves_nothing_and_breaks_nothing() {
    // §8.3.7.1.3 wants a positive base, and decoding does not check it. Read
    // as a reader that forgot, a base of zero would wind the watermark to
    // minus one, and the next produce would plan a GAP starting at zero —
    // which fails validation and takes `produce`, and with it the
    // participant's whole cadence, down with an error. A peer must not be
    // able to do that with one datagram.
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    let invalid = crate::messages::AckNack::new(
        reader_guid().entity_id,
        writer_guid().entity_id,
        SequenceNumberSet::new(SequenceNumber::ZERO),
        2,
    );
    assert!(
        writer.on_acknack(reader_guid(), &invalid),
        "a fresh count is applied, as it always was"
    );
    let outbound = writer
        .produce(now + writer.config().heartbeat_period)
        .expect("produce still works");
    assert!(data_numbers(&outbound).is_empty(), "nothing is owed");
    assert!(gaps(&outbound).is_empty());
    assert!(writer.is_acknowledged());
}

#[test]
fn a_reader_owed_nothing_while_matched_is_owed_nothing_once_lapsed() {
    // An inactive proxy does not hold retirements back while it is matched
    // (see `settled_through`); remembering it as lapsed must not start to.
    let now = Instant::now();
    let mut writer = sedp_writer_with_two_acknowledged(now);
    let mut idle = ReaderProxy::new(
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [3; 10]),
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
        ),
        vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 45_002)],
        true,
    );
    idle.deactivate();
    let idle_guid = idle.guid();
    writer.match_reader(idle);
    assert_eq!(
        writer.lapse_participant(idle_guid, now + StdDuration::from_secs(60)),
        1
    );
    assert!(writer.lapsed.is_empty(), "nothing to remember it for");

    let retired = writer.dispose(keyed(1), now).unwrap();
    writer.produce(now).unwrap();
    acknowledge_through(&mut writer, retired.value(), 2);
    assert_eq!(
        held(&writer),
        vec![(2, instance(2))],
        "the one reader owed the retirement has it"
    );
}
