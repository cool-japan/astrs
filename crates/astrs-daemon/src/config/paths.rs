//! Where the daemon puts things (§24.2).
//!
//! | Item | Default | Override |
//! |---|---|---|
//! | Runtime dir | `$XDG_RUNTIME_DIR/astrs` | `ASTRS_RUNTIME_DIR` |
//! | Node socket | `<runtime dir>/daemon.sock` | [`RuntimePaths::with_socket_name`] |
//! | SHM broker socket | `<runtime dir>/shm.sock` | — |
//!
//! On a machine with no `XDG_RUNTIME_DIR` (macOS, a bare container, a cron
//! job) the fallback is a per-user directory under [`std::env::temp_dir`],
//! because a daemon that refuses to start for want of an XDG variable is a
//! daemon nobody can run.
//!
//! # Socket names are short on purpose
//!
//! `sun_path` is 104 bytes on Darwin and 108 on Linux — for the *whole*
//! path, not the file name. A socket named after a dataflow UUID inside a
//! macOS temp directory (`/var/folders/xx/yyyy…/T/`) overflows it, and the
//! resulting `EINVAL` is famously hard to read. [`RuntimePaths::socket_path`]
//! is therefore built from a short, fixed base name, and
//! [`check_socket_path_len`] rejects an over-long one with a message that says
//! what actually happened.

use std::path::{Path, PathBuf};

use crate::error::{DaemonError, DaemonResult};

/// The environment variable overriding the runtime directory (§24.2).
pub const ENV_RUNTIME_DIR: &str = "ASTRS_RUNTIME_DIR";

/// The XDG variable the runtime directory defaults to.
pub const ENV_XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";

/// The directory name appended to `$XDG_RUNTIME_DIR`.
pub const RUNTIME_DIR_NAME: &str = "astrs";

/// The default node-listener socket file name (§4.2).
pub const DEFAULT_SOCKET_NAME: &str = "daemon.sock";

/// The default SHM broker socket file name.
pub const DEFAULT_SHM_SOCKET_NAME: &str = "shm.sock";

/// The portable floor for `sockaddr_un::sun_path`, minus the NUL terminator.
///
/// Darwin's is 104 and Linux's is 108; the smaller one is the one that has to
/// hold, because a manifest that works on the developer's Linux box and fails
/// on the robot's macOS bridge is exactly the failure this constant prevents.
pub const MAX_SOCKET_PATH_LEN: usize = 103;

/// The daemon's directory layout.
///
/// # Examples
///
/// ```
/// use astrs_daemon::config::RuntimePaths;
///
/// let paths = RuntimePaths::under(std::env::temp_dir().join("astrs-doc"));
/// assert!(paths.socket_path().ends_with("daemon.sock"));
/// assert!(paths.shm_socket_path().ends_with("shm.sock"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    /// The directory everything lives under.
    root: PathBuf,
    /// The node listener socket's file name.
    socket_name: String,
    /// The SHM broker socket's file name.
    shm_socket_name: String,
}

impl RuntimePaths {
    /// The layout under an explicit root.
    #[must_use]
    pub fn under(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            socket_name: DEFAULT_SOCKET_NAME.to_string(),
            shm_socket_name: DEFAULT_SHM_SOCKET_NAME.to_string(),
        }
    }

    /// The layout the environment selects (§24.2).
    ///
    /// `ASTRS_RUNTIME_DIR` wins; then `$XDG_RUNTIME_DIR/astrs`; then a
    /// per-user directory under [`std::env::temp_dir`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::under(default_runtime_dir())
    }

    /// Uses a different socket file name — one daemon per name, so two
    /// daemons (a test and a real one) can share a runtime directory.
    #[must_use]
    pub fn with_socket_name(mut self, name: impl Into<String>) -> Self {
        self.socket_name = name.into();
        self
    }

    /// Uses a different SHM broker socket file name.
    #[must_use]
    pub fn with_shm_socket_name(mut self, name: impl Into<String>) -> Self {
        self.shm_socket_name = name.into();
        self
    }

    /// The directory everything lives under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The node listener socket.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.root.join(&self.socket_name)
    }

    /// The SHM broker socket (stage 2 uses it; stage 1 only reports it).
    #[must_use]
    pub fn shm_socket_path(&self) -> PathBuf {
        self.root.join(&self.shm_socket_name)
    }

    /// The per-dataflow log directory.
    #[must_use]
    pub fn log_dir(&self, dataflow: astrs_wire::DataflowId) -> PathBuf {
        self.root.join("logs").join(dataflow.to_string())
    }

    /// Creates the runtime directory if it does not exist, and checks that the
    /// socket path will fit in a `sockaddr_un`.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::Io`] if the directory cannot be created.
    /// - [`DaemonError::BadPath`] if the root exists but is not a directory,
    ///   or the socket path is too long for the platform.
    pub fn prepare(&self) -> DaemonResult<()> {
        match std::fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(DaemonError::BadPath {
                    what: "runtime dir",
                    path: self.root.clone(),
                    reason: "exists but is not a directory".into(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.root)
                    .map_err(|source| DaemonError::io("create runtime dir", source))?;
            }
            Err(source) => return Err(DaemonError::io("stat runtime dir", source)),
        }
        check_socket_path_len(&self.socket_path())
    }
}

