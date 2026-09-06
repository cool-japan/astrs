//! Turning what an application wrote into a fully-qualified name.
//!
//! Four forms go in and one comes out:
//!
//! | Written | Called | Expands to |
//! |---|---|---|
//! | `/scan` | absolute | itself |
//! | `scan` | relative | `<namespace>/scan` |
//! | `~/scan` | private | `<namespace>/<node>/scan` |
//! | `{node}/scan` | substituted | `<namespace>/<node>/scan` |
//!
//! Two substitutions are defined: `{node}` for the node's name and `{ns}`
//! (spelled `{namespace}` as well) for its namespace. `{ns}` expands to the
//! namespace *including* its leading slash, so `{ns}/scan` under `/robot`
//! is `/robot/scan` and not `//robot/scan` — the join is where that
//! collapse happens, not the substitution.
//!
//! # Remapping
//!
//! [`RemapRules`] is the `--ros-args -r from:=to` table. Rules apply to the
//! *expanded* name, in declaration order, first match wins, and the
//! replacement is itself expanded — so `-r ~/scan:=/lidar/scan` and
//! `-r scan:=/lidar/scan` behave the same way for a node in `/`. A rule
//! whose replacement is relative is expanded against the same node, which is
//! what makes `-r scan:=filtered_scan` do the obvious thing.
//!
//! Node-name remapping (`__node:=`) and namespace remapping (`__ns:=`) are
//! separate fields rather than entries in the table, because they change
//! what every *other* rule expands against and so cannot be applied in the
//! same pass.

use crate::error::NameFault;
use crate::names::validate::{ROOT_NAMESPACE, validate_unexpanded_name};

/// The substitution spelling for a node's own name.
pub const SUBSTITUTION_NODE: &str = "node";

/// The substitution spelling for a node's namespace, short form.
pub const SUBSTITUTION_NS: &str = "ns";

/// The substitution spelling for a node's namespace, long form.
pub const SUBSTITUTION_NAMESPACE: &str = "namespace";

/// Join a namespace and a relative name into an absolute one.
///
/// The root namespace `/` joins without doubling the slash, which is the one
/// case a naive `format!("{namespace}/{name}")` gets wrong.
#[must_use]
pub fn join(namespace: &str, name: &str) -> String {
    let name = name.strip_prefix('/').unwrap_or(name);
    if namespace == ROOT_NAMESPACE || namespace.is_empty() {
        format!("/{name}")
    } else if name.is_empty() {
        namespace.to_owned()
    } else {
        format!("{namespace}/{name}")
    }
}

/// Expand `name` against a node's identity.
///
/// # Errors
///
/// Whatever [`validate_unexpanded_name`] reports, plus
/// [`NameFault::MalformedSubstitution`] for a substitution this crate does
/// not define.
pub fn expand(name: &str, node_name: &str, namespace: &str) -> Result<String, NameFault> {
    validate_unexpanded_name(name)?;

    // `~` and `~/rest` both mean "under this node's private namespace".
    let expanded = if let Some(rest) = name.strip_prefix('~') {
        let private = join(namespace, node_name);
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            private
        } else {
            format!("{private}/{rest}")
        }
    } else if name.starts_with('/') {
        name.to_owned()
    } else {
        join(namespace, name)
    };

    let substituted = substitute(&expanded, node_name, namespace)?;
    Ok(collapse(&substituted))
}

