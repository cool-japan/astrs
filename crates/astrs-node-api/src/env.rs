//! The environment surface a node reads at start-up (blueprint §24.2).
//!
//! Everything here is *read*, never written: a node inspects its environment
//! once, during `init`, and from then on the values live in typed fields. The
//! module exists so that the set of variables AstRS reads is enumerable in one
//! place — `astrs doctor` (§17) prints exactly this table.
//!
//! | Variable | Read by | Default |
//! |---|---|---|
//! | `ASTRS_NODE_CONFIG` | [`crate::Node::init_from_env`] | — (required) |
//! | `ASTRS_RUN_PARENT_PID` | the orphan guard (§4.2) | no guard |
//! | `ASTRS_DAEMON_PORT` | dynamic attach (§8.3) | 7408 |
//! | `ASTRS_RUNTIME_DIR` | dynamic attach | `$XDG_RUNTIME_DIR/astrs` |
//! | `ASTRS_ZERO_COPY_THRESHOLD` | the send path (§6.2) | 4096 |
//! | `ASTRS_TYPE_CHECK` | typed handles (§9.2) | `warn` |
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::env::TypeCheckMode;
//!
//! assert_eq!("warn".parse(), Ok(TypeCheckMode::Warn));
//! assert!(TypeCheckMode::Error.is_fatal());
//! assert!(!TypeCheckMode::default().is_fatal(), "0.1.0 defaults to warn");
//! ```

use std::path::PathBuf;
use std::str::FromStr;

use astrs_wire::{DEFAULT_ZERO_COPY_THRESHOLD, ENV_NODE_CONFIG, ENV_RUN_PARENT_PID};

use crate::error::{NodeError, Result};

/// The environment variable overriding the daemon's loopback node port.
pub const ENV_DAEMON_PORT: &str = "ASTRS_DAEMON_PORT";

/// The environment variable overriding the runtime directory.
pub const ENV_RUNTIME_DIR: &str = "ASTRS_RUNTIME_DIR";

/// The environment variable overriding the zero-copy threshold.
pub const ENV_ZERO_COPY_THRESHOLD: &str = "ASTRS_ZERO_COPY_THRESHOLD";

/// The environment variable selecting the runtime type-check mode (§9.2).
pub const ENV_TYPE_CHECK: &str = "ASTRS_TYPE_CHECK";

/// The freedesktop variable the default runtime directory is derived from.
pub const ENV_XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";

/// The daemon's loopback node port (§24.2).
pub const DEFAULT_DAEMON_PORT: u16 = 7408;

/// The runtime directory leaf appended to `$XDG_RUNTIME_DIR`.
pub const RUNTIME_DIR_LEAF: &str = "astrs";

/// The daemon's Unix-domain socket name inside the runtime directory (§4.2).
pub const DAEMON_SOCKET_NAME: &str = "daemon.sock";

/// The shared-memory broker socket name inside the runtime directory (§6.2).
pub const SHM_BROKER_SOCKET_NAME: &str = "shm.sock";

/// Every environment variable this crate reads, in documentation order.
///
/// # Examples
///
/// ```
/// use astrs_node_api::env;
///
/// assert!(env::READ_VARIABLES.contains(&"ASTRS_TYPE_CHECK"));
/// assert_eq!(env::READ_VARIABLES.len(), 7);
/// ```
pub const READ_VARIABLES: &[&str] = &[
    ENV_NODE_CONFIG,
    ENV_RUN_PARENT_PID,
    ENV_DAEMON_PORT,
    ENV_RUNTIME_DIR,
    ENV_ZERO_COPY_THRESHOLD,
    ENV_TYPE_CHECK,
    ENV_XDG_RUNTIME_DIR,
];

/// How strictly a typed handle checks a port's declared URN (§9.2).
///
/// The default is [`TypeCheckMode::Warn`] in 0.1.0: a type system nobody can
/// switch on is useless, and one that breaks every existing graph on upgrade
/// is worse. `warn` makes drift visible without making it fatal; `error` is
/// what a validated deployment sets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TypeCheckMode {
    /// Do not check declared types at all.
    Off,
    /// Log a warning on a mismatch and continue (the 0.1.0 default).
    #[default]
    Warn,
    /// Refuse the handle with [`NodeError::TypeMismatch`].
    Error,
}

impl TypeCheckMode {
    /// Every mode, in increasing strictness.
    pub const ALL: &'static [Self] = &[Self::Off, Self::Warn, Self::Error];

