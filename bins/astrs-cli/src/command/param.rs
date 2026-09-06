//! `astrs param get/set/list/delete` (blueprint §17's Graph-ops row).
//!
//! ```text
//!   astrs param set <ref> <key> 30     ─► SetParam{scope,key,Integer(30)} ─► Ok
//!   astrs param get <ref> <key>        ─► GetParam{scope,key,inherited}   ─► ParamValue
//!   astrs param list <ref> [--node n]  ─► GetParams{scope,prefix}         ─► ParamList
//!   astrs param delete <ref> <key>     ─► DeleteParam{scope,key}          ─► Ok
//! ```
//!
//! # Three scopes, one positional word
//!
//! [`astrs_wire::ParamScope`] has three levels — global, one dataflow, one
//! node — and the CLI spells all three with the argument a user already
//! types: the dataflow reference, plus an optional `--node`. The literal
//! word `global` in the dataflow position selects
//! [`astrs_wire::ParamScope::Global`], which is how a cluster-wide default
//! is written without inventing a flag for it.
//!
//! # Values are plain JSON, not the wire's tagged form
//!
//! [`astrs_wire::Parameter`] serializes as an externally tagged enum
//! (`{"integer": 30}`), which is the right thing on the wire and the wrong
//! thing to make a human type. `astrs param set flow camera.fps 30` takes
//! plain JSON — `30`, `1.5`, `true`, `"left"`, `[1,2,3]` — and infers the
//! variant, exactly as a user expects. The tagged spelling is still
//! accepted for the one case inference cannot serve: pinning a whole number
//! as a float (`'{"float": 30}'`).

use std::io::Write;
use std::time::Duration;

use astrs_wire::{ControlReply, ControlRequest, NodeId, ParamKey, ParamScope, Parameter};

use crate::command::client::{Client, DataflowRef, Endpoint, reply_name, runtime};
use crate::command::signals::Signals;
use crate::error::CliError;

/// The word that selects [`ParamScope::Global`] in the dataflow position.
pub const GLOBAL_SCOPE_WORD: &str = "global";

/// Where a parameter lives, before the dataflow reference is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeRef {
    /// Cluster-wide defaults.
    Global,
    /// One dataflow's parameters.
    Dataflow(DataflowRef),
    /// One node's parameters.
    Node(DataflowRef, NodeId),
}

impl ScopeRef {
    /// Classifies the `<dataflow> [--node <id>]` pair a user typed.
    ///
    /// # Errors
    ///
    /// [`CliError::BadArgument`] if `--node` is not a usable node id, or if
    /// a node scope was asked for inside the global scope (there is no such
    /// thing: a node belongs to a dataflow).
    pub fn parse(dataflow: &str, node: Option<&str>) -> Result<Self, CliError> {
        let node = match node {
            Some(text) => Some(NodeId::new(text).map_err(|error| CliError::BadArgument {
                flag: "node",
                value: text.to_owned(),
                reason: error.to_string(),
            })?),
            None => None,
        };
        if dataflow.trim().eq_ignore_ascii_case(GLOBAL_SCOPE_WORD) {
            return match node {
                None => Ok(Self::Global),
                Some(node) => Err(CliError::BadArgument {
                    flag: "node",
                    value: node.as_str().to_owned(),
                    reason: "the global scope has no nodes; name a dataflow instead".to_owned(),
                }),
            };
        }
        let reference = DataflowRef::parse(dataflow);
        Ok(match node {
            Some(node) => Self::Node(reference, node),
            None => Self::Dataflow(reference),
        })
    }

    /// Resolves this scope against a connected coordinator.
    ///
    /// # Errors
    ///
    /// As [`Client::resolve`].
    pub async fn resolve(&self, client: &mut Client) -> Result<ParamScope, CliError> {
        Ok(match self {
            Self::Global => ParamScope::Global,
            Self::Dataflow(reference) => ParamScope::Dataflow {
                dataflow: client.resolve(reference).await?,
            },
            Self::Node(reference, node) => ParamScope::Node {
                dataflow: client.resolve(reference).await?,
                node: node.clone(),
            },
        })
    }
}

