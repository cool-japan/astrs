//! Propose-first parameter writes.
//!
//! # Why parameters get the stricter path
//!
//! A parameter is authoritative state that nothing else re-derives: if a
//! coordinator writes one locally and then dies before the cluster agrees, the
//! phantom value can sit there forever, read by every node that asks. So the
//! parameter path never writes locally first. The leader reads the current
//! record, computes the **complete** new one — value, revision, both
//! timestamps — proposes it, and only the committed entry is applied, by every
//! replica including the leader.
//!
//! # Determinism comes from deciding everything before proposing
//!
//! `revision + 1` computed independently on three replicas is three different
//! answers the moment their reads disagree. Computing it *once*, on the
//! leader, and shipping the finished record makes applying a pure overwrite —
//! which is exactly the property
//! [`astrs_store::CoordinatorStore::apply_replayed`] is built around, and the
//! reason this integration needed no new record type.
//!
//! # Why the leader must be *ready*, not merely elected
//!
//! `revision + 1` is a read-modify-write, so it is only correct if the read
//! saw everything the previous leader committed. A freshly elected leader has
//! not applied its own no-op yet and may be behind. The gate in
//! `handlers::dispatch` is [`crate::ha::HaHandle::is_ready`], not
//! "am I the leader", precisely for this.

use astrs_store::record::{MutationOp, ParamRecord};
use astrs_wire::{ControlReply, ControlRequest, ParamKey, ParamScope, Parameter};

use crate::coordinator::Coordinator;
use crate::error::{CoordinatorError, Result};
use crate::ha::HaHandle;
use crate::param_scope;

/// Routes a parameter write through Raft, if `request` is one.
///
/// Returns `None` for any other verb, leaving it to the caller's own path.
///
/// # Panics
///
/// Never panics.
pub async fn route(
    coordinator: &Coordinator,
    ha: &HaHandle,
    request: &ControlRequest,
) -> Option<ControlReply> {
    match request {
        ControlRequest::SetParam {
            scope,
            key,
            value,
            create_only,
        } => Some(
            set_param(
                coordinator,
                ha,
                scope.clone(),
                key.clone(),
                value.clone(),
                *create_only,
            )
            .await,
        ),
        ControlRequest::DeleteParam { scope, key } => {
            Some(delete_param(coordinator, ha, scope.clone(), key.clone()).await)
        }
        _ => None,
    }
}

/// Whether `request` is one this module replicates.
///
/// Kept beside [`route`] so the two cannot disagree about which verbs take
/// the propose-first path.
#[must_use]
pub fn is_replicated_directly(request: &ControlRequest) -> bool {
    matches!(
        request,
        ControlRequest::SetParam { .. } | ControlRequest::DeleteParam { .. }
    )
}

/// The propose-first form of `SetParam`.
async fn set_param(
    coordinator: &Coordinator,
    ha: &HaHandle,
    scope: ParamScope,
    key: ParamKey,
    value: Parameter,
    create_only: bool,
) -> ControlReply {
    let existing = match read_exact(coordinator, &scope, &key).await {
        Ok(existing) => existing,
        Err(error) => return error.into_reply(),
    };
    if create_only && existing.is_some() {
        return CoordinatorError::AlreadyExists {
            kind: "parameter",
            name: key.to_string(),
        }
        .into_reply();
    }

    let record = match next_record(coordinator, existing, &value) {
        Ok(record) => record,
        Err(error) => return error.into_reply(),
    };
    let op = match put_op(&scope, key.clone(), record) {
        Ok(op) => op,
        Err(error) => return error.into_reply(),
    };

    if let Err(error) = ha.propose(vec![op]).await {
        return error.into_reply();
    }
    // The nodes that read this parameter learn about it from their own
    // daemon; that fan-out is the leader's job and is not replicated, because
    // a follower has no connections to fan out to.
    crate::handlers::dispatch_param_update(coordinator, &scope, &key, Some(value)).await;
    ControlReply::Ok
}

/// The propose-first form of `DeleteParam`.
async fn delete_param(
    coordinator: &Coordinator,
    ha: &HaHandle,
    scope: ParamScope,
    key: ParamKey,
) -> ControlReply {
    let op = match delete_op(&scope, key.clone()) {
        Ok(op) => op,
        Err(error) => return error.into_reply(),
    };
    if let Err(error) = ha.propose(vec![op]).await {
        return error.into_reply();
    }
    crate::handlers::dispatch_param_update(coordinator, &scope, &key, None).await;
    ControlReply::Ok
}

