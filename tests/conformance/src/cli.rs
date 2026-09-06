//! Driving the real `astrs` binary as a child process.
//!
//! # Why a subprocess, when the suite can call the verb directly
//!
//! `tests/m1_single_machine.rs` calls `astrs_cli::command::run::run` in
//! process. That proves the dataflow machinery, and it gives a failure the
//! whole captured terminal to explain itself with. What it does *not* exercise
//! is everything between a shell prompt and that function: clap's parsing,
//! `main`'s exit-code mapping, the process's own stdout, and the fact that a
//! node is spawned by a program the adopter installed rather than by a test
//! binary that happens to link the same crate.
//!
//! [`AstrsCli`] closes that gap. It runs the same command a reader types —
//! `astrs run examples/…/dataflow.yml` — and captures what the terminal would
//! have shown.
//!
//! # Two details that matter, and are easy to get wrong
//!
//! * **Both pipes are drained concurrently.** A graph that fills the stderr
//!   pipe while the harness is blocked reading stdout deadlocks, and the
//!   deadlock looks exactly like a hung dataflow. Two reader threads make the
//!   failure impossible rather than unlikely.
//! * **A timeout kills, and still reports.** [`FixtureError::Timeout`] carries
//!   everything the process had printed, because the first question about a
//!   hang is always "how far did it get".

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::FixtureError;
use crate::paths::cli_binary;

/// How often a running child is checked for having finished.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The default ceiling on one `astrs` invocation.
///
/// Generous: these are real process graphs on a machine shared with whatever
/// else is building. The tests assert on outcomes, never on how long they
/// took, so a high ceiling costs nothing and a low one costs flakes.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// A configured invocation of the `astrs` command-line binary.
#[derive(Debug, Clone)]
pub struct AstrsCli {
    /// The binary to run.
    exe: PathBuf,
    /// The working directory, if the invocation needs a particular one.
    current_dir: Option<PathBuf>,
    /// Environment variables to set on the child.
    env_set: BTreeMap<String, String>,
    /// Environment variables to remove from the child's inherited environment.
    env_remove: BTreeSet<String>,
    /// How long the child may run before it is killed.
    timeout: Duration,
}

