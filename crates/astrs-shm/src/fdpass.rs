//! Descriptor passing over Unix sockets (`SCM_RIGHTS`).
//!
//! A Linux `memfd` segment has no name: the *only* way a consumer process can
//! reach it is a descriptor handed over by the daemon. That is what makes
//! §6.3's slow-start handshake possible at all — "the daemon *knows*
//! attachment state because it brokers the segment fds".
//!
//! # The one rule that bites everyone
//!
//! **Always send at least one byte of ordinary payload with the ancillary
//! data.** A `sendmsg` with an empty `iovec` is permitted to deliver nothing,
//! and the descriptors go with it. Every function here enforces that by
//! construction: [`send_with_fds`] refuses an empty payload.
//!
//! # Truncation
//!
//! If the receiver's ancillary buffer is too small, the kernel silently drops
//! the descriptors that did not fit (`MSG_CTRUNC`) — and a leaked descriptor
//! in the sender is a segment that never gets unlinked. [`recv_with_fds`]
//! therefore reports truncation as [`ShmError::Protocol`] rather than
//! returning a short list.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use std::os::fd::AsFd;
//! use std::os::unix::net::UnixStream;
//! use astrs_shm::fdpass::{recv_with_fds, send_with_fds};
//!
//! let (left, right) = UnixStream::pair().expect("socketpair");
//! let payload = b"segment";
//!
//! // Send this process's own stdout descriptor as a stand-in.
//! let stdout = std::io::stdout();
//! send_with_fds(left.as_fd(), payload, &[stdout.as_fd()])?;
//!
//! let mut buffer = [0u8; 32];
//! let (read, fds) = recv_with_fds(right.as_fd(), &mut buffer, 1)?;
//! assert_eq!(&buffer[..read], payload);
//! assert_eq!(fds.len(), 1);
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::io::Errno;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

use crate::error::{ShmError, ShmResult};

/// The largest number of descriptors one message may carry.
///
/// A broker reply carries at most two (the segment and a doorbell), so eight
/// is generous; the cap exists so a hostile peer cannot make the receiver
/// reserve an unbounded ancillary buffer.
pub const MAX_FDS_PER_MESSAGE: usize = 8;

/// Ancillary space for [`MAX_FDS_PER_MESSAGE`] descriptors.
const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MESSAGE));

/// Send `payload` together with `fds` over a connected Unix socket.
///
/// Returns the number of payload bytes accepted.
///
/// # Errors
///
/// - [`ShmError::Protocol`] if `payload` is empty (descriptors would be lost)
///   or `fds` exceeds [`MAX_FDS_PER_MESSAGE`].
/// - [`ShmError::Os`] if `sendmsg` fails.
pub fn send_with_fds(
    socket: BorrowedFd<'_>,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
) -> ShmResult<usize> {
    if payload.is_empty() {
        return Err(ShmError::protocol(
            "SCM_RIGHTS requires at least one payload byte; an empty iovec may drop the descriptors",
        ));
    }
    if fds.len() > MAX_FDS_PER_MESSAGE {
        return Err(ShmError::protocol(format!(
            "{} descriptors exceeds the {MAX_FDS_PER_MESSAGE} per-message limit",
            fds.len()
        )));
    }

    let mut space = [MaybeUninit::uninit(); CMSG_SPACE];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(ShmError::protocol(
            "ancillary buffer too small for the requested descriptors",
        ));
    }

    let iov = [IoSlice::new(payload)];
    loop {
        match sendmsg(socket, &iov, &mut control, SendFlags::empty()) {
            Ok(sent) => return Ok(sent),
            Err(Errno::INTR) => {}
            Err(errno) => return Err(ShmError::os("sendmsg", errno)),
        }
    }
}

/// Receive a message and up to `max_fds` descriptors.
///
/// Returns the number of payload bytes read and the descriptors received. A
/// zero-byte read means the peer closed the connection.
///
/// # Errors
///
/// - [`ShmError::Protocol`] if the kernel truncated the ancillary data, or
///   more descriptors arrived than `max_fds` allows.
/// - [`ShmError::Os`] if `recvmsg` fails.
pub fn recv_with_fds(
    socket: BorrowedFd<'_>,
    buffer: &mut [u8],
    max_fds: usize,
) -> ShmResult<(usize, Vec<OwnedFd>)> {
    if max_fds > MAX_FDS_PER_MESSAGE {
        return Err(ShmError::protocol(format!(
            "requested {max_fds} descriptors, above the {MAX_FDS_PER_MESSAGE} limit"
        )));
    }

    let mut space = [MaybeUninit::uninit(); CMSG_SPACE];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut iov = [IoSliceMut::new(buffer)];

    let received = loop {
        match recvmsg(socket, &mut iov, &mut control, RecvFlags::empty()) {
            Ok(received) => break received,
            Err(Errno::INTR) => {}
            Err(errno) => return Err(ShmError::os("recvmsg", errno)),
        }
    };

    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = message {
            fds.extend(rights);
        }
    }

    if received.flags.contains(ReturnFlags::CTRUNC) {
        return Err(ShmError::protocol(
            "ancillary data was truncated; descriptors were dropped by the kernel",
        ));
    }
    if fds.len() > max_fds {
        return Err(ShmError::protocol(format!(
            "peer sent {} descriptors, more than the {max_fds} expected",
            fds.len()
        )));
    }

    Ok((received.bytes, fds))
}

