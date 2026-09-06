//! Compute kernels: `slice`, `concat`, `cast`, `take` and `filter` over the
//! closed array-type set.
//!
//! ```text
//!   slice    slice_columns · try_slice_columns      batch-level windows
//!   concat   concat · concat_batches                stitch arrays/batches together
//!   cast     cast · OverflowPolicy                   convert between types
//!   take     take · take_batch                       gather rows by index
//!   filter   filter · filter_batch                   gather rows by boolean mask
//!   gather   (private)                               the take/filter core, built on concat
//! ```
//!
//! Every kernel here works on the same terms [`crate::array::Array::slice`]
//! already does: zero-copy where the operation allows it, the crate-wide
//! clamping convention where a window is involved, and a fully populated
//! [`crate::DataError`] rather than a panic for everything else.

mod gather;

pub mod cast;
pub mod concat;
pub mod filter;
pub mod slice;
pub mod take;

pub use crate::kernel::cast::{OverflowPolicy, cast};
pub use crate::kernel::concat::{concat, concat_batches};
pub use crate::kernel::filter::{filter, filter_batch};
pub use crate::kernel::slice::{slice_columns, try_slice_columns};
pub use crate::kernel::take::{take, take_batch};
