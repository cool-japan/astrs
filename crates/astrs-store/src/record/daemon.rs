//! The daemon-registry bucket's record type.

use astrs_time::HlcTimestamp;
use astrs_wire::DaemonInfo;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// One daemon's registration: identity, build version, placement labels and
/// liveness, keyed by [`astrs_wire::DaemonId`].
///
/// Wraps [`DaemonInfo`] (machine label is carried inside
/// [`astrs_wire::DaemonId`] itself; `labels` and `reachable` live on
/// [`DaemonInfo`] already) rather than re-declaring its fields, and adds the
/// one field the coordinator's heartbeat loop needs that `DaemonInfo` does
/// not carry: the timestamp of the most recent heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DaemonRecord {
    /// The daemon's registration info.
    pub info: DaemonInfo,
    /// The hybrid logical clock timestamp of the most recent heartbeat.
    pub last_heartbeat: HlcTimestamp,
    /// Monotonically increasing per-daemon version counter, incremented on
    /// every [`crate::CoordinatorStore::upsert_daemon`] /
    /// [`crate::CoordinatorStore::record_heartbeat`] call.
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{DaemonId, WireDecode, WireEncode};
    use std::collections::BTreeMap;

    fn ts(n: u64) -> HlcTimestamp {
        HlcTimestamp::new(n, 0)
    }

    fn sample() -> DaemonRecord {
        DaemonRecord {
            info: DaemonInfo {
                id: DaemonId::generate(None),
                version: astrs_wire::AstrsVersion::current(),
                address: "127.0.0.1:7408".to_owned(),
                connected_at: ts(1),
                node_count: 2,
                labels: BTreeMap::from([("zone".to_owned(), "lab-1".to_owned())]),
                reachable: true,
            },
            last_heartbeat: ts(5),
            revision: 3,
        }
    }

    #[test]
    fn daemon_record_survives_both_codecs() {
        let record = sample();
        let bytes = record.encode_to_vec().unwrap();
        assert_eq!(DaemonRecord::decode_exact(&bytes).unwrap(), record);

        let json = serde_json::to_vec(&record).unwrap();
        let back: DaemonRecord = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, record);
    }
}
