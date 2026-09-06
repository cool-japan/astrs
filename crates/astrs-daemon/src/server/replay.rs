//! Determinism mode — the recorded clock, wired into the loop (§14).
//!
//! > *`astrs run --deterministic` fixes the timer wheel to the recording's
//! > HLC stream and delivers inputs in recorded order, making single-host
//! > replays reproducible to the message level.* — blueprint §14
//!
//! [`crate::local::ReplaySource`] owns the timeline; this module is the six
//! places the daemon's loop touches it. All of it is a no-op — one [`Option`]
//! test — for the ordinary wall-clock run.
//!
//! ```text
//!   Daemon::tick(wall_now)
//!        │
//!        ├─ advance_replay ─┬─ release the clock once every spawned node registered
//!        │                  ├─ step ONE recorded point   ← virtual now moves here
//!        │                  ├─ inject that point's entry (if the run stood its producer down)
//!        │                  └─ on exhaustion: close those ports, close the timers
//!        ▼
//!   fire_timers(virtual now)   ← ticks land only on recorded points
//! ```
//!
//! # Which recorded entries become messages
//!
//! Every recorded entry is a clock point. An entry is *also* fanned out as a
//! message when its producer is a node this run **stood down** — a
//! `path: dynamic` node, which the daemon never spawns (§8.3) and which
//! `astrs run --deterministic --from-recording` rewrote each recorded
//! producer into. That single test is what keeps the recording from
//! competing with a live producer: if a node is running, its own output wins
//! and the recording is only a clock.
//!
//! Entries produced by the reserved virtual node (a recorded `astrs/timer/*`
//! tick, an `astrs/logs/*` record) are never injected. The wheel produces
//! ticks; replaying recorded ones beside it would deliver every tick twice.
//!
//! # Why the graph has to be up first
//!
//! The wheel's tick grid is anchored on the virtual instant each subscription
//! was armed at, and node registration is a wall-clock race. So virtual time
//! is frozen at the recording's first point until every node the daemon
//! actually spawns has registered — `Daemon::spawned_nodes_registered`.
//! Stood-down nodes are excluded by construction: they have no process, so
//! waiting for them to register would wait forever.
//!
//! # How a deterministic run ends
//!
//! When the last recorded point is consumed, no further tick can ever fire —
//! virtual time has nowhere left to go. Pretending otherwise would hang the
//! graph, so exhaustion is made *visible* through the ordinary closure paths
//! (§12): every stood-down producer is recorded as having finished (which
//! closes its outputs), and every `astrs/timer/*` route is closed. Consumers
//! see `InputClosed`, then `AllInputsClosed`, and a manifest with
//! `exit_when_nodes_finish:` ends on its own.

use std::collections::BTreeSet;
use std::time::Instant;

use astrs_recording::Entry;
use astrs_wire::{DataflowId, NodeExitCause, NodeId, PortRef, RouteCloseReason};

use crate::local::PayloadOrigin;
use crate::server::core::Daemon;
use crate::state::{DataflowState, is_virtual_port};

impl Daemon {
    /// The instant the timer wheel should be advanced to.
    ///
    /// The wall clock for an ordinary run; the recorded point the replay
    /// cursor sits on for a deterministic one. Also what
    /// [`Daemon::admit`](crate::server::core::Daemon::admit) arms every
    /// `astrs/timer/*` subscription on, so the grid and the advance share a
    /// timeline.
    #[must_use]
    pub(crate) fn virtual_now(&self) -> Instant {
        self.replay
            .as_ref()
            .map_or_else(Instant::now, crate::local::ReplaySource::virtual_now)
    }

    /// Steps the recorded timeline by at most one point, returning the
    /// instant the timer wheel should now be advanced to.
    ///
    /// Returns `wall_now` unchanged when the run is not deterministic, which
    /// is the whole of this feature's cost on the ordinary path.
    pub(crate) fn advance_replay(&mut self, wall_now: Instant) -> Instant {
        if self.replay.is_none() {
            return wall_now;
        }
        // Computed before the mutable borrow below, and cheap: a handful of
        // booleans over the node table, once per loop iteration.
        let ready = self.spawned_nodes_registered();

        let (virtual_now, entry, finalize) = {
            let Some(replay) = self.replay.as_mut() else {
                return wall_now;
            };
            if ready {
                replay.release(wall_now);
            }
            let entry = replay.advance(wall_now);
            (replay.virtual_now(), entry, replay.take_finalization())
        };

        if let Some(entry) = entry {
            self.inject_replayed(entry);
        }
        if finalize {
            self.finish_replay();
        }
        virtual_now
    }

