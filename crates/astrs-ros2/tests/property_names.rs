//! Property tests over ROS 2 ⇄ DDS name mangling and validation
//! (`crate::names`, blueprint §10.4's `rt/`/`rq/`/`rr/` prefixes and
//! namespace rules).
//!
//! Three shapes, each exercised over generated rather than hand-picked
//! input:
//!
//! 1. **Round trip on valid input.** Every fully-qualified ROS name, mangled
//!    to a DDS topic name under any [`TopicKind`] and demangled back, must
//!    recover exactly the name and kind it started from — and the
//!    generator's own names must themselves satisfy
//!    [`validate_full_name`], so a failure here can never be blamed on a
//!    bad generator. Likewise for the five action endpoint names and both
//!    directions of the `pkg/msg/Type` ⇄ `pkg::msg::dds_::Type_` type
//!    mangling.
//! 2. **No panic on adversarial input.** [`ros_topic_name`] and
//!    [`demangle_type_name`] see whatever a DDS peer announces — this stack
//!    does not control that string — so they must degrade to `None` on
//!    malformed input rather than panic, for *any* input, not just the
//!    malformed shapes a hand-written test happened to think of.
//!
//! Mangling is lossy in one direction that matters: [`ros_topic_name`]
//! recovers a name from whatever a peer announced without re-validating its
//! character set (blueprint's "half-mangled name is a peer's bug, not a
//! topic this stack should show in the graph" applies to the *prefix/suffix*
//! shape, not to what is left over) — so this file's round-trip properties
//! only ever start from a name this crate's own mangler produced, never
//! assert that a demangled arbitrary string is itself a valid name.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_ros2::names::mangle::{
    ActionEndpoint, TopicKind, TypeNamespace, action_name, dds_topic_name, dds_type_name,
    demangle_type_name, is_ros_topic, mangle_type_name, ros_topic_name, ros_type_name,
    split_action_name,
};
use astrs_ros2::names::validate::validate_full_name;
use proptest::prelude::*;

mod arb {
    use super::*;

    /// One legal ROS 2 name token: `[a-zA-Z_][a-zA-Z0-9_]{0,10}` — never
    /// starts with a digit, never empty, well under
    /// [`astrs_ros2::names::MAX_NAME_LEN`] even joined several deep.
    pub fn token() -> impl Strategy<Value = String> {
        "[a-zA-Z_][a-zA-Z0-9_]{0,10}"
    }

    /// A fully-qualified ROS 2 name: `/` followed by one to four tokens.
    pub fn full_name() -> impl Strategy<Value = String> {
        proptest::collection::vec(token(), 1..=4)
            .prop_map(|tokens| format!("/{}", tokens.join("/")))
    }

    pub fn topic_kind() -> impl Strategy<Value = TopicKind> {
        proptest::sample::select(&TopicKind::ALL[..])
    }

    pub fn action_endpoint() -> impl Strategy<Value = ActionEndpoint> {
        proptest::sample::select(&ActionEndpoint::ALL[..])
    }

    pub fn type_namespace() -> impl Strategy<Value = TypeNamespace> {
        proptest::sample::select(&TypeNamespace::ALL[..])
    }
}

proptest! {
    /// The generator's own contract: every [`arb::full_name`] is one
    /// [`validate_full_name`] itself accepts. If this ever fails, the bug is
    /// in this file's generator, not in `crate::names` — every other
    /// property in this suite depends on that being true.
    #[test]
    fn generated_full_names_are_themselves_valid(name in arb::full_name()) {
        prop_assert_eq!(validate_full_name(&name), Ok(()));
    }

    /// Mangling a valid name to a DDS topic name under any [`TopicKind`] and
    /// demangling it back recovers the exact name and kind — including the
    /// `rs/`/`rr/` pair, which share the `Reply` suffix but not the prefix
    /// (the prefix alone must disambiguate them).
    #[test]
    fn topic_mangling_round_trips_for_every_kind(name in arb::full_name(), kind in arb::topic_kind()) {
        let dds = dds_topic_name(&name, kind).expect("a validated name always mangles");
        let (back_kind, back_name) = ros_topic_name(&dds).expect("a name this crate mangled always demangles");
        prop_assert_eq!(back_name, name);
        prop_assert_eq!(back_kind, kind);
        prop_assert!(is_ros_topic(&dds));
    }

    /// [`ros_topic_name`] never panics, on any input at all — it is the
    /// first thing that sees a string a DDS peer announced, and a
    /// misbehaving peer must produce `None`, never a crash.
    #[test]
    fn ros_topic_name_never_panics_on_arbitrary_input(dds_name in ".*") {
        let _ = ros_topic_name(&dds_name);
        let _ = is_ros_topic(&dds_name);
    }

    /// [`demangle_type_name`] has the same never-panic obligation, for the
    /// same reason: a DDS type name also arrives from an untrusted peer.
    #[test]
    fn demangle_type_name_never_panics_on_arbitrary_input(dds_type in ".*") {
        let _ = demangle_type_name(&dds_type);
    }

    /// An action's five endpoint names all derive from one action name and
    /// split back apart into exactly that `(action, endpoint)` pair.
    #[test]
    fn action_endpoint_names_round_trip(
        action in arb::full_name(),
        endpoint in arb::action_endpoint(),
    ) {
        let name = action_name(&action, endpoint);
        prop_assert_eq!(split_action_name(&name), Some((action.as_str(), endpoint)));
    }

    /// `pkg`/namespace/type mangles to the `rosidl` DDS spelling and
    /// demangles back to the exact ROS spelling those three parts describe
    /// — via [`dds_type_name`]/[`demangle_type_name`], the pair this crate's
    /// own generated interfaces (`crate::interfaces::*::DDS_TYPE_NAME`)
    /// exercise for real types.
    #[test]
    fn type_name_mangling_round_trips(
        package in arb::token(),
        namespace in arb::type_namespace(),
        type_name in arb::token(),
    ) {
        let dds = dds_type_name(&package, namespace, &type_name, "");
        let expected = ros_type_name(&package, namespace, &type_name);
        prop_assert_eq!(demangle_type_name(&dds), Some(expected));
    }

    /// The other pair, [`mangle_type_name`]/[`ros_type_name`], round-trips
    /// the same way starting from the ROS spelling instead of the parts —
    /// the entry point `ros2 topic pub`-style callers actually use.
    #[test]
    fn ros_type_spelling_round_trips_through_mangle_and_demangle(
        package in arb::token(),
        namespace in arb::type_namespace(),
        type_name in arb::token(),
    ) {
        let ros = ros_type_name(&package, namespace, &type_name);
        let dds = mangle_type_name(&ros).expect("a three-component ROS type spelling always mangles");
        prop_assert_eq!(demangle_type_name(&dds), Some(ros));
    }
}