/// A human name for a resolved scope, for output.
#[must_use]
pub fn scope_name(scope: &ParamScope) -> String {
    match scope {
        ParamScope::Global => GLOBAL_SCOPE_WORD.to_owned(),
        ParamScope::Dataflow { dataflow } => dataflow.to_string(),
        ParamScope::Node { dataflow, node } => format!("{dataflow}/{}", node.as_str()),
        // `ParamScope` is `#[non_exhaustive]`: a scope added at the tail
        // still has to be nameable.
        _ => "an unknown scope".to_owned(),
    }
}

/// Arguments shared by every parameter verb.
#[derive(Debug, Clone)]
pub struct ScopeArgs {
    /// The dataflow (or the word `global`).
    pub dataflow: String,
    /// The node, for a node-scoped parameter.
    pub node: Option<String>,
    /// Emit JSON rather than a human line.
    pub json: bool,
}

/// Reads one parameter.
///
/// Inherited by default: a node that does not set `camera.fps` should see
/// its dataflow's value, and a dataflow that does not set it should see the
/// cluster's — which is the whole point of having three scopes. `--exact`
/// turns the fallback off for the one caller that must know *where* a value
/// lives rather than what it is.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if the key or `--node` is unusable.
/// - [`CliError::UnknownDataflow`] if the reference resolves to nothing.
/// - As [`Client::request`] otherwise.
pub fn get(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ScopeArgs,
    key: &str,
    inherited: bool,
) -> Result<Option<Parameter>, CliError> {
    let scope_ref = ScopeRef::parse(&args.dataflow, args.node.as_deref())?;
    let key = parse_key(key)?;
    let runtime = runtime()?;
    let (value, scope) = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let scope = scope_ref.resolve(&mut client).await?;
        match client
            .request(
                "param get",
                &ControlRequest::GetParam {
                    scope: scope.clone(),
                    key: key.clone(),
                    inherited,
                },
            )
            .await?
        {
            ControlReply::ParamValue { value, scope, .. } => Ok((value, scope)),
            other => Err(CliError::UnexpectedReply {
                request: "param get",
                reply: reply_name(&other),
            }),
        }
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "key": key.as_str(),
                "scope": scope_name(&scope),
                "found": value.is_some(),
                "value": value.as_ref().map(to_plain_json),
                "type": value.as_ref().map(|value| value.type_name()),
            }),
        );
    } else {
        match &value {
            Some(value) => {
                let _ = writeln!(out, "{value}");
            }
            None => {
                let _ = writeln!(out, "{} is not set in {}", key.as_str(), scope_name(&scope));
            }
        }
        let _ = out.flush();
    }
    Ok(value)
}

/// How often `watch` polls [`ControlRequest::GetParam`].
///
/// There is no `ParamSubscribe` verb (§7.3's frozen family has `Log`/`Topic`
/// subscriptions, not a third one for parameters) — see this crate's final
/// report — so `--watch` polls rather than opens a push subscription like
/// `astrs logs -f`/`astrs topic echo` do. Every transition is still seen;
/// what polling gives up is *when within a poll interval* it was written,
/// and a revision that lands and is immediately overwritten between two
/// polls, neither of which this flag's own `--help` overstates.
pub const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Watches one parameter, printing every value it transitions to.
///
/// Prints the value already set (if any) once at the start, then again
/// only when a poll observes a different value than the last one printed
/// — two consecutive polls seeing the same value print nothing, which is
/// what makes this a *change* feed rather than a heartbeat. Runs until
/// `count` distinct values have been observed (the initial read counts as
/// one), or, when `count` is `None`, until interrupted (`Ctrl-C`/`SIGTERM`).
///
/// `count` is what makes this function callable from a test: unbounded,
/// the only way to end it is a signal, which a deadline-polled assertion
/// cannot wait on without racing the harness's own signal handling. A
/// caller that wants the production "stream forever" behaviour passes
/// `None`, exactly as `--watch` does without `--count`.
///
/// # Errors
///
/// As [`get`] for the first read; a transport failure on a later poll ends
/// the watch with that error rather than retrying forever, so a coordinator
/// that goes away is visible rather than silently watched forever.
pub fn watch(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ScopeArgs,
    key: &str,
    inherited: bool,
    count: Option<u32>,
) -> Result<Vec<Option<Parameter>>, CliError> {
    let scope_ref = ScopeRef::parse(&args.dataflow, args.node.as_deref())?;
    let key = parse_key(key)?;
    let runtime = runtime()?;
    runtime.block_on(async move {
        let mut client = Client::connect(endpoint).await?;
        let scope = scope_ref.resolve(&mut client).await?;
        let mut last: Option<Option<Parameter>> = None;
        let mut seen = Vec::new();
        let mut signals = Signals::install();
        loop {
            let value = match client
                .request(
                    "param watch",
                    &ControlRequest::GetParam {
                        scope: scope.clone(),
                        key: key.clone(),
                        inherited,
                    },
                )
                .await?
            {
                ControlReply::ParamValue { value, .. } => value,
                other => {
                    return Err(CliError::UnexpectedReply {
                        request: "param watch",
                        reply: reply_name(&other),
                    });
                }
            };
            if last.as_ref() != Some(&value) {
                print_watch_line(out, &key, &scope, &value, args.json);
                seen.push(value.clone());
                last = Some(value);
                if count.is_some_and(|limit| seen.len() >= limit as usize) {
                    break;
                }
            }
            tokio::select! {
                () = tokio::time::sleep(WATCH_POLL_INTERVAL) => {}
                () = signals.next() => break,
            }
        }
        Ok(seen)
    })
}

