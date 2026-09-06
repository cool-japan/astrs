//! The hidden verbs: `astrs coordinator`, `astrs daemon`, `astrs runtime`
//! (blueprint §17's "Internal (hidden)" row).
//!
//! These are what make the `astrs` binary *self-contained*: `astrs up`
//! brings a cluster up by spawning **itself** twice rather than by requiring
//! two more executables on the machine, and a multi-machine deployment
//! (§4.2) copies one file. Each verb is a thin, honest wiring of the crate
//! that owns the behavior — nothing here re-implements a server.
//!
//! ```text
//!   astrs coordinator ─► astrs_coordinator::CoordinatorServer::{bind,serve}
//!   astrs daemon      ─► astrs_daemon::Daemon::{new,bind,run}
//!   astrs runtime     ─► astrs_runtime::RuntimeHost::run   (node-shaped)
//! ```
//!
//! # The three things every server verb here does for its parent
//!
//! 1. **Binds first, announces second.** The port is only known after the
//!    bind when `--port 0` was asked for, so nothing is written anywhere
//!    until the listener exists.
//! 2. **Writes a pidfile** (§24.2) holding `"<pid>\n<address>\n"`. This is
//!    the *only* channel `astrs up` uses to learn a child's real address:
//!    reading a long-lived child's stdout pipe would either block on a pipe
//!    that never closes or leave the child writing into a closed one.
//! 3. **Stops gracefully on `SIGINT`/`SIGTERM`**, then removes its pidfile,
//!    so `astrs down`'s polite `SIGTERM` is enough and its `SIGKILL`
//!    escalation stays the exception.
//!
//! # Why the coordinator binds loopback by default
//!
//! Blueprint §4.2 puts the coordinator on TCP 7407 with daemons dialling
//! *out* to it, and §16 gives the cluster one shared token. A coordinator
//! bound to every interface before an operator has said so is a control
//! plane exposed to the whole network, so `--bind` defaults to loopback and
//! a real multi-machine cluster opts in with `--bind 0.0.0.0`.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_daemon::config::{ListenConfig, RuntimePaths};
use astrs_daemon::{Daemon, DaemonConfig};
use astrs_manifest::OperatorConfig;
use astrs_node_api::Node;
use astrs_operator_api::OperatorRegistry;
use astrs_runtime::{RuntimeConfig, RuntimeHost};
use astrs_store::{AsyncStore, CoordinatorStore};
use astrs_wire::{AuthToken, MachineName, NodeSource};

use crate::command::client::resolve_text;
use crate::command::signals::Signals;
use crate::error::CliError;
use crate::runtime_dir::{PidFile, remove_pidfile, write_pidfile};

/// The file name the coordinator's store gets inside the runtime directory
/// when `--store` is not given an explicit path.
pub const STORE_FILE: &str = "coordinator.redb";

/// What one server verb did, from bind to shutdown.
#[derive(Debug, Clone)]
pub struct ServeReport {
    /// Which server ran: `"coordinator"`, `"daemon"` or `"runtime"`.
    pub role: &'static str,
    /// The process that ran it.
    pub pid: u32,
    /// Where it listened (or, for a runtime, what it hosted).
    pub address: String,
    /// A human note worth surfacing — an unimplemented uplink, an
    /// unauthenticated listener, how many dataflows finished.
    pub detail: String,
    /// Whether what this process hosted ended healthy — always true for a
    /// server that was asked to stop, and the operators' verdict for
    /// [`runtime`].
    pub healthy: bool,
}

impl ServeReport {
    /// The process exit code this run should produce: `0` unless something
    /// this process hosted failed.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.healthy { 0 } else { 1 }
    }

    /// The one-line announcement a parent process (and a log file) reads.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "role": self.role,
            "pid": self.pid,
            "address": self.address,
            "detail": self.detail,
            "healthy": self.healthy,
        })
    }
}

