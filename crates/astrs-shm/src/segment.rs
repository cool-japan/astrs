//! The mapped segment: creation, attachment, generation checking, teardown.
//!
//! A [`Segment`] is one `mmap` of one shared-memory object plus the validated
//! [`SegmentLayout`] that says where everything in it lives. It owns nothing
//! about the *protocol* — that is [`crate::Producer`] and
//! [`crate::Consumer`] — but it is the only type in the crate that forms
//! pointers into the mapping, so every bounds and alignment argument is made
//! exactly once, here.
//!
//! # Creating and attaching
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_shm::{Segment, SegmentConfig, SegmentKey};
//! use astrs_wire::DataflowId;
//!
//! let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
//! let segment = Segment::create(key.clone(), SegmentConfig::new(8, 4096)?)?;
//!
//! // A consumer in another process receives the descriptor from the broker
//! // and attaches to it, naming the generation it expects.
//! let attached = Segment::open_fd(segment.try_clone_fd()?, Some(&key))?;
//! assert_eq!(attached.header().generation(), 1);
//!
//! // A mapping from a previous incarnation is rejected, not silently used.
//! let stale = key.clone().with_generation(0);
//! assert!(Segment::open_fd(segment.try_clone_fd()?, Some(&stale)).is_err());
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```
//!
//! # Lifetime and unlinking
//!
//! The creator owns the name. Dropping a created [`Segment`] unmaps it and,
//! for a named backing, unlinks it — existing mappings survive an unlink, so
//! this is the "unlink after drain" of §6.2 in its simplest form. A broker
//! that wants to outlive the creating process holds the descriptor instead
//! ([`Segment::try_clone_fd`]) and unlinks explicitly.

#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::ptr::NonNull;
use std::sync::Arc;
#[cfg(any(unix, test))]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{Backing, SegmentConfig};
use crate::consumer_table::ConsumerEntry;
#[cfg(unix)]
use crate::doorbell::DoorbellRegistry;
use crate::error::{ShmError, ShmResult};
use crate::header::{SegmentHeader, SegmentHeaderView};
use crate::key::{SegmentKey, SegmentName};
use crate::layout::SegmentLayout;
use crate::os;
use crate::slot::SlotHeader;
// No doorbell primitive exists on this platform yet (see `os::windows`'s
// module docs — it needs `Win32_System_Threading`), so [`Segment`] carries
// the same always-empty registry the fully-unsupported stub platforms use.
// This keeps [`Segment::doorbells`] callable without a `cfg` at every call
// site, exactly like the crate's other cross-platform export pairs (see
// `lib.rs`'s note on why the two export lists are name-for-name identical).
#[cfg(windows)]
use crate::unsupported::DoorbellRegistry;

/// The OS handle type backing one segment's mapping: a descriptor on Unix, a
/// kernel-object handle on Windows. Never exposed outside this module —
/// callers reach the mapping through [`Segment`]'s methods, not the handle
/// itself.
#[cfg(unix)]
type Handle = OwnedFd;
#[cfg(windows)]
type Handle = OwnedHandle;

/// A live mapping of one shared-memory ring.
///
/// Cheap to share: wrap it in an [`Arc`] and hand clones to the producer and
/// to in-process consumers, which then all use one mapping instead of one
/// per participant.
pub struct Segment {
    key: Option<SegmentKey>,
    name: Option<SegmentName>,
    /// Held for its `Drop` alone on Windows: an `OwnedHandle` closes the
    /// section handle when this field is dropped, and this platform has no
    /// `as_fd`/`try_clone_fd`-style accessor to read it through first (see
    /// `crate::os::windows`'s module docs — attach is by name, not by
    /// descriptor). Unix reads it via [`Segment::as_fd`]/
    /// [`Segment::try_clone_fd`], so the `cfg_attr` below only fires the
    /// `expect` — and only needs to — on Windows.
    #[cfg_attr(
        windows,
        expect(
            dead_code,
            reason = "RAII-only on this platform; see field doc comment"
        )
    )]
    fd: Handle,
    base: NonNull<u8>,
    len: usize,
    layout: SegmentLayout,
    /// Whether this handle is responsible for unlinking the name on drop.
    owns_name: bool,
    /// Process-local guard: at most one [`crate::Producer`] per mapping.
    ///
    /// Read only by [`Segment::claim_producer`]/[`Segment::release_producer`],
    /// which the `#[cfg(unix)]` producer and this module's own portable unit
    /// tests are the only callers of — never read on a Windows build
    /// otherwise, though it is still written at construction below since
    /// `Segment` itself stays portable.
    #[cfg(any(unix, test))]
    producer_taken: AtomicBool,
    /// Process-local doorbell set. Descriptors cannot live in shared memory,
    /// so this is the rendezvous point between in-process consumers (which
    /// register themselves on attach) and remote ones (whose write ends the
    /// daemon delivers over `SCM_RIGHTS`). Always empty on a platform with no
    /// doorbell primitive (currently Windows — see `crate::os::windows`).
    doorbells: DoorbellRegistry,
}

// SAFETY: every byte of the mapping that this crate reads or writes is
// accessed through an atomic (header, slot table, consumer table) or through
// the slot-ownership protocol (payload and metadata regions), which is what
// makes concurrent access from several threads — and several processes —
// sound. The `NonNull` field is the only reason the automatic derivation
// fails; it names a mapping that outlives every handle to it.
unsafe impl Send for Segment {}
// SAFETY: as above — `&Segment` exposes only atomic access and the
// protocol-guarded payload accessors.
unsafe impl Sync for Segment {}

