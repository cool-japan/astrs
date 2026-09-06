//! Environment hygiene at spawn — blueprint §16, exactly.
//!
//! > *Inherited env scrubbed to an allowlist; manifest `env:` filtered against
//! > a denylist (`ASTRS_*` internals, `LD_PRELOAD`, `DYLD_*`); daemon-owned
//! > vars applied last so a manifest can never override the handshake.*
//!
//! Three filters, in one fixed order, and the order is the security property:
//!
//! ```text
//!   daemon's own environment
//!            │  keep only DEFAULT_ENV_ALLOWLIST + explicit passthrough
//!            ▼
//!      scrubbed base ──────────────────┐
//!            │                         │ used as the $VAR lookup source
//!            │                         │ (a manifest cannot read what the
//!            │                         │  scrub already removed)
//!            ▼                         │
//!   manifest env: (expanded) ◄─────────┘
//!            │  drop every denied name, recording why
//!            ▼
//!      merged environment
//!            │  daemon-owned variables, unconditionally
//!            ▼
//!      the child's environment
//! ```
//!
//! Two consequences worth stating out loud, because both are tested below:
//!
//! - a manifest cannot smuggle a value *in* — `LD_PRELOAD: ./evil.so` never
//!   reaches the child, and neither does anything named `ASTRS_*`;
//! - a manifest cannot read a value *out* — `LEAK: $AWS_SECRET_ACCESS_KEY`
//!   fails to expand, because the scrub removed the name before expansion
//!   ever looked at it.
//!
//! The child's environment is built as a complete map and applied with
//! `env_clear` + `envs` (see [`crate::spawn::process`]), so nothing survives
//! by accident: a variable reaches the node because this module put it there
//! or not at all.
//!
//! # Examples
//!
//! ```
//! use std::collections::BTreeMap;
//! use astrs_daemon::spawn::{EnvPolicy, DenyReason};
//! use astrs_manifest::EnvValue;
//!
//! let policy = EnvPolicy::default();
//! let inherited = BTreeMap::from([
//!     ("PATH".to_string(), "/usr/bin".to_string()),
//!     ("AWS_SECRET_ACCESS_KEY".to_string(), "hunter2".to_string()),
//! ]);
//! let manifest = BTreeMap::from([
//!     ("CAMERA_INDEX".to_string(), EnvValue::Int(0)),
//!     ("LD_PRELOAD".to_string(), EnvValue::String("./evil.so".into())),
//! ]);
//!
//! let built = policy.build(&inherited, &manifest)?;
//! assert_eq!(built.get("PATH"), Some("/usr/bin"));
//! assert_eq!(built.get("CAMERA_INDEX"), Some("0"));
//! assert!(built.get("AWS_SECRET_ACCESS_KEY").is_none(), "scrubbed");
//! assert!(built.get("LD_PRELOAD").is_none(), "denied");
//! assert_eq!(built.denied()[0].reason, DenyReason::LoaderInjection);
//! # Ok::<(), astrs_daemon::spawn::EnvError>(())
//! ```

use std::collections::BTreeMap;

use astrs_manifest::{EnvExpandError, EnvValue};

use crate::config::DEFAULT_ENV_ALLOWLIST;

/// Variable-name prefixes a manifest may never set.
///
/// `ASTRS_` covers every daemon-owned variable at once, present and future,
/// which is why the handshake blob's name never needs its own rule.
pub const DENIED_PREFIXES: &[&str] = &["ASTRS_", "DYLD_"];

/// Exact variable names a manifest may never set.
///
/// The dynamic-loader injection points. `LD_LIBRARY_PATH` is deliberately
/// *not* here: it is how a legitimate node finds a vendored library, and
/// unlike `LD_PRELOAD` it cannot substitute a symbol in an already-resolved
/// binary.
pub const DENIED_NAMES: &[&str] = &["LD_PRELOAD", "LD_AUDIT", "LD_PROFILE"];

/// Why a manifest variable was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DenyReason {
    /// The name is in the daemon's own `ASTRS_*` namespace.
    ///
    /// Letting a manifest set one would let it forge the handshake blob.
    DaemonNamespace,
    /// The name is a dynamic-loader injection point (`LD_PRELOAD`, `DYLD_*`).
    LoaderInjection,
    /// The name is not a usable environment variable name at all — empty, or
    /// containing `=` or a NUL.
    Malformed,
}

