//! A helper shared by every [`astrs_operator_api::Operator`] in this crate:
//! decoding an incoming event payload into a typed
//! [`astrs_data::AstrsMessage`], the first step of nearly every `on_event`
//! implementation here.

use astrs_data::AstrsMessage;
use astrs_operator_api::OpResult;

/// Decodes `payload` — the raw bytes an
/// [`astrs_operator_api::OpEvent::Input`] carries — into a typed message.
///
/// # Errors
///
/// [`astrs_operator_api::OpError::Ipc`] if `payload` is not a well-formed,
/// single-batch Arrow IPC stream, or [`astrs_operator_api::OpError::Encode`]
/// if the decoded batch does not match `M`'s expected columnar layout.
pub(crate) fn decode<M: AstrsMessage>(payload: &[u8]) -> OpResult<M> {
    let batch = astrs_data::ipc::decode_payload(payload)?;
    Ok(M::from_record_batch(&batch)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::message::Frame;

    #[test]
    fn decode_round_trips_a_frame_through_ipc_bytes() {
        let frame = Frame::new(vec![1.0_f32, 2.0, 3.0]);
        let batch = frame.to_record_batch().unwrap();
        let bytes = astrs_data::ipc::encode_payload(&batch).unwrap();
        let decoded: Frame = decode(&bytes).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn decode_reports_malformed_bytes_as_an_ipc_error() {
        let err = decode::<Frame>(b"not arrow ipc").unwrap_err();
        assert!(matches!(err, astrs_operator_api::OpError::Ipc(_)));
    }
}
