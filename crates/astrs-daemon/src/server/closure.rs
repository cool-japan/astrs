//! Truthful producer failure: when a closed output becomes an `InputClosed`,
//! and what reason it carries (blueprint §12).
//!
//! ```text
//!   node: output.close()   ─► OutputDone   ─► InputClosed{ProducerFinished}   now
//!
//!   node: drop / shutdown  ─► CloseOutputs ─┐
//!   process exits 23       ─► reap ─────────┴► InputClosed{ProducerCrashed{g}}
//!   process exits 0        ─► reap ─────────┴► InputClosed{ProducerFinished}
//! ```
//!
//! # The defect this module exists for
//!
//! `astrs_node_api::Node`'s `Drop` calls `shutdown`, which sends
//! [`astrs_wire::NodeRequest::CloseOutputs`] and then ends the session. A
//! producer that does that and *then* exits non-zero has crashed — but the
//! frame said only "these outputs are done", and the daemon used to answer it
//! with [`astrs_wire::RouteCloseReason::ProducerFinished`] on the spot. Every
//! consumer therefore learned that a crashed producer had *finished*, and
//! [`astrs_wire::RouteCloseReason::ProducerCrashed`] — which exists, and which
//! §12's error-propagation contract is written in terms of — was never
//! reachable on this path at all.
//!
//! The exit status is the evidence, and at the instant the closure arrives it
//! does not exist yet. So the closure waits for it. That wait is the whole
//! module: `Daemon::defer_closure` opens it, `Daemon::after_exit` closes it
//! with the reason the reap justifies, and `Daemon::fire_closure_deadlines`
//! bounds it for the one case the protocol does not promise — a node that
//! announces its exit and then keeps running. (All three are internal to the
//! loop that owns the state, so they are named rather than linked.)
//!
//! # Why the wait is safe
//!
//! It is the mirror of a wait the daemon already keeps. `Daemon`'s
//! process-exit path holds an exit's *consequences* until the session has been
//! read to the end, because the node's last publishes may still be in the
//! socket ([`crate::server::core::DRAIN_GRACE`]); this holds a closure's
//! *reason* until the exit is known ([`crate::server::core::TEARDOWN_CLOSE_GRACE`]).
//! Both are bounded, both are keyed by generation so a restart cannot inherit
//! one, and both resolve on the ordinary event the daemon was going to receive
//! anyway. In the common case the reap is a scheduling hiccup away: the waiter
//! task is blocked in `wait(2)` and reports the moment the child is gone.
//!
//! # What is *not* deferred
//!
//! Two cases, and they are the reason this is a decision rather than a switch:
//!
//! - [`astrs_wire::NodeRequest::OutputDone`] — one named port, from a node
//!   that goes on running. Nothing about it predicts an exit, and making its
//!   consumers wait for one would stall a live graph.
//! - A node with no daemon-owned process: a `path: dynamic` attach
//!   (`astrs node attach`, `astrs topic pub`). No reap will ever arrive to
//!   contradict the closure, so waiting for one would hold its consumers open
//!   forever.

use std::time::Instant;

use astrs_wire::{DataflowId, NodeId, PortRef};

use crate::server::core::{Daemon, PendingClosure, TEARDOWN_CLOSE_GRACE};

/// Where a closure came from, which decides when its consumers hear about it
/// (blueprint §12).
///
/// The two are different questions, not two spellings of one:
///
/// | Request | Origin | Consumers hear |
/// |---|---|---|
/// | [`astrs_wire::NodeRequest::OutputDone`] | [`ClosureOrigin::Explicit`] | at once, `ProducerFinished` |
/// | [`astrs_wire::NodeRequest::CloseOutputs`] | [`ClosureOrigin::Teardown`] | when the reap says what to call it |
///
/// `CloseOutputs` is the protocol's own "what a node does as it exits" (§7.3;
/// [`astrs_wire::NodeRequest::is_terminal`] says so too), and
/// `astrs_node_api::Node::shutdown` — which `Drop` calls — sends exactly that
/// and then ends the session. A producer whose process is torn down and *then*
/// exits non-zero has crashed, and its consumers must be told
/// `ProducerCrashed`; announcing `ProducerFinished` the moment the frame
/// arrives makes that impossible, because the exit status does not exist yet.
///
/// `OutputDone` says nothing about the process. A node that closes one port
/// and keeps running is finished with that port, now, and its consumers must
/// not be made to wait for an exit that may be hours away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClosureOrigin {
    /// One named output, from a node that goes on running.
    Explicit,
    /// The whole node's outputs, on the way out.
    Teardown,
}