/// Reads the record at exactly `scope` — never an inherited one, because a
/// write at this scope must not be versioned against a parent's revision.
async fn read_exact(
    coordinator: &Coordinator,
    scope: &ParamScope,
    key: &ParamKey,
) -> Result<Option<ParamRecord>> {
    let store = coordinator.store.sync().clone();
    let scope = scope.clone();
    let key = key.clone();
    tokio::task::spawn_blocking(move || param_scope::get_param(&store, &scope, &key))
        .await
        .map_err(|_| CoordinatorError::invalid("a store task panicked or was cancelled"))?
}

/// Builds the complete record this write produces.
fn next_record(
    coordinator: &Coordinator,
    existing: Option<ParamRecord>,
    value: &Parameter,
) -> Result<ParamRecord> {
    let json = param_scope::parameter_to_json(value)?;
    let value_json = serde_json::to_string(&json).map_err(|source| {
        CoordinatorError::invalid(format!("parameter did not serialize: {source}"))
    })?;
    // One clock reading for both fields on a first write, so a record created
    // and updated in the same operation carries one consistent timestamp.
    let now = coordinator.clock.now();
    let (revision, created_at) = match &existing {
        Some(old) => (old.revision.saturating_add(1), old.created_at),
        None => (1, now),
    };
    Ok(ParamRecord {
        value_json,
        revision,
        created_at,
        updated_at: now,
    })
}

/// The write operation for `scope`.
fn put_op(scope: &ParamScope, key: ParamKey, record: ParamRecord) -> Result<MutationOp> {
    match scope {
        ParamScope::Global => Ok(MutationOp::ParamPut {
            dataflow: param_scope::GLOBAL_SCOPE_DATAFLOW,
            key,
            record,
        }),
        ParamScope::Dataflow { dataflow } => Ok(MutationOp::ParamPut {
            dataflow: *dataflow,
            key,
            record,
        }),
        ParamScope::Node { dataflow, node } => Ok(MutationOp::NodeParamPut {
            dataflow: *dataflow,
            node: node.clone(),
            key,
            record,
        }),
        // `ParamScope` is `#[non_exhaustive]`: a scope added in a later
        // protocol revision has no bucket mapping decided for it yet, and
        // guessing one would replicate a write into the wrong place.
        _ => Err(unknown_scope(scope)),
    }
}

/// The delete operation for `scope`.
fn delete_op(scope: &ParamScope, key: ParamKey) -> Result<MutationOp> {
    match scope {
        ParamScope::Global => Ok(MutationOp::ParamDelete {
            dataflow: param_scope::GLOBAL_SCOPE_DATAFLOW,
            key,
        }),
        ParamScope::Dataflow { dataflow } => Ok(MutationOp::ParamDelete {
            dataflow: *dataflow,
            key,
        }),
        ParamScope::Node { dataflow, node } => Ok(MutationOp::NodeParamDelete {
            dataflow: *dataflow,
            node: node.clone(),
            key,
        }),
        _ => Err(unknown_scope(scope)),
    }
}