/// Receive exactly `buffer.len()` bytes, plus any descriptors that arrive
/// with the first fragment.
///
/// Stream sockets may split a message; the broker protocol frames its
/// messages with an explicit length precisely so this loop can exist.
///
/// # Errors
///
/// [`ShmError::Protocol`] if the peer closes before the buffer is full, plus
/// everything [`recv_with_fds`] can return.
pub fn recv_exact_with_fds(
    socket: BorrowedFd<'_>,
    buffer: &mut [u8],
    max_fds: usize,
) -> ShmResult<Vec<OwnedFd>> {
    let mut filled = 0;
    let mut fds = Vec::new();
    // Descriptors ride the *first* fragment only — [`send_all_with_fds`]
    // guarantees it — so the allowance drops to zero afterwards. Decrementing
    // it by how many arrived instead would refuse a legitimate single
    // descriptor whenever the payload happened to fragment.
    let mut allowed = max_fds;
    while filled < buffer.len() {
        let (read, mut more) = recv_with_fds(socket, &mut buffer[filled..], allowed)?;
        if read == 0 {
            return Err(ShmError::protocol(format!(
                "peer closed after {filled} of {} bytes",
                buffer.len()
            )));
        }
        fds.append(&mut more);
        filled += read;
        allowed = 0;
    }
    Ok(fds)
}

