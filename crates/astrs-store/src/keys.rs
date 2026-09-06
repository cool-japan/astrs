//! Byte-level key encoding for each bucket.
//!
//! `oxistore-core`'s [`oxistore_core::KvStore`] is a single flat byte-keyed
//! namespace — it has no native concept of separate tables. "Buckets" are a
//! key-prefixing convention layered on top of that one namespace, chosen so
//! that:
//!
//! - Meta keys (tag `0x00`: the schema marker, the next-sequence counter,
//!   the compaction watermark) never appear inside a data bucket's
//!   `[tag, tag+1)` range scan, since `0x00` sorts before every data-bucket
//!   tag.
//! - Every data-bucket key is a fixed-width tag, followed by zero or more
//!   further fixed-width components, followed by at most one
//!   variable-width component *last* — so a key can always be parsed
//!   unambiguously without a length prefix on the variable part.
//! - The mutation log's sequence suffix is 8 big-endian bytes, so
//!   lexicographic key order equals numeric sequence order. That is the
//!   property [`crate::store::mutation_log`] depends on to page through the
//!   log with a plain range scan instead of a sort.
//!
//! Every bucket's value type except [`crate::record::ParamRecord`] already
//! carries its own key fields (`DataflowMeta::id`, `NodeInfo::{dataflow,
//! node}`, `DaemonInfo::id`, `BuildCacheEntry::hash`), so only the params
//! bucket needs a key *decoder* here — the others only ever need an
//! *encoder*, since a listing reads the key back out of the decoded value
//! instead.

use std::fmt::Write as _;

use astrs_wire::{DaemonId, DataflowId, NodeId, ParamKey};

use crate::error::{Error, Result};
use crate::record::{BuildCacheKey, MutationSeq};

const TAG_META: u8 = 0x00;
const TAG_PARAM: u8 = 0x01;
const TAG_DATAFLOW_META: u8 = 0x02;
const TAG_NODE_STATUS: u8 = 0x03;
const TAG_DAEMON: u8 = 0x04;
const TAG_BUILD_CACHE: u8 = 0x05;
const TAG_MUTATION_LOG: u8 = 0x06;
const TAG_NODE_PARAM: u8 = 0x07;

const META_SCHEMA_VERSION: u8 = 0x01;
const META_NEXT_SEQ: u8 = 0x02;
const META_COMPACTED_BEFORE: u8 = 0x03;

const DATAFLOW_ID_LEN: usize = 16;
const SEQ_LEN: usize = 8;

/// The reserved key holding the [`crate::schema::STORE_SCHEMA_VERSION`]
/// marker.
pub(crate) const fn schema_marker_key() -> [u8; 2] {
    [TAG_META, META_SCHEMA_VERSION]
}

/// The reserved key holding the last mutation-log sequence number issued.
pub(crate) const fn next_seq_key() -> [u8; 2] {
    [TAG_META, META_NEXT_SEQ]
}

/// The reserved key holding the compaction watermark (see
/// [`crate::store::mutation_log`]).
pub(crate) const fn compacted_before_key() -> [u8; 2] {
    [TAG_META, META_COMPACTED_BEFORE]
}

/// A bounded, human-readable hex preview of a key, for error messages. Never
/// panics regardless of key length or content.
pub(crate) fn preview(raw: &[u8]) -> String {
    const MAX_BYTES: usize = 24;
    let mut out = String::with_capacity(MAX_BYTES * 2 + 3);
    for byte in raw.iter().take(MAX_BYTES) {
        let _ = write!(out, "{byte:02x}");
    }
    if raw.len() > MAX_BYTES {
        out.push_str("...");
    }
    out
}

