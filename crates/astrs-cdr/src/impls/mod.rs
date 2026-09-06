//! [`CdrSerde`](crate::CdrSerde) implementations for the standard IDL types.
//!
//! Split by the three groups the OMG specification itself distinguishes:
//!
//! - [`primitive`] — `boolean`, `octet`, the sized integers, `float`,
//!   `double`.
//! - [`string`] — `string` and `wstring`, whose length rules differ from each
//!   other in both what is counted and whether a terminator is written.
//! - [`collection`] — `sequence<T>` and `T[N]`, including the XCDR2 DHEADER
//!   rule for non-primitive elements.
//!
//! Bounded forms (`string<N>`, `sequence<T, N>`) live in
//! [`crate::bounded`], because they add a check rather than a wire rule.

pub mod collection;
pub mod primitive;
pub mod string;
