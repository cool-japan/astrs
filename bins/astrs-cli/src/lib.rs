//! The `astrs` binary's library half — everything `main.rs` needs beyond
//! parsing `argv` and choosing a process exit code (blueprint §17).
//!
//! Every verb is implemented as a library-testable function under
//! [`command`], taking a `&mut dyn std::io::Write` sink and a typed args
//! struct, returning a typed report. [`dispatch`] is the one place that
//! maps a parsed [`cli::Cli`] onto the right `command::*::run` call and
//! flattens whatever it returns into a process exit code — the only
//! thing `main.rs` needs.
//!
//! ```text
//!   single process   run                    ─► command::run     (embedded daemon)
//!   cluster          up · down · status     ─► command::cluster (spawns this binary)
//!   over the wire    start · stop · restart ─► command::lifecycle
//!                    destroy · clean
//!                    list · logs · status   ─► command::monitor
//!                    param get/set/list/delete ─► command::param
//!   local            build                  ─► command::build
//!                    validate · expand · graph · new · migrate ·
//!                    doctor · completion · schema
//!   hidden servers   coordinator · daemon · runtime ─► command::serve
//! ```
//!
//! # Every §17 verb reaches an implementation
//!
//! `ros2 doctor` and `ros2 topics` were the last two that did not: the ROS
//! 2 interop stack (`astrs-rtps`/`astrs-ros2`) landed, and both now spin a
//! real probe participant through [`command::ros2`]. [`command::stub`]'s
//! table is consequently empty, and it is kept as the mechanism a future
//! wave would use again rather than as a live list.
//!
//! # Read verbs and mutating verbs authenticate differently
//!
//! Blueprint §16 splits the coordinator API in two, and [`dispatch`] is
//! where that split is applied: a mutating verb (`start`, `stop`,
//! `restart`, `destroy`, `clean`, `param set`, `param delete`) refuses
//! locally when no cluster token can be found, while a read verb (`list`,
//! `logs`, `status`, `param get`, `param list`) falls back to the all-zero
//! token so a token-less development coordinator stays inspectable.

pub mod cli;
pub mod command;
pub mod diagnostic;
pub mod error;
pub mod runtime_dir;

use std::io::Write;
use std::time::Duration;

use clap::CommandFactory;

pub use cli::{Cli, Command};
pub use error::CliError;

use command::client::{DataflowRef, Endpoint};