/// Prints one `--watch` transition.
fn print_watch_line(
    out: &mut dyn Write,
    key: &ParamKey,
    scope: &ParamScope,
    value: &Option<Parameter>,
    json: bool,
) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({
                "key": key.as_str(),
                "scope": scope_name(scope),
                "value": value.as_ref().map(to_plain_json),
                "type": value.as_ref().map(Parameter::type_name),
            })
        );
    } else {
        match value {
            Some(value) => {
                let _ = writeln!(out, "{} = {value} ({})", key.as_str(), value.type_name());
            }
            None => {
                let _ = writeln!(out, "{} is unset", key.as_str());
            }
        }
    }
    let _ = out.flush();
}

/// Writes one parameter.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if the key, `--node` or the value is
///   unusable.
/// - [`CliError::Refused`] if `--create-only` was given and the key exists.
/// - As [`Client::request`] otherwise.
pub fn set(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ScopeArgs,
    key: &str,
    value: &str,
    create_only: bool,
) -> Result<Parameter, CliError> {
    let scope_ref = ScopeRef::parse(&args.dataflow, args.node.as_deref())?;
    let key = parse_key(key)?;
    let parameter = parse_value(value)?;
    let runtime = runtime()?;
    let scope = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let scope = scope_ref.resolve(&mut client).await?;
        client
            .request_ok(
                "param set",
                &ControlRequest::SetParam {
                    scope: scope.clone(),
                    key: key.clone(),
                    value: parameter.clone(),
                    create_only,
                },
            )
            .await?;
        Ok::<ParamScope, CliError>(scope)
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "key": key.as_str(),
                "scope": scope_name(&scope),
                "value": to_plain_json(&parameter),
                "type": parameter.type_name(),
            }),
        );
    } else {
        let _ = writeln!(
            out,
            "{} = {parameter} ({}) in {}",
            key.as_str(),
            parameter.type_name(),
            scope_name(&scope)
        );
        let _ = out.flush();
    }
    Ok(parameter)
}

