//! The Windows half of the platform layer: named file mappings.
//!
//! Blueprint §22 (0.2.0) asks for a Windows same-host data plane on top of
//! the same ring layout the Unix plane uses. This module supplies the three
//! primitives [`crate::segment::Segment`] needs — create/open a named
//! mapping object, map it, unmap it — using `CreateFileMappingW` /
//! `OpenFileMappingW` / `MapViewOfFile` / `UnmapViewOfFile` /
//! `CloseHandle`, the Windows counterparts of `shm_open` / `mmap` / `munmap`.
//!
//! # What this module does *not* provide, and why
//!
//! [`crate::Producer`], [`crate::Consumer`], [`crate::Doorbell`] and
//! [`crate::SegmentBroker`] stay `#[cfg(unix)]`-only. Their Windows
//! counterparts need `CreateEventW`/`SetEvent`/`WaitForSingleObject` (a
//! doorbell) and `OpenProcess`/`GetExitCodeProcess` (a liveness watch) —
//! every one of them lives in the `Win32_System_Threading` feature of
//! `windows-sys`, which this workspace does not currently enable (only
//! `Win32_Foundation`, `Win32_System_Memory` and `Win32_Security` are). Segment
//! *creation, mapping, and attach-by-name* need none of that: this module
//! covers exactly the primitives the granted feature set can reach, and no
//! further. A `Segment` built here is fully usable from a single process
//! today (see [`crate::segment`]'s tests), and is the foundation a follow-up
//! wave completes once `Win32_System_Threading` is added.
//!
//! # Naming: the `Local\` kernel-object namespace
//!
//! POSIX shared memory has one flat namespace per §6.2's Unix plane
//! ([`crate::key::SegmentName`], `/astrs<base32 digest>`). Windows kernel
//! objects are namespaced instead: `Local\` is the per-session namespace (no
//! special privilege required, and invisible across Terminal Services
//! sessions — the Windows analogue of "this segment is for one host's
//! processes", which is all §6.2 ever assumed), while `Global\` requires the
//! `SeCreateGlobalPrivilege` most service accounts do not hold. This module
//! therefore mirrors the existing hashed short name
//! ([`crate::key::SegmentName::from_digest`]) into the `Local\` namespace by
//! stripping the leading POSIX `/` and prefixing `Local\` in its place:
//!
//! ```text
//! POSIX:   /astrs<24 base32 chars>
//! Windows: Local\astrs<24 base32 chars>
//! ```
//!
//! Windows object names tolerate far more length and character variety than
//! Darwin's `PSHMNAMLEN` (`MAX_PATH`, 260 chars, is the practical ceiling for
//! `\`-free names, and the existing 30-byte name is nowhere near it), so no
//! new truncation risk is introduced; the header's 128-bit key digest
//! ([`crate::header::SegmentHeader::key_digest`]) remains the authoritative
//! check either way, exactly as on Unix ([`crate::key`]'s module docs).
//!
//! # Crash reclamation: what carries over from the Unix plane, and what does
//! # not
//!
//! The ring's *logic* — the slot gate ([`crate::slot`]), the generation
//! stamp ([`crate::key::SegmentKey::generation`]), the consumer table's
//! drop-token protocol ([`crate::consumer_table`]) — is platform-independent
//! and every atomic operation in it is honoured identically once a Windows
//! mapping exists, because [`crate::header::SegmentHeader`] and
//! [`crate::slot::SlotHeader`] are `#[repr(C)]` structs of `std::sync::atomic`
//! types with no OS-specific behaviour. What differs is *how a dead
//! participant's resources get cleaned up*, and that split is worth stating
//! precisely:
//!
//! | Mechanism | Unix plane | Windows plane (this module) |
//! |---|---|---|
//! | Slot pin/reclaim ([`crate::slot`]) | Atomics in shared memory | **Identical** — no OS involvement either way |
//! | Consumer drop-token eviction ([`crate::consumer_table`]) | Atomics in shared memory, gated on `kill(pid, 0)` | **Identical protocol**; liveness check needs `Win32_System_Threading` (not yet wired — see above) |
//! | Stale generation detection ([`Segment::verify_identity`](crate::segment::Segment::verify_identity)) | Header digest + generation compare | **Identical** — pure memory comparison, no syscall |
//! | Backing object survives a crashed creator | A named POSIX object (`Backing::Named`) leaks until explicitly unlinked or the machine reboots; an anonymous `memfd` (`Backing::Memfd`) is reference-counted by the kernel and vanishes with the last handle | A named section is reference-counted by the kernel *object manager*: it survives exactly as long as at least one process (or the daemon broker, once ported) holds an open [`std::os::windows::io::OwnedHandle`] to it, and is deleted automatically once every handle closes — **closer to `memfd` than to POSIX named memory**, with no leak-on-crash case to document |
//! | Robust futex / `PTHREAD_MUTEX_ROBUST`-style automatic unlock | Not used — §6.2 deliberately avoids OS mutexes; see [`crate::slot`]'s module docs for why the CAS-gate design needs no robust-futex equivalent at all | **N/A for the same reason.** There is no lock to abandon: the gate is a plain atomic CAS, and a crashed producer or consumer leaves the gate exactly where its last successful CAS put it. This is why the "no robust futex on Windows" risk named in the blueprint does not, in fact, apply to this protocol — it only applies to lock-based designs, which this one is not. |
//!
//! In short: the one asymmetry that is real (rather than a restated
//! non-issue) is that a `Local\`-namespaced section, like a `memfd`, is
//! reclaimed by the kernel itself once every handle is gone — there is no
//! Windows equivalent of "an orphaned POSIX shared-memory *name* outliving
//! every mapping", so [`Segment::unlink`](crate::segment::Segment::unlink)
//! is a documented no-op on this platform (see its Windows-specific doc
//! comment). Everything else — pin/reclaim, generation staleness, the
//! consumer table's drop tokens — is the same code, running unmodified,
//! because none of it was ever OS-specific to begin with.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};

