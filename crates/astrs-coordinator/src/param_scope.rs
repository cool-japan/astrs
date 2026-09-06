//! Mapping [`ParamScope`] onto `astrs-store`'s buckets, and [`Parameter`]
//! onto the store's JSON value.
//!
//! `astrs-store` persists two levels: dataflow-scoped
//! (`(DataflowId, ParamKey)`, [`astrs_store::CoordinatorStore::set_param`])
//! and node-scoped (`(DataflowId, NodeId, ParamKey)`,
//! [`astrs_store::CoordinatorStore::set_node_param`] — added additively by
//! this wave, see that crate's `keys` module). [`ParamScope`] has *three*
//! levels (`Global | Dataflow | Node`, blueprint §17): the coordinator's own
//! convention, entirely local to this crate, is that
//! [`ParamScope::Global`] lives in the dataflow-scoped bucket under the
//! reserved sentinel [`GLOBAL_SCOPE_DATAFLOW`] — [`astrs_wire::DataflowId::NIL`].
//! No dataflow a coordinator ever assigns is ever `NIL` (every real
//! [`astrs_wire::DataflowId`] is minted by
//! [`astrs_wire::DataflowId::generate`], a UUIDv7, and the all-zero UUID is
//! not one of those), so the sentinel can never collide with a real
//! dataflow's own parameters.

use astrs_store::CoordinatorStore;
use astrs_store::record::{MutationSeq, ParamRecord};
use astrs_wire::{DataflowId, NodeId, ParamKey, ParamScope, Parameter};
use oxistore_core::KvStore;

use crate::error::{CoordinatorError, Result};

/// The sentinel dataflow id [`ParamScope::Global`] parameters are stored
/// under in the dataflow-scoped params bucket.
pub const GLOBAL_SCOPE_DATAFLOW: DataflowId = DataflowId::NIL;

/// Serializes a [`Parameter`] to the [`serde_json::Value`]
/// `astrs-store` persists.
///
/// [`Parameter`] already derives `serde::Serialize`/`Deserialize`
/// (blueprint §6.1's closed value set), so this is a direct `serde_json`
/// round trip rather than a hand-written case analysis — one caller, one
/// conversion, nothing for the two to drift apart on.
///
/// # Errors
///
/// Never fails for a well-formed [`Parameter`]: every variant serializes to
/// a finite JSON value. Kept fallible because `serde_json::to_value` is.
pub fn parameter_to_json(value: &Parameter) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|source| {
        CoordinatorError::invalid(format!("parameter did not serialize: {source}"))
    })
}

/// The inverse of [`parameter_to_json`].
///
/// # Errors
///
/// [`CoordinatorError::invalid`] if `value` is not a JSON encoding of a
/// [`Parameter`] — unreachable for a value this crate wrote itself, but
/// reachable if a store file was edited by hand or written by a foreign
/// process.
pub fn json_to_parameter(value: serde_json::Value) -> Result<Parameter> {
    serde_json::from_value(value).map_err(|source| {
        CoordinatorError::invalid(format!("stored value is not a parameter: {source}"))
    })
}

/// Writes `value` at `scope`/`key`, in whichever bucket the scope maps to.
///
/// # Errors
///
/// As [`astrs_store::CoordinatorStore::set_param`] /
/// [`astrs_store::CoordinatorStore::set_node_param`].
pub fn set_param<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
    key: ParamKey,
    value: serde_json::Value,
) -> Result<MutationSeq> {
    match scope {
        ParamScope::Global => Ok(store.set_param(GLOBAL_SCOPE_DATAFLOW, key, value)?),
        ParamScope::Dataflow { dataflow } => Ok(store.set_param(*dataflow, key, value)?),
        ParamScope::Node { dataflow, node } => {
            Ok(store.set_node_param(*dataflow, node.clone(), key, value)?)
        }
        // `ParamScope` is `#[non_exhaustive]` (blueprint principle 4:
        // append-only wire enums); a variant added in a later protocol
        // revision has no bucket mapping decided for it yet.
        _ => Err(unknown_scope(scope)),
    }
}

/// Reads the record at `scope`/`key`, if any, from whichever bucket the
/// scope maps to.
///
/// # Errors
///
/// As [`astrs_store::CoordinatorStore::get_param`] /
/// [`astrs_store::CoordinatorStore::get_node_param`].
pub fn get_param<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
    key: &ParamKey,
) -> Result<Option<ParamRecord>> {
    match scope {
        ParamScope::Global => Ok(store.get_param(GLOBAL_SCOPE_DATAFLOW, key)?),
        ParamScope::Dataflow { dataflow } => Ok(store.get_param(*dataflow, key)?),
        ParamScope::Node { dataflow, node } => Ok(store.get_node_param(*dataflow, node, key)?),
        _ => Err(unknown_scope(scope)),
    }
}

