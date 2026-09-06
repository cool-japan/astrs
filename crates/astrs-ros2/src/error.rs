//! The error taxonomy an rcl-level call can fail with.
//!
//! One enum, grouped the way a caller has to *react* rather than the way the
//! layers below happen to be stacked:
//!
//! | Group | Variants | What a caller does |
//! |---|---|---|
//! | **Naming** | [`Ros2Error::InvalidNodeName`], [`InvalidNamespace`](Ros2Error::InvalidNamespace), [`InvalidTopicName`](Ros2Error::InvalidTopicName), [`InvalidServiceName`](Ros2Error::InvalidServiceName), [`NameTooLong`](Ros2Error::NameTooLong) | fix the string; never retry |
//! | **Configuration** | [`IncompatibleQos`](Ros2Error::IncompatibleQos), [`DuplicateEntity`](Ros2Error::DuplicateEntity), [`UnknownParameter`](Ros2Error::UnknownParameter), [`ParameterAlreadyDeclared`](Ros2Error::ParameterAlreadyDeclared), [`ParameterReadOnly`](Ros2Error::ParameterReadOnly), [`ParameterTypeMismatch`](Ros2Error::ParameterTypeMismatch), [`ParameterOutOfRange`](Ros2Error::ParameterOutOfRange), [`ParameterRejected`](Ros2Error::ParameterRejected) | fix the program; never retry |
//! | **Protocol** | [`Timeout`](Ros2Error::Timeout), [`ServiceUnavailable`](Ros2Error::ServiceUnavailable), [`GoalRejected`](Ros2Error::GoalRejected), [`UnknownGoal`](Ros2Error::UnknownGoal), [`GoalNotCancelable`](Ros2Error::GoalNotCancelable), [`GoalAlreadyTerminal`](Ros2Error::GoalAlreadyTerminal), [`MalformedRequestHeader`](Ros2Error::MalformedRequestHeader) | retry, or report to the operator |
//! | **Delegated** | [`Rtps`](Ros2Error::Rtps), [`Cdr`](Ros2Error::Cdr), [`Data`](Ros2Error::Data) | whatever the layer below says |
//! | **Lifecycle** | [`NodeShutDown`](Ros2Error::NodeShutDown) | stop |
//!
//! [`Ros2Error::is_transient`] is the predicate that split: true exactly for
//! the protocol group, false for everything else. A supervisor that retries
//! on `is_transient` and reports otherwise is doing the right thing without
//! matching on twenty-odd variants.

use core::fmt;

use astrs_rtps::behavior::QosPolicyId;

/// The result type every fallible call in this crate returns.
pub type Ros2Result<T> = Result<T, Ros2Error>;

/// Which kind of ROS 2 name failed validation.
///
/// Carried by [`Ros2Error::NameTooLong`] and the malformed-name variants so
/// one message can say *node namespace* rather than *name*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum NameKind {
    /// A node name: no slashes, no leading digit.
    NodeName,
    /// A node namespace: absolute, slash-separated tokens.
    Namespace,
    /// A topic name, before mangling.
    TopicName,
    /// A service name, before mangling.
    ServiceName,
    /// An action name, before mangling.
    ActionName,
    /// A parameter name.
    ParameterName,
}

impl NameKind {
    /// The noun phrase a message uses.
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::NodeName => "node name",
            Self::Namespace => "node namespace",
            Self::TopicName => "topic name",
            Self::ServiceName => "service name",
            Self::ActionName => "action name",
            Self::ParameterName => "parameter name",
        }
    }
}

impl fmt::Display for NameKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.noun())
    }
}

