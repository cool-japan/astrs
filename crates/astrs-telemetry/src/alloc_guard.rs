//! A counting global allocator, compiled in only for this crate's own
//! unit-test binary, that backs the allocation-free proof in
//! [`crate::metrics`].
//!
//! # Why a global allocator, and why only here
//!
//! There is no portable, allocator-independent way to ask "did the last N
//! lines of code allocate?" from safe Rust. Wrapping [`std::alloc::System`]
//! in a counting [`std::alloc::GlobalAlloc`] and installing it via
//! `#[global_allocator]` is the standard technique (the same one
//! `stats_alloc`-style crates use, reimplemented here rather than adding
//! a dependency for one counter). `#[global_allocator]` may be declared at
//! most once in a linked binary; gating the `static` behind `#[cfg(test)]`
//! keeps it out of every normal build and confines it to the binary
//! `cargo test`/`cargo nextest` produce for *this crate's* `src/`-resident
//! unit tests. It has no effect on integration tests under `tests/`
//! (those link this crate as an ordinary rlib, compiled without
//! `cfg(test)`) — which is exactly why the allocation-free assertion in
//! [`crate::metrics::tests`] lives as a unit test, not an integration one.
//!
//! # Process-per-test assumption
//!
//! The counter is a single process-wide [`std::sync::atomic::AtomicU64`],
//! not a per-thread one: a per-thread counter risks a well-known
//! reentrancy hazard (the *first* access to a `thread_local!` on a given
//! thread can itself allocate on some platforms, which would recurse back
//! into this allocator). A process-wide counter has no such hazard, but
//! it does mean the measurement is only exact under a process-per-test
//! runner — `cargo nextest` (this project's primary runner), which is
//! exactly why the test that reads it retries a few times: see
//! [`crate::metrics::tests::proves_the_record_path_is_allocation_free`].

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// Total allocation-class calls (`alloc`, `alloc_zeroed`, `realloc`)
/// observed since the process started.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Delegates to [`System`], counting every call that can grow or move the
/// heap.
struct CountingAllocator;

// SAFETY: every method forwards, byte-for-byte, to `System`'s
// implementation of the same method; the only addition is a relaxed
// atomic increment before the call, which is sound to perform from
// within an allocator (it touches no thread-local or lazily-initialized
// state — see the module docs for why that distinction matters here).
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// The running total of allocation-class calls observed so far in this
/// process.
///
/// Meant to be read twice, around a region of interest, and compared:
/// `after == before` means that region performed no allocation, growth,
/// or reallocation (deallocation-only activity, e.g. dropping a
/// pre-existing `Vec`, does not move this counter).
#[must_use]
pub fn allocations_so_far() -> u64 {
    ALLOCATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_heap_allocation_advances_the_counter() {
        let before = allocations_so_far();
        let v: Vec<u8> = Vec::with_capacity(64);
        let after = allocations_so_far();
        assert!(after > before, "Vec::with_capacity(64) must allocate");
        drop(v);
    }

    #[test]
    fn purely_stack_work_does_not_advance_the_counter() {
        let before = allocations_so_far();
        let mut sum = 0u64;
        for i in 0..1_000u64 {
            sum = sum.wrapping_add(i);
        }
        let after = allocations_so_far();
        assert_eq!(sum, (0..1_000u64).sum::<u64>());
        assert_eq!(after, before);
    }
}
