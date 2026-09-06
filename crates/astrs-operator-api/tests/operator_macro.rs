//! End-to-end exercise of `#[operator]` (from `astrs-operator-macros`,
//! re-exported here) actually compiling and running.
//!
//! This lives here rather than in `astrs-operator-macros`'s own test suite
//! because the macro's generated code references
//! `astrs_operator_api::{Operator, OperatorConstructor}` — and
//! `astrs-operator-macros` cannot depend on `astrs-operator-api` (this
//! crate already depends on it for the derive, so the reverse would be
//! circular). Here, both crates are legitimately available, so this is
//! where the attribute's output gets to actually compile and run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_operator_api::{
    OpEvent, OpOutput, OpResult, Operator, OperatorRegistry, Status, operator,
};

#[operator]
#[derive(Default)]
struct EchoOperator;

impl Operator for EchoOperator {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input {
                metadata, payload, ..
            } => {
                out.send_bytes("echo", metadata.clone(), payload.clone())?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

#[operator("custom-name")]
#[derive(Default)]
struct NamedOperator;

impl Operator for NamedOperator {
    fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
        Ok(Status::Continue)
    }
}

#[test]
fn default_operator_entry_name_is_snake_case() {
    let (name, _ctor) = EchoOperator::operator_entry();
    assert_eq!(name, "echo_operator");
}

#[test]
fn explicit_operator_entry_name_is_used_verbatim() {
    let (name, _ctor) = NamedOperator::operator_entry();
    assert_eq!(name, "custom-name");
}

#[test]
fn operator_entry_constructs_a_working_operator_through_the_registry() {
    let registry = OperatorRegistry::from_entries([EchoOperator::operator_entry()]).unwrap();
    assert!(registry.contains("echo_operator"));

    let mut echo = registry.build("echo_operator").unwrap();
    let mut out = OpOutput::new();

    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataId, Metadata};
    let event = OpEvent::Input {
        id: DataId::new("in").unwrap(),
        source: "camera/image".parse().unwrap(),
        metadata: Metadata::new(HlcTimestamp::EPOCH),
        payload: vec![9, 8, 7],
    };
    assert_eq!(echo.on_event(&event, &mut out).unwrap(), Status::Continue);
    let sends = out.drain();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].id().as_str(), "echo");
    assert_eq!(sends[0].payload(), &[9, 8, 7]);

    let stop = OpEvent::Stop {
        cause: astrs_wire::StopCause::Requested,
        grace: None,
    };
    assert_eq!(echo.on_event(&stop, &mut out).unwrap(), Status::Finished);
}

#[test]
fn operator_entry_and_register_operator_macro_are_interchangeable() {
    let from_attribute = EchoOperator::operator_entry();
    let from_macro = astrs_operator_api::register_operator!(EchoOperator);

    // Different default names by design (see `astrs-operator-macros`'s
    // `operator_attr` module docs): the attribute's default is
    // `heck`-cased for manifest-style naming, the macro's default is the
    // literal `stringify!`.
    assert_eq!(from_attribute.0, "echo_operator");
    assert_eq!(from_macro.0, "EchoOperator");

    // Both are the same registry-entry shape and both actually work.
    let registry = OperatorRegistry::from_entries([from_attribute, from_macro]).unwrap();
    assert_eq!(registry.len(), 2);
    assert!(registry.build("echo_operator").is_ok());
    assert!(registry.build("EchoOperator").is_ok());
}
