//! Mapping a parsed [`DoraManifest`] onto an [`astrs_manifest::Manifest`]
//! (blueprint §8.6): the mechanical-mapping scope explicitly named there —
//! timer virtual-source paths, `dora/…`→`astrs/…`, restart fields, queue
//! policies, service/action patterns, and modules — gets a full, tested
//! translation. Everything else dora's descriptor can carry that AstRS has
//! no manifest-level equivalent for is never silently dropped: it becomes
//! a [`MigrationNote`], surfaced both in the returned list and (by
//! [`super::render`]) as a `TODO(astrs migrate)` YAML comment next to the
//! node it came from.

use std::collections::BTreeMap;

use serde::Serialize;

use super::byte_size::resolve_byte_size;
use super::model::{DoraDebug, DoraDeploy, DoraInput, DoraManifest, DoraNode, DoraOperator};

/// How serious a [`MigrationNote`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteSeverity {
    /// A dora construct that had no effect in dora either (verified dead
    /// configuration) was dropped outright — informational only.
    Dropped,
    /// A dora construct with no AstRS equivalent needs a human decision;
    /// nothing was silently lost, but nothing was automatically ported
    /// either.
    NeedsAttention,
}

/// One observation made while migrating a dora descriptor.
///
/// [`super::migrate_str`] returns the complete list; `render`
/// (crate-private, not this module) turns each into a `TODO(astrs
/// migrate)` comment placed next to the node it names (or, for `node:
/// None`, into a header comment at the top of the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationNote {
    /// The node this note is about, or `None` for a root-level (whole
    /// dataflow) observation.
    pub node: Option<String>,
    /// How serious this observation is.
    pub severity: NoteSeverity,
    /// A human-readable explanation, written to stand on its own inside a
    /// YAML comment.
    pub message: String,
}

impl MigrationNote {
    fn needs_attention(node: &str, message: String) -> Self {
        Self {
            node: Some(node.to_string()),
            severity: NoteSeverity::NeedsAttention,
            message,
        }
    }

    fn dropped(node: &str, message: String) -> Self {
        Self {
            node: Some(node.to_string()),
            severity: NoteSeverity::Dropped,
            message,
        }
    }

    fn root_needs_attention(message: String) -> Self {
        Self {
            node: None,
            severity: NoteSeverity::NeedsAttention,
            message,
        }
    }
}

/// Render a `astrs_yaml::Value` compactly enough to quote inline inside a
/// one-line note (`to_string` on a mapping/sequence value is multi-line;
/// this collapses it to a single line so it never breaks a YAML comment
/// across lines when [`super::render`] writes it back out).
fn render_yaml_inline(value: &astrs_yaml::Value) -> String {
    astrs_yaml::to_string(value)
        .unwrap_or_default()
        .trim()
        .replace('\n', "; ")
}

/// Parse `raw` as a bare YAML scalar into `T` — used for the several dora
/// fields that share AstRS's own enum wire spelling exactly (`pattern`,
/// `queue_policy`, `min_log_level`), so this importer does not hand-roll a
/// second copy of a mapping table that already exists as a `Deserialize`
/// impl.
fn try_parse_enum<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    astrs_yaml::from_str(raw).ok()
}

/// Convert a `dora/...` (or ordinary `node/output`) source string, per
/// blueprint §8.6's mapping scope.
///
/// `dora/logs...` and `dora/status` rename their prefix directly (AstRS
/// uses the identical suffix spelling); `dora/timer/...` is handled by
/// [`rewrite_timer`], which also absorbs the one real impedance mismatch
/// (sub-millisecond/fractional-hz resolution AstRS's `u64`-valued timers
/// cannot represent exactly). An ordinary `node/output` reference (no
/// `dora/` prefix) passes through unchanged.
fn rewrite_source(source: &str, node: &str, input: &str, notes: &mut Vec<MigrationNote>) -> String {
    if let Some(rest) = source.strip_prefix("dora/timer/") {
        return rewrite_timer(rest, source, node, input, notes);
    }
    if source == "dora/status" || source.starts_with("dora/logs") {
        return format!("astrs/{}", &source["dora/".len()..]);
    }
    if source.starts_with("dora/") {
        notes.push(MigrationNote::needs_attention(
            node,
            format!(
                "input `{input}`: unrecognized dora virtual source `{source}`; carried through \
                 verbatim (this will fail `astrs validate` until corrected by hand)"
            ),
        ));
        return source.to_string();
    }
    source.to_string()
}

/// A positive integer timer value, or — for a value dora accepted that is
/// not representable as a positive `u64` (a fraction, e.g. dora's own
/// `dora/timer/hz/0.5`) — the nearest positive integer, with a note
/// explaining the rounding.
fn integer_or_rounded(
    value: &str,
    source: &str,
    node: &str,
    input: &str,
    notes: &mut Vec<MigrationNote>,
) -> u64 {
    if let Ok(exact) = value.parse::<u64>()
        && exact > 0
    {
        return exact;
    }
    let rounded = value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.round() as u64)
        .unwrap_or(1)
        .max(1);
    notes.push(MigrationNote::needs_attention(
        node,
        format!(
            "input `{input}`: timer value `{value}` in `{source}` is not a positive integer \
             AstRS's timer accepts; rounded to `{rounded}`"
        ),
    ));
    rounded
}