fn corrupt(bucket: &'static str, raw: &[u8], reason: impl Into<String>) -> Error {
    Error::CorruptKey {
        bucket,
        key_preview: preview(raw),
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------
// params: [TAG_PARAM][dataflow: 16B][param key: variable, UTF-8]
// ---------------------------------------------------------------------

/// The prefix matching every parameter of every dataflow.
///
/// [`param_prefix`] and [`param_key`] both build on this rather than
/// re-pushing [`TAG_PARAM`] themselves, so "every dataflow's prefix starts
/// with the bucket prefix, and every key starts with its dataflow's prefix"
/// holds by construction instead of by keeping three literals in sync by
/// hand.
pub(crate) const fn param_bucket_prefix() -> [u8; 1] {
    [TAG_PARAM]
}

/// The prefix matching every parameter of one dataflow.
pub(crate) fn param_prefix(dataflow: DataflowId) -> Vec<u8> {
    let mut out = param_bucket_prefix().to_vec();
    out.extend_from_slice(&dataflow.to_bytes());
    out
}

/// The full key for one dataflow-scoped parameter.
pub(crate) fn param_key(dataflow: DataflowId, key: &ParamKey) -> Vec<u8> {
    let mut out = param_prefix(dataflow);
    out.extend_from_slice(key.as_str().as_bytes());
    out
}

/// Recovers the `(dataflow, key)` pair a params-bucket key was built from.
///
/// # Errors
///
/// [`Error::CorruptKey`] if `raw` is not a well-formed params key — too
/// short, wrongly tagged, or carrying a param-key suffix that is not valid
/// UTF-8 / not a legal [`ParamKey`]. Every key this crate writes is
/// well-formed; this only fires on a corrupted or foreign-written store
/// file.
pub(crate) fn decode_param_key(raw: &[u8]) -> Result<(DataflowId, ParamKey)> {
    if raw.len() <= 1 + DATAFLOW_ID_LEN {
        return Err(corrupt("params", raw, "shorter than tag + dataflow id"));
    }
    if raw[0] != TAG_PARAM {
        return Err(corrupt("params", raw, "wrong bucket tag"));
    }
    let mut id_bytes = [0u8; DATAFLOW_ID_LEN];
    id_bytes.copy_from_slice(&raw[1..1 + DATAFLOW_ID_LEN]);
    let dataflow = DataflowId::from_bytes(id_bytes);
    let text = std::str::from_utf8(&raw[1 + DATAFLOW_ID_LEN..])
        .map_err(|_| corrupt("params", raw, "param-key suffix is not utf-8"))?;
    let key = ParamKey::new(text).map_err(|source| corrupt("params", raw, source.to_string()))?;
    Ok((dataflow, key))
}

// ---------------------------------------------------------------------
// node_param: [TAG_NODE_PARAM][dataflow: 16B][node id len: 2B BE][node id:
//             variable, UTF-8][param key: variable, UTF-8]
// ---------------------------------------------------------------------
//
// Unlike the dataflow-scoped params bucket, a node-scoped key has *two*
// variable-width components after the fixed dataflow id — the node id and
// the parameter key — so the node id's byte length is written explicitly
// (2 bytes, big-endian; a node id is capped at
// [`astrs_wire::MAX_NAME_LEN`] = 255 bytes, so 2 bytes is generous headroom
// rather than a tight fit) instead of relying on a delimiter that could
// collide with a character either component's grammar already permits
// (both node ids and parameter keys allow `.`, `_` and `-`).

const NODE_ID_LEN_PREFIX: usize = 2;

/// The prefix matching every node-scoped parameter of every dataflow.
pub(crate) const fn node_param_bucket_prefix() -> [u8; 1] {
    [TAG_NODE_PARAM]
}

/// The prefix matching every node-scoped parameter of one dataflow.
pub(crate) fn node_param_dataflow_prefix(dataflow: DataflowId) -> Vec<u8> {
    let mut out = node_param_bucket_prefix().to_vec();
    out.extend_from_slice(&dataflow.to_bytes());
    out
}

/// The prefix matching every parameter of one `(dataflow, node)` pair.
pub(crate) fn node_param_prefix(dataflow: DataflowId, node: &NodeId) -> Vec<u8> {
    let text = node.as_str().as_bytes();
    let mut out = node_param_dataflow_prefix(dataflow);
    // `NodeId` is capped at `astrs_wire::MAX_NAME_LEN` (255), which fits in
    // one byte already; the explicit `as u16`/`to_be_bytes` round trip keeps
    // the on-disk width fixed at two bytes even if that cap ever grows.
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text);
    out
}

/// The full key for one node-scoped parameter.
pub(crate) fn node_param_key(dataflow: DataflowId, node: &NodeId, key: &ParamKey) -> Vec<u8> {
    let mut out = node_param_prefix(dataflow, node);
    out.extend_from_slice(key.as_str().as_bytes());
    out
}

/// Recovers the `(dataflow, node, key)` triple a node-param-bucket key was
/// built from.
///
/// # Errors
///
/// [`Error::CorruptKey`] if `raw` is not a well-formed node-param key. Every
/// key this crate writes is well-formed; this only fires on a corrupted or
/// foreign-written store file.
pub(crate) fn decode_node_param_key(raw: &[u8]) -> Result<(DataflowId, NodeId, ParamKey)> {
    let header_len = 1 + DATAFLOW_ID_LEN + NODE_ID_LEN_PREFIX;
    if raw.len() < header_len {
        return Err(corrupt(
            "node_params",
            raw,
            "shorter than tag + dataflow id + node-id length prefix",
        ));
    }
    if raw[0] != TAG_NODE_PARAM {
        return Err(corrupt("node_params", raw, "wrong bucket tag"));
    }
    let mut id_bytes = [0u8; DATAFLOW_ID_LEN];
    id_bytes.copy_from_slice(&raw[1..1 + DATAFLOW_ID_LEN]);
    let dataflow = DataflowId::from_bytes(id_bytes);

    let len_offset = 1 + DATAFLOW_ID_LEN;
    let mut len_bytes = [0u8; NODE_ID_LEN_PREFIX];
    len_bytes.copy_from_slice(&raw[len_offset..len_offset + NODE_ID_LEN_PREFIX]);
    let node_len = usize::from(u16::from_be_bytes(len_bytes));

    let node_start = header_len;
    let node_end = node_start
        .checked_add(node_len)
        .filter(|&end| end <= raw.len())
        .ok_or_else(|| {
            corrupt(
                "node_params",
                raw,
                "node-id length prefix runs past the key",
            )
        })?;
    let node_text = std::str::from_utf8(&raw[node_start..node_end])
        .map_err(|_| corrupt("node_params", raw, "node-id suffix is not utf-8"))?;
    let node =
        NodeId::new(node_text).map_err(|source| corrupt("node_params", raw, source.to_string()))?;

    let key_text = std::str::from_utf8(&raw[node_end..])
        .map_err(|_| corrupt("node_params", raw, "param-key suffix is not utf-8"))?;
    let key = ParamKey::new(key_text)
        .map_err(|source| corrupt("node_params", raw, source.to_string()))?;

    Ok((dataflow, node, key))
}

// ---------------------------------------------------------------------
// dataflow_meta: [TAG_DATAFLOW_META][dataflow: 16B]
// ---------------------------------------------------------------------

/// The key for one dataflow's [`crate::record::DataflowMeta`] row.
pub(crate) fn dataflow_meta_key(dataflow: DataflowId) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + DATAFLOW_ID_LEN);
    out.push(TAG_DATAFLOW_META);
    out.extend_from_slice(&dataflow.to_bytes());
    out
}

