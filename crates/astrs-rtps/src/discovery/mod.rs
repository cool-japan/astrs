//! Discovery: SPDP, SEDP, QoS, and the database that remembers the answers.
//!
//! Two protocols, layered. **SPDP** ([`spdp`]) is one best-effort sample
//! repeated on a well-known address, saying "here I am and here is how to
//! reach me". **SEDP** ([`sedp`]) is two reliable topics carried over the
//! connection SPDP established, saying "and here is what I publish and
//! subscribe to". Neither means anything without the third piece: the
//! request-versus-offered rules ([`matching`]) that decide which of the
//! endpoints so announced should actually be wired together.
//!
//! | Module | Contents |
//! |---|---|
//! | [`qos`] | every DDS QoS policy, with its exact `PID_*` wire layout |
//! | [`matching`] | [`WriterQos`] / [`ReaderQos`] and the RxO rules |
//! | [`builtin`] | [`BuiltinEndpointSet`]: which discovery endpoints a peer runs |
//! | [`participant_data`] | the SPDP sample |
//! | [`endpoint_data`] | the two SEDP samples |
//! | [`plist`] | the bounded readers every discovery sample is parsed with |
//! | [`db`] | [`DiscoveryDb`]: who is out there, and what they publish |
//! | [`spdp`] | the announcement, its cadence and its targets |
//! | [`sedp`] | local endpoints out, remote proxies in |
//! | [`compat`] | [`RosCompat`]: Humble's 24-octet GID versus Jazzy's 16 |
//!
//! # One encoding, one exception
//!
//! Every discovery sample is `PL_CDR_LE` — a four-octet encapsulation header,
//! `PID`-tagged parameters, a sentinel — *except* the WLP participant
//! message, which is plain `CDR_LE`. That exception lives in
//! [`behavior::liveliness`](crate::behavior::liveliness) with the rest of the
//! liveliness protocol, and it is called out there because it is exactly the
//! kind of asymmetry an implementation gets wrong by pattern-matching on the
//! other four.
//!
//! # Robustness is asymmetric on purpose
//!
//! Decoding is permissive where the specification says to be and strict where
//! it matters: unknown parameters are skipped (§8.5.3.2's forward
//! compatibility), absent parameters take their DDS default, and repeated
//! locator parameters accumulate — but a parameter that is present and
//! malformed is an error, every length is bounded before anything is
//! allocated, and the two or three parameters a sample cannot mean anything
//! without are required. A blank topic name would match another blank topic
//! name, and two unrelated endpoints would be wired together; that is why
//! `PID_TOPIC_NAME` is required rather than defaulted.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::discovery::{ReaderQos, WriterQos, check_qos};
//! use astrs_rtps::behavior::QosPolicyId;
//!
//! // A reliable reader cannot be served by a best-effort writer, and DDS
//! // wants to be told which policy failed rather than left to guess.
//! assert_eq!(
//!     check_qos(&ReaderQos::reliable(10), &WriterQos::sensor_data()),
//!     Err(QosPolicyId::Reliability),
//! );
//! assert_eq!(
//!     check_qos(&ReaderQos::reliable(10), &WriterQos::services_default()),
//!     Ok(()),
//! );
//! ```

pub mod builtin;
pub mod compat;
pub mod db;
pub mod endpoint_data;
pub mod matching;
pub mod participant_data;
pub mod plist;
pub mod qos;
pub mod sedp;
pub mod spdp;

pub use builtin::{
    BuiltinEndpointQos, BuiltinEndpointSet, BuiltinPair, builtin_pairs, builtin_reader_ids,
    builtin_writer_ids,
};
pub use compat::{Gid, RosCompat};
pub use db::{DiscoveryDb, DiscoveryEvent, RemoteParticipant};
pub use endpoint_data::{DiscoveredReaderData, DiscoveredWriterData, EndpointIdentity};
pub use matching::{MatchOutcome, ReaderQos, WriterQos, check_qos, match_endpoints, qos_matches};
pub use participant_data::ParticipantData;
pub use qos::{
    DeadlineQos, DestinationOrderQos, DurabilityKind, DurabilityQos, HistoryKind, HistoryQos,
    LatencyBudgetQos, LifespanQos, LivelinessKind, LivelinessQos, OwnershipKind, OwnershipQos,
    OwnershipStrengthQos, PresentationQos, ReliabilityKind, ReliabilityQos, ResourceLimitsQos,
};
pub use sedp::{
    builtin_reader_proxy, builtin_writer_proxy, publication_for, reader_proxy_for,
    subscription_for, writer_proxy_for,
};
pub use spdp::{Spdp, SpdpConfig};