/// Why a ROS 2 name was rejected.
///
/// Split out of [`Ros2Error`] because the four name variants would otherwise
/// carry four copies of the same six reasons, and because the reason is what
/// a diagnostic wants to render next to the offending character index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum NameFault {
    /// The name was empty.
    Empty,
    /// A character outside `[A-Za-z0-9_/~{}]` appeared.
    IllegalCharacter {
        /// The offending character.
        character: char,
        /// Its byte offset in the name.
        offset: usize,
    },
    /// A token began with a digit.
    TokenStartsWithDigit {
        /// The offending token's byte offset.
        offset: usize,
    },
    /// Two `/` characters were adjacent, or the name ended with `/`.
    EmptyToken {
        /// The offending byte offset.
        offset: usize,
    },
    /// A `~` appeared anywhere but at the start.
    MisplacedTilde {
        /// The offending byte offset.
        offset: usize,
    },
    /// A substitution `{…}` was unbalanced or empty.
    MalformedSubstitution {
        /// The offending byte offset.
        offset: usize,
    },
    /// The name had to be absolute (start with `/`) and was not.
    NotAbsolute,
    /// The name must not contain `/` at all — a node name, for instance.
    UnexpectedSlash {
        /// The offending byte offset.
        offset: usize,
    },
    /// A `~` or `{…}` survived into a place that takes fully-qualified names
    /// only.
    NotFullyQualified,
}

impl fmt::Display for NameFault {
    #[allow(clippy::min_ident_chars)] // `f` is the conventional formatter name in this one spot.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("it is empty"),
            Self::IllegalCharacter { character, offset } => write!(
                f,
                "the character {character:?} at offset {offset} is not one of \
                 the alphanumerics, `_`, `/`, `~`, `{{` or `}}` ROS 2 allows"
            ),
            Self::TokenStartsWithDigit { offset } => {
                write!(f, "the token at offset {offset} starts with a digit")
            }
            Self::EmptyToken { offset } => write!(
                f,
                "the token at offset {offset} is empty — `//` and a trailing `/` are both illegal"
            ),
            Self::MisplacedTilde { offset } => write!(
                f,
                "the `~` at offset {offset} is not at the start, where the private-name \
                 substitution has to be"
            ),
            Self::MalformedSubstitution { offset } => write!(
                f,
                "the substitution starting at offset {offset} is unbalanced or empty"
            ),
            Self::NotAbsolute => f.write_str("it does not start with `/`"),
            Self::UnexpectedSlash { offset } => {
                write!(f, "it contains a `/` at offset {offset}")
            }
            Self::NotFullyQualified => f.write_str(
                "it still contains `~` or a `{substitution}` — expand it against a node first",
            ),
        }
    }
}