/// Lists a scope's parameters.
///
/// # Errors
///
/// As [`get`], plus [`CliError::UnexpectedReply`] for a non-`ParamList`
/// answer.
pub fn list(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ScopeArgs,
    prefix: Option<&str>,
    inherited: bool,
) -> Result<Vec<(ParamKey, Parameter)>, CliError> {
    let scope_ref = ScopeRef::parse(&args.dataflow, args.node.as_deref())?;
    let runtime = runtime()?;
    let (params, scope) = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let scope = scope_ref.resolve(&mut client).await?;
        match client
            .request(
                "param list",
                &ControlRequest::GetParams {
                    scope: scope.clone(),
                    prefix: prefix.map(ToOwned::to_owned),
                    inherited,
                },
            )
            .await?
        {
            ControlReply::ParamList { params, scope } => Ok((params, scope)),
            other => Err(CliError::UnexpectedReply {
                request: "param list",
                reply: reply_name(&other),
            }),
        }
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "scope": scope_name(&scope),
                "count": params.len(),
                "params": params
                    .iter()
                    .map(|(key, value)| serde_json::json!({
                        "key": key.as_str(),
                        "value": to_plain_json(value),
                        "type": value.type_name(),
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
    } else {
        if params.is_empty() {
            let _ = writeln!(out, "no parameters in {}", scope_name(&scope));
        }
        for (key, value) in &params {
            let _ = writeln!(out, "{:<32} {value}", key.as_str());
        }
        let _ = out.flush();
    }
    Ok(params)
}

/// Deletes one parameter.
///
/// # Errors
///
/// As [`get`].
pub fn delete(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &ScopeArgs,
    key: &str,
) -> Result<(), CliError> {
    let scope_ref = ScopeRef::parse(&args.dataflow, args.node.as_deref())?;
    let key = parse_key(key)?;
    let runtime = runtime()?;
    let scope = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let scope = scope_ref.resolve(&mut client).await?;
        client
            .request_ok(
                "param delete",
                &ControlRequest::DeleteParam {
                    scope: scope.clone(),
                    key: key.clone(),
                },
            )
            .await?;
        Ok::<ParamScope, CliError>(scope)
    })?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "key": key.as_str(),
                "scope": scope_name(&scope),
                "deleted": true,
            }),
        );
    } else {
        let _ = writeln!(out, "deleted {} from {}", key.as_str(), scope_name(&scope));
        let _ = out.flush();
    }
    Ok(())
}

/// Validates a parameter key.
///
/// # Errors
///
/// [`CliError::BadArgument`] naming what a key may contain.
fn parse_key(key: &str) -> Result<ParamKey, CliError> {
    ParamKey::new(key).map_err(|error| CliError::BadArgument {
        flag: "key",
        value: key.to_owned(),
        reason: error.to_string(),
    })
}

/// Turns the JSON a user typed into a [`Parameter`].
///
/// # Errors
///
/// [`CliError::BadArgument`] when the text is not JSON at all, is a shape
/// the protocol has no variant for (`null`, a nested object), or is a
/// mixed-type array.
pub fn parse_value(text: &str) -> Result<Parameter, CliError> {
    let refuse = |reason: String| CliError::BadArgument {
        flag: "value",
        value: text.to_owned(),
        reason,
    };
    let value: serde_json::Value = serde_json::from_str(text.trim()).map_err(|error| {
        refuse(format!(
            "expected JSON (a number, `true`, a quoted string, or an array): {error}"
        ))
    })?;
    from_plain_json(&value).ok_or_else(|| {
        // The tagged spelling is the escape hatch for the one thing
        // inference cannot do: `{"float": 30}` for a whole number that must
        // stay a float.
        if value.is_object()
            && let Ok(parameter) = serde_json::from_value::<Parameter>(value.clone())
        {
            return CliError::BadArgument {
                flag: "value",
                value: text.to_owned(),
                reason: format!("(unreachable: {parameter} decoded)"),
            };
        }
        refuse(
            "no parameter type matches that value: use a number, a boolean, a string, a \
             same-typed array, or the tagged form like `{\"float\": 30}`"
                .to_owned(),
        )
    })
}

/// The inference [`parse_value`] documents, without the error wrapping.
fn from_plain_json(value: &serde_json::Value) -> Option<Parameter> {
    match value {
        serde_json::Value::Bool(flag) => Some(Parameter::Bool(*flag)),
        serde_json::Value::Number(number) => number.as_i64().map_or_else(
            || number.as_f64().map(Parameter::Float),
            |integer| Some(Parameter::Integer(integer)),
        ),
        serde_json::Value::String(text) => Some(Parameter::String(text.clone())),
        serde_json::Value::Array(items) => from_array(items),
        // An object may still be the tagged wire form.
        serde_json::Value::Object(_) => serde_json::from_value::<Parameter>(value.clone()).ok(),
        serde_json::Value::Null => None,
    }
}

