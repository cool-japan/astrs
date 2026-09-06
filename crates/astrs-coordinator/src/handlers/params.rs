//! `GetParams`/`GetParam`/`SetParam`/`DeleteParam` (blueprint §17).
//!
//! Every write here funnels through [`crate::param_scope`]'s async
//! wrappers, which is also what persists it (`astrs-store`'s mutation log)
//! and therefore what a reconnecting daemon replays via
//! [`crate::catchup`]. Nothing in this module talks to a daemon directly —
//! propagating a change to the nodes that read it is
//! [`crate::handlers::lifecycle`]'s job once a dataflow exists to
//! propagate it *to* (see [`super::dispatch_param_update`]).

use astrs_wire::{ControlReply, ParamKey, ParamScope, Parameter};

use crate::coordinator::Coordinator;
use crate::param_scope;

/// `GetParams`.
pub async fn get_params(
    coordinator: &Coordinator,
    scope: ParamScope,
    prefix: Option<String>,
    inherited: bool,
) -> ControlReply {
    match param_scope::list_params_async(&coordinator.store, scope.clone(), prefix, inherited).await
    {
        Ok(params) => ControlReply::ParamList { scope, params },
        Err(err) => err.into_reply(),
    }
}

/// `GetParam`.
pub async fn get_param(
    coordinator: &Coordinator,
    scope: ParamScope,
    key: ParamKey,
    inherited: bool,
) -> ControlReply {
    match param_scope::get_param_async(&coordinator.store, scope.clone(), key.clone(), inherited)
        .await
    {
        Ok(Some((found_scope, value))) => ControlReply::ParamValue {
            key,
            value: Some(value),
            scope: found_scope,
        },
        Ok(None) => ControlReply::ParamValue {
            key,
            value: None,
            scope,
        },
        Err(err) => err.into_reply(),
    }
}

/// `SetParam`.
///
/// `create_only` refuses the write if the key already has a value *at this
/// exact scope* (not counting an inherited one from a parent scope, which
/// is not what "already exists" should mean for a scope-qualified write).
pub async fn set_param(
    coordinator: &Coordinator,
    scope: ParamScope,
    key: ParamKey,
    value: Parameter,
    create_only: bool,
) -> ControlReply {
    if create_only {
        match param_scope::get_param_async(&coordinator.store, scope.clone(), key.clone(), false)
            .await
        {
            Ok(Some(_)) => {
                return crate::error::CoordinatorError::AlreadyExists {
                    kind: "parameter",
                    name: key.to_string(),
                }
                .into_reply();
            }
            Ok(None) => {}
            Err(err) => return err.into_reply(),
        }
    }
    match param_scope::set_param_async(&coordinator.store, scope.clone(), key.clone(), &value).await
    {
        Ok(_seq) => {
            super::dispatch_param_update(coordinator, &scope, &key, Some(value)).await;
            ControlReply::Ok
        }
        Err(err) => err.into_reply(),
    }
}

/// `DeleteParam`.
pub async fn delete_param(
    coordinator: &Coordinator,
    scope: ParamScope,
    key: ParamKey,
) -> ControlReply {
    match param_scope::delete_param_async(&coordinator.store, scope.clone(), key.clone()).await {
        Ok(_seq) => {
            super::dispatch_param_update(coordinator, &scope, &key, None).await;
            ControlReply::Ok
        }
        Err(err) => err.into_reply(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_wire::{AuthToken, DataflowId};

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([4; 32])).with_port(0),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn set_then_get_round_trips() {
        let coordinator = coordinator();
        let scope = ParamScope::Global;
        let key = ParamKey::new("gain").unwrap();
        let reply = set_param(
            &coordinator,
            scope.clone(),
            key.clone(),
            Parameter::Integer(3),
            false,
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);

        let reply = get_param(&coordinator, scope.clone(), key, false).await;
        match reply {
            ControlReply::ParamValue {
                value,
                scope: got_scope,
                ..
            } => {
                assert_eq!(value, Some(Parameter::Integer(3)));
                assert_eq!(got_scope, scope);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_on_an_unset_key_reports_the_asked_scope() {
        let coordinator = coordinator();
        let scope = ParamScope::dataflow_scope(DataflowId::generate());
        let reply = get_param(
            &coordinator,
            scope.clone(),
            ParamKey::new("x").unwrap(),
            false,
        )
        .await;
        match reply {
            ControlReply::ParamValue {
                value, scope: got, ..
            } => {
                assert_eq!(value, None);
                assert_eq!(got, scope);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_only_refuses_to_overwrite() {
        let coordinator = coordinator();
        let scope = ParamScope::Global;
        let key = ParamKey::new("gain").unwrap();
        set_param(
            &coordinator,
            scope.clone(),
            key.clone(),
            Parameter::Integer(1),
            false,
        )
        .await;

        let reply = set_param(&coordinator, scope, key, Parameter::Integer(2), true).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::AlreadyExists)
        );
    }

    #[tokio::test]
    async fn create_only_succeeds_on_a_fresh_key() {
        let coordinator = coordinator();
        let reply = set_param(
            &coordinator,
            ParamScope::Global,
            ParamKey::new("fresh").unwrap(),
            Parameter::Bool(true),
            true,
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);
    }

    #[tokio::test]
    async fn delete_then_get_reports_unset() {
        let coordinator = coordinator();
        let scope = ParamScope::Global;
        let key = ParamKey::new("gain").unwrap();
        set_param(
            &coordinator,
            scope.clone(),
            key.clone(),
            Parameter::Integer(1),
            false,
        )
        .await;
        let reply = delete_param(&coordinator, scope.clone(), key.clone()).await;
        assert_eq!(reply, ControlReply::Ok);

        let reply = get_param(&coordinator, scope, key, false).await;
        assert!(matches!(
            reply,
            ControlReply::ParamValue { value: None, .. }
        ));
    }

    #[tokio::test]
    async fn get_params_lists_everything_at_the_scope() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let scope = ParamScope::dataflow_scope(dataflow);
        set_param(
            &coordinator,
            scope.clone(),
            ParamKey::new("a").unwrap(),
            Parameter::Integer(1),
            false,
        )
        .await;
        set_param(
            &coordinator,
            scope.clone(),
            ParamKey::new("b").unwrap(),
            Parameter::Integer(2),
            false,
        )
        .await;

        let reply = get_params(&coordinator, scope, None, false).await;
        match reply {
            ControlReply::ParamList { params, .. } => assert_eq!(params.len(), 2),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_params_honours_inheritance_and_prefix() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        set_param(
            &coordinator,
            ParamScope::Global,
            ParamKey::new("cam.gain").unwrap(),
            Parameter::Integer(1),
            false,
        )
        .await;
        set_param(
            &coordinator,
            ParamScope::Global,
            ParamKey::new("lidar.rate").unwrap(),
            Parameter::Integer(2),
            false,
        )
        .await;

        let reply = get_params(
            &coordinator,
            ParamScope::dataflow_scope(dataflow),
            Some("cam.".to_owned()),
            true,
        )
        .await;
        match reply {
            ControlReply::ParamList { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0.as_str(), "cam.gain");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn revisions_increase_on_every_write_in_order() {
        let coordinator = coordinator();
        let scope = ParamScope::Global;
        let key = ParamKey::new("gain").unwrap();
        for value in 1..=3 {
            set_param(
                &coordinator,
                scope.clone(),
                key.clone(),
                Parameter::Integer(value),
                false,
            )
            .await;
        }
        let record = coordinator
            .store
            .get_param(param_scope::GLOBAL_SCOPE_DATAFLOW, key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.revision, 3);
    }
}
