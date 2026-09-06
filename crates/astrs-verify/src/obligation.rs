//! Obligation identity: what is being claimed, about what, and which
//! solver answer settles it.

use std::fmt;

/// The four graph obligations of blueprint §15, plus the type-rule
/// consistency check that section lists alongside them.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObligationKind {
    /// No set of nodes can wait on each other forever (blueprint §15(a)).
    DeadlockFreedom,
    /// No queue's backlog outgrows its declared depth under the declared
    /// rates (blueprint §15(b)).
    QueueBoundedness,
    /// Every declared rate expectation is met by the rate the graph
    /// actually delivers (blueprint §15's rate consistency).
    RateConsistency,
    /// Every declared end-to-end latency budget is feasible
    /// (blueprint §15(c)).
    LatencyBudget,
    /// Every typed edge is coercible under the manifest's `type_rules`,
    /// and the rule set itself is consistent (blueprint §15(d)).
    TypeConsistency,
}

impl ObligationKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 5] = [
        Self::DeadlockFreedom,
        Self::QueueBoundedness,
        Self::RateConsistency,
        Self::LatencyBudget,
        Self::TypeConsistency,
    ];

    /// A stable machine-readable slug, used in `--json` output and as the
    /// prefix of every obligation id.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DeadlockFreedom => "deadlock-freedom",
            Self::QueueBoundedness => "queue-boundedness",
            Self::RateConsistency => "rate-consistency",
            Self::LatencyBudget => "latency-budget",
            Self::TypeConsistency => "type-consistency",
        }
    }

    /// A one-line title for a human report.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::DeadlockFreedom => "deadlock freedom",
            Self::QueueBoundedness => "queue boundedness",
            Self::RateConsistency => "rate consistency",
            Self::LatencyBudget => "latency budgets",
            Self::TypeConsistency => "type-rule consistency",
        }
    }

    /// What holding this obligation actually buys, stated so a reader
    /// knows what was and was not proved.
    #[must_use]
    pub const fn claim(self) -> &'static str {
        match self {
            Self::DeadlockFreedom => {
                "no node can be permanently starved, and no service or action correlation can wait in a cycle"
            }
            Self::QueueBoundedness => {
                "no input queue's backlog outgrows its declared depth at the declared rates"
            }
            Self::RateConsistency => {
                "every declared rate expectation is met by the rate the graph delivers"
            }
            Self::LatencyBudget => {
                "every declared latency budget is met by the worst-case queueing delay along the way"
            }
            Self::TypeConsistency => {
                "every typed edge is coercible under the manifest's type rules"
            }
        }
    }
}

impl fmt::Display for ObligationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.title())
    }
}

/// Which solver answer means the obligation is *violated*.
///
/// Recorded per obligation rather than assumed, because a flipped sign is
/// silent: an obligation whose polarity is read backwards reports exactly
/// the opposite of the truth and nothing else changes. Every obligation
/// this crate ships states its polarity here, and every one is covered by
/// a *pair* of tests — a sound graph and a broken one — so a single test
/// cannot pass under a flipped sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    /// The system encodes a *violation*: a satisfying assignment exhibits
    /// the failure, and `unsat` proves the obligation holds. Every
    /// obligation in this crate is written this way.
    SatIsViolation,
    /// The system encodes the *property*: a satisfying assignment is a
    /// witness that the property is achievable, and `unsat` refutes it.
    UnsatIsViolation,
}

impl Polarity {
    /// Whether a satisfying assignment exhibits a violation.
    #[must_use]
    pub const fn sat_is_violation(self) -> bool {
        matches!(self, Self::SatIsViolation)
    }

    /// How this polarity reads in an explanation.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::SatIsViolation => {
                "the encoded system describes a violation: `unsat` proves the obligation, `sat` exhibits a counterexample"
            }
            Self::UnsatIsViolation => {
                "the encoded system describes the property: `sat` witnesses it, `unsat` refutes it"
            }
        }
    }
}

/// One concrete proof obligation: a kind, plus the thing it is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Obligation {
    /// Which of the five obligations this is.
    pub kind: ObligationKind,
    /// A stable id, unique within one report: the kind's slug, then the
    /// subject. Stable across runs, so a CI job can diff two reports.
    pub id: String,
    /// What this obligation is about — a node, a channel, a named path,
    /// or the whole graph.
    pub subject: Subject,
    /// Which solver answer means "violated".
    pub polarity: Polarity,
}

impl Obligation {
    /// Build an obligation about the whole graph.
    #[must_use]
    pub fn whole_graph(kind: ObligationKind) -> Self {
        Self {
            kind,
            id: kind.slug().to_string(),
            subject: Subject::Graph,
            polarity: Polarity::SatIsViolation,
        }
    }