    /// Whether every node this daemon actually spawns has registered.
    ///
    /// The release condition for the recorded clock. A node that has already
    /// ended counts as settled — a graph in which one node crashed on startup
    /// must still be able to run out its recording rather than freeze at
    /// point zero — and a stood-down (`path: dynamic`) node is excluded
    /// because no process will ever register for it.
    #[must_use]
    pub(crate) fn spawned_nodes_registered(&self) -> bool {
        if self.state.is_empty() {
            return false;
        }
        self.state.dataflows().all(|state| {
            state.nodes().all(|node| {
                node.is_dynamic() || node.is_terminal() || node.registered_this_incarnation()
            })
        })
    }

    /// Fans one recorded entry out on the port it was recorded from.
    ///
    /// The metadata is the **recorded** one, carried through untouched: the
    /// point of a deterministic replay is that a consumer sees the timestamp,
    /// sequence number and correlation keys the original run carried, not
    /// fresh ones minted now. That is exactly what a node re-publishing a
    /// recording cannot do — `astrs-replay-node` re-stamps, by design — and
    /// exactly why the daemon does this itself.
    fn inject_replayed(&mut self, entry: Entry) {
        let source = PortRef::new(entry.node, entry.output);
        if is_virtual_port(&source) {
            // A recorded tick or log record: a clock point, never a message.
            return;
        }
        let Some(dataflow) = self.replayed_dataflow(&source) else {
            return;
        };
        self.fan_out(
            dataflow,
            &source,
            entry.meta,
            entry.payload,
            PayloadOrigin::Inline,
        );
    }

    /// The dataflow whose stood-down node produces `source`, if any.
    ///
    /// One dataflow, not all of them: `astrs run` — determinism mode's only
    /// entry point (§14 documents cross-host as best-effort, and a
    /// coordinator-driven daemon is never given a clock source) — admits
    /// exactly one graph, so the first match is the only match and the
    /// payload moves instead of being cloned per dataflow.
    fn replayed_dataflow(&self, source: &PortRef) -> Option<DataflowId> {
        self.state
            .dataflows()
            .find(|state| {
                state
                    .node(source.node())
                    .is_some_and(|node| node.is_dynamic() && !node.is_terminal())
            })
            .map(DataflowState::id)
    }

    /// Applies the end of the recording (§12's closure paths).
    ///
    /// Called once, from [`Daemon::advance_replay`], on the step after the
    /// last recorded point is consumed.
    fn finish_replay(&mut self) {
        if let Some(failure) = self.replay.as_ref().and_then(|replay| replay.failure()) {
            tracing::error!(
                failure,
                "the deterministic clock source stopped early; the run ends here"
            );
        }
        let producers: BTreeSet<NodeId> = self
            .replay
            .as_ref()
            .map(|replay| replay.producers().clone())
            .unwrap_or_default();
        let dataflows: Vec<DataflowId> = self.state.dataflows().map(DataflowState::id).collect();

        for dataflow in dataflows {
            for node in &producers {
                self.finish_stood_down_node(dataflow, node);
            }
            self.close_timer_sources(dataflow);
        }
    }

