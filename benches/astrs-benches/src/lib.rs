//! Unit-testable helpers behind `benches/cold_start.rs`, the blueprint
//! §20.4 `astrs run` cold-start gate.
//!
//! Everything that talks to a child process, a socket, or the wall clock
//! stays in the bench file itself, on purpose: `[[bench]] test = false`
//! (see this crate's `Cargo.toml`, and `crates/astrs-shm/Cargo.toml`'s
//! identical note) keeps `cargo test`/nextest from building *or running*
//! `cold_start.rs` — a multi-second real-process measurement has no place
//! in the ordinary green-suite loop. This crate is the pure, deterministic
//! logic around that measurement — manifest text, the "is this node ready"
//! line predicate, percentile arithmetic, the pass/fail verdict — split out
//! specifically so it keeps ordinary unit test coverage rather than only
//! being exercised by actually running the bench.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Walks `levels` `parent()` steps up from `manifest_dir`.
///
/// The intended caller passes its own `env!("CARGO_MANIFEST_DIR")`; kept
/// parameterized (rather than reading the macro in here) so the walk itself
/// is testable against an arbitrary path.
///
/// # Examples
///
/// ```
/// use astrs_benches::ancestor;
///
/// assert_eq!(
///     ancestor("/work/astrs/benches/astrs-benches", 2),
///     Some("/work/astrs".into()),
/// );
/// ```
#[must_use]
pub fn ancestor(manifest_dir: &str, levels: usize) -> Option<PathBuf> {
    let mut path = Path::new(manifest_dir).to_path_buf();
    for _ in 0..levels {
        path = path.parent()?.to_path_buf();
    }
    Some(path)
}

/// The manifest text for a cold-start graph: `node_ids.len()` `hello-timer`
/// nodes, each on its own virtual `astrs/timer/millis/tick_millis` input,
/// each bounded to `ticks_per_node` ticks, none of them wired to each
/// other's inputs or outputs — a graph whose cold start is pure
/// spawn-and-register latency, nothing else.
///
/// # Examples
///
/// ```
/// use astrs_benches::manifest_text;
/// use std::path::Path;
///
/// let text = manifest_text(Path::new("/bin/hello-timer"), &["n0".to_owned()], 20, 8);
/// assert!(text.contains("exit_when_nodes_finish: true"));
/// assert!(text.contains("id: n0"));
/// assert!(text.contains("astrs/timer/millis/20"));
/// assert!(text.contains("HELLO_TIMER_TICKS: \"8\""));
/// assert!(text.contains("/bin/hello-timer"));
/// ```
#[must_use]
pub fn manifest_text(
    node_bin: &Path,
    node_ids: &[String],
    tick_millis: u64,
    ticks_per_node: u64,
) -> String {
    let mut nodes = String::new();
    for id in node_ids {
        nodes.push_str(&format!(
            "  - id: {id}\n    path: \"{path}\"\n    inputs:\n      tick: astrs/timer/millis/{tick_millis}\n    env:\n      HELLO_TIMER_TICKS: \"{ticks_per_node}\"\n",
            path = node_bin.display(),
        ));
    }
    format!("astrs: \"1\"\nname: cold-start-bench\nexit_when_nodes_finish: true\nnodes:\n{nodes}")
}

/// Whether `line` — one line of `astrs run`'s streamed terminal output —
/// names `id` as its node prefix.
///
/// Matches on the `[<id> ...]` convention
/// `bins/astrs-cli/src/command/log_stream.rs::render` produces (padding and
/// an optional level tag follow the id, so this is a prefix match on
/// `[<id>`, not an exact-line match) — the same convention
/// `bins/astrs-cli/tests/run_e2e.rs`'s own assertions read
/// (`text.contains("[pub")`).
///
/// # Examples
///
/// ```
/// use astrs_benches::line_names_node;
///
/// assert!(line_names_node("[n3      ] hello from n3, 8 ticks to go", "n3"));
/// assert!(!line_names_node("[n30     ] hello from n30, 8 ticks to go", "n3"));
/// assert!(!line_names_node("[n4      ] hello from n4, 8 ticks to go", "n3"));
/// ```
#[must_use]
pub fn line_names_node(line: &str, id: &str) -> bool {
    let mut bracket = String::with_capacity(id.len() + 1);
    bracket.push('[');
    bracket.push_str(id);
    // A padded prefix match, `[n3     ]`, is fine; `[n30    ]` must not
    // match `n3` — the character right after the id must end the id (a
    // space, closing bracket, or nothing) rather than continue it.
    line.split_once(bracket.as_str())
        .is_some_and(|(_, rest)| !rest.starts_with(|c: char| c.is_ascii_alphanumeric()))
}

