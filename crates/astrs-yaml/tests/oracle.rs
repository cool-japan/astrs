//! The compatibility oracle: every scalar-resolution, structure and error
//! rule this crate implements, asserted against `serde_yaml` 0.9.
//!
//! This is the test that gates the switchover. `serde_yaml` is the parser
//! `astrs-manifest` uses today, so "does the new parser mean the same thing"
//! is not a question about the YAML specification — it is a question about
//! `serde_yaml`'s actual behaviour, including the places where that behaviour
//! is itself a choice (a leading zero disqualifies an integer; `1e400` is a
//! string because it overflows; `yes` is not a boolean).
//!
//! [`AGREE`] holds the inputs where the two parsers must produce identical
//! values — or must both reject the input. Every entry was derived by
//! *running* `serde_yaml`, not by reading the spec.
//!
//! [`documented_divergences`] holds the handful of inputs where the two
//! deliberately differ, each with the behaviour this crate guarantees. They
//! are the switchover's caveat list, in executable form.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use astrs_yaml::{ErrorKind, Limits, Value};

/// Inputs where `astrs-yaml` and `serde_yaml` must agree exactly.
const AGREE: &[&str] = &[
    // --- the core schema's booleans, and the "Norway problem" non-booleans
    "y",
    "Y",
    "yes",
    "Yes",
    "YES",
    "no",
    "No",
    "on",
    "On",
    "off",
    "Off",
    "n",
    "N",
    "true",
    "True",
    "TRUE",
    "tRue",
    "false",
    "False",
    "FALSE",
    // --- integers, in every radix and at the range boundaries
    "0",
    "42",
    "+1",
    "-1",
    "-0",
    "00",
    "017",
    "-00",
    "+00",
    "0123",
    "1_000",
    "1:30",
    "0x1F",
    "0X1f",
    "0o17",
    "0b101",
    "-0x1F",
    "+0x1F",
    "-0o17",
    "+0o17",
    "-0b101",
    "0o",
    "0x",
    "0b",
    "0o8",
    "0b2",
    "18446744073709551615",
    "0xFFFFFFFFFFFFFFFF",
    "-9223372036854775808",
    "9223372036854775807",
    "9223372036854775808",
    // --- floats, including the ones with a dot on only one side
    "1e3",
    "1E3",
    ".5",
    "5.",
    "1.",
    "-1.",
    "+.5",
    "1.0e+3",
    "0.5e-3",
    "3.14",
    "0.0",
    "-0.0",
    "5e-324",
    ".inf",
    ".Inf",
    ".INF",
    "-.inf",
    "+.inf",
    ".nan",
    ".NaN",
    ".NAN",
    "nan",
    "inf",
    "1e400",
    "1.7976931348623157e309",
    ".",
    "..",
    "e5",
    "1e",
    "1e+",
    ".e5",
    "1.2.3",
    "-",
    // --- nulls
    "~",
    "null",
    "Null",
    "NULL",
    "nUll",
    "",
    "   ",
    "\n",
    // --- tags
    "!!str 1\n",
    "!!int \"42\"\n",
    "!!bool true\n",
    "!!float 1\n",
    "!custom foo\n",
    "!!str\n",
    "a: !!str 1\n",
    "!!seq [1, 2]\n",
    "!!map {a: 1}\n",
    "!!binary aGk=\n",
    "!!timestamp 2020-01-01\n",
    "!!omap\n- a: 1\n",
    "!!set\n? a\n",
    "!!str [1]\n",
    "!!seq abc\n",
    "!<tag:yaml.org,2002:str> 1\n",
    "!<!custom> foo\n",
    "%TAG !e! tag:example.com,2000:\n---\n!e!foo bar\n",
    "!!merge x\n",
    "!!value x\n",
    "a: !!int 0x10\n",
    "!!float .inf\n",
    // --- anchors and aliases (no expansion bomb: those diverge on purpose)
    "a: &x 1\nb: *x\n",
    "a: &x {p: 1}\nb: *x\n",
    "- &x a\n- *x\n",
    "a: &x\n  p: 1\nb: *x\n",
    "&x a\n",
    "a: &x\nb: *x\n",
    // The merge key is *not* honoured by either parser: `<<` is a plain key.
    "base: &b {x: 1}\nchild:\n  <<: *b\n  y: 2\n",
    // --- block and flow structure
    "{a:b}\n",
    "{\"a\":1}\n",
    "{a: b}\n",
    "[a, b]\n",
    "[a,b]\n",
    "{a: }\n",
    "{}\n",
    "[]\n",
    "a:\n",
    "a: \n",
    "- a\n- b\n",
    "a:\n- 1\n- 2\n",
    "a:\n  - 1\n",
    "- - a\n",
    "- a: 1\n  b: 2\n",
    "? a\n: b\n",
    "[a, b]: v\n",
    "a:b: c\n",
    "- \n- b\n",
    "-\n- b\n",
    "-",
    "- ",
    "  a: 1\n  b: 2\n",
    "'a': 1\n",
    "\"a\": 1\n",
    "? a\n",
    "? a\n? b\n",
    "[a: 1]\n",
    "{a: 1, b}\n",
    "[a, [b, c]]\n",
    "{a: {b: c}}\n",
    "[\n  a,\n  b,\n]\n",
    "{\n  a: 1,\n}\n",
    "a: [1,\n  2]\n",
    "key with spaces: 1\n",
    "trailing:   \n",
    "a: \"\"\n",
    "a: ''\n",
    "a: b\tc\n",
    "a: b \n",
    "a:\tb\n",
    "a: !!str\n",
    // --- comments, documents, directives, encodings
    "key: value # comment\n",
    "key: value#nocomment\n",
    "# only a comment\n",
    "#c\na: 1\n",
    "a: #c\n  b\n",
    "a: 1 # c\n# c2\nb: 2\n",
    "---\n",
    "---\na: 1\n...\n",
    "%YAML 1.2\n---\na: 1\n",
    "\u{feff}a: 1\n",
    "a: 1\r\nb: 2\r\n",
    "\n\n\na: 1\n",
    "a: 1\n\n\n",
    // --- scalar styles
    "a: 'it''s'\n",
    "a: \"x\\ty\"\n",
    "plain multi\n  line\n",
    "a: plain multi\n  line\n",
    "a: 'x\n  y'\n",
    "a: \"x\n  y\"\n",
    "a: x\n  y\nb: 1\n",
    "a: >-\n  x\n\nb: 1\n",
    "a: |\n  x\n  y\n",
    "a: |-\n  x\n  y\n",
    "a: |+\n  x\n\n",
    "a: >\n  x\n  y\n",
    "a: >-\n  x\n  y\n",
    "a: >\n  x\n\n  y\n",
    "a: |2\n   x\n",
    "a: |\n",
    "a: >\n\n",
    "a: |\n  x\n\n\n",
    "a: >\n  x\n   y\n  z\n",
    // --- every double-quoted escape
    r#""\x41""#,
    r#""\u00e9""#,
    r#""\U0001F600""#,
    r#""\N""#,
    r#""\_""#,
    r#""\L""#,
    r#""\P""#,
    r#""\e""#,
    r#""\/""#,
    r#""\ ""#,
    r#""\0""#,
    "\"a\\\nb\"",
    r#""a\nb""#,
    "\"multi\nline\"",
    "\"multi\n\nline\"",
    r#""\'""#,
    // --- inputs both parsers must reject
    "a: b: c\n",
    "a: 1\na: 2\n",
    "{a: 1, a: 2}\n",
    "a: *missing\n",
    "\ta: 1\n",
    "a:\n  b: 1\n c: 2\n",
    "a: 1\n b: 2\n",
    "a: |\n    x\n  y\n",
    "...\n",
    "nodes: [\n",
    "a: \"unterminated\n",
    "a: 'unterminated\n",
    "- a\n- b\n  c: 1\n",
    "@invalid\n",
    "a: [1, 2\n",
    "a: {b: 1\n",
    "18446744073709551616",
    "-9223372036854775809",
    "0xFFFFFFFFFFFFFFFFF",
    // --- keys that are not strings stay distinct from the strings that
    //     spell them
    "1: x\n\"1\": y\n",
    // --- mapping keys compare the way `serde_yaml`'s do, which is what
    //     makes duplicate detection agree: `+0.0` is `-0.0`, and NaN equals
    //     itself even though IEEE says otherwise
    "0.0: a\n-0.0: b\n",
    ".nan: a\n.nan: b\n",
    "1e3: a\n1000.0: b\n",
    "0x10: a\n16: b\n",
    "true: a\nTrue: b\n",
    "null: a\n~: b\n",
    "'': a\n\"\": b\n",
    "1: a\n1.0: b\n",
    "0.0: a\n0: b\n",
    ".inf: a\n-.inf: b\n",
    // --- an anchor or a tag standing in for an *empty* node in flow
    //     context: the properties are the whole node, and its content is the
    //     empty scalar
    "[&i , *i]\n",
    "[&i]\n",
    "[!t , x]\n",
    "{a: &i , b: *i}\n",
    "{a: !t }\n",
    "[&i , &j , *i]\n",
    "{? &k : &v }\n",
    "[&a [1], *a]\n",
    // ...while an *undecorated* empty entry stays the error it always was
    "[,]\n",
    "[a,,b]\n",
    // --- the same shape in block context, where the properties belong to
    //     the empty key rather than to the mapping it opens
    "&base: 1\n",
    "&base: 1\nb: *base\n",
    "!t : 1\n",
    "&k key: 1\n",
    "? &k\n: 1\n",
    "- &x\n- *x\n",
    // --- a block scalar whose last line has no line break of its own: no
    //     chomping indicator may invent one
    "a: |\n  x",
    "a: |-\n  x",
    "a: |+\n  x",
    "a: >\n  x",
    "a: >-\n  x",
    "a: >+\n  x",
    "a: |2\n   x",
    "a: |\n  x\n  y",
    "a: >\n  x\n  y",
    "a: x",
    // --- characters YAML does not allow in a stream at all
    "a: x\u{0}y\n",
    "a: x\u{1}y\n",
    "a: \"x\u{1}y\"\n",
    "a: x\u{b}y\n",
    "a: x\u{c}y\n",
    "a: x\u{1b}y\n",
    "a: x\u{7f}y\n",
    "a: x\u{85}y\n",
    "a: x\u{9f}y\n",
    // ...but the printable ones next to them are fine
    "a: x\u{a0}y\n",
    "a: \"x\\x01y\"\n",
    // --- a global tag this crate does not implement still suppresses
    //     core-schema resolution, exactly as `serde_yaml` does
    "!!nope 0x10\n",
    "!!nope true\n",
    "!!nope\n",
    "!!binary 42\n",
    "!!timestamp 42\n",
    "a: !!nope 42\n",
    "!<tag:example.com,2000:thing> 42\n",
    "!<tag:yaml.org,2002:nope> 42\n",
    "%TAG !e! tag:example.com,2000:\n---\n!e!foo 42\n",
    "!!nope [1]\n",
    "!!nope {a: 1}\n",
    "!!nope \"42\"\n",
    // --- a named tag handle is `!`, word characters, `!` — and nothing
    //     else, so `!:!bool` is an ordinary local tag rather than a
    //     reference to an undefined handle
    "a: !:!bool x\n",
    "a: !&!float x\n",
    "a: !.!x y\n",
    "a: !my-tag x\n",
    "a: !my/tag x\n",
    "a: !foo x\n",
    "a: !e!foo x\n",
    "%TAG !e! tag:x,2000:\n---\na: !e!foo x\n",
];

