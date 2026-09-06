//! Deadlock freedom (blueprint §15(a)).
//!
//! # What can actually deadlock in AstRS
//!
//! Start from what the manifest can express. `queue_policy` is
//! `drop_oldest` or `backpressure`, and blueprint §11.2 is explicit that
//! **both drop**: backpressure buffers to ten times the declared depth and
//! then drops the newest with an ERROR log. So an AstRS producer is *never*
//! stalled by a full queue, and the textbook "cycle of blocking channels"
//! deadlock is not representable. Encoding it would be dead code dressed
//! as rigour.
//!
//! Two deadlocks are representable, and both are encoded here.
//!
//! ## 1. Starvation — a channel that can never carry a message
//!
//! Model the graph as a Petri net: one **place** per channel, and one
//! **transition per `(node, input)` pair** — not per node — because a node
//! runs a merged event loop and fires on *whichever* input has an event
//! (blueprint §9.1). Getting that wrong flips the verdict on every fan-in
//! graph.
//!
//! Every queue starts empty, so the initial marking is empty everywhere.
//! A **siphon** is a set of places `S` such that every transition
//! producing into `S` also consumes from `S`; the standard Petri-net
//! invariant is that a siphon unmarked at the initial marking stays
//! unmarked in *every* reachable marking. So if some node's inputs all lie
//! in such a siphon, that node can never fire — not "might not", *never*.
//! That is what makes the counterexample a
//! [`crate::Strength::Proof`] rather than a suspicion.
//!
//! Soundness rests on production being modelled **optimistically**: a
//! firing is assumed to emit on every declared output (see
//! [`Model::messages_per_firing`](crate::Model::messages_per_firing)).
//! Assuming more production than really happens can only make more places
//! reachable, so a place this analysis still proves unreachable really is.
//!
//! Timers and the aperiodic virtual sources (`astrs/logs`, `astrs/status`)
//! become transitions with **no** input place. The closure rule then reads
//! `starved[q] ⇒ false` for anything they feed, which is exactly right: a
//! timer-fed channel can never starve, and any siphon containing one is
//! ruled out automatically rather than by a special case.
//!
//! ## 2. Correlated waits — a client that will never be answered
//!
//! A service or action **client** blocks on its correlated response
//! (blueprint §9.4). Unlike an ordinary node it is not saved by having
//! other live inputs: a client whose timer keeps ticking is still wedged
//! if the response never comes. Three ways that happens, all encoded:
//!
//! - the correlation is not fully wired, so there is no response path;
//! - the response channel lies in the starving siphon;
//! - the request channel lies in the siphon, so the server never hears.
//!
//! ## Where queue capacity enters
//!
//! Through §11.2's eviction immunity. Correlated messages are never
//! evicted to make room — dropping one "would wedge a client forever" — so
//! a correlated queue cannot shed load, and its declared depth has to hold
//! everything the correlation puts in flight. An action's goal status
//! walks a multi-state FSM (Accepted → Executing → terminal, §9.4), so a
//! `goal_status` channel shallower than [`ACTION_STATUS_DEPTH`] can be
//! offered a status it has no room for and cannot evict for — and the
//! client waits for a status that was dropped. That is a genuinely
//! capacity-dependent deadlock, and it is the one the manifest can
//! actually express.

use std::collections::{BTreeMap, BTreeSet};

use astrs_graph::{EdgeKey, NodeId, PatternKind};

use crate::constraint::{Assignment, ConstraintSystem, Formula, Term, VarId};
use crate::counterexample::{Counterexample, Strength, TraceStep};
use crate::model::{Channel, Model, Producer};
use crate::obligation::{Obligation, ObligationKind};
use crate::report::{Discharge, Inconclusive, NotAttempted, ObligationOutcome};
use crate::smt::{SolveOutcome, SolverBudget, solve, solve_with_extra};

/// How many correlated status messages an action's response channel must
/// be able to hold.
///
/// The goal FSM of blueprint §9.4 is `Accepted → Executing →
/// {Succeeded, Aborted, Canceled}`: three statuses can be in flight for a
/// single goal before the client has drained any of them. Because
/// correlated messages are eviction-immune (§11.2), a shallower queue has
/// no way to make room, and a dropped status leaves the client waiting for
/// a transition that will never arrive.
pub const ACTION_STATUS_DEPTH: u64 = 3;