impl Default for RuntimePaths {
    fn default() -> Self {
        Self::from_env()
    }
}

/// The runtime directory the environment selects.
#[must_use]
pub fn default_runtime_dir() -> PathBuf {
    if let Some(explicit) = non_empty_var(ENV_RUNTIME_DIR) {
        return PathBuf::from(explicit);
    }
    if let Some(xdg) = non_empty_var(ENV_XDG_RUNTIME_DIR) {
        return PathBuf::from(xdg).join(RUNTIME_DIR_NAME);
    }
    std::env::temp_dir().join(format!("{RUNTIME_DIR_NAME}-{}", current_uid()))
}

/// Reads an environment variable, treating an empty value as unset.
fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The effective user id, for the temp-directory fallback's per-user suffix.
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Checks that `path` fits in a `sockaddr_un`.
///
/// # Errors
///
/// [`DaemonError::BadPath`] with the measured length, because "invalid
/// argument" from `bind(2)` tells nobody anything.
pub fn check_socket_path_len(path: &Path) -> DaemonResult<()> {
    let len = path.as_os_str().as_encoded_bytes().len();
    if len > MAX_SOCKET_PATH_LEN {
        return Err(DaemonError::BadPath {
            what: "node socket",
            path: path.to_path_buf(),
            reason: format!(
                "the path is {len} bytes; a Unix socket path may be at most \
                 {MAX_SOCKET_PATH_LEN}. Set {ENV_RUNTIME_DIR} to something shorter"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("astrs-paths-{}-{name}", std::process::id()))
    }

    #[test]
    fn an_explicit_root_is_used_verbatim() {
        let paths = RuntimePaths::under("/run/astrs");
        assert_eq!(paths.root(), Path::new("/run/astrs"));
        assert_eq!(paths.socket_path(), PathBuf::from("/run/astrs/daemon.sock"));
        assert_eq!(
            paths.shm_socket_path(),
            PathBuf::from("/run/astrs/shm.sock")
        );
    }

    #[test]
    fn socket_names_are_overridable_so_two_daemons_can_share_a_root() {
        let first = RuntimePaths::under("/run/astrs").with_socket_name("a.sock");
        let second = RuntimePaths::under("/run/astrs").with_socket_name("b.sock");
        assert_ne!(first.socket_path(), second.socket_path());
        assert_eq!(first.root(), second.root());
    }

    #[test]
    fn the_shm_socket_name_is_overridable_too() {
        let paths = RuntimePaths::under("/run/astrs").with_shm_socket_name("broker.sock");
        assert_eq!(
            paths.shm_socket_path(),
            PathBuf::from("/run/astrs/broker.sock")
        );
    }

    #[test]
    fn log_directories_are_per_dataflow() {
        let paths = RuntimePaths::under("/run/astrs");
        let first = paths.log_dir(astrs_wire::DataflowId::from_u128(1));
        let second = paths.log_dir(astrs_wire::DataflowId::from_u128(2));
        assert_ne!(first, second);
        assert!(first.starts_with("/run/astrs/logs"));
    }

    #[test]
    fn prepare_creates_a_missing_directory() {
        let root = scratch("create");
        let _ = std::fs::remove_dir_all(&root);
        let paths = RuntimePaths::under(&root);
        paths.prepare().unwrap();
        assert!(root.is_dir());
        // Idempotent.
        paths.prepare().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prepare_refuses_a_root_that_is_a_file() {
        let root = scratch("file");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::write(&root, b"not a directory").unwrap();
        let error = RuntimePaths::under(&root).prepare().unwrap_err();
        assert!(matches!(error, DaemonError::BadPath { .. }), "{error}");
        let _ = std::fs::remove_file(&root);
    }

    #[test]
    fn an_over_long_socket_path_is_refused_with_a_readable_reason() {
        let long = PathBuf::from(format!("/tmp/{}/daemon.sock", "x".repeat(200)));
        let error = check_socket_path_len(&long).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("at most 103"), "{rendered}");
        assert!(rendered.contains(ENV_RUNTIME_DIR), "{rendered}");
    }

    #[test]
    fn a_short_socket_path_is_accepted() {
        check_socket_path_len(Path::new("/tmp/a.sock")).unwrap();
    }

    #[test]
    fn the_default_root_is_absolute_and_ends_somewhere_writable() {
        let root = default_runtime_dir();
        assert!(root.is_absolute(), "{}", root.display());
    }

    #[test]
    fn the_documented_constants_match_the_appendix() {
        assert_eq!(ENV_RUNTIME_DIR, "ASTRS_RUNTIME_DIR");
        assert_eq!(ENV_XDG_RUNTIME_DIR, "XDG_RUNTIME_DIR");
        assert_eq!(RUNTIME_DIR_NAME, "astrs");
        assert_eq!(DEFAULT_SOCKET_NAME, "daemon.sock");
    }
}
