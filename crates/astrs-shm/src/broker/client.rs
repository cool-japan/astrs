//! The node side of the broker channel.
//!
//! Two types, and the split between them is the whole design:
//!
//! - [`SegmentClient`] is request/reply — attach, register a doorbell, close
//!   a segment, ping. Every method sends one frame and reads exactly one
//!   reply.
//! - [`ProducerChannel`] is push-only. A producer *claims* a segment, which
//!   converts its client connection into a channel the broker only writes to,
//!   carrying consumer doorbell descriptors into the producer's own process.
//!
//! Mixing the two on one connection would mean a reply read racing an
//! unsolicited push, so [`SegmentClient::claim_producer`] consumes the client
//! and hands back the channel — the type system enforces the transition.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::doorbell::DoorbellRinger;
use crate::error::{ShmError, ShmResult};
use crate::fdpass::{recv_exact_with_fds, recv_with_fds, send_all_with_fds};
use crate::key::SegmentKey;
use crate::os;
use crate::protocol::{
    AttachReply, AttachRequest, BROKER_HEADER_LEN, BrokerFrame, DoorbellRegistration, Opcode,
    ProducerClaim, StatusReply,
};
use crate::segment::Segment;

/// How long a relayed frame's payload may take to arrive after its header.
///
/// Bounded because the alternative is a producer's event loop blocked on a
/// dead broker; generous because the payload is 32 bytes on a local socket
/// and only a scheduling accident splits it from its header.
const RELAY_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(200);

/// The longest single wait inside [`RELAY_FRAGMENT_TIMEOUT`].
const RELAY_POLL_SLICE: Duration = Duration::from_millis(10);

/// The node side of the broker channel.
#[derive(Debug)]
pub struct SegmentClient {
    stream: UnixStream,
}