impl Segment {
    /// Create a new segment for `key` with the given geometry.
    ///
    /// The producer pid recorded in the header is this process's.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] for an unusable geometry, [`ShmError::Os`]
    /// for a failing syscall, or [`ShmError::Unsupported`] when the requested
    /// [`Backing`] does not exist on this platform.
    pub fn create(key: SegmentKey, config: SegmentConfig) -> ShmResult<Self> {
        Self::create_for_pid(key, config, os::current_pid())
    }

    /// Create a new segment and record `producer_pid` as its producer.
    ///
    /// The daemon creates segments *before* spawning the node that will
    /// produce into them (§6.3 — the route exists before the process does),
    /// so the pid it stamps is corrected later via
    /// [`SegmentHeader::set_producer_pid`]. This constructor lets it stamp a
    /// known pid directly when the order is the other way round.
    ///
    /// # Errors
    ///
    /// As [`Segment::create`].
    pub fn create_for_pid(
        key: SegmentKey,
        config: SegmentConfig,
        producer_pid: i64,
    ) -> ShmResult<Self> {
        config.validate()?;
        let layout = SegmentLayout::new(&config)?;
        let backing = resolve_backing(config.backing())?;
        let config = config.with_backing(backing);

        let (fd, name, base) = Self::create_backing(&key, backing, layout.total_len())?;
        let len = layout.total_len() as usize;

        let segment = Self {
            key: Some(key.clone()),
            name,
            fd,
            base,
            len,
            layout,
            owns_name: true,
            #[cfg(any(unix, test))]
            producer_taken: AtomicBool::new(false),
            doorbells: DoorbellRegistry::new(),
        };

        // SAFETY: the mapping is freshly created and not yet reachable by any
        // other process (a `memfd` has no name; a named object is `O_EXCL`
        // and its magic is not published until the end of this call).
        unsafe { segment.initialize(&key, &config, producer_pid) };
        Ok(segment)
    }

    /// Create the OS backing object for a fresh segment, sized and mapped.
    ///
    /// The Unix and Windows shapes genuinely differ — Unix is a three-step
    /// `open` → `ftruncate` → `mmap` (four platform calls, three of which can
    /// each fail independently and must each unwind the name on the way out);
    /// Windows fixes the size at creation and the first `MapViewOfFile`
    /// already covers the whole object, so [`os::create_named_mapped`] does
    /// both in one call. Both branches return the same triple so every
    /// caller above this line is platform-agnostic.
    #[cfg(unix)]
    fn create_backing(
        key: &SegmentKey,
        backing: Backing,
        total_len: u64,
    ) -> ShmResult<(Handle, Option<SegmentName>, NonNull<u8>)> {
        let (fd, name) = match backing {
            Backing::Named => {
                let name = key.os_name();
                let fd = os::create_named(&name)?;
                (fd, Some(name))
            }
            Backing::Memfd | Backing::Auto => {
                let label = format!("astrs.{}.{}", key.node(), key.output());
                (os::create_anonymous(&label)?, None)
            }
        };

        if let Err(err) = os::set_len(fd.as_fd(), total_len) {
            if let Some(name) = &name {
                let _ = os::unlink_named(name);
            }
            return Err(err);
        }
        if backing == Backing::Memfd {
            os::seal_anonymous(fd.as_fd());
        }

        match os::map_shared(fd.as_fd(), total_len) {
            Ok(base) => Ok((fd, name, base)),
            Err(err) => {
                if let Some(name) = &name {
                    let _ = os::unlink_named(name);
                }
                Err(err)
            }
        }
    }

    /// As the Unix [`Segment::create_backing`], but for a named Windows
    /// section object.
    ///
    /// `backing` is always [`Backing::Named`] by the time this runs:
    /// [`resolve_backing`] already turns a [`Backing::Memfd`] request into
    /// [`ShmError::Unsupported`] on this platform (there is no anonymous
    /// mapping wired yet — see `crate::os::windows::create_anonymous`) and
    /// resolves [`Backing::Auto`] to [`Backing::Named`]
    /// (`crate::os::windows::AUTO_BACKING`). This function still branches
    /// on `backing` explicitly, exactly like the Unix
    /// [`Segment::create_backing`], rather than trusting that guarantee and
    /// ignoring the parameter: a caller that somehow reached this function
    /// with [`Backing::Memfd`] anyway gets the same typed
    /// [`ShmError::Unsupported`] a Unix build without `memfd_create` would
    /// give it, not a silently-created named segment of the wrong kind.
    #[cfg(windows)]
    fn create_backing(
        key: &SegmentKey,
        backing: Backing,
        total_len: u64,
    ) -> ShmResult<(Handle, Option<SegmentName>, NonNull<u8>)> {
        if backing == Backing::Memfd {
            // Always errs (see `create_anonymous`'s own doc comment); the
            // `?` returns before anything below is reached.
            let label = format!("astrs.{}.{}", key.node(), key.output());
            os::create_anonymous(&label)?;
        }
        let name = key.os_name();
        let (handle, base) = os::create_named_mapped(&name, total_len)?;
        Ok((handle, Some(name), base))
    }