/// Rewrite the `<unit>/<value>` remainder of a `dora/timer/...` source.
fn rewrite_timer(
    rest: &str,
    source: &str,
    node: &str,
    input: &str,
    notes: &mut Vec<MigrationNote>,
) -> String {
    let Some((unit, value)) = rest.split_once('/') else {
        notes.push(MigrationNote::needs_attention(
            node,
            format!(
                "input `{input}`: malformed dora timer source `{source}`; carried through verbatim"
            ),
        ));
        return source.to_string();
    };
    match unit {
        "millis" | "secs" | "hz" => {
            let n = integer_or_rounded(value, source, node, input, notes);
            format!("astrs/timer/{unit}/{n}")
        }
        "micros" | "nanos" => {
            let divisor = if unit == "micros" {
                1_000.0
            } else {
                1_000_000.0
            };
            let Some(millis) = value
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| (v / divisor).round().max(1.0) as u64)
            else {
                notes.push(MigrationNote::needs_attention(
                    node,
                    format!(
                        "input `{input}`: timer value `{value}` in `{source}` could not be \
                         interpreted; carried through verbatim"
                    ),
                ));
                return source.to_string();
            };
            notes.push(MigrationNote::needs_attention(
                node,
                format!(
                    "input `{input}`: `{source}` has no lossless AstRS timer equivalent \
                     (AstRS's finest timer resolution is milliseconds); approximated as \
                     `astrs/timer/millis/{millis}`"
                ),
            ));
            format!("astrs/timer/millis/{millis}")
        }
        other => {
            notes.push(MigrationNote::needs_attention(
                node,
                format!(
                    "input `{input}`: unrecognized dora timer unit `{other}` in `{source}`; \
                     carried through verbatim"
                ),
            ));
            source.to_string()
        }
    }
}

fn convert_env(
    env: &BTreeMap<String, super::model::DoraEnvValue>,
) -> BTreeMap<String, astrs_manifest::EnvValue> {
    env.iter()
        .map(|(k, v)| (k.clone(), v.clone().into()))
        .collect()
}

fn convert_input(
    node: &str,
    name: &str,
    input: &DoraInput,
    notes: &mut Vec<MigrationNote>,
) -> astrs_manifest::Input {
    match input {
        DoraInput::Short(source) => {
            astrs_manifest::Input::from_source(rewrite_source(source, node, name, notes))
        }
        DoraInput::Long {
            source,
            queue_size,
            queue_policy,
            input_timeout,
        } => {
            let mut out =
                astrs_manifest::Input::from_source(rewrite_source(source, node, name, notes));
            if let Some(size) = queue_size {
                out.queue_size = *size;
            }
            if let Some(policy) = queue_policy {
                match try_parse_enum::<astrs_manifest::QueuePolicy>(policy) {
                    Some(parsed) => out.queue_policy = parsed,
                    None => notes.push(MigrationNote::needs_attention(
                        node,
                        format!(
                            "input `{name}`: unrecognized queue_policy `{policy}`; left at the \
                             AstRS default (`drop_oldest`)"
                        ),
                    )),
                }
            }
            if let Some(timeout) = input_timeout {
                match astrs_manifest::DurationSecs::from_secs_f64(*timeout) {
                    Ok(duration) => out.timeout = Some(duration),
                    Err(err) => notes.push(MigrationNote::needs_attention(
                        node,
                        format!(
                            "input `{name}`: input_timeout `{timeout}` is not a valid duration \
                             ({err}); left unset"
                        ),
                    )),
                }
            }
            out
        }
    }
}

fn convert_duration(
    node: &str,
    field: &str,
    value: Option<f64>,
    notes: &mut Vec<MigrationNote>,
) -> Option<astrs_manifest::DurationSecs> {
    let raw = value?;
    match astrs_manifest::DurationSecs::from_secs_f64(raw) {
        Ok(duration) => Some(duration),
        Err(err) => {
            notes.push(MigrationNote::needs_attention(
                node,
                format!("{field}: `{raw}` is not a valid duration ({err}); left unset"),
            ));
            None
        }
    }
}

fn convert_operator(
    node: &str,
    op: &DoraOperator,
    notes: &mut Vec<MigrationNote>,
) -> astrs_manifest::OperatorConfig {
    let op_id = op.id.clone().unwrap_or_else(|| "operator".to_string());
    let placeholder = format!("TODO_REIMPLEMENT_{op_id}");
    if !op.extra.is_empty() {
        let extras: Vec<String> = op
            .extra
            .iter()
            .map(|(k, v)| format!("{k}: {}", render_yaml_inline(v)))
            .collect();
        notes.push(MigrationNote::needs_attention(
            node,
            format!(
                "operator `{op_id}`: dora loads this operator dynamically ({}); AstRS operators \
                 are compiled-in Rust trait objects (blueprint §9.3) with no automatic \
                 equivalent — implement and `register_operator!` it, then replace the \
                 placeholder name `{placeholder}`",
                extras.join(", ")
            ),
        ));
    }
    astrs_manifest::OperatorConfig {
        id: op_id,
        operator: placeholder,
        dylib: None,
        wasm: None,
        hub: None,
        inputs: op
            .inputs
            .iter()
            .map(|(name, input)| (name.clone(), convert_input(node, name, input, notes)))
            .collect(),
        outputs: op.outputs.clone(),
        config: BTreeMap::new(),
    }
}

/// Push one note scoped to `node` (`Some(id)` for a per-node observation,
/// `None` for a manifest-root one) — the one piece [`MigrationNote`]'s two
/// private constructors (`needs_attention`/`root_needs_attention`) don't
/// share, factored out because [`convert_deploy`] now needs it at two call
/// sites (`distribute`, `extra`) for the same root-vs-node choice.
fn push_scoped_note(node: Option<&str>, notes: &mut Vec<MigrationNote>, message: String) {
    notes.push(match node {
        Some(id) => MigrationNote::needs_attention(id, message),
        None => MigrationNote::root_needs_attention(message),
    });
}

