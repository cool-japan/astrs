//! Segment identity: the logical key, its 128-bit digest, and the short
//! OS-visible name derived from it.
//!
//! Blueprint §6.2 fixes the identity of a segment as
//! `{dataflow_id}/{node_id}/{generation}`; AstRS extends it with the output
//! port, because §6.2 also says **one ring per (producer, output)** — two
//! outputs of the same node are two segments and must not collide.
//!
//! # Why the OS name is a hash
//!
//! A spelled-out key is far too long for POSIX shared memory on macOS, where
//! `shm_open` names are capped at [`MACOS_SHM_NAME_MAX`] bytes *including*
//! the leading slash (§23 risk #7). A single UUID in canonical text form is
//! already 36 characters. So the OS-visible name is
//!
//! ```text
//! /astrs<base32(low 120 bits of the key digest)>
//! //^^^^ 6 chars                     24 chars    = 30 chars total
//! ```
//!
//! and the **full 128-bit digest is stamped into the segment header**, where
//! every attach verifies it ([`crate::ShmError::KeyMismatch`]). A truncated
//! name can therefore collide without ever cross-wiring two dataflows: the
//! collision surfaces as a typed error at attach time.
//!
//! # Examples
//!
//! ```
//! use astrs_shm::SegmentKey;
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let key = SegmentKey::new(
//!     DataflowId::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//!     7,
//! );
//!
//! let name = key.os_name();
//! assert!(name.as_str().starts_with("/astrs"));
//! assert!(name.as_str().len() <= astrs_shm::MACOS_SHM_NAME_MAX);
//! // The same key always yields the same name and digest.
//! assert_eq!(key.os_name(), name);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::fmt;

use astrs_wire::{DataId, DataflowId, NodeId};

use crate::error::{ShmError, ShmResult};

/// The maximum length of a POSIX shared-memory name on macOS, in bytes,
/// including the leading `/`.
///
/// Darwin's `PSHMNAMLEN` is 31; exceeding it makes `shm_open` fail with
/// `ENAMETOOLONG`. Linux allows 255, but AstRS uses the same short scheme
/// everywhere so a name minted on one host is valid on the other — a
/// requirement for the recording/replay tooling that reproduces a run on a
/// developer laptop (§14).
pub const MACOS_SHM_NAME_MAX: usize = 31;

/// The fixed prefix of every AstRS segment name.
pub const SEGMENT_NAME_PREFIX: &str = "/astrs";

/// The number of base32 characters appended to [`SEGMENT_NAME_PREFIX`].
///
/// 24 characters carry 120 bits of the digest; the remaining 8 bits live only
/// in the header field, which is where the authoritative comparison happens.
pub const SEGMENT_NAME_DIGEST_CHARS: usize = 24;

/// The total length of a generated segment name, in bytes.
pub const SEGMENT_NAME_LEN: usize = SEGMENT_NAME_PREFIX.len() + SEGMENT_NAME_DIGEST_CHARS;

const _: () = assert!(
    SEGMENT_NAME_LEN <= MACOS_SHM_NAME_MAX,
    "generated segment names must fit Darwin's PSHMNAMLEN"
);

/// The RFC 4648 base32 alphabet, lowercased.
///
/// Lowercase because some tooling lowercases `/dev/shm` entries when
/// reporting them, and a case-folded duplicate would be confusing.
const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The logical identity of one shared-memory ring.
///
/// One ring per `(producer node, output port, generation)` inside one
/// dataflow. The generation is the node's incarnation counter (blueprint
/// §6.2, glossary §24.4): restarting a node mints a new generation, so a
/// consumer still holding the previous mapping detects it immediately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    dataflow: DataflowId,
    node: NodeId,
    output: DataId,
    generation: u64,
}

