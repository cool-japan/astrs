//! An env-gated structured fuzz loop over the parser and the emitter.
//!
//! Follows the workspace's fuzzing convention (blueprint §15): the loop runs
//! `ASTRS_FUZZ_ITERS` cases — [`DEFAULT_ITERATIONS`] by default, fast enough
//! to stay in the ordinary test suite — seeded from `ASTRS_FUZZ_SEED` so a
//! default run is byte-for-byte reproducible and a deliberately varied one
//! is one environment variable away. `cargo-fuzz`/libFuzzer is not used
//! anywhere in this workspace: it links a C++ runtime, which the pure-Rust
//! policy rules out.
//!
//! Two properties are checked on every generated case:
//!
//! 1. **The parser never panics.** Whatever the bytes are, the answer is a
//!    `Value` or an [`Error`](astrs_yaml::Error) — never an abort, never a
//!    stack overflow, never an unbounded loop.
//! 2. **Whatever it accepts, it can rewrite.** If a mutated document parses,
//!    emitting it and reading it back must produce the same value. This is
//!    where the interesting bugs live: a document nobody would write by hand
//!    that nonetheless parses into something the emitter cannot spell.
//!
//! Each case runs behind [`std::panic::catch_unwind`], so a hit is reported
//! with the exact input rather than taking the process down at the first
//! one, and the loop stops after [`MAX_FAILURES`] distinct hits so a
//! systemic bug cannot flood the log.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::panic::{self, AssertUnwindSafe};

/// Cases per run when `ASTRS_FUZZ_ITERS` is unset.
const DEFAULT_ITERATIONS: usize = 10_000;

/// Fixed default seed, so a default run is reproducible.
const DEFAULT_SEED: u64 = 0x59A1_4D07_C0DE_1234;

/// How many distinct failures to collect before giving up.
const MAX_FAILURES: usize = 5;

/// Documents the mutator starts from: between them they touch every
/// construct the parser implements.
const SEEDS: &[&str] = &[
    "name: perception\nnodes:\n- id: camera\n  path: ./camera\n  outputs: [frames]\n",
    "a: 1\nb: [1, 2, {c: 3}]\nd: {e: f}\n",
    "- a\n- - b\n  - c\n- d: 1\n  e: 2\n",
    "a: |\n  literal\n  block\nb: >-\n  folded\n  block\n",
    "a: 'single ''quoted'''\nb: \"double \\\"quoted\\\" \\x41\\u00e9\"\n",
    "base: &b {x: 1}\nchild: *b\nlist: [&i 1, *i]\n",
    "? [complex, key]\n: value\n? {another: key}\n: value2\n",
    "%YAML 1.2\n---\n!!str tagged\n...\n",
    "!Local\na: 1\n",
    "# comment\nkey: value # trailing\n\nnext: 2\n",
    "a: !!int 0x10\nb: !!float .inf\nc: !!bool true\nd: !!null ~\n",
    "top:\n  nested:\n    deep:\n    - 1\n    - 2\n",
    "empty_map: {}\nempty_seq: []\nnull_value:\nblank: ''\n",
    "multi: plain scalar\n  continued here\n  and here\n",
    "---\ndoc: 1\n---\ndoc: 2\n---\ndoc: 3\n",
];

/// Characters the mutator likes to inject, because they are the ones that
/// change how a line parses.
const INDICATORS: &[u8] = b"-?:,[]{}#&*!|>'\"%@`\n\t \\";

/// A tiny xorshift64* generator: deterministic, dependency-free, and good
/// enough to decorrelate a mutation stream.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.0 = state;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

