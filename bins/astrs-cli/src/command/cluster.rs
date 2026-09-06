//! `astrs up`, `astrs down` and the cluster half of `astrs status`
//! (blueprint §17's Lifecycle row, §4.2's process model, §16's token,
//! §24.2's runtime directory).
//!
//! ```text
//!   astrs up
//!     ├─ <working dir>/.astrs-token        generate (0600) or reuse   §16
//!     ├─ spawn self: `astrs coordinator --announce --pidfile …`
//!     │      └─ wait for <runtime>/coordinator.pid to name an address
//!     ├─ greet it as a CLI (Hello{role=Cli,token}) — proof, not hope
//!     └─ spawn self: `astrs daemon --coordinator <addr> --pidfile …`
//!            └─ wait for <runtime>/daemon.pid
//!
//!   astrs status ─► pidfiles + one `Check` over the wire
//!   astrs down   ─► Destroy ─► SIGTERM ─► grace ─► SIGKILL ─► rm pidfiles
//! ```
//!
//! # One binary, two more processes
//!
//! `up` spawns **this same executable** with the hidden verbs
//! ([`crate::command::serve`]) rather than looking for `astrs-coordinator`
//! and `astrs-daemon` on `PATH`. That is what makes a deployment one file
//! to copy (§4.2) and what makes `up` work from a `cargo run` target, a
//! container layer, or a test's `CARGO_BIN_EXE_astrs` without any of them
//! having to install anything.
//!
//! # These children must *outlive* their parent
//!
//! Exactly opposite to `astrs run`'s orphan guard. `astrs run` wants every
//! child dead the moment the CLI dies; `astrs up` returns to the shell
//! immediately and the cluster must survive that — and must survive the
//! Ctrl-C that ends the *next* foreground command in that terminal. Each
//! child therefore gets its own process group (so a terminal's `SIGINT`
//! never reaches it) and is explicitly *not* given
//! [`astrs_wire::ENV_RUN_PARENT_PID`]. `astrs down` is the way to stop
//! them, and the pidfile is how it finds them.
//!
//! # Nothing here trusts a pid alone
//!
//! A pidfile can outlive its process and a pid can be recycled, so
//! [`status`] confirms a *coordinator* by greeting it over the wire and
//! only falls back to the `kill(pid, 0)` probe for the daemon, which has no
//! CLI-facing listener of its own.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use astrs_wire::{ControlReply, ControlRequest};

use crate::command::client::{Client, Endpoint, resolve_addr, runtime};
use crate::error::CliError;
use crate::runtime_dir::{
    COORDINATOR_PIDFILE, DAEMON_PIDFILE, PidFile, read_pidfile, remove_pidfile,
};

/// How long a spawned child has to bind its listener and write its pidfile
/// before `astrs up` gives up on it.
///
/// Generous: a cold coordinator opens (and possibly creates) a redb store
/// on a filesystem that may be slow, and a machine under a heavy build is
/// exactly when someone runs `astrs up`.
pub const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the parent looks for a child's pidfile while waiting.
pub const READY_POLL: Duration = Duration::from_millis(25);

/// How long a stopping cluster process has after `SIGTERM` before `SIGKILL`.
///
/// The coordinator has connections to close and a store to flush; the
/// daemon has nodes to stop through its own finish ladder (§12). Five
/// seconds is long enough for both to end cleanly and short enough that
/// `astrs down` stays an interactive command.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often a stopping process is re-checked while the grace runs.
pub const STOP_POLL: Duration = Duration::from_millis(20);

/// The file each spawned child's stdout and stderr are redirected into.
///
/// A child of `astrs up` has no terminal — the parent returns to the shell
/// while it keeps running — so its output goes where an operator can read
/// it afterwards, next to its pidfile (§24.2).
#[must_use]
pub fn log_path(runtime_dir: &Path, role: &str) -> PathBuf {
    runtime_dir.join(format!("{role}.log"))
}

/// What one cluster process is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessStatus {
    /// `"coordinator"` or `"daemon"`.
    pub role: &'static str,
    /// Its process id, when a pidfile named one.
    pub pid: Option<u32>,
    /// Where it listens, when it announced an address.
    pub address: Option<String>,
    /// Whether the process exists right now.
    pub running: bool,
    /// What was observed, in words.
    pub note: String,
}

impl ProcessStatus {
    /// A status for a process no pidfile describes.
    #[must_use]
    pub fn absent(role: &'static str) -> Self {
        Self {
            role,
            pid: None,
            address: None,
            running: false,
            note: "not running here (no pidfile)".to_owned(),
        }
    }

    /// The status a pidfile plus a liveness probe implies.
    #[must_use]
    pub fn from_pidfile(role: &'static str, record: &PidFile) -> Self {
        let running = record.is_alive();
        Self {
            role,
            pid: Some(record.pid),
            address: record.addr.clone(),
            running,
            note: if running {
                "running".to_owned()
            } else {
                "a pidfile is here but the process is gone (stale)".to_owned()
            },
        }
    }