impl SegmentKey {
    /// Build a key from its four components.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentKey;
    /// use astrs_wire::{DataId, DataflowId, NodeId};
    ///
    /// let key = SegmentKey::new(
    ///     DataflowId::generate(),
    ///     NodeId::new("lidar")?,
    ///     DataId::new("points")?,
    ///     1,
    /// );
    /// assert_eq!(key.generation(), 1);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub const fn new(dataflow: DataflowId, node: NodeId, output: DataId, generation: u64) -> Self {
        Self {
            dataflow,
            node,
            output,
            generation,
        }
    }

    /// Build a key from string components, validating each id.
    ///
    /// # Errors
    ///
    /// Returns [`ShmError::InvalidName`] if a component is not a legal
    /// [`NodeId`] / [`DataId`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentKey;
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 3)?;
    /// assert_eq!(key.node().as_str(), "camera");
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn from_parts(
        dataflow: DataflowId,
        node: &str,
        output: &str,
        generation: u64,
    ) -> ShmResult<Self> {
        let node = NodeId::new(node).map_err(|err| ShmError::InvalidName {
            reason: format!("node id: {err}"),
        })?;
        let output = DataId::new(output).map_err(|err| ShmError::InvalidName {
            reason: format!("output id: {err}"),
        })?;
        Ok(Self::new(dataflow, node, output, generation))
    }

    /// The dataflow this segment belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> &DataflowId {
        &self.dataflow
    }

    /// The producing node.
    #[must_use]
    pub const fn node(&self) -> &NodeId {
        &self.node
    }

    /// The producing output port.
    #[must_use]
    pub const fn output(&self) -> &DataId {
        &self.output
    }

    /// The producer's incarnation counter.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The same key with a different generation — what a restart produces.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentKey;
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "out", 1)?;
    /// let restarted = key.clone().with_generation(2);
    /// assert_ne!(key.digest(), restarted.digest());
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// The canonical textual form, `{dataflow}/{node}/{output}/{generation}`.
    ///
    /// This is what gets hashed, what logs print, and what `astrs doctor`
    /// shows. It is never handed to `shm_open`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentKey;
    /// use astrs_wire::DataflowId;
    ///
    /// let dataflow = DataflowId::from_u128(1);
    /// let key = SegmentKey::from_parts(dataflow, "camera", "image", 4)?;
    /// assert!(key.canonical().ends_with("/camera/image/4"));
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.dataflow.as_uuid(),
            self.node.as_str(),
            self.output.as_str(),
            self.generation
        )
    }

    /// The 128-bit digest of [`SegmentKey::canonical`].
    ///
    /// Stamped into the segment header and verified by every attach. Two
    /// keys that differ in any component — including the generation —
    /// produce different digests.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentKey;
    /// use astrs_wire::DataflowId;
    ///
    /// let dataflow = DataflowId::from_u128(0xdead_beef);
    /// let a = SegmentKey::from_parts(dataflow, "n", "a", 1)?;
    /// let b = SegmentKey::from_parts(dataflow, "n", "b", 1)?;
    /// assert_ne!(a.digest(), b.digest());
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn digest(&self) -> u128 {
        fnv1a_128(self.canonical().as_bytes())
    }

    /// The OS-visible short name for this key.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{SegmentKey, SEGMENT_NAME_LEN};
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::from_u128(9), "n", "o", 0)?;
    /// assert_eq!(key.os_name().as_str().len(), SEGMENT_NAME_LEN);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn os_name(&self) -> SegmentName {
        SegmentName::from_digest(self.digest())
    }
}

impl fmt::Display for SegmentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// A validated POSIX shared-memory object name.
///
/// Guaranteed to start with `/`, contain no other `/`, be non-empty, and fit
/// [`MACOS_SHM_NAME_MAX`]. Constructing one is the only way to reach
/// `shm_open`/`shm_unlink` in this crate, so the platform limit cannot be
/// violated by accident.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentName(String);