/// What the daemon knows about the producer's process when a closure arrives.
///
/// Two independent facts, because the interesting case sets neither on its
/// own: a process can be reaped *before* the frames it sent just beforehand
/// have been read out of its socket (that is the whole reason
/// `Daemon`'s exit path waits for `SessionClosed`), so "the handle is gone"
/// does not mean "no exit status is coming" — it can mean the opposite.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessLiveness {
    /// A daemon-owned process for this incarnation has not been reaped yet.
    pub(crate) unreaped: bool,
    /// The node has already been marked exited.
    pub(crate) terminal: bool,
}

/// When the consumers of a closed port are told about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClosureRelease {
    /// Immediately, as `ProducerFinished`.
    Now,
    /// When the exit status arrives and names the reason.
    AwaitExit,
    /// Never: the exit has already closed these ports, truthfully.
    AlreadySaid,
}

impl Daemon {
    /// Decides when a closure reaches its consumers (§12).
    ///
    /// # Why the exit can be *behind* the closure and *ahead* of it at once
    ///
    /// A node's last frames and its process exit reach the loop from two
    /// different tasks — the session actor reading the socket, and the waiter
    /// blocked in `wait(2)`. Nothing orders them, so all three of these
    /// happen in practice for the same teardown:
    ///
    /// | Order seen by the loop | State when `CloseOutputs` is applied | Release |
    /// |---|---|---|
    /// | close, then reap | a live handle | `AwaitExit` — the status is coming |
    /// | reap, then close, then session EOF | terminal, exit held for the drain | `AwaitExit` — the status is *already* known and about to be applied |
    /// | close on a node nobody reaps (`path: dynamic`) | no handle, not terminal | `Now` |
    ///
    /// The second row is the one that makes `unreaped` alone the wrong test:
    /// `Daemon`'s process-exit path marks the node exited (dropping the
    /// handle) and then *defers* the consequences until the session has been
    /// read to the end — so a `CloseOutputs` read out of that same socket
    /// arrives with the handle already gone and the true reason already
    /// determined. Answering it with `ProducerFinished` there is exactly the
    /// defect this module exists for; `examples/error-propagation` reproduced
    /// it every run.
    pub(crate) fn release_for(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        origin: ClosureOrigin,
        alive: ProcessLiveness,
    ) -> ClosureRelease {
        if origin == ClosureOrigin::Explicit {
            return ClosureRelease::Now;
        }
        let exit_pending = self
            .draining_exits
            .get(&(dataflow, node.clone()))
            .is_some_and(|pending| pending.generation == generation);
        if exit_pending || alive.unreaped {
            return ClosureRelease::AwaitExit;
        }
        if alive.terminal {
            ClosureRelease::AlreadySaid
        } else {
            ClosureRelease::Now
        }
    }
}

impl Daemon {
    /// Holds a teardown closure until the reap names its reason (§12).
    pub(crate) fn defer_closure(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        sources: Vec<PortRef>,
    ) {
        let key = (dataflow, node.clone());
        // A node may close its outputs in more than one frame on the way out
        // (`OutputDone` per port, then a final `CloseOutputs`); the pending
        // entry accumulates rather than replaces, so the last frame does not
        // strand the ports the earlier ones named.
        let entry = self
            .deferred_closures
            .entry(key.clone())
            .or_insert_with(|| PendingClosure {
                generation,
                sources: Vec::new(),
            });
        if entry.generation == generation {
            entry.sources.extend(sources);
        } else {
            // A restart got in first: the ports belong to an incarnation that
            // no longer exists, and its own `after_exit` has already closed
            // them. Replace rather than merge.
            *entry = PendingClosure {
                generation,
                sources,
            };
        }
        self.closure_deadlines
            .arm_in(key, generation, Instant::now(), TEARDOWN_CLOSE_GRACE);
    }

    /// Forgets a node's deferred teardown closure, returning it.
    ///
    /// Called from `Daemon::after_exit`, which then closes every port the node
    /// produced with the reason the exit status actually justifies — so the
    /// held ports need no separate replay, only to stop being held.
    pub(crate) fn take_deferred_closure(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
    ) -> Option<PendingClosure> {
        let key = (dataflow, node.clone());
        let matches = self
            .deferred_closures
            .get(&key)
            .is_some_and(|pending| pending.generation == generation);
        if !matches {
            return None;
        }
        self.closure_deadlines.disarm_generation(&key, generation);
        self.deferred_closures.remove(&key)
    }