/// Sorts `samples` in place and returns the value at percentile `p`
/// (`0.0..=100.0`), nearest-rank on the sorted sample set. `Duration::ZERO`
/// for an empty slice.
///
/// # Examples
///
/// ```
/// use astrs_benches::percentile;
/// use std::time::Duration;
///
/// let mut samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
/// // Nearest-rank: round(0.50 * 99) = 50 -> the 51st-smallest value.
/// assert_eq!(percentile(&mut samples, 50.0), Duration::from_millis(51));
/// assert_eq!(percentile(&mut samples, 99.0), Duration::from_millis(99));
/// assert_eq!(percentile(&mut [], 99.0), Duration::ZERO);
/// ```
#[must_use]
pub fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();
    let rank = ((p / 100.0) * (samples.len() - 1) as f64).round() as usize;
    samples[rank.min(samples.len() - 1)]
}

/// `"PASS"` when `measured <= target`, else `"FAIL"` — the one comparison
/// every `BENCH_GATE` line in the workspace's bench estate reports.
///
/// # Examples
///
/// ```
/// use astrs_benches::verdict;
/// use std::time::Duration;
///
/// assert_eq!(verdict(Duration::from_micros(90), Duration::from_micros(120)), "PASS");
/// assert_eq!(verdict(Duration::from_micros(150), Duration::from_micros(120)), "FAIL");
/// assert_eq!(verdict(Duration::from_micros(120), Duration::from_micros(120)), "PASS");
/// ```
#[must_use]
pub const fn verdict(measured: Duration, target: Duration) -> &'static str {
    if measured.as_nanos() <= target.as_nanos() {
        "PASS"
    } else {
        "FAIL"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn ancestor_walks_up_the_requested_number_of_levels() {
        assert_eq!(
            ancestor("/work/astrs/benches/astrs-benches", 0),
            Some(PathBuf::from("/work/astrs/benches/astrs-benches"))
        );
        assert_eq!(
            ancestor("/work/astrs/benches/astrs-benches", 1),
            Some(PathBuf::from("/work/astrs/benches"))
        );
        assert_eq!(
            ancestor("/work/astrs/benches/astrs-benches", 2),
            Some(PathBuf::from("/work/astrs"))
        );
    }

    #[test]
    fn ancestor_past_the_root_reports_none_rather_than_wrapping() {
        assert_eq!(ancestor("/", 1), None);
    }

    #[test]
    fn manifest_text_names_every_node_and_no_data_plane_wiring() {
        let ids: Vec<String> = (0..10).map(|index| format!("n{index}")).collect();
        let text = manifest_text(Path::new("/release/hello-timer"), &ids, 20, 8);

        assert!(text.starts_with("astrs: \"1\"\n"));
        assert!(text.contains("exit_when_nodes_finish: true"));
        for id in &ids {
            assert!(
                text.contains(&format!("id: {id}\n")),
                "missing node {id} in:\n{text}"
            );
        }
        assert_eq!(
            text.matches("astrs/timer/millis/20").count(),
            10,
            "every node needs its own virtual timer input"
        );
        assert!(
            !text.contains("outputs:") && !text.contains("->"),
            "a cold-start graph must carry no data-plane wiring between nodes"
        );
    }

    #[test]
    fn manifest_text_quotes_a_path_with_a_space() {
        let text = manifest_text(
            Path::new("/has a space/hello-timer"),
            &["n0".to_owned()],
            20,
            8,
        );
        assert!(text.contains("\"/has a space/hello-timer\""));
    }

    #[test]
    fn line_names_node_matches_the_bracketed_prefix_only() {
        assert!(line_names_node(
            "[n0      ] hello from n0, 8 ticks to go",
            "n0"
        ));
        assert!(line_names_node("[n0] tick 1", "n0"));
        assert!(!line_names_node("[n01     ] hello from n01", "n0"));
        assert!(!line_names_node("[astrs   ] dataflow finished", "n0"));
        assert!(!line_names_node("no brackets here at all", "n0"));
    }

    #[test]
    fn line_names_node_does_not_confuse_adjacent_ids() {
        let ids: Vec<String> = (0..10).map(|index| format!("n{index}")).collect();
        let line = "[n7      ] hello from n7, 8 ticks to go";
        let matches: Vec<&str> = ids
            .iter()
            .filter(|id| line_names_node(line, id))
            .map(String::as_str)
            .collect();
        assert_eq!(matches, vec!["n7"]);
    }

    #[test]
    fn percentile_of_one_sample_is_that_sample() {
        let mut one = [Duration::from_micros(42)];
        assert_eq!(percentile(&mut one, 50.0), Duration::from_micros(42));
        assert_eq!(percentile(&mut one, 99.0), Duration::from_micros(42));
    }

    #[test]
    fn percentile_sorts_out_of_order_input() {
        let mut samples = [
            Duration::from_millis(5),
            Duration::from_millis(1),
            Duration::from_millis(3),
        ];
        assert_eq!(percentile(&mut samples, 50.0), Duration::from_millis(3));
        assert_eq!(
            samples,
            [
                Duration::from_millis(1),
                Duration::from_millis(3),
                Duration::from_millis(5)
            ]
        );
    }

    #[test]
    fn verdict_is_inclusive_of_the_target() {
        let target = Duration::from_millis(800);
        assert_eq!(verdict(target, target), "PASS");
        assert_eq!(verdict(target - Duration::from_nanos(1), target), "PASS");
        assert_eq!(verdict(target + Duration::from_nanos(1), target), "FAIL");
    }
}