impl SegmentName {
    /// Derive the canonical name for a key digest.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentName;
    ///
    /// let name = SegmentName::from_digest(0);
    /// assert_eq!(name.as_str(), "/astrsaaaaaaaaaaaaaaaaaaaaaaaa");
    /// ```
    #[must_use]
    pub fn from_digest(digest: u128) -> Self {
        let mut name = String::with_capacity(SEGMENT_NAME_LEN);
        name.push_str(SEGMENT_NAME_PREFIX);
        // Take the low 120 bits, most-significant base32 digit first, so
        // lexicographic order over names matches numeric order over the
        // truncated digest — handy when eyeballing a `/dev/shm` listing.
        for index in (0..SEGMENT_NAME_DIGEST_CHARS).rev() {
            let shift = index * 5;
            let value = ((digest >> shift) & 0x1f) as usize;
            name.push(char::from(BASE32_ALPHABET[value]));
        }
        Self(name)
    }

    /// Validate an arbitrary name.
    ///
    /// Accepts externally supplied names (a manifest override, a broker
    /// request) after checking them against the POSIX and Darwin rules.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidName`] when the name is empty, does not begin with
    /// `/`, contains an interior `/` or a NUL, or exceeds
    /// [`MACOS_SHM_NAME_MAX`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentName;
    ///
    /// assert!(SegmentName::new("/astrs-demo").is_ok());
    /// assert!(SegmentName::new("astrs-demo").is_err()); // no leading slash
    /// assert!(SegmentName::new("/a/b").is_err()); // interior slash
    /// ```
    pub fn new(name: impl Into<String>) -> ShmResult<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(ShmError::InvalidName {
                reason: "name is empty".to_owned(),
            });
        }
        if !name.starts_with('/') {
            return Err(ShmError::InvalidName {
                reason: format!("{name:?} does not start with '/'"),
            });
        }
        if name[1..].contains('/') {
            return Err(ShmError::InvalidName {
                reason: format!("{name:?} contains an interior '/'"),
            });
        }
        if name.contains('\0') {
            return Err(ShmError::InvalidName {
                reason: format!("{name:?} contains a NUL byte"),
            });
        }
        if name.len() > MACOS_SHM_NAME_MAX {
            return Err(ShmError::InvalidName {
                reason: format!(
                    "{name:?} is {} bytes, above the {MACOS_SHM_NAME_MAX}-byte portable limit",
                    name.len()
                ),
            });
        }
        Ok(Self(name))
    }

    /// The name as a string slice, including the leading `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the owned string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for SegmentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SegmentName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// FNV-1a, widened to 128 bits.