/// The error for a scope this build has no bucket mapping for.
fn unknown_scope(scope: &ParamScope) -> CoordinatorError {
    CoordinatorError::invalid(format!(
        "this coordinator build has no storage mapping for parameter scope {scope:?}"
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::{AuthToken, DataflowId, NodeId};

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([21; 32])).with_port(0),
        )
        .unwrap()
    }

    #[test]
    fn only_the_two_parameter_verbs_take_this_path() {
        assert!(is_replicated_directly(&ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: ParamKey::new("a").unwrap(),
            value: Parameter::Integer(1),
            create_only: false,
        }));
        assert!(is_replicated_directly(&ControlRequest::DeleteParam {
            scope: ParamScope::Global,
            key: ParamKey::new("a").unwrap(),
        }));
        assert!(!is_replicated_directly(&ControlRequest::List { all: true }));
        assert!(!is_replicated_directly(&ControlRequest::Destroy {
            force: false
        }));
    }

    #[test]
    fn every_scope_maps_to_the_bucket_the_store_uses() {
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("gain").unwrap();
        let record = ParamRecord {
            value_json: "1".to_owned(),
            revision: 1,
            created_at: astrs_time::HlcTimestamp::EPOCH,
            updated_at: astrs_time::HlcTimestamp::EPOCH,
        };

        assert!(matches!(
            put_op(&ParamScope::Global, key.clone(), record.clone()).unwrap(),
            MutationOp::ParamPut { dataflow, .. } if dataflow == param_scope::GLOBAL_SCOPE_DATAFLOW
        ));
        assert!(matches!(
            put_op(
                &ParamScope::dataflow_scope(dataflow),
                key.clone(),
                record.clone()
            )
            .unwrap(),
            MutationOp::ParamPut { dataflow: found, .. } if found == dataflow
        ));
        assert!(matches!(
            put_op(
                &ParamScope::Node {
                    dataflow,
                    node: node.clone()
                },
                key.clone(),
                record
            )
            .unwrap(),
            MutationOp::NodeParamPut { .. }
        ));

        assert!(matches!(
            delete_op(&ParamScope::Global, key.clone()).unwrap(),
            MutationOp::ParamDelete { .. }
        ));
        assert!(matches!(
            delete_op(&ParamScope::Node { dataflow, node }, key).unwrap(),
            MutationOp::NodeParamDelete { .. }
        ));
    }

    #[tokio::test]
    async fn a_first_write_starts_at_revision_one_with_one_timestamp() {
        let coordinator = coordinator();
        let record = next_record(&coordinator, None, &Parameter::Integer(7)).unwrap();
        assert_eq!(record.revision, 1);
        assert_eq!(
            record.created_at, record.updated_at,
            "a record created and updated in one operation must carry one reading"
        );
        // `Parameter` is a tagged enum, so its JSON form is an object — the
        // same text `CoordinatorStore::set_param` would have stored.
        assert_eq!(record.value_json, r#"{"integer":7}"#);
    }

    #[tokio::test]
    async fn a_later_write_increments_the_revision_and_keeps_the_creation_time() {
        let coordinator = coordinator();
        let first = next_record(&coordinator, None, &Parameter::Integer(1)).unwrap();
        let second =
            next_record(&coordinator, Some(first.clone()), &Parameter::Integer(2)).unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(second.created_at, first.created_at);
        assert!(second.updated_at >= first.updated_at);
        assert_eq!(second.value_json, r#"{"integer":2}"#);
    }

    #[tokio::test]
    async fn the_revision_saturates_rather_than_wrapping() {
        // A wrapped revision would make a newer record look older to anything
        // comparing them.
        let coordinator = coordinator();
        let old = ParamRecord {
            value_json: "1".to_owned(),
            revision: u64::MAX,
            created_at: astrs_time::HlcTimestamp::EPOCH,
            updated_at: astrs_time::HlcTimestamp::EPOCH,
        };
        let next = next_record(&coordinator, Some(old), &Parameter::Integer(2)).unwrap();
        assert_eq!(next.revision, u64::MAX);
    }

    #[tokio::test]
    async fn reading_at_an_exact_scope_never_inherits_from_a_parent() {
        // Versioning a scoped write against a parent's revision would produce
        // a record whose revision is unrelated to its own history.
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        param_scope::set_param_async(
            &coordinator.store,
            ParamScope::Global,
            key.clone(),
            &Parameter::Integer(1),
        )
        .await
        .unwrap();

        let scoped = read_exact(&coordinator, &ParamScope::dataflow_scope(dataflow), &key)
            .await
            .unwrap();
        assert!(scoped.is_none(), "the global value must not be inherited");

        let global = read_exact(&coordinator, &ParamScope::Global, &key)
            .await
            .unwrap();
        assert!(global.is_some());
    }

    #[tokio::test]
    async fn values_serialize_the_way_the_store_would_have_written_them() {
        // The record this module builds must be byte-identical to what
        // `CoordinatorStore::set_param` would have produced, or a replicated
        // write and a local one would disagree on the same value.
        let coordinator = coordinator();
        let key = ParamKey::new("mixed").unwrap();
        for value in [
            Parameter::Integer(-3),
            Parameter::Bool(true),
            Parameter::String("text".to_owned()),
            Parameter::Float(1.5),
        ] {
            param_scope::set_param_async(
                &coordinator.store,
                ParamScope::Global,
                key.clone(),
                &value,
            )
            .await
            .unwrap();
            let written = read_exact(&coordinator, &ParamScope::Global, &key)
                .await
                .unwrap()
                .unwrap();
            let ours = next_record(&coordinator, None, &value).unwrap();
            assert_eq!(
                ours.value_json, written.value_json,
                "the replicated encoding of {value:?} must match the store's own"
            );
        }
    }
}