    /// Retires a stood-down producer: its outputs close, then it is recorded
    /// as finished.
    ///
    /// Both halves, in the order a real node does them (§7.3, §12). A live
    /// producer sends `CloseOutputs` on its way out and *then* its process
    /// exits; a stood-down one has no process to send anything, so the
    /// closure is issued here — through the same
    /// [`Daemon::close_source`](crate::server::core::Daemon) the frame's
    /// handler uses, which is what turns "every input of this consumer is
    /// closed" into the `AllInputsClosed` its node API is waiting for.
    /// Then the ordinary post-exit path runs, because the graph's completion
    /// accounting, the result table and the `astrs/status` stream all hang
    /// off that one call.
    ///
    /// The cause is [`NodeExitCause::Success`]: the recording delivered
    /// everything it had. The default `restart_policy: never` then leaves the
    /// node alone.
    fn finish_stood_down_node(&mut self, dataflow: DataflowId, node: &NodeId) {
        let generation = {
            let Some(state) = self.state.dataflow(dataflow) else {
                return;
            };
            let Some(node_state) = state.node(node) else {
                return;
            };
            if !node_state.is_dynamic() || node_state.is_terminal() {
                return;
            }
            node_state.generation()
        };

        let produced: Vec<PortRef> = self
            .state
            .dataflow(dataflow)
            .map(|state| {
                state
                    .routes()
                    .produced_by(node)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for source in &produced {
            self.close_source(dataflow, source, RouteCloseReason::ProducerFinished);
        }

        if let Some(state) = self.state_mut().dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(node)
        {
            node_state.mark_exited(NodeExitCause::Success);
        }
        self.after_exit(dataflow, node, generation, NodeExitCause::Success);
    }

    /// Closes every `astrs/timer/*` route of `dataflow`, and forgets its
    /// subscriptions.
    ///
    /// The truthful signal for "no further tick is possible": virtual time
    /// has run out of recorded points, so a node waiting on a tick would wait
    /// forever. `astrs/logs/*` and `astrs/status` are deliberately left open
    /// — the daemon can still produce those, and a node subscribed to logs
    /// has not lost its producer.
    fn close_timer_sources(&mut self, dataflow: DataflowId) {
        let sources: Vec<PortRef> = match self.state.dataflow(dataflow) {
            Some(state) => state
                .routes()
                .sources()
                .filter(|source| {
                    is_virtual_port(source) && source.port().as_str().starts_with(TIMER_PORT_PREFIX)
                })
                .cloned()
                .collect(),
            None => return,
        };
        for source in &sources {
            self.close_source(dataflow, source, RouteCloseReason::ProducerFinished);
        }
        self.timers.unsubscribe_dataflow(dataflow);
    }
}

/// The prefix every `astrs/timer/*` port id carries once
/// [`crate::state::virtual_port_ref`] has folded the source string's `/`
/// separators into `.` (`astrs/timer/millis/20` → `astrs`/`timer.millis.20`).
const TIMER_PORT_PREFIX: &str = "timer";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::{Path, PathBuf};

    use astrs_manifest::Manifest;
    use astrs_recording::{Writer, WriterOptions};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, Metadata};

    use crate::config::{DaemonConfig, RuntimePaths};
    use crate::dataflow::plan::plan_dataflow;

