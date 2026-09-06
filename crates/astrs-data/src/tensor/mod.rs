//! Tensor views: checked, zero-copy N-dimensional access over flat columnar
//! data.
//!
//! ```text
//!   view    TensorView<T>   generic N-D view over a PrimitiveArray<T>
//!   image   ImageView       the std/media/v1/Image accessor, built on it
//! ```
//!
//! [`TensorView`] is generic over anything [`FixedSizeListArray`]'s own
//! module doc uses as its running example — a `FixedSizeList(UInt8, 3)` row,
//! or an image's flat pixel buffer — but reached from the *flat*
//! [`PrimitiveArray`] side rather than by walking a chain of nested list
//! arrays. [`ImageView`] is the one accessor this crate ships against a
//! concrete `std` layout; other node libraries needing the same "flat column
//! plus a shape" access pattern (point clouds, spectrograms, …) build their
//! own thin wrapper the same way, on the same [`TensorView`].
//!
//! [`FixedSizeListArray`]: crate::array::FixedSizeListArray
//! [`PrimitiveArray`]: crate::array::PrimitiveArray

pub mod image;
pub mod view;

pub use crate::tensor::image::ImageView;
pub use crate::tensor::view::TensorView;