#[test]
fn every_oracle_case_agrees_with_serde_yaml() {
    let mut failures = Vec::new();
    for case in AGREE {
        let outcome = support::compare(case);
        if !outcome.agree {
            failures.push(format!(
                "{case:?}\n    astrs-yaml: {}\n    serde_yaml: {}",
                outcome.mine, outcome.theirs
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} oracle cases disagree:\n  {}",
        failures.len(),
        AGREE.len(),
        failures.join("\n  ")
    );
}

#[test]
fn the_oracle_actually_covers_something() {
    // A guard against the table being emptied by an over-eager edit: the
    // agreement assertion above passes vacuously on an empty list.
    assert!(
        AGREE.len() > 260,
        "the oracle table shrank to {}",
        AGREE.len()
    );
}

/// Where the two parsers deliberately differ, and what this crate promises
/// instead.
#[test]
fn documented_divergences() {
    // 1. Alias expansion. `serde_yaml` has no budget at all: it expands
    //    every alias until the machine runs out of memory. This crate
    //    charges each expansion against `Limits::max_alias_nodes` *before*
    //    cloning, so the same document is refused rather than materialized.
    //    A four-level bomb (6561 nodes) is small enough that both parsers
    //    build it, which is what makes the comparison meaningful.
    let modest = "a: &a [x,x,x,x,x,x,x,x,x]\n\
                  b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\n\
                  c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\n\
                  d: [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n";
    assert!(serde_yaml::from_str::<serde_yaml::Value>(modest).is_ok());
    assert!(astrs_yaml::parse_str(modest).is_ok());
    let budgeted =
        astrs_yaml::parse_str_multi_with(modest, Limits::default().with_max_alias_nodes(100))
            .expect_err("the budget bites");
    assert!(budgeted.is_limit());
    assert!(matches!(
        budgeted.kind(),
        ErrorKind::AliasBudgetExhausted { .. }
    ));

    // And with the *default* budget, a bomb one level deeper than
    // `serde_yaml` itself survives is still refused in bounded time.
    let bomb = format!(
        "a: &a [x,x,x,x,x,x,x,x,x]\n{}boom: [*i,*i,*i,*i,*i,*i,*i,*i,*i]\n",
        [
            ('b', 'a'),
            ('c', 'b'),
            ('d', 'c'),
            ('e', 'd'),
            ('f', 'e'),
            ('g', 'f'),
            ('h', 'g'),
            ('i', 'h'),
        ]
        .iter()
        .map(|(name, previous)| format!(
            "{name}: &{name} [{}]\n",
            (0..9)
                .map(|_| format!("*{previous}"))
                .collect::<Vec<_>>()
                .join(",")
        ))
        .collect::<String>()
    );
    let refused = astrs_yaml::parse_str(&bomb).expect_err("the bomb is refused");
    assert!(refused.is_limit(), "{refused}");

    // 2. A stream of several documents. `serde_yaml::from_str` reports a
    //    bespoke error; this crate reports `MultipleDocuments` and offers
    //    `from_str_multi` for the case where several are expected.
    let stream = "a: 1\n---\nb: 2\n";
    assert!(serde_yaml::from_str::<serde_yaml::Value>(stream).is_err());
    let error = astrs_yaml::parse_str(stream).expect_err("one document expected");
    assert!(matches!(
        error.kind(),
        ErrorKind::MultipleDocuments { found: 2 }
    ));
    assert_eq!(
        astrs_yaml::parse_str_multi(stream).expect("stream").len(),
        2
    );

    // 3. The bare non-specific tag `!`. `serde_yaml` keeps it as a tagged
    //    value; this crate resolves the node normally, which is what the
    //    YAML specification says a non-specific tag means.
    assert!(matches!(
        serde_yaml::from_str::<serde_yaml::Value>("! x\n"),
        Ok(serde_yaml::Value::Tagged(_))
    ));
    assert_eq!(
        astrs_yaml::parse_str("! x\n").expect("bare tag"),
        Value::from("x")
    );

    // 4. A mapping whose key is itself a mapping. `serde_yaml`'s emitter
    //    cannot write one at all — it fails with an emitter error — while
    //    this crate writes the explicit `?` form and reads it straight back.
    let nested_key: serde_yaml::Value =
        serde_yaml::from_str("? {a: 1}\n: v\n").expect("serde_yaml parses it");
    assert!(serde_yaml::to_string(&nested_key).is_err());
    let mine = astrs_yaml::parse_str("? {a: 1}\n: v\n").expect("astrs-yaml parses it");
    let emitted = astrs_yaml::to_string(&mine).expect("astrs-yaml emits it");
    assert_eq!(emitted, "? a: 1\n: v\n");
    assert_eq!(astrs_yaml::parse_str(&emitted).expect("round trip"), mine);

    // 5. Duplicate keys are rejected by both, but this crate points at the
    //    *second* key rather than at the start of the mapping.
    let duplicate = "a: 1\nb: 2\na: 3\n";
    assert!(serde_yaml::from_str::<serde_yaml::Value>(duplicate).is_err());
    let error = astrs_yaml::parse_str(duplicate).expect_err("duplicate");
    assert!(matches!(error.kind(), ErrorKind::DuplicateKey { .. }));
    assert_eq!((error.line(), error.column()), (Some(3), Some(1)));

    // 6. The Unicode line separators. YAML 1.1 counted U+2028/U+2029 as
    //    line breaks and YAML 1.2 removed that rule; `libyaml` still
    //    implements the 1.1 behaviour, so a raw U+2028 inside a plain scalar
    //    *ends the line* for `serde_yaml` and is an ordinary character here.
    let split = "a: x\u{2028}y\n";
    assert!(serde_yaml::from_str::<serde_yaml::Value>(split).is_err());
    assert_eq!(
        astrs_yaml::parse_str(split).expect("an ordinary character"),
        astrs_yaml::parse_str("a: \"x\\Ly\"\n").expect("the escaped spelling")
    );

    //    On the way out the same disagreement is what makes `serde_yaml`
    //    lossy: it writes U+2028 raw inside single quotes and pads it with
    //    spaces when reading back, while this crate escapes it as `\L` in a
    //    double-quoted scalar, which is exact.
    let separator = Value::from("a\u{2028}b");
    let emitted = astrs_yaml::to_string(&separator).expect("emit");
    assert_eq!(emitted, "\"a\\Lb\"\n");
    assert_eq!(
        astrs_yaml::parse_str(&emitted).expect("round trip"),
        separator
    );

    // 7. A tag immediately followed by the bracket that closes its flow
    //    collection. The YAML grammar lets node properties stand alone with
    //    an empty scalar for content, so `[!t]` is a one-element sequence;
    //    `libyaml` demands whitespace after a tag and refuses the document.
    //    This crate accepts strictly more than `serde_yaml` here, so no
    //    document that reads today stops reading after a switchover.
    assert!(serde_yaml::from_str::<serde_yaml::Value>("[!t]\n").is_err());
    let bare = astrs_yaml::parse_str("[!t]\n").expect("a tag may stand alone");
    assert_eq!(
        bare.get_index(0)
            .and_then(Value::tag)
            .map(|t| t.to_string()),
        Some("!t".to_owned())
    );

    // 8. Continuation lines of a plain scalar *inside* a flow collection.
    //    YAML requires them to be indented further than the block node the
    //    collection belongs to; `libyaml` does not enforce that and accepts
    //    a closing bracket at column 0. This crate follows the
    //    specification, which is the one place it is *stricter* than
    //    `serde_yaml` on input a human might really write — so it is the
    //    divergence a switchover has to grep for: a `[` whose `]` sits on a
    //    later line at an indentation no deeper than its own key.
    for stricter in ["b: [x\ny]\n", "a:\n  b: [x\n  y]\n"] {
        assert!(
            serde_yaml::from_str::<serde_yaml::Value>(stricter).is_ok(),
            "{stricter:?}"
        );
        let error = astrs_yaml::parse_str(stricter).expect_err("under-indented continuation");
        assert!(error.line().is_some(), "{error}");
    }
    // Indenting the continuation one column further is accepted by both.
    assert!(support::compare("a:\n  b: [x\n   y]\n").agree);

    // 9. Malformed and unknown `%` directives. YAML says an unknown
    //    directive should be ignored with a warning; `libyaml` refuses the
    //    stream. This crate follows the specification and ignores them,
    //    which again accepts strictly more than `serde_yaml`.
    for ignored in ["%NOPE x\n---\na: 1\n", "%YAML`1.2\n---\na: 1\n"] {
        assert!(serde_yaml::from_str::<serde_yaml::Value>(ignored).is_err());
        assert_eq!(
            astrs_yaml::parse_str(ignored).expect("directive ignored"),
            astrs_yaml::parse_str("a: 1\n").expect("plain")
        );
    }
}

/// The diagnostics this crate exists to provide, which `serde_yaml` cannot.
#[test]
fn syntax_errors_carry_a_span_and_render_a_caret() {
    let source = "name: demo\nnodes:\n\t- id: camera\n";
    let error = astrs_yaml::parse_str(source).expect_err("tab indentation");
    assert!(matches!(error.kind(), ErrorKind::TabInIndentation));
    let rendered = error.render(source);
    assert!(rendered.contains("3:1"), "{rendered}");
    assert!(rendered.ends_with("\t- id: camera\n^"), "{rendered:?}");
}
