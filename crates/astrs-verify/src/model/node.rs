//! One node of the verification model: how it is activated, how long it
//! takes, and what it is wired into.

use astrs_graph::{EdgeKey, NodeId, PortName};
use astrs_manifest::Pattern;

use crate::scale::{Nanos, Rate};

/// How a node comes to run.
///
/// The distinction is the backbone of every rate argument: a node whose
/// activation is *derived* from declared rates contributes exact numbers
/// to the flow model, while a node that runs on its own schedule
/// contributes an unknown unless a profile pins it down. Recording that
/// difference in the model — rather than silently assuming a rate — is
/// what keeps the report from claiming more than it proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// The node fires once per delivered input event (blueprint §9.1's
    /// merged event loop), so its rate is the sum of its inputs'.
    EventDriven,
    /// The node has no wired inputs and a verification profile declared
    /// how often it emits.
    DeclaredSource(Rate),
    /// The node has no wired inputs and nothing declared its rate. It can
    /// emit at any rate at all, so no rate, boundedness or latency claim
    /// downstream of it is discharged — see
    /// [`crate::Caveat::UndeclaredSourceRate`].
    UndeclaredSource,
}

impl Activation {
    /// Whether this activation leaves the node's rate unknown.
    #[must_use]
    pub fn is_indeterminate(&self) -> bool {
        matches!(self, Self::UndeclaredSource)
    }
}

/// A node's per-firing service time, as an interval on the integer
/// nanosecond scale.
///
/// Both bounds are optional and mean exactly what they say:
///
/// - `min` is a *lower* bound, and only a lower bound proves overload
///   ("this node cannot possibly keep up").
/// - `max` is an *upper* bound, and only an upper bound proves a latency
///   budget ("this node cannot possibly be slower than that").
///
/// A manifest declares neither. That is not a gap this crate papers over
/// with a default: an obligation that needs a bound it does not have
/// reports [`crate::Discharge::NotAttempted`] naming the node, and a
/// verification profile is how the bound is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServiceTime {
    /// A lower bound on one firing's duration.
    pub min: Option<Nanos>,
    /// An upper bound on one firing's duration.
    pub max: Option<Nanos>,
}

impl ServiceTime {
    /// A service time with neither bound known.
    pub const UNKNOWN: Self = Self {
        min: None,
        max: None,
    };

    /// A service time pinned to exactly `nanos`.
    #[must_use]
    pub const fn exact(nanos: Nanos) -> Self {
        Self {
            min: Some(nanos),
            max: Some(nanos),
        }
    }

    /// Whether either bound is known.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    /// Whether the interval is empty (`min > max`), which a validated
    /// profile never produces.
    #[must_use]
    pub fn is_inverted(&self) -> bool {
        matches!((self.min, self.max), (Some(min), Some(max)) if min > max)
    }

    /// How this interval reads in a report.
    #[must_use]
    pub fn render(&self) -> String {
        match (self.min, self.max) {
            (Some(min), Some(max)) if min == max => min.render(),
            (Some(min), Some(max)) => format!("{}..{}", min.render(), max.render()),
            (Some(min), None) => format!(">={}", min.render()),
            (None, Some(max)) => format!("<={}", max.render()),
            (None, None) => "unknown".to_string(),
        }
    }
}

/// A node as the obligations see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The node's id.
    pub id: NodeId,
    /// How it is activated.
    pub activation: Activation,
    /// Its per-firing service time bounds.
    pub service_time: ServiceTime,
    /// Its wired inputs, as channel keys, in graph order.
    pub inputs: Vec<EdgeKey>,
    /// Its declared output ports, in graph order.
    pub outputs: Vec<PortName>,
    /// Every channel this node's outputs feed, in graph order.
    ///
    /// Precomputed because the Petri-net encoding needs a node's
    /// *produced places* per firing, and re-deriving them by scanning
    /// every edge for each transition would make the encoding quadratic
    /// in a graph where it need not be.
    pub produces: Vec<EdgeKey>,
    /// The service/action pattern this node participates in, if any.
    pub pattern: Option<Pattern>,
    /// The exact firing rate derived from declared rates, when the flow
    /// model determines one.
    ///
    /// `None` means indeterminate — the node sits downstream of an
    /// undeclared source, inside a feedback cycle, or behind an aperiodic
    /// virtual source. Obligations that need a rate skip such nodes and
    /// say so.
    pub derived_rate: Option<Rate>,
}