    /// Attach to an existing segment through a descriptor.
    ///
    /// This is the primitive attach path: on Linux a `memfd` segment has no
    /// filesystem name at all, so the descriptor — brokered by the daemon
    /// over `SCM_RIGHTS` ([`crate::SegmentBroker`]) — is the only way in.
    ///
    /// When `expect` is `Some`, both the generation and the 128-bit key
    /// digest are verified.
    ///
    /// # Errors
    ///
    /// - [`ShmError::BadMagic`] / [`ShmError::LayoutVersion`] /
    ///   [`ShmError::CorruptHeader`] — not a usable segment.
    /// - [`ShmError::StaleGeneration`] — the mapping belongs to a previous
    ///   incarnation of the producer.
    /// - [`ShmError::KeyMismatch`] — the segment carries a different key
    ///   (the macOS short-name collision case).
    #[cfg(unix)]
    pub fn open_fd(fd: OwnedFd, expect: Option<&SegmentKey>) -> ShmResult<Self> {
        Self::attach_handle(fd, None, expect)
    }

    /// Attach by name.
    ///
    /// Only usable for [`Backing::Named`] segments. On Linux the default
    /// backing is anonymous, so this path is for segments explicitly created
    /// with [`SegmentConfig::with_backing`] — the inspection (`astrs doctor`)
    /// and replay routes. On Windows every segment is named (there is no
    /// anonymous backing wired yet — see `crate::os::windows::AUTO_BACKING`,
    /// a Windows-only item not linkable from every host platform's rendered
    /// docs), so this is the *primary* attach path there, not a fallback; it is also
    /// the route `astrs-node-api` already takes when no broker socket is
    /// configured, on every platform.
    ///
    /// # Errors
    ///
    /// As [`Segment::open_fd`] on Unix, plus [`ShmError::Os`] if the name
    /// does not exist.
    pub fn open_named(name: &SegmentName, expect: Option<&SegmentKey>) -> ShmResult<Self> {
        let fd = os::open_named(name)?;
        Self::attach_handle(fd, Some(name.clone()), expect)
    }

    /// Attach to the segment a key names, deriving the name from the key.
    ///
    /// # Errors
    ///
    /// As [`Segment::open_named`].
    pub fn attach(key: &SegmentKey) -> ShmResult<Self> {
        Self::open_named(&key.os_name(), Some(key))
    }

    fn attach_handle(
        fd: Handle,
        name: Option<SegmentName>,
        expect: Option<&SegmentKey>,
    ) -> ShmResult<Self> {
        let (base, map_len) = Self::map_for_attach(&fd)?;
        // SAFETY: `map_for_attach` returned a page-aligned mapping of at
        // least `HEADER_LEN` bytes, and it stays alive until the `unmap`
        // below or until the `Segment` that adopts it is dropped.
        let fields = unsafe { SegmentHeader::from_ptr(base.as_ptr()) }.fields();
        let layout = match SegmentLayout::validate_header(&fields, map_len) {
            Ok(layout) => layout,
            Err(err) => {
                // SAFETY: nothing else has taken ownership of the mapping yet.
                unsafe { os::unmap(base, map_len) };
                return Err(err);
            }
        };

        let segment = Self {
            key: expect.cloned(),
            name,
            fd,
            base,
            // The mapping may be larger than the segment describes (a page
            // rounding, or an object sized generously); unmapping must return
            // exactly what was mapped.
            len: map_len,
            layout,
            owns_name: false,
            #[cfg(any(unix, test))]
            producer_taken: AtomicBool::new(false),
            doorbells: DoorbellRegistry::new(),
        };

        // From here on `Drop` owns the mapping, so an identity failure
        // unmaps on the way out.
        if let Some(expected) = expect {
            segment.verify_identity(expected)?;
        }
        Ok(segment)
    }

    /// Map an already-open handle for attach, and report how many bytes were
    /// actually mapped.
    ///
    /// The object's real size — not the header's claim about itself — is
    /// what bounds the mapping. On Unix, `mmap` will happily map *past* the
    /// end of a short object: the call succeeds and the first access beyond
    /// the object raises `SIGBUS`, so the size must be learned (via `fstat`)
    /// and passed to `mmap` explicitly — before mapping, since `mmap` itself
    /// needs it as an argument. On Windows the order inverts: `MapViewOfFile`
    /// with a zero length always maps the *entire* section regardless of its
    /// size (there is no `mmap`-past-EOF failure mode to guard against), so
    /// the size is only learned afterwards, via `VirtualQuery` on the
    /// resulting view. Both branches converge on the same postcondition —
    /// asking the kernel how big the object actually is turns a latent
    /// out-of-bounds access into a typed error, and it means the mapping
    /// length returned here is safe by construction, so the header can be
    /// read in place with no probe-and-remap step and no window in which the
    /// geometry could differ between two reads.
    #[cfg(unix)]
    fn map_for_attach(fd: &Handle) -> ShmResult<(NonNull<u8>, usize)> {
        let object_len = os::object_len(fd.as_fd())?;
        let map_len = clamp_to_usize(object_len.min(crate::layout::MAX_SEGMENT_LEN));
        if map_len < crate::layout::HEADER_LEN as usize {
            return Err(ShmError::CorruptHeader {
                reason: crate::error::CorruptReason::MappingTooSmall {
                    mapped: map_len,
                    required: crate::layout::HEADER_LEN as usize,
                },
            });
        }
        let base = os::map_shared(fd.as_fd(), map_len as u64)?;
        Ok((base, map_len))
    }

