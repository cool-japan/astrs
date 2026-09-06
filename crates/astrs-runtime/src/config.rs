//! [`RuntimeConfig`] — what a caller hands to [`crate::run_runtime`] or
//! [`crate::RuntimeHost::new`].

use std::path::PathBuf;

use astrs_manifest::OperatorConfig;
use astrs_operator_api::OperatorRegistry;

/// Sandbox limits applied to every `wasm:`-sourced operator instance
/// (`astrs-runtime`'s `wasm-operators` feature; see `crate::wasm`'s module
/// docs for the guest ABI these limits bound — a plain code span rather
/// than an intra-doc link, since that module only exists when its own
/// feature is on, and a link to a `cfg`-ed-out module is a broken link in
/// every build that does not enable it, matching this crate's own top-level
/// docs' precedent for the `dylib`/`wasm` modules).
///
/// Unconditional (not `#[cfg]`-gated) even though only the `wasm-operators`
/// feature ever reads it: a plain data struct with no `wasmi` types of its
/// own costs nothing in a build without that feature, and keeping
/// [`RuntimeConfig`]'s own field list free of `#[cfg]` is worth that.
/// Blueprint's `wasm:` operator source kind (`astrs-manifest`'s frozen
/// schema) carries no per-operator options of its own — a bare path — so
/// these limits are necessarily a runtime-wide default, not something a
/// manifest author tunes per instance today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSandboxConfig {
    /// Fuel granted before every single guest call — `astrs-op-alloc`,
    /// `astrs-op-init` and `astrs-op-event` each start from this budget, not
    /// a total shared across a run, so one slow event never starves the
    /// next (see `crate::wasm`'s module docs).
    pub fuel_per_call: u64,
    /// The maximum size, in bytes, any one guest linear memory may grow to.
    pub max_memory_bytes: usize,
}

impl Default for WasmSandboxConfig {
    /// 10,000,000 fuel per call — generous enough for tens of thousands of
    /// ordinary instructions (wasmi charges roughly one fuel unit per
    /// simple instruction) while still tripping an infinite loop in well
    /// under a second; 64 MiB of linear memory — enough for a modest
    /// per-message working set without letting one runaway operator exhaust
    /// host memory.
    fn default() -> Self {
        Self {
            fuel_per_call: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Everything a [`crate::RuntimeHost`] needs beyond the connected node
/// itself.
///
/// The two halves come from different places and stay separate on purpose:
/// `operators` is manifest data (blueprint §9.3's `operators:` node
/// source — read at spawn time, from wherever the process that starts this
/// one assembles it), while `registry` is compiled into *this* binary via
/// [`astrs_operator_api::register_operator!`] (blueprint §9.3: "operators
/// compile into the runtime binary via the registry macro — static
/// dispatch, no `dlopen`"). Building [`RuntimeConfig`] is exactly the step
/// that brings the two together.
#[derive(Default)]
pub struct RuntimeConfig {
    /// The `operators:` entries this host runs, in manifest order.
    pub operators: Vec<OperatorConfig>,
    /// The statically-registered operator constructors, keyed by
    /// `operators[].operator` (the `register_operator!` name).
    pub registry: OperatorRegistry,
    /// The directory a relative `operators[].dylib` path resolves against
    /// (blueprint §22: "the runtime resolves the path relative to the
    /// dataflow file"). `None` falls back to the process's own current
    /// directory — the same default a relative path on a shell command
    /// line would get — for a caller (today, `astrs-runtime`'s own
    /// `main.rs`) that has no dataflow file path of its own to offer.
    /// Ignored entirely for operators with no `dylib:` locator.
    pub dataflow_dir: Option<PathBuf>,
    /// Fuel and memory limits applied to every `wasm:`-sourced operator
    /// instance (`astrs-runtime`'s `wasm-operators` feature). Ignored
    /// entirely for operators with no `wasm:` locator, and — like
    /// `dataflow_dir` above — unconditional so this struct's field list
    /// stays free of `#[cfg]`.
    pub wasm_sandbox: WasmSandboxConfig,
}

// Hand-written: `OperatorRegistry` holds `Box<dyn Fn() -> Box<dyn Operator>>`
// constructors, which have no useful `Debug`, so a derived impl could not
// exist. This prints the registered names instead, which is what a caller
// debugging "why didn't my operator build" actually wants to see.
impl core::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field(
                "operators",
                &self
                    .operators
                    .iter()
                    .map(|op| op.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("registry", &self.registry.names().collect::<Vec<_>>())
            .field("dataflow_dir", &self.dataflow_dir)
            .field("wasm_sandbox", &self.wasm_sandbox)
            .finish()
    }
}

impl RuntimeConfig {
    /// Pairs a manifest's `operators:` list with the registry that knows
    /// how to build each one.
    ///
    /// [`RuntimeConfig::dataflow_dir`] starts `None`; chain
    /// [`RuntimeConfig::with_dataflow_dir`] onto this when any operator
    /// declares a relative `dylib:` path.
    #[must_use]
    pub fn new(operators: Vec<OperatorConfig>, registry: OperatorRegistry) -> Self {
        Self {
            operators,
            registry,
            dataflow_dir: None,
            wasm_sandbox: WasmSandboxConfig::default(),
        }
    }

    /// Sets the directory a relative `operators[].dylib` path resolves
    /// against — see [`RuntimeConfig::dataflow_dir`]'s own docs.
    #[must_use]
    pub fn with_dataflow_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dataflow_dir = Some(dir.into());
        self
    }

    /// Overrides the fuel and memory limits applied to every `wasm:`-sourced
    /// operator instance — see [`RuntimeConfig::wasm_sandbox`]'s own docs.
    #[must_use]
    pub fn with_wasm_sandbox(mut self, sandbox: WasmSandboxConfig) -> Self {
        self.wasm_sandbox = sandbox;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_default_config_hosts_nothing() {
        let config = RuntimeConfig::default();
        assert!(config.operators.is_empty());
        assert!(config.registry.is_empty());
    }

    #[test]
    fn new_pairs_operators_with_a_registry() {
        let config = RuntimeConfig::new(
            vec![OperatorConfig {
                id: "crop".to_owned(),
                operator: "Crop".to_owned(),
                dylib: None,
                wasm: None,
                hub: None,
                inputs: Default::default(),
                outputs: Vec::new(),
                config: Default::default(),
            }],
            OperatorRegistry::new(),
        );
        assert_eq!(config.operators.len(), 1);
        assert!(config.registry.is_empty());
        assert!(config.dataflow_dir.is_none());
        assert_eq!(config.wasm_sandbox, WasmSandboxConfig::default());
    }

    #[test]
    fn with_dataflow_dir_sets_the_resolution_base() {
        let dir = std::env::temp_dir().join("astrs-runtime-config-test-graphs");
        let config =
            RuntimeConfig::new(Vec::new(), OperatorRegistry::new()).with_dataflow_dir(dir.clone());
        assert_eq!(config.dataflow_dir, Some(dir));
    }

    #[test]
    fn with_wasm_sandbox_overrides_the_default_limits() {
        let sandbox = WasmSandboxConfig {
            fuel_per_call: 42,
            max_memory_bytes: 1024,
        };
        let config = RuntimeConfig::new(Vec::new(), OperatorRegistry::new())
            .with_wasm_sandbox(sandbox.clone());
        assert_eq!(config.wasm_sandbox, sandbox);
    }
}