/// `astrs coordinator`'s arguments, already parsed.
#[derive(Debug, Clone, Default)]
pub struct CoordinatorServeArgs {
    /// The TCP port to listen on; `0` binds a free one.
    pub port: u16,
    /// The interface to bind; loopback when absent.
    pub bind: Option<String>,
    /// The cluster token, inline (§16).
    pub token: Option<String>,
    /// The file holding that token.
    pub token_file: Option<PathBuf>,
    /// Where `.astrs-token` is looked for.
    pub working_dir: Option<PathBuf>,
    /// The parameter/state store file; in-memory when absent.
    pub store: Option<PathBuf>,
    /// Delete the store before opening it.
    pub recreate_store: bool,
    /// Where to record the pid and the bound address (§24.2).
    pub pidfile: Option<PathBuf>,
    /// Print the announce line once the listener is up.
    pub announce: bool,
    /// This coordinator's Raft peer id within a replicated set (§22).
    pub ha_node_id: Option<u64>,
    /// The replicated set, as `id=host:port` specifications.
    pub ha_peers: Vec<String>,
}

/// Runs a coordinator until it is signalled to stop.
///
/// # Errors
///
/// - [`CliError::BadAddress`] if `--bind` cannot be resolved.
/// - [`CliError::BadToken`] if a token was supplied but is not 64 hex
///   digits.
/// - [`CliError::Store`] if the store cannot be opened or deleted.
/// - [`CliError::Coordinator`] if the listener cannot be bound.
/// - [`CliError::Io`] if the pidfile cannot be written or the runtime
///   cannot be built.
pub fn coordinator(
    out: &mut dyn Write,
    args: &CoordinatorServeArgs,
) -> Result<ServeReport, CliError> {
    let addr = bind_addr(args.bind.as_deref(), args.port)?;
    let working = crate::runtime_dir::working_dir(args.working_dir.as_deref());
    let (token, token_note) =
        resolve_token(args.token.as_deref(), args.token_file.as_deref(), &working)?;
    let store = open_store(args.store.as_deref(), args.recreate_store)?;

    let config = CoordinatorConfig::new(token).with_bind_addr(addr);
    let hub = Coordinator::new(config, store);
    install_trace_sink(&hub);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::io("tokio runtime", source))?;

    runtime.block_on(async move {
        // Armed *before* the pidfile exists, so a parent that waits for the
        // pidfile (which is how `astrs up` learns the port) can never signal
        // this process during the window where `SIGTERM` would still be
        // fatal rather than graceful.
        let mut signals = Signals::install();
        // Blueprint §22: joining a replicated set has to happen before the CLI
        // listener accepts anything, or the first request could reach a
        // coordinator that has not yet decided whether it leads.
        let hub = start_high_availability(hub, args).await?;
        let server = CoordinatorServer::bind(hub).await?;
        let bound = server.local_addr()?;
        let handle = server.handle();

        let report = ServeReport {
            role: "coordinator",
            pid: std::process::id(),
            address: bound.to_string(),
            detail: token_note,
            healthy: true,
        };
        if let Some(path) = &args.pidfile {
            write_pidfile(path, &PidFile::listening(report.pid, bound.to_string()))?;
        }
        announce(out, args.announce, &report);

        let mut serving = tokio::spawn(server.serve());
        tokio::select! {
            joined = &mut serving => finish_join(joined)?,
            () = signals.next() => {
                let _ = writeln!(out, "astrs coordinator: stopping on a signal");
                let _ = out.flush();
                handle.shutdown();
                finish_join(serving.await)?;
            }
        }
        if let Some(path) = &args.pidfile {
            remove_pidfile(path)?;
        }
        Ok::<ServeReport, CliError>(report)
    })
}

/// Joins this coordinator to a replicated set, if `--ha-node-id`/`--ha-peer`
/// asked for one (blueprint §22).
///
/// Returns the hub unchanged when no HA flags were given, which is the
/// ordinary single-coordinator deployment.
#[cfg(feature = "ha")]
async fn start_high_availability(
    hub: Coordinator,
    args: &CoordinatorServeArgs,
) -> Result<Coordinator, CliError> {
    let Some(node_id) = args.ha_node_id else {
        if !args.ha_peers.is_empty() {
            return Err(CliError::BadArgument {
                flag: "ha-peer",
                value: args.ha_peers.join(", "),
                reason: "--ha-peer needs --ha-node-id to say which of those peers this \
                         coordinator is"
                    .to_owned(),
            });
        }
        return Ok(hub);
    };
    let mut config = astrs_coordinator::ha::HaConfig::parse(node_id, &args.ha_peers)?;
    // The Raft log lives beside the store it replicates, so one `--store`
    // directory holds one coordinator's whole durable state.
    if let Some(store) = &args.store
        && let Some(directory) = store.parent()
        && !directory.as_os_str().is_empty()
    {
        config = config.with_log_dir(directory);
    }
    let handle = std::sync::Arc::new(astrs_coordinator::ha::HaHandle::start(&hub, config).await?);
    Ok(hub.with_ha(handle))
}