impl AstrsCli {
    /// Finds the built `astrs` binary.
    ///
    /// # Errors
    ///
    /// [`FixtureError::MissingBinary`] naming `cargo build -p astrs-cli`.
    pub fn discover() -> Result<Self, FixtureError> {
        Ok(Self {
            exe: cli_binary()?,
            current_dir: None,
            env_set: BTreeMap::new(),
            env_remove: BTreeSet::new(),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// The binary this invocation will run.
    #[must_use]
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Runs the child from `dir`.
    ///
    /// Relative manifest paths — the ones a README prints — are resolved
    /// against this, so a test that runs a committed manifest verbatim sets it
    /// to the workspace root.
    #[must_use]
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Sets an environment variable on the child.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_set.insert(key.into(), value.into());
        self
    }

    /// Removes an inherited environment variable from the child.
    #[must_use]
    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.insert(key.into());
        self
    }

    /// Sets the ceiling on how long the child may run.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs `astrs` with `args`, capturing both streams to completion.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Spawn`] when the process cannot be started and
    /// [`FixtureError::Timeout`] when it outlives [`Self::timeout`], with
    /// whatever it printed attached.
    pub fn run(&self, args: &[&str]) -> Result<CliOutcome, FixtureError> {
        let command_line = self.command_line(args);
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        for key in &self.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &self.env_set {
            command.env(key, value);
        }

        let started = Instant::now();
        let mut child = command.spawn().map_err(|source| FixtureError::Spawn {
            command: command_line.clone(),
            source,
        })?;

        // Both pipes drain on their own threads: reading them in sequence
        // deadlocks the moment the other one fills its buffer.
        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);

        let deadline = started + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // Kill first, then collect: the reader threads only
                        // finish once the pipes close, which needs the child
                        // gone.
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(source) => {
                    let _ = child.kill();
                    return Err(FixtureError::Spawn {
                        command: command_line,
                        source,
                    });
                }
            }
        };
        let elapsed = started.elapsed();

        let stdout = stdout_reader.map_or_else(String::new, join_reader);
        let stderr = stderr_reader.map_or_else(String::new, join_reader);

        let Some(status) = status else {
            return Err(FixtureError::Timeout {
                command: command_line,
                after: self.timeout,
                output: combine(&stdout, &stderr),
            });
        };

        Ok(CliOutcome {
            command: command_line,
            code: status.code(),
            signal: signal_of(&status),
            stdout,
            stderr,
            elapsed,
        })
    }

    /// The command line this invocation would run, for a failure message.
    fn command_line(&self, args: &[&str]) -> String {
        let mut line = String::new();
        for (key, value) in &self.env_set {
            line.push_str(key);
            line.push('=');
            line.push_str(value);
            line.push(' ');
        }
        line.push_str(&self.exe.display().to_string());
        for arg in args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// The exit signal of a status, on platforms that have them.
#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// The exit signal of a status; always [`None`] off Unix.
#[cfg(not(unix))]
const fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Reads a pipe to end on its own thread.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // A read error on a pipe whose process is being killed is expected;
        // whatever arrived before it is still the useful part.
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

/// Collects a reader thread's bytes as text.
///
/// A panicked reader yields a note rather than propagating: the child's own
/// behaviour is what is under test, and losing one stream must not hide it.
fn join_reader(handle: std::thread::JoinHandle<Vec<u8>>) -> String {
    handle.join().map_or_else(
        |_| "<the capture thread did not finish>".to_owned(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Concatenates the two captured streams in the order a terminal would.
fn combine(stdout: &str, stderr: &str) -> String {
    let mut text = String::with_capacity(stdout.len() + stderr.len());
    text.push_str(stdout);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(stderr);
    }
    text
}

/// What one `astrs` invocation did.
#[derive(Debug, Clone)]
pub struct CliOutcome {
    /// The command line that produced it.
    pub command: String,
    /// The exit code, or [`None`] when the process was signalled.
    pub code: Option<i32>,
    /// The signal that ended the process, on Unix.
    pub signal: Option<i32>,
    /// Everything the process wrote to stdout.
    pub stdout: String,
    /// Everything the process wrote to stderr.
    pub stderr: String,
    /// How long it took.
    pub elapsed: Duration,
}

impl CliOutcome {
    /// Whether the process exited zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    /// Both streams, in the order a terminal would have shown them.
    #[must_use]
    pub fn combined(&self) -> String {
        combine(&self.stdout, &self.stderr)
    }

    /// Whether `needle` appears in either stream.
    #[must_use]
    pub fn says(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }

    /// The last `lines` lines of the combined output.
    ///
    /// What a report quotes as evidence: the tail of a run is where the
    /// summary line is.
    #[must_use]
    pub fn tail(&self, lines: usize) -> String {
        let combined = self.combined();
        let kept: Vec<&str> = combined
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        kept.join("\n")
    }

    /// A full account of the invocation, for an assertion message.
    #[must_use]
    pub fn describe(&self) -> String {
        let ended = match (self.code, self.signal) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "no status".to_owned(),
        };
        format!(
            "$ {command}\n({ended} after {elapsed:?})\n{output}",
            command = self.command,
            elapsed = self.elapsed,
            output = self.combined()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The harness finds the built CLI, or says how to build it.
    #[test]
    fn the_cli_is_found_or_the_build_line_is_named() {
        match AstrsCli::discover() {
            Ok(cli) => assert!(cli.exe().is_file(), "{}", cli.exe().display()),
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("cargo build -p astrs-cli"), "{text}");
            }
        }
    }

    /// The rendered command line shows the environment a failure would need.
    #[test]
    fn the_command_line_shows_the_environment() {
        let cli = AstrsCli {
            exe: PathBuf::from("/t/astrs"),
            current_dir: None,
            env_set: BTreeMap::from([("PROBE_FRAMES".to_owned(), "4".to_owned())]),
            env_remove: BTreeSet::new(),
            timeout: DEFAULT_TIMEOUT,
        };
        let line = cli.command_line(&["run", "x.yml"]);
        assert_eq!(line, "PROBE_FRAMES=4 /t/astrs run x.yml");
    }

    /// `tail` keeps the *last* lines, which is where a summary lives.
    #[test]
    fn the_tail_is_the_last_lines() {
        let outcome = CliOutcome {
            command: "astrs run x.yml".to_owned(),
            code: Some(0),
            signal: None,
            stdout: "one\ntwo\nthree\nfour\n".to_owned(),
            stderr: String::new(),
            elapsed: Duration::from_millis(5),
        };
        assert_eq!(outcome.tail(2), "three\nfour");
        assert_eq!(outcome.tail(99), "one\ntwo\nthree\nfour");
        assert!(outcome.succeeded());
        assert!(outcome.says("three"));
        assert!(!outcome.says("five"));
    }

    /// Both streams appear in the combined view, stdout first, with a
    /// separator the reader does not have to guess at.
    #[test]
    fn both_streams_are_combined() {
        let outcome = CliOutcome {
            command: "astrs run x.yml".to_owned(),
            code: Some(1),
            signal: None,
            stdout: "out".to_owned(),
            stderr: "err".to_owned(),
            elapsed: Duration::from_millis(5),
        };
        assert_eq!(outcome.combined(), "out\nerr");
        assert!(!outcome.succeeded());
        let described = outcome.describe();
        assert!(described.contains("exit 1"), "{described}");
        assert!(described.contains("astrs run x.yml"), "{described}");
    }

    /// A signalled process is described as such rather than as exit code
    /// nothing.
    #[test]
    fn a_signalled_process_is_described_by_its_signal() {
        let outcome = CliOutcome {
            command: "astrs run x.yml".to_owned(),
            code: None,
            signal: Some(9),
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::from_secs(1),
        };
        assert!(outcome.describe().contains("signal 9"));
        assert!(!outcome.succeeded());
    }

    /// A run that outlives its budget reports a timeout rather than hanging
    /// the suite. `--help` on a binary that does not exist cannot be used for
    /// this, so the check is on the error rendering path instead.
    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let cli = AstrsCli {
            exe: PathBuf::from("/nonexistent/astrs-conformance-cli"),
            current_dir: None,
            env_set: BTreeMap::new(),
            env_remove: BTreeSet::new(),
            timeout: Duration::from_secs(1),
        };
        let error = cli.run(&["--version"]).unwrap_err();
        assert!(matches!(error, FixtureError::Spawn { .. }), "{error}");
    }

    /// The real binary answers `--version` quickly, which is the cheapest
    /// possible proof that the harness spawns, captures and reaps correctly.
    ///
    /// Fails rather than skips when the binary is missing — the crate docs
    /// name the `cargo build -p astrs-cli` line this needs, and a suite that
    /// quietly passes without running anything proves nothing.
    #[test]
    fn the_harness_captures_a_real_invocation() {
        let cli = AstrsCli::discover().unwrap_or_else(|error| panic!("{error}"));
        let outcome = cli
            .timeout(Duration::from_secs(30))
            .run(&["--version"])
            .expect("astrs --version ran");
        assert!(outcome.succeeded(), "{}", outcome.describe());
        assert!(outcome.says("astrs"), "{}", outcome.describe());
    }
}
