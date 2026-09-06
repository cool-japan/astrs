//! The `ros2: { qos: … }` manifest block ⇄ [`QosProfile`].
//!
//! `astrs-manifest` models the block as pure data (blueprint §10.5): every
//! field is an `Option`, nothing is defaulted, and no cross-field semantics
//! are applied. That was the right split — the manifest crate has no
//! business knowing what a DDS default is — and it leaves exactly three
//! decisions to this module, all of which are stated here rather than
//! buried in an `unwrap_or`.
//!
//! # Decision 1: an absent field means the ROS 2 default, not the DDS one
//!
//! A `ros2:` block with no `qos:` at all, or a `qos:` that sets only
//! `keep_last`, means [`QosProfile::default`] for everything it did not
//! mention — `RELIABLE`, `VOLATILE`, `KEEP_LAST(10)`. It does **not** mean
//! `WriterQos::default()`/`ReaderQos::default()`, whose reliability differs
//! by side. The blueprint's own §10.5 example (`qos: { reliable: true,
//! keep_last: 10 }`) spells out what is already the default, which is a good
//! sign that the default is what a manifest author expects.
//!
//! # Decision 2: `keep_all: true` wins over `keep_last`
//!
//! Both are settable in one block, and the schema does not forbid it.
//! `KEEP_ALL` is the strictly more retentive of the two, and a manifest that
//! asks for both is asking not to drop samples; honouring `keep_last` there
//! would silently drop them. So `keep_all: true` wins, `keep_all: false` is
//! ignored (it is the absence of a request, not a request for `KEEP_LAST`),
//! and the pair is not an error — a manifest is a configuration file, and
//! failing a deployment over a redundant line would be the wrong trade.
//!
//! # Decision 3: `lease_duration` implies nothing about the liveliness kind
//!
//! The block has a `lease_duration` but no liveliness *kind*, so a lease
//! alone means `AUTOMATIC` with that lease — the middleware asserts
//! liveliness, and a peer that stops being heard from inside the lease is
//! declared dead. Manual liveliness is an API-level choice
//! ([`QosProfile::with_liveliness`]), not a manifest one.

use std::time::Duration as StdDuration;

use astrs_manifest::{Durability as ManifestDurability, Qos as ManifestQos};

use crate::qos::profile::{Durability, History, Liveliness, QosProfile, Reliability};

/// Read a manifest `qos:` block into a profile.
///
/// `base` is what an unmentioned field falls back to — [`QosProfile::default`]
/// for a topic, [`QosProfile::services_default`] for a service, and so on.
/// See this module's docs for the three decisions this makes.
#[must_use]
pub fn from_manifest_qos_with_base(qos: &ManifestQos, base: QosProfile) -> QosProfile {
    let mut profile = base;

    if let Some(reliable) = qos.reliable {
        profile.reliability = if reliable {
            Reliability::Reliable
        } else {
            Reliability::BestEffort
        };
    }

    if let Some(durability) = qos.durability {
        profile.durability = match durability {
            ManifestDurability::Volatile => Durability::Volatile,
            ManifestDurability::TransientLocal => Durability::TransientLocal,
        };
    }

    // Decision 2: `keep_all: true` wins; `keep_all: false` is silence.
    if qos.keep_all == Some(true) {
        profile.history = History::KeepAll;
    } else if let Some(depth) = qos.keep_last {
        profile.history = History::KeepLast {
            depth: clamp_depth(depth),
        };
    }

    if let Some(deadline) = qos.deadline {
        profile.deadline = Some(StdDuration::from_secs_f64(deadline.as_secs_f64().max(0.0)));
    }

    // Decision 3: a lease with no kind is AUTOMATIC with that lease.
    if let Some(lease) = qos.lease_duration {
        profile.liveliness_lease = Some(StdDuration::from_secs_f64(lease.as_secs_f64().max(0.0)));
        if profile.liveliness == Liveliness::SystemDefault {
            profile.liveliness = Liveliness::Automatic;
        }
    }

    profile
}

/// Read a manifest `qos:` block into a profile, over
/// [`QosProfile::default`].
#[must_use]
pub fn from_manifest_qos(qos: &ManifestQos) -> QosProfile {
    from_manifest_qos_with_base(qos, QosProfile::default())
}