/// The prefix matching every dataflow's metadata row.
pub(crate) const fn dataflow_meta_bucket_prefix() -> [u8; 1] {
    [TAG_DATAFLOW_META]
}

// ---------------------------------------------------------------------
// node_status: [TAG_NODE_STATUS][dataflow: 16B][node id: variable, UTF-8]
// ---------------------------------------------------------------------

/// The key for one node's [`crate::record::NodeStatusRecord`] row.
pub(crate) fn node_status_key(dataflow: DataflowId, node: &NodeId) -> Vec<u8> {
    let text = node.as_str().as_bytes();
    let mut out = Vec::with_capacity(1 + DATAFLOW_ID_LEN + text.len());
    out.push(TAG_NODE_STATUS);
    out.extend_from_slice(&dataflow.to_bytes());
    out.extend_from_slice(text);
    out
}

/// The prefix matching every node-status row of one dataflow.
pub(crate) fn node_status_prefix(dataflow: DataflowId) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + DATAFLOW_ID_LEN);
    out.push(TAG_NODE_STATUS);
    out.extend_from_slice(&dataflow.to_bytes());
    out
}

// ---------------------------------------------------------------------
// daemon: [TAG_DAEMON][daemon id text: variable, UTF-8]
// ---------------------------------------------------------------------

