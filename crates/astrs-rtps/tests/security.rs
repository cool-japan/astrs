//! DDS-Security submessage protection, end to end and octet by octet.
//!
//! Three kinds of test live here, and the middle one is the reason the file
//! is not just a round trip:
//!
//! 1. **Round trips** — a signed and an encrypted exchange between two real
//!    participants on real sockets, plus a fragmented one, because
//!    fragmentation and protection interact through the datagram budget.
//! 2. **A capture of the wire.** A round trip proves that what was protected
//!    could be recovered. It does not prove that *everything* was protected:
//!    a submessage that escaped the transform would round-trip perfectly. So
//!    [`CapturingSocket`] records every octet a protected participant hands
//!    the kernel, and the test asserts that no user-endpoint submessage ever
//!    appears in the clear — in either direction, so the reader's `ACKNACK`s
//!    are covered too.
//! 3. **Rejections** — tampering, the wrong key by two distinct paths, a
//!    replayed counter, and a plaintext submessage aimed at a protected
//!    reader.
//!
//! And one more that is easy to fake and worth doing honestly: `protection:
//! none` must be *byte-identical* to a build with no security at all. Not
//! "the existing suite still passes", which only says nothing crashed, but
//! the same octets.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astrs_rtps::behavior::Participant;
use astrs_rtps::behavior::transport::{DatagramSocket, SocketFuture, UdpTransport};
use astrs_rtps::discovery::{ReaderQos, RosCompat, WriterQos};
use astrs_rtps::messages::{
    Data, DataPayload, Message, SerializedPayload, Submessage, SubmessageId,
};
use astrs_rtps::security::{
    AadBinding, EndpointSecurity, KeyMaterial, ProtectionKind, Psk, SecurityContext, SecurityError,
    TransformationKind,
};
use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber};

use harness::{PATIENCE, Pair, await_matched, config, large_payload, payload, topic};

/// The key both sides of every loopback test are configured with.
fn shared_psk() -> Psk {
    Psk::new(vec![0x5a_u8; 32]).expect("32 octets is long enough")
}

/// A different key, for the peer that must be refused.
fn other_psk() -> Psk {
    Psk::new(vec![0xa5_u8; 32]).expect("32 octets is long enough")
}

// ---------------------------------------------------------------------------
// A socket that keeps what it sent
// ---------------------------------------------------------------------------

/// A [`DatagramSocket`] that records every datagram before forwarding it.
///
/// The seam the "nothing escaped" test needs. It sees exactly what the kernel
/// sees, so a submessage that slipped past the transform is visible here and
/// nowhere else.
#[derive(Debug)]
struct CapturingSocket {
    inner: UdpTransport,
    sent: Mutex<Vec<Vec<u8>>>,
}

impl CapturingSocket {
    async fn bind_loopback() -> Arc<Self> {
        let inner = UdpTransport::bind_loopback()
            .await
            .expect("an ephemeral loopback socket must bind");
        Arc::new(Self {
            inner,
            sent: Mutex::new(Vec::new()),
        })
    }

    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.sent
            .lock()
            .expect("the capture lock is never poisoned")
            .clone()
    }
}

impl DatagramSocket for CapturingSocket {
    fn send_to<'a>(&'a self, datagram: &'a [u8], target: SocketAddr) -> SocketFuture<'a, usize> {
        self.sent
            .lock()
            .expect("the capture lock is never poisoned")
            .push(datagram.to_vec());
        Box::pin(self.inner.send_to(datagram, target))
    }