impl Node {
    /// Whether this node has no wired inputs at all.
    #[must_use]
    pub fn is_source(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Whether this node's firing rate is known exactly.
    #[must_use]
    pub fn has_derived_rate(&self) -> bool {
        self.derived_rate.is_some()
    }

    /// Whether this node blocks awaiting a correlated response, i.e. it is
    /// the client half of a service or action pattern (blueprint §9.4).
    ///
    /// This is the only blocking wait an AstRS manifest can express:
    /// input queues never block a producer (both `queue_policy` values
    /// drop rather than stall — blueprint §11.2), so a *wait-for* cycle
    /// can only run through a request/response correlation.
    #[must_use]
    pub fn blocks_on_response(&self) -> bool {
        matches!(
            self.pattern,
            Some(Pattern::ServiceClient | Pattern::ActionClient)
        )
    }

    /// Whether this node answers correlated requests.
    #[must_use]
    pub fn answers_requests(&self) -> bool {
        matches!(
            self.pattern,
            Some(Pattern::ServiceServer | Pattern::ActionServer)
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn node(pattern: Option<Pattern>, inputs: Vec<EdgeKey>) -> Node {
        Node {
            id: NodeId::new("n"),
            activation: Activation::EventDriven,
            service_time: ServiceTime::UNKNOWN,
            inputs,
            outputs: Vec::new(),
            produces: Vec::new(),
            pattern,
            derived_rate: None,
        }
    }

    #[test]
    fn source_nodes_have_no_inputs() {
        assert!(node(None, Vec::new()).is_source());
        let wired = node(
            None,
            vec![EdgeKey::new(NodeId::new("n"), PortName::new("i"))],
        );
        assert!(!wired.is_source());
    }

    #[test]
    fn client_patterns_block_on_a_response() {
        assert!(node(Some(Pattern::ServiceClient), Vec::new()).blocks_on_response());
        assert!(node(Some(Pattern::ActionClient), Vec::new()).blocks_on_response());
        assert!(!node(Some(Pattern::ServiceServer), Vec::new()).blocks_on_response());
        assert!(!node(None, Vec::new()).blocks_on_response());
    }

    #[test]
    fn server_patterns_answer_requests() {
        assert!(node(Some(Pattern::ServiceServer), Vec::new()).answers_requests());
        assert!(node(Some(Pattern::ActionServer), Vec::new()).answers_requests());
        assert!(!node(Some(Pattern::ServiceClient), Vec::new()).answers_requests());
    }

    #[test]
    fn activation_reports_indeterminacy() {
        assert!(Activation::UndeclaredSource.is_indeterminate());
        assert!(!Activation::EventDriven.is_indeterminate());
        assert!(!Activation::DeclaredSource(Rate::new(30, 1).expect("rate")).is_indeterminate());
    }

    #[test]
    fn service_time_renders_every_shape() {
        assert_eq!(ServiceTime::UNKNOWN.render(), "unknown");
        assert_eq!(ServiceTime::exact(Nanos::new(5_000_000)).render(), "5ms");
        assert_eq!(
            ServiceTime {
                min: Some(Nanos::new(1_000_000)),
                max: Some(Nanos::new(4_000_000)),
            }
            .render(),
            "1ms..4ms"
        );
        assert_eq!(
            ServiceTime {
                min: Some(Nanos::new(1_000)),
                max: None,
            }
            .render(),
            ">=1us"
        );
        assert_eq!(
            ServiceTime {
                min: None,
                max: Some(Nanos::new(1_000)),
            }
            .render(),
            "<=1us"
        );
    }

    #[test]
    fn service_time_knows_when_it_knows_nothing() {
        assert!(!ServiceTime::UNKNOWN.is_known());
        assert!(ServiceTime::exact(Nanos::ZERO).is_known());
    }

    #[test]
    fn inverted_intervals_are_detectable() {
        let bad = ServiceTime {
            min: Some(Nanos::new(10)),
            max: Some(Nanos::new(1)),
        };
        assert!(bad.is_inverted());
        assert!(!ServiceTime::exact(Nanos::new(3)).is_inverted());
        assert!(!ServiceTime::UNKNOWN.is_inverted());
    }

    #[test]
    fn derived_rate_presence_is_reported() {
        let mut n = node(None, Vec::new());
        assert!(!n.has_derived_rate());
        n.derived_rate = Rate::new(10, 1);
        assert!(n.has_derived_rate());
    }
}