use crate::config::Backing;
use crate::error::{ShmError, ShmResult};
use crate::key::SegmentName;

/// The backing [`crate::Segment`] creation resolves [`Backing::Auto`] to.
///
/// There is no `memfd_create` equivalent reachable from the granted feature
/// set (`CreateFileMapping2`'s `extendedparameters` route to an anonymous,
/// swap-file-backed mapping needs nothing beyond what is already granted, but
/// a *named* mapping is what this module implements first because it is what
/// [`crate::segment::Segment::attach`] and the node-side "no broker" fallback
/// path already assume — see `astrs-node-api`'s `Segment::open_named` use).
/// Always [`Backing::Named`], mirroring the BSD/Darwin resolution
/// ([`crate::os::bsd::AUTO_BACKING`]) for the same reason: no anonymous path
/// is wired yet.
pub(crate) const AUTO_BACKING: Backing = Backing::Named;

/// Anonymous segments are not implemented on this platform yet.
///
/// # Errors
///
/// Always [`ShmError::Unsupported`]. Callers reach this only by asking for
/// [`Backing::Memfd`] explicitly; [`Backing::Auto`] resolves to
/// [`Backing::Named`] here (see [`AUTO_BACKING`]).
pub(crate) fn create_anonymous(_label: &str) -> ShmResult<OwnedHandle> {
    Err(ShmError::unsupported("CreateFileMapping (anonymous)"))
}

/// No-op: a named section has no POSIX-style sealing analogue, and the
/// mapping's size is fixed at `CreateFileMappingW` time — there is nothing
/// later to seal.
///
/// Unlike [`create_anonymous`], which `Segment::create_backing` calls
/// defensively even though it always errs, nothing calls this today:
/// sealing only makes sense *after* a successful anonymous creation, and
/// `create_anonymous` never succeeds on this platform yet. Kept — rather
/// than deleted — for parity with every other platform module's
/// `seal_anonymous`, and so the day `create_anonymous` is implemented for
/// real, this is already the function that wires into it.
#[expect(
    dead_code,
    reason = "no caller until create_anonymous can succeed; see doc comment"
)]
pub(crate) const fn seal_anonymous(_handle: &OwnedHandle) {}