    fn recv_from<'a>(&'a self, buffer: &'a mut [u8]) -> SocketFuture<'a, (usize, SocketAddr)> {
        self.inner.recv_from(buffer)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Build a participant whose *user-traffic* socket keeps a copy of everything.
async fn capturing_participant(
    seed: u8,
    peer: Option<astrs_rtps::structure::Locator>,
    security: EndpointSecurity,
) -> (Participant, Arc<CapturingSocket>) {
    let metatraffic = UdpTransport::bind_loopback()
        .await
        .expect("the metatraffic socket must bind");
    let user_data = CapturingSocket::bind_loopback().await;
    let participant = Participant::with_transport(
        config(seed, peer, RosCompat::Jazzy).with_security(security),
        Arc::new(metatraffic),
        user_data.clone(),
        None,
    )
    .await
    .expect("the participant must build on the supplied sockets");
    (participant, user_data)
}

/// Every submessage id a datagram carries.
fn ids(datagram: &[u8]) -> Vec<SubmessageId> {
    Message::decode(datagram)
        .expect("a datagram this crate produced must decode")
        .iter()
        .map(Submessage::id)
        .collect()
}

/// True when a datagram carries a submessage that names a user endpoint in
/// the clear.
fn carries_plaintext_user_traffic(datagram: &[u8]) -> bool {
    let Ok(message) = Message::decode(datagram) else {
        return false;
    };
    message.iter().any(|submessage| {
        let named = astrs_rtps::security::source_entity(submessage)
            .into_iter()
            .chain(astrs_rtps::security::destination_entity(submessage));
        submessage.is_entity() && named.into_iter().any(|entity| !entity.is_builtin())
    })
}

// ---------------------------------------------------------------------------
// Round trips over real sockets
// ---------------------------------------------------------------------------

async fn secure_pair(security: EndpointSecurity) -> Pair {
    let left = Participant::new(config(41, None, RosCompat::Jazzy).with_security(security.clone()))
        .await
        .expect("bind");
    let right = Participant::new(
        config(42, Some(left.metatraffic_locator()), RosCompat::Jazzy).with_security(security),
    )
    .await
    .expect("bind");
    Pair::start(left, right)
}

async fn round_trip_under(protection: ProtectionKind) {
    let security = EndpointSecurity {
        psk: Some(shared_psk()),
        protection,
        aad: AadBinding::HeaderBound,
    };
    let pair = secure_pair(security).await;
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(32))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;

    for tag in 1..=5_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(5))
        .await
        .unwrap_or_else(|_| panic!("{protection}: every sample must arrive"));
    let mut numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5], "{protection}");
    for sample in &samples {
        let tag = u8::try_from(sample.sequence_number.value()).expect("small");
        assert_eq!(sample.as_slice(), payload(tag).as_slice(), "{protection}");
    }

    // The reliability protocol still works: the writer only knows every
    // sample arrived because the reader's protected ACKNACKs came back.
    tokio::time::timeout(PATIENCE, async {
        while !writer.is_acknowledged().await {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{protection}: protected ACKNACKs must reach the writer"));

    pair.shutdown().await;
}

#[tokio::test]
async fn an_encrypted_exchange_round_trips() {
    round_trip_under(ProtectionKind::Encrypt).await;
}

#[tokio::test]
async fn a_signed_exchange_round_trips() {
    round_trip_under(ProtectionKind::Sign).await;
}

#[tokio::test]
async fn a_fragmented_sample_round_trips_under_encryption() {
    // Protection and fragmentation meet at the datagram budget: wrapping a
    // DATA_FRAG adds octets, and the writer's effective threshold has to have
    // left room for them or the kernel refuses the datagram.
    let pair = secure_pair(EndpointSecurity::encrypted(shared_psk())).await;
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(64))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;

    let sample = large_payload(96 * 1024);
    writer.write(sample.clone()).await.expect("write");
    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("a fragmented sample must be reassembled through the transform");
    assert_eq!(received.len(), sample.len());
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}

// ---------------------------------------------------------------------------
// What actually goes on the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_user_submessage_ever_leaves_in_the_clear() {
    // The test a round trip cannot replace. A submessage that escaped the
    // transform would still be delivered and still be correct; only the
    // capture shows it.
    let security = EndpointSecurity::encrypted(shared_psk());
    let (left, left_socket) = capturing_participant(51, None, security.clone()).await;
    let (right, right_socket) =
        capturing_participant(52, Some(left.metatraffic_locator()), security).await;
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(32))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;

    for tag in 1..=4_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(4))
        .await
        .expect("the samples must arrive");
    assert_eq!(samples.len(), 4);
    tokio::time::timeout(PATIENCE, async {
        while !writer.is_acknowledged().await {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("and be acknowledged, so ACKNACKs are in the capture too");

    // Both directions: the writer's DATA and HEARTBEAT, the reader's ACKNACK.
    let mut secured = 0_usize;
    for (side, socket) in [
        ("writer side", &left_socket),
        ("reader side", &right_socket),
    ] {
        let datagrams = socket.datagrams();
        assert!(!datagrams.is_empty(), "{side}: nothing was captured");
        for datagram in &datagrams {
            assert!(
                !carries_plaintext_user_traffic(datagram),
                "{side}: a user submessage went out in the clear: {:?}",
                ids(datagram)
            );
            if ids(datagram).contains(&SubmessageId::SecurePrefix) {
                secured += 1;
            }
        }
    }
    assert!(
        secured >= 2,
        "both sides must have sent at least one protected datagram, saw {secured}"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn protection_none_is_byte_identical_to_no_security_at_all() {
    // "The existing suite stays green" proves nothing crashed. This proves
    // the early-out fired: the same message, through a context with no
    // protected endpoint, is the same octets.
    let mut inactive = SecurityContext::new(1_400);
    inactive
        .register(
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            &EndpointSecurity::none(),
            "rt/chatter\0std_msgs::msg::dds_::String_",
        )
        .expect("an unprotected endpoint registers as a no-op");
    assert!(
        !inactive.is_active(),
        "a `none` endpoint must leave the context inactive"
    );
    assert_eq!(inactive.protected_endpoints(), 0);

    let message = Message::from_participant(GuidPrefix::new([7; 12])).with(Data::new(
        EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY),
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        SequenceNumber::FIRST,
        DataPayload::Data(SerializedPayload::from_cdr(&1_234_i32).expect("encode")),
    ));
    let plain = message.encode().expect("encode");

    let protected = inactive
        .protect_datagram(&plain)
        .expect("an inactive context protects nothing");
    assert_eq!(protected, vec![plain.clone()], "not one octet may change");
    assert_eq!(
        inactive
            .unprotect_datagram(&plain)
            .expect("and verifies nothing"),
        plain
    );
}

#[tokio::test]
async fn an_unsecured_peer_and_a_secured_one_do_not_exchange_user_data() {
    // The honest consequence of having no handshake: there is no negotiation,
    // so a peer without the key simply does not get the traffic. Stated as a
    // test so it cannot be mistaken for a bug later.
    let left = Participant::new(
        config(61, None, RosCompat::Jazzy).with_security(EndpointSecurity::encrypted(shared_psk())),
    )
    .await
    .expect("bind");
    let right = Participant::new(config(
        62,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(32))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;
    writer.write(payload(1)).await.expect("write");

    assert!(
        reader
            .take_within(Duration::from_millis(200))
            .await
            .is_none(),
        "an unkeyed reader must not receive a protected sample"
    );

    pair.shutdown().await;
}

// ---------------------------------------------------------------------------
// Rejections, at the transform
// ---------------------------------------------------------------------------

fn writer_id() -> EntityId {
    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
}

fn reader_id() -> EntityId {
    EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
}

const CONTEXT: &str = "rt/chatter\0std_msgs::msg::dds_::String_";

fn sample_message(sequence_number: i64) -> Message<'static> {
    Message::from_participant(GuidPrefix::new([9; 12])).with(Data::new(
        reader_id(),
        writer_id(),
        SequenceNumber::new(sequence_number),
        DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
    ))
}

/// A publisher and a subscriber configured with the same key.
fn keyed_pair(protection: ProtectionKind) -> (SecurityContext, SecurityContext) {
    let security = EndpointSecurity {
        psk: Some(shared_psk()),
        protection,
        aad: AadBinding::HeaderBound,
    };
    let mut publisher = SecurityContext::new(1_400);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    let mut subscriber = SecurityContext::new(1_400);
    subscriber
        .register(reader_id(), &security, CONTEXT)
        .expect("register");
    (publisher, subscriber)
}

#[test]
fn an_encrypted_submessage_is_a_prefix_a_body_and_a_postfix() {
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let message = sample_message(1);
    let protected = publisher.protect(&message).expect("protect");
    assert_eq!(protected.len(), 1);
    assert_eq!(
        protected[0].iter().map(Submessage::id).collect::<Vec<_>>(),
        vec![
            SubmessageId::SecurePrefix,
            SubmessageId::SecureBody,
            SubmessageId::SecurePostfix,
        ]
    );
    // The payload octets are nowhere in the datagram.
    let octets = protected[0].encode().expect("encode");
    assert!(
        !octets.windows(16).any(|window| window == [0xab_u8; 16]),
        "the plaintext must not survive encryption"
    );
    assert_eq!(subscriber.unprotect(&protected[0]).expect("open"), message);
}

#[test]
fn a_signed_submessage_stays_readable_between_its_prefix_and_postfix() {
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Sign);
    let message = sample_message(1);
    let protected = publisher.protect(&message).expect("protect");
    assert_eq!(
        protected[0].iter().map(Submessage::id).collect::<Vec<_>>(),
        vec![
            SubmessageId::SecurePrefix,
            SubmessageId::Data,
            SubmessageId::SecurePostfix,
        ]
    );
    let octets = protected[0].encode().expect("encode");
    assert!(
        octets.windows(16).any(|window| window == [0xab_u8; 16]),
        "signing authenticates; it does not hide"
    );
    assert_eq!(subscriber.unprotect(&protected[0]).expect("open"), message);
}

#[test]
fn a_tampered_submessage_is_refused_under_both_protection_levels() {
    for protection in [ProtectionKind::Sign, ProtectionKind::Encrypt] {
        let (mut publisher, mut subscriber) = keyed_pair(protection);
        let protected = publisher.protect(&sample_message(1)).expect("protect");
        let octets = protected[0].encode().expect("encode");

        // Flip one octet of the protected submessage's body, everywhere it
        // could plausibly matter, and require every flip to be caught.
        for offset in 24..octets.len() - 24 {
            let mut altered = octets.clone();
            altered[offset] ^= 0x01;
            let Ok(message) = Message::decode(&altered) else {
                // The flip broke the framing before the tag was consulted,
                // which is also a rejection.
                continue;
            };
            assert!(
                subscriber.unprotect(&message).is_err(),
                "{protection}: a flip at octet {offset} was accepted"
            );
        }
    }
}

#[test]
fn a_peer_with_a_different_key_is_refused_by_the_key_id_lookup() {
    // The cheap path: a different pre-shared key derives a different key id,
    // so the rejection happens before any cryptography runs.
    let mut publisher = SecurityContext::new(1_400);
    publisher
        .register(
            writer_id(),
            &EndpointSecurity::encrypted(other_psk()),
            CONTEXT,
        )
        .expect("register");
    let (_, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);

    let protected = publisher.protect(&sample_message(1)).expect("protect");
    assert!(
        matches!(
            subscriber.unprotect(&protected[0]),
            Err(SecurityError::UnknownKeyId { .. })
        ),
        "a different key must be a different key id"
    );
}

#[test]
fn a_peer_with_the_right_key_id_and_the_wrong_key_is_refused_by_the_tag() {
    // The other path, and the one a `BTreeMap` miss would hide: same key id,
    // different material. Only `from_parts` can build this, which is exactly
    // why it is public — a PKI handshake produces material the same way.
    let kind = TransformationKind::Aes256Gcm;
    let mut publisher = SecurityContext::new(1_400);
    publisher.register_material(
        writer_id(),
        KeyMaterial::from_parts(0x2222_3333, [0x11; 32], [0x22; 32]),
        kind,
        AadBinding::HeaderBound,
    );
    let mut subscriber = SecurityContext::new(1_400);
    subscriber.register_material(
        reader_id(),
        KeyMaterial::from_parts(0x2222_3333, [0xee; 32], [0x22; 32]),
        kind,
        AadBinding::HeaderBound,
    );

    let protected = publisher.protect(&sample_message(1)).expect("protect");
    assert_eq!(
        subscriber.unprotect(&protected[0]),
        Err(SecurityError::AuthenticationFailed),
        "the id matched, so the tag had to do the work"
    );
}

#[test]
fn a_replayed_datagram_is_refused_and_a_reordered_one_is_not() {
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let first = publisher.protect(&sample_message(1)).expect("protect")[0]
        .encode()
        .expect("encode");
    let second = publisher.protect(&sample_message(2)).expect("protect")[0]
        .encode()
        .expect("encode");
    let third = publisher.protect(&sample_message(3)).expect("protect")[0]
        .encode()
        .expect("encode");

    // Out of order is fine: UDP reorders and the reliability protocol repairs
    // by resending, so a strict high-water mark would break both.
    assert!(subscriber.unprotect_datagram(&third).is_ok());
    assert!(
        subscriber.unprotect_datagram(&first).is_ok(),
        "an older but unseen counter must still be accepted"
    );
    assert!(subscriber.unprotect_datagram(&second).is_ok());

    // The same octets a second time are not.
    for (name, datagram) in [("first", &first), ("second", &second), ("third", &third)] {
        assert!(
            matches!(
                subscriber.unprotect_datagram(datagram),
                Err(SecurityError::ReplayedCounter { .. })
            ),
            "the {name} datagram was accepted twice"
        );
    }
}

#[test]
fn a_protected_reader_refuses_a_plaintext_submessage() {
    // The downgrade an attacker would try first: send it in the clear and
    // hope the receiver treats protection as optional.
    let (_, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    assert!(
        matches!(
            subscriber.unprotect(&sample_message(1)),
            Err(SecurityError::ProtectionMismatch { .. })
        ),
        "an unprotected DATA aimed at a protected reader must be refused"
    );
}

#[test]
fn a_protected_reader_refuses_the_wrong_transformation() {
    // Same key, same key id, different transformation kind: a peer configured
    // for `sign` must not be able to satisfy a reader that requires
    // `encrypt`, or the weaker setting wins by being sent.
    let security = EndpointSecurity {
        psk: Some(shared_psk()),
        protection: ProtectionKind::Sign,
        aad: AadBinding::HeaderBound,
    };
    let mut publisher = SecurityContext::new(1_400);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    let (_, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);

    let protected = publisher.protect(&sample_message(1)).expect("protect");
    assert!(
        matches!(
            subscriber.unprotect(&protected[0]),
            Err(SecurityError::ProtectionMismatch { .. })
        ),
        "a signed submessage must not satisfy an encrypting endpoint"
    );
}

#[test]
fn an_unprotected_endpoint_refuses_a_protected_submessage() {
    let (mut publisher, _) = keyed_pair(ProtectionKind::Encrypt);
    let protected = publisher.protect(&sample_message(1)).expect("protect");

    // A context that protects a *different* endpoint, so it is active but has
    // no policy for this reader.
    let mut bystander = SecurityContext::new(1_400);
    bystander
        .register(
            EntityId::user_defined(9, EntityKind::USER_READER_NO_KEY),
            &EndpointSecurity::encrypted(other_psk()),
            "rt/other\0T",
        )
        .expect("register");
    assert!(
        matches!(
            bystander.unprotect(&protected[0]),
            Err(SecurityError::UnknownKeyId { .. })
        ),
        "an unrelated key id is refused before anything else"
    );
}

#[test]
fn a_truncated_envelope_is_refused_rather_than_half_applied() {
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let protected = publisher.protect(&sample_message(1)).expect("protect");
    let full = protected[0].clone();

    // A prefix with no postfix.
    let mut orphan = Message::new(full.header);
    for submessage in full.iter().take(2) {
        orphan.push(submessage.clone());
    }
    assert!(matches!(
        subscriber.unprotect(&orphan),
        Err(SecurityError::Malformed { .. })
    ));

    // A prefix that opens before the previous one closed.
    let mut nested = Message::new(full.header);
    nested.push(full.submessages[0].clone());
    nested.push(full.submessages[0].clone());
    assert!(matches!(
        subscriber.unprotect(&nested),
        Err(SecurityError::Malformed { .. })
    ));
}

#[test]
fn a_configuration_with_protection_and_no_key_is_refused() {
    let mut context = SecurityContext::new(1_400);
    let broken = EndpointSecurity {
        psk: None,
        protection: ProtectionKind::Encrypt,
        aad: AadBinding::HeaderBound,
    };
    assert!(matches!(
        context.register(writer_id(), &broken, CONTEXT),
        Err(SecurityError::Malformed { .. })
    ));
    assert!(!context.is_active(), "and nothing was registered");
}

#[test]
fn forgetting_an_endpoint_takes_its_key_with_it() {
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let protected = publisher.protect(&sample_message(1)).expect("protect");
    assert!(subscriber.unprotect(&protected[0]).is_ok());

    assert!(subscriber.forget(reader_id()));
    assert!(!subscriber.forget(reader_id()), "and it is gone for good");
    assert!(!subscriber.is_active());
    assert_eq!(subscriber.protected_endpoints(), 0);
}

#[test]
fn a_message_that_does_not_fit_the_budget_is_split_and_keeps_its_interpreters() {
    use astrs_rtps::messages::{InfoDestination, InfoTimestamp};
    use astrs_rtps::structure::Time;

    // Four samples that fit one datagram in the clear, and do not once each
    // has grown by a prefix, a body and a postfix.
    let security = EndpointSecurity::encrypted(shared_psk());
    let mut publisher = SecurityContext::new(320);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    let mut subscriber = SecurityContext::new(320);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    subscriber
        .register(reader_id(), &security, CONTEXT)
        .expect("register");

    let mut message = Message::from_participant(GuidPrefix::new([9; 12]));
    message.push(InfoDestination::new(GuidPrefix::new([8; 12])));
    message.push(InfoTimestamp::at(Time::new(17, 0)));
    for number in 1..=4_i64 {
        message.push(Data::new(
            reader_id(),
            writer_id(),
            SequenceNumber::new(number),
            DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
        ));
    }

    let protected = publisher.protect(&message).expect("protect");
    assert!(
        protected.len() > 1,
        "the budget must have forced a split, got {} datagram(s)",
        protected.len()
    );

    let mut recovered = Vec::new();
    for part in &protected {
        assert!(
            part.serialized_len() <= 320,
            "a split datagram of {} octets is still over budget",
            part.serialized_len()
        );
        let clear = subscriber.unprotect(part).expect("open");
        let interpreters: Vec<SubmessageId> = clear
            .iter()
            .filter(|submessage| submessage.is_interpreter())
            .map(Submessage::id)
            .collect();
        assert!(
            interpreters.contains(&SubmessageId::InfoDestination)
                && interpreters.contains(&SubmessageId::InfoTimestamp),
            "every continuation must repeat the interpreter state, saw {interpreters:?}"
        );
        for submessage in clear.iter() {
            if let Some(data) = submessage.as_data() {
                recovered.push(data.writer_sn.value());
            }
        }
    }
    recovered.sort_unstable();
    assert_eq!(recovered, vec![1, 2, 3, 4], "and no sample was lost");
}

#[test]
fn one_submessage_too_large_for_the_budget_is_reported_rather_than_sent() {
    let security = EndpointSecurity::encrypted(shared_psk());
    let mut publisher = SecurityContext::new(64);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    assert!(matches!(
        publisher.protect(&sample_message(1)),
        Err(SecurityError::OverBudget { .. })
    ));
}

// ---------------------------------------------------------------------------
// Two publishers, and the broadcast downgrade
// ---------------------------------------------------------------------------

/// The same sample, from a named participant.
fn sample_message_from(prefix: GuidPrefix, sequence_number: i64) -> Message<'static> {
    Message::from_participant(prefix).with(Data::new(
        reader_id(),
        writer_id(),
        SequenceNumber::new(sequence_number),
        DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
    ))
}

#[test]
fn two_publishers_on_one_topic_do_not_share_a_replay_window() {
    // The bug a single-publisher test cannot see, and the reason the replay
    // windows are keyed by sending participant.
    //
    // A key id is a pure function of the pre-shared key and the topic, so
    // every publisher on a topic derives the *same* id — asserted below,
    // because the collision is only interesting if the ids really do match —
    // and each starts its session counter at one. Keyed on the key alone, the
    // second publisher's first submessage is a replay of the first
    // publisher's, and the whole of `/tf`, `/rosout` and every other
    // multi-publisher ROS 2 topic stops working the moment a second node
    // starts.
    let security = EndpointSecurity::encrypted(shared_psk());
    let material = KeyMaterial::from_psk(&shared_psk(), CONTEXT).expect("derive");

    let mut left = SecurityContext::new(1_400);
    left.register(writer_id(), &security, CONTEXT)
        .expect("register");
    let mut right = SecurityContext::new(1_400);
    right
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    let mut subscriber = SecurityContext::new(1_400);
    subscriber
        .register(reader_id(), &security, CONTEXT)
        .expect("register");

    let left_prefix = GuidPrefix::new([1; 12]);
    let right_prefix = GuidPrefix::new([2; 12]);

    // Both publishers protect their first sample. Same key, same session,
    // same counter — everything but the participant prefix collides.
    let from_left = left
        .protect(&sample_message_from(left_prefix, 1))
        .expect("protect")[0]
        .encode()
        .expect("encode");
    let from_right = right
        .protect(&sample_message_from(right_prefix, 1))
        .expect("protect")[0]
        .encode()
        .expect("encode");
    assert_ne!(
        from_left, from_right,
        "the two datagrams differ, or the test proves nothing"
    );

    subscriber
        .unprotect_datagram(&from_left)
        .expect("the first publisher is accepted");
    subscriber
        .unprotect_datagram(&from_right)
        .unwrap_or_else(|error| {
            panic!(
                "the second publisher on the same topic must not look like a replay \
             of the first (key id 0x{:08x} is shared by construction): {error}",
                material.key_id()
            )
        });

    // And each publisher's own counters are still tracked independently: a
    // genuine replay from either one is still refused.
    assert!(
        matches!(
            subscriber.unprotect_datagram(&from_left),
            Err(SecurityError::ReplayedCounter { .. })
        ),
        "per-sender windows must not cost the anti-replay guarantee"
    );
    assert!(matches!(
        subscriber.unprotect_datagram(&from_right),
        Err(SecurityError::ReplayedCounter { .. })
    ));

    // Both keep flowing afterwards, interleaved.
    for sequence_number in 2..=6_i64 {
        for (context, prefix) in [(&mut left, left_prefix), (&mut right, right_prefix)] {
            let datagram = context
                .protect(&sample_message_from(prefix, sequence_number))
                .expect("protect")[0]
                .encode()
                .expect("encode");
            subscriber
                .unprotect_datagram(&datagram)
                .expect("interleaved publishers must both keep flowing");
        }
    }
}

#[test]
fn a_plaintext_broadcast_cannot_slip_past_a_protected_participant() {
    // The downgrade that omits a *field* rather than the transform. An
    // attacker who cannot forge a tag clears `reader_id`: the submessage then
    // names no endpoint whose policy could refuse it, but is still delivered
    // to every matched reader — including the protected one.
    let (_, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let broadcast = Message::from_participant(GuidPrefix::new([9; 12])).with(Data::new(
        EntityId::UNKNOWN,
        writer_id(),
        SequenceNumber::new(1),
        DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
    ));
    assert!(
        matches!(
            subscriber.unprotect(&broadcast),
            Err(SecurityError::ProtectionMismatch { .. })
        ),
        "a plaintext broadcast must be refused, not waved through for naming no endpoint"
    );
}

#[test]
fn a_plaintext_metatraffic_broadcast_still_passes_a_secured_participant() {
    // The other side of the rule above, and the one that would have broken
    // discovery if the refusal had been written participant-wide.
    //
    // Metatraffic is never protected — SPDP has to be readable by a peer that
    // has not been configured yet — so a builtin writer's broadcast must pass
    // even though this participant secures a user topic. Refusing it would
    // contradict the promise `ParticipantConfig::security` makes, and both
    // sides of every loopback test being astrs participants is exactly why a
    // round trip cannot catch it.
    let (_, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let announcement = Message::from_participant(GuidPrefix::new([9; 12])).with(Data::new(
        EntityId::UNKNOWN,
        astrs_rtps::structure::ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
        SequenceNumber::new(1),
        DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
    ));
    assert_eq!(
        subscriber
            .unprotect(&announcement)
            .expect("discovery must survive"),
        announcement,
        "a builtin writer's broadcast is discovery, not a downgrade"
    );
}

#[test]
fn a_datagram_that_does_not_verify_allocates_no_replay_state() {
    // A crypto header names its key id in the clear, so an attacker who has
    // seen one datagram can name that key while holding none of it — and pair
    // it with any participant prefix and session id it likes. If the receiver
    // derived and stored a session before checking the tag, each such
    // datagram would leave sixteen octets and a key behind, across a 2^96
    // prefix space and a 2^32 session space.
    let (mut publisher, mut subscriber) = keyed_pair(ProtectionKind::Encrypt);
    let genuine = publisher.protect(&sample_message(1)).expect("protect")[0]
        .encode()
        .expect("encode");
    subscriber
        .unprotect_datagram(&genuine)
        .expect("the genuine datagram is accepted");
    let after_one_real = subscriber.tracked_sessions();
    assert_eq!(after_one_real, 1, "one real sender, one tracked session");

    // Now the flood: the same protected datagram with its tag corrupted, sent
    // under a fresh participant prefix every time. Every one is a distinct
    // (prefix, key, session) triple, so a receiver that allocated before
    // authenticating would grow by one per datagram.
    for seed in 0..64_u8 {
        let mut forged = genuine.clone();
        // The RTPS header's guidPrefix sits at octets 8..20. Changing it
        // alone is *not* enough to make the datagram fail: the AAD binds the
        // crypto header, not the RTPS one, which is limit 1 in the module
        // documentation — so the tag has to be corrupted too, or this test
        // would be measuring successful verification.
        forged[8..20].copy_from_slice(&[seed; 12]);
        // The SEC_POSTFIX body is the last twenty octets: a sixteen-octet
        // common MAC then a four-octet receiver-MAC count. Corrupt the MAC,
        // not the count — a bad count is refused while parsing the footer,
        // which never reaches the code under test.
        let tag_at = forged.len() - 20;
        forged[tag_at] ^= 0xff;
        assert!(
            matches!(
                subscriber.unprotect_datagram(&forged),
                Err(SecurityError::AuthenticationFailed)
            ),
            "a forged datagram must be refused by the tag, not by the parser"
        );
    }
    assert_eq!(
        subscriber.tracked_sessions(),
        after_one_real,
        "unauthenticated traffic must not allocate replay state"
    );
}

#[test]
fn a_protected_broadcast_is_accepted() {
    // The other half, so the rule above is a policy rather than a blanket
    // refusal of `ENTITYID_UNKNOWN`: a broadcast that verified under a key
    // this participant holds has proved everything that can be asked of it.
    let security = EndpointSecurity::encrypted(shared_psk());
    let mut publisher = SecurityContext::new(1_400);
    publisher
        .register(writer_id(), &security, CONTEXT)
        .expect("register");
    let mut subscriber = SecurityContext::new(1_400);
    subscriber
        .register(reader_id(), &security, CONTEXT)
        .expect("register");

    let broadcast = Message::from_participant(GuidPrefix::new([9; 12])).with(Data::new(
        EntityId::UNKNOWN,
        writer_id(),
        SequenceNumber::new(1),
        DataPayload::Data(SerializedPayload::new(vec![0xab_u8; 16])),
    ));
    let protected = publisher.protect(&broadcast).expect("protect");
    assert_eq!(
        subscriber.unprotect(&protected[0]).expect("open"),
        broadcast,
        "a broadcast that verified is a broadcast that may be delivered"
    );
}
