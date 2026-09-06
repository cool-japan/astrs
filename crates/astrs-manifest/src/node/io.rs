//! Node input wiring: the short/long input form and queue policy (§8.3, §11.2).

use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::DurationSecs;

/// The default `queue_size` for an input that does not specify one (§11.2).
#[must_use]
pub fn default_queue_size() -> u32 {
    10
}

fn is_default_queue_size(v: &u32) -> bool {
    *v == default_queue_size()
}

/// How a full input queue behaves under backpressure (§11.2).
///
/// A node with no `queue_policy` field behaves as
/// [`QueuePolicy::DropOldest`] — the safer default for a real-time
/// dataflow, where an unbounded backlog is worse than a dropped stale
/// frame. Written in the manifest with underscores (`drop_oldest`),
/// matching §8.1's literal `queue_policy: drop_oldest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueuePolicy {
    /// Drop the oldest queued message to make room for a new one.
    #[default]
    DropOldest,
    /// Buffer beyond `queue_size` (up to 10x, per §11.2) before dropping
    /// the newest message and logging/metricizing the overflow.
    Backpressure,
}

/// Which delivery lane an input rides (§11.3).
///
/// The daemon's control/data lane mechanism already decides this
/// automatically for a few reserved cases — the `astrs/status` virtual
/// source, in particular, always rides the control lane so lifecycle events
/// pre-empt data traffic, whatever this field says. What this field adds is
/// the ability to *promote* an ordinary input onto the control lane too (a
/// safety-critical command topic that must pre-empt a hot data stream, for
/// instance) without touching the daemon's own heuristics. The default,
/// `data`, changes nothing: an input that does not name this field keeps
/// exactly the lane it would already have gotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriorityLane {
    /// The ordinary lane — fair round-robin against every other data input.
    #[default]
    Data,
    /// Pre-empts the data lane (blueprint §11.3): a control-lane message is
    /// always delivered before anything still queued on data.
    Control,
}

fn is_default_priority_lane(v: &PriorityLane) -> bool {
    *v == PriorityLane::default()
}

/// One node input: a source reference plus queueing behavior (§8.3).
///
/// Deserializes from either form the manifest allows:
///
/// - **Short form** — a bare `"node/output"` (or `astrs/...` virtual
///   source) string, taking every other field's default:
///   ```yaml
///   tick: astrs/timer/hz/50
///   ```
/// - **Long form** — a map with a required `source` and optional
///   `queue_size` / `queue_policy` / `timeout` / `priority_lane` / `deadline`:
///   ```yaml
///   frames:
///     source: camera/frames
///     queue_size: 2
///     queue_policy: drop_oldest
///     priority_lane: control
///     deadline: 50ms
///   ```
///
/// Round-trips back to the short form when every other field is still at
/// its default, and to the long form otherwise, keeping `to_yaml` output as
/// close to hand-written as possible.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// The `node/output` or `astrs/...` virtual-source string this input
    /// reads from. Resolved against declared nodes/virtual sources by
    /// [`crate::Manifest::validate`], not by this type.
    pub source: String,
    /// The bounded queue depth for this input (default 10, §11.2).
    pub queue_size: u32,
    /// What happens when the queue is full (default `drop_oldest`, §11.2).
    pub queue_policy: QueuePolicy,
    /// An optional per-input delivery timeout.
    pub timeout: Option<DurationSecs>,
    /// Which delivery lane this input rides (default `data`, §11.3).
    pub priority_lane: PriorityLane,
    /// The input-to-output latency budget monitored for this port, when one
    /// is declared (§11.3). `None` means no deadline is monitored — the
    /// common case, not a made-up default budget.
    pub deadline: Option<DurationSecs>,
}

impl Input {
    /// Build an input with only a source, taking every other default —
    /// equivalent to the manifest's short form.
    #[must_use]
    pub fn from_source(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            queue_size: default_queue_size(),
            queue_policy: QueuePolicy::default(),
            timeout: None,
            priority_lane: PriorityLane::default(),
            deadline: None,
        }
    }

    /// Whether this input would round-trip through the manifest's short
    /// `"node/output"` form, i.e. every field but `source` is at its
    /// default.
    #[must_use]
    pub fn is_short_form(&self) -> bool {
        self.queue_size == default_queue_size()
            && self.queue_policy == QueuePolicy::default()
            && self.timeout.is_none()
            && self.priority_lane == PriorityLane::default()
            && self.deadline.is_none()
    }
}