impl SegmentClient {
    /// Connect to a broker.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if the socket cannot be reached.
    pub fn connect(path: impl AsRef<Path>) -> ShmResult<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .map_err(|err| ShmError::io("connect to the segment broker", err))?;
        Ok(Self { stream })
    }

    /// Ask the broker for a segment's descriptor and map it.
    ///
    /// The returned [`Segment`] has already been validated against `key`'s
    /// generation and digest.
    ///
    /// # Errors
    ///
    /// - [`ShmError::BrokerRefused`] — the broker does not hold that segment.
    /// - [`ShmError::StaleGeneration`] / [`ShmError::KeyMismatch`] — the
    ///   descriptor does not map what was asked for.
    /// - [`ShmError::Os`] / [`ShmError::Protocol`] — transport failures.
    pub fn attach(&mut self, key: &SegmentKey) -> ShmResult<Segment> {
        let request = AttachRequest {
            key_digest: key.digest(),
            generation: key.generation(),
        };
        let frame = BrokerFrame::new(Opcode::Attach, request.encode());
        send_all_with_fds(self.stream.as_fd(), &frame.encode(), &[])?;

        let (opcode, payload, mut fds) = self.read_reply(1)?;
        match opcode {
            Opcode::AttachReply => {
                let reply = AttachReply::decode(&payload)?;
                let fd = fds.pop().ok_or_else(|| {
                    ShmError::protocol("attach reply carried no segment descriptor")
                })?;
                let segment = Segment::open_fd(fd, Some(key))?;
                if reply.key_digest != key.digest() || reply.generation != key.generation() {
                    return Err(ShmError::KeyMismatch {
                        expected: key.digest(),
                        found: reply.key_digest,
                    });
                }
                Ok(segment)
            }
            Opcode::Refused => Err(ShmError::BrokerRefused {
                reason: String::from_utf8_lossy(&payload).into_owned(),
            }),
            other => Err(ShmError::protocol(format!(
                "unexpected {} in answer to an attach",
                other.as_str()
            ))),
        }
    }

    /// Hand a consumer's doorbell write end to the broker, which registers it
    /// with the producer.
    ///
    /// # Errors
    ///
    /// [`ShmError::BrokerRefused`] if the broker does not hold the segment,
    /// plus the transport errors of [`SegmentClient::attach`].
    pub fn register_doorbell(
        &mut self,
        key: &SegmentKey,
        consumer_index: u32,
        token: u32,
        ringer: DoorbellRinger,
    ) -> ShmResult<()> {
        let registration = DoorbellRegistration {
            key_digest: key.digest(),
            generation: key.generation(),
            consumer_index,
            token,
        };
        let frame = BrokerFrame::new(Opcode::RegisterDoorbell, registration.encode());
        let fd = ringer.into_fd();
        send_all_with_fds(self.stream.as_fd(), &frame.encode(), &[fd.as_fd()])?;
        drop(fd);
        self.expect_ack()
    }

    /// Ask the broker to mark a segment closed.
    ///
    /// # Errors
    ///
    /// As [`SegmentClient::register_doorbell`].
    pub fn close_segment(&mut self, key: &SegmentKey) -> ShmResult<()> {
        let request = AttachRequest {
            key_digest: key.digest(),
            generation: key.generation(),
        };
        let frame = BrokerFrame::new(Opcode::CloseSegment, request.encode());
        send_all_with_fds(self.stream.as_fd(), &frame.encode(), &[])?;
        self.expect_ack()
    }

    /// Claim the producer role for a segment, turning this connection into
    /// the channel the broker pushes consumer doorbells down.
    ///
    /// This is what makes blocking receives work *across processes*. A
    /// [`crate::DoorbellRegistry`] holds file descriptors and so cannot live
    /// in shared memory; a producer in a different process from the broker
    /// would otherwise never learn that a consumer wants to be woken, and
    /// every consumer would silently fall back to bounded polling. Claiming
    /// also stamps this process's pid into the segment header, so the
    /// broker's liveness watch and every consumer's
    /// [`Segment::producer_alive`] check aim at the right process.
    ///
    /// The returned [`ProducerChannel`] must be polled — cheaply and
    /// non-blockingly — from the producer's loop; see
    /// [`ProducerChannel::poll`].
    ///
    /// # Errors
    ///
    /// [`ShmError::BrokerRefused`] if the broker does not hold the segment,
    /// plus the transport errors of [`SegmentClient::attach`].
    pub fn claim_producer(
        mut self,
        key: &SegmentKey,
        segment: Arc<Segment>,
    ) -> ShmResult<ProducerChannel> {
        let claim = ProducerClaim {
            key_digest: key.digest(),
            generation: key.generation(),
            pid: os::current_pid(),
        };
        let frame = BrokerFrame::new(Opcode::ClaimProducer, claim.encode());
        send_all_with_fds(self.stream.as_fd(), &frame.encode(), &[])?;
        self.expect_ack()?;
        // From here on the broker only pushes; a blocking read would stall
        // the producer, so the channel is switched to non-blocking.
        self.stream
            .set_nonblocking(true)
            .map_err(|err| ShmError::io("set producer channel non-blocking", err))?;
        Ok(ProducerChannel {
            stream: self.stream,
            segment,
        })
    }

    /// Probe the broker.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] if the answer is not a pong.
    pub fn ping(&mut self) -> ShmResult<()> {
        send_all_with_fds(
            self.stream.as_fd(),
            &BrokerFrame::empty(Opcode::Ping).encode(),
            &[],
        )?;
        let (opcode, _, _) = self.read_reply(0)?;
        if opcode == Opcode::Pong {
            Ok(())
        } else {
            Err(ShmError::protocol(format!(
                "expected a pong, got {}",
                opcode.as_str()
            )))
        }
    }

    /// Ask the broker for a summary of what it holds.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] if the answer is malformed.
    pub fn status(&mut self) -> ShmResult<StatusReply> {
        send_all_with_fds(
            self.stream.as_fd(),
            &BrokerFrame::empty(Opcode::Status).encode(),
            &[],
        )?;
        let (opcode, payload, _) = self.read_reply(0)?;
        if opcode == Opcode::StatusReply {
            StatusReply::decode(&payload)
        } else {
            Err(ShmError::protocol(format!(
                "expected a status reply, got {}",
                opcode.as_str()
            )))
        }
    }

    fn expect_ack(&mut self) -> ShmResult<()> {
        let (opcode, payload, _) = self.read_reply(0)?;
        match opcode {
            Opcode::Ack => Ok(()),
            Opcode::Refused => Err(ShmError::BrokerRefused {
                reason: String::from_utf8_lossy(&payload).into_owned(),
            }),
            other => Err(ShmError::protocol(format!(
                "expected an ack, got {}",
                other.as_str()
            ))),
        }
    }

    fn read_reply(&mut self, max_fds: usize) -> ShmResult<(Opcode, Vec<u8>, Vec<OwnedFd>)> {
        let mut header = [0u8; BROKER_HEADER_LEN];
        let fds = recv_exact_with_fds(self.stream.as_fd(), &mut header, max_fds)?;
        let (opcode, len) = BrokerFrame::decode_header(&header)?;
        let mut payload = vec![0u8; len];
        if len > 0 {
            recv_exact_with_fds(self.stream.as_fd(), &mut payload, 0)?;
        }
        Ok((opcode, payload, fds))
    }
}

