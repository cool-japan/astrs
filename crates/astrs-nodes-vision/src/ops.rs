//! Shared operator plumbing, and [`operators`] — the registry every
//! operator this crate defines is collected into.
//!
//! Every `Image`-in/`Image`-out operator in this crate ([`crate::resize`],
//! [`crate::blur`], [`crate::sobel`], [`crate::threshold`],
//! [`crate::morphology`], [`crate::color`], [`crate::draw`],
//! [`crate::camera`]) performs the exact same three steps around its own
//! one-line transform: decode the input payload as a
//! `std/media/v1/Image[pixel=…]`, run the transform, encode and publish the
//! result. `forward_image` (below) is that shared shape, so each operator's
//! own `on_event` is one call. [`crate::components::ComponentsOperator`] and
//! this crate's two PNG operators do not fit it — their output message
//! type is not `Image` — and write their own `on_event` instead.

use astrs_node_api::FromPayload as _;
use astrs_operator_api::{
    OpError, OpEvent, OpOutput, OpResult, OperatorRegistry, Status, register_operator,
};

use crate::blur::GaussianBlurOperator;
use crate::buffer::ImageBuffer;
use crate::camera::UndistortOperator;
use crate::color::ColorConvertOperator;
use crate::components::ComponentsOperator;
use crate::draw::DrawRectOperator;
use crate::error::VisionError;
use crate::morphology::MorphologyOperator;
use crate::png::{PngDecodeOperator, PngEncodeOperator};
use crate::resize::ResizeOperator;
use crate::sobel::SobelOperator;
use crate::threshold::ThresholdOperator;

/// Bridges this crate's own [`VisionError`] into [`OpError`], so a
/// transform's `crate::Result<T>` composes with `?` inside an operator's
/// `OpResult`-returning method instead of every call site spelling out
/// `.map_err(|e| OpError::failed(e.to_string()))` by hand.
///
/// Legal by the orphan rules the same way `astrs-data`'s own error types
/// wrap *into* [`VisionError`] (`#[from]` in [`crate::error::VisionError`]):
/// exactly one of the two types in a `From<A> for B` impl has to be local
/// to this crate, and here that is `VisionError`, not [`OpError`]. The
/// direction is new (this crate has no other reason to convert *out* of its
/// own error type), which is why it lives beside `forward_image` (below)
/// rather than in [`crate::error`].
impl From<VisionError> for OpError {
    fn from(source: VisionError) -> Self {
        OpError::failed(source.to_string())
    }
}

/// Decodes an [`OpEvent::Input`] payload as a `std/media/v1/Image[pixel=…]`
/// into an [`ImageBuffer`].
///
/// # Errors
///
/// [`OpError::Ipc`] when `payload` is not a valid Arrow IPC payload;
/// [`OpError::Encode`] when it decodes but is not shaped like an `Image`;
/// an [`OpError::Failed`] (via [`VisionError`]'s [`From`] impl above) when
/// it is shaped like one but carries a pixel format or sample width this
/// crate's 8-bit-only [`ImageBuffer`] has no equivalent for.
pub(crate) fn decode_image(payload: &[u8]) -> OpResult<ImageBuffer> {
    let batch = astrs_data::ipc::decode_payload(payload)?;
    let wire_image = astrs_node_api::message::Image::from_batch(&batch)?;
    Ok(ImageBuffer::from_message(&wire_image)?)
}

/// Encodes `image` as a `std/media/v1/Image[pixel=…]` and publishes it on
/// `output_id` with `metadata`.
///
/// # Errors
///
/// An [`OpError::Failed`] (via [`VisionError`]) when `image`'s format has
/// no wire representation ([`crate::PixelFormat::Yuyv`]/
/// [`crate::PixelFormat::Uyvy`] — see [`ImageBuffer::to_message`]);
/// [`OpError::Encode`] or [`OpError::InvalidOutputId`] otherwise.
pub(crate) fn encode_image(
    out: &mut OpOutput,
    output_id: &str,
    metadata: astrs_wire::Metadata,
    image: &ImageBuffer,
) -> OpResult<()> {
    let wire_image = image.to_message()?;
    let batch = wire_image.to_record_batch()?;
    out.send_batch(output_id, metadata, &batch)
}

/// The shared body of every `Image`-in/`Image`-out operator in this crate:
/// decode `event`'s payload, run `transform` over it, publish the result on
/// `output_id`.
///
/// Every [`OpEvent`] variant that carries no image ([`OpEvent::InputClosed`],
/// [`OpEvent::Reload`], [`OpEvent::ParamUpdate`]) is a no-op returning
/// [`Status::Continue`]; [`OpEvent::Stop`] returns [`Status::Finished`].
///
/// # Errors
///
/// As [`decode_image`] and [`encode_image`]; an [`OpError::Failed`] (via
/// [`VisionError`]) when `transform` itself fails.
pub(crate) fn forward_image(
    event: &OpEvent,
    out: &mut OpOutput,
    output_id: &str,
    transform: impl FnOnce(&ImageBuffer) -> crate::error::Result<ImageBuffer>,
) -> OpResult<Status> {
    match event {
        OpEvent::Input {
            metadata, payload, ..
        } => {
            let image = decode_image(payload)?;
            let result = transform(&image)?;
            encode_image(out, output_id, metadata.clone(), &result)?;
            Ok(Status::Continue)
        }
        OpEvent::Stop { .. } => Ok(Status::Finished),
        _ => Ok(Status::Continue),
    }
}

/// Every operator this crate registers, ready for `astrs-runtime` to build
/// by name (blueprint §9.3) — the same
/// `OperatorRegistry::from_entries([register_operator!(...), ...])` shape
/// `examples/module-composition` uses.
///
/// # Errors
///
/// [`OpError::DuplicateOperator`] — unreachable in practice (every name
/// below is a distinct literal), propagated rather than unwrapped so a
/// future copy-pasted entry fails loudly instead of silently shadowing an
/// existing one.
pub fn operators() -> OpResult<OperatorRegistry> {
    OperatorRegistry::from_entries([
        register_operator!(ResizeOperator),
        register_operator!(GaussianBlurOperator),
        register_operator!(SobelOperator),
        register_operator!(ThresholdOperator),
        register_operator!(MorphologyOperator),
        register_operator!(ComponentsOperator),
        register_operator!(DrawRectOperator),
        register_operator!(UndistortOperator),
        register_operator!(ColorConvertOperator),
        register_operator!(PngEncodeOperator),
        register_operator!(PngDecodeOperator),
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn every_operator_registers_under_a_distinct_name() {
        let registry = operators().unwrap();
        for name in [
            "ResizeOperator",
            "GaussianBlurOperator",
            "SobelOperator",
            "ThresholdOperator",
            "MorphologyOperator",
            "ComponentsOperator",
            "DrawRectOperator",
            "UndistortOperator",
            "ColorConvertOperator",
            "PngEncodeOperator",
            "PngDecodeOperator",
        ] {
            assert!(registry.contains(name), "{name} should be registered");
        }
    }

    #[test]
    fn a_vision_error_becomes_a_failed_op_error_carrying_its_message() {
        let source = VisionError::TooManyComponents { found: 3, max: 2 };
        let message = source.to_string();
        let error: OpError = source.into();
        assert!(matches!(error, OpError::Failed { message: got } if got == message));
    }
}
