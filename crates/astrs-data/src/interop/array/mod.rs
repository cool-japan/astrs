//! Array-level astrs-data <-> arrow-rs conversions.
//!
//! Split by direction rather than by type, because the two directions use
//! genuinely different strategies — see [`to_arrow`] and [`from_arrow`]'s own
//! module docs for why. Plain functions, not `From`/`TryFrom` impls: the
//! `Self` type on either side would be `Arc<dyn Trait>` (`crate::ArrayRef` or
//! `arrow_array::ArrayRef`), and a trait impl over an opaque trait-object
//! alias is a strictly worse API for callers than a named function — no
//! clearer under coherence, and `.into()` cannot infer which concrete
//! `Arc<dyn _>` a caller wants from context alone the way it can for a
//! concrete type.

pub mod from_arrow;
pub mod to_arrow;

pub use from_arrow::from_arrow_array;
pub use to_arrow::to_arrow_array;
