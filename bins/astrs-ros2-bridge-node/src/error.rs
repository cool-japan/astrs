//! The bridge's error taxonomy.
//!
//! Four layers, each with its own enum, because they fail for genuinely
//! different reasons and a caller wants to tell them apart:
//!
//! | Error | Raised by | Means |
//! |---|---|---|
//! | [`ConfigError`] | [`crate::config`] | the spawn handshake did not carry a usable `ros2:` block |
//! | [`PlanError`] | [`mod@crate::plan`] | the block and the node's declared ports do not agree |
//! | [`ResolveError`] | [`crate::resolve`] | a `message_type:`/`service:`/`action:` type is not known to this build |
//! | [`CodecError`] | [`crate::codec`] | a message would not cross the CDR ⇄ columnar boundary |
//!
//! [`BridgeError`] is the union the binary reports, and every variant of it
//! is a *typed startup error* rather than a panic — the blueprint's §10.5
//! bridge is a supervised node, and a node that panics on an unknown type
//! restarts forever under `restart_policy: always` without ever printing
//! the type's name.

use astrs_node_api::NodeError;
use astrs_ros2::Ros2Error;

/// Reading the `ros2:` block out of the spawn handshake failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The node's source is not [`astrs_wire::NodeSource::Ros2Bridge`].
    ///
    /// The daemon picks the bridge binary *because* the source is a bridge
    /// (`astrs-daemon`'s `command_for`), so this means the binary was
    /// started by hand or by a daemon of a different vintage.
    #[error(
        "this node was spawned with a {kind} source, not a ros2 bridge; \
         `astrs-ros2-bridge-node` is only startable from a manifest node \
         that carries a `ros2:` block"
    )]
    NotABridge {
        /// The source kind that was found, from
        /// [`astrs_wire::NodeSource::kind_name`].
        kind: &'static str,
    },

    /// The blob carried something that is not a serialized
    /// [`astrs_manifest::Ros2Config`].
    ///
    /// Older daemons forwarded only the manifest node id here; the message
    /// says so, because that is the one failure a user can act on by
    /// upgrading rather than by editing their manifest.
    #[error(
        "the spawn handshake did not carry a ros2 bridge configuration \
         ({source}); the daemon must forward the manifest's `ros2:` block \
         as JSON in `NodeSource::Ros2Bridge`"
    )]
    Malformed {
        /// The JSON error underneath.
        #[from]
        source: serde_json::Error,
    },

    /// An environment variable that must parse did not.
    #[error("the environment variable {name} is not a {expected}: {value:?}")]
    BadEnv {
        /// The variable's name.
        name: &'static str,
        /// What it should have been.
        expected: &'static str,
        /// What was actually set.
        value: String,
    },
}

/// The `ros2:` block and the node's declared ports do not agree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// The block names none of `topic:`, `topics:`, `service:` or `action:`.
    #[error(
        "the ros2 block bridges nothing: name a `topic:` (with \
         `message_type:` and `direction:`), a `topics:` list, a `service:` \
         or an `action:`"
    )]
    Empty,

    /// The single-topic form is missing one of its three fields.
    #[error("`{present}` was given without `{missing}`; the single-topic form needs both")]
    IncompleteTopic {
        /// The field that was present.
        present: &'static str,
        /// The field that was not.
        missing: &'static str,
    },

    /// `service:`/`action:` was given without `role:`.
    #[error("`{kind}: {name}` needs a `role:` of `client` or `server`")]
    MissingRole {
        /// `service` or `action`.
        kind: &'static str,
        /// The name that was given.
        name: String,
    },

    /// Two bridged entries would drive the same AstRS port.
    #[error("{first} and {second} both bridge onto the {direction} `{port}`")]
    DuplicatePort {
        /// The first claimant, as `topic /scan`.
        first: String,
        /// The second claimant.
        second: String,
        /// `"output"` or `"input"`.
        direction: &'static str,
        /// The port they collide on.
        port: String,
    },

    /// No declared port matches what a bridged entry needs.
    #[error(
        "{entry} needs {article} {direction} named `{wanted}`, but this node \
         declares {declared}"
    )]
    NoSuchPort {
        /// The entry that wanted a port, as `topic /scan`.
        entry: String,
        /// `"an"`/`"a"`, so the sentence reads.
        article: &'static str,
        /// `"output"` or `"input"`.
        direction: &'static str,
        /// The port name that was looked for.
        wanted: String,
        /// What the node actually declares, already rendered.
        declared: String,
    },

    /// A bridged name cannot become a legal ROS 2 name.
    #[error("`{name}` is not a usable ROS 2 {kind} name: {reason}")]
    BadRosName {
        /// The offending name.
        name: String,
        /// `topic`, `service` or `action`.
        kind: &'static str,
        /// Why it was refused.
        reason: String,
    },
}

