//! Golden packet: a complete SPDP participant announcement.
//!
//! The first datagram any RTPS participant sends. It goes to the discovery
//! multicast group `239.255.0.1` on port `PB + DG*domainId` (§9.6.1.1), and
//! it carries the sender's whole `ParticipantProxy` as a `PL_CDR` parameter
//! list inside one `DATA` (§8.5.3.2).
//!
//! Every octet below was derived from the specification's field tables, in
//! order, with the arithmetic in the comment beside it. The derivation is the
//! point of the fixture: a byte array with no explanation proves only that
//! the code still does what it did yesterday.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};
use astrs_rtps::messages::{
    Data, DataPayload, InfoTimestamp, Message, SerializedPayload, SubmessageId,
};
use astrs_rtps::structure::{
    Duration, ENTITYID_PARTICIPANT, ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER, Locator, ProtocolVersion, SequenceNumber, Time,
    VendorId, port,
};

use common::{SENDER, SENDER_HEADER, assert_golden, assert_octets, concat};

/// The unicast port this fixture's participant listens for metatraffic on:
/// domain 0, participant 0 → `7400 + 0 + 10 + 0` = 7410 = `0x1cf2`.
const METATRAFFIC_PORT: u16 = 7410;

/// Its user-traffic unicast port: `7400 + 0 + 11 + 0` = 7411 = `0x1cf3`.
const USER_PORT: u16 = 7411;

/// The `BuiltinEndpointSet_t` this fixture carries.
///
/// The parameter is an opaque four-octet bitmask; which bit means which
/// built-in endpoint is the discovery half's business, and this fixture fixes
/// the *framing* — parameter id, declared length, four octets in the stream's
/// byte order — rather than the bit assignments.
const BUILTIN_ENDPOINT_SET: u32 = 0x0000_003f;

/// Build the announcement with the public API.
fn spdp_announcement() -> Message<'static> {
    let mut proxy = ParameterList::new(Encoding::DISCOVERY);
    proxy
        .push_value(
            ParameterId::new(pid::PROTOCOL_VERSION),
            &ProtocolVersion::V2_3,
        )
        .expect("short enough");
    proxy
        .push_value(ParameterId::new(pid::VENDORID), &VendorId::ASTRS)
        .expect("short enough");
    proxy
        .push_value(
            ParameterId::new(pid::PARTICIPANT_GUID),
            &SENDER.with_entity(ENTITYID_PARTICIPANT),
        )
        .expect("short enough");
    proxy
        .push_value(
            ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR),
            &Locator::udpv4(std::net::Ipv4Addr::LOCALHOST, METATRAFFIC_PORT),
        )
        .expect("short enough");
    proxy
        .push_value(
            ParameterId::new(pid::DEFAULT_UNICAST_LOCATOR),
            &Locator::udpv4(std::net::Ipv4Addr::LOCALHOST, USER_PORT),
        )
        .expect("short enough");
    proxy
        .push_value(
            ParameterId::new(pid::PARTICIPANT_LEASE_DURATION),
            &Duration::from_secs(100),
        )
        .expect("short enough");
    proxy
        .push_value(
            ParameterId::new(pid::BUILTIN_ENDPOINT_SET),
            &BUILTIN_ENDPOINT_SET,
        )
        .expect("short enough");

    Message::from_participant(SENDER)
        .with(InfoTimestamp::at(Time::new(100, 0)))
        .with(Data::new(
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
            SequenceNumber::FIRST,
            DataPayload::Data(
                SerializedPayload::from_parameter_list(&proxy).expect("encode the proxy"),
            ),
        ))
}

