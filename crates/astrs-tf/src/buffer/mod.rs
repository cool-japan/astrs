//! The tf2 frame tree: [`TransformBuffer`] over per-child-frame
//! `record::FrameStore`s (crate-private — see `transform_buffer`'s own
//! module docs for the storage design).

mod record;

mod transform_buffer;

pub use transform_buffer::{DEFAULT_MAX_HISTORY, TransformBuffer};