/// Refuses the HA flags in a build that has no consensus code in it.
///
/// Silently ignoring them would be worse than refusing: a deployment that
/// believed it had three replicated coordinators would have three independent
/// ones, each accepting writes the others never see.
#[cfg(not(feature = "ha"))]
async fn start_high_availability(
    hub: Coordinator,
    args: &CoordinatorServeArgs,
) -> Result<Coordinator, CliError> {
    if args.ha_node_id.is_some() || !args.ha_peers.is_empty() {
        return Err(CliError::BadArgument {
            flag: "ha-node-id",
            value: args
                .ha_node_id
                .map_or_else(|| args.ha_peers.join(", "), |id| id.to_string()),
            reason: "this build has no coordinator high availability; rebuild the CLI with the \
                     `ha` feature"
                .to_owned(),
        });
    }
    Ok(hub)
}

/// Flattens a joined `serve` task into this crate's error type.
///
/// A task that *panicked* is reported as a coordinator failure rather than
/// re-panicked: this process is a server whose job includes dying legibly.
fn finish_join(
    joined: Result<astrs_coordinator::Result<()>, tokio::task::JoinError>,
) -> Result<(), CliError> {
    match joined {
        Ok(result) => Ok(result?),
        Err(error) => Err(CliError::Cluster {
            action: "run",
            process: "coordinator",
            reason: format!("the accept loop ended abnormally: {error}"),
        }),
    }
}

/// `astrs daemon`'s arguments, already parsed.
#[derive(Debug, Clone, Default)]
pub struct DaemonServeArgs {
    /// The coordinator to register with (§4.2). `None` runs a purely local
    /// daemon, which is what `astrs run` embeds.
    pub coordinator: Option<String>,
    /// This machine's name, as a manifest's `deploy.machine` spells it.
    pub machine: Option<String>,
    /// A loopback TCP port for nodes that cannot use the socket; `None`
    /// opens no TCP listener at all (§4.2: UDS preferred).
    pub port: Option<u16>,
    /// The TCP port other daemons dial for cross-machine routes (§6.4).
    pub peer_port: u16,
    /// The interface the peer listener binds; loopback when absent (§16).
    pub peer_bind: Option<String>,
    /// Placement labels, as `KEY=VALUE` strings (§8.3 `deploy`).
    pub labels: Vec<String>,
    /// Where the node socket and captured logs live (§24.2).
    pub runtime_dir: Option<PathBuf>,
    /// The directory node paths resolve against.
    pub working_dir: Option<PathBuf>,
    /// The cluster token every node must present (§16).
    pub token: Option<String>,
    /// The file holding that token.
    pub token_file: Option<PathBuf>,
    /// Where to record the pid and the bound endpoints (§24.2).
    pub pidfile: Option<PathBuf>,
    /// Print the announce line once the listeners are up.
    pub announce: bool,
}