/// A ROS 2 interface type is not one this build can bridge.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The type is neither pre-generated nor findable on an ament tree.
    #[error(
        "no {kind} definition for `{type_name}`: it is not in the \
         pre-generated `common_interfaces` set, and {search}"
    )]
    UnknownType {
        /// `message`, `service` or `action`.
        kind: &'static str,
        /// The type as the manifest spelled it.
        type_name: String,
        /// What was (or was not) searched, already rendered.
        search: String,
    },

    /// The name is not `pkg/kind/Type` (nor the legacy `pkg/Type`).
    #[error(
        "`{type_name}` is not a ROS 2 interface name; write it as \
         `package/{kind}/TypeName`, e.g. `sensor_msgs/msg/LaserScan`"
    )]
    MalformedName {
        /// The name that was given.
        type_name: String,
        /// The interface directory the name should have named.
        kind: &'static str,
    },

    /// An ament prefix was configured but could not be read.
    #[error("the interface search path entry `{path}` could not be read: {reason}")]
    SearchPath {
        /// The entry that failed.
        path: String,
        /// Why.
        reason: String,
    },

    /// A definition was found on disk but would not parse.
    #[error("`{type_name}` was found at `{path}` but does not parse: {reason}")]
    Parse {
        /// The type being resolved.
        type_name: String,
        /// Where its definition was read from.
        path: String,
        /// The parser's complaint.
        reason: String,
    },

    /// A found definition refers to a type that is itself unavailable.
    #[error("`{type_name}` refers to `{missing}`, which is not available")]
    MissingDependency {
        /// The type being resolved.
        type_name: String,
        /// The reference that could not be satisfied.
        missing: String,
    },
}

/// A message would not cross the CDR ⇄ columnar boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// The CDR octets did not decode as the declared type.
    #[error("a `{type_name}` sample ({len} octets) did not decode: {source}")]
    Cdr {
        /// The declared type.
        type_name: &'static str,
        /// How many octets arrived.
        len: usize,
        /// The CDR reader's complaint.
        source: astrs_cdr::CdrError,
    },

    /// The columnar batch did not match the declared type's layout.
    #[error("a `{type_name}` batch did not convert: {source}")]
    Columnar {
        /// The declared type.
        type_name: &'static str,
        /// The columnar layer's complaint.
        source: astrs_data::DataError,
    },
}

/// Anything the bridge node reports.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BridgeError {
    /// Joining the dataflow failed.
    #[error("node initialization failed: {0}")]
    Node(#[from] NodeError),
    /// Reading the `ros2:` block failed.
    #[error("bridge configuration: {0}")]
    Config(#[from] ConfigError),
    /// Turning the block into endpoints failed.
    #[error("bridge plan: {0}")]
    Plan(#[from] PlanError),
    /// A declared interface type is unavailable.
    #[error("interface type: {0}")]
    Resolve(#[from] ResolveError),
    /// A conversion failed on a path that cannot continue.
    #[error("conversion: {0}")]
    Codec(#[from] CodecError),
    /// The ROS 2 side failed.
    #[error("ros2: {0}")]
    Ros2(#[from] Ros2Error),
    /// The bridge needed an async runtime and could not have one.
    #[error("the bridge needs a multi-threaded tokio runtime: {0}")]
    Runtime(String),
}

impl BridgeError {
    /// The process exit code this error should produce.
    ///
    /// Everything the bridge can fail with is a configuration fault except
    /// [`BridgeError::Node`], which usually means the daemon went away —
    /// `1` for that, so a `restart_policy: on-failure` node retries, and
    /// `78` (the `sysexits.h` `EX_CONFIG`) for the rest, so a supervisor
    /// can tell a bad manifest from a lost daemon without parsing text.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Node(_) | Self::Runtime(_) => 1,
            Self::Config(_) | Self::Plan(_) | Self::Resolve(_) | Self::Codec(_) | Self::Ros2(_) => {
                78
            }
        }
    }

    /// Whether this error was raised before any traffic could flow.
    ///
    /// A startup error is worth printing in full; a mid-flight one has
    /// already been logged in context.
    #[must_use]
    pub const fn is_startup(&self) -> bool {
        matches!(self, Self::Config(_) | Self::Plan(_) | Self::Resolve(_))
    }
}

/// The bridge's result alias.
pub type BridgeResult<T> = Result<T, BridgeError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_configuration_fault_exits_with_ex_config() {
        let error = BridgeError::Plan(PlanError::Empty);
        assert_eq!(error.exit_code(), 78);
        assert!(error.is_startup());
    }

    #[test]
    fn a_lost_daemon_exits_with_one() {
        let error = BridgeError::Runtime("no runtime".to_owned());
        assert_eq!(error.exit_code(), 1);
        assert!(!error.is_startup());
    }

    #[test]
    fn the_not_a_bridge_message_names_the_source_kind() {
        let error = ConfigError::NotABridge { kind: "executable" };
        assert!(error.to_string().contains("executable"), "{error}");
    }

    #[test]
    fn an_unknown_type_names_the_type_and_the_search() {
        let error = ResolveError::UnknownType {
            kind: "message",
            type_name: "my_msgs/msg/Custom".to_owned(),
            search: "no interface search path is configured".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("my_msgs/msg/Custom"), "{text}");
        assert!(text.contains("no interface search path"), "{text}");
    }

    #[test]
    fn a_missing_port_renders_a_readable_sentence() {
        let error = PlanError::NoSuchPort {
            entry: "topic /scan".to_owned(),
            article: "an",
            direction: "output",
            wanted: "scan".to_owned(),
            declared: "no outputs at all".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "topic /scan needs an output named `scan`, but this node declares no outputs at all"
        );
    }

    #[test]
    fn a_duplicate_port_names_both_claimants() {
        let error = PlanError::DuplicatePort {
            first: "topic /a".to_owned(),
            second: "topic /b".to_owned(),
            direction: "output",
            port: "out".to_owned(),
        };
        let text = error.to_string();
        assert!(
            text.contains("topic /a") && text.contains("topic /b"),
            "{text}"
        );
    }
}