///
/// Chosen over a stronger hash on purpose: the digest is a *collision
/// detector*, not a security primitive, and the authoritative check is a
/// full-width comparison against the header field, so a preimage attack buys
/// an attacker nothing that mapping the segment directly would not. FNV-1a is
/// eight lines, has no dependency, and is byte-order independent, which
/// matters because a `.arec` recording made on one host names segments that a
/// replay on another host must reproduce exactly (§14).
#[must_use]
fn fnv1a_128(bytes: &[u8]) -> u128 {
    // The standard 128-bit FNV parameters.
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::collections::HashSet;

    fn key(node: &str, output: &str, generation: u64) -> SegmentKey {
        SegmentKey::from_parts(DataflowId::from_u128(0x1234), node, output, generation).unwrap()
    }

    #[test]
    fn generated_names_fit_the_darwin_limit() {
        for generation in 0..512u64 {
            let name = key("perception.detector", "detections", generation).os_name();
            assert_eq!(name.as_str().len(), SEGMENT_NAME_LEN);
            assert!(name.as_str().len() <= MACOS_SHM_NAME_MAX);
            assert!(SegmentName::new(name.as_str()).is_ok());
        }
    }

    #[test]
    fn names_are_deterministic() {
        let a = key("camera", "image", 5).os_name();
        let b = key("camera", "image", 5).os_name();
        assert_eq!(a, b);
    }

    #[test]
    fn every_component_participates_in_the_digest() {
        let base = key("camera", "image", 1);
        assert_ne!(base.digest(), key("camera", "image", 2).digest());
        assert_ne!(base.digest(), key("camera", "depth", 1).digest());
        assert_ne!(base.digest(), key("lidar", "image", 1).digest());
        let other_dataflow =
            SegmentKey::from_parts(DataflowId::from_u128(0x4321), "camera", "image", 1).unwrap();
        assert_ne!(base.digest(), other_dataflow.digest());
    }

    #[test]
    fn a_large_key_space_produces_no_name_collisions() {
        let mut seen = HashSet::new();
        for node in 0..64 {
            for generation in 0..64u64 {
                let key = key(&format!("node-{node}"), "out", generation);
                assert!(
                    seen.insert(key.os_name().into_string()),
                    "collision at node-{node}/{generation}"
                );
            }
        }
        assert_eq!(seen.len(), 64 * 64);
    }

    #[test]
    fn base32_encodes_the_low_120_bits_big_endian() {
        // Digest 1 sets only the least significant bit → last char is 'b'.
        let name = SegmentName::from_digest(1);
        assert!(name.as_str().ends_with('b'), "{name}");
        assert_eq!(
            &name.as_str()[..SEGMENT_NAME_PREFIX.len()],
            SEGMENT_NAME_PREFIX
        );

        // 31 fills the low five bits → last char is the final alphabet entry.
        let name = SegmentName::from_digest(31);
        assert!(name.as_str().ends_with('7'), "{name}");

        // Bits above 120 are dropped by construction.
        let low = SegmentName::from_digest(0x2a);
        let high = SegmentName::from_digest(0x2a | (1u128 << 120));
        assert_eq!(low, high);
    }

    #[test]
    fn name_validation_rejects_the_hostile_shapes() {
        assert!(SegmentName::new("").is_err());
        assert!(SegmentName::new("relative").is_err());
        assert!(SegmentName::new("/a/b").is_err());
        assert!(SegmentName::new("/has\0nul").is_err());
        assert!(SegmentName::new(format!("/{}", "x".repeat(MACOS_SHM_NAME_MAX))).is_err());
        assert!(SegmentName::new(format!("/{}", "x".repeat(MACOS_SHM_NAME_MAX - 1))).is_ok());
    }

    #[test]
    fn canonical_form_round_trips_through_display() {
        let key = key("camera", "image", 9);
        assert_eq!(key.to_string(), key.canonical());
        assert!(key.canonical().ends_with("/camera/image/9"));
    }

    #[test]
    fn with_generation_only_changes_the_generation() {
        let key = key("camera", "image", 1);
        let next = key.clone().with_generation(2);
        assert_eq!(next.node(), key.node());
        assert_eq!(next.output(), key.output());
        assert_eq!(next.dataflow(), key.dataflow());
        assert_eq!(next.generation(), 2);
        assert_ne!(next.os_name(), key.os_name());
    }

    #[test]
    fn fnv_matches_the_published_offset_basis_for_the_empty_input() {
        assert_eq!(fnv1a_128(b""), 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d);
        // "a" — one round of the published algorithm.
        let expected = (0x6c62_272e_07bb_0142_62b8_2175_6295_c58du128 ^ u128::from(b'a'))
            .wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        assert_eq!(fnv1a_128(b"a"), expected);
    }

    #[test]
    fn from_parts_rejects_malformed_ids() {
        let dataflow = DataflowId::from_u128(1);
        assert!(matches!(
            SegmentKey::from_parts(dataflow, "", "out", 0),
            Err(ShmError::InvalidName { .. })
        ));
        assert!(matches!(
            SegmentKey::from_parts(dataflow, "node", "bad/port", 0),
            Err(ShmError::InvalidName { .. })
        ));
    }
}
