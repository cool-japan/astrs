//! One module per attack surface (blueprint §15): the wire frame decoder,
//! the Arrow IPC reader, the CDR reader, and the RTPS submessage parser.
//!
//! Each module exposes the same three free functions, called by its own
//! `tests/*_fuzz.rs`:
//!
//! - `seeds() -> Vec<Vec<u8>>` — valid encodings from the target crate's own
//!   encoder, spanning the shapes worth mutating.
//! - `generate(&mut Rng, &[Vec<u8>]) -> Vec<u8>` — one fuzz case: either
//!   unstructured bytes or a structure-aware mutation of a seed.
//! - `check(&[u8])` — calls the decoder(s) and asserts this crate's
//!   invariants; never called outside `catch_unwind` (see
//!   [`crate::support::driver`]).

pub mod cdr;
pub mod data;
pub mod rtps;
pub mod wire;
