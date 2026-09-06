//! [`Operator`] and [`Status`] — the trait an in-process dataflow stage
//! implements, and the result it returns (blueprint §9.3).

use std::collections::BTreeMap;

use astrs_wire::Parameter;

use crate::error::OpResult;
use crate::event::OpEvent;
use crate::output::OpOutput;

/// What an [`Operator::on_event`] call decided.
///
/// ```
/// use astrs_operator_api::Status;
///
/// assert_ne!(Status::Continue, Status::Finished);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Status {
    /// Keep running; more events may still arrive.
    Continue,
    /// This operator is done — its host may retire it without treating that
    /// as a failure (distinct from an `Err` return, which the host reports
    /// as `NodeFailed`, blueprint §9.3).
    Finished,
}

impl Status {
    /// Whether the operator should keep receiving events.
    #[must_use]
    pub const fn is_continue(self) -> bool {
        matches!(self, Self::Continue)
    }

    /// Whether the operator is done.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Finished)
    }
}

/// An in-process dataflow stage hosted by `astrs-runtime` (blueprint §9.3).
///
/// ```text
/// pub trait Operator: Default + Send {
///     fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status>;
/// }
/// astrs::register_operator!(MyOp);
/// ```
///
/// is the blueprint's own sketch (quoted verbatim, hence untested as code
/// — it does not compile, which is exactly the point below); this trait
/// realizes it faithfully with one
/// necessary correction. §9.3 also writes `Operator: Default + Send`, but
/// the registry it describes in the same breath —
/// `Box<dyn Fn() -> Box<dyn Operator>>` — requires `Box<dyn Operator>` to be
/// a legal type, and a trait with `Default` as a supertrait is not
/// object-safe (`Default::default()` has no receiver to dispatch on). This
/// trait therefore drops `Default` from its own supertraits and asks for it
/// at the registration site instead: [`register_operator!`](crate::register_operator)
/// requires `$ty: Default` on the type it wraps, so a registered operator
/// is exactly as default-constructible as the blueprint intends, enforced
/// at the one place that actually needs it.
///
/// Operators compile **into** the runtime binary via the registry macro by
/// default — static dispatch, the flagship path. A type that implements
/// this trait can *also* be exported from a `cdylib` via
/// [`crate::export_dylib_operator`] (the `dylib` feature) and loaded at
/// `dlopen` time by `astrs-runtime`'s `dylib-operators` feature — the same
/// trait either way; only how the host obtains a `Box<dyn Operator>`
/// differs. Each operator runs on its own thread over a bounded channel;
/// panics are caught, reported as `NodeFailed`, and honor the operator's
/// restart policy without tearing down siblings (blueprint §9.3) — none of
/// which this trait needs to know about, since it is the *contract* the
/// host schedules against, not the scheduler itself.
///
/// # Lifecycle
///
/// [`Operator::configure`] runs once immediately after construction, before
/// [`Operator::on_start`] and before any event — it is how this operator
/// instance receives its own entry's manifest `config:` map (blueprint
/// §9.3's `operators:` list), the one thing `Default::default()` itself
/// cannot supply. [`Operator::on_start`] runs once after that, before the
/// first [`Operator::on_event`]. [`Operator::on_stop`] runs once, when the
/// host is winding the operator down (typically because
/// [`Operator::on_event`] returned [`Status::Finished`] or an
/// [`OpEvent::Stop`] arrived). [`Operator::on_reload`] runs on
/// [`OpEvent::Reload`] instead of routing it through `on_event`, so an
/// operator that only cares about resetting state does not need a
/// catch-all arm in its own event match. All four default to a no-op, so a
/// minimal operator implements only `on_event`.
pub trait Operator: Send + 'static {
    /// Runs once immediately after construction, before
    /// [`Operator::on_start`].
    ///
    /// `config` is this operator instance's manifest `config:` map
    /// (`astrs_manifest::OperatorConfig::config`, converted from JSON to
    /// this crate's own [`Parameter`] vocabulary by `astrs-runtime` — the
    /// same closed value set [`OpEvent::ParamUpdate`] already carries, so
    /// an operator author reads static construction-time configuration and
    /// live parameter updates through one shared set of types rather than
    /// two). Because the host always builds a fresh operator via
    /// `Default::default()` (blueprint §9.3's static registry), this hook
    /// — not the constructor — is where a `crop`/`nms`-style operator
    /// reads a threshold, a label set, or any other per-instance setting
    /// its wiring alone does not carry.
    ///
    /// Runs again, with the same map, on every restarted incarnation
    /// (blueprint §9.3): a fresh `Default::default()` has no memory of the
    /// previous incarnation's configuration, so re-delivering it here is
    /// what makes a restarted operator behave like the manifest describes
    /// rather than like a blank slate.
    ///
    /// The default does nothing, which is correct for an operator with no
    /// configuration of its own.
    ///
    /// # Errors
    ///
    /// Any [`crate::OpError`] the operator wants to refuse its
    /// configuration with — folded into the same restart/give-up decision
    /// as any other failed hook (blueprint §9.3).
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let _ = config;
        Ok(())
    }

    /// Runs once before the first [`Operator::on_event`] call.
    ///
    /// The default does nothing.
    ///
    /// # Errors
    ///
    /// Any [`crate::OpError`] the operator wants to fail startup with.
    fn on_start(&mut self, out: &mut OpOutput) -> OpResult<()> {
        let _ = out;
        Ok(())
    }

    /// Handles one event, optionally producing output.
    ///
    /// # Errors
    ///
    /// Any [`crate::OpError`] the operator wants to report as a failure
    /// (surfaced as `NodeFailed` by the host, blueprint §9.3).
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status>;

    /// Runs once when the host is winding the operator down.
    ///
    /// The default does nothing. A operator that wants to flush a final
    /// message overrides this instead of trying to catch [`OpEvent::Stop`]
    /// in [`Operator::on_event`] and remembering to still return
    /// [`Status::Finished`].
    ///
    /// # Errors
    ///
    /// Any [`crate::OpError`] the operator wants to report.
    fn on_stop(&mut self, out: &mut OpOutput) -> OpResult<()> {
        let _ = out;
        Ok(())
    }

    /// Runs on [`OpEvent::Reload`] instead of `on_event`.
    ///
    /// The default does nothing, which is the correct behavior for an
    /// operator with no state to reset.
    ///
    /// # Errors
    ///
    /// Any [`crate::OpError`] the operator wants to report.
    fn on_reload(&mut self, out: &mut OpOutput) -> OpResult<()> {
        let _ = out;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn status_predicates() {
        assert!(Status::Continue.is_continue());
        assert!(!Status::Continue.is_finished());
        assert!(Status::Finished.is_finished());
        assert!(!Status::Finished.is_continue());
    }

    /// A toy operator driven through a scripted event sequence — the
    /// integration-style test the task calls for, exercised here at the
    /// trait level (the registry-level version lives in `registry.rs`).
    #[derive(Default)]
    struct Counter {
        seen: u32,
        started: bool,
        stopped: bool,
        reloaded: bool,
    }

    impl Operator for Counter {
        fn on_start(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            self.started = true;
            Ok(())
        }

        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input { metadata, .. } => {
                    self.seen += 1;
                    out.send_bytes("count", metadata.clone(), vec![self.seen as u8])?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }

        fn on_stop(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            self.stopped = true;
            Ok(())
        }

        fn on_reload(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            self.reloaded = true;
            self.seen = 0;
            Ok(())
        }
    }

    fn input_event(payload: Vec<u8>) -> OpEvent {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{DataId, Metadata};

        OpEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::EPOCH),
            payload,
        }
    }

    #[test]
    fn a_toy_operator_runs_through_a_scripted_sequence() {
        let mut op = Counter::default();
        let mut out = OpOutput::new();

        op.on_start(&mut out).unwrap();
        assert!(op.started);

        for i in 0..3u8 {
            let status = op.on_event(&input_event(vec![i]), &mut out).unwrap();
            assert_eq!(status, Status::Continue);
        }
        assert_eq!(op.seen, 3);
        assert_eq!(out.len(), 3);
        let sends = out.drain();
        assert_eq!(sends[2].payload(), &[3]);

        op.on_reload(&mut out).unwrap();
        assert!(op.reloaded);
        assert_eq!(op.seen, 0);

        let stop = OpEvent::Stop {
            cause: astrs_wire::StopCause::Requested,
            grace: None,
        };
        let status = op.on_event(&stop, &mut out).unwrap();
        assert_eq!(status, Status::Finished);

        op.on_stop(&mut out).unwrap();
        assert!(op.stopped);
    }

    #[test]
    fn operator_is_object_safe() {
        // The compile-time check the blueprint's literal `Default + Send`
        // signature fails: this line does not compile unless `Operator`
        // has no non-object-safe supertrait.
        let boxed: Box<dyn Operator> = Box::new(Counter::default());
        drop(boxed);
    }

    #[test]
    fn configure_defaults_to_a_no_op() {
        // `Counter` never overrides `configure`; the default must accept
        // any map, including a non-empty one, without complaint.
        let mut op = Counter::default();
        let mut config = BTreeMap::new();
        config.insert("threshold".to_owned(), Parameter::Float(0.5));
        op.configure(&config).unwrap();
        assert_eq!(op.seen, 0, "the default configure touches no state");
    }

    /// An operator whose behavior is driven entirely by
    /// [`Operator::configure`] — the shape a real `crop`/`nms`-style
    /// manifest `config:` map is meant to reach.
    #[derive(Default)]
    struct Thresholded {
        threshold: i64,
        labels: Vec<String>,
    }

    impl Operator for Thresholded {
        fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
            match config.get("threshold") {
                Some(Parameter::Integer(value)) => self.threshold = *value,
                Some(other) => {
                    return Err(crate::OpError::failed(format!(
                        "threshold must be an integer, got {other:?}"
                    )));
                }
                None => {}
            }
            if let Some(Parameter::ListString(values)) = config.get("labels") {
                self.labels = values.clone();
            }
            Ok(())
        }

        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(Status::Continue)
        }
    }

    #[test]
    fn configure_can_read_a_typed_config_map() {
        let mut op = Thresholded::default();
        let mut config = BTreeMap::new();
        config.insert("threshold".to_owned(), Parameter::Integer(7));
        config.insert(
            "labels".to_owned(),
            Parameter::ListString(vec!["person".to_owned(), "car".to_owned()]),
        );
        op.configure(&config).unwrap();
        assert_eq!(op.threshold, 7);
        assert_eq!(op.labels, vec!["person".to_owned(), "car".to_owned()]);
    }

    #[test]
    fn configure_can_reject_a_bad_value() {
        let mut op = Thresholded::default();
        let mut config = BTreeMap::new();
        config.insert("threshold".to_owned(), Parameter::String("nope".to_owned()));
        let error = op.configure(&config).unwrap_err();
        assert!(matches!(error, crate::OpError::Failed { .. }));
        assert_eq!(op.threshold, 0, "the rejected value must never be applied");
    }
}