/// A producer's push-only channel to the broker.
///
/// Created by [`SegmentClient::claim_producer`]. The broker forwards every
/// consumer doorbell registration for the claimed segment down this channel;
/// [`ProducerChannel::poll`] drains whatever has arrived and registers it in
/// this process's own [`crate::DoorbellRegistry`], which is the one
/// [`crate::Producer::commit`] reads.
///
/// # Where to call `poll`
///
/// Once per event-loop turn, or once per batch of commits. It is a single
/// non-blocking `recvmsg` when nothing has arrived, which is the steady
/// state: doorbells are registered at attach time, not per message.
/// [`ProducerChannel::as_fd`] is pollable, so a node with its own reactor can
/// wait on it instead.
#[derive(Debug)]
pub struct ProducerChannel {
    stream: UnixStream,
    segment: Arc<Segment>,
}

impl ProducerChannel {
    /// The segment this channel wires doorbells into.
    #[must_use]
    pub fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// The pollable descriptor, for a caller folding this into its own event
    /// loop.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// Register every doorbell the broker has pushed since the last call.
    ///
    /// Returns how many were registered. Never blocks.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] if the broker sent something malformed, or
    /// [`ShmError::Os`] for a transport failure other than "nothing to read".
    pub fn poll(&self) -> ShmResult<usize> {
        let mut registered = 0;
        loop {
            let mut header = [0u8; BROKER_HEADER_LEN];
            let fds = match recv_exact_with_fds(self.stream.as_fd(), &mut header, 1) {
                Ok(fds) => fds,
                Err(ShmError::Os { source, .. }) if source.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(registered);
                }
                // The broker closed, or a partial frame is pending. Either way
                // there is nothing more to register right now; a closed broker
                // is reported by the segment's own `closed` flag, not here.
                Err(ShmError::Protocol { .. }) => return Ok(registered),
                Err(err) => return Err(err),
            };
            let (opcode, len) = BrokerFrame::decode_header(&header)?;
            let mut payload = vec![0u8; len];
            if len > 0 {
                // The header arrived, so the rest of this frame is in flight;
                // a short blocking read here is bounded by one frame.
                self.read_payload_blocking(&mut payload)?;
            }
            if opcode != Opcode::RegisterDoorbell {
                // Forward compatibility: an opcode this build does not push
                // for is ignored rather than fatal.
                continue;
            }
            let registration = DoorbellRegistration::decode(&payload)?;
            let Some(fd) = fds.into_iter().next() else {
                return Err(ShmError::protocol(
                    "relayed doorbell registration carried no descriptor",
                ));
            };
            self.segment.doorbells().register(
                registration.consumer_index,
                registration.token,
                DoorbellRinger::from_fd(fd),
            );
            registered += 1;
        }
    }

    /// Read the rest of a frame whose header has already arrived.
    ///
    /// The channel is non-blocking, so a fragmented push has to be waited
    /// for explicitly. The wait is bounded: a broker that dies mid-frame must
    /// not wedge a producer's event loop, and the alternative — treating a
    /// partial frame as "nothing arrived" — would desynchronise the stream
    /// for every later push.
    fn read_payload_blocking(&self, payload: &mut [u8]) -> ShmResult<()> {
        let deadline = Instant::now() + RELAY_FRAGMENT_TIMEOUT;
        let mut filled = 0;
        while filled < payload.len() {
            match recv_with_fds(self.stream.as_fd(), &mut payload[filled..], 0) {
                Ok((0, _)) => {
                    return Err(ShmError::protocol(format!(
                        "the broker closed after {filled} of {} payload bytes",
                        payload.len()
                    )));
                }
                Ok((read, _)) => filled += read,
                Err(ShmError::Os { source, .. }) if source.kind() == io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(ShmError::protocol(format!(
                            "only {filled} of {} relayed payload bytes arrived",
                            payload.len()
                        )));
                    }
                    os::wait_readable(
                        self.stream.as_fd(),
                        Some((deadline - now).min(RELAY_POLL_SLICE)),
                    )?;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}
