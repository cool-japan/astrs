//! The build-cache bucket's key and record types.

use std::fmt;
use std::fmt::Write as _;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// An opaque content-hash key identifying one build's inputs.
///
/// `astrs-store` does not prescribe which hash function produced the
/// digest — the build driver (`astrs-cli`, a Layer 4 crate this one must not
/// depend on) chooses one and hashes whatever it considers a build's
/// identity (source files, manifest, toolchain version, ...). This type only
/// needs the bytes to be stable, comparable and orderable, which is why the
/// bucket key derived from it (see `crate::keys`) is simply its raw bytes.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode,
)]
#[serde(transparent)]
pub struct BuildCacheKey(Vec<u8>);

impl BuildCacheKey {
    /// Wraps raw hash bytes as a build-cache key.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::record::BuildCacheKey;
    ///
    /// let key = BuildCacheKey::new(vec![0xDE, 0xAD]);
    /// assert_eq!(key.as_bytes(), &[0xDE, 0xAD]);
    /// ```
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Borrows the raw hash bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the key and returns its raw hash bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Renders the key as lower-case hex, for logs and the CLI.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::record::BuildCacheKey;
    ///
    /// assert_eq!(BuildCacheKey::new(vec![0xDE, 0xAD]).to_hex(), "dead");
    /// ```
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(self.0.len() * 2);
        for byte in &self.0 {
            // `write!` to a `String` is infallible; the `Result` exists only
            // because `Write` is a shared trait with fallible implementors.
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parses a lower- or upper-case hex string back into a key.
    ///
    /// Returns `None` if `text` has an odd length or contains a non-hex-digit
    /// character; never panics on malformed input.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::record::BuildCacheKey;
    ///
    /// assert_eq!(
    ///     BuildCacheKey::from_hex("dead"),
    ///     Some(BuildCacheKey::new(vec![0xDE, 0xAD]))
    /// );
    /// assert_eq!(BuildCacheKey::from_hex("ded"), None);
    /// assert_eq!(BuildCacheKey::from_hex("zz"), None);
    /// ```
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        if !text.is_ascii() || !text.len().is_multiple_of(2) {
            return None;
        }
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        // The even-length check above leaves `as_chunks::<2>` no remainder.
        for pair in bytes.as_chunks::<2>().0 {
            // A two-byte window of an ASCII string always lands on character
            // boundaries, so this `from_utf8` cannot fail; guard it anyway
            // rather than assuming.
            let text_pair = std::str::from_utf8(pair).ok()?;
            out.push(u8::from_str_radix(text_pair, 16).ok()?);
        }
        Some(Self(out))
    }
}

impl fmt::Display for BuildCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// One cached build artifact: where it lives on disk and when it was built
/// and last reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct BuildCacheEntry {
    /// The source-hash key this entry is stored under (also the bucket key;
    /// carried here too so a caller iterating [`crate::CoordinatorStore::list_build_cache`]
    /// does not need to re-derive it).
    pub hash: BuildCacheKey,
    /// Where the built artifact lives, as UTF-8 text.
    ///
    /// Stored as `String` rather than `PathBuf` for two reasons: `PathBuf`
    /// has no oxicode encoding (this record is also embedded in a
    /// [`crate::record::MutationOp`] for the oxicode-encoded mutation log),
    /// and a build cache entry is metadata for humans and tooling (`astrs
    /// build --json`), not a value that needs byte-exact non-UTF-8 path
    /// round-tripping. A non-UTF-8 artifact path is converted lossily by the
    /// caller before it reaches this crate.
    pub artifact_path: String,
    /// When this artifact was built.
    pub built_at: HlcTimestamp,
    /// When this artifact was last reused by a `astrs start --build`.
    pub last_used_at: HlcTimestamp,
    /// The artifact's size in bytes, if known.
    pub size_bytes: Option<u64>,
    /// Monotonically increasing per-entry version counter, incremented on
    /// every [`crate::CoordinatorStore::record_build`] /
    /// [`crate::CoordinatorStore::touch_build_cache`] call for this hash.
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{WireDecode, WireEncode};

    fn ts(n: u64) -> HlcTimestamp {
        HlcTimestamp::new(n, 0)
    }

    #[test]
    fn hex_round_trips() {
        let key = BuildCacheKey::new(vec![0x00, 0x01, 0xFE, 0xFF]);
        assert_eq!(BuildCacheKey::from_hex(&key.to_hex()).as_ref(), Some(&key));
        assert_eq!(key.to_string(), "0001feff");
    }

    #[test]
    fn hex_rejects_odd_length_and_non_hex() {
        assert_eq!(BuildCacheKey::from_hex("f"), None);
        assert_eq!(BuildCacheKey::from_hex("gg"), None);
        assert_eq!(
            BuildCacheKey::from_hex(""),
            Some(BuildCacheKey::new(vec![]))
        );
    }

    #[test]
    fn hex_rejects_non_ascii_without_panicking() {
        // A multi-byte UTF-8 character must not cause a char-boundary panic
        // when byte-indexed.
        assert_eq!(BuildCacheKey::from_hex("é0"), None);
    }

    #[test]
    fn entry_survives_both_codecs() {
        let entry = BuildCacheEntry {
            hash: BuildCacheKey::new(vec![1, 2, 3]),
            artifact_path: "/var/lib/astrs/artifacts/abc".to_owned(),
            built_at: ts(1),
            last_used_at: ts(2),
            size_bytes: Some(4096),
            revision: 1,
        };
        let bytes = entry.encode_to_vec().unwrap();
        assert_eq!(BuildCacheEntry::decode_exact(&bytes).unwrap(), entry);

        let json = serde_json::to_vec(&entry).unwrap();
        let back: BuildCacheEntry = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, entry);
    }
}