/// The key for one daemon's [`crate::record::DaemonRecord`] row.
///
/// Uses [`DaemonId`]'s canonical `Display` text form. That form is a single
/// trailing component with no further structure to parse back out (the
/// stored [`astrs_wire::DaemonInfo::id`] already carries the typed id for
/// any caller that lists the bucket), so byte-for-byte round-tripping
/// through `FromStr` is not required here.
pub(crate) fn daemon_key(daemon: &DaemonId) -> Vec<u8> {
    let text = daemon.to_string();
    let mut out = Vec::with_capacity(1 + text.len());
    out.push(TAG_DAEMON);
    out.extend_from_slice(text.as_bytes());
    out
}

/// The prefix matching every daemon's row.
pub(crate) const fn daemon_bucket_prefix() -> [u8; 1] {
    [TAG_DAEMON]
}

// ---------------------------------------------------------------------
// build_cache: [TAG_BUILD_CACHE][hash: variable]
// ---------------------------------------------------------------------

/// The key for one [`crate::record::BuildCacheEntry`] row.
pub(crate) fn build_cache_key(hash: &BuildCacheKey) -> Vec<u8> {
    let bytes = hash.as_bytes();
    let mut out = Vec::with_capacity(1 + bytes.len());
    out.push(TAG_BUILD_CACHE);
    out.extend_from_slice(bytes);
    out
}

/// The prefix matching every build-cache row.
pub(crate) const fn build_cache_bucket_prefix() -> [u8; 1] {
    [TAG_BUILD_CACHE]
}

// ---------------------------------------------------------------------
// mutation_log: [TAG_MUTATION_LOG][seq: 8B big-endian]
// ---------------------------------------------------------------------

/// The key for one [`crate::record::MutationRecord`] row.
pub(crate) fn mutation_log_key(seq: MutationSeq) -> [u8; 1 + SEQ_LEN] {
    let mut out = [0u8; 1 + SEQ_LEN];
    out[0] = TAG_MUTATION_LOG;
    out[1..].copy_from_slice(&seq.get().to_be_bytes());
    out
}

