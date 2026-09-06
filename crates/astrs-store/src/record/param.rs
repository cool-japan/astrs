//! The parameter bucket's record type.

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// One dataflow-scoped parameter's stored value and version metadata.
///
/// The value is kept as pre-serialized JSON text (`value_json`) rather than
/// a `serde_json::Value`, for two reasons (see the crate root docs for the
/// full rationale):
///
/// 1. `serde_json::Value` has no oxicode encoding, and this record is also
///    embedded verbatim in a [`crate::record::MutationOp`] for the
///    oxicode-encoded mutation log.
/// 2. Keeping the value as opaque text means a `ControlReply::ParamValue`
///    response (or a bucket-to-log replay, see
///    [`crate::CoordinatorStore::apply_replayed`]) can reuse these bytes
///    directly, with no re-serialization step that could reformat the
///    caller's original JSON (key order, number formatting, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct ParamRecord {
    /// The parameter's value, as compact JSON text.
    pub value_json: String,
    /// Monotonically increasing per-key version counter.
    ///
    /// Starts at `1` on the first write to a key and increments by one on
    /// every subsequent [`crate::CoordinatorStore::set_param`] call. A
    /// [`crate::CoordinatorStore::delete_param`] removes the record
    /// entirely, so a later write to the same key restarts the counter at
    /// `1` — this crate does not keep tombstones across a delete.
    pub revision: u64,
    /// The hybrid logical clock timestamp of the first write to this key.
    pub created_at: HlcTimestamp,
    /// The hybrid logical clock timestamp of this revision's write.
    pub updated_at: HlcTimestamp,
}

impl ParamRecord {
    /// Parses [`ParamRecord::value_json`] back into a [`serde_json::Value`].
    ///
    /// # Errors
    ///
    /// Returns a JSON error if `value_json` is not valid JSON. Unreachable
    /// for a record this crate wrote itself: [`crate::CoordinatorStore::set_param`]
    /// only ever stores the output of `serde_json::to_string`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::record::ParamRecord;
    /// use astrs_time::HlcTimestamp;
    ///
    /// let record = ParamRecord {
    ///     value_json: "1.5".to_owned(),
    ///     revision: 1,
    ///     created_at: HlcTimestamp::new(1, 0),
    ///     updated_at: HlcTimestamp::new(1, 0),
    /// };
    /// assert_eq!(record.value().unwrap(), serde_json::json!(1.5));
    /// ```
    pub fn value(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::from_str(&self.value_json)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ts(n: u64) -> HlcTimestamp {
        HlcTimestamp::new(n, 0)
    }

    #[test]
    fn value_round_trips_through_json_text() {
        let record = ParamRecord {
            value_json: serde_json::json!({"gain": 1.5, "unit": "db"}).to_string(),
            revision: 1,
            created_at: ts(1),
            updated_at: ts(1),
        };
        assert_eq!(
            record.value().unwrap(),
            serde_json::json!({"gain": 1.5, "unit": "db"})
        );
    }

    #[test]
    fn record_survives_the_oxicode_codec() {
        use astrs_wire::{WireDecode, WireEncode};

        let record = ParamRecord {
            value_json: "\"hello\"".to_owned(),
            revision: 7,
            created_at: ts(1),
            updated_at: ts(9),
        };
        let bytes = record.encode_to_vec().unwrap();
        assert_eq!(ParamRecord::decode_exact(&bytes).unwrap(), record);
    }

    #[test]
    fn record_survives_the_json_codec() {
        let record = ParamRecord {
            value_json: "42".to_owned(),
            revision: 2,
            created_at: ts(3),
            updated_at: ts(4),
        };
        let json = serde_json::to_vec(&record).unwrap();
        let back: ParamRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn malformed_value_json_is_a_typed_error_not_a_panic() {
        let record = ParamRecord {
            value_json: "{not json".to_owned(),
            revision: 1,
            created_at: ts(1),
            updated_at: ts(1),
        };
        assert!(record.value().is_err());
    }
}