/// Discharge the deadlock-freedom obligation.
#[must_use]
pub fn discharge(model: &Model, budget: &SolverBudget) -> ObligationOutcome {
    let obligation = Obligation::whole_graph(ObligationKind::DeadlockFreedom);
    if model.nodes().is_empty() {
        return ObligationOutcome::unattempted(obligation, NotAttempted::NothingToProve);
    }

    let encoding = encode(model);
    let system = &encoding.system;
    let smtlib = system.to_smtlib();

    match solve(system, budget) {
        SolveOutcome::Unsat => ObligationOutcome::decided(
            obligation,
            Discharge::Holds {
                claim: "no channel can be permanently starved, and every service or action correlation can be answered"
                    .to_string(),
            },
            smtlib,
        ),
        SolveOutcome::Sat(assignment) => {
            let minimized = minimize(&encoding, &assignment, budget);
            match interpret(model, &encoding, &minimized) {
                Some(counterexample) => ObligationOutcome::decided(
                    obligation,
                    Discharge::Violated {
                        counterexample: Box::new(counterexample),
                    },
                    smtlib,
                ),
                // The solver satisfied the system, but replaying the model
                // against the violation disjuncts fires none of them — so
                // there is no failure this crate can name. Reporting a
                // violation it cannot explain would be worse than
                // reporting that the two disagreed, which is exactly what
                // `ModelRejected` is for.
                None => ObligationOutcome::decided(
                    obligation,
                    Discharge::Inconclusive {
                        reason: Inconclusive::ModelRejected {
                            detail: "the satisfying assignment fires no deadlock mode".to_string(),
                        },
                    },
                    smtlib,
                ),
            }
        }
        SolveOutcome::Inconclusive(reason) => {
            ObligationOutcome::decided(obligation, Discharge::Inconclusive { reason }, smtlib)
        }
        SolveOutcome::Unavailable => ObligationOutcome::encoded_but_unsolved(
            obligation,
            NotAttempted::SolverUnavailable,
            smtlib,
        ),
    }
}

/// One way the encoded system can be satisfied — i.e. one way the graph
/// can deadlock.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Every one of this node's inputs is starved, so it can never fire.
    NodeStarved(NodeId),
    /// A correlation has no response path at all.
    CorrelationUnwired {
        /// The waiting client.
        client: NodeId,
        /// The server that would answer.
        server: NodeId,
        /// Which correlation kind.
        kind: PatternKind,
    },
    /// A correlation's request or response channel is starved.
    CorrelationStarved {
        /// The waiting client.
        client: NodeId,
        /// The server that would answer.
        server: NodeId,
        /// The channel that never carries a message.
        channel: EdgeKey,
        /// Whether the starved channel is the request leg.
        request_leg: bool,
    },
    /// An action's status channel is too shallow to hold the goal FSM.
    StatusDepth {
        /// The waiting client.
        client: NodeId,
        /// The status channel.
        channel: EdgeKey,
        /// Its declared depth.
        declared: u64,
    },
}

/// The encoded system plus the bookkeeping needed to read a model back.
struct Encoding {
    system: ConstraintSystem,
    /// `starved!<edge>` variable per channel, in edge-key order.
    starved: BTreeMap<EdgeKey, VarId>,
    /// Each violation disjunct, with the formula that decides whether it
    /// is the one a model satisfied.
    modes: Vec<(Mode, Formula)>,
}

