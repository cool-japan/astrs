//! `astrs doctor` (blueprint §17): environment checks that need no live
//! daemon or coordinator — port availability, SHM limits, multicast
//! availability, real-time scheduling capability (§11.3/§22), toolchain
//! versions, runtime directory writability.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;

// Only [`check_rt_capability`]'s Linux body reads this — the non-Linux
// fallback below it is purely informational and names no priority number —
// so importing it unconditionally would be an unused import on every other
// platform.
#[cfg(target_os = "linux")]
use astrs_manifest::RT_PRIORITY_MAX;
use serde::Serialize;

use crate::error::CliError;

/// The default coordinator port (blueprint §4.2: "Listens on TCP 7407
/// (`ASTRS_COORDINATOR_PORT`; 7407 honors Atom's birthday 2003-04-07)").
///
/// Duplicated here (rather than depending on `astrs-transport`, which
/// also defines `DEFAULT_COORDINATOR_PORT`) because this stage of
/// `astrs-cli` has no other reason to depend on the transport crate at
/// all — see this crate's top-level report for the deliberate deviation
/// this is. Both constants must stay in sync; a snapshot test in this
/// module pins the value.
pub const DEFAULT_COORDINATOR_PORT: u16 = 7407;
/// The default local daemon port (blueprint §4.2).
pub const DEFAULT_DAEMON_PORT: u16 = 7408;

/// Arguments for `astrs doctor`.
#[derive(Debug, Clone)]
pub struct DoctorArgs {
    /// Emit the report as JSON instead of a table.
    pub json: bool,
    /// The coordinator port to probe.
    pub coordinator_port: u16,
    /// The local daemon port to probe.
    pub daemon_port: u16,
}

impl Default for DoctorArgs {
    fn default() -> Self {
        Self {
            json: false,
            coordinator_port: DEFAULT_COORDINATOR_PORT,
            daemon_port: DEFAULT_DAEMON_PORT,
        }
    }
}

/// How a [`DoctorCheck`] came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Everything is as expected.
    Ok,
    /// Worth knowing, not a problem (e.g. a platform-specific check that
    /// does not apply here).
    Info,
    /// Might be a problem; `astrs run`/`up` may still work.
    Warning,
    /// Very likely to break `astrs run`/`up`.
    Error,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// One environment check's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    /// A short, stable name (`"port:7407"`, `"toolchain:rustc"`, ...).
    pub name: String,
    /// The outcome.
    pub status: CheckStatus,
    /// A human-readable explanation, always present regardless of
    /// [`Self::status`] (even a passing check says what it found).
    pub detail: String,
}

/// The complete `astrs doctor` report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Every check, in a fixed, deterministic order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// `1` if any check is [`CheckStatus::Error`], else `0`.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(self.checks.iter().any(|c| c.status == CheckStatus::Error))
    }
}

/// Run every `astrs doctor` check and print the report to `out`.
///
/// Always returns `Ok` unless writing to `out` fails — every check that
/// can go wrong reports [`CheckStatus::Error`]/[`CheckStatus::Warning`]
/// as *content*, matching `validate`'s own "diagnostic tools report, they
/// do not abort" convention.
///
/// # Errors
///
/// Returns [`CliError::Io`] if writing to `out` fails.
pub fn run(out: &mut dyn Write, args: &DoctorArgs) -> Result<DoctorReport, CliError> {
    let checks = vec![
        check_port("port:coordinator", args.coordinator_port),
        check_port("port:daemon", args.daemon_port),
        check_shm(),
        check_multicast(),
        check_rt_capability(),
        check_toolchain("rustc"),
        check_toolchain("cargo"),
        check_runtime_dir(),
    ];
    let report = DoctorReport { checks };

    if args.json {
        let json = serde_json::to_string_pretty(&report).unwrap_or_else(|err| {
            format!("{{\"error\": \"failed to serialize doctor report: {err}\"}}")
        });
        writeln!(out, "{json}").map_err(|e| CliError::io("<output>", e))?;
    } else {
        writeln!(out, "{}", render_table(&report)).map_err(|e| CliError::io("<output>", e))?;
    }

    Ok(report)
}

/// Probe whether `port` is currently bindable on loopback.
fn check_port(name: &str, port: u16) -> DoctorCheck {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_listener) => DoctorCheck {
            name: name.to_string(),
            status: CheckStatus::Ok,
            detail: format!("127.0.0.1:{port} is available"),
        },
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => DoctorCheck {
            name: name.to_string(),
            status: CheckStatus::Warning,
            detail: format!(
                "127.0.0.1:{port} is already in use (a daemon/coordinator may already be running): {err}"
            ),
        },
        Err(err) => DoctorCheck {
            name: name.to_string(),
            status: CheckStatus::Error,
            detail: format!("could not probe 127.0.0.1:{port}: {err}"),
        },
    }
}

