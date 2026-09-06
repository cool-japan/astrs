//! `send_arrow` — publishing arrow-rs data straight onto an output
//! (blueprint §9.1's `send_arrow(ArrayRef) [arrow-interop]`), behind this
//! crate's `arrow-interop` feature.
//!
//! ```text
//!   arrow_array::RecordBatch ──TryFrom──► astrs_data::RecordBatch ──► send_batch
//!                              (astrs-data's `arrow-interop` bridge)
//! ```
//!
//! Nothing here re-implements a conversion: every byte of the mapping already
//! lives in `astrs_data::interop`, and this module is the two lines that put
//! its output on the wire.
//!
//! # Why the argument is generic rather than `arrow_array::ArrayRef`
//!
//! Because `astrs-node-api` must not become a direct parent of arrow-rs. The
//! workspace's `deny.toml` bans the four `arrow-*` crates outright and then
//! re-admits them through a `wrappers` list naming exactly one crate —
//! `astrs-data` — so the whole workspace has a single, auditable arrow-rs
//! edge (blueprint §6.1, §18.1). Adding `arrow-array` to this crate's
//! manifest would create a second one and fail `cargo deny check bans` on any
//! run that activates the feature.
//!
//! A crate that does not depend on arrow-rs cannot *name* an arrow-rs type,
//! so the parameter is named by the **conversion** instead: any `A` for which
//! `astrs-data`'s bridge provides `RecordBatch: TryFrom<&A>`. Today that is
//! precisely `arrow_array::RecordBatch`, so the call site reads exactly as it
//! would with a concrete signature —
//!
//! ```ignore
//! out.send_arrow(&arrow_batch, meta)?;
//! ```
//!
//! — while this crate's dependency graph stays arrow-free. A single
//! `arrow_array::ArrayRef` reaches the same path through
//! [`RawOutput::send_array`](crate::output::RawOutput::send_array) after
//! `astrs_data::interop::from_arrow_array`, or by wrapping it in a one-column
//! `arrow_array::RecordBatch`.
//!
//! # Why its own error type
//!
//! The conversion fails with `astrs_data::interop::InteropError`, which
//! [`NodeError`] has no variant for — deliberately: `NodeError` is the
//! *default* build's public surface, and a variant that exists only under a
//! non-default feature would make the enum's shape feature-dependent. So the
//! two failure modes are kept apart in [`ArrowSendError`], which is
//! `#[error(transparent)]` on both arms: the rendered message is the
//! underlying error's own, and `source()` reaches it, so nothing is hidden by
//! the extra layer.

use astrs_data::RecordBatch;
use astrs_data::interop::InteropError;
use astrs_wire::Metadata;

use crate::error::NodeError;
use crate::output::{Output, RawOutput};

/// Either half of a `send_arrow` can fail: the conversion, or the send.
///
/// See the [module docs](self) for why this is a separate type rather than a
/// [`NodeError`] variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArrowSendError {
    /// The arrow-rs value could not be mapped onto AstRS's closed columnar
    /// type set (blueprint §6.1) — an arrow `DataType` outside the P0 set, a
    /// non-nanosecond timestamp, and so on.
    #[error(transparent)]
    Interop(#[from] InteropError),

    /// The converted batch could not be published.
    #[error(transparent)]
    Send(#[from] NodeError),
}

/// Result alias for the `send_arrow` family.
pub type ArrowSendResult<T = ()> = core::result::Result<T, ArrowSendError>;

/// Converts `value` through `astrs-data`'s arrow bridge and publishes the
/// result on `raw`.
///
/// The one implementation both [`RawOutput::send_arrow`] and
/// [`Output::send_arrow`] call.
fn send_arrow_through<A>(raw: &mut RawOutput, value: &A, metadata: Metadata) -> ArrowSendResult<()>
where
    A: ?Sized,
    for<'a> RecordBatch: TryFrom<&'a A, Error = InteropError>,
{
    let batch = RecordBatch::try_from(value)?;
    raw.send_batch(&batch, metadata)?;
    Ok(())
}

impl RawOutput {
    /// Publishes an arrow-rs value (blueprint §9.1's `send_arrow`).
    ///
    /// `A` is any type `astrs-data`'s `arrow-interop` bridge can convert into
    /// a [`RecordBatch`] — in practice `arrow_array::RecordBatch`. See the
    /// [module docs](self) for why the bound is written that way instead of
    /// naming the arrow type.
    ///
    /// # Errors
    ///
    /// [`ArrowSendError::Interop`] when the arrow value's layout has no AstRS
    /// counterpart, [`ArrowSendError::Send`] for whatever
    /// [`RawOutput::send_batch`] reports.
    #[cfg_attr(docsrs, doc(cfg(feature = "arrow-interop")))]
    pub fn send_arrow<A>(&mut self, value: &A, metadata: Metadata) -> ArrowSendResult<()>
    where
        A: ?Sized,
        for<'a> RecordBatch: TryFrom<&'a A, Error = InteropError>,
    {
        send_arrow_through(self, value, metadata)
    }
}

impl<T> Output<T> {
    /// Publishes an arrow-rs value, bypassing the type.
    ///
    /// As [`RawOutput::send_arrow`]; the typed skin offers it for the same
    /// reason it offers [`Output::send_batch`] — a typed handle should not be
    /// a reason to go and fetch the raw one.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_arrow`].
    #[cfg_attr(docsrs, doc(cfg(feature = "arrow-interop")))]
    pub fn send_arrow<A>(&mut self, value: &A, metadata: Metadata) -> ArrowSendResult<()>
    where
        A: ?Sized,
        for<'a> RecordBatch: TryFrom<&'a A, Error = InteropError>,
    {
        send_arrow_through(self.raw_mut(), value, metadata)
    }
}