/// The `PL_CDR_LE` payload, derived parameter by parameter.
///
/// Each entry is `parameterId:u16, parameterLength:u16, value[parameterLength]`
/// in the stream's byte order, and `parameterLength` is always a multiple of
/// four so the next entry lands on a four-octet boundary (§9.4.2.11).
fn expected_payload() -> Vec<u8> {
    concat(&[
        // Encapsulation: PL_CDR_LE is identifier 0x0003, read big-endian, and
        // the options word declares no trailing padding.
        &[0x00, 0x03, 0x00, 0x00],
        // PID_PROTOCOL_VERSION = 0x0015. ProtocolVersion_t is two octets
        // (major, minor), padded to the four-octet multiple the length field
        // must be.
        &[0x15, 0x00, 0x04, 0x00],
        &[0x02, 0x03, 0x00, 0x00],
        // PID_VENDORID = 0x0016. Two octets, "AS", padded to four.
        &[0x16, 0x00, 0x04, 0x00],
        &[0x41, 0x53, 0x00, 0x00],
        // PID_PARTICIPANT_GUID = 0x0050. A GUID_t is the twelve-octet prefix
        // then the four-octet entity id; ENTITYID_PARTICIPANT is 00 00 01 c1.
        &[0x50, 0x00, 0x10, 0x00],
        &[
            0x41, 0x53, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a,
        ],
        &[0x00, 0x00, 0x01, 0xc1],
        // PID_METATRAFFIC_UNICAST_LOCATOR = 0x0032. Locator_t is 24 octets:
        // kind (long), port (unsigned long), address (octet[16]). kind and
        // port take the stream's byte order; the address does not, and for
        // UDPv4 it is twelve zero octets then the four address octets.
        // 7410 = 0x1cf2.
        &[0x32, 0x00, 0x18, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0xf2, 0x1c, 0x00, 0x00],
        &[0x00; 12],
        &[0x7f, 0x00, 0x00, 0x01],
        // PID_DEFAULT_UNICAST_LOCATOR = 0x0031, the same shape. 7411 = 0x1cf3.
        &[0x31, 0x00, 0x18, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0xf3, 0x1c, 0x00, 0x00],
        &[0x00; 12],
        &[0x7f, 0x00, 0x00, 0x01],
        // PID_PARTICIPANT_LEASE_DURATION = 0x0002. The *RTPS* Duration_t:
        // seconds (long) and a 2^-32 fraction (unsigned long), not the DDS
        // seconds-and-nanoseconds form. 100 s exactly is fraction 0.
        &[0x02, 0x00, 0x08, 0x00],
        &[0x64, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        // PID_BUILTIN_ENDPOINT_SET = 0x0058, an unsigned long bitmask.
        &[0x58, 0x00, 0x04, 0x00],
        &[0x3f, 0x00, 0x00, 0x00],
        // PID_SENTINEL = 0x0001, length 0, terminates the list.
        &[0x01, 0x00, 0x00, 0x00],
    ])
}

/// The whole datagram.
fn expected_datagram() -> Vec<u8> {
    let payload = expected_payload();
    // DATA body = extraFlags(2) + octetsToInlineQos(2) + readerId(4)
    //           + writerId(4) + writerSN(8) + serializedPayload
    //           = 20 + 120 = 140 = 0x8c.
    let data_body_len = u16::try_from(20 + payload.len()).expect("fits");
    concat(&[
        &SENDER_HEADER,
        // INFO_TS: id 0x09, flags 0x01 (E only — little-endian body, the I
        // flag clear so a timestamp follows), octetsToNextHeader 8.
        &[0x09, 0x01, 0x08, 0x00],
        // Time_t: seconds = 100 (long, little-endian), fraction = 0.
        &[0x64, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        // DATA: id 0x15, flags 0x05 = E | D (little-endian, payload is the
        // sample), octetsToNextHeader 140.
        &[SubmessageId::DATA, 0x05],
        &data_body_len.to_le_bytes(),
        // extraFlags — reserved, zero.
        &[0x00, 0x00],
        // octetsToInlineQos = 16: readerId + writerId + writerSN.
        &[0x10, 0x00],
        // ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER = 00 01 00 c7.
        &[0x00, 0x01, 0x00, 0xc7],
        // ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER = 00 01 00 c2.
        &[0x00, 0x01, 0x00, 0xc2],
        // writerSN = 1: high (long) then low (unsigned long).
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &payload,
    ])
}

#[test]
fn the_spdp_announcement_is_byte_exact() {
    let expected = expected_datagram();
    // 20 header + (4 + 8) INFO_TS + (4 + 140) DATA.
    assert_eq!(expected.len(), 20 + 12 + 144);
    assert_golden(&spdp_announcement(), &expected);
}

#[test]
fn the_announcement_payload_is_a_parameter_list_that_decodes() {
    let message = spdp_announcement();
    let data = message.submessages[1].as_data().expect("the DATA");
    let payload = data.payload.payload().expect("a payload");
    assert_octets(payload.as_slice(), &expected_payload());
    assert!(payload.is_parameter_list());

    let (proxy, encoding) = payload.parameter_list().expect("decode the proxy");
    assert_eq!(encoding, Encoding::DISCOVERY);
    assert_eq!(proxy.len(), 7);

    let version: ProtocolVersion = proxy
        .get_by_base(pid::PROTOCOL_VERSION)
        .expect("PID_PROTOCOL_VERSION")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(version, ProtocolVersion::V2_3);

    let vendor: VendorId = proxy
        .get_by_base(pid::VENDORID)
        .expect("PID_VENDORID")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(vendor, VendorId::ASTRS);

    let guid: astrs_rtps::structure::Guid = proxy
        .get_by_base(pid::PARTICIPANT_GUID)
        .expect("PID_PARTICIPANT_GUID")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(guid, SENDER.participant_guid());

    let metatraffic: Locator = proxy
        .get_by_base(pid::METATRAFFIC_UNICAST_LOCATOR)
        .expect("PID_METATRAFFIC_UNICAST_LOCATOR")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(metatraffic.udp_port(), Some(METATRAFFIC_PORT));
    assert!(metatraffic.is_loopback());

    let lease: Duration = proxy
        .get_by_base(pid::PARTICIPANT_LEASE_DURATION)
        .expect("PID_PARTICIPANT_LEASE_DURATION")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(lease, Duration::from_secs(100));

    let endpoints: u32 = proxy
        .get_by_base(pid::BUILTIN_ENDPOINT_SET)
        .expect("PID_BUILTIN_ENDPOINT_SET")
        .decode_value(encoding)
        .expect("decode");
    assert_eq!(endpoints, BUILTIN_ENDPOINT_SET);
}

#[test]
fn the_announcement_is_addressed_to_the_standard_spdp_endpoints() {
    let message = spdp_announcement();
    let data = message.submessages[1].as_data().expect("the DATA");
    assert_eq!(
        data.reader_id.well_known_name(),
        Some("ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER")
    );
    assert_eq!(
        data.writer_id.well_known_name(),
        Some("ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER")
    );
    assert!(data.reader_id.is_builtin() && data.reader_id.is_reader());
    assert!(data.writer_id.is_builtin() && data.writer_id.is_writer());
    assert_eq!(data.validate(), Ok(()));
}

#[test]
fn the_announcement_goes_to_the_standard_multicast_locator() {
    // §9.6.1.1 with §9.6.1.4.1: 239.255.0.1 : PB + DG*domainId + d0.
    let group = port::default_multicast_locator(0).expect("domain 0 fits");
    assert_eq!(group.to_string(), "239.255.0.1:7400");
    assert_eq!(port::metatraffic_unicast(0, 0), Some(METATRAFFIC_PORT));
    assert_eq!(port::user_unicast(0, 0), Some(USER_PORT));
}

#[test]
fn a_big_endian_announcement_swaps_the_numbers_and_the_identifier() {
    use astrs_cdr::{EncapsulationKind, Endianness};

    let mut proxy = ParameterList::new(Encoding::new(EncapsulationKind::PlCdrBe));
    proxy
        .push_value(ParameterId::new(pid::VENDORID), &VendorId::ASTRS)
        .expect("short enough");
    let message = Message::from_participant(SENDER).with(
        Data::new(
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::from_parameter_list(&proxy).expect("encode")),
        )
        .with_endianness(Endianness::Big),
    );

    let expected = concat(&[
        &SENDER_HEADER,
        // DATA with flags 0x04 = D only: the E bit is clear, so the body is
        // big-endian. octetsToNextHeader is therefore big-endian too:
        // 20 + 16 = 36 = 0x0024.
        &[0x15, 0x04, 0x00, 0x24],
        &[0x00, 0x00], // extraFlags
        &[0x00, 0x10], // octetsToInlineQos = 16, big-endian
        &[0x00, 0x01, 0x00, 0xc7],
        &[0x00, 0x01, 0x00, 0xc2],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x01],
        // PL_CDR_BE is identifier 0x0002; the entry headers are big-endian.
        &[0x00, 0x02, 0x00, 0x00],
        &[0x00, 0x16, 0x00, 0x04],
        &[0x41, 0x53, 0x00, 0x00],
        &[0x00, 0x01, 0x00, 0x00],
    ]);
    assert_golden(&message, &expected);
}