fn iterations() -> usize {
    std::env::var("ASTRS_FUZZ_ITERS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS)
}

fn seed() -> u64 {
    std::env::var("ASTRS_FUZZ_SEED")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

/// Build one case by mutating a seed document, sometimes splicing two.
fn generate(rng: &mut Rng) -> Vec<u8> {
    let mut bytes = SEEDS[rng.below(SEEDS.len())].as_bytes().to_vec();
    if rng.below(4) == 0 {
        let other = SEEDS[rng.below(SEEDS.len())].as_bytes();
        let cut = rng.below(bytes.len() + 1);
        bytes.truncate(cut);
        bytes.extend_from_slice(&other[..rng.below(other.len() + 1)]);
    }
    for _ in 0..=rng.below(4) {
        if bytes.is_empty() {
            bytes.push(b'a');
        }
        match rng.below(6) {
            0 => {
                let at = rng.below(bytes.len());
                bytes[at] = INDICATORS[rng.below(INDICATORS.len())];
            }
            1 => {
                let at = rng.below(bytes.len() + 1);
                bytes.insert(at, INDICATORS[rng.below(INDICATORS.len())]);
            }
            2 => {
                let at = rng.below(bytes.len());
                bytes.remove(at);
            }
            3 => {
                // Duplicate a slice: makes duplicate keys and runaway
                // indentation far more likely than random bytes would.
                let start = rng.below(bytes.len());
                let end = start + rng.below(bytes.len() - start + 1);
                let slice = bytes[start..end].to_vec();
                let at = rng.below(bytes.len() + 1);
                bytes.splice(at..at, slice);
            }
            4 => {
                let at = rng.below(bytes.len() + 1);
                for _ in 0..=rng.below(6) {
                    bytes.insert(at, b' ');
                }
            }
            _ => {
                let at = rng.below(bytes.len());
                bytes[at] = bytes[at].wrapping_add(1);
            }
        }
    }
    bytes
}

/// Run one case, asserting both properties.
fn check(bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        // The public API takes `&str`; invalid UTF-8 cannot reach it.
        return;
    };
    let Ok(documents) = astrs_yaml::parse_str_multi(text) else {
        return;
    };
    for document in &documents {
        let emitted = astrs_yaml::to_string(document).expect("emitting a parsed value cannot fail");
        let reread = astrs_yaml::parse_str(&emitted)
            .unwrap_or_else(|error| panic!("re-parse failed: {error}\n--- emitted ---\n{emitted}"));
        assert_eq!(
            &reread, document,
            "round trip changed the value\n--- emitted ---\n{emitted}"
        );
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

#[test]
fn the_parser_survives_mutated_documents() {
    let mut rng = Rng(seed() | 1);
    let mut failures = Vec::new();
    let total = iterations();
    for case in 0..total {
        let input = generate(&mut rng);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| check(&input)));
        if let Err(payload) = outcome {
            failures.push(format!(
                "case {case}: {}\n    input: {:?}",
                panic_message(&payload),
                String::from_utf8_lossy(&input)
            ));
            if failures.len() >= MAX_FAILURES {
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failure(s) in {total} case(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The regression corpus: inputs that once broke the parser, replayed
/// unconditionally regardless of `ASTRS_FUZZ_ITERS`, so a fixed bug stays
/// fixed even in a zero-iteration run.
#[test]
fn the_regression_corpus_still_passes() {
    const REGRESSIONS: &[&str] = &[
        // A sequence entry with no value followed by a sibling entry: the
        // second `-` is the next entry, not a nested sequence.
        "- \n- b\n",
        "-\n- b\n",
        // A quote in the middle of a plain key is a character, not a quote.
        "? a\": null\n: null\n",
        "a\"b: 1\n",
        "a[b: 1\n",
        // An explicit key introducing a compact block sequence.
        "? - null\n: null\n",
        // A block scalar ends at a line boundary, so the `:` on the next
        // line is not a stray mapping value.
        "? - |2+\n\n: null\n",
        // A tagged root whose payload is a block scalar.
        "!A |2-\n\n  ]\n",
        // Doubly tagged values need the inner tag on its own line.
        "!A\n!B 1\n",
        // Alias budget accounting must survive an anchor defined twice.
        "a: &x [1, 2]\nb: &x [3]\nc: *x\n",
        // An anchor or tag standing in for an empty node in flow context.
        // These once raised "unexpected character ','" and left the alias
        // below them dangling.
        "[&i , *i]\n",
        "[&i]\n",
        "[!t , x]\n",
        "{a: &i , b: *i}\n",
        "{? &k : &v }\n",
        "[!t]\n",
        // The same shape in block context. `&base: 1` once parsed as the
        // bare scalar `1` — a silently wrong value, with the mapping gone
        // and the anchor named `base:`.
        "&base: 1\n",
        "&base: 1\nb: *base\n",
        "!t : 1\n",
        // A block scalar whose last line has no line break: no chomping
        // indicator may invent one, `+` included.
        "a: |\n  x",
        "a: |+\n  x",
        "a: >\n  x\n  y",
        "a: |2\n   x",
        // A named tag handle is word characters only, so this is a local
        // tag rather than a reference to an undefined handle.
        "a: !:!bool x\n",
        "a: !.!x y\n",
        // A global tag this crate does not implement suppresses core-schema
        // resolution rather than being ignored outright.
        "!!binary 42\n",
        "!!nope\n",
    ];
    for regression in REGRESSIONS {
        let documents = astrs_yaml::parse_str_multi(regression)
            .unwrap_or_else(|error| panic!("{regression:?}: {error}"));
        for document in &documents {
            let emitted = astrs_yaml::to_string(document).expect("emit");
            let reread = astrs_yaml::parse_str(&emitted)
                .unwrap_or_else(|error| panic!("{regression:?} -> {emitted:?}: {error}"));
            assert_eq!(&reread, document, "{regression:?} -> {emitted:?}");
        }
    }
}

#[test]
fn the_iteration_and_seed_knobs_have_sane_defaults() {
    // Reading the environment is process-global, so assert the defaults
    // rather than setting anything.
    assert!(iterations() > 0);
    assert_eq!(DEFAULT_ITERATIONS, 10_000);
    assert_ne!(seed(), 0);
}