    /// Reads `path` and classifies whatever is there.
    #[must_use]
    pub fn probe(role: &'static str, path: &Path) -> Self {
        read_pidfile(path).map_or_else(
            || Self::absent(role),
            |record| Self::from_pidfile(role, &record),
        )
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "role": self.role,
            "pid": self.pid,
            "address": self.address,
            "running": self.running,
            "note": self.note,
        })
    }

    /// The one-line human form.
    #[must_use]
    pub fn to_line(&self) -> String {
        let pid = self
            .pid
            .map_or_else(|| "-".to_owned(), |pid| pid.to_string());
        let addr = self.address.clone().unwrap_or_else(|| "-".to_owned());
        format!("{:<12} {:<8} {:<24} {}", self.role, pid, addr, self.note)
    }
}

/// `astrs up`'s arguments, already parsed and resolved.
#[derive(Debug, Clone, Default)]
pub struct UpArgs {
    /// The port the coordinator should bind; `0` takes a free one.
    pub port: Option<u16>,
    /// Where pidfiles, sockets and child logs live (§24.2).
    pub runtime_dir: Option<PathBuf>,
    /// Where `.astrs-token` is written or found (§16).
    pub working_dir: Option<PathBuf>,
    /// An explicit token, instead of generating one.
    pub token: Option<String>,
    /// A file holding that token.
    pub token_file: Option<PathBuf>,
    /// Delete the coordinator's store before it opens it.
    pub recreate_store: bool,
    /// Start the coordinator only.
    pub no_daemon: bool,
    /// Emit JSON rather than a human summary.
    pub json: bool,
}

/// What one `astrs up` did.
#[derive(Debug, Clone)]
pub struct UpReport {
    /// The coordinator, started or adopted.
    pub coordinator: ProcessStatus,
    /// This machine's daemon, unless `--no-daemon`.
    pub daemon: Option<ProcessStatus>,
    /// Where the cluster token lives (§16).
    pub token_path: Option<PathBuf>,
    /// Where the pidfiles and logs are (§24.2).
    pub runtime_dir: PathBuf,
    /// Whether anything was already running and was adopted rather than
    /// started.
    pub adopted: bool,
}

impl UpReport {
    /// The human summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = vec![if self.adopted {
            "cluster already running (adopted)".to_owned()
        } else {
            "cluster up".to_owned()
        }];
        lines.push(self.coordinator.to_line());
        if let Some(daemon) = &self.daemon {
            lines.push(daemon.to_line());
        } else {
            lines.push(
                "daemon       -        -                        not started (--no-daemon)"
                    .to_owned(),
            );
        }
        if let Some(token) = &self.token_path {
            lines.push(format!("token: {}", token.display()));
        }
        lines.push(format!("runtime dir: {}", self.runtime_dir.display()));
        lines.join("\n")
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "adopted": self.adopted,
            "coordinator": self.coordinator.to_json(),
            "daemon": self.daemon.as_ref().map(ProcessStatus::to_json),
            "token": self.token_path.as_ref().map(|p| p.display().to_string()),
            "runtime_dir": self.runtime_dir.display().to_string(),
        })
    }
}

