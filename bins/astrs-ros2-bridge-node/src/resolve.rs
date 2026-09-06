//! `message_type: my_msgs/msg/Custom` → something that can actually carry it.
//!
//! Two sources, tried in order, and a typed error if neither has it.
//!
//! # 1. The pre-generated set
//!
//! §10.3 ships `common_interfaces` pre-generated, so bridging a standard
//! topic needs no ROS files on disk at all. [`crate::codec::registry`] is
//! that set, and a hit there gives a [`TypeBinding::Generated`] — the full
//! §10.5 promise: CDR ⇄ columnar through the generated type, header stamps
//! preserved.
//!
//! # 2. An ament tree, when one is configured
//!
//! A robot with its own `my_msgs` package has no pre-generated binding, and
//! failing there would make the bridge useless to exactly the deployments
//! that need it most. So when `AMENT_PREFIX_PATH` (or
//! `ASTRS_ROS2_INTERFACE_PATH`, for a package that is checked out rather
//! than installed) names a tree, `astrs-idl`'s discovery and parser are run
//! over it at startup: the package is located, the `.msg`/`.srv`/`.action`
//! file is parsed, its name is validated and its DDS type name and §10.3
//! URN are minted.
//!
//! A hit there gives a [`TypeBinding::Discovered`], and the sample crosses
//! the boundary as **opaque CDR** — one binary payload plus the type's
//! identity in metadata — rather than as columns.
//!
//! ## Why opaque, and why that is still worth having
//!
//! Turning a `.msg` into columns needs a Rust type; that is what
//! `astrs-idl`'s code generator is for, and it runs at *build* time. A
//! process that discovers a definition at *startup* has a schema but no
//! type, and the honest options are to refuse the topic or to carry the
//! octets with their schema named beside them. The second is strictly
//! better: the data flows, the recording (§14) captures it, a ROS-aware
//! consumer decodes it, and `astrs idl generate` upgrades it to columns
//! with a rebuild and no manifest change. Which mode a topic is in is
//! logged at startup and is visible in [`TypeBinding::is_columnar`].
//!
//! # 3. Neither
//!
//! [`ResolveError::UnknownType`], naming the type *and* saying whether
//! anything was searched. That is the difference between a user who edits
//! their manifest and one who sets one environment variable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use astrs_idl::discovery::{DiscoveredPackage, discover_ament_prefix, discover_source_tree};
use astrs_idl::naming::{InterfaceKind, TypeName, mint_urn};
use astrs_idl::span::{Position, Span};

use crate::codec::{MessageCodec, registry};
use crate::config::BridgeSettings;
use crate::error::ResolveError;
use crate::plan::BridgePlan;

/// A ROS 2 interface type found on an ament tree but not generated into
/// this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredType {
    /// The ROS 2 spelling, `pkg/msg/Type`.
    pub ros_type_name: String,
    /// The DDS spelling SEDP announces, `pkg::msg::dds_::Type_`.
    pub dds_type_name: String,
    /// The columnar URN §10.3 mints for it, `std/ros2/v1/PkgType`.
    pub urn: String,
    /// The file its definition was read from.
    pub source_path: PathBuf,
}

/// How one interface type will be carried.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeBinding {
    /// Pre-generated: full CDR ⇄ columnar conversion.
    Generated(MessageCodec),
    /// Discovered at startup: carried as opaque CDR octets.
    Discovered(DiscoveredType),
}

impl TypeBinding {
    /// The ROS 2 type name this binding carries.
    #[must_use]
    pub fn ros_type_name(&self) -> &str {
        match self {
            Self::Generated(codec) => codec.ros_type_name,
            Self::Discovered(found) => &found.ros_type_name,
        }
    }

    /// The DDS type name to announce on SEDP.
    #[must_use]
    pub fn dds_type_name(&self) -> &str {
        match self {
            Self::Generated(codec) => codec.dds_type_name,
            Self::Discovered(found) => &found.dds_type_name,
        }
    }

    /// The columnar type URN.
    #[must_use]
    pub fn urn(&self) -> &str {
        match self {
            Self::Generated(codec) => codec.urn,
            Self::Discovered(found) => &found.urn,
        }
    }

    /// Whether samples cross as columns rather than as opaque octets.
    #[must_use]
    pub const fn is_columnar(&self) -> bool {
        matches!(self, Self::Generated(_))
    }