/// The multicast group RTPS SPDP participant discovery uses (blueprint
/// §10: "SPDP: multicast 239.255.0.1, standard port mapping"). Probed
/// directly rather than guessed at: joining it end to end is a much
/// stronger signal than checking for the presence of a multicast-capable
/// interface, and it is the exact address AstRS's own ROS 2 bridge will
/// need later.
const RTPS_SPDP_GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 0, 1);

/// Probe UDP multicast join capability (blueprint §17: "multicast
/// availability").
///
/// [`CheckStatus::Warning`], never [`CheckStatus::Error`], on failure:
/// multicast is only needed for ROS 2 discovery (blueprint §10) — AstRS's
/// own control plane (coordinator/daemon registration, §6.2–§6.4) never
/// uses it, so a dataflow-only AstRS install is unaffected.
fn check_multicast() -> DoctorCheck {
    let name = "multicast".to_string();
    let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => socket,
        Err(err) => {
            return DoctorCheck {
                name,
                status: CheckStatus::Warning,
                detail: format!("could not open a UDP socket to probe multicast: {err}"),
            };
        }
    };
    match socket.join_multicast_v4(&RTPS_SPDP_GROUP, &std::net::Ipv4Addr::UNSPECIFIED) {
        Ok(()) => DoctorCheck {
            name,
            status: CheckStatus::Ok,
            detail: format!("joined the RTPS SPDP probe group {RTPS_SPDP_GROUP} successfully"),
        },
        Err(err) => DoctorCheck {
            name,
            status: CheckStatus::Warning,
            detail: format!(
                "could not join multicast group {RTPS_SPDP_GROUP}: {err} (ROS 2 discovery, \
                 blueprint §10, will not work on this network; AstRS's own control plane does \
                 not use multicast and is unaffected)"
            ),
        },
    }
}

/// Probe this process's real-time scheduling capability (hard-RT executor
/// reservations, blueprint §11.3/§22): the highest `sched_priority` a
/// manifest `rt: { policy: fifo|rr, priority: N }` block could actually
/// apply, combining `CAP_SYS_NICE` (unbounded — the full POSIX `1..=99`
/// range regardless of any rlimit) with the `RLIMIT_RTPRIO` soft limit (the
/// ceiling without it — `0`, the default for an unprivileged process, means
/// none at all). Mirrors exactly what `astrs-daemon`'s `spawn::rt` module
/// checks right before it would otherwise hit `EPERM`, so a `rt:`-using
/// manifest's failure mode is visible before `astrs run` ever attempts it.
#[cfg(target_os = "linux")]
fn check_rt_capability() -> DoctorCheck {
    let name = "rt".to_string();
    let capabilities = match rustix::thread::capabilities(None) {
        Ok(capabilities) => capabilities,
        Err(err) => {
            return DoctorCheck {
                name,
                status: CheckStatus::Warning,
                detail: format!("could not read this process's capabilities: {err}"),
            };
        }
    };
    let has_cap_sys_nice = capabilities
        .effective
        .contains(rustix::thread::CapabilitySet::SYS_NICE);
    let rtprio_soft = rustix::process::getrlimit(rustix::process::Resource::Rtprio)
        .current
        .unwrap_or(u64::MAX);
    let max_priority = if has_cap_sys_nice {
        u64::from(RT_PRIORITY_MAX)
    } else {
        rtprio_soft.min(u64::from(RT_PRIORITY_MAX))
    };

    if max_priority == 0 {
        DoctorCheck {
            name,
            status: CheckStatus::Warning,
            detail: "no real-time scheduling available: this process has neither \
                     CAP_SYS_NICE nor an RLIMIT_RTPRIO soft limit above 0 — a manifest \
                     `rt: { policy: fifo|rr, ... }` node will fail to spawn with EPERM. \
                     Grant CAP_SYS_NICE to the daemon binary (e.g. \
                     `setcap cap_sys_nice+ep <path-to-astrs-daemon>`) or raise \
                     RLIMIT_RTPRIO for the user running it (e.g. an `rtprio` line in \
                     /etc/security/limits.conf) to use it."
                .to_string(),
        }
    } else {
        DoctorCheck {
            name,
            status: CheckStatus::Ok,
            detail: format!(
                "real-time scheduling available up to priority {max_priority} (via {})",
                if has_cap_sys_nice {
                    "CAP_SYS_NICE"
                } else {
                    "RLIMIT_RTPRIO"
                }
            ),
        }
    }
}

