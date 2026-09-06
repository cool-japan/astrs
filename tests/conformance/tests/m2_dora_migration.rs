//! **Milestone M2**, half three (blueprint §21): *a dora manifest migrates
//! **and runs***.
//!
//! §8.6 promises a mechanical mapping — timer paths, `dora/…`→`astrs/…`,
//! restart fields, patterns — with a labelled TODO for anything that has no
//! AstRS equivalent, rather than a silent drop. `astrs-migrate`'s own tests
//! prove the *mapping*, byte for byte, against golden files. What only this
//! file proves is the second half of the milestone row: that the manifest
//! which comes out the other end is a graph the runtime actually executes.
//!
//! ```text
//!   fixtures/dora/pipeline.yml ──astrs migrate from-dora──► YAML
//!                                                            │
//!                                              stage onto built binaries
//!                                                            ▼
//!                                                      astrs run ──► exit 0
//! ```
//!
//! | Test | M2 evidence |
//! |---|---|
//! | `a_migrated_dora_pipeline_runs_to_completion` | migrate, then run: three processes, a service pair, a timer, exit zero, a result file |
//! | `the_migration_maps_every_construct_the_blueprint_names` | the §8.6 mapping, on the same fixture the run uses |
//! | `unmapped_constructs_are_labelled_rather_than_dropped` | `hub:`/`conda_env:`/`_unstable_debug:` become TODO comments and exit code 2 |
//!
//! # Why the fixture names this workspace's own binaries
//!
//! A dora descriptor pointing at `../../target/debug/service-example-client`
//! migrates just as well, and proves nothing about running: the binary does
//! not exist. This fixture's `path:` entries name binaries the
//! `hello-timer` and `service-roundtrip` examples build, so the migrated
//! manifest is staged onto them exactly like a committed example manifest and
//! then run for real.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use astrs_cli::command::migrate::{FromDoraArgs, FromDoraReport, run_from_dora};
use astrs_cli::command::run::{EXIT_OK, RunArgs, run};
use astrs_conformance::{StageOptions, binary, fixture, result_file, stage_manifest_with};
use astrs_migrate::NoteSeverity;
use astrs_wire::DataflowStatus;

/// The ceiling on the one graph this file runs.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// Migrates a committed dora fixture through the real `astrs migrate
/// from-dora` verb, returning its report and everything it printed.
fn migrate(name: &str) -> (FromDoraReport, String) {
    let args = FromDoraArgs {
        input: fixture(PathBuf::from("dora").join(name)),
        output: None,
        json: false,
    };
    let mut out: Vec<u8> = Vec::new();
    let report = run_from_dora(&mut out, &args).expect("astrs migrate from-dora");
    (report, String::from_utf8_lossy(&out).into_owned())
}

/// The §8.6 mapping, asserted on the fixture the run below uses — so a
/// mapping change that broke the run would be named here first.
#[test]
fn the_migration_maps_every_construct_the_blueprint_names() {
    let (report, printed) = migrate("pipeline.yml");

    assert!(
        report.notes.is_empty(),
        "a runnable fixture must migrate cleanly: {:?}",
        report.notes
    );
    assert_eq!(report.exit_code(), 0);
    assert_eq!(printed.trim_end(), report.yaml.trim_end());

    // Timer paths, rewritten; nothing left in dora's namespace.
    assert!(
        report.yaml.contains("astrs/timer/millis/50"),
        "{}",
        report.yaml
    );
    assert!(!report.yaml.contains("dora/"), "{}", report.yaml);
    // The hyphen spelling dora uses, rewritten to the manifest's own.
    assert!(report.yaml.contains("on_failure"), "{}", report.yaml);
    assert!(!report.yaml.contains("on-failure"), "{}", report.yaml);

    let manifest = astrs_manifest::Manifest::from_yaml_str(&report.yaml).expect("parses");
    manifest.validate().expect("validates");
    assert_eq!(manifest.nodes.len(), 3);

    let node = |id: &str| {
        manifest
            .nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("the migrated manifest has no `{id}`"))
    };
    // `build:` and `path:` survive untouched — they are what makes it runnable.
    assert_eq!(
        node("greeter").build.as_deref(),
        Some("cargo build -p hello-timer")
    );
    assert_eq!(
        node("greeter").path.as_deref(),
        Some("../../target/debug/hello-timer")
    );
    // Restart fields.
    assert_eq!(
        node("greeter").restart_policy,
        Some(astrs_manifest::RestartPolicy::OnFailure)
    );
    assert_eq!(node("greeter").max_restarts, Some(3));
    // The service pair, both halves.
    assert_eq!(
        node("client").pattern,
        Some(astrs_manifest::Pattern::ServiceClient)
    );
    assert_eq!(
        node("server").pattern,
        Some(astrs_manifest::Pattern::ServiceServer)
    );
}

