//! Deep-fuzz harness for `astrs-cdr`'s reader (blueprint §10.1, §15).
//!
//! Attack surface: [`from_bytes`] over a handful of representative types,
//! [`ParameterList::decode`]/[`ParameterList::decode_any`] (the `PL_CDR`
//! shape RTPS discovery is built from), and [`CdrReader::new`] (the
//! encapsulation-header front door every one of those goes through). Every
//! one of them must turn arbitrary octets into a value or a typed
//! [`astrs_cdr::CdrError`] — never a panic, never an allocation the declared length did
//! not already justify (see `astrs-cdr`'s own
//! `a_hostile_sequence_length_costs_one_comparison` test for the shape of
//! bug this guards against).
//!
//! This is the same `bytes -> five-or-so decoders` shape as
//! `astrs-cdr/tests/property.rs`'s `arbitrary_octets_never_panic`, run far
//! deeper and against a committed corpus — see that crate's module docs for
//! why deep coverage of this one surface lives here rather than growing that
//! property test's case count.

use astrs_cdr::{
    CdrReader, EncapsulationKind, Encoding, ParameterId, ParameterList, WString, cdr_struct,
    from_bytes, to_vec,
};

use crate::support::mutate;
use crate::support::rng::Rng;

cdr_struct! {
    /// A member for every alignment class (1/2/4/8) plus a length-prefixed
    /// string and a sequence, so a generated mutation has real field
    /// boundaries to land on.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Mixed {
        /// Alignment 1.
        pub flag: bool,
        /// Alignment 1.
        pub tag: u8,
        /// Alignment 2.
        pub small: i16,
        /// Alignment 4.
        pub medium: u32,
        /// Alignment 8 (4 under XCDR2).
        pub large: i64,
        /// A length-prefixed member.
        pub name: String,
        /// A sequence of primitives.
        pub samples: Vec<f64>,
    }
}

cdr_struct! {
    /// Nested inside a sequence, so the XCDR2 `DHEADER` rule for
    /// non-primitive elements is reachable from a seed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Entry {
        /// Key.
        pub key: String,
        /// Value.
        pub count: u32,
    }
}

cdr_struct! {
    /// A sequence of non-primitives plus a fixed array member.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Table {
        /// Rows.
        pub entries: Vec<Entry>,
        /// A fixed-size member, which carries no length of its own.
        pub checksum: [u8; 4],
    }
}

/// Per-iteration cap on generated input length.
pub const MAX_INPUT_LEN: usize = 8 * 1024;

/// Where the committed regression corpus for this surface lives.
pub const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/cdr");

fn sample_mixed() -> Vec<Mixed> {
    vec![
        Mixed {
            flag: false,
            tag: 0,
            small: 0,
            medium: 0,
            large: 0,
            name: String::new(),
            samples: Vec::new(),
        },
        Mixed {
            flag: true,
            tag: 0xFF,
            small: i16::MIN,
            medium: u32::MAX,
            large: i64::MIN,
            name: "astrs-fuzz".to_owned(),
            samples: vec![f64::NAN, f64::INFINITY, -0.0, 1.5],
        },
    ]
}

fn sample_table() -> Table {
    Table {
        entries: vec![
            Entry {
                key: String::new(),
                count: 0,
            },
            Entry {
                key: "row".to_owned(),
                count: 7,
            },
        ],
        checksum: [0xDE, 0xAD, 0xBE, 0xEF],
    }
}

/// Valid encodings from [`to_vec`] (every encapsulation kind, both struct
/// shapes) plus a [`ParameterList`] built through its own encoder.
#[must_use]
pub fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    for kind in EncapsulationKind::ALL {
        let encoding = Encoding::new(kind);
        for value in sample_mixed() {
            if let Ok(bytes) = to_vec(&value, encoding) {
                seeds.push(bytes);
            }
        }
        if let Ok(bytes) = to_vec(&sample_table(), encoding) {
            seeds.push(bytes);
        }
        if let Ok(bytes) = to_vec(&"astrs-fuzz string".to_owned(), encoding) {
            seeds.push(bytes);
        }
    }

    for kind in [EncapsulationKind::PlCdrLe, EncapsulationKind::PlCdrBe] {
        let encoding = Encoding::new(kind);
        let mut list = ParameterList::new(encoding);
        let entries: [(u16, Vec<u8>); 3] = [
            (0x0005, vec![1, 2, 3, 4]),
            (0x0071, Vec::new()),
            (0x1234, vec![0xAA; 12]),
        ];
        let mut built = true;
        for (id, value) in entries {
            if list.push_octets(ParameterId::new(id), value).is_err() {
                built = false;
            }
        }
        if built && let Ok(bytes) = list.encode() {
            seeds.push(bytes);
        }
    }
    seeds
}