/// Convert a dora `deploy:` block (old spelling `_unstable_deploy:`) into
/// [`astrs_manifest::Deploy`], flagging a non-`local` `distribute:` (no
/// AstRS equivalent — every daemon builds from source in 0.1) and any
/// other key the block carried that this importer does not model
/// explicitly — never silently dropped, matching every other `extra`
/// catch-all's treatment in this module.
///
/// `node` scopes every emitted note: `Some(id)` for a per-node block
/// ([`DoraNode::deploy`]), `None` for the manifest-root block
/// ([`DoraManifest::deploy`]) — dora uses the identical shape at both
/// levels, so this one function maps both call sites in [`convert_node`]
/// and [`convert`].
fn convert_deploy(
    node: Option<&str>,
    deploy: &DoraDeploy,
    notes: &mut Vec<MigrationNote>,
) -> astrs_manifest::Deploy {
    if let Some(strategy) = &deploy.distribute
        && strategy != "local"
    {
        push_scoped_note(
            node,
            notes,
            format!(
                "deploy.distribute: `{strategy}` has no AstRS equivalent; every daemon builds \
                 from source in 0.1"
            ),
        );
    }
    for (key, value) in &deploy.extra {
        push_scoped_note(
            node,
            notes,
            format!(
                "deploy.{key}: unrecognized field (dora-only, or not yet handled by this \
                 importer); dropped — original value: {}",
                render_yaml_inline(value)
            ),
        );
    }
    astrs_manifest::Deploy {
        machine: deploy.machine.clone(),
        working_dir: deploy.working_dir.clone(),
        labels: deploy.labels.clone(),
    }
}

/// Convert a dora `debug:` block (old spelling `_unstable_debug:`), root-
/// scoped like [`DoraManifest::debug`] itself, onto AstRS's single
/// `debug: bool` ([`astrs_manifest::Manifest::debug`]) — dora's
/// `enable_debug_inspection` flag is the whole of that mapping. Every other
/// key the block carried (dora's own removed `publish_all_messages_to_zenoh`
/// alias, or a future field this importer does not know about yet) is
/// never silently dropped: each becomes its own root-scoped
/// [`MigrationNote`], matching every other `extra` catch-all's treatment in
/// this module.
fn convert_debug(debug: &DoraDebug, notes: &mut Vec<MigrationNote>) -> bool {
    for (key, value) in &debug.extra {
        notes.push(MigrationNote::root_needs_attention(format!(
            "debug.{key}: unrecognized field (dora-only, or not yet handled by this importer); \
             dropped — original value: {}",
            render_yaml_inline(value)
        )));
    }
    debug.enable_debug_inspection
}

fn convert_node(dora: &DoraNode, notes: &mut Vec<MigrationNote>) -> astrs_manifest::Node {
    let id = dora.id.clone();
    // Seeded via `with_path` (which initializes every other field to its
    // documented empty/None default) purely for a starting value; `path`
    // is overwritten immediately below with dora's own (possibly absent)
    // value, since `Node` has no `Default` impl of its own to build a
    // struct-update literal from.
    let mut node = astrs_manifest::Node::with_path(id.clone(), String::new());
    node.path = dora.path.clone();
    node.name = dora.name.clone();
    node.description = dora.description.clone();
    node.git = dora.git.clone();
    node.branch = dora.branch.clone();
    node.tag = dora.tag.clone();
    node.rev = dora.rev.clone();
    node.module = dora.module.clone();
    node.build = dora.build.clone();

    node.args = match &dora.args {
        Some(raw) => match shlex::split(raw) {
            Some(tokens) => tokens,
            None => {
                notes.push(MigrationNote::needs_attention(
                    &id,
                    format!(
                        "args `{raw}` could not be tokenized as a shell command line \
                         (unbalanced quoting?); left empty"
                    ),
                ));
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    node.env = convert_env(&dora.env);
    node.inputs = dora
        .inputs
        .iter()
        .map(|(name, input)| (name.clone(), convert_input(&id, name, input, notes)))
        .collect();
    node.outputs = dora.outputs.clone();
    node.input_types = dora
        .input_types
        .iter()
        .map(|(k, v)| (k.clone(), astrs_manifest::Urn::new(v.clone())))
        .collect();
    node.output_types = dora
        .output_types
        .iter()
        .map(|(k, v)| (k.clone(), astrs_manifest::Urn::new(v.clone())))
        .collect();

    if let Some(pattern) = &dora.pattern {
        match try_parse_enum::<astrs_manifest::Pattern>(pattern) {
            Some(parsed) => node.pattern = Some(parsed),
            None => notes.push(MigrationNote::needs_attention(
                &id,
                format!("unrecognized pattern `{pattern}`; left unset"),
            )),
        }
    }

    node.send_stdout_as = dora.send_stdout_as.clone();
    if let Some(name) = &dora.send_logs_as {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "send_logs_as: `{name}` has no AstRS equivalent (only send_stdout_as exists); \
                 structured log records are already reachable via the `astrs/logs` virtual \
                 source instead"
            ),
        ));
    }

    if let Some(level) = &dora.min_log_level {
        match try_parse_enum::<astrs_manifest::LogLevel>(level) {
            Some(parsed) => node.min_log_level = Some(parsed),
            None => notes.push(MigrationNote::needs_attention(
                &id,
                format!("unrecognized min_log_level `{level}`; left unset"),
            )),
        }
    }

    if let Some(size) = &dora.max_log_size {
        match resolve_byte_size(size) {
            Ok(bytes) => node.max_log_size = Some(bytes),
            Err(err) => notes.push(MigrationNote::needs_attention(
                &id,
                format!("max_log_size could not be parsed ({err}); left unset"),
            )),
        }
    }
    if let Some(max_rotated_files) = dora.max_rotated_files {
        node.max_rotated_files = max_rotated_files;
    }

    if let Some(policy) = &dora.restart_policy {
        // The one confirmed spelling mismatch: dora's `on-failure` versus
        // AstRS's `on_failure` (see this module's parent docs). `never`
        // and `always` contain no hyphen, so the blanket replace is a
        // no-op for them.
        match try_parse_enum::<astrs_manifest::RestartPolicy>(&policy.replace('-', "_")) {
            Some(parsed) => node.restart_policy = Some(parsed),
            None => notes.push(MigrationNote::needs_attention(
                &id,
                format!("unrecognized restart_policy `{policy}`; left unset"),
            )),
        }
    }
    node.max_restarts = dora.max_restarts;
    node.restart_delay = convert_duration(&id, "restart_delay", dora.restart_delay, notes);
    node.max_restart_delay =
        convert_duration(&id, "max_restart_delay", dora.max_restart_delay, notes);
    node.restart_window = convert_duration(&id, "restart_window", dora.restart_window, notes);
    node.health_check_timeout = convert_duration(
        &id,
        "health_check_timeout",
        dora.health_check_timeout,
        notes,
    );
    node.finish_grace_secs =
        convert_duration(&id, "finish_grace_secs", dora.finish_grace_secs, notes);

    node.cpu_affinity = dora.cpu_affinity.clone();
    if let Some(size) = &dora.shared_memory_pool_size {
        match resolve_byte_size(size) {
            Ok(bytes) => node.shm_pool_size = Some(bytes),
            Err(err) => notes.push(MigrationNote::needs_attention(
                &id,
                format!("shared_memory_pool_size could not be parsed ({err}); left unset"),
            )),
        }
    }

    if let Some(hub) = &dora.hub {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "hub: `{hub}` has no AstRS equivalent (no package registry in 0.1); replace \
                 with `git:` or `path:` to a vendored copy"
            ),
        ));
    }
    if !dora.params.is_empty() {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "params: {:?} (module compile-time parameter substitution) has no AstRS \
                 equivalent; if the referenced module used `${{_param.<key>}}`, substitute these \
                 values by hand once that module file is migrated separately",
                dora.params
            ),
        ));
    }
    if let Some(hash) = &dora.path_sha256 {
        notes.push(MigrationNote::dropped(
            &id,
            format!("path_sha256: `{hash}` (build artifact integrity hash) has no AstRS equivalent; dropped"),
        ));
    }
    if !dora.output_framing.is_empty() {
        notes.push(MigrationNote::dropped(
            &id,
            "output_framing was dead configuration in dora as well (every path already used \
             one wire format); dropped"
                .to_string(),
        ));
    }
    if !dora.output_metadata.is_empty() {
        notes.push(MigrationNote::needs_attention(
            &id,
            "output_metadata (required per-output metadata keys) has no manifest-level AstRS \
             equivalent; if these outputs correlate a service/action pattern, set `pattern:` on \
             this node instead (§9.4)"
                .to_string(),
        ));
    }

    if let Some(op) = &dora.operator {
        node.operators = Some(vec![convert_operator(&id, op, notes)]);
    } else if !dora.operators.is_empty() {
        node.operators = Some(
            dora.operators
                .iter()
                .map(|op| convert_operator(&id, op, notes))
                .collect(),
        );
    }

    if let Some(ros2) = &dora.ros2 {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "ros2: bridge configuration carried through as a comment only (blueprint §8.6's \
                 mapping scope does not include ros2:; AstRS's own ros2: shape, §10.5, is not \
                 field-compatible with dora's) — original value: {}",
                render_yaml_inline(ros2)
            ),
        ));
    }

    if let Some(deploy) = &dora.deploy {
        node.deploy = Some(convert_deploy(Some(&id), deploy, notes));
    }

    if dora.module.is_some() {
        notes.push(MigrationNote::needs_attention(
            &id,
            "module: carried through verbatim; if the referenced file is itself a dora module \
             descriptor, run `astrs migrate from-dora` on it too and update this path if the \
             output filename differs"
                .to_string(),
        ));
    }

    for (key, value) in &dora.extra {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "unrecognized field `{key}`; carried through as a comment only — original \
                 value: {}",
                render_yaml_inline(value)
            ),
        ));
    }

    node
}