    /// The codec, when this binding has one.
    #[must_use]
    pub const fn codec(&self) -> Option<&MessageCodec> {
        match self {
            Self::Generated(codec) => Some(codec),
            Self::Discovered(_) => None,
        }
    }
}

/// A plan whose every interface type has a binding.
#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    /// The plan itself.
    pub plan: BridgePlan,
    /// One binding per ROS 2 type name the plan needs, including the
    /// request/response and action endpoint types derived from a
    /// `service:`/`action:` block.
    pub bindings: BTreeMap<String, TypeBinding>,
}

impl ResolvedPlan {
    /// The binding for `ros_type_name`.
    ///
    /// Every type [`BridgePlan::required_types`] named — and every
    /// request/response/endpoint type derived from one — is present, so a
    /// miss here is a bug in this module rather than a configuration fault.
    #[must_use]
    pub fn binding(&self, ros_type_name: &str) -> Option<&TypeBinding> {
        self.bindings.get(ros_type_name)
    }

    /// How many of the bound types cross as columns.
    #[must_use]
    pub fn columnar_count(&self) -> usize {
        self.bindings
            .values()
            .filter(|binding| binding.is_columnar())
            .count()
    }
}

/// Bind every interface type a plan needs.
///
/// # Errors
///
/// [`ResolveError::MalformedName`] for a name that is not `pkg/kind/Type`,
/// [`ResolveError::UnknownType`] for one that is neither pre-generated nor
/// on the configured search path, [`ResolveError::SearchPath`] when a
/// configured path cannot be read, and [`ResolveError::Parse`] when a found
/// definition does not parse.
pub fn resolve_plan(
    plan: &BridgePlan,
    settings: &BridgeSettings,
) -> Result<ResolvedPlan, ResolveError> {
    let mut index = SearchIndex::new(settings)?;
    let mut bindings = BTreeMap::new();

    for topic in &plan.topics {
        bind(&mut bindings, &mut index, "message", &topic.message_type)?;
    }
    for service in &plan.services {
        for name in [
            registry::request_type_name(&service.service_type),
            registry::response_type_name(&service.service_type),
        ] {
            bind(&mut bindings, &mut index, "service", &name)?;
        }
    }
    for action in &plan.actions {
        for suffix in ACTION_WIRE_SUFFIXES {
            let name = registry::action_endpoint_type_name(&action.action_type, suffix);
            bind(&mut bindings, &mut index, "action", &name)?;
        }
        // Every action shares these three: the cancel service's two halves
        // and the status topic. They are always pre-generated, but binding
        // them through the same path keeps one lookup rule.
        for name in [
            registry::request_type_name(CANCEL_GOAL_SERVICE),
            registry::response_type_name(CANCEL_GOAL_SERVICE),
            GOAL_STATUS_ARRAY.to_owned(),
        ] {
            bind(&mut bindings, &mut index, "action", &name)?;
        }
    }

    Ok(ResolvedPlan {
        plan: plan.clone(),
        bindings,
    })
}

/// The five synthesized wire types every action has, by the suffix
/// `astrs-idl`'s `codegen/action.rs` appends.
pub const ACTION_WIRE_SUFFIXES: [&str; 5] = [
    "SendGoal_Request",
    "SendGoal_Response",
    "GetResult_Request",
    "GetResult_Response",
    "FeedbackMessage",
];

/// The service every action's `cancel_goal` endpoint is.
pub const CANCEL_GOAL_SERVICE: &str = "action_msgs/srv/CancelGoal";

/// The message every action's `status` topic carries.
pub const GOAL_STATUS_ARRAY: &str = "action_msgs/msg/GoalStatusArray";

/// Bind one type name, if it is not bound already.
fn bind(
    bindings: &mut BTreeMap<String, TypeBinding>,
    index: &mut SearchIndex,
    kind: &'static str,
    ros_type_name: &str,
) -> Result<(), ResolveError> {
    if bindings.contains_key(ros_type_name) {
        return Ok(());
    }
    let binding = resolve_type(index, kind, ros_type_name)?;
    bindings.insert(ros_type_name.to_owned(), binding);
    Ok(())
}