/// Render a profile back into the manifest spelling.
///
/// Only the fields the manifest can express survive; a `SystemDefault`
/// policy renders as an absent field, which is exactly what it means.
#[must_use]
pub fn to_manifest_qos(profile: &QosProfile) -> ManifestQos {
    let mut qos = ManifestQos::default();

    if profile.reliability != Reliability::SystemDefault {
        qos.reliable = Some(profile.reliability.is_reliable());
    }
    if profile.durability != Durability::SystemDefault {
        qos.durability = Some(if profile.durability.is_transient_local() {
            ManifestDurability::TransientLocal
        } else {
            ManifestDurability::Volatile
        });
    }
    match profile.history {
        History::KeepAll => qos.keep_all = Some(true),
        History::KeepLast { depth } => {
            qos.keep_last = Some(u32::try_from(depth.max(0)).unwrap_or(u32::MAX));
        }
        History::SystemDefault => {}
    }
    qos.deadline = profile.deadline.and_then(|deadline| {
        astrs_manifest::DurationSecs::from_secs_f64(deadline.as_secs_f64()).ok()
    });
    qos.lease_duration = profile
        .liveliness_lease
        .and_then(|lease| astrs_manifest::DurationSecs::from_secs_f64(lease.as_secs_f64()).ok());

    qos
}