/// Parse, dispatch, and compute a process exit code — everything
/// `main.rs` does not have to know how to do itself.
///
/// `stdout_is_terminal` decides [`cli::ColorMode::Auto`]; pass the real
/// `std::io::stdout().is_terminal()` from `main.rs` (never introspected
/// here, since `stdout` is a generic sink that may not be the real
/// process stdout in a test).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn dispatch(
    cli: Cli,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    stdout_is_terminal: bool,
) -> i32 {
    let json = cli.json;
    let use_color = match cli.color {
        cli::ColorMode::Always => true,
        cli::ColorMode::Never => false,
        cli::ColorMode::Auto => stdout_is_terminal,
    };

    match cli.command {
        Command::Validate(args) => {
            let a = command::validate::ValidateArgs {
                manifest_path: args.manifest,
                prove: args.prove,
                profile: args.profile,
                json,
                color: use_color,
            };
            finish(stderr, command::validate::run(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Expand(args) => {
            let a = command::expand::ExpandArgs {
                manifest_path: args.manifest,
            };
            finish(stderr, command::expand::run(stdout, &a), |_| 0)
        }
        Command::Graph(args) => {
            let a = command::graph::GraphArgs {
                manifest_path: args.manifest,
                format: args.format.into(),
            };
            finish(stderr, command::graph::run(stdout, &a), |_| 0)
        }
        Command::New(new_command) => {
            let a = new_command.into_new_args(json);
            finish(stderr, command::new::run(stdout, &a), |_| 0)
        }
        Command::Migrate(cli::MigrateCommand::FromDora(args)) => {
            let a = command::migrate::FromDoraArgs {
                input: args.input,
                output: args.output,
                json,
            };
            finish(stderr, command::migrate::run_from_dora(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Migrate(cli::MigrateCommand::FromRos2(args)) => {
            let a = command::migrate::FromRos2Args {
                input: args.input,
                output: args.output,
                json,
            };
            finish(stderr, command::migrate::run_from_ros2(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Doctor(args) => {
            let a = command::doctor::DoctorArgs {
                json,
                coordinator_port: args.coordinator_port,
                daemon_port: args.daemon_port,
            };
            finish(stderr, command::doctor::run(stdout, &a), |r| r.exit_code())
        }
        Command::Completion(args) => {
            let a = command::completion::CompletionArgs { shell: args.shell };
            finish(
                stderr,
                command::completion::run(stdout, cli::Cli::command(), &a),
                |_| 0,
            )
        }
        Command::Schema(args) => {
            let a = command::schema::SchemaArgs {
                output: args.output,
            };
            finish(stderr, command::schema::run(stdout, &a), |_| 0)
        }
        Command::Token(cli::TokenCommand::Mint(args)) => {
            let a = command::token::MintArgs {
                scope: args.scope,
                token: args.token,
                token_file: args.token_file,
                working_dir: args.working_dir,
                out: args.out,
                json,
            };
            finish(stderr, command::token::mint(stdout, &a), |_| 0)
        }
        Command::Hub(cli::HubCommand::Update(args)) => {
            let a = command::hub::UpdateArgs {
                index: args.index,
                cache_dir: None,
                config_path: None,
                json,
            };
            finish(stderr, command::hub::update(stdout, &a), |_| 0)
        }
        Command::Hub(cli::HubCommand::Search(args)) => {
            let a = command::hub::SearchArgs {
                term: args.term,
                cache_dir: None,
                json,
            };
            finish(stderr, command::hub::search(stdout, &a), |_| 0)
        }
        Command::Hub(cli::HubCommand::Info(args)) => {
            let a = command::hub::InfoArgs {
                name: args.name,
                cache_dir: None,
                json,
            };
            finish(stderr, command::hub::info(stdout, &a), |_| 0)
        }
        Command::Hub(cli::HubCommand::Init(args)) => {
            let a = command::hub::InitArgs {
                dir: args.dir,
                force: args.force,
                json,
            };
            finish(stderr, command::hub::init(stdout, &a), |_| 0)
        }

        // ---- Single-process and local ---------------------------------
        Command::Run(args) => {
            let level = match level_of(args.level.as_deref()) {
                Ok(level) => level,
                Err(err) => return report_error(stderr, &err),
            };
            let a = command::run::RunArgs {
                manifest_path: args.manifest,
                deterministic: args.deterministic,
                from_recording: args.from_recording,
                speed: args.speed,
                exit_when_nodes_finish: args.exit_when_nodes_finish,
                skip_build: args.skip_build,
                level,
                working_dir: args.working_dir,
                runtime_dir: args.runtime_dir,
                timeout: args.timeout.map(Duration::from_secs_f64),
                grace: args.grace.map(Duration::from_secs_f64),
                json,
                color: use_color,
            };
            // Streamed straight onto the caller's sink, never buffered: a
            // graph that runs for an hour must print its first line in the
            // first second.
            finish(stderr, command::run::run(stdout, &a), |r| r.exit_code())
        }
        Command::Build(args) => {
            let a = command::build::BuildArgs {
                manifest_path: args.manifest,
                release: args.release,
                node: args.node,
                working_dir: args.working_dir,
                runtime_dir: args.runtime_dir,
                json,
            };
            finish(stderr, command::build::run(stdout, &a), |r| r.exit_code())
        }

        // ---- Cluster lifecycle ----------------------------------------
        Command::Up(args) => {
            let a = command::cluster::UpArgs {
                port: args.port,
                runtime_dir: args.runtime_dir,
                working_dir: args.connect.working_dir,
                token: args.connect.token,
                token_file: args.connect.token_file,
                recreate_store: args.recreate_store,
                no_daemon: args.no_daemon,
                json,
            };
            finish(stderr, command::cluster::up(stdout, &a), |_| 0)
        }
        Command::Down(args) => {
            let a = command::cluster::DownArgs {
                runtime_dir: args.runtime_dir,
                working_dir: args.connect.working_dir,
                coordinator: args.connect.coordinator,
                token: args.connect.token,
                token_file: args.connect.token_file,
                force: args.force,
                json,
            };
            finish(stderr, command::cluster::down(stdout, &a), |_| 0)
        }
        Command::Status(args) => match args.dataflow {
            // A named dataflow is the coordinator's business; no dataflow
            // is this machine's (§17: `status` covers both).
            Some(reference) => {
                let a = command::monitor::DataflowStatusArgs {
                    dataflow: Some(DataflowRef::parse(&reference)),
                    json,
                };
                with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                    command::monitor::dataflow_status(out, endpoint, &a)
                        .map(|report| report.exit_code())
                })
            }
            None => {
                let a = command::cluster::StatusArgs {
                    runtime_dir: args.runtime_dir,
                    working_dir: args.connect.working_dir,
                    coordinator: args.connect.coordinator,
                    token: args.connect.token,
                    token_file: args.connect.token_file,
                    json,
                };
                finish(stderr, command::cluster::status(stdout, &a), |r| {
                    r.exit_code()
                })
            }
        },

        // ---- Over the wire: dataflow lifecycle ------------------------
        Command::Start(args) => {
            let level = match level_of(args.level.as_deref()) {
                Ok(level) => level,
                Err(err) => return report_error(stderr, &err),
            };
            let a = command::lifecycle::StartArgs {
                manifest_path: args.manifest,
                name: args.name,
                machines: args.machines,
                attach: args.attach,
                level,
                json,
                color: use_color,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::lifecycle::start(out, endpoint, &a).map(|report| report.exit_code())
            })
        }
        Command::Stop(args) => {
            let a = command::lifecycle::StopArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                grace: args.grace.map(Duration::from_secs_f64),
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::lifecycle::stop(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Restart(args) => {
            let a = command::lifecycle::RestartArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                rebuild: args.rebuild,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::lifecycle::restart(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Destroy(args) => {
            let a = command::lifecycle::DestroyArgs {
                dataflow: args.dataflow.as_deref().map(DataflowRef::parse),
                force: args.force,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::lifecycle::destroy(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Clean(args) => {
            let a = command::lifecycle::CleanArgs {
                dataflow: args.dataflow.as_deref().map(DataflowRef::parse),
                artifacts: args.artifacts,
                logs: args.logs,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::lifecycle::clean(out, endpoint, &a).map(|()| 0)
            })
        }

        // ---- Over the wire: monitoring --------------------------------
        Command::List(args) => {
            let a = command::monitor::ListArgs {
                all: args.all,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::monitor::list(out, endpoint, &a).map(|_| 0)
            })
        }
        Command::Logs(args) => {
            let level = match level_of(args.level.as_deref()) {
                Ok(level) => level,
                Err(err) => return report_error(stderr, &err),
            };
            let a = command::monitor::LogsArgs {
                dataflow: args.dataflow.as_deref().map(DataflowRef::parse),
                node: args.node,
                level,
                limit: args.limit,
                follow: args.follow,
                json,
                color: use_color,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::monitor::logs(out, endpoint, &a).map(|_| 0)
            })
        }

        // ---- Over the wire: parameters --------------------------------
        Command::Param(cli::ParamCommand::Get(args)) => {
            let scope = command::param::ScopeArgs {
                dataflow: args.dataflow,
                node: args.node,
                json,
            };
            let inherited = !args.exact;
            if args.watch {
                with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                    command::param::watch(out, endpoint, &scope, &args.key, inherited, args.count)
                        .map(|_| 0)
                })
            } else {
                with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                    command::param::get(out, endpoint, &scope, &args.key, inherited)
                        // A key that is not set exits non-zero, so
                        // `astrs param get … >/dev/null && …` is usable.
                        .map(|value| i32::from(value.is_none()))
                })
            }
        }
        Command::Param(cli::ParamCommand::Set(args)) => {
            let scope = command::param::ScopeArgs {
                dataflow: args.dataflow,
                node: args.node,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::param::set(
                    out,
                    endpoint,
                    &scope,
                    &args.key,
                    &args.value,
                    args.create_only,
                )
                .map(|_| 0)
            })
        }
        Command::Param(cli::ParamCommand::List(args)) => {
            let scope = command::param::ScopeArgs {
                dataflow: args.dataflow,
                node: args.node,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::param::list(
                    out,
                    endpoint,
                    &scope,
                    args.prefix.as_deref(),
                    args.inherited,
                )
                .map(|_| 0)
            })
        }
        Command::Param(cli::ParamCommand::Delete(args)) => {
            let scope = command::param::ScopeArgs {
                dataflow: args.dataflow,
                node: args.node,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::param::delete(out, endpoint, &scope, &args.key).map(|()| 0)
            })
        }

        // ---- Hidden servers -------------------------------------------
        Command::Coordinator(args) => {
            let a = command::serve::CoordinatorServeArgs {
                port: args.port,
                bind: args.bind,
                token: args.token,
                token_file: args.token_file,
                working_dir: args.working_dir,
                store: args.store,
                recreate_store: args.recreate_store,
                pidfile: args.pidfile,
                announce: args.announce,
                ha_node_id: args.ha_node_id,
                ha_peers: args.ha_peers,
            };
            finish(stderr, command::serve::coordinator(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Daemon(args) => {
            let a = command::serve::DaemonServeArgs {
                coordinator: args.coordinator,
                machine: args.machine,
                port: args.port,
                peer_port: args.peer_port,
                peer_bind: args.peer_bind,
                labels: args.labels,
                runtime_dir: args.runtime_dir,
                working_dir: args.working_dir,
                token: args.token,
                token_file: args.token_file,
                pidfile: args.pidfile,
                announce: args.announce,
            };
            finish(stderr, command::serve::daemon(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Runtime(args) => {
            let a = command::serve::RuntimeServeArgs {
                node_id: args.node_id,
            };
            finish(stderr, command::serve::runtime(stdout, &a), |r| {
                r.exit_code()
            })
        }

        // ---- Over the wire: topics -------------------------------------
        Command::Topic(cli::TopicCommand::Echo(args)) => {
            let a = command::topic::TopicStreamArgs {
                topic: args.topic,
                dataflow: args.dataflow,
                json,
                count: args.count,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::topic::echo(out, endpoint, &a).map(|_| 0)
            })
        }
        Command::Topic(cli::TopicCommand::Hz(args)) => {
            let window = std::time::Duration::from_secs_f64(args.window.max(0.001));
            let a = command::topic::TopicStreamArgs {
                topic: args.topic,
                dataflow: args.dataflow,
                json,
                count: args.count,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::topic::hz(out, endpoint, &a, window).map(|_| 0)
            })
        }
        Command::Topic(cli::TopicCommand::Info(args)) => {
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::topic::info(
                    out,
                    endpoint,
                    &args.topic,
                    args.dataflow.as_deref(),
                    args.manifest.as_deref(),
                    json,
                )
                .map(|_| 0)
            })
        }
        Command::Topic(cli::TopicCommand::Pub(args)) => {
            let a = command::topic::PublishArgs {
                topic: args.topic,
                message: args.message,
                dataflow: args.dataflow,
                rate: args.rate,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::topic::publish(out, endpoint, &a).map(|_| 0)
            })
        }
        Command::Trace(args) => {
            let a = command::trace::TraceArgs {
                dataflow: args.dataflow,
                node: args.node,
                since: None,
                limit: args.limit,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |out, endpoint| {
                command::trace::run(out, endpoint, &a).map(|_| 0)
            })
        }

        Command::Top(args) => {
            let a = command::top::TopArgs {
                replay: args.replay,
            };
            with_endpoint(stdout, stderr, &args.connect, false, |_out, endpoint| {
                command::top::run(endpoint, &a).map(|()| 0)
            })
        }

        // ---- Dynamic topology (blueprint §8, §17) ---------------------
        Command::Node(cli::NodeCommand::Add(args)) => {
            let a = command::node::AddArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                node_manifest: args.node_manifest,
                start: !args.no_start,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::node::add(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Node(cli::NodeCommand::Remove(args)) => {
            let a = command::node::RemoveArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                node_id: args.node_id,
                grace: args.grace,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::node::remove(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Node(cli::NodeCommand::Replace(args)) => {
            let a = command::node::ReplaceArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                node_id: args.node_id,
                node_manifest: args.node_manifest,
                drain: args.drain,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::node::replace(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Node(cli::NodeCommand::Connect(args)) => {
            let a = command::node::ConnectArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                node_id: args.node_id,
                input: args.input,
                source: args.source,
                queue_size: args.queue_size,
                queue_policy: args.queue_policy,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::node::connect(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Node(cli::NodeCommand::Disconnect(args)) => {
            let a = command::node::DisconnectArgs {
                dataflow: DataflowRef::parse(&args.dataflow),
                node_id: args.node_id,
                input: args.input,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::node::disconnect(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Record(cli::RecordCommand::Start(args)) => {
            let a = command::record::StartArgs {
                dataflow: args.dataflow,
                output: args.output.display().to_string(),
                only: args.only,
                overwrite: args.overwrite,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::record::start(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Record(cli::RecordCommand::Stop(args)) => {
            let a = command::record::StopArgs {
                dataflow: args.dataflow,
                json,
            };
            with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                command::record::stop(out, endpoint, &a).map(|()| 0)
            })
        }
        Command::Replay(args) => {
            match (args.into, args.dataflow) {
                (Some(into), _) => {
                    let a = command::replay::ReplayIntoArgs {
                        input: args.input,
                        into: std::path::PathBuf::from(into),
                        mode: args.mode,
                        speed: args.speed,
                        rate: args.rate,
                        r#loop: args.r#loop,
                        replace: args.replace,
                    };
                    finish(stderr, command::replay::run(stdout, &a), |_| 0)
                }
                (None, Some(dataflow)) => {
                    let a = command::replay::ReplayLiveArgs {
                        input: args.input,
                        dataflow: DataflowRef::parse(&dataflow),
                        mode: args.mode,
                        speed: args.speed,
                        rate: args.rate,
                        r#loop: args.r#loop,
                        replace: args.replace,
                        drain: args.drain,
                        json,
                    };
                    // A mutating verb (§16): it replaces nodes in a running
                    // cluster, so it needs a token before it dials.
                    with_endpoint(stdout, stderr, &args.connect, true, |out, endpoint| {
                        command::replay::run_live(out, endpoint, &a).map(|_| 0)
                    })
                }
                // Neither: the verb has two forms and no default. Saying so
                // beats guessing, because the two do very different things —
                // one prints YAML, the other changes a running cluster.
                (None, None) => report_error(
                    stderr,
                    &CliError::BadArgument {
                        flag: "into",
                        value: String::new(),
                        reason: "astrs replay needs a target: `--into <manifest.yml>` to \
                                 print a rewritten manifest, or a running dataflow's id \
                                 or name to cut it over in place"
                            .to_owned(),
                    },
                ),
            }
        }
        Command::Bag(cli::BagCommand::Info(args)) => {
            let a = command::bag::InfoArgs {
                input: args.input,
                json,
            };
            finish(stderr, command::bag::info(stdout, &a), |_| 0)
        }
        Command::Bag(cli::BagCommand::Convert(args)) => {
            let a = command::bag::ConvertArgs {
                input: args.input,
                output: args.output,
                json,
            };
            finish(stderr, command::bag::convert(stdout, &a), |_| 0)
        }
        Command::Ros2(cli::Ros2Command::Doctor(args)) => {
            let a = probe_args(args.domain_id, args.timeout, args.hidden, json);
            finish(stderr, command::ros2_doctor::run(stdout, &a), |r| {
                r.exit_code()
            })
        }
        Command::Ros2(cli::Ros2Command::Topics(args)) => {
            let a = probe_args(args.domain_id, args.timeout, args.hidden, json);
            finish(stderr, command::ros2_topics::run(stdout, &a), |r| {
                r.exit_code()
            })
        }
    }
}

/// The [`command::ros2::ProbeArgs`] an `astrs ros2 …` flag set describes.
///
/// One helper for both verbs: they take the same four flags, and a second
/// copy of this mapping is a second place for them to drift.
fn probe_args(
    domain_id: Option<u32>,
    timeout_ms: Option<u64>,
    hidden: bool,
    json: bool,
) -> command::ros2::ProbeArgs {
    let defaults = command::ros2::ProbeArgs::default();
    command::ros2::ProbeArgs {
        domain_id: domain_id.unwrap_or(defaults.domain_id),
        timeout: timeout_ms.map_or(defaults.timeout, std::time::Duration::from_millis),
        include_hidden: hidden,
        json,
    }
}

/// Parses an optional `--level` argument.
///
/// # Errors
///
/// [`CliError::BadArgument`] naming the accepted spellings.
fn level_of(text: Option<&str>) -> Result<Option<astrs_wire::LogLevel>, CliError> {
    match text {
        Some(text) => command::log_stream::parse_level("level", text).map(Some),
        None => Ok(None),
    }
}

/// Builds the coordinator endpoint a client verb needs, runs `body` with
/// it, and flattens the result into an exit code.
///
/// `require_token` is §16's read/mutate split (see this module's own docs):
/// a mutating verb refuses locally rather than dialling without one.
fn with_endpoint(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    connect: &cli::ConnectArgs,
    require_token: bool,
    body: impl FnOnce(&mut dyn Write, &Endpoint) -> Result<i32, CliError>,
) -> i32 {
    let endpoint = match endpoint_of(connect, require_token) {
        Ok(endpoint) => endpoint,
        Err(err) => return report_error(stderr, &err),
    };
    finish(stderr, body(stdout, &endpoint), |code| *code)
}

/// The endpoint the shared `--coordinator`/`--token`/`--token-file`/
/// `--working-dir` flags describe.
///
/// # Errors
///
/// As [`command::client::endpoint`].
fn endpoint_of(connect: &cli::ConnectArgs, require_token: bool) -> Result<Endpoint, CliError> {
    command::client::endpoint(
        connect.coordinator.as_deref(),
        connect.token.as_deref(),
        connect.token_file.as_deref(),
        connect.working_dir.as_deref(),
        require_token,
    )
}

/// Print `err` to `stderr` and return its exit code.
fn report_error(stderr: &mut dyn Write, err: &CliError) -> i32 {
    let _ = writeln!(stderr, "error: {err}");
    err.exit_code()
}

/// Flatten a command's `Result` into an exit code: on `Ok`, apply
/// `ok_exit_code` to the report; on `Err`, print the error to `stderr`
/// and use its own exit code.
///
/// Takes no `stdout` parameter: every `command::*::run` function already
/// wrote its report to whatever sink it was given before returning, so
/// there is nothing left for this step to write on the `Ok` path.
fn finish<R>(
    stderr: &mut dyn Write,
    result: Result<R, CliError>,
    ok_exit_code: impl FnOnce(&R) -> i32,
) -> i32 {
    match result {
        Ok(report) => ok_exit_code(&report),
        Err(err) => report_error(stderr, &err),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv).unwrap()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-dispatch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Both `astrs ros2` verbs parse into the probe arguments they
    /// describe. The probe itself is not run here — that needs a live DDS
    /// domain, which `astrs-ros2-bridge-node`'s loopback tests provide —
    /// so what is pinned is the flag mapping, which is the part that can
    /// silently drift.
    #[test]
    fn the_ros2_verbs_map_their_flags_onto_probe_arguments() {
        let defaults = probe_args(None, None, false, false);
        assert_eq!(defaults.timeout, command::ros2::DEFAULT_TIMEOUT);
        assert!(!defaults.include_hidden);
        assert!(!defaults.json);

        let explicit = probe_args(Some(7), Some(250), true, true);
        assert_eq!(explicit.domain_id, 7);
        assert_eq!(explicit.timeout, std::time::Duration::from_millis(250));
        assert!(explicit.include_hidden);
        assert!(explicit.json);

        // …and the parser accepts exactly those flags on both verbs.
        for verb in ["doctor", "topics"] {
            let cli = parse(&[
                "astrs",
                "ros2",
                verb,
                "--domain-id",
                "3",
                "--timeout",
                "100",
                "--hidden",
            ]);
            match cli.command {
                Command::Ros2(cli::Ros2Command::Doctor(args)) => {
                    assert_eq!(args.domain_id, Some(3));
                    assert_eq!(args.timeout, Some(100));
                    assert!(args.hidden);
                }
                Command::Ros2(cli::Ros2Command::Topics(args)) => {
                    assert_eq!(args.domain_id, Some(3));
                    assert_eq!(args.timeout, Some(100));
                    assert!(args.hidden);
                }
                other => panic!("`ros2 {verb}` parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn bag_info_dispatches_end_to_end_against_a_real_file() {
        let dir = scratch("bag-info");
        let path = dir.join("session.arec");
        let options = astrs_recording::WriterOptions::new(
            astrs_wire::DataflowId::from_u128(1),
            astrs_time::HlcTimestamp::EPOCH,
        );
        let mut writer = astrs_recording::Writer::create(&path, options).unwrap();
        writer
            .append_parts(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("frames").unwrap(),
                astrs_wire::Metadata::new(astrs_time::HlcTimestamp::new(1, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        writer.finish().unwrap();

        let cli = parse(&["astrs", "bag", "info", path.to_str().unwrap()]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "{}", String::from_utf8(err).unwrap());
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("camera/frames"), "{printed}");
    }

    #[test]
    fn bag_convert_dispatches_end_to_end_against_a_real_file() {
        let dir = scratch("bag-convert");
        let input = dir.join("session.arec");
        let output = dir.join("session.db3");
        let options = astrs_recording::WriterOptions::new(
            astrs_wire::DataflowId::from_u128(1),
            astrs_time::HlcTimestamp::EPOCH,
        );
        let mut writer = astrs_recording::Writer::create(&input, options).unwrap();
        writer
            .append_parts(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("frames").unwrap(),
                astrs_wire::Metadata::new(astrs_time::HlcTimestamp::new(1, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        writer.finish().unwrap();

        let cli = parse(&[
            "astrs",
            "bag",
            "convert",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "{}", String::from_utf8(err).unwrap());
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("arec -> db3"), "{printed}");
        assert!(output.exists());

        // The whole point of `bag convert`: the written `.db3` opens with
        // a real rosbag2 reader and carries the same message.
        let reader = astrs_rosbag::db3::Reader::open(&output).unwrap();
        assert_eq!(reader.topics().len(), 1);
    }

    #[test]
    fn replay_dispatches_end_to_end_and_prints_a_rewritten_manifest() {
        let dir = scratch("replay-into");
        let recording = dir.join("session.arec");
        let options = astrs_recording::WriterOptions::new(
            astrs_wire::DataflowId::from_u128(1),
            astrs_time::HlcTimestamp::EPOCH,
        );
        let mut writer = astrs_recording::Writer::create(&recording, options).unwrap();
        writer
            .append_parts(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("frames").unwrap(),
                astrs_wire::Metadata::new(astrs_time::HlcTimestamp::new(1, 0)),
                vec![1],
            )
            .unwrap();
        writer.finish().unwrap();

        let manifest = dir.join("graph.yaml");
        std::fs::write(
            &manifest,
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        )
        .unwrap();

        let cli = parse(&[
            "astrs",
            "replay",
            recording.to_str().unwrap(),
            "--into",
            manifest.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "{}", String::from_utf8(err).unwrap());
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("astrs-replay-node"), "{printed}");
        let rewritten = astrs_manifest::Manifest::from_yaml_str(&printed).unwrap();
        rewritten.validate().expect("must still validate");
    }

    #[test]
    fn replay_without_into_is_reported_rather_than_guessed() {
        let cli = parse(&["astrs", "replay", "session.arec"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 1);
        let message = String::from_utf8(err).unwrap();
        assert!(message.contains("--into"), "{message}");
    }

    #[test]
    fn schema_dispatches_and_prints_json_to_stdout() {
        let cli = parse(&["astrs", "schema"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0);
        assert!(err.is_empty());
        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim_end()).unwrap();
        assert!(value.is_object());
    }

    #[test]
    fn validate_dispatches_with_exit_code_two_for_a_missing_file() {
        let cli = parse(&["astrs", "validate", "/does/not/exist.yaml"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 2);
        assert!(
            err.is_empty(),
            "validate embeds failures in its report, not stderr"
        );
    }

    #[test]
    fn migrate_from_ros2_reports_a_missing_file_on_stderr() {
        let cli = parse(&["astrs", "migrate", "from-ros2", "does-not-exist.launch.xml"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 1, "a missing input file is an ordinary failure");
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    #[test]
    fn migrate_from_ros2_dispatches_a_real_launch_file() {
        let dir = scratch("migrate-ros2");
        let input = dir.join("robot.launch.xml");
        std::fs::write(
            &input,
            r#"<launch><node pkg="p" exec="e" name="n"/></launch>"#,
        )
        .unwrap();
        let cli = parse(&["astrs", "migrate", "from-ros2", input.to_str().unwrap()]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(
            code, 2,
            "message_type is never inferable from a launch file"
        );
        assert!(err.is_empty(), "err: {}", String::from_utf8_lossy(&err));
        assert!(!out.is_empty());
    }

    #[test]
    fn completion_dispatches_for_every_shell() {
        for shell in ["bash", "zsh", "fish"] {
            let cli = parse(&["astrs", "completion", shell]);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = dispatch(cli, &mut out, &mut err, false);
            assert_eq!(code, 0, "shell {shell}");
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn new_graph_dispatches_into_a_real_scaffold() {
        let dir = scratch("new-graph");
        let cli = parse(&[
            "astrs",
            "new",
            "graph",
            "perception",
            "--dir",
            dir.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert!(dir.join("perception.astrs.yml").exists());
    }

    #[test]
    fn a_bad_level_is_reported_before_anything_runs() {
        for argv in [
            &["astrs", "run", "d.yml", "--level", "loud"][..],
            &["astrs", "logs", "--level", "loud"][..],
            &["astrs", "start", "d.yml", "--level", "loud"][..],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = dispatch(parse(argv), &mut out, &mut err, false);
            assert_eq!(code, 1, "{argv:?}");
            assert!(out.is_empty(), "{argv:?}");
            let message = String::from_utf8(err).unwrap();
            assert!(message.contains("trace"), "{message}");
        }
    }

    #[test]
    fn every_mutating_verb_refuses_locally_without_a_token() {
        // §16's split: no token, no mutation — and the refusal names every
        // way one could have been supplied, without dialling anything.
        let dir = scratch("no-token");
        let working = dir.to_str().unwrap();
        for argv in [
            &["astrs", "stop", "flow"][..],
            &["astrs", "restart", "flow"][..],
            &["astrs", "destroy"][..],
            &["astrs", "clean"][..],
            &["astrs", "param", "set", "flow", "k", "1"][..],
            &["astrs", "param", "delete", "flow", "k"][..],
            &["astrs", "record", "start", "flow", "out.arec"][..],
            &["astrs", "record", "stop", "flow"][..],
        ] {
            let mut full: Vec<&str> = argv.to_vec();
            full.extend(["--working-dir", working, "--coordinator", "127.0.0.1:1"]);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = dispatch(parse(&full), &mut out, &mut err, false);
            assert_eq!(code, 1, "{full:?}");
            let message = String::from_utf8(err).unwrap();
            assert!(message.contains("no cluster token"), "{full:?}: {message}");
        }
    }

    #[test]
    fn every_read_verb_dials_without_a_token_and_reports_no_cluster() {
        let dir = scratch("read-verbs");
        let working = dir.to_str().unwrap();
        for argv in [
            &["astrs", "list"][..],
            &["astrs", "logs"][..],
            &["astrs", "status", "flow"][..],
            &["astrs", "param", "get", "flow", "k"][..],
            &["astrs", "param", "list", "flow"][..],
        ] {
            let mut full: Vec<&str> = argv.to_vec();
            full.extend(["--working-dir", working, "--coordinator", "127.0.0.1:1"]);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = dispatch(parse(&full), &mut out, &mut err, false);
            assert_eq!(code, 1, "{full:?}");
            let message = String::from_utf8(err).unwrap();
            assert!(
                message.contains("no cluster is running"),
                "{full:?}: {message}"
            );
        }
    }

    #[test]
    fn cluster_status_reports_an_empty_runtime_directory_rather_than_failing() {
        let dir = scratch("status");
        let cli = parse(&[
            "astrs",
            "--json",
            "status",
            "--runtime-dir",
            dir.to_str().unwrap(),
            "--working-dir",
            dir.to_str().unwrap(),
            "--coordinator",
            "127.0.0.1:1",
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 1, "an unreachable cluster exits non-zero");
        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(value["reachable"], false);
        assert_eq!(value["coordinator"]["running"], false);
        assert!(err.is_empty(), "status embeds the outcome in its report");
    }

    #[test]
    fn down_on_an_empty_runtime_directory_succeeds() {
        let dir = scratch("down");
        let cli = parse(&[
            "astrs",
            "down",
            "--runtime-dir",
            dir.to_str().unwrap(),
            "--working-dir",
            dir.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("nothing was running")
        );
    }

    #[test]
    fn build_dispatches_into_the_real_build_engine() {
        let dir = scratch("build");
        let manifest = dir.join("d.yml");
        std::fs::write(
            &manifest,
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    build: /bin/sh -c 'echo built-by-dispatch'\n",
        )
        .unwrap();
        let cli = parse(&[
            "astrs",
            "build",
            manifest.to_str().unwrap(),
            "--runtime-dir",
            dir.to_str().unwrap(),
            "--working-dir",
            dir.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("built-by-dispatch")
        );
    }

    #[test]
    fn a_runtime_process_started_by_hand_fails_with_a_typed_node_error() {
        // The hidden `runtime` verb is spawned by a daemon; by hand there is
        // no handshake blob, and the failure must be typed, not a panic.
        let cli = parse(&["astrs", "runtime", "--node-id", "cam"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(cli, &mut out, &mut err, false);
        assert_eq!(code, 1);
        assert!(!err.is_empty());
    }
}