/// A JSON array as one of the three list parameters, or `None` when its
/// elements do not share a type the protocol has a list for.
///
/// An *empty* array is a `list_string`: some variant has to be chosen, and
/// a list of strings is the one that can later hold anything a user meant.
fn from_array(items: &[serde_json::Value]) -> Option<Parameter> {
    if items.is_empty() {
        return Some(Parameter::ListString(Vec::new()));
    }
    if items.iter().all(|item| item.is_i64()) {
        return Some(Parameter::ListInt(
            items.iter().filter_map(serde_json::Value::as_i64).collect(),
        ));
    }
    if items.iter().all(serde_json::Value::is_number) {
        return Some(Parameter::ListFloat(
            items.iter().filter_map(serde_json::Value::as_f64).collect(),
        ));
    }
    if items.iter().all(serde_json::Value::is_string) {
        return Some(Parameter::ListString(
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect(),
        ));
    }
    None
}

/// A [`Parameter`] as the plain JSON [`parse_value`] accepts, so
/// `astrs param get --json | astrs param set` round-trips.
#[must_use]
pub fn to_plain_json(value: &Parameter) -> serde_json::Value {
    match value {
        Parameter::Bool(flag) => serde_json::json!(flag),
        Parameter::Integer(integer) => serde_json::json!(integer),
        Parameter::Float(float) => serde_json::json!(float),
        Parameter::String(text) => serde_json::json!(text),
        Parameter::ListInt(items) => serde_json::json!(items),
        Parameter::ListFloat(items) => serde_json::json!(items),
        Parameter::ListString(items) => serde_json::json!(items),
        Parameter::Timestamp(stamp) => serde_json::json!(stamp.to_string()),
        // A tail-appended variant (§3) has no plain spelling yet; the
        // tagged form is always correct, so fall back to it rather than
        // guessing.
        other => serde_json::to_value(other).unwrap_or(serde_json::Value::Null),
    }
}