    /// As the Unix [`Segment::map_for_attach`], reordered for
    /// `MapViewOfFile`'s "always the whole section" semantics — see that
    /// method's doc comment for the full comparison.
    #[cfg(windows)]
    fn map_for_attach(handle: &Handle) -> ShmResult<(NonNull<u8>, usize)> {
        let base = os::map_view(handle)?;
        let view_len = os::view_len(base)?;
        let map_len = clamp_to_usize(view_len.min(crate::layout::MAX_SEGMENT_LEN));
        if map_len < crate::layout::HEADER_LEN as usize {
            // SAFETY: `base` came from `map_view` immediately above, is not
            // referenced anywhere else, and this platform's `unmap` ignores
            // its length argument (see `crate::os::windows::unmap`), so an
            // under-reported `map_len` here cannot leave part of the view
            // mapped.
            unsafe { os::unmap(base, map_len) };
            return Err(ShmError::CorruptHeader {
                reason: crate::error::CorruptReason::MappingTooSmall {
                    mapped: map_len,
                    required: crate::layout::HEADER_LEN as usize,
                },
            });
        }
        Ok((base, map_len))
    }

    /// Check that this mapping is the incarnation the caller expected.
    ///
    /// # Errors
    ///
    /// [`ShmError::StaleGeneration`] or [`ShmError::KeyMismatch`].
    pub fn verify_identity(&self, expected: &SegmentKey) -> ShmResult<()> {
        let header = self.header();
        let found = header.generation();
        if found != expected.generation() {
            return Err(ShmError::StaleGeneration {
                expected: expected.generation(),
                found,
            });
        }
        let digest = header.key_digest();
        if digest != expected.digest() {
            return Err(ShmError::KeyMismatch {
                expected: expected.digest(),
                found: digest,
            });
        }
        Ok(())
    }

    /// Initialise a freshly created mapping and publish it.
    ///
    /// # Safety
    ///
    /// Must be called exactly once, on a mapping no other process can yet
    /// observe as valid.
    unsafe fn initialize(&self, key: &SegmentKey, config: &SegmentConfig, producer_pid: i64) {
        for index in 0..self.layout.slot_count() {
            // SAFETY: `index < slot_count`, so the offset is inside the
            // validated slot table.
            unsafe { self.slot(index).initialize() };
        }
        for index in 0..self.layout.max_consumers() {
            // SAFETY: `index < max_consumers`, so the offset is inside the
            // validated consumer table.
            unsafe { self.consumer_entry(index).initialize() };
        }
        let created_ns = crate::now_ns();
        // SAFETY: this is the single initialisation of this header, and it
        // publishes the magic last.
        unsafe {
            self.header()
                .initialize(key, &self.layout, config, producer_pid, created_ns);
        }
    }

    /// The segment's logical key, when this handle knows it.
    ///
    /// `None` for a segment attached without an expected key — a diagnostic
    /// tool mapping a descriptor it was handed. [`Segment::header`] remains
    /// authoritative for generation and key digest in every case.
    #[must_use]
    pub const fn key(&self) -> Option<&SegmentKey> {
        self.key.as_ref()
    }

    /// The OS-visible name, when the backing has one.
    #[must_use]
    pub const fn name(&self) -> Option<&SegmentName> {
        self.name.as_ref()
    }

    /// The validated layout.
    #[must_use]
    pub const fn layout(&self) -> &SegmentLayout {
        &self.layout
    }

    /// The number of bytes mapped.
    #[must_use]
    pub const fn mapped_len(&self) -> usize {
        self.len
    }

    /// The address of the first mapped byte.
    ///
    /// Together with [`Segment::mapped_len`] this bounds the mapping, which is
    /// what lets a caller check that a pointer it was handed — a
    /// [`crate::Sample`]'s payload, say — really points *into* the ring rather
    /// than at a copy of it. Every other address in the mapping is this one
    /// plus a [`crate::SegmentLayout`] offset, so the two together also make
    /// the layout independently verifiable.
    ///
    /// Returning `usize` rather than a pointer is deliberate: this is an
    /// address to compare, not one to read through.
    #[must_use]
    pub fn mapping_base(&self) -> usize {
        self.base.as_ptr() as usize
    }

    /// Borrow the segment's descriptor — what a broker sends over
    /// `SCM_RIGHTS`.
    ///
    /// Unix-only: `SCM_RIGHTS` descriptor passing has no Windows counterpart
    /// reachable from this crate's granted `windows-sys` features (see
    /// `crate::os::windows`'s module docs), and a Windows attach goes
    /// through [`Segment::open_named`]/[`Segment::attach`] instead — every
    /// process that knows the deterministic name can open the section
    /// directly, with no descriptor to hand over in the first place.
    #[cfg(unix)]
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Duplicate the segment's descriptor.
    ///
    /// Unix-only — see [`Segment::as_fd`].
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `dup` fails.
    #[cfg(unix)]
    pub fn try_clone_fd(&self) -> ShmResult<OwnedFd> {
        os::dup_cloexec(self.fd.as_fd())
    }

    /// The mapped header.
    #[must_use]
    pub fn header(&self) -> &SegmentHeader {
        // SAFETY: the mapping is at least `HEADER_LEN` bytes (validated) and
        // page-aligned, and it outlives `&self`.
        unsafe { SegmentHeader::from_ptr(self.base.as_ptr()) }
    }