/// Runs this machine's daemon until it is signalled to stop.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if `--machine` is not a usable machine name.
/// - [`CliError::BadToken`] if a token was supplied but is not 64 hex
///   digits.
/// - [`CliError::Daemon`] if the listeners cannot be bound.
/// - [`CliError::Io`] if the pidfile cannot be written or the runtime
///   cannot be built.
pub fn daemon(out: &mut dyn Write, args: &DaemonServeArgs) -> Result<ServeReport, CliError> {
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    let working = crate::runtime_dir::working_dir(args.working_dir.as_deref());
    let (token, token_note) =
        resolve_token(args.token.as_deref(), args.token_file.as_deref(), &working)?;

    let paths = RuntimePaths::under(&runtime_dir);
    let mut listen = ListenConfig::uds(paths.socket_path());
    if let Some(port) = args.port {
        listen = listen.with_tcp_port(port);
    }
    let mut config = DaemonConfig::new(paths)
        .with_listen(listen)
        .with_auth(token.clone())
        .with_working_dir(working);
    let machine = machine_name(args.machine.as_deref())?;
    if let Some(machine) = machine.clone() {
        config = config.with_machine(machine);
    }

    // The peer listener (§6.4) is opened only for a daemon that has a
    // coordinator: a standalone daemon has no peers, and binding a port
    // nobody was told about is a hole for nothing (§16).
    let uplink = match &args.coordinator {
        Some(address) => {
            let address = astrs_daemon::coordinator::resolve_coordinator_addr(address)
                .map_err(CliError::Daemon)?;
            config = config.with_peer(peer_config(&token, args)?);
            let mut uplink = astrs_daemon::coordinator::UplinkConfig::new(address, token);
            if let Some(machine) = machine {
                uplink = uplink.with_machine(machine);
            }
            uplink = uplink.with_labels(parse_labels(&args.labels)?);
            Some(uplink)
        }
        None => None,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::io("tokio runtime", source))?;

    runtime.block_on(async move {
        // Armed before anything observable exists, for the reason
        // [`coordinator`] spells out: the pidfile is a parent's signal that
        // signalling is safe.
        let mut signals = Signals::install();
        let mut daemon = Daemon::new(config)?;
        daemon.bind().await?;
        let endpoints = daemon.config().listen().endpoints().join(", ");

        let mut detail = token_note;
        if let Some(uplink) = uplink {
            let coordinator = uplink.address();
            daemon.connect_coordinator(uplink).await?;
            let peer = daemon
                .peers()
                .listen_addr()
                .map_or_else(|| "no peer listener".to_owned(), |addr| addr.to_string());
            detail.push_str(&format!(
                "; registering with the coordinator at {coordinator} (peers dial {peer})"
            ));
        } else {
            detail.push_str("; no coordinator — this daemon serves local nodes only");
        }
        let report = ServeReport {
            role: "daemon",
            pid: std::process::id(),
            address: endpoints.clone(),
            detail,
            healthy: true,
        };
        if let Some(path) = &args.pidfile {
            write_pidfile(path, &PidFile::listening(report.pid, endpoints))?;
        }
        announce(out, args.announce, &report);

        let handle = daemon.handle();
        tokio::spawn(async move {
            signals.next().await;
            // `DaemonEvent::Shutdown` is what the merged event loop turns
            // into `begin_shutdown()`; nothing else may touch the daemon
            // while `run` holds it.
            handle.shutdown();
        });

        let results = daemon.run().await;
        if let Some(path) = &args.pidfile {
            remove_pidfile(path)?;
        }
        let finished = results.len();
        let _ = writeln!(
            out,
            "astrs daemon: stopped after {finished} dataflow(s) finished here"
        );
        let _ = out.flush();
        Ok::<ServeReport, CliError>(ServeReport {
            detail: format!("{} ({finished} dataflow(s) finished)", report.detail),
            ..report
        })
    })
}

/// `astrs runtime`'s arguments, already parsed.
#[derive(Debug, Clone, Default)]
pub struct RuntimeServeArgs {
    /// The node id this process expects to host operators for; advisory,
    /// since the authoritative id arrives in the handshake blob.
    pub node_id: Option<String>,
}