    /// Build an obligation about one named subject.
    #[must_use]
    pub fn about(kind: ObligationKind, subject: Subject) -> Self {
        let id = format!("{}:{}", kind.slug(), subject.key());
        Self {
            kind,
            id,
            subject,
            polarity: Polarity::SatIsViolation,
        }
    }

    /// A one-line heading for this obligation in a human report.
    #[must_use]
    pub fn heading(&self) -> String {
        match &self.subject {
            Subject::Graph => self.kind.title().to_string(),
            other => format!("{} ({})", self.kind.title(), other.render()),
        }
    }
}

/// What an obligation is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum Subject {
    /// The dataflow as a whole.
    Graph,
    /// One node.
    Node(String),
    /// One channel, named by its `consumer.input` edge key.
    Channel(String),
    /// One named latency budget.
    Path(String),
    /// One named facet of the whole graph, where a single obligation kind
    /// splits into several independent claims.
    Aspect(String),
}

impl Subject {
    /// A stable key for building obligation ids.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Graph => "graph".to_string(),
            Self::Node(name) | Self::Channel(name) | Self::Path(name) | Self::Aspect(name) => {
                name.clone()
            }
        }
    }

    /// How this subject reads in a report.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Graph => "whole graph".to_string(),
            Self::Node(name) => format!("node `{name}`"),
            Self::Channel(name) => format!("channel `{name}`"),
            Self::Path(name) => format!("path `{name}`"),
            Self::Aspect(name) => name.clone(),
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn every_kind_has_a_distinct_slug_and_title() {
        let mut slugs: Vec<&str> = ObligationKind::ALL.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), ObligationKind::ALL.len());
        for kind in ObligationKind::ALL {
            assert!(!kind.title().is_empty());
            assert!(!kind.claim().is_empty());
            assert_eq!(kind.to_string(), kind.title());
        }
    }

    #[test]
    fn whole_graph_obligations_use_the_bare_slug() {
        let obligation = Obligation::whole_graph(ObligationKind::DeadlockFreedom);
        assert_eq!(obligation.id, "deadlock-freedom");
        assert_eq!(obligation.subject, Subject::Graph);
        assert_eq!(obligation.heading(), "deadlock freedom");
    }

    #[test]
    fn subject_obligations_get_a_qualified_id() {
        let obligation = Obligation::about(
            ObligationKind::QueueBoundedness,
            Subject::Channel("detector.frames".to_string()),
        );
        assert_eq!(obligation.id, "queue-boundedness:detector.frames");
        assert_eq!(
            obligation.heading(),
            "queue boundedness (channel `detector.frames`)"
        );
    }

    #[test]
    fn ids_are_stable_across_construction() {
        let build = || {
            Obligation::about(
                ObligationKind::LatencyBudget,
                Subject::Path("shutter-to-plan".to_string()),
            )
        };
        assert_eq!(build(), build());
        assert_eq!(build().id, "latency-budget:shutter-to-plan");
    }

    #[test]
    fn subjects_render_and_key_distinctly() {
        assert_eq!(Subject::Graph.key(), "graph");
        assert_eq!(Subject::Node("a".to_string()).render(), "node `a`");
        assert_eq!(
            Subject::Channel("a.b".to_string()).to_string(),
            "channel `a.b`"
        );
        assert_eq!(Subject::Path("p".to_string()).render(), "path `p`");
        assert_eq!(
            Subject::Aspect("starvation".to_string()).render(),
            "starvation"
        );
        assert_eq!(
            Subject::Aspect("starvation".to_string()).key(),
            "starvation"
        );
    }

    #[test]
    fn polarity_is_explicit_in_both_directions() {
        assert!(Polarity::SatIsViolation.sat_is_violation());
        assert!(!Polarity::UnsatIsViolation.sat_is_violation());
        assert_ne!(
            Polarity::SatIsViolation.explanation(),
            Polarity::UnsatIsViolation.explanation()
        );
    }

    #[test]
    fn obligations_default_to_sat_meaning_violation() {
        for kind in ObligationKind::ALL {
            assert_eq!(
                Obligation::whole_graph(kind).polarity,
                Polarity::SatIsViolation,
                "{kind}"
            );
        }
    }

    #[test]
    fn obligations_serialize_stably() {
        let obligation = Obligation::about(
            ObligationKind::RateConsistency,
            Subject::Channel("detector.frames".to_string()),
        );
        let json = serde_json::to_string(&obligation).expect("serializable");
        assert!(json.contains("rate_consistency"), "{json}");
        assert!(json.contains("detector.frames"), "{json}");
        let back: Obligation = serde_json::from_str(&json).expect("round trips");
        assert_eq!(back, obligation);
    }
}