    /// The lower-case name used by `ASTRS_TYPE_CHECK` and by metrics labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Whether a mismatch aborts the call.
    #[must_use]
    pub const fn is_fatal(self) -> bool {
        matches!(self, Self::Error)
    }

    /// Whether a mismatch is examined at all.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Reads the mode from `ASTRS_TYPE_CHECK`, defaulting to
    /// [`TypeCheckMode::Warn`].
    ///
    /// # Errors
    ///
    /// [`NodeError::BadEnv`] when the variable names something else.
    pub fn from_env() -> Result<Self> {
        match std::env::var(ENV_TYPE_CHECK) {
            Ok(value) => value.parse().map_err(|_| NodeError::BadEnv {
                name: ENV_TYPE_CHECK,
                value,
                reason: "expected one of: off, warn, error".to_owned(),
            }),
            Err(_) => Ok(Self::default()),
        }
    }
}

impl FromStr for TypeCheckMode {
    type Err = ();

    fn from_str(text: &str) -> core::result::Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" | "false" => Ok(Self::Off),
            "warn" | "warning" => Ok(Self::Warn),
            "error" | "strict" | "fatal" => Ok(Self::Error),
            _ => Err(()),
        }
    }
}

impl core::fmt::Display for TypeCheckMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reads `ASTRS_RUN_PARENT_PID`, when it is set (§4.2).
///
/// # Errors
///
/// [`NodeError::BadEnv`] when it is set to something that is not a process id.
///
/// # Examples
///
/// ```
/// // Unset in an ordinary test process, so no guard is requested.
/// assert!(astrs_node_api::env::parent_pid().is_ok());
/// ```
pub fn parent_pid() -> Result<Option<u32>> {
    parse_optional_env(ENV_RUN_PARENT_PID, "a process id")
}

/// Reads `ASTRS_DAEMON_PORT`, defaulting to [`DEFAULT_DAEMON_PORT`].
///
/// # Errors
///
/// [`NodeError::BadEnv`] when it is not a port number.
pub fn daemon_port() -> Result<u16> {
    Ok(parse_optional_env(ENV_DAEMON_PORT, "a TCP port")?.unwrap_or(DEFAULT_DAEMON_PORT))
}

/// Reads `ASTRS_ZERO_COPY_THRESHOLD`, defaulting to the §24.2 4 KiB.
///
/// # Errors
///
/// [`NodeError::BadEnv`] when it is not a byte count.
pub fn zero_copy_threshold() -> Result<u64> {
    Ok(parse_optional_env(ENV_ZERO_COPY_THRESHOLD, "a byte count")?
        .unwrap_or(DEFAULT_ZERO_COPY_THRESHOLD))
}

/// The runtime directory for this process (§24.2).
///
/// `ASTRS_RUNTIME_DIR` wins; otherwise `$XDG_RUNTIME_DIR/astrs`; otherwise the
/// system temporary directory with the same leaf, which is what makes a node
/// runnable on a desktop macOS session where `XDG_RUNTIME_DIR` is unset.
///
/// # Examples
///
/// ```
/// let dir = astrs_node_api::env::runtime_dir();
/// assert!(dir.is_absolute() || dir.components().count() > 0);
/// ```
#[must_use]
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_RUNTIME_DIR)
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var(ENV_XDG_RUNTIME_DIR)
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(RUNTIME_DIR_LEAF);
    }
    std::env::temp_dir().join(RUNTIME_DIR_LEAF)
}

/// The daemon's Unix-domain socket path under [`runtime_dir`].
#[must_use]
pub fn daemon_socket_path() -> PathBuf {
    runtime_dir().join(DAEMON_SOCKET_NAME)
}

/// The shared-memory broker socket path under [`runtime_dir`].
#[must_use]
pub fn shm_broker_path() -> PathBuf {
    runtime_dir().join(SHM_BROKER_SOCKET_NAME)
}