/// The `[lo, hi)` range covering up to `max_entries + 1` sequence numbers
/// strictly after `after` — one more than requested, so the caller can tell
/// whether more remain beyond the page it asked for without a second query.
///
/// All arithmetic saturates at [`u64::MAX`] rather than wrapping, so a
/// pathological `after` or `max_entries` near the top of the sequence space
/// narrows the window instead of wrapping back into already-seen history.
pub(crate) fn mutation_log_window(
    after: MutationSeq,
    max_entries: usize,
) -> ([u8; 1 + SEQ_LEN], [u8; 1 + SEQ_LEN]) {
    let lo_seq = after.saturating_add(1);
    let hi_seq = after.saturating_add(max_entries as u64).saturating_add(2);
    (mutation_log_key(lo_seq), mutation_log_key(hi_seq))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10)
    }

    #[test]
    fn meta_keys_are_all_distinct_and_tagged_zero() {
        let keys = [
            schema_marker_key().to_vec(),
            next_seq_key().to_vec(),
            compacted_before_key().to_vec(),
        ];
        assert!(keys.iter().all(|k| k[0] == TAG_META));
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        assert_ne!(keys[0], keys[2]);
    }

    #[test]
    fn meta_tag_sorts_before_every_data_bucket_tag() {
        for tag in [
            TAG_PARAM,
            TAG_DATAFLOW_META,
            TAG_NODE_STATUS,
            TAG_DAEMON,
            TAG_BUILD_CACHE,
            TAG_MUTATION_LOG,
        ] {
            assert!(TAG_META < tag);
        }
    }

    #[test]
    fn param_key_round_trips() {
        let key = ParamKey::new("gain").unwrap();
        let raw = param_key(dataflow(), &key);
        let (decoded_dataflow, decoded_key) = decode_param_key(&raw).unwrap();
        assert_eq!(decoded_dataflow, dataflow());
        assert_eq!(decoded_key, key);
    }

    #[test]
    fn param_key_starts_with_its_dataflow_prefix() {
        let key = ParamKey::new("gain").unwrap();
        let raw = param_key(dataflow(), &key);
        assert!(raw.starts_with(&param_prefix(dataflow())));
        assert!(raw.starts_with(&param_bucket_prefix()));
    }

    #[test]
    fn decode_param_key_rejects_short_input_without_panicking() {
        assert!(decode_param_key(&[TAG_PARAM]).is_err());
        assert!(decode_param_key(&[]).is_err());
    }

    #[test]
    fn decode_param_key_rejects_wrong_tag() {
        let key = ParamKey::new("gain").unwrap();
        let mut raw = param_key(dataflow(), &key);
        raw[0] = TAG_DAEMON;
        assert!(matches!(
            decode_param_key(&raw),
            Err(Error::CorruptKey {
                bucket: "params",
                ..
            })
        ));
    }

    #[test]
    fn decode_param_key_rejects_non_utf8_suffix() {
        let mut raw = param_prefix(dataflow());
        raw.extend_from_slice(&[0xFF, 0xFE]);
        assert!(decode_param_key(&raw).is_err());
    }

    #[test]
    fn different_dataflows_never_share_a_param_prefix() {
        let a = param_prefix(DataflowId::from_u128(1));
        let b = param_prefix(DataflowId::from_u128(2));
        assert_ne!(a, b);
    }

    #[test]
    fn node_param_key_round_trips() {
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        let raw = node_param_key(dataflow(), &node, &key);
        let (decoded_dataflow, decoded_node, decoded_key) = decode_node_param_key(&raw).unwrap();
        assert_eq!(decoded_dataflow, dataflow());
        assert_eq!(decoded_node, node);
        assert_eq!(decoded_key, key);
    }

    #[test]
    fn node_param_key_nests_under_its_prefixes() {
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        let raw = node_param_key(dataflow(), &node, &key);
        assert!(raw.starts_with(&node_param_prefix(dataflow(), &node)));
        assert!(raw.starts_with(&node_param_dataflow_prefix(dataflow())));
        assert!(raw.starts_with(&node_param_bucket_prefix()));
    }

    #[test]
    fn node_param_keys_do_not_collide_across_node_ids_with_shared_prefixes() {
        // Without an explicit length prefix, `node="a", key="bc"` and
        // `node="ab", key="c"` would both serialize their variable-width
        // tail as `abc` and collide. The length-prefixed encoding must keep
        // them distinct.
        let a = node_param_key(
            dataflow(),
            &NodeId::new("a").unwrap(),
            &ParamKey::new("bc").unwrap(),
        );
        let b = node_param_key(
            dataflow(),
            &NodeId::new("ab").unwrap(),
            &ParamKey::new("c").unwrap(),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn node_param_keys_are_scoped_per_node() {
        let key = ParamKey::new("exposure").unwrap();
        let a = node_param_key(dataflow(), &NodeId::new("camera").unwrap(), &key);
        let b = node_param_key(dataflow(), &NodeId::new("lidar").unwrap(), &key);
        assert_ne!(a, b);
        assert_ne!(
            node_param_prefix(dataflow(), &NodeId::new("camera").unwrap()),
            node_param_prefix(dataflow(), &NodeId::new("lidar").unwrap())
        );
    }

    #[test]
    fn decode_node_param_key_rejects_short_input_without_panicking() {
        assert!(decode_node_param_key(&[TAG_NODE_PARAM]).is_err());
        assert!(decode_node_param_key(&[]).is_err());
    }

    #[test]
    fn decode_node_param_key_rejects_wrong_tag() {
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        let mut raw = node_param_key(dataflow(), &node, &key);
        raw[0] = TAG_PARAM;
        assert!(matches!(
            decode_node_param_key(&raw),
            Err(Error::CorruptKey {
                bucket: "node_params",
                ..
            })
        ));
    }

    #[test]
    fn decode_node_param_key_rejects_a_length_prefix_that_runs_past_the_key() {
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        let mut raw = node_param_key(dataflow(), &node, &key);
        // Claim the node id is far longer than the remaining bytes.
        let len_offset = 1 + DATAFLOW_ID_LEN;
        raw[len_offset..len_offset + NODE_ID_LEN_PREFIX].copy_from_slice(&0xFFFFu16.to_be_bytes());
        assert!(decode_node_param_key(&raw).is_err());
    }

    #[test]
    fn node_param_tag_is_distinct_from_every_other_bucket_tag() {
        assert!(
            [
                TAG_META,
                TAG_PARAM,
                TAG_DATAFLOW_META,
                TAG_NODE_STATUS,
                TAG_DAEMON,
                TAG_BUILD_CACHE,
                TAG_MUTATION_LOG,
            ]
            .iter()
            .all(|&tag| tag != TAG_NODE_PARAM)
        );
    }

    #[test]
    fn dataflow_meta_key_is_fixed_width() {
        let key = dataflow_meta_key(dataflow());
        assert_eq!(key.len(), 1 + DATAFLOW_ID_LEN);
        assert!(key.starts_with(&dataflow_meta_bucket_prefix()));
    }

    #[test]
    fn node_status_key_is_scoped_to_its_dataflow() {
        let node = NodeId::new("camera").unwrap();
        let key = node_status_key(dataflow(), &node);
        assert!(key.starts_with(&node_status_prefix(dataflow())));
        assert_ne!(
            node_status_prefix(dataflow()),
            node_status_prefix(DataflowId::from_u128(999))
        );
    }

    #[test]
    fn daemon_key_uses_display_text_and_is_prefixed() {
        let daemon = DaemonId::generate(None);
        let key = daemon_key(&daemon);
        assert!(key.starts_with(&daemon_bucket_prefix()));
        assert_eq!(&key[1..], daemon.to_string().as_bytes());
    }

    #[test]
    fn build_cache_key_uses_raw_hash_bytes() {
        let hash = BuildCacheKey::new(vec![1, 2, 3]);
        let key = build_cache_key(&hash);
        assert!(key.starts_with(&build_cache_bucket_prefix()));
        assert_eq!(&key[1..], hash.as_bytes());
    }

    #[test]
    fn mutation_log_key_is_big_endian_and_sorts_numerically() {
        let low = mutation_log_key(MutationSeq::new(1));
        let high = mutation_log_key(MutationSeq::new(2));
        assert!(low < high);
        let far = mutation_log_key(MutationSeq::new(0x0102_0304_0506_0708));
        assert_eq!(&far[1..], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    }

    #[test]
    fn mutation_log_keys_sort_numerically_across_many_values() {
        let mut seqs: Vec<u64> = vec![1, 2, 9, 10, 99, 100, 999, 1000, u64::MAX];
        let mut keyed: Vec<_> = seqs
            .iter()
            .map(|&s| mutation_log_key(MutationSeq::new(s)))
            .collect();
        keyed.sort();
        seqs.sort_unstable();
        let resorted_seqs: Vec<u64> = keyed
            .iter()
            .map(|k| u64::from_be_bytes(k[1..].try_into().unwrap()))
            .collect();
        assert_eq!(resorted_seqs, seqs);
    }

    #[test]
    fn mutation_log_window_covers_max_entries_plus_one() {
        let (lo, hi) = mutation_log_window(MutationSeq::ZERO, 3);
        assert_eq!(lo, mutation_log_key(MutationSeq::new(1)));
        assert_eq!(hi, mutation_log_key(MutationSeq::new(5)));
    }

    #[test]
    fn mutation_log_window_with_zero_page_still_covers_one_sentinel() {
        let (lo, hi) = mutation_log_window(MutationSeq::new(5), 0);
        assert_eq!(lo, mutation_log_key(MutationSeq::new(6)));
        assert_eq!(hi, mutation_log_key(MutationSeq::new(7)));
    }

    #[test]
    fn mutation_log_window_saturates_at_the_top_of_the_sequence_space() {
        let (lo, hi) = mutation_log_window(MutationSeq::new(u64::MAX), 10);
        assert_eq!(lo, mutation_log_key(MutationSeq::new(u64::MAX)));
        assert_eq!(hi, mutation_log_key(MutationSeq::new(u64::MAX)));
        assert_eq!(lo, hi, "an exhausted sequence space yields an empty window");
    }

    #[test]
    fn preview_never_panics_and_truncates_long_keys() {
        let long = vec![0xABu8; 1000];
        let text = preview(&long);
        assert!(text.ends_with("..."));
        assert!(text.len() < 100);
        assert_eq!(preview(&[]), "");
    }
}
