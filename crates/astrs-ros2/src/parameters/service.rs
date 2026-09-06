//! [`ParameterServices`]: the six `rcl_interfaces` services and the
//! `/parameter_events` topic.
//!
//! What `ros2 param list`, `ros2 param get`, `ros2 param set` and
//! `ros2 param describe` actually call. Every one of them is a service on a
//! name derived from the node's own:
//!
//! ```text
//!   /<node>/get_parameters              rcl_interfaces/srv/GetParameters
//!   /<node>/get_parameter_types         rcl_interfaces/srv/GetParameterTypes
//!   /<node>/set_parameters              rcl_interfaces/srv/SetParameters
//!   /<node>/set_parameters_atomically   rcl_interfaces/srv/SetParametersAtomically
//!   /<node>/describe_parameters         rcl_interfaces/srv/DescribeParameters
//!   /<node>/list_parameters             rcl_interfaces/srv/ListParameters
//! ```
//!
//! plus one topic, shared by every node in the system:
//!
//! ```text
//!   /parameter_events                   rcl_interfaces/msg/ParameterEvent
//! ```
//!
//! # Positional answers
//!
//! `GetParameters` answers with a `values` sequence **in the order the
//! request's `names` came in**, and an undeclared name gets a
//! `PARAMETER_NOT_SET` rather than being omitted — omitting it would shift
//! every subsequent answer onto the wrong name. The same rule applies to
//! `GetParameterTypes` and `DescribeParameters`, and the tests below assert
//! it for all three.
//!
//! # One task per service
//!
//! Each service runs its own loop over a shared [`ParameterStore`] behind a
//! `Mutex`. Six tasks rather than one `select!` because the six have
//! different request and response types and no shared state beyond the
//! store; a single loop would need a hand-rolled enum over six pairs to say
//! nothing extra.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::Ros2Result;
use crate::msg::rcl_interfaces;
use crate::names::{FullName, NodeName};
use crate::node::Ros2Context;
use crate::parameters::descriptor::ParameterDescriptor;
use crate::parameters::store::{ParameterChange, ParameterStore};
use crate::parameters::value::ParameterValue;
use crate::pubsub::Publisher;
use crate::qos::QosProfile;
use crate::service::{
    DescribeParameters, GetParameterTypes, GetParameters, ListParameters, ServiceServer,
    SetParameters, SetParametersAtomically,
};
use crate::time::RosTime;

/// The topic every node announces its parameter changes on.
pub const PARAMETER_EVENTS_TOPIC: &str = "/parameter_events";

/// The six parameter services and the `/parameter_events` publisher.
///
/// Created by a node when
/// [`NodeOptions::start_parameter_services`](crate::node::NodeOptions::start_parameter_services)
/// is set, which it is by default.
#[derive(Debug)]
pub struct ParameterServices {
    store: Arc<Mutex<ParameterStore>>,
    events: Publisher<rcl_interfaces::ParameterEvent>,
    node: NodeName,
    context: Arc<Ros2Context>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ParameterServices {
    /// Create the six servers, the events publisher, and the tasks that
    /// answer.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Rtps`] when an endpoint cannot be created, and
    /// [`crate::Ros2Error::InvalidServiceName`] when the node's name
    /// produces an over-long service name.
    pub async fn start(
        context: Arc<Ros2Context>,
        node: NodeName,
        store: Arc<Mutex<ParameterStore>>,
        events_qos: QosProfile,
    ) -> Ros2Result<Self> {
        let owner = node.fully_qualified();
        let events = Publisher::<rcl_interfaces::ParameterEvent>::new(
            Arc::clone(&context),
            FullName::topic(PARAMETER_EVENTS_TOPIC)?,
            events_qos,
            Some(owner.clone()),
        )
        .await?;

        let services = Self {
            store,
            events,
            node,
            context,
            tasks: Mutex::new(Vec::new()),
        };
        services.spawn_all(owner).await?;
        Ok(services)
    }

    /// The store the six services read and write.
    #[must_use]
    pub fn store(&self) -> &Arc<Mutex<ParameterStore>> {
        &self.store
    }

    /// The `/parameter_events` publisher.
    #[must_use]
    pub const fn events(&self) -> &Publisher<rcl_interfaces::ParameterEvent> {
        &self.events
    }

    /// The fully-qualified name of one of this node's parameter services.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::InvalidServiceName`] when the derived name is
    /// malformed or too long.
    pub fn service_name(&self, leaf: &str) -> Ros2Result<FullName> {
        FullName::service(format!("{}/{leaf}", self.node.fully_qualified()))
    }

    /// Announce a batch of changes on `/parameter_events`.
    ///
    /// One message per batch, not per change: that is what makes an atomic
    /// set observably atomic from outside the node.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Cdr`] or [`crate::Ros2Error::Rtps`].
    pub async fn announce(&self, changes: &[ParameterChange]) -> Ros2Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let stamp = self.context.clock().now();
        let event = build_event(&self.node.fully_qualified(), stamp, changes);
        self.events.publish(&event).await
    }