/// Brings a cluster up: a coordinator, and (unless `--no-daemon`) this
/// machine's daemon.
///
/// # Errors
///
/// - [`CliError::AlreadyRunning`] if `--recreate-store` is asked for while a
///   coordinator is live — wiping the store under a running cluster would
///   lose exactly the state the flag exists to reset.
/// - [`CliError::Io`] if the runtime directory, the token file or a child
///   log cannot be written, or a child cannot be spawned.
/// - [`CliError::Cluster`] if a child exits before it is ready, or never
///   becomes ready.
/// - [`CliError::BadToken`] if an existing token file holds something
///   unusable.
/// - Whatever greeting the coordinator refuses with.
pub fn up(out: &mut dyn Write, args: &UpArgs) -> Result<UpReport, CliError> {
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    crate::runtime_dir::ensure_dir(&runtime_dir)?;
    let working_dir = crate::runtime_dir::working_dir(args.working_dir.as_deref());

    let coordinator_pidfile = runtime_dir.join(COORDINATOR_PIDFILE);
    let daemon_pidfile = runtime_dir.join(DAEMON_PIDFILE);
    let live_coordinator = ProcessStatus::probe("coordinator", &coordinator_pidfile);
    if live_coordinator.running && args.recreate_store {
        return Err(CliError::AlreadyRunning {
            process: "coordinator",
            pid: live_coordinator.pid.unwrap_or(0),
            address: live_coordinator
                .address
                .as_ref()
                .map_or_else(String::new, |addr| format!(", {addr}")),
        });
    }

    // §16: the token is per-cluster and lives beside the manifest, not in
    // the per-machine runtime directory.
    let (token_argument, token_path) = match (&args.token, &args.token_file) {
        (Some(token), _) => (TokenArgument::Inline(token.clone()), None),
        (None, Some(path)) => (TokenArgument::File(path.clone()), Some(path.clone())),
        (None, None) => {
            let (_, path) = crate::runtime_dir::ensure_token_file(&working_dir)?;
            (TokenArgument::File(path.clone()), Some(path))
        }
    };

    let mut adopted = live_coordinator.running;
    let coordinator = if live_coordinator.running {
        // Gated on `--json`: the report already carries `adopted`, and a
        // stray human line ahead of the object would make the whole output
        // unparsable for the script that asked for JSON.
        if !args.json {
            let _ = writeln!(
                out,
                "coordinator already running (pid {}); adopting it",
                live_coordinator.pid.unwrap_or(0)
            );
        }
        live_coordinator
    } else {
        remove_pidfile(&coordinator_pidfile)?;
        let mut argv = vec![
            "coordinator".to_owned(),
            "--port".to_owned(),
            args.port
                .unwrap_or_else(crate::command::client::default_port)
                .to_string(),
            "--store".to_owned(),
            runtime_dir
                .join(crate::command::serve::STORE_FILE)
                .display()
                .to_string(),
            "--working-dir".to_owned(),
            working_dir.display().to_string(),
            "--pidfile".to_owned(),
            coordinator_pidfile.display().to_string(),
            "--announce".to_owned(),
        ];
        argv.extend(token_argument.to_argv());
        if args.recreate_store {
            argv.push("--recreate-store".to_owned());
        }
        start_child("coordinator", &argv, &runtime_dir, &working_dir)?;
        wait_for_ready(
            "coordinator",
            &coordinator_pidfile,
            &runtime_dir,
            true,
            READY_TIMEOUT,
        )?
    };

    // Proof rather than hope: a pidfile says a process exists, a completed
    // greeting says the *cluster* is usable with the token this `up` chose.
    let endpoint = greet(&coordinator, args, &working_dir)?;

    let daemon = if args.no_daemon {
        None
    } else {
        let live_daemon = ProcessStatus::probe("daemon", &daemon_pidfile);
        if live_daemon.running {
            adopted = true;
            if !args.json {
                let _ = writeln!(
                    out,
                    "daemon already running (pid {}); adopting it",
                    live_daemon.pid.unwrap_or(0)
                );
            }
            Some(live_daemon)
        } else {
            remove_pidfile(&daemon_pidfile)?;
            let mut argv = vec![
                "daemon".to_owned(),
                "--coordinator".to_owned(),
                endpoint.clone(),
                "--runtime-dir".to_owned(),
                runtime_dir.display().to_string(),
                "--working-dir".to_owned(),
                working_dir.display().to_string(),
                "--pidfile".to_owned(),
                daemon_pidfile.display().to_string(),
                // An ephemeral peer port (§6.4), not §24.2's 7409: the
                // address other daemons dial reaches them through the
                // daemon's *registration*, never through a well-known
                // number, so a fixed port buys nothing and costs an
                // `EADDRINUSE` the moment two clusters share a machine —
                // which is exactly what a test suite is.
                "--peer-port".to_owned(),
                "0".to_owned(),
                "--announce".to_owned(),
            ];
            argv.extend(token_argument.to_argv());
            start_child("daemon", &argv, &runtime_dir, &working_dir)?;
            Some(wait_for_ready(
                "daemon",
                &daemon_pidfile,
                &runtime_dir,
                false,
                READY_TIMEOUT,
            )?)
        }
    };

    let report = UpReport {
        coordinator,
        daemon,
        token_path,
        runtime_dir,
        adopted,
    };
    emit(out, args.json, &report.summary(), || report.to_json());
    Ok(report)
}

/// How a child is told which token to use.
#[derive(Debug, Clone)]
enum TokenArgument {
    /// A 64-hex value on the command line.
    Inline(String),
    /// A path to read it from — always preferred, since an argument is
    /// visible in `ps` to every user on the machine (§16).
    File(PathBuf),
}

impl TokenArgument {
    /// The two argv entries this spelling contributes.
    fn to_argv(&self) -> Vec<String> {
        match self {
            Self::Inline(token) => vec!["--token".to_owned(), token.clone()],
            Self::File(path) => vec!["--token-file".to_owned(), path.display().to_string()],
        }
    }
}