impl DenyReason {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DaemonNamespace => "daemon_namespace",
            Self::LoaderInjection => "loader_injection",
            Self::Malformed => "malformed",
        }
    }
}

impl core::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::DaemonNamespace => "reserved for the daemon",
            Self::LoaderInjection => "a dynamic-loader injection point",
            Self::Malformed => "not a usable variable name",
        })
    }
}

/// One manifest variable that did not reach the child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeniedVar {
    /// The name the manifest used.
    pub name: String,
    /// Why it was dropped.
    pub reason: DenyReason,
}

impl core::fmt::Display for DeniedVar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.name, self.reason)
    }
}

/// Why an environment could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvError {
    /// A `$VAR` reference could not be resolved against the scrubbed base.
    #[error("environment variable `{key}`: {source}")]
    Expand {
        /// The manifest key whose value failed.
        key: String,
        /// What went wrong.
        #[source]
        source: EnvExpandError,
    },
}

/// Whether a manifest may set `name`, and if not, why not.
///
/// # Examples
///
/// ```
/// use astrs_daemon::spawn::{DenyReason, deny_reason};
///
/// assert_eq!(deny_reason("RUST_LOG"), None);
/// assert_eq!(deny_reason("ASTRS_NODE_CONFIG"), Some(DenyReason::DaemonNamespace));
/// assert_eq!(deny_reason("DYLD_INSERT_LIBRARIES"), Some(DenyReason::LoaderInjection));
/// assert_eq!(deny_reason("LD_PRELOAD"), Some(DenyReason::LoaderInjection));
/// assert_eq!(deny_reason("A=B"), Some(DenyReason::Malformed));
/// ```
#[must_use]
pub fn deny_reason(name: &str) -> Option<DenyReason> {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return Some(DenyReason::Malformed);
    }
    if name.starts_with("ASTRS_") {
        return Some(DenyReason::DaemonNamespace);
    }
    if name.starts_with("DYLD_") {
        return Some(DenyReason::LoaderInjection);
    }
    if DENIED_NAMES.contains(&name) {
        return Some(DenyReason::LoaderInjection);
    }
    None
}

/// The §16 environment policy: what a spawned node inherits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// The always-inherited names (§16: `PATH`, `HOME`, `USER`, `TMPDIR`,
    /// `RUST_LOG`).
    allowlist: Vec<String>,
    /// Extra names an operator explicitly passes through.
    passthrough: Vec<String>,
}

impl EnvPolicy {
    /// The blueprint allowlist with no extra passthrough.
    #[must_use]
    pub fn new() -> Self {
        Self {
            allowlist: DEFAULT_ENV_ALLOWLIST
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            passthrough: Vec::new(),
        }
    }

