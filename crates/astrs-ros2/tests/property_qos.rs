//! Property tests over astrs QoS ⇄ RTPS QoS mapping (`crate::qos`, blueprint
//! §10.2's QoS surface: reliability, durability, history, deadline,
//! lifespan, liveliness).
//!
//! # `SystemDefault` is not injective — this suite does not pretend it is
//!
//! [`Reliability`], [`Durability`] and [`Liveliness`] each carry a
//! `SystemDefault` variant with no RTPS counterpart of its own: `to_rtps`
//! resolves it to a concrete value ([`Reliability::resolved`] and friends),
//! so `SystemDefault` and its resolved concrete value produce the *same*
//! wire QoS and therefore cannot be told apart coming back. Asserting
//! `from_rtps(x.to_rtps()) == x` would fail for `SystemDefault` by
//! construction and would not be finding a bug — so every round-trip
//! property here is stated against `x.resolved()`, the honest fixed point:
//! whatever the wire actually carries survives exactly, and `SystemDefault`
//! survives as the concrete value it means.
//!
//! # What a full profile's RTPS round trip actually preserves
//!
//! [`QosProfile::from_writer_qos`]/[`QosProfile::from_reader_qos`] do not
//! reconstruct every field a [`QosProfile`] carries — by design, matching
//! what `rmw` itself does (see that function's own docs):
//! `avoid_ros_namespace_conventions` has no RTPS QoS policy at all (it is a
//! mangling instruction, not a wire contract), and `LIFESPAN` is a
//! writer-side-only policy, so [`QosProfile::to_reader_qos`] never encodes
//! it and [`QosProfile::from_reader_qos`] cannot read it back. The
//! whole-profile properties below assert the *documented* fixed point, not
//! plain equality — a regression that started silently dropping `DEADLINE`
//! or `HISTORY` instead would still be caught.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_ros2::qos::{Durability, History, Liveliness, QosProfile, Reliability};
use proptest::prelude::*;

mod arb {
    use super::*;

    /// A bounded, exactly-representable duration: whole seconds under
    /// `i32::MAX` (so [`astrs_rtps::structure::DdsDuration::from_std`] never
    /// saturates) and nanoseconds under one second, so encoding to DDS's
    /// `(sec, nanosec)` pair and back is exact — precision loss at the
    /// saturation boundary is a real, separate concern from the mapping
    /// this suite is about.
    pub fn duration() -> impl Strategy<Value = Duration> {
        (0_u64..100_000, 0_u32..1_000_000_000).prop_map(|(secs, nanos)| Duration::new(secs, nanos))
    }

    pub fn optional_duration() -> impl Strategy<Value = Option<Duration>> {
        proptest::option::of(duration())
    }

    pub fn reliability() -> impl Strategy<Value = Reliability> {
        proptest::sample::select(&Reliability::ALL[..])
    }

    pub fn durability() -> impl Strategy<Value = Durability> {
        proptest::sample::select(&Durability::ALL[..])
    }

    pub fn liveliness() -> impl Strategy<Value = Liveliness> {
        proptest::sample::select(&Liveliness::ALL[..])
    }

    pub fn history() -> impl Strategy<Value = History> {
        prop_oneof![
            (1_i32..=100_000).prop_map(|depth| History::KeepLast { depth }),
            Just(History::KeepAll),
            Just(History::SystemDefault),
        ]
    }

    /// An arbitrary [`QosProfile`], every field independently generated.
    pub fn qos_profile() -> impl Strategy<Value = QosProfile> {
        (
            history(),
            reliability(),
            durability(),
            optional_duration(),
            optional_duration(),
            liveliness(),
            optional_duration(),
            any::<bool>(),
        )
            .prop_map(
                |(
                    history,
                    reliability,
                    durability,
                    deadline,
                    lifespan,
                    liveliness,
                    liveliness_lease,
                    avoid_ros_namespace_conventions,
                )| QosProfile {
                    history,
                    reliability,
                    durability,
                    deadline,
                    lifespan,
                    liveliness,
                    liveliness_lease,
                    avoid_ros_namespace_conventions,
                },
            )
    }
}

proptest! {
    /// Reliability's honest round trip: whatever the RTPS side carries is
    /// exactly [`Reliability::resolved`], for every variant including
    /// `SystemDefault`.
    #[test]
    fn reliability_round_trips_to_its_resolved_value(value in arb::reliability()) {
        prop_assert_eq!(Reliability::from_rtps(value.to_rtps()), value.resolved());
    }

    /// Same shape for durability.
    #[test]
    fn durability_round_trips_to_its_resolved_value(value in arb::durability()) {
        prop_assert_eq!(Durability::from_rtps(value.to_rtps()), value.resolved());
    }

    /// Liveliness's *kind* round-trips the same way; its lease is a
    /// separate value threaded alongside the kind rather than resolved by
    /// it, so its own round trip is exercised as part of the whole-profile
    /// properties below (`liveliness_lease` survives `resolved()`
    /// untouched, since it is data, not an enum with a `SystemDefault` to
    /// resolve).
    #[test]
    fn liveliness_kind_round_trips_to_its_resolved_value(
        value in arb::liveliness(),
        lease in arb::optional_duration(),
    ) {
        prop_assert_eq!(Liveliness::from_rtps(value.to_rtps(lease)), value.resolved());
    }

    /// History's depth is not a `SystemDefault`-style enum resolution at
    /// all -- it is real data -- so unlike the other three policies its
    /// round trip is exact, not merely "exact after resolving": the depth
    /// itself must survive, not just collapse onto some fixed default.
    #[test]
    fn history_round_trips_to_its_resolved_value(value in arb::history()) {
        prop_assert_eq!(History::from_rtps(value.to_rtps()), value.resolved());
    }

    /// A `QosProfile`'s writer-side RTPS mapping preserves every resolved
    /// policy and both durations exactly; `avoid_ros_namespace_conventions`
    /// is the one field with no RTPS counterpart to preserve it in, and is
    /// documented to come back `false` regardless of what was sent.
    #[test]
    fn writer_qos_round_trips_to_the_resolved_profile(profile in arb::qos_profile()) {
        let mut expected = profile.resolved();
        expected.avoid_ros_namespace_conventions = false;

        let round_tripped = QosProfile::from_writer_qos(&profile.to_writer_qos());
        prop_assert_eq!(round_tripped, expected);
    }

    /// The reader-side mapping preserves the same fields, minus `LIFESPAN`
    /// -- a writer-only RTPS policy `ReaderQos` never carries -- which
    /// [`QosProfile::from_reader_qos`] documents coming back `None`
    /// regardless of what the profile asked for.
    #[test]
    fn reader_qos_round_trips_to_the_resolved_profile_minus_lifespan(profile in arb::qos_profile()) {
        let mut expected = profile.resolved();
        expected.avoid_ros_namespace_conventions = false;
        expected.lifespan = None;

        let round_tripped = QosProfile::from_reader_qos(&profile.to_reader_qos());
        prop_assert_eq!(round_tripped, expected);
    }

    /// [`QosProfile::resolved`] is idempotent -- resolving twice is the same
    /// as resolving once -- which every property above implicitly assumes
    /// (`profile.resolved().resolved() == profile.resolved()`) by comparing
    /// against a single `resolved()` call rather than re-resolving the
    /// round-tripped value before comparing.
    #[test]
    fn resolving_a_profile_twice_is_the_same_as_once(profile in arb::qos_profile()) {
        prop_assert_eq!(profile.resolved().resolved(), profile.resolved());
    }
}