/// Greets a running coordinator, returning the address that worked.
///
/// # Errors
///
/// As [`Client::connect`] — a refusal here is the useful one: it means the
/// process is up but the token (or the protocol) does not match.
fn greet(
    coordinator: &ProcessStatus,
    args: &UpArgs,
    working_dir: &Path,
) -> Result<String, CliError> {
    let address = coordinator.address.clone().unwrap_or_else(|| {
        format!(
            "127.0.0.1:{}",
            args.port
                .unwrap_or_else(crate::command::client::default_port)
        )
    });
    let endpoint = crate::command::client::endpoint(
        Some(&address),
        args.token.as_deref(),
        args.token_file.as_deref(),
        Some(working_dir),
        false,
    )?;
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut client = Client::connect(&endpoint).await?;
        client
            .request_ok("status", &ControlRequest::Check { dataflow: None })
            .await
            .or_else(|error| match error {
                // A cluster with dataflows but no daemon answers `Check`
                // with an error; the greeting still proved the listener and
                // the token, which is all `up` is asserting here.
                CliError::Refused { .. } | CliError::UnexpectedReply { .. } => Ok(()),
                other => Err(other),
            })?;
        Ok::<(), CliError>(())
    })?;
    Ok(address)
}

/// Spawns one hidden verb as a detached child of this process.
///
/// # Errors
///
/// [`CliError::Io`] if this executable cannot be located, the log file
/// cannot be opened, or the process cannot be spawned.
fn start_child(
    role: &'static str,
    argv: &[String],
    runtime_dir: &Path,
    working_dir: &Path,
) -> Result<u32, CliError> {
    let exe = std::env::current_exe().map_err(|source| CliError::io("this executable", source))?;
    let log = log_path(runtime_dir, role);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|source| CliError::io(&log, source))?;
    let errors = file
        .try_clone()
        .map_err(|source| CliError::io(&log, source))?;

    let mut command = Command::new(exe);
    command
        .args(argv)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors));
    // A cluster process is not this shell's child in any way that matters:
    // its own process group means the terminal's Ctrl-C never reaches it,
    // and no orphan guard means it does not die with the CLI that started
    // it (see this module's own docs).
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.env_remove(astrs_wire::ENV_RUN_PARENT_PID);
    command.env_remove(astrs_wire::ENV_NODE_CONFIG);

    let child = command
        .spawn()
        .map_err(|source| CliError::io(format!("spawning `astrs {role}`"), source))?;
    Ok(child.id())
}

/// Waits for a spawned child to write a pidfile, and (for a listener) an
/// address in it.
///
/// # Errors
///
/// [`CliError::Cluster`] when the child never becomes ready, with the tail
/// of its own log attached — the only place its failure was written.
fn wait_for_ready(
    role: &'static str,
    pidfile: &Path,
    runtime_dir: &Path,
    needs_address: bool,
    timeout: Duration,
) -> Result<ProcessStatus, CliError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(record) = read_pidfile(pidfile)
            && (!needs_address || record.addr.is_some())
        {
            return Ok(ProcessStatus::from_pidfile(role, &record));
        }
        if Instant::now() >= deadline {
            return Err(CliError::Cluster {
                action: "start",
                process: role,
                reason: format!(
                    "it did not become ready within {:.0}s; its log says:\n{}",
                    timeout.as_secs_f64(),
                    log_tail(&log_path(runtime_dir, role))
                ),
            });
        }
        std::thread::sleep(READY_POLL);
    }
}

/// The last few lines of a child's log, for an error message.
fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("(nothing was written to {})", path.display());
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(12);
    let tail = lines[start..].join("\n");
    if tail.trim().is_empty() {
        format!("(nothing was written to {})", path.display())
    } else {
        tail
    }
}

/// `astrs down`'s arguments, already parsed and resolved.
#[derive(Debug, Clone, Default)]
pub struct DownArgs {
    /// Where the pidfiles live (§24.2).
    pub runtime_dir: Option<PathBuf>,
    /// Where `.astrs-token` is found (§16).
    pub working_dir: Option<PathBuf>,
    /// The coordinator address, when it is not the one in the pidfile.
    pub coordinator: Option<String>,
    /// An explicit token.
    pub token: Option<String>,
    /// A file holding that token.
    pub token_file: Option<PathBuf>,
    /// Stop the cluster even while dataflows are running.
    pub force: bool,
    /// Emit JSON rather than a human summary.
    pub json: bool,
}

/// How one process was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Nothing was running under that pidfile.
    NotRunning,
    /// It exited after `SIGTERM`.
    Terminated,
    /// It ignored `SIGTERM` and was killed.
    Killed,
    /// It could not be signalled at all (another user's process, or a pid
    /// that vanished between the probe and the signal).
    Unreachable,
}

impl StopOutcome {
    /// A stable lowercase word, for both output forms.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRunning => "not-running",
            Self::Terminated => "terminated",
            Self::Killed => "killed",
            Self::Unreachable => "unreachable",
        }
    }
}

