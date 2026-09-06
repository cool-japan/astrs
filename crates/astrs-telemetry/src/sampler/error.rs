//! [`SamplerError`] — everything that can go wrong sampling a process's
//! CPU/RSS usage.

use thiserror::Error;

/// Errors from [`crate::sampler::CpuMemSampler`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SamplerError {
    /// The target process does not exist (or is not visible to this
    /// process) — checked via a `kill(pid, 0)`-equivalent probe before
    /// attempting the more expensive OS-specific read.
    #[error("process {pid} not found (exited, or not visible to this process)")]
    ProcessNotFound {
        /// The pid that was probed.
        pid: u32,
    },

    /// Reading or parsing an OS-provided source (`/proc/<pid>/stat`,
    /// `/proc/<pid>/status`, `getrusage(2)`) failed.
    #[error("failed to read process {pid}'s {source_name}: {message}")]
    Read {
        /// The pid being sampled.
        pid: u32,
        /// Which source failed (e.g. `"/proc/1234/stat"`).
        source_name: String,
        /// A human-readable description of the failure.
        message: String,
    },

    /// The macOS `ps`-fallback path (see
    /// [`crate::sampler::macos::MacSampler`]) could not run at all — the
    /// binary is missing from every path this crate tries. This is
    /// distinct from a *specific pid* not being found (which is a normal
    /// [`SamplerError::ProcessNotFound`], since `ps` itself ran
    /// successfully and simply had nothing to report for that pid).
    #[error("the 'ps' fallback sampler could not run: {message}")]
    PsFallback {
        /// A human-readable description of the failure.
        message: String,
    },

    /// This platform has no implemented sampler.
    #[error("process sampling is not implemented on this platform")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn messages_mention_the_pid() {
        let err = SamplerError::ProcessNotFound { pid: 4242 };
        assert!(err.to_string().contains("4242"));
    }
}