    /// A snapshot of the header, for telemetry and diagnostics.
    #[must_use]
    pub fn view(&self) -> SegmentHeaderView {
        SegmentHeaderView::capture(self.header())
    }

    /// The slot-table entry for `index`.
    ///
    /// # Panics
    ///
    /// Never — an out-of-range index is clamped into the table. Callers
    /// derive indices from `seq % slot_count` and are always in range; the
    /// clamp exists so a future caller cannot turn an arithmetic slip into
    /// out-of-bounds memory access.
    #[must_use]
    pub fn slot(&self, index: u32) -> &SlotHeader {
        let index = index % self.layout.slot_count();
        let offset = self.layout.slot_entry_offset(index);
        // SAFETY: `index < slot_count`, and `validate_header` proved the slot
        // table lies wholly inside the mapping.
        unsafe { SlotHeader::from_ptr(self.base.as_ptr().add(offset as usize)) }
    }

    /// The consumer-table entry for `index`.
    ///
    /// Out-of-range indices are clamped, as for [`Segment::slot`].
    #[must_use]
    pub fn consumer_entry(&self, index: u32) -> &ConsumerEntry {
        let index = index % self.layout.max_consumers();
        let offset = self.layout.consumer_entry_offset(index);
        // SAFETY: `index < max_consumers`, and `validate_header` proved the
        // consumer table lies wholly inside the mapping.
        unsafe { ConsumerEntry::from_ptr(self.base.as_ptr().add(offset as usize)) }
    }

    /// Iterate the consumer table.
    pub fn consumer_entries(&self) -> impl Iterator<Item = (u32, &ConsumerEntry)> + Clone {
        (0..self.layout.max_consumers()).map(move |index| (index, self.consumer_entry(index)))
    }

    /// The base pointer of slot `index`'s payload region.
    ///
    /// Always 128-byte aligned (see [`crate::layout`]).
    ///
    /// # Safety
    ///
    /// The caller must hold the slot — exclusively for writes (a
    /// [`crate::SampleMut`]), or pinned for reads (a [`crate::Sample`]).
    #[must_use]
    pub unsafe fn payload_ptr(&self, index: u32) -> *mut u8 {
        let index = index % self.layout.slot_count();
        let offset = self.layout.payload_offset(index);
        // SAFETY: `index < slot_count`; `validate_header` proved the data
        // region lies wholly inside the mapping.
        unsafe { self.base.as_ptr().add(offset as usize) }
    }

    /// The base pointer of slot `index`'s metadata region.
    ///
    /// # Safety
    ///
    /// As [`Segment::payload_ptr`].
    #[must_use]
    pub unsafe fn meta_ptr(&self, index: u32) -> *mut u8 {
        let index = index % self.layout.slot_count();
        let offset = self.layout.meta_offset(index);
        // SAFETY: as `payload_ptr`.
        unsafe { self.base.as_ptr().add(offset as usize) }
    }

    /// Mark the segment closed.
    ///
    /// Producers stop being able to allocate; consumers drain and then see
    /// [`crate::RecvError::Closed`].
    pub fn mark_closed(&self) {
        self.header().mark_closed();
    }

    /// Unlink the segment's name, if it has one.
    ///
    /// Live mappings survive: this only removes the name, so a consumer that
    /// has already attached keeps draining. Calling it on an anonymous
    /// segment, or twice, is a no-op.
    ///
    /// On Windows this is *always* a no-op that returns `Ok(())`: a named
    /// section has no separable name to remove independently of the object
    /// itself — the object manager deletes it automatically once every
    /// handle closes. See `crate::os::windows`'s crash-reclamation table
    /// for the full comparison against the Unix named-`shm_open` case this
    /// method exists for.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `shm_unlink` fails for a reason other than the
    /// name already being gone. Never fails on Windows.
    pub fn unlink(&self) -> ShmResult<()> {
        match &self.name {
            Some(name) => os::unlink_named(name),
            None => Ok(()),
        }
    }

    /// Give up responsibility for unlinking the name on drop.
    ///
    /// The broker calls this when it takes over a segment's lifetime.
    pub fn disown_name(&mut self) {
        self.owns_name = false;
    }

    /// Whether this handle will unlink the name when dropped.
    #[must_use]
    pub const fn owns_name(&self) -> bool {
        self.owns_name
    }

    /// The process-local doorbell registry for this mapping.
    ///
    /// In-process consumers register themselves here on attach; the daemon
    /// registers remote consumers' write ends here when they arrive over the
    /// control socket. [`crate::Producer::commit`] rings everything in it.
    #[must_use]
    pub const fn doorbells(&self) -> &DoorbellRegistry {
        &self.doorbells
    }

    /// Whether the producer process recorded in the header is still alive.
    ///
    /// Used by the consumer's quiet-ring check: a ring that has stopped
    /// advancing with a dead producer and no `closed` flag means the daemon
    /// died too, and the consumer must not wait forever.
    ///
    /// On Windows this always answers `true` today: `OpenProcess` /
    /// `GetExitCodeProcess` need `Win32_System_Threading`, not yet granted
    /// (see `crate::os::windows::process_alive`'s doc comment) — the
    /// conservative answer, matching the bias every liveness check in this
    /// crate already takes under uncertainty.
    #[must_use]
    pub fn producer_alive(&self) -> bool {
        os::process_alive(self.header().producer_pid())
    }