/// Everything an rcl-level call can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Ros2Error {
    /// A node name broke the ROS 2 rules.
    #[error("`{name}` is not a valid node name: {fault}")]
    InvalidNodeName {
        /// The name as given.
        name: String,
        /// Why it was rejected.
        fault: NameFault,
    },

    /// A node namespace broke the ROS 2 rules.
    #[error("`{namespace}` is not a valid node namespace: {fault}")]
    InvalidNamespace {
        /// The namespace as given.
        namespace: String,
        /// Why it was rejected.
        fault: NameFault,
    },

    /// A topic name broke the ROS 2 rules.
    #[error("`{name}` is not a valid topic name: {fault}")]
    InvalidTopicName {
        /// The name as given.
        name: String,
        /// Why it was rejected.
        fault: NameFault,
    },

    /// A service, action or parameter name broke the ROS 2 rules.
    #[error("`{name}` is not a valid {kind}: {fault}")]
    InvalidServiceName {
        /// Which kind of name it was.
        kind: NameKind,
        /// The name as given.
        name: String,
        /// Why it was rejected.
        fault: NameFault,
    },

    /// A name exceeded the length a discovery announcement can carry.
    #[error("this {kind} is {len} octets long; the limit is {limit}")]
    NameTooLong {
        /// Which kind of name it was.
        kind: NameKind,
        /// How long it was.
        len: usize,
        /// The ceiling.
        limit: usize,
    },

    /// A publisher and a subscription on one topic could not be paired.
    #[error("the {policy} QoS policy is incompatible between `{topic}`'s endpoints")]
    IncompatibleQos {
        /// The topic they disagree on.
        topic: String,
        /// The first policy that failed.
        policy: QosPolicyId,
    },

    /// Two entities of one node claimed the same name.
    #[error("this node already has a {kind} named `{name}`")]
    DuplicateEntity {
        /// What kind of entity it was.
        kind: &'static str,
        /// The name both wanted.
        name: String,
    },

    /// A parameter that was never declared was read or written.
    #[error("the parameter `{name}` has not been declared")]
    UnknownParameter {
        /// The name asked for.
        name: String,
    },

    /// `declare_parameter` was called twice for one name.
    #[error("the parameter `{name}` is already declared")]
    ParameterAlreadyDeclared {
        /// The name declared twice.
        name: String,
    },

    /// A read-only parameter was written after declaration.
    #[error("the parameter `{name}` is read-only")]
    ParameterReadOnly {
        /// The name written.
        name: String,
    },

    /// A parameter was set to a value of the wrong type.
    #[error("the parameter `{name}` is {expected} and cannot hold a {actual} value")]
    ParameterTypeMismatch {
        /// The name written.
        name: String,
        /// The declared type's ROS name.
        expected: &'static str,
        /// The offered type's ROS name.
        actual: &'static str,
    },

    /// A parameter was set outside its declared range.
    #[error("the parameter `{name}` was set to {value}, outside its declared range {range}")]
    ParameterOutOfRange {
        /// The name written.
        name: String,
        /// The offered value, rendered.
        value: String,
        /// The declared range, rendered.
        range: String,
    },

    /// An `on_set_parameters` callback refused the change.
    #[error("the parameter `{name}` was rejected: {reason}")]
    ParameterRejected {
        /// The name written.
        name: String,
        /// What the callback said.
        reason: String,
    },

    /// A call gave up waiting.
    #[error("{operation} timed out after {}ms", .elapsed_ms)]
    Timeout {
        /// What was being waited for.
        operation: &'static str,
        /// How long it waited.
        elapsed_ms: u64,
    },

    /// No server has been discovered for a service or action.
    #[error("no server has been discovered for `{name}`")]
    ServiceUnavailable {
        /// The service or action name.
        name: String,
    },

    /// An action server refused a goal.
    #[error("the action server refused goal {goal}")]
    GoalRejected {
        /// The goal's UUID, rendered.
        goal: String,
    },

    /// A goal id names no goal this server knows.
    #[error("goal {goal} is not known to this action server")]
    UnknownGoal {
        /// The goal's UUID, rendered.
        goal: String,
    },

    /// A goal was asked to cancel from a state that cannot cancel.
    #[error("goal {goal} cannot be canceled from {status}")]
    GoalNotCancelable {
        /// The goal's UUID, rendered.
        goal: String,
        /// The status it was in.
        status: &'static str,
    },

    /// A goal transition was attempted after the goal had finished.
    #[error("goal {goal} is already {status}")]
    GoalAlreadyTerminal {
        /// The goal's UUID, rendered.
        goal: String,
        /// The terminal status.
        status: &'static str,
    },

    /// A request or reply arrived without a well-formed correlation header.
    #[error("a sample on `{topic}` carries {len} octets, too few for a {needed}-octet {what}")]
    MalformedRequestHeader {
        /// The topic it arrived on.
        topic: String,
        /// What was being read.
        what: &'static str,
        /// How many octets there were.
        len: usize,
        /// How many were needed.
        needed: usize,
    },

    /// The node has been shut down.
    #[error("this node has been shut down")]
    NodeShutDown,

    /// The RTPS layer failed.
    #[error(transparent)]
    Rtps(#[from] astrs_rtps::BehaviorError),

    /// A payload would not encode or decode.
    #[error(transparent)]
    Cdr(#[from] astrs_cdr::CdrError),

    /// A columnar conversion failed.
    #[error("columnar conversion failed: {0}")]
    Data(String),
}

impl Ros2Error {
    /// True when retrying, or waiting longer, could succeed.
    ///
    /// The protocol group: a timeout, an undiscovered server, a goal the
    /// server has not heard of yet, a malformed header from one peer. A
    /// supervisor retries on this and reports on everything else.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::ServiceUnavailable { .. }
                | Self::UnknownGoal { .. }
                | Self::MalformedRequestHeader { .. }
        )
    }

    /// True when the fault is in a name the program supplied.
    #[must_use]
    pub const fn is_naming(&self) -> bool {
        matches!(
            self,
            Self::InvalidNodeName { .. }
                | Self::InvalidNamespace { .. }
                | Self::InvalidTopicName { .. }
                | Self::InvalidServiceName { .. }
                | Self::NameTooLong { .. }
        )
    }

    /// True when the fault is in how the program configured an entity.
    #[must_use]
    pub const fn is_configuration(&self) -> bool {
        matches!(
            self,
            Self::IncompatibleQos { .. }
                | Self::DuplicateEntity { .. }
                | Self::UnknownParameter { .. }
                | Self::ParameterAlreadyDeclared { .. }
                | Self::ParameterReadOnly { .. }
                | Self::ParameterTypeMismatch { .. }
                | Self::ParameterOutOfRange { .. }
                | Self::ParameterRejected { .. }
        )
    }

    /// True when the node is gone and nothing will work again.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::NodeShutDown)
    }

    /// The name a diagnostic should print for this variant's group.
    #[must_use]
    pub const fn group(&self) -> &'static str {
        if self.is_naming() {
            "naming"
        } else if self.is_configuration() {
            "configuration"
        } else if self.is_terminal() {
            "lifecycle"
        } else if self.is_transient() {
            "protocol"
        } else {
            "delegated"
        }
    }
}