/// Non-Linux platforms: `rt:` has no application implemented at all (see
/// `astrs-daemon::spawn::rt`'s own module docs) — reported plainly, the same
/// honest-fallback framing [`check_shm`] gives macOS's missing `/dev/shm`.
#[cfg(not(target_os = "linux"))]
fn check_rt_capability() -> DoctorCheck {
    DoctorCheck {
        name: "rt".to_string(),
        status: CheckStatus::Info,
        detail: "real-time scheduling reservations (rt:) are Linux-only; this platform \
                 always runs nodes under the ordinary time-sharing class regardless of any \
                 rt: block in the manifest"
            .to_string(),
    }
}

/// Format a byte count with the nearest binary unit, e.g. `1536` →
/// `"1.5 KiB"`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `/dev/shm` size on Linux (a real `statvfs`, via `rustix`, already used
/// elsewhere in this workspace for the same syscall); a documented,
/// `sysctl`-free informational note everywhere else (blueprint §17:
/// "macOS SC limits" — stock macOS has no `/dev/shm` mount at all, and
/// this importer deliberately never shells out to `sysctl(8)` to
/// approximate a kernel SHM limit; [`check_runtime_dir`] already
/// functionally probes the directory AstRS's own SHM broker actually
/// needs writable).
#[cfg(unix)]
fn check_shm() -> DoctorCheck {
    match rustix::fs::statvfs("/dev/shm") {
        Ok(stat) => {
            let total = stat.f_frsize.saturating_mul(stat.f_blocks);
            let available = stat.f_frsize.saturating_mul(stat.f_bavail);
            DoctorCheck {
                name: "shm".to_string(),
                status: CheckStatus::Ok,
                detail: format!(
                    "/dev/shm: {} total, {} available",
                    human_bytes(total),
                    human_bytes(available)
                ),
            }
        }
        Err(_) => DoctorCheck {
            name: "shm".to_string(),
            status: CheckStatus::Info,
            detail: "/dev/shm is not present on this platform (expected on macOS); AstRS's \
                     POSIX shared memory (shm_open) does not require it, and this check \
                     deliberately does not shell out to `sysctl` to approximate a kernel limit \
                     — see the runtime-dir check for a real writability probe instead"
                .to_string(),
        },
    }
}

/// Non-Unix platforms (Windows): SHM introspection is not implemented —
/// reported plainly rather than silently skipped.
#[cfg(not(unix))]
fn check_shm() -> DoctorCheck {
    DoctorCheck {
        name: "shm".to_string(),
        status: CheckStatus::Info,
        detail: "SHM introspection is implemented for Unix platforms only".to_string(),
    }
}

/// Run `<binary> --version` and report its output.
fn check_toolchain(binary: &str) -> DoctorCheck {
    let name = format!("toolchain:{binary}");
    match std::process::Command::new(binary).arg("--version").output() {
        Ok(output) if output.status.success() => DoctorCheck {
            name,
            status: CheckStatus::Ok,
            detail: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        },
        Ok(output) => DoctorCheck {
            name,
            status: CheckStatus::Error,
            detail: format!("`{binary} --version` exited with {}", output.status),
        },
        Err(err) => DoctorCheck {
            name,
            status: CheckStatus::Error,
            detail: format!("could not run `{binary} --version`: {err}"),
        },
    }
}

/// The directory the daemon's Unix domain socket lives in (blueprint
/// §4.2: `$XDG_RUNTIME_DIR/astrs/daemon.sock`), or the documented
/// fallback when `XDG_RUNTIME_DIR` is unset (routine on macOS) — created
/// if missing, then probed with a real create-and-remove write, not just
/// a metadata check.
fn check_runtime_dir() -> DoctorCheck {
    let (dir, source) = runtime_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return DoctorCheck {
            name: "runtime_dir".to_string(),
            status: CheckStatus::Error,
            detail: format!("could not create `{}` ({source}): {err}", dir.display()),
        };
    }
    let probe = dir.join(format!(".astrs-doctor-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"astrs doctor probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            DoctorCheck {
                name: "runtime_dir".to_string(),
                status: CheckStatus::Ok,
                detail: format!("`{}` is writable ({source})", dir.display()),
            }
        }
        Err(err) => DoctorCheck {
            name: "runtime_dir".to_string(),
            status: CheckStatus::Error,
            detail: format!("`{}` exists but is not writable: {err}", dir.display()),
        },
    }
}

/// Resolve the runtime directory and name where it came from.
fn runtime_dir() -> (PathBuf, &'static str) {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(value) if !value.is_empty() => (PathBuf::from(value).join("astrs"), "XDG_RUNTIME_DIR"),
        _ => (
            std::env::temp_dir().join("astrs"),
            "std::env::temp_dir() fallback (XDG_RUNTIME_DIR unset)",
        ),
    }
}