/// The long-form shape of [`Input`], used both as the `visit_map` target for
/// [`Input`]'s hand-written `Deserialize` and as the source of its
/// `Serialize`/`JsonSchema` long-form representation. `deny_unknown_fields`
/// here is what actually enforces the "no stray keys" contract on the long
/// form — [`Input`] itself cannot derive that attribute because it has a
/// hand-written `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InputLongForm {
    source: String,
    #[serde(
        default = "default_queue_size",
        skip_serializing_if = "is_default_queue_size"
    )]
    queue_size: u32,
    #[serde(default, skip_serializing_if = "is_default_queue_policy")]
    queue_policy: QueuePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<DurationSecs>,
    #[serde(default, skip_serializing_if = "is_default_priority_lane")]
    priority_lane: PriorityLane,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline: Option<DurationSecs>,
}

fn is_default_queue_policy(v: &QueuePolicy) -> bool {
    *v == QueuePolicy::default()
}

impl From<InputLongForm> for Input {
    fn from(long: InputLongForm) -> Self {
        Self {
            source: long.source,
            queue_size: long.queue_size,
            queue_policy: long.queue_policy,
            timeout: long.timeout,
            priority_lane: long.priority_lane,
            deadline: long.deadline,
        }
    }
}

impl From<&Input> for InputLongForm {
    fn from(short: &Input) -> Self {
        Self {
            source: short.source.clone(),
            queue_size: short.queue_size,
            queue_policy: short.queue_policy,
            timeout: short.timeout,
            priority_lane: short.priority_lane,
            deadline: short.deadline,
        }
    }
}

impl<'de> Deserialize<'de> for Input {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct InputVisitor;

        impl<'de> Visitor<'de> for InputVisitor {
            type Value = Input;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "an input given as a `node/output` string, or a map with a required \
                     `source` field and optional `queue_size`/`queue_policy`/`timeout`",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Input, E> {
                Ok(Input::from_source(v))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Input, E> {
                Ok(Input::from_source(v))
            }

            fn visit_map<A>(self, map: A) -> Result<Input, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                // Delegate to `InputLongForm`'s derived `Deserialize` so the
                // long form gets `deny_unknown_fields` and serde's own
                // crisp "missing field `source`" / "unknown field `x`"
                // messages, instead of hand-rolling map traversal here.
                let long = InputLongForm::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(long.into())
            }
        }

        deserializer.deserialize_any(InputVisitor)
    }
}

impl Serialize for Input {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_short_form() {
            serializer.serialize_str(&self.source)
        } else {
            InputLongForm::from(self).serialize(serializer)
        }
    }
}