/// Send every byte of `payload`, with `fds` attached to the first fragment.
///
/// # Errors
///
/// As [`send_with_fds`].
pub fn send_all_with_fds(
    socket: BorrowedFd<'_>,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
) -> ShmResult<()> {
    let mut sent = 0;
    let mut pending = fds;
    while sent < payload.len() {
        let written = send_with_fds(socket, &payload[sent..], pending)?;
        if written == 0 {
            return Err(ShmError::protocol("peer accepted zero bytes"));
        }
        sent += written;
        // Descriptors ride the first fragment only; resending them would
        // duplicate them in the receiver.
        pending = &[];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    /// A descriptor whose identity is checkable after the round trip.
    fn probe() -> (OwnedFd, OwnedFd) {
        let (read, write) = crate::os::doorbell_pair().expect("doorbell");
        (read, write)
    }

    #[test]
    fn a_descriptor_survives_the_round_trip_and_still_works() {
        let (left, right) = UnixStream::pair().unwrap();
        let (reader, writer) = probe();

        send_with_fds(left.as_fd(), b"hello", &[writer.as_fd()]).unwrap();

        let mut buffer = [0u8; 16];
        let (read, fds) = recv_with_fds(right.as_fd(), &mut buffer, 1).unwrap();
        assert_eq!(&buffer[..read], b"hello");
        assert_eq!(fds.len(), 1);

        // The received descriptor rings the original doorbell.
        drop(writer);
        crate::os::doorbell_ring(fds[0].as_fd()).unwrap();
        assert!(crate::os::doorbell_drain(reader.as_fd()).unwrap() > 0);
    }

    #[test]
    fn several_descriptors_arrive_together() {
        let (left, right) = UnixStream::pair().unwrap();
        let probes: Vec<(OwnedFd, OwnedFd)> = (0..4).map(|_| probe()).collect();
        let writers: Vec<BorrowedFd<'_>> =
            probes.iter().map(|(_, writer)| writer.as_fd()).collect();

        send_with_fds(left.as_fd(), b"x", &writers).unwrap();
        let mut buffer = [0u8; 4];
        let (read, fds) = recv_with_fds(right.as_fd(), &mut buffer, 4).unwrap();
        assert_eq!(read, 1);
        assert_eq!(fds.len(), 4);

        for (index, fd) in fds.iter().enumerate() {
            crate::os::doorbell_ring(fd.as_fd()).unwrap();
            assert!(
                crate::os::doorbell_drain(probes[index].0.as_fd()).unwrap() > 0,
                "descriptor {index} did not survive in order"
            );
        }
    }

    #[test]
    fn an_empty_payload_is_refused_rather_than_losing_the_descriptor() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (_, writer) = probe();
        assert!(matches!(
            send_with_fds(left.as_fd(), b"", &[writer.as_fd()]),
            Err(ShmError::Protocol { .. })
        ));
    }

    #[test]
    fn too_many_descriptors_are_refused_on_both_sides() {
        let (left, right) = UnixStream::pair().unwrap();
        let probes: Vec<(OwnedFd, OwnedFd)> =
            (0..MAX_FDS_PER_MESSAGE + 1).map(|_| probe()).collect();
        let writers: Vec<BorrowedFd<'_>> = probes.iter().map(|(_, w)| w.as_fd()).collect();
        assert!(matches!(
            send_with_fds(left.as_fd(), b"x", &writers),
            Err(ShmError::Protocol { .. })
        ));

        let mut buffer = [0u8; 4];
        assert!(matches!(
            recv_with_fds(right.as_fd(), &mut buffer, MAX_FDS_PER_MESSAGE + 1),
            Err(ShmError::Protocol { .. })
        ));
    }

    #[test]
    fn receiving_more_descriptors_than_expected_is_an_error() {
        let (left, right) = UnixStream::pair().unwrap();
        let probes: Vec<(OwnedFd, OwnedFd)> = (0..3).map(|_| probe()).collect();
        let writers: Vec<BorrowedFd<'_>> = probes.iter().map(|(_, w)| w.as_fd()).collect();
        send_with_fds(left.as_fd(), b"x", &writers).unwrap();
        let mut buffer = [0u8; 4];
        assert!(matches!(
            recv_with_fds(right.as_fd(), &mut buffer, 1),
            Err(ShmError::Protocol { .. })
        ));
    }

    #[test]
    fn a_closed_peer_reads_zero_bytes() {
        let (left, right) = UnixStream::pair().unwrap();
        drop(left);
        let mut buffer = [0u8; 8];
        let (read, fds) = recv_with_fds(right.as_fd(), &mut buffer, 0).unwrap();
        assert_eq!(read, 0);
        assert!(fds.is_empty());
    }

    #[test]
    fn exact_reads_reassemble_a_fragmented_message() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let (_reader, writer) = probe();
        send_with_fds(left.as_fd(), b"abcd", &[writer.as_fd()]).unwrap();
        left.write_all(b"efgh").unwrap();

        let mut buffer = [0u8; 8];
        let fds = recv_exact_with_fds(right.as_fd(), &mut buffer, 1).unwrap();
        assert_eq!(&buffer, b"abcdefgh");
        assert_eq!(fds.len(), 1);
    }

    #[test]
    fn a_fragmented_read_still_accepts_its_single_descriptor() {
        // The regression this guards: an allowance decremented per fragment
        // would drop to zero after the first one and refuse the descriptor a
        // later fragment might carry — or, with `max_fds == 1`, refuse the
        // whole read.
        let (mut left, right) = UnixStream::pair().unwrap();
        let (reader, writer) = probe();
        send_with_fds(left.as_fd(), b"ab", &[writer.as_fd()]).unwrap();
        // A second, descriptor-free fragment completes the message.
        left.write_all(b"cdefgh").unwrap();

        let mut buffer = [0u8; 8];
        let fds = recv_exact_with_fds(right.as_fd(), &mut buffer, 1).unwrap();
        assert_eq!(&buffer, b"abcdefgh");
        assert_eq!(fds.len(), 1);
        crate::os::doorbell_ring(fds[0].as_fd()).unwrap();
        assert!(crate::os::doorbell_drain(reader.as_fd()).unwrap() > 0);
    }

    #[test]
    fn exact_reads_report_a_short_stream() {
        let (left, right) = UnixStream::pair().unwrap();
        send_with_fds(left.as_fd(), b"ab", &[]).unwrap();
        drop(left);
        let mut buffer = [0u8; 8];
        assert!(matches!(
            recv_exact_with_fds(right.as_fd(), &mut buffer, 0),
            Err(ShmError::Protocol { .. })
        ));
    }

    #[test]
    fn send_all_delivers_a_large_payload_with_one_descriptor() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_reader, writer) = probe();
        let payload: Vec<u8> = (0..4096u32).map(|value| value as u8).collect();

        let sender = std::thread::spawn(move || {
            send_all_with_fds(left.as_fd(), &payload, &[writer.as_fd()]).map(|()| left)
        });

        let mut buffer = vec![0u8; 4096];
        let fds = recv_exact_with_fds(right.as_fd(), &mut buffer, 1).unwrap();
        assert_eq!(fds.len(), 1);
        for (index, byte) in buffer.iter().enumerate() {
            assert_eq!(*byte, index as u8);
        }
        let _left = sender.join().expect("sender thread").expect("send");
    }
}
