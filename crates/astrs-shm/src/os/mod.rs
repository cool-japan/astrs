//! The platform layer.
//!
//! Everything in this crate that touches a syscall goes through here, and the
//! surface is kept deliberately small: three families of operation, one
//! portable Unix implementation, and per-platform files holding *only* the
//! calls that genuinely diverge.
//!
//! | Operation | Linux | macOS / other Unix | Windows |
//! |---|---|---|---|
//! | segment backing | `memfd_create` (+ shrink/grow seals) | `shm_open` with the short hashed name | `CreateFileMappingW` with the `Local\` hashed short name |
//! | mapping | `mmap`/`munmap` (shared) | same | `MapViewOfFile`/`UnmapViewOfFile` |
//! | doorbell | `eventfd` | `pipe` | — (needs `Win32_System_Threading`, not yet granted) |
//! | producer watch | `pidfd_open` + `poll` | `kqueue` `EVFILT_PROC`/`NOTE_EXIT` | — (needs `Win32_System_Threading`, not yet granted) |
//! | liveness fallback | `kill(pid, 0)` | `kill(pid, 0)` | — (see `crate::os::windows::process_alive`) |
//! | fd passing | `SCM_RIGHTS` | `SCM_RIGHTS` | — (attach is by name instead; see `crate::os::windows`) |
//!
//! [`crate::Segment`] is portable: on Windows it maps a named section object
//! (`crate::os::windows`), so segment creation, attach-by-name, generation
//! checking and teardown all work today. [`crate::Producer`], [`crate::
//! Consumer`], [`crate::Doorbell`] and [`crate::SegmentBroker`] stay
//! `#[cfg(unix)]`-only: their Windows counterparts need
//! `Win32_System_Threading` (`CreateEventW`/`WaitForSingleObject`/
//! `OpenProcess`), which this workspace does not currently enable — see
//! `crate::os::windows`'s module docs (a Windows-only module, linkable only
//! when docs are built on that target) for exactly what that gap means and
//! does not mean for crash reclamation. Everywhere the plane is truly absent
//! (no `Segment` either — every target this crate does not name above),
//! every constructor returns [`crate::ShmError::Unsupported`] and callers
//! degrade to the reliable daemon path (§6.3). The shared-memory plane is a
//! same-host optimisation, never a correctness requirement.
//!
//! # Why the mapping is read-write even for consumers
//!
//! Blueprint §6.2 says consumers "attach read-only", which is a statement
//! about the *payload*: a consumer never mutates a message. It is not a
//! statement about page protection, and it cannot be — the reclamation
//! protocol lives in shared atomics (§6.2 again: "drop-token protocol done in
//! shared atomics, not messages"), so a consumer must be able to write its
//! own cursor, its heartbeat, and the slot refcounts it pins. A `PROT_READ`
//! mapping would make zero-copy reads impossible to account for. Payload
//! immutability is instead enforced by the API: [`crate::Sample`] hands out
//! `&[u8]` and nothing else.

//! This module as a whole compiles on Unix and on Windows (both are named in
//! the table above); the non-portable stub in `crate::unsupported` covers
//! every *other* target, so that file never has to carry a third, empty
//! implementation of every function.

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(all(unix, target_os = "linux"))]
mod linux;

#[cfg(all(unix, target_os = "linux"))]
pub(crate) use linux::*;

#[cfg(all(unix, not(target_os = "linux")))]
mod bsd;

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) use bsd::*;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub(crate) use windows::*;