/// Build the constraint system.
fn encode(model: &Model) -> Encoding {
    let mut system = ConstraintSystem::new();
    let mut starved = BTreeMap::new();

    for (key, channel) in model.channels() {
        let id = system.bool_var(
            format!("starved!{key}"),
            format!("channel `{}` never carries a message", channel.render()),
        );
        starved.insert(key.clone(), id);
    }

    // Spontaneous producers: a place fed by a transition with no input
    // place can always be marked.
    for (key, channel) in model.channels() {
        if channel.producer.is_spontaneous()
            && let Some(&id) = starved.get(key)
        {
            system.assert(
                "siphon",
                format!(
                    "`{}` is produced by {}, which needs no input",
                    key,
                    channel.producer.render()
                ),
                Formula::bool_var(id).not(),
            );
        }
    }

    // Siphon closure, one rule per produced place: a place can only stay
    // empty if *every* transition producing it is disabled, and a node's
    // transitions are its `(node, input)` pairs.
    for (id, node) in model.nodes() {
        for produced in &node.produces {
            let Some(&place) = starved.get(produced) else {
                continue;
            };
            if node.inputs.is_empty() {
                system.assert(
                    "siphon",
                    format!("`{produced}` is produced by source node `{id}`, which needs no input"),
                    Formula::bool_var(place).not(),
                );
                continue;
            }
            let all_inputs_starved = Formula::all(
                node.inputs
                    .iter()
                    .filter_map(|key| starved.get(key).copied())
                    .map(Formula::bool_var),
            );
            system.assert(
                "siphon",
                format!("`{produced}` can only stay empty while every input of `{id}` does"),
                Formula::bool_var(place).implies(all_inputs_starved),
            );
        }
    }

    let mut modes = Vec::new();

    // Mode 1: a node all of whose inputs are starved.
    for (id, node) in model.nodes() {
        if node.inputs.is_empty() {
            continue;
        }
        let condition = Formula::all(
            node.inputs
                .iter()
                .filter_map(|key| starved.get(key).copied())
                .map(Formula::bool_var),
        );
        modes.push((Mode::NodeStarved(id.clone()), condition));
    }

    // Modes 2-4: correlated waits.
    for pair in model.pattern_pairs() {
        let wired = system.bool_var(
            format!("wired!{}~{}", pair.client, pair.server),
            format!(
                "the {:?} correlation between `{}` and `{}` has both legs wired",
                pair.kind, pair.client, pair.server
            ),
        );
        let fully_wired = pair.is_fully_wired();
        system.assert(
            "correlation",
            format!(
                "`{}`/`{}` {} both correlation legs wired",
                pair.client,
                pair.server,
                if fully_wired { "has" } else { "lacks" }
            ),
            if fully_wired {
                Formula::bool_var(wired)
            } else {
                Formula::bool_var(wired).not()
            },
        );
        modes.push((
            Mode::CorrelationUnwired {
                client: pair.client.clone(),
                server: pair.server.clone(),
                kind: pair.kind,
            },
            Formula::bool_var(wired).not(),
        ));

        for (leg, keys) in [
            (true, &pair.client_to_server),
            (false, &pair.server_to_client),
        ] {
            for key in keys {
                let Some(&place) = starved.get(key) else {
                    continue;
                };
                modes.push((
                    Mode::CorrelationStarved {
                        client: pair.client.clone(),
                        server: pair.server.clone(),
                        channel: key.clone(),
                        request_leg: leg,
                    },
                    Formula::bool_var(place),
                ));
            }
        }

        if pair.kind == PatternKind::Action {
            for key in &pair.server_to_client {
                let Some(channel) = model.channel(key) else {
                    continue;
                };
                let capacity = system.int_var(
                    format!("capacity!{key}"),
                    format!("declared queue depth of `{key}`"),
                );
                system.assert(
                    "capacity",
                    format!("`{key}` declares queue_size {}", channel.declared_capacity),
                    Formula::eq(Term::var(capacity), i128::from(channel.declared_capacity)),
                );
                modes.push((
                    Mode::StatusDepth {
                        client: pair.client.clone(),
                        channel: key.clone(),
                        declared: channel.declared_capacity,
                    },
                    Formula::lt(Term::var(capacity), i128::from(ACTION_STATUS_DEPTH)),
                ));
            }
        }
    }

    system.assert(
        "violation",
        "some node is permanently blocked",
        Formula::any(modes.iter().map(|(_, formula)| formula.clone())),
    );

    Encoding {
        system,
        starved,
        modes,
    }
}

/// Shrink a satisfying assignment to a locally minimal starving set.
///
/// A solver is free to mark far more channels starved than the argument
/// needs, and a counterexample listing every channel in the graph teaches
/// nobody anything. Channels are excluded one at a time in edge-key order
/// — a deterministic sweep — and an exclusion is kept whenever the system
/// stays satisfiable without it.
fn minimize(encoding: &Encoding, assignment: &Assignment, budget: &SolverBudget) -> Assignment {
    let mut excluded: Vec<Formula> = Vec::new();
    let mut best = assignment.clone();
    for &id in encoding.starved.values() {
        if best.get_bool(id) != Some(true) {
            continue;
        }
        let mut candidate = excluded.clone();
        candidate.push(Formula::bool_var(id).not());
        if let SolveOutcome::Sat(narrowed) = solve_with_extra(&encoding.system, &candidate, budget)
        {
            excluded = candidate;
            best = *narrowed;
        }
    }
    best
}