/// What one `astrs down` did.
#[derive(Debug, Clone)]
pub struct DownReport {
    /// The daemon's outcome, and the pid it had.
    pub daemon: (StopOutcome, Option<u32>),
    /// The coordinator's outcome, and the pid it had.
    pub coordinator: (StopOutcome, Option<u32>),
    /// Whether the cluster was asked to destroy its dataflows first, and
    /// what came of it.
    pub destroy: Option<String>,
}

impl DownReport {
    /// Whether anything at all was running.
    #[must_use]
    pub fn stopped_anything(&self) -> bool {
        !matches!(
            (self.daemon.0, self.coordinator.0),
            (StopOutcome::NotRunning, StopOutcome::NotRunning)
        )
    }

    /// The human summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        if let Some(note) = &self.destroy {
            lines.push(format!("dataflows: {note}"));
        }
        for (role, (outcome, pid)) in [("daemon", self.daemon), ("coordinator", self.coordinator)] {
            lines.push(match pid {
                Some(pid) => format!("{role}: {} (pid {pid})", outcome.as_str()),
                None => format!("{role}: {}", outcome.as_str()),
            });
        }
        lines.push(if self.stopped_anything() {
            "cluster down".to_owned()
        } else {
            "nothing was running here".to_owned()
        });
        lines.join("\n")
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "stopped_anything": self.stopped_anything(),
            "destroy": self.destroy,
            "daemon": { "outcome": self.daemon.0.as_str(), "pid": self.daemon.1 },
            "coordinator": {
                "outcome": self.coordinator.0.as_str(),
                "pid": self.coordinator.1,
            },
        })
    }
}

/// Takes a cluster down: dataflows first, then the daemon, then the
/// coordinator.
///
/// That order is the one that avoids lying to anybody: nodes are stopped
/// through their own daemon while the coordinator still exists to be told
/// about it, the daemon goes next so no node outlives its supervisor, and
/// the coordinator last so it can record what happened.
///
/// # Errors
///
/// - [`CliError::Io`] if a pidfile cannot be removed.
/// - Never for "nothing was running": that is a report, not a failure —
///   `astrs down` twice in a script is a normal thing to do.
pub fn down(out: &mut dyn Write, args: &DownArgs) -> Result<DownReport, CliError> {
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    let coordinator_pidfile = runtime_dir.join(COORDINATOR_PIDFILE);
    let daemon_pidfile = runtime_dir.join(DAEMON_PIDFILE);

    let coordinator = ProcessStatus::probe("coordinator", &coordinator_pidfile);
    let daemon = ProcessStatus::probe("daemon", &daemon_pidfile);

    let destroy = if coordinator.running {
        Some(destroy_dataflows(args, &coordinator, &runtime_dir))
    } else {
        None
    };

    let daemon_outcome = stop_process(&daemon);
    let coordinator_outcome = stop_process(&coordinator);

    remove_pidfile(&daemon_pidfile)?;
    remove_pidfile(&coordinator_pidfile)?;

    let report = DownReport {
        daemon: (daemon_outcome, daemon.pid),
        coordinator: (coordinator_outcome, coordinator.pid),
        destroy,
    };
    emit(out, args.json, &report.summary(), || report.to_json());
    Ok(report)
}

/// Asks a live coordinator to stop its dataflows, reporting what it said.
///
/// Best-effort by design: `down`'s job is to stop *processes*, and a
/// coordinator that refuses (because dataflows are running and `--force`
/// was not given) or cannot be reached must not stop that from happening.
/// The refusal is reported rather than swallowed.
fn destroy_dataflows(args: &DownArgs, coordinator: &ProcessStatus, working: &Path) -> String {
    let address = args
        .coordinator
        .clone()
        .or_else(|| coordinator.address.clone());
    let working_dir = args
        .working_dir
        .clone()
        .unwrap_or_else(|| working.to_path_buf());
    let endpoint = match crate::command::client::endpoint(
        address.as_deref(),
        args.token.as_deref(),
        args.token_file.as_deref(),
        Some(&working_dir),
        false,
    ) {
        Ok(endpoint) => endpoint,
        Err(error) => return format!("not asked to stop ({error})"),
    };
    let Ok(runtime) = runtime() else {
        return "not asked to stop (no async runtime)".to_owned();
    };
    let outcome = runtime.block_on(async {
        let mut client = Client::connect(&endpoint).await?;
        client
            .request("destroy", &ControlRequest::Destroy { force: args.force })
            .await
    });
    match outcome {
        Ok(ControlReply::Ok) => "destroyed".to_owned(),
        Ok(other) => format!(
            "the coordinator answered `{}`",
            crate::command::client::reply_name(&other)
        ),
        Err(error) => format!("not stopped cleanly ({error})"),
    }
}