    /// The blueprint allowlist plus `names`.
    #[must_use]
    pub fn with_passthrough<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.passthrough.extend(names.into_iter().map(Into::into));
        self.passthrough.sort_unstable();
        self.passthrough.dedup();
        self
    }

    /// Replaces the allowlist wholesale.
    ///
    /// For a caller that has a reason to depart from §16 — a hermetic build
    /// runner that wants *nothing* inherited, say. Departing from the default
    /// is deliberate and visible at the call site, which is the point.
    #[must_use]
    pub fn with_allowlist<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowlist = names.into_iter().map(Into::into).collect();
        self.allowlist.sort_unstable();
        self.allowlist.dedup();
        self
    }

    /// The names inherited from the daemon's own environment.
    #[must_use]
    pub fn allowlist(&self) -> &[String] {
        &self.allowlist
    }

    /// The extra explicitly passed-through names.
    #[must_use]
    pub fn passthrough(&self) -> &[String] {
        &self.passthrough
    }

    /// Whether `name` survives the inherited-environment scrub.
    #[must_use]
    pub fn inherits(&self, name: &str) -> bool {
        // A passthrough entry cannot re-admit a denied name: the operator's
        // convenience does not outrank the loader-injection rule.
        deny_reason(name).is_none()
            && (self.allowlist.iter().any(|allowed| allowed == name)
                || self.passthrough.iter().any(|allowed| allowed == name))
    }

    /// The scrubbed view of `inherited`.
    ///
    /// This map is *also* the `$VAR` lookup source for manifest expansion,
    /// which is what stops a manifest reading a variable the scrub removed.
    #[must_use]
    pub fn scrub(&self, inherited: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        inherited
            .iter()
            .filter(|(name, _)| self.inherits(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// The daemon's own environment, scrubbed.
    #[must_use]
    pub fn scrub_process_env(&self) -> BTreeMap<String, String> {
        self.scrub(&process_env())
    }

    /// Builds a child environment from an inherited map and a manifest map.
    ///
    /// Daemon-owned variables are *not* added here — see
    /// [`BuiltEnv::set_daemon_owned`], which is called last by design and
    /// cannot be bypassed by anything in the manifest.
    ///
    /// # Errors
    ///
    /// [`EnvError::Expand`] if a manifest value references a variable the
    /// scrubbed base does not have. That includes a variable the scrub
    /// removed, which is the intended failure mode: an explicit error beats a
    /// silent empty string.
    pub fn build(
        &self,
        inherited: &BTreeMap<String, String>,
        manifest: &BTreeMap<String, EnvValue>,
    ) -> Result<BuiltEnv, EnvError> {
        let base = self.scrub(inherited);
        let mut vars = base.clone();
        let mut denied = Vec::new();
        let mut shadowed = Vec::new();

        for (name, value) in manifest {
            if let Some(reason) = deny_reason(name) {
                denied.push(DeniedVar {
                    name: name.clone(),
                    reason,
                });
                continue;
            }
            let expanded = value
                .expand(|lookup| base.get(lookup).cloned())
                .map_err(|source| EnvError::Expand {
                    key: name.clone(),
                    source,
                })?;
            if vars.insert(name.clone(), expanded).is_some() {
                shadowed.push(name.clone());
            }
        }

        Ok(BuiltEnv {
            vars,
            denied,
            shadowed,
            overridden: Vec::new(),
        })
    }
}

impl Default for EnvPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// A finished child environment, plus what was dropped on the way.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuiltEnv {
    /// The variables the child will see.
    vars: BTreeMap<String, String>,
    /// Manifest variables that were refused.
    denied: Vec<DeniedVar>,
    /// Manifest variables that replaced an inherited value.
    shadowed: Vec<String>,
    /// Manifest variables a daemon-owned variable then replaced.
    ///
    /// Always empty in practice — a manifest variable named `ASTRS_*` is
    /// already denied one step earlier — but recorded rather than assumed, so
    /// that the "daemon wins" rule is observable and not merely believed.
    overridden: Vec<String>,
}

impl BuiltEnv {
    /// An environment holding exactly `vars`, with nothing dropped.
    #[must_use]
    pub const fn from_vars(vars: BTreeMap<String, String>) -> Self {
        Self {
            vars,
            denied: Vec::new(),
            shadowed: Vec::new(),
            overridden: Vec::new(),
        }
    }

    /// The value the child will see for `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Whether the child will see `name` at all.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// The variables the child will see.
    #[must_use]
    pub const fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// How many variables the child will see.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether the child sees nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// The manifest variables that were refused, in manifest order.
    #[must_use]
    pub fn denied(&self) -> &[DeniedVar] {
        &self.denied
    }

    /// The manifest variables that replaced an inherited value.
    #[must_use]
    pub fn shadowed(&self) -> &[String] {
        &self.shadowed
    }

    /// The manifest variables a daemon-owned variable replaced.
    #[must_use]
    pub fn overridden(&self) -> &[String] {
        &self.overridden
    }

    /// Merges already-expanded manifest values, applying the §16 deny-filter.
    ///
    /// The counterpart of [`EnvPolicy::build`] for values that went through
    /// `astrs_manifest::expand_map` upstream: the deny-filter still applies —
    /// expansion having already happened does not make `LD_PRELOAD`
    /// acceptable — but the value itself is copied through verbatim, so a
    /// literal `$` stays a `$`.
    pub fn merge_literal(&mut self, values: &BTreeMap<String, String>) {
        for (name, value) in values {
            if let Some(reason) = deny_reason(name) {
                self.denied.push(DeniedVar {
                    name: name.clone(),
                    reason,
                });
                continue;
            }
            if self.vars.insert(name.clone(), value.clone()).is_some() {
                self.shadowed.push(name.clone());
            }
        }
    }

    /// Applies the daemon-owned variables — **last**, unconditionally (§16).
    ///
    /// Anything already present under one of these names is replaced and
    /// recorded in [`BuiltEnv::overridden`].
    pub fn set_daemon_owned<I, K, V>(&mut self, owned: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (name, value) in owned {
            let name = name.into();
            if self.vars.insert(name.clone(), value.into()).is_some() {
                self.overridden.push(name);
            }
        }
    }

    /// Sets one daemon-owned variable.
    pub fn set_daemon_var(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.set_daemon_owned([(name.into(), value.into())]);
    }

    /// Consumes this environment into the map a `Command` is given.
    #[must_use]
    pub fn into_vars(self) -> BTreeMap<String, String> {
        self.vars
    }

    /// A one-line summary for a log record: how many variables, and what was
    /// dropped.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut summary = format!("{} variable(s)", self.vars.len());
        if !self.denied.is_empty() {
            let names: Vec<&str> = self
                .denied
                .iter()
                .map(|denied| denied.name.as_str())
                .collect();
            summary.push_str(&format!(", denied [{}]", names.join(", ")));
        }
        if !self.shadowed.is_empty() {
            summary.push_str(&format!(", shadowed [{}]", self.shadowed.join(", ")));
        }
        summary
    }
}