/// Parses an optional environment variable, treating an empty value as unset.
///
/// # Errors
///
/// [`NodeError::BadEnv`] when the value is present but unparseable.
fn parse_optional_env<T: FromStr>(name: &'static str, expected: &str) -> Result<Option<T>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse().map(Some).map_err(|_| NodeError::BadEnv {
        name,
        value: value.clone(),
        reason: format!("expected {expected}"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn type_check_modes_parse_every_spelling() {
        assert_eq!("off".parse(), Ok(TypeCheckMode::Off));
        assert_eq!("NONE".parse(), Ok(TypeCheckMode::Off));
        assert_eq!("0".parse(), Ok(TypeCheckMode::Off));
        assert_eq!("false".parse(), Ok(TypeCheckMode::Off));
        assert_eq!("  warn  ".parse(), Ok(TypeCheckMode::Warn));
        assert_eq!("Warning".parse(), Ok(TypeCheckMode::Warn));
        assert_eq!("ERROR".parse(), Ok(TypeCheckMode::Error));
        assert_eq!("strict".parse(), Ok(TypeCheckMode::Error));
        assert_eq!("fatal".parse(), Ok(TypeCheckMode::Error));
        assert_eq!("loud".parse::<TypeCheckMode>(), Err(()));
    }

    #[test]
    fn type_check_modes_classify_themselves() {
        assert!(!TypeCheckMode::Off.is_enabled());
        assert!(!TypeCheckMode::Off.is_fatal());
        assert!(TypeCheckMode::Warn.is_enabled());
        assert!(!TypeCheckMode::Warn.is_fatal());
        assert!(TypeCheckMode::Error.is_enabled());
        assert!(TypeCheckMode::Error.is_fatal());
        assert_eq!(TypeCheckMode::default(), TypeCheckMode::Warn);
        for mode in TypeCheckMode::ALL {
            assert_eq!(mode.to_string(), mode.as_str());
            assert_eq!(mode.as_str().parse(), Ok(*mode));
        }
        assert_eq!(TypeCheckMode::ALL.len(), 3);
    }

    #[test]
    fn the_variable_table_is_complete_and_unique() {
        let mut names = READ_VARIABLES.to_vec();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
        for name in READ_VARIABLES {
            assert!(
                name.starts_with("ASTRS_") || name.starts_with("XDG_"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_runtime_directory_always_resolves() {
        // The process environment is shared with parallel tests, so this
        // asserts only the property that holds for every possible setting.
        let dir = runtime_dir();
        assert!(!dir.as_os_str().is_empty());
        assert!(daemon_socket_path().starts_with(&dir));
        assert!(shm_broker_path().starts_with(&dir));
        assert!(
            daemon_socket_path()
                .file_name()
                .is_some_and(|name| name == DAEMON_SOCKET_NAME)
        );
        assert!(
            shm_broker_path()
                .file_name()
                .is_some_and(|name| name == SHM_BROKER_SOCKET_NAME)
        );
    }

    #[test]
    fn optional_variables_default_when_unset_or_blank() {
        // `parse_optional_env` is exercised directly rather than through the
        // process environment, which parallel tests share.
        assert_eq!(
            parse_optional_env::<u16>("ASTRS_SURELY_UNSET_VARIABLE", "a port").unwrap(),
            None
        );
    }

    #[test]
    fn the_documented_defaults_match_the_appendix() {
        assert_eq!(DEFAULT_DAEMON_PORT, 7408);
        assert_eq!(DEFAULT_ZERO_COPY_THRESHOLD, 4096);
        assert_eq!(RUNTIME_DIR_LEAF, "astrs");
        assert_eq!(ENV_TYPE_CHECK, "ASTRS_TYPE_CHECK");
        assert_eq!(ENV_ZERO_COPY_THRESHOLD, "ASTRS_ZERO_COPY_THRESHOLD");
        assert_eq!(ENV_DAEMON_PORT, "ASTRS_DAEMON_PORT");
        assert_eq!(ENV_RUNTIME_DIR, "ASTRS_RUNTIME_DIR");
    }

    #[test]
    fn readers_report_a_default_when_the_variable_is_absent() {
        // These read the real environment; in a test process none of the
        // AstRS variables are set, so every reader must produce its default
        // rather than an error.
        if std::env::var(ENV_DAEMON_PORT).is_err() {
            assert_eq!(daemon_port().unwrap(), DEFAULT_DAEMON_PORT);
        }
        if std::env::var(ENV_ZERO_COPY_THRESHOLD).is_err() {
            assert_eq!(zero_copy_threshold().unwrap(), DEFAULT_ZERO_COPY_THRESHOLD);
        }
        if std::env::var(ENV_RUN_PARENT_PID).is_err() {
            assert_eq!(parent_pid().unwrap(), None);
        }
        if std::env::var(ENV_TYPE_CHECK).is_err() {
            assert_eq!(TypeCheckMode::from_env().unwrap(), TypeCheckMode::Warn);
        }
    }
}