/// One strongly-connected component's member indices, in the (arbitrary,
/// traversal-order) shape [`strongly_connected_components`] discovers them
/// — callers that need a stable order (a rendered note, a test assertion)
/// sort the node ids they map to, not this.
type Component = Vec<usize>;

/// Tarjan's algorithm: every strongly-connected component of the directed
/// graph `adjacency` describes (`adjacency[i]` lists every `j` with an edge
/// `i -> j`). A singleton component with no self-edge is not "a cycle" in
/// any useful sense but is still returned — [`detect_pattern_free_cycles`],
/// this function's one caller, filters on `component.len() > 1`.
///
/// Recursive rather than an explicit-stack rewrite: dora dataflow graphs
/// are a handful to a few dozen nodes (never a deep recursion risk), and
/// the recursive shape is the textbook one, which matters more here than
/// the last constant factor of performance for a one-shot CLI migration.
fn strongly_connected_components(adjacency: &[Vec<usize>]) -> Vec<Component> {
    struct State {
        index: Vec<Option<usize>>,
        low_link: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next_index: usize,
        components: Vec<Component>,
    }

    fn visit(v: usize, adjacency: &[Vec<usize>], state: &mut State) {
        let v_index = state.next_index;
        state.index[v] = Some(v_index);
        state.low_link[v] = v_index;
        state.next_index += 1;
        state.stack.push(v);
        state.on_stack[v] = true;

        for &w in &adjacency[v] {
            match state.index[w] {
                None => {
                    // `w` unvisited: recurse, then inherit its low-link if
                    // lower — a back-edge reachable through `w` reaches `v`
                    // too.
                    visit(w, adjacency, state);
                    state.low_link[v] = state.low_link[v].min(state.low_link[w]);
                }
                Some(w_index) if state.on_stack[w] => {
                    // `w` is an ancestor still on the stack: a direct back
                    // edge, closing a cycle through `v`.
                    state.low_link[v] = state.low_link[v].min(w_index);
                }
                Some(_) => {
                    // `w` already finished and popped into an earlier
                    // component — cross edge, irrelevant to `v`'s own
                    // component.
                }
            }
        }

        // `v` is its component's root exactly when nothing under it in the
        // DFS tree links back above `v` itself — pop the stack down to and
        // including `v` as one finished component.
        if state.low_link[v] == v_index {
            let mut component = Component::new();
            while let Some(w) = state.stack.pop() {
                state.on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            state.components.push(component);
        }
    }

    let mut state = State {
        index: vec![None; adjacency.len()],
        low_link: vec![0; adjacency.len()],
        on_stack: vec![false; adjacency.len()],
        stack: Vec::new(),
        next_index: 0,
        components: Vec::new(),
    };
    for v in 0..adjacency.len() {
        if state.index[v].is_none() {
            visit(v, adjacency, &mut state);
        }
    }
    state.components
}

/// Flags a converted graph's untagged service/action wiring (blueprint
/// §9.4: "services... ride ordinary edges plus metadata correlation — no
/// separate RPC subsystem", so a request/response or goal/feedback pair
/// looks, structurally, like exactly this — node A's output feeding node
/// B, and B's own output feeding back to A). dora has no `pattern:`
/// concept, so a converted graph that wired one of these never declares
/// it; left that way, the pair gets none of §11.2's queue-eviction
/// immunity for correlated messages, and a busy queue can silently drop
/// the reply. The audit that motivated this reproduced the shape 9 times
/// across dora's own example dataflows (`service-example`,
/// `action-example`, …).
///
/// Scoped to strongly-connected components of size > 1 (a cycle can run
/// through more than two nodes) in the graph of ordinary `node/output`
/// input wiring only — never `astrs/timer|logs|status` virtual sources,
/// which cannot coincide with a real node's id — and only fires when *no*
/// member of the component declares `pattern:` already; one side declaring
/// it is enough to assume the migration (or the human following up on it)
/// has this wiring in hand.
fn detect_pattern_free_cycles(manifest: &astrs_manifest::Manifest, notes: &mut Vec<MigrationNote>) {
    let index_of: BTreeMap<&str, usize> = manifest
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); manifest.nodes.len()];
    for (i, node) in manifest.nodes.iter().enumerate() {
        for input in node.inputs.values() {
            if let Some((prefix, _)) = input.source.split_once('/')
                && let Some(&j) = index_of.get(prefix)
            {
                adjacency[i].push(j);
            }
        }
    }

    for component in strongly_connected_components(&adjacency) {
        if component.len() < 2 {
            continue; // a lone node is never "a cycle among nodes"
        }
        if component
            .iter()
            .any(|&i| manifest.nodes[i].pattern.is_some())
        {
            continue; // at least one side already declares its role
        }
        let mut ids: Vec<&str> = component
            .iter()
            .map(|&i| manifest.nodes[i].id.as_str())
            .collect();
        ids.sort_unstable();
        let names = ids
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(MigrationNote::root_needs_attention(format!(
            "nodes {names} form a cycle with no `pattern:` set on any of them; if this is a \
             service or action request/response pair, set `pattern: service-client`/\
             `service-server` (or `action-client`/`action-server` for a long-running goal) on \
             each side — untagged, the scheduler grants this wiring none of §11.2's queue-\
             eviction immunity for correlated messages, so a busy queue can silently drop the \
             reply"
        )));
    }
}