/// The milestone row itself: migrate a dora descriptor, then run the result.
#[test]
fn a_migrated_dora_pipeline_runs_to_completion() {
    let (report, _) = migrate("pipeline.yml");

    // The migrated YAML is written out and staged exactly like a committed
    // example manifest: `path:` rewritten onto the binaries cargo built, the
    // per-run artefact paths injected as `env:`.
    let scratch = astrs_conformance::make_scratch_dir().expect("a scratch directory");
    let source = scratch.join("migrated.yml");
    std::fs::write(&source, &report.yaml).expect("the migrated manifest is writable");

    let ticks_path = result_file("dora-ticks.txt");
    let service_path = result_file("dora-service.json");
    let _ = std::fs::remove_file(&ticks_path);
    let _ = std::fs::remove_file(&service_path);

    // Each node's binary comes from a *different* package, so every path is
    // resolved explicitly rather than through one package-wide search.
    let options = StageOptions::new("hello-timer")
        .with_node_path("greeter", built("hello-timer", "hello-timer"))
        .with_node_path("client", built("service-client", "service-roundtrip"))
        .with_node_path("server", built("service-server", "service-roundtrip"))
        .with_env_path("HELLO_TIMER_RESULT", &ticks_path)
        .with_env("HELLO_TIMER_TICKS", "5")
        .with_env_path(service_roundtrip::ENV_RESULT_PATH, &service_path)
        .with_env("SERVICE_CALLS", "4");
    let staged = stage_manifest_with(&source, &options).expect("the migrated manifest stages");

    let mut args = RunArgs::new(&staged.manifest);
    args.skip_build = true;
    args.working_dir = Some(staged.dir.clone());
    args.runtime_dir = Some(staged.dir.clone());
    args.timeout = Some(RUN_TIMEOUT);
    args.grace = Some(Duration::from_millis(500));
    // A dora descriptor has no `exit_when_nodes_finish:` to migrate, so the
    // flag comes from the run rather than from the file — the same thing
    // `astrs run --exit-when-nodes-finish` gives an adopter trying a freshly
    // migrated graph.
    args.exit_when_nodes_finish = true;

    let mut terminal: Vec<u8> = Vec::new();
    let outcome = run(&mut terminal, &args).expect("astrs run returned a report");
    let text = String::from_utf8_lossy(&terminal).into_owned();

    assert_eq!(outcome.exit_code(), EXIT_OK, "non-zero exit:\n{text}");
    assert_eq!(
        outcome.result.status,
        DataflowStatus::Finished,
        "the migrated dataflow did not finish:\n{text}"
    );
    assert!(!outcome.result.has_failures(), "a node failed:\n{text}");
    assert_eq!(outcome.result.node_results.len(), 3, "{text}");

    // The migrated timer really drove its node…
    let ticks = std::fs::read_to_string(&ticks_path)
        .unwrap_or_else(|error| panic!("the greeter wrote no result: {error}\n{text}"));
    assert_eq!(ticks.trim(), "5", "unexpected tick count:\n{text}");

    // …and the migrated `pattern:` pair really correlated its calls (§9.4).
    let json = std::fs::read_to_string(&service_path)
        .unwrap_or_else(|error| panic!("the client wrote no result: {error}\n{text}"));
    let result: service_roundtrip::RoundTripResult =
        serde_json::from_str(&json).expect("a readable result");
    assert!(result.is_clean(), "{result:?}\n{text}");
    assert_eq!(result.requests, 4, "{result:?}\n{text}");
    assert_eq!(result.correct, result.requests, "{result:?}\n{text}");

    let _ = std::fs::remove_file(&ticks_path);
    let _ = std::fs::remove_file(&service_path);
    staged.clean();
    let _ = std::fs::remove_dir_all(&scratch);
}

/// §8.6's other half: a construct with no AstRS equivalent becomes a labelled
/// TODO and a non-zero exit code, never a silent drop.
#[test]
fn unmapped_constructs_are_labelled_rather_than_dropped() {
    let (report, printed) = migrate("unmapped.yml");

    assert_eq!(
        report.exit_code(),
        2,
        "a migration needing a human must say so in its exit code"
    );
    let attention = report
        .notes
        .iter()
        .filter(|note| note.severity == NoteSeverity::NeedsAttention)
        .count();
    assert_eq!(attention, 3, "notes: {:?}", report.notes);

    for construct in ["hub", "conda_env", "_unstable_debug"] {
        assert!(
            printed.contains(construct),
            "`{construct}` was dropped instead of being named:\n{printed}"
        );
    }
    assert!(
        printed.contains("TODO(astrs migrate)"),
        "every note must be greppable in the output:\n{printed}"
    );

    // What is left is still a parseable manifest — a human edits it, they do
    // not start over.
    let manifest = astrs_manifest::Manifest::from_yaml_str(&report.yaml).expect("parses");
    assert_eq!(manifest.nodes.len(), 2);
}

/// The absolute path of a built binary, or a failure naming the build line
/// that produces it.
fn built(name: &str, package: &str) -> String {
    binary(name, package)
        .unwrap_or_else(|error| panic!("{error}"))
        .display()
        .to_string()
}