/// Unstructured bytes about a quarter of the time; a structure-aware
/// mutation of a randomly chosen seed otherwise.
#[must_use]
pub fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    if rng.one_in(4) {
        return mutate::random_bytes(rng, MAX_INPUT_LEN);
    }
    match rng.pick(seeds) {
        Some(seed) => mutate::mutate(rng, seed, MAX_INPUT_LEN),
        None => mutate::random_bytes(rng, MAX_INPUT_LEN),
    }
}

/// The invariant: every decoder this crate exposes, pointed at the same
/// bytes, returns a value or a typed [`CdrError`](astrs_cdr::CdrError) —
/// never a panic.
pub fn check(bytes: &[u8]) {
    let _ = from_bytes::<Mixed>(bytes);
    let _ = from_bytes::<Table>(bytes);
    let _ = from_bytes::<String>(bytes);
    let _ = from_bytes::<WString>(bytes);
    let _ = from_bytes::<Vec<u8>>(bytes);
    let _ = from_bytes::<Vec<f64>>(bytes);
    let _ = from_bytes::<Vec<String>>(bytes);
    let _ = from_bytes::<[u64; 4]>(bytes);
    let _ = ParameterList::decode(bytes);
    let _ = ParameterList::decode_any(bytes);
    if let Ok(reader) = CdrReader::new(bytes) {
        let _ = reader.encoding();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_mixed_and_table_seed_actually_decodes() {
        for kind in EncapsulationKind::ALL {
            let encoding = Encoding::new(kind);
            for value in sample_mixed() {
                let bytes = to_vec(&value, encoding).expect("encode");
                assert!(
                    from_bytes::<Mixed>(&bytes).is_ok(),
                    "a cdr Mixed seed did not decode under {kind:?}"
                );
            }
            let bytes = to_vec(&sample_table(), encoding).expect("encode");
            assert!(
                from_bytes::<Table>(&bytes).is_ok(),
                "a cdr Table seed did not decode under {kind:?}"
            );
        }
    }

    #[test]
    fn every_parameter_list_seed_actually_decodes() {
        let mut found = 0;
        for kind in [EncapsulationKind::PlCdrLe, EncapsulationKind::PlCdrBe] {
            let encoding = Encoding::new(kind);
            let mut list = ParameterList::new(encoding);
            list.push_octets(ParameterId::new(0x0005), vec![1, 2, 3, 4])
                .expect("short enough");
            let bytes = list.encode().expect("encode");
            assert!(ParameterList::decode(&bytes).is_ok());
            found += 1;
        }
        assert_eq!(found, 2);
    }

    #[test]
    fn seeds_is_non_empty() {
        assert!(!seeds().is_empty());
    }

    #[test]
    fn generate_never_exceeds_the_cap() {
        let seeds = seeds();
        let mut rng = Rng::new(789);
        for _ in 0..500 {
            assert!(generate(&mut rng, &seeds).len() <= MAX_INPUT_LEN);
        }
    }

    #[test]
    fn check_never_panics_on_a_handful_of_hand_picked_edge_cases() {
        // Mirrors astrs-cdr's own `a_hostile_sequence_length_costs_one_comparison`:
        // a declared count of four billion eight-octet elements with four
        // octets actually present.
        let hostile = [
            0x00, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00,
        ];
        for case in [Vec::new(), vec![0u8; 4], vec![0xFFu8; 4], hostile.to_vec()] {
            check(&case);
        }
    }
}