/// Deletes the record at `scope`/`key`, if any.
///
/// # Errors
///
/// As [`astrs_store::CoordinatorStore::delete_param`] /
/// [`astrs_store::CoordinatorStore::delete_node_param`].
pub fn delete_param<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
    key: &ParamKey,
) -> Result<Option<MutationSeq>> {
    match scope {
        ParamScope::Global => Ok(store.delete_param(GLOBAL_SCOPE_DATAFLOW, key)?),
        ParamScope::Dataflow { dataflow } => Ok(store.delete_param(*dataflow, key)?),
        ParamScope::Node { dataflow, node } => Ok(store.delete_node_param(*dataflow, node, key)?),
        _ => Err(unknown_scope(scope)),
    }
}

/// Lists every parameter set directly at `scope` (never a parent scope —
/// see [`lookup_with_inheritance`] for that).
///
/// # Errors
///
/// As [`astrs_store::CoordinatorStore::list_params`] /
/// [`astrs_store::CoordinatorStore::list_node_params`].
pub fn list_params<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
) -> Result<Vec<(ParamKey, ParamRecord)>> {
    match scope {
        ParamScope::Global => Ok(store.list_params(GLOBAL_SCOPE_DATAFLOW)?),
        ParamScope::Dataflow { dataflow } => Ok(store.list_params(*dataflow)?),
        ParamScope::Node { dataflow, node } => Ok(store.list_node_params(*dataflow, node)?),
        _ => Err(unknown_scope(scope)),
    }
}

/// The error a [`ParamScope`] variant this build does not recognise
/// produces.
fn unknown_scope(scope: &ParamScope) -> CoordinatorError {
    CoordinatorError::invalid(format!(
        "unrecognised parameter scope {}: {scope}",
        scope.kind_name()
    ))
}

/// Looks `key` up at `scope`, falling back through
/// [`ParamScope::lookup_chain`] (node → dataflow → global) when `inherited`
/// is set and the more specific scope has no value.
///
/// Returns the record together with the scope it was actually found in, so
/// a caller (`ControlReply::ParamValue::scope`) can tell a caller whether an
/// answer was inherited.
///
/// # Errors
///
/// As [`get_param`].
pub fn lookup_with_inheritance<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
    key: &ParamKey,
    inherited: bool,
) -> Result<Option<(ParamScope, ParamRecord)>> {
    let chain = if inherited {
        scope.lookup_chain()
    } else {
        vec![scope.clone()]
    };
    for candidate in chain {
        if let Some(record) = get_param(store, &candidate, key)? {
            return Ok(Some((candidate, record)));
        }
    }
    Ok(None)
}

/// Lists every parameter visible at `scope`, optionally including values
/// inherited from parent scopes that are not shadowed by a more specific
/// one, optionally restricted to keys starting with `prefix`.
///
/// A key present at more than one scope in the chain shows only its
/// most-specific value — the same shadowing [`lookup_with_inheritance`]
/// implements for a single key.
///
/// # Errors
///
/// As [`list_params`].
pub fn list_with_inheritance<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    scope: &ParamScope,
    prefix: Option<&str>,
    inherited: bool,
) -> Result<Vec<(ParamKey, ParamRecord)>> {
    let mut seen = std::collections::BTreeMap::new();
    let chain = if inherited {
        scope.lookup_chain()
    } else {
        vec![scope.clone()]
    };
    // Nearest scope first (per `lookup_chain`'s own contract): inserting in
    // that order and never overwriting an existing key is what makes a
    // node's own value shadow its dataflow's, which shadows the global one.
    for candidate in chain {
        for (key, record) in list_params(store, &candidate)? {
            seen.entry(key).or_insert(record);
        }
    }
    let mut out: Vec<_> = seen.into_iter().collect();
    if let Some(prefix) = prefix {
        out.retain(|(key, _)| key.as_str().starts_with(prefix));
    }
    Ok(out)
}

