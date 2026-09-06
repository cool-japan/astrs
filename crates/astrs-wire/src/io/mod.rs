//! Framed I/O: the frame format applied to a byte stream, blocking and async.
//!
//! | Module | Contents |
//! |---|---|
//! | [`buffer`] | [`FrameBuffer`] — the shared stream-reassembly engine |
//! | [`sync_io`] | [`FrameReader`], [`FrameWriter`] over `std::io` |
//! | [`async_io`] | [`AsyncFrameReader`], [`AsyncFrameWriter`] over `tokio::io` |
//!
//! Both flavours exist because both are needed: the daemon, the coordinator and
//! `astrs-transport` are tokio programs, while the CLI's one-shot request, the
//! replay of a recorded stream (§14) and most test harnesses are simpler
//! blocking code. Sharing [`FrameBuffer`] between them means there is still
//! exactly one implementation of "where does this frame end".
//!
//! # What the readers guarantee
//!
//! - **A frame is never handed out until it is complete and checked.** Length,
//!   limits and checksum are all verified before the payload is exposed.
//!   A stream that ends mid-frame is [`crate::WireError::Truncated`], never a
//!   short payload.
//! - **A malformed header fails immediately.** A reader does not wait for bytes
//!   that could never make the frame valid, and does not attempt to
//!   resynchronise by hunting for the next magic — a stream that lost framing
//!   has lost it.
//! - **The declared length is bounded before it is allocated.** A peer claiming
//!   a 4 GiB payload is refused against [`crate::FrameLimits`] before the buffer
//!   grows.
//!
//! # Sizing an unauthenticated socket
//!
//! That last guarantee is only as strong as the limit it checks against. A
//! reader opened with the default policy will grow its buffer to the full
//! 64 MiB ceiling for a peer that declares it — which is correct *after* the
//! handshake, and generous before one. An endpoint that accepts connections
//! should therefore open with a small ceiling, complete the handshake, and
//! widen to the negotiated budget once it knows who it is talking to (§7.2):
//!
//! ```
//! use astrs_wire::{FrameLimits, FrameReader, NegotiatedLimits};
//!
//! // Before the handshake: enough for a `Hello`, and nothing more.
//! let mut reader = FrameReader::new(
//!     std::io::empty(),
//!     FrameLimits::network().with_max_payload_bytes(64 * 1024),
//! );
//!
//! // After it: whatever the two ends agreed to.
//! let agreed = NegotiatedLimits::network().with_max_payload_bytes(4 << 20);
//! reader.set_limits(agreed.to_frame_limits());
//! assert_eq!(reader.limits().max_payload_bytes(), 4 << 20);
//! ```
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlReply, ControlRequest, FrameLimits, FrameReader, FrameWriter};
//!
//! // A CLI's request/response exchange, over any blocking transport.
//! let limits = FrameLimits::uds();
//! let mut to_server: Vec<u8> = Vec::new();
//! FrameWriter::new(&mut to_server, limits).send(&ControlRequest::List { all: false })?;
//!
//! let mut server_side = FrameReader::new(to_server.as_slice(), limits);
//! let request: ControlRequest = server_side.read_message()?.expect("a request");
//! assert!(request.is_read_only());
//!
//! let mut to_client: Vec<u8> = Vec::new();
//! FrameWriter::new(&mut to_client, limits).send(&ControlReply::Ok)?;
//!
//! let mut client_side = FrameReader::new(to_client.as_slice(), limits);
//! assert_eq!(client_side.read_message::<ControlReply>()?, Some(ControlReply::Ok));
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

pub mod async_io;
pub mod buffer;
pub mod sync_io;

pub use async_io::{AsyncFrameReader, AsyncFrameWriter};
pub use buffer::{DEFAULT_BUFFER_CAPACITY, FrameBuffer};
pub use sync_io::{FrameReader, FrameWriter};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::frame::FrameLimits;
    use crate::messages::samples;
    use crate::messages::{AnyMessage, ControlRequest, WireMessage};

    /// The whole protocol, written by one flavour and read by the other.
    #[test]
    fn the_two_flavours_agree_frame_for_frame() {
        let limits = FrameLimits::network();

        // Write every sample of every family with the blocking writer.
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire, limits);
        for request in samples::control_requests().unwrap() {
            writer.queue(&request).unwrap();
        }
        for reply in samples::control_replies().unwrap() {
            writer.queue(&reply).unwrap();
        }
        for event in samples::coordinator_events().unwrap() {
            writer.queue(&event).unwrap();
        }
        for event in samples::daemon_events().unwrap() {
            writer.queue(&event).unwrap();
        }
        for request in samples::node_requests().unwrap() {
            writer.queue(&request).unwrap();
        }
        for event in samples::node_events().unwrap() {
            writer.queue(&event).unwrap();
        }
        for event in samples::peer_events().unwrap() {
            writer.queue(&event).unwrap();
        }
        writer
            .queue(&samples::sample_data_frame().unwrap())
            .unwrap();
        writer.queue(&samples::sample_log_frame().unwrap()).unwrap();
        writer
            .queue(&samples::sample_telemetry_frame().unwrap())
            .unwrap();
        let expected_frames = writer.frames_written();
        writer.flush().unwrap();

        // Read them all back with the async reader.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(async {
            let mut reader = AsyncFrameReader::new(wire.as_slice(), limits);
            let mut seen = 0u64;
            while let Some(frame) = reader.read_frame().await.unwrap() {
                let message = AnyMessage::from_frame(&frame.as_view()).unwrap();
                assert_eq!(message.kind(), frame.kind());
                seen += 1;
            }
            assert_eq!(seen, expected_frames);
        });
    }

    #[test]
    fn a_buffer_can_be_driven_by_hand_by_a_datagram_transport() {
        // Not every transport is a stream: a QUIC datagram arrives whole, and a
        // caller can push it straight into the buffer.
        let limits = FrameLimits::uds();
        let mut buffer = FrameBuffer::new(limits);
        let bytes = ControlRequest::List { all: true }
            .to_frame(crate::frame::FrameFlags::EMPTY, &limits)
            .unwrap();
        buffer.push(&bytes);
        let view = buffer.next_frame().unwrap().expect("a complete frame");
        assert_eq!(
            ControlRequest::from_frame(&view).unwrap(),
            ControlRequest::List { all: true }
        );
    }
}