/// `SIGTERM`, then the grace, then `SIGKILL` (§12's ladder, applied to a
/// cluster process rather than a node).
fn stop_process(status: &ProcessStatus) -> StopOutcome {
    let Some(pid) = status.pid.filter(|_| status.running) else {
        return StopOutcome::NotRunning;
    };
    let Ok(pid) = rustix::process::Pid::from_raw(pid.cast_signed()).ok_or(()) else {
        return StopOutcome::Unreachable;
    };
    if rustix::process::kill_process(pid, rustix::process::Signal::TERM).is_err() {
        return StopOutcome::Unreachable;
    }
    if wait_for_exit(pid, STOP_GRACE) {
        return StopOutcome::Terminated;
    }
    if rustix::process::kill_process(pid, rustix::process::Signal::KILL).is_err() {
        return StopOutcome::Unreachable;
    }
    if wait_for_exit(pid, STOP_GRACE) {
        StopOutcome::Killed
    } else {
        StopOutcome::Unreachable
    }
}

/// Polls until `pid` is gone or `grace` elapses.
///
/// Reaps first, every round: a process that has exited but is still this
/// process's un-waited child is a zombie, and a zombie answers `kill(pid,
/// 0)` exactly like a live process would. That case is rare in production —
/// `astrs down` stops processes an earlier `astrs up` spawned, which were
/// reparented when that CLI exited — and constant in a test that spawns its
/// own. `waitpid` with `NOHANG` costs one syscall, never blocks, and simply
/// fails with `ECHILD` when the pid is not ours, so it is the cheap way to
/// be right in both worlds.
fn wait_for_exit(pid: rustix::process::Pid, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG);
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// `astrs status`'s arguments when no dataflow is named.
#[derive(Debug, Clone, Default)]
pub struct StatusArgs {
    /// Where the pidfiles live (§24.2).
    pub runtime_dir: Option<PathBuf>,
    /// Where `.astrs-token` is found (§16).
    pub working_dir: Option<PathBuf>,
    /// The coordinator address, when it is not the one in the pidfile.
    pub coordinator: Option<String>,
    /// An explicit token.
    pub token: Option<String>,
    /// A file holding that token.
    pub token_file: Option<PathBuf>,
    /// Emit JSON rather than a human summary.
    pub json: bool,
}

/// What `astrs status` found.
#[derive(Debug, Clone)]
pub struct ClusterStatus {
    /// The coordinator's process, per its pidfile.
    pub coordinator: ProcessStatus,
    /// The daemon's process, per its pidfile.
    pub daemon: ProcessStatus,
    /// The address that was greeted.
    pub address: Option<SocketAddr>,
    /// Whether the coordinator answered a `Check` over the wire.
    pub reachable: bool,
    /// What the greeting said, when it did not succeed.
    pub reachability_note: String,
}

impl ClusterStatus {
    /// `0` when the coordinator answered, `1` otherwise — so
    /// `astrs status >/dev/null && …` is a usable cluster-liveness test.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.reachable { 0 } else { 1 }
    }

    /// The human summary.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "{:<12} {:<8} {:<24} {}",
            "PROCESS", "PID", "ADDRESS", "STATE"
        )];
        lines.push(self.coordinator.to_line());
        lines.push(self.daemon.to_line());
        lines.push(match (&self.address, self.reachable) {
            (Some(addr), true) => format!("the coordinator at {addr} answered a health check"),
            (Some(addr), false) => {
                format!(
                    "the coordinator at {addr} did not answer: {}",
                    self.reachability_note
                )
            }
            (None, _) => format!("no coordinator was reached: {}", self.reachability_note),
        });
        lines.join("\n")
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "reachable": self.reachable,
            "address": self.address.map(|addr| addr.to_string()),
            "note": self.reachability_note,
            "coordinator": self.coordinator.to_json(),
            "daemon": self.daemon.to_json(),
            "exit_code": self.exit_code(),
        })
    }
}

/// Probes this machine's cluster: both pidfiles, and one greeting.
///
/// # Errors
///
/// [`CliError::BadAddress`] if an explicit `--coordinator` cannot be
/// resolved. Everything else is *content* of the report: a coordinator that
/// is down is the answer to `astrs status`, not a failure of it.
pub fn status(out: &mut dyn Write, args: &StatusArgs) -> Result<ClusterStatus, CliError> {
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    let coordinator = ProcessStatus::probe("coordinator", &runtime_dir.join(COORDINATOR_PIDFILE));
    let daemon = ProcessStatus::probe("daemon", &runtime_dir.join(DAEMON_PIDFILE));

    let address_text = args
        .coordinator
        .clone()
        .or_else(|| coordinator.address.clone());
    let address = resolve_addr(address_text.as_deref())?;
    let working_dir = crate::runtime_dir::working_dir(args.working_dir.as_deref());
    let endpoint = crate::command::client::endpoint(
        address_text.as_deref(),
        args.token.as_deref(),
        args.token_file.as_deref(),
        Some(&working_dir),
        false,
    )?;

    let (reachable, note) = check(&endpoint);
    let report = ClusterStatus {
        coordinator,
        daemon,
        address: Some(address),
        reachable,
        reachability_note: note,
    };
    emit(out, args.json, &report.summary(), || report.to_json());
    Ok(report)
}