/// Bind one type name.
///
/// # Errors
///
/// As [`resolve_plan`].
pub fn resolve_type(
    index: &mut SearchIndex,
    kind: &'static str,
    ros_type_name: &str,
) -> Result<TypeBinding, ResolveError> {
    if let Some(codec) = registry::lookup(ros_type_name) {
        return Ok(TypeBinding::Generated(codec));
    }
    if let Some(found) = index.find(ros_type_name)? {
        return Ok(TypeBinding::Discovered(found));
    }
    Err(ResolveError::UnknownType {
        kind,
        type_name: ros_type_name.to_owned(),
        search: index.describe(),
    })
}

/// The interface packages on the configured search path.
///
/// Built once at startup — walking an ament prefix is a directory tree
/// crawl, and a bridge with twenty topics would otherwise do it twenty
/// times.
#[derive(Debug)]
pub struct SearchIndex {
    /// Every discovered package, by name.
    packages: BTreeMap<String, DiscoveredPackage>,
    /// How the search path renders in a diagnostic.
    description: String,
}

impl SearchIndex {
    /// Crawl every configured prefix and source tree.
    ///
    /// # Errors
    ///
    /// [`ResolveError::SearchPath`] when a configured entry exists but
    /// cannot be read. A *non-existent* entry is skipped rather than fatal:
    /// `AMENT_PREFIX_PATH` routinely names prefixes that a given machine
    /// does not have, and refusing to start over one would make the
    /// variable unusable.
    pub fn new(settings: &BridgeSettings) -> Result<Self, ResolveError> {
        let mut packages = BTreeMap::new();

        for prefix in &settings.ament_prefixes {
            for package in crawl(prefix, discover_ament_prefix)? {
                packages.insert(package.name.as_str().to_owned(), package);
            }
        }
        for tree in &settings.source_trees {
            for package in crawl(tree, discover_source_tree)? {
                packages.insert(package.name.as_str().to_owned(), package);
            }
        }

        Ok(Self {
            packages,
            description: settings.describe_search_path(),
        })
    }

    /// An index that searches nothing — what a bridge with no configured
    /// path has, and what a unit test wants.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            packages: BTreeMap::new(),
            description: BridgeSettings::default().describe_search_path(),
        }
    }

    /// How many packages were found.
    #[must_use]
    pub fn len(&self) -> usize {
        self.packages.len()
    }

    /// Whether nothing at all was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// The search path, rendered for a diagnostic.
    #[must_use]
    pub fn describe(&self) -> String {
        self.description.clone()
    }

    /// Find `pkg/kind/Type` on the search path.
    ///
    /// # Errors
    ///
    /// [`ResolveError::MalformedName`] when the name is not three segments,
    /// and [`ResolveError::Parse`] when the file exists but does not parse.
    pub fn find(&self, ros_type_name: &str) -> Result<Option<DiscoveredType>, ResolveError> {
        let (package_name, kind, type_name) = split_type_name(ros_type_name)?;

        // A service's `Foo_Request` and an action's `Foo_SendGoal_Request`
        // are *derived* names: what exists on disk is `Foo.srv` /
        // `Foo.action`. Strip the suffix back off before looking.
        let (file_stem, suffix) = split_derived(type_name, kind);

        let Some(package) = self.packages.get(package_name) else {
            return Ok(None);
        };
        let Some(file) = package
            .interfaces
            .iter()
            .find(|file| file.kind == kind && file.type_name == file_stem)
        else {
            return Ok(None);
        };

        // Parse it, so a definition that cannot be read is reported at
        // startup rather than as a mysterious decode failure later.
        let source = std::fs::read_to_string(&file.path).map_err(|source| ResolveError::Parse {
            type_name: ros_type_name.to_owned(),
            path: file.path.display().to_string(),
            reason: source.to_string(),
        })?;
        parse_for_kind(kind, &source).map_err(|reason| ResolveError::Parse {
            type_name: ros_type_name.to_owned(),
            path: file.path.display().to_string(),
            reason,
        })?;

        // Only the *stem* is a legal `TypeName` — `^[A-Z][A-Za-z0-9]*$`, so
        // `DoThing_Request` is not one. Validating the stem and re-attaching
        // the suffix is what `astrs-idl`'s own code generator does, and it
        // is why the two spellings differ: the DDS name keeps the
        // underscores (`my_msgs::srv::dds_::DoThing_Request_`) and the URN
        // drops them (`std/ros2/v1/MyMsgsDoThingRequest`).
        let checked = TypeName::new(
            package.name.clone(),
            kind,
            file_stem,
            Span::empty(Position::START),
        )
        .map_err(|error| ResolveError::MalformedName {
            type_name: format!("{ros_type_name} ({error})"),
            kind: kind.as_str(),
        })?;
        let urn_name = format!("{file_stem}{}", suffix.unwrap_or("").replace('_', ""));
        let urn =
            mint_urn(&package.name, &urn_name, Span::empty(Position::START)).map_err(|error| {
                ResolveError::MalformedName {
                    type_name: format!("{ros_type_name} ({error})"),
                    kind: kind.as_str(),
                }
            })?;

        Ok(Some(DiscoveredType {
            ros_type_name: ros_type_name.to_owned(),
            dds_type_name: dds_type_name(&checked, suffix.unwrap_or("")),
            urn: urn.to_string(),
            source_path: file.path.clone(),
        }))
    }
}