impl JsonSchema for Input {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Input".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let short = generator.subschema_for::<String>();
        let long = generator.subschema_for::<InputLongForm>();
        json_schema!({
            "description": "A `node/output` (or astrs/... virtual source) string, or a long-form object with `source` plus optional `queue_size`/`queue_policy`/`timeout`/`priority_lane`/`deadline`.",
            "oneOf": [short, long],
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn short_form_deserializes_to_defaults() {
        let input: Input = astrs_yaml::from_str("astrs/timer/hz/50").unwrap();
        assert_eq!(input.source, "astrs/timer/hz/50");
        assert_eq!(input.queue_size, 10);
        assert_eq!(input.queue_policy, QueuePolicy::DropOldest);
        assert!(input.timeout.is_none());
        assert_eq!(input.priority_lane, PriorityLane::Data);
        assert!(input.deadline.is_none());
    }

    #[test]
    fn long_form_deserializes_all_fields() {
        let yaml = "source: camera/frames\nqueue_size: 2\nqueue_policy: drop_oldest\n";
        let input: Input = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(input.source, "camera/frames");
        assert_eq!(input.queue_size, 2);
        assert_eq!(input.queue_policy, QueuePolicy::DropOldest);
    }

    #[test]
    fn long_form_applies_defaults_for_omitted_fields() {
        let yaml = "source: camera/frames\n";
        let input: Input = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(input.queue_size, 10);
        assert_eq!(input.queue_policy, QueuePolicy::DropOldest);
        assert!(input.timeout.is_none());
        assert_eq!(input.priority_lane, PriorityLane::Data);
        assert!(input.deadline.is_none());
    }

    #[test]
    fn long_form_deserializes_priority_lane_and_deadline() {
        let yaml = "source: cmd/velocity\npriority_lane: control\ndeadline: 50ms\n";
        let input: Input = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(input.priority_lane, PriorityLane::Control);
        assert_eq!(input.deadline.map(DurationSecs::as_secs_f64), Some(0.05));
    }

    #[test]
    fn deadline_accepts_a_bare_number_of_seconds_like_its_sibling_durations() {
        let yaml = "source: cmd/velocity\ndeadline: 0.25\n";
        let input: Input = astrs_yaml::from_str(yaml).unwrap();
        assert_eq!(input.deadline.map(DurationSecs::as_secs_f64), Some(0.25));
    }

    #[test]
    fn priority_lane_rejects_anything_but_control_or_data() {
        let err = astrs_yaml::from_str::<Input>("source: cmd/velocity\npriority_lane: urgent\n")
            .unwrap_err();
        assert!(err.to_string().contains("urgent"), "error was: {err}");
    }

    #[test]
    fn priority_lane_alone_still_promotes_the_input_to_the_long_form() {
        // `priority_lane: control` is the one field this input sets — it must
        // still round-trip as the long form rather than being silently
        // dropped because every *other* field is at its default.
        let mut input = Input::from_source("cmd/velocity");
        input.priority_lane = PriorityLane::Control;
        assert!(!input.is_short_form());
        let yaml = astrs_yaml::to_string(&input).unwrap();
        assert!(yaml.contains("priority_lane: control"), "yaml was: {yaml}");

        let round_tripped: Input = astrs_yaml::from_str(&yaml).unwrap();
        assert_eq!(round_tripped, input);
    }

    #[test]
    fn long_form_requires_source() {
        let err = astrs_yaml::from_str::<Input>("queue_size: 2\n").unwrap_err();
        assert!(err.to_string().contains("source"), "error was: {err}");
    }

    #[test]
    fn long_form_rejects_unknown_fields() {
        let err = astrs_yaml::from_str::<Input>("source: camera/frames\nbogus: 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"), "error was: {err}");
    }

    #[test]
    fn rejects_non_string_non_map_scalars() {
        assert!(astrs_yaml::from_str::<Input>("42").is_err());
        assert!(astrs_yaml::from_str::<Input>("true").is_err());
    }

    #[test]
    fn short_form_round_trips_as_short_form() {
        let input = Input::from_source("camera/frames");
        let yaml = astrs_yaml::to_string(&input).unwrap();
        assert_eq!(yaml.trim(), "camera/frames");
    }

    #[test]
    fn long_form_round_trips_when_not_all_default() {
        let input = Input {
            source: "camera/frames".to_string(),
            queue_size: 2,
            queue_policy: QueuePolicy::DropOldest,
            timeout: None,
            priority_lane: PriorityLane::default(),
            deadline: None,
        };
        let yaml = astrs_yaml::to_string(&input).unwrap();
        assert!(yaml.contains("source: camera/frames"), "yaml was: {yaml}");
        assert!(yaml.contains("queue_size: 2"), "yaml was: {yaml}");
        // queue_policy is already at its default, so it should be omitted.
        assert!(!yaml.contains("queue_policy"), "yaml was: {yaml}");
    }

    #[test]
    fn round_trip_preserves_semantics() {
        for yaml in [
            "astrs/timer/hz/50",
            "source: camera/frames\nqueue_size: 2\nqueue_policy: drop_oldest\n",
            "source: cmd/velocity\npriority_lane: control\ndeadline: 50ms\n",
        ] {
            let input: Input = astrs_yaml::from_str(yaml).unwrap();
            let re_yaml = astrs_yaml::to_string(&input).unwrap();
            let round_tripped: Input = astrs_yaml::from_str(&re_yaml).unwrap();
            assert_eq!(input, round_tripped);
        }
    }
}