/// One `Check`, reduced to "did it answer" plus what it said.
fn check(endpoint: &Endpoint) -> (bool, String) {
    let Ok(runtime) = runtime() else {
        return (false, "no async runtime could be built".to_owned());
    };
    let outcome = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        client
            .request("status", &ControlRequest::Check { dataflow: None })
            .await
    });
    match outcome {
        Ok(_) => (true, "healthy".to_owned()),
        // A refusal is an *answer*: the listener is there and speaking the
        // protocol, and saying so is more useful than calling it dead.
        Err(CliError::Refused { message, .. }) => (
            true,
            format!("reachable, but the health check reports: {message}"),
        ),
        Err(error) => (false, error.to_string()),
    }
}

/// Writes either the human text or the JSON object.
fn emit(out: &mut dyn Write, json: bool, text: &str, value: impl FnOnce() -> serde_json::Value) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&value()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{text}");
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::runtime_dir::write_pidfile;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-cluster-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pid that cannot be alive: `kill(0, …)` addresses a process group,
    /// so `Pid::from_raw(0)` is rejected outright.
    const DEAD_PID: u32 = 0;

    #[test]
    fn a_missing_pidfile_reads_as_absent() {
        let dir = scratch("absent");
        let status = ProcessStatus::probe("coordinator", &dir.join(COORDINATOR_PIDFILE));
        assert!(!status.running);
        assert_eq!(status.pid, None);
        assert!(status.note.contains("no pidfile"), "{status:?}");
    }

    #[test]
    fn a_live_pidfile_reads_as_running_with_its_address() {
        let dir = scratch("live");
        let path = dir.join(COORDINATOR_PIDFILE);
        write_pidfile(
            &path,
            &PidFile::listening(std::process::id(), "127.0.0.1:7407"),
        )
        .unwrap();
        let status = ProcessStatus::probe("coordinator", &path);
        assert!(status.running);
        assert_eq!(status.address.as_deref(), Some("127.0.0.1:7407"));
        assert!(status.to_line().contains("127.0.0.1:7407"));
    }

    #[test]
    fn a_stale_pidfile_is_named_as_stale_rather_than_running() {
        let dir = scratch("stale");
        let path = dir.join(DAEMON_PIDFILE);
        write_pidfile(&path, &PidFile::new(DEAD_PID)).unwrap();
        let status = ProcessStatus::probe("daemon", &path);
        assert!(!status.running);
        assert!(status.note.contains("stale"), "{status:?}");
    }

    #[test]
    fn stopping_something_that_is_not_running_is_not_an_error() {
        let status = ProcessStatus::absent("daemon");
        assert_eq!(stop_process(&status), StopOutcome::NotRunning);
    }

    #[test]
    fn a_real_child_is_terminated_by_the_stop_ladder() {
        // `sleep` ignores nothing, so `SIGTERM` alone must be enough; the
        // point is that the ladder observes the exit rather than returning
        // before the process is actually gone.
        let child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("a child");
        let status = ProcessStatus::from_pidfile("daemon", &PidFile::new(child.id()));
        assert!(status.running);
        assert_eq!(stop_process(&status), StopOutcome::Terminated);
        // `stop_process` reaped it (see `wait_for_exit`), so this only
        // releases the handle's own bookkeeping.
        let mut child = child;
        let _ = child.try_wait();
    }

    #[test]
    fn down_reports_an_empty_runtime_directory_without_failing() {
        let dir = scratch("down-empty");
        let args = DownArgs {
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir),
            ..DownArgs::default()
        };
        let mut out = Vec::new();
        let report = down(&mut out, &args).expect("a report");
        assert!(!report.stopped_anything());
        assert_eq!(report.daemon.0, StopOutcome::NotRunning);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nothing was running here"), "{text}");
    }

    #[test]
    fn down_removes_the_pidfiles_it_finds() {
        let dir = scratch("down-pidfiles");
        let coordinator = dir.join(COORDINATOR_PIDFILE);
        let daemon = dir.join(DAEMON_PIDFILE);
        write_pidfile(&coordinator, &PidFile::new(DEAD_PID)).unwrap();
        write_pidfile(&daemon, &PidFile::new(DEAD_PID)).unwrap();

        let args = DownArgs {
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir),
            ..DownArgs::default()
        };
        let report = down(&mut Vec::new(), &args).expect("a report");
        assert!(!report.stopped_anything());
        assert!(!coordinator.exists(), "a stale pidfile must be cleared");
        assert!(!daemon.exists());
    }

    #[test]
    fn status_of_an_empty_runtime_directory_is_unreachable_but_reported() {
        let dir = scratch("status-empty");
        let args = StatusArgs {
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir),
            // Port 1 is refused instantly rather than timing out.
            coordinator: Some("127.0.0.1:1".to_owned()),
            ..StatusArgs::default()
        };
        let mut out = Vec::new();
        let report = status(&mut out, &args).expect("a report");
        assert!(!report.reachable);
        assert_eq!(report.exit_code(), 1);
        assert!(!report.coordinator.running);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("PROCESS"), "{text}");
    }

    #[test]
    fn status_json_carries_both_processes_and_the_exit_code() {
        let dir = scratch("status-json");
        let args = StatusArgs {
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir),
            coordinator: Some("127.0.0.1:1".to_owned()),
            json: true,
            ..StatusArgs::default()
        };
        let mut out = Vec::new();
        let _ = status(&mut out, &args).expect("a report");
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["reachable"], false);
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["coordinator"]["running"], false);
        assert_eq!(value["daemon"]["running"], false);
    }

    #[test]
    fn recreating_the_store_under_a_live_coordinator_is_refused() {
        let dir = scratch("recreate-live");
        write_pidfile(
            &dir.join(COORDINATOR_PIDFILE),
            &PidFile::listening(std::process::id(), "127.0.0.1:7407"),
        )
        .unwrap();
        let args = UpArgs {
            runtime_dir: Some(dir.clone()),
            working_dir: Some(dir),
            recreate_store: true,
            ..UpArgs::default()
        };
        let error = up(&mut Vec::new(), &args).unwrap_err();
        match error {
            CliError::AlreadyRunning { process, .. } => assert_eq!(process, "coordinator"),
            other => panic!("expected AlreadyRunning, got {other}"),
        }
    }

    #[test]
    fn a_token_argument_is_spelled_as_a_file_whenever_there_is_one() {
        assert_eq!(
            TokenArgument::File(PathBuf::from("/x/.astrs-token")).to_argv(),
            vec!["--token-file".to_owned(), "/x/.astrs-token".to_owned()]
        );
        assert_eq!(
            TokenArgument::Inline("ab".to_owned()).to_argv(),
            vec!["--token".to_owned(), "ab".to_owned()]
        );
    }

    #[test]
    fn a_child_that_never_becomes_ready_reports_its_own_log() {
        let dir = scratch("never-ready");
        std::fs::write(
            log_path(&dir, "coordinator"),
            "astrs coordinator: Address already in use (os error 48)\n",
        )
        .unwrap();
        let error = wait_for_ready(
            "coordinator",
            &dir.join(COORDINATOR_PIDFILE),
            &dir,
            true,
            Duration::from_millis(80),
        )
        .unwrap_err();
        match error {
            CliError::Cluster {
                action,
                process,
                reason,
            } => {
                assert_eq!(action, "start");
                assert_eq!(process, "coordinator");
                assert!(reason.contains("Address already in use"), "{reason}");
            }
            other => panic!("expected Cluster, got {other}"),
        }
    }

    #[test]
    fn a_pidfile_without_an_address_is_not_ready_for_a_listener_but_is_for_a_daemon() {
        let dir = scratch("ready-address");
        let path = dir.join(DAEMON_PIDFILE);
        write_pidfile(&path, &PidFile::new(std::process::id())).unwrap();

        let ready = wait_for_ready("daemon", &path, &dir, false, Duration::from_millis(80))
            .expect("a daemon needs no address to be ready");
        assert_eq!(ready.pid, Some(std::process::id()));

        let error = wait_for_ready("coordinator", &path, &dir, true, Duration::from_millis(80))
            .unwrap_err();
        assert!(matches!(error, CliError::Cluster { .. }), "{error}");
    }

    #[test]
    fn a_log_tail_of_a_missing_file_says_so() {
        let dir = scratch("log-tail");
        let text = log_tail(&dir.join("nothing.log"));
        assert!(text.contains("nothing was written"), "{text}");
    }

    #[test]
    fn a_log_tail_keeps_only_the_last_lines() {
        let dir = scratch("log-tail-long");
        let path = dir.join("long.log");
        let body: String = (0..40).map(|index| format!("line {index}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let tail = log_tail(&path);
        assert!(tail.contains("line 39"), "{tail}");
        assert!(!tail.contains("line 0\n"), "{tail}");
        assert_eq!(tail.lines().count(), 12);
    }

    #[test]
    fn every_stop_outcome_has_a_stable_word() {
        assert_eq!(StopOutcome::NotRunning.as_str(), "not-running");
        assert_eq!(StopOutcome::Terminated.as_str(), "terminated");
        assert_eq!(StopOutcome::Killed.as_str(), "killed");
        assert_eq!(StopOutcome::Unreachable.as_str(), "unreachable");
    }
}