/// Hosts a manifest node's `operators:` in this process.
///
/// Unlike the other two verbs here, this one is *spawned by a daemon*, not
/// by `astrs up`: it is a node-shaped process that connects with
/// [`astrs_node_api::Node::init_from_env`] and is told what to host by the
/// handshake blob (§4.2). It has no listener, so no pidfile and no
/// announcement.
///
/// # Errors
///
/// - [`CliError::BadArgument`] if `--node-id` disagrees with the id the
///   daemon actually assigned — a misconfiguration worth refusing rather
///   than silently hosting the wrong node's operators.
/// - [`CliError::Node`] if this process cannot connect to its daemon.
/// - [`CliError::Runtime`] if the operator host cannot be built or fails.
pub fn runtime(out: &mut dyn Write, args: &RuntimeServeArgs) -> Result<ServeReport, CliError> {
    let (node, events) = Node::init_from_env()?;
    let assigned = node.descriptor().node.as_str().to_owned();
    if let Some(expected) = &args.node_id
        && expected != &assigned
    {
        return Err(CliError::BadArgument {
            flag: "node-id",
            value: expected.clone(),
            reason: format!("the daemon assigned this process the node `{assigned}`"),
        });
    }

    // The wire's `OperatorSpec` carries an id, a registry name and static
    // config, but not the per-operator input/output wiring
    // `astrs_manifest::OperatorConfig` models — see `astrs-runtime`'s own
    // `main.rs`, which builds exactly this shape for exactly this reason.
    let operators: Vec<OperatorConfig> = match &node.descriptor().source {
        NodeSource::Runtime { operators } => operators
            .iter()
            .map(|spec| OperatorConfig {
                id: spec.id.as_str().to_owned(),
                operator: spec.registry_name.clone(),
                dylib: None,
                wasm: None,
                hub: None,
                inputs: std::collections::BTreeMap::new(),
                outputs: Vec::new(),
                config: std::collections::BTreeMap::new(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let hosted = operators.len();

    // This binary registers no operators of its own (blueprint §9.3:
    // operators compile *into* a runtime binary, statically). A deployment
    // that hosts real operators links its own binary against
    // `astrs-runtime` and passes its own registry here.
    let config = RuntimeConfig::new(operators, OperatorRegistry::new());
    let host = RuntimeHost::new(node, events, config)?;
    let report = host.run()?;

    for operator in &report.operators {
        let _ = writeln!(out, "{}: {:?}", operator.id, operator.outcome);
    }
    let _ = out.flush();
    Ok(ServeReport {
        role: "runtime",
        pid: std::process::id(),
        address: assigned,
        detail: format!(
            "{hosted} operator(s) hosted, {}",
            if report.all_healthy() {
                "all healthy"
            } else {
                "at least one failed"
            }
        ),
        healthy: report.all_healthy(),
    })
}

/// Resolves `--bind`/`--port` into the address to listen on.
///
/// `--bind` may name an interface (`0.0.0.0`), a full `host:port` (whose
/// port wins over `--port`, because it is the more specific spelling), or
/// nothing at all — loopback, per this module's own docs.
///
/// # Errors
///
/// [`CliError::BadAddress`] if the interface cannot be resolved.
fn bind_addr(bind: Option<&str>, port: u16) -> Result<SocketAddr, CliError> {
    let Some(text) = bind else {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    };
    let trimmed = text.trim();
    if trimmed.contains(':') {
        return resolve_text(trimmed);
    }
    let mut addr = resolve_text(trimmed)?;
    addr.set_port(port);
    Ok(addr)
}

/// Finds the cluster token, falling back to the all-zero one with a note
/// saying so.
///
/// A server that refused to start without a token would make `astrs
/// coordinator` unusable for the first thing anyone does with it, and a
/// server that silently accepted anything would be a security hole nobody
/// was told about. The fallback is therefore explicit *and* announced.
///
/// # Errors
///
/// As [`crate::runtime_dir::find_token`].
fn resolve_token(
    token: Option<&str>,
    token_file: Option<&Path>,
    working_dir: &Path,
) -> Result<(AuthToken, String), CliError> {
    match crate::runtime_dir::find_token(token, token_file, working_dir)? {
        Some((token, source)) => Ok((
            token,
            format!("authenticating with the token from {source}"),
        )),
        None => Ok((
            AuthToken::ZERO,
            format!(
                "no cluster token found (looked for {}, then `{}`): accepting the all-zero token, \
                 so anyone who can reach this listener can drive it",
                crate::runtime_dir::ENV_TOKEN,
                crate::runtime_dir::token_path(working_dir).display()
            ),
        )),
    }
}

/// Opens the coordinator's store, honoring `--recreate-store`.
///
/// # Errors
///
/// - [`CliError::Io`] if an existing store cannot be deleted.
/// - [`CliError::Store`] if the store cannot be opened.
fn open_store(path: Option<&Path>, recreate: bool) -> Result<AsyncStore, CliError> {
    let Some(path) = path else {
        return Ok(AsyncStore::new(CoordinatorStore::open_in_memory()?));
    };
    if recreate {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(CliError::io(path, source)),
        }
    }
    if let Some(parent) = path.parent() {
        crate::runtime_dir::ensure_dir(parent)?;
    }
    Ok(AsyncStore::new(CoordinatorStore::open(path)?))
}

/// Installs `astrs-telemetry`'s subscriber for this process, with its
/// span layer's sink wired straight into `hub`'s own request-span buffer
/// via [`Coordinator::trace_sink`] — what makes `GetTraces` (blueprint
/// §13, §17) genuinely answer from astrs-telemetry's finished-span
/// collection rather than only from `astrs-coordinator`'s own per-request
/// bookkeeping (`handlers::dispatch`, private to that crate — see its doc
/// comment for how the two now compose: the manual bookkeeping still runs
/// unconditionally, so every existing test that opens a bare `Coordinator`
/// with no subscriber installed sees exactly what it always has, and a
/// real span, entered around the same dispatch, additionally reaches this
/// sink whenever a subscriber *is* installed — exactly the case this
/// function exists for).
///
/// Best-effort and silent on failure: a coordinator's job is to run the
/// cluster's control plane, and losing the span feed (a second install in
/// the same process — every `#[test]` in this crate that calls
/// [`coordinator`] directly shares one process under plain `cargo test`,
/// though not under `cargo nextest`'s per-test processes; an invalid
/// `RUST_LOG` directive; any other `TelemetryError`) must never be a
/// reason this function's caller refuses to start.
fn install_trace_sink(hub: &Coordinator) {
    let directives = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned());
    let config = astrs_telemetry::subscriber::TelemetryConfig {
        service_name: "astrs-coordinator".to_owned(),
        filter_directives: directives,
        ..astrs_telemetry::subscriber::TelemetryConfig::default()
    };
    let _ = astrs_telemetry::subscriber::init_telemetry_with_spans(config, hub.trace_sink());
}

/// Validates a `--machine` argument.
///
/// # Errors
///
/// [`CliError::BadArgument`] naming what a machine name may contain.
fn machine_name(machine: Option<&str>) -> Result<Option<MachineName>, CliError> {
    let Some(text) = machine else {
        return Ok(None);
    };
    MachineName::new(text)
        .map(Some)
        .map_err(|error| CliError::BadArgument {
            flag: "machine",
            value: text.to_owned(),
            reason: error.to_string(),
        })
}

/// The peer configuration a cluster daemon listens and dials with (§6.4).
///
/// Loopback by default, exactly as the coordinator's own `--bind` is and for
/// the same §16 reason: a data-plane port reachable from the whole network
/// before an operator has said so is a hole opened by default.
///
/// # Errors
///
/// [`CliError::BadAddress`] if `--peer-bind` cannot be resolved.
fn peer_config(
    token: &AuthToken,
    args: &DaemonServeArgs,
) -> Result<astrs_daemon::PeerConfig, CliError> {
    let config = astrs_daemon::PeerConfig::new(token.clone());
    let Some(bind) = args.peer_bind.as_deref() else {
        return Ok(config.with_loopback(args.peer_port));
    };
    let addr = resolve_bind(bind, args.peer_port)?;
    Ok(config.with_listen(addr))
}

/// Resolves a `--peer-bind` interface to a socket address.
///
/// # Errors
///
/// [`CliError::BadAddress`] if nothing resolves.
fn resolve_bind(bind: &str, port: u16) -> Result<SocketAddr, CliError> {
    use std::net::ToSocketAddrs as _;

    let candidate = if bind.contains(':') {
        bind.to_owned()
    } else {
        format!("{bind}:{port}")
    };
    candidate
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .ok_or_else(|| CliError::BadAddress {
            input: bind.to_owned(),
            reason: "no address resolves for it".to_owned(),
        })
}

/// Parses repeated `--label KEY=VALUE` arguments (§8.3 `deploy`).
///
/// # Errors
///
/// [`CliError::BadArgument`] for a label with no `=`, or an empty key.
fn parse_labels(labels: &[String]) -> Result<BTreeMap<String, String>, CliError> {
    let mut parsed = BTreeMap::new();
    for label in labels {
        let Some((key, value)) = label.split_once('=') else {
            return Err(CliError::BadArgument {
                flag: "label",
                value: label.clone(),
                reason: "a label is written KEY=VALUE".to_owned(),
            });
        };
        if key.is_empty() {
            return Err(CliError::BadArgument {
                flag: "label",
                value: label.clone(),
                reason: "a label's key may not be empty".to_owned(),
            });
        }
        parsed.insert(key.to_owned(), value.to_owned());
    }
    Ok(parsed)
}

/// Prints the one-line announcement, when asked for.
///
/// One JSON object on one line, flushed: whatever is reading this stream is
/// a log file or a human, and both want the whole line at once rather than
/// whenever a buffer happens to fill.
fn announce(out: &mut dyn Write, enabled: bool, report: &ServeReport) {
    if !enabled {
        return;
    }
    let line = serde_json::to_string(&report.to_json())
        .unwrap_or_else(|_| format!("{{\"role\":\"{}\"}}", report.role));
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-serve-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_bind_argument_means_loopback_on_the_asked_for_port() {
        let addr = bind_addr(None, 7407).unwrap();
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 7407)));
    }

    #[test]
    fn an_interface_keeps_the_port_flag() {
        let addr = bind_addr(Some("0.0.0.0"), 9000).unwrap();
        assert_eq!(addr, SocketAddr::from(([0, 0, 0, 0], 9000)));
    }

    #[test]
    fn a_full_host_and_port_wins_over_the_port_flag() {
        let addr = bind_addr(Some("127.0.0.1:9100"), 7407).unwrap();
        assert_eq!(addr.port(), 9100);
    }

    #[test]
    fn an_unresolvable_interface_is_refused() {
        let error = bind_addr(Some("no-such-host.invalid"), 1).unwrap_err();
        assert!(matches!(error, CliError::BadAddress { .. }), "{error}");
    }

    #[test]
    fn a_missing_token_falls_back_to_zero_and_says_so_loudly() {
        let dir = scratch("no-token");
        let (token, note) = resolve_token(None, None, &dir).unwrap();
        assert!(token.is_zero());
        assert!(note.contains("all-zero"), "{note}");
    }

    #[test]
    fn a_token_file_is_used_and_named_in_the_note() {
        let dir = scratch("token");
        let (written, path) = crate::runtime_dir::ensure_token_file(&dir).unwrap();
        let (token, note) = resolve_token(None, None, &dir).unwrap();
        assert_eq!(token, written);
        assert!(note.contains(&path.display().to_string()), "{note}");
    }

    #[test]
    fn a_store_path_is_created_and_recreated_on_request() {
        let dir = scratch("store");
        let path = dir.join("nested").join(STORE_FILE);
        let store = open_store(Some(&path), false).unwrap();
        drop(store);
        assert!(path.exists(), "the store file must exist after opening it");

        let before = std::fs::metadata(&path).unwrap().len();
        let store = open_store(Some(&path), true).unwrap();
        drop(store);
        assert!(path.exists());
        let _ = before;
    }

    #[test]
    fn no_store_path_means_an_in_memory_store() {
        // Nothing to assert on disk: the point is only that it opens.
        let store = open_store(None, false).unwrap();
        drop(store);
    }

    #[test]
    fn a_machine_name_is_validated() {
        assert!(machine_name(None).unwrap().is_none());
        assert_eq!(
            machine_name(Some("robot-1"))
                .unwrap()
                .map(|m| m.to_string()),
            Some("robot-1".to_owned())
        );
        let error = machine_name(Some("robot 1")).unwrap_err();
        match error {
            CliError::BadArgument { flag, .. } => assert_eq!(flag, "machine"),
            other => panic!("expected BadArgument, got {other}"),
        }
    }

    #[test]
    fn the_announce_line_is_one_json_object_and_is_skipped_when_off() {
        let report = ServeReport {
            role: "coordinator",
            pid: 42,
            address: "127.0.0.1:7407".to_owned(),
            detail: "note".to_owned(),
            healthy: true,
        };
        let mut out = Vec::new();
        announce(&mut out, false, &report);
        assert!(out.is_empty());

        announce(&mut out, true, &report);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let value: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(value["role"], "coordinator");
        assert_eq!(value["address"], "127.0.0.1:7407");
        assert_eq!(value["pid"], 42);
    }

    #[test]
    fn ha_peers_without_a_node_id_are_refused_rather_than_ignored() {
        // Silently ignoring them would leave a deployment believing it had a
        // replicated set when it had three independent coordinators.
        let args = CoordinatorServeArgs {
            ha_peers: vec!["1=127.0.0.1:7601".to_owned()],
            ..CoordinatorServeArgs::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = astrs_coordinator::Coordinator::open_in_memory(
            astrs_coordinator::CoordinatorConfig::new(astrs_wire::AuthToken::from_bytes([3; 32]))
                .with_port(0),
        )
        .unwrap();
        let outcome = runtime.block_on(start_high_availability(hub, &args));
        assert!(
            outcome.is_err(),
            "an incomplete HA flag set must be refused"
        );
    }

    #[test]
    fn no_ha_flags_leaves_the_coordinator_exactly_as_it_was() {
        let args = CoordinatorServeArgs::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = astrs_coordinator::Coordinator::open_in_memory(
            astrs_coordinator::CoordinatorConfig::new(astrs_wire::AuthToken::from_bytes([3; 32]))
                .with_port(0),
        )
        .unwrap();
        assert!(
            runtime
                .block_on(start_high_availability(hub, &args))
                .is_ok()
        );
    }

    #[test]
    fn a_coordinator_binds_announces_and_stops_on_a_signal() {
        let dir = scratch("coordinator-lifecycle");
        let pidfile = dir.join(crate::runtime_dir::COORDINATOR_PIDFILE);
        let args = CoordinatorServeArgs {
            port: 0,
            working_dir: Some(dir.clone()),
            store: Some(dir.join(STORE_FILE)),
            pidfile: Some(pidfile.clone()),
            announce: true,
            ..CoordinatorServeArgs::default()
        };

        // The signal arrives from a thread, because `coordinator` owns this
        // one until it returns — exactly how `astrs down` reaches it.
        let waiter = std::thread::spawn(move || {
            let pid =
                rustix::process::Pid::from_raw(std::process::id().cast_signed()).expect("a pid");
            for _ in 0..200 {
                if crate::runtime_dir::read_pidfile(&pidfile)
                    .and_then(|record| record.addr)
                    .is_some()
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        });

        let mut out = Vec::new();
        let report = coordinator(&mut out, &args).expect("a coordinator");
        waiter.join().expect("the signalling thread");

        assert_eq!(report.role, "coordinator");
        assert!(report.address.starts_with("127.0.0.1:"), "{report:?}");
        assert_ne!(
            report.address, "127.0.0.1:0",
            "an ephemeral port must be reported as the one actually bound"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"role\":\"coordinator\""), "{text}");
        assert!(
            !dir.join(crate::runtime_dir::COORDINATOR_PIDFILE).exists(),
            "the pidfile must not outlive the process that wrote it"
        );
    }

    #[test]
    fn a_daemon_binds_its_socket_starts_its_uplink_and_stops() {
        let dir = scratch("daemon-lifecycle");
        let pidfile = dir.join(crate::runtime_dir::DAEMON_PIDFILE);
        let args = DaemonServeArgs {
            // A coordinator that is not there: the uplink is started
            // anyway and keeps retrying, because a daemon is the
            // autonomous half of the pair (§12). Port 1 on loopback is
            // never listening, so this exercises exactly that.
            coordinator: Some("127.0.0.1:1".to_owned()),
            peer_port: 0,
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir.clone()),
            pidfile: Some(pidfile.clone()),
            announce: true,
            ..DaemonServeArgs::default()
        };

        let waiter = std::thread::spawn(move || {
            let pid =
                rustix::process::Pid::from_raw(std::process::id().cast_signed()).expect("a pid");
            for _ in 0..200 {
                if crate::runtime_dir::read_pidfile(&pidfile).is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        });

        let mut out = Vec::new();
        let report = daemon(&mut out, &args).expect("a daemon");
        waiter.join().expect("the signalling thread");

        assert_eq!(report.role, "daemon");
        assert!(report.address.contains("daemon.sock"), "{report:?}");
        assert!(
            report
                .detail
                .contains("registering with the coordinator at 127.0.0.1:1"),
            "{report:?}"
        );
        assert!(
            report.detail.contains("peers dial 127.0.0.1:"),
            "{report:?}"
        );
        assert!(!dir.join(crate::runtime_dir::DAEMON_PIDFILE).exists());
    }

    #[test]
    fn a_runtime_process_without_a_handshake_blob_reports_a_node_error() {
        // `astrs runtime` is spawned *by a daemon*; run by hand there is no
        // `ASTRS_NODE_CONFIG`, and the failure must be the node client's own
        // typed one rather than a panic.
        let error = runtime(&mut Vec::new(), &RuntimeServeArgs::default()).unwrap_err();
        assert!(matches!(error, CliError::Node(_)), "{error}");
    }
}