    use super::*;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// A short-named scratch directory.
    ///
    /// Short on purpose: this suite builds a real [`Daemon`], whose node
    /// socket must fit a `sun_path` (103 bytes on macOS), and a descriptive
    /// directory name under `$TMPDIR` does not leave room for one.
    fn scratch(label: char) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ad-r{}-{}{label}", std::process::id(), uniq()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A recording of `frames` entries on `camera/frames`, 20 ms apart.
    fn recording(dir: &Path, frames: u64) -> PathBuf {
        let path = dir.join("session.arec");
        let mut writer = Writer::create(
            &path,
            WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH),
        )
        .unwrap();
        for index in 0..frames {
            writer
                .append_parts(
                    NodeId::new("camera").unwrap(),
                    DataId::new("frames").unwrap(),
                    Metadata::new(HlcTimestamp::new((index + 1) * 20_000_000, 0)),
                    vec![u8::try_from(index).unwrap_or(u8::MAX)],
                )
                .unwrap();
        }
        writer.finish().unwrap();
        path
    }

    /// A daemon whose clock source is `recording`, holding the stood-down
    /// graph a deterministic run would have rewritten.
    fn daemon_with(dir: &Path, recording: &Path) -> Daemon {
        let paths = RuntimePaths::under(dir).with_socket_name("r.sock");
        let config = DaemonConfig::new(paths)
            .with_working_dir(dir)
            .with_deterministic(true)
            .with_replay_recording(Some(recording.to_path_buf()), None);
        let mut daemon = Daemon::new(config).expect("a daemon");

        let manifest = Manifest::from_yaml_str(
            "exit_when_nodes_finish: true\n\
             nodes:\n\
             \x20 - id: camera\n\
             \x20   path: dynamic\n\
             \x20   outputs: [frames]\n\
             \x20 - id: probe\n\
             \x20   path: /usr/bin/true\n\
             \x20   inputs:\n\
             \x20     frames: camera/frames\n\
             \x20     tick: astrs/timer/millis/20\n",
        )
        .unwrap();
        let plan = plan_dataflow(
            DataflowId::from_u128(7),
            &manifest,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        daemon.admit(&plan).expect("admitted");
        daemon
    }

    #[test]
    fn a_wall_clock_daemon_pays_one_option_test() {
        let dir = scratch('w');
        let paths = RuntimePaths::under(&dir).with_socket_name("w.sock");
        let mut daemon = Daemon::new(DaemonConfig::new(paths).with_working_dir(&dir)).unwrap();
        let before = Instant::now();
        let now = daemon.advance_replay(before);
        assert_eq!(now, before, "the wall path hands its own instant back");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_clock_is_frozen_until_every_spawned_node_has_registered() {
        let dir = scratch('f');
        let session = recording(&dir, 3);
        let mut daemon = daemon_with(&dir, &session);

        // `probe` has not registered, so nothing may move — a stood-down
        // `camera` must not be waited for, which is the other half of the
        // predicate.
        assert!(!daemon.spawned_nodes_registered());
        let virtual_now = daemon.virtual_now();
        assert_eq!(daemon.advance_replay(Instant::now()), virtual_now);
        assert_eq!(
            daemon.advance_replay(Instant::now()),
            virtual_now,
            "still frozen"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_released_clock_steps_one_recorded_point_per_call() {
        let dir = scratch('s');
        let session = recording(&dir, 3);
        let mut daemon = daemon_with(&dir, &session);
        daemon
            .state_mut()
            .dataflow_mut(DataflowId::from_u128(7))
            .unwrap()
            .node_mut(&NodeId::new("probe").unwrap())
            .unwrap()
            .mark_registered();
        assert!(daemon.spawned_nodes_registered());

        let origin = daemon.virtual_now();
        assert_eq!(daemon.advance_replay(Instant::now()), origin);
        assert_eq!(
            daemon.advance_replay(Instant::now()),
            origin + std::time::Duration::from_millis(20)
        );
        assert_eq!(
            daemon.advance_replay(Instant::now()),
            origin + std::time::Duration::from_millis(40)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exhaustion_finishes_the_stood_down_producer_and_closes_the_timers() {
        let dir = scratch('e');
        let session = recording(&dir, 2);
        let mut daemon = daemon_with(&dir, &session);
        let dataflow = DataflowId::from_u128(7);
        daemon
            .state_mut()
            .dataflow_mut(dataflow)
            .unwrap()
            .node_mut(&NodeId::new("probe").unwrap())
            .unwrap()
            .mark_registered();

        assert_eq!(daemon.timers.len(), 1, "the 20 ms wheel entry is armed");
        for _ in 0..8 {
            daemon.advance_replay(Instant::now());
        }

        let camera = NodeId::new("camera").unwrap();
        assert!(
            daemon
                .state()
                .dataflow(dataflow)
                .and_then(|state| state.node(&camera))
                .is_some_and(crate::state::NodeState::is_terminal),
            "the recording stood in for camera, and it has now finished"
        );
        assert!(
            daemon.timers.is_empty(),
            "no further tick is possible, so the subscriptions are gone"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_recording_fails_the_daemon_before_anything_runs() {
        let dir = scratch('m');
        let paths = RuntimePaths::under(&dir).with_socket_name("m.sock");
        let config = DaemonConfig::new(paths)
            .with_working_dir(&dir)
            .with_replay_recording(Some(dir.join("absent.arec")), None);
        let error = Daemon::new(config).unwrap_err();
        assert!(
            matches!(error, crate::error::DaemonError::Configuration(_)),
            "{error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