/// The daemon's own environment as a map.
#[must_use]
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn inherited() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), "/home/robot".to_string()),
            ("USER".to_string(), "robot".to_string()),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            ("RUST_LOG".to_string(), "info".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "hunter2".to_string()),
            ("LD_PRELOAD".to_string(), "/opt/evil.so".to_string()),
            (
                "DYLD_INSERT_LIBRARIES".to_string(),
                "/evil.dylib".to_string(),
            ),
            ("ASTRS_NODE_CONFIG".to_string(), "stale".to_string()),
            ("CUDA_VISIBLE_DEVICES".to_string(), "0".to_string()),
        ])
    }

    fn manifest(pairs: &[(&str, EnvValue)]) -> BTreeMap<String, EnvValue> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn the_allowlist_is_exactly_the_blueprint_five() {
        let policy = EnvPolicy::new();
        assert_eq!(policy.allowlist(), DEFAULT_ENV_ALLOWLIST);
        for name in DEFAULT_ENV_ALLOWLIST {
            assert!(policy.inherits(name), "{name} should be inherited");
        }
    }

    #[test]
    fn the_scrub_keeps_only_the_allowlist() {
        let scrubbed = EnvPolicy::new().scrub(&inherited());
        let names: Vec<&str> = scrubbed.keys().map(String::as_str).collect();
        assert_eq!(names, ["HOME", "PATH", "RUST_LOG", "TMPDIR", "USER"]);
    }

    #[test]
    fn explicit_passthrough_admits_one_more_name() {
        let policy = EnvPolicy::new().with_passthrough(["CUDA_VISIBLE_DEVICES"]);
        let scrubbed = policy.scrub(&inherited());
        assert_eq!(
            scrubbed.get("CUDA_VISIBLE_DEVICES").map(String::as_str),
            Some("0")
        );
        assert!(!scrubbed.contains_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn passthrough_cannot_re_admit_a_denied_name() {
        let policy = EnvPolicy::new().with_passthrough(["LD_PRELOAD", "ASTRS_NODE_CONFIG"]);
        let scrubbed = policy.scrub(&inherited());
        assert!(!scrubbed.contains_key("LD_PRELOAD"));
        assert!(!scrubbed.contains_key("ASTRS_NODE_CONFIG"));
    }

    #[test]
    fn an_empty_allowlist_inherits_nothing() {
        let policy = EnvPolicy::new().with_allowlist(Vec::<String>::new());
        assert!(policy.scrub(&inherited()).is_empty());
    }

    #[test]
    fn the_deny_table_covers_the_blueprint_names() {
        assert_eq!(deny_reason("PATH"), None);
        assert_eq!(deny_reason("MY_VAR"), None);
        assert_eq!(
            deny_reason("LD_LIBRARY_PATH"),
            None,
            "not an injection point"
        );
        assert_eq!(
            deny_reason("ASTRS_RUN_PARENT_PID"),
            Some(DenyReason::DaemonNamespace)
        );
        assert_eq!(deny_reason("ASTRS_"), Some(DenyReason::DaemonNamespace));
        assert_eq!(
            deny_reason("DYLD_LIBRARY_PATH"),
            Some(DenyReason::LoaderInjection)
        );
        for name in DENIED_NAMES {
            assert_eq!(
                deny_reason(name),
                Some(DenyReason::LoaderInjection),
                "{name}"
            );
        }
        for name in ["", "A=B", "A\0B"] {
            assert_eq!(deny_reason(name), Some(DenyReason::Malformed), "{name:?}");
        }
    }

    #[test]
    fn denied_prefixes_are_the_documented_ones() {
        assert_eq!(DENIED_PREFIXES, &["ASTRS_", "DYLD_"]);
        for prefix in DENIED_PREFIXES {
            assert!(deny_reason(&format!("{prefix}ANYTHING")).is_some());
        }
    }

    #[test]
    fn a_manifest_cannot_smuggle_a_loader_variable_in() {
        let built = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[
                    ("LD_PRELOAD", EnvValue::String("./evil.so".into())),
                    (
                        "DYLD_INSERT_LIBRARIES",
                        EnvValue::String("./evil.dylib".into()),
                    ),
                    ("ASTRS_NODE_CONFIG", EnvValue::String("forged".into())),
                    ("GOOD", EnvValue::String("kept".into())),
                ]),
            )
            .unwrap();

        assert!(!built.contains("LD_PRELOAD"));
        assert!(!built.contains("DYLD_INSERT_LIBRARIES"));
        assert!(!built.contains("ASTRS_NODE_CONFIG"));
        assert_eq!(built.get("GOOD"), Some("kept"));

        let reasons: Vec<(&str, DenyReason)> = built
            .denied()
            .iter()
            .map(|denied| (denied.name.as_str(), denied.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                ("ASTRS_NODE_CONFIG", DenyReason::DaemonNamespace),
                ("DYLD_INSERT_LIBRARIES", DenyReason::LoaderInjection),
                ("LD_PRELOAD", DenyReason::LoaderInjection),
            ]
        );
    }

    #[test]
    fn a_manifest_cannot_read_a_scrubbed_value_out() {
        let error = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[("LEAK", EnvValue::String("$AWS_SECRET_ACCESS_KEY".into()))]),
            )
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("LEAK"), "{rendered}");
        assert!(rendered.contains("AWS_SECRET_ACCESS_KEY"), "{rendered}");
    }

    #[test]
    fn expansion_reads_the_scrubbed_base() {
        let built = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[("BIN", EnvValue::String("$HOME/bin".into()))]),
            )
            .unwrap();
        assert_eq!(built.get("BIN"), Some("/home/robot/bin"));
    }

    #[test]
    fn non_string_manifest_values_render_without_expansion() {
        let built = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[
                    ("INDEX", EnvValue::Int(3)),
                    ("ENABLED", EnvValue::Bool(true)),
                    ("GAIN", EnvValue::Float(0.5)),
                ]),
            )
            .unwrap();
        assert_eq!(built.get("INDEX"), Some("3"));
        assert_eq!(built.get("ENABLED"), Some("true"));
        assert_eq!(built.get("GAIN"), Some("0.5"));
    }

    #[test]
    fn a_manifest_value_shadows_an_inherited_one_and_says_so() {
        let built = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[("RUST_LOG", EnvValue::String("debug".into()))]),
            )
            .unwrap();
        assert_eq!(built.get("RUST_LOG"), Some("debug"));
        assert_eq!(built.shadowed(), ["RUST_LOG"]);
    }

    #[test]
    fn daemon_owned_variables_are_applied_last_and_win() {
        let mut built = EnvPolicy::new()
            .build(&inherited(), &manifest(&[("KEEP", EnvValue::Int(1))]))
            .unwrap();
        built.set_daemon_var("ASTRS_NODE_CONFIG", "the-real-blob");
        built.set_daemon_var("ASTRS_RUN_PARENT_PID", "42");

        assert_eq!(built.get("ASTRS_NODE_CONFIG"), Some("the-real-blob"));
        assert_eq!(built.get("ASTRS_RUN_PARENT_PID"), Some("42"));
        assert_eq!(built.get("KEEP"), Some("1"));
        assert!(
            built.overridden().is_empty(),
            "nothing to override: the manifest name was already denied"
        );
    }

    #[test]
    fn a_daemon_variable_replacing_something_records_the_override() {
        let mut built = BuiltEnv::from_vars(BTreeMap::from([(
            "ASTRS_NODE_CONFIG".to_string(),
            "smuggled".to_string(),
        )]));
        built.set_daemon_var("ASTRS_NODE_CONFIG", "real");
        assert_eq!(built.get("ASTRS_NODE_CONFIG"), Some("real"));
        assert_eq!(built.overridden(), ["ASTRS_NODE_CONFIG"]);
    }

    #[test]
    fn a_built_environment_summarizes_what_it_dropped() {
        let built = EnvPolicy::new()
            .build(
                &inherited(),
                &manifest(&[
                    ("LD_PRELOAD", EnvValue::String("x".into())),
                    ("RUST_LOG", EnvValue::String("trace".into())),
                ]),
            )
            .unwrap();
        let summary = built.summary();
        assert!(summary.contains("denied [LD_PRELOAD]"), "{summary}");
        assert!(summary.contains("shadowed [RUST_LOG]"), "{summary}");
    }

    #[test]
    fn an_empty_environment_reports_itself_empty() {
        let built = BuiltEnv::default();
        assert!(built.is_empty());
        assert_eq!(built.len(), 0);
        assert_eq!(built.summary(), "0 variable(s)");
    }

    #[test]
    fn deny_reasons_have_stable_labels() {
        for reason in [
            DenyReason::DaemonNamespace,
            DenyReason::LoaderInjection,
            DenyReason::Malformed,
        ] {
            assert!(!reason.as_str().is_empty());
            assert!(!reason.to_string().is_empty());
        }
        assert_eq!(DenyReason::LoaderInjection.as_str(), "loader_injection");
        assert_eq!(
            DeniedVar {
                name: "LD_PRELOAD".into(),
                reason: DenyReason::LoaderInjection,
            }
            .to_string(),
            "LD_PRELOAD: a dynamic-loader injection point"
        );
    }

    #[test]
    fn the_process_environment_scrub_never_leaks_a_denied_name() {
        // Whatever this test process happens to have, the scrub result may
        // not contain anything the deny table refuses.
        let scrubbed = EnvPolicy::new()
            .with_passthrough(["CARGO_PKG_NAME"])
            .scrub_process_env();
        for name in scrubbed.keys() {
            assert_eq!(deny_reason(name), None, "{name} leaked");
        }
    }

    #[test]
    fn already_expanded_values_are_copied_verbatim() {
        let mut built = EnvPolicy::new()
            .build(&inherited(), &BTreeMap::new())
            .unwrap();
        built.merge_literal(&BTreeMap::from([
            ("PRICE".to_string(), "$5.00".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]));
        assert_eq!(
            built.get("PRICE"),
            Some("$5.00"),
            "a literal dollar is not a reference"
        );
        assert_eq!(built.get("EMPTY"), Some(""));
    }

    #[test]
    fn the_deny_filter_still_applies_to_already_expanded_values() {
        let mut built = EnvPolicy::new()
            .build(&inherited(), &BTreeMap::new())
            .unwrap();
        built.merge_literal(&BTreeMap::from([
            ("LD_PRELOAD".to_string(), "./evil.so".to_string()),
            ("ASTRS_NODE_CONFIG".to_string(), "forged".to_string()),
            ("DYLD_INSERT_LIBRARIES".to_string(), "x".to_string()),
            ("GOOD".to_string(), "kept".to_string()),
        ]));
        assert!(!built.contains("LD_PRELOAD"));
        assert!(!built.contains("ASTRS_NODE_CONFIG"));
        assert!(!built.contains("DYLD_INSERT_LIBRARIES"));
        assert_eq!(built.get("GOOD"), Some("kept"));
        assert_eq!(built.denied().len(), 3);
    }

    #[test]
    fn merging_a_literal_over_an_inherited_value_is_reported_as_shadowing() {
        let mut built = EnvPolicy::new()
            .build(&inherited(), &BTreeMap::new())
            .unwrap();
        built.merge_literal(&BTreeMap::from([(
            "RUST_LOG".to_string(),
            "trace".to_string(),
        )]));
        assert_eq!(built.get("RUST_LOG"), Some("trace"));
        assert_eq!(built.shadowed(), ["RUST_LOG"]);
    }

    #[test]
    fn into_vars_hands_over_exactly_what_the_child_sees() {
        let mut built = EnvPolicy::new()
            .build(&inherited(), &manifest(&[("A", EnvValue::Int(1))]))
            .unwrap();
        built.set_daemon_var("ASTRS_NODE_CONFIG", "blob");
        let vars = built.clone().into_vars();
        assert_eq!(vars.len(), built.len());
        assert_eq!(
            vars.get("ASTRS_NODE_CONFIG").map(String::as_str),
            Some("blob")
        );
    }
}