    /// Releases every teardown closure whose reap never arrived (§12).
    ///
    /// The bounded half of the rendezvous: a node that announced its exit and
    /// then kept running is not a crash, so its consumers are told
    /// `ProducerFinished` — the truth as far as it is known. If the process
    /// does eventually exit non-zero, `after_exit` closes the same ports again
    /// with the real reason, exactly as it does for an explicit close.
    pub(crate) fn fire_closure_deadlines(&mut self, now: Instant) {
        for expiry in self.closure_deadlines.expired(now) {
            let key = expiry.key.clone();
            let stale = self
                .deferred_closures
                .get(&key)
                .is_none_or(|pending| pending.generation != expiry.generation);
            if stale {
                continue;
            }
            let (dataflow, node) = key.clone();
            let Some(pending) = self.deferred_closures.remove(&key) else {
                continue;
            };
            tracing::warn!(
                %dataflow,
                %node,
                generation = pending.generation,
                "a node closed its outputs on the way out and is still running; \
                 telling its consumers the producer finished"
            );
            for source in &pending.sources {
                self.close_source(
                    dataflow,
                    source,
                    astrs_wire::RouteCloseReason::ProducerFinished,
                );
            }
            self.propagate_input_closure(dataflow);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use astrs_manifest::Manifest;
    use astrs_wire::{
        DataId, DataflowId, NodeEvent, NodeExitCause, NodeHandshake, NodeId, NodeRequest,
        RouteCloseReason, SessionId,
    };

    use super::*;
    use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
    use crate::dataflow::{DataflowPlan, plan_dataflow};

    /// A producer with one output and one consumer of it. Both `path:`
    /// entries name a real file only so the manifest validates; nothing here
    /// spawns anything — the process is simulated with `mark_spawning`, which
    /// is exactly what installs the `ProcessHandle` whose presence means "a
    /// reap is coming".
    const PIPELINE: &str = "\
nodes:
  - id: camera
    path: /usr/bin/true
    outputs: [image]
  - id: detect
    path: /usr/bin/true
    inputs:
      frames: camera/image
";

    const DATAFLOW: DataflowId = DataflowId::from_u128(1);
    const CAMERA_SESSION: SessionId = SessionId::from_u128(11);
    const DETECT_SESSION: SessionId = SessionId::from_u128(12);

    fn config() -> DaemonConfig {
        let root = std::env::temp_dir().join(format!(
            "astrs-daemon-closure-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none())
    }

    fn plan(yaml: &str) -> DataflowPlan {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        plan_dataflow(DATAFLOW, &manifest, &BTreeMap::new()).expect("valid plan")
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    /// A daemon hosting [`PIPELINE`] with `detect` registered and listening.
    ///
    /// `spawn_camera` decides whether the producer looks like a spawned
    /// process (a `ProcessHandle` the daemon will reap) or a `path: dynamic`
    /// attach that nobody will ever reap — the discriminator the whole module
    /// turns on.
    fn pipeline(spawn_camera: bool) -> Daemon {
        let mut daemon = Daemon::new(config()).unwrap();
        daemon.admit(&plan(PIPELINE)).unwrap();
        if spawn_camera {
            daemon
                .state_mut()
                .dataflow_mut(DATAFLOW)
                .unwrap()
                .node_mut(&node("camera"))
                .unwrap()
                .mark_spawning(4_242);
        }
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::Register(NodeHandshake::new(DATAFLOW, node("camera"), 0)),
        );
        daemon.handle_request(
            DETECT_SESSION,
            NodeRequest::Register(NodeHandshake::new(DATAFLOW, node("detect"), 0)),
        );
        daemon
    }

    /// Every `InputClosed` reason `detect`'s mailbox holds, oldest first,
    /// leaving anything else (its `Registered`) where it was.
    fn closures(daemon: &Daemon) -> Vec<RouteCloseReason> {
        daemon
            .mailboxes
            .get(&(DATAFLOW, node("detect")))
            .map(|mailbox| {
                mailbox
                    .drain(64)
                    .into_iter()
                    .filter_map(|(_, event)| match event {
                        NodeEvent::InputClosed { reason, .. } => Some(reason),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The producer's own exit, as the loop applies it once the reap and the
    /// session drain have both happened.
    fn reap(daemon: &mut Daemon, cause: NodeExitCause) {
        daemon
            .state_mut()
            .dataflow_mut(DATAFLOW)
            .unwrap()
            .node_mut(&node("camera"))
            .unwrap()
            .mark_exited(cause.clone());
        daemon.after_exit(DATAFLOW, &node("camera"), 0, cause);
    }

    #[tokio::test]
    async fn a_teardown_closure_followed_by_a_non_zero_exit_is_a_crash() {
        // The defect, in its original order: `Node::drop` closes the outputs,
        // and only then does the process exit non-zero.
        let mut daemon = pipeline(true);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );
        assert!(
            closures(&daemon).is_empty(),
            "the reason is not known yet, so nothing may be said yet"
        );

        reap(&mut daemon, NodeExitCause::ExitCode { code: 23 });

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerCrashed { generation: 0 }],
            "a producer that closed its outputs and then failed has crashed, once"
        );
    }

    #[tokio::test]
    async fn a_teardown_closure_followed_by_a_clean_exit_is_a_finish() {
        let mut daemon = pipeline(true);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );
        reap(&mut daemon, NodeExitCause::Success);

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerFinished],
            "waiting for the status must not turn an ordinary finish into a crash"
        );
    }

    #[tokio::test]
    async fn an_exit_with_no_closure_frame_at_all_still_reaches_the_consumer() {
        // The other ordering: the process dies without ever getting a
        // `CloseOutputs` out — the `examples/error-propagation` shape. Nothing
        // is deferred, and the exit closes the outputs on its own, as before.
        let mut daemon = pipeline(true);
        reap(&mut daemon, NodeExitCause::ExitCode { code: 23 });

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerCrashed { generation: 0 }],
        );
    }

    #[tokio::test]
    async fn an_explicit_output_close_on_a_live_process_is_not_deferred() {
        // `RawOutput::close()` — one named port, from a node that goes on
        // running. Its consumers must not be made to wait for an exit.
        let mut daemon = pipeline(true);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::OutputDone {
                output: DataId::new("image").unwrap(),
            },
        );

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerFinished],
            "an explicit close on a live process is answered on the spot"
        );
    }

    #[tokio::test]
    async fn a_teardown_closure_from_a_node_with_no_process_is_not_deferred() {
        // `astrs topic pub` attaches a `path: dynamic` node and closes its
        // outputs. Nobody will ever reap it, so a deferral would hold its
        // consumers open forever.
        let mut daemon = pipeline(false);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerFinished],
            "no reap is coming, so there is nothing to wait for"
        );
    }

    #[tokio::test]
    async fn a_deferred_closure_is_released_when_its_grace_runs_out() {
        // The bounded half: a node that announced its exit and then kept
        // running. It has not crashed, so the optimistic answer is the true
        // one as far as it is known.
        let mut daemon = pipeline(true);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );
        assert!(closures(&daemon).is_empty());

        daemon.fire_closure_deadlines(Instant::now() + TEARDOWN_CLOSE_GRACE);

        assert_eq!(
            closures(&daemon),
            vec![RouteCloseReason::ProducerFinished],
            "a process that is still alive has not crashed"
        );
    }

    #[tokio::test]
    async fn a_superseded_incarnation_does_not_close_its_replacement_s_outputs() {
        // §8/§17's `ReplaceNode` cutover: the replacement is spawned *before*
        // the outgoing incarnation is asked to leave, precisely so a
        // reliable-path consumer sees no gap in delivery. `NodeState` holds
        // one incarnation, so during that dual-run window the outgoing
        // session is still bound while the node already describes the
        // incoming one — and the outgoing node's ordinary teardown would
        // otherwise close the ports its replacement had just opened, ending
        // the very consumers the cutover exists to keep running.
        //
        // Observed end to end before this guard: `astrs node replace` on a
        // live graph, and `astrs replay <file> <dataflow>` built on it, both
        // stopped their consumers at the cutover instead of carrying on
        // through it.
        let mut daemon = pipeline(true);

        // The cutover: `camera` advances to the next incarnation while the
        // old session stays bound at the old one.
        let next = daemon
            .state_mut()
            .dataflow_mut(DATAFLOW)
            .unwrap()
            .node_mut(&node("camera"))
            .unwrap()
            .begin_next_generation();
        daemon
            .state_mut()
            .dataflow_mut(DATAFLOW)
            .unwrap()
            .node_mut(&node("camera"))
            .unwrap()
            .mark_spawning(4_243);
        assert_eq!(next, 1, "the replacement is a new incarnation");

        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );

        assert!(
            closures(&daemon).is_empty(),
            "the outgoing incarnation's teardown must not reach the              replacement's consumers"
        );
        assert!(
            daemon
                .take_deferred_closure(DATAFLOW, &node("camera"), next)
                .is_none(),
            "and nothing is left pending against the new incarnation either"
        );
        assert!(
            daemon
                .state()
                .dataflow(DATAFLOW)
                .and_then(|state| state.node(&node("camera")))
                .is_some_and(|state| !state.outputs_done()),
            "the replacement still owns its declared outputs"
        );
    }

    #[tokio::test]
    async fn a_deferred_closure_does_not_outlive_its_incarnation() {
        // Generation discipline, the same rule `PendingExit` keeps: a restart
        // must not inherit the previous incarnation's held closure.
        let mut daemon = pipeline(true);
        daemon.handle_request(
            CAMERA_SESSION,
            NodeRequest::CloseOutputs { outputs: vec![] },
        );
        assert!(
            daemon
                .take_deferred_closure(DATAFLOW, &node("camera"), 1)
                .is_none(),
            "generation 1 has no claim on generation 0's closure"
        );
        assert!(
            daemon
                .take_deferred_closure(DATAFLOW, &node("camera"), 0)
                .is_some(),
            "the incarnation that closed them does"
        );
    }
}