/// Replace every `{substitution}` in an already-absolute name.
///
/// # Errors
///
/// [`NameFault::MalformedSubstitution`] naming the offset of an unknown
/// substitution.
pub fn substitute(name: &str, node_name: &str, namespace: &str) -> Result<String, NameFault> {
    if !name.contains('{') {
        return Ok(name.to_owned());
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    let mut consumed = 0_usize;
    while let Some(start) = rest.find('{') {
        let (before, from_brace) = rest.split_at(start);
        out.push_str(before);
        let end = from_brace
            .find('}')
            .ok_or(NameFault::MalformedSubstitution {
                offset: consumed.saturating_add(start),
            })?;
        let key = from_brace.get(1..end).unwrap_or_default();
        let replacement = match key {
            SUBSTITUTION_NODE => node_name.to_owned(),
            SUBSTITUTION_NS | SUBSTITUTION_NAMESPACE => namespace.to_owned(),
            _ => {
                return Err(NameFault::MalformedSubstitution {
                    offset: consumed.saturating_add(start),
                });
            }
        };
        out.push_str(&replacement);
        rest = from_brace.get(end.saturating_add(1)..).unwrap_or_default();
        consumed = consumed
            .saturating_add(start)
            .saturating_add(end)
            .saturating_add(1);
    }
    out.push_str(rest);
    Ok(out)
}

/// Collapse `//` runs and drop a trailing `/` from a name longer than one
/// character.
///
/// Only ever reached through a substitution: `{ns}/scan` under the root
/// namespace produces `//scan` before this runs, and `{ns}` alone under
/// `/robot` produces `/robot/` when it was the last token.
#[must_use]
pub fn collapse(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous_was_slash = false;
    for character in name.chars() {
        if character == '/' {
            if previous_was_slash {
                continue;
            }
            previous_was_slash = true;
        } else {
            previous_was_slash = false;
        }
        out.push(character);
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// One `from:=to` remapping rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemapRule {
    /// The name to match, as written (may be relative or private).
    pub from: String,
    /// The name to substitute, as written (may be relative or private).
    pub to: String,
}

impl RemapRule {
    /// Build a rule, checking both halves.
    ///
    /// # Errors
    ///
    /// Whatever [`validate_unexpanded_name`] reports for either half.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Result<Self, NameFault> {
        let from = from.into();
        let to = to.into();
        validate_unexpanded_name(&from)?;
        validate_unexpanded_name(&to)?;
        Ok(Self { from, to })
    }

    /// Parse the `from:=to` spelling `--ros-args -r` uses.
    ///
    /// # Errors
    ///
    /// [`NameFault::Empty`] when there is no `:=`, plus whatever
    /// [`new`](Self::new) reports.
    pub fn parse(rule: &str) -> Result<Self, NameFault> {
        let (from, to) = rule.split_once(":=").ok_or(NameFault::Empty)?;
        Self::new(from.trim(), to.trim())
    }
}

/// The remapping table a node was launched with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemapRules {
    rules: Vec<RemapRule>,
}

impl RemapRules {
    /// An empty table: every name expands to itself.
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a rule, keeping declaration order.
    #[must_use]
    pub fn with(mut self, rule: RemapRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Add a rule in place.
    pub fn push(&mut self, rule: RemapRule) {
        self.rules.push(rule);
    }

    /// How many rules there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when no rule has been declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Every rule, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &RemapRule> {
        self.rules.iter()
    }