impl From<astrs_data::DataError> for Ros2Error {
    fn from(error: astrs_data::DataError) -> Self {
        Self::Data(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_four_groups_partition_the_enum() {
        let naming = Ros2Error::InvalidNodeName {
            name: "9lives".to_owned(),
            fault: NameFault::TokenStartsWithDigit { offset: 0 },
        };
        let configuration = Ros2Error::UnknownParameter {
            name: "gain".to_owned(),
        };
        let protocol = Ros2Error::Timeout {
            operation: "a service call",
            elapsed_ms: 250,
        };
        let terminal = Ros2Error::NodeShutDown;

        assert_eq!(naming.group(), "naming");
        assert_eq!(configuration.group(), "configuration");
        assert_eq!(protocol.group(), "protocol");
        assert_eq!(terminal.group(), "lifecycle");

        assert!(naming.is_naming() && !naming.is_transient());
        assert!(configuration.is_configuration() && !configuration.is_transient());
        assert!(protocol.is_transient() && !protocol.is_naming());
        assert!(terminal.is_terminal());
    }

    #[test]
    fn a_delegated_error_is_neither_naming_nor_configuration() {
        let error = Ros2Error::Rtps(astrs_rtps::BehaviorError::Shutdown);
        assert_eq!(error.group(), "delegated");
        assert!(!error.is_naming());
        assert!(!error.is_configuration());
        assert!(!error.is_transient());
    }

    #[test]
    fn every_name_fault_renders_something_actionable() {
        let faults = [
            NameFault::Empty,
            NameFault::IllegalCharacter {
                character: '!',
                offset: 3,
            },
            NameFault::TokenStartsWithDigit { offset: 1 },
            NameFault::EmptyToken { offset: 4 },
            NameFault::MisplacedTilde { offset: 2 },
            NameFault::MalformedSubstitution { offset: 0 },
            NameFault::NotAbsolute,
            NameFault::UnexpectedSlash { offset: 5 },
            NameFault::NotFullyQualified,
        ];
        for fault in faults {
            let rendered = fault.to_string();
            assert!(!rendered.is_empty(), "{fault:?} rendered empty");
            assert!(
                rendered.chars().next().is_some_and(char::is_lowercase),
                "{fault:?} should read as a clause: {rendered}"
            );
        }
    }

    #[test]
    fn name_kinds_have_distinct_nouns() {
        let kinds = [
            NameKind::NodeName,
            NameKind::Namespace,
            NameKind::TopicName,
            NameKind::ServiceName,
            NameKind::ActionName,
            NameKind::ParameterName,
        ];
        let mut nouns: Vec<&str> = kinds.iter().map(|kind| kind.noun()).collect();
        nouns.sort_unstable();
        let before = nouns.len();
        nouns.dedup();
        assert_eq!(nouns.len(), before, "two NameKinds share a noun");
        assert_eq!(NameKind::Namespace.to_string(), "node namespace");
    }

    #[test]
    fn a_data_error_converts_without_losing_its_message() {
        let error: Ros2Error = astrs_data::DataError::MessageRowCount { actual: 3 }.into();
        assert!(matches!(error, Ros2Error::Data(_)));
        assert!(error.to_string().contains('3'), "{error}");
    }

    #[test]
    fn a_cdr_error_converts_transparently() {
        let cdr = astrs_cdr::CdrError::Truncated {
            context: "u32",
            needed: 4,
            available: 1,
        };
        let error: Ros2Error = cdr.clone().into();
        assert_eq!(error.to_string(), cdr.to_string());
    }
}