/// Render the report as a plain, fixed-width table.
fn render_table(report: &DoctorReport) -> String {
    let name_width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = String::new();
    for check in &report.checks {
        out.push_str(&format!(
            "{:<name_width$}  {:<7}  {}\n",
            check.name,
            check.status.to_string(),
            check.detail,
        ));
    }
    out.push_str(&format!(
        "\n{} check(s), exit code {}",
        report.checks.len(),
        report.exit_code()
    ));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_bound_port_is_reported_in_use() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let check = check_port("port:test", port);
        assert_eq!(check.status, CheckStatus::Warning);
        drop(listener);
    }

    #[test]
    fn a_freed_port_is_reported_available() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let check = check_port("port:test", port);
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn human_bytes_formats_common_magnitudes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 2), "2.0 MiB");
    }

    #[test]
    fn shm_check_never_errors_only_ok_or_info() {
        let check = check_shm();
        assert!(matches!(check.status, CheckStatus::Ok | CheckStatus::Info));
    }

    #[test]
    fn toolchain_checks_find_rustc_and_cargo_in_this_dev_environment() {
        // This crate cannot build at all without a working rustc/cargo,
        // so both must be found in the environment running this test.
        let rustc = check_toolchain("rustc");
        assert_eq!(rustc.status, CheckStatus::Ok, "{rustc:?}");
        let cargo = check_toolchain("cargo");
        assert_eq!(cargo.status, CheckStatus::Ok, "{cargo:?}");
    }

    #[test]
    fn toolchain_check_reports_error_for_a_nonexistent_binary() {
        let check = check_toolchain("astrs-doctor-definitely-does-not-exist-as-a-binary");
        assert_eq!(check.status, CheckStatus::Error);
    }

    #[test]
    fn runtime_dir_check_creates_and_probes_writability() {
        let check = check_runtime_dir();
        assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
    }

    #[test]
    fn multicast_check_never_errors_only_ok_or_warning() {
        // CI/sandboxed network namespaces routinely cannot join
        // multicast groups at all -- this must degrade to a warning,
        // never an error, and never panic either way.
        let check = check_multicast();
        assert!(
            matches!(check.status, CheckStatus::Ok | CheckStatus::Warning),
            "{check:?}"
        );
        assert!(check.detail.contains("239.255.0.1"));
    }

    #[test]
    fn full_report_has_eight_checks_and_exit_code_is_zero_or_one() {
        let mut out = Vec::new();
        let report = run(&mut out, &DoctorArgs::default()).unwrap();
        assert_eq!(report.checks.len(), 8);
        assert!(report.exit_code() == 0 || report.exit_code() == 1);
    }

    #[test]
    fn rt_capability_check_never_errors_only_ok_warning_or_info() {
        // Whatever this test process's own capabilities are (root, a
        // container with CAP_SYS_NICE dropped, macOS with no concept of
        // either) — never a panic, never CheckStatus::Error.
        let check = check_rt_capability();
        assert!(
            matches!(
                check.status,
                CheckStatus::Ok | CheckStatus::Warning | CheckStatus::Info
            ),
            "{check:?}"
        );
        assert_eq!(check.name, "rt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rt_capability_check_names_the_grant_when_unavailable() {
        let check = check_rt_capability();
        if check.status == CheckStatus::Warning {
            assert!(check.detail.contains("CAP_SYS_NICE"), "{}", check.detail);
            assert!(check.detail.contains("RLIMIT_RTPRIO"), "{}", check.detail);
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn rt_capability_check_is_informational_off_linux() {
        let check = check_rt_capability();
        assert_eq!(check.status, CheckStatus::Info);
        assert!(check.detail.contains("Linux-only"), "{}", check.detail);
    }

    #[test]
    fn json_output_round_trips_every_check() {
        let mut out = Vec::new();
        let args = DoctorArgs {
            json: true,
            ..DoctorArgs::default()
        };
        let report = run(&mut out, &args).unwrap();
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["checks"].as_array().map(Vec::len),
            Some(report.checks.len())
        );
    }

    #[test]
    fn table_output_lists_every_check_name() {
        let mut out = Vec::new();
        let report = run(&mut out, &DoctorArgs::default()).unwrap();
        let text = String::from_utf8(out).unwrap();
        for check in &report.checks {
            assert!(
                text.contains(&check.name),
                "missing {} in:\n{text}",
                check.name
            );
        }
    }

    #[test]
    fn default_ports_match_the_blueprint() {
        assert_eq!(DEFAULT_COORDINATOR_PORT, 7407);
        assert_eq!(DEFAULT_DAEMON_PORT, 7408);
    }
}
