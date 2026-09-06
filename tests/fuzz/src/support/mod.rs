//! Shared fuzzing infrastructure: a tiny in-crate PRNG ([`rng`], no `rand` —
//! not a workspace dependency), generic byte mutation ([`mutate`]), and the
//! fuzz-loop / corpus-replay / minimizer driver every surface in
//! [`crate::surfaces`] builds on ([`driver`]).

pub mod driver;
pub mod mutate;
pub mod rng;