/// A manifest depth is a `u32`; an RTPS one is an `i32`.
///
/// Anything past `i32::MAX` is clamped rather than wrapped: a manifest that
/// writes `keep_last: 5000000000` means "an enormous queue", and turning
/// that into a negative depth would mean `KEEP_ALL`'s sentinel.
fn clamp_depth(depth: u32) -> i32 {
    i32::try_from(depth).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_manifest::DurationSecs;

    fn secs(value: f64) -> DurationSecs {
        DurationSecs::from_secs_f64(value).expect("a finite non-negative duration")
    }

    #[test]
    fn an_empty_block_is_the_ros_default_not_the_dds_one() {
        let profile = from_manifest_qos(&ManifestQos::default());
        assert_eq!(profile, QosProfile::default());
        assert!(
            profile.to_reader_qos().is_reliable(),
            "an empty `qos:` block must not produce a best-effort subscription"
        );
    }

    #[test]
    fn the_blueprint_example_parses_to_reliable_keep_last_ten() {
        // The §10.5 example block, transcribed field by field: the manifest
        // crate owns the YAML parsing and tests it there; what is asserted here
        // is what this mapping makes of the parsed result.
        let block = ManifestQos {
            reliable: Some(true),
            keep_last: Some(10),
            ..ManifestQos::default()
        };
        let profile = from_manifest_qos(&block);
        assert!(profile.is_reliable());
        assert_eq!(profile.depth(), Some(10));
        assert_eq!(
            profile,
            QosProfile::default(),
            "the blueprint's example spells out what is already the default"
        );
    }

    #[test]
    fn reliable_false_means_best_effort() {
        let block = ManifestQos {
            reliable: Some(false),
            ..ManifestQos::default()
        };
        assert_eq!(
            from_manifest_qos(&block).reliability,
            Reliability::BestEffort
        );
    }

    #[test]
    fn transient_local_maps_across() {
        let block = ManifestQos {
            durability: Some(ManifestDurability::TransientLocal),
            ..ManifestQos::default()
        };
        assert!(from_manifest_qos(&block).is_transient_local());

        let volatile = ManifestQos {
            durability: Some(ManifestDurability::Volatile),
            ..ManifestQos::default()
        };
        assert!(!from_manifest_qos(&volatile).is_transient_local());
    }

    #[test]
    fn keep_all_wins_over_keep_last() {
        let block = ManifestQos {
            keep_all: Some(true),
            keep_last: Some(10),
            ..ManifestQos::default()
        };
        assert_eq!(from_manifest_qos(&block).history, History::KeepAll);
        assert_eq!(from_manifest_qos(&block).depth(), None);
    }

    #[test]
    fn keep_all_false_is_silence_not_a_request_for_keep_last() {
        let block = ManifestQos {
            keep_all: Some(false),
            keep_last: Some(3),
            ..ManifestQos::default()
        };
        assert_eq!(from_manifest_qos(&block).depth(), Some(3));

        let alone = ManifestQos {
            keep_all: Some(false),
            ..ManifestQos::default()
        };
        assert_eq!(
            from_manifest_qos(&alone).history,
            QosProfile::default().history,
            "`keep_all: false` alone changes nothing"
        );
    }

    #[test]
    fn an_enormous_depth_clamps_rather_than_wrapping() {
        let block = ManifestQos {
            keep_last: Some(u32::MAX),
            ..ManifestQos::default()
        };
        assert_eq!(from_manifest_qos(&block).depth(), Some(i32::MAX));
        assert!(
            from_manifest_qos(&block)
                .depth()
                .is_some_and(|depth| depth > 0),
            "a wrapped depth would be negative, which is KEEP_ALL's sentinel"
        );
    }

    #[test]
    fn a_deadline_crosses_as_a_duration() {
        let block = ManifestQos {
            deadline: Some(secs(0.25)),
            ..ManifestQos::default()
        };
        assert_eq!(
            from_manifest_qos(&block).deadline,
            Some(StdDuration::from_millis(250))
        );
    }

    #[test]
    fn a_lease_alone_means_automatic_liveliness() {
        let block = ManifestQos {
            lease_duration: Some(secs(2.0)),
            ..ManifestQos::default()
        };
        let profile = from_manifest_qos(&block);
        assert_eq!(profile.liveliness_lease, Some(StdDuration::from_secs(2)));
        assert_eq!(profile.liveliness, Liveliness::Automatic);
    }

    #[test]
    fn a_lease_over_a_manual_base_keeps_the_manual_kind() {
        let block = ManifestQos {
            lease_duration: Some(secs(2.0)),
            ..ManifestQos::default()
        };
        let base = QosProfile::default().with_liveliness(Liveliness::ManualByTopic, None);
        let profile = from_manifest_qos_with_base(&block, base);
        assert_eq!(profile.liveliness, Liveliness::ManualByTopic);
        assert_eq!(profile.liveliness_lease, Some(StdDuration::from_secs(2)));
    }

    #[test]
    fn a_base_survives_every_field_the_block_omits() {
        let block = ManifestQos {
            keep_last: Some(1),
            ..ManifestQos::default()
        };
        let profile = from_manifest_qos_with_base(&block, QosProfile::sensor_data());
        assert_eq!(
            profile.reliability,
            Reliability::BestEffort,
            "the base's reliability survives"
        );
        assert_eq!(profile.depth(), Some(1), "the block's depth wins");
    }

    #[test]
    fn a_profile_renders_back_into_the_manifest_spelling() {
        let profile = QosProfile::latched(4).with_deadline(Some(StdDuration::from_millis(500)));
        let block = to_manifest_qos(&profile);
        assert_eq!(block.reliable, Some(true));
        assert_eq!(block.durability, Some(ManifestDurability::TransientLocal));
        assert_eq!(block.keep_last, Some(4));
        assert_eq!(block.keep_all, None);
        assert_eq!(block.deadline.map(DurationSecs::as_secs_f64), Some(0.5_f64));
    }

    #[test]
    fn rendering_and_reading_round_trip_for_expressible_profiles() {
        let cases = [
            QosProfile::default(),
            QosProfile::sensor_data(),
            QosProfile::latched(7),
            QosProfile::default().with_deadline(Some(StdDuration::from_millis(125))),
            QosProfile {
                history: History::KeepAll,
                ..QosProfile::default()
            },
        ];
        for profile in cases {
            let round_tripped = from_manifest_qos(&to_manifest_qos(&profile));
            assert_eq!(
                round_tripped, profile,
                "{profile} did not survive the manifest round trip"
            );
        }
    }

    #[test]
    fn a_system_default_policy_renders_as_an_absent_field() {
        let block = to_manifest_qos(&QosProfile::system_default());
        assert_eq!(block.reliable, None);
        assert_eq!(block.durability, None);
        assert_eq!(block.keep_last, None);
        assert_eq!(block.keep_all, None);
    }
}