    /// Claim the single producer role for this mapping.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if a producer was already taken from this
    /// mapping — the ring is SPMC, and a second writer would corrupt it.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) fn claim_producer(&self) -> ShmResult<()> {
        if self.producer_taken.swap(true, Ordering::AcqRel) {
            return Err(ShmError::invalid_config(
                "this segment mapping already has a producer (the ring is single-producer)",
            ));
        }
        Ok(())
    }

    /// Release the producer role, so a replacement can take it.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) fn release_producer(&self) {
        self.producer_taken.store(false, Ordering::Release);
    }

    /// Wrap the segment in an [`Arc`] so a producer and in-process consumers
    /// can share one mapping.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(unix)] {
    /// use std::sync::Arc;
    /// use astrs_shm::{AttachOptions, Consumer, Producer, Segment, SegmentConfig, SegmentKey};
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "o", 1)?;
    /// let shared = Segment::create(key, SegmentConfig::new(4, 512)?)?.shared();
    /// let producer = Producer::new(Arc::clone(&shared))?;
    /// let consumer = Consumer::attach(Arc::clone(&shared), AttachOptions::default())?;
    /// assert_eq!(producer.slot_count(), 4);
    /// assert_eq!(consumer.cursor(), 1);
    /// # }
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Create a segment already wrapped in an [`Arc`].
    ///
    /// # Errors
    ///
    /// As [`Segment::create`].
    pub fn create_shared(key: SegmentKey, config: SegmentConfig) -> ShmResult<Arc<Self>> {
        Ok(Self::create(key, config)?.shared())
    }
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("key", &self.key.as_ref().map(SegmentKey::canonical))
            .field("name", &self.name.as_ref().map(SegmentName::as_str))
            .field("slots", &self.layout.slot_count())
            .field("payload_capacity", &self.layout.payload_capacity())
            .field("mapped_len", &self.len)
            .field("write_seq", &self.header().write_seq())
            .field("closed", &self.header().is_closed())
            .finish()
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        if self.owns_name
            && let Some(name) = &self.name
        {
            let _ = os::unlink_named(name);
        }
        // SAFETY: `base`/`len` came from one mapping call
        // (`os::map_shared`/`os::create_backing` on Unix,
        // `os::map_view`/`os::create_named_mapped` on Windows), no reference
        // into the mapping outlives `self` (every accessor borrows `&self`),
        // and this runs exactly once.
        unsafe { os::unmap(self.base, self.len) };
    }
}

/// Resolve [`Backing::Auto`] to a concrete backing for this platform.
fn resolve_backing(requested: Backing) -> ShmResult<Backing> {
    match requested {
        Backing::Auto => Ok(os::AUTO_BACKING),
        Backing::Named => Ok(Backing::Named),
        Backing::Memfd => {
            if os::AUTO_BACKING == Backing::Memfd {
                Ok(Backing::Memfd)
            } else {
                Err(ShmError::unsupported("memfd_create"))
            }
        }
    }
}