/// Map a parsed dora descriptor onto an AstRS [`astrs_manifest::Manifest`],
/// collecting a [`MigrationNote`] for every dora construct this importer
/// could not automatically translate.
///
/// The returned manifest is not validated — [`super::migrate_str`] does
/// that after rendering, so a manifest that fails to validate is still
/// visible to the caller as ordinary `astrs validate` diagnostics rather
/// than a migration failure.
pub(crate) fn convert(dora: &DoraManifest) -> (astrs_manifest::Manifest, Vec<MigrationNote>) {
    let mut notes = Vec::new();
    let mut manifest = astrs_manifest::Manifest {
        // `Manifest::default()`'s `#[derive(Default)]` does not know about
        // `#[serde(default = "...")]`'s custom defaulter functions — it
        // zero-initializes `astrs` to `""` and `health_check_interval` to
        // `0.0`. Both are set explicitly below to AstRS's own documented
        // defaults rather than inheriting that mismatch.
        astrs: astrs_manifest::DEFAULT_MANIFEST_FORMAT.to_string(),
        health_check_interval: astrs_manifest::default_health_check_interval(),
        ..astrs_manifest::Manifest::default()
    };

    if let Some(deploy) = &dora.deploy {
        manifest.deploy = Some(convert_deploy(None, deploy, &mut notes));
    }
    if let Some(debug) = &dora.debug {
        manifest.debug = convert_debug(debug, &mut notes);
    }

    manifest.env = convert_env(&dora.env);
    if let Some(interval) = dora.health_check_interval {
        manifest.health_check_interval = interval;
    }
    manifest.exit_when_nodes_finish = dora.exit_when_nodes_finish.unwrap_or(false);
    manifest.strict_types = dora.strict_types.unwrap_or(false);
    manifest.type_rules = dora
        .type_rules
        .iter()
        .map(|rule| astrs_manifest::TypeRule {
            from: astrs_manifest::Urn::new(rule.from.clone()),
            to: astrs_manifest::Urn::new(rule.to.clone()),
        })
        .collect();

    for (key, value) in &dora.extra {
        notes.push(MigrationNote::root_needs_attention(format!(
            "unrecognized root field `{key}` (dora-only, or not yet handled by this importer); \
             dropped — original value: {}",
            render_yaml_inline(value)
        )));
    }

    manifest.nodes = dora
        .nodes
        .iter()
        .map(|n| convert_node(n, &mut notes))
        .collect();
    detect_pattern_free_cycles(&manifest, &mut notes);

    (manifest, notes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::dora::model::{DoraByteSize, DoraEnvValue};

    fn node(id: &str) -> DoraNode {
        astrs_yaml::from_str(&format!("id: {id}\npath: ./{id}\n")).unwrap()
    }

    #[test]
    fn root_defaults_are_astrs_defaults_not_derived_zero_values() {
        let dora = DoraManifest::default();
        let (manifest, _) = convert(&dora);
        assert_eq!(manifest.astrs, astrs_manifest::DEFAULT_MANIFEST_FORMAT);
        assert_eq!(
            manifest.health_check_interval,
            astrs_manifest::default_health_check_interval()
        );
        let yaml = manifest.to_yaml().unwrap();
        assert!(!yaml.contains("astrs: \"\""), "yaml was: {yaml}");
    }

    #[test]
    fn plain_path_node_maps_fields_directly() {
        let mut dora = node("camera");
        dora.outputs = vec!["frames".to_string()];
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(manifest.nodes[0].id, "camera");
        assert_eq!(manifest.nodes[0].path.as_deref(), Some("./camera"));
        assert_eq!(manifest.nodes[0].outputs, vec!["frames".to_string()]);
    }

    #[test]
    fn timer_millis_and_hz_rewrite_prefix_only() {
        assert_eq!(
            rewrite_source("dora/timer/millis/100", "n", "tick", &mut Vec::new()),
            "astrs/timer/millis/100"
        );
        assert_eq!(
            rewrite_source("dora/timer/hz/50", "n", "tick", &mut Vec::new()),
            "astrs/timer/hz/50"
        );
        assert_eq!(
            rewrite_source("dora/timer/secs/2", "n", "tick", &mut Vec::new()),
            "astrs/timer/secs/2"
        );
    }

    #[test]
    fn timer_logs_and_status_rewrite_prefix_only() {
        assert_eq!(
            rewrite_source("dora/logs", "n", "l", &mut Vec::new()),
            "astrs/logs"
        );
        assert_eq!(
            rewrite_source("dora/logs/warn", "n", "l", &mut Vec::new()),
            "astrs/logs/warn"
        );
        assert_eq!(
            rewrite_source("dora/status", "n", "s", &mut Vec::new()),
            "astrs/status"
        );
    }

    #[test]
    fn ordinary_node_output_reference_is_unchanged() {
        assert_eq!(
            rewrite_source("camera/frames", "n", "i", &mut Vec::new()),
            "camera/frames"
        );
    }

    #[test]
    fn fractional_hz_is_rounded_with_a_note() {
        let mut notes = Vec::new();
        let out = rewrite_source("dora/timer/hz/0.5", "n", "tick", &mut notes);
        assert_eq!(out, "astrs/timer/hz/1");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].severity, NoteSeverity::NeedsAttention);
    }

    #[test]
    fn micros_timer_is_approximated_to_millis_when_lossless() {
        let mut notes = Vec::new();
        let out = rewrite_source("dora/timer/micros/5000", "n", "tick", &mut notes);
        assert_eq!(out, "astrs/timer/millis/5");
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn nanos_timer_is_approximated_to_millis() {
        let mut notes = Vec::new();
        let out = rewrite_source("dora/timer/nanos/2000000", "n", "tick", &mut notes);
        assert_eq!(out, "astrs/timer/millis/2");
    }

    #[test]
    fn restart_policy_hyphen_is_rewritten_to_underscore() {
        let mut dora = node("x");
        dora.restart_policy = Some("on-failure".to_string());
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(
            manifest.nodes[0].restart_policy,
            Some(astrs_manifest::RestartPolicy::OnFailure)
        );
    }

    #[test]
    fn restart_policy_never_and_always_need_no_rewrite() {
        for (raw, expected) in [
            ("never", astrs_manifest::RestartPolicy::Never),
            ("always", astrs_manifest::RestartPolicy::Always),
        ] {
            let mut dora = node("x");
            dora.restart_policy = Some(raw.to_string());
            let (manifest, notes) = convert(&DoraManifest {
                nodes: vec![dora],
                ..DoraManifest::default()
            });
            assert!(notes.is_empty());
            assert_eq!(manifest.nodes[0].restart_policy, Some(expected));
        }
    }

    #[test]
    fn queue_policy_and_pattern_need_no_rewrite() {
        let mut dora = node("client");
        dora.pattern = Some("service-client".to_string());
        dora.inputs.insert(
            "resp".to_string(),
            DoraInput::Long {
                source: "server/resp".to_string(),
                queue_size: None,
                queue_policy: Some("backpressure".to_string()),
                input_timeout: None,
            },
        );
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(
            manifest.nodes[0].pattern,
            Some(astrs_manifest::Pattern::ServiceClient)
        );
        assert_eq!(
            manifest.nodes[0].inputs["resp"].queue_policy,
            astrs_manifest::QueuePolicy::Backpressure
        );
    }

    #[test]
    fn input_timeout_renames_to_timeout() {
        let mut dora = node("x");
        dora.inputs.insert(
            "i".to_string(),
            DoraInput::Long {
                source: "y/o".to_string(),
                queue_size: None,
                queue_policy: None,
                input_timeout: Some(1.5),
            },
        );
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        let timeout = manifest.nodes[0].inputs["i"].timeout.unwrap();
        assert!((timeout.as_secs_f64() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn shared_memory_pool_size_renames_to_shm_pool_size() {
        let mut dora = node("x");
        dora.shared_memory_pool_size = Some(DoraByteSize::Text("128MB".to_string()));
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(manifest.nodes[0].shm_pool_size, Some(128 * 1024 * 1024));
    }

    #[test]
    fn args_string_is_shell_tokenized() {
        let mut dora = node("x");
        dora.args = Some("--flag \"quoted value\" plain".to_string());
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(
            manifest.nodes[0].args,
            vec![
                "--flag".to_string(),
                "quoted value".to_string(),
                "plain".to_string()
            ]
        );
    }

    #[test]
    fn unbalanced_quoting_in_args_notes_and_leaves_empty() {
        let mut dora = node("x");
        dora.args = Some("--flag \"unterminated".to_string());
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(manifest.nodes[0].args.is_empty());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn hub_field_produces_a_needs_attention_note_and_no_astrs_field() {
        let mut dora = node("x");
        dora.hub = Some("dora-yolo@^0.5".to_string());
        let (_manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].severity, NoteSeverity::NeedsAttention);
        assert!(notes[0].message.contains("dora-yolo@^0.5"));
    }

    #[test]
    fn output_framing_is_a_dropped_note_not_needs_attention() {
        let mut dora = node("x");
        dora.output_framing.insert(
            "out".to_string(),
            astrs_yaml::Value::String("raw".to_string()),
        );
        let (_manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].severity, NoteSeverity::Dropped);
    }

    #[test]
    fn node_deploy_maps_to_astrs_deploy() {
        let mut dora = node("x");
        dora.deploy = Some(DoraDeploy {
            machine: Some("robot-1".to_string()),
            working_dir: Some("/opt/x".to_string()),
            labels: BTreeMap::from([("gpu".to_string(), "true".to_string())]),
            distribute: None,
            ..DoraDeploy::default()
        });
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
        let deploy = manifest.nodes[0].deploy.as_ref().unwrap();
        assert_eq!(deploy.machine.as_deref(), Some("robot-1"));
        assert_eq!(deploy.working_dir.as_deref(), Some("/opt/x"));
        assert_eq!(deploy.labels.get("gpu"), Some(&"true".to_string()));
    }

    #[test]
    fn non_local_distribute_strategy_is_flagged_on_a_node_deploy() {
        let mut dora = node("x");
        dora.deploy = Some(DoraDeploy {
            distribute: Some("scp".to_string()),
            ..DoraDeploy::default()
        });
        let (_manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.contains("scp"));
        assert_eq!(notes[0].node.as_deref(), Some("x"));
    }

    #[test]
    fn root_deploy_maps_to_astrs_root_deploy() {
        let dora: DoraManifest = astrs_yaml::from_str(
            "nodes: []\ndeploy:\n  machine: robot-1\n  labels:\n    gpu: \"true\"\n",
        )
        .unwrap();
        let (manifest, notes) = convert(&dora);
        assert!(notes.is_empty(), "notes: {notes:?}");
        let deploy = manifest.deploy.as_ref().unwrap();
        assert_eq!(deploy.machine.as_deref(), Some("robot-1"));
        assert_eq!(deploy.labels.get("gpu"), Some(&"true".to_string()));
    }

    #[test]
    fn non_local_distribute_strategy_on_root_deploy_is_a_root_scoped_note() {
        let dora: DoraManifest =
            astrs_yaml::from_str("nodes: []\ndeploy:\n  distribute: http\n").unwrap();
        let (_manifest, notes) = convert(&dora);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.contains("http"));
        assert_eq!(notes[0].node, None, "root deploy is a root-scoped note");
    }

    #[test]
    fn deploy_unrecognized_key_is_a_scoped_note_not_a_silent_drop() {
        let mut dora = node("x");
        dora.deploy = astrs_yaml::from_str("machine: robot-1\nsome_future_field: 1\n").unwrap();
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert_eq!(
            manifest.nodes[0]
                .deploy
                .as_ref()
                .and_then(|d| d.machine.clone()),
            Some("robot-1".to_string()),
            "the recognized field must still map even though a sibling key did not"
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].node.as_deref(), Some("x"));
        assert!(notes[0].message.contains("some_future_field"));
    }

    #[test]
    fn deploy_unrecognized_key_on_root_deploy_is_a_root_scoped_note() {
        let dora: DoraManifest =
            astrs_yaml::from_str("nodes: []\ndeploy:\n  some_future_field: 1\n").unwrap();
        let (_manifest, notes) = convert(&dora);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].node, None);
        assert!(notes[0].message.contains("some_future_field"));
    }

    #[test]
    fn root_debug_enable_flag_maps_to_astrs_root_debug_bool() {
        let dora: DoraManifest =
            astrs_yaml::from_str("nodes: []\ndebug:\n  enable_debug_inspection: true\n").unwrap();
        let (manifest, notes) = convert(&dora);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(manifest.debug);
    }

    #[test]
    fn root_debug_false_maps_to_astrs_root_debug_false() {
        let dora: DoraManifest =
            astrs_yaml::from_str("nodes: []\ndebug:\n  enable_debug_inspection: false\n").unwrap();
        let (manifest, notes) = convert(&dora);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert!(!manifest.debug);
    }

    #[test]
    fn debug_unrecognized_key_is_a_root_scoped_note_not_a_silent_drop() {
        let dora: DoraManifest = astrs_yaml::from_str(
            "nodes: []\ndebug:\n  enable_debug_inspection: true\n  \
             publish_all_messages_to_zenoh: true\n",
        )
        .unwrap();
        let (manifest, notes) = convert(&dora);
        assert!(manifest.debug, "the recognized field must still map");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].node, None);
        assert!(notes[0].message.contains("publish_all_messages_to_zenoh"));
    }

    #[test]
    fn deploy_and_debug_are_recognized_identically_under_the_legacy_underscore_spelling() {
        let dora: DoraManifest = astrs_yaml::from_str(
            "nodes: []\n_unstable_deploy:\n  machine: robot-1\n_unstable_debug:\n  \
             enable_debug_inspection: true\n",
        )
        .unwrap();
        let (manifest, notes) = convert(&dora);
        assert!(notes.is_empty(), "notes: {notes:?}");
        assert_eq!(
            manifest.deploy.as_ref().and_then(|d| d.machine.clone()),
            Some("robot-1".to_string())
        );
        assert!(manifest.debug);
    }

    #[test]
    fn single_operator_form_maps_wiring_and_flags_the_source() {
        let mut dora = node("runtime");
        dora.path = None;
        dora.operator = Some(astrs_yaml::from_str(
            "id: op\npython: script.py\ninputs:\n  tick: dora/timer/millis/100\noutputs: [data]\n",
        )
        .unwrap());
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        let ops = manifest.nodes[0].operators.as_ref().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, "op");
        assert_eq!(ops[0].operator, "TODO_REIMPLEMENT_op");
        assert_eq!(ops[0].inputs["tick"].source, "astrs/timer/millis/100");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.contains("python"));
    }

    #[test]
    fn ros2_block_is_never_translated_only_noted() {
        let mut dora = node("bridge");
        dora.ros2 = Some(astrs_yaml::from_str("topic: /scan\ndirection: from_astrs\n").unwrap());
        let (manifest, notes) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert!(manifest.nodes[0].ros2.is_none());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.contains("/scan"));
    }

    #[test]
    fn unrecognized_root_field_becomes_a_root_scoped_note() {
        let dora: DoraManifest =
            astrs_yaml::from_str("nodes: []\nsome_future_root_thing: 1\n").unwrap();
        let (_manifest, notes) = convert(&dora);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].node, None);
    }

    #[test]
    fn env_values_convert_variant_for_variant() {
        let mut dora = node("x");
        dora.env.insert("A".to_string(), DoraEnvValue::Bool(true));
        dora.env.insert("B".to_string(), DoraEnvValue::Float(1.5));
        let (manifest, _) = convert(&DoraManifest {
            nodes: vec![dora],
            ..DoraManifest::default()
        });
        assert_eq!(
            manifest.nodes[0].env.get("A"),
            Some(&astrs_manifest::EnvValue::Bool(true))
        );
        assert_eq!(
            manifest.nodes[0].env.get("B"),
            Some(&astrs_manifest::EnvValue::Float(1.5))
        );
    }

    #[test]
    fn scc_reports_only_singletons_for_a_linear_chain() {
        // a -> b -> c, no back edge anywhere.
        let adjacency = vec![vec![1], vec![2], vec![]];
        let components = strongly_connected_components(&adjacency);
        assert!(
            components.iter().all(|c| c.len() == 1),
            "components: {components:?}"
        );
    }

    #[test]
    fn scc_finds_a_two_node_cycle() {
        let adjacency = vec![vec![1], vec![0]];
        let mut big: Vec<Vec<usize>> = strongly_connected_components(&adjacency)
            .into_iter()
            .filter(|c| c.len() > 1)
            .collect();
        assert_eq!(big.len(), 1, "components: {big:?}");
        big[0].sort_unstable();
        assert_eq!(big[0], vec![0, 1]);
    }

    #[test]
    fn scc_finds_a_longer_cycle_through_three_nodes() {
        let adjacency = vec![vec![1], vec![2], vec![0]];
        let mut big: Vec<Vec<usize>> = strongly_connected_components(&adjacency)
            .into_iter()
            .filter(|c| c.len() > 1)
            .collect();
        assert_eq!(big.len(), 1, "components: {big:?}");
        big[0].sort_unstable();
        assert_eq!(big[0], vec![0, 1, 2]);
    }

    #[test]
    fn scc_keeps_two_disjoint_cycles_separate() {
        // 0<->1 and 2<->3; nothing connects the two pairs.
        let adjacency = vec![vec![1], vec![0], vec![3], vec![2]];
        let mut components: Vec<Vec<usize>> = strongly_connected_components(&adjacency)
            .into_iter()
            .filter(|c| c.len() > 1)
            .collect();
        for component in &mut components {
            component.sort_unstable();
        }
        components.sort();
        assert_eq!(components, vec![vec![0, 1], vec![2, 3]]);
    }

    /// A minimal client/server pair whose wiring is a two-node cycle
    /// (client's `response` sources server's output, server's `request`
    /// sources client's output) — the shape `timer_restart_service.yml`
    /// exercises with `pattern:` declared, and this test exercises without
    /// it, matching the audit's own reproduction (dora's `service-example`,
    /// `action-example`, and 7 more of its 94 example dataflows wire this
    /// same shape and, being dora, never declare a `pattern:` for it).
    fn client_server_cycle() -> DoraManifest {
        let mut client = node("client");
        client.inputs.insert(
            "response".to_string(),
            DoraInput::Short("server/response".to_string()),
        );
        client.outputs = vec!["request".to_string()];

        let mut server = node("server");
        server.inputs.insert(
            "request".to_string(),
            DoraInput::Short("client/request".to_string()),
        );
        server.outputs = vec!["response".to_string()];

        DoraManifest {
            nodes: vec![client, server],
            ..DoraManifest::default()
        }
    }

    #[test]
    fn an_untagged_service_style_cycle_gets_a_root_level_pattern_note() {
        let (_manifest, notes) = convert(&client_server_cycle());

        let cycle_notes: Vec<_> = notes
            .iter()
            .filter(|n| n.message.contains("form a cycle"))
            .collect();
        assert_eq!(cycle_notes.len(), 1, "notes: {notes:?}");
        let note = cycle_notes[0];
        assert_eq!(note.node, None, "a cycle spans nodes, so it is root-level");
        assert_eq!(note.severity, NoteSeverity::NeedsAttention);
        assert!(note.message.contains("`client`"), "{}", note.message);
        assert!(note.message.contains("`server`"), "{}", note.message);
        assert!(
            note.message.contains("pattern: service-client")
                && note.message.contains("service-server")
                && note.message.contains("action-client")
                && note.message.contains("action-server"),
            "{}",
            note.message
        );
    }

    #[test]
    fn a_cycle_is_not_flagged_once_either_side_declares_a_pattern() {
        let mut dora = client_server_cycle();
        dora.nodes[0].pattern = Some("service-client".to_string());
        let (_manifest, notes) = convert(&dora);
        assert!(
            notes.iter().all(|n| !n.message.contains("form a cycle")),
            "notes: {notes:?}"
        );
    }

    #[test]
    fn an_acyclic_graph_never_gets_a_cycle_note() {
        let mut upstream = node("camera");
        upstream.outputs = vec!["frames".to_string()];
        let mut downstream = node("recorder");
        downstream.inputs.insert(
            "frames".to_string(),
            DoraInput::Short("camera/frames".to_string()),
        );
        let (_manifest, notes) = convert(&DoraManifest {
            nodes: vec![upstream, downstream],
            ..DoraManifest::default()
        });
        assert!(notes.is_empty(), "notes: {notes:?}");
    }
}