/// The `rosidl` DDS spelling of a (possibly derived) type name.
///
/// `astrs-idl`'s own `naming::dds_type_name` takes no suffix, because a
/// generated *file* always names a plain type; a service's request half is
/// `pkg::srv::dds_::Service_Request_`, which is that function's output with
/// the suffix spliced in before the trailing underscore.
/// [`astrs_ros2::names::mangle::dds_type_name`] is the form that takes one.
fn dds_type_name(type_name: &TypeName, suffix: &str) -> String {
    let namespace = match type_name.kind {
        InterfaceKind::Msg => astrs_ros2::names::mangle::TypeNamespace::Msg,
        InterfaceKind::Srv => astrs_ros2::names::mangle::TypeNamespace::Srv,
        InterfaceKind::Action => astrs_ros2::names::mangle::TypeNamespace::Action,
    };
    astrs_ros2::names::mangle::dds_type_name(
        type_name.package.as_str(),
        namespace,
        &type_name.name,
        suffix,
    )
}

/// Crawl one configured entry, skipping one that is not there.
fn crawl(
    path: &Path,
    walk: fn(&Path) -> Result<Vec<DiscoveredPackage>, astrs_idl::error::IdlError>,
) -> Result<Vec<DiscoveredPackage>, ResolveError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    walk(path).map_err(|error| ResolveError::SearchPath {
        path: path.display().to_string(),
        reason: error.to_string(),
    })
}

/// Split `pkg/kind/Type` into its three parts.
fn split_type_name(ros_type_name: &str) -> Result<(&str, InterfaceKind, &str), ResolveError> {
    let mut parts = ros_type_name.split('/');
    let (Some(package), Some(kind), Some(type_name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ResolveError::MalformedName {
            type_name: ros_type_name.to_owned(),
            kind: "msg",
        });
    };
    let kind = match kind {
        "msg" => InterfaceKind::Msg,
        "srv" => InterfaceKind::Srv,
        "action" => InterfaceKind::Action,
        _ => {
            return Err(ResolveError::MalformedName {
                type_name: ros_type_name.to_owned(),
                kind: "msg",
            });
        }
    };
    if package.is_empty() || type_name.is_empty() {
        return Err(ResolveError::MalformedName {
            type_name: ros_type_name.to_owned(),
            kind: kind.as_str(),
        });
    }
    Ok((package, kind, type_name))
}

/// Strip a derived suffix (`_Request`, `_SendGoal_Response`, …) back off a
/// type name, leaving the stem the file on disk is called.
fn split_derived(type_name: &str, kind: InterfaceKind) -> (&str, Option<&str>) {
    if kind == InterfaceKind::Msg {
        return (type_name, None);
    }
    for suffix in DERIVED_SUFFIXES {
        if let Some(stem) = type_name.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return (stem, Some(suffix));
        }
    }
    (type_name, None)
}

/// Every suffix `astrs-idl`'s service and action code generators append,
/// longest first so `_SendGoal_Request` wins over `_Request`.
const DERIVED_SUFFIXES: [&str; 8] = [
    "_SendGoal_Request",
    "_SendGoal_Response",
    "_GetResult_Request",
    "_GetResult_Response",
    "_FeedbackMessage",
    "_Feedback",
    "_Request",
    "_Response",
];