/// Narrow a byte count to `usize`, saturating.
///
/// A segment larger than the address space cannot be mapped anyway, and the
/// layout validator rejects anything above [`crate::layout::MAX_SEGMENT_LEN`]
/// long before this matters.
const fn clamp_to_usize(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::Backing;
    #[cfg(unix)]
    use crate::config::OverflowPolicy;
    use crate::layout::REGION_ALIGN;
    use astrs_wire::DataflowId;

    fn key(generation: u64) -> SegmentKey {
        SegmentKey::from_parts(DataflowId::from_u128(0xabcd), "camera", "image", generation)
            .unwrap()
    }

    fn named_key(generation: u64) -> SegmentKey {
        // Include the pid so concurrently running test binaries do not race
        // on the same POSIX name.
        SegmentKey::from_parts(
            DataflowId::from_u128(u128::from(os::current_pid() as u64)),
            "named",
            "out",
            generation,
        )
        .unwrap()
    }

    #[test]
    fn create_produces_a_validated_published_segment() {
        let segment = Segment::create(key(3), SegmentConfig::new(8, 4096).unwrap()).unwrap();
        let header = segment.header();
        assert_eq!(header.generation(), 3);
        assert_eq!(header.key_digest(), key(3).digest());
        assert_eq!(header.write_seq(), 0);
        assert!(!header.is_closed());
        assert_eq!(header.producer_pid(), os::current_pid());
        assert_eq!(segment.layout().slot_count(), 8);
        assert_eq!(segment.mapped_len(), segment.layout().total_len() as usize);
        assert!(segment.producer_alive());
    }

    #[test]
    fn every_slot_starts_free_and_every_consumer_entry_empty() {
        let segment = Segment::create(key(1), SegmentConfig::new(6, 256).unwrap()).unwrap();
        for index in 0..6 {
            let snapshot = segment.slot(index).snapshot();
            assert_eq!(snapshot.state, crate::slot::SlotState::Free);
            assert!(snapshot.is_producer_owned());
            assert_eq!(snapshot.violation(), None);
        }
        for (_, entry) in segment.consumer_entries() {
            assert!(!entry.is_occupied());
            assert_eq!(entry.cursor(), 0);
        }
    }

    #[test]
    fn pointers_satisfy_the_alignment_contract() {
        let segment = Segment::create(key(1), SegmentConfig::new(7, 1000).unwrap()).unwrap();
        for index in 0..7 {
            let slot = segment.slot(index) as *const _ as usize;
            assert_eq!(slot % 64, 0, "slot table entries are cache-line aligned");
            // SAFETY: pointer arithmetic only; nothing is dereferenced.
            let payload = unsafe { segment.payload_ptr(index) } as usize;
            let meta = unsafe { segment.meta_ptr(index) } as usize;
            assert_eq!(payload % REGION_ALIGN as usize, 0);
            assert_eq!(meta % REGION_ALIGN as usize, 0);
        }
        for (index, _) in segment.consumer_entries() {
            let entry = segment.consumer_entry(index) as *const _ as usize;
            assert_eq!(entry % 64, 0);
        }
    }

    #[test]
    fn indices_are_clamped_rather_than_running_off_the_table() {
        let segment = Segment::create(key(1), SegmentConfig::new(4, 128).unwrap()).unwrap();
        let wrapped = segment.slot(4) as *const _;
        let zero = segment.slot(0) as *const _;
        assert!(std::ptr::eq(wrapped, zero));
        let entry_wrapped = segment.consumer_entry(u32::MAX) as *const _;
        let expected =
            segment.consumer_entry(u32::MAX % segment.layout().max_consumers()) as *const _;
        assert!(std::ptr::eq(entry_wrapped, expected));
    }

    // `open_fd`/`try_clone_fd` are Unix-only (see their doc comments): a
    // Windows attach goes through `open_named`/`attach` instead, exercised
    // by `named_segments_attach_by_name_and_unlink_on_drop` below and by
    // `attaching_by_name_verifies_generation_and_key` further down.
    #[cfg(unix)]
    #[test]
    fn attaching_by_fd_verifies_generation_and_key() {
        let segment = Segment::create(key(5), SegmentConfig::new(4, 512).unwrap()).unwrap();

        let attached = Segment::open_fd(segment.try_clone_fd().unwrap(), Some(&key(5))).unwrap();
        assert_eq!(attached.header().generation(), 5);
        assert_eq!(attached.layout(), segment.layout());
        assert!(!attached.owns_name(), "an attach must not own the name");

        let stale = Segment::open_fd(segment.try_clone_fd().unwrap(), Some(&key(4)));
        assert!(matches!(
            stale,
            Err(ShmError::StaleGeneration {
                expected: 4,
                found: 5
            })
        ));

        let other =
            SegmentKey::from_parts(DataflowId::from_u128(0xabcd), "lidar", "points", 5).unwrap();
        assert!(matches!(
            Segment::open_fd(segment.try_clone_fd().unwrap(), Some(&other)),
            Err(ShmError::KeyMismatch { .. })
        ));

        // Attaching without an expectation still validates the layout.
        let anonymous = Segment::open_fd(segment.try_clone_fd().unwrap(), None).unwrap();
        assert_eq!(anonymous.header().generation(), 5);
    }

    // Fd-based; see `attaching_by_fd_verifies_generation_and_key`'s comment.
    // `two_mappings_of_one_segment_see_each_others_writes_by_name` below
    // covers the same "two live mappings observe each other's writes"
    // property through the name-based path every platform shares.
    #[cfg(unix)]
    #[test]
    fn two_mappings_of_one_segment_see_each_others_writes() {
        let segment = Segment::create(key(1), SegmentConfig::new(4, 512).unwrap()).unwrap();
        let attached = Segment::open_fd(segment.try_clone_fd().unwrap(), Some(&key(1))).unwrap();
        segment.header().publish_seq(11);
        assert_eq!(attached.header().write_seq(), 11);
        attached.mark_closed();
        assert!(segment.header().is_closed());
    }

    /// The name-based analogue of
    /// [`attaching_by_fd_verifies_generation_and_key`] and
    /// [`two_mappings_of_one_segment_see_each_others_writes`]: everything
    /// those two prove through a Unix-only `SCM_RIGHTS` descriptor, this
    /// proves through [`Segment::attach`] — the path every platform has,
    /// and the *only* attach path on Windows (see `crate::os::windows`'s
    /// module docs).
    #[test]
    fn attaching_by_name_verifies_generation_and_two_mappings_see_each_others_writes() {
        let key = named_key(6);
        let config = SegmentConfig::new(4, 512)
            .unwrap()
            .with_backing(Backing::Named);
        let segment = Segment::create(key.clone(), config).unwrap();

        let attached = Segment::attach(&key).unwrap();
        assert_eq!(attached.header().generation(), 6);
        assert_eq!(attached.layout(), segment.layout());
        assert!(!attached.owns_name(), "an attach must not own the name");

        // A mapping from a previous incarnation is rejected, not silently
        // used — the same staleness guarantee `open_fd` gives on Unix.
        let stale = Segment::open_named(&key.os_name(), Some(&key.clone().with_generation(5)));
        assert!(matches!(
            stale,
            Err(ShmError::StaleGeneration {
                expected: 5,
                found: 6
            })
        ));

        // The two mappings alias the same segment: a write through one is
        // visible through the other.
        segment.header().publish_seq(11);
        assert_eq!(attached.header().write_seq(), 11);
        attached.mark_closed();
        assert!(segment.header().is_closed());
    }

    #[test]
    fn named_segments_attach_by_name_and_unlink_on_drop() {
        let key = named_key(1);
        let config = SegmentConfig::new(4, 256)
            .unwrap()
            .with_backing(Backing::Named);
        let name = key.os_name();
        {
            let segment = Segment::create(key.clone(), config).unwrap();
            assert_eq!(segment.name(), Some(&name));
            assert!(segment.owns_name());
            let attached = Segment::attach(&key).unwrap();
            assert_eq!(attached.header().generation(), 1);
            assert!(attached.name().is_some());
        }
        assert!(
            Segment::attach(&key).is_err(),
            "dropping the creator must unlink the name"
        );
    }

    #[test]
    fn explicit_unlink_is_idempotent_and_survives_live_mappings() {
        let key = named_key(2);
        let config = SegmentConfig::new(2, 256)
            .unwrap()
            .with_backing(Backing::Named);
        let mut segment = Segment::create(key.clone(), config).unwrap();
        let attached = Segment::attach(&key).unwrap();
        segment.unlink().unwrap();
        segment.unlink().unwrap();
        segment.disown_name();
        assert!(!segment.owns_name());
        // The live mapping keeps working after the name is gone.
        segment.header().publish_seq(4);
        assert_eq!(attached.header().write_seq(), 4);
        assert!(Segment::attach(&key).is_err());
    }

    #[test]
    fn only_one_producer_may_be_claimed_per_mapping() {
        let segment = Segment::create(key(1), SegmentConfig::new(2, 128).unwrap()).unwrap();
        segment.claim_producer().unwrap();
        assert!(segment.claim_producer().is_err());
        segment.release_producer();
        segment.claim_producer().unwrap();
    }

    // Re-attaches by fd; see `attaching_by_fd_verifies_generation_and_key`'s
    // comment.
    #[cfg(unix)]
    #[test]
    fn the_overflow_policy_survives_a_round_trip_through_the_header() {
        let config = SegmentConfig::new(4, 256)
            .unwrap()
            .with_overflow(OverflowPolicy::Overwrite);
        let segment = Segment::create(key(1), config).unwrap();
        assert_eq!(
            segment.header().overflow_policy(),
            OverflowPolicy::Overwrite
        );
        let attached = Segment::open_fd(segment.try_clone_fd().unwrap(), None).unwrap();
        assert_eq!(
            attached.header().overflow_policy(),
            OverflowPolicy::Overwrite
        );
    }

    #[test]
    fn debug_rendering_names_the_segment_without_dumping_the_mapping() {
        let segment = Segment::create(key(1), SegmentConfig::new(2, 128).unwrap()).unwrap();
        let rendered = format!("{segment:?}");
        assert!(rendered.contains("camera/image/1"), "{rendered}");
        assert!(rendered.contains("write_seq"), "{rendered}");
    }

    // Uses the Unix-only `os::create_named`/`os::set_len`/`os::map_shared`
    // trio to build the hostile "small object, large header" fixture
    // directly; `os::windows` fuses those three steps into
    // `create_named_mapped` (see its doc comment), so this exact
    // construction does not translate. The property it proves —
    // `open_named` refuses a header describing more than the mapping
    // holds — is exercised on Windows too, by `map_for_attach`'s own
    // `MappingTooSmall` check.
    #[cfg(unix)]
    #[test]
    fn a_header_describing_more_than_the_object_holds_is_refused() {
        // A legitimate header for a large segment…
        let template = Segment::create(key(3), SegmentConfig::new(64, 65_536).unwrap()).unwrap();
        // SAFETY: the header is the first `HEADER_LEN` bytes of a live
        // mapping, and this borrow does not outlive `template`.
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(template.header()).cast::<u8>(),
                crate::layout::HEADER_LEN as usize,
            )
        };

        // …stamped into an object far too small to hold what it describes.
        // This is the `SIGBUS`-on-first-touch case that `fstat` now catches.
        let name =
            SegmentName::from_digest(u128::from(os::current_pid() as u64) ^ 0x7e57_0bfe_c700);
        let _ = os::unlink_named(&name);
        let small = os::create_named(&name).unwrap();
        os::set_len(small.as_fd(), 4096).unwrap();
        let mapping = os::map_shared(small.as_fd(), 4096).unwrap();
        // SAFETY: the mapping is 4096 bytes and the copy is 128.
        unsafe {
            std::ptr::copy_nonoverlapping(
                header_bytes.as_ptr(),
                mapping.as_ptr(),
                header_bytes.len(),
            );
            os::unmap(mapping, 4096);
        }

        let attached = Segment::open_named(&name, None);
        assert!(
            matches!(attached, Err(ShmError::CorruptHeader { .. })),
            "a header describing more than the object holds must not map: {attached:?}"
        );
        os::unlink_named(&name).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_default_backing_is_anonymous_on_linux() {
        let segment = Segment::create(key(1), SegmentConfig::new(2, 128).unwrap()).unwrap();
        assert!(segment.name().is_none());
        assert!(
            Segment::attach(&key(1)).is_err(),
            "an anonymous segment has no name to attach to"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn memfd_is_refused_where_it_does_not_exist() {
        let config = SegmentConfig::new(2, 128)
            .unwrap()
            .with_backing(Backing::Memfd);
        assert!(matches!(
            Segment::create(key(1), config),
            Err(ShmError::Unsupported { .. })
        ));
    }
}