/// Every scope a [`ParamKey`] deleted at `node`/`dataflow` should also be
/// checked at, for cleanup when a dataflow is destroyed — not exported
/// publicly, folded straight into [`delete_all_for_dataflow`].
///
/// # Errors
///
/// As [`astrs_store::CoordinatorStore::list_params`].
pub fn delete_all_for_dataflow<S: KvStore + Clone>(
    store: &CoordinatorStore<S>,
    dataflow: DataflowId,
    nodes: &[NodeId],
) -> Result<()> {
    for (key, _) in store.list_params(dataflow)? {
        store.delete_param(dataflow, &key)?;
    }
    for node in nodes {
        for (key, _) in store.list_node_params(dataflow, node)? {
            store.delete_node_param(dataflow, node, &key)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Async, `Parameter`-typed wrappers — what `crate::handlers::params`
// actually calls.
// ---------------------------------------------------------------------
//
// The functions above are generic over any `KvStore` backend and speak
// `serde_json::Value` directly, which is what makes them straightforward
// to unit-test against an in-memory store. A coordinator's real store
// (`astrs_store::AsyncStore`) is `CoordinatorStore<RedbStore>` wrapped for
// async use, and every value that actually crosses the wire is a
// `Parameter`, never bare JSON — these wrappers are the one place that
// bridges both gaps at once: they push the blocking backend call through
// [`tokio::task::spawn_blocking`] (mirroring
// [`astrs_store::AsyncCoordinatorStore`]'s own pattern, which this crate
// cannot reuse directly since it wraps the *whole* store, not one
// scope-routed operation), and they convert [`Parameter`] to and from the
// JSON the store persists via [`parameter_to_json`] / [`json_to_parameter`].

/// Runs a synchronous store closure on the blocking thread pool.
///
/// # Errors
///
/// [`CoordinatorError::invalid`] if the blocking task panicked or was
/// cancelled; otherwise whatever `f` returns.
async fn run_blocking<T, F>(store: &astrs_store::AsyncStore, f: F) -> Result<T>
where
    F: FnOnce(&CoordinatorStore<oxistore_kv_redb::RedbStore>) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let store = store.sync().clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|_| CoordinatorError::invalid("a store task panicked or was cancelled"))?
}

/// Async, [`Parameter`]-typed wrapper for [`set_param`].
///
/// # Errors
///
/// As [`set_param`], plus [`CoordinatorError::invalid`] if `value` fails to
/// serialize (unreachable for a well-formed [`Parameter`]).
pub async fn set_param_async(
    store: &astrs_store::AsyncStore,
    scope: ParamScope,
    key: ParamKey,
    value: &Parameter,
) -> Result<MutationSeq> {
    let json = parameter_to_json(value)?;
    run_blocking(store, move |store| set_param(store, &scope, key, json)).await
}

/// Async, [`Parameter`]-typed wrapper for [`lookup_with_inheritance`].
///
/// # Errors
///
/// As [`lookup_with_inheritance`], plus [`CoordinatorError::invalid`] if
/// the stored value is not a valid [`Parameter`] encoding (on-disk
/// corruption, or a value written by something other than this
/// coordinator).
pub async fn get_param_async(
    store: &astrs_store::AsyncStore,
    scope: ParamScope,
    key: ParamKey,
    inherited: bool,
) -> Result<Option<(ParamScope, Parameter)>> {
    let found = run_blocking(store, move |store| {
        lookup_with_inheritance(store, &scope, &key, inherited)
    })
    .await?;
    match found {
        Some((found_scope, record)) => {
            let value = json_to_parameter(record.value().map_err(|source| {
                CoordinatorError::invalid(format!(
                    "stored parameter value is not valid JSON: {source}"
                ))
            })?)?;
            Ok(Some((found_scope, value)))
        }
        None => Ok(None),
    }
}

/// Async wrapper for [`delete_param`].
///
/// # Errors
///
/// As [`delete_param`].
pub async fn delete_param_async(
    store: &astrs_store::AsyncStore,
    scope: ParamScope,
    key: ParamKey,
) -> Result<Option<MutationSeq>> {
    run_blocking(store, move |store| delete_param(store, &scope, &key)).await
}

/// Async, [`Parameter`]-typed wrapper for [`list_with_inheritance`].
///
/// # Errors
///
/// As [`list_with_inheritance`], plus [`CoordinatorError::invalid`] if a
/// stored value is not a valid [`Parameter`] encoding.
pub async fn list_params_async(
    store: &astrs_store::AsyncStore,
    scope: ParamScope,
    prefix: Option<String>,
    inherited: bool,
) -> Result<Vec<(ParamKey, Parameter)>> {
    let listed = run_blocking(store, move |store| {
        list_with_inheritance(store, &scope, prefix.as_deref(), inherited)
    })
    .await?;
    listed
        .into_iter()
        .map(|(key, record)| {
            let value = json_to_parameter(record.value().map_err(|source| {
                CoordinatorError::invalid(format!(
                    "stored parameter value is not valid JSON: {source}"
                ))
            })?)?;
            Ok((key, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_store::CoordinatorStore;

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    fn key(text: &str) -> ParamKey {
        ParamKey::new(text).unwrap()
    }

    #[test]
    fn parameter_json_round_trips_every_variant() {
        let node = NodeId::new("camera").unwrap();
        let _ = &node; // silence an unused warning if the list below shrinks
        let samples = vec![
            Parameter::Bool(true),
            Parameter::Integer(-7),
            Parameter::Float(1.5),
            Parameter::String("hi".into()),
            Parameter::ListInt(vec![1, 2, 3]),
            Parameter::ListFloat(vec![1.0, 2.0]),
            Parameter::ListString(vec!["a".into(), "b".into()]),
            Parameter::Timestamp(astrs_time::HlcTimestamp::new(1, 0)),
        ];
        for sample in samples {
            let json = parameter_to_json(&sample).unwrap();
            let back = json_to_parameter(json).unwrap();
            assert_eq!(back, sample);
        }
    }

    #[test]
    fn global_scope_lives_under_the_nil_dataflow_sentinel() {
        let store = store();
        set_param(
            &store,
            &ParamScope::Global,
            key("gain"),
            serde_json::json!(1),
        )
        .unwrap();
        assert_eq!(
            store
                .get_param(GLOBAL_SCOPE_DATAFLOW, &key("gain"))
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!(1)
        );
    }

    #[test]
    fn each_scope_routes_to_its_own_bucket_without_cross_talk() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let k = key("gain");

        set_param(
            &store,
            &ParamScope::Global,
            k.clone(),
            serde_json::json!("global"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::dataflow_scope(dataflow),
            k.clone(),
            serde_json::json!("dataflow"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::node(dataflow, node.clone()),
            k.clone(),
            serde_json::json!("node"),
        )
        .unwrap();

        assert_eq!(
            get_param(&store, &ParamScope::Global, &k)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!("global")
        );
        assert_eq!(
            get_param(&store, &ParamScope::dataflow_scope(dataflow), &k)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!("dataflow")
        );
        assert_eq!(
            get_param(&store, &ParamScope::node(dataflow, node), &k)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!("node")
        );
    }

    #[test]
    fn lookup_without_inheritance_sees_only_the_exact_scope() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let k = key("gain");
        set_param(&store, &ParamScope::Global, k.clone(), serde_json::json!(1)).unwrap();

        assert!(
            lookup_with_inheritance(&store, &ParamScope::node(dataflow, node), &k, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lookup_with_inheritance_falls_back_through_the_chain() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let k = key("gain");
        set_param(
            &store,
            &ParamScope::Global,
            k.clone(),
            serde_json::json!("global"),
        )
        .unwrap();

        let (found_scope, record) =
            lookup_with_inheritance(&store, &ParamScope::node(dataflow, node), &k, true)
                .unwrap()
                .expect("falls back to global");
        assert_eq!(found_scope, ParamScope::Global);
        assert_eq!(record.value().unwrap(), serde_json::json!("global"));
    }

    #[test]
    fn a_node_value_shadows_an_inherited_dataflow_value() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let k = key("gain");
        set_param(
            &store,
            &ParamScope::dataflow_scope(dataflow),
            k.clone(),
            serde_json::json!("dataflow"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::node(dataflow, node.clone()),
            k.clone(),
            serde_json::json!("node"),
        )
        .unwrap();

        let (found_scope, record) =
            lookup_with_inheritance(&store, &ParamScope::node(dataflow, node.clone()), &k, true)
                .unwrap()
                .expect("present");
        assert_eq!(found_scope, ParamScope::node(dataflow, node));
        assert_eq!(record.value().unwrap(), serde_json::json!("node"));
    }

    #[test]
    fn list_with_inheritance_merges_and_shadows_by_key() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        set_param(
            &store,
            &ParamScope::Global,
            key("a"),
            serde_json::json!("global-a"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::Global,
            key("shared"),
            serde_json::json!("global"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::node(dataflow, node.clone()),
            key("shared"),
            serde_json::json!("node"),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::node(dataflow, node.clone()),
            key("b"),
            serde_json::json!("node-b"),
        )
        .unwrap();

        let listed =
            list_with_inheritance(&store, &ParamScope::node(dataflow, node), None, true).unwrap();
        let map: std::collections::BTreeMap<String, serde_json::Value> = listed
            .into_iter()
            .map(|(k, r)| (k.to_string(), r.value().unwrap()))
            .collect();
        assert_eq!(map.get("a"), Some(&serde_json::json!("global-a")));
        assert_eq!(map.get("b"), Some(&serde_json::json!("node-b")));
        assert_eq!(
            map.get("shared"),
            Some(&serde_json::json!("node")),
            "node shadows global"
        );
    }

    #[test]
    fn list_with_inheritance_respects_a_prefix_filter() {
        let store = store();
        set_param(
            &store,
            &ParamScope::Global,
            key("cam.gain"),
            serde_json::json!(1),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::Global,
            key("lidar.rate"),
            serde_json::json!(2),
        )
        .unwrap();

        let listed =
            list_with_inheritance(&store, &ParamScope::Global, Some("cam."), false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0.as_str(), "cam.gain");
    }

    #[test]
    fn list_without_inheritance_never_pulls_in_a_parent_scope() {
        let store = store();
        let dataflow = DataflowId::generate();
        set_param(&store, &ParamScope::Global, key("a"), serde_json::json!(1)).unwrap();
        let listed =
            list_with_inheritance(&store, &ParamScope::dataflow_scope(dataflow), None, false)
                .unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn delete_removes_from_the_scoped_bucket_only() {
        let store = store();
        let dataflow = DataflowId::generate();
        let k = key("gain");
        set_param(&store, &ParamScope::Global, k.clone(), serde_json::json!(1)).unwrap();
        set_param(
            &store,
            &ParamScope::dataflow_scope(dataflow),
            k.clone(),
            serde_json::json!(2),
        )
        .unwrap();

        assert!(
            delete_param(&store, &ParamScope::Global, &k)
                .unwrap()
                .is_some()
        );
        assert!(
            get_param(&store, &ParamScope::Global, &k)
                .unwrap()
                .is_none()
        );
        assert!(
            get_param(&store, &ParamScope::dataflow_scope(dataflow), &k)
                .unwrap()
                .is_some(),
            "an unrelated scope's value must survive"
        );
    }

    #[test]
    fn delete_all_for_dataflow_clears_dataflow_and_every_named_node() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        set_param(
            &store,
            &ParamScope::dataflow_scope(dataflow),
            key("a"),
            serde_json::json!(1),
        )
        .unwrap();
        set_param(
            &store,
            &ParamScope::node(dataflow, node.clone()),
            key("b"),
            serde_json::json!(2),
        )
        .unwrap();

        delete_all_for_dataflow(&store, dataflow, std::slice::from_ref(&node)).unwrap();

        assert!(store.list_params(dataflow).unwrap().is_empty());
        assert!(store.list_node_params(dataflow, &node).unwrap().is_empty());
    }

    #[test]
    fn an_invalid_stored_value_is_a_typed_error_not_a_panic() {
        let err = json_to_parameter(serde_json::json!({"not": "a parameter"})).unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }

    fn async_store() -> astrs_store::AsyncStore {
        astrs_store::AsyncStore::new(CoordinatorStore::open_in_memory().unwrap())
    }

    #[tokio::test]
    async fn the_async_wrappers_round_trip_a_parameter_through_the_blocking_pool() {
        let store = async_store();
        let scope = ParamScope::dataflow_scope(DataflowId::generate());
        let k = key("gain");

        set_param_async(&store, scope.clone(), k.clone(), &Parameter::Float(1.5))
            .await
            .unwrap();
        let (found_scope, value) = get_param_async(&store, scope.clone(), k.clone(), false)
            .await
            .unwrap()
            .expect("just written");
        assert_eq!(found_scope, scope);
        assert_eq!(value, Parameter::Float(1.5));

        let listed = list_params_async(&store, scope.clone(), None, false)
            .await
            .unwrap();
        assert_eq!(listed, vec![(k.clone(), Parameter::Float(1.5))]);

        assert!(
            delete_param_async(&store, scope.clone(), k.clone())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            get_param_async(&store, scope, k, false)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_async_wrappers_still_apply_scope_inheritance() {
        let store = async_store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let k = key("gain");
        set_param_async(
            &store,
            ParamScope::Global,
            k.clone(),
            &Parameter::Bool(true),
        )
        .await
        .unwrap();

        let (found_scope, value) =
            get_param_async(&store, ParamScope::node(dataflow, node), k, true)
                .await
                .unwrap()
                .expect("falls back to global");
        assert_eq!(found_scope, ParamScope::Global);
        assert_eq!(value, Parameter::Bool(true));
    }
}