/// Parse a definition with the parser its kind calls for, reporting the
/// complaint as a string.
fn parse_for_kind(kind: InterfaceKind, source: &str) -> Result<(), String> {
    let outcome = match kind {
        InterfaceKind::Msg => astrs_idl::parser::parse_message(source).map(|_| ()),
        InterfaceKind::Srv => astrs_idl::parser::parse_service(source).map(|_| ()),
        InterfaceKind::Action => astrs_idl::parser::parse_action(source).map(|_| ()),
    };
    outcome.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::fs;

    use super::*;

    fn temp_tree(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let dir = std::env::temp_dir().join(format!(
            "astrs-ros2-bridge-resolve-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A minimal source-tree package: `package.xml` plus one `.msg`.
    fn write_package(root: &Path, package: &str, files: &[(&str, &str, &str)]) {
        let dir = root.join(package);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.xml"),
            format!("<package format=\"3\"><name>{package}</name></package>"),
        )
        .unwrap();
        for (kind, name, body) in files {
            let sub = dir.join(kind);
            fs::create_dir_all(&sub).unwrap();
            let extension = if *kind == "msg" { "msg" } else { *kind };
            fs::write(sub.join(format!("{name}.{extension}")), body).unwrap();
        }
    }

    fn settings_for(tree: &Path) -> BridgeSettings {
        BridgeSettings {
            source_trees: vec![tree.to_path_buf()],
            ..BridgeSettings::default()
        }
    }

    #[test]
    fn a_pre_generated_type_binds_to_a_columnar_codec() {
        let mut index = SearchIndex::empty();
        let binding = resolve_type(&mut index, "message", "sensor_msgs/msg/LaserScan").unwrap();
        assert!(binding.is_columnar());
        assert_eq!(binding.ros_type_name(), "sensor_msgs/msg/LaserScan");
        assert_eq!(
            binding.dds_type_name(),
            "sensor_msgs::msg::dds_::LaserScan_"
        );
        assert!(binding.codec().is_some());
    }

    #[test]
    fn an_unknown_type_with_no_search_path_says_none_is_configured() {
        let mut index = SearchIndex::empty();
        let error = resolve_type(&mut index, "message", "my_msgs/msg/Custom").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("my_msgs/msg/Custom"), "{text}");
        assert!(text.contains("no interface search path"), "{text}");
        assert!(text.contains("AMENT_PREFIX_PATH"), "{text}");
    }

    #[test]
    fn a_malformed_name_is_refused_before_any_search() {
        let mut index = SearchIndex::empty();
        for bad in [
            "Custom",
            "my_msgs/Custom",
            "my_msgs/bogus/Custom",
            "a/b/c/d",
        ] {
            let error = resolve_type(&mut index, "message", bad).unwrap_err();
            assert!(
                matches!(error, ResolveError::MalformedName { .. }),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn a_type_on_a_source_tree_binds_as_opaque_cdr() {
        let tree = temp_tree("found");
        write_package(
            &tree,
            "my_msgs",
            &[("msg", "Custom", "int32 value\nstring label\n")],
        );

        let mut index = SearchIndex::new(&settings_for(&tree)).unwrap();
        assert_eq!(index.len(), 1);

        let binding = resolve_type(&mut index, "message", "my_msgs/msg/Custom").unwrap();
        assert!(!binding.is_columnar(), "a discovered type has no codec");
        assert_eq!(binding.dds_type_name(), "my_msgs::msg::dds_::Custom_");
        assert_eq!(binding.urn(), "std/ros2/v1/MyMsgsCustom");
        assert!(binding.codec().is_none());

        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_type_the_tree_does_not_have_names_the_paths_that_were_searched() {
        let tree = temp_tree("missing");
        write_package(&tree, "my_msgs", &[("msg", "Custom", "int32 value\n")]);

        let mut index = SearchIndex::new(&settings_for(&tree)).unwrap();
        let error = resolve_type(&mut index, "message", "my_msgs/msg/Absent").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("my_msgs/msg/Absent"), "{text}");
        assert!(text.contains(&tree.display().to_string()), "{text}");

        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_definition_that_does_not_parse_is_reported_with_its_path() {
        let tree = temp_tree("broken");
        write_package(
            &tree,
            "my_msgs",
            &[("msg", "Broken", "not a field line!!\n")],
        );

        let mut index = SearchIndex::new(&settings_for(&tree)).unwrap();
        let error = resolve_type(&mut index, "message", "my_msgs/msg/Broken").unwrap_err();
        match error {
            ResolveError::Parse {
                type_name, path, ..
            } => {
                assert_eq!(type_name, "my_msgs/msg/Broken");
                assert!(path.contains("Broken.msg"), "{path}");
            }
            other => panic!("expected Parse, got {other}"),
        }

        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_services_derived_request_name_finds_the_srv_file() {
        let tree = temp_tree("srv");
        write_package(
            &tree,
            "my_msgs",
            &[("srv", "DoThing", "int32 request_value\n---\nbool ok\n")],
        );

        let mut index = SearchIndex::new(&settings_for(&tree)).unwrap();
        let binding = resolve_type(&mut index, "service", "my_msgs/srv/DoThing_Request").unwrap();
        assert_eq!(
            binding.dds_type_name(),
            "my_msgs::srv::dds_::DoThing_Request_"
        );

        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn a_missing_search_path_entry_is_skipped_rather_than_fatal() {
        let settings = BridgeSettings {
            ament_prefixes: vec![PathBuf::from("/definitely/not/here")],
            ..BridgeSettings::default()
        };
        let index = SearchIndex::new(&settings).unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn a_whole_plan_binds_every_type_it_needs() {
        use astrs_manifest::Ros2Config;
        use astrs_wire::{
            DataflowId, InputSpec, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef,
        };

        let config: Ros2Config = astrs_yaml::from_str(
            "\
compat: humble
topics:
  - topic: /scan
    message_type: sensor_msgs/msg/LaserScan
    direction: to_astrs
  - topic: /cmd_vel
    message_type: geometry_msgs/msg/Twist
    direction: from_astrs
",
        )
        .unwrap();
        let mut spec = NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("bridge").unwrap(),
            0,
            NodeSource::Ros2Bridge {
                config: "{}".into(),
            },
        );
        spec = spec.with_output(OutputSpec::new(astrs_wire::DataId::new("scan").unwrap()));
        spec = spec.with_input(InputSpec::new(
            astrs_wire::DataId::new("cmd_vel").unwrap(),
            PortRef::from_parts("planner", "cmd").unwrap(),
        ));

        let plan = crate::plan::plan(&config, &spec).unwrap();
        let resolved = resolve_plan(&plan, &BridgeSettings::default()).unwrap();

        assert_eq!(resolved.bindings.len(), 2);
        assert_eq!(resolved.columnar_count(), 2);
        assert!(
            resolved
                .binding("sensor_msgs/msg/LaserScan")
                .is_some_and(TypeBinding::is_columnar)
        );
    }

    #[test]
    fn an_action_plan_binds_the_five_wire_types_and_the_three_shared_ones() {
        use astrs_manifest::Ros2Config;
        use astrs_wire::{
            DataflowId, InputSpec, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef,
        };

        let config: Ros2Config = astrs_yaml::from_str(
            "\
compat: humble
action: /fibonacci
message_type: example_interfaces/action/Fibonacci
role: server
",
        )
        .unwrap();
        let mut spec = NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("bridge").unwrap(),
            0,
            NodeSource::Ros2Bridge {
                config: "{}".into(),
            },
        );
        spec = spec.with_output(OutputSpec::new(astrs_wire::DataId::new("goal").unwrap()));
        spec = spec.with_input(InputSpec::new(
            astrs_wire::DataId::new("result").unwrap(),
            PortRef::from_parts("solver", "out").unwrap(),
        ));

        let plan = crate::plan::plan(&config, &spec).unwrap();
        let resolved = resolve_plan(&plan, &BridgeSettings::default()).unwrap();

        for suffix in ACTION_WIRE_SUFFIXES {
            let name =
                registry::action_endpoint_type_name("example_interfaces/action/Fibonacci", suffix);
            assert!(resolved.binding(&name).is_some(), "{name}");
        }
        assert!(resolved.binding(GOAL_STATUS_ARRAY).is_some());
        assert!(
            resolved
                .binding(&registry::request_type_name(CANCEL_GOAL_SERVICE))
                .is_some()
        );
        assert_eq!(resolved.columnar_count(), resolved.bindings.len());
    }

    #[test]
    fn the_derived_suffix_stripper_prefers_the_longest_match() {
        assert_eq!(
            split_derived("Fibonacci_SendGoal_Request", InterfaceKind::Action),
            ("Fibonacci", Some("_SendGoal_Request"))
        );
        assert_eq!(
            split_derived("DoThing_Request", InterfaceKind::Srv),
            ("DoThing", Some("_Request"))
        );
        assert_eq!(
            split_derived("Custom_Request", InterfaceKind::Msg),
            ("Custom_Request", None),
            "a plain message keeps its whole name"
        );
    }
}