/// Translate a [`SegmentName`] into the `Local\` kernel-object namespace.
///
/// See the module docs for why `Local\` (session-local, no special
/// privilege) rather than `Global\`.
fn windows_object_name(name: &SegmentName) -> Vec<u16> {
    // `SegmentName` always starts with `/` (enforced by
    // `SegmentName::new`/`from_digest`); the Windows form drops it in favour
    // of the `Local\` prefix.
    let stripped = name.as_str().strip_prefix('/').unwrap_or(name.as_str());
    let mut wide: Vec<u16> = "Local\\".encode_utf16().collect();
    wide.extend(stripped.encode_utf16());
    wide.push(0);
    wide
}

/// Create a named section object, sized to `len` bytes, and map it.
///
/// Windows has no `shm_open` + `ftruncate` + `mmap` three-step: a section's
/// size is fixed at creation and the first successful mapping already covers
/// the whole object, so this single call does the work
/// [`crate::os::unix::create_named`] + [`crate::os::unix::set_len`] +
/// [`crate::os::unix::map_shared`] together perform on Unix. The caller
/// ([`crate::segment::Segment::create_for_pid`]) still calls them as
/// logically separate steps on Unix; on Windows the size is threaded through
/// at creation instead.
///
/// A leftover object with the same name is a stale segment from a process
/// that died without every handle closing — reference-counted kernel objects
/// make this rare, but `CreateFileMappingW` on an existing name returns a
/// handle to the *existing* object with `GetLastError() ==
/// ERROR_ALREADY_EXISTS`, silently ignoring the requested size. Because
/// segment names embed the generation (§6.2, [`crate::key`]'s module docs), a
/// name collision can only mean a previous incarnation of *this exact* name
/// is still referenced somewhere; that is reported as a typed error rather
/// than silently handing back a mapping whose size does not match what the
/// caller asked for — the same "typed error rather than a wild pointer"
/// discipline [`crate::layout::SegmentLayout::validate_header`] applies to a
/// corrupt header.
///
/// # Errors
///
/// [`ShmError::Os`] if `CreateFileMappingW` or `MapViewOfFile` fails, or if
/// the name is already in use by a live object of a different generation.
pub(crate) fn create_named_mapped(
    name: &SegmentName,
    len: u64,
) -> ShmResult<(OwnedHandle, NonNull<u8>)> {
    let wide_name = windows_object_name(name);
    let size_high = (len >> 32) as u32;
    let size_low = (len & 0xFFFF_FFFF) as u32;

    // A security descriptor of `NULL` with `bInheritHandle == FALSE` gives
    // the object the creating process's default DACL — sufficient here
    // because `Local\` already confines visibility to this session, and no
    // child process is expected to inherit the raw handle (every consumer
    // reaches the object by name, via `open_named_mapped`, exactly like the
    // Unix named-`shm_open` path).
    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 0,
    };

    // SAFETY: `security_attributes` is a valid, fully initialised
    // `SECURITY_ATTRIBUTES` whose `nLength` matches its own size;
    // `INVALID_HANDLE_VALUE` for `hfile` is the documented way to request a
    // pagefile-backed (rather than file-backed) section; `wide_name` is a
    // NUL-terminated UTF-16 buffer that outlives the call.
    let raw = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::addr_of_mut!(security_attributes),
            PAGE_READWRITE,
            size_high,
            size_low,
            wide_name.as_ptr(),
        )
    };
    if raw.is_null() {
        return Err(ShmError::io(
            "CreateFileMappingW",
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: `raw` is a valid, freshly returned kernel-object handle that no
    // other code has taken ownership of yet; wrapping it in `OwnedHandle`
    // makes `CloseHandle` run exactly once, on drop.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };

    // `CreateFileMappingW` on a name that already exists returns a handle to
    // the *existing* section (silently ignoring the size and protection
    // requested) and sets the last error to `ERROR_ALREADY_EXISTS`. Since
    // names are per-generation, this can only be a genuine stale-name
    // collision — treat it exactly like the Unix `EEXIST` case in
    // `crate::os::unix::create_named`, but as a typed error rather than a
    // delete-and-retry: unlike a POSIX name, closing this handle only
    // *unreferences* the object, which is not enough to unlink it out from
    // under whatever other process is still holding it, so a delete-and-retry
    // would not actually free the name.
    // SAFETY: `GetLastError` is always safe to call and reflects the most
    // recent failing (or, here, informational) API call.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        drop(handle);
        return Err(ShmError::invalid_config(format!(
            "a section named {:?} already exists (a previous incarnation's \
             handles have not all closed yet)",
            name.as_str()
        )));
    }

    let base = map_view(&handle)?;
    Ok((handle, base))
}

/// Open an existing named section object for read-write mapping.
///
/// # Errors
///
/// [`ShmError::Os`] if `OpenFileMappingW` fails — most commonly because no
/// object of that name currently exists (every handle to it has closed).
pub(crate) fn open_named(name: &SegmentName) -> ShmResult<OwnedHandle> {
    let wide_name = windows_object_name(name);
    // SAFETY: `wide_name` is a NUL-terminated UTF-16 buffer live for the
    // duration of the call; `FALSE` inheritance matches `create_named_mapped`.
    let raw = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
    if raw.is_null() {
        return Err(ShmError::io("OpenFileMappingW", io::Error::last_os_error()));
    }
    // SAFETY: as in `create_named_mapped` — a freshly returned, uniquely
    // owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}

/// Map an already-open section handle, read-write, over its full size.
///
/// Unlike `MapViewOfFile`'s general contract (offset and length may name any
/// sub-range), every caller in this crate always wants the whole section — a
/// [`crate::segment::Segment`] never maps a partial view — so passing `0` for
/// both `dwNumberOfBytesToMap` and the offset (documented as "map the entire
/// file") is not a simplification of the API, it is the only shape this
/// module ever needs.
///
/// # Errors
///
/// [`ShmError::Os`] if `MapViewOfFile` fails.
pub(crate) fn map_view(handle: &OwnedHandle) -> ShmResult<NonNull<u8>> {
    let raw_handle = handle.as_raw_handle() as HANDLE;
    // SAFETY: `raw_handle` names a live section object owned by `handle`,
    // which outlives this call; offset 0 and a zero length request the
    // entire section, which is always well-defined once the object has a
    // fixed size (true for every section this module creates or opens).
    let view = unsafe { MapViewOfFile(raw_handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
    if view.Value.is_null() {
        return Err(ShmError::io("MapViewOfFile", io::Error::last_os_error()));
    }
    NonNull::new(view.Value.cast::<u8>()).ok_or_else(|| ShmError::Os {
        operation: "MapViewOfFile",
        source: io::Error::other("MapViewOfFile returned a null address"),
    })
}

/// The size, in bytes, of the mapped region starting at `base`.
///
/// Consulted on attach so a header claiming a geometry larger than what is
/// actually mapped is caught as a typed
/// [`crate::error::CorruptReason::TotalLenMismatch`] rather than an
/// access violation on first touch — the same defence
/// [`crate::os::unix::object_len`] provides via `fstat`. `VirtualQuery`
/// reports the size of the *committed region*, which for a mapped view is
/// exactly the section's size (Windows commits a mapped view atomically; it
/// has no `mmap`-past-EOF equivalent — a `MapViewOfFile` past the object's
/// size simply fails at map time, so by the time this function is reached
/// the view is already known to fit the object it names). It is queried
/// anyway, both for parity with the Unix path's explicit belt-and-suspenders
/// check and because a header inside the mapping is not yet trusted at this
/// point in `Segment::open_fd_named`.
///
/// # Errors
///
/// [`ShmError::Os`] if `VirtualQuery` fails.
pub(crate) fn view_len(base: NonNull<u8>) -> ShmResult<u64> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `base` points at a live mapping produced by `map_view`, and
    // `info` is a correctly sized, freshly zeroed output buffer whose exact
    // size is passed as `dwlength`.
    let written = unsafe {
        VirtualQuery(
            base.as_ptr().cast(),
            std::ptr::addr_of_mut!(info),
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        return Err(ShmError::io("VirtualQuery", io::Error::last_os_error()));
    }
    Ok(info.RegionSize as u64)
}

/// Unmap a section view.
///
/// Takes (and ignores) a `_len` parameter purely so every call site in
/// [`crate::segment`] can stay textually identical across platforms —
/// `UnmapViewOfFile` itself needs only the base address: Windows tracks a
/// view's extent internally (it is always "the whole section", per
/// [`map_view`]'s doc comment), unlike `munmap`, which requires the caller to
/// state the length because a POSIX mapping can legitimately cover only part
/// of an object.
///
/// # Safety
///
/// `ptr` must come from a matching [`map_view`] (or [`create_named_mapped`])
/// call, and no reference into the mapping may outlive this call.
pub(crate) unsafe fn unmap(ptr: NonNull<u8>, _len: usize) {
    // SAFETY: the caller guarantees `ptr` names exactly one live view
    // produced by this module's mapping functions.
    let _ = unsafe {
        UnmapViewOfFile(
            windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: ptr.as_ptr().cast(),
            },
        )
    };
}

/// Remove a named section's Windows object-manager entry.
///
/// A no-op that always succeeds. Windows section objects have no POSIX-style
/// separable "name" to unlink out from under a live mapping: the object
/// manager reference-counts the object itself, and it is deleted
/// automatically once the last [`OwnedHandle`] to it closes — there is no
/// operation that removes the *name* while leaving existing mappings intact,
/// because there is no window in which that distinction exists. See the
/// module docs' crash-reclamation table for why this makes
/// [`crate::segment::Segment::unlink`] intentionally inert here rather than a
/// missing feature.
pub(crate) const fn unlink_named(_name: &SegmentName) -> ShmResult<()> {
    Ok(())
}

/// This process's id.
///
/// `GetCurrentProcessId` lives in the (ungranted) `Win32_System_Threading`
/// feature; `std::process::id()` is the platform-agnostic standard-library
/// equivalent and needs no `windows-sys` feature at all, so it is used here
/// instead — the same value, reached without waiting on a feature grant this
/// module does not otherwise need.
#[must_use]
pub(crate) fn current_pid() -> i64 {
    i64::from(std::process::id())
}

/// Whether a process with this id exists.
///
/// Always `true` (the conservative answer — see
/// [`crate::os::unix::process_alive`]'s doc comment for why "alive" is the
/// safe default under uncertainty). A real liveness check needs `OpenProcess`
/// / `GetExitCodeProcess`, both in the ungranted `Win32_System_Threading`
/// feature; until that is wired, [`crate::segment::Segment::producer_alive`]
/// and the consumer-table eviction path degrade to "never evict on this
/// platform" rather than to a false "dead" that would let a live producer's
/// slot be reclaimed out from under it. This mirrors the Unix path's own
/// stated bias ("Any other error is treated as alive so a transient failure
/// never causes a spurious eviction") taken to its limit for a platform with
/// no liveness primitive wired yet at all.
#[must_use]
pub(crate) const fn process_alive(_pid: i64) -> bool {
    true
}

// A `dup_cloexec`-equivalent handle-duplication primitive is deliberately
// not offered here: `DuplicateHandle` needs a *target process* handle
// (`GetCurrentProcess`, in the ungranted `Win32_System_Threading` feature).
// Windows also has no `exec`-time inheritance to guard against by default (a
// raw `HANDLE` is **not** inherited by a child process started without
// explicitly opting each handle in — the reverse default from POSIX), so
// there is no `FD_CLOEXEC`-shaped gap this leaves open. Every place the Unix
// plane would duplicate a descriptor to hand it to a broker or a peer
// process, the Windows plane instead reopens by name via [`open_named`],
// which is already how [`crate::segment::Segment::attach`] works on every
// platform — see the module docs' naming section.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn test_name(salt: u128) -> SegmentName {
        // A name derived from this process's pid keeps concurrent test
        // binaries from colliding, exactly as the Unix `os` tests do.
        SegmentName::from_digest(u128::from(current_pid() as u64) ^ salt)
    }

    #[test]
    fn current_pid_matches_the_standard_library() {
        assert_eq!(current_pid(), i64::from(std::process::id()));
        assert!(current_pid() > 0);
    }

    #[test]
    fn process_alive_is_conservatively_true() {
        // No liveness primitive is wired yet (see the module docs); the
        // conservative answer must never flip to `false` on its own.
        assert!(process_alive(current_pid()));
        assert!(process_alive(1));
        assert!(process_alive(-1));
    }

    #[test]
    fn a_named_mapping_round_trips_through_the_windows_object_namespace() {
        let name = test_name(0x5a5a_5a5a);
        let (handle, base) = create_named_mapped(&name, 4096).expect("create + map");

        let reopened = open_named(&name).expect("reopen by name");
        let second = map_view(&reopened).expect("second map");

        // SAFETY: both mappings are 4096 bytes and alias the same section.
        unsafe {
            base.as_ptr().write(0xab);
            assert_eq!(second.as_ptr().read(), 0xab);
        }

        let observed = view_len(base).expect("query size");
        assert!(observed >= 4096, "region must cover the requested size");

        // SAFETY: `base`/`second` each came from a matching mapping call
        // above and neither is referenced again after this point.
        unsafe {
            unmap(second, 4096);
            unmap(base, 4096);
        }
        drop(reopened);
        drop(handle);
    }

    #[test]
    fn opening_a_name_nothing_created_fails_with_a_typed_error() {
        let name = test_name(0x9999_9999);
        assert!(matches!(open_named(&name), Err(ShmError::Os { .. })));
    }

    #[test]
    fn recreating_a_still_referenced_name_is_a_typed_error_not_silent_reuse() {
        let name = test_name(0x1234_5678);
        let (handle, base) = create_named_mapped(&name, 4096).expect("first create");

        // The handle (and therefore the section) is still alive, so a second
        // create under the same name must not silently succeed with a
        // possibly different size — see `create_named_mapped`'s docs on why
        // this differs from the Unix stale-name retry.
        let collision = create_named_mapped(&name, 8192);
        assert!(
            matches!(collision, Err(ShmError::InvalidConfig { .. })),
            "{collision:?}"
        );

        // SAFETY: `base` came from the matching mapping above and is not
        // referenced again.
        unsafe { unmap(base, 4096) };
        drop(handle);
    }

    #[test]
    fn unlinking_a_windows_name_is_always_a_no_op_success() {
        // There is no separable "name" to remove (see `unlink_named`'s doc
        // comment); the call must simply succeed, whether or not the name is
        // currently in use.
        let name = test_name(0xdead_beef);
        assert!(unlink_named(&name).is_ok());
        let (handle, base) = create_named_mapped(&name, 4096).expect("create");
        assert!(
            unlink_named(&name).is_ok(),
            "unlink must not disturb a live mapping"
        );
        // SAFETY: `base` came from the matching mapping above.
        unsafe { unmap(base, 4096) };
        drop(handle);
    }

    #[test]
    fn windows_object_name_strips_the_posix_slash_and_uses_the_local_namespace() {
        let name = SegmentName::from_digest(0);
        let wide = windows_object_name(&name);
        let decoded = String::from_utf16(&wide[..wide.len() - 1]).unwrap();
        assert!(decoded.starts_with("Local\\"));
        assert!(!decoded.contains('/'));
        assert!(decoded.ends_with(&name.as_str()[1..]));
    }
}
