//! Shared test-only helpers. Not part of the public API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty, uniquely named directory under
/// [`std::env::temp_dir`], created and ready to use.
///
/// Uniqueness comes from the process id plus a per-process atomic
/// counter, so parallel tests (nextest runs each test in its own thread
/// within one process by default) never collide, without depending on any
/// crate beyond `std`.
pub(crate) fn unique_temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut dir = std::env::temp_dir();
    dir.push(format!("astrs-log-test-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create unique temp dir for test");
    dir
}