    /// Expand `name`, then apply the first rule whose `from` expands to the
    /// same thing.
    ///
    /// The comparison is between *expanded* names, so a rule written
    /// `scan:=/lidar/scan` matches a call site that wrote `~/scan` when both
    /// resolve to the same absolute name — which is what makes remapping
    /// composable with private names at all.
    ///
    /// # Errors
    ///
    /// Whatever [`expand`] reports, for the name or for either half of a
    /// rule.
    pub fn apply(&self, name: &str, node_name: &str, namespace: &str) -> Result<String, NameFault> {
        let expanded = expand(name, node_name, namespace)?;
        for rule in &self.rules {
            if expand(&rule.from, node_name, namespace)? == expanded {
                return expand(&rule.to, node_name, namespace);
            }
        }
        Ok(expanded)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn joining_the_root_namespace_does_not_double_the_slash() {
        assert_eq!(join("/", "scan"), "/scan");
        assert_eq!(join("/", "/scan"), "/scan");
        assert_eq!(join("/robot", "scan"), "/robot/scan");
        assert_eq!(join("/robot", "/scan"), "/robot/scan");
        assert_eq!(join("/robot", ""), "/robot");
        assert_eq!(join("", "scan"), "/scan");
    }

    #[test]
    fn an_absolute_name_expands_to_itself() {
        assert_eq!(expand("/scan", "talker", "/robot").unwrap(), "/scan");
    }

    #[test]
    fn a_relative_name_takes_the_namespace() {
        assert_eq!(expand("scan", "talker", "/").unwrap(), "/scan");
        assert_eq!(expand("scan", "talker", "/robot").unwrap(), "/robot/scan");
        assert_eq!(
            expand("sensors/scan", "talker", "/robot").unwrap(),
            "/robot/sensors/scan"
        );
    }

    #[test]
    fn a_private_name_takes_the_namespace_and_the_node() {
        assert_eq!(expand("~/scan", "talker", "/").unwrap(), "/talker/scan");
        assert_eq!(
            expand("~/scan", "talker", "/robot").unwrap(),
            "/robot/talker/scan"
        );
        assert_eq!(expand("~", "talker", "/robot").unwrap(), "/robot/talker");
    }

    #[test]
    fn substitutions_resolve_to_the_node_and_the_namespace() {
        assert_eq!(
            expand("{node}/scan", "talker", "/robot").unwrap(),
            "/robot/talker/scan"
        );
        assert_eq!(
            expand("/{ns}/scan", "talker", "/robot").unwrap(),
            "/robot/scan",
            "the namespace carries its own leading slash; the join collapses the double"
        );
        assert_eq!(
            expand("/{namespace}/scan", "talker", "/robot").unwrap(),
            "/robot/scan"
        );
        assert_eq!(
            expand("/{ns}/scan", "talker", "/").unwrap(),
            "/scan",
            "the root namespace collapses to nothing at all"
        );
    }

    #[test]
    fn an_unknown_substitution_is_rejected_with_its_offset() {
        assert_eq!(
            expand("/{robot}/scan", "talker", "/"),
            Err(NameFault::MalformedSubstitution { offset: 1 })
        );
    }

    #[test]
    fn collapsing_is_idempotent_and_never_empties_a_name() {
        assert_eq!(collapse("//scan"), "/scan");
        assert_eq!(collapse("/a///b//c"), "/a/b/c");
        assert_eq!(collapse("/robot/"), "/robot");
        assert_eq!(collapse("/"), "/");
        assert_eq!(collapse(""), "/");
        assert_eq!(collapse(&collapse("/a//b/")), collapse("/a//b/"));
    }

    #[test]
    fn a_rule_parses_from_the_launch_spelling() {
        let rule = RemapRule::parse("scan:=/lidar/scan").unwrap();
        assert_eq!(rule.from, "scan");
        assert_eq!(rule.to, "/lidar/scan");
        assert_eq!(RemapRule::parse("no assignment"), Err(NameFault::Empty));
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let rules = RemapRules::new()
            .with(RemapRule::new("scan", "/lidar/scan").unwrap())
            .with(RemapRule::new("scan", "/never").unwrap());
        assert_eq!(rules.len(), 2);
        assert!(!rules.is_empty());
        assert_eq!(rules.apply("scan", "talker", "/").unwrap(), "/lidar/scan");
    }

    #[test]
    fn a_rule_matches_after_expansion_not_before() {
        let rules = RemapRules::new().with(RemapRule::new("~/scan", "/lidar/scan").unwrap());
        assert_eq!(
            rules.apply("/talker/scan", "talker", "/").unwrap(),
            "/lidar/scan",
            "the call site wrote the absolute form of what the rule wrote privately"
        );
    }

    #[test]
    fn a_relative_replacement_is_expanded_too() {
        let rules = RemapRules::new().with(RemapRule::new("scan", "filtered_scan").unwrap());
        assert_eq!(
            rules.apply("scan", "talker", "/robot").unwrap(),
            "/robot/filtered_scan"
        );
    }

    #[test]
    fn an_empty_table_is_the_identity() {
        let rules = RemapRules::new();
        assert!(rules.is_empty());
        assert_eq!(rules.iter().count(), 0);
        assert_eq!(
            rules.apply("scan", "talker", "/robot").unwrap(),
            "/robot/scan"
        );
    }

    #[test]
    fn pushing_and_building_agree() {
        let mut pushed = RemapRules::new();
        pushed.push(RemapRule::new("a", "b").unwrap());
        let built = RemapRules::new().with(RemapRule::new("a", "b").unwrap());
        assert_eq!(pushed, built);
    }
}