/// Writes one JSON object.
fn emit_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{AuthToken, DataflowId};

    fn endpoint() -> Endpoint {
        Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            AuthToken::ZERO,
        )
    }

    fn args(dataflow: &str, node: Option<&str>) -> ScopeArgs {
        ScopeArgs {
            dataflow: dataflow.to_owned(),
            node: node.map(ToOwned::to_owned),
            json: false,
        }
    }

    #[test]
    fn the_word_global_selects_the_global_scope() {
        assert_eq!(ScopeRef::parse("global", None).unwrap(), ScopeRef::Global);
        assert_eq!(ScopeRef::parse("GLOBAL", None).unwrap(), ScopeRef::Global);
    }

    #[test]
    fn a_node_inside_the_global_scope_is_refused() {
        let error = ScopeRef::parse("global", Some("cam")).unwrap_err();
        match error {
            CliError::BadArgument { flag, reason, .. } => {
                assert_eq!(flag, "node");
                assert!(reason.contains("no nodes"), "{reason}");
            }
            other => panic!("expected BadArgument, got {other}"),
        }
    }

    #[test]
    fn a_dataflow_and_a_node_scope_are_told_apart() {
        assert!(matches!(
            ScopeRef::parse("perception", None).unwrap(),
            ScopeRef::Dataflow(DataflowRef::Name(_))
        ));
        let id = DataflowId::generate();
        assert!(matches!(
            ScopeRef::parse(&id.to_string(), Some("cam")).unwrap(),
            ScopeRef::Node(DataflowRef::Id(_), _)
        ));
    }

    #[test]
    fn an_unusable_node_id_is_refused_before_anything_is_dialled() {
        let error = ScopeRef::parse("flow", Some("not a node")).unwrap_err();
        assert!(matches!(error, CliError::BadArgument { .. }), "{error}");
    }

    #[test]
    fn a_resolved_scope_has_a_readable_name() {
        assert_eq!(scope_name(&ParamScope::Global), "global");
        let id = DataflowId::from_u128(9);
        assert_eq!(
            scope_name(&ParamScope::Dataflow { dataflow: id }),
            id.to_string()
        );
        assert_eq!(
            scope_name(&ParamScope::node(id, NodeId::new("cam").unwrap())),
            format!("{id}/cam")
        );
    }

    #[test]
    fn plain_json_scalars_infer_their_parameter_type() {
        assert_eq!(parse_value("30").unwrap(), Parameter::Integer(30));
        assert_eq!(parse_value("-1").unwrap(), Parameter::Integer(-1));
        assert_eq!(parse_value("1.5").unwrap(), Parameter::Float(1.5));
        assert_eq!(parse_value("true").unwrap(), Parameter::Bool(true));
        assert_eq!(
            parse_value("\"left\"").unwrap(),
            Parameter::String("left".to_owned())
        );
    }

    #[test]
    fn arrays_infer_the_matching_list_type() {
        assert_eq!(
            parse_value("[1,2,3]").unwrap(),
            Parameter::ListInt(vec![1, 2, 3])
        );
        assert_eq!(
            parse_value("[1.5,2]").unwrap(),
            Parameter::ListFloat(vec![1.5, 2.0])
        );
        assert_eq!(
            parse_value("[\"a\",\"b\"]").unwrap(),
            Parameter::ListString(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(
            parse_value("[]").unwrap(),
            Parameter::ListString(Vec::new())
        );
    }

    #[test]
    fn the_tagged_wire_form_is_accepted_for_the_case_inference_cannot_serve() {
        assert_eq!(
            parse_value("{\"float\": 30}").unwrap(),
            Parameter::Float(30.0)
        );
    }

    #[test]
    fn a_value_with_no_parameter_type_is_refused_with_the_text_that_was_typed() {
        for text in ["null", "{\"nested\": {}}", "[1, \"two\"]", "not json"] {
            let error = parse_value(text).unwrap_err();
            match error {
                CliError::BadArgument { flag, value, .. } => {
                    assert_eq!(flag, "value");
                    assert_eq!(value, text);
                }
                other => panic!("expected BadArgument for `{text}`, got {other}"),
            }
        }
    }

    #[test]
    fn every_parameter_round_trips_through_its_plain_json_form() {
        for parameter in [
            Parameter::Bool(false),
            Parameter::Integer(-7),
            Parameter::Float(0.25),
            Parameter::String("hi".to_owned()),
            Parameter::ListInt(vec![1, 2]),
            Parameter::ListFloat(vec![0.5, 1.5]),
            Parameter::ListString(vec!["a".to_owned()]),
        ] {
            let text = to_plain_json(&parameter).to_string();
            assert_eq!(parse_value(&text).unwrap(), parameter, "{text}");
        }
    }

    #[test]
    fn a_key_must_be_a_usable_parameter_key() {
        assert_eq!(parse_key("camera.fps").unwrap().as_str(), "camera.fps");
        let error = parse_key("not a key").unwrap_err();
        match error {
            CliError::BadArgument { flag, .. } => assert_eq!(flag, "key"),
            other => panic!("expected BadArgument, got {other}"),
        }
    }

    #[test]
    fn every_param_verb_reports_a_dead_cluster_rather_than_hanging() {
        let endpoint = endpoint();
        let args = args("flow", None);
        let get_error = get(&mut Vec::new(), &endpoint, &args, "k", true).unwrap_err();
        assert!(
            matches!(get_error, CliError::NoCluster { .. }),
            "{get_error}"
        );

        let set_error = set(&mut Vec::new(), &endpoint, &args, "k", "1", false).unwrap_err();
        assert!(
            matches!(set_error, CliError::NoCluster { .. }),
            "{set_error}"
        );

        let list_error = list(&mut Vec::new(), &endpoint, &args, None, false).unwrap_err();
        assert!(
            matches!(list_error, CliError::NoCluster { .. }),
            "{list_error}"
        );

        let delete_error = delete(&mut Vec::new(), &endpoint, &args, "k").unwrap_err();
        assert!(
            matches!(delete_error, CliError::NoCluster { .. }),
            "{delete_error}"
        );

        let watch_error = watch(&mut Vec::new(), &endpoint, &args, "k", true, None).unwrap_err();
        assert!(
            matches!(watch_error, CliError::NoCluster { .. }),
            "{watch_error}"
        );
    }

    #[test]
    fn a_bad_value_is_refused_before_the_dial() {
        let error = set(
            &mut Vec::new(),
            &endpoint(),
            &args("flow", None),
            "k",
            "null",
            false,
        )
        .unwrap_err();
        assert!(matches!(error, CliError::BadArgument { .. }), "{error}");
    }
}