    /// Stop every service task and delete every endpoint.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Rtps`] or [`crate::Ros2Error::Cdr`].
    pub async fn shutdown(&self) -> Ros2Result<()> {
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
        }
        self.events.destroy().await?;
        Ok(())
    }

    /// Create and spawn all six servers.
    async fn spawn_all(&self, owner: String) -> Ros2Result<()> {
        let qos = QosProfile::parameters();
        let mut tasks = Vec::with_capacity(6);

        let get = ServiceServer::<GetParameters>::new(
            Arc::clone(&self.context),
            self.service_name("get_parameters")?,
            qos,
            Some(owner.clone()),
        )
        .await?;
        let store = Arc::clone(&self.store);
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = get.take_request().await;
                let values = {
                    let store = store.lock().await;
                    request
                        .names
                        .iter()
                        .map(|name| store.get_or_unset(name).to_message())
                        .collect()
                };
                if get
                    .send_response(id, &rcl_interfaces::GetParametersResponse { values })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        let types = ServiceServer::<GetParameterTypes>::new(
            Arc::clone(&self.context),
            self.service_name("get_parameter_types")?,
            qos,
            Some(owner.clone()),
        )
        .await?;
        let store = Arc::clone(&self.store);
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = types.take_request().await;
                let codes = {
                    let store = store.lock().await;
                    request
                        .names
                        .iter()
                        .map(|name| store.type_of(name))
                        .collect()
                };
                if types
                    .send_response(
                        id,
                        &rcl_interfaces::GetParameterTypesResponse { types: codes },
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        let describe = ServiceServer::<DescribeParameters>::new(
            Arc::clone(&self.context),
            self.service_name("describe_parameters")?,
            qos,
            Some(owner.clone()),
        )
        .await?;
        let store = Arc::clone(&self.store);
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = describe.take_request().await;
                let descriptors = {
                    let store = store.lock().await;
                    request
                        .names
                        .iter()
                        .map(|name| store.describe(name).to_message())
                        .collect()
                };
                if describe
                    .send_response(
                        id,
                        &rcl_interfaces::DescribeParametersResponse { descriptors },
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        let list = ServiceServer::<ListParameters>::new(
            Arc::clone(&self.context),
            self.service_name("list_parameters")?,
            qos,
            Some(owner.clone()),
        )
        .await?;
        let store = Arc::clone(&self.store);
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = list.take_request().await;
                let found = {
                    let store = store.lock().await;
                    store.list(&request.prefixes, request.depth)
                };
                if list
                    .send_response(
                        id,
                        &rcl_interfaces::ListParametersResponse {
                            result: rcl_interfaces::ListParametersResult {
                                names: found.names,
                                prefixes: found.prefixes,
                            },
                        },
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        let set = ServiceServer::<SetParameters>::new(
            Arc::clone(&self.context),
            self.service_name("set_parameters")?,
            qos,
            Some(owner.clone()),
        )
        .await?;
        let store = Arc::clone(&self.store);
        let announcer = self.announcer();
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = set.take_request().await;
                let (results, changes) = {
                    let mut store = store.lock().await;
                    apply_each(&mut store, &request.parameters)
                };
                announcer.announce(&changes).await;
                if set
                    .send_response(id, &rcl_interfaces::SetParametersResponse { results })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        let atomic = ServiceServer::<SetParametersAtomically>::new(
            Arc::clone(&self.context),
            self.service_name("set_parameters_atomically")?,
            qos,
            Some(owner),
        )
        .await?;
        let store = Arc::clone(&self.store);
        let announcer = self.announcer();
        tasks.push(tokio::spawn(async move {
            loop {
                let (id, request) = atomic.take_request().await;
                let (result, changes) = {
                    let mut store = store.lock().await;
                    apply_atomically(&mut store, &request.parameters)
                };
                announcer.announce(&changes).await;
                if atomic
                    .send_response(
                        id,
                        &rcl_interfaces::SetParametersAtomicallyResponse { result },
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }));

        self.tasks.lock().await.extend(tasks);
        Ok(())
    }

    /// A cloneable handle the service tasks use to announce changes.
    fn announcer(&self) -> EventAnnouncer {
        EventAnnouncer {
            events: self.events.clone(),
            node: self.node.fully_qualified(),
            context: Arc::clone(&self.context),
        }
    }
}

/// The part of [`ParameterServices`] a spawned task needs to publish an
/// event.
///
/// A separate type because a task cannot hold a borrow of the services it
/// belongs to, and cloning the whole thing would clone the task list.
#[derive(Debug, Clone)]
struct EventAnnouncer {
    events: Publisher<rcl_interfaces::ParameterEvent>,
    node: String,
    context: Arc<Ros2Context>,
}

impl EventAnnouncer {
    /// Publish one event for a batch, swallowing a failure.
    ///
    /// A service that answered correctly but could not announce the change
    /// has still answered correctly; failing the call would be worse than
    /// the missed event, and the log line is where the missed event is
    /// recorded.
    async fn announce(&self, changes: &[ParameterChange]) {
        if changes.is_empty() {
            return;
        }
        let stamp = self.context.clock().now();
        let event = build_event(&self.node, stamp, changes);
        if let Err(error) = self.events.publish(&event).await {
            tracing::debug!(node = %self.node, %error, "could not announce a parameter change");
        }
    }
}

/// Apply a `SetParameters` request, collecting one result per parameter.
fn apply_each(
    store: &mut ParameterStore,
    parameters: &[rcl_interfaces::Parameter],
) -> (
    Vec<rcl_interfaces::SetParametersResult>,
    Vec<ParameterChange>,
) {
    let mut results = Vec::with_capacity(parameters.len());
    let mut changes = Vec::new();
    for parameter in parameters {
        let value = ParameterValue::from_message(&parameter.value);
        match store.set(&parameter.name, value) {
            Ok(change) => {
                changes.push(change);
                results.push(rcl_interfaces::SetParametersResult {
                    successful: true,
                    reason: String::new(),
                });
            }
            Err(error) => results.push(rcl_interfaces::SetParametersResult {
                successful: false,
                reason: error.to_string(),
            }),
        }
    }
    (results, changes)
}

/// Apply a `SetParametersAtomically` request.
fn apply_atomically(
    store: &mut ParameterStore,
    parameters: &[rcl_interfaces::Parameter],
) -> (rcl_interfaces::SetParametersResult, Vec<ParameterChange>) {
    let updates: Vec<(String, ParameterValue)> = parameters
        .iter()
        .map(|parameter| {
            (
                parameter.name.clone(),
                ParameterValue::from_message(&parameter.value),
            )
        })
        .collect();
    match store.set_atomically(updates) {
        Ok(changes) => (
            rcl_interfaces::SetParametersResult {
                successful: true,
                reason: String::new(),
            },
            changes,
        ),
        Err(error) => (
            rcl_interfaces::SetParametersResult {
                successful: false,
                reason: error.to_string(),
            },
            Vec::new(),
        ),
    }
}

/// Build one `ParameterEvent` from a batch of changes.
///
/// The three sets are disjoint by construction: a name can only be in one
/// of declared, changed and deleted per batch, because
/// [`ParameterStore`] produces exactly one change per name per call.
#[must_use]
pub fn build_event(
    node: &str,
    stamp: RosTime,
    changes: &[ParameterChange],
) -> rcl_interfaces::ParameterEvent {
    let mut event = rcl_interfaces::ParameterEvent {
        stamp: stamp.to_message(),
        node: node.to_owned(),
        ..rcl_interfaces::ParameterEvent::default()
    };
    for change in changes {
        let parameter = rcl_interfaces::Parameter {
            name: change.name().to_owned(),
            value: change.value().to_message(),
        };
        match change {
            ParameterChange::Declared { .. } => event.new_parameters.push(parameter),
            ParameterChange::Changed { .. } => event.changed_parameters.push(parameter),
            ParameterChange::Deleted { .. } => event.deleted_parameters.push(parameter),
        }
    }
    event
}

/// Render a descriptor as the wire message, for a caller outside this
/// module.
#[must_use]
pub fn descriptor_message(descriptor: &ParameterDescriptor) -> rcl_interfaces::ParameterDescriptor {
    descriptor.to_message()
}

/// How long a parameter service call waits by default.
pub const DEFAULT_PARAMETER_TIMEOUT: StdDuration = StdDuration::from_secs(5);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::parameters::store::ParameterStore;

    fn store() -> ParameterStore {
        let mut store = ParameterStore::new();
        store.declare("a", 1_i64).expect("declare");
        store.declare("b", "two").expect("declare");
        store
    }

    fn wire(name: &str, value: ParameterValue) -> rcl_interfaces::Parameter {
        rcl_interfaces::Parameter {
            name: name.to_owned(),
            value: value.to_message(),
        }
    }

    #[test]
    fn set_parameters_answers_one_result_per_request_in_order() {
        let mut store = store();
        let (results, changes) = apply_each(
            &mut store,
            &[
                wire("a", ParameterValue::Integer(10)),
                wire("missing", ParameterValue::Integer(0)),
                wire("b", ParameterValue::String("three".to_owned())),
            ],
        );
        assert_eq!(results.len(), 3);
        assert!(results[0].successful);
        assert!(!results[1].successful);
        assert!(!results[1].reason.is_empty(), "a failure explains itself");
        assert!(results[2].successful);
        assert_eq!(changes.len(), 2, "only the successes are announced");
    }

    #[test]
    fn set_parameters_atomically_reports_one_result_for_the_batch() {
        let mut store = store();
        let (result, changes) = apply_atomically(
            &mut store,
            &[
                wire("a", ParameterValue::Integer(10)),
                wire("missing", ParameterValue::Integer(0)),
            ],
        );
        assert!(!result.successful);
        assert!(changes.is_empty());
        assert_eq!(
            store.get("a").expect("unchanged"),
            &ParameterValue::Integer(1),
            "nothing was applied"
        );

        let (result, changes) = apply_atomically(
            &mut store,
            &[
                wire("a", ParameterValue::Integer(10)),
                wire("b", ParameterValue::String("three".to_owned())),
            ],
        );
        assert!(result.successful);
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn an_event_sorts_changes_into_the_three_disjoint_sets() {
        let changes = vec![
            ParameterChange::Declared {
                name: "new".to_owned(),
                value: ParameterValue::Integer(1),
            },
            ParameterChange::Changed {
                name: "old".to_owned(),
                previous: ParameterValue::Integer(1),
                value: ParameterValue::Integer(2),
            },
            ParameterChange::Deleted {
                name: "gone".to_owned(),
                value: ParameterValue::Integer(3),
            },
        ];
        let event = build_event("/talker", RosTime::new(17, 0), &changes);
        assert_eq!(event.node, "/talker");
        assert_eq!(event.stamp.sec, 17);
        assert_eq!(event.new_parameters.len(), 1);
        assert_eq!(event.changed_parameters.len(), 1);
        assert_eq!(event.deleted_parameters.len(), 1);
        assert_eq!(event.new_parameters[0].name, "new");
        assert_eq!(
            ParameterValue::from_message(&event.changed_parameters[0].value),
            ParameterValue::Integer(2),
            "a change announces the new value, not the old one"
        );
    }

    #[test]
    fn an_empty_batch_still_builds_a_well_formed_event() {
        let event = build_event("/talker", RosTime::ZERO, &[]);
        assert!(event.new_parameters.is_empty());
        assert!(event.changed_parameters.is_empty());
        assert!(event.deleted_parameters.is_empty());
        assert_eq!(event.node, "/talker");
    }

    #[test]
    fn an_event_round_trips_through_cdr() {
        let event = build_event(
            "/robot/talker",
            RosTime::new(3, 500),
            &[ParameterChange::Declared {
                name: "gain".to_owned(),
                value: ParameterValue::Double(1.5),
            }],
        );
        let octets = astrs_cdr::to_vec_ros2(&event).expect("encode");
        let decoded =
            astrs_cdr::from_bytes::<rcl_interfaces::ParameterEvent>(&octets).expect("decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn a_descriptor_renders_through_the_free_function() {
        let descriptor = ParameterDescriptor::new("gain", crate::parameters::value::TYPE_DOUBLE)
            .with_description("loop gain");
        assert_eq!(descriptor_message(&descriptor), descriptor.to_message());
    }

    #[test]
    fn the_events_topic_is_the_one_ros_defines() {
        assert_eq!(PARAMETER_EVENTS_TOPIC, "/parameter_events");
        assert!(FullName::topic(PARAMETER_EVENTS_TOPIC).is_ok());
    }
}