/// Turn a satisfying assignment into a counterexample a reader can act on.
///
/// Returns `None` when the assignment satisfies the system without firing
/// any violation disjunct. That should be unreachable — the system asserts
/// the disjunction — and treating it as a corroboration failure rather
/// than as a nameless violation is what keeps an encoding bug from
/// printing as a graph bug.
fn interpret(
    model: &Model,
    encoding: &Encoding,
    assignment: &Assignment,
) -> Option<Counterexample> {
    let starving: Vec<&EdgeKey> = encoding
        .starved
        .iter()
        .filter(|&(_, id)| assignment.get_bool(*id) == Some(true))
        .map(|(key, _)| key)
        .collect();

    let fired: Vec<&Mode> = encoding
        .modes
        .iter()
        .filter(|(_, formula)| formula.evaluate(&|id| assignment.get(id)) == Some(true))
        .map(|(mode, _)| mode)
        .collect();

    let headline = headline_for(fired.first().copied()?);

    let mut counterexample =
        Counterexample::from_assignment(headline, Strength::Proof, &encoding.system, assignment);

    if !starving.is_empty() {
        counterexample = counterexample.with_fact(
            "channels that can never carry a message",
            starving
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    // The narrative comes from the mode the headline names; the remaining
    // starving channels are appended only where they add something the
    // narrative did not already say, so a small cycle does not read as the
    // same two facts stated four times.
    if let Some(mode) = fired.first() {
        counterexample = counterexample.with_steps(steps_for(model, mode));
    }
    let already: BTreeSet<String> = counterexample
        .trace
        .iter()
        .map(|step| step.actor.clone())
        .collect();
    for key in &starving {
        if already.contains(&key.to_string()) {
            continue;
        }
        if let Some(channel) = model.channel(key) {
            counterexample = counterexample.with_step(key.to_string(), starved_detail(channel));
        }
    }

    if let Some(remedy) = fired.first().map(|mode| remedy_for(mode)) {
        counterexample = counterexample.with_remedy(remedy);
    }
    Some(counterexample)
}

fn headline_for(mode: &Mode) -> String {
    match mode {
        Mode::NodeStarved(node) => format!("node `{node}` can never fire"),
        Mode::CorrelationUnwired {
            client,
            server,
            kind,
        } => format!(
            "`{client}` waits forever: its {} correlation with `{server}` has no response path",
            kind_label(*kind)
        ),
        Mode::CorrelationStarved {
            client,
            channel,
            request_leg,
            ..
        } => {
            let leg = if *request_leg { "request" } else { "response" };
            format!(
                "`{client}` waits forever: the {leg} channel `{channel}` can never carry a message"
            )
        }
        Mode::StatusDepth {
            client,
            channel,
            declared,
        } => format!(
            "`{client}` can miss a goal status: `{channel}` declares queue_size {declared}, below the {ACTION_STATUS_DEPTH} an action's status FSM needs"
        ),
    }
}

fn steps_for(model: &Model, mode: &Mode) -> Vec<TraceStep> {
    match mode {
        Mode::NodeStarved(node) => {
            let mut steps = vec![TraceStep::new(
                node.to_string(),
                "every wired input is starved, so its event loop never receives an event"
                    .to_string(),
            )];
            for channel in model.inputs_of(node) {
                steps.push(TraceStep::new(
                    channel.key.to_string(),
                    format!("fed by {}, which never produces", channel.producer.render()),
                ));
            }
            steps
        }
        Mode::CorrelationUnwired {
            client,
            server,
            kind,
        } => vec![
            TraceStep::new(
                client.to_string(),
                format!(
                    "issues a {} request and blocks on the correlated reply",
                    kind_label(*kind)
                ),
            ),
            TraceStep::new(
                server.to_string(),
                "has no wired path back to the client, so the reply is never delivered".to_string(),
            ),
        ],
        Mode::CorrelationStarved {
            client,
            server,
            channel,
            request_leg,
        } => vec![
            TraceStep::new(
                client.to_string(),
                "blocks on a correlated reply rather than returning to its event loop".to_string(),
            ),
            TraceStep::new(
                channel.to_string(),
                if *request_leg {
                    format!("the request never reaches `{server}`")
                } else {
                    format!("`{server}`'s reply never reaches `{client}`")
                },
            ),
        ],
        Mode::StatusDepth {
            client,
            channel,
            declared,
        } => vec![
            TraceStep::new(
                channel.to_string(),
                format!(
                    "holds {declared} message(s); goal status walks Accepted -> Executing -> terminal"
                ),
            ),
            TraceStep::new(
                channel.to_string(),
                "correlated messages are eviction-immune (blueprint §11.2), so a full queue cannot make room and the surplus status is dropped".to_string(),
            ),
            TraceStep::new(
                client.to_string(),
                "waits for a status transition that was never delivered".to_string(),
            ),
        ],
    }
}

fn remedy_for(mode: &Mode) -> String {
    match mode {
        Mode::NodeStarved(node) => format!(
            "give `{node}` a trigger that does not depend on it — an `astrs/timer/*` input, or a producer outside the cycle"
        ),
        Mode::CorrelationUnwired { client, server, .. } => {
            format!("wire `{server}`'s reply output back into an input of `{client}`")
        }
        Mode::CorrelationStarved { .. } => {
            "break the starvation on the correlation's channel, or drop the `pattern:` declaration if the exchange is not really request/response".to_string()
        }
        Mode::StatusDepth { channel, .. } => format!(
            "raise `queue_size` on `{channel}` to at least {ACTION_STATUS_DEPTH}"
        ),
    }
}

fn starved_detail(channel: &Channel) -> String {
    match &channel.producer {
        Producer::Node { node, output } => {
            format!("fed by `{node}`'s `{output}`, which can only fire on starved inputs")
        }
        Producer::Timer { source, rate } => format!("fed by {source} at {rate}"),
        Producer::Aperiodic { source } => format!("fed by {source}"),
    }
}

fn kind_label(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::Service => "service",
        PatternKind::Action => "action",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::model_of;

    #[test]
    fn a_trigger_free_cycle_encodes_a_reachable_violation() {
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { from_b: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { from_a: a/out }
    outputs: [out]
",
        );
        let encoding = encode(&model);
        let text = encoding.system.to_smtlib();
        assert!(
            text.contains("(declare-const starved!a.from_b Bool)"),
            "{text}"
        );
        assert!(text.contains("; --- violation ---"), "{text}");
        // Both channels can be starved together, and each node's whole
        // input set is then starved.
        let mut assignment = Assignment::new();
        for &id in encoding.starved.values() {
            assignment.set_bool(id, true);
        }
        assignment.complete_with(encoding.system.registry(), 0);
        assert!(encoding.system.evaluate(&assignment).is_satisfied());
    }

    #[test]
    fn a_timer_fed_cycle_cannot_starve() {
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/10, from_b: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { from_a: a/out }
    outputs: [out]
",
        );
        let encoding = encode(&model);
        // The timer channel is asserted never-starved, which forces the
        // whole siphon empty.
        let mut assignment = Assignment::new();
        for &id in encoding.starved.values() {
            assignment.set_bool(id, true);
        }
        assignment.complete_with(encoding.system.registry(), 0);
        assert!(
            !encoding.system.evaluate(&assignment).is_satisfied(),
            "a timer-fed channel must not be allowed into the siphon"
        );
    }

    #[test]
    fn source_nodes_cannot_feed_a_siphon() {
        let model = model_of(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs: { raw: sensor/raw }
",
        );
        let encoding = encode(&model);
        let text = encoding.system.to_smtlib();
        assert!(
            text.contains("source node `sensor`"),
            "a node with no inputs produces unconditionally: {text}"
        );
    }

    #[test]
    fn fan_in_uses_one_transition_per_input() {
        // `sink` has two inputs; only starving *both* may block it.
        let model = model_of(
            "
nodes:
  - id: timed
    path: ./timed
    inputs: { tick: astrs/timer/hz/1 }
    outputs: [out]
  - id: looped
    path: ./looped
    inputs: { back: sink/out }
    outputs: [out]
  - id: sink
    path: ./sink
    inputs: { a: timed/out, b: looped/out }
    outputs: [out]
",
        );
        let encoding = encode(&model);
        let text = encoding.system.to_smtlib();
        assert!(
            text.contains("can only stay empty while every input of `sink` does"),
            "{text}"
        );
    }

    #[test]
    fn action_status_depth_is_encoded_as_a_capacity_comparison() {
        let model = model_of(
            "
nodes:
  - id: client
    path: ./client
    pattern: action-client
    inputs:
      tick: astrs/timer/hz/1
      status:
        source: server/status
        queue_size: 1
    outputs: [goal]
  - id: server
    path: ./server
    pattern: action-server
    inputs: { goal: client/goal }
    outputs: [status]
",
        );
        let encoding = encode(&model);
        let text = encoding.system.to_smtlib();
        assert!(
            text.contains("(declare-const capacity!client.status Int)"),
            "{text}"
        );
        assert!(text.contains("declares queue_size 1"), "{text}");
        assert!(
            encoding
                .modes
                .iter()
                .any(|(mode, _)| matches!(mode, Mode::StatusDepth { .. })),
            "the shallow status queue must be a violation mode"
        );
    }

    #[test]
    fn a_deep_enough_action_status_queue_is_not_a_capacity_violation() {
        let model = model_of(
            "
nodes:
  - id: client
    path: ./client
    pattern: action-client
    inputs:
      tick: astrs/timer/hz/1
      status:
        source: server/status
        queue_size: 8
    outputs: [goal]
  - id: server
    path: ./server
    pattern: action-server
    inputs: { goal: client/goal }
    outputs: [status]
",
        );
        let encoding = encode(&model);
        let mut assignment = Assignment::new();
        assignment.complete_with(encoding.system.registry(), 0);
        let capacity = encoding
            .system
            .registry()
            .id_of("capacity!client.status")
            .expect("declared");
        assignment.set(capacity, 8);
        for (mode, formula) in &encoding.modes {
            if matches!(mode, Mode::StatusDepth { .. }) {
                assert_eq!(
                    formula.evaluate(&|id| assignment.get(id)),
                    Some(false),
                    "queue_size 8 must clear the status-depth floor"
                );
            }
        }
    }

    #[test]
    fn empty_graphs_have_nothing_to_prove() {
        let model = model_of("nodes: []\n");
        let outcome = discharge(&model, &SolverBudget::default());
        assert!(matches!(
            outcome.discharge,
            Discharge::NotAttempted {
                reason: NotAttempted::NothingToProve
            }
        ));
    }

    #[test]
    fn encoding_is_deterministic() {
        let yaml = "
nodes:
  - id: a
    path: ./a
    inputs: { from_b: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { from_a: a/out }
    outputs: [out]
";
        let first = encode(&model_of(yaml)).system.to_smtlib();
        let second = encode(&model_of(yaml)).system.to_smtlib();
        assert_eq!(first, second);
    }

    #[test]
    fn an_assignment_that_fires_no_mode_is_not_a_counterexample() {
        let model = model_of(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/10 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        );
        let encoding = encode(&model);
        // Nothing starved: the siphon closure is satisfied, but no
        // violation disjunct holds.
        let mut assignment = Assignment::new();
        assignment.complete_with(encoding.system.registry(), 0);
        assert!(
            interpret(&model, &encoding, &assignment).is_none(),
            "a model that names no failure must not become a counterexample"
        );
    }

    #[test]
    fn every_input_of_every_node_has_a_siphon_variable() {
        // The violation disjunct for a node is a conjunction over its
        // inputs; an input with no variable would silently drop out and
        // turn the conjunction into the constant `true`.
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/1, back: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { fwd: a/out }
    outputs: [out]
",
        );
        let encoding = encode(&model);
        for node in model.nodes().values() {
            for key in &node.inputs {
                assert!(
                    encoding.starved.contains_key(key),
                    "`{key}` has no siphon variable"
                );
            }
        }
    }

    #[test]
    fn mode_headlines_and_remedies_are_populated() {
        let modes = [
            Mode::NodeStarved(NodeId::new("a")),
            Mode::CorrelationUnwired {
                client: NodeId::new("c"),
                server: NodeId::new("s"),
                kind: PatternKind::Service,
            },
            Mode::CorrelationStarved {
                client: NodeId::new("c"),
                server: NodeId::new("s"),
                channel: EdgeKey::new(NodeId::new("s"), astrs_graph::PortName::new("req")),
                request_leg: true,
            },
            Mode::StatusDepth {
                client: NodeId::new("c"),
                channel: EdgeKey::new(NodeId::new("c"), astrs_graph::PortName::new("status")),
                declared: 1,
            },
        ];
        for mode in &modes {
            assert!(!headline_for(mode).is_empty());
            assert!(!remedy_for(mode).is_empty());
        }
        assert_eq!(kind_label(PatternKind::Service), "service");
        assert_eq!(kind_label(PatternKind::Action), "action");
    }
}
